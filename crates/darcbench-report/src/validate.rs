//! Bundle validation: the rules that decide what a result is allowed to claim.
//!
//! The same ruleset runs in two places, which is the point:
//!
//! * **In the agent**, so an operator sees immediately why their run is only
//!   `Partial` and does not discover it after uploading.
//! * **In the control plane**, where it is authoritative. The server never
//!   trusts the verdict inside an uploaded bundle; it recomputes it.
//!
//! Recomputation is what makes editing a bundle pointless: the scores are a
//! pure function of the raw metrics, so changing a score without changing the
//! metrics is detected, and changing the metrics breaks the signature.

use darcbench_protocol::metrics::{ModuleStatus, Warning, WarningCode};
use darcbench_protocol::{ResultState, RunState, Verdict, VerdictReason};
use darcbench_scoring::{CategoryKey, ScoringModel};

use crate::bundle::Bundle;

/// Version of this ruleset. Bundles record which validator judged them.
pub const VALIDATOR_VERSION: &str = "dbv/0.1.0";

/// Maximum coefficient of variation tolerated before a module's contribution
/// is considered too noisy for a comparable score.
const MAX_ACCEPTABLE_CV: f64 = 0.25;

#[derive(Clone, Debug, PartialEq)]
pub struct ValidationOutcome {
    pub verdict: Verdict,
    /// True when scores recomputed from the raw metrics matched the ones in
    /// the bundle. `None` when recomputation was not attempted - which, on the
    /// server path, only happens for a scoring model this build does not
    /// implement, and is itself fatal.
    pub recomputation_matched: Option<bool>,
    /// Name of the first score field that disagreed with recomputation, so a
    /// rejection can say *what* did not add up.
    pub mismatch_field: Option<String>,
}

