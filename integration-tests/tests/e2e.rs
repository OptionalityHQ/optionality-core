use anyhow::{anyhow, Result};
use near_workspaces::types::{Gas, NearToken};
use near_workspaces::{Account, AccountId, Contract, Worker};
use serde_json::{json, Value};

const SCALE: u128 = 1_000_000;
const DEPOSIT: &str = "100000000"; // 100.000000

const OPTIONS_WASM: &[u8] =
    include_bytes!("../../target/near/bounded_options/bounded_options.wasm");
const TOKEN_WASM: &[u8] = include_bytes!("../../target/near/mock_ft/mock_ft.wasm");
const ORACLE_WASM: &[u8] =
    include_bytes!("../../target/near/price_oracle/price_oracle.wasm");

/// A seat at the committee and the key that speaks for it. The tests keep two:
/// a quorum of two is the smallest rule where one honest price is not yet a
/// settlement, which is the whole point of the oracle.
struct Seat {
    name: &'static str,
    key: ed25519_dalek::SigningKey,
}

/// The same four names the operator seats, minus the ones no test needs. Our own
/// index is at the table so a test can ask the exact question that matters: what
/// happens when only it speaks, and what happens when only the others do.
fn seats() -> Vec<Seat> {
    ["venue-index", "kraken", "okx"]
        .iter()
        .enumerate()
        .map(|(index, name)| Seat {
            name,
            key: ed25519_dalek::SigningKey::from_bytes(&[index as u8 + 3; 32]),
        })
        .collect()
}

fn seat_of<'a>(seats: &'a [Seat], name: &str) -> &'a Seat {
    seats
        .iter()
        .find(|seat| seat.name == name)
        .expect("the test committee seats this source")
}

struct Env {
    worker: Worker<near_workspaces::network::Sandbox>,
    options: Contract,
    token: Contract,
    oracle: Contract,
    seats: Vec<Seat>,
    admin: Account,
    alice: Account,
    bob: Account,
    carol: Account,
}

async fn deposit(
    account: &Account,
    token: &Contract,
    receiver: &AccountId,
    amount: u128,
) -> Result<()> {
    account
        .call(token.id(), "ft_transfer_call")
        .args_json(json!({
            "receiver_id": receiver,
            "amount": amount.to_string(),
            "msg": "deposit"
        }))
        .deposit(NearToken::from_yoctonear(1))
        .gas(Gas::from_tgas(80))
        .transact()
        .await?
        .into_result()?;
    Ok(())
}

async fn register(account: &Account, token: &Contract, target: Option<AccountId>) -> Result<()> {
    account
        .call(token.id(), "storage_deposit")
        .args_json(json!({ "account_id": target }))
        .deposit(NearToken::from_millinear(100))
        .transact()
        .await?
        .into_result()?;
    Ok(())
}

async fn setup() -> Result<Env> {
    let worker = near_workspaces::sandbox().await?;
    let options = worker.dev_deploy(OPTIONS_WASM).await?;
    let token = worker.dev_deploy(TOKEN_WASM).await?;
    let oracle = worker.dev_deploy(ORACLE_WASM).await?;

    let admin = worker.dev_create_account().await?;
    let alice = worker.dev_create_account().await?;
    let bob = worker.dev_create_account().await?;
    let carol = worker.dev_create_account().await?;

    token
        .call("new")
        .args_json(json!({ "owner": admin.id() }))
        .transact()
        .await?
        .into_result()?;
    let seats = seats();
    oracle
        .call("new")
        .args_json(json!({ "owner": admin.id() }))
        .transact()
        .await?
        .into_result()?;
    for seat in &seats {
        admin
            .call(oracle.id(), "add_source")
            .args_json(json!({
                "name": seat.name,
                "public_key": seat.key.verifying_key().to_bytes().to_vec(),
            }))
            .gas(Gas::from_tgas(50))
            .transact()
            .await?
            .into_result()?;
    }
    options
        .call("new")
        .args_json(json!({
            "admin": admin.id(),
            "token": token.id(),
            "oracle": oracle.id()
        }))
        .transact()
        .await?
        .into_result()?;

    for account in [&admin, &alice, &bob] {
        register(account, &token, None).await?;
    }
    register(&admin, &token, Some(options.id().clone())).await?;

    for account in [&alice, &bob] {
        admin
            .call(token.id(), "mint")
            .args_json(json!({
                "account_id": account.id(),
                "amount": DEPOSIT
            }))
            .transact()
            .await?
            .into_result()?;
    }

    for account in [&alice, &bob] {
        deposit(account, &token, options.id(), DEPOSIT.parse()?).await?;
    }
    worker.fast_forward(3).await?;

    Ok(Env {
        worker,
        options,
        token,
        oracle,
        seats,
        admin,
        alice,
        bob,
        carol,
    })
}

