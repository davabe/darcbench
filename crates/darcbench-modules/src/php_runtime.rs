//! `php.runtime` - the Phase 3 PHP module.
//!
//! # What it measures
//!
//! | Metric | Unit | What it predicts |
//! |---|---|---|
//! | `json.encode` | ops/s | Building a response body: where every API request ends |
//! | `json.decode` | ops/s | Reading a request body or a cached blob |
//! | `array.ops` | ops/s | Sorting, filtering and looking up: every listing page |
//! | `template.render` | ops/s | Concatenating a page, which is what templates compile to |
//! | `hash.sha256` | ops/s | Sessions, ETags, cache keys, integrity checks |
//! | `hash.password` | ops/s | A login: the most expensive thing a page does per user |
//! | `startup.cold` | ms | Interpreter start and script compile, per request without a warm cache |
//!
//! Framework-free, as the roadmap specifies. A framework benchmark measures the
//! framework's authors; what a hosting buyer needs is what *this machine* does
//! with the handful of things every PHP application actually spends its time on.
//!
//! # It measures the operator's PHP, and that is the point
//!
//! Unlike `web.static`, whose origin is DARCBench's own so that two machines are
//! compared under the same server, this module executes the interpreter the
//! operator installed. It could not do otherwise, since DARCBench does not ship
//! a PHP, and it should not: "how will my site run here" is a question about the
//! PHP that is on the machine, with the extensions and limits that host set.
//!
//! The cost is that comparability is conditional, so every run discloses the
//! interpreter path, version, SAPI, OPcache state and memory limit, and those
//! fields are in the manifest's `comparability` list. Two PHP results from
//! differently configured interpreters are not comparable, and the comparison
//! layer must be able to refuse on evidence rather than on trust.
//!
//! # Executing a discovered binary
//!
//! This is the first module to do it, and it is the reason
//! [ADR-0013](../../../docs/adr/0013-executing-a-discovered-runtime.md) and
//! `docs/THREAT-MODEL.md` T-EXEC exist. The mechanics - a compile-time path
//! allow-list, a safe-path check on the binary and every ancestor directory,
//! fixed argv, no shell, a cleared environment and a hard timeout - live in
//! [`crate::runtime_exec`], which is where to read about why each is necessary.
//!
//! # What it deliberately does not measure
//!
//! - **OPcache's effect.** It is *disclosed*, not measured, which is what the
//!   methodology asks for. A framework-free single-file workload compiles once
//!   and then runs a loop, so an opcode cache barely touches it; publishing a
//!   with/without comparison from this workload would put a number on something
//!   the workload cannot see. It becomes measurable with the multi-file
//!   application workloads in Phase 4.
//! - **FPM, worker counts and pool behaviour.** Those are properties of a
//!   configuration this module refuses to touch (T-CONFIG). The SAPI actually
//!   measured is recorded so nobody reads a CLI number as an FPM one.
//! - **Anything about a PHP that is not on the machine.** No PHP means the
//!   module fails with a stated precondition rather than reporting zeroes.

use std::time::{Duration, Instant};

use darcbench_protocol::metrics::{Direction, Metric, Warning, WarningCode};
use darcbench_protocol::stats::{outlier_indices, summarize};
use darcbench_protocol::ModuleId;

use crate::harness::{calibrate_with, time_reps};
use crate::module::{
    BenchmarkModule, ModuleError, ModuleManifest, ModuleOutput, ModuleParams, ModuleReporter,
    SafetyClass,
};
use crate::runtime_exec::{self, ScriptFile};

/// Workload-definition version. Major bump = results are not comparable.
pub const VERSION: &str = "1.0.0";

/// The module's identifier, validated against the [`ModuleId`] grammar by a
/// unit test in this file.
pub const MODULE_ID: &str = "php.runtime";

/// The workload script, compiled into the binary.
///
/// Embedded rather than installed, so the agent stays one file that is copied
/// to a server and run - and so that nothing on the machine can substitute the
/// script the interpreter is given.
const BENCH_PHP: &str = include_str!("../php/bench.php");

/// Name the script is written under, inside the agent's own scratch directory.
///
/// A compile-time constant appended to an already-validated `StatePath`. There
/// is no string from any caller anywhere in this path.
const SCRIPT_NAME: &str = "darcbench-php-bench.php";

