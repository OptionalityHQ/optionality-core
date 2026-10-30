#!/usr/bin/env bash
# Everything that has to pass before a change is worth calling work: the Rust
# unit suites, the sandbox end-to-end run (one thread, because every test drives
# the same in-process chain), the JavaScript fixtures, and the gateway's own
# suite. A green build script is the difference between "it compiles" and "the
# venue still adds up".
set -euo pipefail
cd "$(dirname "$0")/.."

./scripts/build.sh
cargo test -p bounded-core
cargo test -p surface
cargo test -p bounded-options --lib
cargo test -p price-oracle --lib
cargo test -p engine
cargo test -p integration-tests --test e2e -- --test-threads=1
cargo test -p chain-host
cargo test -p oracle-signer
node --test api/test/*.test.ts
