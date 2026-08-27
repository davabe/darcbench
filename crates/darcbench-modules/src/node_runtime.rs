//! `node.runtime` - the Phase 3 Node.js module.
//!
//! # What it measures
//!
//! | Metric | Unit | What it predicts |
//! |---|---|---|
//! | `json.stringify` | ops/s | Serialising a response body: where every API request ends |
//! | `json.parse` | ops/s | Reading a request body or a cached blob |
//! | `ssr.render` | ops/s | Server-side rendering, reduced to what every framework compiles to |
//! | `crypto.hash` | ops/s | Sessions, ETags, cache keys, integrity checks |
//! | `async.fileio` | ops/s | The event loop and the libuv thread pool between the CPU work |
//! | `module.load` | ops/s | A 64-module dependency tree resolved and compiled: cold-start cost |
//! | `startup.cold` | ms | Process creation and V8 start-up, per invocation without a warm worker |
//!
//! # Dependency installation is not build performance
//!
//! `docs/BENCHMARK-METHODOLOGY.md` is explicit: *"Node.js runs must separate
//! dependency installation from compilation - package download time is network
//! measurement, not build performance."*
//!
//! This module satisfies that in the strongest available way: it never installs
//! anything. `module.load` generates its own 64-module tree and measures
//! resolution, compilation and first execution with the require cache cleared
//! between iterations, which is the compile half of a cold start with the
//! download half structurally absent rather than merely subtracted. An
//! `npm install` benchmark would measure a registry, a CDN and a lockfile
//! resolver, which is `network.transfer`'s question and not this one.
//!
//! # It measures the operator's Node, and that is the point
//!
//! Same reasoning as `php.runtime`, and the same cost: comparability is
//! conditional, so every run discloses the Node version, the V8 version, the
//! architecture, whether the build is jitless, and the heap limit. Two Node
//! results from different major versions are not comparable, because V8 changes
//! what the same JavaScript costs between them.
//!
//! Executing a discovered binary is guarded by [`crate::runtime_exec`] - a
//! compile-time path allow-list, a safe-path check on the binary and every
//! ancestor directory, fixed argv, no shell, a cleared environment and a hard
//! timeout. See
//! [ADR-0013](../../../docs/adr/0013-executing-a-discovered-runtime.md) and
//! `docs/THREAT-MODEL.md` T-EXEC.
//!
//! **`nvm`, `fnm` and `volta` installs are not measured**, and that is not an
//! oversight. They live under `$HOME`, which is not a fixed path and is owned
//! by an ordinary user - so they fail the safe-path check by construction. A
//! benchmark running as root must not execute a binary the user it is
//! benchmarking for can rewrite. The refusal is reported, so an operator whose
//! Node is installed that way learns why rather than seeing an empty result.
//!
//! # What it deliberately does not measure
//!
//! - **`npm install`.** See above.
//! - **Worker threads and clustering.** Node's concurrency story for a web
//!   server is one process per core behind a load balancer, which is a
//!   deployment property rather than a machine one, and `web.static` already
//!   measures what the machine does with many concurrent connections.
//! - **TypeScript compilation.** It needs a toolchain this module would have to
//!   install, which is the thing the methodology says not to fold in.

use std::time::{Duration, Instant};

use darcbench_protocol::metrics::{Direction, Metric, Warning, WarningCode};
use darcbench_protocol::stats::{outlier_indices, summarize};
use darcbench_protocol::ModuleId;

use crate::harness::{calibrate_with, time_reps};
use crate::module::{
    BenchmarkModule, ModuleError, ModuleManifest, ModuleOutput, ModuleParams, ModuleReporter,
    SafetyClass,
};
use crate::runtime_exec::{self, Interpreter, ScriptFile};

/// Workload-definition version. Major bump = results are not comparable.
pub const VERSION: &str = "1.0.0";

/// The module's identifier, validated against the [`ModuleId`] grammar by a
/// unit test in this file.
pub const MODULE_ID: &str = "node.runtime";

/// The workload script, compiled into the binary.
const BENCH_JS: &str = include_str!("../js/bench.cjs");

