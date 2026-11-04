#![doc = "Fully collateralized, cash-settled put-spread tickets on NEAR testnet."]

// Payoff, fee and vault arithmetic all come from the shared crate, so the
// engine that quotes this market off-chain evaluates the identical functions.
// Nothing here is float: the contract sees integer token units and integer
// basis points, and the mid it is handed is bounded by the payoff ceiling.
pub use bounded_core::{accounting, fees, pool};
use pool::{Pool, Written};

use near_sdk::borsh::BorshSerialize;
use near_sdk::json_types::U128;
use near_sdk::store::LookupMap;
use near_sdk::{
    env, ext_contract, near, require, AccountId, BorshStorageKey, Gas, NearToken, PanicOnDefault,
    Promise,
};

const GAS_FOR_FT_TRANSFER: Gas = Gas::from_tgas(20);
const GAS_FOR_ORACLE_QUERY: Gas = Gas::from_tgas(25);
const GAS_FOR_CALLBACK: Gas = Gas::from_tgas(25);
const MAX_OBSERVATIONS: u32 = 100;
const MAX_OPEN_OFFERS: u64 = 10_000;
const MAX_OPEN_BIDS: u64 = 10_000;
const MAX_OPEN_TICKETS: u64 = 10_000;
const MAX_PENDING_WITHDRAWALS: u64 = 1_000;
const MAX_OFFERS_PER_SERIES: usize = 200;
const UNIT_PRECISION: u128 = 1_000_000;
const MAX_ACCEPT_WINDOW_NS: u64 = 24 * 60 * 60 * 1_000_000_000;
const MAX_FINALIZE_WINDOW_NS: u64 = 30 * 24 * 60 * 60 * 1_000_000_000;
const SETTLEMENT_GRACE_NS: u64 = 300 * 1_000_000_000;
const PRICE_BOUND_FACTOR: u128 = 10;
/// The venue will not pay out on one party's word. Whatever oracle is wired in,
/// a settlement price has to arrive naming at least this many independent sources
/// — so a single-reporter oracle, or a compromised one, cannot settle a ticket
/// even if it is the only price the venue is told about.
const MIN_SOURCES_FOR_FIX: usize = 2;
/// How long a venue quote stays payable. The vault only trades at a price the
/// venue itself published, and a published price is only worth what it said
/// while it is still current.
const MAX_QUOTE_AGE_NS: u64 = 60 * 1_000_000_000;
/// How stale a quote may be on a ladder that is days away. The freshness a market
/// needs scales with how near it is: an hour out has to be a minute fresh, a week
/// out can afford five, and the operator refreshes seven ladders in rotation.
const MAX_QUOTE_AGE_FAR_NS: u64 = 300 * 1_000_000_000;
/// A quote may carry a stamp a little ahead of the chain clock; the gateway and
/// the host read different clocks and a few milliseconds of skew is normal.
const MAX_QUOTE_SLACK_NS: u64 = 5 * 1_000_000_000;
/// One batched quote write covers a whole ladder and no more, the same ceiling
/// `get_pool_prices` puts on a read.
const MAX_QUOTES_PER_CALL: usize = 64;
/// How many series one read may ask for. The registry is derived — this contract
/// is the only record of which strikes exist — so every reader walks the id range,
/// and one view per series made a week of ladders into hundreds of round trips.
const MAX_SERIES_PER_CALL: usize = 128;

// ---------- storage rent ----------
//
// The sandbox mints accounts with NEAR to spare, so nobody ever noticed that
// this contract pays for every byte it stores out of its own balance and that
// nothing ever put money in. On a public chain that is the difference between a
// venue that runs for a week and one that runs out of rent and stops accepting
// writes — 500 series is ~40 KB of state, and a series row alone is ~270 bytes
// that the venue account, not the trader, is charged for.
//
// The model here is deliberately narrow: it does not re-price users' own rows
// (a ticket is theirs and they paid gas for it), it charges for the state the
// *venue* creates on their behalf, and it leaves the vault's own bookkeeping on
// the venue's tab at boot. That is small, honest, and enough to keep the
// contract funded instead of silently bankrupt.
/// Bytes of contract state one series row occupies, measured against the
/// serialized `Series` plus its `LookupMap` key: an immutable 270-ish byte
/// record that never goes away, because a settled series is still a series.
const SERIES_STATE_BYTES: u64 = 344;
/// A ladder quote is one `Quote` (16 bytes of payload) plus the map key.
const QUOTE_STATE_BYTES: u64 = 88;
/// A venue-published offer row: `Offer` plus its two index entries.
const OFFER_STATE_BYTES: u64 = 208;
/// `near_sdk::env::storage_byte_cost()` is 10^19 yoctoNEAR per byte on both
/// testnet and mainnet, but it is protocol configuration and not a constant we
/// get to assume: read it, never hard-code it.
fn storage_price_per_byte() -> NearToken {
    env::storage_byte_cost()
}

#[derive(BorshStorageKey, BorshSerialize)]
#[borsh(crate = "near_sdk::borsh")]
enum StorageKey {
    Series,
    Offers,
    Tickets,
    Balances,
    Withdrawals,
    OwnerWithdrawals,
    SeriesOffers,
    Bids,
    SeriesBids,
    HolderTickets,
    LpShares,
    PoolQuotes,
}

#[near(serializers = [json, borsh])]
#[derive(Clone)]
pub struct Config {
    pub admin: AccountId,
    pub token: AccountId,
    pub oracle: AccountId,
    pub paused: bool,
}

/// Immutable per-expiry settlement policy. A series is created once and its
/// oracle account, feed, strikes and timing windows cannot be replaced.
#[near(serializers = [json, borsh])]
#[derive(Clone, PartialEq, Debug)]
pub struct Series {
    pub underlying: String,
    pub kind: SpreadKind,
    pub expiry_ns: u64,
    pub accept_until_ns: u64,
    pub finalize_until_ns: u64,
    pub low_strike: u128,
    pub high_strike: u128,
    pub price_scale: u128,
    pub min_confidence_bps: u16,
    pub oracle_feed: String,
    pub settlement: Settlement,
    pub pending_since_ns: u64,
    /// Who signed the price this series fixed on. Empty until it fixes, and kept
    /// afterwards: a ticket that paid out should always be able to say which
    /// sources priced it, years later, from the chain alone.
    pub settle_sources: Vec<String>,
    /// How far those sources were apart, in basis points.
    pub settle_dispersion_bps: u32,
}

/// The payoff family of a series. Puts protect a fall, calls protect a rise;
/// both are bounded spreads with the same collateral rule.
#[near(serializers = [json, borsh])]
#[derive(Clone, PartialEq, Debug)]
pub enum SpreadKind {
    Put,
    Call,
}

#[near(serializers = [json, borsh])]
#[derive(Clone, PartialEq, Debug)]
pub enum Settlement {
    Open,
    Pending,
    Fixed { price: u128, publish_time_ns: u64 },
    Void,
}

#[near(serializers = [json, borsh])]
#[derive(Clone)]
pub struct Offer {
    pub series_id: u64,
    pub maker: AccountId,
    pub taker: Option<AccountId>,
    pub quantity: u128,
    pub premium: u128,
    pub collateral: u128,
    pub expires_ns: u64,
}

#[near(serializers = [json, borsh])]
#[derive(Clone)]
pub struct Ticket {
    pub series_id: u64,
    pub buyer: AccountId,
    pub maker: AccountId,
    pub quantity: u128,
    pub collateral: u128,
    pub premium: u128,
    /// What the current holder paid for this ticket. It starts equal to
    /// `premium` and is restated when the ticket is closed into a bid, so a
    /// position view can report a real cost basis. Settlement still pays out
    /// `premium`, which is the amount the maker is owed.
    pub cost: u128,
    /// The venue's share of `premium`, for a ticket the pool wrote. Zero for a
    /// ticket a person wrote. It is stored rather than recomputed because the
    /// margin can change between entry and settlement, and a settlement must
    /// unwind exactly the split that was made.
    pub venue_cut: u128,
    /// Held to expiry: the buyer paid the mid with no venue margin and gave up
    /// the right to leave. See `pool_buy_held`.
    pub held: bool,
}

/// A resting bid: someone willing to BUY a protection ticket at `premium`,
/// with that premium escrowed. A seller fills it and becomes the maker.
#[near(serializers = [json, borsh])]
#[derive(Clone)]
pub struct Bid {
    pub series_id: u64,
    pub owner: AccountId,
    pub quantity: u128,
    pub premium: u128,
    pub expires_ns: u64,
}

/// One held ticket, as the portfolio screen reads it.
#[near(serializers = [json])]
#[derive(Clone)]
pub struct PositionView {
    pub ticket_id: u64,
    pub series_id: u64,
    pub side: String,
    /// Who stands behind this ticket. A holder needs it to know whether they can
    /// sell the position back to the vault or only into someone's bid, and a
    /// gateway should not have to read the ticket record to find out.
    pub maker: AccountId,
    pub quantity: U128,
    pub cost: U128,
    pub premium: U128,
    pub collateral: U128,
    pub kind: SpreadKind,
    pub underlying: String,
    pub low_strike: U128,
    pub high_strike: U128,
    pub price_scale: U128,
    pub expiry_ns: u64,
    pub settlement: String,
    pub fixed_price: Option<U128>,
    /// Who signed the price a finished ticket paid out on. Empty while the
    /// series is still open.
    pub settle_sources: Vec<String>,
    /// Held to expiry: no fee was charged and the row cannot be closed.
    pub held: bool,
}

#[near(serializers = [json])]
#[derive(Clone)]
pub struct BidView {
    pub bid_id: u64,
    pub owner: AccountId,
    pub quantity: U128,
    pub premium: U128,
    pub unit_premium: U128,
    pub expires_ns: u64,
}

/// The vault as the UI and the LP screen read it. `free_equity` is what the pool
/// can still stand behind; `utilisation_bps` is the same number as a percentage
/// of the whole balance sheet.
/// What the venue last said a series is worth, and when it said it.
///
/// The pool used to price from the `mid_units` a caller sent, which is not a
/// price: any account could buy a $100-worst-case ticket for two units of the
/// token by naming a tiny mid, and close it for ninety by naming a large one —
/// about 0.9 × width of other people's money per pair of transactions,
/// repeatable until the vault was empty. Now the venue publishes the number and
/// a trade must agree to exactly that.
#[near(serializers = [json, borsh])]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Quote {
    pub mid_units: u128,
    pub stamp_ns: u64,
}

/// What the vault charges and pays for a whole quantity, at its own quote. The
/// arithmetic is `core::fees`, the same the execution path runs, so a screen
/// reading this is not estimating the price — it is reading it.
#[near(serializers = [json])]
#[derive(Clone)]
pub struct PoolPrice {
    pub series_id: u64,
    pub quantity: U128,
    pub mid_units: U128,
    pub entry_units: U128,
    pub exit_units: U128,
    pub stamp_ns: u64,
    pub age_ns: u64,
}

#[near(serializers = [json])]
#[derive(Clone)]
pub struct PoolState {
    pub equity: U128,
    pub reserved: U128,
    /// Margin on open tickets, which a void would hand back.
    pub venue_locked: U128,
    /// Margin the venue can sweep now.
    pub venue: U128,
    pub consolation: U128,
    pub shares: U128,
    pub free_equity: U128,
    pub utilisation_bps: U128,
    pub open_tickets: u64,
    pub venue_account: AccountId,
    pub margin_bps: u32,
    pub impact_bps: u32,
}

#[near(serializers = [json, borsh])]
#[derive(Clone, PartialEq, Debug)]
pub enum WithdrawalStatus {
    Pending,
    Succeeded,
    Failed,
}

#[near(serializers = [json, borsh])]
#[derive(Clone)]
pub struct Withdrawal {
    pub owner: AccountId,
    pub amount: u128,
    pub status: WithdrawalStatus,
    pub requested_ns: u64,
}

/// Public price-level view. Amounts are `U128` so a browser never parses a
/// value wider than IEEE-754 can hold exactly.
#[near(serializers = [json])]
#[derive(Clone)]
pub struct BookOffer {
    pub offer_id: u64,
    pub maker: AccountId,
    pub quantity: U128,
    pub premium: U128,
    pub unit_premium: U128,
    pub taker: Option<AccountId>,
    pub expires_ns: u64,
}

/// Result of a market sweep: one ticket per offer taken, plus the slippage the
/// taker actually paid versus the size requested.
#[near(serializers = [json])]
#[derive(Clone)]
pub struct MarketFill {
    pub tickets: Vec<u64>,
    pub filled_quantity: U128,
    pub total_premium: U128,
    pub unfilled_quantity: U128,
    pub worst_unit_premium: U128,
}

/// What the oracle hands back for a settlement window. `sources` names who
/// signed the number: a fix with fewer than two of them is one party's word, and
/// the venue refuses it whatever the oracle that said it is.
#[near(serializers = [json])]
#[derive(Clone)]
pub struct PriceObservation {
    pub price: u128,
    pub publish_time_ns: u64,
    pub confidence_bps: u16,
    #[serde(default)]
    pub sources: Vec<String>,
    #[serde(default)]
    pub dispersion_bps: u32,
}

#[near(serializers = [json])]
pub struct Stats {
    pub series_count: u64,
    pub offer_count: u64,
    pub ticket_count: u64,
    pub withdrawal_count: u64,
    pub open_offer_count: u64,
    pub open_bid_count: u64,
    pub open_ticket_count: u64,
    pub pending_withdrawal_count: u64,
    pub total_available: U128,
    pub total_offer_collateral: U128,
    pub total_bid_escrow: U128,
    pub total_ticket_collateral: U128,
    pub total_ticket_premium: U128,
    pub total_pending_withdrawals: U128,
}

/// What the venue owes for the state it has written, and what it has behind it.
///
/// A public deployment fails in a way a sandbox never does: the contract pays
/// storage rent out of its own account balance, and a venue that never collected
/// any is one busy week from being unable to accept a write. This view exists so
/// that failure is visible as a number before it is visible as an error.
#[near(serializers = [json])]
#[derive(Clone, PartialEq, Debug)]
pub struct StorageState {
    /// NEAR this contract must hold to keep every byte it has stored.
    pub required_yocto: U128,
    /// NEAR callers have put on the tab through `storage_deposit`.
    pub paid_yocto: U128,
    /// The contract account's balance right now.
    pub balance_yocto: U128,
    /// Whether that balance covers `required_yocto`.
    pub funded: bool,
    /// The protocol's price per byte, so a reader can check the arithmetic.
    pub price_per_byte: U128,
    /// `env::storage_usage()` — code, collections and every row.
    pub usage_bytes: u64,
}

#[ext_contract(ext_oracle)]
pub trait Oracle {
    fn get_observations(
        &self,
        feed_id: String,
        from_ns: u64,
        to_ns: u64,
        limit: u32,
    ) -> Vec<PriceObservation>;
}

#[ext_contract(ext_token)]
pub trait FungibleToken {
    fn ft_transfer(&mut self, receiver_id: AccountId, amount: U128, memo: Option<String>);
}

fn emit(event: &str, data: near_sdk::serde_json::Value) {
    env::log_str(&format!(
        "EVENT_JSON:{}",
        near_sdk::serde_json::json!({
            "standard": "bounded-options",
            "version": "1.0.0",
            "event": event,
            "data": data
        })
    ));
}

/// Whether the venue may pay out on a number the oracle returned.
///
/// Five separate reasons to refuse it, none of which is the oracle's own
/// business: a price outside the band this series could conceivably settle at is
/// a wrong feed, a stamp outside the series' own window is the wrong moment, a
/// thin confidence is a committee that does not know, and fewer than two named
/// sources is one party's word however well it is signed. Every rule is checked
/// here rather than trusted from the oracle, because the oracle is a separate
/// contract someone could swap for a friendlier one.
fn usable_fix(obs: &PriceObservation, series: &Series, now: u64) -> bool {
    let upper_bound = series.high_strike.saturating_mul(PRICE_BOUND_FACTOR);
    let lower_bound = series.low_strike / PRICE_BOUND_FACTOR;
    obs.price > 0
        && obs.price >= lower_bound
        && obs.price <= upper_bound
        && obs.publish_time_ns <= now
        && obs.publish_time_ns >= series.expiry_ns
        && obs.publish_time_ns <= series.accept_until_ns
        && obs.confidence_bps >= series.min_confidence_bps
        && distinct_sources(&obs.sources).len() >= MIN_SOURCES_FOR_FIX
}

/// The named sources, once each.
///
/// Two is a count of *entries*, and an oracle that repeats one name satisfies it
/// while still being one party's word — which is exactly what this layer exists to
/// refuse. The list is sorted and deduplicated here, and what the series stores is
/// this list, so the provenance a ticket reports is the number of voices that
/// actually stood behind its price.
fn distinct_sources(sources: &[String]) -> Vec<String> {
    let mut names = sources.to_vec();
    names.sort_unstable();
    names.dedup();
    names
}

#[near(contract_state)]
#[derive(PanicOnDefault)]
pub struct Contract {
    config: Config,
    series: LookupMap<u64, Series>,
    offers: LookupMap<u64, Offer>,
    tickets: LookupMap<u64, Ticket>,
    balances: LookupMap<AccountId, u128>,
    withdrawals: LookupMap<u64, Withdrawal>,
    owner_withdrawals: LookupMap<AccountId, Vec<u64>>,
    series_offers: LookupMap<u64, Vec<u64>>,
    bids: LookupMap<u64, Bid>,
    series_bids: LookupMap<u64, Vec<u64>>,
    /// Which tickets each account is exposed to, so the portfolio view
    /// reads a list instead of scanning every ticket.
    holder_tickets: LookupMap<AccountId, Vec<u64>>,
    // ---- the vault ----
    // Held as plain fields rather than one `Pool` so `bounded-core` needs
    // no borsh or near-sdk dependency: the chain-safe crate stays chain-safe,
    // and the contract is the only place that knows how these are stored.
    pool_equity: u128,
    pool_reserved: u128,
    /// Margin on tickets that are still open: a void gives it back, so it is
    /// never sweepable and never part of what an LP owns.
    pool_venue_locked: u128,
    /// Margin on tickets that have run off: the venue may take it.
    pool_venue: u128,
    pool_consolation: u128,
    pool_shares: u128,
    /// Shares each LP holds against `pool_shares`.
    lp_shares: LookupMap<AccountId, u128>,
    /// The venue's own price per series. A trade agrees to it; it does not
    /// propose one.
    pool_quotes: LookupMap<u64, Quote>,
    /// Count of open tickets whose maker is the vault.
    pool_tickets: u64,
    venue: AccountId,
    margin_bps: u32,
    impact_bps: u32,
    next_series: u64,
    next_offer: u64,
    next_bid: u64,
    next_ticket: u64,
    next_withdrawal: u64,
    total_available: u128,
    total_offer_collateral: u128,
    total_bid_escrow: u128,
    total_ticket_collateral: u128,
    total_ticket_premium: u128,
    total_pending_withdrawals: u128,
    open_offer_count: u64,
    open_bid_count: u64,
    open_ticket_count: u64,
    pending_withdrawal_count: u64,
    /// NEAR callers have given the venue to cover the state their trades
    /// created. A running total, not a slice of the account balance: the balance
    /// is what is left, this is what was ever put in, and only the second one
    /// answers whether the venue is being kept alive or quietly draining.
    storage_paid: NearToken,
}

#[near]
impl Contract {
    #[init]
    pub fn new(admin: AccountId, token: AccountId, oracle: AccountId) -> Self {
        // The venue accrual is swept by the admin until a deployment names a
        // separate fee account.
        let venue = admin.clone();
        Self {
            config: Config {
                admin,
                token,
                oracle,
                paused: false,
            },
            series: LookupMap::new(StorageKey::Series),
            offers: LookupMap::new(StorageKey::Offers),
            tickets: LookupMap::new(StorageKey::Tickets),
            balances: LookupMap::new(StorageKey::Balances),
            withdrawals: LookupMap::new(StorageKey::Withdrawals),
            owner_withdrawals: LookupMap::new(StorageKey::OwnerWithdrawals),
            series_offers: LookupMap::new(StorageKey::SeriesOffers),
            bids: LookupMap::new(StorageKey::Bids),
            series_bids: LookupMap::new(StorageKey::SeriesBids),
            holder_tickets: LookupMap::new(StorageKey::HolderTickets),
            pool_equity: 0,
            pool_reserved: 0,
            pool_venue_locked: 0,
            pool_venue: 0,
            pool_consolation: 0,
            pool_shares: 0,
            lp_shares: LookupMap::new(StorageKey::LpShares),
            pool_quotes: LookupMap::new(StorageKey::PoolQuotes),
            pool_tickets: 0,
            venue,
            margin_bps: fees::Terms::STANDARD.margin_bps,
            impact_bps: fees::Terms::STANDARD.impact_bps,
            next_series: 0,
            next_offer: 0,
            next_bid: 0,
            next_ticket: 0,
            next_withdrawal: 0,
            total_available: 0,
            total_offer_collateral: 0,
            total_bid_escrow: 0,
            total_ticket_collateral: 0,
            total_ticket_premium: 0,
            total_pending_withdrawals: 0,
            open_offer_count: 0,
            open_bid_count: 0,
            open_ticket_count: 0,
            pending_withdrawal_count: 0,
            storage_paid: NearToken::from_yoctonear(0),
        }
    }

    // ---------- custody ----------

    /// NEP-141 receiver. Returns the unused amount; a wrong token, wrong
    /// message or paused contract rejects the whole transfer without state
    /// changes.
    ///
    /// Two destinations: `"deposit"` credits the sender's tradable balance,
    /// `"pool-deposit"` mints vault shares against the pool's net asset value.
    pub fn ft_on_transfer(&mut self, sender_id: AccountId, amount: U128, msg: String) -> String {
        if env::predecessor_account_id() != self.config.token || self.config.paused {
            return amount.0.to_string();
        }
        match msg.as_str() {
            "deposit" => self.credit(&sender_id, amount.0),
            "pool-deposit" => {
                let mut vault = self.load_pool();
                let Some(minted) = vault.deposit(amount.0) else {
                    return amount.0.to_string();
                };
                self.save_pool(&vault);
                let held = self.lp_shares.get(&sender_id).copied().unwrap_or(0);
                self.lp_shares.insert(
                    sender_id.clone(),
                    held.checked_add(minted).expect("share overflow"),
                );
                emit(
                    "pool_deposit",
                    near_sdk::serde_json::json!({
                        "lp": sender_id,
                        "amount": amount.0,
                        "shares_minted": minted,
                        "equity": vault.equity
                    }),
                );
            }
            _ => return amount.0.to_string(),
        }
        "0".to_string()
    }

    /// The venue's price for a series, and the trade it makes possible. Only
    /// the venue account may speak for the vault, and a caller may only agree:
    /// `pool_buy` and `pool_close` require the mid they send to be exactly this
    /// number, so nobody can name a price that is not the venue's.
    #[payable]
    pub fn set_pool_quote(&mut self, series_id: u64, mid_units: U128, stamp_ns: u64) {
        self.assert_venue();
        if self.pool_quotes.get(&series_id).is_none() {
            self.charge_storage(QUOTE_STATE_BYTES, "quote");
        }
        self.write_quote(series_id, mid_units.0, stamp_ns);
    }

    /// The same write for a whole ladder.
    ///
    /// One transaction per series does not scale to seven tenors: a ladder is
    /// sixty cells, and at one call each a venue with a week of expiries cannot
    /// refresh them inside its own quote age. Batching keeps a ladder one write,
    /// so what the screen offers never depends on how many markets it lists.
    #[payable]
    pub fn set_pool_quotes(&mut self, series_ids: Vec<u64>, mid_units: Vec<U128>, stamp_ns: u64) {
        self.assert_venue();
        require!(
            series_ids.len() == mid_units.len(),
            "every series needs exactly one mid"
        );
        // Bounded like `get_pool_prices`: one call, one block's worth of work.
        require!(series_ids.len() <= MAX_QUOTES_PER_CALL, "too many quotes at once");
        // One charge for the whole ladder, and only for the cells that are new:
        // re-quoting a series the venue already quotes costs no additional state,
        // and charging for it would make a refresh cycle a tax on staying fresh.
        let fresh = series_ids
            .iter()
            .filter(|id| self.pool_quotes.get(id).is_none())
            .count() as u64;
        if fresh > 0 {
            self.charge_storage(fresh * QUOTE_STATE_BYTES, "ladder");
        }
        for (series_id, mid) in series_ids.into_iter().zip(mid_units.into_iter()) {
            self.write_quote(series_id, mid.0, stamp_ns);
        }
    }