impl Env {
    async fn advance_past(&self, timestamp_ns: u64) -> Result<()> {
        for _ in 0..40 {
            if self.worker.view_block().await?.timestamp() >= timestamp_ns {
                return Ok(());
            }
            self.worker.fast_forward(10).await?;
        }
        Err(anyhow!("could not advance sandbox time"))
    }

    async fn balance(&self, account: &Account) -> Result<u128> {
        let value: String = self
            .options
            .view("get_balance")
            .args_json(json!({ "account_id": account.id() }))
            .await?
            .json()?;
        Ok(value.parse()?)
    }

    async fn token_balance(&self, account: &Account) -> Result<u128> {
        let value: String = self
            .token
            .view("ft_balance_of")
            .args_json(json!({ "account_id": account.id() }))
            .await?
            .json()?;
        Ok(value.parse()?)
    }

    async fn custody(&self) -> Result<(u128, u128)> {
        let expected: String = self.options.view("expected_custody").await?.json()?;
        let token: String = self
            .token
            .view("ft_balance_of")
            .args_json(json!({ "account_id": self.options.id() }))
            .await?
            .json()?;
        Ok((expected.parse()?, token.parse()?))
    }

    /// Put prices on the oracle the way real sources do: one signature per seat,
    /// and nothing a relay can forge. A single price is a candidate; a quorum of
    /// them agreeing inside the tolerance is what becomes a fix.
    async fn report(&self, feed: &str, spokes: &[(&str, u128)]) -> Result<Value> {
        let now = self.worker.view_block().await?.timestamp();
        let stamp = now.saturating_sub(1_000_000_000);
        let observations: Vec<Value> = spokes
            .iter()
            .enumerate()
            .map(|(index, (name, price))| {
                let seat = seat_of(&self.seats, name);
                // One nanosecond apart so each seat's own history still advances.
                let publish_time_ns = stamp + index as u64;
                let message = bounded_core::oracle::signing_message(
                    feed,
                    seat.name,
                    *price,
                    publish_time_ns,
                    9_000,
                );
                let signature = ed25519_dalek::Signer::sign(&seat.key, &message).to_bytes();
                json!({
                    "feed_id": feed,
                    "source": seat.name,
                    "price": price,
                    "publish_time_ns": publish_time_ns,
                    "confidence_bps": 9_000,
                    "signature": signature.to_vec(),
                })
            })
            .collect();
        self.admin
            .call(self.oracle.id(), "submit_observations")
            .args_json(json!({ "observations": observations }))
            .gas(Gas::from_tgas(150))
            .transact()
            .await?
            .into_result()?
            .json::<Value>()
            .map_err(Into::into)
    }

    /// The committee in agreement, and specifically *not* including our own seat:
    /// two independent sources signing the same number is the price every
    /// settlement test below is paid on.
    async fn fix(&self, feed: &str, price: u128) -> Result<()> {
        let outcome = self
            .report(feed, &[("kraken", price), ("okx", price)])
            .await?;
        if outcome["fixed"].as_array().map(|rows| rows.is_empty()) != Some(false) {
            return Err(anyhow!("the committee did not fix {feed}: {outcome}"));
        }
        Ok(())
    }

    /// Ask the contract what a series settled at, and wait for the oracle's
    /// answer to come back as a callback.
    async fn settle(&self, series_id: u64) -> Result<Value> {
        self.admin
            .call(self.options.id(), "request_settlement")
            .args_json(json!({ "series_id": series_id }))
            .gas(Gas::from_tgas(100))
            .transact()
            .await?
            .into_result()?;
        self.worker.fast_forward(5).await?;
        self.series(series_id).await
    }

    async fn series(&self, id: u64) -> Result<Value> {
        let value: Option<Value> = self
            .options
            .view("get_series")
            .args_json(json!({ "series_id": id }))
            .await?
            .json()?;
        value.ok_or_else(|| anyhow!("missing series"))
    }

    /// Onboard an account that `setup()` leaves unfunded, so a test can add a
    /// third maker without changing the custody baseline of other tests.
    async fn fund_account(&self, account: &Account, amount: u128) -> Result<()> {
        register(account, &self.token, None).await?;
        self.admin
            .call(self.token.id(), "mint")
            .args_json(json!({ "account_id": account.id(), "amount": amount.to_string() }))
            .transact()
            .await?
            .into_result()?;
        deposit(account, &self.token, self.options.id(), amount).await?;
        self.worker.fast_forward(3).await?;
        Ok(())
    }

