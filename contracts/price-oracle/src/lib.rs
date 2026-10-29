//! Testnet oracle adapter with the same observation interface the options
//! contract consumes. A Pyth-backed adapter can replace this contract without
//! changing settlement logic; the reporter key here is explicitly trusted.

use near_sdk::borsh::BorshSerialize;
use near_sdk::store::LookupMap;
use near_sdk::{env, near, require, AccountId, BorshStorageKey, PanicOnDefault};

#[derive(BorshStorageKey, BorshSerialize)]
#[borsh(crate = "near_sdk::borsh")]
enum StorageKey {
    Feeds,
}

#[near(serializers = [json, borsh])]
#[derive(Clone)]
pub struct Observation {
    pub price: u128,
    pub publish_time_ns: u64,
    pub confidence_bps: u16,
}

#[near(contract_state)]
#[derive(PanicOnDefault)]
pub struct Contract {
    owner: AccountId,
    reporter: AccountId,
    feeds: LookupMap<String, Vec<Observation>>,
    max_history: u32,
}

#[near]
impl Contract {
    #[init]
    pub fn new(owner: AccountId, reporter: AccountId) -> Self {
        Self {
            owner,
            reporter,
            feeds: LookupMap::new(StorageKey::Feeds),
            max_history: 1_000,
        }
    }

    pub fn report(
        &mut self,
        feed_id: String,
        price: u128,
        publish_time_ns: u64,
        confidence_bps: u16,
    ) {
        require!(
            env::predecessor_account_id() == self.reporter,
            "only reporter"
        );
        require!(price > 0, "price must be positive");
        require!(confidence_bps <= 10_000, "invalid confidence");

        let mut history = self.feeds.get(&feed_id).cloned().unwrap_or_default();
        if let Some(last) = history.last() {
            require!(
                publish_time_ns > last.publish_time_ns,
                "observation must advance time"
            );
        }
        history.push(Observation {
            price,
            publish_time_ns,
            confidence_bps,
        });
        if history.len() > self.max_history as usize {
            history.remove(0);
        }
        self.feeds.insert(feed_id, history);
    }

    pub fn get_observations(
        &self,
        feed_id: String,
        from_ns: u64,
        to_ns: u64,
        limit: u32,
    ) -> Vec<Observation> {
        let Some(history) = self.feeds.get(&feed_id) else {
            return Vec::new();
        };
        history
            .iter()
            .filter(|obs| obs.publish_time_ns >= from_ns && obs.publish_time_ns <= to_ns)
            .take(limit.min(100) as usize)
            .cloned()
            .collect()
    }

    pub fn get_latest(&self, feed_id: String) -> Option<Observation> {
        self.feeds
            .get(&feed_id)
            .and_then(|history| history.last().cloned())
    }

    pub fn set_reporter(&mut self, reporter: AccountId) {
        require!(
            env::predecessor_account_id() == self.owner,
            "only owner"
        );
        self.reporter = reporter;
    }

    pub fn set_max_history(&mut self, max_history: u32) {
        require!(
            env::predecessor_account_id() == self.owner,
            "only owner"
        );
        require!(max_history > 0 && max_history <= 10_000, "invalid history");
        self.max_history = max_history;
    }

    pub fn get_reporter(&self) -> AccountId {
        self.reporter.clone()
    }
}
