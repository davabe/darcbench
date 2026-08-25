//! Raw measurement records.
//!
//! # The evidence / score separation
//!
//! Everything in this module is **immutable evidence**: what the machine
//! actually did, in physical units, with every repetition retained. Scores are
//! derived elsewhere (`darcbench-scoring`) and are always recomputable from
//! this data. That separation is what makes it possible to publish a new
//! scoring model version and retroactively rescore historical runs without
//! re-running a single benchmark. See `docs/DATA-MODEL.md`.

use serde::{Deserialize, Serialize};

use crate::ids::ModuleRef;
use crate::stats::Summary;

/// Whether a larger raw value means better performance.
///
/// Latency metrics are `LowerIsBetter` and are inverted exactly once, during
/// normalisation. Inverting twice (a classic scoring bug) would rank the
/// slowest machine first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    HigherIsBetter,
    LowerIsBetter,
}

/// One timed repetition of one workload.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MetricSample {
    /// 0-based repetition index within the measured (non-warm-up) phase.
    pub rep: u32,
    pub value: f64,
    /// Wall-clock duration of the repetition, in milliseconds.
    pub duration_ms: f64,
    /// True when this repetition was a warm-up and is excluded from `summary`.
    #[serde(default)]
    pub warmup: bool,
}

/// A fully summarised measurement.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Metric {
    /// Dotted key, unique within a module, e.g. `crypto_sha256.single`.
    pub key: String,
    /// Human-readable label for reports.
    pub label: String,
    /// Physical unit, e.g. `MiB/s`, `ops/s`, `ms`, `MFLOP/s`. Never a score.
    pub unit: String,
    pub direction: Direction,
    /// Headline value. Always the median of the measured repetitions.
    pub value: f64,
    pub summary: Summary,
    /// Every measured repetition, warm-ups included and flagged.
    pub samples: Vec<MetricSample>,
    /// Indices into `samples` flagged by MAD outlier detection. Flagged, never
    /// silently dropped.
    #[serde(default)]
    pub outliers: Vec<usize>,
    /// True when this metric *measures* dispersion, so its own variation
    /// between repetitions is the subject rather than a defect.
    ///
    /// `network.transfer/tcp_connect.jitter` is the case this exists for. It is
    /// a spread, and a spread that is stable across repetitions would be a
    /// suspiciously quiet network rather than a good measurement. The module
    /// already exempts it from its own stability warning, with the reasoning
    /// written beside the exemption - but the *validator* applied a blanket CV
    /// bound to every metric and knew nothing about it, so a healthy run was
    /// downgraded to `Partial` by the one metric whose variance is the point.
    ///
    /// `Partial` is not rankable, so on any host with ordinary internet jitter
    /// that made a standard run unrankable.
    ///
    /// A property of the metric rather than a list in the validator, for the
    /// same reason [`Direction`] is: only the module knows what it measured,
    /// and a second list elsewhere is a second thing to keep in step.
    #[serde(default)]
    pub measures_dispersion: bool,
}

/// Why a module did not produce a usable result.
///
/// `Copy` for the same reason `RunState` is: it is a fieldless discriminant
/// that callers pass around by value, and without it every read of
/// `ModuleResult::status` through a shared reference needs a clone that copies
/// exactly one byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModuleStatus {
    Completed,
    /// Ran, but a validation check failed; metrics exist and are retained but
    /// must not contribute to a standard score.
    Degraded,
    Failed,
    Cancelled,
    Skipped,
}

/// Everything one module produced during a run.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModuleResult {
    pub module: ModuleRef,
    pub status: ModuleStatus,
    /// Which pass over the module set produced this result.
    ///
    /// Always 0 for every profile but `endurance`, which repeats its module set
    /// until a duration target elapses so that a decline over an hour is
    /// visible. The same module id therefore appears once per cycle, and the
    /// cycle index is what makes those results comparable with each other
    /// instead of merely duplicated.
    ///
    /// Defaulted rather than required so a bundle written before cycles existed
    /// still deserialises, and reads as the single-pass run it was.
    #[serde(default)]
    pub cycle: u32,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub finished_at: chrono::DateTime<chrono::Utc>,
    pub duration_ms: f64,
    pub metrics: Vec<Metric>,
    /// Non-fatal observations: high CV, detected throttling, steal time, etc.
    #[serde(default)]
    pub warnings: Vec<Warning>,
    /// Present when `status` is `Failed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Free-form, typed-at-the-edge module context (thread count, work sizes,
    /// compiler target). Used for comparability checks, never for scoring.
    #[serde(default)]
    pub context: serde_json::Map<String, serde_json::Value>,
}

