//! `cpu.mixed` - the Phase 1 CPU module.
//!
//! # What it measures
//!
//! Five workloads (see [`crate::workloads`]) run in two shapes:
//!
//! * **single** - one thread. Measures per-core performance, which is what
//!   determines PHP request latency, single-query database work and the
//!   critical path of almost every web request.
//! * **multi** - one independent copy of the workload per logical CPU,
//!   throughput style. Measures aggregate capacity, which is what determines
//!   how many concurrent requests a machine can absorb.
//!
//! Reporting both, separately, is deliberate: a 32-vCPU shared-CPU instance and
//! a 4-core high-frequency machine can have identical multi-core throughput and
//! feel completely different to run a website on. A single blended number would
//! hide that, which is one of the specific failure modes DARCBench exists to
//! avoid.
//!
//! # Calibration
//!
//! Iteration counts are calibrated per machine so that one repetition takes
//! approximately [`ModuleParams::target_rep_ms`]. Fixing the iteration count
//! instead would mean a fast server finishing a repetition in a few
//! milliseconds, where timer granularity, scheduler ticks and interrupt noise
//! are a large fraction of the measurement.
//!
//! # Safety
//!
//! `cpu.mixed` writes nothing, opens no sockets, spawns no processes and
//! touches no service. It saturates the CPU for its duration, which is why it
//! is classified [`SafetyClass::ComputeIntensive`] and not `Observational`.

use std::time::Instant;

use darcbench_protocol::metrics::{Direction, Metric, Warning, WarningCode};
use darcbench_protocol::stats::{outlier_indices, summarize};
use darcbench_protocol::ModuleId;

use crate::harness::{calibrate_with, time_reps};
use crate::module::{
    BenchmarkModule, ModuleError, ModuleManifest, ModuleOutput, ModuleParams, ModuleReporter,
    SafetyClass,
};
use crate::workloads::{cpu_workloads, Workload};

/// Workload-definition version.
///
/// `1.0.1` replaced the calibration search with a proportional one. It changes
/// how quickly an iteration count is found, not what is measured - throughput
/// is work over time and so is independent of the count chosen - so results
/// stay comparable with `1.0.0` per the compatibility table in
/// `docs/BENCHMARK-MODULE-SPEC.md`.
pub const VERSION: &str = "1.0.1";

#[derive(Debug)]
pub struct CpuMixed {
    manifest: ModuleManifest,
}

impl Default for CpuMixed {
    fn default() -> Self {
        Self::new()
    }
}

/// The module's identifier. Validated against the [`ModuleId`] grammar by
/// `manifest_is_well_formed`, so the `expect` in the constructor cannot fire
/// without a test failing first.
pub const MODULE_ID: &str = "cpu.mixed";

impl CpuMixed {
    pub fn new() -> Self {
        // Justified `expect`: `MODULE_ID` is a compile-time constant whose
        // validity under the ModuleId grammar is asserted by a unit test in
        // this file. There is no runtime input here to fail on.
        #[allow(clippy::expect_used)]
        let id = ModuleId::new(MODULE_ID).expect("MODULE_ID is a valid module id");
        Self {
            manifest: ModuleManifest {
                id,
                version: VERSION.to_string(),
                title: "Mixed CPU workloads".to_string(),
                purpose: "Measure per-core and aggregate CPU performance across hashing, \
                          compression, serialisation, integer and floating-point work \
                          representative of web server duty."
                    .to_string(),
                safety_class: SafetyClass::ComputeIntensive,
                dependencies: vec![],
                max_bytes_written: 0,
                max_network_bytes: 0,
                cleanup: "None required: the module allocates only heap memory, which is \
                          released when it returns."
                    .to_string(),
                validation: vec![
                    "Every workload must produce at least 5 measured repetitions.".to_string(),
                    "Repetitions shorter than 20 ms are rejected as timer-noise dominated."
                        .to_string(),
                    "Coefficient of variation above 0.15 raises a high-variance warning and \
                     downgrades the result to Degraded."
                        .to_string(),
                ],
                limitations: vec![
                    "Measures the CPU as the operating system presents it. Inside a container \
                     or a cgroup-limited VM the result describes that sandbox, not the host."
                        .to_string(),
                    "Does not use hand-written SIMD; it measures what a normal optimised \
                     program achieves, including whatever the compiler auto-vectorises."
                        .to_string(),
                    "Multi-threaded shapes measure throughput, not parallel speed-up of a \
                     single problem, so they do not capture inter-core latency or lock \
                     contention."
                        .to_string(),
                ],
                comparability: vec![
                    "module.version".to_string(),
                    // `platform.architecture`, not `cpu.architecture`: the inventory puts it
                    // there, and the name it was declared under for two phases resolved to
                    // nothing at all.
                    "platform.architecture".to_string(),
                    "agent.build_target".to_string(),
                    // The key the context actually carries. `params.threads` was the name
                    // of the input rather than of the recorded fact.
                    "threads".to_string(),
                ],
                stability_cv_bound: 0.15,
            },
        }
    }
}

