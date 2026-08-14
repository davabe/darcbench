#!/usr/bin/env bash
# Fails if the Rust event enum and the TypeScript EVENT_KINDS list disagree.
#
# The TypeScript protocol types are hand-maintained (see ADR-0004). This is what
# stops that from silently rotting: an event kind added in Rust and forgotten in
# the UI would otherwise just never render.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RUST="$ROOT/crates/darcbench-protocol/src/events.rs"
TS="$ROOT/apps/web/src/types.ts"

rust_kinds=$(grep -oP '(?<=#\[serde\(rename = ")[a-z.]+(?="\)\])' "$RUST" | sort -u)
ts_kinds=$(sed -n '/EVENT_KINDS = \[/,/\] as const/p' "$TS" | grep -oP "(?<=')[a-z.]+(?=')" | sort -u)

missing_in_ts=$(comm -23 <(echo "$rust_kinds") <(echo "$ts_kinds"))
missing_in_rust=$(comm -13 <(echo "$rust_kinds") <(echo "$ts_kinds"))

status=0
if [ -n "$missing_in_ts" ]; then
  echo "Event kinds in Rust but not in apps/web/src/types.ts EVENT_KINDS:" >&2
  echo "$missing_in_ts" | sed 's/^/  /' >&2
  status=1
fi
if [ -n "$missing_in_rust" ]; then
  echo "Event kinds in EVENT_KINDS but not in the Rust enum:" >&2
  echo "$missing_in_rust" | sed 's/^/  /' >&2
  status=1
fi

if [ "$status" -eq 0 ]; then
  echo "protocol parity ok ($(echo "$rust_kinds" | wc -l) event kinds)"
fi
exit "$status"
