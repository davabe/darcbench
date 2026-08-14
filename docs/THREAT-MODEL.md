# DARCBench threat model

Method: STRIDE over the assets and trust boundaries in
`docs/ARCHITECTURE.md`. Every mitigation marked ✅ is implemented and covered by
a test; ⏳ is planned with a phase; ⚠️ is an accepted residual risk.

The framing that shapes everything below: **DARCBench is installed on machines
that are already doing something.** The likely host is a production web server
with customer sites on it, and the operator will often run the agent as root.
A benchmark tool that is careless in that environment is a liability regardless
of how good its numbers are.

---

## 1. Assets

| # | Asset | Why it matters |
|---|---|---|
| A1 | Availability of the host and the sites on it | A benchmark that causes an outage is a failure, however accurate |
| A2 | Existing configuration (nginx, Apache, Plesk/cPanel, firewall, systemd) | Damage here is often unrecoverable without a restore |
| A3 | Customer data on the host | Databases, site files, backups |
| A4 | The agent's Ed25519 signing key | Compromise lets an attacker mint "authentic" results |
| A5 | The dashboard access token | Grants full agent API access |
| A6 | Result integrity | The product's entire value |
| A7 | Identifying metadata | Hostnames, MACs, IPs, instance ids, panel inventory |
| A8 | Third parties | The host must never be used to attack them |

## 2. Adversaries

| # | Adversary | Capability |
|---|---|---|
| a1 | Unauthenticated network attacker | Can reach any listening port |
| a2 | Local unprivileged user on the host | Shared/reseller hosting is common |
| a3 | Malicious operator | Wants a better score than the hardware deserves; controls the machine completely |
| a4 | Malicious third-party module author | Phase 8 |
| a5 | Supply-chain attacker | Compromised dependency or release artifact |
| a6 | Curious insider at DARCBench | Control plane operator, Phase 5 |

---

## 3. Findings

### T-AGENT-RCE — Remote command execution via the API
**STRIDE:** Elevation of privilege · **Adversary:** a1, a2 · **Severity:** critical

The dashboard exposes a "start benchmark" button. The naive implementation
accepts a command, or a test name interpolated into one, and is a remote shell.

**Mitigation ✅ — structural, not filtering.** The API accepts a *module id
string*; `Registry::get` maps it to a compiled-in Rust implementation or rejects
it. There is no code path from an HTTP request to a process, a path or a command
line. `ModuleId` has a restricted grammar (`[a-z][a-z0-9_]*` segments) that
makes it safe as a path component, and `RunId` is `run_` plus 32 lowercase hex
characters. Both reject traversal attempts at parse time, before any use.

**Tests:** `unknown_modules_are_rejected_not_interpreted`,
`run_ids_from_the_path_are_validated_before_use`, `module_id_grammar`.
**Verified live:** `GET /api/v1/runs/..%2f..%2fetc%2fpasswd` → `400
invalid_run_id`.

---

### T-DASH-UNAUTH — Unauthenticated dashboard
**STRIDE:** Information disclosure, Elevation · **Adversary:** a1 · **Severity:** critical

**Mitigation ✅.** Every endpoint except `/healthz` and `/api/v1/meta` requires a
256-bit token. Default bind is `127.0.0.1`. `/api/v1/meta` deliberately reveals
nothing about the machine — only that an agent is here and which protocol it
speaks. Token comparison is constant-time. There is no "convenience" mode that
disables auth on loopback.

**Verified live:** `GET /api/v1/inventory` with no credential → `401`.

---

### T-CSRF — Cross-site request forgery starting or cancelling runs
**STRIDE:** Tampering, Denial of service · **Adversary:** a1 · **Severity:** high

The operator's browser holds a session cookie. A malicious page they visit could
`POST /api/v1/runs` and start a CPU-saturating benchmark on a production server.

**Mitigation ✅ — capability split.** Authentication produces one of two
capabilities:

- `Auth::Header` — token in `Authorization: Bearer`. May mutate.
- `Auth::Ambient` — token in the session cookie or query string. **Read-only.**

