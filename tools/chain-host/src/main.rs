// Chain host: boots a local NEAR sandbox, deploys the three contracts that make
// this market, gives every simulated participant a real account with a real
// key, funds them, and writes the session file the Node side signs with.
//
//   cargo run -p chain-host -- --out data/chain-session.json --spot 2350
//
// Everything here is local: no testnet, no wallet, no keys of yours. The
// sandbox is the same runtime a validator runs, so the transactions the demo
// screen sends are real transactions with real hashes against the real contract.
//
// With `--engine http://127.0.0.1:8791` the host also becomes the operator: it
// asks the engine which series the live ladder needs and sends the
// `create_series` calls for the ones the chain does not have. The engine decides
// the market; this process is the only thing holding an admin key.
//
// The host parks when it is done. The sandbox lives and dies with this process,
// so Ctrl-C takes the chain down with it, and re-lists on `--list-secs`.
use anyhow::{Context, Result};
mod operator;

use near_workspaces::types::{Gas, NearToken};
use near_workspaces::{Account, Contract};
use serde_json::json;
use std::path::PathBuf;
use std::{env, fs};

const SCALE: u128 = 1_000_000;
/// What each participant can trade with, in USDT with six decimals.
const EACH: u128 = 5_000_000;
/// What the vault opens with. A pool with no equity writes nothing, so a fresh
/// sandbox would answer every click with "pool full" until someone read the
/// README far enough to fund it.
const LP_SEED: u128 = 100_000;
const GAS: Gas = Gas::from_tgas(80);
const ROLES: [&str; 10] = [
    "you", "lp", "maker-1", "maker-2", "maker-3", "bidder-1", "bidder-2", "taker-1", "taker-2",
    "taker-3",
];

fn arg(name: &str, fallback: &str) -> String {
    let argv: Vec<String> = env::args().collect();
    argv.iter()
        .position(|a| a == name)
        .and_then(|i| argv.get(i + 1))
        .map(|value| value.to_string())
        .unwrap_or_else(|| fallback.to_string())
}

fn optional(name: &str) -> Option<String> {
    let argv: Vec<String> = env::args().collect();
    argv.iter()
        .position(|a| a == name)
        .and_then(|i| argv.get(i + 1))
        .map(|value| value.to_string())
}

fn assets() -> Vec<String> {
    optional("--assets")
        .unwrap_or_else(|| "ETH,BTC".to_string())
        .split(',')
        .map(|asset| asset.trim().to_uppercase())
        .filter(|asset| !asset.is_empty())
        .collect()
}

/// Every series the chain holds, as `(id, view)`. Ids are assigned from zero and
/// never reused, so `0..series_count` is the complete picture and one pass of
/// reads is cheaper than an index the contract would have to maintain.
async fn chain_series(options: &Contract) -> Result<Vec<(u64, serde_json::Value)>> {
    let stats = options.view("stats").await?.json()?;
    let mut rows = Vec::new();
    for id in 0..operator::series_count(&stats).unwrap_or(0) {
        let view = options
            .view("get_series")
            .args_json(json!({ "series_id": id }))
            .await?
            .json()?;
        rows.push((id, view));
    }
    Ok(rows)
}

/// The engine's plan against the chain's ledger, executed.
///
/// Only the *missing* series are sent, so a re-list is a few reads and no
/// transactions: series ids are assigned from zero and never reused, which makes
/// `get_series` over `0..series_count` a complete picture of what is listed.
/// The expiry of the ladder the engine is marking. It rolls before it dies, so
/// this is a moving target the operator has to keep up with.
async fn engine_expiry(engine: &str) -> Result<u64> {
    let health: serde_json::Value = serde_json::from_str(
        &operator::http_get(&format!("{}/health", engine.trim_end_matches('/'))).map_err(anyhow::Error::msg)?,
    )
    .context("engine /health is not JSON")?;
    health
        .get("expiryNs")
        .and_then(serde_json::Value::as_u64)
        .context("engine /health has no expiryNs")
}

