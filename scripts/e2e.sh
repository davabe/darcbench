#!/usr/bin/env bash
# End-to-end validation of the real binary.
#
# Exercises the full path: doctor -> run -> artifacts -> verify -> tamper
# detection -> serve -> API auth -> CSRF -> endurance cycles -> run index,
# comparison and retention -> SSE -> cancel.
#
# Everything runs against a throwaway state directory, so this is safe to run
# on a development machine. It is CPU-intensive for ~30 seconds.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="${DARCBENCH_BIN:-$ROOT/target/release/darcbench}"
PORT="${DARCBENCH_E2E_PORT:-7899}"
STATE="$(mktemp -d -t darcbench-e2e-XXXXXX)"
export DARCBENCH_HOME="$STATE"

pass=0; fail=0
ok()   { printf '  \033[32mok\033[0m   %s\n' "$1"; pass=$((pass+1)); }
bad()  { printf '  \033[31mFAIL\033[0m %s\n' "$1"; fail=$((fail+1)); }
check(){ if [ "$2" = "$3" ]; then ok "$1"; else bad "$1 (expected '$3', got '$2')"; fi; }
# Runs a python assertion block; a failure is reported, never silently skipped.
pycheck(){ local label="$1"; shift; if python3 -c "$*" 2>"$STATE/pyerr"; then ok "$label"; else bad "$label"; sed 's/^/       /' "$STATE/pyerr" >&2; fi; }
# Runs a command; reports either way.
#
# This exists because `cmd && ok "label"` is a silent skip, not a check. Under
# `set -e` a failure inside a `&&` list does not exit the shell, and with no
# `|| bad` arm nothing is recorded at all: `pass` does not rise, `fail` stays
# at zero, and the final `[ "$fail" -eq 0 ]` reports success for a run in which
# the assertion never held. Five checks were written that way, `darcbench
# verify` among them. Every check must land on `ok` or `bad`.
try(){ local label="$1"; shift; if "$@" >/dev/null 2>&1; then ok "$label"; else bad "$label"; fi; }

cleanup() {
  [ -n "${SERVE_PID:-}" ] && kill "$SERVE_PID" 2>/dev/null || true
  rm -rf "$STATE"
}
trap cleanup EXIT

if [ ! -x "$BIN" ]; then
  echo "darcbench binary not found at $BIN" >&2
  echo "build it with: cargo build --release" >&2
  exit 1
fi

echo "DARCBench end-to-end validation"
echo "  binary $BIN"
echo "  state  $STATE"
echo

# --- CLI ------------------------------------------------------------------
echo "CLI"
try "--version" "$BIN" --version
if "$BIN" doctor --json > "$STATE/doctor.json"; then ok "doctor --json exits 0"; else bad "doctor --json exits 0"; fi
pycheck "doctor reports uncalibrated model and zero writes" "
import json
d=json.load(open('$STATE/doctor.json'))
assert d['scoring_calibrated'] is False, 'model must report itself uncalibrated'
assert d['preflight']['estimated_bytes_written'] > 0, 'storage.mixed must declare the space it needs'
assert d['preflight']['estimated_write_volume_bytes'] > d['preflight']['estimated_bytes_written'], \
    'flash wear must be disclosed and must exceed the one-off space bound'
assert any(f['check']=='storage.wear' and not f['blocking'] for f in d['preflight']['findings']), \
    'a storage run must disclose wear without blocking'
assert d['preflight']['risk'] in ('heavy_load','production_risk'), d['preflight']['risk']
assert d['preflight']['estimated_network_bytes'] == 0, \
    'the default profile must make no outbound connection'
assert not any(f['check']=='network.egress' for f in d['preflight']['findings']), \
    'nothing to disclose means nothing to say'
"

"$BIN" inspect --json > "$STATE/inv.json"
pycheck "inspect redacts identifying fields by default" "
import json
i=json.load(open('$STATE/inv.json'))
assert i['redacted'] is True
assert i['inventory']['platform']['hostname'] == '[redacted]', 'hostname must redact'
assert i['inventory']['cpu']['logical_cpus'] >= 1
"

