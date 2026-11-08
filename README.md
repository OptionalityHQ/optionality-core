# Optionality Core (`optionality-core`)

Smart contracts, Rust pricing & volatility surface engine, confidential TEE copy-trading ledger, and on-chain oracle/market-maker daemons for **Optionality** — a bounded options protocol on NEAR.

## Repository Structure

- **`contracts/bounded-options`** — On-chain NEAR Bounded Options Pool & Order Book smart contract (collateralized call/put spreads, pool holds, resting bids/asks, settlement).
- **`contracts/price-oracle`** — Multi-source committee price oracle smart contract (Pyth, Kraken, OKX, Venue Index quorum).
- **`contracts/mock-ft`** — NEP-141 USDT fungible token contract for collateral and settlement.
- **`core/`** — Spread option pricing, Black-Scholes & margin math library.
- **`engine/`** — Rust HTTP pricing & volatility surface engine (`:8787`).
- **`surface/`** — Implied volatility smile and surface calibration crate.
- **`private-ledger/` & `confidential-protocol/`** — Confidential TEE copy-trading ledger and shielded execution protocol crates.
- **`tools/chain-host`** — Continuous market-maker quoting, series rolling, and settlement daemon.
- **`tools/oracle-signer` & `tools/oracle-sources`** — Independent committee oracle signers and exchange feed readers.
- **`tools/confidential-worker`** — Confidential enclave worker.
- **`integration-tests/`** — Rust contract and lifecycle integration test suite.
- **`scripts/` & `deploy/`** — Build, lifecycle, testnet deployment, and production service manifests.

## Build & Test

```bash
cargo test --workspace --exclude bounded-options-integration-tests
bash scripts/build.sh
```