/// Name the script is written under, inside the agent's own scratch directory.
///
/// `.cjs` rather than `.js` so it stays CommonJS whatever a `package.json`
/// somewhere above the scratch directory says. `module.load` measures
/// `require`, and an accidental ESM reinterpretation would measure a different
/// loader.
const SCRIPT_NAME: &str = "darcbench-node-bench.cjs";

/// Where a Node binary may be executed from.
///
/// A compile-time allow-list, for the same reason `network.transfer`'s endpoint
/// table is one: `$PATH` is environment, and a benchmark that executes whatever
/// `node` resolves to executes whatever the environment says.
///
/// `/usr/local/bin` precedes `/usr/bin` because that is the order a default
/// `$PATH` uses, and a hand-installed Node there is what `node` actually
/// resolves to for the operator's own cron and shell.
///
/// Version-manager installs (`nvm`, `fnm`, `volta`, `asdf`) are absent because
/// they live under `$HOME` - not a fixed path, and owned by an ordinary user,
/// so they would fail the safe-path check anyway. Listing them would only turn
/// a clean "no Node found" into a confusing rejection.
const NODE_CANDIDATES: &[&str] = &[
    "/usr/local/bin/node",
    "/usr/bin/node",
    // Debian shipped the binary as `nodejs` for years and plenty of hosts still
    // have only that name.
    "/usr/bin/nodejs",
    "/usr/local/bin/nodejs",
    // The shape a tarball install from nodejs.org takes when an administrator
    // unpacks it into /opt.
    "/opt/node22/bin/node",
    "/opt/node20/bin/node",
    "/opt/node18/bin/node",
    "/opt/nodejs/bin/node",
];

/// Longest a single workload invocation may take before it is killed.
const INVOCATION_TIMEOUT: Duration = Duration::from_secs(120);

/// One measured workload.
struct Workload {
    /// Argument passed to the script.
    arg: &'static str,
    key: &'static str,
    label: &'static str,
}

const WORKLOADS: &[Workload] = &[
    Workload {
        arg: "json_stringify",
        key: "json.stringify",
        label: "JSON stringify",
    },
    Workload {
        arg: "json_parse",
        key: "json.parse",
        label: "JSON parse",
    },
    Workload {
        arg: "ssr_render",
        key: "ssr.render",
        label: "Server-side render",
    },
    Workload {
        arg: "crypto_hash",
        key: "crypto.hash",
        label: "SHA-256 hashing",
    },
    Workload {
        arg: "async_fileio",
        key: "async.fileio",
        label: "Async file I/O through the event loop",
    },
    Workload {
        arg: "module_load",
        key: "module.load",
        label: "Require a 64-module tree",
    },
];

/// One `{"kind":"measure",...}` line from the script.
#[derive(Debug, serde::Deserialize)]
struct Measurement {
    elapsed_ms: f64,
    checksum: f64,
    #[serde(default)]
    heap_used_bytes: u64,
}

pub struct NodeRuntime {
    manifest: ModuleManifest,
}