/// Whether the chain already holds the ladder the engine is marking.
///
/// A roll that goes unlisted is a venue with marks and no counterparty: the
/// operator quotes only series whose expiry the engine named, so until the next
/// listing pass every cell on the screen is untradeable. An unreachable engine
/// answers "listed", because the cadence will retry it either way.
async fn ladder_is_listed(engine: &str, rows: &[(u64, serde_json::Value)]) -> bool {
    let wanted = match engine_expiry(engine).await {
        Ok(expiry_ns) => expiry_ns,
        Err(_) => return true,
    };
    rows.iter().any(|(_, row)| {
        row.get("settlement").and_then(serde_json::Value::as_str) == Some("Open")
            && row.get("expiry_ns").and_then(serde_json::Value::as_u64) == Some(wanted)
    })
}

async fn list_ladder(
    admin: &Account,
    options: &Contract,
    engine: &str,
    assets: &[String],
    rows: &[(u64, serde_json::Value)],
) -> Result<(usize, usize)> {
    let engine = engine.trim_end_matches('/');
    let expiry_ns = engine_expiry(engine).await?;

    let mut existing: Vec<operator::Listed> = rows
        .iter()
        .filter_map(|(_, row)| operator::Listed::from_view(row))
        .collect();

    let mut planned = 0usize;
    let mut created = 0usize;
    for asset in assets {
        let body = operator::http_get(&format!("{engine}/plan?asset={asset}&expiry_ns={expiry_ns}"))
            .map_err(anyhow::Error::msg)?;
        let plans = operator::parse_plan(&body)
            .map_err(|e| anyhow::anyhow!("plan for {asset}: {e}"))?;
        planned += plans.len();
        for plan in operator::missing(&plans, &existing) {
            admin
                .call(options.id(), "create_series")
                .args_json(plan.args())
                .gas(GAS)
                .transact()
                .await?
                .into_result()?;
            existing.push(operator::Listed {
                underlying: plan.underlying.clone(),
                kind: plan.kind.clone(),
                expiry_ns: plan.expiry_ns,
                low_units: plan.low_units,
                high_units: plan.high_units,
            });
            created += 1;
        }
    }
    Ok((created, planned))
}

/// Report the index for every feed that has a series past expiry, then ask the
/// contract to fix them. Without this leg a ticket can be bought and closed but
/// never paid out: the contract will only settle on an observation the oracle
/// holds, and the oracle only accepts one from this account.
///
/// One print per feed per pass, because the oracle rejects an observation that
/// does not advance time and the whole grid expires on the same clock.
async fn fix_due(
    admin: &Account,
    options: &Contract,
    oracle: &Contract,
    engine: &str,
    rows: &[(u64, serde_json::Value)],
    now_ns: u64,
    last_print_ns: &mut std::collections::HashMap<String, u64>,
) -> Result<(usize, usize)> {
    let due = operator::due_for_fix(rows, now_ns);
    if due.is_empty() {
        return Ok((0, 0));
    }
    let engine = engine.trim_end_matches('/');
    let mut reported = 0usize;
    for (feed, asset) in operator::feeds_needing_prints(&due) {
        let body = operator::http_get(&format!("{engine}/index?asset={asset}")).map_err(anyhow::Error::msg)?;
        let print = operator::parse_print(&body)
            .map_err(|e| anyhow::anyhow!("no print for {feed}: {e}"))?;
        if last_print_ns.get(&feed).is_some_and(|last| print.publish_time_ns <= *last) {
            // The clock has not moved since the last report on this feed. Waiting
            // one more pass is right; panicking on the oracle would not be.
            continue;
        }
        admin
            .call(oracle.id(), "report")
            .args_json(json!({
                "feed_id": feed,
                "price": print.price_units,
                "publish_time_ns": print.publish_time_ns,
                "confidence_bps": print.confidence_bps as u16,
            }))
            .gas(GAS)
            .transact()
            .await?
            .into_result()?;
        last_print_ns.insert(feed, print.publish_time_ns);
        reported += 1;
    }
    let mut asked = 0usize;
    for item in &due {
        admin
            .call(options.id(), "request_settlement")
            .args_json(json!({ "series_id": item.series_id }))
            .gas(GAS)
            .transact()
            .await?
            .into_result()?;
        asked += 1;
    }
    Ok((reported, asked))
}

