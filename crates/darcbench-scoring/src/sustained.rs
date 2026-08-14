//! Retention: how much of its opening performance a machine still had at the
//! end of a long run.
//!
//! # What this measures and why it is a separate number
//!
//! `docs/MARKET-RESEARCH.md` states the problem plainly: *"A 3-minute benchmark
//! on a T-series instance measures the credit balance, not the instance."* The
//! same is true of a thermally-limited mini PC, of a consumer SSD emptying its
//! SLC cache, and of a shared vCPU whose neighbour wakes up. All of them look
//! excellent for the first few minutes.
//!
//! The `endurance` profile answers that by repeating its whole module set in
//! **cycles** until a duration target elapses. This module compares the cycles.
//!
//! # Retention is drift; the coefficient of variation is noise
//!
//! Those are different findings and the model keeps them apart deliberately.
//!
//! Variation *within* a cycle is measurement noise - scheduler jitter, a
//! background daemon, the observer itself - and it feeds the stability
//! multiplier. Variation *across* cycles that points in one direction is not
//! noise: it is the machine getting slower, and averaging it into a CV would
//! report a machine that halved at minute forty as merely "unstable". A reader
//! told a machine is unstable will re-run it on a quieter host. A reader told a
//! machine retains 55% of its opening throughput has learned what they are
//! buying.
//!
//! Pooling would also punish the same fact twice: once through the stability
//! multiplier and once through this score.
//!
//! # Why the scored cycle is the last one
//!
//! An endurance run publishes its category scores from the **last complete
//! cycle**, not from an average over the run. Averaging burst and sustained
//! throughput produces a number that describes neither, and the number an
//! operator needs is the one they will live with once the credits are gone.

use std::collections::{BTreeMap, BTreeSet};

use darcbench_protocol::metrics::ModuleStatus;
use darcbench_protocol::{Direction, ModuleResult};
use serde::{Deserialize, Serialize};

/// Fewest cycles from which retention may be computed.
///
/// Two, because retention is a comparison.
pub const MIN_CYCLES_FOR_RETENTION: usize = 2;

/// Retention at or below which a run is reported as having declined.
///
/// 0.95 rather than 1.0 because a few per cent between the start and end of an
/// hour is ordinary: ambient temperature moves, a log rotates, the page cache
/// fills. Flagging that would make the finding meaningless through overuse.
pub const RETENTION_FLOOR: f64 = 0.95;

/// How a run's throughput held up across its cycles.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SustainedOutcome {
    /// Fraction of opening performance still present at the end, as a geometric
    /// mean over every metric measured in both windows. 1.0 is no decline.
    ///
    /// Unclamped, so the raw observation stays auditable even when the score
    /// derived from it is capped.
    pub retention: f64,
    /// `1000 x min(retention, 1)`. A machine that got *faster* scores 1000
    /// rather than more: this axis measures what was lost, and there is no such
    /// thing as losing a negative amount.
    pub score: f64,
    /// Number of cycles the run completed.
    pub cycles: usize,
    /// Cycle whose measurements the category scores were taken from.
    pub scored_cycle: u32,
    /// Per-metric retention, keyed `<module_id>/<metric_key>`, so a reader can
    /// see whether the decline was the disk, the CPU or all of it.
    pub by_metric: BTreeMap<String, f64>,
}

impl SustainedOutcome {
    /// Whether the run declined by more than ordinary drift.
    pub fn declined(&self) -> bool {
        self.retention < RETENTION_FLOOR
    }

    /// The metric that lost the most, if anything lost anything.
    pub fn worst_metric(&self) -> Option<(&str, f64)> {
        self.by_metric
            .iter()
            .min_by(|a, b| a.1.total_cmp(b.1))
            .map(|(key, value)| (key.as_str(), *value))
    }
}