/// Applies the ruleset to a bundle.
///
/// `server_side` selects the stricter path: it additionally requires a valid
/// signature and score recomputation, and can award states above
/// `SelfReported`. The agent calls it with `false`.
pub fn validate_bundle(bundle: &Bundle, server_side: bool) -> ValidationOutcome {
    let mut reasons: Vec<VerdictReason> = Vec::new();
    let mut fatal = false;

    // --- run completion -------------------------------------------------
    match bundle.run.state {
        RunState::Completed => {}
        RunState::Cancelled => {
            reasons.push(VerdictReason::Interrupted);
            fatal = true;
        }
        _ => {
            reasons.push(VerdictReason::Interrupted);
            fatal = true;
        }
    }

    // --- protocol / schema compatibility ---------------------------------
    if bundle.meta.schema != darcbench_protocol::BUNDLE_SCHEMA_VERSION {
        reasons.push(VerdictReason::IncompatibleBenchmarkVersion {
            found: bundle.meta.schema.clone(),
            expected: darcbench_protocol::BUNDLE_SCHEMA_VERSION.to_string(),
        });
        fatal = true;
    }

    // --- build profile ----------------------------------------------------
    // A debug build runs several times slower than a release build. Comparing
    // the two would be meaningless, so a debug bundle is never comparable.
    if bundle.meta.build_profile != "release" {
        reasons.push(VerdictReason::IncompatibleBenchmarkVersion {
            found: format!("build_profile={}", bundle.meta.build_profile),
            expected: "build_profile=release".to_string(),
        });
    }

    // --- module outcomes --------------------------------------------------
    for module in &bundle.modules {
        match module.status {
            ModuleStatus::Completed => {}
            ModuleStatus::Degraded => {
                // Report *why* it was degraded. A module can now be degraded by
                // any measurement-invalidating warning, so calling every one of
                // them "excessive variance" would tell a reader to re-run on a
                // quieter machine when the real answer might be that this
                // machine cannot produce a comparable number at all.
                reasons.push(degradation_reason(module));
            }
            ModuleStatus::Failed => {
                reasons.push(VerdictReason::ModuleFailed {
                    module: module.module.id.clone(),
                });
            }
            ModuleStatus::Cancelled => {
                reasons.push(VerdictReason::Interrupted);
                fatal = true;
            }
            ModuleStatus::Skipped => {
                reasons.push(VerdictReason::RequiredModuleMissing {
                    module: module.module.id.clone(),
                });
            }
        }

        for metric in &module.metrics {
            if let Some(cv) = metric.summary.cv {
                if cv > MAX_ACCEPTABLE_CV {
                    reasons.push(VerdictReason::ExcessiveVariance {
                        module: module.module.id.clone(),
                    });
                    break;
                }
            }
        }
    }

    // --- required metadata -------------------------------------------------
    for (field, present) in [
        (
            "environment.cpu.logical_cpus",
            bundle.environment.cpu.logical_cpus > 0,
        ),
        (
            "environment.memory.total_bytes",
            bundle.environment.memory.total_bytes > 0,
        ),
        (
            "run.environment_digest",
            !bundle.run.environment_digest.is_empty(),
        ),
        ("run.events_digest", !bundle.run.events_digest.is_empty()),
    ] {
        if !present {
            reasons.push(VerdictReason::MissingMetadata {
                field: field.to_string(),
            });
        }
    }

    // --- clock sanity ------------------------------------------------------
    if bundle.run.finished_at < bundle.run.started_at {
        reasons.push(VerdictReason::ClockAnomaly);
        fatal = true;
    }

    // --- profile -----------------------------------------------------------
    if !bundle.run.profile.is_standard() {
        reasons.push(VerdictReason::CustomProfile);
    }

    // --- signature and recomputation (server path only) ---------------------
    //
    // This runs *before* the eligibility checks below, because on the server
    // path the recomputed score card - not the one in the bundle - is what
    // those checks must be based on.
    let mut recomputation_matched = None;
    let mut mismatch_field = None;
    let mut recomputed_scores = None;

    if server_side {
        if bundle.verify_signature().is_err() {
            reasons.push(VerdictReason::SignatureInvalid);
            fatal = true;
        }

        let model = ScoringModel::current();
        if model.version == bundle.scores.scoring_model {
            let recomputed = model.score_run(bundle.run.profile, &bundle.modules);
            match scores_match(&recomputed, &bundle.scores) {
                Ok(()) => recomputation_matched = Some(true),
                Err(field) => {
                    reasons.push(VerdictReason::ScoreRecomputationMismatch);
                    mismatch_field = Some(field.to_string());
                    recomputation_matched = Some(false);
                    fatal = true;
                }
            }
            recomputed_scores = Some(recomputed);
        } else {
            // A model this server does not implement cannot be recomputed, and
            // a score the server cannot recompute has not been validated by
            // anything. Falling through to `Validated` here would make
            // arbitrary numbers rankable simply by naming an unknown model, so
            // an unrecognised model is fatal rather than merely unchecked.
            reasons.push(VerdictReason::IncompatibleBenchmarkVersion {
                found: bundle.scores.scoring_model.clone(),
                expected: model.version.clone(),
            });
            fatal = true;
        }
    }

    // --- missing required categories ---------------------------------------
    //
    // Derived from the recomputed card when there is one. Trusting the
    // bundle's own `missing_required_categories` would let an uploader clear
    // the list, keep every score identical, re-sign, and dodge the `Partial`
    // downgrade.
    let authoritative_scores = recomputed_scores.as_ref().unwrap_or(&bundle.scores);
    let missing_required = !authoritative_scores.missing_required_categories.is_empty();
    for category in &authoritative_scores.missing_required_categories {
        // Represented as a missing module because that is what a reader needs
        // to act on: the category is empty because nothing filled it.
        if let Ok(id) = darcbench_protocol::ModuleId::new(placeholder_module_for(*category)) {
            reasons.push(VerdictReason::RequiredModuleMissing { module: id });
        }
    }

    // A verdict is a set of distinct facts about why a run is what it is, and a
    // human reads it. The same reason can be reached by two routes - a module
    // degraded by high variance, and a metric of that module also exceeding the
    // validator's own CV ceiling - and listing it twice adds no information.
    // Stable dedup, so the agent and the server produce byte-identical verdicts.
    let mut seen: Vec<VerdictReason> = Vec::with_capacity(reasons.len());
    for reason in reasons {
        if !seen.contains(&reason) {
            seen.push(reason);
        }
    }
    let reasons = seen;

    let state = if fatal {
        ResultState::Invalid
    } else if !bundle.run.profile.is_standard() {
        ResultState::Custom
    } else if missing_required || reasons.iter().any(is_partial_reason) {
        ResultState::Partial
    } else if server_side {
        ResultState::Validated
    } else {
        bundle.local_result_state()
    };

    ValidationOutcome {
        verdict: Verdict {
            state,
            reasons,
            validator_version: VALIDATOR_VERSION.to_string(),
        },
        recomputation_matched,
        mismatch_field,
    }
}

