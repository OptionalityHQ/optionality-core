//! Minimal NEP-141 token for testnet demos.
//!
//! It implements registration, transfers, `ft_transfer_call` and the refund
//! resolver. It is test collateral, not a production stablecoin: minting is
//! owner-controlled and there is no allowance mechanism.
//!
//! Storage is funded, though. A NEAR contract pays rent per byte of state out of
//! its own balance, so a token that registers accounts for a yoctoNEAR is a token
//! that drains its account and then refuses every transfer — on a public network,
//! usually on the day of the demo.

use near_sdk::borsh::BorshSerialize;
use near_sdk::json_types::U128;
use near_sdk::store::LookupMap;
use near_sdk::{
    env, ext_contract, near, require, AccountId, BorshStorageKey, Gas, NearToken, PanicOnDefault,
    Promise,
};

/// One row of `accounts`: a 64-ish byte `AccountId` key and a 32-byte balance,
/// plus the `LookupMap` overhead. Measured from the store, then rounded up.
const ACCOUNT_STORAGE_BYTES: u64 = 128;

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
    /// Rent this contract has been paid for the rows it stores. Kept so an
    /// operator can see the tab, the same way the options contract does.
    storage_paid: NearToken,
}

#[near]
impl Contract {
    #[init]
    pub fn new(owner: AccountId) -> Self {
        Self {
            owner,
            accounts: LookupMap::new(StorageKey::Accounts),
            total_supply: 0,
            storage_paid: NearToken::from_yoctonear(0),
        }
    }

    /// Register an account, paying for the row it occupies.
    ///
    /// The old version accepted a single yoctoNEAR, which is not a payment, it is
    /// a formality: the contract paid the rent for every account it registered
    /// and had no way to recover it. NEP-141 defines this call, so the amount is
    /// the protocol's, not ours: the caller covers `ACCOUNT_STORAGE_BYTES`, and
    /// overpayment is refunded rather than kept.
    #[payable]
    pub fn storage_deposit(&mut self, account_id: Option<AccountId>) -> AccountId {
        let target = account_id.unwrap_or_else(env::predecessor_account_id);
        let known = self.accounts.contains_key(&target);
        // Registering twice costs nothing the second time: the row already exists.
        if !known {
            let price = env::storage_byte_cost();
            let owed = price.saturating_mul(ACCOUNT_STORAGE_BYTES as u128);
            let attached = env::attached_deposit();
            require!(
                attached >= owed,
                format!(
                    "registering an account stores {ACCOUNT_STORAGE_BYTES} bytes; attach at least {} yoctoNEAR",
                    owed.as_yoctonear()
                )
            );
            self.accounts.insert(target.clone(), Account { balance: 0 });
            self.storage_paid = self.storage_paid.saturating_add(owed);
            if attached > owed {
                Promise::new(env::predecessor_account_id())
                    .transfer(attached.saturating_sub(owed))
                    .detach();
            }
            return target;
        }
        // Registration is idempotent. Return any attached deposit to the payer.
        let attached = env::attached_deposit();
        if attached.as_yoctonear() > 0 {
            Promise::new(env::predecessor_account_id())
                .transfer(attached)
                .detach();
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
        U128(
            self.accounts
                .get(&account_id)
                .map(|a| a.balance)
                .unwrap_or(0),
        )
    }

    #[payable]
    pub fn ft_transfer(&mut self, receiver_id: AccountId, amount: U128, _memo: Option<String>) {
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
        let balance = self
            .accounts
            .get(account_id)
            .map(|a| a.balance)
            .unwrap_or(0);
        self.accounts.insert(
            account_id.clone(),
            Account {
                balance: balance.checked_add(amount).expect("balance overflow"),
            },
        );
    }

    fn debit(&mut self, account_id: &AccountId, amount: u128) {
        let balance = self
            .accounts
            .get(account_id)
            .map(|a| a.balance)
            .unwrap_or(0);
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

#[cfg(test)]
mod tests {
    use super::*;
    use near_sdk::test_utils::{accounts, VMContextBuilder};
    use near_sdk::testing_env;

    fn setup() -> Contract {
        let mut ctx = VMContextBuilder::new();
        ctx.current_account_id(accounts(0))
            .predecessor_account_id(accounts(0))
            .attached_deposit(NearToken::from_yoctonear(0));
        testing_env!(ctx.build());
        Contract::new(accounts(0))
    }

    fn price() -> NearToken {
        env::storage_byte_cost().saturating_mul(ACCOUNT_STORAGE_BYTES as u128)
    }

    /// Registration is the one write this token makes on a stranger's behalf, and
    /// it is permanent. A yoctoNEAR is not payment for it.
    #[test]
    fn registering_an_account_costs_its_own_rent() {
        let mut contract = setup();
        let mut ctx = VMContextBuilder::new();
        ctx.current_account_id(accounts(0))
            .predecessor_account_id(accounts(2))
            .attached_deposit(NearToken::from_yoctonear(1));
        testing_env!(ctx.build());
        let refused = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            contract.storage_deposit(None)
        }));
        assert!(
            refused.is_err(),
            "one yoctoNEAR must not register an account"
        );

        // Exactly the rent is enough.
        let mut ctx = VMContextBuilder::new();
        ctx.current_account_id(accounts(0))
            .predecessor_account_id(accounts(2))
            .attached_deposit(price());
        testing_env!(ctx.build());
        assert_eq!(contract.storage_deposit(None), accounts(2));
        assert_eq!(contract.storage_balance_of(accounts(2)), Some(U128(0)));
    }