# --- full run --------------------------------------------------------------
echo
echo "Benchmark run"
"$BIN" run --profile quick --force --json > "$STATE/run.json"
pycheck "run produces a signed, correctly-labelled Partial bundle" "
import json
b=json.load(open('$STATE/run.json'))
assert b['meta']['schema']=='darcbench.bundle/1'
assert b['meta']['build_profile']=='release', 'e2e must run a release build'
assert b['run']['state']=='completed'
expected={'cpu.mixed':10,'memory.bandwidth':13,'storage.mixed':10}
got={m['module']['id']: len(m['metrics']) for m in b['modules']}
assert got==expected, f'expected {expected}, got {got}'
assert b['scores']['uncalibrated'] is True
assert b['scores']['total_is_standard'] is False, 'the quick profile omits network and web'
assert b['verdict']['state']=='partial'
assert b['signature']['algorithm']=='ed25519'
for mod in b['modules']:
    for m in mod['metrics']:
        assert m['value']>0, m['key']
        assert m['summary']['n']==5, m['key']
        assert m['unit'], m['key']
# Latency is the one inverted metric; getting its direction wrong would rank
# the slowest machine first.
directions={m['key']: m['direction'] for mod in b['modules'] for m in mod['metrics']}
assert directions['latency_random.single']=='lower_is_better'
assert directions['sequential_read.single']=='higher_is_better'
cats={c['key'] for c in b['scores']['categories']}
assert {'compute','memory','storage'} <= cats, f'expected compute, memory and storage, got {cats}'
# The storage module writes; the run must leave nothing behind.
import os
scratch = os.path.join('$STATE', 'scratch')
leftovers = os.listdir(scratch) if os.path.isdir(scratch) else []
assert not leftovers, f'the storage fixture was not cleaned up: {leftovers}'
"

RUN_DIR=$(find "$STATE/runs" -mindepth 1 -maxdepth 1 -type d | head -1)
for f in bundle.json report.html events.ndjson; do
  [ -s "$RUN_DIR/$f" ] && ok "artifact $f written" || bad "artifact $f missing"
done

pycheck "event stream is gapless, complete and terminal" "
import json
lines=[json.loads(l) for l in open('$RUN_DIR/events.ndjson')]
assert [e['seq'] for e in lines]==list(range(len(lines))), 'event sequence must be gapless'
kinds={e['type'] for e in lines}
for k in ['run.created','run.preflight.completed','module.started','module.sample',
          'module.completed','score.provisional','score.final','report.generated','run.completed']:
    assert k in kinds, 'missing '+k
assert lines[-1]['type']=='run.completed', 'log must end with run.completed, ended with '+lines[-1]['type']
"

try "report carries the uncalibrated banner" grep -q "Provisional scores" "$RUN_DIR/report.html"
grep -qE 'https?://' "$RUN_DIR/report.html" && bad "report references an external URL" \
  || ok "report is self-contained"

# --- verification -----------------------------------------------------------
echo
echo "Verification"
try "honest bundle verifies" "$BIN" verify "$RUN_DIR/bundle.json"

python3 -c "
import json
b=json.load(open('$RUN_DIR/bundle.json')); b['scores']['total']=99999.0
json.dump(b,open('$STATE/tampered-score.json','w'))
b=json.load(open('$RUN_DIR/bundle.json')); b['modules'][0]['metrics'][0]['value']*=10
json.dump(b,open('$STATE/tampered-metric.json','w'))
"
for t in tampered-score tampered-metric; do
  if "$BIN" verify "$STATE/$t.json" >/dev/null 2>&1; then
    bad "$t was accepted"
  else
    ok "$t is rejected"
  fi
done