impl Default for NodeRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl NodeRuntime {
    pub fn new() -> Self {
        // Justified `expect`: `MODULE_ID` is a compile-time constant whose
        // validity under the ModuleId grammar is asserted by a unit test in
        // this file. There is no runtime input here to fail on.
        #[allow(clippy::expect_used)]
        let id = ModuleId::new(MODULE_ID).expect("MODULE_ID is a valid module id");
        Self {
            manifest: ModuleManifest {
                id,
                version: VERSION.into(),
                title: "Node.js runtime".into(),
                purpose: "Measure how fast this machine runs Node.js: JSON, server-side \
                          rendering, hashing, event-loop file I/O, module loading and process \
                          start-up, against the runtime the operator installed, with its Node \
                          and V8 versions disclosed."
                    .into(),
                safety_class: SafetyClass::ProvisionsServices,
                dependencies: vec!["a Node.js binary at an allow-listed, root-owned path".into()],
                // The script, plus a 64-module tree and the small files the
                // async workload cycles through.
                max_bytes_written: 1024 * 1024,
                max_network_bytes: 0,
                cleanup: "The workload script and everything it generated are removed when the \
                          module returns, on every path including cancellation and an error. They \
                          are not removed on a panic, because the release profile sets \
                          `panic = \"abort\"` and destructors do not run on abort; the next run \
                          removes them before writing its own. No configuration is read or \
                          altered and no Node process outlives the run."
                    .into(),
                validation: vec![
                    "The runtime must be at an allow-listed path that is root-owned and writable \
                     only by root, along with every directory above it. A Node that fails that \
                     check is refused and the reason is reported."
                        .into(),
                    "Every workload returns a checksum, and a repetition whose checksum differs \
                     from the first is rejected: V8 is an optimising compiler, and a loop whose \
                     result is never observed is a loop it is entitled to delete."
                        .into(),
                    "Every metric needs at least three successful repetitions; below that it is \
                     withheld rather than reported from noise."
                        .into(),
                    "A jitless or lite-mode build of Node is disclosed and degrades the result: \
                     it runs an order of magnitude slower and would otherwise read as a slow \
                     machine."
                        .into(),
                ],
                limitations: vec![
                    "This measures the Node the operator installed, so results are comparable \
                     only between machines running the same major version - V8 changes what the \
                     same JavaScript costs between releases. The version is recorded in every \
                     bundle for exactly that reason."
                        .into(),
                    "Dependency installation is not measured at all, rather than measured and \
                     subtracted. `module.load` generates its own tree, so the download half of a \
                     cold start is structurally absent - which is what the methodology asks for, \
                     since package download time is a network measurement."
                        .into(),
                    "Version-manager installs (nvm, fnm, volta, asdf) are not measured. They live \
                     under $HOME, owned by an ordinary user, so a benchmark running as root must \
                     not execute them. The refusal is reported rather than silent."
                        .into(),
                    "`async.fileio` deliberately uses small files and shallow batches, so it \
                     measures the event loop and the libuv thread pool rather than the device. \
                     `storage.mixed` measures the device; doing it again here would put the same \
                     disk in two categories."
                        .into(),
                    "Worker threads and clustering are not measured. Node's concurrency story for \
                     a web server is one process per core behind a load balancer, which is a \
                     deployment property rather than a machine one."
                        .into(),
                    "`startup.cold` includes process creation as well as V8 start-up, because \
                     that is what an invocation without a warm worker actually pays."
                        .into(),
                ],
                comparability: vec![
                    "module.version".into(),
                    "agent.build_target".into(),
                    "node.version".into(),
                    "node.v8".into(),
                    "node.jitless".into(),
                ],
                // The methodology's `web.*` row: warn above 0.20.
                stability_cv_bound: 0.20,
            },
        }
    }

