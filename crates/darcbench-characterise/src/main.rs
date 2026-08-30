//! Runs the portable measurement engine and prints every repetition it took.
//!
//! # What this is for
//!
//! Before a machine can anchor a scoring model it has to be characterised, and
//! for the client line that includes characterising it *under each operating
//! system it will be compared across*
//! ([ADR-0016](../../../docs/adr/0016-client-reference-darc-ref-c1.md)). Nothing
//! in this workspace could do that: `darcbench-agent` is the server line and is
//! Linux-only by design, and `darcbench-core` is a library with no entry point.
//! So the cross-OS delta that ADR-0016 promises to disclose was not measurable
//! by anyone, including its author.
//!
//! This is the missing instrument. It runs `cpu.mixed` and `memory.bandwidth`
//! through the real [`BenchmarkModule`] path - the same calibration, the same
//! repetition loop, the same statistics the product publishes - and writes one
//! CSV row per repetition.
//!
//! # What this is not
//!
//! **It never produces a score.** No reference profile is consulted, no
//! normalisation happens, and no bundle is written or signed. The output is raw
//! measurements in physical units, which is the only thing that can honestly be
//! compared across two operating systems before anyone knows what the delta is.
//! Scoring a machine against an anchor calibrated on the other OS is the
//! question this instrument exists to make answerable, not one it may assume.
//!
//! # Identical inputs on purpose
//!
//! `MachineFacts` is left at its default - "nothing known" - on every platform.
//! That is deliberate. Fact discovery is `/proc` and `/sys` on Linux and would
//! have to be WMI on Windows, so populating it would feed the two runs
//! *different* inputs and quietly fold the difference between two inventory
//! implementations into a number reported as a difference between two operating
//! systems. The modules document their fallbacks for absent facts and record
//! that they used them.
//!
//! The consequence, stated because it is a real limit: `memory.bandwidth` sizes
//! its working set from an assumed cache rather than the real one, so its
//! absolute figures are not comparable with a full agent run. The *delta*
//! between two operating systems on one machine, which is what this measures,
//! is unaffected, because both sides assume the same thing.
//!
//! # Usage
//!
//! ```text
//! darcbench-characterise [--profile quick|standard|deep] [--passes N] [--label TEXT]
//! ```
//!
//! CSV on stdout, one row per repetition. Provenance as NDJSON on stderr, one
//! object per module per pass, carrying the calibrated work sizes and the ISA
//! dispatch the run actually took. Redirect them separately:
//!
//! ```text
//! darcbench-characterise --label windows > windows.csv 2> windows.ndjson
//! ```
//!
//! See `docs/CHARACTERISATION-RUNBOOK.md` for the procedure the output is for.

use std::io::Write as _;

use darcbench_core::cpu_mixed::CpuMixed;
use darcbench_core::memory_bandwidth::MemoryBandwidth;
use darcbench_core::module::NullReporter;
use darcbench_core::{BenchmarkModule, ModuleParams};
use darcbench_protocol::metrics::Direction;
use darcbench_protocol::Profile;

/// The target triple, captured at build time by `build.rs`.
const TARGET: &str = env!("DARCBENCH_TARGET");

const USAGE: &str = "\
darcbench-characterise - raw cross-OS characterisation of the measurement engine

USAGE:
    darcbench-characterise [OPTIONS]

OPTIONS:
    --profile <quick|standard|deep>  Repetition counts and target duration [default: deep]
    --passes <N>                     Whole-suite repeats, 1..=20 [default: 3]
    --label <TEXT>                   Free text recorded in every row, e.g. the OS
    -h, --help                       Print this

OUTPUT:
    CSV on stdout, one row per repetition. NDJSON provenance on stderr.
    Never a score: raw measurements in physical units only.
";

struct Options {
    profile: Profile,
    passes: u32,
    label: String,
}

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("darcbench-characterise: {message}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let Some(options) = parse_args(std::env::args().skip(1))? else {
        print!("{USAGE}");
        return Ok(());
    };

    let params = ModuleParams::for_profile(options.profile);
    let cpu_mixed = CpuMixed::new();
    let memory_bandwidth = MemoryBandwidth::new();
    let modules: [&dyn BenchmarkModule; 2] = [&cpu_mixed, &memory_bandwidth];
    let reporter = NullReporter::default();

    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    writeln!(
        out,
        "label,target,os,arch,profile,pass,module,module_version,metric,unit,direction,rep,warmup,\
         value,duration_ms"
    )
    .map_err(|error| format!("writing the header: {error}"))?;

    // Passes are whole-suite repeats, not extra repetitions. A cross-OS delta
    // means nothing without the within-OS spread to compare it against, and
    // raising `measured_reps` would not give that: it would widen one sample of
    // one machine state. Re-running the whole suite re-calibrates, re-warms and
    // re-schedules, which is the variation an operator actually sees.
    for pass in 1..=options.passes {
        for module in modules {
            let manifest = module.manifest();
            let id = manifest.id.as_str();
            let output = module
                .run(&params, &reporter)
                .map_err(|error| format!("`{id}` failed on pass {pass}: {error}"))?;

            for metric in &output.metrics {
                let direction = match metric.direction {
                    Direction::HigherIsBetter => "higher_is_better",
                    Direction::LowerIsBetter => "lower_is_better",
                };
                for sample in &metric.samples {
                    writeln!(
                        out,
                        "{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}",
                        options.label,
                        TARGET,
                        std::env::consts::OS,
                        std::env::consts::ARCH,
                        options.profile.as_str(),
                        pass,
                        id,
                        manifest.version,
                        metric.key,
                        metric.unit,
                        direction,
                        sample.rep,
                        sample.warmup,
                        sample.value,
                        sample.duration_ms,
                    )
                    .map_err(|error| format!("writing a sample: {error}"))?;
                }
            }

            // Provenance, not results: the calibrated work sizes, the thread
            // count and the ISA paths this run actually took. Two CSVs with the
            // same numbers and different calibrations are not the same
            // measurement, and this is what makes that visible.
            let provenance = serde_json::json!({
                "label": options.label,
                "target": TARGET,
                "os": std::env::consts::OS,
                "arch": std::env::consts::ARCH,
                "profile": options.profile.as_str(),
                "pass": pass,
                "module": id,
                "module_version": manifest.version,
                "warnings": output
                    .warnings
                    .iter()
                    .map(|warning| warning.message.clone())
                    .collect::<Vec<_>>(),
                "context": output.context,
            });
            eprintln!("{provenance}");
        }
    }

    out.flush()
        .map_err(|error| format!("flushing stdout: {error}"))
}