A browser can be made to send a cookie cross-site; it cannot attach a custom
header without a CORS preflight, which is refused (no CORS headers are emitted
at all). SSE rides the cookie because `EventSource` cannot set headers, and
streaming is read-only, so that is not a CSRF surface. The cookie is `HttpOnly`
and `SameSite=Strict`, and is marked `Secure` when the browser reached the agent
over TLS — determined from a terminating proxy's `X-Forwarded-Proto`, not from
the bind address, because the agent itself always speaks plain HTTP. Marking it
`Secure` on a plain-HTTP connection would make the browser discard it and break
the SSE stream, which has no other way to authenticate.

**Tests:** `cookie_auth_cannot_start_or_cancel_a_run`,
`query_token_cannot_start_a_run_either`.
**Verified live:** cookie-only and query-only `POST /api/v1/runs` → `403
csrf_protection`.

---

### T-TOKEN-URL — Token leakage via the URL
**STRIDE:** Information disclosure · **Adversary:** a1 · **Severity:** medium

The agent prints `http://127.0.0.1:7842/?token=…` because `EventSource` cannot
send headers, so the bootstrap has to arrive somehow.

**Mitigation ✅.** The UI immediately exchanges the token for an `HttpOnly`
cookie and calls `history.replaceState` to strip it from the address bar — so it
does not persist in history, in a `Referer`, or in a URL the operator
copy-pastes into a ticket. `Referrer-Policy: no-referrer` is set globally.

**Verified live:** the browser's URL after load is `http://127.0.0.1:7843/`.

**⚠️ Residual risk.** The token is in the operator's shell scrollback and, if
the agent is behind a reverse proxy, in that proxy's access log. Documented in
`docs/OPERATIONS.md`. Mitigated in practice by short-lived per-start tokens.

---

### T-XSS — Script injection into the dashboard or a report
**STRIDE:** Tampering · **Adversary:** a2 · **Severity:** medium

Inventory strings come from `/proc`, `/sys` and DMI on a machine DARCBench does
not control. A local user who can influence a DMI string or an interface name
could try to inject markup into a shared report.

**Mitigation ✅.**
- The HTML report escapes every dynamic value (`&`, `<`, `>`, `"`, `'`), tested
  against a hostile CPU model string and kernel release.
- React escapes by default; the UI never uses `dangerouslySetInnerHTML`.
- The fallback console builds DOM nodes with `textContent`, never HTML strings.
- CSP: `default-src 'self'; script-src 'self'; frame-ancestors 'none';
  base-uri 'none'; form-action 'none'; object-src 'none'`.
- The fallback console's script is served from its own route specifically so
  `script-src` needs no `'unsafe-inline'`. A test fails the build if an inline
  `<script>` reappears.

**Verified live:** all CSP and hardening headers present on every response.

---

### T-SSRF-METADATA — Cloud metadata endpoint access
**STRIDE:** Information disclosure · **Adversary:** a1, a2 · **Severity:** high

Reading `169.254.169.254` would let a benchmark harvest IAM credentials, and any
API that accepted a URL could be steered there.

**Mitigation ✅.** DARCBench **never** queries a metadata endpoint. Cloud
platform is inferred from DMI strings only. No API accepts a URL, a hostname or
a path from a caller. Phase 2's network module will use a compile-time
allow-list of endpoints, not caller-supplied ones.

---

### T-PATH — Path traversal and symlink attacks
**STRIDE:** Tampering, Elevation · **Adversary:** a2 · **Severity:** high