    async fn post_offer(
        &self,
        maker: &Account,
        series_id: u64,
        quantity: u128,
        premium: u128,
        taker: Option<&Account>,
    ) -> Result<u64> {
        let series = self.series(series_id).await?;
        let expires_ns = series["expiry_ns"].as_u64().expect("expiry") - 1_000_000_000;
        let id: u64 = maker
            .call(self.options.id(), "post_offer")
            .args_json(json!({
                "series_id": series_id,
                "quantity": quantity.to_string(),
                "premium": premium.to_string(),
                "taker": taker.map(|account| account.id()),
                "expires_ns": expires_ns
            }))
            .gas(Gas::from_tgas(50))
            .transact()
            .await?
            .into_result()?
            .json()?;
        Ok(id)
    }

    async fn book(&self, series_id: u64) -> Result<Vec<Value>> {
        Ok(self
            .options
            .view("get_book")
            .args_json(json!({ "series_id": series_id, "limit": 50 }))
            .await?
            .json()?)
    }
}

async fn create_series(env: &Env, expiry_offset_ns: u64, finalize_offset_ns: u64) -> Result<u64> {
    create_series_with_floor(env, expiry_offset_ns, finalize_offset_ns, 5_000).await
}

/// `min_confidence_bps` is the series' own demand of the oracle: how sure the
/// committee has to be before this particular ticket pays out.
async fn create_series_with_floor(
    env: &Env,
    expiry_offset_ns: u64,
    finalize_offset_ns: u64,
    min_confidence_bps: u16,
) -> Result<u64> {
    let now = env.worker.view_block().await?.timestamp();
    let expiry = now + expiry_offset_ns;
    let id: u64 = env
        .admin
        .call(env.options.id(), "create_series")
        .args_json(json!({
            "underlying": "ETH",
            "kind": "Put",
            "expiry_ns": expiry,
            "accept_until_ns": expiry + 60_000_000_000u64,
            "finalize_until_ns": expiry + finalize_offset_ns,
            "low_strike": (100u128 * SCALE).to_string(),
            "high_strike": (110u128 * SCALE).to_string(),
            "price_scale": SCALE.to_string(),
            "min_confidence_bps": min_confidence_bps,
            "oracle_feed": "ETH-USD"
        }))
        .transact()
        .await?
        .into_result()?
        .json()?;
    Ok(id)
}

async fn post_and_fill(env: &Env, series_id: u64) -> Result<u64> {
    let now = env.worker.view_block().await?.timestamp();
    let series = env.series(series_id).await?;
    let series_expiry = series["expiry_ns"].as_u64().expect("expiry");
    let offer_expiry = (series_expiry - 1_000_000_000).max(now + 1_000_000_000);
    let offer_id: u64 = env
        .bob
        .call(env.options.id(), "post_offer")
        .args_json(json!({
            "series_id": series_id,
            "quantity": SCALE.to_string(),
            "premium": (3u128 * SCALE).to_string(),
            "taker": null,
            "expires_ns": offer_expiry
        }))
        .transact()
        .await?
        .into_result()?
        .json()?;
    let ticket_id: u64 = env
        .alice
        .call(env.options.id(), "fill_offer")
        .args_json(json!({ "offer_id": offer_id }))
        .transact()
        .await?
        .into_result()?
        .json()?;
    Ok(ticket_id)
}

async fn sweep(env: &Env, taker: &Account, series_id: u64) -> Result<Value> {
    Ok(taker
        .call(env.options.id(), "fill_market_order")
        .args_json(json!({
            "series_id": series_id,
            "quantity": (2u128 * SCALE).to_string(),
            "max_total_premium": (20u128 * SCALE).to_string(),
            "min_quantity": null
        }))
        .gas(Gas::from_tgas(150))
        .transact()
        .await?
        .into_result()?
        .json()?)
}

