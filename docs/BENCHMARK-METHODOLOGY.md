# DARCBench benchmark methodology

What DARCBench measures, how, and what would make a result invalid.

---

## 1. Principles

**1. Variance is a result, not noise.** On shared infrastructure, run-to-run
spread is often the single most useful number a buyer can see. DARCBench
computes it, publishes it, and factors it into the Stability Score. Outliers are
*flagged*, never silently dropped — deleting a sample the hypervisor genuinely
produced turns a noisy-neighbour signal into a clean lie.

**2. Raw measurements are the product.** Scores are derived and recomputable.
Every repetition is retained in the bundle, in physical units.

**3. The observer is part of the system under test.** Telemetry sampling is
capped at 1 Hz, the UI coalesces sample events, and a unit test fails if the
sampler costs more than 25 ms per sample.

**4. Refuse rather than mislead.** A run that cannot produce a defensible number
must produce no number. `Partial`, `Invalid` and `Custom` exist so the suite
never has to round a broken run up to a score.

**5. Nothing is measured that cannot be explained.** Every metric has a unit, a
direction, a reference anchor and a documented workload.

---

## 2. Timing and repetitions

### Calibration

Iteration counts are calibrated **per machine** so that one repetition takes
approximately `target_rep_ms` (200 ms quick, 300 ms standard, 500 ms deep).

Fixing the iteration count instead would mean a fast server finishing a
repetition in a few milliseconds, where timer granularity, scheduler ticks and
interrupt coalescing are a large fraction of the measurement. Calibration starts
at one iteration and doubles until the measurement exceeds a quarter of the
target, then extrapolates linearly — so a slow or throttled machine is never
asked to do an enormous amount of work just to discover that it is slow.

A repetition shorter than **20 ms** is rejected with a `ValidationFailed`
warning: below that, the clock is measuring itself.

Because throughput (work ÷ time) is reported rather than elapsed time, differing
iteration counts across machines do not affect comparability. Within a single
run, the count is fixed across all repetitions of a metric.

### Warm-up and measurement

| Profile | Warm-up | Measured | Target rep | Passes |
|---|---:|---:|---:|---|
| quick | 1 | 5 | 200 ms | one |
| standard / web | 2 | 7 | 300 ms | one |
| deep | 3 | 11 | 500 ms | one |
| endurance | 1 | 5 | 200 ms | **cycles, for 60 min** |
| read-only | 1 | 5 | 200 ms | one |

Endurance repetitions are **per cycle**, and it is the only profile that repeats
its module set. Its counts are the smallest in the table on purpose — see
"Endurance" below.

Warm-up repetitions are **streamed to the UI** (so the operator sees activity)
and **retained in the bundle** flagged `warmup: true`, but excluded from every
statistic. Discarding them entirely would hide a machine whose first run is
pathologically slow — which is itself a finding on burstable instances.

Every profile measures at least 5 repetitions, asserted by
`every_profile_measures_enough_reps_for_a_median`.

### Statistics

Implemented in `darcbench-protocol::stats`.

- **Median** is the headline estimator. Benchmark distributions on shared
  infrastructure are right-skewed: one steal-time spike inflates a mean but
  barely moves a median.
- **Sample standard deviation** (Bessel-corrected) and **coefficient of
  variation** are always reported.
- **Confidence interval**: a non-parametric order-statistic (sign-test) interval
  for the median, computed only when `n ≥ 6`. Below that, no interval is
  reported rather than a meaningless one — which is why `quick` runs show `—` in
  the CI column, by design.
- **Outlier detection**: modified Z-score over the median absolute deviation,
  threshold 3.5. MAD rather than standard deviation, because standard deviation
  is itself dragged by the outlier it is supposed to detect.
- **Aggregation across metrics**: geometric mean, never arithmetic. See
  `docs/SCORING-SYSTEM.md`.

### Dynamic repetition (Phase 2)

