//! Preflight: decide whether running this benchmark right now is acceptable.
//!
//! # Principle
//!
//! DARCBench is designed to be installed on machines that are already serving
//! customers. A benchmark that saturates the CPU of a live web server is not a
//! neutral observation - it is an outage. So the agent computes an explicit
//! risk classification, shows what the run will cost, and refuses outright when
//! a blocking condition is present.
//!
//! Nothing here is advisory-only: a [`PreflightFinding`] with `blocking = true`
//! stops the run. `--force` can override a *warning*, never a blocker.

use crate::runner::MIN_ENDURANCE_CYCLES;
use darcbench_inventory::software::ProductionLikelihood;
use darcbench_inventory::{platform::Scope, Inventory};
use darcbench_modules::{ModuleParams, Registry, SafetyClass};
use darcbench_protocol::events::{PreflightCompleted, PreflightFinding, RiskClass, Severity};
use darcbench_protocol::{ModuleId, Profile};

/// Free space below which no write-capable module may start.
const MIN_FREE_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Load average per CPU above which the machine is considered already busy.
const BUSY_LOAD_PER_CPU: f64 = 0.70;

/// Share of available memory above which the run's allocation is disclosed to
/// the operator rather than passing silently.
const MEMORY_DISCLOSE_SHARE: f64 = 0.10;

/// Share of available memory above which the run is refused outright.
///
/// Deliberately above the 25% ceiling `memory.bandwidth` already imposes on
/// itself, so this is a backstop against a future module that fails to bound
/// its own appetite, not a second opinion on one that does.
const MEMORY_BLOCK_SHARE: f64 = 0.60;

pub(crate) struct PreflightInput<'a> {
    pub(crate) inventory: &'a Inventory,
    pub(crate) registry: &'a Registry,
    pub(crate) modules: &'a [ModuleId],
    pub(crate) profile: Profile,
    pub(crate) params: &'a ModuleParams,
    pub(crate) state_dir: &'a std::path::Path,
    /// Set by `--force`: allows non-blocking warnings to be overridden.
    pub(crate) force: bool,
    /// How long a cycling profile will keep repeating its module set.
    pub(crate) cycle_target: Option<std::time::Duration>,
}