    fn write_quote(&mut self, series_id: u64, mid_units: u128, stamp_ns: u64) {
        require!(mid_units > 0, "a quote must be positive");
        let series = self.series.get(&series_id).expect("series not found");
        require!(series.settlement == Settlement::Open, "series not open");
        // A stamp in the future is a quote that never goes stale, which is the
        // one thing a quote must be able to do.
        require!(
            stamp_ns <= env::block_timestamp().saturating_add(MAX_QUOTE_SLACK_NS),
            "quote stamp is in the future"
        );
        self.pool_quotes.insert(
            series_id,
            Quote {
                mid_units,
                stamp_ns,
            },
        );
    }

    pub fn withdraw(&mut self, amount: U128) -> Promise {
        require!(!self.config.paused, "withdrawals paused");
        require!(amount.0 > 0, "amount must be positive");
        require!(
            self.pending_withdrawal_count < MAX_PENDING_WITHDRAWALS,
            "withdrawal capacity reached"
        );
        let owner = env::predecessor_account_id();
        self.debit(&owner, amount.0);

        let withdrawal_id = self.next_withdrawal;
        self.next_withdrawal = self
            .next_withdrawal
            .checked_add(1)
            .expect("withdrawal counter overflow");
        self.total_pending_withdrawals = self
            .total_pending_withdrawals
            .checked_add(amount.0)
            .expect("pending total overflow");
        self.pending_withdrawal_count = self
            .pending_withdrawal_count
            .checked_add(1)
            .expect("withdrawal count overflow");
        self.withdrawals.insert(
            withdrawal_id,
            Withdrawal {
                owner: owner.clone(),
                amount: amount.0,
                status: WithdrawalStatus::Pending,
                requested_ns: env::block_timestamp(),
            },
        );
        let mut ids = self
            .owner_withdrawals
            .get(&owner)
            .cloned()
            .unwrap_or_default();
        ids.push(withdrawal_id);
        self.owner_withdrawals.insert(owner.clone(), ids);
        emit(
            "withdrawal_requested",
            near_sdk::serde_json::json!({
                "withdrawal_id": withdrawal_id,
                "owner": owner,
                "amount": amount.0
            }),
        );

        ext_token::ext(self.config.token.clone())
            .with_static_gas(GAS_FOR_FT_TRANSFER)
            .with_attached_deposit(NearToken::from_yoctonear(1))
            .ft_transfer(owner, amount, Some("bounded-options withdrawal".to_string()))
            .then(
                Self::ext(env::current_account_id())
                    .with_static_gas(GAS_FOR_CALLBACK)
                    .on_withdraw(withdrawal_id),
            )
    }

    // ---------- market ----------

    #[allow(clippy::too_many_arguments)]
    #[payable]
    pub fn create_series(
        &mut self,
        underlying: String,
        kind: SpreadKind,
        expiry_ns: u64,
        accept_until_ns: u64,
        finalize_until_ns: u64,
        low_strike: U128,
        high_strike: U128,
        price_scale: U128,
        min_confidence_bps: u16,
        oracle_feed: String,
    ) -> u64 {
        self.assert_admin();
        require!(is_tag(&underlying), "underlying must be a short ticker");
        require!(is_tag(&oracle_feed), "oracle feed must be a short id");
        let now = env::block_timestamp();
        require!(expiry_ns > now, "expiry must be in the future");
        require!(
            accept_until_ns > expiry_ns,
            "accept window must follow expiry"
        );
        require!(
            finalize_until_ns > accept_until_ns,
            "finalize must follow accept window"
        );
        require!(
            accept_until_ns
                <= expiry_ns
                    .checked_add(MAX_ACCEPT_WINDOW_NS)
                    .expect("accept window overflow"),
            "accept window too long"
        );
        require!(
            finalize_until_ns
                <= expiry_ns
                    .checked_add(MAX_FINALIZE_WINDOW_NS)
                    .expect("finalize window overflow"),
            "finalize window too long"
        );
        require!(high_strike.0 > low_strike.0, "invalid strikes");
        require!(price_scale.0 > 0, "price scale required");
        require!(min_confidence_bps <= 10_000, "invalid confidence");
        // A series is a permanent row: it outlives its own settlement, because
        // a ticket that paid has to be explainable years later. The caller who
        // creates it pays the rent for it, not the venue.
        self.charge_storage(SERIES_STATE_BYTES, "series");

        let id = self.next_series;
        self.next_series = self
            .next_series
            .checked_add(1)
            .expect("series counter overflow");
        self.series.insert(
            id,
            Series {
                underlying,
                kind,
                expiry_ns,
                accept_until_ns,
                finalize_until_ns,
                low_strike: low_strike.0,
                high_strike: high_strike.0,
                price_scale: price_scale.0,
                min_confidence_bps,
                oracle_feed,
                settlement: Settlement::Open,
                pending_since_ns: 0,
                settle_sources: Vec::new(),
                settle_dispersion_bps: 0,
            },
        );
        emit(
            "series_created",
            near_sdk::serde_json::json!({
                "series_id": id,
                "expiry_ns": expiry_ns,
                "low_strike": low_strike.0,
                "high_strike": high_strike.0
            }),
        );
        id
    }

    pub fn post_offer(
        &mut self,
        series_id: u64,
        quantity: U128,
        premium: U128,
        taker: Option<AccountId>,
        expires_ns: u64,
    ) -> u64 {
        require!(!self.config.paused, "trading paused");
        require!(
            self.open_offer_count < MAX_OPEN_OFFERS,
            "offer capacity reached"
        );
        let maker = env::predecessor_account_id();
        let now = env::block_timestamp();
        let series = self.series.get(&series_id).expect("series not found").clone();
        require!(series.settlement == Settlement::Open, "series not open");
        require!(now < series.expiry_ns, "series expired");
        require!(quantity.0 > 0, "quantity must be positive");
        require!(premium.0 > 0, "premium must be positive");
        require!(
            expires_ns > now && expires_ns <= series.expiry_ns,
            "invalid offer expiry"
        );
        if let Some(reserved) = &taker {
            require!(*reserved != maker, "maker cannot reserve own offer");
        }

        let width = series
            .high_strike
            .checked_sub(series.low_strike)
            .expect("strike width");
        let collateral = accounting::max_payout(quantity.0, width, series.price_scale)
            .expect("numeric overflow");
        // Check the per-series cap before touching balances so a rejected quote
        // cannot leave the maker's collateral half-moved.
        let mut series_offer_ids = self
            .series_offers
            .get(&series_id)
            .cloned()
            .unwrap_or_default();
        require!(
            series_offer_ids.len() < MAX_OFFERS_PER_SERIES,
            "series offer capacity reached"
        );
        self.debit(&maker, collateral);
        self.total_offer_collateral = self
            .total_offer_collateral
            .checked_add(collateral)
            .expect("offer total overflow");

        self.open_offer_count = self
            .open_offer_count
            .checked_add(1)
            .expect("offer count overflow");
        let offer_id = self.next_offer;
        self.next_offer = self.next_offer.checked_add(1).expect("offer overflow");
        self.offers.insert(
            offer_id,
            Offer {
                series_id,
                maker: maker.clone(),
                taker,
                quantity: quantity.0,
                premium: premium.0,
                collateral,
                expires_ns,
            },
        );
        series_offer_ids.push(offer_id);
        self.series_offers.insert(series_id, series_offer_ids);
        emit(
            "offer_posted",
            near_sdk::serde_json::json!({
                "offer_id": offer_id,
                "series_id": series_id,
                "maker": maker,
                "collateral": collateral,
                "premium": premium.0
            }),
        );
        offer_id
    }

    pub fn cancel_offer(&mut self, offer_id: u64) {
        let caller = env::predecessor_account_id();
        let offer = self.offers.get(&offer_id).expect("offer not found").clone();
        require!(offer.maker == caller, "only maker");

        self.total_offer_collateral = self
            .total_offer_collateral
            .checked_sub(offer.collateral)
            .expect("offer total underflow");
        self.open_offer_count -= 1;
        self.credit(&offer.maker, offer.collateral);
        self.offers.remove(&offer_id);
        self.remove_from_book(offer.series_id, offer_id);
        emit(
            "offer_cancelled",
            near_sdk::serde_json::json!({
                "offer_id": offer_id,
                "maker": offer.maker,
                "collateral": offer.collateral
            }),
        );
    }

    /// Drop expired quotes from a series' matching index. Anyone may call; it
    /// moves no funds. The offer records stay so their makers can still cancel
    /// and recover collateral, but sweeps stop paying for dead reads and the
    /// per-series cap counts live quotes only. Without this, posting short-dated
    /// offers up to the cap would permanently block a series' book.
    pub fn prune_offers(&mut self, series_id: u64, limit: Option<u8>) -> u64 {
        let now = env::block_timestamp();
        let limit = usize::from(limit.unwrap_or(50).min(50));
        let ids = match self.series_offers.get(&series_id) {
            Some(ids) => ids.clone(),
            None => return 0,
        };
        let mut removed = 0u64;
        let mut kept: Vec<u64> = Vec::with_capacity(ids.len());
        for offer_id in ids {
            let expired = match self.offers.get(&offer_id) {
                Some(offer) => offer.expires_ns <= now,
                None => true,
            };
            if expired && removed < limit as u64 {
                removed += 1;
                continue;
            }
            kept.push(offer_id);
        }
        if removed > 0 {
            if kept.is_empty() {
                self.series_offers.remove(&series_id);
            } else {
                self.series_offers.insert(series_id, kept);
            }
        }
        // Bids share the maintenance entry point: expired quotes must not sit
        // in the book forever in either direction.
        let mut bids_removed = 0u64;
        if let Some(ids) = self.series_bids.get(&series_id).cloned() {
            let mut kept_bids: Vec<u64> = Vec::with_capacity(ids.len());
            for bid_id in ids {
                let expired = match self.bids.get(&bid_id) {
                    Some(bid) => bid.expires_ns <= now,
                    None => true,
                };
                if expired && bids_removed < limit as u64 {
                    bids_removed += 1;
                    continue;
                }
                kept_bids.push(bid_id);
            }
            if bids_removed > 0 {
                if kept_bids.is_empty() {
                    self.series_bids.remove(&series_id);
                } else {
                    self.series_bids.insert(series_id, kept_bids);
                }
            }
        }
        if removed > 0 || bids_removed > 0 {
            emit(
                "offers_pruned",
                near_sdk::serde_json::json!({
                    "series_id": series_id,
                    "offers_removed": removed,
                    "bids_removed": bids_removed
                }),
            );
        }
        removed + bids_removed
    }

    // ---------- bids ----------

    /// Rest a bid: escrow the premium you are willing to pay for a protection
    /// ticket. A seller fills it and becomes the maker of that ticket.
    pub fn post_bid(
        &mut self,
        series_id: u64,
        quantity: U128,
        premium: U128,
        expires_ns: u64,
    ) -> u64 {
        require!(!self.config.paused, "trading paused");
        require!(
            self.open_bid_count < MAX_OPEN_BIDS,
            "bid capacity reached"
        );
        let owner = env::predecessor_account_id();
        let now = env::block_timestamp();
        let series = self.series.get(&series_id).expect("series not found").clone();
        require!(series.settlement == Settlement::Open, "series not open");
        require!(now < series.expiry_ns, "series expired");
        require!(quantity.0 > 0, "quantity must be positive");
        require!(premium.0 > 0, "premium must be positive");
        require!(
            expires_ns > now && expires_ns <= series.expiry_ns,
            "invalid bid expiry"
        );
        let mut ids = self
            .series_bids
            .get(&series_id)
            .cloned()
            .unwrap_or_default();
        require!(
            ids.len() < MAX_OFFERS_PER_SERIES,
            "series bid capacity reached"
        );
        self.debit(&owner, premium.0);
        self.total_bid_escrow = self
            .total_bid_escrow
            .checked_add(premium.0)
            .expect("bid escrow overflow");
        self.open_bid_count += 1;
        let bid_id = self.next_bid;
        self.next_bid = self.next_bid.checked_add(1).expect("bid overflow");
        self.bids.insert(
            bid_id,
            Bid {
                series_id,
                owner: owner.clone(),
                quantity: quantity.0,
                premium: premium.0,
                expires_ns,
            },
        );
        ids.push(bid_id);
        self.series_bids.insert(series_id, ids);
        emit(
            "bid_posted",
            near_sdk::serde_json::json!({
                "bid_id": bid_id,
                "series_id": series_id,
                "owner": owner,
                "quantity": quantity.0,
                "premium": premium.0
            }),
        );
        bid_id
    }

    pub fn cancel_bid(&mut self, bid_id: u64) {
        let caller = env::predecessor_account_id();
        let bid = self.bids.get(&bid_id).expect("bid not found").clone();
        require!(bid.owner == caller, "only bidder");
        self.total_bid_escrow = self
            .total_bid_escrow
            .checked_sub(bid.premium)
            .expect("bid escrow underflow");
        self.open_bid_count -= 1;
        self.credit(&bid.owner, bid.premium);
        self.bids.remove(&bid_id);
        self.remove_from_bid_index(bid.series_id, bid_id);
        emit(
            "bid_cancelled",
            near_sdk::serde_json::json!({
                "bid_id": bid_id,
                "owner": bid.owner,
                "premium": bid.premium
            }),
        );
    }

    /// A seller takes a resting bid: they post collateral, the bidder's
    /// escrowed premium becomes ticket premium, and a ticket is created.
    pub fn fill_bid(&mut self, bid_id: u64) -> u64 {
        require!(!self.config.paused, "trading paused");
        let maker = env::predecessor_account_id();
        let now = env::block_timestamp();
        let bid = self.bids.get(&bid_id).expect("bid not found").clone();
        let series = self.series.get(&bid.series_id).expect("series not found").clone();
        require!(now < bid.expires_ns, "bid expired");
        require!(now < series.expiry_ns, "series expired");
        require!(series.settlement == Settlement::Open, "series not open");
        require!(bid.owner != maker, "bidder cannot fill own bid");
        require!(
            self.open_ticket_count < MAX_OPEN_TICKETS,
            "ticket capacity reached"
        );

        let width = series
            .high_strike
            .checked_sub(series.low_strike)
            .expect("strike width");
        let collateral = accounting::max_payout(bid.quantity, width, series.price_scale)
            .expect("numeric overflow");
        self.debit(&maker, collateral);
        self.total_bid_escrow = self
            .total_bid_escrow
            .checked_sub(bid.premium)
            .expect("bid escrow underflow");
        self.total_ticket_premium = self
            .total_ticket_premium
            .checked_add(bid.premium)
            .expect("ticket premium overflow");
        self.total_ticket_collateral = self
            .total_ticket_collateral
            .checked_add(collateral)
            .expect("ticket total overflow");
        self.open_bid_count -= 1;
        self.open_ticket_count += 1;

        let ticket_id = self.next_ticket;
        self.next_ticket = self.next_ticket.checked_add(1).expect("ticket overflow");
        self.tickets.insert(
            ticket_id,
            Ticket {
                series_id: bid.series_id,
                buyer: bid.owner.clone(),
                maker: maker.clone(),
                quantity: bid.quantity,
                collateral,
                premium: bid.premium,
                cost: bid.premium,
                venue_cut: 0,
                held: false,
            },
        );
        self.note_ticket(&bid.owner, ticket_id);
        self.note_ticket(&maker, ticket_id);
        self.bids.remove(&bid_id);
        self.remove_from_bid_index(bid.series_id, bid_id);
        emit(
            "bid_filled",
            near_sdk::serde_json::json!({
                "bid_id": bid_id,
                "ticket_id": ticket_id,
                "series_id": bid.series_id,
                "buyer": bid.owner,
                "maker": maker,
                "premium": bid.premium,
                "collateral": collateral
            }),
        );
        ticket_id
    }

    pub fn fill_offer(&mut self, offer_id: u64) -> u64 {
        require!(!self.config.paused, "trading paused");
        let buyer = env::predecessor_account_id();
        let now = env::block_timestamp();
        let offer = self.offers.get(&offer_id).expect("offer not found").clone();
        let series = self
            .series
            .get(&offer.series_id)
            .expect("series not found")
            .clone();

        require!(now < offer.expires_ns, "offer expired");
        require!(now < series.expiry_ns, "series expired");
        require!(series.settlement == Settlement::Open, "series not open");
        require!(
            self.open_ticket_count < MAX_OPEN_TICKETS,
            "ticket capacity reached"
        );
        require!(buyer != offer.maker, "maker cannot fill own offer");
        if let Some(taker) = &offer.taker {
            require!(*taker == buyer, "offer reserved for another taker");
        }

        self.debit(&buyer, offer.premium);
        self.total_offer_collateral = self
            .total_offer_collateral
            .checked_sub(offer.collateral)
            .expect("offer total underflow");
        self.total_ticket_collateral = self
            .total_ticket_collateral
            .checked_add(offer.collateral)
            .expect("ticket total overflow");
        self.total_ticket_premium = self
            .total_ticket_premium
            .checked_add(offer.premium)
            .expect("ticket premium overflow");
        self.open_offer_count -= 1;
        self.open_ticket_count = self
            .open_ticket_count
            .checked_add(1)
            .expect("ticket count overflow");

        let ticket_id = self.next_ticket;
        self.next_ticket = self.next_ticket.checked_add(1).expect("ticket overflow");
        self.tickets.insert(
            ticket_id,
            Ticket {
                series_id: offer.series_id,
                buyer: buyer.clone(),
                maker: offer.maker.clone(),
                quantity: offer.quantity,
                collateral: offer.collateral,
                premium: offer.premium,
                cost: offer.premium,
                venue_cut: 0,
                held: false,
            },
        );
        self.note_ticket(&buyer, ticket_id);
        self.note_ticket(&offer.maker, ticket_id);
        self.offers.remove(&offer_id);
        self.remove_from_book(offer.series_id, offer_id);
        emit(
            "offer_filled",
            near_sdk::serde_json::json!({
                "offer_id": offer_id,
                "ticket_id": ticket_id,
                "series_id": offer.series_id,
                "buyer": buyer,
                "maker": offer.maker,
                "premium": offer.premium,
                "collateral": offer.collateral
            }),
        );
        ticket_id
    }

    // ---------- player vs pool ----------

    /// Buy a bounded spread straight from the vault. The pool is the maker, so
    /// there is no resting offer to take — and there is nothing for the buyer to
    /// price either: the trade happens at the quote the venue published
    /// (`set_pool_quote`) or not at all. A taker sends bounds, not a number.
    ///
    /// Three bounds make a wrong mark survivable, and none of them is trust:
    /// - the vault is protected because it prices itself: a caller cannot buy a
    ///   spread for less than the venue decided it was worth;
    /// - the buyer is protected by `max_total_premium`, their own slippage
    ///   budget, and by the rule that a ticket can never cost more than the
    ///   most it can ever pay;
    /// - the pool is protected by its capacity invariant, so a quote that is
    ///   generous to buyers can drain the equity LPs committed but can never
    ///   make the vault owe more than it holds.
    pub fn pool_buy(
        &mut self,
        series_id: u64,
        quantity: U128,
        max_total_premium: U128,
    ) -> u64 {
        self.buy_from_pool(series_id, quantity, max_total_premium, false)
    }

    /// Buy from the vault and commit to hold to expiry — at the mid, with no
    /// venue margin at all.
    ///
    /// This is the trade the product is willing to make with a holder who will
    /// not cut and run: no fee in exchange for no exit. The holder gives up the
    /// right to close early or sell into a bid, which is the option the margin
    /// was charging for; the pool gives up its fee but also stops facing
    /// early-exit flow. Settlement at expiry is unchanged — the payoff, not a
    /// haircut.
    pub fn pool_buy_held(
        &mut self,
        series_id: u64,
        quantity: U128,
        max_total_premium: U128,
    ) -> u64 {
        self.buy_from_pool(series_id, quantity, max_total_premium, true)
    }

    fn buy_from_pool(
        &mut self,
        series_id: u64,
        quantity: U128,
        max_total_premium: U128,
        held: bool,
    ) -> u64 {
        require!(!self.config.paused, "trading paused");
        require!(quantity.0 > 0, "quantity must be positive");
        require!(max_total_premium.0 > 0, "premium budget must be positive");
        let buyer = env::predecessor_account_id();
        let now = env::block_timestamp();
        let series = self
            .series
            .get(&series_id)
            .expect("series not found")
            .clone();
        require!(series.settlement == Settlement::Open, "series not open");
        require!(now < series.expiry_ns, "series expired");
        require!(
            self.open_ticket_count < MAX_OPEN_TICKETS,
            "ticket capacity reached"
        );

        let width = series
            .high_strike
            .checked_sub(series.low_strike)
            .expect("strike width");
        // The venue's number, or nothing: a caller cannot price the vault.
        let mid_units = self.live_quote(series_id);
        // A held ticket pays the mid and keeps the right to nothing else. `FLAT`
        // is the contract's own zero-fee policy, so there is no second fee path
        // to audit: the same arithmetic, with the margin set to zero.
        let terms = if held { fees::Terms::FLAT } else { self.terms() };
        let premium = terms
            .entry_premium(quantity.0, mid_units, series.price_scale)
            .expect("premium overflow");
        require!(premium > 0, "premium rounded to zero");
        require!(
            premium <= max_total_premium.0,
            "premium above the budget"
        );
        let venue_cut = terms.venue_cut(premium).expect("premium split");
        let lp_notional = terms.lp_notional(premium).expect("premium split");

        let mut vault = self.load_pool();
        let Some(written) = vault.write(
            quantity.0,
            width,
            series.price_scale,
            lp_notional,
            venue_cut,
        ) else {
            env::panic_str("pool full: this worst case does not fit the equity left");
        };
        // The ceiling is the written worst case itself, so a mark cannot price a
        // ticket above its own maximum payoff.
        require!(
            premium <= written.max_payout,
            "premium above the maximum payoff"
        );
        self.debit(&buyer, premium);
        self.save_pool(&vault);

        self.open_ticket_count += 1;
        self.pool_tickets += 1;
        let ticket_id = self.next_ticket;
        self.next_ticket = self.next_ticket.checked_add(1).expect("ticket overflow");
        self.tickets.insert(
            ticket_id,
            Ticket {
                series_id,
                buyer: buyer.clone(),
                // The vault is the maker. No account can be its own
                // predecessor, so this identity is unforgeable and marks the
                // ticket as pool money for the rest of its life.
                maker: env::current_account_id(),
                quantity: quantity.0,
                collateral: written.max_payout,
                premium,
                cost: premium,
                venue_cut,
                held,
            },
        );
        self.note_ticket(&buyer, ticket_id);
        emit(
            "pool_bought",
            near_sdk::serde_json::json!({
                "ticket_id": ticket_id,
                "series_id": series_id,
                "buyer": buyer,
                "quantity": quantity.0,
                "premium": premium,
                "venue_cut": venue_cut,
                "max_payout": written.max_payout,
                "equity": vault.equity,
                "reserved": vault.reserved
            }),
        );
        ticket_id
    }

