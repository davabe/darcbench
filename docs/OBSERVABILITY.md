# Observability

Two distinct things share the word "telemetry" here, and conflating them would
be a design error:

1. **Measurement telemetry** — what the machine under test was doing during a
   run. This is *evidence*, part of the result, and lives in the bundle.
2. **Operational observability** — whether the agent and control plane are
   healthy. This is diagnostics.

## Measurement telemetry

Sampled at **1 Hz**, deliberately slow: the observer is part of the system under
test. A unit test fails if sampling costs more than 25 ms per sample.

| Series | Source | Why it matters |
|---|---|---|
| CPU busy / iowait / **steal** | `/proc/stat` deltas | Steal reveals oversubscription or exhausted burst credits |
| Load 1m | `/proc/loadavg` | Competing work |
| Memory used / total, swap used | `/proc/meminfo` | Pressure and paging |
| CPU frequency | `cpufreq/scaling_cur_freq` | Throttling across a long run |
| Temperature | `/sys/class/thermal` | Thermal saturation; often absent in VMs, reported as `None` not 0 |
| PSI cpu/io/memory `avg10` | `/proc/pressure/*` | Real contention the load average hides |
| Disk read/write rates | `/proc/diskstats` deltas | Whole devices only, partitions excluded |
| Network rx/tx rates | `/proc/net/dev` deltas | Loopback excluded |

`busy` deliberately **excludes** iowait and steal. Neither is the workload making
progress, and folding them into one "CPU used" number is exactly how monitoring
tools hide noisy neighbours.

The full series is written to the run directory; the bundle carries a summary
(means, maxima, first/last frequency) because a 60-minute run at 1 Hz is 3600
samples per field and does not belong in a shareable artifact.

`frequency_drop()` — the fractional decline between first and last observation —
is the throttling signature: sustained decline means thermal or power limits on
bare metal, and credit exhaustion on burstable cloud instances.

## Operational observability

### Logs

Structured via `tracing`, to **stderr** so `--json` on stdout stays clean.
`--log error|warn|info|debug|trace`, or `DARCBENCH_LOG`. `--json` switches logs
to JSON too.

Logging rules: never log the access token (it has no `Display`), never log the
private key (`Debug` prints only the key id), never log identifying inventory
values outside a `Reveal` scope.

### Health

`GET /healthz` — unauthenticated, returns status and agent version, nothing about
the machine.

`GET /api/v1/meta` — unauthenticated, returns protocol and scoring versions and
whether the model is calibrated. Deliberately reveals nothing about the host: an
unauthenticated caller learns an agent is here and nothing else.

### The event stream as observability

`events.ndjson` is a complete, sequence-numbered, timestamped record of
everything a run did, with two clocks. For diagnosing "why did this run behave
strangely" it is more useful than any log, and it is retained by default.

## Phase 5 — control plane

OpenTelemetry traces and metrics. Trace spans: upload → validate → rescore →
publish. Metrics: runs ingested, validation outcomes by verdict, rescoring lag,
queue depth. Structured logs correlated by `run_id`.

Metrics the product will care about, chosen now so instrumentation is not
retrofitted:

- Share of uploaded runs that are `Invalid`, and why — a rising rate means either
  a real problem or an attack.
- Distribution of `Partial` reasons — tells us which modules to build next.
- Score recomputation mismatches — the tamper signal.
- Median CV per module per provider segment — the noisy-neighbour dataset, and
  arguably the most commercially interesting thing DARCBench will ever hold.

## Not collected

No usage analytics, no update checks, no crash reporting, no phone-home of any
kind in standalone mode. The agent makes **no outbound connection** unless a run
explicitly requires one (network modules) or the operator opts into upload.