/// Where a PHP CLI binary may be executed from.
///
/// A compile-time allow-list, for the same reason `network.transfer`'s endpoint
/// table is one: `$PATH` is environment, and a benchmark that executes whatever
/// `php` resolves to executes whatever the environment says. See T-EXEC.
///
/// Ordered so an unversioned PHP wins over a versioned one, and a
/// panel-managed PHP is used only when there is no unversioned one - a Plesk or
/// cPanel host has both, and the unversioned path is the one the operator's
/// sites are most likely served by.
///
/// `/usr/local/bin` comes before `/usr/bin` because that is the order a default
/// `$PATH` uses: a source-built or hand-installed PHP there is what `php`
/// actually resolves to for the operator's own cron and shell, so measuring the
/// distro one instead would measure an interpreter nothing on the machine runs.
///
/// Every entry still has to pass the safe-path check before it is used, and
/// every entry that fails is reported whether or not it was reached first.
const PHP_CANDIDATES: &[&str] = &[
    "/usr/local/bin/php",
    "/usr/bin/php",
    "/usr/bin/php8.4",
    "/usr/bin/php8.3",
    "/usr/bin/php8.2",
    "/usr/bin/php8.1",
    "/usr/bin/php8.0",
    "/usr/bin/php7.4",
    // Panel-managed and alternative SAPIs, which is what a large share of this
    // market actually runs.
    "/opt/plesk/php/8.3/bin/php",
    "/opt/plesk/php/8.2/bin/php",
    "/opt/plesk/php/8.1/bin/php",
    "/opt/cpanel/ea-php83/root/usr/bin/php",
    "/opt/cpanel/ea-php82/root/usr/bin/php",
    "/opt/cpanel/ea-php81/root/usr/bin/php",
    "/usr/local/lsws/lsphp83/bin/php",
    "/usr/local/lsws/lsphp82/bin/php",
    // CloudLinux, which is a large share of the reseller hosting this product
    // is aimed at, and Remi, which is how most RHEL-family hosts get a current
    // PHP at all.
    "/opt/alt/php83/usr/bin/php",
    "/opt/alt/php82/usr/bin/php",
    "/opt/alt/php81/usr/bin/php",
    "/opt/remi/php83/root/usr/bin/php",
    "/opt/remi/php82/root/usr/bin/php",
];

/// Longest a single workload invocation may take before it is killed.
///
/// Generous, because `hash.password` at bcrypt cost 8 is deliberately expensive
/// and a slow machine is the case this module exists to detect. A benchmark
/// that hangs is still worse than one that fails.
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
        arg: "json_encode",
        key: "json.encode",
        label: "JSON encode",
    },
    Workload {
        arg: "json_decode",
        key: "json.decode",
        label: "JSON decode",
    },
    Workload {
        arg: "array_ops",
        key: "array.ops",
        label: "Array sort, filter and lookup",
    },
    Workload {
        arg: "string_template",
        key: "template.render",
        label: "HTML fragment assembly",
    },
    Workload {
        arg: "hash_general",
        key: "hash.sha256",
        label: "SHA-256 hashing",
    },
    Workload {
        arg: "hash_password",
        key: "hash.password",
        label: "Password hashing (bcrypt cost 8)",
    },
];

/// One `{"kind":"measure",...}` line from the script.
#[derive(Debug, serde::Deserialize)]
struct Measurement {
    elapsed_ms: f64,
    checksum: i64,
    #[serde(default)]
    peak_memory_bytes: u64,
}

pub struct PhpRuntime {
    manifest: ModuleManifest,
}

