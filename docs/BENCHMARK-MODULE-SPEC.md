# Benchmark module specification

**Version:** `darcbench.module/1`
**Contract:** `crates/darcbench-modules/src/module.rs`

A module is a **versioned workload definition**, not a script. It declares
everything the agent and the safety layer need *before* it runs.

## The manifest

```rust
pub struct ModuleManifest {
    pub id: ModuleId,              // [a-z][a-z0-9_]* segments, dot separated
    pub version: String,           // semver of the WORKLOAD, not the agent
    pub title: String,
    pub purpose: String,
    pub safety_class: SafetyClass,
    pub dependencies: Vec<String>, // external binaries/services; empty = self-contained
    pub max_bytes_written: u64,    // upper bound, for the disk guard
    pub max_network_bytes: u64,    // upper bound, for the transfer ceiling
    pub cleanup: String,           // what is removed, including on cancellation
    pub validation: Vec<String>,   // what makes results unscoreable
    pub limitations: Vec<String>,  // shown in every report
    pub comparability: Vec<String>,// fields that must match for comparison
    pub stability_cv_bound: f64,   // CV above which the module flags itself
}
```

`limitations` is mandatory and non-empty by convention. A module that claims no
limitations has not thought about what it measures.

## Safety classes

Ordered by invasiveness; preflight takes the maximum across the selected set.

| Class | Meaning |
|---|---|
| `Observational` | Read-only. No writes, no service interaction, bounded CPU |
| `ComputeIntensive` | Saturates CPU or memory; writes nothing, touches no service |
| `WritesTemporaryFiles` | Writes under a DARCBench-owned path |
| `UsesNetwork` | Generates outbound traffic to third-party endpoints |
| `ProvisionsServices` | Creates and destroys its own databases or containers |

## The trait

```rust
pub trait BenchmarkModule: Send + Sync {
    fn manifest(&self) -> &ModuleManifest;
    fn estimated_duration_s(&self, params: &ModuleParams) -> u64;
    fn run(&self, params: &ModuleParams, reporter: &dyn ModuleReporter)
        -> Result<ModuleOutput, ModuleError>;
}
```

`estimated_duration_s` must not be optimistic — it is what the operator sees
before agreeing to run anything on a production host.

`run` executes on a blocking thread. It must:

1. **Check `reporter.is_cancelled()` between repetitions.** Cancellation is
   cooperative; killing a thread mid-workload would leak temporary resources.
2. **Clean up on every exit path**, including errors and cancellation.
3. **Emit a sample per repetition**, warm-ups flagged, so the UI shows progress.
4. **Never construct a command line from external input.** Subprocess modules use
   fixed argv, never a shell.
5. **Return `Err(NoSamples)` rather than a fabricated value** when measurement
   failed.

## Reporter

```rust
pub trait ModuleReporter: Send + Sync {
    fn sample(&self, metric_key: &str, unit: &str, rep: u32, warmup: bool,
              value: f64, duration_ms: f64, module_progress: f64);
    fn warn(&self, warning: Warning);
    fn is_cancelled(&self) -> bool;
}
```

Warnings are typed (`WarningCode`), not free strings, so the UI, the scoring
model and the verification tiers can all reason about the same code.

## Metric conventions

- **Key:** `<workload>.<shape>`, e.g. `crypto_sha256.single`. Restricted to
  `[a-z][a-z0-9_]*` because keys index the scoring reference table.
- **Unit:** a physical unit — `MiB/s`, `ops/s`, `ms`, `IOPS`, `MFLOP/s`. Never a
  score, never dimensionless.
- **Direction:** `higher_is_better` or `lower_is_better`. Inverted exactly once,
  during normalisation, by the scoring crate only.
- **Value:** always the median of measured repetitions.

## Adding a module

1. Implement `BenchmarkModule`.
2. Add reference anchors in `darcbench-scoring::reference`.
3. Register it in `Registry::builtin()`.
4. Write `benchmarks/<category>/<id>.json` (the machine-readable manifest).
5. Add module tests: manifest well-formedness, full run, cancellation
   responsiveness, determinism of any corpus.

Steps 1–3 are coupled by design: a module whose metrics have no anchors appears
in `ScoreCard::unreferenced_metrics` immediately, and a test forbids shipping
anchors for a module that does not exist.

## Third-party modules

**Not supported in Phase 1.** The registry is a compile-time table; there is no
dynamic loading. See [ADR-0006](adr/0006-module-isolation.md).

When they land (Phase 8) they will require signed manifests, integrity hashes,
declared resource bounds, sandboxed execution, and they will **never** contribute
to the official total score. Their runs are `Custom`. A total score whose inputs
anyone can extend is not a score anyone can trust.

## Compatibility

| Change | Version impact |
|---|---|
| Bug fix, no change to measured values | patch |
| New metric added | minor; existing metrics stay comparable |
| Workload, corpus, seed or sizing changed | **major**; results not comparable |

Two `ModuleResult`s are comparable only when the module id and **major** version
match, plus every field named in `comparability`.

## Reference: `cpu.mixed` v1.0.0

```json
{
  "id": "cpu.mixed", "version": "1.0.0",
  "safety_class": "compute_intensive",
  "dependencies": [], "max_bytes_written": 0, "max_network_bytes": 0,
  "stability_cv_bound": 0.15,
  "comparability": ["module.version", "cpu.architecture",
                    "agent.build_target", "params.threads"]
}
```

Full manifest in `benchmarks/cpu/cpu.mixed.json`.