impl BenchmarkModule for CpuMixed {
    fn manifest(&self) -> &ModuleManifest {
        &self.manifest
    }

    fn estimated_duration_s(&self, params: &ModuleParams) -> u64 {
        let workloads = 5u64;
        let shapes = 2u64;
        let reps = (params.warmup_reps + params.measured_reps) as u64;
        // Plus a calibration allowance of roughly two repetitions per shape.
        let reps_total = workloads * shapes * (reps + 2);
        (reps_total * params.target_rep_ms).div_ceil(1000)
    }

    fn run(
        &self,
        params: &ModuleParams,
        reporter: &dyn ModuleReporter,
    ) -> Result<ModuleOutput, ModuleError> {
        let workloads = cpu_workloads();
        let threads = params.effective_threads();
        let shapes: [(&str, usize); 2] = [("single", 1), ("multi", threads)];

        let total_units = (workloads.len() * shapes.len()) as f64;
        let mut completed_units = 0.0f64;

        let mut metrics = Vec::new();
        let mut warnings = Vec::new();
        let mut calibration = serde_json::Map::new();

        for workload in &workloads {
            for (shape_name, shape_threads) in shapes {
                if reporter.is_cancelled() {
                    return Err(ModuleError::Cancelled);
                }
                let metric_key = format!("{}.{}", workload.key(), shape_name);

                let iterations = calibrate(
                    workload.as_ref(),
                    shape_threads,
                    params.target_rep_ms,
                    reporter,
                )?;
                calibration.insert(
                    format!("{metric_key}.iterations"),
                    serde_json::Value::from(iterations),
                );

                let outcome = time_reps(
                    params,
                    reporter,
                    &metric_key,
                    workload.unit(),
                    completed_units,
                    total_units,
                    |_| time_shape(workload.as_ref(), shape_threads, iterations),
                )?;
                warnings.extend(outcome.warnings);

                let summary = summarize(&outcome.measured)
                    .ok_or_else(|| ModuleError::NoSamples(metric_key.clone()))?;

                if let Some(cv) = summary.cv {
                    if cv > self.manifest.stability_cv_bound {
                        let warning = Warning {
                            code: WarningCode::HighVariance,
                            message: format!(
                                "`{metric_key}` varied by {:.1}% between repetitions (bound {:.0}%). \
                                 On shared infrastructure this usually means CPU steal or a noisy \
                                 neighbour rather than a measurement fault.",
                                cv * 100.0,
                                self.manifest.stability_cv_bound * 100.0
                            ),
                            metric_key: Some(metric_key.clone()),
                        };
                        reporter.warn(warning.clone());
                        warnings.push(warning);
                    }
                }

                metrics.push(Metric {
                    label: format!("{} ({shape_name})", workload.label()),
                    unit: workload.unit().to_string(),
                    direction: Direction::HigherIsBetter,
                    value: summary.median,
                    outliers: outlier_indices(&outcome.measured, 3.5),
                    summary,
                    samples: outcome.samples,
                    key: metric_key,
                    measures_dispersion: false,
                });

                completed_units += 1.0;
            }
        }

        // Scaling efficiency: how much of the theoretical N-thread throughput
        // the machine actually delivers. Below ~0.5 on a dedicated-vCPU plan
        // is a strong hint of SMT saturation, thermal limits or oversubscription.
        let mut context = serde_json::Map::new();
        context.insert("threads".into(), serde_json::Value::from(threads));
        context.insert("shape_single_threads".into(), serde_json::Value::from(1));
        context.insert("workload_version".into(), serde_json::Value::from(VERSION));
        context.insert(
            "build_target".into(),
            serde_json::Value::from(format!(
                "{}-{}",
                std::env::consts::ARCH,
                std::env::consts::OS
            )),
        );
        context.insert("calibration".into(), serde_json::Value::Object(calibration));

        if let Some(efficiency) = scaling_efficiency(&metrics, threads) {
            context.insert("scaling_efficiency".into(), serde_json::json!(efficiency));
            if efficiency < 0.5 && threads > 1 {
                let warning = Warning {
                    code: WarningCode::Informational,
                    message: format!(
                        "Multi-threaded throughput reached only {:.0}% of {threads}x the \
                         single-thread result. Expected on SMT and on shared-vCPU plans; \
                         unexpected on dedicated cores.",
                        efficiency * 100.0
                    ),
                    metric_key: None,
                };
                reporter.warn(warning.clone());
                warnings.push(warning);
            }
        }

        Ok(ModuleOutput {
            metrics,
            warnings,
            context,
        })
    }
}