    /// Sell a pool ticket back before expiry. Unlike `close_ticket`, which hands
    /// a position to a resting bidder and leaves the maker's obligation alone,
    /// this destroys the position: the pool pays its marked exit value less the
    /// impact haircut and gives back the reservation.
    ///
    /// The price is the venue's, exactly as on the way in — `min_exit_units` is
    /// the seller's floor, not their bid, and a floor the quote no longer clears
    /// is a refusal rather than a worse fill.
    pub fn pool_close(&mut self, ticket_id: u64, min_exit_units: Option<U128>) -> U128 {
        require!(!self.config.paused, "trading paused");
        let seller = env::predecessor_account_id();
        let now = env::block_timestamp();
        let ticket = self.tickets.get(&ticket_id).expect("ticket not found").clone();
        require!(
            ticket.maker == env::current_account_id(),
            "not a pool ticket"
        );
        require!(ticket.buyer == seller, "only the buyer can close a ticket");
        require!(
            !ticket.held,
            "a ticket held to expiry cannot be sold back"
        );
        let series = self
            .series
            .get(&ticket.series_id)
            .expect("series not found")
            .clone();
        require!(series.settlement == Settlement::Open, "series not open");
        require!(now < series.expiry_ns, "series expired");

        // Selling back is priced by the venue too. Without this the same holder
        // could name a large mid and be paid 0.9 × width of someone else's
        // equity for it.
        let mid_units = self.live_quote(ticket.series_id);
        let exit = self
            .terms()
            .exit_value(ticket.quantity, mid_units, series.price_scale)
            .expect("exit overflow");
        if let Some(floor) = min_exit_units {
            require!(
                exit >= floor.0,
                "the exit fell below the floor the seller set"
            );
        }
        let written = self.written_of(&ticket);
        let mut vault = self.load_pool();
        require!(
            vault.close(&written, exit).is_some(),
            "pool cannot pay this exit"
        );
        self.save_pool(&vault);
        self.tickets.remove(&ticket_id);
        self.open_ticket_count -= 1;
        self.pool_tickets -= 1;
        self.drop_ticket(&seller, ticket_id);
        self.credit(&seller, exit);
        emit(
            "pool_closed",
            near_sdk::serde_json::json!({
                "ticket_id": ticket_id,
                "seller": seller,
                "exit_value": exit,
                "equity": vault.equity,
                "reserved": vault.reserved
            }),
        );
        U128(exit)
    }

    /// Enter the vault with money already at the venue, minting the same shares
    /// the external `"pool-deposit"` message would. The contract holds the cash
    /// either way, so an FT round trip would only make LPs withdraw first.
    /// Custody is untouched by construction: one bucket shrinks as equity grows.
    pub fn pool_deposit_from_balance(&mut self, amount: U128) -> U128 {
        require!(!self.config.paused, "deposits paused");
        require!(amount.0 > 0, "amount must be positive");
        let lp = env::predecessor_account_id();
        let mut vault = self.load_pool();
        // Price the mint before moving anything: a deposit too small to buy a
        // whole share keeps the caller's balance exactly where it was.
        let Some(minted) = vault.deposit(amount.0) else {
            env::panic_str("deposit too small to mint a share");
        };
        self.debit(&lp, amount.0);
        self.save_pool(&vault);
        let held = self.lp_shares.get(&lp).copied().unwrap_or(0);
        self.lp_shares
            .insert(lp.clone(), held.checked_add(minted).expect("share overflow"));
        emit(
            "pool_deposit",
            near_sdk::serde_json::json!({
                "lp": lp,
                "amount": amount.0,
                "shares_minted": minted,
                "equity": vault.equity,
                "from_balance": true
            }),
        );
        U128(minted)
    }

    /// Redeem vault shares for the LP's own money. Refused when the pool cannot
    /// free the equity without leaving a worst case uncovered, so no LP can step
    /// in front of a settlement.
    pub fn pool_withdraw(&mut self, shares: U128) -> U128 {
        require!(!self.config.paused, "withdrawals paused");
        require!(shares.0 > 0, "shares must be positive");
        let lp = env::predecessor_account_id();
        let held = self.lp_shares.get(&lp).copied().unwrap_or(0);
        require!(held >= shares.0, "not enough shares");
        let mut vault = self.load_pool();
        let Some(amount) = vault.withdraw(shares.0) else {
            env::panic_str("pool committed: free equity does not cover this exit");
        };
        self.save_pool(&vault);
        let rest = held - shares.0;
        if rest == 0 {
            self.lp_shares.remove(&lp);
        } else {
            self.lp_shares.insert(lp.clone(), rest);
        }
        self.credit(&lp, amount);
        emit(
            "pool_withdraw",
            near_sdk::serde_json::json!({
                "lp": lp,
                "shares": shares.0,
                "amount": amount,
                "equity": vault.equity
            }),
        );
        U128(amount)
    }

    /// The venue takes its accrued margin. Only the slice on tickets that have
    /// already run off is sweepable: while a ticket is open its margin is the
    /// source a void refunds from, so sweeping it would strand the settlement.
    /// Nothing here touches `equity`, so collecting fees never dilutes a share.
    pub fn pool_sweep(&mut self) -> U128 {
        self.assert_venue();
        let venue = self.venue.clone();
        let mut vault = self.load_pool();
        let claimable = vault.sweep_venue();
        self.save_pool(&vault);
        if claimable > 0 {
            self.credit(&venue, claimable);
        }
        emit(
            "venue_swept",
            near_sdk::serde_json::json!({
                "venue": venue,
                "amount": claimable,
                "venue_left": vault.venue
            }),
        );
        U128(claimable)
    }

    pub fn set_fee_terms(&mut self, margin_bps: u32, impact_bps: u32) {
        self.assert_admin();
        require!(margin_bps < fees::BASIS as u32, "margin must be below 100%");
        require!(impact_bps < fees::BASIS as u32, "haircut must be below 100%");
        self.margin_bps = margin_bps;
        self.impact_bps = impact_bps;
    }

    pub fn set_venue(&mut self, venue: AccountId) {
        self.assert_admin();
        self.venue = venue;
    }

    // ---------- settlement ----------

    pub fn request_settlement(&mut self, series_id: u64) -> Promise {
        let now = env::block_timestamp();
        let mut series = self.series.get(&series_id).expect("series not found").clone();
        // A retry is allowed while a previous query is still pending: a caller
        // with too little gas must not be able to block settlement liveness.
        require!(
            matches!(series.settlement, Settlement::Open | Settlement::Pending),
            "settlement not open"
        );
        require!(now >= series.expiry_ns, "not expired");
        require!(now < series.finalize_until_ns, "acceptance window over");

        series.settlement = Settlement::Pending;
        series.pending_since_ns = now;
        self.series.insert(series_id, series.clone());
        emit(
            "settlement_requested",
            near_sdk::serde_json::json!({ "series_id": series_id }),
        );

        ext_oracle::ext(self.config.oracle.clone())
            .with_static_gas(GAS_FOR_ORACLE_QUERY)
            .get_observations(
                series.oracle_feed,
                series.expiry_ns,
                series.accept_until_ns,
                MAX_OBSERVATIONS,
            )
            .then(
                Self::ext(env::current_account_id())
                    .with_static_gas(GAS_FOR_CALLBACK)
                    .on_settlement_observation(series_id),
            )
    }

    /// Fixed on the first valid observation returned in the series window.
    /// A late or duplicate callback cannot overwrite a fixed or voided series.
    #[private]
    pub fn on_settlement_observation(&mut self, series_id: u64) -> bool {
        let mut series = self.series.get(&series_id).expect("series not found").clone();
        if series.settlement != Settlement::Pending {
            return false;
        }

        let observations = env::promise_result_checked(0, 64 * 1024)
            .ok()
            .and_then(|bytes| {
                near_sdk::serde_json::from_slice::<Vec<PriceObservation>>(&bytes).ok()
            });

        let now = env::block_timestamp();
        let chosen = observations.unwrap_or_default().into_iter().find(|obs| usable_fix(obs, &series, now));

        series.settlement = match chosen {
            Some(obs) => {
                series.settle_dispersion_bps = obs.dispersion_bps;
                series.settle_sources = distinct_sources(&obs.sources);
                Settlement::Fixed { price: obs.price, publish_time_ns: obs.publish_time_ns }
            }
            None => Settlement::Open,
        };
        series.pending_since_ns = 0;
        self.series.insert(series_id, series.clone());
        emit(
            "settlement_resolved",
            near_sdk::serde_json::json!({
                "series_id": series_id,
                "settlement": series.settlement,
                "sources": series.settle_sources,
                "dispersion_bps": series.settle_dispersion_bps,
            }),
        );
        true
    }

    pub fn void_series(&mut self, series_id: u64) {
        let now = env::block_timestamp();
        let mut series = self.series.get(&series_id).expect("series not found").clone();
        require!(
            now >= series.finalize_until_ns,
            "finalize deadline not reached"
        );
        match series.settlement {
            Settlement::Open => {}
            Settlement::Pending => require!(
                now >= series.pending_since_ns
                    .saturating_add(SETTLEMENT_GRACE_NS),
                "settlement grace period"
            ),
            _ => env::panic_str("already fixed or voided"),
        }
        series.settlement = Settlement::Void;
        series.pending_since_ns = 0;
        self.series.insert(series_id, series);
        emit(
            "series_voided",
            near_sdk::serde_json::json!({ "series_id": series_id }),
        );
    }

    /// Pay a ticket off its series' fixed price, or unwind it if the series was
    /// voided. A ticket whose maker is the vault settles out of pool equity
    /// instead of a maker's escrow; the buyer's credit is the same either way.
    pub fn settle_ticket(&mut self, ticket_id: u64) -> (U128, U128) {
        let ticket = self.tickets.get(&ticket_id).expect("ticket not found").clone();
        let series = self
            .series
            .get(&ticket.series_id)
            .expect("series not found")
            .clone();
        let from_pool = ticket.maker == env::current_account_id();
        let mut vault = self.load_pool();

        let (payout, voided) = match series.settlement {
            Settlement::Fixed { price, .. } => (
                match series.kind {
                    SpreadKind::Put => accounting::payout(
                        ticket.quantity,
                        series.price_scale,
                        series.high_strike,
                        series.low_strike,
                        price,
                    ),
                    SpreadKind::Call => accounting::payout_call(
                        ticket.quantity,
                        series.price_scale,
                        series.high_strike,
                        series.low_strike,
                        price,
                    ),
                }
                .expect("payoff overflow"),
                false,
            ),
            Settlement::Void => (0, true),
            _ => env::panic_str("series not settled"),
        };

        let (buyer_credit, maker_credit) = if from_pool {
            // The vault is the maker: it pays from its own equity and keeps the
            // premium it was handed, so there is no second credit to split.
            // A void instead unwinds the entry exactly, which is why the split
            // is stored on the ticket rather than recomputed from today's fee.
            let written = self.written_of(&ticket);
            if voided {
                require!(
                    vault.void_out(&written).is_some(),
                    "pool cannot unwind this ticket"
                );
                (ticket.premium, 0)
            } else {
                require!(
                    vault.settle(&written, payout).is_some(),
                    "pool cannot settle this ticket"
                );
                (payout, 0)
            }
        } else {
            accounting::split_settlement(ticket.collateral, ticket.premium, payout, voided)
                .expect("settlement accounting")
        };
        self.save_pool(&vault);
        self.open_ticket_count -= 1;
        if from_pool {
            self.pool_tickets -= 1;
        } else {
            self.total_ticket_collateral = self
                .total_ticket_collateral
                .checked_sub(ticket.collateral)
                .expect("ticket collateral underflow");
            self.total_ticket_premium = self
                .total_ticket_premium
                .checked_sub(ticket.premium)
                .expect("ticket premium underflow");
        }
        self.tickets.remove(&ticket_id);
        self.drop_ticket(&ticket.buyer, ticket_id);
        self.drop_ticket(&ticket.maker, ticket_id);
        self.credit(&ticket.buyer, buyer_credit);
        self.credit(&ticket.maker, maker_credit);
        emit(
            "ticket_settled",
            near_sdk::serde_json::json!({
                "ticket_id": ticket_id,
                "series_id": ticket.series_id,
                "buyer": ticket.buyer,
                "maker": ticket.maker,
                "buyer_credit": buyer_credit,
                "maker_credit": maker_credit,
                "voided": voided,
                "pool": from_pool
            }),
        );
        (U128(buyer_credit), U128(maker_credit))
    }

    /// Close a long ticket by selling it into the best resting bid on its
    /// series. The bidder's escrowed premium pays the exiting buyer and the
    /// bidder takes the ticket over; the maker's collateral never moves, so a
    /// close leaves the custody sum exactly as it was.
    ///
    /// Whole positions only: a bid for a different size cannot absorb the
    /// ticket without splitting it, and this contract does not split.
    pub fn close_ticket(&mut self, ticket_id: u64, min_unit_premium: Option<U128>) -> U128 {
        require!(!self.config.paused, "trading paused");
        let seller = env::predecessor_account_id();
        let now = env::block_timestamp();
        let ticket = self.tickets.get(&ticket_id).expect("ticket not found").clone();
        require!(ticket.buyer == seller, "only the buyer can close a ticket");
        require!(
            !ticket.held,
            "a ticket held to expiry cannot be sold on"
        );
        let series = self
            .series
            .get(&ticket.series_id)
            .expect("series not found")
            .clone();
        require!(series.settlement == Settlement::Open, "series not open");
        require!(now < series.expiry_ns, "series expired");

        let Some(bid) = self
            .get_bids(ticket.series_id, 50)
            .into_iter()
            .find(|bid| bid.owner != seller && bid.quantity.0 == ticket.quantity)
        else {
            env::panic_str("no resting bid for this whole ticket");
        };
        if let Some(min) = min_unit_premium {
            require!(
                bid.unit_premium.0 >= min.0,
                "close below the minimum unit premium"
            );
        }

        self.total_bid_escrow = self
            .total_bid_escrow
            .checked_sub(bid.premium.0)
            .expect("bid escrow underflow");
        self.open_bid_count -= 1;
        self.bids.remove(&bid.bid_id);
        self.remove_from_bid_index(ticket.series_id, bid.bid_id);
        self.credit(&seller, bid.premium.0);
        self.drop_ticket(&seller, ticket_id);
        self.note_ticket(&bid.owner, ticket_id);

        let taker = ticket.buyer.clone();
        let closed = Ticket {
            buyer: bid.owner.clone(),
            cost: bid.premium.0,
            ..ticket.clone()
        };
        self.tickets.insert(ticket_id, closed);
        emit(
            "ticket_closed",
            near_sdk::serde_json::json!({
                "ticket_id": ticket_id,
                "series_id": ticket.series_id,
                "seller": taker,
                "new_buyer": bid.owner,
                "bid_id": bid.bid_id,
                "proceeds": bid.premium.0,
                "unit_premium": bid.unit_premium.0
            }),
        );
        U128(bid.premium.0)
    }

    /// Every ticket an account is exposed to, long or short, with the cost
    /// basis and the series terms needed to mark it. PnL is computed by the
    /// caller against a live price; the contract only states facts.
    pub fn get_positions(&self, account: AccountId, limit: u8) -> Vec<PositionView> {
        let ids = self.holder_tickets.get(&account).cloned().unwrap_or_default();
        let mut rows = ids
            .into_iter()
            .filter_map(|ticket_id| {
                let ticket = self.tickets.get(&ticket_id).cloned()?;
                let holder = if ticket.buyer == account {
                    "long"
                } else if ticket.maker == account {
                    "short"
                } else {
                    return None;
                };
                let series = self.series.get(&ticket.series_id)?;
                Some(PositionView {
                    ticket_id,
                    series_id: ticket.series_id,
                    side: holder.to_string(),
                    maker: ticket.maker.clone(),
                    quantity: U128(ticket.quantity),
                    cost: U128(ticket.cost),
                    premium: U128(ticket.premium),
                    collateral: U128(ticket.collateral),
                    kind: series.kind.clone(),
                    underlying: series.underlying.clone(),
                    low_strike: U128(series.low_strike),
                    high_strike: U128(series.high_strike),
                    price_scale: U128(series.price_scale),
                    expiry_ns: series.expiry_ns,
                    settlement: format!("{:?}", series.settlement),
                    fixed_price: match series.settlement {
                        Settlement::Fixed { price, .. } => Some(U128(price)),
                        _ => None,
                    },
                    settle_sources: series.settle_sources.clone(),
                    held: ticket.held,
                })
            })
            .collect::<Vec<_>>();
        rows.sort_by_key(|row| row.ticket_id);
        rows.truncate(limit.min(100) as usize);
        rows
    }

    // ---------- callbacks ----------

    #[private]
    pub fn on_withdraw(&mut self, withdrawal_id: u64) -> String {
        let mut withdrawal = self
            .withdrawals
            .get(&withdrawal_id)
            .expect("withdrawal not found")
            .clone();
        require!(
            withdrawal.status == WithdrawalStatus::Pending,
            "withdrawal already resolved"
        );

        let owner = withdrawal.owner.clone();
        let mut resolved = false;
        match env::promise_result_checked(0, 64 * 1024) {
            Ok(_) => {
                self.total_pending_withdrawals = self
                    .total_pending_withdrawals
                    .checked_sub(withdrawal.amount)
                    .expect("pending total underflow");
                withdrawal.status = WithdrawalStatus::Succeeded;
                resolved = true;
            }
            Err(near_sdk::PromiseError::Failed) => {
                self.total_pending_withdrawals = self
                    .total_pending_withdrawals
                    .checked_sub(withdrawal.amount)
                    .expect("pending total underflow");
                withdrawal.status = WithdrawalStatus::Failed;
                self.credit(&withdrawal.owner, withdrawal.amount);
                resolved = true;
            }
            // An oversized or future error variant means the transfer may have
            // succeeded. Keep the debit and pending record rather than risk a
            // double credit.
            Err(_) => {}
        }

        if resolved {
            self.withdrawals.remove(&withdrawal_id);
            self.pending_withdrawal_count -= 1;
            let mut ids = self
                .owner_withdrawals
                .get(&owner)
                .cloned()
                .unwrap_or_default();
            ids.retain(|id| *id != withdrawal_id);
            if ids.is_empty() {
                self.owner_withdrawals.remove(&owner);
            } else {
                self.owner_withdrawals.insert(owner, ids);
            }
            emit(
                "withdrawal_resolved",
                near_sdk::serde_json::json!({
                    "withdrawal_id": withdrawal_id,
                    "owner": withdrawal.owner,
                    "amount": withdrawal.amount,
                    "status": withdrawal.status
                }),
            );
        } else {
            self.withdrawals.insert(withdrawal_id, withdrawal.clone());
        }
        format!("{:?}", withdrawal.status)
    }

    // ---------- administration ----------

    pub fn set_paused(&mut self, paused: bool) {
        self.assert_admin();
        self.config.paused = paused;
    }

    // ---------- storage rent ----------

    /// Put money on the venue's tab so it can go on storing state.
    ///
    /// Anyone may call this: a judge who wants the demo to keep quoting, the
    /// operator after it topped the account up, or an admin script. It is a
    /// donation with a receipt, not a payment that buys a right.
    #[payable]
    pub fn storage_deposit(&mut self) {
        let amount = env::attached_deposit();
        require!(amount.as_yoctonear() > 0, "attach NEAR for storage");
        let deposited = self.storage_paid.as_yoctonear()
            + amount.as_yoctonear();
        self.storage_paid = NearToken::from_yoctonear(deposited);
        emit(
            "storage_deposited",
            near_sdk::serde_json::json!({
                "from": env::predecessor_account_id(),
                "amount": amount.as_yoctonear().to_string(),
                "paid_total": deposited.to_string(),
            }),
        );
    }

    /// What the venue owes for the state it has already created, and whether the
    /// balance behind it can still cover that plus the next write.
    ///
    /// This is a view because it has to be readable before anything is spent: the
    /// operator asks it every pass, and a screen can show a judge the tab instead
    /// of letting them discover it as a failed order.
    pub fn get_storage_state(&self) -> StorageState {
        let required = self.storage_required();
        let balance = env::account_balance();
        StorageState {
            required_yocto: U128(required.as_yoctonear()),
            paid_yocto: U128(self.storage_paid.as_yoctonear()),
            balance_yocto: U128(balance.as_yoctonear()),
            funded: balance >= required,
            price_per_byte: U128(storage_price_per_byte().as_yoctonear()),
            usage_bytes: env::storage_usage(),
        }
    }

    /// The NEAR this contract must hold to keep every byte it has written.
    ///
    /// `env::storage_usage()` already counts the code, the collections and every
    /// row, and it is maintained by the runtime rather than by us — which is why
    /// the answer is a measurement and not an estimate. Multiplying it by the
    /// protocol's byte price gives the rent claim exactly.
    fn storage_required(&self) -> NearToken {
        storage_price_per_byte().saturating_mul(env::storage_usage() as u128)
    }

    /// Charge the caller for state this call is about to create for them.
    ///
    /// Called before the write, not after: a caller who cannot fund the state
    /// their trade needs is refused with a sentence, not left with a half-written
    /// order the venue then has to pay to keep. Anything left over is refunded so
    /// an over-payment is not silently kept.
    fn charge_storage(&mut self, bytes: u64, what: &str) {
        let cost = storage_price_per_byte()
            .saturating_mul(bytes as u128)
            .as_yoctonear();
        let attached = env::attached_deposit();
        if attached.as_yoctonear() < cost {
            env::panic_str(&format!(
                "this {what} stores {bytes} bytes of state; attach at least {cost} yoctoNEAR"
            ));
        }
        self.storage_paid = NearToken::from_yoctonear(
            self.storage_paid.as_yoctonear() + cost,
        );
        let refund = attached.as_yoctonear() - cost;
        if refund > 0 {
            Promise::new(env::predecessor_account_id())
                .transfer(NearToken::from_yoctonear(refund));
        }
    }

    // ---------- views ----------

    pub fn get_config(&self) -> Config {
        self.config.clone()
    }

    pub fn get_series(&self, series_id: u64) -> Option<Series> {
        self.series.get(&series_id).cloned()
    }

    /// Several series in one round trip, with the id beside each answer so a
    /// missing series is a hole the reader can see rather than a shifted row.
    pub fn get_series_many(&self, series_ids: Vec<u64>) -> Vec<(u64, Option<Series>)> {
        require!(series_ids.len() <= MAX_SERIES_PER_CALL, "too many series at once");
        series_ids
            .into_iter()
            .map(|series_id| (series_id, self.series.get(&series_id).cloned()))
            .collect()
    }

    pub fn get_offer(&self, offer_id: u64) -> Option<Offer> {
        self.offers.get(&offer_id).cloned()
    }

    pub fn get_ticket(&self, ticket_id: u64) -> Option<Ticket> {
        self.tickets.get(&ticket_id).cloned()
    }

    pub fn get_balance(&self, account_id: AccountId) -> U128 {
        U128(self.balances.get(&account_id).copied().unwrap_or(0))
    }

    pub fn get_withdrawal(&self, withdrawal_id: u64) -> Option<Withdrawal> {
        self.withdrawals.get(&withdrawal_id).cloned()
    }

    pub fn get_pool_state(&self) -> PoolState {
        let vault = self.load_pool();
        PoolState {
            equity: U128(vault.equity),
            reserved: U128(vault.reserved),
            venue_locked: U128(vault.venue_locked),
            venue: U128(vault.venue),
            consolation: U128(vault.consolation),
            shares: U128(vault.shares),
            free_equity: U128(vault.free_equity()),
            utilisation_bps: U128(vault.utilisation_bps()),
            open_tickets: self.pool_tickets,
            venue_account: self.venue.clone(),
            margin_bps: self.margin_bps,
            impact_bps: self.impact_bps,
        }
    }

    pub fn get_pool_quote(&self, series_id: u64) -> Option<Quote> {
        self.pool_quotes.get(&series_id).copied()
    }

