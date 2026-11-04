#!/usr/bin/env bash
# Run the existing testnet deployment. PUBLIC_ORIGIN is the HTTPS origin you serve.
set -euo pipefail
cd "$(dirname "$0")/.."
SESSION=${SESSION:-data/testnet-session.json}
CREDENTIALS=${NEAR_ADMIN_CREDENTIALS:-$HOME/.near-credentials/testnet/sable-knee.testnet.json}
ENGINE_PORT=${ENGINE_PORT:-8791}
GATEWAY_PORT=${GATEWAY_PORT:-8790}
mkdir -p data/private logs
chmod 700 data/private
for port in "$ENGINE_PORT" "$GATEWAY_PORT"; do
  if lsof -nP -iTCP:"$port" -sTCP:LISTEN >/dev/null 2>&1; then
    echo "Port $port is already in use; stop the existing process or select another port." >&2
    exit 1
  fi
done
[ -f "$CREDENTIALS" ] || { echo "Missing venue credentials: $CREDENTIALS" >&2; exit 1; }
node sim/build-demo.mjs
children=()
cleanup() { for pid in "${children[@]}"; do kill "$pid" 2>/dev/null || true; done; }
trap cleanup EXIT INT TERM
./target/debug/engine --port "$ENGINE_PORT" --assets ETH,BTC --expiry-hours "${EXPIRY_HOURS:-1}" > logs/testnet-engine.log 2>&1 &
children+=("$!")
./target/debug/chain-host --testnet-session "$SESSION" --credentials "$CREDENTIALS" \
  --engine "http://127.0.0.1:$ENGINE_PORT" --assets ETH,BTC --max-expiries "${MAX_EXPIRIES:-2}" > logs/testnet-operator.log 2>&1 &
children+=("$!")
NEAR_ADMIN_CREDENTIALS="$CREDENTIALS" node api/src/server.ts --session "$SESSION" \
  --engine "http://127.0.0.1:$ENGINE_PORT" --port "$GATEWAY_PORT" --host "${GATEWAY_HOST:-127.0.0.1}" --public-origin "${PUBLIC_ORIGIN:-http://127.0.0.1:$GATEWAY_PORT}" > logs/testnet-gateway.log 2>&1 &
children+=("$!")
printf '%s\n' "${children[@]}" > data/private/testnet.pids
echo "Testnet services running. Gateway: http://127.0.0.1:$GATEWAY_PORT. Logs: logs/testnet-*.log"
# Supervise only our children. A dead operator must not leave a seemingly live gateway.
while true; do
  for pid in "${children[@]}"; do
    kill -0 "$pid" 2>/dev/null || { echo "Testnet service exited; inspect logs/testnet-*.log" >&2; exit 1; }
  done
  sleep 5
done