impl Default for PhpRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl PhpRuntime {
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
                title: "PHP runtime".into(),
                purpose: "Measure how fast this machine runs PHP: JSON, arrays, templating, \
                          hashing and interpreter start-up, against the interpreter the operator \
                          installed, with its version, SAPI and OPcache state disclosed."
                    .into(),
                // It runs a program it did not build. Nothing weaker describes
                // that honestly, and the class is what drives preflight's risk
                // classification and the operator's consent.
                safety_class: SafetyClass::ProvisionsServices,
                dependencies: vec!["a PHP CLI binary at an allow-listed, root-owned path".into()],
                // One small script, rewritten each run.
                max_bytes_written: 64 * 1024,
                max_network_bytes: 0,
                cleanup: "The workload script is removed from the scratch directory when the \
                          module returns, on every path including cancellation and an error. It \
                          is not removed on a panic, because the release profile sets \
                          `panic = \"abort\"` and destructors do not run on abort; the next run \
                          removes it before writing its own. No configuration is read or altered \
                          and no PHP process outlives the run."
                    .into(),
                validation: vec![
                    "The interpreter must be at an allow-listed path that is root-owned and \
                     writable only by root, along with every directory above it. A PHP that fails \
                     that check is refused and the reason is reported."
                        .into(),
                    "Every workload returns a checksum, and a repetition whose checksum differs \
                     from the first is rejected: it means the work performed was not the work \
                     that was measured."
                        .into(),
                    "Every metric needs at least three successful repetitions; below that it is \
                     withheld rather than reported from noise."
                        .into(),
                    "A debug or ZTS build of PHP is disclosed and degrades the result: neither is \
                     comparable with the ordinary NTS release build a host serves sites from."
                        .into(),
                ],
                limitations: vec![
                    "This measures the PHP the operator installed, so results are comparable only \
                     between machines running the same major version, SAPI and OPcache state. \
                     Those fields are recorded in every bundle for exactly that reason."
                        .into(),
                    "The CLI SAPI is what gets measured, because it is the only one that can be \
                     invoked without touching the operator's web server configuration. An FPM \
                     pool differs in worker model and usually in OPcache state; the SAPI actually \
                     used is recorded so nobody reads one as the other."
                        .into(),
                    "OPcache is disclosed, not measured. A framework-free single-file workload \
                     compiles once and then loops, so an opcode cache barely touches it - putting \
                     a number on it here would put a number on something this workload cannot \
                     see. It becomes measurable with the multi-file application workloads in \
                     Phase 4."
                        .into(),
                    "Password hashing is pinned to bcrypt cost 8 rather than the runtime's \
                     default. bcrypt cost is exponential, so comparing a cost-10 machine against \
                     a cost-12 one would compare the configurations rather than the machines."
                        .into(),
                    "`startup.cold` includes process creation as well as interpreter start-up, \
                     because that is what a request without a warm worker actually pays."
                        .into(),
                ],
                comparability: vec![
                    "module.version".into(),
                    "agent.build_target".into(),
                    "php.version".into(),
                    "php.sapi".into(),
                    "php.opcache_enabled".into(),
                    "php.zts".into(),
                ],
                // The methodology's `web.*` row: warn above 0.20. An
                // interpreter benchmark is steadier than an HTTP one, but it is
                // a process-per-repetition measurement, so it inherits the
                // scheduler's noise on a busy machine.
                stability_cv_bound: 0.20,
            },
        }
    }
}

impl BenchmarkModule for PhpRuntime {
    fn manifest(&self) -> &ModuleManifest {
        &self.manifest
    }

    fn estimated_duration_s(&self, params: &ModuleParams) -> u64 {
        let reps = u64::from(params.warmup_reps + params.measured_reps);
        let units = WORKLOADS.len() as u64 + 1;
        // Each repetition is a process spawn plus a calibrated slice of work,
        // and calibration itself costs a handful of spawns per workload.
        // Deliberately pessimistic: preflight may overstate and may never
        // understate.
        // 250 ms of process overhead per repetition, not 60. Cold start is
        // 30-40 ms on an idle machine and routinely 150-200 ms on the loaded,
        // shared hosts this module targets; `startup.cold` pays it eight times
        // per repetition; and calibration spends a handful of spawns per
        // workload that were not modelled at all. The comment on preflight is
        // that it may overstate and may never understate, and the earlier
        // figure understated exactly on the slow machines where that matters.
        let per_rep_ms = params.target_rep_ms + 250;
        (reps * units * per_rep_ms / 1000) + 10
    }

