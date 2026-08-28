# Roadmap

Estimates are in **engineer-weeks (ew)** for one experienced full-stack engineer,
because no team size is known. Complexity is S/M/L/XL. No dates: they would be
fiction.

**Assumption stated up front:** these estimates assume access to at least three
physical DARC-REF-1-class machines and a small fleet of VPS instances across
providers for validation. Without that hardware, Phase 2 cannot complete and
every later phase inherits an uncalibrated scoring model.

---

## Phase 0 — Research and foundations · ✅ Complete

**Delivered:** market and competitor research with dated sources; benchmark
methodology; scoring model design; architecture; threat model; 10 ADRs;
repository standards; documentation suite.

**Exit criteria met:** every material decision recorded with alternatives and
tradeoffs; no contradictory technology choices across documents; the scoring
model is implemented and tested rather than only described.

---

## Phase 1 — Local vertical slice · ✅ Complete

**Delivered:** agent starts; system inventory; embedded browser UI with token
auth; `cpu.mixed` module; live SSE progress and telemetry; cancellation;
provisional and final scoring; signed JSON bundle; HTML report; NDJSON event
stream; `verify` with tamper detection; clean shutdown.

**Exit criteria met:** the full architecture is exercised end to end; verified in
a real browser; 179 tests pass.

**Known gap carried forward:** scoring is uncalibrated, and every artifact says
so.

---

## Phase 2 — Core system benchmarks · ⏳ In progress

**Goal:** a `standard` profile that produces a defensible total score.

| Deliverable | Complexity | Estimate |
|---|---|---|
| ✅ `memory.bandwidth` — sequential, random, latency, cache-sized working sets | M | 2 ew |
| ✅ `storage.mixed` — queue depths, tail latency, fsync, steady state | L | 4 ew |
| ✅ `network.transfer` — multi-endpoint, IPv4/IPv6, jitter, DNS/TCP/TLS/TTFB (loss deferred) | L | 3 ew |
| ✅ Endurance profile: throttling, burst-credit and noisy-neighbour detection | M | 2 ew |
| ✅ Watchdog, max runtime, thermal guard, load ceiling, transfer ceiling | M | 1.5 ew |
| ✅ SQLite run index, comparison queries, retention policy | M | 1.5 ew |
| **DARC-REF-1 calibration on physical hardware** | L | 3 ew + hardware |
| ✅ Radar / balance visualisation, run-to-run comparison in the UI | M | 2 ew |

**Delivered so far:** `memory.bandwidth@1.0.0` — thirteen metrics across seven
access patterns, working sets sized from the host's own cache topology at four
times last-level cache and capped at 25% of available memory, with a
cache-contamination downgrade when the budget cannot afford a credible one. The
Memory category now produces a score, so the balance visualisation has more
than one axis to draw.

Then `storage.mixed@1.0.0` — ten metrics: sequential and 4K random read/write
at queue depths 1 and 16, a 70/30 mixed shape, p99 tail latency and fsync cost,
against a regular file opened `O_DIRECT` so the result describes the device and
not the page cache. Every safety rule in BENCHMARK-METHODOLOGY.md is enforced
mechanically rather than by convention — see the module's own documentation.

**A deviation worth stating:** this deliverable listed "fio adapter", and fio's
*methodology* is what the module follows (`direct=1`, warm-up before recording,
realistic queue depths). It does not shell out to fio, because the product bible
makes a single static binary a hard requirement, and a storage module that only
worked where fio happened to be installed would leave the Storage category empty
on most hosts — the exact condition Phase 2 exists to escape. An fio adapter is
still worth adding as a cross-check and as a route to io_uring queue depths; it
is now an enrichment rather than the foundation.

Then `network.transfer@1.0.0` — seven metrics: DNS resolution, TCP connect,
connect jitter, TLS handshake, time to first byte, and download throughput over
one stream and over four. The four connection phases are timed separately and
never rolled into one number, because they fail for different reasons and have
different fixes. Latency and jitter are sampled across three operators so a
single provider's bad day shows up as spread rather than as the machine's own
latency, and jitter is computed *within* each path rather than across them —
across-path variance measures distance, not steadiness.

Three deliverable-level notes:

**Packet loss is deferred, not delivered.** Measuring it properly needs ICMP or
raw sockets, which need privileges this module does not take and `unsafe` this
workspace forbids. Inferring it from TCP behaviour would be a guess wearing a
precise name, so it is declared as not measured. It belongs with the privileged
helper already on the Agent backlog.

**IPv4/IPv6 is disclosed, not compared.** Whether the host has working IPv6 is
detected and recorded; measuring both families separately would double the
traffic sent to a third party for a number most operators cannot act on.

**The endpoint table is the security boundary.** T-DDOS in THREAT-MODEL.md is
permanent, so the hosts are a compile-time `const` table with a written
justification each, the volume is bounded by a ceiling enforced against a
running total rather than merely documented, and the `quick` profile excludes
the module entirely — the first run anyone makes on an unfamiliar server still
opens no outbound connection.

Then the **endurance profile**, which is a different kind of deliverable: not a
module, but a change to what a run *is*. Every other profile makes one pass over
its module set. Endurance repeats the set in **cycles** until a duration target
elapses — one hour by default — and then compares the cycles.

That comparison produces two outputs. The **Sustained Performance Score** says
how much of its opening performance the machine still had at the end, as a
geometric mean over every metric measured in both windows. The **diagnosis**
says why it lost it, and the three causes are distinguishable because they leave
different traces:

| Cause | Signature | What the buyer does about it |
|---|---|---|
| Thermal / power throttling | Clock falls, temperature rises | Fix the cooling; the silicon is fine |
| Burst credits exhausted | Steal rises, **clock does not** | Expect baseline; price the larger instance |
| Noisy neighbour | Steal high and erratic, no trend | Nothing on this plan fixes it |
| Undiagnosed | Throughput fell, telemetry is silent | Look at per-metric retention; probably the disk |

The fourth row is what keeps the other three honest. A classifier that always
names a cause is a guess wearing a measurement's authority.

Three decisions worth recording:

**Cycles are short, not long.** The instinct is that the thorough profile should
measure hardest, so endurance previously ran 31 repetitions in a single pass.
That produces one very precise number and no curve — it could say what a machine
averaged over an hour while being unable to say that it halved at minute forty,
which is the finding. Endurance now runs five repetitions per cycle, the same as
`quick`, and gets its value from having ten to twenty cycles instead.

**The scored cycle is the last one.** An endurance run publishes its category
scores from the final complete cycle rather than an average over the run.
Averaging burst and post-throttling throughput yields a number describing
neither, and the one an operator lives with is the second.

**`network.transfer` is excluded.** Its transfer ceiling bounds what the suite
pulls from a third party *per run*, and cycling it fifteen times would either
breach that bound or shrink each transfer until it measured nothing. Bandwidth
quotas over an hour are a real gap; closing it needs an endpoint whose operator
has agreed to an hour of traffic, which is a conversation rather than a code
change.

Long runs also brought the deliverable below it forward in part: the telemetry
sampler is now the run watchdog, enforcing a hard runtime ceiling and a thermal
abort. The thermal guard sits *above* the temperature at which a healthy machine
throttles, because throttling is the measurement — guarding against it would
destroy the finding the profile exists to produce.

That deliverable is now complete: the **runtime load ceiling** closes it. The
hard part is that the guard cannot use either of the two obvious signals. A
benchmark drives CPU use and the load average to their maxima by design, so a
rule written against them stops every healthy run on its first sample. The
sampler instead subtracts the agent's own consumption — `/proc/stat` and
`/proc/self/stat` count in the same USER_HZ jiffies — leaving exactly the work
the benchmark did not do.