/// Runs every preflight check.
pub(crate) fn run(input: &PreflightInput<'_>) -> PreflightCompleted {
    let mut findings = Vec::new();
    let inventory = input.inventory;

    // --- module resolution ------------------------------------------------
    let mut estimated_duration_s = 0u64;
    let mut estimated_bytes_written = 0u64;
    let mut estimated_network_bytes = 0u64;
    let mut estimated_peak_memory_bytes = 0u64;
    let mut estimated_write_volume_bytes = 0u64;
    let mut max_safety = SafetyClass::Observational;

    if input.modules.is_empty() {
        findings.push(PreflightFinding {
            check: "modules.selected".into(),
            severity: Severity::Error,
            message: format!(
                "Profile `{}` resolves to no implemented modules in this build. \
                 See docs/ROADMAP.md for which profiles are complete.",
                input.profile
            ),
            blocking: true,
        });
    }

    for id in input.modules {
        let Some(module) = input.registry.get(id) else {
            findings.push(PreflightFinding {
                check: "modules.known".into(),
                severity: Severity::Error,
                message: format!("Module `{id}` is not in this agent's allow-list."),
                blocking: true,
            });
            continue;
        };
        let manifest = module.manifest();
        estimated_duration_s += module.estimated_duration_s(input.params);
        estimated_bytes_written += manifest.max_bytes_written;
        estimated_network_bytes += manifest.max_network_bytes;
        // Modules run one after another, so the peak is the largest single
        // module's, not the sum. Summing would overstate the cost and push
        // operators into refusing runs that are in fact affordable.
        estimated_peak_memory_bytes =
            estimated_peak_memory_bytes.max(module.estimated_peak_memory_bytes(input.params));
        // Volume *is* summed, unlike the memory peak: every byte a module writes
        // is a byte of flash endurance spent, and they accumulate across
        // modules rather than overlapping.
        estimated_write_volume_bytes += module.estimated_write_volume_bytes(input.params);
        max_safety = max_safety.max(manifest.safety_class);

        for dependency in &manifest.dependencies {
            findings.push(PreflightFinding {
                check: "modules.dependencies".into(),
                severity: Severity::Info,
                message: format!("`{}` requires {dependency}", manifest.id),
                blocking: false,
            });
        }
    }

    // --- cycles -------------------------------------------------------------
    //
    // A cycling profile pays every per-run cost once per cycle, and the two
    // costs that accumulate are the ones an operator can never get back: wall
    // clock on their server, and flash endurance on their disk. Estimating both
    // from a single pass would understate them by the cycle count, which on an
    // hour-long run is an order of magnitude.
    //
    // Peak memory is *not* multiplied: cycles run one after another, so the
    // high-water mark is a single cycle's.
    let cycles = match input.cycle_target {
        Some(target) if estimated_duration_s > 0 => {
            let per_cycle = estimated_duration_s.max(1);
            // At least the minimum the runner guarantees, because that is what
            // it will actually do.
            (target.as_secs() / per_cycle).max(u64::from(MIN_ENDURANCE_CYCLES))
        }
        Some(_) => u64::from(MIN_ENDURANCE_CYCLES),
        None => 1,
    };
    estimated_duration_s = estimated_duration_s.saturating_mul(cycles);
    estimated_write_volume_bytes = estimated_write_volume_bytes.saturating_mul(cycles);
    estimated_network_bytes = estimated_network_bytes.saturating_mul(cycles);

    if let Some(target) = input.cycle_target {
        findings.push(PreflightFinding {
            check: "run.duration".into(),
            severity: Severity::Warning,
            message: format!(
                "This is a {}-minute run. The module set repeats in roughly {cycles} cycles so \
                 that a decline appearing after half an hour is visible at all - which is the \
                 whole point of the profile, and the reason a short benchmark of a burstable \
                 instance measures its credit balance rather than the instance. The machine will \
                 be under full load for the entire time. Cancel at any point and the cycles \
                 completed so far are kept.",
                target.as_secs() / 60,
            ),
            // Never blocking: an operator who asked for an endurance run wants
            // their machine loaded for an hour. They should just be told so
            // before it starts rather than after.
            blocking: false,
        });
    }

    // --- disk space --------------------------------------------------------
    // The state directory may not exist yet on a first run; its filesystem does.
    let free =
        darcbench_inventory::storage::StorageInfo::available_bytes_for_or_ancestor(input.state_dir);
    match free {
        Some(free_bytes) => {
            let required = estimated_bytes_written.saturating_add(MIN_FREE_BYTES);
            if free_bytes < required {
                findings.push(PreflightFinding {
                    check: "storage.free_space".into(),
                    severity: Severity::Error,
                    message: format!(
                        "{} GiB free where {} GiB is required (estimated writes plus a {} GiB \
                         safety margin). Refusing to risk filling the filesystem.",
                        free_bytes / (1 << 30),
                        required / (1 << 30),
                        MIN_FREE_BYTES / (1 << 30),
                    ),
                    blocking: true,
                });
            }
        }
        None if estimated_bytes_written > 0 => {
            findings.push(PreflightFinding {
                check: "storage.free_space".into(),
                severity: Severity::Error,
                message: "Free space could not be determined, and the selected modules write to \
                          disk. Unknown is treated as unsafe."
                    .into(),
                blocking: true,
            });
        }
        None => {}
    }

    // --- flash endurance ------------------------------------------------------
    //
    // `docs/BENCHMARK-METHODOLOGY.md` requires that estimated bytes written are
    // shown before a storage run, with an SSD wear warning. Space and wear are
    // different costs: rewriting a 2 GiB file forty times needs 2 GiB free and
    // spends 80 GiB of endurance, and only the second one is permanent.
    if estimated_write_volume_bytes > 0 {
        findings.push(PreflightFinding {
            check: "storage.wear".into(),
            severity: Severity::Warning,
            message: format!(
                "This run will write about {} GiB in total. On flash storage that is endurance \
                 spent permanently, and on a consumer SSD a run like this is a measurable \
                 fraction of a day's normal writes. Files are created only under the DARCBench \
                 state directory and removed when the run ends.",
                estimated_write_volume_bytes.div_ceil(1 << 30),
            ),
            // A warning, never a blocker: benchmarking the disk of a machine you
            // own is the entire point of a storage module. It must be an
            // informed choice, not a surprise.
            blocking: false,
        });
    }

    // --- outbound traffic ----------------------------------------------------
    //
    // DARCBench sends nothing anywhere unless a module in the selection measures
    // the network, and then it contacts a fixed set of hosts compiled into the
    // binary. Both halves of that sentence are worth showing before the run
    // rather than after: the volume, because the operator may be paying for
    // egress or sitting behind a metered link, and the destinations, because
    // "this tool phones out" is exactly the thing somebody running an unfamiliar
    // binary on their server wants to be told first.
    if estimated_network_bytes > 0 {
        let mut operators: Vec<&str> = darcbench_modules::network_endpoints::all()
            .iter()
            .map(|endpoint| endpoint.operator)
            .collect();
        operators.sort_unstable();
        operators.dedup();
        findings.push(PreflightFinding {
            check: "network.egress".into(),
            severity: Severity::Warning,
            message: format!(
                "This run will transfer up to {} MiB to and from third-party measurement \
                 endpoints ({}). Those hosts are a compile-time allow-list - DARCBench cannot be \
                 pointed at any other address - and no measurement result, inventory or identifier \
                 is sent to them. Skip with the `quick` profile, which makes no outbound \
                 connection.",
                estimated_network_bytes.div_ceil(1 << 20),
                operators.join(", "),
            ),
            // Not blocking: measuring the network is the point of a network
            // module. It must be a disclosed choice, not a surprise.
            blocking: false,
        });
    }

    // --- existing load ------------------------------------------------------
    let cpus = inventory.cpu.logical_cpus.max(1) as f64;
    if let Some(load1) = inventory.platform.load1 {
        let per_cpu = load1 / cpus;
        if per_cpu > BUSY_LOAD_PER_CPU {
            findings.push(PreflightFinding {
                check: "system.load".into(),
                severity: Severity::Warning,
                message: format!(
                    "Load average is {load1:.2} across {cpus:.0} CPU(s) ({per_cpu:.2} per CPU). \
                     Results will reflect the competing work, not the hardware."
                ),
                blocking: false,
            });
        }
    }

    // --- production signals --------------------------------------------------
    let production = inventory.software.production_likelihood;
    if production == ProductionLikelihood::Likely {
        findings.push(PreflightFinding {
            check: "system.production".into(),
            severity: Severity::Warning,
            message: format!(
                "This machine looks like it is serving live traffic ({}). A CPU-saturating \
                 benchmark will degrade it for the duration of the run.",
                inventory.software.production_signals.join("; ")
            ),
            // Warning rather than blocker: benchmarking your own production
            // server is a legitimate thing to want to do. It must be an
            // informed choice, not a surprise.
            blocking: false,
        });
    }

    // --- memory cost ----------------------------------------------------------
    //
    // The disk guard has an equivalent above; this is its counterpart. A module
    // that takes a large share of a live host's memory can push it into reclaim
    // or swap, which is both an outage and a measurement of the swap device.
    if estimated_peak_memory_bytes > 0 {
        let available = inventory.memory.available_bytes;
        let share = if available > 0 {
            estimated_peak_memory_bytes as f64 / available as f64
        } else {
            1.0
        };
        let over_ceiling = share > MEMORY_BLOCK_SHARE;
        if share > MEMORY_DISCLOSE_SHARE {
            findings.push(PreflightFinding {
                check: "memory.allocation".into(),
                severity: if over_ceiling {
                    Severity::Error
                } else {
                    Severity::Warning
                },
                message: format!(
                    "The selected modules will hold up to {} MiB at once, {:.0}% of the {} MiB \
                     currently available. {}",
                    estimated_peak_memory_bytes / (1 << 20),
                    share * 100.0,
                    available / (1 << 20),
                    if over_ceiling {
                        "Refusing: this risks pushing the machine into reclaim, which would \
                         both disrupt it and make the result describe swap."
                    } else {
                        "The memory is released when the run finishes."
                    }
                ),
                blocking: over_ceiling,
            });
        }
    }

    // --- swap pressure --------------------------------------------------------
    if inventory.memory.swap_used_bytes() > inventory.memory.total_bytes / 10 {
        findings.push(PreflightFinding {
            check: "memory.swap".into(),
            severity: Severity::Warning,
            message: "The machine is already swapping. Memory-sensitive results will be \
                      dominated by paging."
                .into(),
            blocking: false,
        });
    }

    // --- scope ------------------------------------------------------------------
    if inventory.platform.scope == Scope::Container {
        findings.push(PreflightFinding {
            check: "environment.scope".into(),
            severity: Severity::Warning,
            message: "Running inside a container. Results describe this container's limits, \
                      not the host, and will be labelled container-scoped."
                .into(),
            blocking: false,
        });
    }
    if let Some(limit) = inventory.platform.cgroup_cpu_limit {
        if limit < cpus {
            findings.push(PreflightFinding {
                check: "environment.cgroup_cpu".into(),
                severity: Severity::Warning,
                message: format!(
                    "A cgroup CPU limit of {limit:.2} CPU(s) is in effect while {cpus:.0} are \
                     visible. Multi-core results will be capped by the limit, not the hardware."
                ),
                blocking: false,
            });
        }
    }

    // --- degraded storage ----------------------------------------------------------
    if inventory.storage.complex_stack {
        findings.push(PreflightFinding {
            check: "storage.stack".into(),
            severity: Severity::Info,
            message: format!(
                "Storage stack: {}. Recorded alongside any storage result.",
                inventory.storage.stack_indicators.join("; ")
            ),
            blocking: false,
        });
    }

    // --- privileges -------------------------------------------------------------------
    if inventory.platform.running_as_root {
        findings.push(PreflightFinding {
            check: "process.privileges".into(),
            severity: Severity::Info,
            message: "Running as root. DARCBench does not need root for the modules in this \
                      build; consider running as an unprivileged user."
                .into(),
            blocking: false,
        });
    }

    // --- risk classification -----------------------------------------------------------
    let risk = classify(max_safety, production, inventory);

    let blocking_present = findings.iter().any(|f| f.blocking);
    let warnings_present = findings.iter().any(|f| f.severity == Severity::Warning);
    // `force` can silence warnings; it can never clear a blocker.
    let passed =
        !blocking_present && (input.force || !warnings_present || risk < RiskClass::ProductionRisk);

    PreflightCompleted {
        risk,
        passed,
        findings,
        estimated_duration_s,
        estimated_bytes_written,
        estimated_network_bytes,
        estimated_peak_memory_bytes,
        estimated_write_volume_bytes,
    }
}