# --- server -----------------------------------------------------------------
echo
echo "Agent API"
TOKEN=$(python3 -c "print('e'*64)")
"$BIN" serve --port "$PORT" --token "$TOKEN" --json > "$STATE/serve.json" 2>&1 &
SERVE_PID=$!
B="http://127.0.0.1:$PORT"
for _ in $(seq 1 100); do
  curl -sf "$B/healthz" >/dev/null 2>&1 && break
  sleep 0.1
done

# The run above was made by a *previous* process. Every per-run endpoint used to
# resolve only through the in-memory run list, so `serve` listed this run and
# then 404'd its bundle, its report and its event stream. Nothing caught it:
# the SSE and cancellation checks further down both use a run started inside
# this serving process, which is the one case that always worked.
RUN_ID=$(basename "$RUN_DIR")
for ep in "" "/bundle" "/report"; do
  check "serve answers ${ep:-/} for a run from an earlier process" \
    "$(curl -s -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $TOKEN" "$B/api/v1/runs/$RUN_ID$ep")" "200"
done
pycheck "the replayed event stream is the recorded log, gapless and terminal" "
import json, urllib.request
req = urllib.request.Request('$B/api/v1/runs/$RUN_ID/events',
                             headers={'Authorization': 'Bearer $TOKEN'})
body = urllib.request.urlopen(req, timeout=30).read().decode()
seqs, kinds = [], []
for block in body.split('\n\n'):
    for line in block.splitlines():
        if line.startswith('id:'):   seqs.append(int(line[3:].strip()))
        if line.startswith('event:'): kinds.append(line[6:].strip())
assert seqs, 'the replay delivered no events'
assert seqs == sorted(seqs) and len(seqs) == len(set(seqs)), 'replay must be ordered and gapless'
assert kinds[-1] == 'run.completed', 'replay must end where the log ends, got ' + kinds[-1]
on_disk = sum(1 for _ in open('$RUN_DIR/events.ndjson'))
assert len(seqs) == on_disk, f'replayed {len(seqs)} of {on_disk} recorded events'
"
check "a cancel against a finished run is still refused" \
  "$(curl -s -o /dev/null -w '%{http_code}' -X POST -H "Authorization: Bearer $TOKEN" "$B/api/v1/runs/$RUN_ID/cancel")" "404"
check "an unknown run id is still 404"  "$(curl -s -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $TOKEN" "$B/api/v1/runs/run_$(printf '0%.0s' $(seq 32))/bundle")" "404"

check "healthz is unauthenticated"      "$(curl -s -o /dev/null -w '%{http_code}' "$B/healthz")" "200"
check "inventory requires a token"      "$(curl -s -o /dev/null -w '%{http_code}' "$B/api/v1/inventory")" "401"
check "cookie auth cannot start a run"  "$(curl -s -o /dev/null -w '%{http_code}' -X POST -H "Cookie: darcbench_session=$TOKEN" -H 'Content-Type: application/json' -d '{}' "$B/api/v1/runs")" "403"
check "query auth cannot start a run"   "$(curl -s -o /dev/null -w '%{http_code}' -X POST -H 'Content-Type: application/json' -d '{}' "$B/api/v1/runs?token=$TOKEN")" "403"
check "malformed run id is rejected"    "$(curl -s -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $TOKEN" "$B/api/v1/runs/..%2f..%2fetc%2fpasswd")" "400"
check "unknown profile is rejected"     "$(curl -s -o /dev/null -w '%{http_code}' -X POST -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' -d '{"profile":"nonsense"}' "$B/api/v1/runs")" "400"
# The web profile used to resolve to nothing and be refused. It now runs
# `web.static`, so the catalogue is what proves it - starting it here would run
# a second benchmark inside the API phase.
pycheck "the web profile is available and resolves to the web modules" "
import json, urllib.request
req = urllib.request.Request('$B/api/v1/profiles', headers={'Authorization': 'Bearer $TOKEN'})
profiles = {p['key']: p for p in json.load(urllib.request.urlopen(req))['profiles']}
web = profiles['web_only']
assert web['available'] is True, web
assert web['modules'] == ['web.static', 'php.runtime', 'node.runtime'], web['modules']
assert 'web.static' in profiles['standard']['modules'], profiles['standard']['modules']
"