Two tiers, because two situations are being separated. Sustained competition
**degrades** the modules measured while it lasts, keeping their numbers as
evidence while denying them the claim of being clean. Heavy competition over
five minutes **stops** the run: nothing measured under it describes the machine,
and the machine is evidently wanted for something else. And under container
scope the guard is declared absent rather than enforced, because `/proc/stat`
without a namespace describes the host — the same discipline as packet loss in
`network.transfer`, where firing on the wrong evidence would be worse than
declining to fire.

Then the **run index**. Phase 1 listed runs by scanning the directory and
parsing every `bundle.json` in full — a complete inventory, every metric and
every per-repetition sample — to read four fields, and `GET /api/v1/runs`
answered from memory, so a fresh `serve` reported zero runs next to five hundred
bundles on disk. Both now read one SQLite index, per ADR-0005.

The hierarchy that ADR sets is the design constraint: *"bundles are the source
of truth in both modes; a database is an index over them, never the only copy."*
So the index is disposable. `reconcile` rebuilds it from the bundles at every
startup, indexing what it does not know and forgetting runs whose directory has
gone, and an index that will not open at all degrades to an in-memory one rather
than stopping the agent. A benchmark result that survived being measured must
not be lost to a cache of its metadata.

It also answers the question the directory scan could not: **comparison**.
`darcbench compare A B` and `GET /api/v1/runs/{a}/compare/{b}` line two runs up
metric by metric without opening either bundle. Ratios are direction-adjusted —
above 1.0 always means better, so a doubled fsync latency reads as a regression
rather than a doubling — metrics only one run has are *named* rather than
dropped, and anything that makes the two non-comparable (different machine,
profile, scoring model or agent build) is stated on the comparison rather than
used to refuse it.

**Retention is a command, not a sweep**, and it never deletes an `Invalid` run.
`darcbench prune` reports what it would remove unless given `--confirm`, and
refuses to run at all without an explicit policy: a prune that deletes
everything when told nothing is the wrong default for an operation with no undo.
DATA-MODEL.md is the reason invalid runs are exempt — the reason a run failed is
often more informative than the run succeeding would have been.

And the **balance visualisation**: a category radar in the local console, so the
shape of a machine — fast CPU, slow disk — is visible rather than being a number
in a table. It is the visual counterpart of `balance_index` and of the weak-link
cap, and it is hand-rolled SVG because the agent's CSP is `script-src 'self'`
and a benchmark's dashboard must not become a measurable load on the machine
under test. Fewer than three categories draws no polygon and says why, rather
than rendering a degenerate shape.

Beside it, the **comparison view**: pick two runs from the history, see them
lined up metric by metric. It fetches on demand rather than deriving anything
from the event stream, for the same reason the telemetry is coalesced — a
console that recomputes on every frame becomes a measurable load on the machine
it is measuring. Two things it must never do are handled as content rather than
as edge cases: a `comparable: false` label is rendered *above* the numbers and
never withholds them, and metrics that could not be lined up are listed rather
than dropped.

With that, every Phase 2 deliverable except calibration is closed.

**Still open in this phase:** NUMA-*aware* memory measurement, as distinct from
NUMA-*disclosed*. Binding threads to nodes needs the privileged helper on the
Agent backlog; until then the multi-threaded figures describe default
first-touch placement and say so. Storage has no SSD preconditioning pass: the
steady-state ratio makes burst behaviour visible, which is the affordable half
of that problem. Network measures download only: upload needs an endpoint whose
published purpose covers being sent bulk data, which is a conversation with an
operator rather than a coding task.

**Dependencies:** storage and network modules need the safety guards first.
Calibration needs every Phase 2 module complete.

**Risks:** calibration hardware access is the critical path. Storage behaviour
varies across kernel versions, filesystems and whether `O_DIRECT` is honoured at
all — mitigated by recording the full storage stack in the environment snapshot,
recording per-run which I/O mode was actually used, and treating cross-machine
storage comparison as conditional on both.

**Parallelisable:** memory, storage and network modules are independent of each
other.

**Exit criteria:** `standard` produces a `Validated`-eligible total; scoring
model reaches `dbs/1.0.0` with `calibrated: true`; a repeated run on identical
hardware lands within the CV targets in BENCHMARK-METHODOLOGY.md.

**Where the phase stands.** Every deliverable except calibration is now closed.
Calibration is not close to closing, and no amount of further coding moves it:
it needs three physical DARC-REF-1-class machines, which is the assumption
stated at the top of this document. Until then `dbs/0.1.0-dev` reports itself
uncalibrated in every bundle, report and API response, and none of the exit
criteria above can be met — a `Validated`-eligible total requires a calibrated
model by definition.

That is worth stating plainly rather than letting a column of ticks imply
otherwise. The measurements are real and the machinery around them is finished;
the numbers derived from them are not yet comparable with anything.

**Estimate: ~19 ew.**

---

## Phase 3 — Web workloads

**Goal:** measure what a web server actually does.

| Deliverable | Complexity | Estimate |
|---|---|---|
| ✅ Load generator + saturation detection harness | M | 2 ew |
| ✅ `web.static` — object sizes, keep-alive, TLS, HTTP/1.1 *(HTTP/2, /3 and compression declared unmeasured)* | L | 3 ew |
| ✅ `php.runtime` — framework-free, JSON, hashing, templating, OPcache disclosure | M | 2.5 ew |
| ✅ `node.runtime` — API, SSR, async I/O, build (download excluded) | M | 2.5 ew |
| ✅ External load-generator mode — `web-target` / `web-drive`, consent-gated, reconciled *(its result is a JSON report, not yet a scored bundle)* | M | 1.5 ew |
| ✅ Optional reverse-proxy integration: generate, preview, validate, back up, roll back | L | 3 ew |

**Delivered so far:** the load generator, `darcbench-modules::loadgen`. Every
HTTP module drives its target through it rather than writing its own request
loop, for the same reason the measurement harness owns calibration: what it
encodes is measurement policy, not workload detail.

Two decisions in it are worth reading before the modules that use it, and both
are recorded in [ADR-0012](adr/0012-load-generation.md).

**The model is open.** Request `i` is due at `start + i / rate`, whatever
happened to requests before it. A closed generator — a worker pool looping
"send, wait, send" — lets the target's own slowness reduce the load offered to
it, so the queue that would form in production never forms and the latencies
recorded are those of a server that was politely never overloaded. Every request
is therefore measured from when it was *due*, not from when it was sent; both
series are published, they are equal on an unsaturated system, and they diverge
exactly when queueing begins.

**Saturation is decided by the schedule, not by generator CPU.** The methodology
names CPU utilisation as the signal, and this is a deliberate strengthening of
it rather than a substitution. CPU is a proxy and it is wrong in both
directions: a generator whose connections are all waiting falls behind while
nearly idle, and one at its CPU ceiling can hold the schedule perfectly. Missing
the schedule is not a proxy for anything — if request 40,000 went out two
seconds late, nothing recorded after it describes the load the run claims to
have offered. CPU is still recorded, and it is what says *why*.

A deviation from the deliverable's wording, for the same reason as the fio one
in Phase 2: it says "load generator **selection**", and nothing was selected.
`wrk2` is the reference implementation of the correction above and would have
been the choice, but the single-static-binary requirement and the rule that no
module ever constructs a command line both rule out shelling out to anything.
So its methodology is implemented and its presence is not depended on.

Then `web.static@1.0.0` — seven metrics: small-object serving on a warm
connection, connection setup with and without a TLS handshake, throughput at
64 KiB and 1 MiB, and mean and 99th-percentile response time under load. The
Web category now produces a score, which is the last of the five a standard
total requires.