/// Cycle indices that ran the full module set, in ascending order.
///
/// A cycle interrupted by cancellation has fewer modules in it, and comparing a
/// partial cycle with a complete one would report the missing modules as a
/// collapse. Completeness is judged against the first cycle, which is the only
/// one guaranteed to have been given the whole set.
fn complete_cycles(modules: &[ModuleResult]) -> Vec<u32> {
    let mut by_cycle: BTreeMap<u32, BTreeSet<&str>> = BTreeMap::new();
    // What a complete cycle looks like: every module that produced a scoreable
    // result in *any* cycle.
    //
    // Not "whatever cycle 0 managed", which was the earlier rule and got the
    // recovery case exactly backwards. A module that failed in cycle 0 and
    // succeeded afterwards would be missing from the expected set, so every
    // later cycle held a superset of it and was rejected as incomplete - the
    // recovered cycles, the ones with *more* data, discarded first. Retention
    // then vanished and the burst figures from the one bad cycle were published
    // as the result.
    //
    // Taking the union across cycles instead means a transient failure makes
    // the cycle that suffered it incomplete, which is what it is, while a module
    // broken for the whole run drops out of the expected set entirely rather
    // than silencing retention for every other subsystem.
    let mut expected: BTreeSet<&str> = BTreeSet::new();
    for module in modules {
        if !matches!(
            module.status,
            ModuleStatus::Completed | ModuleStatus::Degraded
        ) {
            continue;
        }
        expected.insert(module.module.id.as_str());
        by_cycle
            .entry(module.cycle)
            .or_default()
            .insert(module.module.id.as_str());
    }
    if expected.is_empty() {
        return Vec::new();
    }
    by_cycle
        .into_iter()
        .filter(|(_, present)| *present == expected)
        .map(|(cycle, _)| cycle)
        .collect()
}

/// The cycle whose measurements should be scored.
///
/// The last complete one. For every profile but `endurance` there is exactly
/// one cycle and this returns 0, which is why nothing else in the model had to
/// change to accommodate cycling.
pub fn scoring_cycle(modules: &[ModuleResult]) -> u32 {
    complete_cycles(modules).last().copied().unwrap_or(0)
}