curl -sI "$B/" | grep -qi 'content-security-policy' && ok "CSP header present" || bad "CSP header missing"
curl -sI "$B/" | grep -qi 'x-frame-options: DENY'   && ok "X-Frame-Options DENY" || bad "X-Frame-Options missing"

# The network module is checked by inspecting the catalogue, never by running
# it. This script executes on every change; a benchmark suite whose test suite
# pulls megabytes from a third party on every commit is exactly the traffic
# amplifier the threat model forbids.
curl -s -H "Authorization: Bearer $TOKEN" "$B/api/v1/profiles" > "$STATE/profiles.json"
curl -s -H "Authorization: Bearer $TOKEN" "$B/api/v1/modules"  > "$STATE/modules.json"
pycheck "quick is egress-free and standard declares its traffic" "
import json
p={e['key']: e for e in json.load(open('$STATE/profiles.json'))['profiles']}
assert 'network.transfer' not in p['quick']['modules'], \
    'the first profile anyone runs must not open an outbound connection'
assert 'network.transfer' not in p['read_only']['modules'], \
    'a profile for sensitive production hosts must not add network traffic'
for key in ('standard','deep'):
    assert 'network.transfer' in p[key]['modules'], key
"
pycheck "the network module declares a real transfer ceiling" "
import json
m={x['id']: x for x in json.load(open('$STATE/modules.json'))['modules']}
n=m['network.transfer']
assert n['safety_class']=='uses_network', n['safety_class']
assert n['max_bytes_written']==0, 'the network module must not write to disk'
assert 0 < n['max_network_bytes'] <= (1<<30), n['max_network_bytes']
assert any('anycast' in l for l in n['limitations']), \
    'the single-provider limitation is a methodology requirement'
assert any('packet loss' in l.lower() for l in n['limitations']), \
    'not measuring packet loss must be declared, not left implied'
"
pycheck "endurance cycles without pulling from a third party" "
import json
p={e['key']: e for e in json.load(open('$STATE/profiles.json'))['profiles']}
assert 'network.transfer' not in p['endurance']['modules'], \
    'cycling the network module for an hour would breach its per-run transfer ceiling'
assert p['endurance']['nominal_minutes'][0] >= 60, p['endurance']['nominal_minutes']
"

# --- endurance -----------------------------------------------------------------
#
# Run with the shortest permitted duration against the cheapest module. The point
# is to exercise the cycle loop, the cycle tagging and the retention computation
# end to end on the real binary - not to spend an hour, which is what the
# profile does in earnest. The minimum-cycles rule is what makes this possible:
# two cycles complete even though the target elapses during the first.
echo
echo "Endurance cycles"
"$BIN" run --profile endurance --duration-minutes 2 --modules cpu.mixed --force --json \
  > "$STATE/endurance.json"
pycheck "a cycling run repeats, tags its cycles and reports what it retained" "
import json
b=json.load(open('$STATE/endurance.json'))
assert b['run']['state']=='completed', b['run']['state']
# An overridden duration is never a standard run: it was given less time to decline.
assert b['run']['profile']=='custom', b['run']['profile']
assert b['run'].get('stopped_because') is None, b['run']['stopped_because']

cycles=[m['cycle'] for m in b['modules']]
assert len(cycles) >= 2, f'a cycling run must complete at least two cycles, got {len(cycles)}'
assert cycles==list(range(len(cycles))), f'cycles must be tagged in order: {cycles}'

s=b['scores']['sustained']
assert s['cycles']==len(cycles), (s['cycles'], len(cycles))
assert s['scored_cycle']==len(cycles)-1, 'scores come from the last cycle, not the opening burst'
assert 0 < s['retention'] < 10, s['retention']
assert s['score'] <= 1000.0, 'the score is capped even when a machine speeds up'
assert s['by_metric'], 'per-metric retention is what localises a decline'