**The origin is DARCBench's own**, started on `127.0.0.1` on a port the OS
assigns and destroyed when the module returns. THREAT-MODEL.md's T-AMPLIFY makes
that permanent — *"HTTP load generation targets only a server the agent started
on loopback"* — but it is also the right measurement. A run against the
operator's nginx would measure their configuration; every machine running the
same server is what makes two machines comparable. It is the first module to use
`SafetyClass::ProvisionsServices`.

Two things it declines to measure. **HTTP/2 and HTTP/3** need protocol stacks
this build does not carry, and approximating them over HTTP/1.1 would be a guess
wearing a precise name. **Compression** would measure this machine's deflate
throughput, which `cpu.mixed` already measures and scores under Compute —
counting it again here would charge the same CPU to two categories.

**A finding that changed the design.** Capacity is measured by a tight
closed loop; latency needs the open model, at a rate below capacity. The
obvious rate — the conventional 70% headroom figure — turned out to be
unreachable, and not by a little. On loopback, serving a 1 KiB object costs
microseconds, so the generator's own per-request work (compute a due time, sleep
to it, record three timings) is *comparable to the work being measured*, and the
generator and the origin are competing for the same cores. Asking for 70% of
capacity asks one machine for about 170% of it.

So the local injector starts from a quarter and halves until it can hold the
schedule, and the share it actually offered is published in the bundle — the
latency figure says exactly what load it describes rather than implying one it
did not offer. Against an external generator the full 70% becomes reachable,
which is precisely what that deliverable is for.

Then `php.runtime@1.0.0` — seven metrics: JSON encode and decode, array
manipulation, HTML assembly, SHA-256, bcrypt password hashing, and interpreter
cold start. Framework-free, as the deliverable specifies: a framework benchmark
measures the framework's authors, and what a hosting buyer needs is what this
machine does with the handful of things every PHP application spends its time
on.

**It measures the operator's PHP**, which is the opposite of `web.static`'s
choice and right for the opposite reason. DARCBench does not ship a PHP and
could not; more to the point, "how will my site run here" is a question about
the PHP that is on the machine, with the extensions and limits that host set. So
comparability becomes conditional, and every run discloses the interpreter path,
version, SAPI, OPcache state and memory limit — the fields are in the manifest's
`comparability` list so the comparison layer can refuse on evidence rather than
on trust.

**This is the first module that executes a program the agent did not build**, and
that needed a decision rather than an implementation. THREAT-MODEL.md's T-CONFIG
had promised that discovered binaries are never executed; the new
[T-EXEC](THREAT-MODEL.md) and [ADR-0013](adr/0013-executing-a-discovered-runtime.md)
separate the two cases — discovery still executes nothing, and measurement now
may, under five constraints. The adversary is not the operator, who controls the
machine anyway, but **a2, a local unprivileged user**: shared hosting is the
common case in this market and the agent is frequently run as root, so a
compromised account that can write `/usr/local/bin/php` would otherwise get its
code run by root. A compile-time path allow-list, a safe-path check on the
binary *and every ancestor directory*, fixed argv, a cleared environment and a
hard timeout are what make that not happen.

**It is deliberately absent from the `standard` profile.** Most machines have no
PHP, and a standard run coming back `Partial` on every machine that is not a PHP
host would report the profile's own assumptions as a fault of the machine. It
runs in `web` and `deep`, which an operator selects because they want it.

**A defect this found in Phase 2 code.** The runtime load ceiling subtracts the
agent's own CPU from the machine's to see what else is competing — and it was
excluding reaped children, on the stated grounds that no module forks. That was
true when it was written and false the moment this module shipped: a module that
measures an interpreter does all its work in child processes, so the guard saw
the benchmark's own workload as somebody else's and degraded every PHP run.

Then `node.runtime@1.0.0` — seven metrics: JSON stringify and parse,
server-side rendering, SHA-256, event-loop file I/O, loading a 64-module
dependency tree, and process cold start. It shares the hardened execution layer
with `php.runtime` rather than copying it: the dangerous part is the same for
both, and a second copy is a second chance to get it wrong.

**"Build, download excluded" is satisfied by never downloading.** The
methodology requires dependency installation to be separated from compilation,
because package download time is a network measurement. `module.load` generates
its own tree and measures resolution, compilation and first execution with the
require cache cleared between iterations — so the download half of a cold start
is *structurally absent* rather than subtracted. An `npm install` benchmark
would measure a registry, a CDN and a lockfile resolver, which is
`network.transfer`'s question.

**Version-manager installs are refused, and this host proved it.** `nvm`, `fnm`
and `volta` put Node under `$HOME`, owned by an ordinary user. The machine this
was developed on has Node only at paths whose binaries are owned by a non-root
user, so the safe-path check refused every one of them — exactly as designed,
and a reminder that the check is not theoretical. The refusal is reported rather
than silent, so an operator learns why instead of seeing an empty result.

That left the measurement path with nothing to exercise it, so `run` splits into
discovery-then-measure and the tests drive `measure` against whatever Node the
host has. The security boundary stays on the production path unconditionally;
what the tests bypass is *which* binary, never how it is invoked.

Then the **external load-generator mode**, which exists because of the finding
above: a local injector and the origin compete for the same cores, and no amount
of tuning fixes one machine being asked for 170% of itself. Two halves of it are
`darcbench web-target --bind <ip>` on the machine being measured, one ticket
carried by hand, `darcbench web-drive --ticket <...>` on the machine generating
the load. **What it produces is a reconciled JSON report under
`$DARCBENCH_HOME/external/`, not yet a scored bundle** — the measurement, the
consent and the anti-fabrication checks are done; folding the numbers into a
`web.static` result with a score is the remaining step and the table says so.

Three decisions in it are worth reading before the rest.

**T-AMPLIFY's wording changed, and this is the first time it has.** It read
"only a server the agent started *on loopback*", and the external mode makes the
loopback clause literally false. The substance is unchanged: the generator takes
a host and a port, and before it offers a single request of load the peer must
return a `SessionOffer` carrying a recognised protocol version, having accepted
a 256-bit token an operator carried between machines by hand. A stranger's web
server cannot produce one. What that gives is **consent, not access control**,
and the threat model now says so in those words — nothing can stop somebody
writing their own load generator; what this stops is this binary being usable as
one, and an operator pointing a benchmark at production by accident.

**The target does not believe the generator.** An external generator is a
different machine, run by whoever holds the token, reporting a throughput figure
about a machine it does not own — which is precisely the number a dishonest one
would inflate. So the origin counts what it actually answered and
`SessionReport::reconcile` checks the claim against it, in both directions: more
claimed than served is impossible without lying, and more served than claimed
means a third party was also loading the origin, which makes the measurement a
measurement of two clients. The bound is an absolute allowance for requests in
flight, not a percentage, because a percentage would grow with the run and let a
long one hide a large lie.

What that does *not* prove is stated in the threat model rather than left to be
discovered: it proves the count of requests, not their timing. A generator that
really issued a million requests and invented the percentiles would pass.

**The offer is carried by hand, not fetched.** The obvious design gives the
target a `GET /session` endpoint the generator calls with its token — and that
endpoint is a thing on the network that answers questions about the session,
which can be probed and timed and whose errors distinguish "wrong token" from
"no session here". The offer is neither secret nor large, and the operator was
already going to carry a secret between two machines, so it rides along in the
same string. That removes the endpoint entirely, and leaves one that accepts one
document at the end of the run. It also makes a whole class of mistake
impossible: a generator cannot be pointed at an address it was not handed an
offer for, because there is nowhere to ask for one.

