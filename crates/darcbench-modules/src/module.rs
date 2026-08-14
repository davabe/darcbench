//! The contract every benchmark module implements.

use darcbench_protocol::metrics::{Metric, Warning};
use darcbench_protocol::{ModuleId, ModuleRef, Profile};
use serde::{Deserialize, Serialize};

/// How disruptive a module is to a machine that is doing real work.
///
/// The agent refuses to run anything above the operator's declared tolerance,
/// and the preflight screen shows the highest class in the selected profile.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SafetyClass {
    /// Read-only. No writes, no service interaction, bounded CPU.
    Observational,
    /// Saturates CPU or memory but writes nothing and touches no service.
    ComputeIntensive,
    /// Writes to a temporary path under a DARCBench-owned directory.
    WritesTemporaryFiles,
    /// Generates outbound network traffic to third-party endpoints.
    UsesNetwork,
    /// Creates and destroys its own service instances (databases, containers).
    ProvisionsServices,
}

/// Everything a module declares about itself.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModuleManifest {
    pub id: ModuleId,
    /// Semantic version of the *workload*. Bumping the major version makes
    /// results incomparable with prior versions, by design.
    pub version: String,
    pub title: String,
    pub purpose: String,
    pub safety_class: SafetyClass,
    /// External binaries or services required. Empty means self-contained.
    pub dependencies: Vec<String>,
    /// Upper bound on bytes written to disk, for the disk-space guard.
    pub max_bytes_written: u64,
    /// Upper bound on bytes transferred over the network.
    pub max_network_bytes: u64,
    /// What the module removes when it finishes or is cancelled.
    pub cleanup: String,
    /// Conditions under which this module's results must not be scored.
    pub validation: Vec<String>,
    /// Known measurement limitations, surfaced in reports.
    pub limitations: Vec<String>,
    /// Fields that must match for two results to be comparable.
    pub comparability: Vec<String>,
    /// Coefficient of variation above which the module flags itself unstable.
    pub stability_cv_bound: f64,
}

impl ModuleManifest {
    pub fn module_ref(&self) -> ModuleRef {
        ModuleRef {
            id: self.id.clone(),
            version: self.version.clone(),
        }
    }
}

/// Machine facts a module needs in order to size its working set.
///
/// Supplied by the agent from the inventory rather than read here, so this
/// crate keeps its promise of touching nothing outside the workload itself.
/// `docs/BENCHMARK-METHODOLOGY.md` requires that memory working sets exceed
/// last-level cache by a documented multiple and that the cache topology
/// captured in inventory is what sizes them; this is how it gets there.
///
/// Every field is optional because containers and some hypervisors export no
/// topology at all. A module that finds `None` must fall back to a documented
/// default **and disclose that it did** - a silently guessed working set is how
/// a benchmark ends up measuring cache and calling it memory.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MachineFacts {
    /// Largest cache level the host reports, in bytes.
    pub last_level_cache_bytes: Option<u64>,
    /// L2 size in bytes, used to size the cache-resident working set.
    pub l2_cache_bytes: Option<u64>,
    /// `MemAvailable`: what a fresh allocation can realistically get. Used as
    /// a ceiling so a benchmark never pushes a live host into swap.
    pub available_bytes: Option<u64>,
    /// NUMA nodes visible to this process.
    pub numa_nodes: Option<usize>,
    /// Free space on the filesystem backing [`ModuleParams::scratch_dir`].
    ///
    /// `None` means it could not be determined, which a module must treat as
    /// unsafe rather than as unlimited - the same rule preflight applies.
    pub free_scratch_bytes: Option<u64>,
}

