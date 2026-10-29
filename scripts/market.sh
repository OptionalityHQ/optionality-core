#!/usr/bin/env bash
# Run the market simulation service and rebuild the screen that talks to it.
#   ./scripts/market.sh            # daemon on :8787 (Ctrl-C to stop)
#   PORT=9000 ./scripts/market.sh
#   ./scripts/market.sh --check    # verify it answers, then exit
set -euo pipefail
cd "$(dirname "$0")/.."

node sim/build-demo.mjs

if [[ "${1:-}" == "--check" ]]; then
  ( node sim/market-daemon.mjs --quiet --port "${PORT:-8787}" & echo $! > /tmp/market-daemon.pid )
  for _ in $(seq 1 40); do
    if curl -sf "http://127.0.0.1:${PORT:-8787}/health" > /dev/null; then
      curl -s "http://127.0.0.1:${PORT:-8787}/health"; echo
      kill "$(cat /tmp/market-daemon.pid)" 2>/dev/null || true
      exit 0
    fi
    sleep 0.25
  done
  echo "daemon did not answer on ${PORT:-8787}" >&2
  kill "$(cat /tmp/market-daemon.pid)" 2>/dev/null || true
  exit 1
fi

exec node sim/market-daemon.mjs --port "${PORT:-8787}"