#[tokio::test]
async fn full_lifecycle_settles_and_pays_out() -> Result<()> {
    let env = setup().await?;
    let series_id = create_series(&env, 8_000_000_000, 120_000_000_000).await?;
    let ticket_id = post_and_fill(&env, series_id).await?;
    let offer: Option<Value> = env
        .options
        .view("get_offer")
        .args_json(json!({ "offer_id": 0 }))
        .await?
        .json()?;
    assert!(offer.is_none(), "filled offer record should be removed");

    assert_eq!(env.balance(&env.alice).await?, 97 * SCALE);
    assert_eq!(env.balance(&env.bob).await?, 90 * SCALE);
    let (expected, actual) = env.custody().await?;
    assert_eq!(expected, 200 * SCALE);
    assert_eq!(actual, expected, "token custody must match ledger");

    env.advance_past(env.worker.view_block().await?.timestamp() + 15_000_000_000)
        .await?;
    env.fix("ETH-USD", 105u128 * SCALE).await?;

    env.admin
        .call(env.options.id(), "request_settlement")
        .args_json(json!({ "series_id": series_id }))
        .gas(Gas::from_tgas(100))
        .transact()
        .await?
        .into_result()?;
    env.worker.fast_forward(5).await?;

    let series = env.series(series_id).await?;
    assert_eq!(series["settlement"]["Fixed"]["price"].as_u64(), Some(105_000_000));
    // The venue's own index never spoke in this test, and the ticket still paid
    // out on schedule: the engine quotes the market and is not what settles it.
    assert_eq!(
        series["settle_sources"].as_array().map(|rows| rows.len()),
        Some(2),
        "two third-party seats priced this settlement"
    );

    env.admin
        .call(env.options.id(), "settle_ticket")
        .args_json(json!({ "ticket_id": ticket_id }))
        .transact()
        .await?
        .into_result()?;

    let ticket: Option<Value> = env
        .options
        .view("get_ticket")
        .args_json(json!({ "ticket_id": ticket_id }))
        .await?
        .json()?;
    assert!(ticket.is_none(), "settled ticket record should be removed");

    assert_eq!(env.balance(&env.alice).await?, 102 * SCALE);
    assert_eq!(env.balance(&env.bob).await?, 98 * SCALE);
    let (expected, actual) = env.custody().await?;
    assert_eq!(expected, 200 * SCALE);
    assert_eq!(actual, expected);

    env.alice
        .call(env.options.id(), "withdraw")
        .args_json(json!({ "amount": (50u128 * SCALE).to_string() }))
        .gas(Gas::from_tgas(100))
        .transact()
        .await?
        .into_result()?;
    let mut delivered = 0;
    for _ in 0..20 {
        env.worker.fast_forward(2).await?;
        delivered = env.token_balance(&env.alice).await?;
        if delivered == 50 * SCALE {
            break;
        }
    }
    assert_eq!(delivered, 50 * SCALE);
    let stats: Value = env.options.view("stats").await?.json()?;
    assert_eq!(stats["pending_withdrawal_count"].as_u64(), Some(0));
    let pending: Vec<u64> = env
        .options
        .view("get_owner_withdrawals")
        .args_json(json!({ "owner": env.alice.id() }))
        .await?
        .json()?;
    assert!(pending.is_empty());
    let (expected, actual) = env.custody().await?;
    assert_eq!(expected, 150 * SCALE);
    assert_eq!(actual, expected);
    Ok(())
}

#[tokio::test]
async fn duplicate_fill_is_rejected() -> Result<()> {
    let env = setup().await?;
    let series_id = create_series(&env, 30_000_000_000, 120_000_000_000).await?;
    let ticket_id = post_and_fill(&env, series_id).await?;
    assert!(ticket_id == 0);
    let second = env
        .alice
        .call(env.options.id(), "fill_offer")
        .args_json(json!({ "offer_id": 0 }))
        .transact()
        .await?
        .into_result();
    assert!(second.is_err(), "a filled offer must not fill twice");
    assert_eq!(env.balance(&env.alice).await?, 97 * SCALE);
    Ok(())
}

#[tokio::test]
async fn outage_voids_then_refunds_premium() -> Result<()> {
    let env = setup().await?;
    let series_id = create_series(&env, 5_000_000_000, 90_000_000_000).await?;
    let ticket_id = post_and_fill(&env, series_id).await?;

    env.advance_past(env.worker.view_block().await?.timestamp() + 120_000_000_000)
        .await?;
    env.admin
        .call(env.options.id(), "void_series")
        .args_json(json!({ "series_id": series_id }))
        .transact()
        .await?
        .into_result()?;
    env.admin
        .call(env.options.id(), "settle_ticket")
        .args_json(json!({ "ticket_id": ticket_id }))
        .transact()
        .await?
        .into_result()?;

    assert_eq!(env.balance(&env.alice).await?, 100 * SCALE);
    assert_eq!(env.balance(&env.bob).await?, 100 * SCALE);
    let (expected, actual) = env.custody().await?;
    assert_eq!(expected, 200 * SCALE);
    assert_eq!(actual, expected);
    Ok(())
}

#[tokio::test]
async fn wrong_deposit_message_is_refunded() -> Result<()> {
    let env = setup().await?;
    let (before_expected, before_actual) = env.custody().await?;

    env.admin
        .call(env.token.id(), "mint")
        .args_json(json!({ "account_id": env.alice.id(), "amount": (10u128 * SCALE).to_string() }))
        .transact()
        .await?
        .into_result()?;
    env.alice
        .call(env.token.id(), "ft_transfer_call")
        .args_json(json!({
            "receiver_id": env.options.id(),
            "amount": (10u128 * SCALE).to_string(),
            "msg": "not-a-deposit"
        }))
        .deposit(NearToken::from_yoctonear(1))
        .gas(Gas::from_tgas(80))
        .transact()
        .await?
        .into_result()?;
    env.worker.fast_forward(5).await?;

    assert_eq!(env.token_balance(&env.alice).await?, 10 * SCALE);
    assert_eq!(env.balance(&env.alice).await?, 100 * SCALE);
    let (expected, actual) = env.custody().await?;
    assert_eq!(expected, before_expected);
    assert_eq!(actual, before_actual);
    Ok(())
}

