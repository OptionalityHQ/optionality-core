#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

./scripts/build.sh
cargo test -p bounded-core
cargo test -p surface
cargo test -p bounded-options --lib
cargo test -p integration-tests --test e2e -- --test-threads=1