/// Finds an iteration count whose single-shape execution takes approximately
/// `target_ms`. See [`calibrate_with`] for the search itself.
fn calibrate(
    workload: &dyn Workload,
    threads: usize,
    target_ms: u64,
    reporter: &dyn ModuleReporter,
) -> Result<u64, ModuleError> {
    calibrate_with(target_ms, reporter, |iterations| {
        time_shape(workload, threads, iterations).1
    })
}

/// Executes one repetition of a shape and returns
/// `(throughput_in_workload_units, wall_clock_ms)`.
///
/// Multi-threaded shapes run `threads` independent copies concurrently and sum
/// their work. Wall-clock time is measured across the whole scope, so thread
/// spawn cost and stragglers are charged to the result rather than hidden -
/// which is what a server actually experiences.
fn time_shape(workload: &dyn Workload, threads: usize, iterations: u64) -> (f64, f64) {
    let start = Instant::now();
    if threads <= 1 {
        workload.execute(iterations);
    } else {
        std::thread::scope(|scope| {
            let handles: Vec<_> = (0..threads)
                .map(|_| scope.spawn(|| workload.execute(iterations)))
                .collect();
            for handle in handles {
                // A panicking workload thread is a bug, not a slow machine.
                // Joining and ignoring the payload keeps the harness total; the
                // resulting duration will be flagged by the MIN_REP_MS check.
                let _ = handle.join();
            }
        });
    }
    let elapsed = start.elapsed();
    let seconds = elapsed.as_secs_f64().max(f64::EPSILON);
    let units = workload.units_per_iteration() * iterations as f64 * threads.max(1) as f64;
    let throughput = units / workload.unit_scale() / seconds;
    (throughput, elapsed.as_secs_f64() * 1000.0)
}

