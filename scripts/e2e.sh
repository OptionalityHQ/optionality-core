#!/usr/bin/env bash
# The Playwright end-to-end, against a venue that has to be up already.
#
#   scripts/venue.sh          # in one terminal
#   scripts/e2e.sh            # in another
#   scripts/e2e.sh --settle   # also hold a ticket to expiry and settle it
#
# It drives the real screen in the machine's own Chrome, clicks with Playwright's
# actionability checks, and fails on a console error, a failed request or a blank
# canvas. Evidence (screenshots, trace, report.json) lands in evidence/e2e.
set -euo pipefail
cd "$(dirname "$0")/.."

URL=${URL:-http://127.0.0.1:8790}
if ! curl -s -m 3 "$URL/health" >/dev/null; then
  echo "the venue is not answering at $URL — start it with scripts/venue.sh" >&2
  exit 1
fi
exec node sim/e2e.mts --url "$URL" --out evidence/e2e "$@"
