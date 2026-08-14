//! The versioned real-time event stream.
//!
//! # Design constraints
//!
//! 1. **Every event is self-describing.** A consumer that joins late, or
//!    reconnects, can reconstruct run state from the replayed prefix alone.
//! 2. **Monotonic sequence numbers.** `seq` starts at 0 and increases by
//!    exactly 1. A gap means the consumer lost events and must re-fetch from
//!    its last known `seq` - it must never silently interpolate.
//! 3. **Two clocks.** `ts` is wall-clock for humans and correlation; `mono_ms`
//!    is a monotonic offset from run start and is what any duration reasoning
//!    must use. Wall-clock can jump (NTP, VM migration); a run whose wall-clock
//!    moves backwards is flagged with [`crate::VerdictReason::ClockAnomaly`].
//! 4. **No stringly-typed payloads.** Every event body is a named struct.

use serde::{Deserialize, Serialize};

use crate::ids::{ModuleId, ModuleRef, RunId};
use crate::metrics::{ModuleResult, Warning};
use crate::run::{Profile, RunState, Verdict};

/// The outer frame carried by every transport (SSE, WebSocket, NDJSON file).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Envelope {
    /// Always [`crate::PROTOCOL_VERSION`] for events produced by this build.
    pub protocol: String,
    pub run_id: RunId,
    /// Gapless, 0-based, per-run sequence number.
    pub seq: u64,
    /// Wall-clock timestamp (UTC, RFC 3339).
    pub ts: chrono::DateTime<chrono::Utc>,
    /// Milliseconds elapsed on the monotonic clock since run start.
    pub mono_ms: u64,
    #[serde(flatten)]
    pub event: Event,
}

impl Envelope {
    pub fn kind(&self) -> &'static str {
        self.event.kind()
    }
}

/// All event bodies. Internally tagged with `type` so a stream can be decoded
/// without out-of-band framing.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Event {
    #[serde(rename = "run.created")]
    RunCreated(RunCreated),
    #[serde(rename = "run.preflight.started")]
    PreflightStarted(PreflightStarted),
    #[serde(rename = "run.preflight.completed")]
    PreflightCompleted(PreflightCompleted),
    #[serde(rename = "module.queued")]
    ModuleQueued(ModuleLifecycle),
    #[serde(rename = "module.preparing")]
    ModulePreparing(ModuleLifecycle),
    #[serde(rename = "module.warmup")]
    ModuleWarmup(ModuleLifecycle),
    #[serde(rename = "module.started")]
    ModuleStarted(ModuleLifecycle),
    #[serde(rename = "module.sample")]
    ModuleSample(ModuleSampleEvent),
    #[serde(rename = "module.telemetry")]
    ModuleTelemetry(TelemetryEvent),
    #[serde(rename = "module.warning")]
    ModuleWarning(ModuleWarningEvent),
    #[serde(rename = "module.completed")]
    ModuleCompleted(Box<ModuleCompletedEvent>),
    #[serde(rename = "module.failed")]
    ModuleFailed(ModuleFailedEvent),
    #[serde(rename = "module.cancelled")]
    ModuleCancelled(ModuleLifecycle),
    #[serde(rename = "score.provisional")]
    ScoreProvisional(ScoreEvent),
    #[serde(rename = "score.final")]
    ScoreFinal(ScoreEvent),
    #[serde(rename = "report.generated")]
    ReportGenerated(ReportGenerated),
    #[serde(rename = "run.completed")]
    RunCompleted(RunCompleted),
    #[serde(rename = "run.invalidated")]
    RunInvalidated(RunInvalidated),
    /// Keeps idle connections and reverse proxies alive and lets a client
    /// detect a stalled agent. Carries no run semantics.
    #[serde(rename = "stream.heartbeat")]
    Heartbeat(Heartbeat),
}

