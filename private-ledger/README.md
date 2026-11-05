# Private ledger foundation

An isolated, non-deployed state machine for Private Copy Trading Subscription.
This crate does **not** provide a confidential service on its own. Never route
public trades through it and label them private.

Implemented and tested:

- Pseudonymous Ed25519 command identities; vault/network domain, deadline and
  nonce binding. Failed operations leave the entire state unchanged.
- Pool deposits, bounded put/call purchases, early exits, expiry/void settlement,
  integer fees and custody conservation using `bounded-core`.
- Atomic subscription payment/entitlement creation and subscriber-only journals.
  Only trades deliberately published to a profile enter its journal.
- Signed copy mandates with asset, expiry, contract-size, per-order and total
  premium limits. Revocation and subscription expiration stop new copying.
- A leader purchase and eligible followers are processed in the same state
  transition, preventing duplicate copies on replay. Active followers also copy
  early exits; expiry payouts remain payable after subscription expiration.
- Withdrawal notes remain liabilities until finalized, verified redemption.
- Snapshot recovery against an independently read finalized sequence/root.
  Reusing an older encrypted snapshot cannot bypass an anchor with a newer root.
- Signed, expiring, nonce-protected journal reads bound to a response encryption
  key. The read changes state; its checkpoint must commit before reply delivery.

Run `cargo test -p private-ledger`.

## Integration boundaries

The host must authenticate funding proofs, oracle prices/listings and finalized
checkpoints. Those facts are represented by trusted host APIs, not user commands.
A user must never supply `Context`, deposit credits or the accepted checkpoint.

`Ledger` deliberately has no public Serialize/Deserialize implementation. Its
private snapshot bytes are available only through an explicit sealing callback.
Use authenticated encryption inside the enclave, then restore only after checking
the current finalized anchor. Debug output redacts identities and balances.

The persistent state and returned journal records are private. The worker must
pad/encrypt commands and encrypt responses to the signed reply key. It must also
reject non-contributory X25519 reply keys. Never print raw state, commands,
records or blinding values to stdout, chain logs or gateway logs.

Snapshot hashing is a salted commitment, **not** a zero-knowledge proof. Every
production transition needs pinned code, verified remote attestation, a durable
pending-state record, consensus-backed checkpoint verification, CAS concurrency
control and recovery after uncertain broadcasts. Ciphertext size/access timing
and public funding metadata need their own protection.

## Still required before any privacy launch

A real attested worker adapter, confidential client protocol, on-chain vault and
checkpoint/custody integration, shielded funding and withdrawal reconciliation,
public performance disclosure controls, key recovery, and end-to-end tests with
multiple real users. Native public-chain token transfers are not shielded.
Do not accept mainnet funds into this prototype.
