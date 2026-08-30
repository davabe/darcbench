//! The measurement harness every module shares: calibration and the
//! repetition loop.
//!
//! Both live here rather than in each module because they encode measurement
//! policy, not workload detail. The rule that a repetition below
//! [`MIN_REP_MS`] is timer noise, the rule that warm-ups are streamed but never
//! scored, and the search that decides how much work a repetition should
//! contain are the same decisions whatever is being measured - and a module
//! that quietly diverged from them would produce numbers that look comparable
//! and are not.
//!
//! # Visibility
//!
//! [`calibrate_with`], [`time_reps`] and [`RepOutcome`] are `pub` rather than
//! `pub(crate)` because the server line's modules call them from
//! `darcbench-modules`, across the crate boundary introduced by
//! [ADR-0015](../../../docs/adr/0015-two-product-lines-one-engine.md).
//! [`MIN_REP_MS`] and [`MAX_CALIBRATION_ITERATIONS`] stay `pub(crate)`: nothing
//! outside this crate reads them, and measurement policy is not a number a
//! consumer should be able to branch on.

use darcbench_protocol::metrics::{MetricSample, Warning, WarningCode};

use crate::module::{ModuleError, ModuleParams, ModuleReporter};

/// Minimum acceptable repetition duration. Below this, timer resolution and
/// scheduler noise dominate and the sample is not trustworthy.
pub(crate) const MIN_REP_MS: f64 = 20.0;

/// Ceiling on calibration iterations, so a pathologically slow or throttled
/// machine cannot make a single repetition run unboundedly long.
pub(crate) const MAX_CALIBRATION_ITERATIONS: u64 = 1 << 24;

/// Largest factor by which the calibration search may grow its probe in one
/// step.
///
/// Bounded so a reading taken at an untrustworthy duration - where timer
/// granularity, not work, dominates - cannot launch a probe orders of magnitude
/// too large.
const MAX_CALIBRATION_GROWTH: f64 = 16.0;

/// Finds an iteration count whose execution takes approximately `target_ms`.
///
/// `probe` runs the workload for a given iteration count and returns the
/// elapsed wall-clock milliseconds.
///
/// The search grows a probe from one iteration until it exceeds a quarter of
/// the target - the point at which the timer reading can be trusted - then
/// extrapolates linearly. Starting small rather than large means a slow machine
/// is never asked to do an enormous amount of work just to discover that it is
/// slow.
///
/// # Why the step is proportional rather than a fixed doubling
///
/// Doubling needs `log2(n)` probes, and for the cheapest workloads `n` is in
/// the tens of thousands: roughly nineteen probes per shape. On a
/// multi-threaded shape each probe spawns a thread per logical CPU, so on a
/// wide machine the search burned hundreds of thread spawns timing amounts of
/// work that were almost entirely spawn overhead. Stepping towards the
/// trustworthy duration converges in a handful of probes instead, and because
/// the probes it skips are the small ones it also cuts calibration's own
/// wall-clock cost.
pub fn calibrate_with(
    target_ms: u64,
    reporter: &dyn ModuleReporter,
    mut probe: impl FnMut(u64) -> f64,
) -> Result<u64, ModuleError> {
    let target = target_ms as f64;
    // Below this the reading is scheduler noise rather than a measurement.
    let trustworthy = target / 4.0;
    let mut iterations = 1u64;

    loop {
        if reporter.is_cancelled() {
            return Err(ModuleError::Cancelled);
        }
        let elapsed_ms = probe(iterations);
        if elapsed_ms >= trustworthy {
            let scale = target / elapsed_ms.max(0.001);
            let scaled = (iterations as f64 * scale).round() as u64;
            return Ok(scaled.clamp(1, MAX_CALIBRATION_ITERATIONS));
        }
        if iterations >= MAX_CALIBRATION_ITERATIONS {
            return Ok(MAX_CALIBRATION_ITERATIONS);
        }
        let growth = if elapsed_ms > 0.0 {
            (trustworthy / elapsed_ms).clamp(2.0, MAX_CALIBRATION_GROWTH)
        } else {
            MAX_CALIBRATION_GROWTH
        };
        // `growth` is at least 2.0, so the probe always grows; the explicit
        // `max` guards against the search stalling should rounding ever
        // conspire to return the current value.
        let next = ((iterations as f64) * growth) as u64;
        iterations = next
            .max(iterations.saturating_add(1))
            .min(MAX_CALIBRATION_ITERATIONS);
    }
}

/// What the repetition loop produced for one metric.
pub struct RepOutcome {
    /// Every repetition, warm-ups included and flagged.
    pub samples: Vec<MetricSample>,
    /// Values from the measured (non-warm-up) repetitions only.
    pub measured: Vec<f64>,
    /// Validation warnings raised during the loop.
    pub warnings: Vec<Warning>,
}