d=b['sustained_diagnosis']
assert d['cause'] in ('sustained','thermal_throttling','burst_credit_exhaustion',
                      'noisy_neighbour','undiagnosed'), d['cause']
assert d['explanation']
assert d['evidence']['samples'] > 0, 'a diagnosis must rest on telemetry actually taken'
"
pycheck "a non-cycling run claims no retention it did not measure" "
import json
b=json.load(open('$STATE/run.json'))
assert 'sustained' not in b['scores'] or b['scores']['sustained'] is None, \
    'a run never given time to decline has not shown that it would not'
assert b.get('sustained_diagnosis') is None
assert all(m.get('cycle',0)==0 for m in b['modules'])
"

# --- the PHP runtime module ----------------------------------------------------
#
# Worth its own leg because it is the only module that executes a program the
# agent did not build. The interesting assertions are about disclosure and about
# the module declining to run rather than reporting zeroes.
echo
echo "Runtime modules"
if [ -x /usr/bin/php ] || [ -x /usr/local/bin/php ]; then
  "$BIN" run --profile web --modules php.runtime --force --json > "$STATE/php.json" 2>/dev/null \
    || true
  pycheck "the PHP module measures the interpreter and discloses exactly which one" "
import json
b=json.load(open('$STATE/php.json'))
mods={m['module']['id']: m for m in b['modules']}
assert 'php.runtime' in mods, list(mods)
php=mods['php.runtime']
assert php['status'] in ('completed','degraded'), php['status']

keys={m['key'] for m in php['metrics']}
expected={'json.encode','json.decode','array.ops','template.render','hash.sha256',
          'hash.password','startup.cold'}
assert keys == expected, f'expected {expected}, got {keys}'
for m in php['metrics']:
    assert m['value'] > 0, m['key']
    assert m['unit'], m['key']

# bcrypt is deliberately expensive and must dominate every cheap workload. If it
# does not, the pinned cost is not being applied.
rate={m['key']: m['value'] for m in php['metrics']}
assert rate['json.encode'] > rate['hash.password'] * 100, rate
assert next(m for m in php['metrics'] if m['key']=='startup.cold')['direction'] == 'lower_is_better'

# Disclosure is part of the deliverable: a PHP number whose runtime cannot be
# described is not comparable with anything.
d=php['context']['php']
assert d['path'].startswith('/'), d
assert d['version'] and d['sapi'], d
assert 'opcache_enabled' in d and 'zts' in d, d
assert php['context']['bcrypt_cost'] == 8

# The module writes one script and must not leave it behind.
import os, glob
assert not glob.glob(os.path.join('$STATE','scratch','*.php')), 'the workload script survived'
"
else
  ok "no PHP on this host; the module's own precondition path is unit-tested"
fi

pycheck "the runtime modules are absent from the profile that claims a standard total" "
import json, urllib.request
req = urllib.request.Request('$B/api/v1/profiles', headers={'Authorization': 'Bearer $TOKEN'})
profiles = {p['key']: p for p in json.load(urllib.request.urlopen(req))['profiles']}
for runtime in ('php.runtime', 'node.runtime'):
    assert runtime not in profiles['standard']['modules'], (runtime, profiles['standard']['modules'])
    assert runtime in profiles['web_only']['modules'], (runtime, profiles['web_only']['modules'])
    assert runtime in profiles['deep']['modules'], (runtime, profiles['deep']['modules'])
"

# The Node module refuses a runtime it cannot vouch for, which is the case on
# any host whose Node came from a version manager or a user-unpacked tarball.
# Both outcomes are correct; what must never happen is silence.
"$BIN" run --profile web --modules node.runtime --force --json > "$STATE/node.json" 2>"$STATE/node.err" || true
pycheck "the Node module either measures its runtime or refuses it with a reason" "
import json
try:
    b=json.load(open('$STATE/node.json'))
