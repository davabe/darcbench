//! Plain line-oriented run progress, for terminals that cannot take the
//! dashboard.
//!
//! This is the path taken when `darcbench run` is not a `--json` invocation but
//! also cannot draw [`crate::tui`]: output redirected to a file, a CI log, a
//! pipe, `--no-color`, `TERM=dumb`, or an explicit `--no-tui`. Before it
//! existed those invocations printed a header and then nothing until the run
//! finished, which on `endurance` is a job that looks hung for hours.
//!
//! # What it deliberately does not print
//!
//! Samples. A `deep` profile emits thousands of `module.sample` events and a
//! line each would bury the events that matter in a log somebody has to scroll.
//! Progress is instead folded and emitted at most once per module per decile,
//! so the volume of output is a function of the module count and not of how
//! fast the machine under test happens to be.

use std::collections::BTreeMap;
use std::sync::Arc;

use darcbench_protocol::metrics::ModuleStatus;
use darcbench_protocol::{Event, ModuleId};

use crate::cli::Style;
use crate::runner::RunHandle;

/// Progress is announced when it crosses one of these fractions.
const STEPS: [f64; 4] = [0.25, 0.5, 0.75, 1.0];

/// Follows `handle` to completion, printing a line per meaningful transition.
pub(crate) async fn plain(handle: Arc<RunHandle>, style: &Style) {
    // Subscribe before taking the backlog, for the same reason `tui::drive`
    // does: the other order has a window in which an event reaches nobody.
    let mut events = handle.subscribe();
    let backlog = handle.events_since(None).unwrap_or_default();

    let mut state = Progress::default();
    for envelope in &backlog {
        state.fold(&envelope.event, envelope.seq, style);
    }

    loop {
        match events.recv().await {
            Ok(envelope) => {
                let terminal = envelope.event.is_stream_terminal();
                state.fold(&envelope.event, envelope.seq, style);
                if terminal {
                    return;
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                // Under lag the replay buffer is the only complete source. The
                // fold is idempotent by sequence number, so re-reading it
                // cannot double-print.
                for envelope in handle.events_since(state.last_seq).unwrap_or_default() {
                    state.fold(&envelope.event, envelope.seq, style);
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
        }

        if handle.state().is_terminal() {
            return;
        }
    }
}

#[derive(Default)]
struct Progress {
    /// Highest decile already announced, per module.
    announced: BTreeMap<ModuleId, usize>,
    last_seq: Option<u64>,
}

impl Progress {
    fn fold(&mut self, event: &Event, seq: u64, style: &Style) {
        if let Some(seen) = self.last_seq {
            if seq <= seen {
                return;
            }
        }
        self.last_seq = Some(seq);

        match event {
            Event::RunCreated(created) => {
                println!(
                    "{} profile {}  ({} module(s), agent {})",
                    style.bold("DARCBench"),
                    created.profile,
                    created.modules.len(),
                    created.agent_version
                );
                println!(
                    "{}",
                    style.yellow(
                        "Scoring model is uncalibrated development output; raw measurements are \
                         real."
                    )
                );
            }
            Event::PreflightCompleted(preflight) => {
                println!(
                    "  preflight    {:?}  {}  ~{}s",
                    preflight.risk,
                    if preflight.passed {
                        style.green("passed")
                    } else {
                        style.red("BLOCKED")
                    },
                    preflight.estimated_duration_s
                );
                for finding in preflight.findings.iter().filter(|f| f.blocking) {
                    println!("    {} {}", style.red("blocked"), finding.message);
                }
            }
            Event::ModuleStarted(life) => {
                // Cleared, not left standing. `endurance` runs the same module
                // once per cycle, and `announced` is a high-water mark: after
                // cycle 0 reached 100% it sat at the last step, so every later
                // cycle restarting at 0% compared below it and printed nothing.
                // An hour-long run would have shown progress for its first
                // cycle and then gone quiet for the rest.
                self.announced.remove(&life.module.id);
                println!(
                    "  {} {} ({}/{})",
                    style.cyan("start "),
                    life.module.id,
                    life.index + 1,
                    life.total
                );
            }
            Event::ModuleSample(sample) => {
                let reached = STEPS
                    .iter()
                    .filter(|step| sample.module_progress + f64::EPSILON >= **step)
                    .count();
                let announced = self.announced.entry(sample.module.clone()).or_insert(0);
                if reached > *announced {
                    *announced = reached;
                    // `saturating_sub(1)` because `reached` is a count, and the
                    // last entry of STEPS is the one just crossed.
                    let step = STEPS[reached.saturating_sub(1).min(STEPS.len() - 1)];
                    println!(
                        "         {}  {:.0}%",
                        style.dim(sample.module.as_str()),
                        step * 100.0
                    );
                }
            }
            Event::ModuleWarning(warning) => {
                println!(
                    "  {} {}: {}",
                    style.yellow("warn  "),
                    warning.module,
                    warning.warning.message
                );
            }
            Event::ModuleCompleted(completed) => {
                let result = &completed.result;
                let status = format!("{:?}", result.status).to_lowercase();
                println!(
                    "  {} {} {} ({} metric(s))",
                    match result.status {
                        ModuleStatus::Completed => style.green("done  "),
                        ModuleStatus::Failed | ModuleStatus::Cancelled => style.red("done  "),
                        _ => style.yellow("done  "),
                    },
                    result.module.id,
                    status,
                    result.metrics.len()
                );
            }
            Event::ModuleFailed(failed) => {
                println!(
                    "  {} {}: {}",
                    style.red("failed"),
                    failed.module.id,
                    failed.error
                );
            }
            Event::RunCompleted(completed) => {
                println!(
                    "  {} {:?}  verdict {:?}",
                    style.bold("finish"),
                    completed.state,
                    completed.verdict.state
                );
            }
            _ => {}
        }
    }
}

#[cfg(test)]
// In tests, `unwrap`/`expect` panicking *is* the failure signal.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic_in_result_fn)]
mod tests {
    use super::*;

    fn module() -> ModuleId {
        ModuleId::new("cpu.mixed").unwrap()
    }

    /// The whole point of the decile gate: a module emitting thousands of
    /// samples must not emit thousands of lines.
    #[test]
    fn progress_is_announced_at_most_once_per_step() {
        let mut progress = Progress::default();
        let id = module();
        let mut announcements = 0;
        for rep in 0..=1000u32 {
            let fraction = f64::from(rep) / 1000.0;
            let reached = STEPS
                .iter()
                .filter(|step| fraction + f64::EPSILON >= **step)
                .count();
            let announced = progress.announced.entry(id.clone()).or_insert(0);
            if reached > *announced {
                *announced = reached;
                announcements += 1;
            }
        }
        assert_eq!(
            announcements,
            STEPS.len(),
            "1001 samples must produce {} lines, not 1001",
            STEPS.len()
        );
    }

    fn started() -> Event {
        Event::ModuleStarted(darcbench_protocol::events::ModuleLifecycle {
            module: darcbench_protocol::ModuleRef {
                id: module(),
                version: "1.0.0".into(),
            },
            index: 0,
            total: 1,
            phase: None,
        })
    }

    /// Regression: `announced` is a high-water mark, and `endurance` runs the
    /// same module once per cycle. Without clearing it when a module starts,
    /// cycle 0 left it at the final step and every later cycle - restarting at
    /// 0% - compared below it and printed nothing. A twelve-hour run would have
    /// reported progress for its first cycle and then gone silent.
    #[test]
    fn each_endurance_cycle_reports_its_own_progress() {
        let mut progress = Progress::default();
        let style = Style::new(false);
        let id = module();
        let mut seq = 0;

        for cycle in 0..3 {
            progress.fold(&started(), seq, &style);
            seq += 1;
            assert_eq!(
                progress.announced.get(&id).copied().unwrap_or(0),
                0,
                "cycle {cycle} must start from nothing announced"
            );

            // Real sample events, folded through the real path - the reset is
            // only worth testing against the code that reads it.
            for rep in 1..=20u32 {
                progress.fold(&sample(f64::from(rep) / 20.0), seq, &style);
                seq += 1;
            }
            assert_eq!(
                progress.announced.get(&id).copied().unwrap_or(0),
                STEPS.len(),
                "cycle {cycle} must announce every step of its own progress"
            );
        }
    }

    fn sample(module_progress: f64) -> Event {
        Event::ModuleSample(darcbench_protocol::events::ModuleSampleEvent {
            module: module(),
            metric_key: "crypto_sha256.single".into(),
            rep: 1,
            warmup: false,
            value: 1.0,
            unit: "MiB/s".into(),
            duration_ms: 1.0,
            module_progress,
        })
    }

    /// A replayed envelope must not print twice, because the backlog and the
    /// live subscription deliberately overlap.
    #[test]
    fn a_replayed_sequence_number_is_ignored() {
        let mut progress = Progress::default();
        let style = Style::new(false);
        let event = Event::ModuleStarted(darcbench_protocol::events::ModuleLifecycle {
            module: darcbench_protocol::ModuleRef {
                id: module(),
                version: "1.0.0".into(),
            },
            index: 0,
            total: 1,
            phase: None,
        });
        progress.fold(&event, 7, &style);
        assert_eq!(progress.last_seq, Some(7));
        // Older and equal sequence numbers are both already folded.
        progress.fold(&event, 7, &style);
        progress.fold(&event, 3, &style);
        assert_eq!(progress.last_seq, Some(7));
    }
}