/// Reasons that make a run unrankable.
///
/// Written as an exhaustive `match` rather than a `matches!` list, because the
/// list form let two reasons fall through it silently.
/// [`VerdictReason::LoadGeneratorSaturated`] was one of them: `degradation_reason`
/// produces it for a degraded module, and a degraded module is meant to make
/// the run `Partial` - but the reason it produced was not in the list, so a run
/// whose only fault was a saturated injector came out `Validated`. That is
/// precisely the outcome `docs/BENCHMARK-METHODOLOGY.md` forbids when it says
/// `GeneratorSaturated` "invalidates the result", and it would have made
/// Phase 3's exit criterion false while every individual piece looked right.
///
/// "Invalidates" there is the methodology's loose phrasing; §6 of the same
/// document is the precise rule, and it puts a degraded module under `Partial`.
/// The distinction matters: `Invalid` is for a run whose evidence cannot be
/// trusted at all - cancelled, clock-anomalous, badly signed - and a saturated
/// generator produced honest measurements of the wrong thing. They are kept and
/// they are not ranked.
fn is_partial_reason(reason: &VerdictReason) -> bool {
    match reason {
        VerdictReason::RequiredModuleMissing { .. }
        | VerdictReason::ModuleFailed { .. }
        | VerdictReason::ExcessiveVariance { .. }
        | VerdictReason::ModuleDegraded { .. }
        | VerdictReason::LoadGeneratorSaturated { .. }
        | VerdictReason::ErrorRateExceeded
        | VerdictReason::IncompatibleBenchmarkVersion { .. }
        | VerdictReason::MissingMetadata { .. } => true,
        // Fatal on their own path, so they never need to be counted here: by
        // the time one of these is present, `fatal` is already set and the
        // state is `Invalid` regardless of what this function says.
        VerdictReason::Interrupted
        | VerdictReason::ClockAnomaly
        | VerdictReason::SignatureInvalid
        | VerdictReason::ScoreRecomputationMismatch
        | VerdictReason::ReplayDetected
        | VerdictReason::EnvironmentChangedMidRun
        | VerdictReason::InsufficientDiskSpace => false,
        // Not a fault. A custom profile is unrankable for its own reason, which
        // `validate_bundle` applies before this is consulted.
        VerdictReason::CustomProfile => false,
    }
}

/// Why a degraded module was degraded, phrased from its own warnings.
///
/// High variance keeps its dedicated reason because it is the common case and
/// has a clear remedy - run it again on a quieter machine. Anything else is
/// reported with the module's own message, because the module is the only thing
/// that knows what condition it failed.
fn degradation_reason(module: &darcbench_protocol::ModuleResult) -> VerdictReason {
    let id = module.module.id.clone();
    let mut declared: Option<&Warning> = None;
    for warning in &module.warnings {
        match warning.code {
            WarningCode::HighVariance => return VerdictReason::ExcessiveVariance { module: id },
            WarningCode::GeneratorSaturated => {
                return VerdictReason::LoadGeneratorSaturated { module: id }
            }
            code if code.degrades_result() => declared = declared.or(Some(warning)),
            _ => {}
        }
    }
    match declared {
        Some(warning) => VerdictReason::ModuleDegraded {
            module: id,
            reason: warning.message.clone(),
        },
        // Degraded with no degrading warning should be unreachable, but a
        // verdict that says nothing is worse than one that says "unspecified".
        None => VerdictReason::ModuleDegraded {
            module: id,
            reason: "the module reported itself degraded without a reason".to_string(),
        },
    }
}

/// The module expected to fill a category, used to phrase "required module
/// missing" in terms a reader can act on.
fn placeholder_module_for(category: CategoryKey) -> &'static str {
    match category {
        CategoryKey::Compute => "cpu.mixed",
        CategoryKey::Memory => "memory.bandwidth",
        CategoryKey::Storage => "storage.mixed",
        CategoryKey::Network => "network.transfer",
        CategoryKey::Web => "web.static",
        CategoryKey::Database => "database.oltp",
        CategoryKey::Deployment => "deployment.container",
    }
}