    /// Measures against an interpreter that has already been checked.
    ///
    /// Split from [`BenchmarkModule::run`] so the measurement path can be
    /// tested on a host whose Node fails the safe-path check - which is not a
    /// hypothetical: a machine whose only Node came from a tarball unpacked by
    /// an ordinary user is exactly the case the check exists for, and it is
    /// also a machine where nothing would otherwise exercise this code.
    ///
    /// The security boundary is [`runtime_exec::discover`], and it stays on the
    /// production path unconditionally.
    fn measure(
        &self,
        interpreter: &Interpreter,
        rejections: &[runtime_exec::Rejection],
        params: &ModuleParams,
        reporter: &dyn ModuleReporter,
    ) -> Result<ModuleOutput, ModuleError> {
        let scratch = params.scratch_dir.as_ref().ok_or_else(|| {
            ModuleError::Precondition(
                "no scratch directory was provided, and this module will not choose one of its \
                 own"
                .into(),
            )
        })?;
        let script = ScriptFile::write(scratch, SCRIPT_NAME, BENCH_JS)?;
        let script_path = script.path.display().to_string();

        // Disclosure before measurement: a number whose runtime cannot be
        // described is not a number anyone can compare, and a Node that cannot
        // run `describe` will not produce a usable measurement either.
        let described =
            runtime_exec::run(interpreter, &[&script_path, "describe"], INVOCATION_TIMEOUT)
                .map_err(|error| {
                    ModuleError::Precondition(format!("Node could not be interrogated: {error}"))
                })?;
        if !described.succeeded() {
            return Err(ModuleError::Precondition(format!(
                "`{} {script_path} describe` exited with {:?}: {}",
                interpreter.path.display(),
                described.status,
                described.stderr.trim()
            )));
        }
        let description: serde_json::Value =
            parse_last_line(&described.stdout).ok_or_else(|| {
                ModuleError::Precondition(format!(
                    "Node produced no usable description. Its output was: {}",
                    described.stdout.trim()
                ))
            })?;

        let mut warnings = Vec::new();
        for rejection in rejections {
            warnings.push(Warning {
                code: WarningCode::Informational,
                message: format!(
                    "A Node binary at `{}` was not used because it {}. This is worth looking at \
                     independently of the benchmark: a binary a non-root user can replace is a \
                     privilege-escalation path on any machine that runs it as root.",
                    rejection.path.display(),
                    rejection.reason
                ),
                metric_key: None,
            });
        }
        if description
            .get("jitless")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            let warning = Warning {
                code: WarningCode::ValidationFailed,
                message: "This Node was built without the JIT (lite mode). It runs an order of \
                          magnitude slower than the build anyone serves traffic with, so these \
                          numbers describe this binary rather than this machine."
                    .into(),
                metric_key: None,
            };
            reporter.warn(warning.clone());
            warnings.push(warning);
        }

        let total_units = WORKLOADS.len() as f64 + 1.0;
        let mut completed_units = 0.0;
        let mut metrics = Vec::new();
        let mut peak_heap = 0u64;

        for workload in WORKLOADS {
            if reporter.is_cancelled() {
                return Err(ModuleError::Cancelled);
            }

            let measure_once = |iterations: u64| -> Result<Measurement, ModuleError> {
                let count = iterations.to_string();
                let output = runtime_exec::run(
                    interpreter,
                    &[&script_path, workload.arg, &count],
                    INVOCATION_TIMEOUT,
                )
                .map_err(|error| ModuleError::Workload(error.to_string()))?;
                if !output.succeeded() {
                    return Err(ModuleError::Workload(format!(
                        "`{}` exited with {:?}: {}",
                        workload.arg,
                        output.status,
                        output.stderr.trim()
                    )));
                }
                parse_last_line(&output.stdout).ok_or_else(|| {
                    ModuleError::Workload(format!(
                        "`{}` produced no usable result. Its output was: {}",
                        workload.arg,
                        output.stdout.trim()
                    ))
                })
            };

            // Calibrated on the script's *internal* elapsed time, so process
            // start-up does not enter the search. A machine with a slow fork
            // would otherwise be calibrated to do less work per repetition and
            // then reported as slow at JavaScript, which is a different claim.
            let mut calibration_error = None;
            let iterations = calibrate_with(params.target_rep_ms, reporter, |n| {
                match measure_once(n) {
                    Ok(result) => result.elapsed_ms,
                    Err(error) => {
                        calibration_error.get_or_insert(error);
                        // Above any plausible target, so the search stops on the
                        // first probe rather than growing the workload while
                        // every invocation fails.
                        f64::from(u32::MAX)
                    }
                }
            })?;
            if let Some(error) = calibration_error {
                return Err(error);
            }

            let mut expected_checksum: Option<f64> = None;
            let mut mismatched = false;
            let outcome = time_reps(
                params,
                reporter,
                workload.key,
                "ops/s",
                completed_units,
                total_units,
                |_rep| match measure_once(iterations) {
                    Ok(result) => {
                        match expected_checksum {
                            None => expected_checksum = Some(result.checksum),
                            // Exact equality is right here: the checksums are
                            // integer-valued sums that JSON happens to carry as
                            // numbers, so any difference at all means different
                            // work was done.
                            Some(expected) if expected != result.checksum => mismatched = true,
                            Some(_) => {}
                        }
                        peak_heap = peak_heap.max(result.heap_used_bytes);
                        let seconds = (result.elapsed_ms / 1000.0).max(f64::MIN_POSITIVE);
                        (iterations as f64 / seconds, result.elapsed_ms)
                    }
                    Err(_) => (0.0, 0.0),
                },
            )?;
            completed_units += 1.0;
            warnings.extend(outcome.warnings.clone());

            if mismatched {
                let warning = Warning {
                    code: WarningCode::ValidationFailed,
                    message: format!(
                        "`{}` produced a different checksum between repetitions, so the work \
                         performed was not the work measured. The metric is withheld.",
                        workload.key
                    ),
                    metric_key: Some(workload.key.to_string()),
                };
                reporter.warn(warning.clone());
                warnings.push(warning);
                continue;
            }

            push_metric(
                &mut metrics,
                &mut warnings,
                workload.key,
                workload.label,
                "ops/s",
                Direction::HigherIsBetter,
                &outcome.measured,
                outcome.samples,
            );
        }