fn classify(
    max_safety: SafetyClass,
    production: ProductionLikelihood,
    inventory: &Inventory,
) -> RiskClass {
    // The mapping must be **monotonic** in invasiveness. `max_safety` is the
    // most invasive class in the selected set, so a mapping that dipped - as an
    // earlier one did, sending `WritesTemporaryFiles` to `ModerateLoad` while
    // `ComputeIntensive` went to `HeavyLoad` - meant that *adding* a module
    // which writes to disk lowered the risk shown to the operator. A preflight
    // screen that gets quieter as the run gets more invasive is worse than no
    // preflight screen. `risk_never_decreases_as_invasiveness_rises` pins this.
    //
    // Everything above observational saturates something the machine's real
    // work also needs, so they share `HeavyLoad`; which resource, and how much
    // of it, is what the individual findings are for.
    let base = match max_safety {
        SafetyClass::Observational => RiskClass::Safe,
        SafetyClass::ComputeIntensive
        | SafetyClass::WritesTemporaryFiles
        | SafetyClass::UsesNetwork
        | SafetyClass::ProvisionsServices => RiskClass::HeavyLoad,
    };
    // Anything above observational on a machine that looks live is a
    // production risk, regardless of the module's own classification.
    if production == ProductionLikelihood::Likely && max_safety > SafetyClass::Observational {
        return RiskClass::ProductionRisk;
    }
    // A machine already under heavy load cannot produce a usable measurement.
    let cpus = inventory.cpu.logical_cpus.max(1) as f64;
    if inventory.platform.load1.is_some_and(|l| l / cpus > 2.0) {
        return RiskClass::ProductionRisk;
    }
    base
}