/// Compares a recomputed score card against the one in the bundle.
///
/// Returns the name of the first field that disagrees, so a mismatch is
/// actionable rather than an opaque rejection.
///
/// **Every field is compared.** Checking only the headline numbers would leave
/// the rest of the card trusted from the bundle, and several of those fields
/// drive eligibility: clearing `missing_required_categories` while keeping the
/// totals identical would dodge the `Partial` downgrade entirely. If a field
/// exists on `ScoreCard`, it is either compared here or is a copy of an input
/// that is compared elsewhere.
///
/// Numeric fields use a relative tolerance rather than exact equality:
/// recomputation happens on a different machine, and `ln`/`exp` are not
/// required to be bit-identical across libm implementations. Structural fields
/// (booleans, keys, category lists) are compared exactly, because there is no
/// legitimate reason for those to drift.
fn scores_match(
    recomputed: &darcbench_scoring::ScoreCard,
    claimed: &darcbench_scoring::ScoreCard,
) -> Result<(), &'static str> {
    const TOLERANCE: f64 = 1e-6;
    let close = |x: Option<f64>, y: Option<f64>| match (x, y) {
        (None, None) => true,
        (Some(x), Some(y)) => (x - y).abs() <= TOLERANCE * x.abs().max(1.0),
        _ => false,
    };
    let check = |ok: bool, field: &'static str| if ok { Ok(()) } else { Err(field) };

    // --- identity ------------------------------------------------------
    check(
        recomputed.scoring_model == claimed.scoring_model,
        "scoring_model",
    )?;
    check(
        recomputed.reference_profile == claimed.reference_profile,
        "reference_profile",
    )?;
    check(
        recomputed.uncalibrated == claimed.uncalibrated,
        "uncalibrated",
    )?;
    check(recomputed.profile == claimed.profile, "profile")?;

    // --- headline ------------------------------------------------------
    check(close(recomputed.total, claimed.total), "total")?;
    check(
        recomputed.total_is_standard == claimed.total_is_standard,
        "total_is_standard",
    )?;
    check(
        close(recomputed.uncapped_total, claimed.uncapped_total),
        "uncapped_total",
    )?;
    check(
        recomputed.weak_link_applied == claimed.weak_link_applied,
        "weak_link_applied",
    )?;
    check(
        close(recomputed.balance_index, claimed.balance_index),
        "balance_index",
    )?;

    // --- stability and efficiency ---------------------------------------
    check(
        close(
            Some(recomputed.stability_score),
            Some(claimed.stability_score),
        ),
        "stability_score",
    )?;
    check(
        close(
            Some(recomputed.stability_index),
            Some(claimed.stability_index),
        ),
        "stability_index",
    )?;
    check(
        close(
            Some(recomputed.stability_multiplier),
            Some(claimed.stability_multiplier),
        ),
        "stability_multiplier",
    )?;
    check(
        close(recomputed.efficiency_score, claimed.efficiency_score),
        "efficiency_score",
    )?;
    check(close(recomputed.median_cv, claimed.median_cv), "median_cv")?;
    check(
        recomputed.instability_flags == claimed.instability_flags,
        "instability_flags",
    )?;

    // --- categories ------------------------------------------------------
    check(
        recomputed.categories.len() == claimed.categories.len(),
        "categories.len",
    )?;
    for (a, b) in recomputed.categories.iter().zip(&claimed.categories) {
        check(a.key == b.key, "categories.key")?;
        check(close(Some(a.score), Some(b.score)), "categories.score")?;
        check(close(Some(a.weight), Some(b.weight)), "categories.weight")?;
        check(a.metric_count == b.metric_count, "categories.metric_count")?;
    }

    // --- facets ------------------------------------------------------------
    check(
        recomputed.facets.len() == claimed.facets.len(),
        "facets.len",
    )?;
    for ((ka, va), (kb, vb)) in recomputed.facets.iter().zip(&claimed.facets) {
        check(ka == kb, "facets.key")?;
        check(close(Some(*va), Some(*vb)), "facets.score")?;
    }

    // --- composites ----------------------------------------------------------
    check(
        recomputed.composites.len() == claimed.composites.len(),
        "composites.len",
    )?;
    for (a, b) in recomputed.composites.iter().zip(&claimed.composites) {
        check(a.key == b.key, "composites.key")?;
        check(close(Some(a.score), Some(b.score)), "composites.score")?;
        check(
            close(Some(a.weight_coverage), Some(b.weight_coverage)),
            "composites.coverage",
        )?;
    }

    // --- sustained performance --------------------------------------------------
    //
    // The endurance profile's headline number, and for an endurance run the one
    // most worth faking: it is the whole answer to "does this machine hold up".
    // It went uncompared for a while, which meant an otherwise honest bundle
    // could claim it never declined and be recomputed, matched and marked
    // `Validated` - the exact failure the recomputation exists to prevent.
    match (&recomputed.sustained, &claimed.sustained) {
        (None, None) => {}
        (Some(recomputed), Some(claimed)) => {
            check(
                close(Some(recomputed.retention), Some(claimed.retention)),
                "sustained.retention",
            )?;
            check(
                close(Some(recomputed.score), Some(claimed.score)),
                "sustained.score",
            )?;
            check(recomputed.cycles == claimed.cycles, "sustained.cycles")?;
            check(
                recomputed.scored_cycle == claimed.scored_cycle,
                "sustained.scored_cycle",
            )?;
            // Per-metric retention is compared key by key, not merely by
            // length: it is the evidence a reader uses to see *what* declined,
            // and a bundle that named the wrong subsystem would be as
            // misleading as one that faked the headline.
            check(
                recomputed.by_metric.len() == claimed.by_metric.len(),
                "sustained.by_metric.len",
            )?;
            for ((ka, va), (kb, vb)) in recomputed.by_metric.iter().zip(&claimed.by_metric) {
                check(ka == kb, "sustained.by_metric.key")?;
                check(close(Some(*va), Some(*vb)), "sustained.by_metric.value")?;
            }
        }
        // A run that claims retention the recomputation cannot produce, or that
        // hides one the recomputation does, is not the run it says it is.
        _ => return Err("sustained.presence"),
    }

    // --- eligibility-bearing fields --------------------------------------------
    check(
        recomputed.missing_required_categories == claimed.missing_required_categories,
        "missing_required_categories",
    )?;
    check(
        recomputed.unreferenced_metrics == claimed.unreferenced_metrics,
        "unreferenced_metrics",
    )?;

    Ok(())
}

