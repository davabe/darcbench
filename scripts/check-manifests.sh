#!/usr/bin/env bash
# Fails if a manifest under benchmarks/ disagrees with what the agent compiles in.
#
# The compiled manifest is authoritative; the JSON files exist so the module
# catalogue can be read and diffed without running the agent. They must not drift.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="${DARCBENCH_BIN:-$ROOT/target/release/darcbench}"

if [ ! -x "$BIN" ]; then
  echo "darcbench binary not found at $BIN; build it with: cargo build --release" >&2
  exit 1
fi

STATE="$(mktemp -d -t darcbench-manifest-XXXXXX)"
trap 'rm -rf "$STATE"' EXIT
export DARCBENCH_HOME="$STATE"

TOKEN=$(python3 -c "print('m'*64)")
PORT="${DARCBENCH_MANIFEST_PORT:-7898}"
"$BIN" serve --port "$PORT" --token "$TOKEN" --json >/dev/null 2>&1 &
PID=$!
trap 'kill $PID 2>/dev/null || true; rm -rf "$STATE"' EXIT

for _ in $(seq 1 100); do
  curl -sf "http://127.0.0.1:$PORT/healthz" >/dev/null 2>&1 && break
  sleep 0.1
done

curl -s -H "Authorization: Bearer $TOKEN" "http://127.0.0.1:$PORT/api/v1/modules" > "$STATE/live.json"

python3 - "$ROOT" "$STATE/live.json" <<'PY'
import json, sys, pathlib
root, live_path = pathlib.Path(sys.argv[1]), sys.argv[2]
live = {m["id"]: m for m in json.load(open(live_path))["modules"]}

status = 0
for path in sorted((root / "benchmarks").rglob("*.json")):
    doc = json.loads(path.read_text())
    mid = doc["id"]
    if mid not in live:
        print(f"{path}: module '{mid}' is not registered in the agent", file=sys.stderr)
        status = 1
        continue
    for field in ("version", "safety_class", "max_bytes_written",
                  "max_network_bytes", "stability_cv_bound"):
        if doc.get(field) != live[mid].get(field):
            print(f"{path}: {field} is {doc.get(field)!r} but the agent reports "
                  f"{live[mid].get(field)!r}", file=sys.stderr)
            status = 1

for mid in live:
    if not any(json.loads(p.read_text()).get("id") == mid
               for p in (root / "benchmarks").rglob("*.json")):
        print(f"module '{mid}' is registered but has no manifest under benchmarks/",
              file=sys.stderr)
        status = 1

if status == 0:
    print(f"manifests ok ({len(live)} module(s))")
sys.exit(status)
PY