#[tokio::test]
async fn market_order_sweeps_the_book_and_settles_every_ticket() -> Result<()> {
    let env = setup().await?;
    env.fund_account(&env.carol, 100 * SCALE).await?;
    let series_id = create_series(&env, 30_000_000_000, 120_000_000_000).await?;

    // Bob quotes one unit at 2.00, Carol two units at a dearer 2.50 per unit.
    let bob_offer = env
        .post_offer(&env.bob, series_id, SCALE, 2 * SCALE, None)
        .await?;
    let carol_offer = env
        .post_offer(&env.carol, series_id, 2 * SCALE, 5 * SCALE, None)
        .await?;
    let book = env.book(series_id).await?;
    assert_eq!(
        book.iter()
            .map(|offer| offer["offer_id"].as_u64())
            .collect::<Vec<_>>(),
        vec![Some(bob_offer), Some(carol_offer)],
        "the book must be sorted by unit premium"
    );

    // Asking for more than the book holds fails, and changes nothing.
    let too_much = env
        .alice
        .call(env.options.id(), "fill_market_order")
        .args_json(json!({
            "series_id": series_id,
            "quantity": (5u128 * SCALE).to_string(),
            "max_total_premium": (50u128 * SCALE).to_string(),
            "min_quantity": (5u128 * SCALE).to_string()
        }))
        .gas(Gas::from_tgas(150))
        .transact()
        .await?
        .into_result();
    assert!(too_much.is_err(), "minimum fill above book depth must revert");
    assert_eq!(env.balance(&env.alice).await?, 100 * SCALE);
    assert_eq!(env.book(series_id).await?.len(), 2);

    let fill: Value = env
        .alice
        .call(env.options.id(), "fill_market_order")
        .args_json(json!({
            "series_id": series_id,
            "quantity": (3u128 * SCALE).to_string(),
            "max_total_premium": (9u128 * SCALE).to_string(),
            "min_quantity": (3u128 * SCALE).to_string()
        }))
        .gas(Gas::from_tgas(150))
        .transact()
        .await?
        .into_result()?
        .json()?;
    assert_eq!(fill["filled_quantity"], "3000000");
    assert_eq!(fill["total_premium"], "7000000");
    assert_eq!(fill["unfilled_quantity"], "0");
    assert_eq!(fill["worst_unit_premium"], "2500000");
    assert_eq!(fill["tickets"], json!([0, 1]));
    assert!(env.book(series_id).await?.is_empty());

    assert_eq!(env.balance(&env.alice).await?, 93 * SCALE);
    assert_eq!(env.balance(&env.bob).await?, 90 * SCALE);
    assert_eq!(env.balance(&env.carol).await?, 80 * SCALE);
    let (expected, actual) = env.custody().await?;
    assert_eq!(expected, 300 * SCALE);
    assert_eq!(actual, expected, "sweeping must not create or destroy value");

    env.advance_past(env.worker.view_block().await?.timestamp() + 40_000_000_000)
        .await?;
    env.fix("ETH-USD", 105u128 * SCALE).await?;
    env.admin
        .call(env.options.id(), "request_settlement")
        .args_json(json!({ "series_id": series_id }))
        .gas(Gas::from_tgas(100))
        .transact()
        .await?
        .into_result()?;
    env.worker.fast_forward(5).await?;

    for ticket_id in [0u64, 1] {
        env.admin
            .call(env.options.id(), "settle_ticket")
            .args_json(json!({ "ticket_id": ticket_id }))
            .transact()
            .await?
            .into_result()?;
    }

    // Price 105 in a 100/110 spread: 0.50 of collateral per unit returns.
    assert_eq!(env.balance(&env.alice).await?, 108 * SCALE);
    assert_eq!(env.balance(&env.bob).await?, 97 * SCALE);
    assert_eq!(env.balance(&env.carol).await?, 95 * SCALE);
    let stats: Value = env.options.view("stats").await?.json()?;
    assert_eq!(stats["open_ticket_count"].as_u64(), Some(0));
    assert_eq!(stats["open_offer_count"].as_u64(), Some(0));
    let (expected, actual) = env.custody().await?;
    assert_eq!(expected, 300 * SCALE);
    assert_eq!(actual, expected);
    Ok(())
}

