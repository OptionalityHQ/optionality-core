//! Private venue state machine. This is NOT a deployed privacy system: the
//! caller must supply verified market/funding context and run it inside a
//! pinned, remotely attested enclave with encrypted input/output/storage.
//! No NEAR account IDs belong in the user or profile records.
use bounded_core::{
    accounting,
    fees::Terms,
    pool::{Pool, Written},
};
use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

pub const SCALE: u128 = 1_000_000;
type Result<T> = std::result::Result<T, String>;
fn require(condition: bool, error: &str) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(error.into())
    }
}
fn checked<T>(value: Option<T>) -> Result<T> {
    value.ok_or_else(|| "numeric overflow or insufficient capacity".into())
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    pub cash: u128,
    pub nonce: u64,
    pub lp_shares: u128,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Profile {
    pub owner: String,
    pub price: u128,
    pub period_ms: u64,
    pub public_performance: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicProfile {
    pub id: String,
    pub price: u128,
    pub period_ms: u64,
    pub performance: Option<PublicPerformance>,
}
/// All closed trades for this private command identity, including unpublished
/// losses. Excludes LP income and subscription revenue. Open positions are not
/// valued; their count is disclosed explicitly when the owner opts in.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicPerformance {
    pub closed_trades: u64,
    pub open_positions: u64,
    pub entry_premiums: u128,
    pub exit_and_settlement_proceeds: u128,
    pub realized_pnl: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CopyAttempt {
    pub subscriber: String,
    pub leader_position: u64,
    pub at_ms: u64,
    pub phase: String,
    pub outcome: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subscription {
    pub subscriber: String,
    pub profile: String,
    pub until_ms: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mandate {
    pub profile: String,
    pub quantity: u128,
    pub max_each: u128,
    pub max_total: u128,
    pub spent: u128,
    pub until_ms: u64,
    pub max_expiry_ms: u64,
    pub asset: String,
    /// Maximum loss relative to entry premium at an automatically copied exit.
    /// 10_000 explicitly permits a complete loss; zero requires break-even.
    pub max_exit_loss_bps: u16,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Kind {
    Put,
    Call,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeriesTerms {
    pub asset: String,
    pub kind: Kind,
    pub low: u128,
    pub high: u128,
    pub expiry_ms: u64,
}
impl From<&Market> for SeriesTerms {
    fn from(m: &Market) -> Self {
        Self {
            asset: m.asset.clone(),
            kind: m.kind,
            low: m.low,
            high: m.high,
            expiry_ms: m.expiry_ms,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Position {
    pub owner: String,
    pub series: u64,
    pub terms: SeriesTerms,
    pub quantity: u128,
    pub premium: u128,
    pub lp_notional: u128,
    pub venue_cut: u128,
    pub max_payout: u128,
    pub held: bool,
    pub copied_from: Option<u64>,
    pub profile: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub position: u64,
    pub owner: String,
    pub profile: Option<String>,
    pub series: u64,
    pub premium: u128,
    pub proceeds: u128,
    pub at_ms: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WithdrawalNote {
    pub owner: String,
    pub amount: u128,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BalanceSheet {
    pub equity: u128,
    pub reserved: u128,
    pub venue_locked: u128,
    pub venue: u128,
    pub consolation: u128,
    pub shares: u128,
}
impl BalanceSheet {
    fn pool(&self) -> Pool {
        Pool {
            equity: self.equity,
            reserved: self.reserved,
            venue_locked: self.venue_locked,
            venue: self.venue,
            consolation: self.consolation,
            shares: self.shares,
        }
    }
    fn update(&mut self, p: Pool) {
        *self = Self {
            equity: p.equity,
            reserved: p.reserved,
            venue_locked: p.venue_locked,
            venue: p.venue,
            consolation: p.consolation,
            shares: p.shares,
        };
    }
}

/// Provided by the trusted host after verifying feed/listing context. It must
/// never be deserialized from the user's unverified command envelope.
#[derive(Clone, Debug)]
pub struct Market {
    pub asset: String,
    pub kind: Kind,
    pub low: u128,
    pub high: u128,
    pub expiry_ms: u64,
    pub quote_at_ms: u64,
    pub mid: u128,
    pub fixed_price: Option<u128>,
    pub voided: bool,
}
#[derive(Clone, Debug)]
pub struct Context {
    pub now_ms: u64,
    pub markets: BTreeMap<u64, Market>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", deny_unknown_fields)]
pub enum Command {
    CreateProfile {
        id: String,
        price: u128,
        period_ms: u64,
    },
    SetPerformanceDisclosure {
        profile: String,
        enabled: bool,
    },
    Subscribe {
        profile: String,
        max_price: u128,
    },
    SetMandate {
        mandate: Mandate,
    },
    RevokeMandate {
        profile: String,
    },
    PoolDeposit {
        amount: u128,
    },
    PoolWithdraw {
        shares: u128,
    },
    Buy {
        series: u64,
        quantity: u128,
        max_premium: u128,
        held: bool,
        publish: Option<String>,
    },
    Close {
        position: u64,
        min_proceeds: u128,
    },
    Settle {
        position: u64,
    },
    Withdraw {
        note: String,
        amount: u128,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedCommand {
    pub actor: String,
    pub nonce: u64,
    pub deadline_ms: u64,
    pub network: String,
    pub vault: String,
    pub command: Command,
    /// The signed recipient of any private result, never selected by a relay.
    pub reply_key: String,
}
impl SignedCommand {
    pub fn signing_bytes(&self) -> Result<Vec<u8>> {
        let mut bytes = b"optionality-private-command-v1\0".to_vec();
        bytes.extend(serde_json::to_vec(self).map_err(|_| "command serialization failed")?);
        Ok(bytes)
    }
}
/// Consensus-anchored public state. A restored private snapshot is usable only
/// when it matches this independently fetched finalized checkpoint.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Checkpoint {
    pub network: String,
    pub vault: String,
    pub sequence: u64,
    pub root: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JournalRequest {
    pub actor: String,
    pub profile: String,
    pub nonce: u64,
    pub deadline_ms: u64,
    pub network: String,
    pub vault: String,
    /// The enclave host must encrypt its reply to this X25519 key. Binding it
    /// into the signature prevents a relay substituting its own reply key.
    pub reply_key: String,
    /// Offset within this profile's published records, not the global journal.
    pub cursor: u64,
    pub limit: u8,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JournalPage {
    pub records: Vec<Record>,
    pub next_cursor: Option<u64>,
}
impl JournalRequest {
    pub fn signing_bytes(&self) -> Result<Vec<u8>> {
        let mut bytes = b"optionality-private-journal-v1\0".to_vec();
        bytes.extend(serde_json::to_vec(self).map_err(|_| "read serialization failed")?);
        Ok(bytes)
    }
}

/// Only public outputs should be serialized for transport. This state type is
/// serialized solely as input to an authenticated enclave-sealing operation.
/// The host must never return raw snapshots, even to the owner.
#[derive(Clone, PartialEq, Eq)]
pub struct Ledger {
    network: String,
    vault: String,
    /// Enclave-generated secret salt prevents public root guessing from a
    /// low-entropy balance sheet. It is never part of a public response.
    blinding: [u8; 32],
    pub sequence: u64,
    pub accounts: BTreeMap<String, Account>,
    pub profiles: BTreeMap<String, Profile>,
    pub subscriptions: BTreeMap<String, Subscription>,
    pub mandates: BTreeMap<String, Mandate>,
    pub positions: BTreeMap<u64, Position>,
    pub records: Vec<Record>,
    pub copy_attempts: Vec<CopyAttempt>,
    pub notes: BTreeMap<String, WithdrawalNote>,
    pub spent_notes: BTreeSet<String>,
    pub deposits: BTreeSet<String>,
    pub total_funded: u128,
    pub total_redeemed: u128,
    pub pool: BalanceSheet,
    next_position: u64,
}
// Remote serde deliberately does not implement Serialize/Deserialize on Ledger:
// accidentally returning the whole state as a JSON response becomes a type error.
#[derive(Serialize, Deserialize)]
#[serde(remote = "Ledger", deny_unknown_fields)]
struct PrivateLedgerSnapshot {
    network: String,
    vault: String,
    /// Enclave-generated secret salt prevents public root guessing from a
    /// low-entropy balance sheet. It is never part of a public response.
    blinding: [u8; 32],
    sequence: u64,
    accounts: BTreeMap<String, Account>,
    profiles: BTreeMap<String, Profile>,
    subscriptions: BTreeMap<String, Subscription>,
    mandates: BTreeMap<String, Mandate>,
    positions: BTreeMap<u64, Position>,
    records: Vec<Record>,
    copy_attempts: Vec<CopyAttempt>,
    notes: BTreeMap<String, WithdrawalNote>,
    spent_notes: BTreeSet<String>,
    deposits: BTreeSet<String>,
    total_funded: u128,
    total_redeemed: u128,
    pool: BalanceSheet,
    next_position: u64,
}
struct SnapshotRef<'a>(&'a Ledger);
impl serde::Serialize for SnapshotRef<'_> {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        PrivateLedgerSnapshot::serialize(self.0, serializer)
    }
}
struct Snapshot(Ledger);
impl<'de> serde::Deserialize<'de> for Snapshot {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        PrivateLedgerSnapshot::deserialize(deserializer).map(Self)
    }
}

fn pair(actor: &str, profile: &str) -> String {
    format!("{actor}:{profile}")
}
fn identity(value: &str) -> Result<[u8; 32]> {
    require(
        value.len() == 64
            && value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "identity must be a canonical 32-byte lowercase hex key",
    )?;
    hex::decode(value)
        .map_err(|_| "invalid identity".to_string())?
        .try_into()
        .map_err(|_| "invalid identity".into())
}
impl Ledger {
    pub fn new(network: String, vault: String, blinding: [u8; 32]) -> Result<Self> {
        require(
            !network.is_empty() && !vault.is_empty() && blinding != [0; 32],
            "domain and random blinding required",
        )?;
        Ok(Self {
            network,
            vault,
            blinding,
            sequence: 0,
            accounts: BTreeMap::new(),
            profiles: BTreeMap::new(),
            subscriptions: BTreeMap::new(),
            mandates: BTreeMap::new(),
            positions: BTreeMap::new(),
            records: Vec::new(),
            copy_attempts: Vec::new(),
            notes: BTreeMap::new(),
            spent_notes: BTreeSet::new(),
            deposits: BTreeSet::new(),
            total_funded: 0,
            total_redeemed: 0,
            pool: BalanceSheet::default(),
            next_position: 0,
        })
    }
    pub fn root(&self) -> Result<String> {
        let mut hash = Sha256::new();
        hash.update(b"optionality-private-root-v1\0");
        hash.update(
            serde_json::to_vec(&SnapshotRef(self)).map_err(|_| "state serialization failed")?,
        );
        Ok(hex::encode(hash.finalize()))
    }
    pub fn checkpoint(&self) -> Result<Checkpoint> {
        Ok(Checkpoint {
            network: self.network.clone(),
            vault: self.vault.clone(),
            sequence: self.sequence,
            root: self.root()?,
        })
    }
    /// Input MUST already be authenticated and decrypted inside the enclave.
    /// `expected` MUST come from finalized contract state, never the request.
    pub fn restore_verified_snapshot(plaintext: &[u8], expected: &Checkpoint) -> Result<Self> {
        let restored = serde_json::from_slice::<Snapshot>(plaintext)
            .map_err(|_| "invalid private snapshot")?
            .0;
        require(
            restored.checkpoint()? == *expected,
            "snapshot does not match finalized checkpoint",
        )?;
        restored.validate_invariants()?;
        Ok(restored)
    }
    /// The closure is an explicit private persistence boundary. It must provide
    /// randomized authenticated encryption (e.g. OutLayer sealed storage), never
    /// a transport serializer. Only ciphertext should leave the closure.
    pub fn with_private_snapshot<T>(&self, seal: impl FnOnce(&[u8]) -> Result<T>) -> Result<T> {
        let mut bytes = serde_json::to_vec(&SnapshotRef(self))
            .map_err(|_| "private snapshot serialization failed")?;
        let result = seal(&bytes);
        bytes.fill(0);
        result
    }
    /// Host-only boundary. The enclave must verify the finalized deposit and
    /// its private recipient binding before calling this, never trust UI claims.
    pub fn credit_verified_deposit(
        &mut self,
        proof: String,
        actor: &str,
        amount: u128,
    ) -> Result<()> {
        identity(actor)?;
        identity(&proof)?;
        require(amount > 0, "deposit must be positive")?;
        require(!self.deposits.contains(&proof), "deposit already credited")?;
        let mut next = self.clone();
        next.credit(actor, amount)?;
        next.total_funded = checked(next.total_funded.checked_add(amount))?;
        next.deposits.insert(proof);
        next.finish()?;
        *self = next;
        Ok(())
    }
    /// A failed operation leaves the complete ledger and nonce unchanged.
    pub fn apply(
        &mut self,
        command: &SignedCommand,
        signature: &[u8],
        context: &Context,
    ) -> Result<()> {
        require(
            command.network == self.network && command.vault == self.vault,
            "wrong signing domain",
        )?;
        require(
            command.deadline_ms >= context.now_ms
                && command.deadline_ms.saturating_sub(context.now_ms) <= 300_000,
            "command expired or deadline too distant",
        )?;
        let key = VerifyingKey::from_bytes(&identity(&command.actor)?)
            .map_err(|_| "invalid signing key")?;
        let signature = Signature::from_slice(signature).map_err(|_| "invalid signature length")?;
        key.verify_strict(&command.signing_bytes()?, &signature)
            .map_err(|_| "invalid command signature")?;
        confidential_protocol::validate_recipient(identity(&command.reply_key)?)?;
        let nonce = self.accounts.get(&command.actor).map_or(0, |a| a.nonce);
        require(
            nonce == command.nonce,
            "command nonce already used or out of order",
        )?;
        let mut next = self.clone();
        next.execute(&command.actor, &command.command, context)?;
        next.accounts
            .entry(command.actor.clone())
            .or_default()
            .nonce = checked(nonce.checked_add(1))?;
        next.finish()?;
        *self = next;
        Ok(())
    }
    fn validate_invariants(&self) -> Result<()> {
        require(self.pool.reserved <= self.pool.equity, "pool is insolvent")?;
        let sum = self
            .accounts
            .values()
            .try_fold(0u128, |s, a| checked(s.checked_add(a.cash)))?;
        let notes = self
            .notes
            .values()
            .try_fold(0u128, |s, n| checked(s.checked_add(n.amount)))?;
        let held = checked(
            checked(sum.checked_add(notes))?.checked_add(self.pool.pool().expected_custody()),
        )?;
        require(
            held == checked(self.total_funded.checked_sub(self.total_redeemed))?,
            "custody conservation failed",
        )?;
        Ok(())
    }
    fn finish(&mut self) -> Result<()> {
        self.validate_invariants()?;
        self.sequence = checked(self.sequence.checked_add(1))?;
        Ok(())
    }
    fn credit(&mut self, actor: &str, amount: u128) -> Result<()> {
        let a = self.accounts.entry(actor.into()).or_default();
        a.cash = checked(a.cash.checked_add(amount))?;
        Ok(())
    }
    fn debit(&mut self, actor: &str, amount: u128) -> Result<()> {
        let a = self.accounts.entry(actor.into()).or_default();
        a.cash = a
            .cash
            .checked_sub(amount)
            .ok_or("insufficient private balance")?;
        Ok(())
    }
    fn subscribed(&self, actor: &str, profile: &str, now: u64) -> bool {
        self.subscriptions
            .get(&pair(actor, profile))
            .is_some_and(|s| s.until_ms > now)
    }
    /// Only this deliberately bounded projection may be published. It contains
    /// no command identities, wallet addresses, trade times or position IDs.
    /// Exact aggregates can still fingerprint activity; opt-in is essential.
    pub fn public_profile(&self, id: &str) -> Result<PublicProfile> {
        let p = self.profiles.get(id).ok_or("profile not found")?;
        let performance = if p.public_performance {
            let mut entry = 0u128;
            let mut proceeds = 0u128;
            let mut count = 0u64;
            for r in self.records.iter().filter(|r| r.owner == p.owner) {
                entry = checked(entry.checked_add(r.premium))?;
                proceeds = checked(proceeds.checked_add(r.proceeds))?;
                count = checked(count.checked_add(1))?;
            }
            let realized_pnl = if proceeds >= entry {
                (proceeds - entry).to_string()
            } else {
                format!("-{}", entry - proceeds)
            };
            Some(PublicPerformance {
                closed_trades: count,
                open_positions: self
                    .positions
                    .values()
                    .filter(|position| position.owner == p.owner)
                    .count() as u64,
                entry_premiums: entry,
                exit_and_settlement_proceeds: proceeds,
                realized_pnl,
            })
        } else {
            None
        };
        Ok(PublicProfile {
            id: id.into(),
            price: p.price,
            period_ms: p.period_ms,
            performance,
        })
    }
    /// Authenticated read consumes the account nonce like a command. The host
    /// commits the checkpoint, then encrypts these private records to reply_key.
    /// A failed or expired read does not consume a nonce or disclose records.
    pub fn read_journal(
        &mut self,
        request: &JournalRequest,
        signature: &[u8],
        now: u64,
    ) -> Result<JournalPage> {
        require(
            request.network == self.network && request.vault == self.vault,
            "wrong journal signing domain",
        )?;
        require(
            request.deadline_ms >= now && request.deadline_ms.saturating_sub(now) <= 300_000,
            "journal request expired or deadline too distant",
        )?;
        confidential_protocol::validate_recipient(identity(&request.reply_key)?)?;
        require(
            (1..=4).contains(&request.limit),
            "journal page limit must be 1 to 4",
        )?;
        let key = VerifyingKey::from_bytes(&identity(&request.actor)?)
            .map_err(|_| "invalid signing key")?;
        let signature = Signature::from_slice(signature).map_err(|_| "invalid signature length")?;
        key.verify_strict(&request.signing_bytes()?, &signature)
            .map_err(|_| "invalid journal signature")?;
        let nonce = self.accounts.get(&request.actor).map_or(0, |a| a.nonce);
        require(
            nonce == request.nonce,
            "journal nonce already used or out of order",
        )?;
        let p = self
            .profiles
            .get(&request.profile)
            .ok_or("profile not found")?;
        require(
            p.owner == request.actor || self.subscribed(&request.actor, &request.profile, now),
            "subscription required",
        )?;
        let cursor = usize::try_from(request.cursor).map_err(|_| "invalid journal cursor")?;
        let mut matching = self
            .records
            .iter()
            .filter(|r| {
                r.owner == p.owner && r.profile.as_deref() == Some(request.profile.as_str())
            })
            .skip(cursor)
            .peekable();
        let records: Vec<Record> = matching
            .by_ref()
            .take(request.limit as usize)
            .cloned()
            .collect();
        let next_cursor = if matching.peek().is_some() {
            Some(checked(request.cursor.checked_add(records.len() as u64))?)
        } else {
            None
        };
        let page = JournalPage {
            records,
            next_cursor,
        };
        // Leave room for response status/checkpoint metadata inside the fixed
        // 4096-byte padded transport. Refuse before consuming the nonce.
        require(
            serde_json::to_vec(&page)
                .map_err(|_| "journal serialization failed")?
                .len()
                <= 3000,
            "journal page exceeds private response limit",
        )?;
        let mut next = self.clone();
        next.accounts
            .entry(request.actor.clone())
            .or_default()
            .nonce = checked(nonce.checked_add(1))?;
        next.finish()?;
        *self = next;
        Ok(page)
    }
    fn execute(&mut self, actor: &str, command: &Command, context: &Context) -> Result<()> {
        match command {
            Command::CreateProfile {
                id,
                price,
                period_ms,
            } => {
                identity(id)?;
                require(!self.profiles.contains_key(id), "profile already exists")?;
                require(
                    *price > 0 && (60_000..=31_536_000_000).contains(period_ms),
                    "invalid subscription terms",
                )?;
                self.profiles.insert(
                    id.clone(),
                    Profile {
                        owner: actor.into(),
                        price: *price,
                        period_ms: *period_ms,
                        public_performance: false,
                    },
                );
            }
            Command::SetPerformanceDisclosure { profile, enabled } => {
                let p = self.profiles.get_mut(profile).ok_or("profile not found")?;
                require(
                    p.owner == actor,
                    "only the creator can disclose performance",
                )?;
                p.public_performance = *enabled;
            }
            Command::Subscribe { profile, max_price } => {
                let p = self
                    .profiles
                    .get(profile)
                    .ok_or("profile not found")?
                    .clone();
                require(
                    p.owner != actor && p.price <= *max_price,
                    "subscription price exceeds authorization or self-subscription",
                )?;
                self.debit(actor, p.price)?;
                self.credit(&p.owner, p.price)?;
                let k = pair(actor, profile);
                let from = self
                    .subscriptions
                    .get(&k)
                    .map_or(context.now_ms, |s| s.until_ms.max(context.now_ms));
                self.subscriptions.insert(
                    k,
                    Subscription {
                        subscriber: actor.into(),
                        profile: profile.clone(),
                        until_ms: checked(from.checked_add(p.period_ms))?,
                    },
                );
            }
            Command::SetMandate { mandate } => {
                require(
                    self.subscribed(actor, &mandate.profile, context.now_ms),
                    "active subscription required",
                )?;
                require(
                    mandate.spent == 0
                        && mandate.quantity > 0
                        && mandate.quantity % SCALE == 0
                        && mandate.quantity <= 10_000 * SCALE
                        && mandate.max_each > 0
                        && mandate.max_total >= mandate.max_each
                        && mandate.max_exit_loss_bps <= 10_000,
                    "invalid copy budget or size",
                )?;
                require(
                    mandate.until_ms > context.now_ms
                        && mandate.max_expiry_ms > context.now_ms
                        && ["ETH", "BTC"].contains(&mandate.asset.as_str()),
                    "invalid mandate expiry or asset",
                )?;
                self.mandates
                    .insert(pair(actor, &mandate.profile), mandate.clone());
            }
            Command::RevokeMandate { profile } => {
                self.mandates.remove(&pair(actor, profile));
            }
            Command::PoolDeposit { amount } => {
                self.debit(actor, *amount)?;
                let mut p = self.pool.pool();
                let shares = checked(p.deposit(*amount))?;
                self.pool.update(p);
                let a = self.accounts.entry(actor.into()).or_default();
                a.lp_shares = checked(a.lp_shares.checked_add(shares))?;
            }
            Command::PoolWithdraw { shares } => {
                let a = self.accounts.entry(actor.into()).or_default();
                a.lp_shares = checked(a.lp_shares.checked_sub(*shares))?;
                let mut p = self.pool.pool();
                let amount = checked(p.withdraw(*shares))?;
                self.pool.update(p);
                self.credit(actor, amount)?;
            }
            Command::Buy {
                series,
                quantity,
                max_premium,
                held,
                publish,
            } => {
                if let Some(profile) = publish {
                    require(
                        self.profiles.get(profile).is_some_and(|p| p.owner == actor),
                        "cannot publish to another trader's profile",
                    )?;
                }
                let id = self.buy(
                    actor,
                    *series,
                    *quantity,
                    *max_premium,
                    *held,
                    publish.clone(),
                    None,
                    context,
                )?;
                if let Some(profile) = publish {
                    self.copy_buy(profile, id, *series, *held, context);
                }
            }
            Command::Close {
                position,
                min_proceeds,
            } => {
                let published = self.positions.get(position).and_then(|p| p.profile.clone());
                self.close(actor, *position, *min_proceeds, context)?;
                if let Some(profile) = published {
                    self.copy_close(&profile, *position, context);
                }
            }
            Command::Settle { position } => {
                self.settle(actor, *position, context)?;
            }
            Command::Withdraw { note, amount } => {
                identity(note)?;
                require(
                    *amount > 0
                        && !self.notes.contains_key(note)
                        && !self.spent_notes.contains(note),
                    "invalid or reused withdrawal note",
                )?;
                self.debit(actor, *amount)?;
                self.notes.insert(
                    note.clone(),
                    WithdrawalNote {
                        owner: actor.into(),
                        amount: *amount,
                    },
                );
            }
        }
        Ok(())
    }
    fn market<'a>(context: &'a Context, series: u64) -> Result<&'a Market> {
        let m = context.markets.get(&series).ok_or("market not found")?;
        require(
            m.high > m.low && m.low > 0 && ["ETH", "BTC"].contains(&m.asset.as_str()),
            "invalid market terms",
        )?;
        Ok(m)
    }
    fn live_market<'a>(context: &'a Context, series: u64) -> Result<&'a Market> {
        let m = Self::market(context, series)?;
        require(
            context.now_ms < m.expiry_ms && m.fixed_price.is_none() && !m.voided,
            "market expired",
        )?;
        require(
            m.quote_at_ms <= context.now_ms
                && context.now_ms - m.quote_at_ms <= 60_000
                && m.mid > 0
                && m.mid <= m.high - m.low,
            "stale or invalid quote",
        )?;
        Ok(m)
    }
    fn buy(
        &mut self,
        actor: &str,
        series: u64,
        quantity: u128,
        max: u128,
        held: bool,
        profile: Option<String>,
        copied_from: Option<u64>,
        context: &Context,
    ) -> Result<u64> {
        require(
            quantity > 0 && quantity % SCALE == 0 && quantity <= 10_000 * SCALE,
            "invalid contract quantity",
        )?;
        let m = Self::live_market(context, series)?;
        let terms = if held { Terms::FLAT } else { Terms::VENUE };
        let premium = checked(terms.entry_premium(quantity, m.mid, SCALE))?;
        require(premium <= max, "premium exceeds authorization")?;
        self.debit(actor, premium)?;
        let lp = checked(terms.lp_notional(premium))?;
        let fee = checked(terms.venue_cut(premium))?;
        let mut pool = self.pool.pool();
        let written = checked(pool.write(quantity, m.high - m.low, SCALE, lp, fee))?;
        self.pool.update(pool);
        let id = self.next_position;
        self.next_position = checked(id.checked_add(1))?;
        self.positions.insert(
            id,
            Position {
                owner: actor.into(),
                series,
                terms: SeriesTerms::from(m),
                quantity,
                premium,
                lp_notional: lp,
                venue_cut: fee,
                max_payout: written.max_payout,
                held,
                copied_from,
                profile,
            },
        );
        Ok(id)
    }
    fn copy_buy(
        &mut self,
        profile: &str,
        leader_id: u64,
        series: u64,
        held: bool,
        context: &Context,
    ) {
        let Some(market) = context.markets.get(&series) else {
            return;
        };
        let mandates: Vec<_> = self
            .mandates
            .iter()
            .filter(|(_, m)| m.profile == profile)
            .map(|(k, m)| (k.clone(), m.clone()))
            .collect();
        for (k, m) in mandates {
            let actor = k.split(':').next().unwrap_or("");
            if m.until_ms <= context.now_ms
                || !self.subscribed(actor, profile, context.now_ms)
                || m.asset != market.asset
                || market.expiry_ms > m.max_expiry_ms
            {
                self.copy_attempts.push(CopyAttempt {
                    subscriber: actor.into(),
                    leader_position: leader_id,
                    at_ms: context.now_ms,
                    phase: "entry".into(),
                    outcome: "subscription_or_scope_refused".into(),
                });
                continue;
            }
            let Some(left) = m.max_total.checked_sub(m.spent) else {
                continue;
            };
            let mut candidate = self.clone();
            if let Ok(id) = candidate.buy(
                actor,
                series,
                m.quantity,
                m.max_each.min(left),
                held,
                None,
                Some(leader_id),
                context,
            ) {
                let spent = candidate.positions[&id].premium;
                if let Some(updated) = candidate.mandates.get_mut(&k) {
                    if let Some(total) = updated.spent.checked_add(spent) {
                        updated.spent = total;
                        *self = candidate;
                        self.copy_attempts.push(CopyAttempt {
                            subscriber: actor.into(),
                            leader_position: leader_id,
                            at_ms: context.now_ms,
                            phase: "entry".into(),
                            outcome: "copied".into(),
                        });
                    }
                }
            } else {
                self.copy_attempts.push(CopyAttempt {
                    subscriber: actor.into(),
                    leader_position: leader_id,
                    at_ms: context.now_ms,
                    phase: "entry".into(),
                    outcome: "budget_balance_quote_or_liquidity_refused".into(),
                });
            }
        }
    }
    fn copy_close(&mut self, profile: &str, leader_id: u64, context: &Context) {
        let copies: Vec<_> = self
            .positions
            .iter()
            .filter(|(_, p)| p.copied_from == Some(leader_id))
            .map(|(id, p)| (*id, p.owner.clone()))
            .collect();
        for (id, actor) in copies {
            if !self.subscribed(&actor, profile, context.now_ms)
                || !self
                    .mandates
                    .get(&pair(&actor, profile))
                    .is_some_and(|m| m.until_ms > context.now_ms)
            {
                self.copy_attempts.push(CopyAttempt {
                    subscriber: actor.clone(),
                    leader_position: leader_id,
                    at_ms: context.now_ms,
                    phase: "exit".into(),
                    outcome: "subscription_or_mandate_refused".into(),
                });
                continue;
            }
            let mut next = self.clone();
            let mandate = &self.mandates[&pair(&actor, profile)];
            let premium = self.positions[&id].premium;
            let Some(numerator) =
                premium.checked_mul(u128::from(10_000 - mandate.max_exit_loss_bps))
            else {
                continue;
            };
            // Round the floor upward so integer division cannot authorize a
            // loss larger than the subscriber explicitly approved.
            let minimum = numerator / 10_000 + u128::from(numerator % 10_000 != 0);
            // A stale quote or an exit below the signed loss floor is refused.
            if next.close(&actor, id, minimum, context).is_ok() {
                *self = next;
                self.copy_attempts.push(CopyAttempt {
                    subscriber: actor,
                    leader_position: leader_id,
                    at_ms: context.now_ms,
                    phase: "exit".into(),
                    outcome: "copied".into(),
                });
            } else {
                self.copy_attempts.push(CopyAttempt {
                    subscriber: actor,
                    leader_position: leader_id,
                    at_ms: context.now_ms,
                    phase: "exit".into(),
                    outcome: "loss_floor_quote_or_terms_refused".into(),
                });
            }
        }
    }
    /// Host-only terminal settlement from authenticated oracle context. No
    /// spending authority is needed: payout can only go to each existing owner.
    /// Do not accept arbitrary user-supplied Context at a service boundary.
    pub fn settle_verified_matured(&mut self, context: &Context) -> Result<usize> {
        let ids: Vec<_> = self
            .positions
            .iter()
            .filter(|(_, p)| {
                context.markets.get(&p.series).is_some_and(|m| {
                    context.now_ms >= m.expiry_ms && (m.voided || m.fixed_price.is_some())
                })
            })
            .map(|(id, p)| (*id, p.owner.clone()))
            .collect();
        if ids.is_empty() {
            return Ok(0);
        }
        let mut next = self.clone();
        for (id, actor) in &ids {
            next.settle(actor, *id, context)?;
        }
        next.finish()?;
        *self = next;
        Ok(ids.len())
    }
    fn written(p: &Position) -> Written {
        Written {
            max_payout: p.max_payout,
            lp_notional: p.lp_notional,
            venue_cut: p.venue_cut,
        }
    }
    fn close(&mut self, actor: &str, id: u64, min: u128, context: &Context) -> Result<()> {
        let p = self.positions.get(&id).ok_or("position not found")?.clone();
        require(p.owner == actor, "not position owner")?;
        require(!p.held, "held position has no early exit")?;
        let m = Self::live_market(context, p.series)?;
        require(p.terms == SeriesTerms::from(m), "series terms changed")?;
        let proceeds = checked(Terms::VENUE.exit_value(p.quantity, m.mid, SCALE))?;
        require(proceeds >= min, "exit below authorization")?;
        let mut pool = self.pool.pool();
        checked(pool.close(&Self::written(&p), proceeds))?;
        self.pool.update(pool);
        self.credit(actor, proceeds)?;
        self.positions.remove(&id);
        self.records.push(Record {
            position: id,
            owner: actor.into(),
            profile: p.profile,
            series: p.series,
            premium: p.premium,
            proceeds,
            at_ms: context.now_ms,
        });
        Ok(())
    }
    fn settle(&mut self, actor: &str, id: u64, context: &Context) -> Result<()> {
        let p = self.positions.get(&id).ok_or("position not found")?.clone();
        require(p.owner == actor, "not position owner")?;
        let m = Self::market(context, p.series)?;
        require(p.terms == SeriesTerms::from(m), "series terms changed")?;
        require(context.now_ms >= m.expiry_ms, "not expired")?;
        let mut pool = self.pool.pool();
        let proceeds = if m.voided {
            checked(pool.void_out(&Self::written(&p)))?;
            p.premium
        } else {
            let price = m.fixed_price.ok_or("verified settlement price missing")?;
            let value = checked(match m.kind {
                Kind::Put => accounting::payout(p.quantity, SCALE, m.high, m.low, price),
                Kind::Call => accounting::payout_call(p.quantity, SCALE, m.high, m.low, price),
            })?;
            checked(pool.settle(&Self::written(&p), value))?;
            value
        };
        self.pool.update(pool);
        self.credit(actor, proceeds)?;
        self.positions.remove(&id);
        self.records.push(Record {
            position: id,
            owner: actor.into(),
            profile: p.profile,
            series: p.series,
            premium: p.premium,
            proceeds,
            at_ms: context.now_ms,
        });
        Ok(())
    }
    /// Host-only: only after the shielded redemption is finalized. Pending
    /// notes remain liabilities until then, even if an HTTP request timed out.
    pub fn complete_verified_redemption(&mut self, note: &str) -> Result<()> {
        let mut next = self.clone();
        let n = next
            .notes
            .remove(note)
            .ok_or("note missing or already redeemed")?;
        next.total_redeemed = checked(next.total_redeemed.checked_add(n.amount))?;
        next.spent_notes.insert(note.into());
        next.finish()?;
        *self = next;
        Ok(())
    }
}

impl std::fmt::Debug for Ledger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ledger")
            .field("sequence", &self.sequence)
            .field("private_state", &"[REDACTED]")
            .finish()
    }
}
