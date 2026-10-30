#!/usr/bin/env bash
# The whole venue, one command: the Rust engine holds the exchange sockets and
# quotes the ladder, the chain host boots a local NEAR sandbox and turns that
# ladder into series (then feeds the oracle so tickets can pay), and the
# TypeScript gateway is the only origin the screen talks to.
#
#   scripts/venue.sh          start everything and print the address
#   scripts/venue.sh stop     stop the three processes this script started
#
# Local sandbox only. Nothing here deploys to a public chain, creates keys of
# yours, or asks for a wallet: the operator role is the sandbox's own account.
set -euo pipefail
cd "$(dirname "$0")/.."

ENGINE_PORT=${ENGINE_PORT:-8791}
GATEWAY_PORT=${GATEWAY_PORT:-8790}
ASSETS=${ASSETS:-ETH,BTC}
EXPIRY_HOURS=${EXPIRY_HOURS:-1}
TICK_SECS=${TICK_SECS:-4}
LIST_SECS=${LIST_SECS:-30}
SESSION=${SESSION:-data/chain-session.json}
LOGS=logs

busy_on() { lsof -nP -tiTCP:"$1" -sTCP:LISTEN 2>/dev/null | head -3; }

stop_all() {
  for pattern in "target/debug/chain-host" "target/debug/engine" "api/src/server.ts"; do
    local pids
    pids=$(pgrep -f "$pattern" || true)
    if [ -n "$pids" ]; then
      echo "stopping $pattern: $pids"
      # shellcheck disable=SC2086
      kill $pids 2>/dev/null || true
    fi
  done
}

if [ "${1:-}" = "stop" ]; then
  stop_all
  exit 0
fi

# Only our own three ports are checked, and a clash is reported with the process
# holding it rather than killed: something else may be listening on purpose.
for port in "$ENGINE_PORT" "$GATEWAY_PORT"; do
  if [ -n "$(busy_on "$port")" ]; then
    echo "port $port is already held by: $(busy_on "$port")" >&2
    echo "stop it yourself, or pass another port: GATEWAY_PORT=8795 scripts/venue.sh" >&2
    exit 1
  fi
done

mkdir -p "$LOGS" data
./scripts/build.sh
cargo build -q -p engine -p chain-host
# The page the gateway serves is the template's output, so building the venue
# without rebuilding the page would serve yesterday's screen from today's chain.
node sim/build-demo.mjs

nohup ./target/debug/engine --port "$ENGINE_PORT" --assets "$ASSETS" \
  --expiry-hours "$EXPIRY_HOURS" > "$LOGS/engine.log" 2>&1 &
sleep 2

nohup ./target/debug/chain-host --out "$SESSION" \
  --engine "http://127.0.0.1:$ENGINE_PORT" --assets "$ASSETS" \
  --tick-secs "$TICK_SECS" --list-secs "$LIST_SECS" > "$LOGS/host.log" 2>&1 &

echo "booting the sandbox and listing the engine's ladder (watch $LOGS/host.log)…"
# The gate is the session file being *written*, not the ladder being listed: the
# host quotes the vault after listing, and a gateway that starts in between reads
# the previous run's session — a dead RPC and yesterday's series. Waiting out the
# budget is reported rather than papered over, because a gateway pointed at a stale
# session serves a screen that looks alive and reverts every trade it plans.
ready=no
# A week of ladders is five hundred series to commit before the session exists,
# so the budget is minutes rather than seconds.
for _ in $(seq 1 300); do
  grep -q "session written" "$LOGS/host.log" 2>/dev/null && { ready=yes; break; }
  sleep 2
done
if [ "$ready" != yes ]; then
  echo "the operator never wrote $SESSION · see $LOGS/host.log" >&2
  tail -6 "$LOGS/host.log" >&2
  exit 1
fi
tail -4 "$LOGS/host.log"

nohup node api/src/server.ts --session "$SESSION" \
  --engine "http://127.0.0.1:$ENGINE_PORT" --port "$GATEWAY_PORT" > "$LOGS/gateway.log" 2>&1 &
sleep 4

CONTRACT=$(curl -s -m 5 "http://127.0.0.1:$GATEWAY_PORT/health" || true)
echo
echo "  screen   http://127.0.0.1:$GATEWAY_PORT"
echo "  engine   http://127.0.0.1:$ENGINE_PORT/state?asset=ETH · /feed · /index · /plan"
echo "  gateway  $CONTRACT"
echo "  logs     $LOGS/engine.log · $LOGS/host.log · $LOGS/gateway.log"
echo "  stop     scripts/venue.sh stop"