impl ModuleResult {
    pub fn metric(&self, key: &str) -> Option<&Metric> {
        self.metrics.iter().find(|m| m.key == key)
    }
}

/// A structured, machine-actionable warning.
///
/// Warnings are typed rather than free strings so the UI, the scoring model and
/// the verification tier logic can all reason about the same code.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Warning {
    pub code: WarningCode,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metric_key: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WarningCode {
    /// Coefficient of variation exceeded the module's declared stability bound.
    HighVariance,
    /// Hypervisor stole measurable CPU time during the measured window.
    StealTimeObserved,
    /// CPU frequency dropped materially between the first and last repetition.
    FrequencyDrop,
    /// Thermal or power throttling reported by the platform.
    ThermalThrottle,
    /// System load was already elevated when the module started.
    PreexistingLoad,
    /// Work that is not this benchmark competed for the CPU *during* the
    /// measured window.
    ///
    /// Distinct from [`Self::PreexistingLoad`], and degrading where that one is
    /// not, because of consent and of timing. Pre-existing load is disclosed at
    /// preflight, before anything runs, and the operator chooses to continue
    /// knowing the machine is busy. External load arrives afterwards, is
    /// measured against this process's own CPU accounting rather than guessed
    /// from a load average, and lands inside the window whose numbers are being
    /// published.
    ExternalLoad,
    /// Memory pressure / reclaim observed during the measured window.
    MemoryPressure,
    /// The run executes inside a container or constrained cgroup, so results
    /// describe the container, not the host.
    ContainerScoped,
    /// A module-declared validation check failed.
    ValidationFailed,
    /// The load generator, not the system under test, was the bottleneck.
    GeneratorSaturated,
    /// Something the module wants a human to read, with no automated meaning.
    Informational,
}

impl WarningCode {
    /// True when this observation means the module's own measurement cannot be
    /// treated as clean and comparable.
    ///
    /// A degraded module still contributes every metric it produced - those are
    /// real measurements and stay in the bundle as evidence - but the run is
    /// reported `Partial` rather than ranked. Module manifests that promise a
    /// downgrade are kept honest by this: a manifest saying "rejected as
    /// timer-noise dominated" while the code only logged a warning was a
    /// promise the suite did not keep.
    ///
    /// Environmental observations are deliberately **not** in this set. Steal
    /// time, an already-loaded machine, a frequency drop and container scope
    /// describe the conditions a measurement was taken under, not a fault in
    /// the measurement; they are disclosed in telemetry, folded into the
    /// stability score, and shown in the report. Degrading on them would make
    /// every shared-instance run `Partial`, which would defeat the point of
    /// being able to benchmark a VPS at all.
    ///
    /// [`Self::ExternalLoad`] is the exception that proves the boundary. It is
    /// not the machine's standing condition but a change to it, arriving after
    /// the operator agreed to the run and inside the window being published.
    pub fn degrades_result(self) -> bool {
        match self {
            Self::HighVariance
            | Self::ValidationFailed
            | Self::MemoryPressure
            | Self::ThermalThrottle
            | Self::ExternalLoad
            | Self::GeneratorSaturated => true,
            Self::StealTimeObserved
            | Self::FrequencyDrop
            | Self::PreexistingLoad
            | Self::ContainerScoped
            | Self::Informational => false,
        }
    }
}

#[cfg(test)]
// In tests, `unwrap`/`expect` panicking *is* the failure signal.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic_in_result_fn)]
mod tests {
    use super::*;

    #[test]
    fn direction_serialises_as_snake_case() {
        let json = serde_json::to_string(&Direction::LowerIsBetter).expect("json");
        assert_eq!(json, "\"lower_is_better\"");
    }

    #[test]
    fn warning_code_roundtrips() {
        for code in [
            WarningCode::HighVariance,
            WarningCode::StealTimeObserved,
            WarningCode::ContainerScoped,
            WarningCode::GeneratorSaturated,
        ] {
            let s = serde_json::to_string(&code).expect("ser");
            let back: WarningCode = serde_json::from_str(&s).expect("de");
            assert_eq!(code, back);
        }
    }
}