except Exception:
    b=None
if b is None or not b.get('modules'):
    err=open('$STATE/node.err').read() + open('$STATE/node.json').read()
    assert 'T-EXEC' in err or 'nvm' in err or 'no Node' in err or 'refused' in err, err[:400]
else:
    node=b['modules'][0]
    keys={m['key'] for m in node['metrics']}
    if node['status'] in ('completed','degraded') and keys:
        expected={'json.stringify','json.parse','ssr.render','crypto.hash','async.fileio',
                  'module.load','startup.cold'}
        assert keys == expected, f'expected {expected}, got {keys}'
        rate={m['key']: m['value'] for m in node['metrics']}
        assert rate['json.stringify'] > rate['module.load'] * 100, rate
        d=node['context']['node']
        assert d['path'].startswith('/') and d['version'] and d['v8'], d
        assert d['jitless'] is False, d
        assert node['context']['dependency_install'].startswith('not performed'), node['context']
    else:
        assert node.get('error'), node
"

# --- run index, comparison and retention ---------------------------------------
#
# By this point the state directory holds the quick run and the endurance run,
# both written by processes that have exited - so everything below also proves
# the index survives the process that wrote it, which is the whole point of it
# not living in memory.
echo
echo "Run index"
"$BIN" status --json > "$STATE/status.json"
pycheck "the index lists runs written by processes that have exited" "
import json
runs=json.load(open('$STATE/status.json'))['runs']
assert len(runs) >= 2, f'expected the quick and endurance runs, got {len(runs)}'
ids=[r['run_id'] for r in runs]
assert len(set(ids))==len(ids), 'a run must appear once'
finished=[r['finished_at'] for r in runs]
assert finished==sorted(finished, reverse=True), 'newest first'
for r in runs:
    assert r['run_id'].startswith('run_'), r['run_id']
    assert r['bundle_digest'].startswith('sha256:'), r['bundle_digest']
    assert r['environment_digest'], 'comparability needs the machine digest'
    assert r['modules'], r
# Named explicitly rather than by position: which runs are newest depends on
# which legs of this script ran, and a comparison test that silently changes
# which two runs it compares tests something different every time it is edited.
quick=json.load(open('$STATE/run.json'))['run']['run_id']
endurance=json.load(open('$STATE/endurance.json'))['run']['run_id']
assert quick in ids and endurance in ids, (quick, endurance, ids)
open('$STATE/ids.txt','w').write(quick + chr(10) + endurance)
"
[ -f "$STATE/index.db" ] && ok "the index is a file under the state directory" \
  || bad "no index.db under the state directory"

# The quick run and the endurance run: different profiles and different module
# sets, so the comparison has both matched and unmatched metrics to report.
#
# Named `INDEX_A`/`INDEX_B` rather than `A`/`B`: `$B` is the server base URL in
# the phase below, and shadowing it silently pointed every later curl at a run id.
INDEX_A="$(sed -n 1p "$STATE/ids.txt")"; INDEX_B="$(sed -n 2p "$STATE/ids.txt")"
"$BIN" compare "$INDEX_A" "$INDEX_B" --json > "$STATE/compare.json"
pycheck "comparison is direction-adjusted and names what it could not match" "
import json
c=json.load(open('$STATE/compare.json'))
assert c['baseline']=='$INDEX_A' and c['candidate']=='$INDEX_B'
# Same machine, different profiles: the comparison is produced and labelled.
assert c['comparable'] is False, 'these two runs used different profiles'
assert any('profile' in r for r in c['incomparable_reasons']), c['incomparable_reasons']
assert c['metrics'], 'both runs ran cpu.mixed, so metrics must line up'
for m in c['metrics']:
    assert m['baseline'] > 0 and m['candidate'] > 0, m
    assert m['ratio'] > 0 and m['ratio'] < 100, m
    assert m['unit'], m