#[cfg(test)]
// In tests, `unwrap`/`expect` panicking *is* the failure signal.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic_in_result_fn)]
mod tests {
    use super::*;
    use crate::bundle::{BundleMeta, RunRecord, TelemetrySummary};
    use darcbench_inventory::Inventory;
    use darcbench_protocol::{Profile, RunId};

    fn bundle_with(state: RunState, profile: Profile) -> Bundle {
        let now = chrono::Utc::now();
        let inventory = Inventory::collect();
        let mut meta = BundleMeta::new("0.1.0-test");
        // Tests run under `cargo test`, which is a debug build unless
        // --release is used; pin it so the assertions are about the rules
        // under test rather than about how the suite was invoked.
        meta.build_profile = "release".into();
        Bundle {
            meta,
            run: RunRecord {
                run_id: RunId::try_new().expect("id"),
                profile,
                state,
                started_at: now,
                finished_at: now,
                duration_ms: 1000,
                modules: vec![],
                environment_digest: inventory.performance_digest(),
                events_digest: "sha256:abc".into(),
                event_count: 10,
                stopped_because: None,
                guards_not_enforced: vec![],
            },
            environment: inventory,
            modules: vec![],
            scores: ScoringModel::current().score_run(profile, &[]),
            verdict: Verdict {
                state: ResultState::Local,
                reasons: vec![],
                validator_version: VALIDATOR_VERSION.into(),
            },
            telemetry: TelemetrySummary::default(),
            sustained_diagnosis: None,
            signature: None,
        }
    }

    /// The same fact must appear once, however many checks reached it.
    ///
    /// A module degraded by high variance and a metric of that same module
    /// exceeding the validator's CV ceiling are two routes to one conclusion.
    /// Reporting it twice tells a reader nothing and looks like two problems.
    #[test]
    fn verdict_reasons_are_deduplicated() {
        use darcbench_protocol::metrics::{Metric, ModuleResult, Warning, WarningCode};
        use darcbench_protocol::stats::Summary;

        let mut bundle = bundle_with(RunState::Completed, Profile::Standard);
        let noisy = Metric {
            key: "demo.metric".into(),
            label: "Demo".into(),
            unit: "IOPS".into(),
            direction: darcbench_protocol::Direction::HigherIsBetter,
            value: 100.0,
            // Well above MAX_ACCEPTABLE_CV, so the per-metric check also fires.
            summary: Summary {
                n: 5,
                cv: Some(0.9),
                ..Default::default()
            },
            samples: vec![],
            outliers: vec![],
        };
        let module_ref = darcbench_protocol::ModuleRef {
            id: darcbench_protocol::ModuleId::new("cpu.mixed").expect("id"),
            version: "1.0.0".into(),
        };
        bundle.modules.push(ModuleResult {
            module: module_ref.clone(),
            status: darcbench_protocol::metrics::ModuleStatus::Degraded,
            cycle: 0,
            started_at: bundle.run.started_at,
            finished_at: bundle.run.finished_at,
            duration_ms: 1.0,
            metrics: vec![noisy],
            warnings: vec![Warning {
                code: WarningCode::HighVariance,
                message: "noisy".into(),
                metric_key: None,
            }],
            error: None,
            context: Default::default(),
        });

        let reasons = validate_bundle(&bundle, false).verdict.reasons;
        let variance = reasons
            .iter()
            .filter(|r| matches!(r, VerdictReason::ExcessiveVariance { module } if *module == module_ref.id))
            .count();
        assert_eq!(
            variance, 1,
            "both routes reached the same conclusion; it belongs in the verdict once: {reasons:?}"
        );
    }

