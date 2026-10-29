#!/usr/bin/env bash
# Verify the simulator and live feed against the contract's frozen vectors,
# rebuild the demo, then print the market economics for both assets.
#
#   ./scripts/simulate.sh              # offline checks + report on the pinned snapshot
#   REFRESH=1 ./scripts/simulate.sh    # refetch BTC/ETH spot, candles and vol first
#   RUNS=1500 EDGE=150 ./scripts/simulate.sh
set -euo pipefail
cd "$(dirname "$0")/.."

if [[ "${REFRESH:-0}" == "1" ]]; then
  node sim/data.mjs --write
fi

node --test sim/test/parity.test.mjs sim/test/feed.test.mjs
node sim/build-demo.mjs

RUNS="${RUNS:-1500}"
EDGE="${EDGE:-150}"
for asset in ETH BTC; do
  node sim/simulate.mjs --asset "$asset" --runs "$RUNS" --edge-bps "$EDGE"
done
