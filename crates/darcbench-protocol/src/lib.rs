//! DARCBench wire protocol.
//!
//! This crate is the single source of truth for everything that crosses a
//! process or network boundary in DARCBench:
//!
//! * [`ids`]      - opaque, URL-safe identifiers for runs and modules.
//! * [`external`] - the consented two-machine load-generation session.
//! * [`metrics`]  - raw measurement records (the immutable evidence layer).
//! * [`stats`]    - the statistics used to turn samples into metrics.
//! * [`events`]   - the versioned real-time event stream.
//! * [`run`]      - run lifecycle, profiles, result states and verdicts.
//!
//! # Compatibility contract
//!
//! The protocol carries an explicit version string ([`PROTOCOL_VERSION`]).
//! Consumers MUST reject envelopes whose major version they do not understand
//! and MUST ignore unknown fields inside a known major version. See
//! `docs/REALTIME-PROTOCOL.md` for the full compatibility policy.
//!
//! # Deliberate non-goals
//!
//! There is no generic "command" or "exec" message in this protocol. The
//! browser and the control plane can only ask the agent to start, cancel or
//! report on a *run built from an allow-listed module set*. This is a security
//! property, not an oversight - see `docs/THREAT-MODEL.md` (T-AGENT-RCE).

pub mod events;
pub mod external;
pub mod ids;
pub mod metrics;
pub mod run;
pub mod stats;

pub use events::{Envelope, Event};
pub use ids::{ModuleId, ModuleRef, RunId};
pub use metrics::{Direction, Metric, MetricSample, ModuleResult};
pub use run::{
    Profile, ResultState, RunState, RunSummary, Verdict, VerdictReason, ENDURANCE_DEFAULT_MINUTES,
    ENDURANCE_MAX_MINUTES, ENDURANCE_MIN_MINUTES,
};

/// Version of the real-time event protocol produced by this crate.
///
/// Format: `darcbench.events/<major>`. A change to `<major>` is a breaking
/// change; additive fields do not bump it.
pub const PROTOCOL_VERSION: &str = "darcbench.events/1";

/// Version of the on-disk / uploaded result bundle schema.
pub const BUNDLE_SCHEMA_VERSION: &str = "darcbench.bundle/1";

/// Errors produced while validating protocol values.
#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("invalid identifier: {0}")]
    InvalidId(String),
    #[error("unsupported protocol version: {found} (this build speaks {expected})")]
    UnsupportedVersion { found: String, expected: String },
    #[error("serialization failed: {0}")]
    Serde(#[from] serde_json::Error),
}

/// Returns `Ok(())` when `found` is a protocol version this build can decode.
pub fn check_protocol_version(found: &str) -> Result<(), ProtocolError> {
    if found == PROTOCOL_VERSION {
        Ok(())
    } else {
        Err(ProtocolError::UnsupportedVersion {
            found: found.to_string(),
            expected: PROTOCOL_VERSION.to_string(),
        })
    }
}
