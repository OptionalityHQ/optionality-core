#!/usr/bin/env bash
# Start the market service (which also serves the screen). Add --chain to also
# boot the local NEAR sandbox the contract runs on; the screen does not need it.
#
#   bash scripts/demo.sh            then open http://127.0.0.1:8787
#   bash scripts/demo.sh --chain
#   bash scripts/demo.sh --stop
set -euo pipefail
cd "$(dirname "$0")/.."
mkdir -p logs

if [[ "${1:-}" == "--stop" ]]; then
  pkill -f market-daemon.mjs || true
  pkill -f "target/debug/chain-host" || true
  echo "stopped"
  exit 0
fi

[[ -f demo/index.html ]] || node sim/build-demo.mjs

pkill -f market-daemon.mjs 2>/dev/null || true
pkill -f "target/debug/chain-host" 2>/dev/null || true
sleep 1

nohup node sim/market-daemon.mjs --quiet > logs/market.log 2>&1 &

for _ in $(seq 1 20); do
  curl -s -m 2 http://127.0.0.1:8787/health >/dev/null 2>&1 && break
  sleep 1
done
printf 'market  '; curl -s -m 3 http://127.0.0.1:8787/health; echo
printf 'screen  http://127.0.0.1:8787\n'

if [[ "${1:-}" == "--chain" ]]; then
  [[ -x target/debug/chain-host ]] || cargo build -p chain-host
  nohup ./target/debug/chain-host --out data/chain-session.json --spot 2350 > logs/chain-host.log 2>&1 &
  for _ in $(seq 1 40); do
    grep -q "session written" logs/chain-host.log 2>/dev/null && break
    sleep 1
  done
  printf 'chain   '; grep -m1 '^rpc' logs/chain-host.log || echo "booting — see logs/chain-host.log"
fi