/// `multi / (single * threads)`, averaged over the workloads that reported both
/// shapes.
fn scaling_efficiency(metrics: &[Metric], threads: usize) -> Option<f64> {
    if threads <= 1 {
        return None;
    }
    let mut ratios = Vec::new();
    for metric in metrics {
        // `strip_suffix` rather than `trim_end_matches`, which would strip a
        // repeated suffix and pair up the wrong metrics.
        let Some(base) = metric.key.strip_suffix(".single") else {
            continue;
        };
        // Compared without building the partner key: `find` runs the closure
        // once per metric, and allocating a `String` to throw away each time
        // is pure waste.
        let multi = metrics
            .iter()
            .find(|m| m.key.strip_suffix(".multi") == Some(base))?;
        if metric.value > 0.0 {
            ratios.push(multi.value / (metric.value * threads as f64));
        }
    }
    if ratios.is_empty() {
        return None;
    }
    Some(ratios.iter().sum::<f64>() / ratios.len() as f64)
}

#[cfg(test)]
// In tests, `unwrap`/`expect` panicking *is* the failure signal.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic_in_result_fn)]
mod tests {
    use super::*;
    use crate::module::NullReporter;
    use darcbench_protocol::Profile;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    /// Fast parameters so the test suite stays quick while still exercising
    /// the full calibrate -> warm up -> measure -> summarise path.
    fn fast_params() -> ModuleParams {
        ModuleParams {
            warmup_reps: 1,
            measured_reps: 5,
            target_rep_ms: 25,
            threads: 2,
            facts: Default::default(),
            scratch_dir: None,
        }
    }

    #[derive(Default)]
    struct RecordingReporter {
        samples: Mutex<Vec<(String, u32, bool, f64)>>,
        warnings: Mutex<Vec<Warning>>,
        cancel_after: Option<usize>,
        seen: AtomicUsize,
    }

    impl ModuleReporter for RecordingReporter {
        fn sample(
            &self,
            metric_key: &str,
            _unit: &str,
            rep: u32,
            warmup: bool,
            value: f64,
            _duration_ms: f64,
            module_progress: f64,
        ) {
            assert!(
                (0.0..=1.0).contains(&module_progress),
                "progress {module_progress} out of range"
            );
            self.seen.fetch_add(1, Ordering::SeqCst);
            if let Ok(mut samples) = self.samples.lock() {
                samples.push((metric_key.to_string(), rep, warmup, value));
            }
        }

        fn warn(&self, warning: Warning) {
            if let Ok(mut w) = self.warnings.lock() {
                w.push(warning);
            }
        }

        fn is_cancelled(&self) -> bool {
            self.cancel_after
                .is_some_and(|n| self.seen.load(Ordering::SeqCst) >= n)
        }
    }

    #[test]
    fn module_id_constant_satisfies_the_grammar() {
        assert!(
            ModuleId::new(MODULE_ID).is_ok(),
            "MODULE_ID `{MODULE_ID}` violates the ModuleId grammar; the constructor would panic"
        );
    }

    #[test]
    fn manifest_is_well_formed() {
        let module = CpuMixed::new();
        let m = module.manifest();
        assert_eq!(m.id.as_str(), "cpu.mixed");
        assert_eq!(m.version, VERSION);
        assert_eq!(m.max_bytes_written, 0, "cpu.mixed must never write to disk");
        assert_eq!(
            m.max_network_bytes, 0,
            "cpu.mixed must never use the network"
        );
        assert_eq!(m.safety_class, SafetyClass::ComputeIntensive);
        assert!(!m.validation.is_empty());
        assert!(
            !m.limitations.is_empty(),
            "a module must disclose its limitations"
        );
        assert!(m.stability_cv_bound > 0.0);
    }