/// Per-run parameters derived from the selected profile.
///
/// Deliberately not `Copy`: it carries an owned scratch path, and a path that
/// silently copies is a path that is easy to lose track of.
#[derive(Clone, Debug, PartialEq)]
pub struct ModuleParams {
    /// Untimed repetitions used to warm caches, branch predictors and clocks.
    pub warmup_reps: u32,
    /// Timed repetitions that feed the statistics.
    pub measured_reps: u32,
    /// Target wall-clock duration of a single repetition, in milliseconds.
    ///
    /// Work sizes are calibrated to hit this, so a fast machine does *more*
    /// work rather than finishing sooner. Without this, a repetition on a
    /// modern CPU would be short enough for timer granularity and scheduler
    /// noise to dominate.
    pub target_rep_ms: u64,
    /// Threads for the multi-threaded shapes. Zero means "all logical CPUs".
    pub threads: usize,
    /// What the agent discovered about the machine. Defaults to "nothing
    /// known", which every module must handle.
    pub facts: MachineFacts,
    /// Directory a module may create its working files in.
    ///
    /// Already validated by the agent's `StatePath`, which is the only type in
    /// the system permitted to compose a filesystem path from components. A
    /// module places **fixed, self-chosen names directly inside this
    /// directory** and never joins anything caller-supplied onto it, so there
    /// is no traversal surface here at all.
    ///
    /// `None` means no scratch space was provided, and a module that needs one
    /// must fail rather than pick a directory of its own.
    pub scratch_dir: Option<std::path::PathBuf>,
}

impl ModuleParams {
    pub fn for_profile(profile: Profile) -> Self {
        let (warmup_reps, measured_reps, target_rep_ms) = match profile {
            Profile::Standard | Profile::WebOnly => (2, 7, 300),
            Profile::Deep => (3, 11, 500),
            // Endurance repetitions are *per cycle*, and the profile runs many
            // cycles across its duration target. So a cycle is deliberately
            // short - the same shape as `quick` - rather than long.
            //
            // The instinct is the opposite: endurance is the thorough profile,
            // so it should measure hardest. But endurance's output is a curve,
            // not a point, and the resolution of that curve is the cycle count.
            // Thirty-one repetitions in one pass yields a single very precise
            // number and no curve at all - it would say what the machine
            // averaged over an hour while being unable to say that it halved at
            // minute forty, which is the finding. Five repetitions per cycle
            // gives ten to twenty points across the hour, and the last cycle
            // still rests on exactly the sample count the `quick` profile
            // publishes as a headline.
            Profile::Endurance => (1, 5, 200),
            Profile::Quick | Profile::ReadOnly | Profile::Custom => (1, 5, 200),
        };
        Self {
            warmup_reps,
            measured_reps,
            target_rep_ms,
            threads: 0,
            facts: MachineFacts::default(),
            scratch_dir: None,
        }
    }

    /// Attaches the machine facts discovered by the agent's inventory pass.
    pub fn with_facts(mut self, facts: MachineFacts) -> Self {
        self.facts = facts;
        self
    }

    /// Attaches a scratch directory the agent has already validated.
    pub fn with_scratch_dir(mut self, dir: std::path::PathBuf) -> Self {
        self.scratch_dir = Some(dir);
        self
    }

    pub fn effective_threads(&self) -> usize {
        if self.threads > 0 {
            self.threads
        } else {
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1)
        }
    }
}

/// The channel a running module uses to report progress.
///
/// Implemented by the agent's run loop. Modules must call
/// [`ModuleReporter::is_cancelled`] between repetitions: cancellation is
/// cooperative, because killing a thread mid-workload would leave temporary
/// resources behind.
pub trait ModuleReporter: Send + Sync {
    /// Reports one completed repetition.
    #[allow(clippy::too_many_arguments)]
    fn sample(
        &self,
        metric_key: &str,
        unit: &str,
        rep: u32,
        warmup: bool,
        value: f64,
        duration_ms: f64,
        module_progress: f64,
    );

    fn warn(&self, warning: Warning);

    /// True once the operator has asked for the run to stop.
    fn is_cancelled(&self) -> bool;
}

/// A reporter that discards everything. Used by tests and `--dry-run`.
#[derive(Debug, Default)]
pub struct NullReporter {
    cancelled: std::sync::atomic::AtomicBool,
}