    /// Registering twice adds no row, so it must not be a second charge — and it
    /// must not quietly pocket a deposit either.
    #[test]
    fn a_second_registration_refunds_the_deposit() {
        let mut contract = setup();
        let mut ctx = VMContextBuilder::new();
        ctx.current_account_id(accounts(0))
            .predecessor_account_id(accounts(2))
            .attached_deposit(price());
        testing_env!(ctx.build());
        contract.storage_deposit(None);

        // Free when nothing is attached.
        let mut ctx = VMContextBuilder::new();
        ctx.current_account_id(accounts(0))
            .predecessor_account_id(accounts(2))
            .attached_deposit(NearToken::from_yoctonear(0));
        testing_env!(ctx.build());
        assert_eq!(contract.storage_deposit(None), accounts(2));

        // A repeated registration succeeds and refunds the entire deposit.
        let mut ctx = VMContextBuilder::new();
        ctx.current_account_id(accounts(0))
            .predecessor_account_id(accounts(2))
            .attached_deposit(price());
        testing_env!(ctx.build());
        assert_eq!(contract.storage_deposit(None), accounts(2));
        let receipts = near_sdk::test_utils::get_created_receipts();
        assert_eq!(receipts.len(), 1);
        assert_eq!(receipts[0].receiver_id, accounts(2));
        assert!(format!("{:?}", receipts[0].actions).contains(&price().as_yoctonear().to_string()));
    }

    /// The mint is owner-only, which is what stops a visitor from writing
    /// themselves test dollars.
    #[test]
    fn only_the_owner_can_mint() {
        let mut contract = setup();
        let mut ctx = VMContextBuilder::new();
        ctx.current_account_id(accounts(0))
            .predecessor_account_id(accounts(2))
            .attached_deposit(price());
        testing_env!(ctx.build());
        contract.storage_deposit(None);

        testing_env!(VMContextBuilder::new()
            .current_account_id(accounts(0))
            .predecessor_account_id(accounts(2))
            .attached_deposit(NearToken::from_yoctonear(0))
            .build());
        let refused = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            contract.mint(accounts(2), U128(1_000))
        }));
        assert!(refused.is_err(), "a stranger must not mint");

        // The owner can, and the supply follows.
        testing_env!(VMContextBuilder::new()
            .current_account_id(accounts(0))
            .predecessor_account_id(accounts(0))
            .attached_deposit(NearToken::from_yoctonear(0))
            .build());
        contract.mint(accounts(2), U128(1_000));
        assert_eq!(contract.ft_balance_of(accounts(2)), U128(1_000));
        assert_eq!(contract.ft_total_supply(), U128(1_000));
    }

    /// A transfer needs the receiver registered, because the transfer writes a row
    /// for them and someone has to have paid for it.
    #[test]
    fn a_transfer_to_an_unregistered_account_is_refused() {
        let mut contract = setup();
        for who in [accounts(2)] {
            let mut ctx = VMContextBuilder::new();
            ctx.current_account_id(accounts(0))
                .predecessor_account_id(who)
                .attached_deposit(price());
            testing_env!(ctx.build());
            contract.storage_deposit(None);
        }
        testing_env!(VMContextBuilder::new()
            .current_account_id(accounts(0))
            .predecessor_account_id(accounts(0))
            .attached_deposit(NearToken::from_yoctonear(0))
            .build());
        contract.mint(accounts(2), U128(1_000));

        // accounts(3) never registered.
        testing_env!(VMContextBuilder::new()
            .current_account_id(accounts(0))
            .predecessor_account_id(accounts(2))
            .attached_deposit(NearToken::from_yoctonear(1))
            .build());
        let refused = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            contract.ft_transfer(accounts(3), U128(10), None)
        }));
        assert!(refused.is_err());
        // And the sender kept every unit.
        assert_eq!(contract.ft_balance_of(accounts(2)), U128(1_000));
    }
}
