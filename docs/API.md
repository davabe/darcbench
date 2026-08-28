# Agent HTTP API

**Base:** `http://127.0.0.1:7842` · **Version:** `v1` in the path
**Implementation:** `crates/darcbench-agent/src/server.rs`

## Authentication

A 256-bit token, generated per `serve` start unless `--token` is given.
Authentication yields one of two capabilities:

| Presented as | Capability | May mutate |
|---|---|---|
| `Authorization: Bearer <token>` | Header | ✅ |
| `Cookie: darcbench_session=<token>` | Ambient | ❌ |
| `?token=<token>` | Ambient | ❌ |

**Mutating requests require the header.** A browser can be made to send a cookie
cross-site but cannot attach a custom header without a CORS preflight, which is
refused — that is the CSRF defence, not a style preference. `EventSource` cannot
set headers, so the read-only SSE stream accepts the cookie.

No CORS headers are ever emitted, so no other origin can read a response.

## Error model

```json
{ "code": "run_in_progress", "message": "...", "detail": "run_fbf979..." }
```

Clients branch on `code`, never on `message`.

| Code | HTTP | Meaning |
|---|---|---|
| `unauthenticated` | 401 | Missing or invalid token |
| `csrf_protection` | 403 | Mutation attempted with cookie/query auth |
| `unknown_run` | 404 | No such run |
| `invalid_run_id` | 400 | Malformed run id |
| `invalid_module_id` | 400 | Malformed module id |
| `unknown_module` | 400 | Module not in the allow-list |
| `unknown_profile` | 400 | Unrecognised profile |
| `profile_unavailable` | 400 | Profile has no implemented modules |
| `run_in_progress` | 409 | Another run is active |
| `run_incomplete` | 409 | Run has not finished |
| `replay_unavailable` | 410 | Requested SSE position evicted |
| `internal` | 500 | Agent could not complete the request |

## Endpoints

### `GET /healthz` — unauthenticated
```json
{ "status": "ok", "agent_version": "0.1.0" }
```

### `GET /api/v1/meta` — unauthenticated

Deliberately reveals nothing about the machine: an unauthenticated caller learns
that an agent is here and which protocol it speaks, and nothing else.

```json
{
  "product": "DARCBench", "agent_version": "0.1.0",
  "protocol": "darcbench.events/1", "bundle_schema": "darcbench.bundle/1",
  "scoring_model": "dbs/0.2.0-dev", "scoring_calibrated": false,
  "authentication_required": true, "loopback_only": true
}
```

### `POST /api/v1/session`

Exchanges a bootstrap token for an `HttpOnly; SameSite=Strict` cookie. The UI
calls this once, then strips the token from the address bar.

`Secure` is added only when the browser reached the agent over TLS, as reported
by a terminating proxy's `X-Forwarded-Proto`. The agent itself always speaks
plain HTTP, so keying `Secure` off the bind address instead would make the
browser silently discard the cookie - and the SSE stream, which can only
authenticate by cookie, would fail with 401 on every non-loopback deployment.

### `GET /api/v1/inventory`

`?include_sensitive=true` is honoured **only** on a loopback bind. Over a tunnel
the answer is always redacted.

```json
{ "inventory": { "platform": {...}, "cpu": {...}, "memory": {...},
                 "storage": {...}, "network": {...}, "software": {...},
                 "gaps": [] },
  "redacted": true, "performance_digest": "sha256:..." }
```

### `GET /api/v1/profiles`

```json
{ "profiles": [ { "key": "quick", "standard": true,
                  "nominal_minutes": [3, 6],
                  "modules": ["cpu.mixed", "memory.bandwidth",
                              "storage.mixed"],
                  "available": true } ] }
```

### `GET /api/v1/modules`

Full manifests: safety class, dependencies, `max_bytes_written`,
`max_network_bytes`, cleanup, validation conditions, limitations, comparability
fields, stability bound.

### `POST /api/v1/runs` — mutating

```json
{ "profile": "quick", "force": false, "duration_minutes": null }
```

Supplying `modules` forces the run to `Custom`, whatever `profile` says —
letting a hand-picked module set claim a standard score is the easiest way to
game a benchmark suite. `force` overrides preflight **warnings** only; a blocking
finding can never be forced.