/// Runs the warm-up and measured repetitions for one metric, streaming each to
/// the reporter.
///
/// `run_rep` executes a single repetition and returns
/// `(value_in_the_metric_unit, wall_clock_ms)`.
///
/// `completed_units` and `total_units` position this metric within the module
/// so the streamed progress fraction is monotonic across the whole module
/// rather than restarting per metric.
#[allow(clippy::too_many_arguments)]
pub fn time_reps(
    params: &ModuleParams,
    reporter: &dyn ModuleReporter,
    metric_key: &str,
    unit: &str,
    completed_units: f64,
    total_units: f64,
    mut run_rep: impl FnMut(u32) -> (f64, f64),
) -> Result<RepOutcome, ModuleError> {
    let total_reps = params.warmup_reps + params.measured_reps;
    let mut samples = Vec::with_capacity(total_reps as usize);
    let mut measured = Vec::with_capacity(params.measured_reps as usize);
    let mut warnings = Vec::new();

    for rep in 0..total_reps {
        if reporter.is_cancelled() {
            return Err(ModuleError::Cancelled);
        }
        let warmup = rep < params.warmup_reps;
        let (value, duration_ms) = run_rep(rep);

        // A warm-up that came in short is expected - caches are still cold.
        // A *measured* repetition that did is a calibration failure, and the
        // sample is kept and flagged rather than silently trusted.
        if !warmup && duration_ms < MIN_REP_MS {
            warnings.push(Warning {
                code: WarningCode::ValidationFailed,
                message: format!(
                    "repetition of `{metric_key}` completed in {duration_ms:.1} ms, below the \
                     {MIN_REP_MS} ms floor; calibration failed to reach a trustworthy duration"
                ),
                metric_key: Some(metric_key.to_string()),
            });
        }

        let progress = (completed_units + (rep + 1) as f64 / total_reps as f64) / total_units;
        reporter.sample(
            metric_key,
            unit,
            rep,
            warmup,
            value,
            duration_ms,
            progress.clamp(0.0, 1.0),
        );

        samples.push(MetricSample {
            rep,
            value,
            duration_ms,
            warmup,
        });
        if !warmup {
            measured.push(value);
        }
    }

    Ok(RepOutcome {
        samples,
        measured,
        warnings,
    })
}

#[cfg(test)]
// In tests, `unwrap`/`expect` panicking *is* the failure signal.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic_in_result_fn)]
mod tests {
    use super::*;
    use crate::module::NullReporter;
    use std::cell::Cell;

    /// Calibration must converge in a handful of probes.
    ///
    /// Regression: the search doubled from a single iteration, so for the
    /// cheapest workloads it needed ~19 probes to reach a duration the timer
    /// could be trusted at. Every one of those probes on a multi-threaded shape
    /// spawns a thread per logical CPU, so on a wide machine the search burned
    /// hundreds of thread spawns measuring almost pure spawn overhead. This
    /// bound is what stops that regressing.
    #[test]
    fn calibration_converges_in_a_bounded_number_of_probes() {
        // A modelled workload at 100 ns per iteration: reaching a quarter of a
        // 200 ms target takes 500_000 iterations, which is 19 doublings from 1.
        let probes = Cell::new(0usize);
        let iterations = calibrate_with(200, &NullReporter::default(), |n| {
            probes.set(probes.get() + 1);
            n as f64 * 100.0 / 1_000_000.0
        })
        .expect("calibrate");

        assert!(
            probes.get() <= 10,
            "calibration used {} probes; doubling from 1 would need ~19, and each probe on a \
             multi-threaded shape spawns a thread per logical CPU",
            probes.get()
        );
        assert!(probes.get() >= 2, "calibration must search, not guess");
        // 200 ms at 100 ns per iteration is 2_000_000 iterations.
        assert_eq!(iterations, 2_000_000);
    }

    #[test]
    fn calibration_lands_on_the_requested_duration() {
        // Costs for which the target is reachable inside the iteration
        // ceiling: 300 ms over 2^24 iterations needs at least ~18 ns each.
        for nanos in [20.0, 1_000.0, 250_000.0] {
            let iterations = calibrate_with(300, &NullReporter::default(), |n| {
                n as f64 * nanos / 1_000_000.0
            })
            .expect("calibrate");
            let modelled_ms = iterations as f64 * nanos / 1_000_000.0;
            assert!(
                (modelled_ms - 300.0).abs() < 1.0,
                "at {nanos} ns per iteration, {iterations} iterations model {modelled_ms:.1} ms"
            );
        }
    }