**A defect it found in Phase 3 code.** The external driver offers a fixed rate
to every shape, where `web.static` derives its rate from measured capacity — so
it drove the 64 KiB and 1 MiB shapes far harder than anything had before, and
3,834 requests out of 4,000 failed with "response headers exceeded 8 KiB". The
client's response reader checked that bound *before* looking for the end of the
headers, so a single 16 KiB read that delivered a small head plus thousands of
bytes of body counted the body as header and failed the request. It was never
external-only: it depended on how much of a response one `read` happened to
return, so on loopback it was a rare failure and at volume it was every request.
Fixed, and 1 MiB throughput on this host went from 105 req/s to 2,999.

**Risks:** the injector outrunning the target on fast machines — mitigated by
generator-side CPU accounting and `GeneratorSaturated` invalidation. The
external mode is the first benchmark listener DARCBench opens beyond loopback,
which is [T-EXPOSE](THREAT-MODEL.md); it is refused without a token, refused on
a wildcard bind, and its `401`s are counted apart from its `200`s so a passing
port scanner cannot invalidate somebody's run. Reverse-proxy integration touches
customer configuration and must stay strictly opt-in with a tested rollback.

Finally the **reverse-proxy integration**, `darcbench proxy`, which is the only
part of this program that writes anywhere near the operator's configuration —
on a machine that is probably serving customers right now.

It is delivered under a stronger rule than the deliverable asked for:
**DARCBench never writes a byte into a path the web server reads.** The
generated snippet goes to `/etc/nginx/darcbench-location.conf`, which nothing
scans; it is inert until the operator adds one `include` line inside the server
block they choose. This program does not write that line, does not know which
file it went into, and cannot remove it.

**That rule was learned rather than designed.** The first version wrote
`/etc/nginx/conf.d/darcbench.conf`, which is the obvious choice and is wrong:
`conf.d` is included at nginx's *http* level, so a bare `location` there is a
syntax error, and the first run against a real nginx said so. The safety
machinery did its job — the file was removed and the server left exactly as it
was — but the fix is not a better template. A program that never stages
anything live cannot stage anything broken.

One consequence falls straight out of it: because the file is inert, "could not
be validated" stops being a reason to delete it. `apply` still removes a
snippet its isolated check rejects, for a different reason — handing an
operator a broken fragment to `include` is a trap even if it sits inert until
they spring it.

**Rollback refuses to break the server.** Removing the snippet while something
still includes it guarantees an outage at the next reload, which may be days
later, for an unrelated reason, done by somebody who has never heard of
DARCBench. The config tree is searched for references first and a live one
stops the rollback, naming the file and the line.

The whole lifecycle is verified against nginx 1.24 rather than asserted:
apply, isolated check, the operator's include, `nginx -t`, a *refused*
rollback, the line removed, a successful rollback, and a server byte-for-byte
as it started. [ADR-0014](adr/0014-reverse-proxy-integration.md) records it,
including the design that was wrong.

**The cost, stated.** It is not one command. The operator edits one line by
hand, and a setup guide that says "then edit your vhost" is a worse product
experience than one that says "done". Accepted: the alternative is a program
that edits production web server configurations, and no benchmark result is
worth that.

**Exit criteria:** a saturated generator provably invalidates a result; PHP and
Node runtime configuration is disclosed in every bundle.

**Both halves are now met.** PHP discloses path, version, SAPI, OPcache state,
memory limit and build flags; Node discloses path, Node and V8 versions, libuv,
architecture, the jitless flag and the heap limit. Both sets are in their
manifests' `comparability` lists, so the comparison layer can refuse rather than
mislead.

The saturation half is pinned by a test. Proving it needs a target that
cooperates by being exactly slow enough, which no real server does — so the
generator is written against a `LoadTarget` trait rather than an HTTP client,
and the test drives a synthetic target whose latency it dictates.

**Estimate: ~14.5 ew.**

---

## Phase 4 — Databases and CMS

| Deliverable | Complexity | Estimate |
|---|---|---|
| ✅ Container-based module isolation tier — discovery, argument boundary, reaping, launch and readiness, exercised against a real daemon | M | 2 ew |
| ✅ `database.oltp` — PostgreSQL, read-only and read-write, durability disclosed, registered and scored. *MariaDB/MySQL not delivered* | L | 4 ew |
| ✅ `database.cache` — Valkey; throughput and unloaded round-trip, registered and scored. *No latency-under-load metric* | S | 1 ew |
| ✅ WordPress fixture generator (deterministic content) — WXR, checksum-pinned | M | 2 ew |
| ✅ `wordpress.site` — cold, warm, archive and admin, on a pinned WordPress + MariaDB stack, with cache disclosure. Registered and scored | L | 3 ew |
| ✅ `deployment.container` — build (cached and uncached), image write-out and extraction, startup and health. Registered and scored | M | 2 ew |

**Delivered so far:** the **container isolation tier**,
`darcbench-modules::container`. It is what makes "never touch a production
database" enforceable rather than promised: a database module gets a container
this agent started, or it gets nothing.

Three decisions in it are worth reading.

**There is no fallback path, anywhere in the type.** Every way of failing to
get a container is a reason to report a module as *not measured*. A
`database.oltp` that quietly measured the operator's production MySQL because
no container runtime was available would be the single worst thing this program
could do, so `ContainerError` has no variant that leads anywhere but a refusal,
and a test reads the operator-facing text to confirm none of them offers one.

**The argument vector is a pure function, because it is the isolation.** Every
dangerous thing a container can do — a `-v /:/host` bind mount, a `--privileged`
flag, a port published on a routable address — is something that would have to
appear in that vector. So `run_args` takes constants and a run id and returns
`Vec<String>`, and the tests read the whole vector rather than the code that
builds it. That also makes the boundary provable on a machine with no container
runtime at all, which matters: most CI has none.

What it asserts: no host path, ever; ports on `127.0.0.1` only; the data
directory is a tmpfs so nothing is written to a host disk and every run starts
from an identical empty database; every capability dropped and
`no-new-privileges` set; memory, swap and pid ceilings so a runaway container
cannot take down the machine being benchmarked; and environment values that
cannot become flags, since each follows its own `--env`.

**Images are pinned by digest, and `Image` has no public constructor.** The
allow-list is the same shape as `network_endpoints`' host table — compile-time,
justified per entry, unreachable from configuration or an HTTP request. A tag
is a mutable pointer, so a tag-pinned benchmark measures whatever the publisher
pushed last and two runs a month apart are not comparable even though nothing
in DARCBench changed.

**The table was empty for two commits, and that was not an oversight.** A
digest has to be resolved against a real registry rather than invented, and the
machine the tier was written on had neither a daemon nor registry access. The
two entries were pinned on 2026-08-14 against Docker Hub, with the date
recorded beside each: a digest is a fact about a moment, and a reader six
months out needs to know which moment.

**What the tier cannot guarantee, stated rather than hidden.** The release
profile aborts on panic and nothing runs on `SIGKILL`, so a run that dies
leaves its container behind; `--rm` reaps when the container exits, not when
the agent does. The mitigation is `reap`, which removes containers carrying
this agent's label — label-scoped rather than name-scoped, so no coincidence in
an operator's naming can put one of theirs in range. A module calls it before
starting, which bounds the cost of a crash to "until the next run".

**Validated first by a host with no daemon.** That machine had the Docker
client at a root-owned path and nothing behind it, so `Runtime::discover` took
the branch that matters: *"/usr/bin/docker is installed but its daemon did not
answer … The module is reported as not measured; nothing on this host was used
instead."* Discovery and daemon reachability are separate failures precisely
because an operator acts on them differently — one is a thing to install, the
other a thing to start.

### What the first real daemon changed

`Sandbox::launch`, `wait_ready`, `exec` and `Drop` first ran against a daemon
on 2026-08-14. `docs/DEVELOPMENT-HOST.md` said to expect that step to find
defects and to treat a clean first run as suspicious. It found five, three of
them in code that had been reviewed and tested and was wrong anyway. They are
recorded here because each is a *kind* of mistake rather than a typo, and the
kinds recur.

