//! Minimal NEP-141 token for testnet demos.
//!
//! It implements registration, transfers, `ft_transfer_call` and the refund
//! resolver. It is test collateral, not a production stablecoin: storage is
//! underfunded and minting is owner-controlled.

use near_sdk::borsh::BorshSerialize;
use near_sdk::json_types::U128;
use near_sdk::store::LookupMap;
use near_sdk::{
    env, ext_contract, near, require, AccountId, BorshStorageKey, Gas, NearToken, PanicOnDefault,
    Promise,
};

const GAS_FOR_FT_ON_TRANSFER: Gas = Gas::from_tgas(30);
const GAS_FOR_RESOLVE: Gas = Gas::from_tgas(25);

#[derive(BorshStorageKey, BorshSerialize)]
#[borsh(crate = "near_sdk::borsh")]
enum StorageKey {
    Accounts,
}

#[near(serializers = [json, borsh])]
#[derive(Clone)]
pub struct Account {
    pub balance: u128,
}

#[near(serializers = [json])]
pub struct FungibleTokenMetadata {
    pub spec: String,
    pub name: String,
    pub symbol: String,
    pub icon: Option<String>,
    pub reference: Option<String>,
    pub reference_hash: Option<String>,
    pub decimals: u8,
}

#[ext_contract(ext_ft_receiver)]
pub trait FungibleTokenReceiver {
    fn ft_on_transfer(&mut self, sender_id: AccountId, amount: U128, msg: String) -> String;
}

#[near(contract_state)]
#[derive(PanicOnDefault)]
pub struct Contract {
    owner: AccountId,
    accounts: LookupMap<AccountId, Account>,
    total_supply: u128,
}

#[near]
impl Contract {
    #[init]
    pub fn new(owner: AccountId) -> Self {
        Self {
            owner,
            accounts: LookupMap::new(StorageKey::Accounts),
            total_supply: 0,
        }
    }

    #[payable]
    pub fn storage_deposit(&mut self, account_id: Option<AccountId>) -> AccountId {
        require!(
            env::attached_deposit() >= NearToken::from_yoctonear(1),
            "attach NEAR for storage"
        );
        let target = account_id.unwrap_or_else(env::predecessor_account_id);
        if !self.accounts.contains_key(&target) {
            self.accounts.insert(target.clone(), Account { balance: 0 });
        }
        target
    }

    pub fn storage_balance_of(&self, account_id: AccountId) -> Option<U128> {
        self.accounts
            .get(&account_id)
            .map(|account| U128(account.balance))
    }

    pub fn mint(&mut self, account_id: AccountId, amount: U128) {
        require!(
            env::predecessor_account_id() == self.owner,
            "only owner can mint"
        );
        require!(amount.0 > 0, "amount must be positive");
        require!(
            self.accounts.contains_key(&account_id),
            "receiver not registered"
        );
        self.total_supply = self
            .total_supply
            .checked_add(amount.0)
            .expect("supply overflow");
        self.credit(&account_id, amount.0);
    }

    pub fn ft_metadata(&self) -> FungibleTokenMetadata {
        FungibleTokenMetadata {
            spec: "ft-1.0.0".to_string(),
            name: "Bounded Options Test Dollar".to_string(),
            symbol: "bUSD".to_string(),
            icon: None,
            reference: None,
            reference_hash: None,
            decimals: 6,
        }
    }

    pub fn ft_total_supply(&self) -> U128 {
        U128(self.total_supply)
    }

    pub fn ft_balance_of(&self, account_id: AccountId) -> U128 {
        U128(self.accounts.get(&account_id).map(|a| a.balance).unwrap_or(0))
    }

    #[payable]
    pub fn ft_transfer(
        &mut self,
        receiver_id: AccountId,
        amount: U128,
        _memo: Option<String>,
    ) {
        require_one_yocto();
        require!(amount.0 > 0, "amount must be positive");
        self.internal_transfer(&env::predecessor_account_id(), &receiver_id, amount.0);
    }

    #[payable]
    pub fn ft_transfer_call(
        &mut self,
        receiver_id: AccountId,
        amount: U128,
        _memo: Option<String>,
        msg: String,
    ) -> Promise {
        require_one_yocto();
        require!(amount.0 > 0, "amount must be positive");
        let sender_id = env::predecessor_account_id();
        self.internal_transfer(&sender_id, &receiver_id, amount.0);

        ext_ft_receiver::ext(receiver_id.clone())
            .with_static_gas(GAS_FOR_FT_ON_TRANSFER)
            .ft_on_transfer(sender_id.clone(), amount, msg)
            .then(
                Self::ext(env::current_account_id())
                    .with_static_gas(GAS_FOR_RESOLVE)
                    .resolve_transfer(sender_id, receiver_id, amount),
            )
    }

    /// Refunds whatever the receiver reports as unused. A panic or malformed
    /// return refunds the full amount, bounded by the receiver's balance.
    #[private]
    pub fn resolve_transfer(
        &mut self,
        sender_id: AccountId,
        receiver_id: AccountId,
        amount: U128,
    ) -> U128 {
        let unused = env::promise_result_checked(0, 1024)
            .ok()
            .and_then(|bytes| near_sdk::serde_json::from_slice::<String>(&bytes).ok())
            .and_then(|value| value.parse::<u128>().ok())
            .unwrap_or(amount.0)
            .min(amount.0);

        let receiver_balance = self
            .accounts
            .get(&receiver_id)
            .map(|account| account.balance)
            .unwrap_or(0);
        let refund = unused.min(receiver_balance);
        if refund > 0 {
            self.debit(&receiver_id, refund);
            self.credit(&sender_id, refund);
        }
        U128(refund)
    }

    fn internal_transfer(&mut self, sender_id: &AccountId, receiver_id: &AccountId, amount: u128) {
        require!(sender_id != receiver_id, "cannot transfer to self");
        require!(
            self.accounts.contains_key(receiver_id),
            "receiver not registered"
        );
        self.debit(sender_id, amount);
        self.credit(receiver_id, amount);
    }

    fn credit(&mut self, account_id: &AccountId, amount: u128) {
        let balance = self.accounts.get(account_id).map(|a| a.balance).unwrap_or(0);
        self.accounts.insert(
            account_id.clone(),
            Account {
                balance: balance.checked_add(amount).expect("balance overflow"),
            },
        );
    }

    fn debit(&mut self, account_id: &AccountId, amount: u128) {
        let balance = self.accounts.get(account_id).map(|a| a.balance).unwrap_or(0);
        require!(balance >= amount, "insufficient token balance");
        self.accounts.insert(
            account_id.clone(),
            Account {
                balance: balance - amount,
            },
        );
    }
}

fn require_one_yocto() {
    require!(
        env::attached_deposit() == NearToken::from_yoctonear(1),
        "requires exactly 1 yoctoNEAR"
    );
}