    /// A repetition may come in short of the target rather than exceed the
    /// iteration ceiling. The floor check in [`time_reps`] is what decides
    /// whether the resulting duration is still trustworthy.
    #[test]
    fn the_iteration_ceiling_wins_over_the_target_duration() {
        let iterations = calibrate_with(300, &NullReporter::default(), |n| {
            // 1 ns per iteration: 300 ms would need 300 million iterations.
            n as f64 / 1_000_000.0
        })
        .expect("calibrate");
        assert_eq!(iterations, MAX_CALIBRATION_ITERATIONS);
    }

    #[test]
    fn a_workload_slower_than_the_target_is_asked_for_one_iteration() {
        let iterations =
            calibrate_with(20, &NullReporter::default(), |n| n as f64 * 50.0).expect("calibrate");
        assert_eq!(iterations, 1);
    }

    /// A workload so cheap that no iteration count reaches the target must stop
    /// at the declared ceiling rather than search forever.
    #[test]
    fn calibration_terminates_at_the_iteration_ceiling() {
        let probes = Cell::new(0usize);
        let iterations = calibrate_with(1_000, &NullReporter::default(), |_| {
            probes.set(probes.get() + 1);
            // Always immeasurable, whatever it is asked to run.
            0.0
        })
        .expect("calibrate");
        assert_eq!(iterations, MAX_CALIBRATION_ITERATIONS);
        assert!(
            probes.get() < 40,
            "the search must not grind: {} probes",
            probes.get()
        );
    }

    #[test]
    fn calibration_is_cancellable() {
        let reporter = NullReporter::default();
        reporter.cancel();
        assert!(matches!(
            calibrate_with(200, &reporter, |_| 0.0),
            Err(ModuleError::Cancelled)
        ));
    }

    fn params(warmup: u32, measured: u32) -> ModuleParams {
        ModuleParams {
            warmup_reps: warmup,
            measured_reps: measured,
            target_rep_ms: 100,
            threads: 1,
            facts: Default::default(),
            scratch_dir: None,
        }
    }

    #[test]
    fn warmups_are_streamed_but_never_summarised() {
        let outcome = time_reps(
            &params(2, 5),
            &NullReporter::default(),
            "demo.single",
            "MiB/s",
            0.0,
            1.0,
            |rep| (100.0 + rep as f64, 50.0),
        )
        .expect("reps");

        assert_eq!(
            outcome.samples.len(),
            7,
            "warm-ups are retained as evidence"
        );
        assert_eq!(outcome.samples.iter().filter(|s| s.warmup).count(), 2);
        assert_eq!(outcome.measured.len(), 5, "warm-ups are not scored");
        assert_eq!(
            outcome.measured[0], 102.0,
            "measurement starts after warm-up"
        );
        assert!(outcome.warnings.is_empty());
    }

    #[test]
    fn a_short_measured_repetition_is_flagged_but_a_short_warmup_is_not() {
        let outcome = time_reps(
            &params(1, 3),
            &NullReporter::default(),
            "demo.single",
            "MiB/s",
            0.0,
            1.0,
            // Every repetition finishes far below the floor.
            |_| (1.0, MIN_REP_MS / 4.0),
        )
        .expect("reps");
        assert_eq!(
            outcome.warnings.len(),
            3,
            "one per measured repetition, none for the warm-up"
        );
        assert!(outcome
            .warnings
            .iter()
            .all(|w| w.code == WarningCode::ValidationFailed));
    }

    #[test]
    fn streamed_progress_stays_inside_the_metric_slice() {
        use std::sync::Mutex;
        #[derive(Default)]
        struct Recorder(Mutex<Vec<f64>>);
        impl ModuleReporter for Recorder {
            fn sample(&self, _: &str, _: &str, _: u32, _: bool, _: f64, _: f64, progress: f64) {
                if let Ok(mut seen) = self.0.lock() {
                    seen.push(progress);
                }
            }
            fn warn(&self, _: Warning) {}
            fn is_cancelled(&self) -> bool {
                false
            }
        }

        let recorder = Recorder::default();
        // The third metric of four: progress must run through (0.5, 0.75].
        time_reps(
            &params(1, 3),
            &recorder,
            "demo.single",
            "u",
            2.0,
            4.0,
            |_| (1.0, 50.0),
        )
        .expect("reps");

        let seen = recorder.0.lock().expect("lock").clone();
        assert_eq!(seen.len(), 4);
        assert!(
            seen.windows(2).all(|w| w[0] < w[1]),
            "progress must advance"
        );
        assert!(seen.iter().all(|p| (0.5..=0.75).contains(p)), "{seen:?}");
    }

    #[test]
    fn the_repetition_loop_is_cancellable() {
        let reporter = NullReporter::default();
        reporter.cancel();
        assert!(matches!(
            time_reps(
                &params(1, 5),
                &reporter,
                "demo.single",
                "u",
                0.0,
                1.0,
                |_| (1.0, 50.0)
            ),
            Err(ModuleError::Cancelled)
        ));
    }
}