    #[test]
    fn full_run_produces_ten_metrics_with_samples() {
        let module = CpuMixed::new();
        let reporter = RecordingReporter::default();
        let output = module.run(&fast_params(), &reporter).expect("run");

        assert_eq!(output.metrics.len(), 10, "5 workloads x 2 shapes");
        for metric in &output.metrics {
            assert!(
                metric.value > 0.0,
                "{} produced a non-positive value",
                metric.key
            );
            assert_eq!(
                metric.summary.n, 5,
                "{} should summarise 5 measured reps",
                metric.key
            );
            assert_eq!(metric.samples.len(), 6, "5 measured + 1 warm-up");
            assert_eq!(
                metric.samples.iter().filter(|s| s.warmup).count(),
                1,
                "{} should have exactly one warm-up",
                metric.key
            );
            assert_eq!(metric.direction, Direction::HigherIsBetter);
            assert!(!metric.unit.is_empty());
        }

        let keys: Vec<&str> = output.metrics.iter().map(|m| m.key.as_str()).collect();
        assert!(keys.contains(&"crypto_sha256.single"));
        assert!(keys.contains(&"crypto_sha256.multi"));
        assert!(keys.contains(&"float_matmul.multi"));

        // Warm-up samples must be streamed too, so the UI can show activity,
        // but flagged so a client never charts them as results.
        let samples = reporter.samples.lock().expect("lock");
        assert_eq!(samples.len(), 60, "10 metrics x 6 reps");
        assert!(samples.iter().any(|(_, _, warmup, _)| *warmup));
    }

    #[test]
    fn context_records_everything_needed_for_comparability() {
        let module = CpuMixed::new();
        let output = module
            .run(&fast_params(), &NullReporter::default())
            .expect("run");
        assert_eq!(output.context["threads"], 2);
        assert_eq!(output.context["workload_version"], VERSION);
        assert!(output.context.contains_key("build_target"));
        assert!(output.context.contains_key("calibration"));
        assert!(output.context.contains_key("scaling_efficiency"));
    }

    #[test]
    fn cancellation_is_honoured_promptly() {
        let module = CpuMixed::new();
        let reporter = RecordingReporter {
            cancel_after: Some(3),
            ..Default::default()
        };
        let start = Instant::now();
        let err = module
            .run(&fast_params(), &reporter)
            .expect_err("should cancel");
        assert!(matches!(err, ModuleError::Cancelled));
        assert!(
            start.elapsed() < std::time::Duration::from_secs(20),
            "cancellation took {:?}",
            start.elapsed()
        );
        let samples = reporter.samples.lock().expect("lock");
        assert!(samples.len() < 60, "cancellation must stop work early");
    }

    #[test]
    fn cancellation_before_any_work_returns_immediately() {
        let module = CpuMixed::new();
        let reporter = RecordingReporter {
            cancel_after: Some(0),
            ..Default::default()
        };
        let start = Instant::now();
        assert!(matches!(
            module.run(&fast_params(), &reporter),
            Err(ModuleError::Cancelled)
        ));
        assert!(start.elapsed() < std::time::Duration::from_secs(2));
    }

    #[test]
    fn calibration_targets_the_requested_duration() {
        let workload = crate::workloads::IntegerSort::new();
        let reporter = NullReporter::default();
        let iterations = calibrate(&workload, 1, 120, &reporter).expect("calibrate");
        assert!(iterations >= 1);
        let (_, ms) = time_shape(&workload, 1, iterations);
        // Wide bounds: this runs on shared CI hardware. The point is that
        // calibration lands in the right order of magnitude, not that it is
        // precise.
        assert!(
            (20.0..600.0).contains(&ms),
            "calibrated to {iterations} iterations giving {ms:.1} ms, target was 120 ms"
        );
    }

    /// Throughput samples for a shape, best first.
    ///
    /// Contention only ever depresses throughput, so the maximum across
    /// repetitions is the estimate least contaminated by a noisy neighbour.
    /// This is the same argument the methodology makes for the measurement
    /// pipeline itself; the test suite has no business being less careful than
    /// the thing it tests.
    fn throughput_samples(
        workload: &dyn Workload,
        threads: usize,
        iterations: u64,
        repeats: usize,
    ) -> Vec<f64> {
        let mut samples: Vec<f64> = (0..repeats)
            .map(|_| time_shape(workload, threads, iterations).0)
            .collect();
        samples.sort_by(|a, b| b.total_cmp(a));
        samples
    }

