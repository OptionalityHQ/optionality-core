//! The price the venue settles against, from sources the venue does not own.
//!
//! A source registers an ed25519 key and signs every observation it publishes, so
//! whoever relays the transaction cannot invent a price on a source's behalf — the
//! relay is transport, not authority. An observation on its own is only ever a
//! candidate: it becomes a *fix*, the number the options contract pays out on,
//! once a quorum of distinct sources agrees on it inside a tolerance band. The
//! venue's own pricing engine holds one seat at that table and no more weight than
//! any other seat, so the party that quoted a ticket can no longer decide whether
//! it won.

use near_sdk::borsh::BorshSerialize;
use near_sdk::store::LookupMap;
use near_sdk::{env, near, require, AccountId, BorshStorageKey, PanicOnDefault};

/// The signed layout and its domain prefix live in `bounded_core::oracle`, shared
/// with the relay so the two cannot disagree about what a price was.
use bounded_core::oracle::signing_message;

const MAX_SOURCES: usize = 16;
const MAX_SUBMIT: usize = 64;
const MAX_TAG_LEN: usize = 32;
/// A source keeps its own clock. Five seconds of slack covers the skew between
/// machines; anything further ahead is a bug or an attack.
const FUTURE_SLACK_NS: u64 = 5_000_000_000;

#[derive(BorshStorageKey, BorshSerialize)]
#[borsh(crate = "near_sdk::borsh")]
enum StorageKey {
    Latest,
    Fixes,
}

/// One source's latest word on one feed, as a candidate price.
#[near(serializers = [json, borsh])]
#[derive(Clone, Debug)]
pub struct Observation {
    pub price: u128,
    pub publish_time_ns: u64,
    pub confidence_bps: u16,
}

/// An observation as it arrives, with the signature its source's key produced.
#[near(serializers = [json])]
#[derive(Clone, Debug)]
pub struct SignedObservation {
    pub feed_id: String,
    pub source: String,
    pub price: u128,
    pub publish_time_ns: u64,
    pub confidence_bps: u16,
    pub signature: Vec<u8>,
}

/// A price the committee agreed on. The options contract reads these through
/// `get_observations` — the same call it made against a single-reporter oracle,
/// except that every answer is now a quorum and names who stood behind it.
#[near(serializers = [json, borsh])]
#[derive(Clone, Debug, PartialEq)]
pub struct Fix {
    pub price: u128,
    pub publish_time_ns: u64,
    pub confidence_bps: u16,
    /// How far the agreeing sources were apart. The confidence below is made of
    /// this, so a committee that barely agrees settles like the shaky committee
    /// it is.
    pub dispersion_bps: u32,
    pub sources: Vec<String>,
    pub fixed_at_ns: u64,
}

#[near(serializers = [json, borsh])]
#[derive(Clone, Debug)]
pub struct Source {
    pub name: String,
    pub public_key: Vec<u8>,
    pub added_ns: u64,
    /// Non-zero once retired. The observations stay so a series that already
    /// fixed on this source can still say who priced it; the seat just stops
    /// counting toward a quorum.
    pub removed_ns: u64,
}

#[near(serializers = [json, borsh])]
#[derive(Clone, Debug)]
struct Stored {
    observation: Observation,
    /// This contract's clock when the observation was accepted. Freshness is
    /// measured against it, so a source cannot keep its seat warm by stamping a
    /// future time and then going quiet.
    seen_at_ns: u64,
}

#[near(serializers = [json])]
#[derive(Clone, Debug)]
pub struct Seat {
    pub source: String,
    pub price: Option<u128>,
    pub publish_time_ns: u64,
    pub confidence_bps: u16,
    pub seen_at_ns: u64,
    pub age_ns: u64,
    pub fresh: bool,
}

/// Who is at the table for a feed, what each said, and whether they agree right
/// now. Read by the screen, so a reviewer sees the venue's own engine as one row
/// among several — fresh or stale like everybody else.
#[near(serializers = [json])]
#[derive(Clone, Debug)]
pub struct Committee {
    pub feed_id: String,
    pub now_ns: u64,
    pub quorum: u32,
    pub tolerance_bps: u32,
    pub max_age_ns: u64,
    pub active_sources: u32,
    pub fresh_sources: u32,
    pub agreeing_sources: u32,
    pub can_fix: bool,
    pub seats: Vec<Seat>,
    pub last_fix: Option<Fix>,
}