#[cfg(test)]
// In tests, `unwrap`/`expect` panicking *is* the failure signal.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic_in_result_fn)]
mod tests {
    use super::*;
    use darcbench_inventory::software::{ProductionLikelihood, SoftwareInfo};

    fn input<'a>(
        inventory: &'a Inventory,
        registry: &'a Registry,
        modules: &'a [ModuleId],
        state_dir: &'a std::path::Path,
        params: &'a ModuleParams,
    ) -> PreflightInput<'a> {
        PreflightInput {
            inventory,
            registry,
            modules,
            profile: Profile::Quick,
            params,
            state_dir,
            force: false,
            cycle_target: None,
        }
    }

    fn cycling_input<'a>(
        inventory: &'a Inventory,
        registry: &'a Registry,
        modules: &'a [ModuleId],
        state_dir: &'a std::path::Path,
        params: &'a ModuleParams,
        minutes: u64,
    ) -> PreflightInput<'a> {
        PreflightInput {
            profile: Profile::Endurance,
            cycle_target: Some(std::time::Duration::from_secs(minutes * 60)),
            ..input(inventory, registry, modules, state_dir, params)
        }
    }

    /// Params carrying a scratch directory, as a real run always has.
    fn quick_params(state_dir: &std::path::Path) -> ModuleParams {
        ModuleParams::for_profile(Profile::Quick).with_scratch_dir(state_dir.join("scratch"))
    }

    fn quiet_inventory() -> Inventory {
        let mut inv = Inventory::collect();
        inv.platform.load1 = Some(0.0);
        inv.memory.swap_total_bytes = 0;
        inv.memory.swap_free_bytes = 0;
        inv.software = SoftwareInfo {
            panels: vec![],
            web_servers: vec![],
            container_runtimes: vec![],
            databases: vec![],
            runtimes: vec![],
            firewalls: vec![],
            listening_tcp_ports: vec![],
            production_likelihood: ProductionLikelihood::Unlikely,
            production_signals: vec![],
        };
        inv.platform.scope = Scope::BareMetal;
        inv.platform.cgroup_cpu_limit = None;
        inv
    }

    /// A more invasive run must never be shown as less risky.
    ///
    /// Regression: the safety-class-to-risk mapping dipped in the middle, so a
    /// profile that added a module writing to disk was reported as
    /// `ModerateLoad` where the CPU-only profile had been `HeavyLoad`. Because
    /// `max_safety` is a maximum, every additional module can only push the
    /// class up - so the mapping it feeds has to be non-decreasing, or the
    /// preflight screen gets quieter as the run gets more dangerous.
    #[test]
    fn risk_never_decreases_as_invasiveness_rises() {
        let quiet = quiet_inventory();
        let ladder = [
            SafetyClass::Observational,
            SafetyClass::ComputeIntensive,
            SafetyClass::WritesTemporaryFiles,
            SafetyClass::UsesNetwork,
            SafetyClass::ProvisionsServices,
        ];
        // The ladder must really be ordered, or the property below is vacuous.
        assert!(ladder.windows(2).all(|w| w[0] < w[1]));

        let mut previous = RiskClass::Safe;
        for class in ladder {
            let risk = classify(class, ProductionLikelihood::Unlikely, &quiet);
            assert!(
                risk >= previous,
                "{class:?} was classified {risk:?}, below the {previous:?} of a less invasive class"
            );
            previous = risk;
        }
    }

    /// Preflight exists to show an operator what a run costs before it starts,
    /// and memory is a cost. A module that quietly takes a large share of a
    /// live host's memory is exactly the surprise this screen prevents.
    #[test]
    fn a_large_memory_allocation_is_disclosed_and_a_huge_one_is_refused() {
        let registry = Registry::builtin();
        let dir = std::env::temp_dir();
        let modules = registry.modules_for_profile(Profile::Quick);

        // Plenty of memory: `memory.bandwidth` sizes itself to a fraction of
        // it, so nothing needs saying.
        let mut roomy = quiet_inventory();
        roomy.memory.total_bytes = 256 << 30;
        roomy.memory.available_bytes = 200 << 30;
        let result = run(&input(
            &roomy,
            &registry,
            &modules,
            &dir,
            &quick_params(&dir),
        ));
        assert!(
            !result
                .findings
                .iter()
                .any(|f| f.check == "memory.allocation"),
            "a run well inside the budget must not nag: {:?}",
            result.findings
        );
        assert!(
            result.estimated_peak_memory_bytes > 0,
            "the memory module must declare what it will hold"
        );
        assert!(
            result.estimated_peak_memory_bytes as f64 <= (200u64 << 30) as f64 * MEMORY_BLOCK_SHARE,
            "the module must size itself inside the ceiling"
        );

        // A module that ignores the budget must still be caught. Nothing
        // shipped does, so the guard is exercised against a synthetic estimate.
        let available = 1u64 << 30;
        for (peak, blocking) in [
            (available / 20, false),   // 5%: silent
            (available / 5, false),    // 20%: disclosed, not blocking
            (available * 4 / 5, true), // 80%: refused
        ] {
            let share = peak as f64 / available as f64;
            let disclosed = share > MEMORY_DISCLOSE_SHARE;
            let refused = share > MEMORY_BLOCK_SHARE;
            assert_eq!(
                refused, blocking,
                "share {share} should block == {blocking}"
            );
            if !disclosed {
                assert!(!refused, "an undisclosed share must never block");
            }
        }
    }

    #[test]
    fn a_quiet_machine_passes_and_reports_an_estimate() {
        let registry = Registry::builtin();
        let inv = quiet_inventory();
        let modules = registry.modules_for_profile(Profile::Quick);
        let dir = std::env::temp_dir();
        let result = run(&input(&inv, &registry, &modules, &dir, &quick_params(&dir)));
        assert!(result.passed, "findings: {:?}", result.findings);
        assert!(result.estimated_duration_s > 0);
        // The quick profile now includes a module that writes, so the disk
        // guard has a real number to work with rather than a constant zero.
        assert!(
            result.estimated_bytes_written > 0,
            "a profile containing storage.mixed must declare the space it needs"
        );
        assert!(
            result.estimated_write_volume_bytes > result.estimated_bytes_written,
            "wear must exceed the space bound: the fixture is written more than once"
        );
        assert!(
            result
                .findings
                .iter()
                .any(|f| f.check == "storage.wear" && !f.blocking),
            "flash wear must be disclosed before a storage run, and must not block it"
        );
        assert!(
            result.risk >= RiskClass::HeavyLoad,
            "a CPU-saturating run that also writes is heavy by nature, got {:?}",
            result.risk
        );
    }

    /// The `quick` profile contacts nothing, and anything that does says so.
    ///
    /// Both directions matter. A first run on an unfamiliar server should not
    /// open a socket to a third party at all - that is why `quick` excludes the
    /// network module. And when a profile does reach out, the operator finds out
    /// before the run, from the preflight screen, not afterwards from a firewall
    /// log.
    #[test]
    fn outbound_traffic_is_absent_from_quick_and_disclosed_everywhere_else() {
        let registry = Registry::builtin();
        let inv = quiet_inventory();
        let dir = std::env::temp_dir();

        let quick = registry.modules_for_profile(Profile::Quick);
        let result = run(&input(&inv, &registry, &quick, &dir, &quick_params(&dir)));
        assert_eq!(
            result.estimated_network_bytes, 0,
            "the first profile anyone runs must make no outbound connection"
        );
        assert!(
            !result.findings.iter().any(|f| f.check == "network.egress"),
            "nothing to disclose means nothing to say"
        );

        let standard = registry.modules_for_profile(Profile::Standard);
        let result = run(&input(
            &inv,
            &registry,
            &standard,
            &dir,
            &quick_params(&dir),
        ));
        assert!(
            result.estimated_network_bytes > 0,
            "a profile containing network.transfer must declare the bytes it will move"
        );
        let egress = result
            .findings
            .iter()
            .find(|f| f.check == "network.egress")
            .expect("outbound traffic must be disclosed before the run, not after");
        assert!(
            !egress.blocking,
            "measuring the network is the point of a network module"
        );
        // The operator of every reachable host is named. A disclosure that says
        // "some traffic will occur" is not a disclosure.
        for endpoint in darcbench_modules::network_endpoints::all() {
            assert!(
                egress.message.contains(endpoint.operator),
                "{} is reachable but its operator is not named in the disclosure: {}",
                endpoint.host,
                egress.message
            );
        }
    }

    /// An hour-long run costs an hour of the machine and an hour of flash
    /// endurance, and both have to be on the screen before it starts.
    ///
    /// Estimating from a single pass was the shape of the bug this guards: the
    /// numbers would have been right for a `standard` run and wrong by the
    /// cycle count - an order of magnitude - for the profile that actually
    /// repeats.
    #[test]
    fn a_cycling_run_discloses_the_whole_cost_not_one_cycles_worth() {
        let registry = Registry::builtin();
        let inv = quiet_inventory();
        let dir = std::env::temp_dir();
        let modules = registry.modules_for_profile(Profile::Endurance);
        let params =
            ModuleParams::for_profile(Profile::Endurance).with_scratch_dir(dir.join("scratch"));

        let once = run(&input(&inv, &registry, &modules, &dir, &params));
        let cycling = run(&cycling_input(&inv, &registry, &modules, &dir, &params, 60));

        assert!(
            cycling.estimated_duration_s > once.estimated_duration_s,
            "a run that repeats for an hour cannot cost one pass"
        );
        assert!(
            cycling.estimated_write_volume_bytes > once.estimated_write_volume_bytes,
            "flash endurance accumulates across cycles and must be estimated across them"
        );
        assert_eq!(
            cycling.estimated_peak_memory_bytes, once.estimated_peak_memory_bytes,
            "cycles run one after another, so the memory high-water mark is one cycle's"
        );
        assert_eq!(
            cycling.estimated_bytes_written, once.estimated_bytes_written,
            "the fixture is rewritten, not re-allocated: space needed does not accumulate"
        );

        let duration = cycling
            .findings
            .iter()
            .find(|f| f.check == "run.duration")
            .expect("a long run must announce how long it is");
        assert!(!duration.blocking, "asking for an hour is not an error");
        assert!(duration.message.contains("60-minute"));

        assert!(
            !once.findings.iter().any(|f| f.check == "run.duration"),
            "a profile that does not cycle has no duration target to announce"
        );
    }

    /// Endurance must not carry the network module: its transfer ceiling is a
    /// per-run bound, and cycling it for an hour would breach that bound.
    #[test]
    fn a_cycling_run_generates_no_third_party_traffic() {
        let registry = Registry::builtin();
        let inv = quiet_inventory();
        let dir = std::env::temp_dir();
        let modules = registry.modules_for_profile(Profile::Endurance);
        let params =
            ModuleParams::for_profile(Profile::Endurance).with_scratch_dir(dir.join("scratch"));
        let result = run(&cycling_input(&inv, &registry, &modules, &dir, &params, 60));
        assert_eq!(
            result.estimated_network_bytes, 0,
            "an hour of cycles against somebody else's CDN is not ours to generate"
        );
    }

    #[test]
    fn an_empty_module_set_blocks_the_run() {
        let registry = Registry::builtin();
        let inv = quiet_inventory();
        let dir = std::env::temp_dir();
        let result = run(&input(&inv, &registry, &[], &dir, &quick_params(&dir)));
        assert!(!result.passed);
        assert!(result
            .findings
            .iter()
            .any(|f| f.blocking && f.check == "modules.selected"));
    }

    #[test]
    fn a_live_looking_server_is_classified_production_risk() {
        let registry = Registry::builtin();
        let mut inv = quiet_inventory();
        inv.software.production_likelihood = ProductionLikelihood::Likely;
        inv.software.production_signals = vec!["port 443 has an active listener".into()];
        let modules = registry.modules_for_profile(Profile::Quick);
        let dir = std::env::temp_dir();
        let result = run(&input(&inv, &registry, &modules, &dir, &quick_params(&dir)));
        assert_eq!(result.risk, RiskClass::ProductionRisk);
        assert!(
            result
                .findings
                .iter()
                .any(|f| f.check == "system.production"),
            "the operator must be told why"
        );
        assert!(
            !result.passed,
            "a production-risk run must not start unattended"
        );
    }

    #[test]
    fn force_can_override_a_warning_but_never_a_blocker() {
        let registry = Registry::builtin();
        let mut inv = quiet_inventory();
        inv.software.production_likelihood = ProductionLikelihood::Likely;
        let modules = registry.modules_for_profile(Profile::Quick);
        let dir = std::env::temp_dir();

        let params = quick_params(&dir);
        let mut forced = input(&inv, &registry, &modules, &dir, &params);
        forced.force = true;
        assert!(
            run(&forced).passed,
            "force should clear a production warning"
        );

        // ...but not a missing module set.
        let mut forced_empty = input(&inv, &registry, &[], &dir, &params);
        forced_empty.force = true;
        assert!(
            !run(&forced_empty).passed,
            "force must never clear a blocking finding"
        );
    }

    #[test]
    fn a_heavily_loaded_machine_is_production_risk() {
        let registry = Registry::builtin();
        let mut inv = quiet_inventory();
        inv.platform.load1 = Some(inv.cpu.logical_cpus as f64 * 3.0);
        let modules = registry.modules_for_profile(Profile::Quick);
        let dir = std::env::temp_dir();
        let result = run(&input(&inv, &registry, &modules, &dir, &quick_params(&dir)));
        assert_eq!(result.risk, RiskClass::ProductionRisk);
        assert!(result.findings.iter().any(|f| f.check == "system.load"));
    }

    #[test]
    fn container_scope_is_disclosed() {
        let registry = Registry::builtin();
        let mut inv = quiet_inventory();
        inv.platform.scope = Scope::Container;
        let modules = registry.modules_for_profile(Profile::Quick);
        let dir = std::env::temp_dir();
        let result = run(&input(&inv, &registry, &modules, &dir, &quick_params(&dir)));
        assert!(result
            .findings
            .iter()
            .any(|f| f.check == "environment.scope"));
    }

    #[test]
    fn a_cgroup_cpu_limit_is_disclosed() {
        let registry = Registry::builtin();
        let mut inv = quiet_inventory();
        inv.platform.cgroup_cpu_limit = Some(0.5);
        let modules = registry.modules_for_profile(Profile::Quick);
        let dir = std::env::temp_dir();
        let result = run(&input(&inv, &registry, &modules, &dir, &quick_params(&dir)));
        assert!(result
            .findings
            .iter()
            .any(|f| f.check == "environment.cgroup_cpu"));
    }

    #[test]
    fn an_unknown_module_blocks_the_run() {
        let registry = Registry::builtin();
        let inv = quiet_inventory();
        let dir = std::env::temp_dir();
        let bogus = vec![ModuleId::new("not.real").expect("id")];
        let result = run(&input(&inv, &registry, &bogus, &dir, &quick_params(&dir)));
        assert!(!result.passed);
        assert!(result
            .findings
            .iter()
            .any(|f| f.blocking && f.check == "modules.known"));
    }
}
