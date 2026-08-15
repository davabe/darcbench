# Changelog

Format: [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
Versioning: [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Note that four things version independently — agent, event protocol, bundle
schema and scoring model. See [docs/RELEASE-STRATEGY.md](docs/RELEASE-STRATEGY.md).

## [Unreleased]

### Added

**The Database category produces a score** — `database.oltp` and
`database.cache` are registered
- Both modules were written, tested and unable to run: they need a container
  daemon and a registry, and the machine they were written on had neither. That
  block is cleared. The `postgres` and `valkey` digests were resolved against
  Docker Hub on **2026-08-14** and pinned, with the date recorded beside each in
  `container.rs` — a digest is a fact about a moment, and a reader six months
  out needs to know which moment.
- Registered in `registry::builtin`, anchored in `darcbench-scoring`,
  manifested under `benchmarks/database/`, and in the **`deep` profile only**.
  The argument is `php.runtime`'s: most machines in this market have no
  container runtime, and a standard run coming back `Partial` on every one of
  them would report the profile's own assumptions as a fault of the machine.
  They run last within `deep`, because their failure depends on a daemon rather
  than on this process.
- The anchors are the first in this file written with a measurement in front of
  them. They are still **declared targets, not calibration** — DARC-REF-1 is a
  machine nobody has run this on — but the ratios come from the workload rather
  than from an estimate of it, and the reasoning records which direction each
  extrapolation goes and why.

**`wordpress.site` — the module this product is aimed at**, and the last
unwritten Phase 4 deliverable
- Four metrics against a WordPress and MariaDB the agent starts and destroys: a
  cold first request, a warm homepage, a query-heavy category archive, and the
  authenticated admin dashboard. `docs/MARKET-RESEARCH.md` names WordPress
  hosting as the segment; every other module measures a *component* of that.
- **It is the only module that runs somebody else's application**, and that is
  an exception rather than a drift. Everywhere else DARCBench supplies the
  workload, because a run against the operator's software measures their
  configuration. Here the question *is* "how will WordPress run on this
  machine", and there is no proxy for WordPress that answers it.
- **Cache disclosure is the deliverable.** The methodology is blunt —
  *"WordPress performance without a cache disclosure is meaningless"* — so no
  page cache and no object cache are installed, the bundle says so, and
  `origin.cold`/`origin.warm` are named as explicitly **not** a
  cached-versus-uncached pair. What differs between them is PHP's opcode cache
  and the database's buffer pool; both are pages WordPress built from scratch.
- **The fixture goes in through WordPress's own API.** `wp import` needs the
  WordPress Importer, which is a plugin and therefore an unpinned download from
  wordpress.org at run time — the supply-chain dependency the image allow-list
  exists to refuse. Per-item `wp post create` would be three hundred interpreter
  start-ups; direct SQL would put this workspace in the business of knowing
  WordPress's schema. So: one generated PHP script piped to `wp eval-file -`,
  calling `wp_insert_post` and `wp_insert_comment` — the functions the importer
  itself calls. The corpus travels as one JSON document inside one PHP
  single-quoted string, so there is exactly one escaping problem rather than one
  per field, and the script reports back its counts and the fixture checksum,
  all of which must match before anything is timed.
- **Two containers is new**, and the tier grew a private per-run network,
  `--volumes-from` sharing, `--sysctl` support and stdin piping for it. Port 80
  without root is `net.ipv4.ip_unprivileged_port_start=0` rather than
  `--cap-add NET_BIND_SERVICE` — the same trade the PostgreSQL entry makes.
- Two defects it found in itself, both by being run: `php_version` and `opcache`
  were asked of the WP-CLI container, a *different image*, so PHP was reported
  as 8.3.33 where Apache ran 8.3.31 and opcache as `disabled` because
  `opcache.enable_cli` is 0 while `opcache.enable` is 1 — both comparability
  keys, and a comparison decided on a false fact is worse than one with no fact.
  And `origin.warm` was measured while the stack was still climbing out of the
  cold start, at 94 ms and 64% variation against a heavier page at 43 ms and
  5.5%; one warm-up pass over every path before timing any of them brought it to
  42 ms at 10.7%.
- **Capacity is not measured.** This is single-request latency; how many
  concurrent visitors the machine sustains needs the open-model load generator
  pointed at a stack that takes minutes to build.

**`deployment.container` is registered, and the Deployment category scores** —
startup and health delivered
- Its five build and image metrics had never run: they needed a daemon, not a
  base image, and there was no daemon. They ran clean, and `docker load` was
  checked by hand to confirm it re-extracts rather than recognising layers the
  build cache still holds — the `rmi` before it is doing its job.
- **`startup.cold` and `health.to_serving`** close the deliverable. The base is
  a pinned BusyBox: 877 KB, one static multi-call binary, no init system. That
  is not a retreat from the module's `FROM scratch` rule but the same argument
  applied to a different question — the build must not start from a real base
  image because that would measure a registry, and the startup measurement must
  start from *something*, so the right something contributes as little of its
  own as a running container can. The build is unchanged.
- `startup.cold` is a foreground run to completion, timed as a wall clock
  rather than by polling. `health.to_serving` times until an **HTTP status
  line** comes back, not until a TCP connect succeeds — the runtime's userland
  proxy accepts as soon as the container exists, which is the trap the
  isolation tier already learned once. The answer is a 404, because the
  document root is an empty tmpfs and filling it would need a host path inside
  a container.
- They are the module's only metrics with a distribution behind them, and the
  contrast is deliberate: a build takes seconds and is dominated by the work, a
  container start takes a few hundred milliseconds and a single sample is
  mostly whatever the daemon was doing.
- Which falsified something immediately. The manifest had declared a
  `stability_cv_bound` since the module was written and nothing checked it —
  harmless while every metric was a single observation, an unkept promise the
  moment a distribution existed. The variance sweep is now here too, over the
  metric list rather than inside one construction path.
- A second argument vector joins `run_args`: `ephemeral_run_args`, for a
  foreground one-shot container. Strictly more contained than the first —
  nothing published, nothing mounted, and `--network none` — and read by the
  same whole-vector tests, because a second vector is a second chance to get
  the isolation wrong.

### Fixed

**An undeclared 156 MB download, inside a measurement.** All three container
modules declared `max_network_bytes: 0`, and `database.oltp`'s comment stated
the assumption outright — "the image is pulled by the container runtime before
the run" — with nothing making it true.
- `docker run` on an absent image pulls it. So on any machine that had never run
  DARCBench, that module fetched 156 MB while preflight told the operator the
  run used no network at all. On a metered VPS that is somebody's money.
- And the pull landed inside the measurement. With the base image removed,
  `deployment.container`'s startup figure came back at a **147% coefficient of
  variation**: six repetitions of a container start and one of a container start
  plus a download. The variance sweep above caught it, which is the sweep
  working — but a metric that needs a warning to be interpretable is one
  measured wrong.
- `Runtime::ensure_image_present` now fetches explicitly, before any clock
  starts, and the bundle records `image_fetched_during_this_run`. Each
  allow-list entry carries its download size and the three manifests declare it.
  With the image absent the coefficient of variation is 3.8% instead of 147%.
- **Worth separating from the defects below**, because it was found a different
  way: those came from running code that had never run, this came from running
  code that already worked on a host deliberately put back into the state a new
  machine would be in. A benchmark that has only ever been run twice on the same
  host has not been run on a second host.

**Five defects that only a real container daemon could find.** `Sandbox::launch`
had never run. `docs/DEVELOPMENT-HOST.md` said to expect that step to find
defects and to treat a clean first run as suspicious; it was right on both
counts. Three of the five are properties of the isolation tier rather than of
PostgreSQL, so they would have been waiting for every image added after it.

- **A container hardened to the point of not starting.** With `--cap-drop ALL`
  the official PostgreSQL entrypoint dies on its second line: it starts as
  root, `chown`s `PGDATA`, and `gosu`s down to `postgres`. The documented fix —
  the one the image's own README gives — is to hand back `CHOWN`,
  `DAC_OVERRIDE`, `FOWNER`, `SETUID` and `SETGID`. That is close to the whole
  interesting surface of a container escape, granted so that a process can hold
  privileges long enough to drop them.

  So the container now starts **as the image's own service account** and the
  privileges are never held at all. The uid and gid are part of the allow-list
  entry, which is defensible precisely because the digest is pinned: they are
  facts about one specific image, and a test refuses an entry that would run as
  root. The tmpfs has to arrive already owned as a consequence — a non-root
  container cannot fix it — so the mount carries `mode=0700,uid=,gid=`, where
  Docker's default is root-owned and `1777`, which PostgreSQL refuses outright.

- **A readiness check that was sound reasoning and worthless in practice.** It
  was a TCP connect from the host to the published port, on the argument that
  it is the one signal every service has in common. But Docker publishes a port
  with a userland proxy, and that proxy listens when the *container* starts, not
  when the service does. Measured on this host: at 0.5 s the port accepted
  connections and `pg_isready` still said no. `database.oltp` was handed a
  sandbox declared ready and failed 0.8 s later with `connection refused`.

  `database.cache` passed the same broken check every time, because Valkey
  starts in about a tenth of a second and won the race. That is the part worth
  keeping: **a green result from an unsound check is not evidence**, and the two
  modules differed only in how fast their service happened to start. Each image
  now brings its own probe, run inside the container.

- **`--rm` deleted the evidence.** A container that dies during startup is the
  one whose log *is* the diagnosis, and `--rm` removes it before anything can
  read it — the capability failure above was invisible on the first attempt for
  exactly this reason. `wait_ready` now watches for the exit, so a failed
  container reports in about the time it took to fail rather than after the
  ninety-second timeout, and the error carries the container's own last words.
  Removal was always `Drop`'s job and `reap`'s; `--rm` never covered the case
  those two exist for.

- **The memory limit and the tmpfs were one budget written as two.** Both were
  `512m`, which reads as half a gig of disk and half a gig of RAM. A tmpfs lives
  in the page cache and its pages are charged to the cgroup that faults them in,
  so they were the same half gigabyte. `database.oltp` built a 324 MiB dataset
  and was OOM-killed part-way through its write phase.

  What it published from that is the part that matters. Not an error: two
  throughput figures — one from a phase whose backends had been killed half-way
  through, because pgbench divides what it completed by the window it was asked
  for and the number therefore looked ordinary — and silence about the other
  four metrics, because the latency loop had a bare `continue` where the
  throughput loop had a warning. A plausible wrong number next to an unexplained
  absence is the worst combination available. The ceiling is now computed from
  the tmpfs plus a measured service allowance, a phase that lost clients is
  refused rather than published, the latency loop warns, and both modules
  declare the footprint to preflight.

- **Two tools whose output is not their documentation.** `--progress 0` reads as
  "no progress reports" and pgbench rejects it — `-P/--progress must be in range
  1..2147483647` — so every phase of every run failed with a usage error before
  a single transaction. And `redis-cli --latency` prints
  `min: 0, max: 3, avg: 0.09 (1234 samples)` to a terminal but `0 2 0.23 471` to
  a pipe, which is the only thing this program ever captures, so the parser
  returned zero samples on every run on every machine.

  Both were caught by the modules' own validity checks rather than published as
  zeroes, which is the design working — but a metric that is *always* withheld
  is not a working metric. No fixture would have found either: a fixture written
  from the documentation agrees with a parser written from the documentation.
  Both tests now carry output captured from the real tool.

**The runtime load ceiling counted a module's own container as a competing
tenant.** The first `darcbench run` over the two modules reported
`database.oltp` degraded on an idle machine: *"work other than this benchmark
used 100% of the machine's CPU"* — the work being pgbench, in the container the
module had just started.
- The ceiling subtracts this process's own CPU from the machine's to see what
  else is competing. `/proc/self/stat` sums every thread of this process **and
  its reaped children**, which is why `php.runtime` is counted correctly despite
  doing all its work in forks — an attribution that was itself a defect found
  and fixed once already. A container is different in kind: started by a daemon,
  never this process's child, so nothing it burns can be attributed to the run
  and the subtraction leaves the benchmark's own workload in the "somebody else"
  column.
- The ceiling is now **suspended** while such a module runs, and the bundle
  discloses it — "the ceiling was not enforced" and "the ceiling was enforced
  and found nothing" are different claims. A module declares the condition
  itself, `workload_runs_outside_this_process`, rather than the agent inferring
  it from a safety class: `web.static` also provisions a service and its origin
  *is* in this process. The thermal guard and the hard runtime ceiling are
  untouched; neither depends on attributing CPU to anything.
- Attributing the container's cgroup back to the run would be better and needs
  the agent to learn what a container is. Until then this is the choice the
  guard already makes under container scope, and that `network.transfer` makes
  about packet loss: a guard that fires on the wrong evidence is worse than one
  that declares itself absent.

**A CPU test that could not pass on a small host** —
`multi_thread_shape_reports_more_total_throughput`
- It was documented as "tests flake under `-j` on a small VPS; re-run pinned to
  fewer cores". On a two-vCPU host that advice does not work: it failed under
  `--test-threads=2` and passed only in complete isolation, and it passed and
  failed on identical code minutes apart.
- It was a defect in the test. The test already refused to conclude anything
  when the machine was too *noisy* — but noise is a shape that cannot reproduce
  itself, and what happened was a shape that reproduced perfectly well while
  being handed one core, because another test had the other one. No amount of
  averaging separates "the threaded shape stopped being parallel", which is the
  defect worth failing on, from "the machine had no second core to give it".
- The precondition is now measured rather than assumed: CPU seconds burned over
  wall seconds elapsed, from `/proc/self/stat`, which is how many cores the
  process really got. Below 1.5 the test says so and asserts only that
  throughput is positive and finite. A test that cannot pass on a small host
  teaches people to ignore the suite, which costs more than the coverage is
  worth.

**A live terminal dashboard for `darcbench run`** — the CLI stops going silent
- `darcbench run` printed a two-line header and then nothing until the bundle
  was ready: minutes on `quick`, over an hour on `endurance`. The information
  was never missing. The run already emits a complete, ordered event stream for
  the browser dashboard; the terminal simply was not reading it.
  `crates/darcbench-agent/src/tui.rs` subscribes to that same stream, so the
  CLI and the web UI are now two renderings of one source of truth rather than
  two implementations of the same idea.
- Per-module progress, sparklines per metric, CPU/external/steal/memory/load
  telemetry, eased category scores, and `q` to cancel. Built on **ratatui**,
  pinned to 0.29: 0.30 raises its MSRV to 1.88 and the workspace promises 1.82.
  crossterm is used through ratatui's re-export rather than as a second direct
  dependency, so the two can never skew.
- **The redraw rate is a budget, not a preference.** This process is part of
  the system under test, which is why telemetry is capped at 1 Hz and why the
  web radar refuses to animate at all. Redraws happen on a fixed ~15 Hz tick and
  events are drained into the fold *inside* that tick, so render cost is
  independent of event rate — a faster machine emits more samples and must not
  therefore pay more for being watched. Off entirely under `--json`, when stdout
  is not a terminal, under `--no-color`, under `TERM=dumb`, and under the new
  `--no-tui`.
- **Plain progress for everywhere it cannot draw** (`follow.rs`). A redirect, a
  pipe or a CI log now gets one line per module transition and progress at
  quarter marks — not per sample, which on a `deep` profile would be thousands
  of lines burying the ones that matter.

**The static commands were given the same treatment** — `darcbench`, `doctor`,
`compare`, `status`
- Grouped into labelled sections with one label column shared across all of
  them, so every value on a screen lands in the same place. The wordmark is
  rendered identically to the live dashboard's header: the product should not
  change appearance depending on which of its own surfaces you are looking at.
- **`compare` sizes its columns from the data.** A `{:<34}` name column does not
  truncate, it just stops padding, so any metric key past that width shoved its
  own value columns right and the table stopped being a table —
  `memory.bandwidth/latency_random.single` is 38 characters and did exactly
  that. Widths are now the widest row.
- **`compare` prints the totals**, which the `--json` output has always carried
  and the human output never did. It is the first number anyone wants from a
  comparison.
- **`compare` colours the change column by sign only.** `MetricDelta::ratio` is
  direction-adjusted upstream, so green-for-positive stays correct on a
  lower-is-better metric: a latency that rose from 109 ns to 121 ns reads
  `-9.4%`. It is deliberately *not* a significance claim — two runs carry no
  confidence interval to make one from.
- **`doctor` wraps its findings** with a hanging indent under the message
  column. Several are written as prose and run past two hundred characters; flat
  they wrapped at the terminal edge back to column zero and stopped looking like
  one field. Wrapping is applied only when stdout is a terminal, so a pipe never
  receives newlines in the middle of a sentence somebody is about to `grep`.
- **Alignment is computed on visible text, not bytes** (`cli::pad`).
  `format!("{:<12}", coloured)` counts the escape sequence as characters, so a
  coloured cell got no padding at all and every column to its right drifted by a
  different amount per row.

**Score reveal animation in the web dashboard**, and the rule it obeys
- Category tiles and the total ease into place when a result lands. The rule is
  in `useMotionAllowed`: animation is suppressed while a run is in flight,
  because the page is open on the machine being measured and decoration during
  the measurement window is load on the thing under test. It is allowed once the
  run is idle and the numbers are final. `prefers-reduced-motion` is honoured on
  top of that, and only `opacity`/`transform` are animated so a reveal never
  triggers layout or paint.
- The one exception that animates during a run is the liveness dot on the run
  status, because a stalled agent and a quiet one are otherwise identical.
- Animated numbers are `aria-hidden` with the settled value exposed to assistive
  technology instead: an `aria-live` region announcing six hundred intermediate
  values is worse than not animating at all.

**The endurance profile** — the first profile that changes what a *run* is
- Every other profile makes one pass over its module set. `endurance` repeats
  the set in **cycles** until a wall-clock target elapses — one hour by default
  — and then compares the cycles.

  This is not a refinement of the single-pass design; it is the only design that
  can measure what the profile is for. `docs/MARKET-RESEARCH.md` puts it
  bluntly: *"a 3-minute benchmark on a T-series instance measures the credit
  balance, not the instance."* A balance accumulated over hours takes tens of
  minutes of full load to spend, and no amount of measuring harder inside three
  minutes can observe that.
- **Sustained Performance Score**: how much of its opening performance the
  machine still had at the end, as a geometric mean of per-metric retention over
  the opening and closing thirds of the run. Direction-adjusted, so a doubled
  fsync latency reads as retaining half rather than as retaining 200%. Capped at
  1000 for a machine that got *faster*, with the raw observation kept unclamped
  so the cap stays auditable.

  Absent — not 1.0 — for every profile that does not cycle. A run never given
  time to decline has not demonstrated that it would not.
- **A cause, not just a number.** The decline is put next to the telemetry taken
  while it happened, and the three explanations separate because they leave
  different traces. The distinguishing observation is again the market
  research's: burst-credit exhaustion is seen as *high steal time, not reduced
  clock speed*. So a falling clock is thermal or power throttling; rising steal
  with a steady clock is a credit balance running out; steal that is high and
  erratic without a trend is a noisy neighbour.

  The fourth outcome is `undiagnosed`, and it is not a fallback to be minimised.
  A classifier that always names a cause is a guess wearing a measurement's
  authority; when the evidence does not separate the hypotheses this says so and
  points at the remaining candidate.
- **Watchdog.** The 1 Hz telemetry sampler is now also the run watchdog, because
  an hour of full load on somebody else's server needs one. A hard runtime
  ceiling stops a run whose cycle stopped making progress, and a thermal abort
  stops a run held at 100 °C for thirty consecutive seconds. The thermal
  threshold sits deliberately **above** the temperature at which a healthy
  machine throttles: throttling is the measurement, and guarding against it
  would destroy the finding the profile exists to produce. Either abort records
  its reason in the signed bundle — a run that ends early and cannot say why is
  indistinguishable from one the operator cancelled.
- **The runtime load ceiling**, which completes the watchdog. Preflight already
  refused to *start* on a busy machine; what was missing was noticing that a
  machine became busy while a long run was in flight — someone deploying, a
  backup window, a cron job that only fires at 03:00.

  The difficulty is that a benchmark saturates the machine on purpose, so total
  CPU use and the load average both read "fully loaded" on a perfectly healthy
  run. The sampler therefore subtracts the agent's own consumption:
  `/proc/stat` counts the machine in USER_HZ jiffies and `/proc/self/stat`
  counts this process's threads in the same jiffies, so machine-busy minus
  self-busy is external work, in units that need no conversion and no assumption
  about the value of USER_HZ.

  Two tiers, because the two situations differ. Ten percent of the machine for
  twenty seconds **degrades** the modules measured while it lasts: the numbers
  stay in the bundle as evidence, and they stop being reported as clean.
  Forty percent for five minutes **stops** the run, because nothing measured
  under that much competition describes the machine, and the machine is
  evidently wanted for something else.

  Under container scope the guard is not enforced and says so, rather than
  firing on evidence it cannot trust: without a namespaced `/proc`,
  `/proc/stat` describes the host, so every other tenant would read as external
  load and a correctly-behaving run would be aborted for the machine merely
  being shared.
- `WarningCode::ExternalLoad`, which degrades a result where the existing
  `PreexistingLoad` does not. Pre-existing load is disclosed at preflight and
  accepted by the operator before anything runs; external load arrives
  afterwards, inside the window whose numbers are being published, and is
  measured against this process's own CPU accounting rather than inferred from a
  load average.
- `TelemetryEvent.cpu_external_busy_pct` and
  `TelemetrySummary.cpu_external_busy_pct_max`, both additive and defaulted. The
  live console and the HTML report both show it: a reader deciding whether to
  trust a degraded result needs to see how much competition there was, not only
  that there was some.
- `ModuleResult.cycle` and `RunRecord.stopped_because`, both additive and
  defaulted, so a bundle written before cycles existed still reads as the
  single-pass run it was.
- `duration_minutes` on `POST /api/v1/runs` and `--duration-minutes` on
  `darcbench run`. Both force the run to `Custom`, for the same reason a
  hand-picked module list does: two endurance runs of different lengths were
  given different amounts of time to decline.

**The open-model load generator** — Phase 3's foundation
- Every HTTP module will drive its target through `darcbench-modules::loadgen`
  rather than writing its own request loop, for the same reason the measurement
  harness owns calibration: what it encodes is measurement policy, not workload
  detail.
- **The model is open.** Request `i` is due at `start + i / rate`, whatever
  happened to requests before it. A closed generator — a worker pool looping
  "send, wait, send" — lets the target's own slowness reduce the load offered to
  it, so the queue that would form in production never forms and the latencies
  recorded belong to a server that was politely never overloaded. Every request
  is measured from when it was *due*, not from when it was sent; both series are
  published, they are equal on an unsaturated system, and they diverge exactly
  when queueing begins.
- **Saturation is decided by the schedule, not by generator CPU** — a deliberate
  strengthening of the methodology, recorded in
  [ADR-0012](docs/adr/0012-load-generation.md). CPU is a proxy and it is wrong
  in both directions: a generator whose connections are all waiting falls behind
  while nearly idle, and one at its CPU ceiling can hold the schedule perfectly.
  Missing the schedule is not a proxy for anything. CPU is still recorded, and
  it is what the warning uses to say *why*.
- Three verdicts, most specific first, because their remedies differ: every
  worker busy (raise the connection count), schedule slip (the injector is
  behind), rate shortfall (the target stopped answering). All emit
  `WarningCode::GeneratorSaturated`.
- Nothing was *selected*, though the deliverable says "load generator
  selection". `wrk2` is the reference implementation of this correction and
  would have been the choice; the single-static-binary requirement and the rule
  that no module ever constructs a command line rule out shelling out to
  anything. Same trade as fio in Phase 2.

**`web.static@1.0.0`** — the Web category now produces a score
- Seven metrics: small-object serving on a warm connection, connection setup
  with and without a TLS handshake, throughput at 64 KiB and 1 MiB, and mean and
  99th-percentile response time under load. Web was the last of the five
  categories a standard total requires.
- **The origin is DARCBench's own**, started on `127.0.0.1` on an OS-assigned
  port and destroyed when the module returns. T-AMPLIFY makes that permanent,
  and it is also the right measurement: a run against the operator's nginx would
  measure their configuration, and every machine running the same server is what
  makes two machines comparable. First use of
  `SafetyClass::ProvisionsServices`.
- Declined, and said so: **HTTP/2 and HTTP/3** need stacks this build does not
  carry; **compression** would measure deflate throughput that `cpu.mixed`
  already scores under Compute, charging the same CPU to two categories.
- **The 70% headroom figure turned out to be unreachable for a local injector**,
  and not by a little. On loopback, serving a 1 KiB object costs microseconds,
  so the generator's own per-request work is comparable to the work being
  measured — and both are competing for the same cores. Asking for 70% of
  capacity asks one machine for about 170% of it. The module now starts from a
  quarter, halves until the generator can hold its schedule, and **publishes the
  share it actually offered**, so the latency figure says exactly what load it
  describes.
- The generator's saturation detector grew a one-millisecond noise floor. Its
  tolerances were expressed relative to the inter-request period, which is right
  until the period gets small: at 18,000 requests a second half a period is 27
  microseconds, and no general-purpose scheduler places a thread that precisely.
  Without the floor it fired on the operating system's own timing granularity
  and declared a run saturated at under nine percent of the machine's capacity.
- The generator no longer takes a global lock per request. One shared
  `Mutex<Samples>` in the hot path meant that at loopback rates it spent more
  time contending than issuing — and then reported itself saturated, which was
  true and was its own fault.

**`php.runtime@1.0.0`** — the first module that executes a program the agent
did not build
- Seven metrics: JSON encode and decode, array manipulation, HTML assembly,
  SHA-256, bcrypt password hashing at a pinned cost of 8, and interpreter cold
  start. Framework-free, as the deliverable specifies.
- **It measures the operator's PHP**, the opposite of `web.static`'s choice and
  right for the opposite reason: "how will my site run here" is a question about
  the PHP that is on the machine. Comparability becomes conditional, so the
  interpreter path, version, SAPI, OPcache state and memory limit are disclosed
  in every bundle and named in `comparability`.
- **[ADR-0013](docs/adr/0013-executing-a-discovered-runtime.md) and a new
  T-EXEC** in the threat model. T-CONFIG had promised that discovered binaries
  are never executed; the two cases are now separated — discovery still executes
  nothing, measurement may. The adversary is `a2`, a local unprivileged user:
  shared hosting is common in this market and the agent often runs as root, so a
  compromised account that can write `/usr/local/bin/php` would otherwise get
  its code run by root. Five constraints: a compile-time path allow-list, a
  safe-path check on the binary *and every ancestor directory*, fixed argv with
  no shell, a **cleared** environment (`PHP_INI_SCAN_DIR` and `NODE_OPTIONS`
  each turn a variable into code execution, and a filter is a list of the ones
  somebody thought of), and a hard timeout. A binary that fails the check is
  refused *and reported* — it is a privilege-escalation path independently of
  this benchmark.
- **Absent from the `standard` profile by design.** Most machines have no PHP,
  and a standard run coming back `Partial` on every machine that is not a PHP
  host would report the profile's own assumptions as a fault of the machine.

**The run index** — SQLite over the bundles, as ADR-0005 specified
- Phase 1 listed runs by scanning `runs/` and parsing every `bundle.json` in
  full — a complete inventory, every metric and every per-repetition sample — to
  read four fields. `GET /api/v1/runs` did not even do that: it answered from
  memory, so a freshly started `serve` reported zero runs next to five hundred
  bundles on disk. Both now read one index.
- **The index is disposable, and that is the design.** ADR-0005: *"bundles are
  the source of truth in both modes; a database is an index over them, never the
  only copy."* `reconcile` runs at every startup — indexing bundles it does not
  know, forgetting runs whose directory has gone — and an index that will not
  open degrades to an in-memory one rather than stopping the agent. A result
  that survived being measured must not be lost to a cache of its metadata.
- `darcbench compare <a> <b>` and `GET /api/v1/runs/{a}/compare/{b}`: two runs
  lined up metric by metric without opening either bundle. Ratios are
  **direction-adjusted**, so above 1.0 always means better and a doubled fsync
  latency reads as a regression rather than a doubling. Metrics present in only
  one run are named rather than dropped — a comparison that silently ignores
  what it could not match looks complete while describing a subset. Anything
  making the two non-comparable (machine, profile, scoring model, agent build)
  is stated *on* the comparison rather than used to refuse it: comparing across
  a kernel upgrade is legitimate, and misreading the result is what has to be
  prevented.
- `darcbench prune`, the retention policy. It is a command and never a
  background sweep; it reports unless given `--confirm`; it refuses to run
  without an explicit policy, because a prune that deletes everything when told
  nothing is the wrong default for an operation with no undo; and it never
  removes an `Invalid` run, which DATA-MODEL.md requires and which it reports as
  retained so the exemption is visible rather than silent.
- `darcbench status --limit`, and richer JSON: profile, duration, scoring model,
  environment digest, bundle digest and module set per row.
- The one C dependency in the workspace, `rusqlite` with `bundled`. Deliberate
  and confined: ADR-0005 chose SQLite precisely because the amalgamation ships
  in the crate, so the agent keeps its single-static-binary promise on hosts
  with no `libsqlite3`.

**The run-to-run comparison view** — the console half of `darcbench compare`
- Pick two runs from the history, see them lined up metric by metric. The
  comparison already existed as a command and an endpoint; this is the same
  answer without needing a shell on the machine under test.
- Fetched on demand, never derived from the event stream, and behind a `memo`
  boundary whose only input changes at most twice per run. The dashboard
  re-renders once a second for the whole of a run, and a comparison recomputed
  on every telemetry frame would make the browser a measurable load on the
  machine it is measuring.
- `comparable: false` never withholds the comparison; every reason renders above
  the numbers, where it is read first. Metrics that could not be lined up are
  listed rather than dropped. Changes are rendered as a signed percentage with
  the word *better* or *worse* beside it, so a regression is readable with every
  colour rule deleted.

**The category radar** — the balance visualisation
- A hand-rolled inline SVG radar of the category scores, so the *shape* of a
  machine — fast CPU, slow disk — is visible rather than being a number in a
  table. It is the visual counterpart of `balance_index` and of the weak-link
  cap.
- Hand-rolled because the agent serves the console under `script-src 'self'`, so
  a charting library could not load; and because a dashboard that re-renders per
  telemetry sample would become a measurable load on the machine it is
  measuring. It renders on score events, not on the 1 Hz stream.
- Every axis carries its category name *and* its score as text, the SVG is
  `aria-hidden` with the ranked list beside it, and `prefers-contrast: more` is
  honoured — no meaning rests on colour or position. Fewer than three categories
  draws no polygon and says why, rather than rendering a degenerate shape.
- `balance_index` is derived in the browser from the category scores, mirroring
  `model.rs`, because the score event does not carry it. Noted as a client-side
  derivation wherever it appears.

### Fixed

- **`php_runtime`'s test fixture built a directory the module then refused.**
  `scratch()` created its temporary directory with `create_dir_all` and let the
  ambient umask decide the mode. Under umask 022 that is 0755 and the suite
  passes; under umask 002 — the default on Debian and Ubuntu with user-private
  groups — it is 0775, and the T-EXEC guard correctly refused a group-writable
  directory it was about to write and execute a script in. The guard was right
  and the test was wrong. `node_runtime.rs` already carried the fix, with a
  comment describing this exact problem; the twin helper in `php_runtime.rs`
  did not. Found by running the suite on a umask 002 host.

- **Five `scripts/e2e.sh` checks could not fail.** They were written
  `cmd && ok "label"`. Under `set -e` a failure inside a `&&` list does not exit
  the shell, and with no `|| bad` arm nothing is recorded at all — `pass` does
  not rise, `fail` stays at zero, and the closing `[ "$fail" -eq 0 ]` reports
  success for a run in which the assertion never held. `darcbench verify` was
  one of the five. They now go through a `try` helper that always lands on `ok`
  or `bad`, which is the discipline `pycheck` and `check` already followed.

- **The web dashboard re-rendered once per `module.sample` event.** The file's
  own docstring warns that this "would make the browser a measurable load on the
  machine being benchmarked, which would corrupt the very numbers on screen",
  and coalescing was described but never implemented: every sample dispatched
  its own reducer action. Events are now pooled and folded in one pass, capping
  re-renders at ~10/s however fast samples arrive. A timer rather than
  `requestAnimationFrame`, because rAF does not fire in a background tab and an
  operator who switched away would return to an unbounded queue.

Defects found by two audits of the Phase 2 code, none of which had a failing
test until they were found.

- **`storage.mixed` steady-state detection was direction-blind**, and the one
  write-shaped workload it gates is `latency_fsync.mean`, which is
  lower-is-better. A drive whose fsync latency degraded from 0.4 ms to 1.6 ms —
  precisely the SLC cache filling the check exists to catch — scored 4.0 and
  passed silently, while a drive that *warmed up* scored 0.25 and was degraded
  for improving, taking the whole run to `Partial`. The ratio is now
  direction-adjusted, as `darcbench-scoring::sustained` already was.
- **`ScoreCard.sustained` was never compared by server-side recomputation**,
  despite `scores_match` promising every field is. An endurance bundle could
  claim it retained 100% of its performance, be re-signed, recomputed, matched
  and marked `Validated` — the endurance profile's headline number, published
  unchecked. It is now compared field by field including per-metric retention,
  and a test destructures `ScoreCard` exhaustively so the next field added
  cannot go uncompared silently.
- **Scoring trusted the metric's own `direction` instead of the reference
  anchor's.** Relabelling `latency_fsync.mean` as higher-is-better made a 5 ms
  fsync normalise to 100× rather than 0.01, and the server reproduced the swing
  exactly because it read the same tampered field. The anchor is the scoring
  model's own data and is now what decides. This also catches a merely *buggy*
  agent mislabelling a metric.
- **`Fixture::create` could leave a multi-GiB file behind.** `Drop` is the whole
  cleanup mechanism and there is no `Fixture` to drop until after the fill
  succeeds — so an ENOSPC part-way through, which is what a co-tenant filling
  the disk mid-run looks like, left everything written so far until some later
  run's stale-fixture sweep. A guard now owns the path from the moment the file
  exists.
- **`network.transfer`'s cross-endpoint median was an upper median.** With an
  even endpoint count — which the table has by construction — connect times of
  2, 3, 40 and 41 ms returned 40 rather than 21.5, biasing every repetition
  toward the slowest endpoints and systematically understating a
  lower-is-better anchor.
- **`WarningCode::ExternalLoad` did not count toward `instability_flags`**, so a
  run contaminated throughout degraded every module while rendering as "no
  instability flags".
- **The load ceiling's absence under container scope was undisclosed.** A guard
  that never fired and a guard that was never armed produced identical bundles.
  `RunRecord.guards_not_enforced` now names it, and the HTML report shows it.
- **The signed bytes depended on a thread-local.** `Bundle::signable()`
  serialises the inventory, whose `Sensitive` fields consult an ambient
  redaction policy, so a bundle signed under one policy and verified under
  another would fail its own signature check. The policy is now pinned inside
  `signable`.
- **The run comparison trusted `metric.direction`** — the same field the scoring
  fix above had just removed from the scoring path, reintroduced one layer up. A
  bundle relabelling `latency_fsync.mean` as higher-is-better scored 0.01×
  correctly while `darcbench compare` rendered the same degradation as `+300%`,
  in the one table an operator reads to decide whether something regressed. The
  index now stores the reference anchor's direction.
- **The load ceiling's container carve-out failed open.** It was keyed on
  `Scope::Container`, but scope detection falls through to `BareMetal` whenever
  DMI is readable — which it is inside a container, because `/sys` is mounted.
  A containerd pod whose `/proc/1/cgroup` reads `0::/` was therefore labelled
  bare metal, and the guard would have aborted a correctly-behaving run for a
  co-tenant's work while the bundle claimed the ceiling was enforced. It now
  also disarms on positive evidence: a cgroup CPU quota below the CPU count
  `/proc/stat` reports.
- **Every index-derived run was reported as `state: "completed"`.** A run the
  watchdog stopped or an operator cancelled still writes a bundle, so after a
  restart it appeared in the API as having finished normally — erasing the exact
  distinction `stopped_because` exists to preserve. Both fields now come from
  the row.
- **`reconcile` never refreshed a run it already knew**, contradicting
  `record`'s own contract, so an edited bundle left the index serving a stale
  digest and result state — and `prune` reads the result state from the row, so
  a stale row decides what gets deleted. Rows now carry the size and mtime of
  the file they were built from.
- **No `busy_timeout` on a database designed for two processes.** WAL removes
  only reader-writer contention, and every CLI read command is a writer because
  it reconciles first — so a `status` run at the moment `serve` persisted a run
  silently lost that run from the history.
- **`prune` lost the record of what it had already deleted** when it failed
  part-way: one unreadable directory returned an error and discarded the account
  of the other 199. Failures are now collected and reported per run, and the
  exit code says the policy did not fully apply.
- **The CLI discarded every index error.** A stranded future-schema database, a
  reconcile that could not run, and bundles this build cannot parse were all
  silent — including for `prune`, which would then apply `--keep-last` to a
  list that did not describe the disk. All three are now printed, and a
  confirmed prune refuses outright if the index could not be brought up to date.
- `--keep-last 0` and `--older-than-days 0` are refused. Each is one keystroke
  from a sensible value and each means "delete all of it".
- `bundle.json` is written to a temporary file and renamed rather than truncated
  in place, so a reader can never observe a half-written bundle and a crash
  cannot leave a run's evidence permanently unparseable.
- A comparison involving an endurance run now says that its metric rows come
  from the opening cycle while the totals beside them come from the last one.
  A flat row under a moved total is the machine declining, not noise.
- **A saturated load generator did not actually make a run unrankable.** Every
  piece existed and the whole did not work: `GeneratorSaturated` was in
  `degrades_result`, `degradation_reason` translated it to
  `LoadGeneratorSaturated`, and `is_partial_reason` did not list it — so a run
  whose only fault was an injector that could not keep up came out `Validated`.
  Phase 3's exit criterion would have been false while every individual file
  looked right. `is_partial_reason` is now an exhaustive `match` rather than a
  list, so the next reason added cannot fall through it silently; the same hole
  had swallowed `ErrorRateExceeded`.
- **The workload script was written without the protection the interpreter
  gets.** `runtime_exec` guards *which binary* runs; nothing guarded *what it is
  told to run*, and a root PHP pointed at somebody else's script is root code
  execution just as surely as somebody else's PHP is. `std::fs::write` follows
  symlinks and leaves the file at the umask's mode until a later `chmod`, so a
  planted symlink redirected root's write, and a permissive umask left a window
  in which the file was world-writable — after which the `chmod 0600` locked the
  attacker's content in. The scratch directory is now refused if anyone but its
  owner can write it, and the script is created `O_EXCL|O_NOFOLLOW` with its
  mode set at creation.
- **A child that wrote more than a pipe buffer was reported as a timeout.** Both
  streams were piped with nobody reading, so a child past 64 KiB blocked in
  `write()` and never exited — reproducible on either stream alone, since their
  buffers are independent, and live the moment a PHP has `display_errors` on.
  Both streams are now drained on threads from the moment the child starts.
- **Neither wait after a timeout was bounded.** A pipe reaches EOF when every
  write end closes, including one a *grandchild* inherited, so reading the
  output held the agent for as long as an orphan lived — ten seconds under a
  two-second timeout, returning success. And `wait()` after `kill()` is
  unbounded, so a child in uninterruptible sleep blocked the agent indefinitely:
  precisely the hang the timeout exists to prevent. Both are bounded now, and a
  killed child that will not die is left unreaped rather than waited on.
- A killed child was not reaped on the error path, leaking a zombie for the life
  of the agent.
- The safe-path check ignored setuid and setgid. Irrelevant while the agent is
  root, and not irrelevant otherwise: an unprivileged agent executing a
  root-owned setuid binary spawns a *root* child pointed at a script in a
  directory that agent owns.
- `discover` stopped at the first safe interpreter, so an unsafe one *after* it
  was never checked and never reported — and on a default `$PATH`,
  `/usr/local/bin` precedes `/usr/bin`, so the writable binary that went
  unreported was the one the operator's own shell would execute. Every candidate
  is now checked; the first safe one is still what runs.
- **The `hash.password` anchor described bcrypt cost 6, not the cost 8 the
  module runs.** 300 ops/s is 3.3 ms per hash; cost 8 is 256 key-setup rounds
  and lands near 16 ms on any current core. Because that metric carries the
  heaviest weight in the module and the module is half the Web basket, the
  anchor multiplied every machine's Web score by about 0.875. `startup.cold` was
  optimistic for the same kind of reason — PHP CLI start-up is dominated by
  dynamic linking and per-extension MINIT rather than core speed, so a stock
  distro build sits at 25-40 ms almost everywhere.
- The bcrypt checksum was vacuous: a bcrypt hash is always 60 characters, so a
  length-only checksum was identical whatever cost was applied. A build that
  ignored `cost => 8` for the ini default of 10 would have been four times
  slower with a byte-identical checksum and published as a slow machine.
- PHP CLI sends `display_errors` output to **stdout**, so one startup notice on
  a machine with that enabled turned the whole module into a precondition
  failure. The result is now read from the last JSON line rather than the whole
  stream.
- `startup.cold` divided total elapsed time by the *successful* invocation
  count, so one timeout inside a repetition folded 120 seconds into the average
  and published it as a finding about the machine.
- A pre-existing timing test (`throughput_scales_with_iterations`) went red on a
  loaded CI runner: a single descheduled sample on the smaller workload made the
  ratio look flat. It now takes the best of several, which discards exactly that
  interference — no amount of contention makes work finish faster than it can.
- **The runtime load ceiling counted the benchmark's own child processes as
  somebody else's load.** It subtracts the agent's CPU from the machine's, and
  excluded reaped children on the stated grounds that no module forks — true
  when written, false the moment `php.runtime` shipped, since a module that
  measures an interpreter does all its work in child processes. Every PHP run
  was degraded for its own workload. `cutime`/`cstime` are now included.
- The telemetry sampler advanced its self-CPU baseline even when the machine
  totals could not be read, so one failed `/proc/stat` read made a two-interval
  machine delta face a one-interval self delta and overstated external load —
  on the signal that stops runs.

**Measurement**
- `memory.bandwidth@1.0.0`, the first Phase 2 module: sequential read, write,
  copy and Triad, random-access throughput, a cache-resident scan and a
  dependent pointer-chase latency, in single- and multi-threaded shapes.
  Working sets are sized from the host's own cache topology at four times the
  last-level cache, first-touched outside every timed region, and capped at 25%
  of available memory so the module stays safe on a machine that is already
  serving traffic. A working set the budget forced below twice cache is
  reported as cache-contaminated rather than published as a DRAM figure.
- `storage.mixed@1.0.0`, the second Phase 2 module and the first that writes:
  sequential and 4K random read/write at queue depths 1 and 16, a 70/30 mixed
  shape, p99 tail latency and fsync durability cost. `O_DIRECT` keeps the
  measurement off the page cache, with a disclosed and downgraded fallback for
  filesystems that refuse it. Steady-state drift across repetitions is reported
  so an SLC cache emptying out is visible rather than published as the drive's
  sustained speed.

  The methodology's safety rules are enforced mechanically: the path comes from
  the agent's `StatePath` and the module appends one compile-time name;
  `O_NOFOLLOW` plus `O_EXCL` mean a symlink planted at that path is unlinked or
  aborts the run rather than redirecting a write at a block device; the open
  handle is asserted to be a regular file; and the fixture removes itself on
  `Drop`, so errors, cancellation and panics all leave the disk as they found
  it.

  Implemented natively rather than as an fio adapter. `docs/ROADMAP.md` records
  why: fio's methodology is followed, but a single static binary is a hard
  product requirement and a module that only worked where fio was installed
  would leave the Storage category empty on most hosts.
- `network.transfer@1.0.0`, the third Phase 2 module and the only one in the
  suite that contacts anything outside the machine: DNS resolution, TCP connect,
  connect jitter, TLS handshake, time to first byte, and download throughput
  over one stream and over four. The four connection phases are timed separately
  and never summed, because they fail for different reasons and have different
  fixes — slow DNS is a resolver problem, slow connect is distance, slow TLS is
  CPU or cipher choice, and slow TTFB after all three is the far end.

  Latency and jitter are sampled across three operators, so one provider's bad
  day shows up as spread rather than as the machine's own latency. Jitter is the
  variation *within* each path: a standard deviation taken across endpoints
  measures how far apart they are, and would report a perfectly steady link as
  jittery.

  The endpoint table is a compile-time `const` with a written justification per
  host, because `docs/THREAT-MODEL.md` (T-DDOS) is permanent — there is no API
  field, environment variable or config file that reaches it, and the only value
  the module formats into a request line is a byte count it computed itself.
  Volume is bounded by a ceiling enforced against a running total spanning
  calibration, warm-ups and every repetition, not merely documented. Packet loss
  and upload are declared as not measured rather than estimated: the first needs
  privileges this module does not take, the second needs an endpoint whose
  published purpose covers being sent bulk data.

  The `quick` profile deliberately excludes it, so the first run anyone makes on
  an unfamiliar server still opens no outbound connection at all.
- Preflight discloses outbound traffic: the volume, and the operator of every
  host the build can reach. Disk and memory costs were already shown before a
  run; "this tool phones out" was the one an operator most wants told first.
- `MachineFacts`: the agent now passes the cache topology and available memory
  it already collected into modules, so working-set sizing uses measured
  topology instead of a constant. `ModuleParams` additionally carries a scratch
  directory the agent has already validated, so a module never composes a
  filesystem path itself.
- Preflight discloses `estimated_peak_memory_bytes`, warns above 10% of
  available memory and refuses above 60%. Preflight already showed what a run
  costs in disk and network bytes; a module that quietly takes gigabytes of a
  live host's memory was the one cost it did not show.
- Shared measurement harness: calibration and the warm-up/measure loop are one
  implementation, so a module cannot quietly diverge from the rules that make
  results comparable.

**Scoring** — still `dbs/0.1.0-dev`, still **uncalibrated**
- DARC-REF-1 anchors for all thirteen `memory.bandwidth` metrics, so the Memory
  category now produces a score. Memory metrics deliberately carry no
  `single_core` / `multi_core` facet: those are published as core-performance
  numbers and folding DRAM latency into them would change what an
  already-shipped score means.
- DARC-REF-1 anchors for all seven `network.transfer` metrics, so the Network
  category now produces a score. Four of the five categories a standard total
  requires are implemented; every run is still `Partial`.
- `latency_random` is the model's first `lower_is_better` anchor.

**Dependencies**
- `rustls` 0.23 with the `ring` provider and `rustls-native-certs`, for the
  network module. `ADR-0011` records why neither default was taken: `aws-lc-rs`
  needs cmake at build time, and `webpki-roots` is MPL-2.0 — outside the
  `deny.toml` allow-list — besides freezing a CA bundle at compile time on a
  tool that runs on somebody else's server. Building now needs a C compiler for
  `ring`'s assembly; there is no new runtime dependency and the binary stays
  statically linked.

### Fixed

- **Event stream ordering under concurrent emitters.** The sequence number was
  allocated outside the replay-buffer lock, so the module thread and the 1 Hz
  telemetry task could interleave between allocating `seq` and appending it.
  The replay buffer, `events.ndjson` and the live SSE stream could each carry a
  lower sequence number after a higher one — and because every client folds the
  stream by `seq` and ignores anything not newer, a reordered event was
  silently dropped rather than merely displayed out of order.
- **A run reported itself terminal before its artifacts were on disk.** The
  state flipped before `events.ndjson` was written, so anything waiting for
  completion and then reading the run directory could find the event log
  missing.
- A `Last-Event-ID` of `u64::MAX` overflowed the replay resume check.
- **Only high variance downgraded a module to `Degraded`.** Every other
  measurement-invalidating warning was logged and then ignored, so a manifest
  could promise a downgrade — "rejected as timer-noise dominated" — and not get
  one. `WarningCode::degrades_result` now decides, and deliberately excludes
  environmental observations such as steal time, which describe the conditions
  a measurement was taken under rather than a fault in it.
- Every degraded module was reported as `ExcessiveVariance` regardless of cause,
  which told a reader to re-run on a quieter machine when the real answer might
  be that the machine cannot produce a comparable number at all. New
  `VerdictReason::ModuleDegraded` carries the module's own explanation.
- **Preflight risk fell as a run got more invasive.** The safety-class-to-risk
  mapping dipped in the middle, sending `WritesTemporaryFiles` to
  `ModerateLoad` while `ComputeIntensive` went to `HeavyLoad` — so adding a
  module that writes to disk made the warning screen quieter. The mapping is
  now monotonic and a test pins that it stays so.
- **The first run on a fresh install would have been refused.** `statvfs`
  needs a path that exists and the state directory is created on first use, so
  free space came back unknown — which every guard correctly treats as unsafe.
  Harmless while nothing wrote to disk; fatal the moment something did. Free
  space now resolves against the nearest existing ancestor.
- `Profile::ReadOnly` reported itself as standard, so a run that cannot measure
  write throughput, write latency or fsync cost could still claim a comparable
  total. `BENCHMARK-METHODOLOGY.md` is explicit that a read-only storage profile
  is not equivalent to a full storage score; it is now `Custom` structurally,
  and the profile no longer resolves to any module that writes.

- Verdict reasons are deduplicated. A module degraded by high variance whose
  metric also exceeded the validator's own CV ceiling was listed twice, which
  reads as two problems.

- **`events.ndjson` and `events_digest` came from the bounded replay buffer.**
  That buffer holds 4096 events and evicts oldest-first, which no profile had
  ever reached — until endurance, whose hour of 1 Hz telemetry passes it
  comfortably. Past that point the audit log written to disk silently began
  part-way through the run, at a nonzero sequence number, while
  `run.event_count` went on reporting the complete total and the digest covered
  only the surviving suffix. The record is now an append-only structure built as
  events are emitted, separate from the reconnect buffer: the buffer is bounded
  because a reconnecting browser only needs recent history, and the log is
  complete because it is evidence.

- **A duration override applied to any profile, not just the cycling one.**
  `{"profile":"quick","duration_minutes":1440}` passed the range check, resolved
  the quick module set, turned the run `Custom`, and then cycled it —
  `storage.mixed` fixture and all — for a day. Bounding the number was pointless
  while nothing bounded what it was applied to. Both the API and the CLI now
  refuse a duration for a profile that runs its module set once.

- **`network.transfer` never looked at the HTTP status line.** An error page or
  a redirect body would be drained, timed and published as this machine's
  download throughput — the endpoint having a bad day reported as a property of
  the server under test. Downloads now require a 200; timing-only probes still
  accept any well-formed response, because they measure the network rather than
  the payload.

- **Response headers were counted as payload.** The whole first read went into
  the byte total while the body clock started after it, so the transfer was
  credited with bytes that moved before the timer did. Only bytes past the
  header terminator count now, and the clock starts at the same boundary.

- **The opening read of every connection was uncharged to the transfer budget.**
  A timing-only probe performs exactly one read and there are more probe
  connections in a run than download connections, so the uncharged share was not
  incidental. A ceiling described as enforced rather than documented cannot be
  approximately right.

- **The reachability and latency probes were sending a malformed request.**
  `/__down?bytes=` with nothing after the `=` earns a `400`, which went
  unnoticed for as long as nothing checked the status: a rejection still costs a
  full DNS, TCP, TLS and round trip, so it still produced plausible timings.
  `ttfb.mean` was measuring how fast the endpoint says no. Probes now request a
  valid zero-byte payload, which costs nothing and returns a real `200`.

- **A module that failed in the first cycle discarded every cycle that worked.**
  Cycle completeness was judged against whatever cycle 0 happened to produce, so
  a module that failed once and recovered was missing from the expected set —
  and every later cycle, holding a superset, was rejected as incomplete. The
  cycles with *more* data were thrown away first, retention vanished, and the
  one bad cycle's burst figures were published as the endurance result.
  Completeness is now judged against every module that produced a result in any
  cycle.

### Notes

Seven defects across `storage.mixed` and `network.transfer` were found by
running the modules against a real disk and a real network rather than by
reading them, and are fixed in the same changes that introduced them. They are
listed here because the pattern is the point: none of them were visible in the
code, and every one of them would have shipped a plausible-looking wrong number.

Storage: calibration had no ramp period, so the first probe absorbed the
previous workload's writeback and the `fsync` shape calibrated down to a single
operation per repetition; and the steady-state ratio required six repetitions,
which silently disabled it on the `quick` profile that measures five.

Network:

- `download.multi` reported **652,721 Mbit/s**. The transfer budget was charged
  per 64 KiB read attempt while TLS records deliver about 16 KiB, so it
  over-charged fourfold and exhausted a 512 MiB ceiling after roughly 128 MiB of
  real traffic. Downloads were then truncated, and a small body divided by a
  near-zero interval is an arbitrarily large rate. The budget now refunds what a
  read did not use, and a truncated transfer yields no rate at all rather than a
  wrong one.
- `tls_handshake.mean` reported 0.5 ms where the real cost was 1.3 ms: rustls
  resumed sessions, so only the first handshake in a run was a full one.
  Resumption is now disabled — the metric is what a *new* client pays.
- Coefficients of variation of 132–236% on every latency metric, because raw
  samples from four different endpoints were pooled into one series. Four names
  legitimately cost different amounts to resolve; that is spread between hosts,
  not instability. Each repetition now contributes one value, as in every other
  module.
- `tcp_connect.jitter` was the standard deviation *across* endpoints, which
  measures how far apart they are. It is now computed within each path.
- The download size was fixed, so `deep` would have transferred 560 MiB and
  `endurance` 1.3 GiB against a 512 MiB ceiling. Size is now derived from the
  ceiling divided by the transfers a profile will make.
- **Warm-up repetitions reached the published statistics** for `dns_resolve`,
  `tls_handshake` and `ttfb`. Those three were accumulated in side vectors that
  the shared harness does not filter, while `tcp_connect` — collected in the
  same loop — correctly excluded them. The first repetition of this module is
  the coldest measurement it ever takes: an empty resolver cache, a TLS stack
  that has never run. Excluding it took `tls_handshake.mean`'s variation from
  25% to 12% on an unchanged path.
- **The variance bound was enforced on two metrics out of seven.** The check sat
  inside the download loop, so the manifest's promise that any CV above the
  bound warns and downgrades was true for `download.*` and quietly false for
  every latency metric — a run published `ttfb.mean` at 118% variation without a
  word while flagging `download.single` at 44%. The check is now a sweep over
  the finished metric list, so a metric added later is covered without anyone
  remembering to cover it. `tcp_connect.jitter` is exempt and says why: its
  samples are one per path rather than one per repetition, so their spread is
  endpoint diversity, not instability.

### Changed

- **The endurance profile's repetition counts dropped from 31 to 5**, and its
  warm-up from 2 to 1. Those are now *per cycle*, and the profile gets its depth
  from running ten to twenty cycles rather than from one very long pass.

  The old shape followed the instinct that the thorough profile should measure
  hardest, and that instinct is wrong here. Endurance's output is a curve, not a
  point: 31 repetitions in one pass could say what a machine averaged over an
  hour while being unable to say that it halved at minute forty — which is the
  finding. The last cycle still rests on exactly the sample count `quick`
  publishes as a headline.
- **`endurance` no longer resolves to `network.transfer`.** That module's
  transfer ceiling bounds what the suite pulls from a third party *per run*, and
  cycling it fifteen times would either breach that bound fifteen-fold or divide
  each cycle's transfer until the measurement said nothing. Sustained load on
  somebody else's CDN is not ours to generate. Bandwidth quotas over an hour are
  a real gap and are on the backlog with the reason they are still open.
- **An endurance run is scored from its last complete cycle**, not from every
  cycle pooled. Averaging burst throughput with post-throttling throughput
  produces a number describing neither, and the one an operator lives with is
  the second. Every other profile has exactly one cycle, so nothing else moved.
- Preflight multiplies duration, flash wear and network bytes by the expected
  cycle count. Estimating an hour-long run from a single pass understated its
  cost by an order of magnitude — right for `standard`, badly wrong for the
  profile that repeats. Peak memory is deliberately *not* multiplied: cycles run
  one after another.
- The workspace is `cargo fmt` clean again. CI has always enforced
  `cargo fmt --all -- --check` and the Phase 2 modules had drifted from it, so
  the check was failing on formatting rather than on anything real.
- `cpu.mixed` to `1.0.1`: the calibration search now steps proportionally
  towards a trustworthy duration instead of doubling from one iteration.
  Measured values are unaffected — throughput is work over time, independent of
  the iteration count chosen — so results stay comparable with `1.0.0`.

### Performance

- Replay buffer is a ring buffer; eviction was `Vec::remove(0)`, an O(n) shift
  on every event once full.
- Finalisation no longer copies the event stream twice or the telemetry series
  once; `GET /api/v1/runs` no longer clones a full result bundle per run.
- Telemetry sampling costs ~40% less per tick. The sampler was re-walking
  `/sys/class/thermal` every second, and on hosts without `cpufreq` — the usual
  case on a VPS — re-reading the whole of `/proc/cpuinfo`, whose cost the
  kernel scales with core count. Both sources are now resolved once, and the
  `/proc` parsers no longer allocate per line.
- `run` no longer polls for completion on a 100 ms timer.
- Scoring aggregates categories and facets in one pass instead of nine, and no
  longer formats a per-metric key that nothing read.
- HTML escaping borrows when there is nothing to escape, which is almost every
  value in a report.

## [0.1.0] — 2026-08-03

First release: the Phase 1 vertical slice. Working end to end, and explicit
about what it is not.

### Added

**Agent**
- `darcbench` CLI: `doctor`, `inspect`, `serve`, `run`, `status`, `report`,
  `verify`, `uninstall`, with `--json`, `--no-color`, `--non-interactive`.
- Local dashboard server bound to `127.0.0.1` by default, 256-bit token auth,
  strict CSP and hardening headers.
- Run orchestration: preflight, telemetry, cancellation, finalisation. One run
  at a time; benchmarks execute off the async runtime.
- Preflight safety: risk classification, disk-space guard, production detection,
  load and swap checks, cgroup and container disclosure. `--force` overrides
  warnings but never blocking findings.

**Measurement**
- `cpu.mixed@1.0.0`: SHA-256, DEFLATE, JSON round-trip, integer sort and
  double-precision matmul, each in single- and multi-threaded shapes.
- Per-machine calibration of iteration counts; warm-up repetitions retained and
  flagged; medians, CV and non-parametric confidence intervals; MAD outlier
  flagging.
- 1 Hz telemetry: CPU busy/steal/iowait, load, memory, swap, frequency,
  temperature, PSI, disk and network rates.

**Scoring** — `dbs/0.1.0-dev`, **uncalibrated**
- Reference-anchored normalisation against the DARC-REF-1 specification.
- Weighted geometric aggregation, single-core and multi-core facets, seven
  workload composites, stability multiplier and efficiency score.
- **Weak-link cap**: the total may not exceed 4× the weakest measured category.

**Results**
- `darcbench.bundle/1` with Ed25519 signatures over DCJ/1 canonical JSON.
- Validation ruleset `dbv/0.1.0`, running in the agent and, strictly, server-side.
- Self-contained HTML report, JSON bundle, NDJSON event stream.
- `verify` detects both a tampered score and a tampered raw metric.

**Protocol** — `darcbench.events/1`
- Nineteen event kinds, gapless sequence numbers, dual wall-clock/monotonic
  timestamps, SSE transport with `Last-Event-ID` replay.

**Web UI**
- React + TypeScript dashboard, compiled into the agent binary.
- Live command centre: preflight risk, telemetry sparklines, provisional scores,
  raw measurements, event log.
- Accessible: no colour-only meaning, live regions, keyboard navigation,
  reduced-motion and high-contrast support.
- A built-in fallback console when the React bundle is not compiled in.

**Privacy**
- `Sensitive<T>` makes redaction the default at serialisation. Hostnames, MAC
  addresses and IPs are redacted; DMI serials, UUIDs and cloud instance ids are
  never collected.

**Documentation**
- Product bible, PRD, market research, competitive analysis, architecture,
  threat model, methodology, scoring system, module spec, real-time protocol,
  API, data model, UX, installer and discovery, privacy, operations,
  observability, test strategy, roadmap, backlog, release and commercial
  strategy, glossary, research sources, and ten ADRs.

### Known limitations

- **The scoring model is not calibrated.** Reference values are declared targets
  for a specified machine, not measurements. Every score is flagged
  `uncalibrated` and a test fails the build if that flag is cleared.
- Only the compute category is implemented, so every run is `Partial` and no
  total is standard.
- The `web` profile has no implemented modules and refuses to start rather than
  running a subset.
- No control plane; `Validated` and above are unreachable in this release.
- Linux only; arm64 is untested.