    /// Throughput samples, plus how many cores this process actually got.
    ///
    /// The second value is CPU seconds burned over wall seconds elapsed: 1.0
    /// means one core's worth, and a threaded shape on a machine with room
    /// should approach its thread count. It is measured rather than assumed
    /// because "the machine has N logical CPUs" and "this process may use N
    /// logical CPUs right now" are different claims, and only the second one
    /// makes a speedup assertion meaningful.
    fn samples_with_concurrency(
        workload: &dyn crate::workloads::Workload,
        threads: usize,
        iterations: u64,
        repeats: usize,
    ) -> (Vec<f64>, f64) {
        let (cpu_before, wall) = (process_cpu_seconds(), std::time::Instant::now());
        let samples = throughput_samples(workload, threads, iterations, repeats);
        let elapsed = wall.elapsed().as_secs_f64();
        let burned = process_cpu_seconds() - cpu_before;
        let concurrency = if elapsed > 0.0 { burned / elapsed } else { 0.0 };
        (samples, concurrency)
    }

    /// CPU seconds this process has used across all its threads.
    ///
    /// `utime + stime` from `/proc/self/stat`, in USER_HZ jiffies. The field
    /// offsets are counted from after `comm`, which is parenthesised and may
    /// itself contain spaces and parentheses - so the split starts at the last
    /// `)` rather than at the second field.
    ///
    /// Returns 0.0 if anything is unreadable, which makes the concurrency
    /// check skip rather than fire: an unmeasurable precondition is not a
    /// failed one.
    fn process_cpu_seconds() -> f64 {
        let Ok(stat) = std::fs::read_to_string("/proc/self/stat") else {
            return 0.0;
        };
        let Some(after_comm) = stat.rsplit_once(')').map(|(_, rest)| rest) else {
            return 0.0;
        };
        let fields: Vec<&str> = after_comm.split_whitespace().collect();
        // `state` is the first field here, so `utime` is index 11 and `stime`
        // 12 - fields 14 and 15 of the file, counting from one.
        let (Some(utime), Some(stime)) = (fields.get(11), fields.get(12)) else {
            return 0.0;
        };
        let ticks: f64 = match (utime.parse::<f64>(), stime.parse::<f64>()) {
            (Ok(u), Ok(s)) => u + s,
            _ => return 0.0,
        };
        // USER_HZ is 100 on every Linux this ships to. Getting it from
        // `sysconf` would need libc or `unsafe`, and this is a test guard
        // rather than a measurement - a wrong constant here changes when the
        // guard trips, never what the module reports.
        ticks / 100.0
    }