#[tokio::test]
async fn market_order_respects_a_premium_budget_and_partial_fills() -> Result<()> {
    let env = setup().await?;
    env.fund_account(&env.carol, 100 * SCALE).await?;
    let series_id = create_series(&env, 30_000_000_000, 120_000_000_000).await?;
    env.post_offer(&env.bob, series_id, SCALE, 2 * SCALE, None)
        .await?;
    let dear = env
        .post_offer(&env.carol, series_id, SCALE, 9 * SCALE, None)
        .await?;

    // Budget of 5.00 buys the 2.00 offer only; the 9.00 one must survive.
    let fill: Value = env
        .alice
        .call(env.options.id(), "fill_market_order")
        .args_json(json!({
            "series_id": series_id,
            "quantity": (2u128 * SCALE).to_string(),
            "max_total_premium": (5u128 * SCALE).to_string(),
            "min_quantity": null
        }))
        .gas(Gas::from_tgas(150))
        .transact()
        .await?
        .into_result()?
        .json()?;
    assert_eq!(fill["filled_quantity"], "1000000");
    assert_eq!(fill["total_premium"], "2000000");
    assert_eq!(fill["unfilled_quantity"], "1000000");
    let book = env.book(series_id).await?;
    assert_eq!(book.len(), 1);
    assert_eq!(book[0]["offer_id"].as_u64(), Some(dear));
    assert_eq!(env.balance(&env.alice).await?, 98 * SCALE);
    let (expected, actual) = env.custody().await?;
    assert_eq!(expected, 300 * SCALE);
    assert_eq!(actual, expected);
    Ok(())
}

#[tokio::test]
async fn market_order_cannot_take_a_reserved_or_self_crossed_offer() -> Result<()> {
    let env = setup().await?;
    env.fund_account(&env.carol, 100 * SCALE).await?;
    let series_id = create_series(&env, 30_000_000_000, 120_000_000_000).await?;
    // Bob reserves one offer for Carol and posts one for anyone.
    let reserved = env
        .post_offer(&env.bob, series_id, SCALE, SCALE, Some(&env.carol))
        .await?;
    env.post_offer(&env.bob, series_id, SCALE, 4 * SCALE, None)
        .await?;

    let fill = sweep(&env, &env.alice, series_id).await?;
    assert_eq!(
        fill["total_premium"], "4000000",
        "only the unreserved offer may be swept"
    );
    let book = env.book(series_id).await?;
    assert_eq!(book.len(), 1);
    assert_eq!(book[0]["offer_id"].as_u64(), Some(reserved));

    // The only quote left is Bob's own, so sweeping must revert, not fill.
    assert!(
        sweep(&env, &env.bob, series_id).await.is_err(),
        "a maker must not sweep their own quote"
    );
    assert_eq!(env.book(series_id).await?.len(), 1);

    // Carol is the reserved taker, so she may take it.
    sweep(&env, &env.carol, series_id).await?;
    assert_eq!(env.book(series_id).await?.len(), 0);
    assert_eq!(env.balance(&env.carol).await?, 99 * SCALE);
    let (expected, actual) = env.custody().await?;
    assert_eq!(expected, 300 * SCALE);
    assert_eq!(actual, expected);
    Ok(())
}

#[tokio::test]
async fn prune_offers_frees_the_book_without_releasing_collateral() -> Result<()> {
    let env = setup().await?;
    let series_id = create_series(&env, 60_000_000_000, 300_000_000_000).await?;
    let now = env.worker.view_block().await?.timestamp();
    let series = env.series(series_id).await?;
    let expiry = series["expiry_ns"].as_u64().expect("expiry");

    let live: u64 = env
        .bob
        .call(env.options.id(), "post_offer")
        .args_json(json!({
            "series_id": series_id,
            "quantity": SCALE.to_string(),
            "premium": SCALE.to_string(),
            "taker": null,
            "expires_ns": expiry - 1_000_000_000
        }))
        .gas(Gas::from_tgas(50))
        .transact()
        .await?
        .into_result()?
        .json()?;
    let stale: u64 = env
        .bob
        .call(env.options.id(), "post_offer")
        .args_json(json!({
            "series_id": series_id,
            "quantity": SCALE.to_string(),
            "premium": SCALE.to_string(),
            "taker": null,
            "expires_ns": now + 2_000_000_000
        }))
        .gas(Gas::from_tgas(50))
        .transact()
        .await?
        .into_result()?
        .json()?;
    assert_eq!(env.book(series_id).await?.len(), 2);
    let bob_before = env.balance(&env.bob).await?;

    env.advance_past(now + 5_000_000_000).await?;
    // The view already hides the expired quote, but it still occupies the index.
    let visible = env.book(series_id).await?;
    assert_eq!(visible.len(), 1);
    assert_eq!(visible[0]["offer_id"].as_u64(), Some(live));
    let stats: Value = env.options.view("stats").await?.json()?;
    assert_eq!(stats["open_offer_count"].as_u64(), Some(2));

    let pruned: u64 = env
        .alice
        .call(env.options.id(), "prune_offers")
        .args_json(json!({ "series_id": series_id, "limit": 50 }))
        .gas(Gas::from_tgas(50))
        .transact()
        .await?
        .into_result()?
        .json()?;
    assert_eq!(pruned, 1, "a non-maker may prune the matching index");
    assert_eq!(env.book(series_id).await?.len(), 1);
    let stats: Value = env.options.view("stats").await?.json()?;
    assert_eq!(
        stats["open_offer_count"].as_u64(),
        Some(2),
        "pruning must not release escrow or drop the record"
    );
    assert_eq!(env.balance(&env.bob).await?, bob_before);
    assert!(env
        .options
        .view("get_offer")
        .args_json(json!({ "offer_id": stale }))
        .await?
        .json::<Option<Value>>()?
        .is_some());

    env.bob
        .call(env.options.id(), "cancel_offer")
        .args_json(json!({ "offer_id": stale }))
        .gas(Gas::from_tgas(50))
        .transact()
        .await?
        .into_result()?;
    assert_eq!(env.balance(&env.bob).await?, bob_before + 10 * SCALE);
    let (expected, actual) = env.custody().await?;
    assert_eq!(expected, 200 * SCALE);
    assert_eq!(actual, expected);
    Ok(())
}