    fn run(
        &self,
        params: &ModuleParams,
        reporter: &dyn ModuleReporter,
    ) -> Result<ModuleOutput, ModuleError> {
        let (interpreter, rejections) = runtime_exec::discover(PHP_CANDIDATES);
        let Some(interpreter) = interpreter else {
            let refused = rejections
                .iter()
                .map(|r| format!("{} ({})", r.path.display(), r.reason))
                .collect::<Vec<_>>()
                .join("; ");
            return Err(ModuleError::Precondition(if refused.is_empty() {
                "no PHP CLI binary was found at any allow-listed path. This module measures the \
                 PHP the machine actually has; there is nothing to measure here, and reporting \
                 zeroes would be worse than saying so."
                    .into()
            } else {
                format!(
                    "a PHP binary exists but was refused as unsafe to execute: {refused}. The \
                     binary and every directory above it must be owned by root and writable only \
                     by root - see docs/THREAT-MODEL.md, T-EXEC."
                )
            }));
        };

        let scratch = params.scratch_dir.as_ref().ok_or_else(|| {
            ModuleError::Precondition(
                "no scratch directory was provided, and this module will not choose one of its \
                 own"
                .into(),
            )
        })?;
        let script = ScriptFile::write(scratch, SCRIPT_NAME, BENCH_PHP)?;
        let script_path = script.path.display().to_string();

        // --- disclosure -----------------------------------------------------
        //
        // Before any measurement, because a number whose runtime cannot be
        // described is not a number anyone can compare - and because a PHP that
        // cannot even run `describe` will not produce a usable measurement
        // either, and should fail here with a readable reason.
        let described = runtime_exec::run(
            &interpreter,
            &[&script_path, "describe"],
            INVOCATION_TIMEOUT,
        )
        .map_err(|error| {
            ModuleError::Precondition(format!("PHP could not be interrogated: {error}"))
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
                    "PHP produced no usable description. Its output was: {}",
                    described.stdout.trim()
                ))
            })?;

        let mut warnings = Vec::new();
        for (rejected, reason) in rejections
            .iter()
            .map(|r| (r.path.display().to_string(), &r.reason))
        {
            warnings.push(Warning {
                code: WarningCode::Informational,
                message: format!(
                    "A PHP binary at `{rejected}` was not used because it {reason}. This is worth \
                     looking at independently of the benchmark: a binary a non-root user can \
                     replace is a privilege-escalation path on any machine that runs it as root."
                ),
                metric_key: None,
            });
        }
        // A debug or thread-safe build is several times slower than the release
        // NTS build a host serves sites from, so a comparison against one is
        // meaningless and the result says so rather than looking merely poor.
        let flag = |key: &str| {
            description
                .get(key)
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
        };
        for (key, what) in [
            ("debug_build", "a debug build"),
            ("zts", "a thread-safe (ZTS) build"),
        ] {
            if flag(key) {
                let warning = Warning {
                    code: WarningCode::ValidationFailed,
                    message: format!(
                        "This is {what} of PHP. It is several times slower than the ordinary NTS \
                         release build a host serves sites from, so these numbers describe this \
                         interpreter rather than this machine."
                    ),
                    metric_key: None,
                };
                reporter.warn(warning.clone());
                warnings.push(warning);
            }
        }

        // --- workloads ------------------------------------------------------
        let total_units = WORKLOADS.len() as f64 + 1.0;
        let mut completed_units = 0.0;
        let mut metrics = Vec::new();
        let mut peak_memory = 0u64;

        for workload in WORKLOADS {
            if reporter.is_cancelled() {
                return Err(ModuleError::Cancelled);
            }

            let measure = |iterations: u64| -> Result<Measurement, ModuleError> {
                let count = iterations.to_string();
                let output = runtime_exec::run(
                    &interpreter,
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
                serde_json::from_str(output.stdout.trim()).map_err(|error| {
                    ModuleError::Workload(format!(
                        "`{}` produced no usable result: {error}",
                        workload.arg
                    ))
                })
            };

            // Calibrated on the script's *internal* elapsed time, so process
            // start-up does not enter the search. Otherwise a machine with a
            // slow fork would be calibrated to do less work per repetition and
            // then be reported as slow at PHP, which is a different claim.
            let mut calibration_error = None;
            let iterations = calibrate_with(params.target_rep_ms, reporter, |n| {
                match measure(n) {
                    Ok(result) => result.elapsed_ms,
                    Err(error) => {
                        calibration_error.get_or_insert(error);
                        // A value above any plausible target stops the search
                        // immediately rather than letting it grow the workload
                        // while every invocation fails.
                        f64::from(u32::MAX)
                    }
                }
            })?;
            if let Some(error) = calibration_error {
                return Err(error);
            }

            // The first repetition's checksum is the reference. A later one
            // that differs means the work performed was not the work measured.
            let mut expected_checksum: Option<i64> = None;
            let mut mismatched = false;
            let outcome = time_reps(
                params,
                reporter,
                workload.key,
                "ops/s",
                completed_units,
                total_units,
                |_rep| match measure(iterations) {
                    Ok(result) => {
                        match expected_checksum {
                            None => expected_checksum = Some(result.checksum),
                            Some(expected) if expected != result.checksum => mismatched = true,
                            Some(_) => {}
                        }
                        peak_memory = peak_memory.max(result.peak_memory_bytes);
                        let seconds = (result.elapsed_ms / 1000.0).max(f64::MIN_POSITIVE);
                        (iterations as f64 / seconds, result.elapsed_ms)
                    }
                    // A failed invocation is not a rate. Zero is filtered out
                    // below and the metric is withheld if too few survive.
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
        //
        // Timed from the outside, because process creation is part of what a
        // request without a warm worker pays and the script cannot see it.
        if reporter.is_cancelled() {
            return Err(ModuleError::Cancelled);
        }
        let startup = time_reps(
            params,
            reporter,
            "startup.cold",
            "ms",
            completed_units,
            total_units,
            |_rep| {
                // Several invocations per repetition: a single fork-exec is a
                // few tens of milliseconds, which is close enough to the
                // harness's trustworthy-duration floor that scheduler noise
                // would dominate one sample.
                const INVOCATIONS: u32 = 8;
                let started = Instant::now();
                #[allow(clippy::items_after_statements)]
                let mut ok = 0u32;
                for _ in 0..INVOCATIONS {
                    if runtime_exec::run(&interpreter, &[&script_path, "noop"], INVOCATION_TIMEOUT)
                        .map(|o| o.succeeded())
                        .unwrap_or(false)
                    {
                        ok += 1;
                    }
                }
                let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
                // Every invocation must have succeeded, or the repetition is
                // discarded. Dividing the elapsed time by the *successful*
                // count folded a 120-second timeout into the average and
                // published a cold start orders of magnitude too slow, which
                // reads as a finding about the machine.
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
            "Interpreter cold start",
            "ms",
            Direction::LowerIsBetter,
            &startup.measured,
            startup.samples,
        );

        // --- variance sweep -------------------------------------------------
        //
        // Over the finished list rather than at each construction site, for the
        // reason `network.transfer` records: a check inside one of two
        // construction paths made the manifest's promise true for some metrics
        // and false for others.
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
            "php".into(),
            serde_json::json!({
                "path": interpreter.path.display().to_string(),
                "version": description.get("version"),
                "sapi": description.get("sapi"),
                "opcache_loaded": description.get("opcache_loaded"),
                "opcache_enabled": description.get("opcache_enabled"),
                "opcache_jit": description.get("opcache_jit"),
                "memory_limit": description.get("memory_limit"),
                "zts": description.get("zts"),
                "debug_build": description.get("debug_build"),
                "extensions": description.get("extensions"),
            }),
        );
        context.insert("peak_php_memory_bytes".into(), peak_memory.into());
        context.insert("bcrypt_cost".into(), serde_json::json!(8));
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

/// The last JSON object on a stream, ignoring anything before it.
///
/// PHP CLI sends `display_errors` output to **stdout**, so a single startup
/// notice or ini deprecation on a machine with that enabled would prefix the
/// result line and turn the whole module into a precondition failure. Reading
/// the last line rather than the whole stream makes the module work on a
/// machine whose PHP is chatty, which is a great many of them.
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
            "darcbench-php-{name}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        // The scratch guard refuses a directory anyone else can write, and
        // `temp_dir()` inherits the umask - so the test makes it look like the
        // real thing rather than working around the guard.
        //
        // Without this the suite passes under umask 022 and fails under umask
        // 002, which is the default on Debian and Ubuntu with user-private
        // groups. `node_runtime.rs` already carried the fix; this twin did not,
        // so the failure looked like a broken guard rather than a test that
        // built its own fixture wrong.
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

    #[test]
    fn module_id_constant_satisfies_the_grammar() {
        assert!(ModuleId::new(MODULE_ID).is_ok());
        assert_eq!(PhpRuntime::new().manifest().id.as_str(), MODULE_ID);
    }

    #[test]
    fn manifest_is_well_formed() {
        let module = PhpRuntime::new();
        let manifest = module.manifest();
        assert_eq!(manifest.version, VERSION);
        assert_eq!(manifest.safety_class, SafetyClass::ProvisionsServices);
        assert_eq!(manifest.max_network_bytes, 0);
        assert!(manifest.max_bytes_written > 0, "it writes its own script");
        assert!(
            !manifest.dependencies.is_empty(),
            "PHP is a real dependency"
        );
        assert!(!manifest.limitations.is_empty());
        assert!(!manifest.validation.is_empty());
        assert!(manifest.stability_cv_bound > 0.0);
        // Comparability must name the runtime facts, or two incomparable PHP
        // results would be compared without anything noticing.
        for field in ["php.version", "php.sapi", "php.opcache_enabled"] {
            assert!(
                manifest.comparability.iter().any(|c| c == field),
                "`{field}` must be a comparability key"
            );
        }
    }

    /// Every candidate is an absolute path. A relative one would be resolved
    /// against the working directory, which is not a security boundary.
    #[test]
    fn every_allow_listed_path_is_absolute_and_unique() {
        let unique: std::collections::BTreeSet<&&str> = PHP_CANDIDATES.iter().collect();
        assert_eq!(unique.len(), PHP_CANDIDATES.len(), "duplicate candidate");
        for candidate in PHP_CANDIDATES {
            assert!(
                candidate.starts_with('/'),
                "`{candidate}` is not an absolute path"
            );
            assert!(!candidate.contains(".."), "`{candidate}` contains ..");
        }
    }

    /// The script must be a complete, self-contained PHP program, and it must
    /// not have acquired anything that reads the environment or the filesystem.
    #[test]
    fn the_embedded_script_is_self_contained() {
        assert!(BENCH_PHP.starts_with("<?php"));
        assert!(BENCH_PHP.contains("hrtime"), "timing must be monotonic");
        for forbidden in [
            "getenv(",
            "exec(",
            "shell_exec",
            "system(",
            "passthru",
            "file_get_contents",
            "include ",
            "require ",
        ] {
            assert!(
                !BENCH_PHP.contains(forbidden),
                "the workload script must not use `{forbidden}`"
            );
        }
        for workload in WORKLOADS {
            assert!(
                BENCH_PHP.contains(&format!("case '{}'", workload.arg)),
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

    /// The script is removed however the module leaves, which is what makes the
    /// manifest's cleanup promise hold on the error paths too.
    #[test]
    fn the_workload_script_does_not_survive_the_module() {
        let dir = scratch("cleanup");
        let path = dir.join(SCRIPT_NAME);
        {
            let script = ScriptFile::write(&dir, SCRIPT_NAME, BENCH_PHP).unwrap();
            assert!(script.path.exists());
            assert_eq!(script.path, path);
        }
        assert!(!path.exists(), "the script must not outlive the module");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A machine with no PHP must say so, not report zeroes.
    #[test]
    fn a_machine_without_php_fails_with_a_stated_reason() {
        let (found, _) = runtime_exec::discover(PHP_CANDIDATES);
        if found.is_some() {
            // This host has PHP; the negative path is covered by the unit test
            // on `discover` itself, and asserting it here would need the
            // interpreter to be removed.
            return;
        }
        let dir = scratch("nophp");
        let error = PhpRuntime::new()
            .run(&fast_params(dir.clone()), &NullReporter::default())
            .expect_err("a machine without PHP cannot produce a PHP measurement");
        assert!(matches!(error, ModuleError::Precondition(_)), "{error:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The full path, when this host has a PHP to run it against.
    #[test]
    fn a_full_run_measures_every_workload_and_discloses_the_runtime() {
        let (found, _) = runtime_exec::discover(PHP_CANDIDATES);
        if found.is_none() {
            return;
        }
        let dir = scratch("full");
        let output = PhpRuntime::new()
            .run(&fast_params(dir.clone()), &NullReporter::default())
            .expect("a host with an allow-listed PHP must produce a measurement");

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

        // Password hashing at bcrypt cost 8 is deliberately expensive and must
        // come out orders of magnitude below the cheap workloads. If it does
        // not, the cost parameter is not being applied and the metric is
        // measuring nothing anybody's login page pays.
        let password = output.metric_value("hash.password").unwrap();
        let encode = output.metric_value("json.encode").unwrap();
        assert!(
            encode > password * 10.0,
            "bcrypt must dominate: json {encode} vs password {password}"
        );

        // Disclosure is part of the deliverable.
        let php = &output.context["php"];
        assert!(php["path"].as_str().unwrap().starts_with('/'));
        assert!(!php["version"].as_str().unwrap().is_empty());
        assert!(!php["sapi"].as_str().unwrap().is_empty());
        assert!(php.get("opcache_enabled").is_some());
        assert_eq!(output.context["bcrypt_cost"], 8);

        // And the script is gone.
        assert!(!dir.join(SCRIPT_NAME).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cancellation_is_honoured() {
        let dir = scratch("cancel");
        let reporter = NullReporter::default();
        reporter.cancel();
        let error = PhpRuntime::new()
            .run(&fast_params(dir.clone()), &reporter)
            .expect_err("a cancelled module must not return a measurement");
        // Either it was cancelled, or this host has no PHP and said so first.
        assert!(
            matches!(error, ModuleError::Cancelled | ModuleError::Precondition(_)),
            "{error:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