`duration_minutes` overrides how long a cycling profile — today only
`endurance` — keeps repeating its module set, and forces the run to `Custom` for
the same reason: two endurance runs of different lengths have been given
different amounts of time to decline, so neither ranks against the other. The
module set still comes from the profile that was named, so a shorter endurance
run is a shorter *endurance* run. Values outside 2…1440 are rejected with
`invalid_duration`; the ceiling exists so a mistyped value cannot hold a machine
at full load for a week.

`202 Accepted`:
```json
{ "run_id": "run_...", "profile": "quick",
  "modules": [{"id": "cpu.mixed", "version": "1.0.1"},
              {"id": "memory.bandwidth", "version": "1.0.0"},
              {"id": "storage.mixed", "version": "1.0.0"}],
  "events_url": "/api/v1/runs/run_.../events" }
```

### `GET /api/v1/runs`, `GET /api/v1/runs/{run_id}`

Summaries: state, progress (derived from completed modules, not elapsed time),
total score, result state.

The list merges two sources and de-duplicates by run id: the runs this process
is executing or has executed, and the run index. The first is the only place a
run *in flight* appears, because it has no bundle yet; the second is the only
place runs from a previous process appear. Listing only the first was the
Phase 1 behaviour, and it meant a freshly started agent reported no runs next to
a directory full of them. Bounded at 200 historical runs — pagination arrives
with the fleet views in Phase 7.

### `POST /api/v1/runs/{run_id}/cancel` — mutating

Cooperative. The run reaches `cancelled` and still produces a bundle, marked
`Invalid` with an `Interrupted` reason.

### `GET /api/v1/runs/{run_id}/events`

SSE. See [REALTIME-PROTOCOL.md](REALTIME-PROTOCOL.md). Honours `Last-Event-ID`.

### `GET /api/v1/runs/{run_id}/bundle`, `GET /api/v1/runs/{run_id}/report`

The signed JSON bundle, and the self-contained HTML report.

### `GET /api/v1/runs/{baseline}/compare/{candidate}`

Two runs lined up metric by metric, answered from the run index rather than by
parsing two bundles.

```json
{
  "baseline":  { "run_id": "run_...", "profile": "standard", "total_score": 712.0 },
  "candidate": { "run_id": "run_...", "profile": "standard", "total_score": 688.0 },
  "comparable": false,
  "incomparable_reasons": ["Scored by different models (dbs/0.1.0-dev and dbs/1.0.0). ..."],
  "metrics": [
    {
      "module": "storage.mixed", "metric_key": "latency_fsync.mean", "unit": "ms",
      "baseline": 0.41, "candidate": 0.82, "ratio": 0.5
    }
  ],
  "unmatched": ["network.transfer/download.single (baseline only)"]
}
```

Three properties worth knowing before reading a `ratio`:

- **It is direction-adjusted.** Above 1.0 always means the candidate did better.
  The example above is a doubled fsync latency, and it reads as 0.5 rather than
  as a 2x improvement.
- **Only cycle 0 is compared.** An endurance run's later cycles measure a
  machine that has been under load for an hour; what a cycling run *retained* is
  already its own published number.
- **`comparable: false` does not withhold the comparison**, it labels it.
  Comparing a run from before a kernel upgrade with one from after is a
  legitimate thing to do; reading the difference as the machine changing when it
  was the measurement that changed is not.

Metrics that could not be lined up - present in one run only, or not a positive
measurement in both - are named in `unmatched` rather than dropped. A comparison
that silently ignores what it could not match looks complete while describing a
subset.

## Security headers

Every response carries:

```
Content-Security-Policy: default-src 'self'; script-src 'self';
  style-src 'self' 'unsafe-inline'; img-src 'self' data:; font-src 'self';
  connect-src 'self'; frame-ancestors 'none'; base-uri 'none';
  form-action 'none'; object-src 'none'
X-Content-Type-Options: nosniff
X-Frame-Options: DENY
Referrer-Policy: no-referrer
Cache-Control: no-store, no-cache, must-revalidate
Permissions-Policy: camera=(), microphone=(), geolocation=(), interest-cohort=()
```

## Deliberately absent

There is no endpoint that accepts a command, a filesystem path, a URL or a
hostname. Module ids resolve against a compile-time allow-list. See
[THREAT-MODEL.md](THREAT-MODEL.md) (T-AGENT-RCE, T-AMPLIFY).

## Not yet implemented

Rate limiting (single-user loopback service today), OpenAPI document generation,
and pagination (run counts are small). All are Phase 5 items tracked in
[BACKLOG.md](BACKLOG.md), and the control-plane API — registration, upload,
comparisons, leaderboards, fleets — is specified there rather than here.
