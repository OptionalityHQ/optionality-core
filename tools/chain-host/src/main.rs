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
mod activity;
mod maker;
mod operator;
mod oracle;
mod testnet;

use near_workspaces::types::{Gas, NearToken};
use near_workspaces::{Account, Contract};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::{env, fs};

const SCALE: u128 = 1_000_000;
/// What each participant can trade with, in USDT with six decimals.
const EACH: u128 = 5_000_000;
/// What the vault opens with. A pool with no equity writes nothing, so a fresh
/// sandbox would answer every click with "pool full" until someone read the
/// README far enough to fund it.
const LP_SEED: u128 = 100_000;
const GAS: Gas = Gas::from_tgas(80);
/// State rent. The contract charges whoever creates a series or a first quote
/// for the bytes they add, because on a public chain the contract pays for that
/// state out of its own balance. These mirror the contract's own byte counts —
/// `SERIES_STATE_BYTES` and `QUOTE_STATE_BYTES` in
/// `contracts/bounded-options/src/lib.rs` — and are over-attached on purpose: the
/// contract refunds the remainder.
const SERIES_RENT_BYTES: u128 = 344;
const QUOTE_RENT_BYTES: u128 = 88;
/// 10^19 yoctoNEAR per byte is the protocol's price on every network today.
/// The contract reads it from the runtime; the host cannot, so it hard-codes the
/// current value and over-attaches. A refund is a better failure than a refusal.
const YOCTO_PER_BYTE: u128 = 10_000_000_000_000_000_000;
/// How often the committee is asked for prices, on its own task. Fast enough
/// that a seat is never stale when a series comes due, slow enough that nobody
/// outside this machine is being polled to death.
const ORACLE_SECS: u64 = 8;
/// How long a registry read is good for. Long enough to keep a pass short, short
/// enough that a series that just settled is not treated as open for a minute.
const ROWS_TTL_SECS: u64 = 12;
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
    let count = operator::series_count(&stats).unwrap_or(0);
    let mut rows = Vec::with_capacity(count as usize);
    // The registry in the batches the contract allows. Every round trip here is a
    // pass that is not quoting a ladder, and a ladder nobody quoted is one the
    // trader cannot buy from — a week of markets turned this into hundreds of
    // reads a pass until it did.
    for start in (0..count).step_by(128) {
        let ids: Vec<u64> = (start..(start + 128).min(count)).collect();
        let batch: Vec<(u64, Option<serde_json::Value>)> = options
            .view("get_series_many")
            .args_json(json!({ "series_ids": ids }))
            .await?
            .json()?;
        rows.extend(batch.into_iter().map(|(id, row)| (id, row.unwrap_or(serde_json::Value::Null))));
    }
    Ok(rows)
}

/// The engine's plan against the chain's ledger, executed.
///
/// Only the *missing* series are sent, so a re-list is a few reads and no
/// transactions: series ids are assigned from zero and never reused, which makes
/// `get_series` over `0..series_count` a complete picture of what is listed.
/// `HH:MM UTC` for a nanosecond stamp, so a log line says which ladder it means.
fn stamp(ns: u64) -> String {
    let secs = (ns / 1_000_000_000) % 86_400;
    format!("{:02}:{:02} UTC", secs / 3600, (secs % 3600) / 60)
}

fn now_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// The expiries the engine lists, nearest first.
///
/// One ladder became seven: two inside the day and a daily close out to a week.
/// The operator lists and quotes every one of them, which is what makes the strip
/// on the screen a set of markets rather than a single one.
async fn engine_expiries(engine: &str) -> Result<Vec<u64>> {
    let health: serde_json::Value = serde_json::from_str(
        &operator::http_get(&format!("{}/health", engine.trim_end_matches('/'))).map_err(anyhow::Error::msg)?,
    )
    .context("engine /health is not JSON")?;
    if let Some(list) = health.get("expiries").and_then(serde_json::Value::as_array) {
        let mut out: Vec<u64> = list.iter().filter_map(serde_json::Value::as_u64).collect();
        out.sort_unstable();
        out.dedup();
        if !out.is_empty() {
            return Ok(out);
        }
    }
    // An older engine names one expiry; that is still a schedule of one.
    health
        .get("expiryNs")
        .and_then(serde_json::Value::as_u64)
        .map(|one| vec![one])
        .context("engine /health has no expiries")
}