impl Event {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::RunCreated(_) => "run.created",
            Self::PreflightStarted(_) => "run.preflight.started",
            Self::PreflightCompleted(_) => "run.preflight.completed",
            Self::ModuleQueued(_) => "module.queued",
            Self::ModulePreparing(_) => "module.preparing",
            Self::ModuleWarmup(_) => "module.warmup",
            Self::ModuleStarted(_) => "module.started",
            Self::ModuleSample(_) => "module.sample",
            Self::ModuleTelemetry(_) => "module.telemetry",
            Self::ModuleWarning(_) => "module.warning",
            Self::ModuleCompleted(_) => "module.completed",
            Self::ModuleFailed(_) => "module.failed",
            Self::ModuleCancelled(_) => "module.cancelled",
            Self::ScoreProvisional(_) => "score.provisional",
            Self::ScoreFinal(_) => "score.final",
            Self::ReportGenerated(_) => "report.generated",
            Self::RunCompleted(_) => "run.completed",
            Self::RunInvalidated(_) => "run.invalidated",
            Self::Heartbeat(_) => "stream.heartbeat",
        }
    }

    /// Whether a client can stop listening after this event.
    pub fn is_stream_terminal(&self) -> bool {
        matches!(self, Self::RunCompleted(_) | Self::RunInvalidated(_))
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RunCreated {
    pub profile: Profile,
    pub modules: Vec<ModuleRef>,
    pub agent_version: String,
    pub scoring_model: String,
    /// Fingerprint of the environment snapshot taken at run start. Used to
    /// detect material changes mid-run.
    pub environment_digest: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PreflightStarted {
    pub checks: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PreflightCompleted {
    pub risk: RiskClass,
    pub passed: bool,
    pub findings: Vec<PreflightFinding>,
    pub estimated_duration_s: u64,
    pub estimated_bytes_written: u64,
    pub estimated_network_bytes: u64,
    /// Peak heap the selected modules expect to hold at once.
    ///
    /// Disclosed for the same reason as bytes written: preflight exists to show
    /// an operator what a run costs on a machine that may already be serving
    /// customers, and a module that quietly takes gigabytes of memory is a cost
    /// they need to see before agreeing. `#[serde(default)]` so a bundle or
    /// event stream written before this field existed still decodes.
    #[serde(default)]
    pub estimated_peak_memory_bytes: u64,
    /// Total bytes the selected modules expect to write over the whole run.
    ///
    /// Distinct from `estimated_bytes_written`, which is the *space* the disk
    /// guard requires. This is flash endurance, and it is what an operator
    /// weighing a storage run on a production SSD actually needs to see.
    #[serde(default)]
    pub estimated_write_volume_bytes: u64,
}

/// How disruptive running this profile on this machine right now is expected
/// to be. Shown before a run can start; never inferred silently.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskClass {
    Safe,
    ModerateLoad,
    HeavyLoad,
    ProductionRisk,
    Unsupported,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PreflightFinding {
    pub check: String,
    pub severity: Severity,
    pub message: String,
    /// True when this finding alone blocks the run from starting.
    pub blocking: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Info,
    Warning,
    Error,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModuleLifecycle {
    pub module: ModuleRef,
    /// 0-based position of this module in the run's execution order.
    pub index: u32,
    pub total: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModuleSampleEvent {
    pub module: ModuleId,
    /// Metric key this sample belongs to, e.g. `crypto_sha256.single`.
    pub metric_key: String,
    pub rep: u32,
    pub warmup: bool,
    pub value: f64,
    pub unit: String,
    pub duration_ms: f64,
    /// Fraction of this module's work completed, in `[0, 1]`.
    pub module_progress: f64,
}

/// Low-rate system telemetry sampled *while* a module runs.
///
/// Sampling rate is deliberately capped (default 1 Hz) because the observer is
/// part of the system under test. See `docs/BENCHMARK-METHODOLOGY.md` section
/// "Measurement overhead".
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TelemetryEvent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub module: Option<ModuleId>,
    /// Percentage of CPU time in user+system across all cores, `[0, 100]`.
    pub cpu_busy_pct: f64,
    /// The share of `cpu_busy_pct` that the agent process did not consume.
    ///
    /// Zero on a dedicated machine. Anything sustained above a few percent is
    /// competition for the measurement, which is why the run watchdog reads
    /// this field and not the load average.
    #[serde(default)]
    pub cpu_external_busy_pct: f64,
    /// Percentage of CPU time stolen by the hypervisor, `[0, 100]`.
    pub cpu_steal_pct: f64,
    pub cpu_iowait_pct: f64,
    pub load1: f64,
    pub mem_used_bytes: u64,
    pub mem_total_bytes: u64,
    pub swap_used_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_freq_mhz: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_temp_c: Option<f64>,
    /// PSI "some" CPU pressure over 10s, when the kernel exposes it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub psi_cpu_some_avg10: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub psi_io_some_avg10: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub psi_mem_some_avg10: Option<f64>,
    pub disk_read_bytes_per_s: u64,
    pub disk_write_bytes_per_s: u64,
    pub net_rx_bytes_per_s: u64,
    pub net_tx_bytes_per_s: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModuleWarningEvent {
    pub module: ModuleId,
    #[serde(flatten)]
    pub warning: Warning,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModuleCompletedEvent {
    pub result: ModuleResult,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModuleFailedEvent {
    pub module: ModuleRef,
    pub error: String,
    /// True when the whole run must abort; false when the run may continue with
    /// a `Partial` result.
    pub fatal: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScoreEvent {
    /// Version of the scoring model that produced these numbers.
    pub scoring_model: String,
    /// True while modules are still outstanding. Provisional scores must be
    /// rendered as such and must never be exported as a final score.
    pub provisional: bool,
    pub total: Option<f64>,
    pub categories: Vec<CategoryScore>,
    /// Set when the model itself is not yet calibrated against a reference
    /// system. Consumers must surface this prominently.
    pub uncalibrated: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CategoryScore {
    pub key: String,
    pub label: String,
    pub score: f64,
    /// Contribution weight of this category within the total, in `[0, 1]`.
    pub weight: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReportGenerated {
    pub formats: Vec<String>,
    pub bundle_sha256: String,
    pub bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RunCompleted {
    pub state: RunState,
    pub verdict: Verdict,
    pub duration_ms: u64,
    pub modules_completed: u32,
    pub modules_failed: u32,
    /// Number of events emitted, so a consumer can prove it saw all of them.
    pub final_seq: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RunInvalidated {
    pub verdict: Verdict,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Heartbeat {
    pub state: RunState,
    /// Highest `seq` the agent has emitted for this run so far.
    pub last_seq: u64,
}

#[cfg(test)]
// In tests, `unwrap`/`expect` panicking *is* the failure signal.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic_in_result_fn)]
mod tests {
    use super::*;
    use crate::ids::ModuleId;
    use crate::PROTOCOL_VERSION;

    fn envelope(event: Event) -> Envelope {
        Envelope {
            protocol: PROTOCOL_VERSION.to_string(),
            run_id: RunId::try_new().expect("id"),
            seq: 7,
            ts: chrono::Utc::now(),
            mono_ms: 1234,
            event,
        }
    }

    #[test]
    fn envelope_roundtrips_and_is_internally_tagged() {
        let env = envelope(Event::ModuleSample(ModuleSampleEvent {
            module: ModuleId::new("cpu.mixed").expect("id"),
            metric_key: "crypto_sha256.single".into(),
            rep: 2,
            warmup: false,
            value: 812.5,
            unit: "MiB/s".into(),
            duration_ms: 301.2,
            module_progress: 0.4,
        }));

        let json = serde_json::to_value(&env).expect("ser");
        assert_eq!(json["type"], "module.sample");
        assert_eq!(json["seq"], 7);
        assert_eq!(json["protocol"], PROTOCOL_VERSION);
        // Flattened: payload fields sit alongside the envelope fields.
        assert_eq!(json["metric_key"], "crypto_sha256.single");

        let back: Envelope = serde_json::from_value(json).expect("de");
        assert_eq!(back, env);
    }

    #[test]
    fn every_variant_reports_a_stable_kind() {
        let env = envelope(Event::Heartbeat(Heartbeat {
            state: RunState::Running,
            last_seq: 3,
        }));
        assert_eq!(env.kind(), "stream.heartbeat");
        assert!(!env.event.is_stream_terminal());
    }

    #[test]
    fn run_completed_is_stream_terminal() {
        let event = Event::RunCompleted(RunCompleted {
            state: RunState::Completed,
            verdict: Verdict {
                state: crate::run::ResultState::Local,
                reasons: vec![],
                validator_version: "0.1.0".into(),
            },
            duration_ms: 10,
            modules_completed: 1,
            modules_failed: 0,
            final_seq: 42,
        });
        assert!(event.is_stream_terminal());
    }

    #[test]
    fn unknown_fields_within_a_known_version_are_tolerated() {
        // Forward compatibility: an older client must not choke on a newer
        // agent adding fields.
        let json = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "run_id": RunId::try_new().expect("id").as_str(),
            "seq": 1,
            "ts": "2026-08-03T10:00:00Z",
            "mono_ms": 5,
            "type": "stream.heartbeat",
            "state": "running",
            "last_seq": 1,
            "future_field_from_a_newer_agent": {"nested": true}
        });
        let decoded: Envelope = serde_json::from_value(json).expect("tolerant decode");
        assert_eq!(decoded.kind(), "stream.heartbeat");
    }
}