/// Name the vault's prices. The contract will only trade a series at a mid the
/// venue published itself, so without this leg nothing on the ladder is buyable
/// and every click answers "the venue has not quoted this series".
///
/// Signed by the venue account — `set_pool_quote` answers to nobody else, and in
/// this sandbox the venue is the admin the contract was initialised with. Only
/// the mids travel; the fee arithmetic is the contract's, from `core::fees`.
async fn quote_ladder(
    venue: &Account,
    options: &Contract,
    engine: &str,
    assets: &[String],
    rows: &[(u64, serde_json::Value)],
    now_ns: u64,
    renew_ns: u64,
) -> Result<(usize, usize)> {
    let engine = engine.trim_end_matches('/');
    let mut live = std::collections::HashMap::new();
    for (series_id, _) in rows {
        let quote = options
            .view("get_pool_quote")
            .args_json(json!({ "series_id": series_id }))
            .await?
            .json::<serde_json::Value>()?;
        let Some(standing) = operator::standing_quote(&quote) else {
            continue;
        };
        live.insert(*series_id, standing);
    }

    let mut posted = 0usize;
    let mut priced = 0usize;
    for asset in assets {
        let body = operator::http_get(&format!("{engine}/state?asset={asset}"))
            .map_err(anyhow::Error::msg)?;
        let (expiry_ns, plans) =
            operator::parse_state(&body).map_err(|e| anyhow::anyhow!("state for {asset}: {e}"))?;
        priced += plans.iter().filter(|plan| plan.mid_units > 0).count();
        for (series_id, mid_units) in
            operator::quotes_needed(&plans, expiry_ns, rows, &live, now_ns, renew_ns)
        {
            venue
                .call(options.id(), "set_pool_quote")
                .args_json(json!({
                    "series_id": series_id,
                    "mid_units": mid_units.to_string(),
                    "stamp_ns": now_ns,
                }))
                .gas(GAS)
                .transact()
                .await?
                .into_result()?;
            posted += 1;
        }
    }
    Ok((posted, priced))
}

fn wasm(relative: &str) -> Result<Vec<u8>> {    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..").join(relative);
    fs::read(&root).with_context(|| format!("{} is missing · run scripts/build.sh", root.display()))
}

