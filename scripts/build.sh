#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

for contract in bounded-options mock-ft price-oracle; do
  (cd "contracts/$contract" && cargo near build non-reproducible-wasm)
done

shasum -a 256 \
  target/near/bounded_options/bounded_options.wasm \
  target/near/mock_ft/mock_ft.wasm \
  target/near/price_oracle/price_oracle.wasm
