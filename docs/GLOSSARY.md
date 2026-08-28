# Glossary

**Agent** — the `darcbench` binary. Runs benchmarks, serves the dashboard,
produces signed bundles.

**Anchor** — the value DARC-REF-1 is expected to produce for a specific metric.
Normalisation divides by it.

**Balance index** — weakest category score divided by the geometric mean of all
category scores, in `(0, 1]`. 1.0 is a perfectly balanced machine.

**Bundle** — a complete, self-contained, signed result: evidence, environment,
scores, verdict and telemetry summary. Schema `darcbench.bundle/1`.

**Calibrated** — a scoring model whose reference anchors are measurements from
physical DARC-REF-1 hardware, not declared targets. `dbs/0.2.0-dev` is **not**
calibrated.

**Category** — a top-level score group: compute, memory, storage, network, web,
database, deployment.

**Composite** — a workload-oriented re-weighting of category scores, e.g.
WordPress Hosting. Withheld below 60% input coverage.

**Coordinated omission** — the measurement error where a closed-loop load
generator stops issuing requests while the system is stalled, systematically
hiding the worst latencies. Named by Gil Tene.

**CV (coefficient of variation)** — standard deviation ÷ mean. DARCBench's
primary stability signal.

**DARC-REF-1** — the reference machine *specification* that scores are normalised
against. See [SCORING-SYSTEM.md](SCORING-SYSTEM.md).

**DCJ/1** — DARCBench Canonical JSON: sorted keys, no insignificant whitespace,
non-finite numbers rejected, correctly-rounding decimal parsing required. What
signatures are computed over.

**Degraded** — a module that produced metrics but failed a validation check. Its
data is retained; the run cannot be standard.

**Environment digest** — SHA-256 over performance-relevant inventory facts only.
Detects a machine changing mid-run; safe to publish.

**Evidence layer** — the immutable raw measurements. Distinct from the derived
score layer.

**Events digest** — SHA-256 over the ordered event stream, so a viewer can prove
the events they saw match the bundle.

**Facet** — a cross-cutting aggregate reported as its own score: Single-Core,
Multi-Core.

**Geometric mean** — the aggregation function used at every level. Penalises
imbalance in a way an arithmetic mean does not.

**Module** — a versioned workload definition, e.g. `cpu.mixed@1.0.0`.

**Open model / closed model** — an open-model load generator issues requests at a
fixed rate regardless of responses; a closed-model one waits. Only open-model
generation produces trustworthy latency figures.

**PSI** — Linux Pressure Stall Information (`/proc/pressure/*`). Reveals real
contention the load average hides.

**Profile** — a named bundle of modules and parameters: quick, standard, deep,
endurance, read-only, web, custom. The unit of comparability.

**Reference anchor (1000)** — the score DARC-REF-1 receives in every category and
in the total.

**Result state** — how much a result may be trusted: Local, SelfReported,
Validated, Verified, Official, Invalid, Partial, Custom. Only the middle three
are rankable.

**Run id** — `run_` plus 128 bits of CSPRNG output. Never derived from any host
property.

**Safety class** — how invasive a module is: Observational, ComputeIntensive,
WritesTemporaryFiles, UsesNetwork, ProvisionsServices.

**Scaling efficiency** — multi-thread throughput ÷ (single-thread × threads).
Exposes SMT saturation and shared vCPUs.

**Scope** — what a measurement actually describes: bare metal, virtual machine,
or container. Always displayed, never silently pooled.

**Sensitive\<T\>** — the wrapper type that makes identifying values redact by
default at serialisation.

**Steal time** — CPU time the hypervisor took from this guest. On burstable
instances it also means credits are exhausted.

**Weak-link cap** — the rule that the total may not exceed 4× the weakest
measured category. Prevents one strong subsystem hiding a catastrophic one.

**Verdict** — the validation outcome: a result state plus typed reasons plus the
validator version.

**Warm-up** — untimed repetitions that populate caches and stabilise clocks.
Streamed and retained, flagged, excluded from statistics.