        // --- cold start -----------------------------------------------------
        if reporter.is_cancelled() {
            return Err(ModuleError::Cancelled);
        }
        const INVOCATIONS: u32 = 8;
        let startup = time_reps(
            params,
            reporter,
            "startup.cold",
            "ms",
            completed_units,
            total_units,
            |_rep| {
                let started = Instant::now();
                let mut ok = 0u32;
                for _ in 0..INVOCATIONS {
                    if runtime_exec::run(interpreter, &[&script_path, "noop"], INVOCATION_TIMEOUT)
                        .map(|o| o.succeeded())
                        .unwrap_or(false)
                    {
                        ok += 1;
                    }
                }
                let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
                // Every invocation must have succeeded, or the repetition is
                // discarded: dividing by the successful count alone would fold
                // a timeout into the average and publish it as a slow machine.
                let per_start = if ok == INVOCATIONS {
                    elapsed_ms / f64::from(INVOCATIONS)
                } else {
                    0.0
                };
                (per_start, elapsed_ms)
            },
        )?;
        warnings.extend(startup.warnings.clone());
        push_metric(
            &mut metrics,
            &mut warnings,
            "startup.cold",
            "Process and V8 cold start",
            "ms",
            Direction::LowerIsBetter,
            &startup.measured,
            startup.samples,
        );

        // --- variance sweep -------------------------------------------------
        for metric in &metrics {
            if let Some(cv) = metric.summary.cv {
                if cv > self.manifest.stability_cv_bound {
                    let warning = Warning {
                        code: WarningCode::HighVariance,
                        message: format!(
                            "`{}` varied by {:.0}% across repetitions, above this module's {:.0}% \
                             bound. Each repetition is its own process, so this usually means the \
                             machine was doing something else at the same time.",
                            metric.key,
                            cv * 100.0,
                            self.manifest.stability_cv_bound * 100.0
                        ),
                        metric_key: Some(metric.key.clone()),
                    };
                    reporter.warn(warning.clone());
                    warnings.push(warning);
                }
            }
        }

        let mut context = serde_json::Map::new();
        context.insert("workload_version".into(), VERSION.into());
        context.insert(
            "build_target".into(),
            format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS).into(),
        );
        context.insert(
            "node".into(),
            serde_json::json!({
                "path": interpreter.path.display().to_string(),
                "version": description.get("version"),
                "v8": description.get("v8"),
                "uv": description.get("uv"),
                "arch": description.get("arch"),
                "jitless": description.get("jitless"),
                "pointer_compression": description.get("pointer_compression"),
                "uv_threadpool_size": description.get("uv_threadpool_size"),
                "heap_size_limit_bytes": description.get("heap_size_limit_bytes"),
            }),
        );
        context.insert("peak_node_heap_bytes".into(), peak_heap.into());
        context.insert("module_tree_size".into(), serde_json::json!(64));
        context.insert(
            "dependency_install".into(),
            "not performed; module.load generates its own tree, so package download is \
             structurally absent rather than subtracted"
                .into(),
        );
        context.insert(
            "refused_interpreters".into(),
            serde_json::json!(rejections
                .iter()
                .map(|r| format!("{}: {}", r.path.display(), r.reason))
                .collect::<Vec<_>>()),
        );

        Ok(ModuleOutput {
            metrics,
            warnings,
            context,
        })
    }
}

