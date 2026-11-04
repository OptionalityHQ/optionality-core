#!/usr/bin/env bash
# Deploy the venue to NEAR testnet, in the order the contracts depend on each
# other, and leave behind a session file describing where everything landed.
#
#   scripts/deploy-testnet.sh                     # plan only: prints what it would do
#   scripts/deploy-testnet.sh --yes               # actually create and deploy
#
# This is the public-chain counterpart to scripts/venue.sh. That one boots a
# sandbox that dies with the process; this one writes to an account that
# survives, so it is gated behind --yes. It uses the `near` CLI rather than
# `cargo near deploy`, because the latter wants an interactive signer choice and
# a deployment script is supposed to run unattended.
#
# Prerequisites, none of which this script does for you:
#   - `near` CLI            https://near.cli.rs
#   - an account on testnet  `cargo near create-dev-account`, or near login
#   - that account's key in the near credentials store
set -euo pipefail
cd "$(dirname "$0")/.."

NETWORK=${NETWORK:-testnet}
RPC_URL=${RPC_URL:-https://rpc.testnet.near.org}
VENUE=${VENUE:-}
CONFIRM=${1:-}

die() { echo "error: $*" >&2; exit 1; }
say() { printf '%s\n' "$*"; }

[ -n "$VENUE" ] || die "set VENUE to the account that will own the venue, e.g. VENUE=optionality.testnet"

TOKEN_ID="token.$VENUE"
ORACLE_ID="oracle.$VENUE"
OPTIONS_ID="options.$VENUE"

SCALE=1000000
GAS=100000000000000   # 100 Tgas: enough for an init call with a map write
WASM_DIR=target/near

say "network   $NETWORK ($RPC_URL)"
say "venue     $VENUE"
say "contracts $TOKEN_ID  $ORACLE_ID  $OPTIONS_ID"
say ""

if [ "$CONFIRM" != "--yes" ]; then
  say "plan only. Re-run with --yes to create the subaccounts and deploy."
  say ""
  say "what --yes does, in this order:"
  say "  1. build the three wasm artifacts (scripts/build.sh)"
  say "  2. create $TOKEN_ID, $ORACLE_ID, $OPTIONS_ID as subaccounts of $VENUE"
  say "     (each with 10 NEAR: they pay storage rent for the state they hold)"
  say "  3. deploy mock-ft         -> $TOKEN_ID       new(owner=$VENUE)"
  say "  4. deploy price-oracle    -> $ORACLE_ID      new(owner=$VENUE)"
  say "  5. deploy bounded-options -> $OPTIONS_ID     new(admin, token, oracle)"
  say "  6. set_venue($VENUE) and set_fee_terms(50,100)"
  say "  7. add_source(...) on the ORACLE for each seat in data/testnet-seats, then set_quorum(2)"
  say "  8. write data/testnet-session.json (no secrets in it)"
  say ""
  say "it does NOT wire a visitor wallet. The gateway still signs with whatever"
  say "key it is given; the wallet path is api/src/wallet.ts and is not connected."
  say "See docs/10-testnet.md."
  exit 0
fi

command -v near >/dev/null 2>&1 || die "'near' CLI not found (https://near.cli.rs)"
command -v cargo >/dev/null 2>&1 || die "cargo not found"

# Everything below is a real transaction. Say so, once, loudly.
say "creating and deploying to $NETWORK. This spends NEAR."
say ""

say "==> 1/8 building"
bash scripts/build.sh

say "==> 2/8 creating subaccounts"
for account in "$TOKEN_ID" "$ORACLE_ID" "$OPTIONS_ID"; do
  if near state "$account" --networkId "$NETWORK" >/dev/null 2>&1; then
    say "    $account exists, leaving it alone"
  else
    near create-account "$account" --masterAccount "$VENUE" \
      --initialBalance 10 --networkId "$NETWORK"
  fi
done

say "==> 3/8 deploying the mock token"
near deploy "$TOKEN_ID" "$WASM_DIR/mock_ft/mock_ft.wasm" \
  --initFunction new \
  --initArgs "{\"owner\":\"$VENUE\"}" \
  --initGas "$GAS" \
  --accountId "$VENUE" --networkId "$NETWORK"

say "==> 4/8 deploying the oracle"
near deploy "$ORACLE_ID" "$WASM_DIR/price_oracle/price_oracle.wasm" \
  --initFunction new \
  --initArgs "{\"owner\":\"$VENUE\"}" \
  --initGas "$GAS" \
  --accountId "$VENUE" --networkId "$NETWORK"

say "==> 5/8 deploying the options contract"
near deploy "$OPTIONS_ID" "$WASM_DIR/bounded_options/bounded_options.wasm" \
  --initFunction new \
  --initArgs "{\"admin\":\"$VENUE\",\"token\":\"$TOKEN_ID\",\"oracle\":\"$ORACLE_ID\"}" \
  --initGas "$GAS" \
  --accountId "$VENUE" --networkId "$NETWORK"

say "==> 6/8 naming the venue and its fee schedule"
# The operator quotes as the venue account; the contract refuses anyone else.
near call "$OPTIONS_ID" set_venue "{\"venue\":\"$VENUE\"}" \
  --accountId "$VENUE" --gas "$GAS" --networkId "$NETWORK"
# 0.5% over mid to open, 1% under mid to close — the sandbox's schedule, and
# honest about being a demo fee rather than a market one.
near call "$OPTIONS_ID" set_fee_terms '{"margin_bps":50,"impact_bps":100}' \
  --accountId "$VENUE" --gas "$GAS" --networkId "$NETWORK"

say "==> 7/8 the settlement committee"
# Seats live on the ORACLE contract, not the options contract, and a seat is a
# name plus a public key that the oracle will accept signed prices from.
#
# This is the step the sandbox does in-process and cannot be done the same way
# here: `oracle-signer` generates its key from a random seed at startup
# (tools/oracle-signer/src/main.rs:39) and never writes it down. In the sandbox
# that is fine, because the seats and the chain die together. On testnet a seat
# whose key changes on restart becomes a registered source that can never sign
# again, and the venue can never settle a ticket.
#
# So the script refuses to guess. It registers seats only from a directory of
# key material you supply, and says so when there is none.
SEATS_DIR=${SEATS_DIR:-data/testnet-seats}
if [ -d "$SEATS_DIR" ] && ls "$SEATS_DIR"/*.json >/dev/null 2>&1; then
  for seat in "$SEATS_DIR"/*.json; do
    say "    registering $(basename "$seat" .json)"
    near call "$ORACLE_ID" add_source "$(cat "$seat")" \
      --accountId "$VENUE" --gas "$GAS" --networkId "$NETWORK"
  done
  near call "$ORACLE_ID" set_quorum '{"quorum":2}' \
    --accountId "$VENUE" --gas "$GAS" --networkId "$NETWORK"
else
  say "    no seat keys in $SEATS_DIR — SKIPPING."
  say ""
  say "    Without seats the venue lists markets that can never settle: the"
  say "    contract refuses to fix a price without two independent sources, and"
  say "    refuses to settle without a fix. Deploying without this step gives you"
  say "    contracts you can call and a product nobody can trade."
  say ""
  say "    Each file must look like:"
  say "      {\"name\": \"kraken\", \"public_key\": [<32 bytes>]}"
  say ""
  say "    The durable-key work — persisting seat keys across restarts — is not"
  say "    done. See docs/10-testnet.md, 'What is still missing'."
fi

say "==> 8/8 writing the session file"
mkdir -p data
cat > data/testnet-session.json <<JSON
{
  "schema": "bounded-options/chain-session/1",
  "network": "$NETWORK",
  "rpc": "$RPC_URL",
  "decimals": 6,
  "scale": "$SCALE",
  "contracts": {
    "options": "$OPTIONS_ID",
    "token": "$TOKEN_ID",
    "oracle": "$ORACLE_ID"
  },
  "admin": { "account": "$VENUE" },
  "venue": "$VENUE",
  "note": "no secrets in this file"
}
JSON
say "    data/testnet-session.json"

say ""
say "done. read the storage tab first — a venue that cannot pay rent is a venue"
say "that stops accepting writes:"
say "  near view $OPTIONS_ID get_storage_state"
say ""
say "if 'funded' is false, anyone can top it up:"
say "  near call $OPTIONS_ID storage_deposit '{}' --accountId $VENUE --deposit 1"