    #[test]
    fn a_cancelled_run_is_invalid() {
        let outcome = validate_bundle(&bundle_with(RunState::Cancelled, Profile::Standard), false);
        assert_eq!(outcome.verdict.state, ResultState::Invalid);
        assert!(outcome
            .verdict
            .reasons
            .contains(&VerdictReason::Interrupted));
    }

    #[test]
    fn a_custom_profile_is_always_custom_never_ranked() {
        let outcome = validate_bundle(&bundle_with(RunState::Completed, Profile::Custom), false);
        assert_eq!(outcome.verdict.state, ResultState::Custom);
        assert!(!outcome.verdict.state.is_rankable());
        assert!(outcome
            .verdict
            .reasons
            .contains(&VerdictReason::CustomProfile));
    }

    #[test]
    fn a_run_missing_required_categories_is_partial() {
        let outcome = validate_bundle(&bundle_with(RunState::Completed, Profile::Standard), false);
        assert_eq!(outcome.verdict.state, ResultState::Partial);
        assert!(!outcome.verdict.state.is_rankable());
    }

    /// Phase 3's exit criterion, as a test: a saturated generator provably
    /// makes a result unrankable.
    ///
    /// Every piece of this existed separately and the whole did not work.
    /// `GeneratorSaturated` was in `degrades_result`, `degradation_reason`
    /// translated it to `LoadGeneratorSaturated`, and `is_partial_reason` did
    /// not list it - so a run whose only fault was an injector that could not
    /// keep up came out `Validated`. The gap was invisible from any one of the
    /// three files.
    #[test]
    fn a_saturated_load_generator_makes_the_run_unrankable() {
        let mut bundle = bundle_with(RunState::Completed, Profile::Standard);
        bundle
            .modules
            .push(degraded_module(WarningCode::GeneratorSaturated));
        // Everything a rankable run needs, so the verdict below can only come
        // from the saturation itself.
        bundle.scores.missing_required_categories.clear();

        let outcome = validate_bundle(&bundle, false);
        assert!(
            outcome
                .verdict
                .reasons
                .iter()
                .any(|reason| matches!(reason, VerdictReason::LoadGeneratorSaturated { .. })),
            "the verdict must name the saturation: {:?}",
            outcome.verdict.reasons
        );
        assert_eq!(outcome.verdict.state, ResultState::Partial);
        assert!(
            !outcome.verdict.state.is_rankable(),
            "a latency distribution recorded while the injector could not keep up describes the \
             injector, and must never be ranked"
        );
    }

    /// The companion hole, closed at the same time and for the same reason.
    #[test]
    fn an_excessive_error_rate_is_also_unrankable() {
        assert!(is_partial_reason(&VerdictReason::ErrorRateExceeded));
    }

    fn degraded_module(code: WarningCode) -> darcbench_protocol::ModuleResult {
        let now = chrono::Utc::now();
        darcbench_protocol::ModuleResult {
            module: darcbench_protocol::ModuleRef {
                id: darcbench_protocol::ModuleId::new("web.static").unwrap(),
                version: "1.0.0".into(),
            },
            status: ModuleStatus::Degraded,
            cycle: 0,
            started_at: now,
            finished_at: now,
            duration_ms: 1.0,
            metrics: vec![],
            warnings: vec![Warning {
                code,
                message: "the load generator, not the system under test, was the bottleneck".into(),
                metric_key: None,
            }],
            error: None,
            context: Default::default(),
        }
    }

    #[test]
    fn a_backwards_clock_invalidates_the_run() {
        let mut bundle = bundle_with(RunState::Completed, Profile::Standard);
        bundle.run.finished_at = bundle.run.started_at - chrono::Duration::seconds(60);
        let outcome = validate_bundle(&bundle, false);
        assert_eq!(outcome.verdict.state, ResultState::Invalid);
        assert!(outcome
            .verdict
            .reasons
            .contains(&VerdictReason::ClockAnomaly));
    }

    #[test]
    fn a_debug_build_is_never_comparable() {
        let mut bundle = bundle_with(RunState::Completed, Profile::Standard);
        bundle.meta.build_profile = "debug".into();
        let outcome = validate_bundle(&bundle, false);
        assert!(outcome.verdict.reasons.iter().any(|r| matches!(
            r,
            VerdictReason::IncompatibleBenchmarkVersion { found, .. } if found.contains("debug")
        )));
        assert!(!outcome.verdict.state.is_rankable());
    }