**Mitigation ✅.** All filesystem writes go through `StatePath::join`, which
rejects empty, `.`, `..`, and any component containing `/`, `\` or NUL, and
asserts the result is still under the root. Static asset lookup normalises to a
compile-time table key and drops — rather than resolves — any segment containing
`..`, including percent-encoded forms.

**⏳ Phase 2.** When storage modules land, temporary files must additionally be
created with `O_NOFOLLOW|O_EXCL` in a directory the agent owns, to close the
symlink-swap race that only appears once we create files an attacker might
predict.

---

### T-DISK — Filling the filesystem
**STRIDE:** Denial of service · **Adversary:** a3 (self-inflicted) · **Severity:** high

**Mitigation ✅.** Preflight refuses to start unless estimated writes plus a
2 GiB margin fit in the free space reported by `statvfs`. `f_bavail` is used
rather than `f_bfree`, so a root-run benchmark cannot eat the reserved-blocks
margin that keeps a full filesystem recoverable. **Unknown free space is treated
as unsafe, never as unlimited.**

---

### T-PROD — Degrading a live production server
**STRIDE:** Denial of service · **Adversary:** a3 (unintentional) · **Severity:** high

**Mitigation ✅.** Read-only discovery detects hosting panels, web servers,
databases, container runtimes and listening ports. `ProductionLikelihood` plus
module safety class produce a `RiskClass`; anything above observational on a
machine that looks live is `ProductionRisk` and does not start unattended.
Preflight shows estimated duration, bytes written and network transfer before
anything runs. `--force` overrides **warnings only** — a blocking finding can
never be forced.

**Tests:** `a_live_looking_server_is_classified_production_risk`,
`force_can_override_a_warning_but_never_a_blocker`.

---

### T-CONFIG — Damaging existing server configuration
**STRIDE:** Tampering · **Adversary:** a3 (unintentional) · **Severity:** critical

Corrupting a Plesk vhost or restarting nginx on a live box is worse than any
benchmark result is worth.

**Mitigation ✅.** The agent **never** modifies configuration. Detection is
purely filesystem existence checks and `/proc` reads; discovered binaries are
never executed to ask for a version. *Discovery* still executes nothing at all;
what a Phase 3 runtime module executes, and under what constraints, is
T-EXEC below — the two are deliberately separate, because "run it to see what
version it is" is a convenience and "run it because measuring it is the
deliverable" is not. The agent refuses to bind a port that
already has a listener, and `FORBIDDEN_PORTS` hard-blocks 80, 443, 22, 21, 25,
3306, 5432, 6379 and 8443 regardless of flags. Uninstall removes only the state
directory, because that is genuinely all that was ever created.

**✅ Phase 3 — `darcbench proxy`, and it still never modifies configuration.**
The deliverable required generate, preview, validate, back up and rollback
before activation. It is delivered under a stronger rule than the one asked
for: *DARCBench never writes a byte into a path the web server reads.*

| Attack / accident | Mitigation |
|---|---|
| A generated file breaks a live server | ✅ The file goes to `/etc/nginx/darcbench-location.conf`, which nothing scans. It is inert until the operator adds one `include` themselves |
| A syntactically broken snippet is handed to the operator | ✅ `apply` validates it in isolation against a generated wrapper, and removes it if the server's own validator rejects it |
| DARCBench reloads the server at a bad moment | ✅ It never reloads. `apply` prints the command |
| An existing file at our path is destroyed | ✅ Moved aside; and if the backup exists too, the whole operation is refused |
| Rollback leaves a dangling `include` and the server will not start | ✅ The config tree is searched for references first; a live one refuses the rollback, naming file and line. `--force` overrides |
| A URL prefix closes the config block and injects directives | ✅ Allow-list grammar `[A-Za-z0-9/._~-]`, so no character that terminates a block can appear |
| A planted `nginx` on `PATH` is executed as root to "validate" | ✅ Validation goes through `runtime_exec` — compile-time allow-list, ownership check on the binary and every ancestor, fixed argv, cleared environment, hard timeout (T-EXEC) |
| A panel regenerates the vhost and the snippet breaks or vanishes | ✅ Refused outright on Plesk, cPanel and DirectAdmin. No `--force` |
| The operator's `include` line is edited or removed by DARCBench | ✅ It never writes that line and cannot remove it; rollback says so |

**Verified live** against nginx 1.24: `apply` → isolated check passes → the
operator adds the `include` → `nginx -t` parses the snippet → `rollback` is
*refused* while the include is present → the line is removed → `rollback`
succeeds → the server is byte-for-byte as it started. See
[ADR-0014](adr/0014-reverse-proxy-integration.md), including the first design
that wrote into `conf.d/` and was rejected by a real nginx on its first run.

**⚠️ Accepted residual risk.** The dashboard becomes reachable on whatever
hostnames that server block answers, protected only by its token — which will
appear in the web server's access log the first time a browser follows the
bootstrap URL (T-TOKEN-URL). `apply` says so and tells the operator to put the
prefix behind whatever authentication their other admin paths use.

---

### T-EXEC — Executing a binary an attacker planted
**STRIDE:** Elevation of privilege · **Adversary:** a2 · **Severity:** critical

`php.runtime` and `node.runtime` (Phase 3) cannot measure how a machine runs PHP
or JavaScript without executing the interpreter the operator installed. Every
module before them executed nothing.

The adversary is not the operator, who controls the machine anyway. It is **a2,
a local unprivileged user** — shared and reseller hosting is the common case in
this market, and the agent is frequently run as root. If a compromised account
can write `/usr/local/bin/php`, a benchmark that executes "the PHP it found"
hands that account root.

**Mitigation ✅ (Phase 3), specified in [ADR-0013](adr/0013-executing-a-discovered-runtime.md):**

- **A compile-time allow-list of absolute paths.** `$PATH` is never consulted:
  it is environment, and a benchmark that runs whatever `php` resolves to runs
  whatever the environment says.
- **A safe-path check before execution.** The resolved binary and every ancestor
  directory must be owned by uid 0 and must not be group- or world-writable. A
  path that fails is skipped and the reason is reported, never silently used.
- **Fixed argv, no shell.** Nothing from any caller reaches the command line;
  the script is a compile-time constant written to the agent's own scratch
  directory at mode `0600` and passed by absolute path.
- **The environment is cleared, not filtered.** PHP reads `PHP_INI_SCAN_DIR` and
  Node reads `NODE_OPTIONS`; both turn an environment variable into code
  execution, and a filter is a list of the ones somebody thought of.
- **A hard timeout**, with the child killed on every exit path including
  cancellation.
- **Everything executed is disclosed** in the bundle: path, version, SAPI,
  OPcache state and limits. A result whose runtime cannot be described is not a
  result anyone can compare.

**Residual risk.** A root-owned interpreter that is itself malicious is outside
this boundary, and so is a machine whose root is already compromised. Both are
`a3`, and against `a3` no local measurement is trustworthy — which is why
`SelfReported` exists.

---

### T-CONTAINER — The container isolation tier
**STRIDE:** Elevation of privilege, Tampering · **Adversary:** a2, a5 ·
**Severity:** critical · **New in Phase 4**

Anything that can run containers can mount the host root. Phase 4 needs
containers anyway, because the alternative — measuring the database already on
the machine — is T-DB.

| Attack | Mitigation |
|---|---|
| A planted `docker` on `PATH` is executed as root | ✅ Compile-time path allow-list and the `runtime_exec` safe-path check on the binary and every ancestor (T-EXEC) |
| A bind mount gives the container the host filesystem | ✅ `run_args` is a pure function and a test reads the whole vector for `-v`, `--volume`, `--mount`, `--privileged`, `--device` and anything shaped like a bind mount |
| The benchmark database is reachable from the network | ✅ Published to `127.0.0.1` only, and a non-loopback binding reported by the runtime is refused rather than connected to |
| A container escalates inside itself | ✅ `--cap-drop ALL`, `--security-opt no-new-privileges` |
| A runaway container takes the host down | ✅ Memory, memory-swap and pid ceilings |
| Benchmark data is written to a host disk, or survives the run | ✅ The data directory is a tmpfs; every run starts from an identical empty instance |
| An environment value becomes a flag | ✅ Each `KEY=VALUE` follows its own `--env`, never spliced into a string |
| A mutable tag changes what is measured | ✅ Images pinned by digest, enforced by a test over the allow-list (T-SUPPLY) |
| An image is named into existence from config or an API call | ✅ `Image` has private fields and no public constructor; the allow-list is the only source |
| DARCBench removes the operator's containers | ✅ `reap` filters on a label this agent sets, never on a name prefix an operator could collide with |
| No runtime available, so the module measures the host's database instead | ✅ **No fallback path exists in the type.** Every failure is "not measured", and a test reads the operator-facing text to confirm none offers one |

**⚠️ Accepted residual risk.** The release profile aborts on panic and nothing
runs on `SIGKILL`, so an agent that dies mid-run leaves its container running;
`--rm` reaps on container exit, not agent exit. `reap` clears it at the start
of the next run, which bounds the cost rather than eliminating it. Stated
rather than hidden — it is the one guarantee the tier cannot make.

---

### T-DB — Destroying a production database
**STRIDE:** Tampering · **Adversary:** a3 (unintentional) · **Severity:** critical

**⏳ Phase 4, specified now.** Database modules must create their own isolated
instance and destroy only what they created. Connecting to an existing database
server is refused. Every created object carries a `darcbench_<run_id>_` prefix,
and cleanup deletes by prefix, never by pattern match on user data.

---

### T-SCORE-FRAUD — Faking a good result
**STRIDE:** Spoofing, Tampering · **Adversary:** a3 · **Severity:** high

| Attack | Mitigation |
|---|---|
| Edit `bundle.json` scores | ✅ Signature breaks; server recomputes scores from raw metrics |
| Edit raw metrics | ✅ Signature breaks |
| Run only favourable modules | ✅ Explicit module list forces `Custom`, never rankable |
| Hide failed modules | ✅ Failures are retained in the bundle and downgrade to `Partial` |
| Claim a container is a machine | ✅ Scope detected with evidence and always displayed |
| Debug build to game a threshold | ✅ `build_profile` recorded; non-release is not comparable |
| Upload scores under a scoring model the server does not implement | ✅ Unrecognised model is fatal; a score the server cannot recompute is never `Validated` |
| Clear `missing_required_categories` to dodge the `Partial` downgrade | ✅ Eligibility is decided from the *recomputed* card; every score field is compared |
| Replay an old good result | ⏳ Phase 6: server-issued nonce + run token |
| Patch the agent binary | ⏳ Phase 6: build hash must match a published release for `Verified` |
| Clock manipulation | ✅ Backwards clock → `ClockAnomaly` → `Invalid`; durations use a monotonic clock |
| Publish a custom run as standard | ✅ `Profile::is_standard()`, enforced in scoring and validation |

**Verified live:** editing `scores.total` in a stored bundle produces
`signature INVALID` **and** `score recompute MISMATCH`, exit code 3.

**⚠️ Accepted residual risk.** An operator who controls the machine can always
patch the agent and sign fabricated numbers with their own key. This is
unfixable without hardware attestation, which DARCBench will not require. It is
handled by *classification*, not prevention: a locally-signed bundle can never
exceed `SelfReported`, and only `Validated`/`Verified`/`Official` are rankable.
We do not build invasive hardware fingerprinting to chase it.

---

### T-KEY — Agent signing key compromise
**STRIDE:** Spoofing · **Adversary:** a2 · **Severity:** medium

**Mitigation ✅.** The key file is created with mode `0600` **at creation time**,
via `OpenOptions::mode()` with `create_new`, so there is no window where it
exists world-readable. It is never logged (`Debug` prints only the key id), never
transmitted, and never included in a bundle. A truncated or malformed key file is
rejected rather than padded.

**Tests:** `persisted_key_is_owner_only_and_reloads_identically`,
`a_truncated_key_file_is_rejected_rather_than_padded`,
`debug_never_prints_the_private_key`.

---

### T-PRIVACY — Leaking identifying data in a shared report
**STRIDE:** Information disclosure · **Adversary:** a6, accidental · **Severity:** medium

**Mitigation ✅ — redaction by default, in the type system.** Identifying values
are wrapped in `Sensitive<T>`, whose `Serialize` emits `[redacted]` unless a
scoped policy says otherwise. The failure mode of forgetting to think about
privacy is *over*-redaction, not a leaked hostname on a public page. Revealing
is opt-in, thread-scoped, and refused entirely on non-loopback binds. DMI serial
numbers and UUIDs are never collected at all. Run ids are random, not derived
from any host property.

**Verified live:** hostname and MAC addresses render as `[redacted]` in a bundle
fetched over the API.

---

### T-DOS-STREAM — Resource exhaustion via slow SSE clients
**STRIDE:** Denial of service · **Adversary:** a1 · **Severity:** low

**Mitigation ✅.** Bounded broadcast channel and a 4096-event replay buffer. A
lagging consumer's stream ends rather than growing without bound, forcing a
reconnect with `Last-Event-ID` — which is the path that can actually recover the
gap. Telemetry is capped at 1 Hz. The UI bounds its own log and telemetry
buffers.

---

### T-SUPPLY — Compromised dependency or release artifact
**STRIDE:** Tampering · **Adversary:** a5 · **Severity:** high

**⏳ Phase 8.** SBOM generation, `cargo audit` and `pnpm audit` in CI,
reproducible builds, signed release artifacts and container images, published
checksums. The install script must offer a verifiable alternative to
`curl | sh` — a documented download-verify-run sequence with published
signatures.

**Mitigation ✅ today.** The dependency set is small and deliberate; `unsafe`
code is forbidden workspace-wide; no third-party benchmark binaries are bundled.

---

### T-MODULE — Malicious third-party module
**STRIDE:** Elevation · **Adversary:** a4 · **Severity:** high

**Mitigation ✅ by omission.** There is no dynamic module loading. The registry
is a compile-time table.

**⏳ Phase 8.** If third-party modules ship, they require signed manifests,
integrity hashes, declared resource bounds, execution under seccomp + namespaces
+ cgroups, and they may **never** contribute to the official total score
(ADR-0006).

---

### T-AMPLIFY — Using DARCBench to attack third parties
**STRIDE:** Denial of service (of others) · **Adversary:** a3 · **Severity:** high

A benchmark suite that lets you point a load generator at an arbitrary URL is a
DDoS tool with a scoring model.

**Mitigation ✅ today:** no module accepts a URL or hostname.
**⏳ Phase 2/3, binding:** network endpoints come from a compile-time
allow-list; HTTP load generation targets **only** a server the agent started.
There will be no "benchmark this URL" feature. This is a permanent product
constraint, not a backlog item.

**Amended in Phase 3 for the external load-generator mode, and the wording
above changed with it.** It previously said "a server the agent started *on
loopback*". The external mode lets the generator run on a second machine, so
the origin it drives is on a different host and the loopback clause is no
longer literally true — but the substance is unchanged and is what the clause
was protecting:

* The generator still cannot be pointed at an arbitrary URL. It takes a
  host and a port, and before it sends a single request of load it must
  receive a `SessionOffer` (`darcbench-protocol::external`) carrying a
  protocol version it recognises, from a peer that accepted a 256-bit token
  the operator carried from the target machine by hand. A stranger's web
  server cannot produce one, so a stranger's web server is never loaded.
* The property is **consent, not access control**, and the difference is
  stated plainly rather than glossed: nothing can stop somebody writing their
  own load generator. What this stops is *this* binary being usable as one,
  and an operator accidentally pointing a benchmark at production.

**Verified live:** `an_external_origin_without_a_token_never_starts_listening`
and the `Refusal::*` tests in `darcbench-protocol::external`.

---

### T-EXPOSE — The benchmark origin reachable by strangers
**STRIDE:** Denial of service, Information disclosure · **Adversary:** a3 ·
**Severity:** medium · **New in Phase 3**

The external load-generator mode is the first time DARCBench opens a listening
socket on something other than loopback for a *benchmark* (the dashboard's
`serve --bind` has always been able to, and warns loudly). A benchmark origin
on a datacentre network is a thing strangers can find.

| Attack | Mitigation |
|---|---|
| Reach the origin without the token | ✅ Every request must carry `x-darcbench-session`; a request without it is `401` and the connection is closed |
| Probe with something that is not a well-formed request at all | ✅ On a gated origin a malformed or oversized head is also `401` and also counted. It used to answer `400`/`431` and count nothing, which told a scanner it had found an HTTP server and left no trace — and most scanner traffic is exactly that shape |
| Map which object sizes exist by reading status codes | ✅ The token gate runs *before* the body table, so an unauthenticated client cannot tell a configured size from an unconfigured one |
| Hold the origin's bounded connection slots while guessing | ✅ A refused request ends the connection; slots are bounded by `MAX_LIVE_CONNECTIONS` |
| Hold a slot indefinitely by trickling a head that never ends | ✅ `HEAD_DEADLINE` is checked on every pass of the read loop. It used to be checked only when a read would have blocked, so a peer producing one byte per poll window never reached it — about 26 minutes per slot, renewable, and needing no token because the head is never finished |
| Invalidate a competitor's run by port-scanning it | ✅ Unauthorised requests are counted in `Origin::refused`, never in `Origin::served`, so they cannot fail the reconciliation |
| Get the origin listening on a network the operator did not intend | ✅ A wildcard bind is refused, including the IPv4-mapped spelling `::ffff:0.0.0.0`, which `IpAddr::is_unspecified` does not recognise and a dual-stack kernel binds as `INADDR_ANY` |
| Start an unauthenticated external origin by omitting the token | ✅ `OriginError::ExternalWithoutToken`, checked before the listener is created |
| Leave a listener open indefinitely | ✅ `TargetSession::start` refuses a TTL outside `MIN_SESSION_SECS..=MAX_SESSION_SECS` (4 hours) and the session shuts the origin down when it expires. *This row read ✅ before either constant was referenced by anything; it is true now* |

**⚠️ Accepted residual risk.** The token travels in clear on a plaintext
origin, exactly as the dashboard's does. An operator on an untrusted network
should use the TLS origin or a private link. This is disclosed rather than
prevented, because the alternative — refusing to run without TLS — would
prevent the plaintext measurement the module exists to take.

---

### T-EXT-FRAUD — A dishonest external generator
**STRIDE:** Tampering · **Adversary:** a3 · **Severity:** medium ·
**New in Phase 3**

The external generator is a different machine reporting numbers about a machine
it does not own, and the headline figure is throughput — exactly what a
dishonest generator would inflate.

| Attack | Mitigation |
|---|---|
| Report more requests than were issued | ✅ `SessionReport::reconcile` rejects a successful count exceeding the origin's own `served`; a response the generator read is a response the origin sent |
| Request a size the origin does not serve and report the 404s as 1 MiB transfers | ✅ `served` counts only requests answered with a body. A 404 is 46 bytes and no work, and counting it would have let a fabricated byte-throughput reconcile perfectly against a machine that did essentially nothing |
| Sum shape counts past `u64::MAX` so the total wraps to something small | ✅ `claimed_requests` saturates |
| Report a run while a third party also loads the origin | ✅ A served count materially above everything the generator says it attempted is rejected — the surplus came from somewhere, and the numbers then describe the origin serving two clients |
| Hide work by discarding a phase and not reporting it | ✅ Warm-up is a reported field and counts toward the claim. The allowance is for requests in flight and nothing else; anything a generator issues and does not report is indistinguishable from a stranger, by design |
| Submit a report for a different session | ✅ Session id checked |
| Report a flattering latency distribution for requests it really did issue | ⚠️ **Not mitigated.** See below |
| Misreport the completed/failed split within a shape | ⚠️ **Not mitigated.** The two-sided bound constrains the sum, not the split |

**⚠️ Accepted residual risk, stated precisely.** Reconciliation proves how many
requests the origin answered with a body, and bounds the generator's claims
above and below by it. It does **not** prove their *timing*, and it does not
prove the split between completed and failed within a shape. A generator that
genuinely issued a million requests and then reported invented percentiles for
them would pass every check. This is why the mode is opt-in, why the generator's identity is
recorded in the bundle, and why an externally-generated result is trusted
exactly as far as the operator running both machines is — which, since both
machines are theirs, is the same trust T-SCORE-FRAUD already accepts for a
local run.

---

## 4. Residual risks, accepted

| Risk | Why accepted |
|---|---|
| Operator can fabricate results on their own machine | Unfixable without attestation; handled by verification tiers |
| Token in shell scrollback / proxy logs | Inherent to a printable bootstrap URL; mitigated by short-lived tokens |
| Benchmarking degrades a production host the operator chose to test | Legitimate use; made explicit and informed, never silent |
| Container-scoped results misread as host results | Detected, labelled and displayed; cannot be prevented, only disclosed |
| Uncalibrated scoring model | Disclosed everywhere; enforced by a test |

## 5. Reporting

`SECURITY.md` has the disclosure process. Security issues go to
`security@getdarc.com`, not to the public issue tracker.