/// Whether the chain already holds the ladder the engine is marking.
///
/// A roll that goes unlisted is a venue with marks and no counterparty: the
/// operator quotes only series whose expiry the engine named, so until the next
/// listing pass every cell on the screen is untradeable. An unreachable engine
/// answers "listed", because the cadence will retry it either way.
fn ladder_is_listed(rows: &[(u64, serde_json::Value)], expiries: &[u64]) -> bool {
    expiries.iter().all(|wanted| {
        rows.iter().any(|(_, row)| {
            row.get("settlement").and_then(serde_json::Value::as_str) == Some("Open")
                && row.get("expiry_ns").and_then(serde_json::Value::as_u64) == Some(*wanted)
        })
    })
}

async fn list_ladder(
    admin: &Account,
    options: &Contract,
    engine: &str,
    assets: &[String],
    rows: &[(u64, serde_json::Value)],
    expiries: &[u64],
    budget: usize,
) -> Result<(usize, usize)> {
    let engine = engine.trim_end_matches('/');

    let mut existing: Vec<operator::Listed> = rows
        .iter()
        .filter_map(|(_, row)| operator::Listed::from_view(row))
        .collect();

    let mut planned = 0usize;
    let mut created = 0usize;
    // A week of ladders is five hundred series, and a sandbox commits them about
    // four to the second. Listing them in one pass kept the venue dark for four
    // minutes, so each pass spends a budget and the next pass continues — the
    // nearest ladder is complete long before the furthest one starts.
    'ladders: for expiry_ns in expiries.iter().copied() {
        for asset in assets {
            let body = operator::http_get(&format!("{engine}/plan?asset={asset}&expiry_ns={expiry_ns}"))
                .map_err(anyhow::Error::msg)?;
            let plans = operator::parse_plan(&body)
                .map_err(|e| anyhow::anyhow!("plan for {asset}: {e}"))?;
            planned += plans.len();
            for plan in operator::missing(&plans, &existing) {
                if created >= budget {
                    break 'ladders;
                }
                admin
                    .call(options.id(), "create_series")
                    .args_json(plan.args())
                    .gas(GAS)
                    .deposit(NearToken::from_yoctonear(SERIES_RENT_BYTES * YOCTO_PER_BYTE))
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
    }
    Ok((created, planned))
}

/// Keep a person's offers resting on the cells nearest the money.
///
/// The vault is the counterparty to every click, which makes a book optional and
/// therefore empty. These are real offers from a real account with its own
/// escrowed collateral — a seeded maker, not organic flow, and one that can lose
/// money exactly like any other seller.
async fn run_maker(
    maker: &Account,
    options: &Contract,
    engine: &str,
    assets: &[String],
    rows: &[(u64, serde_json::Value)],
) -> Result<usize> {
    let mut acted = 0usize;
    for asset in assets {
        let body = match operator::http_get(&format!("{engine}/state?asset={asset}")) {
            Ok(body) => body,
            Err(error) => {
                println!("maker cannot read the {asset} ladder: {error}");
                continue;
            }
        };
        let (expiry_ns, plans) =
            operator::parse_state(&body).map_err(|error| anyhow::anyhow!("{asset} ladder: {error}"))?;
        let Some(index_units) = operator::parse_spot(&body) else { continue };
        let wants = maker::wants(index_units, &plans, rows, expiry_ns);
        if wants.is_empty() {
            continue;
        }
        let mut mine: Vec<(u64, u64, u128)> = Vec::new();
        for want in &wants {
            let book: serde_json::Value = options
                .view("get_book")
                .args_json(json!({ "series_id": want.series_id, "limit": 50 }))
                .await?
                .json()?;
            for level in book.as_array().map(|rows| rows.as_slice()).unwrap_or_default() {
                if level.get("maker").and_then(serde_json::Value::as_str) != Some(maker.id().as_str()) {
                    continue;
                }
                let offer_id = level.get("offer_id").and_then(serde_json::Value::as_u64).unwrap_or(0);
                let premium: u128 = level
                    .get("premium")
                    .and_then(serde_json::Value::as_str)
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(0);
                mine.push((offer_id, want.series_id, premium));
            }
        }
        // A demo maker reprices when the model has genuinely moved, not on every
        // tick: twenty basis points of drift is noise, and a cancel plus a post
        // every pass is two transactions of churn on somebody else's book.
        for action in maker::plan(&mine, &wants, 200) {
            match action {
                maker::Action::Cancel { offer_id } => {
                    maker
                        .call(options.id(), "cancel_offer")
                        .args_json(json!({ "offer_id": offer_id }))
                        .gas(GAS)
                        .transact()
                        .await?
                        .into_result()?;
                    acted += 1;
                }
                maker::Action::Post { series_id, premium_units } => {
                    let expires_ns = expiry_ns.saturating_sub(1_000_000_000);
                    maker
                        .call(options.id(), "post_offer")
                        .args_json(json!({
                            "series_id": series_id,
                            "quantity": (SCALE).to_string(),
                            "premium": premium_units.to_string(),
                            "taker": null,
                            "expires_ns": expires_ns,
                        }))
                        .gas(GAS)
                        .transact()
                        .await?
                        .into_result()?;
                    acted += 1;
                }
            }
        }
    }
    Ok(acted)
}

/// Put every seat's price on chain.
///
/// The host asks each source its own question — the venue's index, Kraken's book,
/// OKX's book, CoinGecko's aggregate — signs each answer with that seat's key, and
/// delivers the batch. Delivering is the host's entire contribution: a price it
/// made up cannot become a fix because it cannot sign for a seat, and a price one
/// real seat made up cannot become a fix either, because a fix needs a quorum.
///
/// Seats are asked on their own cadences. CoinGecko's free page is rate limited
/// and an aggregate that moves once a minute does not want polling every five
/// seconds; the venue's index is asked every pass because it is the only answer
/// carrying an exchange's own timestamp.
///
/// Returns how many prices the oracle accepted. It is the caller's job to keep the
/// venue's index from re-stamping the same print, since the oracle refuses an
/// observation that does not advance time.
async fn feed_committee(
    admin: &Account,
    oracle: &Contract,
    seats: &[oracle::Seat],
    last_print_ns: &mut std::collections::HashMap<String, u64>,
    complaints: &mut std::collections::HashMap<String, String>,
    waited: &mut [u64],
    tick_secs: u64,
) -> Result<usize> {
    let mut batch = Vec::new();
    for (index, seat) in seats.iter().enumerate() {
        waited[index] += tick_secs;
        if waited[index] < oracle_sources::every_secs(seat.name) {
            continue;
        }
        waited[index] = 0;
        for feed in &oracle_sources::FEEDS {
            let key = format!("{}|{}", seat.name, feed.id);
            // The seat fetches and signs; this loop only decides whether to deliver.
            // A feed that is down, or a source that will not answer, costs the
            // committee one vote rather than a settlement — and is said once per
            // distinct problem, because a feed that is down is down every pass.
            let observation = match seat.observe(feed.id) {
                Ok(observation) => observation,
                Err(error) => {
                    complain(
                        complaints,
                        &key,
                        format!("{} says nothing about {}: {error}", seat.name, feed.id),
                    );
                    continue;
                }
            };
            if last_print_ns
                .get(&key)
                .is_some_and(|last| observation.publish_time_ns <= *last)
            {
                // The same print again, or an older one. The oracle would refuse
                // it, and repeating it every eight seconds is noise.
                continue;
            }
            last_print_ns.insert(key, observation.publish_time_ns);
            batch.push(observation.item(seat.name, feed.id));
        }
    }
    if batch.is_empty() {
        return Ok(0);
    }
    let outcome = admin
        .call(oracle.id(), "submit_observations")
        .args_json(json!({ "observations": batch }))
        .gas(GAS)
        .transact()
        .await?
        .into_result()?
        .json::<serde_json::Value>()?;
    let accepted = outcome["accepted"].as_u64().unwrap_or(0) as usize;
    for rejected in outcome["rejected"].as_array().map(|v| v.clone()).unwrap_or_default() {
        println!("oracle refused a price: {}", rejected.as_str().unwrap_or("?"));
    }
    Ok(accepted)
}

/// Say a thing once, then stay quiet until it changes.
///
/// These loops run every few seconds for as long as the venue is up; a feed that
/// is down or a signer that refuses would otherwise fill the log with the same
/// sentence until nobody reads it.
fn complain(
    complaints: &mut std::collections::HashMap<String, String>,
    key: &str,
    message: String,
) {
    if complaints.get(key) == Some(&message) {
        return;
    }
    println!("{message}");
    complaints.insert(key.to_string(), message);
}

/// One simulated taker, buying and closing a contract now and then.
///
/// The venue's numbers only move when somebody trades, and a reviewer who arrives
/// to an empty tape reads that as "nothing works". This is one seeded account on
/// the same public path a person uses: `pool_buy` at whatever the vault published,
/// and `pool_close` when it wants out. It prints what it did, so the tape and the
/// log agree.
async fn run_activity(
    actor: &Account,
    options: &Contract,
    engine: &str,
    assets: &[String],
    rows: &[(u64, serde_json::Value)],
    asset_turn: usize,
) -> Result<Option<String>> {
    let asset = &assets[asset_turn % assets.len()];
    let body = match operator::http_get(&format!("{engine}/state?asset={asset}")) {
        Ok(body) => body,
        Err(_) => return Ok(None),
    };
    let (expiry_ns, plans) =
        operator::parse_state(&body).map_err(|error| anyhow::anyhow!("{asset} ladder: {error}"))?;
    let Some(index_units) = operator::parse_spot(&body) else { return Ok(None) };

    // Every other pass, take one back instead of buying: a tape where every print
    // is a buy is a tape nobody believes.
    if asset_turn % 2 == 1 {
        let held: Vec<serde_json::Value> = actor
            .view(options.id(), "get_positions")
            .args_json(json!({ "account": actor.id(), "limit": 50 }))
            .await?
            .json()?;
        let pool_ticket = held.iter().rev().find(|row| {
            row.get("settlement").and_then(serde_json::Value::as_str) == Some("Open")
                && row.get("side").and_then(serde_json::Value::as_str) == Some("long")
        });
        if let Some(row) = pool_ticket {
            let ticket_id = row.get("ticket_id").and_then(serde_json::Value::as_u64).unwrap_or(0);
            let label = row.get("series").and_then(serde_json::Value::as_str).unwrap_or("a ticket");
            actor
                .call(options.id(), "pool_close")
                .args_json(json!({ "ticket_id": ticket_id, "min_exit_units": null }))
                .gas(GAS)
                .transact()
                .await?
                .into_result()?;
            return Ok(Some(format!("closed {label}")));
        }
    }

    let Some(play) = activity::pick(index_units, &plans, rows, expiry_ns) else { return Ok(None) };
    let label = rows
        .iter()
        .find(|(id, _)| *id == play.series_id)
        .map(|(_, row)| {
            let kind = row.get("kind").and_then(serde_json::Value::as_str).unwrap_or("?");
            let low = row.get("low_strike").and_then(serde_json::Value::as_u64).unwrap_or(0);
            let high = row.get("high_strike").and_then(serde_json::Value::as_u64).unwrap_or(0);
            format!("{} {:.0}/{:.0}", kind, low as f64 / SCALE as f64, high as f64 / SCALE as f64)
        })
        .unwrap_or_else(|| format!("series {}", play.series_id));
    actor
        .call(options.id(), "pool_buy")
        .args_json(json!({
            "series_id": play.series_id,
            "quantity": SCALE.to_string(),
            "max_total_premium": play.budget_units.to_string(),
        }))
        .gas(GAS)
        .transact()
        .await?
        .into_result()?;
    Ok(Some(format!("bought 1 {label}")))
}

/// Ask the contract to fix every series whose expiry has passed and whose
/// acceptance window has not. The committee has to be talking already — a fix is
/// made of prices the oracle holds, not of prices requested here.
async fn request_fixes(
    admin: &Account,
    options: &Contract,
    rows: &[(u64, serde_json::Value)],
    now_ns: u64,
) -> Result<usize> {
    let mut asked = 0usize;
    for series_id in operator::due_for_fix(rows, now_ns) {
        admin
            .call(options.id(), "request_settlement")
            .args_json(json!({ "series_id": series_id }))
            .gas(GAS)
            .transact()
            .await?
            .into_result()?;
        asked += 1;
    }
    Ok(asked)
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
    expiry_ns: u64,
    live: &std::collections::HashMap<u64, operator::Standing>,
    now_ns: u64,
    renew_ns: u64,
) -> Result<(usize, usize)> {
    let engine = engine.trim_end_matches('/');
    let mut posted = 0usize;
    let mut priced = 0usize;
    for asset in assets {
        let body = operator::http_get(&format!("{engine}/state?asset={asset}&expiry_ns={expiry_ns}"))
            .map_err(anyhow::Error::msg)?;
        let (expiry, plans) =
            operator::parse_state(&body).map_err(|e| anyhow::anyhow!("state for {asset}: {e}"))?;
        priced += plans.iter().filter(|plan| plan.mid_units > 0).count();
        let needed = operator::quotes_needed(&plans, expiry, rows, live, now_ns, renew_ns);
        // A ladder is one write, not sixty: the batch is what lets the operator
        // refresh seven tenors inside the sixty-second age the contract allows.
        for chunk in needed.chunks(64) {
            let series_ids: Vec<u64> = chunk.iter().map(|(series_id, _)| *series_id).collect();
            let mid_units: Vec<String> = chunk.iter().map(|(_, mid)| mid.to_string()).collect();
            // Only the cells that are new cost rent; over-attaching the whole
            // chunk is fine because the contract refunds what it does not use.
            venue
                .call(options.id(), "set_pool_quotes")
                .args_json(json!({
                    "series_ids": series_ids,
                    "mid_units": mid_units,
                    "stamp_ns": now_ns,
                }))
                .gas(GAS)
                .deposit(NearToken::from_yoctonear(
                    chunk.len() as u128 * QUOTE_RENT_BYTES * YOCTO_PER_BYTE,
                ))
                .transact()
                .await?
                .into_result()?;
            posted += chunk.len();
        }
    }
    Ok((posted, priced))
}

/// The quotes standing on the series of one expiry.
///
/// Read per expiry rather than for the whole chain: with seven ladders that is a
/// seventh of the calls, and the map is only ever asked about one of them.
async fn read_standing(
    options: &Contract,
    rows: &[(u64, serde_json::Value)],
    expiry_ns: u64,
) -> Result<std::collections::HashMap<u64, operator::Standing>> {
    let mut live = std::collections::HashMap::new();
    for (series_id, row) in rows {
        if row.get("expiry_ns").and_then(serde_json::Value::as_u64) != Some(expiry_ns) {
            continue;
        }
        let quote = options
            .view("get_pool_quote")
            .args_json(json!({ "series_id": series_id }))
            .await?
            .json::<serde_json::Value>()?;
        if let Some(standing) = operator::standing_quote(&quote) {
            live.insert(*series_id, standing);
        }
    }
    Ok(live)
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
    if let Some(path) = optional("--testnet-session") { return testnet::run(&path).await; }
    let out = PathBuf::from(arg("--out", "data/chain-session.json"));
    // Read before the committee starts: the venue's own seat needs the engine's URL
    // to fetch its own index, and a seat is started before anything is listed.
    let engine = optional("--engine");
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
        .args_json(json!({ "owner": admin.id() }))
        .gas(GAS)
        .transact()
        .await?
        .into_result()?;
    // Seat the committee before anything can trade. Each seat's key is generated
    // inside the signer process that owns it — the relay that fetches prices can
    // ask for a signature and cannot make one, and venue.sh stops those
    // processes with the rest.
    //
    // If the deployment is on a public network the seats get a key directory, so
    // that stopping the host and starting it again re-seats the same committee
    // rather than a set of strangers the oracle has never heard of.
    let key_home = std::env::var("ORACLE_KEY_DIR").ok().filter(|dir| !dir.is_empty());
    let (seats, signers) = oracle::start_committee_with(
        Path::new("data/oracle"),
        engine.as_deref().unwrap_or_default(),
        key_home.as_deref().map(Path::new),
    )
    .map_err(|error| anyhow::anyhow!("starting the committee: {error}"))?;
    for seat in &seats {
        admin
            .call(oracle.id(), "add_source")
            .args_json(&seat.registration())
            .gas(GAS)
            .transact()
            .await?
            .into_result()?;
    }
    admin
        .call(oracle.id(), "set_quorum")
        .args_json(json!({ "quorum": 2 }))
        .gas(GAS)
        .transact()
        .await?
        .into_result()?;
    println!("seated {} sources in {} signer processes, quorum 2", seats.len(), signers.len());
    options
        .call("new")
        .args_json(json!({ "admin": admin.id(), "token": token.id(), "oracle": oracle.id() }))
        .gas(GAS)
        .transact()
        .await?
        .into_result()?;
    // The schedule the venue trades on. The contract's own default is the
    // prototype's 400/1000; a venue sets its real one here, in one place, and the
    // log records it so nobody has to read the code to find out what it charges.
    let margin_bps: u32 = arg("--margin-bps", &bounded_core::fees::Terms::VENUE.margin_bps.to_string())
        .parse()
        .context("--margin-bps must be a number")?;
    let impact_bps: u32 = arg("--impact-bps", &bounded_core::fees::Terms::VENUE.impact_bps.to_string())
        .parse()
        .context("--impact-bps must be a number")?;
    admin
        .call(options.id(), "set_fee_terms")
        .args_json(json!({ "margin_bps": margin_bps, "impact_bps": impact_bps }))
        .gas(GAS)
        .transact()
        .await?
        .into_result()?;
    println!(
        "fee schedule: {:.2}% over mid to open, {:.2}% under mid to close early",
        margin_bps as f64 / 100.0,
        impact_bps as f64 / 100.0
    );

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

    let list_secs: u64 = arg("--list-secs", "300").parse().context("--list-secs must be seconds")?;
    // A seeded maker is a demo convenience, not a business: it is a real account
    // with real escrowed collateral, and it can lose money like anyone else.
    let maker_role = optional("--maker");
    // One seeded taker on the same public path a person uses, so the tape and the
    // charts move while somebody is looking at them.
    let activity_role = optional("--activity");
    let maker_secs: u64 = arg("--maker-secs", "20").parse().context("--maker-secs must be seconds")?;
    let assets_list = assets();
    let mut listed = (0usize, 0usize);
    if let Some(url) = &engine {
        // The ladder before the ledger file is written, so the session records
        // what the chain actually holds. The engine is a second process that has
        // only just been told to start, so a first read that fails is retried
        // rather than taken for a broken venue: the host exiting here would leave
        // no session file at all, and the gateway would come up pointed at the
        // previous run's dead sandbox.
        let expiries = engine_expiries(url).await?;
        println!("engine lists {} expiries, nearest in {}h", expiries.len(), (expiries[0].saturating_sub(now_ns()) / 3_600_000_000_000));
        for attempt in 1.. {
            let current = chain_series(&options).await?;
            match list_ladder(&admin, &options, url, &assets_list, &current, &expiries, 200).await {
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
        // …and quoted, expiry by expiry, so the first visitor finds a vault that
        // is already selling rather than five seconds of "no price yet".
        let rows = chain_series(&options).await?;
        let now_ns = worker.view_block().await?.timestamp();
        // The nearest ladder only: the tick loop walks the rest, and a boot that
        // quotes a week of ladders is a boot nobody waits for.
        for expiry_ns in expiries.iter().copied().take(1) {
            match read_standing(&options, &rows, expiry_ns).await {
                Ok(live) => match quote_ladder(
                    &admin,
                    &options,
                    url,
                    &assets_list,
                    &rows,
                    expiry_ns,
                    &live,
                    now_ns,
                    operator::quote_age(expiry_ns, now_ns) / 3,
                )
                .await {
                    Ok((posted, priced)) => println!("quoted {posted} of {priced} priced cells at {}", stamp(expiry_ns)),
                    Err(error) => println!("quoting failed, chain still live: {error}"),
                },
                Err(error) => println!("quote read failed, chain still live: {error}"),
            }
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
            .deposit(NearToken::from_yoctonear(SERIES_RENT_BYTES * YOCTO_PER_BYTE))
            .gas(GAS)
            .transact()
            .await?
            .into_result()?
            .json()?
    };

    // Feed the committee before the screen opens. The first paint should show a
    // table with prices in it rather than four empty seats, and a settlement that
    // comes due minutes from now should not depend on this loop ever having run.
    if engine.is_some() {
        let mut printed = std::collections::HashMap::new();
        let mut complaints = std::collections::HashMap::new();
        let mut ready = vec![1_000u64; seats.len()];
        match feed_committee(&admin, &oracle, &seats, &mut printed, &mut complaints, &mut ready, 1)
            .await
        {
            Ok(accepted) => println!("committee opened with {accepted} signed prices"),
            Err(error) => println!("committee feed failed to start, chain still live: {error}"),
        }
        // The feed runs beside the operator loop rather than inside it. A pass
        // reads every series back from the chain and takes half a minute when the
        // venue is full; a seat that only speaks once per pass would then be
        // stale by the contract's own clock at the moment a ticket needs it.
        let (admin, oracle, seats) = (admin.clone(), oracle.clone(), seats.clone());
        tokio::spawn(async move {
            let mut printed = std::collections::HashMap::new();
            let mut complaints = std::collections::HashMap::new();
            let mut waited = vec![0u64; seats.len()];
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(ORACLE_SECS)).await;
                match feed_committee(
                    &admin,
                    &oracle,
                    &seats,
                    &mut printed,
                    &mut complaints,
                    &mut waited,
                    ORACLE_SECS,
                )
                .await
                {
                    Ok(_) => {}
                    Err(error) => println!("committee feed failed, chain still live: {error}"),
                }
            }
        });
    }

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
        // Cadences are measured on a real clock, not by counting `--tick-secs`:
        // a pass that reads the registry back takes as long as it takes, and a
        // counter that assumes four seconds made every interval a lie — the maker
        // fired every two minutes and the listing every ten.
        let mut last_list = std::time::Instant::now() - std::time::Duration::from_secs(list_secs);
        let mut last_quoted: std::collections::HashMap<u64, u64> = std::collections::HashMap::new();
        let maker_account = maker_role.as_ref().map(|role| {
            roles
                .iter()
                .find(|(name, _)| name == role)
                .map(|(_, account)| account.clone())
                .unwrap_or_else(|| panic!("--maker names a role this host does not fund: {role}"))
        });
        let mut last_maker = std::time::Instant::now() - std::time::Duration::from_secs(maker_secs);
        let activity_account = activity_role.as_ref().map(|role| {
            roles
                .iter()
                .find(|(name, _)| name == role)
                .map(|(_, account)| account.clone())
                .unwrap_or_else(|| panic!("--activity names a role this host does not fund: {role}"))
        });
        let mut activity_turn = 0usize;
        let mut last_activity = std::time::Instant::now();
        let mut rows = chain_series(&options).await?;
        let mut last_rows = std::time::Instant::now();
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(tick_secs)).await;
            // The registry is read on the listing cadence, not on every pass:
            // sixty series come back in a handful of batched calls, but the pass
            // has to be quicker than the contract's quote age or the ladder goes
            // stale between renewals. A row read seconds ago is the same row.
            let listing_due = last_rows.elapsed().as_secs() >= ROWS_TTL_SECS;
            if listing_due {
                match chain_series(&options).await {
                    Ok(fresh) => {
                        rows = fresh;
                        last_rows = std::time::Instant::now();
                    }
                    Err(error) => {
                        println!("read failed, chain still live: {error}");
                        continue;
                    }
                }
            }
            let expiries = match engine_expiries(url).await {
                Ok(list) => list,
                Err(error) => {
                    println!("engine schedule unreadable, chain still live: {error}");
                    continue;
                }
            };
            if last_list.elapsed().as_secs() >= list_secs || !ladder_is_listed(&rows, &expiries) {
                last_list = std::time::Instant::now();
                rows = chain_series(&options).await?;
                last_rows = std::time::Instant::now();
                match list_ladder(&admin, &options, url, &assets_list, &rows, &expiries, 200).await {
                    Ok((created, planned)) if created > 0 => println!("listed {created} of {planned} series"),
                    Ok((_, planned)) => println!("ladder unchanged · {planned} series planned"),
                    Err(error) => println!("listing failed, chain still live: {error}"),
                }
            }
            let now_ns = worker.view_block().await?.timestamp();
            // Quote the ladders whose own renewal deadlines have passed instead of
            // walking a fixed rotation. A pass takes as long as reading the
            // registry back, and a rotation that reached a ladder every few minutes
            // left the near ones — the ones a trader can actually click — past the
            // contract's quote age between visits, which the screen then reports as
            // a vault that will not sell.
            for expiry_ns in operator::ladders_due(&expiries, now_ns, &last_quoted, 8) {
                let renew_ns = operator::quote_age(expiry_ns, now_ns) / 3;
                last_quoted.insert(expiry_ns, now_ns);
                match read_standing(&options, &rows, expiry_ns).await {
                    Ok(live) => match quote_ladder(
                        &admin, &options, url, &assets_list, &rows, expiry_ns, &live, now_ns, renew_ns,
                    )
                    .await
                    {
                        // A still index re-posts nothing, so the line only prints
                        // when the venue actually said something new.
                        Ok((0, _)) => {}
                        Ok((posted, priced)) => {
                            println!("re-quoted {posted} of {priced} priced cells at {}", stamp(expiry_ns))
                        }
                        Err(error) => println!("quoting failed, chain still live: {error}"),
                    },
                    Err(error) => println!("quote read failed, chain still live: {error}"),
                }
            }
            if let Some(maker) = &maker_account {
                if last_maker.elapsed().as_secs() >= maker_secs {
                    last_maker = std::time::Instant::now();
                    match run_maker(maker, &options, url, &assets_list, &rows).await {
                        Ok(0) => {}
                        Ok(acted) => println!("maker quoted {acted} order(s)"),
                        Err(error) => println!("maker failed, chain still live: {error}"),
                    }
                }
            }
            if let Some(actor) = &activity_account {
                if last_activity.elapsed().as_secs() >= 18 {
                    last_activity = std::time::Instant::now();
                    activity_turn += 1;
                    match run_activity(actor, &options, url, &assets_list, &rows, activity_turn).await {
                        Ok(Some(done)) => println!("activity: {done}"),
                        Ok(None) => {}
                        Err(error) => println!("activity failed, chain still live: {error}"),
                    }
                }
            }
            match request_fixes(&admin, &options, &rows, now_ns).await {
                Ok(asked) if asked > 0 => println!("asked the oracle to fix {asked} expired series"),
                Ok(_) => {}
                Err(error) => println!("fixing failed, chain still live: {error}"),
            }
            // A series whose window closed unfixed can never pay: voiding it is
            // what returns the premium instead of leaving the ticket open for
            // the life of the contract.
            for series_id in operator::voidable(&rows, now_ns) {
                // Both failure shapes — a transport error and a reverted receipt —
                // are reported the same way, so one stuck series cannot end the tick.
                let outcome = admin
                    .call(options.id(), "void_series")
                    .args_json(json!({ "series_id": series_id }))
                    .gas(GAS)
                    .transact()
                    .await
                    .map_err(|error| error.to_string())
                    .and_then(|sent| sent.into_result().map_err(|error| format!("{error:?}")));
                match outcome {
                    Ok(_) => println!("voided {series_id}: no print inside the accept window"),
                    Err(error) => println!("void failed for {series_id}: {error}"),
                }
            }
        }
    }
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
    }
}