    #[test]
    fn an_incompatible_schema_is_invalid() {
        let mut bundle = bundle_with(RunState::Completed, Profile::Standard);
        bundle.meta.schema = "darcbench.bundle/99".into();
        assert_eq!(
            validate_bundle(&bundle, false).verdict.state,
            ResultState::Invalid
        );
    }

    #[test]
    fn server_side_validation_rejects_a_missing_signature() {
        let bundle = bundle_with(RunState::Completed, Profile::Standard);
        let outcome = validate_bundle(&bundle, true);
        assert_eq!(outcome.verdict.state, ResultState::Invalid);
        assert!(outcome
            .verdict
            .reasons
            .contains(&VerdictReason::SignatureInvalid));
    }

    #[test]
    fn server_side_validation_recomputes_scores_and_detects_edits() {
        let key = crate::signing::AgentKey::generate().expect("keygen");
        let mut bundle = bundle_with(RunState::Completed, Profile::Standard);
        // Edit the score, then sign - the signature is valid, but the score no
        // longer follows from the raw metrics.
        bundle.scores.total = Some(999_999.0);
        bundle.sign(&key).expect("sign");
        bundle.verify_signature().expect("signature itself is fine");

        let outcome = validate_bundle(&bundle, true);
        assert_eq!(outcome.recomputation_matched, Some(false));
        assert!(outcome
            .verdict
            .reasons
            .contains(&VerdictReason::ScoreRecomputationMismatch));
        assert_eq!(outcome.verdict.state, ResultState::Invalid);
    }

    #[test]
    fn an_honest_signed_bundle_passes_recomputation() {
        let key = crate::signing::AgentKey::generate().expect("keygen");
        let mut bundle = bundle_with(RunState::Completed, Profile::Standard);
        bundle.sign(&key).expect("sign");
        let outcome = validate_bundle(&bundle, true);
        assert_eq!(outcome.recomputation_matched, Some(true));
        assert!(!outcome
            .verdict
            .reasons
            .contains(&VerdictReason::ScoreRecomputationMismatch));
        // Still Partial, because a compute-only run is not a standard total.
        assert_eq!(outcome.verdict.state, ResultState::Partial);
    }

    /// An unknown scoring model must not fall through to `Validated`.
    ///
    /// Regression: recomputation used to be skipped for an unrecognised model,
    /// leaving no downgrade, so a signed bundle carrying arbitrary scores under
    /// a made-up model version became rankable.
    #[test]
    fn an_unknown_scoring_model_is_rejected_not_waved_through() {
        let key = crate::signing::AgentKey::generate().expect("keygen");
        let mut bundle = bundle_with(RunState::Completed, Profile::Standard);
        bundle.scores.scoring_model = "dbs/99.0.0-fictional".into();
        bundle.scores.total = Some(999_999.0);
        bundle.scores.missing_required_categories.clear();
        bundle.sign(&key).expect("sign");
        bundle
            .verify_signature()
            .expect("the signature itself is valid");

        let outcome = validate_bundle(&bundle, true);
        assert_eq!(
            outcome.verdict.state,
            ResultState::Invalid,
            "a score the server cannot recompute must never be Validated"
        );
        assert!(!outcome.verdict.state.is_rankable());
        assert!(outcome.verdict.reasons.iter().any(|r| matches!(
            r,
            VerdictReason::IncompatibleBenchmarkVersion { found, .. }
                if found == "dbs/99.0.0-fictional"
        )));
        assert_eq!(outcome.recomputation_matched, None);
    }

    /// Clearing `missing_required_categories` must not dodge the downgrade.
    ///
    /// Regression: recomputation only compared total, category scores and
    /// stability, so an uploader could empty the missing-categories list, keep
    /// every number identical, re-sign, and turn a `Partial` run into
    /// `Validated`.
    #[test]
    fn clearing_missing_categories_is_detected() {
        let key = crate::signing::AgentKey::generate().expect("keygen");
        let mut bundle = bundle_with(RunState::Completed, Profile::Standard);
        assert!(
            !bundle.scores.missing_required_categories.is_empty(),
            "precondition: this fixture is genuinely missing categories"
        );
        bundle.scores.missing_required_categories.clear();
        bundle.sign(&key).expect("sign");

        let outcome = validate_bundle(&bundle, true);
        assert_eq!(outcome.recomputation_matched, Some(false));
        assert_eq!(
            outcome.mismatch_field.as_deref(),
            Some("missing_required_categories")
        );
        assert_eq!(outcome.verdict.state, ResultState::Invalid);
    }