#[near(serializers = [json])]
#[derive(Clone, Debug)]
pub struct SubmitResult {
    pub accepted: u32,
    pub fixed: Vec<String>,
    pub rejected: Vec<String>,
}

#[near(contract_state)]
#[derive(PanicOnDefault)]
pub struct Contract {
    owner: AccountId,
    sources: Vec<Source>,
    latest: LookupMap<String, Stored>,
    fixes: LookupMap<String, Vec<Fix>>,
    quorum: u32,
    tolerance_bps: u32,
    max_age_ns: u64,
    max_history: u32,
}

fn key_for(feed: &str, source: &str) -> String {
    format!("{feed}\u{1f}{source}")
}

fn is_tag(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_TAG_LEN
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
}

fn spread_bps(low: u128, high: u128) -> u32 {
    if high == 0 {
        return 10_000;
    }
    (((high - low) * 10_000 / high).min(10_000)) as u32
}

/// Whether one price sits inside the band around the median, in basis points of
/// that median, on either side of it.
fn within(price: u128, median: u128, tolerance_bps: u32) -> bool {
    if median == 0 {
        return false;
    }
    let distance = price.max(median) - price.min(median);
    distance.saturating_mul(10_000) <= median.saturating_mul(tolerance_bps as u128)
}

#[near]
impl Contract {
    #[init]
    pub fn new(owner: AccountId) -> Self {
        Self {
            owner,
            sources: Vec::new(),
            latest: LookupMap::new(StorageKey::Latest),
            fixes: LookupMap::new(StorageKey::Fixes),
            // Two is the smallest rule that makes any single seat — including the
            // venue's own engine — unable to settle a ticket by itself.
            quorum: 2,
            tolerance_bps: 100,
            max_age_ns: 120_000_000_000,
            max_history: 200,
        }
    }

    // ---------- committee ----------

    /// Seat a source. The key is what speaks, not the account that relays it, so
    /// one operator can run the transport for several independent sources and
    /// still be unable to set a price alone.
    pub fn add_source(&mut self, name: String, public_key: Vec<u8>) {
        self.assert_owner();
        require!(is_tag(&name), "source name must be a short id");
        require!(public_key.len() == 32, "an ed25519 key is 32 bytes");
        require!(self.sources.len() < MAX_SOURCES, "the committee is full");
        require!(
            self.sources.iter().all(|s| s.name != name || s.removed_ns != 0),
            "source already seated"
        );
        self.sources.push(Source {
            name: name.clone(),
            public_key,
            added_ns: env::block_timestamp(),
            removed_ns: 0,
        });
        emit("source_added", &format!("{{\"source\":\"{name}\"}}"));
    }

    /// Empty the seat. Its observations stop counting immediately; every fix it
    /// already took part in keeps naming it.
    pub fn remove_source(&mut self, name: String) {
        self.assert_owner();
        let now = env::block_timestamp();
        let source = self
            .sources
            .iter_mut()
            .find(|s| s.name == name && s.removed_ns == 0)
            .expect("source not seated");
        source.removed_ns = now;
        emit("source_removed", &format!("{{\"source\":\"{name}\"}}"));
    }

    pub fn set_quorum(&mut self, quorum: u32) {
        self.assert_owner();
        require!(quorum >= 2, "one source is not a quorum");
        require!(quorum <= MAX_SOURCES as u32, "quorum larger than any committee");
        self.quorum = quorum;
    }

    pub fn set_tolerance_bps(&mut self, tolerance_bps: u32) {
        self.assert_owner();
        require!(tolerance_bps > 0 && tolerance_bps <= 1_000, "tolerance out of range");
        self.tolerance_bps = tolerance_bps;
    }

    pub fn set_max_age_ns(&mut self, max_age_ns: u64) {
        self.assert_owner();
        require!(max_age_ns >= 5_000_000_000, "a seat cannot go stale that fast");
        require!(max_age_ns <= 3_600_000_000_000, "an hour is old enough");
        self.max_age_ns = max_age_ns;
    }

    pub fn set_max_history(&mut self, max_history: u32) {
        self.assert_owner();
        require!(max_history > 0 && max_history <= 10_000, "invalid history");
        self.max_history = max_history;
    }

    // ---------- ingestion ----------