/// Median of a slice, which the caller has not sorted.
fn median(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let n = sorted.len();
    Some(if n % 2 == 1 {
        sorted[n / 2]
    } else {
        (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0
    })
}

/// Measures how much of its opening performance a cycled run retained.
///
/// Returns `None` when the run did not cycle, which is every profile but
/// `endurance`, or when too few cycles completed for a comparison to mean
/// anything.
pub fn analyse(modules: &[ModuleResult]) -> Option<SustainedOutcome> {
    let cycles = complete_cycles(modules);
    if cycles.len() < MIN_CYCLES_FOR_RETENTION {
        return None;
    }

    // One series per metric, ordered by cycle.
    let mut series: BTreeMap<String, Vec<(u32, f64, Direction)>> = BTreeMap::new();
    let eligible: BTreeSet<u32> = cycles.iter().copied().collect();
    for module in modules {
        if !eligible.contains(&module.cycle) {
            continue;
        }
        if !matches!(
            module.status,
            ModuleStatus::Completed | ModuleStatus::Degraded
        ) {
            continue;
        }
        for metric in &module.metrics {
            if !metric.value.is_finite() || metric.value <= 0.0 {
                continue;
            }
            series
                .entry(format!("{}/{}", module.module.id.as_str(), metric.key))
                .or_default()
                .push((module.cycle, metric.value, metric.direction));
        }
    }

    // A third at each end rather than a single first-and-last pair. One cycle
    // is five repetitions, so its median still carries real noise, and reading
    // a run's whole trajectory off two of them would make the headline finding
    // hostage to whichever background task happened to wake up during cycle 0.
    // This is the same window `storage.mixed` already uses for its steady-state
    // ratio, kept identical on purpose.
    let window = (cycles.len() / 3).max(1);
    let opening: BTreeSet<u32> = cycles.iter().take(window).copied().collect();
    let closing: BTreeSet<u32> = cycles.iter().rev().take(window).copied().collect();

    let mut by_metric: BTreeMap<String, f64> = BTreeMap::new();
    for (key, points) in series {
        // Only metrics measured in both windows: one that appeared halfway
        // through has nothing to be compared against.
        let direction = points.first().map(|(_, _, d)| *d)?;
        let first: Vec<f64> = points
            .iter()
            .filter(|(c, _, _)| opening.contains(c))
            .map(|(_, v, _)| *v)
            .collect();
        let last: Vec<f64> = points
            .iter()
            .filter(|(c, _, _)| closing.contains(c))
            .map(|(_, v, _)| *v)
            .collect();
        let (Some(first), Some(last)) = (median(&first), median(&last)) else {
            continue;
        };
        if first <= 0.0 || last <= 0.0 {
            continue;
        }
        // Direction-adjusted so retention always means the same thing. For a
        // latency metric the numbers grow as the machine gets worse, so the
        // ratio is inverted; without this, a run whose fsync latency doubled
        // would be reported as having retained 200% of its performance.
        let retained = match direction {
            Direction::HigherIsBetter => last / first,
            Direction::LowerIsBetter => first / last,
        };
        if retained.is_finite() && retained > 0.0 {
            by_metric.insert(key, retained);
        }
    }

    if by_metric.is_empty() {
        return None;
    }

    // Geometric mean, matching how every other aggregate in this model
    // combines ratios: one metric collapsing to a tenth must not be averaged
    // away by six that held steady.
    let log_sum: f64 = by_metric.values().map(|v| v.ln()).sum();
    let retention = (log_sum / by_metric.len() as f64).exp();
    if !retention.is_finite() {
        return None;
    }

    Some(SustainedOutcome {
        retention,
        score: retention.min(1.0) * crate::model::REFERENCE_ANCHOR,
        cycles: cycles.len(),
        scored_cycle: cycles.last().copied().unwrap_or(0),
        by_metric,
    })
}

#[cfg(test)]
// In tests, `unwrap`/`expect` panicking *is* the failure signal.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic_in_result_fn)]
mod tests {
    use super::*;
    use darcbench_protocol::metrics::Metric;
    use darcbench_protocol::stats::summarize;
    use darcbench_protocol::{ModuleId, ModuleRef};

    fn result(module: &str, cycle: u32, metrics: &[(&str, f64, Direction)]) -> ModuleResult {
        let now = chrono::Utc::now();
        ModuleResult {
            module: ModuleRef {
                id: ModuleId::new(module).expect("id"),
                version: "1.0.0".into(),
            },
            status: ModuleStatus::Completed,
            cycle,
            started_at: now,
            finished_at: now,
            duration_ms: 1.0,
            metrics: metrics
                .iter()
                .map(|(key, value, direction)| Metric {
                    key: (*key).to_string(),
                    label: (*key).to_string(),
                    unit: "u".into(),
                    direction: *direction,
                    value: *value,
                    summary: summarize(&[*value]).expect("summary"),
                    samples: vec![],
                    outliers: vec![],
                })
                .collect(),
            warnings: vec![],
            error: None,
            context: Default::default(),
        }
    }

    /// A single-pass run has nothing to compare, and must say so rather than
    /// inventing a perfect score.
    #[test]
    fn a_run_that_did_not_cycle_has_no_retention() {
        let modules = vec![result(
            "cpu.mixed",
            0,
            &[("a", 100.0, Direction::HigherIsBetter)],
        )];
        assert!(analyse(&modules).is_none());
        assert_eq!(scoring_cycle(&modules), 0);
    }

    #[test]
    fn a_steady_machine_retains_everything() {
        let modules: Vec<ModuleResult> = (0..6)
            .map(|c| result("cpu.mixed", c, &[("a", 100.0, Direction::HigherIsBetter)]))
            .collect();
        let outcome = analyse(&modules).expect("retention");
        assert!((outcome.retention - 1.0).abs() < 1e-9);
        assert!((outcome.score - 1000.0).abs() < 1e-9);
        assert_eq!(outcome.cycles, 6);
        assert_eq!(outcome.scored_cycle, 5);
        assert!(!outcome.declined());
    }

    /// The burstable-instance shape: full speed, then a cliff to baseline.
    #[test]
    fn a_machine_that_falls_to_baseline_reports_what_it_kept() {
        let values = [100.0, 100.0, 100.0, 40.0, 40.0, 40.0];
        let modules: Vec<ModuleResult> = values
            .iter()
            .enumerate()
            .map(|(c, v)| {
                result(
                    "cpu.mixed",
                    c as u32,
                    &[("a", *v, Direction::HigherIsBetter)],
                )
            })
            .collect();
        let outcome = analyse(&modules).expect("retention");
        assert!(
            (outcome.retention - 0.4).abs() < 1e-9,
            "got {}",
            outcome.retention
        );
        assert!((outcome.score - 400.0).abs() < 1e-9);
        assert!(outcome.declined());
    }

    /// A latency metric gets worse by getting bigger, and the ratio has to be
    /// inverted or the finding comes out exactly backwards.
    #[test]
    fn a_latency_metric_that_doubled_is_a_loss_not_a_gain() {
        let values = [1.0, 1.0, 2.0, 2.0];
        let modules: Vec<ModuleResult> = values
            .iter()
            .enumerate()
            .map(|(c, v)| {
                result(
                    "storage.mixed",
                    c as u32,
                    &[("latency_fsync.mean", *v, Direction::LowerIsBetter)],
                )
            })
            .collect();
        let outcome = analyse(&modules).expect("retention");
        assert!(
            (outcome.retention - 0.5).abs() < 1e-9,
            "a doubled latency must read as retaining half, got {}",
            outcome.retention
        );
        assert!(outcome.declined());
    }

    /// A machine that speeds up scores 1000, and the raw observation survives.
    #[test]
    fn a_machine_that_got_faster_is_capped_but_still_recorded() {
        let values = [50.0, 50.0, 60.0, 60.0];
        let modules: Vec<ModuleResult> = values
            .iter()
            .enumerate()
            .map(|(c, v)| {
                result(
                    "cpu.mixed",
                    c as u32,
                    &[("a", *v, Direction::HigherIsBetter)],
                )
            })
            .collect();
        let outcome = analyse(&modules).expect("retention");
        assert!(outcome.retention > 1.0, "got {}", outcome.retention);
        assert!(
            (outcome.score - 1000.0).abs() < 1e-9,
            "the score is capped at the reference anchor"
        );
        assert!(!outcome.declined());
    }

    /// One subsystem collapsing must not be averaged away by others holding.
    #[test]
    fn one_collapsing_metric_is_not_averaged_away() {
        let modules: Vec<ModuleResult> = (0..4)
            .map(|c| {
                let disk = if c < 2 { 1000.0 } else { 100.0 };
                result(
                    "storage.mixed",
                    c,
                    &[
                        ("steady", 100.0, Direction::HigherIsBetter),
                        ("steady2", 100.0, Direction::HigherIsBetter),
                        ("collapsing", disk, Direction::HigherIsBetter),
                    ],
                )
            })
            .collect();
        let outcome = analyse(&modules).expect("retention");
        // Arithmetic mean of (1, 1, 0.1) is 0.70; the geometric mean is 0.46.
        assert!(
            outcome.retention < 0.5,
            "a tenfold collapse in one metric must move the headline, got {}",
            outcome.retention
        );
        assert!(outcome.declined());
        let (worst, value) = outcome.worst_metric().expect("worst");
        assert_eq!(worst, "storage.mixed/collapsing");
        assert!((value - 0.1).abs() < 1e-9);
    }

    /// A cycle cut short by cancellation is not a cycle where everything got
    /// slower; it is a cycle that did not finish.
    #[test]
    fn a_partial_final_cycle_is_excluded_rather_than_read_as_a_collapse() {
        let mut modules = Vec::new();
        for cycle in 0..3 {
            modules.push(result(
                "cpu.mixed",
                cycle,
                &[("a", 100.0, Direction::HigherIsBetter)],
            ));
            modules.push(result(
                "memory.bandwidth",
                cycle,
                &[("b", 200.0, Direction::HigherIsBetter)],
            ));
        }
        // Cancellation lands mid-cycle: cpu ran, memory never started.
        modules.push(result(
            "cpu.mixed",
            3,
            &[("a", 100.0, Direction::HigherIsBetter)],
        ));

        let outcome = analyse(&modules).expect("retention");
        assert_eq!(
            outcome.cycles, 3,
            "the truncated cycle must not count as a completed one"
        );
        assert_eq!(
            outcome.scored_cycle, 2,
            "scores must come from the last cycle that ran the whole set"
        );
        assert!((outcome.retention - 1.0).abs() < 1e-9);
        assert_eq!(scoring_cycle(&modules), 2);
    }

    /// A module that stumbles once must not discard every cycle that worked.
    ///
    /// Regression: completeness was judged against whatever cycle 0 happened to
    /// produce. A module that failed in cycle 0 and recovered afterwards was
    /// therefore absent from the expected set, so every *later* cycle held a
    /// superset and was rejected - the cycles with more data thrown away first.
    /// Retention vanished and the one bad cycle's burst figures were published
    /// as the endurance result.
    #[test]
    fn a_module_that_failed_in_the_first_cycle_does_not_discard_the_rest() {
        let mut modules = Vec::new();
        for cycle in 0..4 {
            modules.push(result(
                "cpu.mixed",
                cycle,
                &[("a", 100.0, Direction::HigherIsBetter)],
            ));
            if cycle == 0 {
                // Storage lost a race for its fixture on the opening cycle.
                let mut failed = result("storage.mixed", 0, &[]);
                failed.status = ModuleStatus::Failed;
                modules.push(failed);
            } else {
                modules.push(result(
                    "storage.mixed",
                    cycle,
                    &[("b", 500.0, Direction::HigherIsBetter)],
                ));
            }
        }

        let outcome = analyse(&modules).expect("the cycles that worked are still comparable");
        assert_eq!(
            outcome.cycles, 3,
            "cycles 1-3 ran the whole set and must be the ones compared"
        );
        assert_eq!(outcome.scored_cycle, 3);
        assert_eq!(scoring_cycle(&modules), 3);
        assert!(
            outcome.by_metric.contains_key("storage.mixed/b"),
            "the recovered module must be back in the result, not silently dropped"
        );
    }

    /// A module broken for the entire run must not silence every other one.
    #[test]
    fn a_permanently_failing_module_does_not_silence_retention() {
        let mut modules = Vec::new();
        for cycle in 0..4 {
            modules.push(result(
                "cpu.mixed",
                cycle,
                &[("a", 100.0, Direction::HigherIsBetter)],
            ));
            let mut failed = result("network.transfer", cycle, &[]);
            failed.status = ModuleStatus::Failed;
            modules.push(failed);
        }
        let outcome = analyse(&modules).expect("retention");
        assert_eq!(
            outcome.cycles, 4,
            "a module that never produced a result is not part of what a complete cycle means"
        );
        assert!((outcome.retention - 1.0).abs() < 1e-9);
    }

    /// A module that failed in every cycle contributes no metrics and must not
    /// take the analysis down with it.
    #[test]
    fn a_run_with_no_usable_metrics_reports_nothing_rather_than_zero() {
        let mut modules: Vec<ModuleResult> = (0..3)
            .map(|c| result("cpu.mixed", c, &[("a", 0.0, Direction::HigherIsBetter)]))
            .collect();
        assert!(
            analyse(&modules).is_none(),
            "a zero-valued metric cannot produce a ratio and must be skipped, not divided by"
        );
        modules.clear();
        assert!(analyse(&modules).is_none());
    }
}