Following the Phoronix Test Suite's approach
([phoronix-test-suite.com](https://www.phoronix-test-suite.com/?k=features),
accessed 2026-08-03), a module whose CV exceeds its declared bound will re-run
up to twice its base repetition count. DARCBench differs in one respect: the
final CV is still published. PTS uses the threshold to reach a stable number;
DARCBench uses it to reach a stable number *and then tells you how hard that
was*.

### Endurance: cycles, not longer repetitions · implemented

Every profile above makes one pass over its module set. `endurance` repeats the
set in **cycles** until a wall-clock target elapses — one hour by default — and
then compares the cycles with each other.

Cycling is not an optimisation of the single-pass design; it is the only design
that can measure what the profile is for. `docs/MARKET-RESEARCH.md` states the
problem directly: *"A 3-minute benchmark on a T-series instance measures the
credit balance, not the instance."* A credit balance accumulated over hours
takes tens of minutes of full load to spend, so no amount of measuring harder
inside a three-minute window can observe it.

**Per-cycle repetition counts are the smallest in the suite, deliberately.** The
instinct is the opposite — endurance is the thorough profile, so it should
measure hardest — and an earlier version did exactly that with 31 repetitions in
a single pass. That yields one very precise number and no curve: it could say
what a machine averaged over an hour while being unable to say that it halved at
minute forty, which is the finding. Five repetitions per cycle gives ten to
twenty points across the hour, and the last cycle still rests on exactly the
sample count `quick` publishes as a headline.

**The scored cycle is the last complete one.** Averaging burst throughput with
post-throttling throughput produces a number describing neither.

**Retention is drift, the coefficient of variation is noise.** Variation within
a cycle feeds the stability multiplier; variation across cycles that points one
way is the machine slowing down and is published separately as the Sustained
Performance Score. Formulae in `docs/SCORING-SYSTEM.md` §2.5.

**Attribution, with an explicit "don't know".** The decline is put next to the
telemetry taken while it happened, and the three causes separate because they
leave different traces — the distinguishing observation again from the market
research: burst-credit exhaustion *"is observed as high steal time, not as
reduced clock speed"*.

| Cause | Signature |
|---|---|
| Thermal / power throttling | Clock falls ≥5%, temperature usually rises |
| Burst credits exhausted | Steal rises ≥5 points, **clock unchanged** |
| Noisy neighbour | Steal high or swinging ≥3 points, no trend |
| Undiagnosed | Throughput fell, telemetry is silent |

The fourth row is not a fallback to be minimised. A classifier that always names
a cause is a guess wearing a measurement's authority, and the honest answer when
the evidence does not separate the hypotheses is to say so and point at the
remaining candidate — usually storage.

**Safety, for a profile that loads a machine for an hour:**
- The telemetry sampler doubles as the run watchdog, at 1 Hz.
- A hard runtime ceiling stops a run whose cycle stopped making progress. It
  sits well above the requested duration, because overshooting the final cycle
  boundary is ordinary and being killed for it would be a bug, not a guard.
- A thermal abort stops a run held at 100 °C for 30 consecutive seconds. It sits
  **above** the temperature at which a healthy machine throttles: throttling is
  the measurement, and guarding against it would destroy the finding. What the
  threshold catches is a machine whose own limiter is no longer keeping up,
  where the risk is to the components around the CPU that have less protection.
- Both record why the run stopped in the signed bundle. A run that ends early
  and cannot say why is indistinguishable from one the operator cancelled.
- `network.transfer` is excluded from the profile. Its transfer ceiling bounds
  what the suite pulls from a third party per run, and cycling it for an hour
  would either breach that bound or shrink each transfer until it measured
  nothing. Sustained load on somebody else's CDN is not ours to generate.

**Duration is fixed at one hour** because profile is the unit of comparability
and two endurance runs of different lengths have been given different amounts of
time to decline. An override exists and forces the run to `Custom`, exactly as a
hand-picked module list does.

---

## 3. Implemented module: `cpu.mixed` v1.0.0

**Safety class:** `ComputeIntensive` — saturates CPU, writes zero bytes, opens
zero sockets, spawns zero processes.

Five workloads × two shapes = 10 metrics.

| Workload | Unit | Represents |
|---|---|---|
| `crypto_sha256` | MiB/s | TLS, content hashing, integrity checks |
| `compress_deflate` | MiB/s | `Content-Encoding: gzip` on every response |
| `json_roundtrip` | ops/s | API serialisation and parsing |
| `integer_sort` | Melem/s | ORDER BY, index maintenance, log processing |
| `float_matmul` | MFLOP/s | Floating-point throughput, invisible to integer-only suites |

**Shapes.** `single` uses one thread and measures per-core performance — what
determines PHP request latency and the critical path of nearly every web
request. `multi` runs one independent copy per logical CPU, throughput style
(SPECrate-like), and measures aggregate capacity.

Reporting both separately is the point. A 32-vCPU shared instance and a 4-core
high-frequency machine can have identical multi-core throughput and feel
completely different to host a website on. A single blended number hides that.

**Scaling efficiency** = `multi / (single × threads)`, averaged over workloads.
Below ~0.5 on a plan advertising dedicated cores is a strong signal of SMT
saturation, thermal limits or oversubscription. Recorded in module context and
surfaced as a warning.

### Determinism

- All corpora come from a fixed-seed SplitMix64 generator (`CORPUS_SEED`), so
  the same DARCBench version measures the same bytes on every machine. A
  reference vector test pins the generator: changing it changes every corpus and
  requires a major workload version bump.
- Every result passes through `std::hint::black_box`. Without it LLVM is
  entitled to delete a workload whose output is unused, and the "benchmark"
  measures an empty loop. A test asserts each workload takes measurable time,
  and another asserts runtime scales with iteration count — a suspiciously flat
  scaling curve means the compiler hoisted the work.
- The compression corpus is asserted to compress to between 15% and 75% of its
  size. Incompressible input would make it a memcpy benchmark; all-zero input
  would make it a no-op.
- Release profile is pinned in the workspace `Cargo.toml`, and `bench` inherits
  `release`. A `debug`-built bundle is downgraded during validation.

### Declared limitations

Reproduced from the module manifest, and shown in reports:

1. Measures the CPU **as the operating system presents it**. Inside a container
   or a cgroup-limited VM, the result describes that sandbox, not the host.
2. No hand-written SIMD. It measures what a normal optimised program achieves,
   including whatever the compiler auto-vectorises — not peak theoretical
   throughput.
3. Multi-threaded shapes measure **throughput**, not parallel speed-up of a
   single problem, so they do not capture inter-core latency or lock contention.

---

## 4. Planned modules

Specified here so the methodology is complete; implementation status is in
`docs/ROADMAP.md`. Anchors for these are **not** shipped — a test forbids
shipping reference values for workloads nobody has run.

### Memory (Phase 2) · implemented

Sequential read/write/copy, random access, latency, single- and multi-threaded
bandwidth, NUMA effects where more than one node exists.

**The trap:** measuring the page cache and calling it memory bandwidth. Working
sets must exceed last-level cache by a documented multiple, buffers must be
touched before timing, and the cache topology captured in inventory is used to
size them.

**As implemented** (`memory.bandwidth@1.0.0`): the documented multiple is **4×
last-level cache per thread**, clamped to `[32 MiB, 256 MiB]` and then to 25% of
`MemAvailable` divided across threads — because a benchmark that pushes a live
host into swap is an outage *and* measures the swap device. When the budget
cannot afford at least 2× cache the run says so, and the result is downgraded
rather than published as a DRAM figure. Buffers are written once before any
timing, so page faults and NUMA first-touch placement are paid for outside the
measured region, and they are reused across repetitions so allocation never
lands inside one. Latency uses a Sattolo shuffle, which yields a single
full-length cycle: a Fisher-Yates shuffle can decompose into short cycles, and a
chase that falls into one sits in cache and reports a latency several times
better than the machine can deliver.

NUMA is currently **disclosed, not controlled**. Thread placement is left to the
operating system, so multi-threaded figures describe what an unpinned workload
gets; a run on a multi-node machine says so in its warnings. Binding threads to
nodes needs the privileged helper on the agent backlog.

### Storage (Phase 2) · implemented

Sequential and random 4K read/write, mixed workloads, multiple queue depths,
IOPS, bandwidth, mean and tail latency (p95/p99/p99.9), fsync latency, and a
database-like synchronous workload. Guided by the
[fio documentation](https://fio.readthedocs.io/en/latest/fio_doc.html)
(accessed 2026-08-03): `direct=1` to bypass the page cache, a ramp period before
recording, realistic queue depths rather than vanity depths of 256.

**Safety, non-negotiable:**
- Regular files only. **Never a raw block device**, ever, by any flag.
- Files created only under the DARCBench state directory, via the validated
  `StatePath` type.
- Free space checked before starting; the run is refused unless estimated writes
  plus a 2 GiB margin fit. Unknown free space is treated as unsafe, never as
  unlimited.
- Estimated bytes written shown before the run; SSD wear warning displayed.
- Cleanup on every exit path including cancellation and crash recovery.
- A read-only storage profile exists and is **not** treated as equivalent to a
  full storage score.

**Honesty requirement:** short random-write tests on consumer SSDs measure the
SLC cache, not the drive. Steady-state behaviour must be reported separately
from burst, and preconditioning limitations disclosed.

### Network (Phase 2) · implemented

Download single- and multi-stream, latency, jitter, DNS resolution, TCP connect,
TLS negotiation and TTFB, against several endpoints run by different operators.
The connection phases are timed **separately and never summed**, because they
fail for different reasons and have different fixes: slow DNS is a resolver
problem, slow connect is distance or routing, slow TLS is CPU or cipher choice,
and slow TTFB after all three is the far end.

Jitter is the variation **within** each path, not the spread between paths. The
distinction matters: endpoints sit at different distances, so a standard
deviation taken across them measures geography and would report a perfectly
steady link as jittery.

TLS session resumption is disabled. With it enabled only the first handshake of
a run is a full one, and the metric silently becomes the cost of an abbreviated
handshake — measured at 0.5 ms against a real 1.3 ms on the same path. The
number is meant to be what a *new* client pays.

**Honesty requirements:**
- One CDN endpoint does not represent universal network capacity, and the report
  says so.
- Loopback throughput, datacenter internet throughput, provider routing quality
  and application-layer HTTP throughput are four different measurements and are
  never conflated.
- Remote endpoint, protocol, timestamp and limitations are recorded per
  measurement.
- Third-party test services are used within their published policies, with
  bounded transfer volumes. A benchmark suite must not become a traffic
  amplifier.

**Enforced rather than intended.** The bounded-volume requirement above is a
running total the module charges every read against — calibration, warm-ups and
measured repetitions alike — because a documented intention is not a ceiling. A
download the ceiling truncates yields *no* rate rather than a partial one: a
short body divided by a near-zero interval produces an arbitrarily large number,
and during development that path published 652,721 Mbit/s before the guard
existed.

**Not measured, and declared as such:**
- **Packet loss.** Needs ICMP or raw sockets, and therefore privileges this
  module does not take. Inferring it from TCP behaviour would be a guess wearing
  a precise name.
- **Upload.** Sending bulk data to a third party is a different traffic profile
  and needs an endpoint whose published purpose covers it.
- **IPv6 as a separate measurement.** Availability is detected and recorded;
  measuring both families would double the traffic for a number most operators
  cannot act on.

**Sizing is deliberately not calibrated.** Every other module grows its
repetition until it hits a duration target. This one cannot: calibrating would
mean a faster machine pulling more data from somebody else's service. Transfer
size is instead derived from the ceiling divided by the number of transfers the
profile will make, so a longer profile shrinks each transfer rather than
exceeding the bound.

### Web, PHP, Node.js (Phase 3)

Static objects at three sizes, keep-alive, TLS and plaintext, HTTP/1.1, HTTP/2,
HTTP/3 where available, compression, multiple concurrency levels.

**The load generator must not be the bottleneck.** Every HTTP module must:
- use an **open-model, constant-rate** generator and measure latency from when a
  request *should* have been sent, not when it was — the coordinated-omission
  correction described by Gil Tene in
  [wrk2](https://github.com/giltene/wrk2) (accessed 2026-08-03);
- record generator-side CPU utilisation, and emit `GeneratorSaturated` — which
  invalidates the result — if the injector runs out of headroom;
- support an external-generator mode for machines fast enough to outrun a
  local injector.

PHP runs must disclose the runtime (native, container, panel-managed, FPM,
Apache module, LiteSpeed), OPcache state, worker count and resource limits.
Node.js runs must separate dependency installation from compilation — package
download time is network measurement, not build performance.

### Databases and WordPress (Phase 4)

**An existing production database is never benchmarked.** DARCBench creates a
dedicated isolated instance and destroys only the resources it created.

Naming and claims: `TPC-C` and `tpmC` are registered trademarks of the TPC
Council and may only be used for audited results. Following the HammerDB
precedent ([hammerdb.com/docs/ch03s02.html](https://www.hammerdb.com/docs/ch03s02.html),
accessed 2026-08-03), any derived OLTP workload will carry a distinct name and a
distinct metric, and will never claim tpmC.

WordPress fixtures are deterministic and generated. Separate scores for Origin,
Cached, Database and Admin, because "WordPress performance" without a cache
disclosure is meaningless. The module verifies WordPress is actually installed
and returns expected content — benchmarking the installation wizard by accident
is a real failure mode in this space.

---

## 5. Environmental controls

Captured at run start and used both as context and as validation input:

| Signal | Why it matters |
|---|---|
| CPU governor and scaling driver | `powersave` on an idle machine can halve results |
| CPU steal time | Noisy neighbour, or burst credits exhausted on T-series |
| PSI (`/proc/pressure/*`) | Real contention signal the load average hides |
| CPU frequency drift | Thermal/power throttling, or credit exhaustion |
| Load average at start | Competing work invalidates the measurement |
| CPU used by anything but the agent, sampled throughout | The same competition, arriving *after* the operator agreed to the run |
| cgroup CPU and memory limits | Result describes the sandbox, not the host |
| Execution scope | Bare metal / VM / container, with evidence |
| Storage stack | RAID, LVM, ZFS, network-attached, virtio |
| Mitigations | Materially change syscall-heavy performance |

The second row is not a duplicate of the fifth, and the difference is the whole
reason it exists. A load average taken at start is a precondition the operator
accepts before anything runs. The per-sample figure is a *runtime* signal, and it
cannot be the load average: a benchmark saturates the machine on purpose, so
every healthy run would trip a rule written against total CPU use. It is
`/proc/stat` busy time minus the agent's own `/proc/self/stat` time — both in
USER_HZ jiffies, so the subtraction needs no conversion — which leaves exactly
the work the benchmark did not do.

The run watchdog acts on it in two tiers: 10% of the machine for 20 seconds
degrades the modules measured while it lasts, and 40% for 5 minutes stops the
run. Under container scope the guard is **not enforced**, and the run says so:
without a namespaced `/proc`, `/proc/stat` describes the host, so a correctly
behaving run on a shared machine would be aborted for other tenants' work.

Steal time deserves a specific note. On AWS T-series instances, high steal does
not (only) mean a noisy neighbour — it means the instance has exhausted its CPU
credits and is being throttled to its baseline
([AWS re:Post](https://repost.aws/questions/QU2Ss4A4MMSFarFCBgr3dWuQ/how-to-resolve-very-high-cpu-steal-time-on-a-t3-large-instance-without-using-dedicated-and-reserved-instances),
accessed 2026-08-03). DARCBench therefore reports steal as its own series and
tracks throughput drift across the run, rather than folding everything into one
"CPU used" figure that conceals it.

---

## 6. Invalidation rules

Implemented in `darcbench-report::validate`, run in the agent and again,
authoritatively, on the server.

A run is **`Invalid`** when:
- it was cancelled or interrupted;
- the bundle schema is incompatible with the validator;
- `finished_at` precedes `started_at` (clock anomaly);
- the signature does not verify (server-side);
- scores recomputed from the raw metrics do not match those in the bundle
  (server-side).

A run is **`Partial`** when:
- a required category has no data;
- a module failed, was skipped, or is `Degraded`;
- any metric's CV exceeds 0.25;
- required metadata is missing;
- the agent was a `debug` build.

A run is **`Custom`** when the profile is `Custom` or an explicit module list was
supplied. `Custom` is never rankable, whatever else is true.

Two properties make this hard to defeat:

1. **The server never trusts anything derived in an uploaded bundle.** It
   recomputes the whole score card and decides eligibility from *its own*
   result, comparing every field - not just the headline total. A bundle whose
   scoring model this server cannot recompute is `Invalid`, because a score
   nothing has checked is not a validated score.
2. **Scoring is a pure function of the raw metrics.** Editing a score without
   editing the metrics is caught by recomputation; editing the metrics breaks
   the signature. Both are demonstrated by the `verify` command.

---

## 7. What a signature does and does not prove

A valid signature proves the bundle was produced by an agent holding a
particular key and has not been edited since. It does **not** prove the numbers
are real: the operator controls the machine and could patch the agent.

That is why a locally-signed bundle can never exceed `SelfReported`. Higher
tiers require evidence the operator cannot produce alone — a server-issued
nonce, a run token, and an agent build hash matching a published release. See
`docs/adr/0008-result-verification.md`.

---

## 8. Reproducibility targets

Acceptable run-to-run coefficient of variation on an **idle, dedicated** machine:

| Module | Target CV | Warn above | Invalid above |
|---|---:|---:|---:|
| `cpu.mixed` | < 0.03 | 0.15 | 0.25 |
| `memory.*` | < 0.05 | 0.15 | 0.25 |
| `storage.*` | < 0.10 | 0.20 | 0.35 |
| `network.*` | < 0.15 | 0.30 | 0.50 |
| `web.*` (planned) | < 0.08 | 0.20 | 0.30 |

These are **targets to validate, not measurements**. They encode expected
physical stability per subsystem; Phase 2 calibration will replace them with
figures derived from real repeated runs, and any change will be recorded as a
scoring-model version bump.