**A container hardened to the point of not starting.** With `--cap-drop ALL`,
the official PostgreSQL entrypoint dies on its second line: it starts as root,
`chown`s `PGDATA`, and `gosu`s down to `postgres`. The documented fix — the one
the image's own README gives — is to hand back `CHOWN`, `DAC_OVERRIDE`,
`FOWNER`, `SETUID` and `SETGID`, which is close to the whole interesting
surface of a container escape, granted so that a process could hold privileges
long enough to drop them.

The fix taken instead was to start the container *as* the image's service
account, so the privileges are never held rather than held and surrendered. The
uid and gid became part of the allow-list entry, which is defensible precisely
because the digest is pinned: they are facts about a specific image. The tmpfs
then has to arrive already owned, since a non-root container cannot fix it —
`mode=0700,uid=,gid=`, where Docker's default is root-owned and `1777`, which
PostgreSQL refuses outright and which is worth refusing anyway.

**A readiness check that was sound reasoning and worthless in practice.** It was
a TCP connect from the host to the published port, on the argument that it is
the one signal every service has in common. But Docker publishes a port with a
userland proxy, and that proxy listens when the *container* starts, not when
the service does. Measured here: at 0.5 s the host port accepted connections
and `pg_isready` still said no. `database.oltp` was handed a sandbox declared
ready and failed 0.8 s later with `connection refused`.

`database.cache` passed the same broken check every time, because Valkey starts
in about a tenth of a second and won the race. That is the part worth keeping:
a green result from an unsound check is not evidence, and the two modules
differed only in how fast their service happened to start. Each image now
brings its own probe — `pg_isready`, `redis-cli ping` — run inside the
container.

**`--rm` deleted the evidence.** A container that dies during startup is the one
whose log *is* the diagnosis, and `--rm` removes it before anything can read
it. The capability failure above was invisible on the first attempt for exactly
this reason and appeared the moment the flag came off. `wait_ready` now watches
for the exit, fails in about the time the container took to fail rather than
after the full ninety-second timeout, and puts the container's own last words
in the error.

**The memory limit and the tmpfs were one budget written as two.** Both were
`512m`, which reads as half a gig of disk and half a gig of RAM. A tmpfs lives
in the page cache and its pages are charged to the cgroup that faults them in,
so they were the same half gigabyte. `database.oltp` built a 324 MiB dataset
and was OOM-killed part-way through its write phase.

What it published from that is the part that matters. Not an error: two
throughput figures, one of them from a phase whose backends had been killed
half-way through — pgbench divides what it completed by the window it was
asked for, so the number looked ordinary — and silence about the other four
metrics, because the latency loop had a bare `continue` where the throughput
loop had a warning. So the module reported a plausible wrong number and an
unexplained absence, which is the worst combination available. All three are
fixed: the ceiling is computed from the tmpfs plus a measured service
allowance, a phase that lost clients is refused rather than published, and the
latency loop warns.