/// Parses the argument list. Returns `Ok(None)` when help was requested.
fn parse_args(args: impl Iterator<Item = String>) -> Result<Option<Options>, String> {
    let mut profile = Profile::Deep;
    let mut passes = 3u32;
    let mut label = String::from("unlabelled");
    let mut args = args;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(None),
            "--profile" => {
                let value = args.next().ok_or("`--profile` needs a value")?;
                profile = match value.as_str() {
                    "quick" => Profile::Quick,
                    "standard" => Profile::Standard,
                    "deep" => Profile::Deep,
                    // Only the three that mean something here. `endurance` is a
                    // duration-driven curve and `web_only` selects modules this
                    // binary does not have, so accepting either would produce a
                    // run that silently did not do what was asked.
                    other => {
                        return Err(format!(
                            "unknown profile `{other}`; expected quick, standard or deep"
                        ))
                    }
                };
            }
            "--passes" => {
                let value = args.next().ok_or("`--passes` needs a value")?;
                passes = value
                    .parse::<u32>()
                    .map_err(|_| format!("`--passes` wants a number, got `{value}`"))?;
                if !(1..=20).contains(&passes) {
                    return Err(format!("`--passes` must be 1 to 20, got {passes}"));
                }
            }
            "--label" => {
                let value = args.next().ok_or("`--label` needs a value")?;
                // Rejected rather than quoted: this binary writes CSV by hand,
                // and a label that needs escaping is a label that will be
                // mis-parsed by whatever reads it. Refusing is honest; emitting
                // a row nothing can parse is not.
                if value.contains([',', '"', '\n', '\r']) {
                    return Err("`--label` may not contain a comma, quote or newline".to_string());
                }
                label = value;
            }
            other => return Err(format!("unknown argument `{other}`; try --help")),
        }
    }

    Ok(Some(Options {
        profile,
        passes,
        label,
    }))
}

#[cfg(test)]
// In tests, `unwrap`/`expect` panicking *is* the failure signal.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic_in_result_fn)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Option<Options>, String> {
        parse_args(args.iter().map(|arg| (*arg).to_string()))
    }

    #[test]
    fn the_defaults_are_the_thorough_ones() {
        let options = parse(&[]).unwrap().unwrap();
        assert_eq!(options.profile.as_str(), "deep");
        assert_eq!(options.passes, 3, "one pass cannot show within-OS spread");
        assert_eq!(options.label, "unlabelled");
    }

    #[test]
    fn a_profile_this_binary_cannot_honour_is_refused() {
        // `endurance` is a duration-driven curve and `web_only` selects modules
        // that are not here. Accepting either would run something other than
        // what was asked, quietly.
        for profile in ["endurance", "web_only", "read_only", "custom", "nonsense"] {
            assert!(
                parse(&["--profile", profile]).is_err(),
                "`{profile}` must be refused rather than silently substituted"
            );
        }
        for profile in ["quick", "standard", "deep"] {
            assert!(parse(&["--profile", profile]).is_ok(), "{profile}");
        }
    }

    #[test]
    fn a_label_that_would_break_the_csv_is_refused_not_quoted() {
        for bad in ["a,b", "a\"b", "a\nb"] {
            assert!(
                parse(&["--label", bad]).is_err(),
                "{bad:?} would produce a row nothing can parse"
            );
        }
        assert_eq!(
            parse(&["--label", "windows-11-msvc"])
                .unwrap()
                .unwrap()
                .label,
            "windows-11-msvc"
        );
    }

    #[test]
    fn the_pass_count_is_bounded_at_both_ends() {
        assert!(
            parse(&["--passes", "0"]).is_err(),
            "zero passes measures nothing"
        );
        assert!(
            parse(&["--passes", "21"]).is_err(),
            "an unbounded run is not a characterisation"
        );
        assert!(parse(&["--passes", "x"]).is_err());
        assert_eq!(parse(&["--passes", "20"]).unwrap().unwrap().passes, 20);
    }

    #[test]
    fn a_flag_missing_its_value_fails_rather_than_defaulting() {
        for flag in ["--profile", "--passes", "--label"] {
            assert!(
                parse(&[flag]).is_err(),
                "{flag} consumed nothing and continued"
            );
        }
    }

    #[test]
    fn help_is_not_a_run() {
        assert!(parse(&["--help"]).unwrap().is_none());
        assert!(parse(&["-h"]).unwrap().is_none());
    }

    #[test]
    fn an_unknown_argument_is_refused() {
        assert!(
            parse(&["--score"]).is_err(),
            "this binary has no scoring surface"
        );
    }

    /// The target triple must be real, because comparing two CSVs that both say
    /// `unknown` compares nothing.
    #[test]
    fn the_build_target_was_captured() {
        assert_ne!(TARGET, "unknown", "build.rs did not see cargo's TARGET");
        assert!(TARGET.contains('-'), "not a target triple: {TARGET}");
    }
}