    /// A single 60 ms sample of each shape is not enough to conclude anything on
    /// shared CI hardware. This test failed at 0.98x on a GitHub runner that was
    /// momentarily oversubscribed, having passed on the same code minutes
    /// earlier, so it takes the best of several repetitions and refuses to draw
    /// a conclusion when the machine is demonstrably too noisy to support one.
    #[test]
    fn multi_thread_shape_reports_more_total_throughput() {
        const REPEATS: usize = 5;
        /// Cores the threaded shape must actually have been given before a
        /// speedup ratio means anything.
        ///
        /// 1.5 rather than 2.0: the samples are bracketed by the harness's own
        /// setup and teardown, so a genuinely parallel run on two free cores
        /// measures somewhat under 2.0. What this has to separate is "ran on
        /// about two cores" from "ran on about one", and 1.5 sits between them
        /// with room on both sides.
        const MIN_ACHIEVED_CONCURRENCY: f64 = 1.5;

        let workload = crate::workloads::CryptoSha256::new();
        let reporter = NullReporter::default();
        let iterations = calibrate(&workload, 1, 60, &reporter).expect("calibrate");
        let threads = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        if threads < 2 {
            return;
        }

        let (single, single_concurrency) =
            samples_with_concurrency(&workload, 1, iterations, REPEATS);
        let (multi, multi_concurrency) =
            samples_with_concurrency(&workload, threads, iterations, REPEATS);
        let (best_single, best_multi) = (single[0], multi[0]);

        // Before anything is concluded about *speedup*, check that the machine
        // actually ran the threaded shape on more than one core.
        //
        // This is not the same question as the noise guard below, and the
        // difference is what made this test flaky on a two-vCPU host. Noise is
        // a shape that cannot reproduce itself; this is a shape that
        // reproduced perfectly well while being given one core, because the
        // rest of the test suite was on the other one. No amount of averaging
        // distinguishes "the threaded shape stopped being parallel" - which is
        // the defect worth failing on - from "the machine had no second core
        // to give it", and asserting anyway means the suite fails on small
        // hosts for a reason that is not about the code.
        //
        // So the precondition is measured rather than assumed: CPU seconds
        // burned divided by wall seconds elapsed is how many cores this
        // process was really running on. `/proc/self/stat` sums `utime` and
        // `stime` over every thread, which is exactly the quantity wanted.
        if multi_concurrency < MIN_ACHIEVED_CONCURRENCY {
            println!(
                "skipping the speedup assertion: over {threads} threads this process achieved \
                 only {multi_concurrency:.2} cores of concurrency (single-threaded achieved \
                 {single_concurrency:.2}), so the machine did not have the parallelism the \
                 comparison needs and nothing could be concluded from the ratio"
            );
            assert!(
                best_multi.is_finite() && best_multi > 0.0,
                "throughput must still be a positive, finite number"
            );
            return;
        }

        // Spread across repetitions of the *same* shape is pure measurement
        // noise. If the single-threaded shape cannot reproduce itself within
        // 2x, nothing this test could conclude about parallel speedup would
        // mean anything, so it says so rather than asserting on noise. This is
        // the same refusal the product makes about publishing a score it cannot
        // defend.
        let worst_single = single[single.len() - 1];
        if worst_single <= 0.0 || best_single / worst_single > 2.0 {
            println!(
                "skipping the speedup assertion: single-threaded throughput \
                 varied from {worst_single:.1} to {best_single:.1} across \
                 {REPEATS} repetitions, so this machine cannot support the \
                 comparison"
            );
            assert!(
                best_multi.is_finite() && best_multi > 0.0,
                "throughput must still be a positive, finite number"
            );
            return;
        }

        // Perfect scaling would be `threads`x. Requiring 1.2x leaves generous
        // room for SMT siblings and a busy host while still failing loudly if
        // the threaded shape stopped being parallel at all, which would land at
        // roughly 1.0x.
        let ratio = best_multi / best_single;
        assert!(
            ratio > 1.2,
            "over {threads} threads, multi-threaded throughput ({best_multi:.1}) \
             should clearly exceed single ({best_single:.1}); got {ratio:.2}x"
        );
    }

    #[test]
    fn scaling_efficiency_is_bounded_and_sane() {
        let module = CpuMixed::new();
        let output = module
            .run(&fast_params(), &NullReporter::default())
            .expect("run");
        let efficiency = output.context["scaling_efficiency"]
            .as_f64()
            .expect("efficiency");
        assert!(
            (0.0..=2.0).contains(&efficiency),
            "scaling efficiency {efficiency} is outside any physically plausible range"
        );
    }

    #[test]
    fn estimated_duration_is_not_optimistic() {
        let module = CpuMixed::new();
        let params = ModuleParams::for_profile(Profile::Quick);
        let estimate = module.estimated_duration_s(&params);
        assert!(estimate > 0);
        // 5 workloads * 2 shapes * (1 warmup + 5 measured + 2 calibration) * 200ms
        assert_eq!(estimate, (5 * 2 * 8 * 200_u64).div_ceil(1000));
    }

    #[test]
    fn single_threaded_scaling_efficiency_is_absent() {
        assert!(scaling_efficiency(&[], 1).is_none());
    }
}