#[tokio::test]
async fn one_source_alone_cannot_settle_a_ticket() -> Result<()> {
    // The property the committee exists for. Every other settlement test below has
    // two signatures behind its price; this one hands the chain a single honest
    // price — which is exactly what the venue used to settle on — and gets refused.
    let env = setup().await?;
    let series_id = create_series(&env, 8_000_000_000, 120_000_000_000).await?;
    post_and_fill(&env, series_id).await?;
    env.advance_past(env.worker.view_block().await?.timestamp() + 15_000_000_000)
        .await?;

    env.report("ETH-USD", &[("venue-index", 105u128 * SCALE)]).await?;
    let committee: Value = env
        .oracle
        .view("get_committee")
        .args_json(json!({ "feed_id": "ETH-USD" }))
        .await?
        .json()?;
    assert_eq!(committee["fresh_sources"].as_u64(), Some(1));
    assert_eq!(committee["can_fix"], json!(false), "one seat is not a quorum");

    let series = env.settle(series_id).await?;
    assert_eq!(
        series["settlement"], json!("Open"),
        "the venue's own print, alone, cannot pay a holder out"
    );

    // Two other seats name a number, and the same series settles — while our own
    // seat, still fresh and still outvoted, is not in the price that pays. The
    // committee makes a settlement, not the messenger.
    env.worker.fast_forward(3).await?;
    env.report("ETH-USD", &[("kraken", 108u128 * SCALE), ("okx", 108u128 * SCALE)]).await?;
    let series = env.settle(series_id).await?;
    assert_eq!(
        series["settlement"]["Fixed"]["price"].as_u64(),
        Some(108_000_000),
        "the two third-party seats priced it, not the one that was alone earlier"
    );
    let sources: Vec<String> = series["settle_sources"]
        .as_array()
        .expect("a fix names its sources")
        .iter()
        .map(|source| source.as_str().expect("a source name").to_string())
        .collect();
    assert_eq!(sources, vec!["kraken".to_string(), "okx".to_string()]);
    assert_eq!(
        series["settle_dispersion_bps"].as_u64(),
        Some(0),
        "two seats that said the same thing are a certain price"
    );
    Ok(())
}

#[tokio::test]
async fn a_committee_that_barely_agrees_cannot_meet_a_confident_series() -> Result<()> {
    // A series may demand more certainty than the oracle's tolerance merely
    // offers. The fix is real — two honest signatures ten basis points apart — and
    // the venue still will not pay on it.
    let env = setup().await?;
    let series_id = create_series_with_floor(&env, 8_000_000_000, 120_000_000_000, 9_999).await?;
    post_and_fill(&env, series_id).await?;
    env.advance_past(env.worker.view_block().await?.timestamp() + 15_000_000_000)
        .await?;

    let outcome = env
        .report(
            "ETH-USD",
            &[("kraken", 105u128 * SCALE), ("okx", 105u128 * SCALE + 100_000)],
        )
        .await?;
    assert_eq!(outcome["fixed"].as_array().map(|rows| rows.len()), Some(1));
    let fix: Value = env
        .oracle
        .view("get_latest_fix")
        .args_json(json!({ "feed_id": "ETH-USD" }))
        .await?
        .json()?;
    assert!(fix["dispersion_bps"].as_u64().unwrap_or(0) >= 5);
    assert!(fix["confidence_bps"].as_u64() < Some(9_999));

    let series = env.settle(series_id).await?;
    assert_eq!(
        series["settlement"], json!("Open"),
        "the series asked for near-certainty and the committee gave it ten basis points of doubt"
    );

    // Where every seat signs the identical number, the same series does settle.
    env.worker.fast_forward(3).await?;
    env.fix("ETH-USD", 104u128 * SCALE).await?;
    let series = env.settle(series_id).await?;
    assert_eq!(series["settlement"]["Fixed"]["price"].as_u64(), Some(104_000_000));
    Ok(())
}