    /// What the vault charges and pays for `quantity` at its own quotes, for a
    /// handful of series at once. A screen that worked these prices out from the
    /// mark would be quoting a trade the contract might not make: this is
    /// `core::fees` on the same integers execution uses, so the number shown is
    /// the number charged. A series with no live quote is simply absent.
    pub fn get_pool_prices(&self, series_ids: Vec<u64>, quantity: U128) -> Vec<PoolPrice> {
        require!(series_ids.len() <= 64, "too many series at once");
        let terms = self.terms();
        let now = env::block_timestamp();
        series_ids
            .iter()
            .filter_map(|id| {
                let quote = self.pool_quotes.get(id)?;
                let series = self.series.get(id)?;
                if series.settlement != Settlement::Open || quote.mid_units == 0 {
                    return None;
                }
                if now.saturating_sub(quote.stamp_ns) > Self::quote_age_allowed(series.expiry_ns, now) {
                    return None;
                }
                let entry = terms.entry_premium(quantity.0, quote.mid_units, series.price_scale)?;
                let exit = terms.exit_value(quantity.0, quote.mid_units, series.price_scale)?;
                Some(PoolPrice {
                    series_id: *id,
                    quantity: U128(quantity.0),
                    mid_units: U128(quote.mid_units),
                    entry_units: U128(entry),
                    exit_units: U128(exit),
                    stamp_ns: quote.stamp_ns,
                    age_ns: now.saturating_sub(quote.stamp_ns),
                })
            })
            .collect()
    }

    pub fn get_lp_shares(&self, account_id: AccountId) -> U128 {
        U128(self.lp_shares.get(&account_id).copied().unwrap_or(0))
    }

    /// What an LP balance redeems at the current NAV — `pool_withdraw`'s own
    /// arithmetic, read-only. A screen that estimated this itself would be
    /// quoting an exit the contract might not pay.
    pub fn get_lp_value(&self, account_id: AccountId) -> U128 {
        let vault = self.load_pool();
        U128(vault.exit_value(self.lp_shares.get(&account_id).copied().unwrap_or(0)))
    }

    pub fn get_owner_withdrawals(&self, owner: AccountId) -> Vec<u64> {
        self.owner_withdrawals.get(&owner).cloned().unwrap_or_default()
    }

    pub fn stats(&self) -> Stats {
        Stats {
            series_count: self.next_series,
            offer_count: self.next_offer,
            ticket_count: self.next_ticket,
            withdrawal_count: self.next_withdrawal,
            open_offer_count: self.open_offer_count,
            open_bid_count: self.open_bid_count,
            open_ticket_count: self.open_ticket_count,
            pending_withdrawal_count: self.pending_withdrawal_count,
            total_available: U128(self.total_available),
            total_offer_collateral: U128(self.total_offer_collateral),
            total_bid_escrow: U128(self.total_bid_escrow),
            total_ticket_collateral: U128(self.total_ticket_collateral),
            total_ticket_premium: U128(self.total_ticket_premium),
            total_pending_withdrawals: U128(self.total_pending_withdrawals),
        }
    }

    /// Sum that must equal the contract's FT balance between asynchronous
    /// transfers. Exposed so tests and operators can reconcile custody.
    ///
    /// The vault is part of that balance, and its own buckets are the only
    /// place its money is counted: a ticket the pool wrote contributes nothing
    /// to `total_ticket_collateral` or `total_ticket_premium`, because the
    /// premium it collected already moved into `equity` and `venue` at entry.
    pub fn expected_custody(&self) -> U128 {
        U128(
            self.total_available
                .checked_add(self.total_offer_collateral)
                .and_then(|v| v.checked_add(self.total_bid_escrow))
                .and_then(|v| v.checked_add(self.total_ticket_collateral))
                .and_then(|v| v.checked_add(self.total_ticket_premium))
                .and_then(|v| v.checked_add(self.total_pending_withdrawals))
                .and_then(|v| v.checked_add(self.load_pool().expected_custody()))
                .expect("custody overflow"),
        )
    }

    // ---------- market orders ----------

    /// Sweep whole offers from cheapest to dearest until the requested size or
    /// the worst-case premium budget is exhausted. Offers larger than the
    /// remaining size, reserved for another taker, expired, or authored by the
    /// buyer are skipped rather than partially taken.
    pub fn fill_market_order(
        &mut self,
        series_id: u64,
        quantity: U128,
        max_total_premium: U128,
        min_quantity: Option<U128>,
    ) -> MarketFill {
        require!(!self.config.paused, "trading paused");
        require!(quantity.0 > 0, "quantity must be positive");
        require!(max_total_premium.0 > 0, "premium budget must be positive");
        let buyer = env::predecessor_account_id();
        let now = env::block_timestamp();
        let series = self.series.get(&series_id).expect("series not found").clone();
        require!(series.settlement == Settlement::Open, "series not open");
        require!(now < series.expiry_ns, "series expired");
        require!(
            self.open_ticket_count < MAX_OPEN_TICKETS,
            "ticket capacity reached"
        );

        let mut candidates: Vec<(Offer, u64)> = Vec::new();
        for offer_id in self
            .series_offers
            .get(&series_id)
            .cloned()
            .unwrap_or_default()
        {
            let Some(offer) = self.offers.get(&offer_id).cloned() else {
                continue;
            };
            if offer.maker == buyer
                || now >= offer.expires_ns
                || offer.quantity > quantity.0
                || offer.premium > max_total_premium.0
            {
                continue;
            }
            if let Some(reserved) = &offer.taker {
                if *reserved != buyer {
                    continue;
                }
            }
            candidates.push((offer, offer_id));
        }
        // Sort on the scaled unit premium (1e6 precision) with offer id as the
        // tie-break: a provably total order, unlike cross-multiplication that
        // can overflow and hand `sort_by` an inconsistent comparator.
        candidates.sort_by(|(a, a_id), (b, b_id)| {
            unit_premium(a)
                .cmp(&unit_premium(b))
                .then_with(|| a_id.cmp(b_id))
        });

        let mut remaining = quantity.0;
        let mut budget = max_total_premium.0;
        let mut chosen: Vec<(Offer, u64)> = Vec::new();
        for (offer, offer_id) in candidates {
            if remaining == 0 {
                break;
            }
            if offer.quantity > remaining || offer.premium > budget {
                continue;
            }
            remaining -= offer.quantity;
            budget -= offer.premium;
            chosen.push((offer, offer_id));
        }

        let filled_quantity = quantity.0 - remaining;
        let total_premium = max_total_premium.0 - budget;
        require!(filled_quantity > 0, "no matching offers");
        require!(
            filled_quantity >= min_quantity.map(|value| value.0).unwrap_or(1),
            "minimum quantity not filled"
        );
        let projected_tickets = self
            .open_ticket_count
            .saturating_add(chosen.len() as u64);
        require!(
            projected_tickets <= MAX_OPEN_TICKETS,
            "ticket capacity reached"
        );
        self.debit(&buyer, total_premium);

        let mut tickets = Vec::with_capacity(chosen.len());
        let mut worst_unit_premium = 0u128;
        let mut taken_ids = Vec::with_capacity(chosen.len());
        for (offer, offer_id) in chosen {
            worst_unit_premium = worst_unit_premium.max(unit_premium(&offer));
            self.total_offer_collateral = self
                .total_offer_collateral
                .checked_sub(offer.collateral)
                .expect("offer total underflow");
            self.open_offer_count -= 1;
            self.offers.remove(&offer_id);
            taken_ids.push(offer_id);

            self.total_ticket_collateral = self
                .total_ticket_collateral
                .checked_add(offer.collateral)
                .expect("ticket total overflow");
            self.total_ticket_premium = self
                .total_ticket_premium
                .checked_add(offer.premium)
                .expect("ticket premium overflow");
            self.open_ticket_count = self
                .open_ticket_count
                .checked_add(1)
                .expect("ticket count overflow");

            let ticket_id = self.next_ticket;
            self.next_ticket = self.next_ticket.checked_add(1).expect("ticket overflow");
            self.tickets.insert(
                ticket_id,
                Ticket {
                    series_id: offer.series_id,
                    buyer: buyer.clone(),
                    maker: offer.maker.clone(),
                    quantity: offer.quantity,
                    collateral: offer.collateral,
                    premium: offer.premium,
                    cost: offer.premium,
                    venue_cut: 0,
                    held: false,
                },
            );
            self.note_ticket(&buyer, ticket_id);
            self.note_ticket(&offer.maker, ticket_id);
            tickets.push(ticket_id);
            // One per-offer record, shaped like fill_offer's event, so a maker
            // can recover what it sold from the log alone.
            emit(
                "offer_filled",
                near_sdk::serde_json::json!({
                    "offer_id": offer_id,
                    "ticket_id": ticket_id,
                    "series_id": offer.series_id,
                    "buyer": buyer,
                    "maker": offer.maker,
                    "premium": offer.premium,
                    "collateral": offer.collateral,
                    "swept": true
                }),
            );
        }
        self.remove_many_from_book(series_id, &taken_ids);

        let fill = MarketFill {
            tickets,
            filled_quantity: U128(filled_quantity),
            total_premium: U128(total_premium),
            unfilled_quantity: U128(remaining),
            worst_unit_premium: U128(worst_unit_premium),
        };
        emit(
            "market_order_filled",
            near_sdk::serde_json::json!({
                "series_id": series_id,
                "buyer": buyer,
                "filled_quantity": filled_quantity,
                "total_premium": total_premium,
                "unfilled_quantity": remaining,
                "worst_unit_premium": worst_unit_premium,
                "tickets": fill.tickets
            }),
        );
        fill
    }

    pub fn get_book(&self, series_id: u64, limit: u8) -> Vec<BookOffer> {
        let now = env::block_timestamp();
        let mut offers: Vec<BookOffer> = self
            .series_offers
            .get(&series_id)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|offer_id| {
                let offer = self.offers.get(&offer_id).cloned()?;
                (now < offer.expires_ns).then_some(BookOffer {
                    offer_id,
                    maker: offer.maker.clone(),
                    quantity: U128(offer.quantity),
                    premium: U128(offer.premium),
                    unit_premium: U128(unit_premium(&offer)),
                    taker: offer.taker,
                    expires_ns: offer.expires_ns,
                })
            })
            .collect();
        // Same order the sweep uses, so what a UI shows is what it will pay.
        offers.sort_by(|a, b| {
            a.unit_premium
                .0
                .cmp(&b.unit_premium.0)
                .then_with(|| a.offer_id.cmp(&b.offer_id))
        });
        offers.truncate(limit.min(50) as usize);
        offers
    }

    /// Resting bids for a series, best (highest premium per unit) first.
    pub fn get_bids(&self, series_id: u64, limit: u8) -> Vec<BidView> {
        let now = env::block_timestamp();
        let mut bids: Vec<BidView> = self
            .series_bids
            .get(&series_id)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|bid_id| {
                let bid = self.bids.get(&bid_id).cloned()?;
                (now < bid.expires_ns).then_some(BidView {
                    bid_id,
                    owner: bid.owner,
                    quantity: U128(bid.quantity),
                    premium: U128(bid.premium),
                    unit_premium: U128(premium_per_unit(bid.premium, bid.quantity)),
                    expires_ns: bid.expires_ns,
                })
            })
            .collect();
        bids.sort_by(|a, b| {
            b.unit_premium
                .0
                .cmp(&a.unit_premium.0)
                .then_with(|| a.bid_id.cmp(&b.bid_id))
        });
        bids.truncate(limit.min(50) as usize);
        bids
    }

    // ---------- internals ----------

    /// Rebuild the vault from its fields. `Pool` lives in the chain-safe
    /// crate and knows nothing about storage, so the contract owns only this
    /// translation.
    fn load_pool(&self) -> Pool {
        Pool {
            equity: self.pool_equity,
            reserved: self.pool_reserved,
            venue_locked: self.pool_venue_locked,
            venue: self.pool_venue,
            consolation: self.pool_consolation,
            shares: self.pool_shares,
        }
    }

    fn save_pool(&mut self, vault: &Pool) {
        self.pool_equity = vault.equity;
        self.pool_reserved = vault.reserved;
        self.pool_venue_locked = vault.venue_locked;
        self.pool_venue = vault.venue;
        self.pool_consolation = vault.consolation;
        self.pool_shares = vault.shares;
    }

    /// The vault's record of a position, rebuilt from the ticket. `premium` and
    /// `venue_cut` are what was actually collected, so this is the same split
    /// `pool_buy` made even if the fee terms have moved since.
    fn written_of(&self, ticket: &Ticket) -> Written {
        Written {
            max_payout: ticket.collateral,
            lp_notional: ticket
                .premium
                .checked_sub(ticket.venue_cut)
                .expect("premium split"),
            venue_cut: ticket.venue_cut,
        }
    }

    fn terms(&self) -> fees::Terms {
        fees::Terms {
            margin_bps: self.margin_bps,
            impact_bps: self.impact_bps,
        }
    }

    fn assert_admin(&self) {
        require!(
            env::predecessor_account_id() == self.config.admin,
            "only admin"
        );
    }

    fn assert_venue(&self) {
        require!(env::predecessor_account_id() == self.venue, "only the venue");
    }

    /// The price the vault trades at right now: the venue's own quote for this
    /// series, if it exists and is still current. A caller contributes bounds to
    /// a pool trade, never a price — which is the only arrangement that survives
    /// a moving index, because a price echoed at read time is stale by the time
    /// the transaction lands.
    /// A quote is stale relative to the market it prices: inside the day it has
    /// to be a minute fresh, beyond it five will do. Without this the operator
    /// had to refresh seven ladders inside one minute to keep the nearest
    /// tradeable, and the far ones spent their lives expired.
    fn quote_age_allowed(expiry_ns: u64, now_ns: u64) -> u64 {
        const DAY_NS: u64 = 24 * 3_600 * 1_000_000_000;
        if expiry_ns.saturating_sub(now_ns) <= DAY_NS {
            MAX_QUOTE_AGE_NS
        } else {
            MAX_QUOTE_AGE_FAR_NS
        }
    }

    fn live_quote(&self, series_id: u64) -> u128 {
        let Some(quote) = self.pool_quotes.get(&series_id) else {
            env::panic_str("the venue has not quoted this series");
        };
        let age = env::block_timestamp().saturating_sub(quote.stamp_ns);
        let allowed = Self::quote_age_allowed(
            self.series.get(&series_id).expect("series not found").expiry_ns,
            env::block_timestamp(),
        );
        require!(age <= allowed, "the venue's quote for this series has expired");
        quote.mid_units
    }

    fn credit(&mut self, account: &AccountId, amount: u128) {
        if amount == 0 {
            return;
        }
        let balance = self.balances.get(account).copied().unwrap_or(0);
        let updated = balance.checked_add(amount).expect("balance overflow");
        self.balances.insert(account.clone(), updated);
        self.total_available = self
            .total_available
            .checked_add(amount)
            .expect("available total overflow");
    }

    fn remove_from_book(&mut self, series_id: u64, offer_id: u64) {
        let Some(mut ids) = self.series_offers.get(&series_id).cloned() else {
            return;
        };
        ids.retain(|id| *id != offer_id);
        if ids.is_empty() {
            self.series_offers.remove(&series_id);
        } else {
            self.series_offers.insert(series_id, ids);
        }
    }

    fn note_ticket(&mut self, account: &AccountId, ticket_id: u64) {
        let mut ids = self.holder_tickets.get(account).cloned().unwrap_or_default();
        if !ids.contains(&ticket_id) {
            ids.push(ticket_id);
            self.holder_tickets.insert(account.clone(), ids);
        }
    }

    fn drop_ticket(&mut self, account: &AccountId, ticket_id: u64) {
        let Some(ids) = self.holder_tickets.get(account).cloned() else {
            return;
        };
        let kept: Vec<u64> = ids.into_iter().filter(|id| *id != ticket_id).collect();
        if kept.is_empty() {
            self.holder_tickets.remove(account);
        } else {
            self.holder_tickets.insert(account.clone(), kept);
        }
    }

    fn remove_from_bid_index(&mut self, series_id: u64, bid_id: u64) {
        let Some(mut ids) = self.series_bids.get(&series_id).cloned() else {
            return;
        };
        ids.retain(|id| *id != bid_id);
        if ids.is_empty() {
            self.series_bids.remove(&series_id);
        } else {
            self.series_bids.insert(series_id, ids);
        }
    }

    /// One read and one write of the index for a whole sweep.
    fn remove_many_from_book(&mut self, series_id: u64, ids: &[u64]) {
        if ids.is_empty() {
            return;
        }
        let Some(current) = self.series_offers.get(&series_id).cloned() else {
            return;
        };
        let kept: Vec<u64> = current
            .into_iter()
            .filter(|id| !ids.contains(id))
            .collect();
        if kept.is_empty() {
            self.series_offers.remove(&series_id);
        } else {
            self.series_offers.insert(series_id, kept);
        }
    }

    fn debit(&mut self, account: &AccountId, amount: u128) {
        let balance = self.balances.get(account).copied().unwrap_or(0);
        require!(balance >= amount, "insufficient available balance");
        self.balances.insert(account.clone(), balance - amount);
        self.total_available = self
            .total_available
            .checked_sub(amount)
            .expect("available total underflow");
    }
}