impl NullReporter {
    pub fn cancel(&self) {
        self.cancelled
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

impl ModuleReporter for NullReporter {
    fn sample(&self, _: &str, _: &str, _: u32, _: bool, _: f64, _: f64, _: f64) {}
    fn warn(&self, _: Warning) {}
    fn is_cancelled(&self) -> bool {
        self.cancelled.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// What a module produces on success.
#[derive(Clone, Debug, PartialEq)]
pub struct ModuleOutput {
    pub metrics: Vec<Metric>,
    pub warnings: Vec<Warning>,
    /// Module-specific context recorded for comparability: thread counts,
    /// calibrated work sizes, compile target.
    pub context: serde_json::Map<String, serde_json::Value>,
}

impl ModuleOutput {
    pub fn metric(&self, key: &str) -> Option<&Metric> {
        self.metrics.iter().find(|m| m.key == key)
    }

    /// Headline value of a metric, by key.
    pub fn metric_value(&self, key: &str) -> Option<f64> {
        self.metric(key).map(|m| m.value)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ModuleError {
    #[error("module cancelled by operator")]
    Cancelled,
    #[error("precondition not met: {0}")]
    Precondition(String),
    #[error("workload failed: {0}")]
    Workload(String),
    #[error("measurement produced no usable samples for `{0}`")]
    NoSamples(String),
}

/// A benchmark module.
pub trait BenchmarkModule: Send + Sync {
    fn manifest(&self) -> &ModuleManifest;

    /// Estimated wall-clock duration for the given parameters, in seconds.
    /// Used by preflight; does not have to be exact, but must not be optimistic.
    fn estimated_duration_s(&self, params: &ModuleParams) -> u64;

    /// Peak heap this module expects to hold at once, in bytes.
    ///
    /// Unlike `max_bytes_written`, this cannot be a manifest constant: how much
    /// memory a module needs depends on the machine it is sizing itself
    /// against. Preflight sums it across the selected modules and shows the
    /// operator the total before anything runs.
    ///
    /// Defaults to zero, which is the honest answer for a module whose
    /// allocations are negligible next to the machine it is measuring. A module
    /// that allocates in gigabytes must override it - a run that silently takes
    /// a quarter of a production host's memory is exactly the surprise
    /// preflight exists to prevent.
    fn estimated_peak_memory_bytes(&self, _params: &ModuleParams) -> u64 {
        0
    }

    /// Total bytes this module expects to write over the whole run, in bytes.
    ///
    /// Distinct from `ModuleManifest::max_bytes_written`, which is how much
    /// *space* must be free: a storage module that rewrites a 2 GiB file forty
    /// times needs 2 GiB of space and costs 80 GiB of flash endurance. Space is
    /// what the disk guard checks; volume is what an operator deciding whether
    /// to run this on a production SSD needs to see.
    ///
    /// Defaults to zero, which is correct for any module that writes nothing.
    fn estimated_write_volume_bytes(&self, _params: &ModuleParams) -> u64 {
        0
    }

    /// Runs the workload. Must be cancellation-responsive and must clean up
    /// everything it created before returning, including on the error paths.
    fn run(
        &self,
        params: &ModuleParams,
        reporter: &dyn ModuleReporter,
    ) -> Result<ModuleOutput, ModuleError>;
}

#[cfg(test)]
// In tests, `unwrap`/`expect` panicking *is* the failure signal.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic_in_result_fn)]
mod tests {
    use super::*;

    #[test]
    fn profile_parameters_scale_with_depth() {
        let quick = ModuleParams::for_profile(Profile::Quick);
        let deep = ModuleParams::for_profile(Profile::Deep);
        assert!(deep.measured_reps > quick.measured_reps);
        assert!(deep.target_rep_ms >= quick.target_rep_ms);
        assert!(deep.warmup_reps >= quick.warmup_reps);
    }

    #[test]
    fn every_profile_measures_enough_reps_for_a_median() {
        for profile in [
            Profile::Quick,
            Profile::Standard,
            Profile::Deep,
            Profile::Endurance,
            Profile::ReadOnly,
            Profile::WebOnly,
            Profile::Custom,
        ] {
            let p = ModuleParams::for_profile(profile);
            assert!(
                p.measured_reps >= 5,
                "{profile} measures only {} reps",
                p.measured_reps
            );
            assert!(p.warmup_reps >= 1, "{profile} has no warm-up");
        }
    }

    #[test]
    fn effective_threads_resolves_zero() {
        let p = ModuleParams {
            warmup_reps: 1,
            measured_reps: 5,
            target_rep_ms: 100,
            threads: 0,
            facts: MachineFacts::default(),
            scratch_dir: None,
        };
        assert!(p.effective_threads() >= 1);
        let fixed = ModuleParams { threads: 3, ..p };
        assert_eq!(fixed.effective_threads(), 3);
    }

    #[test]
    fn safety_classes_are_ordered_by_invasiveness() {
        assert!(SafetyClass::Observational < SafetyClass::ComputeIntensive);
        assert!(SafetyClass::WritesTemporaryFiles < SafetyClass::ProvisionsServices);
    }
}