impl BenchmarkModule for NodeRuntime {
    fn manifest(&self) -> &ModuleManifest {
        &self.manifest
    }

    fn estimated_duration_s(&self, params: &ModuleParams) -> u64 {
        let reps = u64::from(params.warmup_reps + params.measured_reps);
        let units = WORKLOADS.len() as u64 + 1;
        // 250 ms of process overhead per repetition: Node cold start is 30-60 ms
        // on an idle machine and routinely far more on a loaded shared host,
        // `startup.cold` pays it eight times per repetition, and calibration
        // spends a handful of spawns per workload.
        let per_rep_ms = params.target_rep_ms + 250;
        (reps * units * per_rep_ms / 1000) + 10
    }

    fn estimated_write_volume_bytes(&self, params: &ModuleParams) -> u64 {
        // `async.fileio` cycles 4 KiB files for a whole repetition. Deliberately
        // generous: an estimate that understates flash wear is worse than one
        // that overstates it.
        let reps = u64::from(params.warmup_reps + params.measured_reps);
        reps * 32 * 1024 * 1024
    }

    fn run(
        &self,
        params: &ModuleParams,
        reporter: &dyn ModuleReporter,
    ) -> Result<ModuleOutput, ModuleError> {
        let (interpreter, rejections) = runtime_exec::discover(NODE_CANDIDATES);
        let Some(interpreter) = interpreter else {
            let refused = rejections
                .iter()
                .map(|r| format!("{} ({})", r.path.display(), r.reason))
                .collect::<Vec<_>>()
                .join("; ");
            return Err(ModuleError::Precondition(if refused.is_empty() {
                "no Node.js binary was found at any allow-listed path. Version-manager installs \
                 (nvm, fnm, volta, asdf) live under $HOME and are deliberately not searched: they \
                 are owned by an ordinary user, and a benchmark running as root must not execute \
                 a binary that user can rewrite."
                    .into()
            } else {
                format!(
                    "a Node.js binary exists but was refused as unsafe to execute: {refused}. The \
                     binary and every directory above it must be owned by root and writable only \
                     by root - see docs/THREAT-MODEL.md, T-EXEC."
                )
            }));
        };
        self.measure(&interpreter, &rejections, params, reporter)
    }
}

/// The last JSON object on a stream, ignoring anything before it.
///
/// Node writes warnings - deprecations, experimental-feature notices - to
/// stderr rather than stdout, so this is less load-bearing than the PHP
/// equivalent. It is here anyway because a `console.log` added to the workload
/// script during debugging should degrade the result rather than destroy it.
fn parse_last_line<T: serde::de::DeserializeOwned>(stdout: &str) -> Option<T> {
    stdout
        .lines()
        .rev()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .find_map(|line| serde_json::from_str(line).ok())
}

#[allow(clippy::too_many_arguments)]
fn push_metric(
    metrics: &mut Vec<Metric>,
    warnings: &mut Vec<Warning>,
    key: &str,
    label: &str,
    unit: &str,
    direction: Direction,
    samples: &[f64],
    raw: Vec<darcbench_protocol::metrics::MetricSample>,
) {
    let usable: Vec<f64> = samples.iter().copied().filter(|v| *v > 0.0).collect();
    match summarize(&usable) {
        Some(summary) if usable.len() >= 3 => metrics.push(Metric {
            key: key.into(),
            label: label.into(),
            value: summary.median,
            unit: unit.into(),
            direction,
            outliers: outlier_indices(&usable, 3.5),
            summary,
            samples: raw,
            measures_dispersion: false,
            tail_quantile: false,
        }),
        _ => warnings.push(Warning {
            code: WarningCode::ValidationFailed,
            message: format!(
                "`{key}` produced {} usable repetitions, below the three needed to report a \
                 value; it is withheld rather than published from noise.",
                usable.len()
            ),
            metric_key: Some(key.to_string()),
        }),
    }
}

