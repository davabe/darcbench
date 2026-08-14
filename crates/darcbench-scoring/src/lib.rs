//! The DARCBench scoring model.
//!
//! # Contract
//!
//! * Scores are **derived**, never stored as the source of truth. Given the raw
//!   [`ModuleResult`]s of a run and a [`ScoringModel`], `score_run` is a pure
//!   function. Any historical run can be rescored under a newer model.
//! * Scores are **versioned**. [`ScoringModel::version`] appears in every
//!   emitted score event and in every result bundle. Two runs are comparable
//!   only when profile *and* scoring model version match.
//! * Higher is always better, for every score in this crate.
//!
//! # Why a geometric mean
//!
//! Aggregating normalised ratios with an arithmetic mean lets one enormous
//! subsystem hide a catastrophic one: a machine with 40x reference network and
//! 0.05x reference storage would average out near reference. The geometric
//! mean is the standard defence, used by SPEC for exactly this reason
//! (<https://www.spec.org/cpu2017/Docs/overview.html>), and it is what
//! `stats::weighted_geometric_mean` implements. `stats` unit tests assert the
//! property directly.
//!
//! # Calibration status
//!
//! The reference values shipped in [`reference::provisional_reference`] are
//! **declared targets for the DARC-REF-1 reference specification, not measured
//! results**. Until a physical DARC-REF-1 host has been measured under the
//! procedure in `docs/SCORING-SYSTEM.md`, every score carries
//! `uncalibrated = true` and must be rendered as provisional. This crate has a
//! test that fails if that flag is ever silently cleared.

pub mod model;
pub mod reference;
pub mod sustained;

pub use model::{
    CategoryKey, CategoryOutcome, CompositeOutcome, ScoreCard, ScoringModel, REFERENCE_ANCHOR,
};
pub use reference::{ReferencePoint, ReferenceProfile};
pub use sustained::SustainedOutcome;

/// Identifier of the scoring model implemented by this build.
///
/// `-dev` marks a model whose reference profile has not been calibrated. It is
/// part of the version string on purpose: a bundle produced today can never be
/// mistaken for one produced by a calibrated release.
pub const SCORING_MODEL_VERSION: &str = "dbs/0.1.0-dev";

#[cfg(test)]
// In tests, `unwrap`/`expect` panicking *is* the failure signal.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic_in_result_fn)]
mod calibration_guard {
    use super::*;

    #[test]
    fn shipped_model_is_marked_uncalibrated() {
        let model = ScoringModel::current();
        assert!(
            !model.reference.calibrated,
            "the shipped reference profile must stay flagged uncalibrated until a physical \
             DARC-REF-1 host has been measured (docs/SCORING-SYSTEM.md)"
        );
        assert!(
            SCORING_MODEL_VERSION.ends_with("-dev"),
            "an uncalibrated model must keep the -dev suffix"
        );
    }
}