    /// The relay's only power: deliver what the sources signed. Each item is
    /// checked against its own source's key, and a forged, stale or
    /// future-stamped item is dropped with a reason instead of stopping the
    /// batch — one broken source cannot hold the others' prices off the chain.
    pub fn submit_observations(&mut self, observations: Vec<SignedObservation>) -> SubmitResult {
        require!(observations.len() <= MAX_SUBMIT, "too many observations at once");
        let now = env::block_timestamp();
        let mut accepted = 0u32;
        let mut rejected: Vec<String> = Vec::new();
        let mut touched: Vec<String> = Vec::new();

        for observation in observations {
            let SignedObservation {
                feed_id,
                source,
                price,
                publish_time_ns,
                confidence_bps,
                signature,
            } = observation;
            let reason = if !is_tag(&feed_id) {
                Some("feed id is not a short tag")
            } else if price == 0 {
                Some("a print of zero is not a price")
            } else if confidence_bps > 10_000 {
                Some("confidence is not a fraction")
            } else if publish_time_ns > now.saturating_add(FUTURE_SLACK_NS) {
                Some("stamped in the future")
            } else {
                None
            };
            if let Some(reason) = reason {
                rejected.push(format!("{feed_id}/{source}: {reason}"));
                continue;
            }
            let seat = self.sources.iter().find(|s| s.name == source && s.removed_ns == 0).cloned();
            let Some(seat) = seat else {
                rejected.push(format!("{feed_id}/{source}: source is not seated"));
                continue;
            };
            if let Some(previous) = self.latest.get(&key_for(&feed_id, &source)) {
                if publish_time_ns <= previous.observation.publish_time_ns {
                    rejected.push(format!(
                        "{feed_id}/{source}: must advance past {}",
                        previous.observation.publish_time_ns
                    ));
                    continue;
                }
            }
            let signed = signing_message(&feed_id, &source, price, publish_time_ns, confidence_bps);
            let signature_ok = <[u8; 32]>::try_from(seat.public_key.as_slice())
                .ok()
                .zip(<[u8; 64]>::try_from(signature.as_slice()).ok())
                .is_some_and(|(key, sig)| env::ed25519_verify(&sig, &signed, &key));
            if !signature_ok {
                rejected.push(format!("{feed_id}/{source}: signature is not this source's"));
                continue;
            }

            self.latest.insert(
                key_for(&feed_id, &source),
                Stored {
                    observation: Observation { price, publish_time_ns, confidence_bps },
                    seen_at_ns: now,
                },
            );
            accepted += 1;
            if !touched.iter().any(|seen| seen == &feed_id) {
                touched.push(feed_id);
            }
        }

        let mut fixed = Vec::new();
        for feed_id in touched {
            let Some(fix) = self.agree(&feed_id, now) else { continue };
            emit(
                "oracle_fixed",
                &near_sdk::serde_json::json!({
                    "feed_id": feed_id,
                    "price": fix.price.to_string(),
                    "publish_time_ns": fix.publish_time_ns,
                    "dispersion_bps": fix.dispersion_bps,
                    "sources": fix.sources,
                })
                .to_string(),
            );
            let mut history = self.fixes.get(&feed_id).cloned().unwrap_or_default();
            history.push(fix);
            while history.len() > self.max_history as usize {
                history.remove(0);
            }
            self.fixes.insert(feed_id.clone(), history);
            fixed.push(feed_id);
        }

        SubmitResult { accepted, fixed, rejected }
    }

    // ---------- views ----------

    /// The settlement interface the options contract calls. Oldest first, so the
    /// venue fixes on the price the committee agreed nearest the expiry rather
    /// than on whatever it happens to agree on now.
    pub fn get_observations(
        &self,
        feed_id: String,
        from_ns: u64,
        to_ns: u64,
        limit: u32,
    ) -> Vec<Fix> {
        let Some(history) = self.fixes.get(&feed_id) else {
            return Vec::new();
        };
        history
            .iter()
            .filter(|fix| fix.publish_time_ns >= from_ns && fix.publish_time_ns <= to_ns)
            .take(limit.min(100) as usize)
            .cloned()
            .collect()
    }

    pub fn get_latest_fix(&self, feed_id: String) -> Option<Fix> {
        self.fixes.get(&feed_id).and_then(|history| history.last().cloned())
    }