#[cfg(test)]
// In tests, `unwrap`/`expect` panicking *is* the failure signal.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic_in_result_fn)]
mod tests {
    use super::*;
    use crate::module::NullReporter;
    use darcbench_protocol::Profile;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "darcbench-node-{name}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        // The scratch guard refuses a directory anyone else can write, and
        // `temp_dir()` inherits the umask - so the test makes it look like the
        // real thing rather than working around the guard.
        std::fs::set_permissions(
            &dir,
            <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o700),
        )
        .unwrap();
        dir
    }

    fn fast_params(dir: std::path::PathBuf) -> ModuleParams {
        let mut params = ModuleParams::for_profile(Profile::Quick);
        params.warmup_reps = 0;
        params.measured_reps = 3;
        params.target_rep_ms = 40;
        params.with_scratch_dir(dir)
    }

    /// Any Node this host has, safe or not.
    ///
    /// The measurement path is worth testing even where the safe-path check
    /// refuses the only Node available - which is the case on a machine whose
    /// Node came from a tarball unpacked by an ordinary user, and is the case
    /// here. `discover` is the security boundary and it is exercised by its own
    /// tests and by `run`; this only bypasses *which* binary, never how it is
    /// invoked.
    fn any_node() -> Option<Interpreter> {
        NODE_CANDIDATES
            .iter()
            .map(std::path::Path::new)
            .find(|path| path.is_file())
            .map(|path| Interpreter {
                path: path.to_path_buf(),
            })
    }

    #[test]
    fn module_id_constant_satisfies_the_grammar() {
        assert!(ModuleId::new(MODULE_ID).is_ok());
        assert_eq!(NodeRuntime::new().manifest().id.as_str(), MODULE_ID);
    }

    #[test]
    fn manifest_is_well_formed() {
        let module = NodeRuntime::new();
        let manifest = module.manifest();
        assert_eq!(manifest.version, VERSION);
        assert_eq!(manifest.safety_class, SafetyClass::ProvisionsServices);
        assert_eq!(manifest.max_network_bytes, 0, "nothing is downloaded");
        assert!(manifest.max_bytes_written > 0);
        assert!(!manifest.dependencies.is_empty());
        assert!(manifest.stability_cv_bound > 0.0);
        for field in ["node.version", "node.v8", "node.jitless"] {
            assert!(
                manifest.comparability.iter().any(|c| c == field),
                "`{field}` must be a comparability key"
            );
        }
        // The methodology's rule about dependency installation must be visible
        // to a reader of the bundle, not only to a reader of the source.
        let text = manifest.limitations.join(" ");
        assert!(text.contains("Dependency installation"));
        assert!(
            text.contains("nvm"),
            "the version-manager refusal is a real gap"
        );
    }

    #[test]
    fn every_allow_listed_path_is_absolute_and_unique() {
        let unique: std::collections::BTreeSet<&&str> = NODE_CANDIDATES.iter().collect();
        assert_eq!(unique.len(), NODE_CANDIDATES.len(), "duplicate candidate");
        for candidate in NODE_CANDIDATES {
            assert!(candidate.starts_with('/'), "`{candidate}` is not absolute");
            assert!(!candidate.contains(".."), "`{candidate}` contains ..");
            // A version-manager path would be under a home directory, which is
            // not a fixed location and is user-owned.
            assert!(
                !candidate.contains(".nvm") && !candidate.contains("$HOME"),
                "`{candidate}` is not a fixed, system-owned path"
            );
        }
    }

    #[test]
    fn the_embedded_script_is_self_contained_and_installs_nothing() {
        assert!(BENCH_JS.contains("'use strict'"));
        assert!(BENCH_JS.contains("hrtime"), "timing must be monotonic");
        // The methodology's rule, enforced rather than promised: nothing here
        // may reach the network or a package manager.
        for forbidden in [
            "child_process",
            "npm",
            "node:http",
            "node:https",
            "node:net",
            "fetch(",
            "eval(",
            "process.env.",
        ] {
            assert!(
                !BENCH_JS.contains(forbidden),
                "the workload script must not use `{forbidden}`"
            );
        }
        for workload in WORKLOADS {
            assert!(
                BENCH_JS.contains(&format!("case '{}'", workload.arg)),
                "the script implements no `{}` workload",
                workload.arg
            );
        }
    }

    #[test]
    fn metric_keys_are_unique_and_in_the_reference_alphabet() {
        let mut keys: Vec<&str> = WORKLOADS.iter().map(|w| w.key).collect();
        keys.push("startup.cold");
        let unique: std::collections::BTreeSet<&&str> = keys.iter().collect();
        assert_eq!(unique.len(), keys.len());
        for key in &keys {
            assert!(key
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '_'));
        }
    }

    /// A machine whose only Node is user-owned must refuse it and say why,
    /// rather than executing it or reporting an empty result.
    #[test]
    fn an_unsafe_node_is_refused_with_a_reason_rather_than_executed() {
        let (chosen, rejections) = runtime_exec::discover(NODE_CANDIDATES);
        if chosen.is_some() {
            // This host has a root-owned Node, so the negative path does not
            // arise here; `runtime_exec`'s own tests cover the check.
            return;
        }
        if rejections.is_empty() {
            // No Node at all: also a valid state, covered by the precondition
            // text asserted below.
        }
        let dir = scratch("unsafe");
        let error = NodeRuntime::new()
            .run(&fast_params(dir.clone()), &NullReporter::default())
            .expect_err("an unsafe or absent Node cannot produce a measurement");
        let message = error.to_string();
        assert!(matches!(error, ModuleError::Precondition(_)), "{error:?}");
        assert!(
            message.contains("T-EXEC") || message.contains("nvm"),
            "the refusal must explain itself: {message}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The full measurement path, against whatever Node this host has.
    #[test]
    fn a_full_run_measures_every_workload_and_discloses_the_runtime() {
        let Some(node) = any_node() else { return };
        let dir = scratch("full");
        let output = NodeRuntime::new()
            .measure(
                &node,
                &[],
                &fast_params(dir.clone()),
                &NullReporter::default(),
            )
            .expect("a host with a Node must produce a measurement");

        for workload in WORKLOADS {
            let metric = output
                .metric(workload.key)
                .unwrap_or_else(|| panic!("missing `{}`", workload.key));
            assert!(metric.value > 0.0, "{}: {}", workload.key, metric.value);
            assert_eq!(metric.unit, "ops/s");
            assert_eq!(metric.direction, Direction::HigherIsBetter);
        }
        let startup = output.metric("startup.cold").expect("startup.cold");
        assert_eq!(startup.direction, Direction::LowerIsBetter);
        assert!(startup.value > 0.0);

        // Requiring a 64-module tree is thousands of times more expensive than
        // serialising one small object. If it is not, `module.load` is not
        // loading anything.
        let stringify = output.metric_value("json.stringify").unwrap();
        let modules = output.metric_value("module.load").unwrap();
        assert!(
            stringify > modules * 100.0,
            "a 64-module tree must dominate a single stringify: {stringify} vs {modules}"
        );

        let node = &output.context["node"];
        assert!(node["path"].as_str().unwrap().starts_with('/'));
        assert!(!node["version"].as_str().unwrap().is_empty());
        assert!(!node["v8"].as_str().unwrap().is_empty());
        assert_eq!(node["jitless"], false);
        assert_eq!(output.context["module_tree_size"], 64);

        // Everything the workload generated is gone, script included.
        assert!(!dir.join(SCRIPT_NAME).exists());
        assert!(!dir.join("darcbench-node-work").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cancellation_is_honoured() {
        let dir = scratch("cancel");
        let reporter = NullReporter::default();
        reporter.cancel();
        let error = NodeRuntime::new()
            .run(&fast_params(dir.clone()), &reporter)
            .expect_err("a cancelled module must not return a measurement");
        assert!(
            matches!(error, ModuleError::Cancelled | ModuleError::Precondition(_)),
            "{error:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