**And one flag that was simply not accepted.** `--progress 0` reads as "no
progress reports" and pgbench rejects it: `-P/--progress must be in range
1..2147483647`. Every phase of every run failed with a usage error before a
single transaction. The argument vector was correct by inspection; it just was
not one pgbench accepts. Likewise `redis-cli --latency` prints
`min: 0, max: 3, avg: 0.09 (1234 samples)` to a terminal and `0 2 0.23 471` to
a pipe — and this program only ever captures a pipe, so the parser written from
the documentation returned zero samples on every run on every machine.

Both were caught by the modules' own validity checks rather than published as
zeroes, which is the design working. But a metric that is *always* withheld is
not a working metric, and no fixture would have found either: a fixture written
from the documentation agrees with a parser written from the documentation.
Both tests now carry output captured from the real tool.

**And a sixth, which only registering them exposed.** The first
`darcbench run` over the two modules reported `database.oltp` **degraded** on
an idle machine: *"work other than this benchmark used 100% of the machine's
CPU."* The work was pgbench, in the container the module had just started.

The runtime load ceiling asks what is competing with the benchmark by
subtracting this process's own CPU from the machine's. `/proc/self/stat` sums
every thread of this process *and its reaped children*, which is why
`php.runtime` is counted correctly despite doing all its work in forks — and
that attribution was itself a defect this project already found and fixed once,
recorded under Phase 2 above. A container is different in kind: it is started
by a daemon and is not this process's child at any remove, so nothing it burns
can ever be attributed to this run, and the subtraction leaves the benchmark's
own workload sitting in the "somebody else" column.

The ceiling is therefore **suspended** while such a module runs, and the bundle
says so. A module declares the condition itself —
`workload_runs_outside_this_process` — rather than the agent inferring it from
a safety class, because `web.static` also provisions a service and its origin
*is* in this process. The thermal guard and the hard runtime ceiling are
untouched: both read the machine directly and neither depends on attributing
CPU to anything.

Attributing the container's cgroup back to the run would be better than
suspending the guard, and it is not something the agent can do without learning
what a container is. Until then this is the same choice the guard already makes
under container scope and that `network.transfer` makes about packet loss: a
guard that fires on the wrong evidence is worse than one that declares itself
absent.

**A seventh, found by deleting an image.** All three container modules declared
`max_network_bytes: 0`, and `database.oltp`'s comment even stated the
assumption — "the image is pulled by the container runtime before the run" —
with nothing making it true. `docker run` on an absent image pulls it. So on
any machine that had never run DARCBench, that module fetched **156 MB** while
preflight told the operator the run used no network at all.

And the pull landed *inside* a measurement. With the base image removed,
`deployment.container`'s startup figure came back with a **147% coefficient of
variation**: six repetitions of a container start and one of a container start
plus a download. The freshly-added variance sweep caught it, which is the sweep
working — but a metric that needs a warning to be interpretable is one measured
wrong.

The fetch is now explicit, before any clock starts, and reported in the bundle
as `image_fetched_during_this_run`. Each allow-list entry carries what it costs
to download, and the three manifests declare it. With the image absent the
coefficient of variation is 3.8% instead of 147%.

This one is worth separating from the other six because it was found a
different way. The first six came from running code that had never run; this
came from running code that had already worked, on a machine deliberately put
back into the state a new one would be in. **A benchmark that has only ever
been run twice on the same host has not been run on a second host.**

**A tenth, and it invalidated something this document said.** The roadmap and a
commit message both claimed that `deployment.container` "records the runtime so
the comparison layer can refuse on evidence". It does not and it could not:
`comparability_gaps` compares run-level facts only — environment digest, scoring
model, profile, build target, agent version — and never reads a module's
`comparability` list at all.

Checking that turned up a bigger thing. The list is the mechanism by which a
reader knows when a difference is the machine and when it is the measurement,
and an audit of a live bundle found most of its entries resolved to nothing:
`cpu.mixed` declared `params.threads` and recorded `threads`; `memory.bandwidth`
declared `cpu.architecture` where the inventory puts it under `platform`;
`database.oltp` declared `postgres_image` and recorded no image at all; and
`storage.mixed` declared `storage.filesystem` while **nothing anywhere in the
bundle recorded which filesystem the fixture was written on** — the single fact
this document names as the mitigation for storage results not being comparable
across machines.

So the list is now resolved against the bundle when the bundle is written, by
dotted path through the module's context, the inventory and the run's own
metadata, and `RunRecord.comparability_not_recorded` names whatever does not
resolve. The same choice `ScoreCard::unreferenced_metrics` makes about a metric
with no anchor: surfaced rather than dropped. Five keys were unresolved when the
check first ran; all five are now recorded, including `storage_driver`, which is
the fact that decides whether two `deployment.container` results describe
comparable machines at all.

**The pattern across the first six is worth naming**, because it is the argument
for the development-host document existing at all. Not one was a logic error.
Every one was a correct-looking piece of code meeting a fact about the world
that no amount of reading it would have supplied: what an entrypoint does with
root, when a userland proxy starts listening, where tmpfs pages are charged,
what a tool prints to a pipe, and which processes a `/proc` file counts.

Then **`database.oltp@1.0.0`** — six metrics: select-only and read-write
throughput, and the mean and an estimated 95th-percentile latency for each.

**It has no configuration for a host, a port, a socket or a credential, and no
code path that could use one.** It asks the container tier for a sandboxed
PostgreSQL and measures that, or it reports itself as not measured. There is no
"fall back to a local server", no `PGHOST`. A `database.oltp` that quietly
measured the operator's production database — and, being read-write, *wrote to
it* — would be the worst thing this program could do. The absence of that path
is the mitigation; nothing in the module validates its way to safety.

**The measurement is taken by `pgbench`, inside the container.** Two
alternatives were rejected: a database driver in this workspace (all pure Rust,
so none breaks the static-binary rule, but each would make the module measure
*that driver's* protocol implementation as much as the server), and writing the
PostgreSQL wire protocol here (feasible, and it would put a few hundred lines
of never-tested-against-a-real-server protocol code between the benchmark and
the truth). `pgbench` is written by the people who write the server. What that
costs is stated rather than buried: pgbench shares the container, so its CPU is
CPU the server did not get, and every figure is a floor.

**The latency phases use `--rate` and the throughput phases do not.** Without
it `pgbench` is closed-loop and offered load falls when the server slows — the
coordinated-omission defect ADR-0012 exists to prevent. With it, each
transaction is measured from when it was *due*. The throughput phases saturate
deliberately, because "how many can this machine do" has no schedule to slip
against. A latency phase that falls behind its own schedule emits
`GeneratorSaturated`, exactly as `web.static` does.

**TPC naming discipline.** `pgbench`'s banner says `TPC-B (sort of)` and its
documentation says *loosely based on*. The module never repeats that without
qualification: the title and purpose contain no "TPC" at all, one limitation
names it and immediately says this is not TPC-B and may not be compared to a
published TPC figure, and a test asserts both. That is how an unauditable
number acquires an auditable name, and it is cheap to prevent.

**Durability is disclosed and never changed.** `fsync`, `synchronous_commit`
and `wal_level` run at the image defaults and are recorded, because turning
them off multiplies write throughput and publishes a number no production
system can reproduce. That the data directory is a tmpfs is itself disclosed:
the WAL is written and fsynced, but to RAM, so these figures describe the
server and the CPU rather than the disk. `storage.mixed` measures the disk, and
measuring it again here would put the same device in two categories.

**One thing is deliberately not delivered, and the module says so in its own
limitations rather than leaving it to be discovered: MariaDB and MySQL.** No
open-model load tool ships in their official images. `mysqlslap` is
closed-loop, and reporting its latencies would be publishing the numbers of a
server that was politely never overloaded — the exact defect Phase 3 spent a
deliverable fixing.

**A registry entry** was the other, and it is now delivered. The module was
withheld from `registry::builtin` while its image was `Pin::Pending`, because a
registered module that cannot run puts a guaranteed precondition failure into
every profile containing it. With the digest pinned it is registered, in `deep`
only.

Then **`database.cache@1.0.0`** — six metrics: GET, SET and INCR throughput,
the same GETs pipelined sixteen deep, and the mean and worst round-trip on an
idle server.

**It reports no latency-under-load figure, and that is the decision the module
rests on.** `redis-benchmark` is closed-loop: each client sends a command,
waits for the reply, sends the next. There is no `--rate`, no scheduled
arrival, no equivalent of `pgbench`'s throttled mode — so when the server slows
the generator slows with it, the queue that would form in production never
forms, and the recorded latencies belong to a server that was politely never
overloaded.

`redis-benchmark` *does* print `p50`, `p95` and `p99` columns. They are trivial
to parse and would look authoritative in a report. They are read by nothing and
published nowhere, because a percentile taken under a closed loop is a
percentile of a different experiment — and printing one beside `web.static`'s
coordinated-omission-corrected figures would invite exactly the comparison that
makes it wrong. An honest gap beats a confident number from the wrong
experiment.

What is reported instead is throughput under saturation, which a closed loop
measures *correctly* because "how many operations can this machine do" has no
schedule to fall behind; and round-trip latency on an idle server, which says
`unloaded` in its own metric key so it cannot be read as a distribution. A test
asserts that no metric key in the module contains `p50`, `p95`, `p99` or a bare
`latency`, and that the gap is a declared limitation.

**Valkey rather than Redis**, because it is the fork the major distributions
and cloud providers moved to after the 2024 licence change and its image is
BSD-licensed throughout. Measuring both would double the runtime to measure two
servers that are, at this workload, the same program — a choice stated in the
manifest rather than left to be inferred from an absence.

**The pipelined phase earns its place by being a difference.** Sixteen deep
against the same GETs unpipelined is how much of a cache's cost is syscalls and
round-trips rather than the data structure, which is the difference between
"buy a faster machine" and "batch your calls".

Persistence — `appendonly`, `save`, `maxmemory-policy`, `io-threads` — is
disclosed and never changed, for the same reason `database.oltp` records
`fsync`. Like it, this module was withheld from the registry until its digest
was pinned, and like it, it is now registered in `deep`.

Then the **WordPress fixture generator**, which is what makes `wordpress.*`
comparable at all. A WordPress serving twelve posts and one serving twelve
hundred are different programs at runtime — different query plans, different
object cache pressure, different theme loop costs — so two machines are only
comparable if they served the same content.

**A generator rather than a shipped file**, for three reasons: a file can drift,
be edited or fail to ship; a file large enough to matter bloats a single static
binary meant to be downloaded onto a production server; and a generator can be
*proved* identical across machines by a checksum where a file can only be
assumed identical.

**The checksum is pinned by a test, and that test is the deliverable.** Change
one word, one count, one draw order, and it fails with the new hash and an
instruction: bump `FIXTURE_VERSION` **first**, then update the hash, in that
order and never the other way round. Moving the hash without the version would
silently make every historical `wordpress.*` comparison wrong while every
artifact still claimed the same fixture. The fixture's content is part of the
workload definition, and a workload definition that changes silently is the
failure this whole project is organised against.

**Output is WXR**, WordPress's own documented export format, not SQL and not a
thousand `wp post create` calls. SQL binds the fixture to a schema version that
WordPress changes; a thousand CLI invocations would measure process startup a
thousand times. `wp import` reads WXR, so the fixture arrives through the same
code path an operator's own migration would use.

**Everything generated is inert by construction, not by filtering.** This
content is imported into a CMS and rendered into HTML, so a generator that could
emit markup would be writing a stored-XSS payload into every machine that ran
the benchmark. Every string comes from a fixed list of lowercase ASCII words and
from integers, and *the module takes no input at all* — no parameter, no
configuration, no path — so there is nothing to inject through. On top of that,
the CDATA writer splits `]]>` and the XML escaper covers the single-quoted
attribute case, and both are tested against strings the generator cannot
currently produce, precisely so a future word list cannot quietly make them
reachable.

Three content decisions worth recording. The comment tree has **depth**, because
a flat list lets a theme's comment loop look cheaper than it is. A third of
posts have **no comments at all**, because a corpus where every post is
commented hides the case an operator actually has. And dates are derived from an
index rather than the clock — the wall clock is the one input that would make
every run's fixture different and every checksum useless.

Then **`wordpress.site@1.0.0`** — four metrics, and the module this whole
product is aimed at. `docs/MARKET-RESEARCH.md` names WordPress hosting as the
segment; every other module measures a *component* of that, and this one
measures the thing an operator is actually buying. A cold first request, a warm
homepage, a query-heavy category archive, and the authenticated admin
dashboard, against a WordPress and MariaDB the agent starts and destroys.

**It is the only module that runs somebody else's application**, and it is a
deliberate exception rather than a drift. Everywhere else DARCBench supplies
the workload, because a run against the operator's software measures their
configuration. Here the question *is* "how will WordPress run on this machine",
and there is no proxy for WordPress that answers it.

**Cache disclosure is the deliverable, not a footnote.** The methodology is
blunt: *"WordPress performance without a cache disclosure is meaningless."* So
the module installs no page cache and no object cache, says so in the bundle,
and names its metrics so they cannot be misread — `origin.cold` and
`origin.warm` are explicitly **not** a cached-versus-uncached pair. What differs
between them is PHP's opcode cache and the database's buffer pool; both are
pages WordPress built from scratch. Installing a page cache would need a plugin
downloaded from wordpress.org at run time, which is the unpinned dependency the
image allow-list exists to refuse.

**The fixture goes in through WordPress's own API**, and choosing that took
rejecting three alternatives. `wp import` needs the WordPress Importer, which is
a plugin and therefore a run-time download. Per-item `wp post create` is three
hundred interpreter start-ups. Direct SQL would put this workspace in the
business of knowing WordPress's schema — term relationships, comment counts,
slug uniqueness — and getting one wrong renders a site that is not the fixture.
So: one generated PHP script, piped to `wp eval-file -`, calling
`wp_insert_post` and `wp_insert_comment`. Those are the functions the importer
itself calls, and WordPress does its own sanitising.

The whole corpus travels as **one JSON document inside one PHP single-quoted
string**, so there is exactly one escaping problem rather than one per field —
and the script reports back how many posts and comments it inserted plus the
fixture's own checksum, all three of which must match before anything is timed.

**Two containers is new**, and the tier grew a private per-run network for it.
That network is deliberately *not* `--internal`: an internal network has no port
publishing, so the web server was unreachable from the process meant to measure
it. The outbound block moved up to `WP_HTTP_BLOCK_EXTERNAL`, WordPress's own
switch, which is a better control anyway — it stops the update check that would
actually have happened rather than stopping packets and hoping.

**Port 80 without root** is `net.ipv4.ip_unprivileged_port_start=0`, a
namespaced sysctl, rather than `--cap-add NET_BIND_SERVICE`. The same trade the
PostgreSQL entry makes: remove the need for the privilege instead of granting
it.

Two defects this found in itself, both by being run:

**Two comparability keys were false.** `php_version` and `opcache` were asked of
the WP-CLI container, which is a *different image* — so PHP came back 8.3.33
where Apache was running 8.3.31, and opcache came back `disabled` because
`opcache.enable_cli` is 0 while `opcache.enable`, the one governing every
measured request, is 1. Both are keys the comparison layer decides on. A
comparison refused or allowed on a false fact is worse than one with no fact.
They are now asked of the container that served the requests.

**`origin.warm` was not warm.** Warming each path immediately before timing it
meant the first steady-state metric was measured while the stack was still
climbing out of the cold start that had just been measured deliberately: 94 ms
at 64% variation, against an archive — a strictly heavier page, timed seconds
later — at 43 ms and 5.5%. A warm page slower and six times noisier than a heavy
one is a finding about the order, not the machine. One warm-up pass over every
path now precedes any timing: 42 ms at 10.7%, and correctly faster than the
archive.

**Capacity is measured too**, by `origin.capacity`, and it is the half a
latency figure cannot answer: a machine that renders in 40 ms and one that
renders in 40 ms while serving four times as many people are the same number
and different purchases.

Closed-loop, which is not a contradiction of everything this project says about
coordinated omission. Workers looping flat out ask the machine for everything it
will give and report what it gave — that is exactly the capacity question, and
the phase produces no latency distribution for the omission to distort. The
argument is `loadgen::measure_capacity`'s own.

**It runs last, and that placement is a precondition rather than an ordering
preference.** The phase saturates the machine; anything timed after it would be
timed against a stack still working through the queue it left. The three
latency metrics are all taken first, on a quiet stack, which is what makes them
latency figures rather than latency-under-undeclared-load figures. Verified:
adding the phase left `origin.warm` at 41 ms, unchanged.

Then **`deployment.container@1.0.0`** — five metrics: a cold-cache build, the
same build warm, what the layer cache is worth as a ratio, and the rates for
writing an image out and reading it back.

**The base image is `scratch`, and that is what unblocks it.** Every other
Phase 4 module waits on a digest resolved against a registry; this one needs no
base image at all. The generated Dockerfile starts `FROM scratch` and copies
files the module wrote, and the build runs `--network none` so nothing *can* be
fetched.

That is not a workaround, it is the correct measurement. A build starting
`FROM node:22` would spend most of its time pulling and extracting somebody
else's layers, so the number would be a network measurement with a build
attached — and `network.transfer` measures the network directly, under a
bounded ceiling. What an operator wants here is the machine's own contribution:
reading a build context, writing layers, committing them to the storage driver,
reading them back.

**Startup and health are now delivered**, on a pinned BusyBox base: 877 KB, one
static multi-call binary, no init system. That is not a retreat from the
paragraph above but the same argument applied to a different question. The
build must not start `FROM` a real base image because that would measure a
registry; the startup measurement must start from *something*, and the right
something contributes as little of its own as a running container can. The
build stays `FROM scratch` regardless — a base image being available is not a
reason to put one in the build.

`startup.cold` is a foreground `run … true`: create, start, exec, exit, remove,
timed as a wall clock rather than by polling. `health.to_serving` runs
BusyBox's `httpd` and times until an **HTTP status line** comes back. Not a TCP
connect, for the reason the isolation tier learned the hard way: the runtime's
userland proxy accepts as soon as the container exists. The response is a 404,
because the document root is an empty tmpfs — filling it would need a host path
inside a container and this tier does not have one — and a 404 from a running
server is a response.

These two are also the module's only metrics with a distribution behind them,
and the contrast is deliberate. A build takes seconds, so one observation is
dominated by the work; a container start takes a few hundred milliseconds,
which is the same order as whatever else the daemon happened to be doing, so a
single sample is mostly noise. Seven and five repetitions respectively, with a
real coefficient of variation.

Which immediately falsified something. The manifest had declared a
`stability_cv_bound` since the module was written and nothing checked it —
harmless while every metric was a single observation with no coefficient of
variation to exceed anything, and an unkept promise the moment a distribution
existed. The variance sweep `network.transfer` arrived at the hard way is now
here too, over the metric list rather than inside one construction path.

**This is the one module that writes to a host filesystem it cannot put on a
tmpfs.** The storage driver is configured daemon-wide and is not this program's
to change — changing it would be T-CONFIG. So the write is bounded and
disclosed instead: `max_bytes_written` covers four copies of an 18 MiB context,
every image carries this agent's label, and images are removed on the way out
*and* by `reap_images` at the start of the next run if this one is killed.
Images are reaped before containers, because an image cannot be removed while a
container made from it exists.

**One new exception to a rule, stated rather than quietly taken.** `Runtime::build`
is the only place a host path enters an argument vector in the container tier,
and it is unavoidable: a build context *is* a directory. The property that
matters — that nothing from the host is ever visible inside a *running*
container — is untouched, because a context is copied into the build rather
than mounted, and the directory named is one the agent created under its own
scratch path. It is checked to be absolute rather than trusted to be, since a
relative path would resolve against the daemon's working directory rather than
this process's.

**A test caught a test.** `the_dockerfile_pulls_nothing_and_runs_nothing`
originally counted the substring `FROM ` and found three, because the
Dockerfile's comment block explains *why* it starts `FROM scratch` rather than
`FROM` a real base image. It now parses directives — non-empty, non-comment
lines — which is what Docker does, and asserts every directive is one of the
two this module emits.

**Risks:** never touching a production database is an absolute requirement —
enforced by creating isolated instances only, and by prefixing every created
object with the run id. TPC naming discipline applies (see
COMPETITIVE-ANALYSIS.md).

**Exit criteria:** every database module creates and destroys its own instance;
a WordPress run verifies the installation before recording anything.

**Estimate: ~14 ew.**

---

## Phase 5 — Control plane

| Deliverable | Complexity | Estimate |
|---|---|---|
| Accounts, organisations, teams, RBAC | L | 3 ew |
| Agent registration and enrolment | M | 2 ew |
| Resumable bundle upload, object storage | M | 2 ew |
| Server-side validation and rescoring jobs | M | 2 ew |
| Historical views and comparisons | L | 3 ew |
| Remote run initiation (agent-pull, never server-push) | M | 2 ew |
| OpenAPI document, API keys, rate limits, audit log | M | 2 ew |

**Exit criteria:** a bundle uploaded from an agent is independently rescored
server-side; remote runs work without any inbound connection to the agent.

**Estimate: ~16 ew.**

---

## Phase 6 — Verification and leaderboards

| Deliverable | Complexity | Estimate |
|---|---|---|
| Server-issued nonce + run token (anti-replay) | M | 2 ew |
| Agent build hash attestation against published releases | M | 1.5 ew |
| Verification tier promotion pipeline | M | 2 ew |
| Public share pages, opt-in, redaction-enforced | M | 2 ew |
| Provider / plan taxonomy | M | 2 ew |
| Community leaderboards partitioned by model version and scope | L | 3 ew |
| Dispute process and score-manipulation handling | S | 1 ew |

**Risks:** leaderboards create the incentive to cheat that Phases 1–5 were built
to resist. Do not ship leaderboards before nonce and attestation.

**The public site is designed in [PUBLIC-RESULTS.md](PUBLIC-RESULTS.md)**, which
carries the submission endpoint, its abuse model and the tier ladder that
decides what a rank is worth. It also records why the explanatory half of
`darcbench.com` can ship now and the leaderboard cannot: replay and attestation
are this phase, and calibration — a Phase 2 exit criterion — gates any number we
would have to stand behind publicly.

**Estimate: ~13.5 ew.**

---

## Phase 7 — Professional and fleet

Scheduled runs, fleet comparison, regression alerts, notifications, API access,
exports and integrations, white-label reports. **~12 ew.**

---

## Phase 8 — Ecosystem and stabilisation

Plugin SDK and signed third-party modules (never contributing to the official
total); arm64 release parity; Debian and RPM packages; container image; installer
integrations; SBOM, dependency audit, reproducible builds, artifact signing;
external security audit; public 1.0. **~14 ew.**

---

## Explicitly deferred

| Item | Why | Revisit |
|---|---|---|
| Windows Server support | Different measurement model entirely; no evidence of demand yet | After 1.0 |
| GPU benchmarking | Different product | Not planned |
| Synthetic transaction / uptime monitoring | Different product | Not planned |
| Cost-efficiency scoring | Needs a price the agent cannot know; risks compromising neutrality | Phase 7, user-supplied price only |
| Third-party modules in the official total | See ADR-0006 | Requires a curated signed registry |
| Generated TypeScript protocol types | Protocol still moving; CI parity check suffices | When the protocol stabilises |

## Critical path

```
Phase 2 modules ──► DARC-REF-1 calibration ──► dbs/1.0.0
                                                  │
                          Phase 3 web ────────────┤
                          Phase 4 db/CMS ─────────┤
                                                  ▼
                    Phase 5 control plane ──► Phase 6 verification ──► leaderboards
