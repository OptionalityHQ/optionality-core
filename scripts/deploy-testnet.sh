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
RPC_URL=${RPC_URL:-https://rpc.testnet.fastnear.com}
VENUE=${VENUE:-}
CONFIRM=${1:-}

die() { echo "error: $*" >&2; exit 1; }
say() { printf '%s\n' "$*"; }

[ -n "$VENUE" ] || die "set VENUE to the account that will own the venue, e.g. VENUE=optionality.testnet"

TOKEN_ID="token.$VENUE"
ORACLE_ID="oracle.$VENUE"
OPTIONS_ID="options.$VENUE"

SCALE=1000000
INITIAL_BALANCE=${INITIAL_BALANCE:-2}
OPTIONS_INITIAL_BALANCE=${OPTIONS_INITIAL_BALANCE:-4}
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
  say "     (token/oracle: $INITIAL_BALANCE NEAR each; options: $OPTIONS_INITIAL_BALANCE NEAR)"
  say "  3. deploy mock-ft         -> $TOKEN_ID       new(owner=$VENUE)"
  say "  4. deploy price-oracle    -> $ORACLE_ID      new(owner=$VENUE)"
  say "  5. deploy bounded-options -> $OPTIONS_ID     new(admin, token, oracle)"
  say "  6. set_venue($VENUE) and set_fee_terms(50,100)"
  say "  7. generate four seat keys in data/testnet-seats, register them on the"
  say "     ORACLE, set_quorum(2) — refuse if any of it fails"
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
  if near state "$account" --network-id "$NETWORK" >/dev/null 2>&1; then
    say "    $account exists, leaving it alone"
  else
    balance=$INITIAL_BALANCE
    [ "$account" != "$OPTIONS_ID" ] || balance=$OPTIONS_INITIAL_BALANCE
    near create-account "$account" --use-account "$VENUE" \
      --initial-balance "$balance" --network-id "$NETWORK"
  fi
done

say "==> 3/8 deploying the mock token"
# Signed with the venue's key, which `near` reads from its credentials store;
# there is no --use-account on `deploy`.
near deploy "$TOKEN_ID" "$WASM_DIR/mock_ft/mock_ft.wasm" \
  --init-function new \
  --init-args "{\"owner\":\"$VENUE\"}" \
  --init-gas "$GAS" \
  --network-id "$NETWORK"

say "==> 4/8 deploying the oracle"
near deploy "$ORACLE_ID" "$WASM_DIR/price_oracle/price_oracle.wasm" \
  --init-function new \
  --init-args "{\"owner\":\"$VENUE\"}" \
  --init-gas "$GAS" \
  --network-id "$NETWORK"

say "==> 5/8 deploying the options contract"
near deploy "$OPTIONS_ID" "$WASM_DIR/bounded_options/bounded_options.wasm" \
  --init-function new \
  --init-args "{\"admin\":\"$VENUE\",\"token\":\"$TOKEN_ID\",\"oracle\":\"$ORACLE_ID\"}" \
  --init-gas "$GAS" \
  --network-id "$NETWORK"

say "==> 6/8 naming the venue and its fee schedule"
# The operator quotes as the venue account; the contract refuses anyone else.
near call "$OPTIONS_ID" set_venue "{\"venue\":\"$VENUE\"}" \
  --use-account "$VENUE" --gas "$GAS" --network-id "$NETWORK"
# 0.5% over mid to open, 1% under mid to close — the sandbox's schedule, and
# honest about being a demo fee rather than a market one.
near call "$OPTIONS_ID" set_fee_terms '{"margin_bps":50,"impact_bps":100}' \
  --use-account "$VENUE" --gas "$GAS" --network-id "$NETWORK"

say "==> 7/8 the settlement committee"
# Seats live on the ORACLE contract: a seat is a name plus a public key the oracle
# will accept signed prices from. The contract refuses to fix a price without two
# independent sources, and refuses to settle without a fix — so a venue deployed
# without this step lists markets that can never settle.
#
# The keys are generated here and kept, not regenerated on boot. A sandbox seat is
# correct to forget its key: the chain it signs for is deleted with the process. A
# testnet seat is the opposite. If its key changes on restart it becomes a source
# registered on chain that no longer exists on the machine, and every settlement
# after that restart fails.
SEATS_DIR=${SEATS_DIR:-data/testnet-seats}
SIGNER=${SIGNER:-target/debug/oracle-signer}
if [ ! -x "$SIGNER" ]; then
  say "    building oracle-signer"
  cargo build -p oracle-signer >/dev/null
fi
for seat in venue-index kraken okx pyth; do
  "$SIGNER" --name "$seat" \
    --key-file "$SEATS_DIR/$seat.key" \
    --registration-out "$SEATS_DIR/$seat.json" >/dev/null
  say "    $seat  $(cat "$SEATS_DIR/$seat.json")"
done
say ""
say "    these four keys own the settlement vote. Back them up: without them the"
say "    venue can never settle a ticket again. They are written 0600 — but on this"
say "    machine the relay can read them, so a real committee keeps them apart."
say ""
# Quorum 2 of 4: two sources can settle, and one source alone never can. That is
# the property the contract's require! enforces, and the reason a single dishonest
# seat is not enough to move a payout.
for seat in venue-index kraken okx pyth; do
  near call "$ORACLE_ID" add_source "$(cat "$SEATS_DIR/$seat.json")" \
    --use-account "$VENUE" --gas "$GAS" --network-id "$NETWORK" >/dev/null
  say "    registered $seat"
done
near call "$ORACLE_ID" set_quorum '{"quorum":2}' \
  --use-account "$VENUE" --gas "$GAS" --network-id "$NETWORK"
say "    quorum 2 of 4"

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
say "  near view $OPTIONS_ID get_storage_state --network-id $NETWORK"
say ""
say "if 'funded' is false, anyone can top it up:"
say "  near call $OPTIONS_ID storage_deposit '{}' --use-account $VENUE --deposit 1 --network-id $NETWORK"