/// A listing's name is read by everything that follows the chain — explorers,
/// indexers, a wallet showing a ticker — and none of them can be assumed to
/// have escaped it. So the vocabulary is fixed at the only boundary that
/// matters, and the length with it: storage is someone else's rent.
fn is_tag(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn premium_per_unit(premium: u128, quantity: u128) -> u128 {
    premium
        .checked_mul(UNIT_PRECISION)
        .and_then(|value| value.checked_div(quantity))
        .unwrap_or(u128::MAX)
}

fn unit_premium(offer: &Offer) -> u128 {
    premium_per_unit(offer.premium, offer.quantity)
}

#[cfg(test)]
mod tests {
    use super::*;
    use near_sdk::test_utils::{accounts, VMContextBuilder};
    use near_sdk::testing_env;
    use near_sdk::NearToken;

    fn outsider() -> AccountId {
        "outsider.testnet".parse().unwrap()
    }

    const SCALE: u128 = 1_000_000;
    const T0: u64 = 1_700_000_000_000_000_000;
    /// The contract's own account. A pool-written ticket is the only ticket
    /// whose maker is this id, so the tests need it fixed rather than defaulted.
    /// Outside the six `accounts()` names, so it can never also be a user.
    fn vault_account() -> AccountId {
        "vault.testnet".parse().unwrap()
    }

    fn context(predecessor: AccountId, timestamp: u64) -> VMContextBuilder {
        let mut builder = VMContextBuilder::new();
        builder
            .current_account_id(vault_account())
            .predecessor_account_id(predecessor)
            .block_timestamp(timestamp)
            .attached_deposit(NearToken::from_yoctonear(0));
        builder
    }

    /// The test runner charges for storage at the same per-byte price as the
    /// chain, so a venue write that now prices its own state needs the deposit in
    /// the context. Paying for a whole series covers any call that creates one.
    fn paid(predecessor: AccountId, timestamp: u64) -> VMContextBuilder {
        let bytes = SERIES_STATE_BYTES + MAX_QUOTES_PER_CALL as u64 * QUOTE_STATE_BYTES;
        let mut builder = context(predecessor, timestamp);
        builder.attached_deposit(storage_price_per_byte().saturating_mul(bytes as u128));
        builder
    }

    fn setup() -> Contract {
        testing_env!(context(accounts(0), T0).build());
        Contract::new(accounts(0), accounts(1), accounts(2))
    }

    fn fund(contract: &mut Contract, account: AccountId, amount: u128) {
        testing_env!(context(accounts(1), T0).build());
        let unused = contract.ft_on_transfer(account, U128(amount), "deposit".to_string());
        assert_eq!(unused, "0");
    }

    fn deposit_to_pool(contract: &mut Contract, lp: AccountId, amount: u128) {
        testing_env!(context(accounts(1), T0).build());
        let unused = contract.ft_on_transfer(lp, U128(amount), "pool-deposit".to_string());
        assert_eq!(unused, "0");
    }

    /// Any positive balance moves the venue's earned margin to the venue account.
    fn sweep(contract: &mut Contract) -> u128 {
        testing_env!(context(accounts(0), T0).build());
        contract.pool_sweep().0
    }

    /// What the venue charges one contract at `mid` tokens, under the contract's
    /// own terms — so a fee change moves these tests with it instead of leaving a
    /// number that was true the day they were written.
    fn entry_of(mid: u128) -> u128 {
        fees::Terms::STANDARD.entry_premium(SCALE, mid * SCALE, SCALE).unwrap()
    }
    fn cut_of(mid: u128) -> u128 {
        fees::Terms::STANDARD.venue_cut(entry_of(mid)).unwrap()
    }
    fn exit_of(mid: u128) -> u128 {
        fees::Terms::STANDARD.exit_value(SCALE, mid * SCALE, SCALE).unwrap()
    }

    fn series(contract: &mut Contract) -> u64 {
        testing_env!(paid(accounts(0), T0).build());
        contract.create_series(
            "ETH".to_string(),
            SpreadKind::Put,
            T0 + 60_000_000_000,
            T0 + 120_000_000_000,
            T0 + 660_000_000_000,
            U128(100 * SCALE),
            U128(110 * SCALE),
            U128(SCALE),
            5_000,
            "ETH-USD".to_string(),
        )
    }

    /// The venue names the price the vault will trade at. Every pool trade in
    /// these tests has to answer to a quote the venue published for itself, so
    /// this is the line between a vault and a piñata. It stamps at the current
    /// block time and leaves the clock where it found it.
    fn quote(contract: &mut Contract, sid: u64, mid: u128) {
        let stamp = env::block_timestamp();
        testing_env!(paid(accounts(0), stamp).build());
        contract.set_pool_quote(sid, U128(mid), stamp);
    }

    fn refused<T>(f: impl FnOnce() -> T) -> bool {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).is_err()
    }

    #[test]
    fn a_series_name_is_a_tag_and_not_a_document() {
        fn listed(underlying: &str, feed: &str) -> Result<u64, ()> {
            let mut contract = setup();
            testing_env!(paid(accounts(0), T0).build());
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                contract.create_series(
                    underlying.to_string(),
                    SpreadKind::Put,
                    T0 + 60_000_000_000,
                    T0 + 120_000_000_000,
                    T0 + 660_000_000_000,
                    U128(100 * SCALE),
                    U128(110 * SCALE),
                    U128(SCALE),
                    5_000,
                    feed.to_string(),
                )
            }))
            .map_err(|_| ())
        }

        assert_eq!(listed("ETH", "ETH-USD"), Ok(0));
        assert_eq!(listed("BTC.2", "bybit_perp-btc_usdt"), Ok(0));
        // Markup is not a ticker. Whoever reads it next did not choose to render
        // it, so it never gets in.
        assert_eq!(listed("<img src=x onerror=alert(1)>", "ETH-USD"), Err(()));
        assert_eq!(listed("ETH", "ETH-USD; DROP"), Err(()));
        assert_eq!(listed("ETH", ""), Err(()), "an unnamed feed is not a feed");
        assert_eq!(listed("", "ETH-USD"), Err(()));
        assert_eq!(listed(&"x".repeat(33), "ETH-USD"), Err(()));
    }

    #[test]
    fn rejects_deposits_from_wrong_token() {
        let mut contract = setup();
        testing_env!(context(outsider(), T0).build());
        assert_eq!(
            contract.ft_on_transfer(accounts(3), U128(1_000_000), "deposit".to_string()),
            "1000000"
        );
        assert_eq!(contract.get_balance(accounts(3)).0, 0);
    }

    #[test]
    fn fill_reserves_both_sides_and_settles_exactly() {
        let mut contract = setup();
        let series_id = series(&mut contract);
        fund(&mut contract, accounts(3), 100 * SCALE);
        fund(&mut contract, accounts(4), 100 * SCALE);

        testing_env!(context(accounts(4), T0).build());
        let offer_id = contract.post_offer(
            series_id,
            U128(SCALE),
            U128(3 * SCALE),
            None,
            T0 + 30_000_000_000,
        );
        assert_eq!(contract.get_balance(accounts(4)).0, 90 * SCALE);

        testing_env!(context(accounts(3), T0).build());
        let ticket_id = contract.fill_offer(offer_id);
        assert_eq!(contract.get_balance(accounts(3)).0, 97 * SCALE);
        assert_eq!(contract.stats().total_ticket_collateral.0, 10 * SCALE);
        assert_eq!(contract.stats().total_ticket_premium.0, 3 * SCALE);
        assert_eq!(contract.expected_custody().0, 200 * SCALE);

        let mut fixed = contract.get_series(series_id).unwrap();
        fixed.settlement = Settlement::Fixed {
            price: 105 * SCALE,
            publish_time_ns: T0 + 60_000_000_000,
        };
        contract.series.insert(series_id, fixed);

        testing_env!(context(outsider(), T0 + 120_000_000_000).build());
        let (buyer, maker) = contract.settle_ticket(ticket_id);
        assert_eq!(buyer.0, 5 * SCALE);
        assert_eq!(maker.0, 8 * SCALE);
        assert_eq!(contract.get_balance(accounts(3)).0, 102 * SCALE);
        assert_eq!(contract.get_balance(accounts(4)).0, 98 * SCALE);
        assert_eq!(contract.expected_custody().0, 200 * SCALE);
        assert_eq!(contract.stats().total_ticket_collateral.0, 0);
        assert_eq!(contract.stats().total_ticket_premium.0, 0);
    }

    #[test]
    fn void_refunds_premium_and_collateral() {
        let mut contract = setup();
        let series_id = series(&mut contract);
        fund(&mut contract, accounts(3), 20 * SCALE);
        fund(&mut contract, accounts(4), 20 * SCALE);
        testing_env!(context(accounts(4), T0).build());
        let offer_id = contract.post_offer(
            series_id,
            U128(SCALE),
            U128(2 * SCALE),
            None,
            T0 + 30_000_000_000,
        );
        testing_env!(context(accounts(3), T0).build());
        let ticket_id = contract.fill_offer(offer_id);

        testing_env!(context(outsider(), T0 + 700_000_000_000).build());
        contract.void_series(series_id);
        contract.settle_ticket(ticket_id);
        assert_eq!(contract.get_balance(accounts(3)).0, 20 * SCALE);
        assert_eq!(contract.get_balance(accounts(4)).0, 20 * SCALE);
        assert_eq!(contract.expected_custody().0, 40 * SCALE);
    }

    #[test]
    fn pause_blocks_risk_actions_but_not_settlement() {
        let mut contract = setup();
        let series_id = series(&mut contract);
        fund(&mut contract, accounts(3), 20 * SCALE);
        fund(&mut contract, accounts(4), 20 * SCALE);
        testing_env!(context(accounts(4), T0).build());
        let offer_id = contract.post_offer(
            series_id,
            U128(SCALE),
            U128(2 * SCALE),
            None,
            T0 + 30_000_000_000,
        );
        testing_env!(context(accounts(3), T0).build());
        let ticket_id = contract.fill_offer(offer_id);

        testing_env!(context(accounts(0), T0).build());
        contract.set_paused(true);
        testing_env!(context(accounts(4), T0).build());
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            contract.post_offer(
                series_id,
                U128(SCALE),
                U128(2 * SCALE),
                None,
                T0 + 30_000_000_000,
            )
        }))
        .is_err());
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            contract.withdraw(U128(SCALE))
        }))
        .is_err());

        let mut fixed = contract.get_series(series_id).unwrap();
        fixed.settlement = Settlement::Fixed {
            price: 105 * SCALE,
            publish_time_ns: T0 + 60_000_000_000,
        };
        contract.series.insert(series_id, fixed);
        testing_env!(context(outsider(), T0 + 120_000_000_000).build());
        contract.settle_ticket(ticket_id);
        assert_eq!(contract.get_balance(accounts(3)).0, 23 * SCALE);
    }

    #[test]
    fn cannot_fill_expired_or_duplicate_offers() {
        let mut contract = setup();
        let series_id = series(&mut contract);
        fund(&mut contract, accounts(3), 20 * SCALE);
        fund(&mut contract, accounts(4), 20 * SCALE);
        testing_env!(context(accounts(4), T0).build());
        let offer_id = contract.post_offer(
            series_id,
            U128(SCALE),
            U128(2 * SCALE),
            None,
            T0 + 30_000_000_000,
        );
        testing_env!(context(accounts(3), T0).build());
        contract.fill_offer(offer_id);
        testing_env!(context(accounts(3), T0).build());
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            contract.fill_offer(offer_id)
        }))
        .is_err());
    }

    fn call_series(contract: &mut Contract) -> u64 {
        testing_env!(paid(accounts(0), T0).build());
        contract.create_series(
            "ETH".to_string(),
            SpreadKind::Call,
            T0 + 60_000_000_000,
            T0 + 120_000_000_000,
            T0 + 660_000_000_000,
            U128(100 * SCALE),
            U128(110 * SCALE),
            U128(SCALE),
            5_000,
            "ETH-USD".to_string(),
        )
    }

    fn post(
        contract: &mut Contract,
        series_id: u64,
        maker: AccountId,
        quantity: u128,
        premium: u128,
        taker: Option<AccountId>,
        expires_ns: u64,
    ) -> u64 {
        testing_env!(context(maker, T0).build());
        contract.post_offer(series_id, U128(quantity), U128(premium), taker, expires_ns)
    }

    #[test]
    fn market_order_sweeps_by_unit_premium_not_absolute() {
        let mut contract = setup();
        let sid = series(&mut contract);
        fund(&mut contract, accounts(3), 200 * SCALE);
        fund(&mut contract, accounts(4), 200 * SCALE);
        fund(&mut contract, accounts(5), 200 * SCALE);

        // Dear in absolute terms but cheap per unit: unit premium 3.
        let big = post(
            &mut contract,
            sid,
            accounts(4),
            10 * SCALE,
            30 * SCALE,
            None,
            T0 + 30_000_000_000,
        );
        // Cheap in absolute terms but dear per unit: unit premium 4.
        let small = post(
            &mut contract,
            sid,
            accounts(5),
            SCALE,
            4 * SCALE,
            None,
            T0 + 30_000_000_000,
        );

        testing_env!(context(accounts(3), T0).build());
        let fill = contract.fill_market_order(
            sid,
            U128(11 * SCALE),
            U128(40 * SCALE),
            Some(U128(11 * SCALE)),
        );
        assert_eq!(fill.filled_quantity.0, 11 * SCALE);
        assert_eq!(fill.unfilled_quantity.0, 0);
        assert_eq!(fill.total_premium.0, 34 * SCALE);
        assert_eq!(fill.worst_unit_premium.0, 4 * SCALE);
        assert_eq!(fill.tickets, vec![0, 1]);
        assert!(contract.get_offer(big).is_none());
        assert!(contract.get_offer(small).is_none());
        assert_eq!(contract.stats().open_ticket_count, 2);
        assert_eq!(contract.get_balance(accounts(3)).0, 200 * SCALE - 34 * SCALE);
        assert_eq!(contract.expected_custody().0, 600 * SCALE);
    }

    #[test]
    fn market_order_stops_at_premium_budget_and_reports_slippage() {
        let mut contract = setup();
        let sid = series(&mut contract);
        for id in [3usize, 4, 5] {
            fund(&mut contract, accounts(id), 100 * SCALE);
        }
        post(
            &mut contract,
            sid,
            accounts(4),
            SCALE,
            2 * SCALE,
            None,
            T0 + 30_000_000_000,
        );
        post(
            &mut contract,
            sid,
            accounts(5),
            SCALE,
            9 * SCALE,
            None,
            T0 + 30_000_000_000,
        );

        testing_env!(context(accounts(3), T0).build());
        let fill = contract.fill_market_order(sid, U128(2 * SCALE), U128(5 * SCALE), None);
        assert_eq!(fill.filled_quantity.0, SCALE);
        assert_eq!(fill.unfilled_quantity.0, SCALE);
        assert_eq!(fill.total_premium.0, 2 * SCALE);
        assert_eq!(fill.worst_unit_premium.0, 2 * SCALE);
        assert_eq!(fill.tickets.len(), 1);
        assert_eq!(contract.get_book(sid, 50).len(), 1);
        assert_eq!(contract.expected_custody().0, 300 * SCALE);
    }

    #[test]
    fn market_order_skips_reserved_expired_and_own_offers() {
        let mut contract = setup();
        let sid = series(&mut contract);
        for id in [3usize, 4, 5] {
            fund(&mut contract, accounts(id), 100 * SCALE);
        }
        let live = post(
            &mut contract,
            sid,
            accounts(4),
            SCALE,
            2 * SCALE,
            None,
            T0 + 55_000_000_000,
        );
        post(
            &mut contract,
            sid,
            accounts(5),
            SCALE,
            SCALE,
            Some(outsider()),
            T0 + 55_000_000_000,
        );
        let stale = post(
            &mut contract,
            sid,
            accounts(5),
            SCALE,
            SCALE,
            None,
            T0 + 30_000_000_000,
        );
        let own = post(
            &mut contract,
            sid,
            accounts(3),
            SCALE,
            SCALE,
            None,
            T0 + 55_000_000_000,
        );

        // Buyer acts after `stale` has expired but before the series does.
        testing_env!(context(accounts(3), T0 + 40_000_000_000).build());
        let fill = contract.fill_market_order(sid, U128(4 * SCALE), U128(50 * SCALE), None);
        assert_eq!(fill.filled_quantity.0, SCALE);
        assert_eq!(fill.total_premium.0, 2 * SCALE);
        assert_eq!(fill.unfilled_quantity.0, 3 * SCALE);
        assert!(contract.get_offer(live).is_none());
        assert!(contract.get_offer(stale).is_some());
        assert!(contract.get_offer(own).is_some());
        // The book hides the expired offer but still lists the reserved one.
        assert_eq!(
            contract
                .get_book(sid, 50)
                .iter()
                .map(|o| o.offer_id)
                .collect::<Vec<_>>(),
            vec![1, 3]
        );
        assert_eq!(contract.expected_custody().0, 300 * SCALE);
    }

    #[test]
    fn a_held_ticket_pays_the_mid_and_gives_up_the_exit() {
        let mut contract = setup();
        let sid = series(&mut contract);
        deposit_to_pool(&mut contract, accounts(5), 1_000 * SCALE);
        fund(&mut contract, accounts(3), 100 * SCALE);
        quote(&mut contract, sid, 3 * SCALE);

        testing_env!(context(accounts(3), T0).build());
        let ticket_id = contract.pool_buy_held(sid, U128(SCALE), U128(4 * SCALE));
        let ticket = contract.get_ticket(ticket_id).unwrap();
        // No venue margin at all: the holder pays the mid and the pool keeps it.
        assert_eq!(ticket.premium, 3 * SCALE);
        assert_eq!(ticket.venue_cut, 0);
        assert!(ticket.held);
        assert_eq!(contract.get_pool_state().venue_locked.0, 0);
        // A plain purchase at the same mid still pays the venue's schedule.
        assert!(entry_of(3) > 3 * SCALE, "the ordinary ticket is loaded");

        // The commitment is the fee: neither exit leaves.
        testing_env!(context(accounts(3), T0 + 1_000_000_000).build());
        assert!(refused(|| contract.pool_close(ticket_id, None)));
        assert!(refused(|| contract.close_ticket(ticket_id, None)));
        // And the ticket is still there, unclosed.
        assert!(contract.get_ticket(ticket_id).is_some());

        // Expiry is unchanged: the payoff, at the fixed price, with no haircut.
        let mut fixed = contract.get_series(sid).unwrap();
        fixed.settlement = Settlement::Fixed {
            price: 95 * SCALE,
            publish_time_ns: T0 + 60_000_000_000,
        };
        contract.series.insert(sid, fixed);
        testing_env!(context(accounts(3), T0 + 700_000_000_000).build());
        let (buyer, _) = contract.settle_ticket(ticket_id);
        assert_eq!(buyer.0, 10 * SCALE, "the put pays its width less the two strikes");
        assert!(contract.get_ticket(ticket_id).is_none());
    }

    #[test]
    fn market_order_rejects_when_budget_or_minimum_cannot_be_met() {
        let mut contract = setup();
        let sid = series(&mut contract);
        fund(&mut contract, accounts(3), 100 * SCALE);
        fund(&mut contract, accounts(4), 100 * SCALE);
        post(
            &mut contract,
            sid,
            accounts(4),
            SCALE,
            5 * SCALE,
            None,
            T0 + 30_000_000_000,
        );

        testing_env!(context(accounts(3), T0).build());
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            contract.fill_market_order(sid, U128(SCALE), U128(2 * SCALE), None)
        }))
        .is_err());
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            contract.fill_market_order(sid, U128(2 * SCALE), U128(50 * SCALE), Some(U128(2 * SCALE)))
        }))
        .is_err());
        // A rejected order must leave no trace of partial state.
        assert_eq!(contract.get_balance(accounts(3)).0, 100 * SCALE);
        assert_eq!(contract.stats().open_ticket_count, 0);
        assert_eq!(contract.stats().open_offer_count, 1);
    }

    #[test]
    fn market_order_requires_funds_and_an_open_series() {
        let mut contract = setup();
        let sid = series(&mut contract);
        fund(&mut contract, accounts(4), 100 * SCALE);
        post(
            &mut contract,
            sid,
            accounts(4),
            SCALE,
            2 * SCALE,
            None,
            T0 + 30_000_000_000,
        );

        testing_env!(context(accounts(3), T0).build());
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            contract.fill_market_order(sid, U128(SCALE), U128(10 * SCALE), None)
        }))
        .is_err());
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            contract.fill_market_order(sid + 7, U128(SCALE), U128(10 * SCALE), None)
        }))
        .is_err());
        // Filling past expiry is refused even though offers are still live.
        testing_env!(context(accounts(3), T0 + 61_000_000_000).build());
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            contract.fill_market_order(sid, U128(SCALE), U128(10 * SCALE), None)
        }))
        .is_err());
        assert_eq!(contract.stats().open_ticket_count, 0);
        assert_eq!(contract.expected_custody().0, 100 * SCALE);
    }

    #[test]
    fn expired_offers_cannot_brick_a_series_book() {
        let mut contract = setup();
        let sid = series(&mut contract);
        fund(&mut contract, accounts(4), 100_000 * SCALE);
        fund(&mut contract, accounts(3), 100 * SCALE);

        // Fill the per-series cap with quotes that expire almost immediately.
        let mut ids = Vec::new();
        for _ in 0..MAX_OFFERS_PER_SERIES {
            ids.push(post(
                &mut contract,
                sid,
                accounts(4),
                1,
                1,
                None,
                T0 + 1_000_000_000,
            ));
        }
        let balance_at_cap = contract.get_balance(accounts(4)).0;
        let collateral_at_cap = contract.stats().total_offer_collateral.0;
        testing_env!(context(accounts(3), T0).build());
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            contract.post_offer(sid, U128(1), U128(1), None, T0 + 1_000_000_000)
        }))
        .is_err());
        // Rejection at the cap must not move collateral or balances.
        assert_eq!(contract.get_balance(accounts(4)).0, balance_at_cap);
        assert_eq!(
            contract.stats().total_offer_collateral.0,
            collateral_at_cap
        );

        testing_env!(context(accounts(3), T0 + 2_000_000_000).build());
        assert!(contract.get_book(sid, 50).is_empty());
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                contract.fill_market_order(sid, U128(4), U128(10), None)
            }))
            .is_err(),
            "an all-expired book must reject a sweep"
        );
        // Prune is bounded and repeatable.
        assert_eq!(contract.prune_offers(sid, Some(50)), 50);
        assert_eq!(contract.prune_offers(sid, Some(50)), 50);
        assert_eq!(contract.prune_offers(sid, Some(50)), 50);
        assert_eq!(contract.prune_offers(sid, Some(50)), 50);
        assert_eq!(contract.prune_offers(sid, Some(50)), 0);
        assert_eq!(
            contract.stats().total_offer_collateral.0,
            collateral_at_cap,
            "pruning the index must not release collateral"
        );
        assert_eq!(contract.get_balance(accounts(4)).0, balance_at_cap);

        // Capacity is back, and the maker can still recover each reservation.
        testing_env!(context(accounts(3), T0 + 2_000_000_000).build());
        let fresh = contract.post_offer(sid, U128(SCALE), U128(SCALE), None, T0 + 30_000_000_000);
        assert_eq!(contract.get_book(sid, 50).len(), 1);
        testing_env!(context(accounts(4), T0 + 2_000_000_000).build());
        contract.cancel_offer(ids[0]);
        assert!(contract.get_offer(ids[0]).is_none());
        assert_eq!(
            contract.get_balance(accounts(4)).0,
            balance_at_cap + collateral_for(&contract, 1),
            "a pruned offer stays cancellable"
        );
        testing_env!(context(accounts(3), T0 + 2_000_000_000).build());
        contract.cancel_offer(fresh);
        assert_eq!(contract.expected_custody().0, 100_100 * SCALE);
    }

    fn collateral_for(contract: &Contract, quantity: u128) -> u128 {
        let series = contract.get_series(0).unwrap();
        accounting::max_payout(
            quantity,
            series.high_strike - series.low_strike,
            series.price_scale,
        )
        .unwrap()
    }

    #[test]
    fn book_view_and_sweep_use_the_same_order() {
        let mut contract = setup();
        let sid = series(&mut contract);
        for id in [3usize, 4, 5] {
            fund(&mut contract, accounts(id), 100 * SCALE);
        }
        post(
            &mut contract, sid, accounts(4), 2 * SCALE, 7 * SCALE, None, T0 + 30_000_000_000,
        );
        post(
            &mut contract,
            sid,
            accounts(5),
            SCALE,
            4 * SCALE,
            None,
            T0 + 30_000_000_000,
        );
        testing_env!(context(accounts(3), T0).build());
        let book = contract.get_book(sid, 50);
        let order = book
            .iter()
            .map(|offer| offer.offer_id)
            .collect::<Vec<_>>();
        let dearest_unit = book.last().expect("book has levels").unit_premium.0;
        let fill = contract.fill_market_order(sid, U128(3 * SCALE), U128(30 * SCALE), None);
        assert_eq!(order, vec![0, 1], "ascending unit premium");
        assert_eq!(fill.filled_quantity.0, 3 * SCALE);
        assert_eq!(fill.total_premium.0, 11 * SCALE);
        assert_eq!(
            fill.worst_unit_premium.0, dearest_unit,
            "the report must name the dearest level the book advertised"
        );
    }

    #[test]
    fn book_view_is_sorted_and_bounded() {
        let mut contract = setup();
        let sid = series(&mut contract);
        fund(&mut contract, accounts(4), 100 * SCALE);
        fund(&mut contract, accounts(5), 100 * SCALE);
        fund(&mut contract, outsider(), 100 * SCALE);
        post(
            &mut contract,
            sid,
            accounts(4),
            2 * SCALE,
            6 * SCALE,
            None,
            T0 + 30_000_000_000,
        );
        post(
            &mut contract,
            sid,
            accounts(5),
            SCALE,
            2 * SCALE,
            None,
            T0 + 30_000_000_000,
        );
        post(
            &mut contract,
            sid,
            outsider(),
            SCALE,
            3 * SCALE,
            Some(accounts(3)),
            T0 + 30_000_000_000,
        );

        testing_env!(context(accounts(3), T0).build());
        // Unit premiums are 3, 2 and 3: cheapest first, ties by offer id.
        let book = contract.get_book(sid, 2);
        assert_eq!(
            book.iter().map(|o| o.offer_id).collect::<Vec<_>>(),
            vec![1, 0]
        );
        let full = contract.get_book(sid, 50);
        assert_eq!(full.len(), 3);
        assert_eq!(full[2].offer_id, 2);
        assert_eq!(full[2].taker, Some(accounts(3)));
        assert!(contract.get_book(99, 50).is_empty());
    }

    #[test]
    fn call_spreads_pay_on_the_upside_and_conserve_value() {
        let mut contract = setup();
        let sid = call_series(&mut contract);
        fund(&mut contract, accounts(3), 100 * SCALE);
        fund(&mut contract, accounts(4), 100 * SCALE);
        let offer = post(
            &mut contract,
            sid,
            accounts(4),
            SCALE,
            3 * SCALE,
            None,
            T0 + 30_000_000_000,
        );
        testing_env!(context(accounts(3), T0).build());
        let ticket = contract.fill_offer(offer);

        let mut fixed = contract.get_series(sid).unwrap();
        fixed.settlement = Settlement::Fixed {
            price: 105 * SCALE,
            publish_time_ns: T0 + 60_000_000_000,
        };
        contract.series.insert(sid, fixed);
        testing_env!(context(outsider(), T0 + 120_000_000_000).build());
        let (buyer, maker) = contract.settle_ticket(ticket);
        assert_eq!(buyer.0, 5 * SCALE, "105 in a 100/110 call spread pays 5");
        assert_eq!(maker.0, 8 * SCALE);
        assert_eq!(contract.expected_custody().0, 200 * SCALE);

        // Above the cap, the payoff stops at the width. A fresh series, because
        // a fixed series cannot take new offers.
        let sid2 = call_series(&mut contract);
        let second = post(
            &mut contract,
            sid2,
            accounts(4),
            SCALE,
            2 * SCALE,
            None,
            T0 + 30_000_000_000,
        );
        testing_env!(context(accounts(3), T0).build());
        let ticket2 = contract.fill_offer(second);
        let mut fixed2 = contract.get_series(sid2).unwrap();
        fixed2.settlement = Settlement::Fixed {
            price: 130 * SCALE,
            publish_time_ns: T0 + 60_000_000_000,
        };
        contract.series.insert(sid2, fixed2);
        testing_env!(context(outsider(), T0 + 120_000_000_000).build());
        let (buyer2, _) = contract.settle_ticket(ticket2);
        assert_eq!(buyer2.0, 10 * SCALE, "capped at the 10 point width");
    }

    #[test]
    fn bids_escrow_premium_and_a_seller_can_take_them() {
        let mut contract = setup();
        let sid = series(&mut contract);
        fund(&mut contract, accounts(3), 100 * SCALE);
        fund(&mut contract, accounts(4), 100 * SCALE);

        testing_env!(context(accounts(3), T0).build());
        let bid = contract.post_bid(
            sid,
            U128(SCALE),
            U128(3 * SCALE),
            T0 + 30_000_000_000,
        );
        assert_eq!(contract.get_balance(accounts(3)).0, 97 * SCALE, "premium escrowed");
        assert_eq!(contract.stats().total_bid_escrow.0, 3 * SCALE);
        assert_eq!(contract.expected_custody().0, 200 * SCALE);
        let bids = contract.get_bids(sid, 10);
        assert_eq!(bids.len(), 1);
        assert_eq!(bids[0].bid_id, bid);
        assert_eq!(bids[0].unit_premium.0, 3 * SCALE);

        // The seller takes the bid and becomes the maker.
        testing_env!(context(accounts(4), T0).build());
        let ticket = contract.fill_bid(bid);
        assert_eq!(contract.get_balance(accounts(4)).0, 90 * SCALE, "collateral reserved");
        assert_eq!(contract.stats().total_bid_escrow.0, 0);
        assert_eq!(contract.stats().total_ticket_premium.0, 3 * SCALE);
        assert_eq!(contract.stats().open_bid_count, 0);
        assert_eq!(contract.expected_custody().0, 200 * SCALE);
        let stored = contract.get_ticket(ticket).unwrap();
        assert_eq!(stored.buyer, accounts(3));
        assert_eq!(stored.maker, accounts(4));

        let mut fixed = contract.get_series(sid).unwrap();
        fixed.settlement = Settlement::Fixed {
            price: 105 * SCALE,
            publish_time_ns: T0 + 60_000_000_000,
        };
        contract.series.insert(sid, fixed);
        testing_env!(context(outsider(), T0 + 120_000_000_000).build());
        let (buyer, maker) = contract.settle_ticket(ticket);
        assert_eq!(buyer.0, 5 * SCALE);
        assert_eq!(maker.0, 8 * SCALE);
        assert_eq!(contract.get_balance(accounts(3)).0, 102 * SCALE);
        assert_eq!(contract.get_balance(accounts(4)).0, 98 * SCALE);
        assert_eq!(contract.expected_custody().0, 200 * SCALE);
    }

    #[test]
    fn cancelling_a_bid_returns_the_premium() {
        let mut contract = setup();
        let sid = series(&mut contract);
        fund(&mut contract, accounts(3), 20 * SCALE);
        testing_env!(context(accounts(3), T0).build());
        let bid = contract.post_bid(sid, U128(SCALE), U128(2 * SCALE), T0 + 30_000_000_000);
        assert_eq!(contract.get_balance(accounts(3)).0, 18 * SCALE);
        testing_env!(context(accounts(4), T0).build());
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            contract.cancel_bid(bid)
        }))
        .is_err());
        testing_env!(context(accounts(3), T0).build());
        contract.cancel_bid(bid);
        assert_eq!(contract.get_balance(accounts(3)).0, 20 * SCALE);
        assert_eq!(contract.get_bids(sid, 10).len(), 0);
        assert_eq!(contract.stats().open_bid_count, 0);
        assert_eq!(contract.expected_custody().0, 20 * SCALE);
    }

    #[test]
    fn closing_a_ticket_pays_the_seller_and_moves_the_position() {
        let mut contract = setup();
        let sid = series(&mut contract);
        for who in [3, 4, 5] {
            fund(&mut contract, accounts(who), 100 * SCALE);
        }

        testing_env!(context(accounts(3), T0).build());
        let bid = contract.post_bid(sid, U128(SCALE), U128(3 * SCALE), T0 + 30_000_000_000);
        testing_env!(context(accounts(4), T0).build());
        let ticket = contract.fill_bid(bid);
        assert_eq!(contract.get_ticket(ticket).unwrap().cost, 3 * SCALE);

        // A deeper bidder wants the same shape: one contract at four.
        testing_env!(context(accounts(5), T0 + 1_000_000_000).build());
        let _deeper = contract.post_bid(sid, U128(SCALE), U128(4 * SCALE), T0 + 40_000_000_000);

        testing_env!(context(accounts(3), T0 + 2_000_000_000).build());
        let proceeds = contract.close_ticket(ticket, Some(U128(4 * SCALE)));
        assert_eq!(proceeds.0, 4 * SCALE);

        let moved = contract.get_ticket(ticket).unwrap();
        assert_eq!(moved.buyer, accounts(5), "the bidder holds it now");
        assert_eq!(moved.maker, accounts(4), "the maker's obligation never moves");
        assert_eq!(moved.cost, 4 * SCALE, "the new holder's basis is their price");

        // Alice paid three and closed at four; the bidder's four left escrow.
        assert_eq!(contract.get_balance(accounts(3)).0, 101 * SCALE);
        assert_eq!(contract.get_balance(accounts(5)).0, 96 * SCALE);
        assert_eq!(contract.stats().total_bid_escrow.0, 0);
        assert_eq!(
            contract.stats().total_ticket_premium.0,
            3 * SCALE,
            "the maker is still owed the original premium"
        );
        assert!(contract.get_bids(sid, 10).is_empty());
        assert_eq!(contract.expected_custody().0, 300 * SCALE);

        // Positions come off the holder index, on both sides.
        let long = contract.get_positions(accounts(5), 10);
        assert_eq!(long.len(), 1);
        assert_eq!(long[0].side, "long");
        assert_eq!(long[0].ticket_id, ticket);
        assert_eq!(long[0].cost.0, 4 * SCALE);
        let short = contract.get_positions(accounts(4), 10);
        assert_eq!(short.len(), 1);
        assert_eq!(short[0].side, "short");
        assert!(contract.get_positions(accounts(3), 10).is_empty(), "closed out");

        // Settlement then pays whoever holds the ticket, not the original buyer.
        let mut fixed = contract.get_series(sid).unwrap();
        fixed.settlement = Settlement::Fixed {
            price: 105 * SCALE,
            publish_time_ns: T0 + 60_000_000_000,
        };
        contract.series.insert(sid, fixed);
        testing_env!(context(outsider(), T0 + 120_000_000_000).build());
        let (buyer, maker) = contract.settle_ticket(ticket);
        assert_eq!(buyer.0, 5 * SCALE);
        assert_eq!(maker.0, 8 * SCALE);
        assert_eq!(contract.get_balance(accounts(5)).0, 101 * SCALE);
        assert_eq!(contract.expected_custody().0, 300 * SCALE);
    }

    #[test]
    fn close_only_matches_a_whole_foreign_bid_above_the_floor() {
        let mut contract = setup();
        let sid = series(&mut contract);
        for who in [3, 4, 5] {
            fund(&mut contract, accounts(who), 100 * SCALE);
        }
        testing_env!(context(accounts(3), T0).build());
        let bid = contract.post_bid(sid, U128(SCALE), U128(3 * SCALE), T0 + 30_000_000_000);
        testing_env!(context(accounts(4), T0).build());
        let ticket = contract.fill_bid(bid);

        testing_env!(context(accounts(3), T0 + 1_000_000_000).build());
        // Her own bid is not a counterparty, and half the size is not a match.
        contract.post_bid(sid, U128(SCALE), U128(9 * SCALE), T0 + 40_000_000_000);
        contract.post_bid(sid, U128(SCALE / 2), U128(8 * SCALE), T0 + 40_000_000_000);
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            contract.close_ticket(ticket, None)
        }))
        .is_err());
        assert_eq!(contract.get_ticket(ticket).unwrap().buyer, accounts(3));

        // A fair price exists but the seller's floor is higher: no fill.
        testing_env!(context(accounts(5), T0 + 2_000_000_000).build());
        contract.post_bid(sid, U128(SCALE), U128(4 * SCALE), T0 + 40_000_000_000);
        testing_env!(context(accounts(3), T0 + 3_000_000_000).build());
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            contract.close_ticket(ticket, Some(U128(5 * SCALE)))
        }))
        .is_err());
        assert_eq!(
            contract.stats().open_bid_count,
            3,
            "a rejected close must not consume a bid"
        );
        assert_eq!(contract.expected_custody().0, 300 * SCALE);

        // Closing after the series is fixed is refused.
        let mut fixed = contract.get_series(sid).unwrap();
        fixed.settlement = Settlement::Fixed {
            price: 105 * SCALE,
            publish_time_ns: T0 + 60_000_000_000,
        };
        contract.series.insert(sid, fixed);
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            contract.close_ticket(ticket, None)
        }))
        .is_err());
    }

    /// One contract at `mid`, with a `budget` cap, bought by `buyer`. The venue
    /// quotes first: a buyer can only ever pay a price the vault named.
    fn buy_from(
        contract: &mut Contract,
        buyer: AccountId,
        sid: u64,
        mid: u128,
        budget: u128,
    ) -> u64 {
        if mid > 0 {
            quote(contract, sid, mid);
        }
        testing_env!(context(buyer, T0).build());
        contract.pool_buy(sid, U128(SCALE), U128(budget))
    }

    #[test]
    fn the_pool_writes_a_ticket_and_holds_only_its_own_worst_case() {
        let mut contract = setup();
        let sid = series(&mut contract);
        deposit_to_pool(&mut contract, accounts(5), 20 * SCALE);
        fund(&mut contract, accounts(3), 100 * SCALE);
        assert_eq!(contract.get_lp_shares(accounts(5)).0, 20 * SCALE);
        assert_eq!(
            contract.get_balance(accounts(5)).0,
            0,
            "vault money is not a tradable balance"
        );

        quote(&mut contract, sid, 3 * SCALE);
        testing_env!(context(accounts(3), T0).build());
        // One contract at a 3.00 mid, with a 4.00 budget: the entry is 3.12.
        let ticket = contract.pool_buy(sid, U128(SCALE), U128(4 * SCALE));

        let vault = contract.get_pool_state();
        assert_eq!(vault.equity.0, 23 * SCALE, "the mid joins equity");
        assert_eq!(vault.venue_locked.0, cut_of(3), "the margin does not");
        assert_eq!(vault.venue.0, 0);
        assert_eq!(
            vault.reserved.0,
            10 * SCALE,
            "the full worst case is held, not just the net of the premium"
        );
        assert_eq!(vault.free_equity.0, 13 * SCALE);
        assert_eq!(vault.utilisation_bps.0, 4_347);
        assert_eq!(vault.open_tickets, 1);
        assert_eq!(vault.margin_bps, 400);

        let stored = contract.get_ticket(ticket).unwrap();
        assert_eq!(stored.maker, vault_account(), "the vault is the maker");
        assert_eq!(stored.collateral, 10 * SCALE);
        assert_eq!(stored.premium, 3_120_000);
        assert_eq!(stored.venue_cut, 120_000);
        assert_eq!(stored.cost, stored.premium);

        assert_eq!(contract.get_balance(accounts(3)).0, 96_880_000);
        // The book buckets stay empty: a pool ticket's money lives in the vault.
        assert_eq!(contract.stats().total_ticket_collateral.0, 0);
        assert_eq!(contract.stats().total_ticket_premium.0, 0);
        assert_eq!(contract.stats().open_ticket_count, 1);
        assert_eq!(contract.expected_custody().0, 120 * SCALE);

        let positions = contract.get_positions(accounts(3), 10);
        assert_eq!(positions.len(), 1);
        assert_eq!(positions[0].side, "long");
        assert_eq!(positions[0].cost.0, 3_120_000, "the fee is inside the basis");
        assert_eq!(positions[0].collateral.0, 10 * SCALE);
        assert_eq!(
            positions[0].maker,
            vault_account(),
            "a holder must be able to see they are trading against the vault"
        );
        assert!(
            contract.get_positions(vault_account(), 10).is_empty(),
            "the vault is indexed by its own balance sheet, not per ticket"
        );
    }

    #[test]
    fn a_pool_entry_is_bounded_by_the_payoff_the_buyers_budget_and_the_equity() {
        let mut contract = setup();
        let sid = series(&mut contract);
        deposit_to_pool(&mut contract, accounts(5), 1_000 * SCALE);
        fund(&mut contract, accounts(3), 1_000 * SCALE);

        // A 10-point spread can never pay more than 10, so it may never cost
        // more than 10 either: an over-marked engine cannot overcharge a ticket.
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            buy_from(&mut contract, accounts(3), sid, 11 * SCALE, 100 * SCALE)
        }))
        .is_err());
        // The buyer's own slippage budget, tighter than the quote.
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            buy_from(&mut contract, accounts(3), sid, 3 * SCALE, 3 * SCALE)
        }))
        .is_err());
        // A series the venue has never quoted has no price to buy at — not a
        // refusal to trade cheaply, simply nothing on the other side.
        let dark = series(&mut contract);
        assert!(refused(|| buy_from(&mut contract, accounts(3), dark, 0, 10 * SCALE)));
        assert_eq!(
            contract.get_pool_state().open_tickets, 0,
            "and the unquoted series wrote nothing"
        );
        // A buyer without the balance cannot open a position, and a rejection
        // leaves the vault exactly as it was.
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            buy_from(&mut contract, accounts(4), sid, 3 * SCALE, 4 * SCALE)
        }))
        .is_err());
        assert_eq!(contract.get_pool_state().equity.0, 1_000 * SCALE);
        assert_eq!(contract.get_pool_state().reserved.0, 0);
        assert_eq!(contract.stats().open_ticket_count, 0);
        assert_eq!(contract.expected_custody().0, 2_000 * SCALE);

        // Capacity: a ticket reserves its whole 10 worst case and brings in 3 of
        // premium, so 14 of equity takes two of them and refuses the third.
        let mut tight = setup();
        let tid = series(&mut tight);
        deposit_to_pool(&mut tight, accounts(5), 14 * SCALE);
        fund(&mut tight, accounts(3), 100 * SCALE);
        buy_from(&mut tight, accounts(3), tid, 3 * SCALE, 10 * SCALE);
        buy_from(&mut tight, accounts(3), tid, 3 * SCALE, 10 * SCALE);
        assert_eq!(tight.get_pool_state().equity.0, 20 * SCALE);
        assert_eq!(tight.get_pool_state().reserved.0, 20 * SCALE);
        assert_eq!(tight.get_pool_state().free_equity.0, 0);
        assert_eq!(tight.get_pool_state().utilisation_bps.0, 10_000);
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            buy_from(&mut tight, accounts(3), tid, 3 * SCALE, 10 * SCALE)
        }))
        .is_err());
        assert_eq!(tight.get_pool_state().open_tickets, 2);
        assert_eq!(tight.get_balance(accounts(3)).0, 100 * SCALE - 6_240_000);
        assert_eq!(tight.expected_custody().0, 114 * SCALE);
    }

    #[test]
    fn a_pool_ticket_settles_out_of_equity_and_the_margin_is_then_earned() {
        let mut contract = setup();
        let sid = series(&mut contract);
        deposit_to_pool(&mut contract, accounts(5), 20 * SCALE);
        fund(&mut contract, accounts(3), 100 * SCALE);
        quote(&mut contract, sid, 3 * SCALE);
        testing_env!(context(accounts(3), T0).build());
        let ticket = contract.pool_buy(sid, U128(SCALE), U128(4 * SCALE));

        // Fixed below the low strike: the spread pays its full width.
        let mut fixed = contract.get_series(sid).unwrap();
        fixed.settlement = Settlement::Fixed {
            price: 95 * SCALE,
            publish_time_ns: T0 + 60_000_000_000,
        };
        contract.series.insert(sid, fixed);
        testing_env!(context(outsider(), T0 + 120_000_000_000).build());
        let (buyer, maker) = contract.settle_ticket(ticket);
        assert_eq!(buyer.0, 10 * SCALE);
        assert_eq!(maker.0, 0, "the vault keeps its premium, it has no escrow");

        let vault = contract.get_pool_state();
        assert_eq!(vault.equity.0, 13 * SCALE, "20 principal plus 3 kept minus 10 paid");
        assert_eq!(vault.reserved.0, 0);
        assert_eq!(vault.venue_locked.0, 0);
        assert_eq!(vault.venue.0, cut_of(3), "the ticket can no longer be voided");
        assert_eq!(vault.open_tickets, 0);
        assert_eq!(contract.get_balance(accounts(3)).0, 106_880_000);
        assert_eq!(contract.stats().open_ticket_count, 0);
        assert_eq!(contract.expected_custody().0, 120 * SCALE);

        // The sweep moves the fee from the vault's bucket to the venue's
        // balance. The LP's share price is untouched by it.
        let shares = contract.get_lp_shares(accounts(5)).0;
        assert_eq!(sweep(&mut contract), 120_000);
        assert_eq!(contract.get_balance(accounts(0)).0, 120_000);
        assert_eq!(contract.get_pool_state().equity.0, 13 * SCALE);
        assert_eq!(contract.get_pool_state().venue.0, 0);
        assert_eq!(contract.expected_custody().0, 120 * SCALE);
        // And the LP still owns the whole equity at par.
        testing_env!(context(accounts(5), T0).build());
        // The number the screen quotes before the click is the number the exit
        // pays — same floor, same operands, read-only.
        assert_eq!(contract.get_lp_value(accounts(5)).0, 13 * SCALE);
        assert_eq!(contract.get_lp_value(accounts(3)).0, 0, "an account with no shares has no exit value");
        assert_eq!(contract.pool_withdraw(U128(shares)).0, 13 * SCALE);
        assert_eq!(contract.expected_custody().0, 120 * SCALE);
    }

    #[test]
    fn the_vault_can_be_entered_with_money_already_at_the_venue() {
        let mut contract = setup();
        fund(&mut contract, accounts(3), 100 * SCALE);
        let custody = contract.expected_custody().0;
        assert_eq!(custody, 100 * SCALE);

        testing_env!(context(accounts(3), T0).build());
        assert_eq!(contract.pool_deposit_from_balance(U128(20 * SCALE)).0, 20 * SCALE);
        assert_eq!(contract.get_balance(accounts(3)).0, 80 * SCALE);
        assert_eq!(contract.get_pool_state().equity.0, 20 * SCALE);
        assert_eq!(contract.get_lp_shares(accounts(3)).0, 20 * SCALE);
        // Cash moved buckets, it did not move the venue: the sum is what the
        // token balance has to match, and entering the vault cannot break that.
        assert_eq!(contract.expected_custody().0, custody);

        // A second LP buys into the NAV, not at par: the first 20 are already
        // in, so the next 20 tokens are worth the same 20 shares only while
        // equity and shares are still 1:1.
        fund(&mut contract, accounts(4), 30 * SCALE);
        testing_env!(context(accounts(4), T0).build());
        assert_eq!(contract.pool_deposit_from_balance(U128(30 * SCALE)).0, 30 * SCALE);
        assert_eq!(contract.get_pool_state().shares.0, 50 * SCALE);
        assert_eq!(contract.get_lp_value(accounts(4)).0, 30 * SCALE);

        // Redeeming pays back into the same tradable balance it came from.
        assert_eq!(contract.pool_withdraw(U128(30 * SCALE)).0, 30 * SCALE);
        assert_eq!(contract.get_balance(accounts(4)).0, 30 * SCALE);
        // 130 entered the venue from two accounts and none of it left: the
        // vault moved money between buckets, never off the balance sheet.
        assert_eq!(contract.expected_custody().0, 130 * SCALE);

        // An account with nothing at the venue mints nothing.
        testing_env!(context(accounts(5), T0).build());
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            contract.pool_deposit_from_balance(U128(SCALE))
        }))
        .is_err());
        assert_eq!(contract.get_pool_state().equity.0, 20 * SCALE);
        assert_eq!(contract.get_lp_shares(accounts(5)).0, 0);
    }

    #[test]
    fn only_the_named_venue_can_sweep_and_sweeping_pays_nothing_while_open() {
        let mut contract = setup();
        let sid = series(&mut contract);
        deposit_to_pool(&mut contract, accounts(5), 20 * SCALE);
        fund(&mut contract, accounts(3), 100 * SCALE);
        quote(&mut contract, sid, 3 * SCALE);
        testing_env!(context(accounts(3), T0).build());
        contract.pool_buy(sid, U128(SCALE), U128(4 * SCALE));

        // A stranger cannot take the fee.
        testing_env!(context(accounts(4), T0).build());
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            contract.pool_sweep()
        }))
        .is_err());
        // The venue can ask, but an open ticket still owns that margin: a void
        // would have nothing to refund from.
        assert_eq!(sweep(&mut contract), 0);
        assert_eq!(contract.get_pool_state().venue_locked.0, cut_of(3));
        assert_eq!(contract.get_balance(accounts(0)).0, 0);

        // Hand the position back early and the same slice becomes takeable.
        quote(&mut contract, sid, 2 * SCALE);
        testing_env!(context(accounts(3), T0 + 1_000_000_000).build());
        let exit = contract.pool_close(
            contract.get_positions(accounts(3), 10)[0].ticket_id,
            // The seller's floor, named where the venue's price is not: taking
            // half of what the quote clears is a refusal, not a worse fill.
            Some(U128(900_000)),
        );
        assert_eq!(exit.0, 1_800_000, "2.00 mid less the 10% haircut");
        assert_eq!(contract.get_pool_state().venue.0, 120_000);
        assert_eq!(sweep(&mut contract), 120_000);
    }

    #[test]
    fn closing_a_pool_ticket_early_destroys_it_and_returns_the_reservation() {
        let mut contract = setup();
        let sid = series(&mut contract);
        deposit_to_pool(&mut contract, accounts(5), 20 * SCALE);
        fund(&mut contract, accounts(3), 100 * SCALE);
        quote(&mut contract, sid, 3 * SCALE);
        testing_env!(context(accounts(3), T0).build());
        let ticket = contract.pool_buy(sid, U128(SCALE), U128(4 * SCALE));

        quote(&mut contract, sid, 2 * SCALE);
        testing_env!(context(accounts(3), T0 + 1_000_000_000).build());
        let exit = contract.pool_close(ticket, None);
        assert_eq!(exit.0, exit_of(2));
        assert!(contract.get_ticket(ticket).is_none(), "the position is gone");
        assert!(contract.get_positions(accounts(3), 10).is_empty());
        assert_eq!(contract.get_pool_state().reserved.0, 0);
        assert_eq!(contract.get_pool_state().equity.0, 21_200_000);
        assert_eq!(contract.stats().open_ticket_count, 0);
        assert_eq!(contract.get_pool_state().open_tickets, 0);
        assert_eq!(contract.get_balance(accounts(3)).0, 98_680_000);
        assert_eq!(contract.expected_custody().0, 120 * SCALE);

        // Selling back at the same mid the entry used still costs the round
        // trip: the margin in, the haircut out.
        quote(&mut contract, sid, 3 * SCALE);
        testing_env!(context(accounts(3), T0 + 2_000_000_000).build());
        let again = contract.pool_buy(sid, U128(SCALE), U128(4 * SCALE));
        testing_env!(context(accounts(3), T0 + 3_000_000_000).build());
        let back = contract.pool_close(again, None);
        assert!(back.0 < 3_120_000, "a round trip cannot be free");
        assert_eq!(contract.expected_custody().0, 120 * SCALE);
    }

    #[test]
    fn a_void_refunds_the_full_premium_and_leaves_the_pool_as_it_started() {
        let mut contract = setup();
        let sid = series(&mut contract);
        deposit_to_pool(&mut contract, accounts(5), 20 * SCALE);
        fund(&mut contract, accounts(3), 100 * SCALE);
        quote(&mut contract, sid, 3 * SCALE);
        testing_env!(context(accounts(3), T0).build());
        let ticket = contract.pool_buy(sid, U128(SCALE), U128(4 * SCALE));

        testing_env!(context(outsider(), T0 + 700_000_000_000).build());
        contract.void_series(sid);
        let (buyer, maker) = contract.settle_ticket(ticket);
        assert_eq!(buyer.0, entry_of(3), "everything the buyer paid comes back");
        assert_eq!(maker.0, 0);
        assert_eq!(contract.get_balance(accounts(3)).0, 100 * SCALE);

        let vault = contract.get_pool_state();
        assert_eq!((vault.equity.0, vault.reserved.0), (20 * SCALE, 0));
        assert_eq!((vault.venue.0, vault.venue_locked.0), (0, 0), "no fee on a void");
        assert_eq!(vault.open_tickets, 0);
        assert_eq!(contract.expected_custody().0, 120 * SCALE);
        // The LP can still take everything out, which only works because the
        // refund was sourced from the two buckets that collected it.
        testing_env!(context(accounts(5), T0).build());
        let shares = contract.get_lp_shares(accounts(5)).0;
        assert_eq!(contract.pool_withdraw(U128(shares)).0, 20 * SCALE);
    }

    #[test]
    fn an_lp_cannot_leave_in_front_of_the_worst_case_the_pool_is_holding() {
        let mut contract = setup();
        let sid = series(&mut contract);
        deposit_to_pool(&mut contract, accounts(5), 14 * SCALE);
        fund(&mut contract, accounts(3), 100 * SCALE);
        let shares = contract.get_lp_shares(accounts(5)).0;
        let mut ticket = Vec::new();
        for _ in 0..2 {
            quote(&mut contract, sid, 3 * SCALE);
            testing_env!(context(accounts(3), T0).build());
            ticket.push(contract.pool_buy(sid, U128(SCALE), U128(4 * SCALE)));
        }
        assert_eq!(contract.get_pool_state().free_equity.0, 0);

        testing_env!(context(accounts(5), T0).build());
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            contract.pool_withdraw(U128(shares))
        }))
        .is_err());
        // Not even one share: the reservation is the whole open worst case.
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            contract.pool_withdraw(U128(SCALE))
        }))
        .is_err());
        assert_eq!(contract.get_lp_shares(accounts(5)).0, shares);

        // Both run off worthless, and the LP's exit is open again: the premium
        // they kept is exactly what the buyers lost.
        let mut fixed = contract.get_series(sid).unwrap();
        fixed.settlement = Settlement::Fixed {
            price: 120 * SCALE,
            publish_time_ns: T0 + 60_000_000_000,
        };
        contract.series.insert(sid, fixed);
        testing_env!(context(outsider(), T0 + 120_000_000_000).build());
        for id in ticket.clone() {
            let (buyer, _) = contract.settle_ticket(id);
            assert_eq!(buyer.0, 0);
        }
        assert_eq!(contract.get_pool_state().reserved.0, 0);
        testing_env!(context(accounts(5), T0).build());
        assert_eq!(contract.pool_withdraw(U128(shares)).0, 20 * SCALE);
        assert_eq!(sweep(&mut contract), 2 * cut_of(3));
        assert_eq!(contract.expected_custody().0, 114 * SCALE);
    }

    #[test]
    fn a_pool_obligation_follows_the_ticket_not_the_person_who_bought_it() {
        let mut contract = setup();
        let sid = series(&mut contract);
        deposit_to_pool(&mut contract, accounts(5), 20 * SCALE);
        for who in [3, 4] {
            fund(&mut contract, accounts(who), 100 * SCALE);
        }
        quote(&mut contract, sid, 3 * SCALE);
        testing_env!(context(accounts(3), T0).build());
        let ticket = contract.pool_buy(sid, U128(SCALE), U128(4 * SCALE));

        // Someone else bids for the whole position, and the holder sells into
        // the book rather than back to the vault.
        testing_env!(context(accounts(4), T0 + 1_000_000_000).build());
        let _bid = contract.post_bid(sid, U128(SCALE), U128(3 * SCALE), T0 + 40_000_000_000);
        testing_env!(context(accounts(3), T0 + 2_000_000_000).build());
        let proceeds = contract.close_ticket(ticket, None);
        assert_eq!(proceeds.0, 3 * SCALE);

        let moved = contract.get_ticket(ticket).unwrap();
        assert_eq!(moved.buyer, accounts(4));
        assert_eq!(moved.maker, vault_account(), "the vault still owes the payoff");
        assert_eq!(moved.premium, entry_of(3), "the entry the vault made is unchanged");
        assert_eq!(moved.venue_cut, cut_of(3));
        assert_eq!(moved.cost, 3 * SCALE, "the new holder's basis is their price");
        assert_eq!(
            contract.get_pool_state().reserved.0,
            10 * SCALE,
            "the worst case moved with the ticket, not with the person"
        );
        assert_eq!(contract.expected_custody().0, 220 * SCALE);

        let mut fixed = contract.get_series(sid).unwrap();
        fixed.settlement = Settlement::Fixed {
            price: 105 * SCALE,
            publish_time_ns: T0 + 60_000_000_000,
        };
        contract.series.insert(sid, fixed);
        testing_env!(context(outsider(), T0 + 120_000_000_000).build());
        let (buyer, maker) = contract.settle_ticket(ticket);
        assert_eq!(buyer.0, 5 * SCALE, "paid to whoever holds it");
        assert_eq!(maker.0, 0);
        assert_eq!(contract.get_balance(accounts(4)).0, 102 * SCALE);
        assert_eq!(contract.get_balance(accounts(3)).0, 99_880_000);
        let vault = contract.get_pool_state();
        assert_eq!(vault.equity.0, 18 * SCALE);
        assert_eq!((vault.reserved.0, vault.venue_locked.0), (0, 0));
        assert_eq!(contract.expected_custody().0, 220 * SCALE);
    }

    #[test]
    fn fee_terms_move_the_entry_price_but_never_the_worst_case() {
        let mut contract = setup();
        let sid = series(&mut contract);
        deposit_to_pool(&mut contract, accounts(5), 100 * SCALE);
        fund(&mut contract, accounts(3), 1_000 * SCALE);

        testing_env!(context(accounts(0), T0).build());
        contract.set_fee_terms(800, 2_000);
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            contract.set_fee_terms(10_000, 0)
        }))
        .is_err());
        // A stranger cannot set them.
        testing_env!(context(accounts(4), T0).build());
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            contract.set_fee_terms(0, 0)
        }))
        .is_err());

        quote(&mut contract, sid, 3 * SCALE);
        testing_env!(context(accounts(3), T0).build());
        let ticket = contract.pool_buy(sid, U128(SCALE), U128(10 * SCALE));
        assert_eq!(contract.get_pool_state().margin_bps, 800);
        // 3.00 mid plus 8% = 3.24, of which 0.24 is the venue's.
        let stored = contract.get_ticket(ticket).unwrap();
        assert_eq!(stored.premium, 3_240_000);
        assert_eq!(stored.venue_cut, 240_000);
        assert_eq!(stored.collateral, 10 * SCALE, "the risk is the same trade");
        assert_eq!(contract.get_pool_state().equity.0, 103 * SCALE);
        assert_eq!(contract.get_pool_state().reserved.0, 10 * SCALE);

        // Exiting at the same mid now gives back 80% of the mid.
        testing_env!(context(accounts(3), T0 + 1_000_000_000).build());
        let exit = contract.pool_close(ticket, None);
        assert_eq!(exit.0, 2_400_000);
        assert_eq!(contract.expected_custody().0, 1_100 * SCALE);
        assert_eq!(sweep(&mut contract), 2 * cut_of(3));
    }

    #[test]
    fn the_vault_rejects_a_deposit_it_cannot_price() {
        let mut contract = setup();
        // Zero never mints, and an unknown message is returned to the sender
        // rather than absorbed into the pool.
        testing_env!(context(accounts(1), T0).build());
        assert_eq!(
            contract.ft_on_transfer(accounts(3), U128(0), "pool-deposit".to_string()),
            "0"
        );
        assert_eq!(
            contract.ft_on_transfer(accounts(3), U128(SCALE), "poolles".to_string()),
            "1000000"
        );
        assert_eq!(contract.get_lp_shares(accounts(3)).0, 0);
        assert_eq!(contract.get_pool_state().equity.0, 0);
        assert_eq!(contract.get_pool_state().utilisation_bps.0, 0);
        assert_eq!(contract.expected_custody().0, 0);

        // An empty pool cannot write, however cheap the mark looks.
        fund(&mut contract, accounts(3), 100 * SCALE);
        let sid = series(&mut contract);
        quote(&mut contract, sid, SCALE);
        testing_env!(context(accounts(3), T0).build());
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            contract.pool_buy(sid, U128(SCALE), U128(10 * SCALE))
        }))
        .is_err());
        // And a share redemption for someone who holds nothing is refused.
        testing_env!(context(accounts(4), T0).build());
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            contract.pool_withdraw(U128(SCALE))
        }))
        .is_err());
    }

    /// One write for a whole ladder, with the same rules on every cell: a week
    /// of expiries is sixty cells per tenor, and a transaction per cell cannot
    /// keep seven tenors inside the quote age the venue promises.
    #[test]
    fn a_ladder_is_quoted_in_one_call() {
        let mut contract = setup();
        let a = series(&mut contract);
        let b = series(&mut contract);
        testing_env!(paid(accounts(0), T0).build());
        contract.set_pool_quotes(
            vec![a, b],
            vec![U128(3 * SCALE), U128(4 * SCALE)],
            T0,
        );
        assert_eq!(contract.get_pool_quote(a).unwrap().mid_units, 3 * SCALE);
        assert_eq!(contract.get_pool_quote(b).unwrap().mid_units, 4 * SCALE);
        assert_eq!(contract.get_pool_quote(a).unwrap().stamp_ns, T0);

        // The guards are per cell, not per call: one bad mid refuses the batch
        // rather than writing half a ladder.
        let before = contract.get_pool_quote(a).unwrap().mid_units;
        assert!(refused(|| contract.set_pool_quotes(vec![a], vec![U128(0)], T0)));
        assert!(refused(|| contract.set_pool_quotes(vec![a, b], vec![U128(1)], T0)));
        assert!(refused(|| contract
            .set_pool_quotes(vec![a], vec![U128(1)], T0 + MAX_QUOTE_SLACK_NS + 1_000_000_000)));
        assert_eq!(contract.get_pool_quote(a).unwrap().mid_units, before);

        // And it answers to the venue, exactly like the single write.
        testing_env!(context(accounts(4), T0).build());
        assert!(refused(|| contract.set_pool_quotes(vec![a], vec![U128(1)], T0)));
    }

    /// The age a quote may reach scales with how near its market is, so a week
    /// of ladders can be refreshed in rotation without the far ones expiring.
    #[test]
    fn a_far_ladder_may_be_quoted_less_often_than_a_near_one() {
        let now = T0;
        let near = now + 30 * 60 * 1_000_000_000;
        let inside = now + 23 * 3_600 * 1_000_000_000;
        let far = now + 7 * 24 * 3_600 * 1_000_000_000;
        assert_eq!(Contract::quote_age_allowed(near, now), MAX_QUOTE_AGE_NS);
        assert_eq!(Contract::quote_age_allowed(inside, now), MAX_QUOTE_AGE_NS);
        assert_eq!(Contract::quote_age_allowed(far, now), MAX_QUOTE_AGE_FAR_NS);
        // It never shrinks below the near-tenor rule, and never grows past the
        // far one, whatever the clock says.
        assert_eq!(Contract::quote_age_allowed(0, now), MAX_QUOTE_AGE_NS);
        assert_eq!(
            Contract::quote_age_allowed(now + 365 * 24 * 3_600 * 1_000_000_000, now),
            MAX_QUOTE_AGE_FAR_NS,
            "and a year out is no looser than a week"
        );
    }

    /// The round trip this contract must never allow: buy a ticket for a
    /// rounding error, then ask the vault to buy it back at a mid made up. At
    /// 100.00 on a 100/110 put the vault would pay 90.00 for a 3.12 entry, and it
    /// is other people's equity paying it. The signature answers it: a taker sends
    /// quantity and bounds, and there is no price argument to lie with.
    #[test]
    fn the_caller_cannot_price_the_vault() {
        let mut contract = setup();
        let sid = series(&mut contract);
        deposit_to_pool(&mut contract, accounts(5), 100 * SCALE);
        fund(&mut contract, accounts(3), 100 * SCALE);

        // With nothing published there is no price to trade at.
        testing_env!(context(accounts(3), T0).build());
        assert!(refused(|| contract.pool_buy(sid, U128(SCALE), U128(10 * SCALE))));
        assert_eq!(contract.get_pool_state().open_tickets, 0);

        // The venue names 3.00, and the vault writes at 3.12 — whatever the
        // buyer hoped. A budget under the quote is the buyer's own protection.
        quote(&mut contract, sid, 3 * SCALE);
        testing_env!(context(accounts(3), T0).build());
        assert!(refused(|| contract.pool_buy(sid, U128(SCALE), U128(3 * SCALE))));
        let ticket = contract.pool_buy(sid, U128(SCALE), U128(4 * SCALE));
        assert_eq!(
            contract.get_ticket(ticket).unwrap().premium,
            3_120_000,
            "the entry is the venue's arithmetic, not the caller's"
        );

        // The sell half pays the vault's exit at *its* quote. A holder can only
        // refuse a price that is too low, never name one that is too high.
        quote(&mut contract, sid, 3 * SCALE);
        testing_env!(context(accounts(3), T0 + 1_000_000_000).build());
        assert!(refused(|| contract.pool_close(ticket, Some(U128(90 * SCALE)))));
        assert_eq!(
            contract.get_ticket(ticket).unwrap().buyer,
            accounts(3),
            "a refused close must not destroy the position"
        );
        assert_eq!(
            contract.get_balance(accounts(3)).0,
            100 * SCALE - 3_120_000,
            "and must not pay anything either"
        );
        assert_eq!(contract.pool_close(ticket, Some(U128(2_690_000))).0, 2_700_000);

        // A stranger cannot answer for the venue, or move its price.
        testing_env!(context(accounts(4), T0).build());
        assert!(refused(|| contract.set_pool_quote(sid, U128(100 * SCALE), T0)));
        assert_eq!(contract.get_pool_quote(sid).unwrap().mid_units, 3 * SCALE);
    }

    /// A quote is a price *now*. A taker who read it a moment ago does not get
    /// that price — they get the one standing when the transaction lands, and
    /// their own budget is what protects them from it having moved. This is the
    /// arrangement a live index forces on a vault: echoing a price back cannot
    /// work, because the echo is stale before the call is signed.
    #[test]
    fn a_taker_gets_the_price_standing_when_the_trade_lands() {
        let mut contract = setup();
        let sid = series(&mut contract);
        deposit_to_pool(&mut contract, accounts(5), 1_000 * SCALE);
        fund(&mut contract, accounts(3), 100 * SCALE);
        quote(&mut contract, sid, 3 * SCALE);

        testing_env!(context(accounts(3), T0).build());
        let seen = contract.pool_buy(sid, U128(SCALE), U128(4 * SCALE));
        assert_eq!(contract.get_ticket(seen).unwrap().premium, entry_of(3));

        // The index moves and the venue re-prices before the next click lands.
        quote(&mut contract, sid, 6 * SCALE);
        testing_env!(context(accounts(3), T0 + 1_000_000_000).build());
        // The budget carried over from the old quote: refused, and loudly.
        assert!(refused(|| contract.pool_buy(sid, U128(SCALE), U128(4 * SCALE))));
        // The same order, sized to what the vault now charges, fills at the new
        // price — 6.00 loaded to 6.24, not the 3.12 the screen first showed.
        let later = contract.pool_buy(sid, U128(SCALE), U128(7 * SCALE));
        assert_eq!(contract.get_ticket(later).unwrap().premium, 6_240_000);
    }

    /// A quote is an offer to trade at a price *now*. Reusing one from last week
    /// is the same bug as letting the caller choose it, so the vault stops
    /// honouring it on the clock rather than on the number.
    #[test]
    fn an_expired_quote_is_not_a_price() {
        let mut contract = setup();
        let sid = series(&mut contract);
        deposit_to_pool(&mut contract, accounts(5), 100 * SCALE);
        fund(&mut contract, accounts(3), 100 * SCALE);
        quote(&mut contract, sid, 3 * SCALE);

        // The list outlives the quote, so what fails here is the clock and not
        // the expiry: the series below stays open for a minute past the age.
        let mut wide = contract.get_series(sid).unwrap();
        wide.expiry_ns = T0 + 600_000_000_000;
        contract.series.insert(sid, wide);

        let later = T0 + MAX_QUOTE_AGE_NS + 1_000_000_000;
        testing_env!(context(accounts(3), later).build());
        assert!(refused(|| contract.pool_buy(sid, U128(SCALE), U128(4 * SCALE))));
        // The screen is told too: a stale quote simply is not in the price list.
        testing_env!(context(accounts(4), later).build());
        assert!(contract.get_pool_prices(vec![sid], U128(SCALE)).is_empty());

        // The venue re-stamps and the identical trade goes through.
        testing_env!(context(accounts(0), later).build());
        contract.set_pool_quote(sid, U128(3 * SCALE), later);
        testing_env!(context(accounts(3), later).build());
        let ticket = contract.pool_buy(sid, U128(SCALE), U128(4 * SCALE));
        assert!(ticket <= 1);

        // A stamp in the future would be a quote that never expires.
        testing_env!(context(accounts(0), later).build());
        assert!(refused(|| contract
            .set_pool_quote(sid, U128(3 * SCALE), later + MAX_QUOTE_SLACK_NS + 1_000_000_000)));
    }

    /// What the vault charges and pays is readable before the click, in the
    /// same integers the trade will use, and only for series it will actually
    /// trade.
    #[test]
    fn the_vault_publishes_the_prices_it_will_charge() {
        let mut contract = setup();
        let sid = series(&mut contract);
        let other = series(&mut contract);
        assert_eq!(other, 1);

        // Nothing quoted yet, so nothing for a screen to show.
        assert!(contract.get_pool_prices(vec![sid, other], U128(SCALE)).is_empty());

        quote(&mut contract, sid, 3 * SCALE);
        let prices = contract.get_pool_prices(vec![sid, other], U128(2 * SCALE));
        assert_eq!(prices.len(), 1, "an unquoted series is absent, not zero");
        let p = &prices[0];
        assert_eq!(p.series_id, sid);
        assert_eq!(p.mid_units.0, 3 * SCALE);
        // Two contracts at 3.00, at whatever the contract's own terms charge.
        assert_eq!(p.entry_units.0, 2 * entry_of(3));
        assert_eq!(p.exit_units.0, 2 * exit_of(3));
        assert_eq!(p.age_ns, 0);
        assert!(p.exit_units.0 < p.entry_units.0, "a round trip is never free");

        // A quoted series that has since been fixed stops being offered.
        let mut fixed = contract.get_series(sid).unwrap();
        fixed.settlement = Settlement::Fixed {
            price: 95 * SCALE,
            publish_time_ns: T0 + 60_000_000_000,
        };
        contract.series.insert(sid, fixed);
        assert!(contract.get_pool_prices(vec![sid], U128(SCALE)).is_empty());
    }

    fn fix(price: u128, stamp: u64, confidence: u16, sources: &[&str]) -> PriceObservation {
        PriceObservation {
            price,
            publish_time_ns: stamp,
            confidence_bps: confidence,
            sources: sources.iter().map(|s| s.to_string()).collect(),
            dispersion_bps: 10,
        }
    }

    /// The series helper makes: expiry T0+60s, accepted until T0+120s, strikes
    /// 100..110, confidence floor 5000 bps.
    fn settled_series(contract: &mut Contract) -> Series {
        let sid = series(contract);
        contract.get_series(sid).expect("the helper listed a series")
    }

    #[test]
    fn the_whole_registry_comes_back_in_a_few_reads() {
        let mut contract = setup();
        let ids = (0..3u64).map(|_| series(&mut contract)).collect::<Vec<_>>();
        testing_env!(context(accounts(0), T0).build());
        let mut asked = ids.clone();
        asked.push(ids[0] + 999);
        let rows = contract.get_series_many(asked);
        assert_eq!(rows.len(), 4, "an id the chain does not have is still answered");
        assert_eq!(rows[3].1, None, "and answered as a hole, not as a shifted row");
        assert_eq!(rows[0].0, ids[0], "each answer carries the id it is for");
        assert_eq!(rows[0].1.as_ref().expect("listed").underlying, "ETH");
    }

    #[test]
    #[should_panic(expected = "too many series at once")]
    fn one_read_cannot_ask_for_the_world() {
        let contract = setup();
        contract.get_series_many((0..129u64).collect::<Vec<u64>>());
    }

    #[test]
    fn one_source_named_twice_is_still_one_source() {
        let mut contract = setup();
        let row = settled_series(&mut contract);
        let twice = fix(105 * SCALE, T0 + 61_000_000_000, 10_000, &["venue-index", "venue-index"]);
        assert!(
            !usable_fix(&twice, &row, T0 + 62_000_000_000),
            "a count of entries is not a count of voices"
        );
        let three = fix(105 * SCALE, T0 + 61_000_000_000, 9_995, &["venue-index", "kraken", "venue-index"]);
        assert!(usable_fix(&three, &row, T0 + 62_000_000_000), "and the honest pair still passes");
        assert_eq!(distinct_sources(&three.sources), vec!["kraken".to_string(), "venue-index".to_string()]);
    }

    #[test]
    fn one_source_is_not_a_settlement_price() {
        let mut contract = setup();
        let row = settled_series(&mut contract);
        let alone = fix(105 * SCALE, T0 + 61_000_000_000, 10_000, &["engine"]);
        assert!(
            !usable_fix(&alone, &row, T0 + 62_000_000_000),
            "the party that quoted the ticket cannot also be the only one to settle it"
        );
        let quorum = fix(105 * SCALE, T0 + 61_000_000_000, 9_990, &["engine", "kraken"]);
        assert!(usable_fix(&quorum, &row, T0 + 62_000_000_000));
    }

    #[test]
    fn a_fix_must_stay_inside_its_own_window_and_band() {
        let mut contract = setup();
        let row = settled_series(&mut contract);
        let good = |stamp: u64, price: u128| {
            usable_fix(&fix(price, stamp, 9_990, &["a", "b"]), &row, T0 + 62_000_000_000)
        };
        assert!(good(T0 + 61_000_000_000, 105 * SCALE));
        assert!(!good(T0 + 59_000_000_000, 105 * SCALE), "a stamp before the expiry is the wrong moment");
        assert!(!good(T0 + 121_000_000_000, 105 * SCALE), "and one after the acceptance window is too late");
        assert!(!good(T0 + 61_000_000_000, 9 * SCALE), "a price under the band is a wrong feed");
        assert!(!good(T0 + 61_000_000_000, 1_200 * SCALE), "and so is one over it");
        assert!(
            !usable_fix(&fix(105 * SCALE, T0 + 61_000_000_000, 4_999, &["a", "b"]), &row, T0 + 62_000_000_000),
            "a committee that cannot agree to within the series' floor is not a price"
        );
    }

    #[test]
    fn a_ticket_settles_once_and_only_after_its_series_is_fixed() {
        let mut contract = setup();
        let sid = series(&mut contract);
        deposit_to_pool(&mut contract, accounts(5), 20 * SCALE);
        fund(&mut contract, accounts(3), 100 * SCALE);
        quote(&mut contract, sid, 3 * SCALE);
        testing_env!(context(accounts(3), T0).build());
        let ticket = contract.pool_buy(sid, U128(SCALE), U128(4 * SCALE));
        let custody = contract.expected_custody().0;

        // An open series has no price to pay against, and a refused settlement
        // must not move a token.
        testing_env!(context(outsider(), T0).build());
        assert!(refused(|| contract.settle_ticket(ticket)));
        assert_eq!(contract.expected_custody().0, custody);
        assert!(contract.get_ticket(ticket).is_some());

        let mut row = contract.get_series(sid).unwrap();
        row.settlement = Settlement::Fixed {
            price: 105 * SCALE,
            publish_time_ns: T0,
        };
        contract.series.insert(sid, row);
        testing_env!(context(outsider(), T0).build());
        let (buyer, maker) = contract.settle_ticket(ticket);
        assert_eq!(buyer.0, 5 * SCALE);
        assert_eq!(maker.0, 0, "the vault is the maker, so there is no second credit");
        assert_eq!(contract.expected_custody().0, custody, "a payout moves money, it does not make it");

        // Paid once, gone: no second settle, no early exit, no sale into a bid.
        testing_env!(context(outsider(), T0).build());
        assert!(refused(|| contract.settle_ticket(ticket)));
        testing_env!(context(accounts(3), T0).build());
        assert!(refused(|| contract.pool_close(ticket, None)));
        assert!(refused(|| contract.close_ticket(ticket, None)));
        assert_eq!(contract.get_balance(accounts(3)).0, 100 * SCALE - entry_of(3) + 5 * SCALE);
        assert_eq!(contract.get_pool_state().open_tickets, 0);
        assert_eq!(contract.get_pool_state().reserved.0, 0);
    }

    #[test]
    fn a_series_is_either_fixed_or_voided_never_both() {
        let mut contract = setup();
        let fixed_sid = series(&mut contract);
        let void_sid = series(&mut contract);
        // Both are past their finalize deadline, so a void would otherwise be
        // allowed on either of them.
        testing_env!(context(accounts(0), T0 + 700_000_000_000).build());

        let mut row = contract.get_series(fixed_sid).unwrap();
        row.settlement = Settlement::Fixed {
            price: 105 * SCALE,
            publish_time_ns: T0 + 61_000_000_000,
        };
        contract.series.insert(fixed_sid, row);
        testing_env!(context(accounts(0), T0 + 700_000_000_000).build());
        assert!(refused(|| contract.void_series(fixed_sid)), "a fixed series is not voidable");

        contract.void_series(void_sid);
        testing_env!(context(accounts(0), T0 + 700_000_000_000).build());
        assert!(refused(|| contract.void_series(void_sid)), "a void is final too");
        let stored = contract.get_series(void_sid).unwrap();
        assert!(matches!(stored.settlement, Settlement::Void));
    }

    #[test]
    fn no_zero_or_overflowing_order_reaches_the_books() {
        let mut contract = setup();
        let sid = series(&mut contract);
        deposit_to_pool(&mut contract, accounts(5), 20 * SCALE);
        fund(&mut contract, accounts(3), 100 * SCALE);
        fund(&mut contract, accounts(4), 100 * SCALE);
        quote(&mut contract, sid, 3 * SCALE);
        let before = contract.expected_custody().0;

        testing_env!(context(accounts(3), T0).build());
        assert!(refused(|| contract.pool_buy(sid, U128(0), U128(4 * SCALE))));
        assert!(refused(|| contract.pool_buy(sid, U128(SCALE), U128(0))));
        assert!(refused(|| contract.pool_buy(sid, U128(u128::MAX), U128(u128::MAX))));
        assert!(refused(|| contract.pool_buy_held(sid, U128(0), U128(4 * SCALE))));

        testing_env!(context(accounts(4), T0).build());
        assert!(refused(|| contract.post_offer(
            sid,
            U128(0),
            U128(SCALE),
            None,
            T0 + 30_000_000_000
        )));
        assert!(refused(|| contract.post_offer(
            sid,
            U128(SCALE),
            U128(0),
            None,
            T0 + 30_000_000_000
        )));
        assert!(refused(|| contract.post_offer(
            sid,
            U128(u128::MAX),
            U128(SCALE),
            None,
            T0 + 30_000_000_000
        )));
        assert!(refused(|| contract.post_bid(
            sid,
            U128(0),
            U128(SCALE),
            T0 + 30_000_000_000
        )));

        testing_env!(context(accounts(3), T0).build());
        assert!(refused(|| contract.fill_market_order(sid, U128(0), U128(10 * SCALE), None)));

        let stats = contract.stats();
        assert_eq!(stats.open_ticket_count, 0);
        assert_eq!(stats.open_offer_count, 0);
        assert_eq!(stats.open_bid_count, 0);
        assert_eq!(stats.total_offer_collateral.0, 0);
        assert_eq!(stats.total_bid_escrow.0, 0);
        assert_eq!(contract.get_pool_state().reserved.0, 0);
        // Zero and overflow are refused before anything is debited, so even the
        // unit-test world (which does not roll back on a panic) is unchanged.
        assert_eq!(contract.expected_custody().0, before);
        assert_eq!(contract.get_balance(accounts(3)).0, 100 * SCALE);
        assert_eq!(contract.get_balance(accounts(4)).0, 100 * SCALE);
    }

    #[test]
    fn only_the_admin_and_the_venue_can_move_the_venue_controls() {
        let mut contract = setup();
        let state = contract.get_pool_state();
        let (margin, impact, venue) = (state.margin_bps, state.impact_bps, state.venue_account);

        testing_env!(context(outsider(), T0).build());
        assert!(refused(|| contract.set_fee_terms(0, 0)));
        assert!(refused(|| contract.set_venue(outsider())));
        assert!(refused(|| contract.set_paused(true)));
        assert!(refused(|| contract.pool_sweep()));
        let after = contract.get_pool_state();
        assert_eq!(
            (after.margin_bps, after.impact_bps, after.venue_account),
            (margin, impact, venue),
            "a refused control leaves the venue exactly as it was"
        );

        testing_env!(context(accounts(0), T0).build());
        contract.set_fee_terms(50, 100);
        contract.set_venue(accounts(5));
        testing_env!(context(accounts(0), T0).build());
        assert!(
            refused(|| contract.pool_sweep()),
            "the admin is no longer the venue and cannot sweep its margin"
        );
        testing_env!(context(accounts(5), T0).build());
        contract.pool_sweep();
    }

    #[test]
    fn a_reserved_offer_is_only_for_the_taker_it_names() {
        let mut contract = setup();
        let sid = series(&mut contract);
        for who in [3, 4, 5] {
            fund(&mut contract, accounts(who), 100 * SCALE);
        }
        testing_env!(context(accounts(4), T0).build());
        let offer = contract.post_offer(
            sid,
            U128(SCALE),
            U128(2 * SCALE),
            Some(accounts(5)),
            T0 + 30_000_000_000,
        );

        testing_env!(context(accounts(3), T0).build());
        assert!(refused(|| contract.fill_offer(offer)));
        assert_eq!(
            contract.get_balance(accounts(3)).0,
            100 * SCALE,
            "a refused fill costs the caller nothing"
        );
        assert!(contract.get_offer(offer).is_some(), "and leaves the offer standing");

        testing_env!(context(accounts(5), T0).build());
        let ticket = contract.fill_offer(offer);
        assert_eq!(contract.get_balance(accounts(5)).0, 98 * SCALE);
        assert!(contract.get_offer(offer).is_none());
        assert_eq!(contract.get_ticket(ticket).unwrap().buyer, accounts(5));
    }

    /// The whole balance sheet, checked after every mutation of the stateful
    /// run. `issued` is what the token contract has credited to this contract:
    /// nothing inside may create or destroy money, only move it between the
    /// available balance, escrow, tickets and the vault's buckets.
    fn assert_books(
        contract: &Contract,
        issued: u128,
        tickets: &[u64],
        offers: &[u64],
        bids: &[u64],
        label: &str,
    ) {
        let stats = contract.stats();
        let vault = contract.get_pool_state();
        assert_eq!(
            contract.expected_custody().0,
            issued,
            "{label}: custody does not equal the money the contract received"
        );
        assert!(
            vault.reserved.0 <= vault.equity.0,
            "{label}: reserved {} exceeds equity {}",
            vault.reserved.0,
            vault.equity.0
        );
        assert_eq!(
            vault.free_equity.0,
            vault.equity.0 - vault.reserved.0,
            "{label}: free equity is not equity less reserved"
        );
        let users = [accounts(0), accounts(3), accounts(4), accounts(5)];
        let available: u128 = users
            .iter()
            .map(|who| contract.get_balance(who.clone()).0)
            .sum();
        assert_eq!(available, stats.total_available.0, "{label}: balances disagree with their total");

        let (mut person_collateral, mut person_premium, mut pool_open, mut locked) =
            (0u128, 0u128, 0u64, 0u128);
        for id in tickets {
            let Some(ticket) = contract.tickets.get(id) else {
                continue;
            };
            let series = contract.series.get(&ticket.series_id).expect("series");
            let width = series.high_strike - series.low_strike;
            assert_eq!(
                ticket.collateral,
                accounting::max_payout(ticket.quantity, width, series.price_scale).unwrap(),
                "{label}: ticket {id} reserved something other than its worst case"
            );
            if ticket.maker == vault_account() {
                pool_open += 1;
                locked += ticket.venue_cut;
                assert!(ticket.premium >= ticket.venue_cut, "{label}: split exceeds premium");
                if ticket.held {
                    assert_eq!(ticket.venue_cut, 0, "{label}: a held ticket paid a venue fee");
                }
            } else {
                assert_eq!(ticket.venue_cut, 0, "{label}: a person ticket charged a venue cut");
                assert!(!ticket.held, "{label}: a person ticket claimed the hold path");
                person_collateral += ticket.collateral;
                person_premium += ticket.premium;
            }
        }
        assert_eq!(
            person_collateral, stats.total_ticket_collateral.0,
            "{label}: person ticket collateral total"
        );
        assert_eq!(
            person_premium, stats.total_ticket_premium.0,
            "{label}: person ticket premium total"
        );
        assert_eq!(pool_open, vault.open_tickets, "{label}: the vault counts a different number of tickets");
        assert_eq!(locked, vault.venue_locked.0, "{label}: refundable margin disagrees");
        assert_eq!(
            tickets.iter().filter(|id| contract.tickets.get(*id).is_some()).count() as u64,
            stats.open_ticket_count,
            "{label}: open ticket count"
        );

        let open_offers: Vec<_> = offers
            .iter()
            .filter_map(|id| contract.offers.get(id))
            .collect();
        assert_eq!(
            open_offers.len() as u64,
            stats.open_offer_count,
            "{label}: open offer count"
        );
        assert_eq!(
            open_offers.iter().map(|offer| offer.collateral).sum::<u128>(),
            stats.total_offer_collateral.0,
            "{label}: offer collateral total"
        );

        let open_bids: Vec<_> = bids.iter().filter_map(|id| contract.bids.get(id)).collect();
        assert_eq!(
            open_bids.len() as u64,
            stats.open_bid_count,
            "{label}: open bid count"
        );
        assert_eq!(
            open_bids.iter().map(|bid| bid.premium).sum::<u128>(),
            stats.total_bid_escrow.0,
            "{label}: bid escrow total"
        );
        assert_eq!(
            stats.pending_withdrawal_count, 0,
            "{label}: the run never asks the token contract for money"
        );
    }

    fn open_series(contract: &Contract, series_id: u64) -> bool {
        contract
            .get_series(series_id)
            .map(|series| series.settlement == Settlement::Open)
            .unwrap_or(false)
    }

    fn fix_at(contract: &mut Contract, series_id: u64, price: u128) {
        let mut series = contract.get_series(series_id).expect("series");
        series.settlement = Settlement::Fixed {
            price,
            publish_time_ns: T0,
        };
        contract.series.insert(series_id, series);
    }

    fn settle_where(contract: &mut Contract, series_id: u64, tickets: &[u64], at: u64) {
        for id in tickets {
            if let Some(ticket) = contract.get_ticket(*id) {
                if ticket.series_id == series_id {
                    testing_env!(context(outsider(), at).build());
                    contract.settle_ticket(*id);
                }
            }
        }
    }

    /// A session, not a scenario: deposits, both pool modes, the book, bids,
    /// early exits, settlements and a void, in a random order. After every
    /// single operation the test re-derives the whole balance sheet and refuses
    /// to continue if a coin moved without a matching entry.
    ///
    /// This is the layer that catches the bugs examples miss: a path where a
    /// refund is paid but the reservation is not released, or a fee is earned
    /// twice, only shows up once one operation follows another — the exact
    /// sequence nobody writes by hand.
    #[test]
    fn the_books_balance_through_a_random_multi_user_session() {
        let mut contract = setup();
        let mut issued = 0u128;
        for who in [3, 4, 5] {
            fund(&mut contract, accounts(who), 500 * SCALE);
            issued += 500 * SCALE;
        }
        deposit_to_pool(&mut contract, accounts(5), 400 * SCALE);
        issued += 400 * SCALE;

        let mut sids = Vec::new();
        for (index, offset) in [60u64, 120, 180, 240].into_iter().enumerate() {
            testing_env!(paid(accounts(0), T0).build());
            let sid = contract.create_series(
                "ETH".to_string(),
                SpreadKind::Put,
                T0 + offset * 1_000_000_000,
                T0 + (offset + 30) * 1_000_000_000,
                T0 + (offset + 600) * 1_000_000_000,
                U128(100 * SCALE),
                U128(110 * SCALE),
                U128(SCALE),
                5_000,
                "ETH-USD".to_string(),
            );
            quote(&mut contract, sid, (3 + index as u128) * SCALE);
            sids.push(sid);
        }

        let mut tickets: Vec<u64> = Vec::new();
        let mut offers: Vec<u64> = Vec::new();
        let mut bids: Vec<u64> = Vec::new();
        let mut settled = [false; 4];
        // Paths the session must actually reach, so a green run cannot be a run
        // of refusals that never touched the interesting code.
        let mut reached = [0u32; 8];
        let mut state = 0x5EED_2026u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };

        for step in 0..220 {
            let who = accounts(3 + (next() % 3) as usize);
            let sid = sids[(next() % 4) as usize];
            let label = format!("step {step}");
            testing_env!(context(who.clone(), T0).build());
            match next() % 100 {
                // Buy from the vault, and the no-exit version of the same trade.
                0..=9 => {
                    if open_series(&contract, sid) {
                        quote(&mut contract, sid, (3 + (next() % 4) as u128) * SCALE);
                        testing_env!(context(who.clone(), T0).build());
                        if let Ok(id) =
                            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                contract.pool_buy(sid, U128(SCALE), U128(10 * SCALE))
                            }))
                        {
                            reached[0] += 1;
                            tickets.push(id);
                        }
                    }
                }
                10..=17 => {
                    if open_series(&contract, sid) {
                        quote(&mut contract, sid, (3 + (next() % 4) as u128) * SCALE);
                        testing_env!(context(who.clone(), T0).build());
                        if let Ok(id) =
                            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                contract.pool_buy_held(sid, U128(SCALE), U128(10 * SCALE))
                            }))
                        {
                            reached[1] += 1;
                            tickets.push(id);
                        }
                    }
                }
                // Sell a pool ticket back before expiry.
                18..=27 => {
                    if !tickets.is_empty() {
                        let id = tickets[(next() as usize) % tickets.len()];
                        if let Some(ticket) = contract.get_ticket(id) {
                            if ticket.maker == vault_account() && !ticket.held {
                                testing_env!(context(ticket.buyer, T0).build());
                                if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                    contract.pool_close(id, None)
                                }))
                                .is_ok()
                                {
                                    reached[2] += 1;
                                }
                            }
                        }
                    }
                }
                // The person-to-person book.
                28..=39 => {
                    let id = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        contract.post_offer(
                            sid,
                            U128(SCALE),
                            U128((2 + (next() % 5) as u128) * SCALE),
                            None,
                            T0 + 30_000_000_000,
                        )
                    }));
                    if let Ok(id) = id {
                        offers.push(id);
                    }
                }
                40..=49 => {
                    if !offers.is_empty() {
                        let id = offers[(next() as usize) % offers.len()];
                        if let Some(offer) = contract.get_offer(id) {
                            if offer.maker != who {
                                testing_env!(context(who.clone(), T0).build());
                                if let Ok(ticket) =
                                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                        contract.fill_offer(id)
                                    }))
                                {
                                    reached[3] += 1;
                                    tickets.push(ticket);
                                }
                            }
                        }
                    }
                }
                50..=60 => {
                    let id = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        contract.post_bid(
                            sid,
                            U128(SCALE),
                            U128((2 + (next() % 5) as u128) * SCALE),
                            T0 + 30_000_000_000,
                        )
                    }));
                    if let Ok(id) = id {
                        bids.push(id);
                    }
                }
                61..=70 => {
                    if !bids.is_empty() {
                        let id = bids[(next() as usize) % bids.len()];
                        if let Some(bid) = contract.bids.get(&id) {
                            if bid.owner != who {
                                testing_env!(context(who.clone(), T0).build());
                                if let Ok(ticket) =
                                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                        contract.fill_bid(id)
                                    }))
                                {
                                    reached[4] += 1;
                                    tickets.push(ticket);
                                }
                            }
                        }
                    }
                }
                71..=73 => {
                    if !offers.is_empty() {
                        let id = offers[(next() as usize) % offers.len()];
                        if let Some(offer) = contract.get_offer(id) {
                            testing_env!(context(offer.maker, T0).build());
                            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                contract.cancel_offer(id)
                            }));
                        }
                    }
                }
                74..=76 => {
                    if !bids.is_empty() {
                        let id = bids[(next() as usize) % bids.len()];
                        if let Some(bid) = contract.bids.get(&id).cloned() {
                            testing_env!(context(bid.owner, T0).build());
                            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                contract.cancel_bid(id)
                            }));
                        }
                    }
                }
                // Sell a whole ticket into a resting bid.
                77..=87 => {
                    if !tickets.is_empty() {
                        let id = tickets[(next() as usize) % tickets.len()];
                        if let Some(ticket) = contract.get_ticket(id) {
                            testing_env!(context(ticket.buyer, T0).build());
                            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                contract.close_ticket(id, None)
                            }))
                            .is_ok()
                            {
                                reached[5] += 1;
                            }
                        }
                    }
                }
                // The LP's exit and the venue's sweep.
                88..=91 => {
                    testing_env!(context(accounts(5), T0).build());
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        contract.pool_withdraw(U128((1 + (next() % 8) as u128) * SCALE))
                    }));
                }
                92..=94 => {
                    testing_env!(context(accounts(0), T0).build());
                    contract.pool_sweep();
                }
                _ => {
                    // Settle one of the first three series at a random price.
                    // The fourth is left for the void at the end.
                    let candidates: Vec<usize> = (0..3).filter(|i| !settled[*i]).collect();
                    if !candidates.is_empty() {
                        let index = candidates[(next() as usize) % candidates.len()];
                        let price = (80 + (next() % 60) as u128) * SCALE;
                        fix_at(&mut contract, sids[index], price);
                        settle_where(&mut contract, sids[index], &tickets, T0 + 60_000_000_000);
                        settled[index] = true;
                        reached[6] += 1;
                    }
                }
            }
            // Every accepted pool ticket has to be payable at its own worst
            // case, which is what the reservation was for.
            assert_books(&contract, issued, &tickets, &offers, &bids, &label);
        }

        // Whatever is still open runs off: the three early series at a random
        // price, the last one through the outage path.
        for index in 0..3 {
            if !settled[index] {
                fix_at(&mut contract, sids[index], (80 + (next() % 60) as u128) * SCALE);
                settle_where(&mut contract, sids[index], &tickets, T0 + 60_000_000_000);
            }
        }
        testing_env!(
            context(outsider(), T0 + 3_000_000_000_000).build()
        );
        contract.void_series(sids[3]);
        reached[7] += 1;
        settle_where(&mut contract, sids[3], &tickets, T0 + 3_000_000_000_000);
        assert_books(&contract, issued, &tickets, &offers, &bids, "after the last settlement");

        assert_eq!(contract.stats().open_ticket_count, 0, "every ticket ran off");
        assert_eq!(contract.get_pool_state().open_tickets, 0);
        assert_eq!(contract.get_pool_state().reserved.0, 0);
        assert_eq!(contract.get_pool_state().venue_locked.0, 0);
        let earned = contract.get_pool_state().venue.0;
        assert_eq!(sweep(&mut contract), earned, "the venue takes its whole accrual");
        assert_eq!(contract.get_pool_state().venue.0, 0);
        assert_books(&contract, issued, &tickets, &offers, &bids, "after the sweep");
        // Every token still belongs to someone: the LPs, the venue, or the
        // traders who funded the run.
        assert_eq!(contract.expected_custody().0, issued);
        assert!(
            reached.iter().all(|count| *count > 0),
            "the session never exercised one of its paths: {reached:?}"
        );
    }

    // ---------- storage rent ----------

    /// A series is the one row nobody gets to write for free: it is permanent,
    /// it is created on the venue's behalf, and on a public chain the contract
    /// pays for it out of its own balance every block until the account is empty.
    #[test]
    fn a_series_must_be_paid_for_by_whoever_creates_it() {
        let mut contract = setup();
        // No deposit at all: refused, with a sentence rather than a silent drain.
        testing_env!(context(accounts(0), T0).build());
        assert!(
            refused(|| {
                contract.create_series(
                    "ETH".to_string(),
                    SpreadKind::Put,
                    T0 + 60_000_000_000,
                    T0 + 120_000_000_000,
                    T0 + 660_000_000_000,
                    U128(100 * SCALE),
                    U128(110 * SCALE),
                    U128(SCALE),
                    5_000,
                    "ETH-USD".to_string(),
                )
            }),
            "an unfunded series write must be refused"
        );

        // Paying exactly the rent the row costs is enough.
        let cost = storage_price_per_byte()
            .saturating_mul(SERIES_STATE_BYTES as u128);
        let mut ctx = context(accounts(0), T0);
        ctx.attached_deposit(cost);
        testing_env!(ctx.build());
        let id = contract.create_series(
            "ETH".to_string(),
            SpreadKind::Put,
            T0 + 60_000_000_000,
            T0 + 120_000_000_000,
            T0 + 660_000_000_000,
            U128(100 * SCALE),
            U128(110 * SCALE),
            U128(SCALE),
            5_000,
            "ETH-USD".to_string(),
        );
        assert_eq!(id, 0);
        assert_eq!(contract.get_storage_state().paid_yocto.0, cost.as_yoctonear());
    }

    /// Re-quoting a series the venue already prices adds no state, so it adds no
    /// charge. Making a refresh cycle pay rent would tax the venue for staying
    /// fresh, and a quote that is expensive to renew is a quote that goes stale.
    #[test]
    fn refreshing_a_quote_costs_nothing_after_the_first_time() {
        let mut contract = setup();
        let sid = series(&mut contract);
        // Quote it once, paying for the row; that row is the state a refresh
        // must not be charged for again.
        let mut ctx = context(accounts(0), T0);
        ctx.attached_deposit(storage_price_per_byte().saturating_mul(QUOTE_STATE_BYTES as u128));
        testing_env!(ctx.build());
        contract.set_pool_quote(sid, U128(3 * SCALE), T0);
        let paid_before = contract.get_storage_state().paid_yocto.0;

        // A refresh attaches nothing, and must still be accepted.
        testing_env!(context(accounts(0), T0 + 1).build());
        contract.set_pool_quote(sid, U128(4 * SCALE), T0 + 1);
        assert_eq!(contract.get_pool_quote(sid).unwrap().mid_units, 4 * SCALE);
        assert_eq!(
            contract.get_storage_state().paid_yocto.0,
            paid_before,
            "a re-quote must not charge again"
        );
    }

    /// The view is the whole point: an operator has to be able to see the tab
    /// before a judge finds it as a failed transaction.
    #[test]
    fn the_storage_tab_reads_back_what_it_owes_and_what_it_has() {
        let mut contract = setup();
        let _ = series(&mut contract);
        let state = contract.get_storage_state();
        // The test runner's `storage_usage` counts the collections this contract
        // created at init plus the row `series()` wrote, so the requirement is
        // never zero and never below the row we just paid for.
        assert!(state.usage_bytes > 0, "the contract has no state at all?");
        assert!(state.required_yocto.0 > 0);
        assert_eq!(
            state.required_yocto.0,
            state.price_per_byte.0 * state.usage_bytes as u128
        );
        // Rent paid is a floor on what was contributed, not the balance: the
        // runner does not fund the account, so `funded` is honest about that.
        assert!(state.paid_yocto.0 >= SERIES_STATE_BYTES as u128 * state.price_per_byte.0);
    }

    /// Anyone may top the venue up. It is a donation with a receipt: no shares,
    /// no claim, no right — which is what makes it safe to let a stranger do it.
    #[test]
    fn anyone_can_put_money_on_the_storage_tab() {
        let mut contract = setup();
        let before = contract.get_storage_state().paid_yocto.0;
        let gift = NearToken::from_millinear(10);
        let mut ctx = context(outsider(), T0);
        ctx.attached_deposit(gift);
        testing_env!(ctx.build());
        contract.storage_deposit();
        assert_eq!(
            contract.get_storage_state().paid_yocto.0,
            before + gift.as_yoctonear()
        );
        // A deposit of nothing is a mistake, not a donation.
        testing_env!(context(outsider(), T0).build());
        assert!(refused(|| contract.storage_deposit()));
    }

    /// Overpaying must not be kept. The surplus goes back to whoever attached it,
    /// because a contract that quietly absorbs the remainder is a contract that
    /// makes money from its callers' arithmetic.
    #[test]
    fn an_overpaid_series_refunds_the_remainder() {
        let mut contract = setup();
        let exact = storage_price_per_byte().saturating_mul(SERIES_STATE_BYTES as u128);
        let overpaid = NearToken::from_yoctonear(exact.as_yoctonear() + 1_000_000);
        let mut ctx = context(accounts(0), T0);
        ctx.attached_deposit(overpaid);
        testing_env!(ctx.build());
        contract.create_series(
            "ETH".to_string(),
            SpreadKind::Put,
            T0 + 60_000_000_000,
            T0 + 120_000_000_000,
            T0 + 660_000_000_000,
            U128(100 * SCALE),
            U128(110 * SCALE),
            U128(SCALE),
            5_000,
            "ETH-USD".to_string(),
        );
        // Only the rent is booked; the rest is on its way back to the caller.
        assert_eq!(contract.get_storage_state().paid_yocto.0, exact.as_yoctonear());
    }
}