async fn fund(env_admin: &Account, token: &Contract, options: &Contract, role: &Account) -> Result<()> {
    role.call(token.id(), "storage_deposit")
        .args_json(json!({ "account_id": null }))
        .deposit(NearToken::from_millinear(100))
        .gas(GAS)
        .transact()
        .await?
        .into_result()?;
    env_admin
        .call(token.id(), "mint")
        .args_json(json!({ "account_id": role.id(), "amount": (EACH * SCALE).to_string() }))
        .gas(GAS)
        .transact()
        .await?
        .into_result()?;
    role.call(token.id(), "ft_transfer_call")
        .args_json(json!({
            "receiver_id": options.id(),
            "amount": (EACH * SCALE).to_string(),
            "msg": "deposit"
        }))
        .deposit(NearToken::from_yoctonear(1))
        .gas(GAS)
        .transact()
        .await?
        .into_result()?;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let out = PathBuf::from(arg("--out", "data/chain-session.json"));
    let spot: u128 = arg("--spot", "2350").parse().context("--spot must be a number")?;
    let width: u128 = arg("--width", "100").parse().context("--width must be a number")?;

    println!("booting a local NEAR sandbox");
    let worker = near_workspaces::sandbox().await?;
    println!("rpc {}", worker.rpc_addr());

    let options = worker.dev_deploy(&wasm("target/near/bounded_options/bounded_options.wasm")?).await?;
    let token = worker.dev_deploy(&wasm("target/near/mock_ft/mock_ft.wasm")?).await?;
    let oracle = worker.dev_deploy(&wasm("target/near/price_oracle/price_oracle.wasm")?).await?;
    println!("deployed {} {} {}", options.id(), token.id(), oracle.id());

    let admin = worker.dev_create_account().await?;
    let mut roles = Vec::with_capacity(ROLES.len());
    for role in ROLES {
        roles.push((role, worker.dev_create_account().await?));
    }

    token
        .call("new")
        .args_json(json!({ "owner": admin.id() }))
        .gas(GAS)
        .transact()
        .await?
        .into_result()?;
    oracle
        .call("new")
        .args_json(json!({ "owner": admin.id(), "reporter": admin.id() }))
        .gas(GAS)
        .transact()
        .await?
        .into_result()?;
    options
        .call("new")
        .args_json(json!({ "admin": admin.id(), "token": token.id(), "oracle": oracle.id() }))
        .gas(GAS)
        .transact()
        .await?
        .into_result()?;

    // The contract needs its own storage slot on the token before it can accept
    // a deposit, and the admin pays for it once.
    admin
        .call(token.id(), "storage_deposit")
        .args_json(json!({ "account_id": null }))
        .deposit(NearToken::from_millinear(100))
        .gas(GAS)
        .transact()
        .await?
        .into_result()?;
    admin
        .call(token.id(), "storage_deposit")
        .args_json(json!({ "account_id": options.id() }))
        .deposit(NearToken::from_millinear(100))
        .gas(GAS)
        .transact()
        .await?
        .into_result()?;
    for (_, role) in &roles {
        fund(&admin, &token, &options, role).await?;
        println!("funded {} with {} USDT", role.id(), EACH);
    }

    // The vault stands behind every ticket the screen will write, so it has to
    // be funded before anything can trade. It comes from its own role, never
    // from the visitor's balance: the first click on a fresh sandbox is a buy,
    // not a lesson in liquidity.
    let seed = LP_SEED * SCALE;
    if let Some((_, lp)) = roles.iter().find(|(name, _)| *name == "lp") {
        lp.call(options.id(), "pool_deposit_from_balance")
            .args_json(json!({ "amount": seed.to_string() }))
            .gas(GAS)
            .transact()
            .await?
            .into_result()?;
        println!("vault opened with {} USDT of equity", LP_SEED);
    }

    let engine = optional("--engine");
    let list_secs: u64 = arg("--list-secs", "300").parse().context("--list-secs must be seconds")?;
    // Well inside the contract's own quote age limit, which is what makes a
    // stopped host a vault that stops trading rather than one stuck on
    // yesterday's price. Ten seconds against a sixty-second limit leaves six
    // passes of slack, and it keeps the price the trader clicks close to the
    // price the contract charges.
    let quote_secs: u64 = arg("--quote-secs", "10").parse().context("--quote-secs must be seconds")?;
    let renew_ns = quote_secs * 1_000_000_000;
    let assets_list = assets();
    let mut listed = (0usize, 0usize);
    if let Some(url) = &engine {
        // The ladder before the ledger file is written, so the session records
        // what the chain actually holds. The engine is a second process that has
        // only just been told to start, so a first read that fails is retried
        // rather than taken for a broken venue: the host exiting here would leave
        // no session file at all, and the gateway would come up pointed at the
        // previous run's dead sandbox.
        for attempt in 1.. {
            match list_ladder(&admin, &options, url, &assets_list, &chain_series(&options).await?).await {
                Ok(out) => {
                    listed = out;
                    println!("listed {} of {} series from the engine", out.0, out.1);
                    break;
                }
                Err(error) if attempt < 10 => {
                    println!("engine not answering yet ({error}), retry {attempt}");
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                }
                Err(error) => return Err(error).with_context(|| format!("listing the ladder from {url}")),
            }
        }
        // …and quoted, so the first visitor to arrive finds a vault that is
        // already selling rather than five seconds of "no price yet".
        let rows = chain_series(&options).await?;
        let now_ns = worker.view_block().await?.timestamp();
        match quote_ladder(&admin, &options, url, &assets_list, &rows, now_ns, renew_ns).await {
            Ok((posted, priced)) => println!("quoted {posted} of {priced} priced cells"),
            Err(error) => println!("quoting failed, chain still live: {error}"),
        }
    }

    let now = worker.view_block().await?.timestamp();
    // The one series the host makes by hand exists for the JavaScript demo, which
    // expects a market without asking the engine for one. When the engine is the
    // operator it must not be created: it lands on the same cell as a rung of the
    // live ladder, and a later id wins the registry's lookup — so a trader would
    // buy the hand-made series and watch it fail to settle while the engine's own
    // series, on the same strikes, fixed correctly.
    let demo_expiry = now + 3_600_000_000_000;
    let hand_made = if engine.is_some() {
        serde_json::Value::Null
    } else {
        admin
            .call(options.id(), "create_series")
            .args_json(json!({
                "underlying": "ETH",
                "kind": "Call",
                "expiry_ns": demo_expiry,
                "accept_until_ns": demo_expiry + 60_000_000_000u64,
                "finalize_until_ns": demo_expiry + 600_000_000_000u64,
                "low_strike": (spot * SCALE).to_string(),
                "high_strike": ((spot + width) * SCALE).to_string(),
                "price_scale": SCALE.to_string(),
                "min_confidence_bps": 3000,
                "oracle_feed": "ETH-USD"
            }))
            .gas(GAS)
            .transact()
            .await?
            .into_result()?
            .json()?
    };

    let session = json!({
        "schema": "bounded-options/chain-session/1",
        "at": now.to_string(),
        "rpc": worker.rpc_addr(),
        "network": "local-sandbox",
        "decimals": 6,
        "scale": SCALE.to_string(),
        "contracts": { "options": options.id().to_string(), "token": token.id().to_string(), "oracle": oracle.id().to_string() },
        "admin": { "account": admin.id().to_string(), "secret": admin.secret_key().to_string() },
        "roles": roles.iter().map(|(name, account)| (name.to_string(), json!({
            "account": account.id().to_string(),
            "secret": account.secret_key().to_string(),
            "deposit": (EACH * SCALE).to_string(),
        }))).collect::<serde_json::Map<_, _>>(),
        "engine": engine,
        "listed": { "created": listed.0, "planned": listed.1 },
        "series": hand_made,
        "note": "sandbox keys only: these accounts exist for as long as this process runs",
    });
    if let Some(parent) = out.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&out, serde_json::to_string_pretty(&session)?.as_bytes())?;
    println!("session written to {} · Ctrl-C to stop the chain", out.display());

    // The sandbox is a child of this process: parking here keeps the chain
    // reachable for the gateway, and exiting takes it down. Re-listing on the
    // interval is what follows the index onto a new rung of the ladder; a
    // strike the chain already has costs one read, not a transaction.
    // One loop, two cadences: a fix has to be prompt because the accept window
    // closes, while a re-list only matters when the index moves a whole rung.
    // Series that pass their window un-fixed are voided by anyone who cares, and
    // a void returns every premium — it is a soft failure, not a loss.
    if let Some(url) = &engine {
        let tick_secs: u64 = arg("--tick-secs", "5").parse().context("--tick-secs must be seconds")?;
        let assets_list = assets();
        let mut last_print: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
        let mut since_list = list_secs;
        let mut since_quote = quote_secs;
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(tick_secs)).await;
            since_list += tick_secs;
            since_quote += tick_secs;
            let rows = match chain_series(&options).await {
                Ok(rows) => rows,
                Err(error) => {
                    println!("read failed, chain still live: {error}");
                    continue;
                }
            };
            if since_list >= list_secs || !ladder_is_listed(url, &rows).await {
                since_list = 0;
                match list_ladder(&admin, &options, url, &assets_list, &rows).await {
                    Ok((created, planned)) if created > 0 => println!("listed {created} of {planned} series"),
                    Ok((_, planned)) => println!("ladder unchanged · {planned} series planned"),
                    Err(error) => println!("listing failed, chain still live: {error}"),
                }
            }
            let now_ns = worker.view_block().await?.timestamp();
            if since_quote >= quote_secs {
                since_quote = 0;
                match quote_ladder(&admin, &options, url, &assets_list, &rows, now_ns, renew_ns).await {
                    // A still index re-posts nothing, so the line only prints
                    // when the venue actually said something new.
                    Ok((0, _)) => {}
                    Ok((posted, priced)) => println!("re-quoted {posted} of {priced} priced cells"),
                    Err(error) => println!("quoting failed, chain still live: {error}"),
                }
            }
            match fix_due(&admin, &options, &oracle, url, &rows, now_ns, &mut last_print).await {
                Ok((reported, asked)) if asked > 0 => println!("fixed {asked} series on {reported} feed prints"),
                Ok(_) => {}
                Err(error) => println!("fixing failed, chain still live: {error}"),
            }
        }
    }
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
    }
}