    pub fn get_committee(&self, feed_id: String) -> Committee {
        let now = env::block_timestamp();
        let active: Vec<Source> = self.sources.iter().filter(|s| s.removed_ns == 0).cloned().collect();
        let mut seats = Vec::new();
        let mut candidates: Vec<(u128, String, u64)> = Vec::new();
        for source in &active {
            let stored = self.latest.get(&key_for(&feed_id, &source.name)).cloned();
            let age_ns = stored.as_ref().map(|s| now.saturating_sub(s.seen_at_ns)).unwrap_or(0);
            let fresh = stored
                .as_ref()
                .is_some_and(|s| age_ns <= self.max_age_ns && s.observation.publish_time_ns <= now);
            if let (true, Some(stored)) = (fresh, stored.as_ref()) {
                candidates.push((
                    stored.observation.price,
                    source.name.clone(),
                    stored.observation.publish_time_ns,
                ));
            }
            seats.push(Seat {
                source: source.name.clone(),
                price: stored.as_ref().map(|s| s.observation.price),
                publish_time_ns: stored.as_ref().map(|s| s.observation.publish_time_ns).unwrap_or(0),
                confidence_bps: stored.as_ref().map(|s| s.observation.confidence_bps).unwrap_or(0),
                seen_at_ns: stored.as_ref().map(|s| s.seen_at_ns).unwrap_or(0),
                age_ns,
                fresh,
            });
        }
        let agreeing = self.agreeing(&candidates).len() as u32;
        let last_fix = self.fixes.get(&feed_id).and_then(|history| history.last().cloned());
        Committee {
            feed_id,
            now_ns: now,
            quorum: self.quorum,
            tolerance_bps: self.tolerance_bps,
            max_age_ns: self.max_age_ns,
            active_sources: active.len() as u32,
            fresh_sources: candidates.len() as u32,
            agreeing_sources: agreeing,
            can_fix: agreeing >= self.quorum,
            seats,
            last_fix,
        }
    }

    pub fn get_sources(&self) -> Vec<Source> {
        self.sources.clone()
    }

    pub fn get_config(&self) -> (u32, u32, u64, u32) {
        (self.quorum, self.tolerance_bps, self.max_age_ns, self.max_history)
    }

    pub fn get_owner(&self) -> AccountId {
        self.owner.clone()
    }
}

impl Contract {
    fn assert_owner(&self) {
        require!(env::predecessor_account_id() == self.owner, "only the oracle owner");
    }

    /// The committee's current price for a feed, or nothing.
    fn agree(&self, feed_id: &str, now_ns: u64) -> Option<Fix> {
        let mut candidates: Vec<(u128, String, u64)> = Vec::new();
        for source in self.sources.iter().filter(|s| s.removed_ns == 0) {
            let Some(stored) = self.latest.get(&key_for(feed_id, &source.name)) else { continue };
            if now_ns.saturating_sub(stored.seen_at_ns) > self.max_age_ns {
                continue;
            }
            if stored.observation.publish_time_ns > now_ns {
                continue;
            }
            candidates.push((
                stored.observation.price,
                source.name.clone(),
                stored.observation.publish_time_ns,
            ));
        }
        let kept = self.agreeing(&candidates);
        if (kept.len() as u32) < self.quorum {
            return None;
        }
        let low = kept.first().map(|c| c.0).unwrap_or(0);
        let high = kept.last().map(|c| c.0).unwrap_or(0);
        let dispersion_bps = spread_bps(low, high);
        // The upper middle of the agreeing subset: an actual print with an actual
        // timestamp, not a number this contract invented.
        let middle = &kept[kept.len() / 2];
        Some(Fix {
            price: middle.0,
            publish_time_ns: middle.2,
            confidence_bps: 10_000u32.saturating_sub(dispersion_bps) as u16,
            dispersion_bps,
            sources: kept.iter().map(|(_, name, _)| name.clone()).collect(),
            fixed_at_ns: now_ns,
        })
    }

    /// The group that agrees with the median. Counting a subset rather than
    /// demanding every seat sit inside the band is what stops one lagging exchange
    /// from vetoing every settlement on the venue.
    fn agreeing(&self, candidates: &[(u128, String, u64)]) -> Vec<(u128, String, u64)> {
        let mut sorted: Vec<(u128, String, u64)> = candidates.to_vec();
        if (sorted.len() as u32) < self.quorum {
            return Vec::new();
        }
        sorted.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        let median = sorted[sorted.len() / 2].0;
        let kept: Vec<(u128, String, u64)> = sorted
            .into_iter()
            .filter(|(price, _, _)| within(*price, median, self.tolerance_bps))
            .collect();
        if (kept.len() as u32) < self.quorum {
            return Vec::new();
        }
        kept
    }
}