# The quick run measured memory and storage too; the endurance run did not.
assert c['unmatched'], 'metrics only one run has must be named, not dropped'
"
"$BIN" compare "$INDEX_A" run_00000000000000000000000000000000 --json > /dev/null 2>&1 \
  && bad "comparing an unknown run must fail" || ok "comparing an unknown run fails cleanly"
"$BIN" compare not-a-run-id "$INDEX_B" --json > /dev/null 2>&1 \
  && bad "a malformed run id must be rejected" || ok "a malformed run id is rejected"

"$BIN" prune --json > /dev/null 2>&1 \
  && bad "a prune with no policy must refuse" || ok "a prune with no policy refuses"
"$BIN" prune --keep-last 1 --json > "$STATE/prune-dry.json"
pycheck "a prune without --confirm reports and deletes nothing" "
import json, os
p=json.load(open('$STATE/prune-dry.json'))
assert p['applied'] is False
assert p['removed'], 'keeping one of several runs must select the rest'
for run_id in p['removed']:
    assert os.path.isdir(os.path.join('$STATE','runs',run_id)), \\
        f'{run_id} was deleted by a dry run'
"
"$BIN" prune --keep-last 0 --json > /dev/null 2>&1 \
  && bad "a policy of zero must refuse" || ok "a policy of zero refuses"
"$BIN" prune --keep-last 1 --confirm --json > "$STATE/prune.json"
# Re-listed *after* the prune: asserting against the pre-prune listing would
# pass whatever the prune actually did, which is the failure mode this check
# exists to catch.
"$BIN" status --json > "$STATE/status-after-prune.json"
pycheck "a confirmed prune removes the directories and keeps the newest run" "
import json, os
p=json.load(open('$STATE/prune.json'))
assert p['applied'] is True
assert not p['failed'], p['failed']
for run_id in p['removed']:
    assert not os.path.exists(os.path.join('$STATE','runs',run_id)), run_id
remaining=json.load(open('$STATE/status-after-prune.json'))['runs']
ids=[r['run_id'] for r in remaining]
assert ids, 'a prune must never empty the directory it was told to keep one of'
for run_id in p['removed']:
    assert run_id not in ids, f'{run_id} was deleted but is still indexed'
"

# --- live run, SSE and cancellation -------------------------------------------
echo
echo "Live run and cancellation"
RID=$(curl -s -X POST -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
      -d '{"profile":"quick","force":true}' "$B/api/v1/runs" \
      | python3 -c "import json,sys; print(json.load(sys.stdin)['run_id'])")
if [ -n "$RID" ]; then ok "run accepted ($RID)"; else bad "the API did not return a run id"; fi

check "concurrent run refused" "$(curl -s -o /dev/null -w '%{http_code}' -X POST -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' -d '{"profile":"quick","force":true}' "$B/api/v1/runs")" "409"

timeout 8 curl -sN -H "Authorization: Bearer $TOKEN" "$B/api/v1/runs/$RID/events" > "$STATE/sse.txt" 2>/dev/null || true
[ "$(grep -c '^data:' "$STATE/sse.txt")" -gt 5 ] && ok "SSE stream delivers events" || bad "SSE stream produced too few events"
grep -q '^id: ' "$STATE/sse.txt" && ok "SSE events carry replay ids" || bad "SSE events missing id"

curl -s -X POST -H "Authorization: Bearer $TOKEN" "$B/api/v1/runs/$RID/cancel" >/dev/null
for _ in $(seq 1 200); do
  STATE_NOW=$(curl -s -H "Authorization: Bearer $TOKEN" "$B/api/v1/runs/$RID" \
    | python3 -c "import json,sys; print(json.load(sys.stdin)['summary']['state'])")
  case "$STATE_NOW" in completed|failed|cancelled) break;; esac
  sleep 0.25
done
check "cancellation reaches a terminal state" "$STATE_NOW" "cancelled"

echo
printf 'passed %d, failed %d\n' "$pass" "$fail"
[ "$fail" -eq 0 ]
