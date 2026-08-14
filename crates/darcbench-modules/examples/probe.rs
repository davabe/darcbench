//! Drives one module end to end, printing every sample as it lands.
//!
//! `cargo run --release -p darcbench-modules --example probe -- database.oltp`
//!
//! Written to exercise `database.oltp` and `database.cache` before either was
//! registered, because there is no CLI entry point for a single unregistered
//! module and `Sandbox::launch` had never touched a daemon. It found five
//! defects doing that, which is the argument for keeping it: `wordpress.*` will
//! be in the same position, and the alternative is writing this again.
//!
//! It is not a test and nothing gates on it. Its value is that a summary hides
//! the difference between a module that produced the right numbers and one that
//! produced them in the wrong order, from the wrong phase, or not at all - and
//! every one of those happened here.

use std::sync::atomic::{AtomicBool, Ordering};

use darcbench_modules::{BenchmarkModule, ModuleParams};
use darcbench_protocol::Profile;

/// Prints every sample as it lands, which is the point of driving a module by
/// hand: a module that produces the right numbers slowly and one that produces
/// them in the wrong order look identical in the summary.
struct Loud {
    cancelled: AtomicBool,
}

impl darcbench_modules::ModuleReporter for Loud {
    fn sample(
        &self,
        metric_key: &str,
        unit: &str,
        rep: u32,
        warmup: bool,
        value: f64,
        duration_ms: f64,
        progress: f64,
    ) {
        let tag = if warmup { "warmup" } else { "sample" };
        println!(
            "  {tag} {metric_key:<40} rep={rep:<3} {value:>14.3} {unit:<12} \
             ({duration_ms:.0} ms, {:.0}%)",
            progress * 100.0
        );
    }

    fn warn(&self, warning: darcbench_protocol::metrics::Warning) {
        println!("  WARN [{:?}] {}", warning.code, warning.message);
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}

fn main() {
    let which = std::env::args().nth(1).unwrap_or_default();
    let module: Box<dyn BenchmarkModule> = match which.as_str() {
        "database.oltp" => Box::new(darcbench_modules::database_oltp::DatabaseOltp::new()),
        "database.cache" => Box::new(darcbench_modules::database_cache::DatabaseCache::new()),
        other => {
            eprintln!("unknown module `{other}`; try database.oltp or database.cache");
            std::process::exit(2);
        }
    };

    let scratch = std::env::temp_dir().join("darcbench-probe");
    // `expect_used` is denied outside tests and an example is not a test, which
    // is the right call: this is a program somebody runs, and a panic message
    // is a worse thing to hand them than a sentence.
    if let Err(error) = std::fs::create_dir_all(&scratch) {
        eprintln!("could not create {}: {error}", scratch.display());
        std::process::exit(1);
    }
    let params = ModuleParams::for_profile(Profile::Deep).with_scratch_dir(scratch);

    let manifest = module.manifest();
    println!(
        "{} v{} — {:?}, est. {} s",
        manifest.id,
        manifest.version,
        manifest.safety_class,
        module.estimated_duration_s(&params)
    );

    let reporter = Loud {
        cancelled: AtomicBool::new(false),
    };
    let started = std::time::Instant::now();
    match module.run(&params, &reporter) {
        Ok(output) => {
            println!("\nOK in {:.1}s", started.elapsed().as_secs_f64());
            println!("\n{} metrics:", output.metrics.len());
            for metric in &output.metrics {
                println!(
                    "  {:<44} {:>14.3} {:<12} n={} cv={:?}",
                    metric.key,
                    metric.value,
                    metric.unit,
                    metric.samples.len(),
                    metric.summary.cv
                );
            }
            println!("\n{} warnings:", output.warnings.len());
            for warning in &output.warnings {
                println!("  [{:?}] {}", warning.code, warning.message);
            }
            println!("\ncontext:");
            for (key, value) in &output.context {
                println!("  {key} = {value}");
            }
        }
        Err(error) => {
            println!(
                "\nFAILED in {:.1}s: {error}",
                started.elapsed().as_secs_f64()
            );
            std::process::exit(1);
        }
    }
}