fn emit(event: &str, data: &str) {
    env::log_str(&format!(
        "EVENT_JSON:{{\"standard\":\"oracle-1\",\"version\":\"1.0.0\",\"event\":\"{event}\",\"data\":{data}}}"
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use bounded_core::oracle::DOMAIN;
    use ed25519_dalek::{Signer, SigningKey};
    use near_sdk::test_utils::{accounts, VMContextBuilder};
    use near_sdk::testing_env;

    const NOW: u64 = 1_000_000_000_000;
    const BTC: &str = "BTC-USD";
    /// Prices in the same micro-dollars the venue lists its strikes in.
    const ATM: u128 = 100_000_000_000;

    fn keypair(seed: u8) -> SigningKey {
        let mut bytes = [seed; 32];
        bytes[0] = seed.wrapping_mul(7).max(1);
        SigningKey::from_bytes(&bytes)
    }

    fn at(predecessor: AccountId, now_ns: u64) -> VMContextBuilder {
        let mut builder = VMContextBuilder::new();
        builder.predecessor_account_id(predecessor);
        builder.block_timestamp(now_ns);
        builder
    }

    /// A committee of `count` seats, each with its own key, seated by the owner.
    fn committee(count: u8) -> (Contract, Vec<(String, SigningKey)>) {
        testing_env!(at(accounts(0), NOW).build());
        let mut contract = Contract::new(accounts(0));
        let mut seats = Vec::new();
        for seed in 1..=count {
            let key = keypair(seed);
            let name = format!("src{seed}");
            contract.add_source(name.clone(), key.verifying_key().to_bytes().to_vec());
            seats.push((name, key));
        }
        (contract, seats)
    }

    fn signed(key: &SigningKey, source: &str, price: u128, time: u64) -> SignedObservation {
        let message = signing_message(BTC, source, price, time, 9_900);
        SignedObservation {
            feed_id: BTC.to_string(),
            source: source.to_string(),
            price,
            publish_time_ns: time,
            confidence_bps: 9_900,
            signature: key.sign(&message).to_bytes().to_vec(),
        }
    }

    /// Two honest seats, a tenth of a percent apart, as a batch.
    fn pair(seats: &[(String, SigningKey)], price: u128, time: u64) -> Vec<SignedObservation> {
        vec![
            signed(&seats[0].1, &seats[0].0, price, time),
            signed(&seats[1].1, &seats[1].0, price + price / 1_000, time),
        ]
    }

    #[test]
    fn the_signed_message_prefixes_every_string() {
        let message = signing_message(BTC, "kraken", 7, 8, 9);
        assert_eq!(&message[..DOMAIN.len()], DOMAIN);
        assert_eq!(message[DOMAIN.len()], 7, "the feed id carries its own length");
        assert_eq!(message.len(), DOMAIN.len() + 2 + 7 + 6 + 16 + 8 + 2);
        assert_ne!(
            signing_message("BTC-US", "DKRAKEN", 7, 8, 9),
            message,
            "a shifted boundary cannot reuse another observation's signature"
        );
    }

    #[test]
    fn one_source_alone_is_never_a_fix() {
        let (mut contract, seats) = committee(3);
        let result = contract.submit_observations(vec![signed(&seats[0].1, &seats[0].0, ATM, NOW)]);
        assert_eq!(result.accepted, 1, "the venue's own print is still a candidate");
        assert!(result.fixed.is_empty());
        let view = contract.get_committee(BTC.to_string());
        assert_eq!(view.fresh_sources, 1);
        assert!(!view.can_fix, "one seat cannot settle a ticket");
        assert!(contract.get_observations(BTC.to_string(), 0, u64::MAX, 100).is_empty());
    }

    #[test]
    fn a_quorum_of_two_fixes_and_names_itself() {
        let (mut contract, seats) = committee(3);
        let result = contract.submit_observations(pair(&seats, ATM, NOW - 1));
        assert_eq!(result.fixed, vec![BTC.to_string()]);
        let fixes = contract.get_observations(BTC.to_string(), 0, u64::MAX, 100);
        assert_eq!(fixes.len(), 1);
        assert_eq!(fixes[0].sources, vec![seats[0].0.clone(), seats[1].0.clone()]);
        assert_eq!(fixes[0].price, ATM + ATM / 1_000, "an actual print, not an average");
        assert_eq!(u32::from(fixes[0].confidence_bps), 10_000 - fixes[0].dispersion_bps);
        assert!(fixes[0].dispersion_bps <= 10, "a tenth of a percent apart, measured");
    }

    #[test]
    fn a_signature_from_another_seat_does_not_count() {
        let (mut contract, seats) = committee(3);
        // The relay takes one seat's price and signs it with a different seat's
        // key: the number is real, the authority is not.
        let mut forged = signed(&seats[1].1, &seats[0].0, ATM, NOW - 1);
        forged.signature = seats[2].1.sign(&forged.signature).to_bytes().to_vec();
        let result = contract.submit_observations(vec![
            forged,
            signed(&seats[1].1, &seats[1].0, ATM, NOW - 1),
        ]);
        assert_eq!(result.accepted, 1, "only the honestly signed seat survives");
        assert_eq!(result.rejected.len(), 1);
        assert!(result.rejected[0].contains("signature is not this source's"));
        assert!(
            contract.get_observations(BTC.to_string(), 0, u64::MAX, 100).is_empty(),
            "one real seat and one forgery is not a quorum"
        );
    }

    #[test]
    fn a_lagging_seat_cannot_veto_the_committee() {
        let (mut contract, seats) = committee(4);
        let mut batch = pair(&seats, ATM, NOW - 1);
        batch.push(signed(&seats[2].1, &seats[2].0, ATM * 1_001 / 1_000, NOW - 1));
        // Two percent away: an exchange that stopped printing hours ago.
        batch.push(signed(&seats[3].1, &seats[3].0, ATM * 102 / 100, NOW - 1));
        assert_eq!(contract.submit_observations(batch).fixed.len(), 1);
        let fix = &contract.get_observations(BTC.to_string(), 0, u64::MAX, 100)[0];
        assert_eq!(fix.sources.len(), 3, "the outlier is excluded rather than obeyed");
        assert!(!fix.sources.contains(&seats[3].0));
    }

    #[test]
    fn sources_that_disagree_past_the_tolerance_fix_nothing() {
        let (mut contract, seats) = committee(3);
        let batch = vec![
            signed(&seats[0].1, &seats[0].0, ATM, NOW - 1),
            signed(&seats[1].1, &seats[1].0, ATM * 11 / 10, NOW - 1),
            signed(&seats[2].1, &seats[2].0, ATM * 12 / 10, NOW - 1),
        ];
        let result = contract.submit_observations(batch);
        assert_eq!(result.accepted, 3, "every seat still gets heard");
        assert!(result.fixed.is_empty(), "and none of them gets to be a price");
        assert!(!contract.get_committee(BTC.to_string()).can_fix);
    }

    #[test]
    fn a_stale_seat_stops_counting() {
        let (mut contract, seats) = committee(3);
        assert_eq!(contract.submit_observations(pair(&seats, ATM, NOW - 1)).fixed.len(), 1);
        let later = NOW + 600_000_000_000;
        testing_env!(at(accounts(1), later).build());
        // One seat keeps talking; the others went quiet ten minutes ago.
        let result = contract.submit_observations(vec![signed(
            &seats[0].1,
            &seats[0].0,
            ATM * 101 / 100,
            later - 1,
        )]);
        assert!(result.fixed.is_empty(), "one fresh seat cannot fix on its own");
        let view = contract.get_committee(BTC.to_string());
        assert_eq!(view.fresh_sources, 1);
        assert_eq!(view.seats.iter().filter(|seat| seat.fresh).count(), 1);
    }

    #[test]
    fn a_replayed_price_is_refused() {
        let (mut contract, seats) = committee(3);
        let first = signed(&seats[0].1, &seats[0].0, ATM, NOW - 1);
        assert_eq!(contract.submit_observations(vec![first.clone()]).accepted, 1);
        let result = contract.submit_observations(vec![first]);
        assert_eq!(result.accepted, 0);
        assert!(result.rejected[0].contains("must advance past"));
    }

    #[test]
    fn an_unseated_source_is_refused() {
        let (mut contract, _) = committee(2);
        let key = keypair(9);
        let result = contract.submit_observations(vec![signed(&key, "src9", ATM, NOW - 1)]);
        assert_eq!(result.accepted, 0);
        assert!(result.rejected[0].contains("not seated"));
    }

    #[test]
    fn removing_a_seat_ends_its_vote_but_not_its_record() {
        let (mut contract, seats) = committee(3);
        assert_eq!(contract.submit_observations(pair(&seats, ATM, NOW - 1)).fixed.len(), 1);
        contract.remove_source(seats[1].0.clone());
        let result = contract.submit_observations(vec![
            signed(&seats[0].1, &seats[0].0, ATM * 101 / 100, NOW),
            signed(&seats[1].1, &seats[1].0, ATM * 101 / 100, NOW),
        ]);
        assert_eq!(result.rejected.len(), 1, "the retired seat can no longer speak");
        assert_eq!(result.accepted, 1);
        assert!(result.fixed.is_empty(), "and one remaining seat is not a quorum");
        let fix = contract.get_latest_fix(BTC.to_string()).expect("the earlier fix stands");
        assert!(fix.sources.contains(&seats[1].0), "and it still says who priced it");
    }

    #[test]
    #[should_panic(expected = "only the oracle owner")]
    fn a_stranger_cannot_seat_a_source() {
        let (mut contract, _) = committee(1);
        testing_env!(at(accounts(5), NOW).build());
        contract.add_source("stranger".to_string(), vec![7u8; 32]);
    }

    #[test]
    fn fixes_come_back_oldest_first_inside_the_window() {
        let (mut contract, seats) = committee(2);
        for step in 0..3u64 {
            let time = NOW + step * 1_000_000_000;
            testing_env!(at(accounts(1), time + 1).build());
            contract.submit_observations(pair(&seats, ATM + step as u128, time));
        }
        let all = contract.get_observations(BTC.to_string(), NOW, u64::MAX, 100);
        assert_eq!(all.len(), 3);
        assert!(all.windows(2).all(|p| p[0].publish_time_ns <= p[1].publish_time_ns));
        let one = contract.get_observations(BTC.to_string(), NOW, NOW, 100);
        assert_eq!(one.len(), 1, "a settlement window can ask for exactly its own fix");
    }

    #[test]
    fn the_history_stays_bounded() {
        let (mut contract, seats) = committee(2);
        contract.set_max_history(3);
        for step in 0..10u64 {
            let time = NOW + step * 1_000_000_000;
            testing_env!(at(accounts(1), time + 1).build());
            contract.submit_observations(pair(&seats, ATM + step as u128, time));
        }
        assert_eq!(contract.get_observations(BTC.to_string(), 0, u64::MAX, 100).len(), 3);
    }

    #[test]
    fn a_zero_price_and_a_future_stamp_are_both_refused() {
        let (mut contract, seats) = committee(2);
        let result = contract.submit_observations(vec![
            signed(&seats[0].1, &seats[0].0, 0, NOW - 1),
            signed(&seats[0].1, &seats[0].0, ATM, NOW + 60_000_000_000),
        ]);
        assert_eq!(result.accepted, 0);
        assert_eq!(result.rejected.len(), 2);
        assert!(result.rejected[0].contains("not a price"));
        assert!(result.rejected[1].contains("future"));
    }

    #[test]
    fn the_committee_view_lists_every_seat_once() {
        let (contract, _) = committee(3);
        let view = contract.get_committee(BTC.to_string());
        assert_eq!(view.active_sources, 3);
        assert_eq!(view.seats.len(), 3);
        assert!(view.seats.iter().all(|seat| seat.price.is_none()));
        assert_eq!(view.quorum, 2);
        assert_eq!(view.tolerance_bps, 100);
        let names: Vec<&str> = view.seats.iter().map(|s| s.source.as_str()).collect();
        assert_eq!(names, vec!["src1", "src2", "src3"]);
        assert_eq!(view.last_fix, None);
    }

    #[test]
    fn a_tighter_quorum_demands_more_seats() {
        let (mut contract, seats) = committee(4);
        assert_eq!(contract.submit_observations(pair(&seats, ATM, NOW - 1)).fixed.len(), 1);
        contract.set_quorum(3);
        testing_env!(at(accounts(1), NOW + 1).build());
        let result = contract.submit_observations(vec![
            signed(&seats[0].1, &seats[0].0, ATM + 1, NOW + 1),
            signed(&seats[1].1, &seats[1].0, ATM + 1, NOW + 1),
        ]);
        assert!(result.fixed.is_empty(), "three of four are needed now, and two showed up");
    }

    #[test]
    #[should_panic(expected = "one source is not a quorum")]
    fn a_quorum_of_one_is_refused() {
        let (mut contract, _) = committee(2);
        contract.set_quorum(1);
    }
}