    /// Even if recomputation somehow agreed, eligibility is decided from the
    /// recomputed card rather than the bundle's own claim.
    #[test]
    fn eligibility_uses_the_recomputed_card() {
        let key = crate::signing::AgentKey::generate().expect("keygen");
        let mut bundle = bundle_with(RunState::Completed, Profile::Standard);
        bundle.sign(&key).expect("sign");

        let outcome = validate_bundle(&bundle, true);
        assert_eq!(outcome.recomputation_matched, Some(true));
        // The recomputed card knows four categories are missing, so the run is
        // Partial even though nothing was tampered with.
        assert_eq!(outcome.verdict.state, ResultState::Partial);
        assert!(outcome
            .verdict
            .reasons
            .iter()
            .any(|r| matches!(r, VerdictReason::RequiredModuleMissing { .. })));
    }

    #[test]
    fn every_tampered_score_field_is_caught_and_named() {
        let key = crate::signing::AgentKey::generate().expect("keygen");
        let base = || {
            let mut b = bundle_with(RunState::Completed, Profile::Standard);
            b.sign(&key).expect("sign");
            b
        };

        let mut total = base();
        total.scores.total = Some(4242.0);
        assert_eq!(
            validate_bundle(&total, true).mismatch_field.as_deref(),
            Some("total")
        );

        let mut standard = base();
        standard.scores.total_is_standard = true;
        assert_eq!(
            validate_bundle(&standard, true).mismatch_field.as_deref(),
            Some("total_is_standard")
        );

        let mut stability = base();
        stability.scores.stability_score = 1000.0;
        assert_eq!(
            validate_bundle(&stability, true).mismatch_field.as_deref(),
            Some("stability_score")
        );

        let mut capped = base();
        capped.scores.weak_link_applied = !capped.scores.weak_link_applied;
        assert_eq!(
            validate_bundle(&capped, true).mismatch_field.as_deref(),
            Some("weak_link_applied")
        );

        let mut uncal = base();
        uncal.scores.uncalibrated = false;
        assert_eq!(
            validate_bundle(&uncal, true).mismatch_field.as_deref(),
            Some("uncalibrated")
        );

        let mut facets = base();
        facets.scores.facets.insert("single_core".into(), 9999.0);
        let outcome = validate_bundle(&facets, true);
        assert!(outcome
            .mismatch_field
            .as_deref()
            .is_some_and(|f| f.starts_with("facets")));

        // The endurance profile's headline. A run that never cycled cannot have
        // retained anything, so claiming it is the simplest possible forgery -
        // and it went undetected until this case existed.
        let mut sustained = base();
        sustained.scores.sustained = Some(darcbench_scoring::sustained::SustainedOutcome {
            retention: 1.0,
            score: 1000.0,
            cycles: 12,
            scored_cycle: 11,
            by_metric: Default::default(),
        });
        assert_eq!(
            validate_bundle(&sustained, true).mismatch_field.as_deref(),
            Some("sustained.presence"),
            "a fabricated Sustained Performance Score must not be recomputed as honest"
        );
    }

    /// A guard against the next field going uncompared.
    ///
    /// `scores_match` promises that every field on `ScoreCard` is checked, and
    /// that promise decayed silently once already: `sustained` was added to the
    /// card and not to the comparison, so an endurance run's headline number
    /// could be rewritten and still validate. Destructuring exhaustively here
    /// means adding a field to `ScoreCard` fails the build until somebody has
    /// looked at this list.
    #[test]
    fn every_score_card_field_is_accounted_for_in_the_comparison() {
        let card = ScoringModel::current().score_run(Profile::Standard, &[]);
        let darcbench_scoring::ScoreCard {
            scoring_model: _,
            reference_profile: _,
            uncalibrated: _,
            profile: _,
            total: _,
            total_is_standard: _,
            categories: _,
            facets: _,
            composites: _,
            stability_score: _,
            stability_index: _,
            stability_multiplier: _,
            uncapped_total: _,
            weak_link_applied: _,
            balance_index: _,
            efficiency_score: _,
            median_cv: _,
            instability_flags: _,
            missing_required_categories: _,
            unreferenced_metrics: _,
            sustained: _,
        } = card;
    }

    #[test]
    fn every_category_has_an_expected_module_name() {
        for category in CategoryKey::ALL {
            let name = placeholder_module_for(category);
            assert!(
                darcbench_protocol::ModuleId::new(name).is_ok(),
                "`{name}` is not a valid module id"
            );
        }
    }
}