#[tokio::test]
async fn a_fix_outside_the_series_band_is_still_not_a_price() -> Result<()> {
    let env = setup().await?;
    let series_id = create_series(&env, 8_000_000_000, 120_000_000_000).await?;
    post_and_fill(&env, series_id).await?;
    env.advance_past(env.worker.view_block().await?.timestamp() + 15_000_000_000)
        .await?;

    // Both seats agree, perfectly — on a number a hundred times too large. The
    // quorum is a rule about who spoke, not about whether the price is sane.
    env.fix("ETH-USD", 105u128 * 100 * SCALE).await?;
    let series = env.settle(series_id).await?;
    assert_eq!(series["settlement"], json!("Open"), "a 100x scale mismatch must not settle");

    env.worker.fast_forward(3).await?;
    env.fix("ETH-USD", 105u128 * SCALE).await?;
    let series = env.settle(series_id).await?;
    assert_eq!(series["settlement"]["Fixed"]["price"].as_u64(), Some(105_000_000));
    Ok(())
}

#[tokio::test]
async fn a_seller_can_take_a_resting_bid() -> Result<()> {
    let env = setup().await?;
    let series_id = create_series(&env, 30_000_000_000, 300_000_000_000).await?;
    let now = env.worker.view_block().await?.timestamp();

    // Alice bids for protection: her premium is escrowed immediately.
    let bid: u64 = env
        .alice
        .call(env.options.id(), "post_bid")
        .args_json(json!({
            "series_id": series_id,
            "quantity": SCALE.to_string(),
            "premium": (3u128 * SCALE).to_string(),
            "expires_ns": now + 20_000_000_000u64
        }))
        .gas(Gas::from_tgas(50))
        .transact()
        .await?
        .into_result()?
        .json()?;
    assert_eq!(env.balance(&env.alice).await?, 97 * SCALE);
    let stats: Value = env.options.view("stats").await?.json()?;
    assert_eq!(stats["total_bid_escrow"], (3u128 * SCALE).to_string());
    let bids: Value = env
        .options
        .view("get_bids")
        .args_json(json!({ "series_id": series_id, "limit": 10 }))
        .await?
        .json()?;
    assert_eq!(bids.as_array().map(|rows| rows.len()), Some(1));
    assert_eq!(bids[0]["unit_premium"], (3u128 * SCALE).to_string());

    // Bob sells into the bid and becomes the maker.
    let ticket: u64 = env
        .bob
        .call(env.options.id(), "fill_bid")
        .args_json(json!({ "bid_id": bid }))
        .gas(Gas::from_tgas(60))
        .transact()
        .await?
        .into_result()?
        .json()?;
    assert_eq!(env.balance(&env.bob).await?, 90 * SCALE);
    let stored: Value = env
        .options
        .view("get_ticket")
        .args_json(json!({ "ticket_id": ticket }))
        .await?
        .json::<Option<Value>>()?
        .ok_or_else(|| anyhow!("ticket missing"))?;
    assert_eq!(stored["buyer"], json!(env.alice.id().to_string()));
    assert_eq!(stored["maker"], json!(env.bob.id().to_string()));
    let (expected, actual) = env.custody().await?;
    assert_eq!(expected, 200 * SCALE);
    assert_eq!(actual, expected);

    // Settle at 105: the bidder buys the 5-point payoff at a 3 premium.
    env.advance_past(now + 40_000_000_000).await?;
    env.fix("ETH-USD", 105u128 * SCALE).await?;
    env.admin
        .call(env.options.id(), "request_settlement")
        .args_json(json!({ "series_id": series_id }))
        .gas(Gas::from_tgas(100))
        .transact()
        .await?
        .into_result()?;
    env.worker.fast_forward(5).await?;
    env.admin
        .call(env.options.id(), "settle_ticket")
        .args_json(json!({ "ticket_id": ticket }))
        .gas(Gas::from_tgas(60))
        .transact()
        .await?
        .into_result()?;
    assert_eq!(env.balance(&env.alice).await?, 102 * SCALE);
    assert_eq!(env.balance(&env.bob).await?, 98 * SCALE);
    let (expected, actual) = env.custody().await?;
    assert_eq!(expected, 200 * SCALE);
    assert_eq!(actual, expected);
    Ok(())
}
