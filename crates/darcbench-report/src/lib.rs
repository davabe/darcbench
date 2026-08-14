//! Result bundles: the unit of evidence DARCBench produces.
//!
//! A bundle contains the raw metrics, the environment they were measured in,
//! the derived scores, the verdict, and a signature over all of it. It is what
//! gets written to disk, uploaded, shared and re-scored.
//!
//! # Trust model in one paragraph
//!
//! A signature proves a bundle was produced by *a* DARCBench agent holding
//! *some* key and has not been edited since. It does **not** prove the numbers
//! are real, because the operator controls the machine and could patch the
//! agent. That is why [`darcbench_protocol::ResultState::SelfReported`] is the
//! ceiling for a locally-signed bundle, and why higher tiers require evidence
//! the operator cannot produce alone (a server-issued nonce, a matching release
//! build hash). See `docs/adr/0008-result-verification.md`.

pub mod bundle;
pub mod canonical;
pub mod diagnosis;
pub mod html;
pub mod signing;
pub mod validate;

pub use bundle::{Bundle, BundleMeta, RunRecord, TelemetrySummary};
pub use canonical::{canonical_json, CANONICALIZATION};
pub use diagnosis::{diagnose, SustainedCause, SustainedDiagnosis};
pub use signing::{AgentKey, Signature, SigningError};
pub use validate::{validate_bundle, ValidationOutcome};