```

Calibration gates everything that claims comparability. Leaderboards must not
ship before anti-replay.

### Open questions the field corpora raised

Three bare-metal hosts, `quick` profile, recorded in
[FIELD-EVIDENCE.md](FIELD-EVIDENCE.md). These are decisions, not defects: each
one moves scores, and none of them is answered by running the calibration.

- **`crypto_sha256` weight.** It is ~7x faster on a CPU with the SHA
  extensions, which is a real property and also more than the spread between
  three machines a decade apart. It currently carries weight 1.0 among ten
  Compute metrics. Calibration will not change this, because DARC-REF-1 has the
  instruction. Options: leave it and disclose (today's behaviour, via
  `isa_dispatch`), reduce its weight, or split ISA-accelerated workloads into
  their own facet the way single- and multi-core are split.
- **An egress-free rankable profile.** `Quick` omits `network.transfer` on
  purpose, so every `quick` run is `Partial` and unrankable. That is the first
  thing a new user runs. Either the first run is not rankable - which is a
  defensible answer, said plainly - or there is a profile that is.
- **`scope: unknown` and rankability.** A host whose DMI reads `Default
  string` cannot be attested as bare metal. Whether it may still be ranked, and
  on what evidence, is unsettled.
- **`quick` is not comparable, and nothing says so.** Two `quick` runs on one
  machine moved the multi-core facet by 17.5% and Compute by 9.2%, where two
  `standard` runs on the same machine held everything inside 4.1%. Five
  repetitions against seven, on the most load-sensitive thing measured. The
  profiles were designed this way; what is missing is saying it where a user
  about to submit a score will read it, and deciding whether a `quick` run
  should be submittable at all.
- **A minimum-passes floor for `memory.bandwidth`.** On the largest host the
  calibrator sized one metric at a single pass per repetition, which leaves no
  internal averaging - and that host is the one whose memory module tripped the
  variance bound.
- **~~Robust stability judgement.~~ Done.** `Summary::is_unstable` requires the
  spread to be wide *and* the robust spread - a median absolute deviation,
  scaled so one bound judges both - to agree. Over five repeated runs on one
  host it clears 7 of 10 flags and adds none, including `triad.single` at a CV
  of 36% around a median the other six repetitions agreed on to 0.3%. The first
  attempt used `ci95` and only ever helped `deep`: that interval trims nothing
  at n = 6 or 7. What none of it clears is the real question below.
- **Is latency to somebody else's network a comparable measurement?**
  `ttfb.mean` on H1 runs 29-129 ms with no outlier and a median determined only
  to within 56%: genuinely irreproducible, honestly measured. It is anchored and
  scored, so it degrades `network.transfer`, which is in both `deep` and
  `standard` - so **this is what blocks rankability today**. Either it becomes
  diagnostic-only the way `tcp_connect.jitter` already is, or the Network
  category is scored on throughput alone and latency is reported beside it.
  Raising the bound until three hosts pass would be fitting a threshold to three
  measurements.
- **The Web anchors, as a group.** Measured 3-6x too low on three hosts, with
  `connections.tls` at 4.42x agreeing to within 0.13 across a decade of
  hardware. Calibration corrects most of it; `throughput.large` and
  `throughput.medium` also disagree *between* hosts, which is the loopback
  shape and needs the metric revisited rather than the anchor moved.
- **A missing interpreter: failure or declared absence?** Six of eleven modules
  measure software the host may not have, and a failed module makes the run
  `Partial`. `standard` already answers most of this by excluding them - it is
  the five required categories and nothing else, so it is the rankable profile
  for a bare server. What is left is narrower: `deep` on an unprovisioned host
  reports six failures for software nobody promised, and the calibration
  runbook asks for `deep`. Either the runbook provisions the software (it now
  says how) or `deep` distinguishes "absent" from "failed".
