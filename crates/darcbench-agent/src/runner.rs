//! Run orchestration: lifecycle, event emission, telemetry and finalisation.
//!
//! # Single run at a time
//!
//! The agent refuses to start a second run while one is in flight. Two
//! benchmarks sharing a machine measure each other, and a suite that lets you
//! do that by accident produces numbers nobody can defend.
//!
//! # Cancellation
//!
//! Cancellation is cooperative and always leaves the run in a consistent
//! terminal state. A cancelled run still produces a bundle - one marked
//! `Invalid` with an `Interrupted` reason - because "the operator stopped it"
//! is itself a fact worth recording, and because half-written state is worse
//! than a clearly-labelled incomplete result.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use crate::index::RunIndex;
use darcbench_inventory::platform::Scope;
use darcbench_inventory::{Inventory, TelemetrySampler, TelemetrySnapshot};
use darcbench_modules::module::ModuleReporter;
use darcbench_modules::{MachineFacts, ModuleError, ModuleParams, Registry};
use darcbench_protocol::events::{
    CategoryScore, Envelope, Event, Heartbeat, ModuleCompletedEvent, ModuleFailedEvent,
    ModuleLifecycle, ModuleSampleEvent, ModuleWarningEvent, PreflightStarted, ReportGenerated,
    RunCompleted, RunCreated, ScoreEvent, TelemetryEvent,
};
use darcbench_protocol::metrics::{ModuleResult, ModuleStatus, Warning, WarningCode};
use darcbench_protocol::{
    ModuleId, ModuleRef, Profile, ResultState, RunId, RunState, RunSummary, ENDURANCE_MAX_MINUTES,
    PROTOCOL_VERSION,
};
use darcbench_report::bundle::{Bundle, BundleMeta, RunRecord, TelemetrySummary};
use darcbench_report::{validate_bundle, AgentKey};
use darcbench_scoring::{ScoreCard, ScoringModel};
use tokio::sync::{broadcast, watch};
use tokio_util::sync::CancellationToken;

use crate::config::StatePath;

pub(crate) const AGENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// How many past events are retained for replay to a reconnecting client.
///
/// Sized to comfortably cover a full quick run so a browser that reconnects
/// mid-run can rebuild complete state. Beyond this, a client is told to refetch
/// the run rather than being handed a silently truncated prefix.
const REPLAY_BUFFER: usize = 4096;

/// Telemetry sampling interval. See `docs/BENCHMARK-METHODOLOGY.md`:
/// the observer is part of the system under test, so this is deliberately slow.
const TELEMETRY_INTERVAL_MS: u64 = 1000;

/// The complete, ordered event stream, kept independently of the replay buffer.
///
/// Two encodings of the same events, both built incrementally as they are
/// emitted, because the two consumers need different bytes:
///
/// * `ndjson` is what `events.ndjson` contains, in the wire encoding a
///   consumer replays from.
/// * `digest` is a running SHA-256 over the *canonical* (DCJ/1) form, which is
///   what a signature can be checked against independently of formatting.
///
/// Retaining serialised bytes rather than `Envelope` values is deliberate:
/// it is the smaller of the two, it is exactly what gets written, and it makes
/// the memory cost of a long run proportional to the file it produces - about
/// a megabyte an hour.
#[derive(Debug)]
struct EventRecord {
    ndjson: Vec<u8>,
    digest: sha2::Sha256,
}

impl EventRecord {
    fn new() -> Self {
        use sha2::Digest;
        Self {
            ndjson: Vec::new(),
            digest: sha2::Sha256::new(),
        }
    }

    fn append(&mut self, envelope: &Envelope) {
        use sha2::Digest;
        // A failure here is a bug in the event types, not a runtime condition,
        // and dropping the event from the record while it stays in the replay
        // buffer would be the one outcome worse than either. Both encodings are
        // therefore attempted independently and a failure is loud.
        match serde_json::to_vec(envelope) {
            Ok(bytes) => {
                self.ndjson.extend_from_slice(&bytes);
                self.ndjson.push(b'\n');
            }
            Err(error) => {
                tracing::warn!(%error, seq = envelope.seq, "event could not be encoded");
            }
        }
        match darcbench_report::canonical_json(envelope) {
            Ok(bytes) => {
                self.digest.update(&bytes);
                self.digest.update(b"\n");
            }
            Err(error) => {
                tracing::warn!(%error, seq = envelope.seq, "event could not be canonicalised");
            }
        }
    }

    /// The digest as of now, without consuming the running state.
    ///
    /// Cloned rather than finalised because the stream continues: this is taken
    /// part-way through finalisation, and the events that announce the bundle
    /// cannot be covered by a digest the bundle carries.
    fn digest_so_far(&self) -> String {
        use sha2::Digest;
        format!("sha256:{}", hex::encode(self.digest.clone().finalize()))
    }
}

/// Cycles an endurance run completes before the duration target may end it.
///
/// Two, because retention is a comparison and one measurement is not one. A
/// single-cycle endurance run would report the same numbers as a standard run
/// while claiming to have measured what happens over an hour, which is worse
/// than not running it: it would answer the question wrongly rather than
/// declining to answer.
pub(crate) const MIN_ENDURANCE_CYCLES: u32 = 2;

/// Package temperature at which the watchdog stops the run.
///
/// **Throttling is measured, never prevented.** A machine that pulls its clocks
/// back at 95 C is doing exactly what it is designed to do, and that behaviour
/// is the endurance profile's most valuable finding on bare metal - guarding
/// against it would destroy the measurement the profile exists to take.
///
/// This threshold is above the point where any sane platform has already
/// engaged its own protection, so reaching it *continuously* says the silicon's
/// own limiter is no longer keeping up. At that point the CPU is still safe
/// (it will halt itself long before damage) but the components around it are
/// less protected and less monitored: VRMs, the NVMe drive a few centimetres
/// away, and whatever else shares the chassis. Fifty more minutes of that on a
/// machine belonging to somebody else is not a risk this tool gets to take on
/// their behalf.
const THERMAL_ABORT_C: f64 = 100.0;

/// Consecutive seconds at [`THERMAL_ABORT_C`] before the run is stopped.
///
/// A single reading is not a condition. Sensors glitch, and a one-second spike
/// during a workload transition is normal on a machine that is otherwise fine.
const THERMAL_ABORT_SAMPLES: u32 = 30;

/// Longest any run may occupy the machine, whatever it was asked for.
///
/// A backstop behind the cycle target rather than a duplicate of it: the target
/// bounds when a *new cycle* may start, and this bounds the run. They differ
/// when something inside a cycle stops making progress, which is the case a
/// watchdog exists for.
fn hard_runtime_ceiling(handle: &RunHandle) -> Duration {
    // A generous multiple of what was asked for, because overshooting a cycle
    // boundary is ordinary and being killed for it would be a bug, not a guard.
    // The absolute ceiling still applies on top.
    let requested = handle.cycle_target.unwrap_or(Duration::from_secs(3600));
    (requested * 2).min(Duration::from_secs(
        u64::from(ENDURANCE_MAX_MINUTES) * 60 + 3600,
    ))
}

/// External CPU use above which the measurement in flight is contaminated.
///
/// A percentage of the *whole machine*, not of one core, and it counts only
/// work this process did not do (see
/// [`TelemetrySnapshot::cpu_external_busy_pct`]). Ten percent of a machine is
/// well beyond a cron job or an ssh session and comfortably below a
/// co-resident service doing real work, which is the line that matters: the
/// first must not degrade a run and the second must.
const EXTERNAL_LOAD_WARN_PCT: f64 = 10.0;

/// Consecutive seconds above [`EXTERNAL_LOAD_WARN_PCT`] before the module in
/// flight is degraded.
///
/// Twenty, because a benchmark's own process transitions - a module finishing,
/// the scoring pass, the report writer - briefly show up as external work in
/// the kernel's accounting, and none of them lasts twenty seconds.
const EXTERNAL_LOAD_WARN_SAMPLES: u32 = 20;

/// External CPU use at which the run is not worth continuing.
const EXTERNAL_LOAD_ABORT_PCT: f64 = 40.0;

/// Consecutive seconds above [`EXTERNAL_LOAD_ABORT_PCT`] before the run stops.
///
/// Deliberately long. Aborting is the right answer to a machine that has been
/// given other work to do, and the wrong answer to a two-minute backup window;
/// five minutes distinguishes them, and every module measured in the meantime
/// has already been marked contaminated by the warn tier.
const EXTERNAL_LOAD_ABORT_SAMPLES: u32 = 300;

/// What the watchdog decided about the latest telemetry sample.
enum WatchdogVerdict {
    /// Stop the run. The warning explains why, and reaches the bundle.
    Abort(Warning),
    /// Keep measuring, but the module in flight cannot be reported as clean.
    Degrade(Warning),
}

/// The facts about this host that decide whether the load ceiling can be armed.
#[derive(Clone, Copy, Debug)]
struct HostView {
    scope: Scope,
    /// Effective cgroup CPU quota in whole CPUs, when one is set.
    cgroup_cpu_limit: Option<f64>,
    logical_cpus: usize,
}

impl HostView {
    /// True when `/proc/stat` probably counts CPUs this run is not entitled to.
    ///
    /// # Why this is not just `scope == Container`
    ///
    /// It was, and that failed open. Scope detection is a heuristic over
    /// `/.dockerenv`, `/proc/1/cgroup` substrings and `container=` in
    /// `/proc/1/environ`, and it falls through to `BareMetal` whenever DMI is
    /// readable - which it is inside a container, because `/sys` is mounted. A
    /// containerd pod under cgroup v2 whose `/proc/1/cgroup` reads `0::/` is
    /// therefore labelled bare metal, and the guard would abort a
    /// correctly-behaving run for a co-tenant's work while the bundle claimed
    /// the ceiling was enforced. Failing open on a *guard that stops runs* is
    /// the wrong direction.
    ///
    /// So the label is only one of two signals. The other is positive evidence:
    /// a cgroup CPU quota smaller than the CPU count `/proc/stat` reports means
    /// this run is entitled to a fraction of what that file counts, whatever
    /// any label says.
    ///
    /// A container with no CPU quota and no detectable marker is still missed.
    /// That case needs a namespaced-`/proc` test rather than an inference, and
    /// it is on the backlog; this closes the common one and says so either way.
    fn proc_stat_may_describe_the_host(self) -> bool {
        if self.scope == Scope::Container {
            return true;
        }
        self.cgroup_cpu_limit
            .is_some_and(|limit| limit > 0.0 && limit < self.logical_cpus as f64)
    }
}

/// Run-scoped watchdog state.
///
/// Held by the telemetry task, which is the only thing already awake once a
/// second holding a live view of the machine.
struct Watchdog {
    consecutive_hot: u32,
    consecutive_contended: u32,
    consecutive_heavily_contended: u32,
    /// True while the current contended stretch has already been announced, so
    /// an hour of competition produces one event rather than 3600. Cleared when
    /// the machine goes quiet again, so a second stretch is announced too.
    contention_reported: bool,
    /// False when `/proc/stat` cannot be trusted to describe the CPUs this run
    /// is entitled to, in which case the load ceiling is not enforced at all.
    external_load_enforced: bool,
}

impl Watchdog {
    /// Builds the watchdog for a run on a host with this execution scope.
    ///
    /// # Why scope decides whether the ceiling exists
    ///
    /// The external-load figure is `/proc/stat` busy time minus this process's
    /// own. Inside a container whose `/proc` is not namespaced - the common
    /// case, without lxcfs - `/proc/stat` describes the *host*, so every other
    /// tenant's work is counted as external load and a correctly-behaving run
    /// would be aborted for the machine merely being shared.
    ///
    /// Rather than guess whether `/proc` is namespaced, the guard is not
    /// enforced under container scope and the run says so. That is the same
    /// choice the network module makes about packet loss: a guard that fires on
    /// the wrong evidence is worse than a guard that declares itself absent.
    fn new(host: HostView) -> Self {
        Self {
            consecutive_hot: 0,
            consecutive_contended: 0,
            consecutive_heavily_contended: 0,
            contention_reported: false,
            external_load_enforced: !host.proc_stat_may_describe_the_host(),
        }
    }

    /// The guards this watchdog could not arm on this host, in the operator's
    /// words, for `RunRecord::guards_not_enforced`.
    #[cfg(test)]
    fn for_scope(scope: Scope) -> Self {
        Self::new(HostView {
            scope,
            cgroup_cpu_limit: None,
            logical_cpus: 8,
        })
    }

    fn disclosures(&self) -> Vec<String> {
        if self.external_load_enforced {
            return Vec::new();
        }
        vec![
            "The runtime load ceiling was not enforced: this run is confined to a fraction of the \
             machine `/proc/stat` describes - a container, or a cgroup CPU quota below the host's \
             CPU count - so work by other tenants cannot be told apart from work competing with \
             this run. Nothing here reports on whether the measurement had the CPU to itself."
                .to_string(),
        ]
    }

    /// Decides what to do about one telemetry sample.
    ///
    /// Ordered by severity: the two abort conditions are checked before the
    /// degrade condition, so a run that is both overheating and contended stops
    /// for the reason that matters.
    fn check(
        &mut self,
        handle: &RunHandle,
        snapshot: &TelemetrySnapshot,
    ) -> Option<WatchdogVerdict> {
        let elapsed = handle.started.elapsed();
        let ceiling = hard_runtime_ceiling(handle);
        if elapsed > ceiling {
            return Some(WatchdogVerdict::Abort(Warning {
                code: WarningCode::ValidationFailed,
                message: format!(
                    "Stopped by the watchdog after {} minutes, past this run's {} minute ceiling. \
                     Something inside a cycle stopped making progress; the results collected up \
                     to that point are kept and the run is marked interrupted.",
                    elapsed.as_secs() / 60,
                    ceiling.as_secs() / 60,
                ),
                metric_key: None,
            }));
        }

        match snapshot.cpu_temp_c {
            Some(temp) if temp >= THERMAL_ABORT_C => {
                self.consecutive_hot += 1;
                if self.consecutive_hot >= THERMAL_ABORT_SAMPLES {
                    return Some(WatchdogVerdict::Abort(Warning {
                        code: WarningCode::ThermalThrottle,
                        message: format!(
                            "Stopped by the watchdog: package temperature held at {temp:.0} C for \
                             {THERMAL_ABORT_SAMPLES} seconds, at or above the \
                             {THERMAL_ABORT_C:.0} C abort threshold. Throttling itself is \
                             measured rather than prevented - it is the finding - but a machine \
                             that stays at this temperature is one whose own limiter is no longer \
                             keeping up, and continuing would put the components around the CPU \
                             at risk on a machine this tool does not own."
                        ),
                        metric_key: None,
                    }));
                }
            }
            // The tally is of *consecutive* samples, so anything cooler resets it.
            _ => self.consecutive_hot = 0,
        }

        self.check_external_load(handle, snapshot)
    }

    /// The load ceiling proper: work that is not this benchmark, during the
    /// window this benchmark is publishing numbers for.
    ///
    /// The contamination is recorded on the handle on *every* sustained sample,
    /// not only on the one that first crosses the line, because a contended
    /// stretch that outlasts one module has to degrade all of them. The return
    /// value is only about whether to announce it.
    fn check_external_load(
        &mut self,
        handle: &RunHandle,
        snapshot: &TelemetrySnapshot,
    ) -> Option<WatchdogVerdict> {
        if !self.external_load_enforced {
            return None;
        }
        let external = snapshot.cpu_external_busy_pct;

        if external >= EXTERNAL_LOAD_ABORT_PCT {
            self.consecutive_heavily_contended += 1;
        } else {
            self.consecutive_heavily_contended = 0;
        }
        if self.consecutive_heavily_contended >= EXTERNAL_LOAD_ABORT_SAMPLES {
            return Some(WatchdogVerdict::Abort(Warning {
                code: WarningCode::ExternalLoad,
                message: format!(
                    "Stopped by the watchdog: work other than this benchmark used {external:.0}% \
                     of the machine's CPU for at least {} minutes, past the \
                     {EXTERNAL_LOAD_ABORT_PCT:.0}% load ceiling. Nothing measured under that much \
                     competition describes the machine, so the run is stopped rather than \
                     continued to a number that would have to be thrown away - and the machine is \
                     handed back to whatever else needs it. The results collected before the \
                     contention began are kept.",
                    EXTERNAL_LOAD_ABORT_SAMPLES / 60,
                ),
                metric_key: None,
            }));
        }

        if external >= EXTERNAL_LOAD_WARN_PCT {
            self.consecutive_contended += 1;
        } else {
            self.consecutive_contended = 0;
            self.contention_reported = false;
            return None;
        }
        if self.consecutive_contended < EXTERNAL_LOAD_WARN_SAMPLES {
            return None;
        }

        let warning = Warning {
            code: WarningCode::ExternalLoad,
            message: format!(
                "Work other than this benchmark used {external:.0}% of the machine's CPU for at \
                 least {EXTERNAL_LOAD_WARN_SAMPLES} seconds during the measured window. The \
                 measurements are kept as evidence, and every module taken while it lasted is \
                 reported as degraded: they describe a machine sharing its CPU with something \
                 else, which is not the machine anyone is asking about."
            ),
            metric_key: None,
        };
        handle.note_contention(&warning);
        if self.contention_reported {
            return None;
        }
        self.contention_reported = true;
        Some(WatchdogVerdict::Degrade(warning))
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum RunError {
    #[error("a run is already in progress: {0}")]
    AlreadyRunning(RunId),
    #[error("no modules resolved for profile `{0}`")]
    NoModules(Profile),
    #[error("unknown module(s): {0}")]
    UnknownModules(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Internal(String),
}

/// Live state of one run.
pub(crate) struct RunHandle {
    pub(crate) id: RunId,
    pub(crate) profile: Profile,
    pub(crate) modules: Vec<ModuleRef>,
    pub(crate) created_at: chrono::DateTime<chrono::Utc>,
    /// How long to keep repeating the module set, for a profile that cycles.
    ///
    /// `None` is one pass and stop, which is every profile but `endurance`.
    pub(crate) cycle_target: Option<Duration>,
    started: Instant,
    seq: AtomicU64,
    /// Lifecycle state, published through a `watch` channel so a waiter is
    /// woken the moment the run finishes instead of discovering it on the next
    /// poll. `state_rx` exists only to make the synchronous `state()` read
    /// cheap from the blocking module threads.
    state: watch::Sender<RunState>,
    state_rx: watch::Receiver<RunState>,
    /// Replay buffer, oldest first. A `VecDeque` rather than a `Vec` because
    /// eviction happens on *every* event once the buffer is full, and
    /// `Vec::remove(0)` shifts all 4096 entries each time.
    events: RwLock<VecDeque<Envelope>>,
    /// True once the replay buffer has dropped events.
    truncated: AtomicBool,
    /// The complete stream, for `events.ndjson` and the digest. Never evicted.
    record: Mutex<EventRecord>,
    tx: broadcast::Sender<Envelope>,
    cancel: CancellationToken,
    results: Mutex<Vec<ModuleResult>>,
    telemetry: Mutex<Vec<TelemetrySnapshot>>,
    bundle: RwLock<Option<Bundle>>,
    current_module: RwLock<Option<ModuleId>>,
    /// Why the watchdog stopped the run, if it did.
    stopped_because: RwLock<Option<String>>,
    /// External-load contamination seen since the module in flight started.
    ///
    /// Written by the telemetry task, cleared by the module loop when a module
    /// starts and read when it finishes. Contention detected in the gap between
    /// two modules is therefore charged to the one that runs next, which is the
    /// conservative direction: the alternative is charging it to a module whose
    /// measured window had already closed.
    contention: RwLock<Option<Warning>>,
    /// Guards that could not be armed on this host, recorded into the bundle.
    guards_not_enforced: RwLock<Vec<String>>,
}

impl std::fmt::Debug for RunHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunHandle")
            .field("id", &self.id)
            .field("profile", &self.profile)
            .field("state", &self.state())
            .finish_non_exhaustive()
    }
}

impl RunHandle {
    fn new(
        id: RunId,
        profile: Profile,
        modules: Vec<ModuleRef>,
        cycle_target: Option<Duration>,
    ) -> Arc<Self> {
        let (tx, _) = broadcast::channel(REPLAY_BUFFER);
        let (state, state_rx) = watch::channel(RunState::Created);
        Arc::new(Self {
            id,
            profile,
            modules,
            created_at: chrono::Utc::now(),
            cycle_target,
            started: Instant::now(),
            seq: AtomicU64::new(0),
            state,
            state_rx,
            events: RwLock::new(VecDeque::new()),
            truncated: AtomicBool::new(false),
            record: Mutex::new(EventRecord::new()),
            tx,
            cancel: CancellationToken::new(),
            results: Mutex::new(Vec::new()),
            telemetry: Mutex::new(Vec::new()),
            bundle: RwLock::new(None),
            current_module: RwLock::new(None),
            stopped_because: RwLock::new(None),
            contention: RwLock::new(None),
            guards_not_enforced: RwLock::new(Vec::new()),
        })
    }

    /// Records that external work competed with the measurement in flight.
    fn note_contention(&self, warning: &Warning) {
        if let Ok(mut slot) = self.contention.write() {
            // Overwritten rather than kept: the newest reading carries the
            // percentage the operator will read, and one warning per module is
            // the contract with `ModuleResult::warnings`.
            *slot = Some(warning.clone());
        }
    }

    /// Takes and clears whatever contention was recorded.
    fn take_contention(&self) -> Option<Warning> {
        self.contention
            .write()
            .ok()
            .and_then(|mut slot| slot.take())
    }

    pub(crate) fn state(&self) -> RunState {
        *self.state_rx.borrow()
    }

    fn set_state(&self, state: RunState) {
        // A send error means every receiver has been dropped, which cannot
        // happen while `state_rx` is held by this handle.
        let _ = self.state.send(state);
    }

    /// Resolves once the run reaches a terminal state.
    ///
    /// Edge cases are handled by `watch`'s own semantics: the initial `borrow`
    /// covers a run that is *already* terminal, and `changed()` only observes
    /// transitions after this receiver was created, so no wake-up can be
    /// missed between the two.
    pub(crate) async fn wait_for_terminal(&self) {
        let mut rx = self.state.subscribe();
        loop {
            if rx.borrow_and_update().is_terminal() {
                return;
            }
            if rx.changed().await.is_err() {
                return;
            }
        }
    }

    pub(crate) fn cancel(&self) {
        self.cancel.cancel();
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }

    pub(crate) fn subscribe(&self) -> broadcast::Receiver<Envelope> {
        self.tx.subscribe()
    }

    /// Events with `seq > after`, for replay after a reconnect.
    ///
    /// Returns `None` when the requested position has already fallen out of
    /// the buffer: the client must refetch rather than be given a gap it
    /// cannot detect.
    pub(crate) fn events_since(&self, after: Option<u64>) -> Option<Vec<Envelope>> {
        let events = self.events.read().ok()?;
        match after {
            None => Some(events.iter().cloned().collect()),
            Some(after) => {
                // The next sequence number this client still needs. Saturating
                // because `Last-Event-ID` is caller-supplied: `u64::MAX + 1`
                // would panic a debug build and wrap to 0 in a release one.
                let needed = after.saturating_add(1);
                let first_available = events.front().map(|e| e.seq);
                if self.truncated.load(Ordering::Relaxed)
                    && first_available.is_some_and(|first| needed < first)
                {
                    return None;
                }
                // The buffer is ordered by `seq` (see `emit`), so the resume
                // point is a binary search rather than a scan of all 4096.
                let start = events.partition_point(|e| e.seq <= after);
                Some(events.iter().skip(start).cloned().collect())
            }
        }
    }

    pub(crate) fn bundle(&self) -> Option<Bundle> {
        self.bundle.read().ok().and_then(|b| b.clone())
    }

    pub(crate) fn summary(&self) -> RunSummary {
        let results = self.results.lock().map(|r| r.len()).unwrap_or(0);
        let total = self.modules.len().max(1);
        // Read the three fields that are needed straight out of the stored
        // bundle rather than cloning it. A bundle carries the full inventory,
        // every metric and every per-repetition sample, and `list()` calls this
        // once per run the agent has ever executed.
        let (finished_at, total_score, result_state) = self
            .bundle
            .read()
            .ok()
            .and_then(|slot| {
                slot.as_ref()
                    .map(|b| (b.run.finished_at, b.scores.total, b.verdict.state))
            })
            .map_or((None, None, None), |(f, t, s)| (Some(f), t, Some(s)));
        RunSummary {
            run_id: self.id.clone(),
            profile: self.profile,
            state: self.state(),
            created_at: self.created_at,
            finished_at,
            modules: self.modules.iter().map(|m| m.id.clone()).collect(),
            progress: (results as f64 / total as f64).clamp(0.0, 1.0),
            total_score,
            result_state,
        }
    }

    /// Emits an event, assigning the next sequence number.
    ///
    /// Sequence allocation, the replay-buffer append and the broadcast all
    /// happen under one lock. Allocating `seq` outside it would let the two
    /// concurrent emitters - the blocking module thread and the 1 Hz telemetry
    /// task - interleave between `fetch_add` and the append, so the replay
    /// buffer, `events.ndjson` and the live SSE stream could each carry a lower
    /// sequence number after a higher one. Clients fold the stream by `seq` and
    /// discard anything not newer than what they have already seen, so an
    /// out-of-order delivery is a silently dropped event, not a cosmetic
    /// problem.
    fn emit(&self, event: Event) {
        // Recover rather than discard on poisoning: losing the event stream is
        // worse than proceeding, and nothing under this lock can panic.
        let mut events = self
            .events
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let envelope = Envelope {
            protocol: PROTOCOL_VERSION.to_string(),
            run_id: self.id.clone(),
            seq: self.seq.fetch_add(1, Ordering::Relaxed),
            ts: chrono::Utc::now(),
            mono_ms: self.started.elapsed().as_millis() as u64,
            event,
        };

        // The permanent record is appended to here, inside the same critical
        // section that allocates `seq`, so the file on disk and the digest carry
        // events in exactly the order the sequence numbers claim.
        //
        // It is a *separate* structure from the replay buffer below, and that
        // separation is the point. The replay buffer is bounded because a
        // reconnecting browser only needs recent history; the record is the
        // audit log and must be complete. Deriving `events.ndjson` and
        // `events_digest` from the bounded buffer worked only for as long as no
        // profile emitted more than REPLAY_BUFFER events - which the endurance
        // profile does after about an hour, at which point the log would have
        // begun at a nonzero sequence number while `event_count` went on
        // claiming the whole stream.
        {
            let mut record = self
                .record
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            record.append(&envelope);
        }

        if events.len() >= REPLAY_BUFFER {
            events.pop_front();
            self.truncated.store(true, Ordering::Relaxed);
        }
        events.push_back(envelope.clone());
        // A send error only means nobody is listening right now, which is
        // normal: the event is already in the replay buffer.
        let _ = self.tx.send(envelope);
    }

    pub(crate) fn last_seq(&self) -> u64 {
        self.seq.load(Ordering::SeqCst).saturating_sub(1)
    }
}

/// Bridges a running module to the event stream.
struct RunReporter {
    handle: Arc<RunHandle>,
    module: ModuleId,
}

impl ModuleReporter for RunReporter {
    fn sample(
        &self,
        metric_key: &str,
        unit: &str,
        rep: u32,
        warmup: bool,
        value: f64,
        duration_ms: f64,
        module_progress: f64,
    ) {
        self.handle.emit(Event::ModuleSample(ModuleSampleEvent {
            module: self.module.clone(),
            metric_key: metric_key.to_string(),
            rep,
            warmup,
            value,
            unit: unit.to_string(),
            duration_ms,
            module_progress,
        }));
    }

    fn warn(&self, warning: Warning) {
        self.handle.emit(Event::ModuleWarning(ModuleWarningEvent {
            module: self.module.clone(),
            warning,
        }));
    }

    fn is_cancelled(&self) -> bool {
        self.handle.is_cancelled()
    }
}

/// Owns every run this agent knows about.
pub(crate) struct RunManager {
    registry: Registry,
    model: ScoringModel,
    state_dir: std::path::PathBuf,
    key: Arc<AgentKey>,
    runs: RwLock<Vec<Arc<RunHandle>>>,
    /// The run index. An index over the bundles, never a second copy of them -
    /// see [`crate::index`] and ADR-0005.
    index: RunIndex,
}

impl std::fmt::Debug for RunManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunManager")
            .field("state_dir", &self.state_dir)
            .finish_non_exhaustive()
    }
}

impl RunManager {
    pub(crate) fn new(state_dir: std::path::PathBuf, key: Arc<AgentKey>) -> Self {
        // An index that cannot be opened is degraded, not fatal. The bundles
        // are the results; refusing to run a benchmark because a cache of their
        // metadata would not open gets the priority exactly backwards. The
        // in-memory fallback keeps this process's own runs listable.
        let index = RunIndex::open(state_dir.join(crate::index::INDEX_FILE))
            .or_else(|error| {
                tracing::warn!(
                    %error,
                    "the run index could not be opened; falling back to an in-memory index for \
                     this process. The bundles under `runs/` are unaffected."
                );
                RunIndex::in_memory()
            })
            .unwrap_or_else(|_| {
                // Two failures in a row means SQLite itself is unusable here.
                // Everything that reads the index treats an empty one as "no
                // history", which is exactly what this is.
                tracing::warn!("no run index is available; run history will be empty");
                RunIndex::unavailable()
            });
        Self {
            registry: Registry::builtin(),
            model: ScoringModel::current(),
            state_dir,
            key,
            runs: RwLock::new(Vec::new()),
            index,
        }
    }

    pub(crate) fn index(&self) -> &RunIndex {
        &self.index
    }

    /// Brings the index back into agreement with the bundles on disk.
    ///
    /// Called once at startup. This is what makes a lost or corrupted index a
    /// rebuild rather than lost history, and what stops a list full of runs an
    /// operator deleted by hand.
    pub(crate) fn reconcile_index(&self) {
        let runs_dir = self.state_dir.join("runs");
        match self.index.reconcile(&runs_dir) {
            Ok(outcome) if outcome.is_noop() => {}
            Ok(outcome) => tracing::info!(
                indexed = outcome.indexed.len(),
                forgotten = outcome.forgotten.len(),
                unreadable = outcome.unreadable.len(),
                total = self.index.count().unwrap_or(0),
                "run index reconciled against the bundles on disk"
            ),
            Err(error) => tracing::warn!(%error, "the run index could not be reconciled"),
        }
    }

    pub(crate) fn registry(&self) -> &Registry {
        &self.registry
    }

    pub(crate) fn model(&self) -> &ScoringModel {
        &self.model
    }

    pub(crate) fn get(&self, id: &RunId) -> Option<Arc<RunHandle>> {
        self.runs.read().ok()?.iter().find(|r| &r.id == id).cloned()
    }

    pub(crate) fn list(&self) -> Vec<RunSummary> {
        self.runs
            .read()
            .map(|runs| runs.iter().rev().map(|r| r.summary()).collect())
            .unwrap_or_default()
    }

    /// The run currently in flight, if any.
    ///
    /// Test-only: `start` deliberately does **not** use this, because a
    /// read-then-write sequence is exactly the race that let two concurrent
    /// requests both claim the single-run slot. The real check happens inside
    /// the same write lock that inserts the handle.
    #[cfg(test)]
    fn active(&self) -> Option<Arc<RunHandle>> {
        self.runs
            .read()
            .ok()?
            .iter()
            .find(|r| !r.state().is_terminal())
            .cloned()
    }

    /// Creates a run and spawns its execution task.
    /// Starts a run.
    ///
    /// `cycle_target` overrides how long a cycling profile keeps repeating its
    /// module set. `None` takes the profile's own target, which is the only way
    /// to get a comparable endurance result - a run of a different length
    /// measures a different amount of decline, so the caller that supplies one
    /// is expected to have marked the run `Custom` already, exactly as it does
    /// for an explicit module list.
    pub(crate) fn start(
        self: &Arc<Self>,
        profile: Profile,
        requested_modules: Option<Vec<ModuleId>>,
        force: bool,
        cycle_target: Option<Duration>,
    ) -> Result<Arc<RunHandle>, RunError> {
        let modules = match requested_modules {
            Some(ids) => {
                self.registry.validate(&ids).map_err(|unknown| {
                    RunError::UnknownModules(
                        unknown
                            .iter()
                            .map(ToString::to_string)
                            .collect::<Vec<_>>()
                            .join(", "),
                    )
                })?;
                ids
            }
            None => self.registry.modules_for_profile(profile),
        };
        if modules.is_empty() {
            return Err(RunError::NoModules(profile));
        }

        let refs: Vec<ModuleRef> = modules
            .iter()
            .filter_map(|id| self.registry.get(id).map(|m| m.manifest().module_ref()))
            .collect();

        // An override still cannot exceed the absolute ceiling. That bound is
        // not about comparability: this software runs on servers belonging to
        // other people, and a mistyped duration must not be able to hold a
        // production machine at full load for a week.
        let cycle_target = cycle_target
            .or_else(|| {
                profile
                    .cycle_target_minutes()
                    .map(|m| Duration::from_secs(u64::from(m) * 60))
            })
            .map(|target| target.min(Duration::from_secs(u64::from(ENDURANCE_MAX_MINUTES) * 60)));

        let id = RunId::try_new().map_err(|e| RunError::Internal(e.to_string()))?;
        let handle = RunHandle::new(id, profile, refs, cycle_target);

        // Claim the single-run slot atomically. Checking `active()` under a read
        // lock and inserting under a later write lock would let two concurrent
        // requests both observe an idle agent and both start a CPU-saturating
        // benchmark - which would then measure each other.
        {
            let mut runs = self
                .runs
                .write()
                .map_err(|_| RunError::Internal("run registry lock poisoned".into()))?;
            if let Some(active) = runs.iter().find(|r| !r.state().is_terminal()) {
                return Err(RunError::AlreadyRunning(active.id.clone()));
            }
            runs.push(handle.clone());
        }

        let manager = self.clone();
        let task_handle = handle.clone();
        tokio::spawn(async move {
            manager.execute(task_handle, modules, force).await;
        });

        Ok(handle)
    }

    async fn execute(&self, handle: Arc<RunHandle>, modules: Vec<ModuleId>, force: bool) {
        let inventory = Inventory::collect();
        let environment_digest = inventory.performance_digest();
        // The scratch directory is composed here, by `StatePath`, and handed to
        // modules already validated. A module never joins a path itself, so
        // there is no path a caller can influence anywhere in the run.
        let scratch = StatePath::join(&self.state_dir, &["scratch"]).ok();
        let params = ModuleParams::for_profile(handle.profile)
            .with_facts(machine_facts(&inventory, scratch.as_ref()));
        let params = match &scratch {
            Some(dir) => params.with_scratch_dir(dir.as_path().to_path_buf()),
            None => params,
        };

        handle.emit(Event::RunCreated(RunCreated {
            profile: handle.profile,
            modules: handle.modules.clone(),
            agent_version: AGENT_VERSION.to_string(),
            scoring_model: self.model.version.clone(),
            environment_digest: environment_digest.clone(),
        }));

        // --- preflight ---------------------------------------------------
        handle.set_state(RunState::Preflight);
        handle.emit(Event::PreflightStarted(PreflightStarted {
            checks: vec![
                "modules.selected".into(),
                "modules.known".into(),
                "storage.free_space".into(),
                "system.load".into(),
                "system.production".into(),
                "memory.swap".into(),
                "memory.allocation".into(),
                "storage.wear".into(),
                "environment.scope".into(),
                "environment.cgroup_cpu".into(),
                "process.privileges".into(),
            ],
        }));

        let preflight = crate::preflight::run(&crate::preflight::PreflightInput {
            inventory: &inventory,
            registry: &self.registry,
            modules: &modules,
            profile: handle.profile,
            params: &params,
            state_dir: &self.state_dir,
            force,
            cycle_target: handle.cycle_target,
        });
        let passed = preflight.passed;
        handle.emit(Event::PreflightCompleted(preflight));

        if !passed {
            self.finalise(&handle, inventory, environment_digest, RunState::Failed)
                .await;
            return;
        }

        // --- telemetry ------------------------------------------------------
        handle.set_state(RunState::Running);
        let telemetry_task = self.spawn_telemetry(
            handle.clone(),
            HostView {
                scope: inventory.platform.scope,
                cgroup_cpu_limit: inventory.platform.cgroup_cpu_limit,
                logical_cpus: inventory.cpu.logical_cpus,
            },
        );

        // --- modules ---------------------------------------------------------
        //
        // One pass for every profile but `endurance`, which repeats the set in
        // cycles until its target elapses. Cycling is the whole mechanism behind
        // the profile: a decline that takes forty minutes to appear cannot be
        // seen by measuring harder for three, and `docs/MARKET-RESEARCH.md` is
        // blunt about the consequence - *"a 3-minute benchmark on a T-series
        // instance measures the credit balance, not the instance."*
        let started = Instant::now();
        let mut cycle: u32 = 0;
        loop {
            self.run_cycle(&handle, &modules, &params, cycle).await;
            cycle += 1;

            let Some(target) = handle.cycle_target else {
                break;
            };
            if handle.is_cancelled() {
                break;
            }
            let elapsed = started.elapsed();
            // Retention needs two points. A single-cycle endurance run is a
            // standard run wearing the wrong name, so the second cycle is run
            // even if the target has already elapsed - and the overshoot is
            // bounded, because a cycle is deliberately short.
            if cycle < MIN_ENDURANCE_CYCLES {
                continue;
            }
            // Past the minimum, the next cycle starts only if there is room for
            // it. Checking `elapsed < target` instead would start a cycle at
            // 59 minutes of a 60-minute target and overshoot by a whole cycle;
            // an operator who asked for an hour of their server should get an
            // hour of it.
            let projected = elapsed + (elapsed / cycle);
            if projected > target {
                break;
            }
        }

        telemetry_task.abort();
        if let Ok(mut current) = handle.current_module.write() {
            *current = None;
        }

        let final_state = if handle.is_cancelled() {
            RunState::Cancelled
        } else {
            RunState::Completed
        };
        self.finalise(&handle, inventory, environment_digest, final_state)
            .await;
    }

    /// Runs the module set once, appending each result tagged with `cycle`.
    async fn run_cycle(
        &self,
        handle: &Arc<RunHandle>,
        modules: &[ModuleId],
        params: &ModuleParams,
        cycle: u32,
    ) {
        let total = modules.len() as u32;
        for (index, module_id) in modules.iter().enumerate() {
            if handle.is_cancelled() {
                break;
            }
            let Some(module) = self.registry.get(module_id) else {
                continue;
            };
            let module_ref = module.manifest().module_ref();
            let lifecycle = |phase: Option<&str>| ModuleLifecycle {
                module: module_ref.clone(),
                index: index as u32,
                total,
                phase: phase.map(str::to_string),
            };

            if let Ok(mut current) = handle.current_module.write() {
                *current = Some(module_id.clone());
            }

            handle.emit(Event::ModuleQueued(lifecycle(None)));
            handle.emit(Event::ModulePreparing(lifecycle(Some("calibration"))));
            handle.emit(Event::ModuleWarmup(lifecycle(Some("warmup"))));
            handle.emit(Event::ModuleStarted(lifecycle(Some("measure"))));

            // Anything the watchdog saw before this module started belongs to
            // the previous one, which has already collected it.
            let _ = handle.take_contention();

            let started_at = chrono::Utc::now();
            let module_start = Instant::now();
            let reporter = RunReporter {
                handle: handle.clone(),
                module: module_id.clone(),
            };
            let module_for_task = module.clone();
            let params_for_task = params.clone();

            // Benchmarks are CPU-bound and must never run on the async
            // runtime's worker threads: they would starve the HTTP server and
            // the telemetry sampler, and the run would appear frozen in the UI.
            let outcome = tokio::task::spawn_blocking(move || {
                module_for_task.run(&params_for_task, &reporter)
            })
            .await;

            let duration_ms = module_start.elapsed().as_secs_f64() * 1000.0;
            let finished_at = chrono::Utc::now();
            // The load ceiling's verdict on the window that just closed. Taken
            // once, so it lands on exactly one result.
            let contention = handle.take_contention();

            match outcome {
                Ok(Ok(mut output)) => {
                    // Appended before the degradation check below, so external
                    // load degrades the result by the same rule every other
                    // measurement-invalidating warning does rather than by a
                    // second, parallel one.
                    output.warnings.extend(contention);
                    // A module that raised any measurement-invalidating warning
                    // is `Degraded`, whatever the warning was. Checking only for
                    // high variance meant a module could promise a downgrade in
                    // its manifest - "rejected as timer-noise dominated", "a
                    // cache-contaminated working set downgrades the result" -
                    // and then not get one.
                    let degraded = output.warnings.iter().any(|w| w.code.degrades_result());
                    let result = ModuleResult {
                        module: module_ref.clone(),
                        status: if degraded {
                            ModuleStatus::Degraded
                        } else {
                            ModuleStatus::Completed
                        },
                        cycle,
                        started_at,
                        finished_at,
                        duration_ms,
                        metrics: output.metrics,
                        warnings: output.warnings,
                        error: None,
                        context: output.context,
                    };
                    if let Ok(mut results) = handle.results.lock() {
                        results.push(result.clone());
                    }
                    handle.emit(Event::ModuleCompleted(Box::new(ModuleCompletedEvent {
                        result,
                    })));
                    self.emit_provisional_score(handle);
                }
                Ok(Err(ModuleError::Cancelled)) => {
                    handle.emit(Event::ModuleCancelled(lifecycle(None)));
                    break;
                }
                Ok(Err(error)) => {
                    handle.emit(Event::ModuleFailed(ModuleFailedEvent {
                        module: module_ref.clone(),
                        error: error.to_string(),
                        fatal: false,
                    }));
                    if let Ok(mut results) = handle.results.lock() {
                        results.push(ModuleResult {
                            module: module_ref,
                            status: ModuleStatus::Failed,
                            cycle,
                            started_at,
                            finished_at,
                            duration_ms,
                            metrics: vec![],
                            // Kept even on a failure: a module that fell over
                            // while something else was using the machine failed
                            // for a reason worth recording.
                            warnings: contention.into_iter().collect(),
                            error: Some(error.to_string()),
                            context: Default::default(),
                        });
                    }
                }
                Err(join_error) => {
                    handle.emit(Event::ModuleFailed(ModuleFailedEvent {
                        module: module_ref,
                        error: format!("module task did not complete: {join_error}"),
                        fatal: true,
                    }));
                }
            }
        }
    }

    fn spawn_telemetry(
        &self,
        handle: Arc<RunHandle>,
        host: HostView,
    ) -> tokio::task::JoinHandle<()> {
        // Recorded before the task starts, not inside it: a short run could
        // otherwise finalise its bundle before the sampler's first tick, and
        // the disclosure would then be missing from exactly the runs most
        // likely to be repeated and compared.
        if let Ok(mut guards) = handle.guards_not_enforced.write() {
            *guards = Watchdog::new(host).disclosures();
        }
        tokio::spawn(async move {
            let mut sampler = TelemetrySampler::new();
            // Discard the first sample: with no previous reading it carries no
            // rate information.
            let _ = tokio::task::block_in_place(|| sampler.sample());
            let mut ticker =
                tokio::time::interval(std::time::Duration::from_millis(TELEMETRY_INTERVAL_MS));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut ticks: u64 = 0;
            let mut watchdog = Watchdog::new(host);

            loop {
                ticker.tick().await;
                if handle.state().is_terminal() {
                    return;
                }
                let snapshot = tokio::task::block_in_place(|| sampler.sample());
                if let Ok(mut series) = handle.telemetry.lock() {
                    series.push(snapshot.clone());
                }

                // --- watchdog -----------------------------------------------
                //
                // The sampler is already awake once a second holding the only
                // live view of the machine, so it is also the watchdog. Every
                // guard below matters because of one fact: this software runs
                // for an hour at full load on a server that belongs to somebody
                // else.
                match watchdog.check(&handle, &snapshot) {
                    Some(WatchdogVerdict::Abort(warning)) => {
                        // Recorded on the handle first, so the reason reaches
                        // the signed bundle rather than living only in the
                        // event log. A run that stops early and cannot say why
                        // is indistinguishable from one the operator cancelled.
                        if let Ok(mut stopped) = handle.stopped_because.write() {
                            *stopped = Some(warning.message.clone());
                        }
                        if let Some(module) =
                            handle.current_module.read().ok().and_then(|m| m.clone())
                        {
                            handle
                                .emit(Event::ModuleWarning(ModuleWarningEvent { module, warning }));
                        }
                        handle.cancel();
                        return;
                    }
                    Some(WatchdogVerdict::Degrade(warning)) => {
                        // The run continues: the numbers are still evidence,
                        // and stopping would discard the only record of what
                        // the machine did while it was contended. The result
                        // stops being *clean*, which the module result carries.
                        if let Some(module) =
                            handle.current_module.read().ok().and_then(|m| m.clone())
                        {
                            handle
                                .emit(Event::ModuleWarning(ModuleWarningEvent { module, warning }));
                        }
                    }
                    None => {}
                }
                let module = handle.current_module.read().ok().and_then(|m| m.clone());
                handle.emit(Event::ModuleTelemetry(TelemetryEvent {
                    module,
                    cpu_busy_pct: snapshot.cpu_busy_pct,
                    cpu_external_busy_pct: snapshot.cpu_external_busy_pct,
                    cpu_steal_pct: snapshot.cpu_steal_pct,
                    cpu_iowait_pct: snapshot.cpu_iowait_pct,
                    load1: snapshot.load1,
                    mem_used_bytes: snapshot.mem_used_bytes,
                    mem_total_bytes: snapshot.mem_total_bytes,
                    swap_used_bytes: snapshot.swap_used_bytes,
                    cpu_freq_mhz: snapshot.cpu_freq_mhz,
                    cpu_temp_c: snapshot.cpu_temp_c,
                    psi_cpu_some_avg10: snapshot.psi_cpu_some_avg10,
                    psi_io_some_avg10: snapshot.psi_io_some_avg10,
                    psi_mem_some_avg10: snapshot.psi_mem_some_avg10,
                    disk_read_bytes_per_s: snapshot.disk_read_bytes_per_s,
                    disk_write_bytes_per_s: snapshot.disk_write_bytes_per_s,
                    net_rx_bytes_per_s: snapshot.net_rx_bytes_per_s,
                    net_tx_bytes_per_s: snapshot.net_tx_bytes_per_s,
                }));

                // A heartbeat every 10 s lets a client distinguish "nothing is
                // happening" from "the connection died".
                ticks += 1;
                if ticks % 10 == 0 {
                    handle.emit(Event::Heartbeat(Heartbeat {
                        state: handle.state(),
                        last_seq: handle.last_seq(),
                    }));
                }
            }
        })
    }

    /// Scores the results accumulated so far, without copying them.
    ///
    /// The guard is released before the caller emits, so the events lock is
    /// never taken while the results lock is held.
    fn score(&self, handle: &RunHandle) -> Option<ScoreCard> {
        let results = handle.results.lock().ok()?;
        Some(self.model.score_run(handle.profile, &results))
    }

    fn emit_provisional_score(&self, handle: &Arc<RunHandle>) {
        if let Some(card) = self.score(handle) {
            handle.emit(Event::ScoreProvisional(score_event(&card, true)));
        }
    }

    async fn finalise(
        &self,
        handle: &Arc<RunHandle>,
        inventory: Inventory,
        environment_digest: String,
        state: RunState,
    ) {
        handle.set_state(RunState::Finalizing);

        let results = handle.results.lock().map(|r| r.clone()).unwrap_or_default();
        let card = self.model.score_run(handle.profile, &results);

        // Summarised under the lock: an endurance run accumulates one snapshot
        // per second for hours, and cloning the series to read it once is the
        // largest avoidable copy in the whole finalisation path. The diagnosis
        // is taken in the same visit for the same reason - it is the only other
        // consumer of the raw series, and the summary it would otherwise have to
        // read has already thrown away the shape it needs.
        let (telemetry_summary, sustained_diagnosis) = handle
            .telemetry
            .lock()
            .map(|t| {
                (
                    TelemetrySummary::from_samples(&t),
                    darcbench_report::diagnose(
                        card.sustained.as_ref().map(|s| s.retention),
                        card.sustained.as_ref().is_some_and(|s| s.declined()),
                        &t,
                    ),
                )
            })
            .unwrap_or_default();

        handle.emit(Event::ScoreFinal(score_event(&card, false)));

        let finished_at = chrono::Utc::now();
        let events_digest = self.events_digest(handle);
        let event_count = handle.last_seq() + 1;

        let mut bundle = Bundle {
            meta: BundleMeta::new(AGENT_VERSION),
            run: RunRecord {
                run_id: handle.id.clone(),
                profile: handle.profile,
                state,
                started_at: handle.created_at,
                finished_at,
                duration_ms: handle.started.elapsed().as_millis() as u64,
                modules: handle.modules.clone(),
                environment_digest,
                events_digest,
                event_count,
                guards_not_enforced: handle
                    .guards_not_enforced
                    .read()
                    .map(|g| g.clone())
                    .unwrap_or_default(),
                stopped_because: handle
                    .stopped_because
                    .read()
                    .ok()
                    .and_then(|reason| reason.clone()),
            },
            environment: inventory,
            modules: results,
            scores: card,
            sustained_diagnosis,
            verdict: darcbench_protocol::Verdict {
                state: ResultState::Local,
                reasons: vec![],
                validator_version: darcbench_report::validate::VALIDATOR_VERSION.to_string(),
            },
            telemetry: telemetry_summary,
            signature: None,
        };

        // Validate, then sign. Signing last means the signature covers the
        // verdict too, so a downgraded verdict cannot be quietly upgraded.
        bundle.verdict = validate_bundle(&bundle, false).verdict;
        if let Err(error) = bundle.sign(&self.key) {
            tracing::warn!(%error, "could not sign the result bundle");
        }

        match self.persist(handle, &bundle) {
            Ok((bytes, digest)) => {
                handle.emit(Event::ReportGenerated(ReportGenerated {
                    formats: vec!["json".into(), "html".into()],
                    bundle_sha256: digest,
                    bytes,
                }));
            }
            Err(error) => {
                tracing::warn!(%error, "could not persist the result bundle");
            }
        }

        let verdict = bundle.verdict.clone();
        let modules_completed = bundle
            .modules
            .iter()
            .filter(|m| matches!(m.status, ModuleStatus::Completed | ModuleStatus::Degraded))
            .count() as u32;
        let modules_failed = bundle
            .modules
            .iter()
            .filter(|m| m.status == ModuleStatus::Failed)
            .count() as u32;

        if let Ok(mut slot) = handle.bundle.write() {
            *slot = Some(bundle);
        }

        handle.emit(Event::RunCompleted(RunCompleted {
            state,
            verdict,
            duration_ms: handle.started.elapsed().as_millis() as u64,
            modules_completed,
            modules_failed,
            final_seq: handle.last_seq() + 1,
        }));

        // The event log is written *after* the terminal event, so the file on
        // disk is the complete stream. `persist` deliberately cannot do this:
        // it runs before `run.completed` exists, and the bundle it writes
        // contains a digest that by definition cannot cover the event
        // announcing that very bundle. See `events_digest` for what the digest
        // does cover.
        if let Err(error) = self.write_event_log(handle) {
            tracing::warn!(%error, "could not write the event log");
        }

        // The terminal state is published last, once every artifact is on
        // disk. Anything watching this run - `darcbench run`, and in Phase 5 an
        // uploader - treats "terminal" as "the results are readable now", so
        // flipping the state before `events.ndjson` exists hands them a run
        // that is finished but whose event log is missing.
        handle.set_state(state);
    }

    /// Writes the complete ordered event stream as NDJSON.
    ///
    /// From the append-only record, never from the replay buffer. The buffer is
    /// bounded and evicts its oldest entries once a run is long enough, so
    /// writing the audit log from it produced a file that silently began
    /// part-way through the run while `run.event_count` went on reporting the
    /// full total. No profile was long enough to reach that until `endurance`.
    fn write_event_log(&self, handle: &RunHandle) -> Result<(), RunError> {
        let dir = StatePath::join(&self.state_dir, &["runs", handle.id.as_str()])
            .map_err(|e| RunError::Internal(e.to_string()))?;
        std::fs::create_dir_all(dir.as_path())?;

        // Already serialised, one event at a time, as they were emitted. This
        // is a single borrow and a single write.
        let record = handle
            .record
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::fs::write(dir.as_path().join("events.ndjson"), &record.ndjson)?;
        Ok(())
    }

    /// SHA-256 over the canonical form of the ordered event stream.
    ///
    /// Covers every event emitted **before** finalisation - that is, everything
    /// up to and including the last `module.completed`. It cannot cover
    /// `report.generated` or `run.completed`, because those announce the bundle
    /// that carries this digest. The complete stream, terminal events included,
    /// is written to `events.ndjson`; `run.final_seq` states how many events
    /// there were in total, so a consumer can tell whether it saw them all.
    fn events_digest(&self, handle: &RunHandle) -> String {
        handle
            .record
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .digest_so_far()
    }

    /// Writes `bundle.json` and `report.html`. The event log is written
    /// separately by [`Self::write_event_log`] after the terminal event.
    fn persist(&self, handle: &RunHandle, bundle: &Bundle) -> Result<(u64, String), RunError> {
        let dir = StatePath::join(&self.state_dir, &["runs", handle.id.as_str()])
            .map_err(|e| RunError::Internal(e.to_string()))?;
        std::fs::create_dir_all(dir.as_path())?;

        let json = serde_json::to_vec_pretty(bundle)
            .map_err(|e| RunError::Internal(format!("bundle serialisation failed: {e}")))?;
        // Written to a temporary file and renamed, never truncated in place.
        // `rename` is atomic within a directory, so a reader - the report
        // endpoint, `verify`, another process reconciling the index - sees
        // either the previous bundle or the new one and never a half-written
        // file, and a crash part-way through cannot leave a run's evidence
        // permanently unparseable.
        let final_path = dir.as_path().join("bundle.json");
        let temporary = dir.as_path().join("bundle.json.partial");
        std::fs::write(&temporary, &json)?;
        std::fs::rename(&temporary, &final_path)?;
        std::fs::write(
            dir.as_path().join("report.html"),
            darcbench_report::html::render(bundle),
        )?;

        // Indexed after the files are on disk, in that order and never the
        // reverse: the index points at bundles, so a row that exists before the
        // file it names would be a dangling reference for as long as the write
        // took. An index write that fails is logged and dropped - the run is
        // complete, and `reconcile_index` will pick it up on the next start.
        // Stamped with the identity of the file that was just renamed into
        // place, so a later reconciliation can tell whether the row still
        // describes it. A row with no identity is one that claims to be
        // current forever.
        if let Err(error) = self.index.record(bundle, &final_path) {
            tracing::warn!(
                %error,
                run = bundle.run.run_id.as_str(),
                "the run was written to disk but could not be indexed; it will be picked up the \
                 next time the index is reconciled"
            );
        }

        let digest = bundle
            .digest()
            .map_err(|e| RunError::Internal(format!("digest failed: {e}")))?;
        Ok((json.len() as u64, digest))
    }

    /// Starts a run and waits for it, with nothing in between.
    ///
    /// `darcbench run` no longer takes this path: it needs the run to be
    /// *observable* while it happens, so it calls `start` and then folds the
    /// event stream itself (see `crate::tui` and `crate::follow`). What is left
    /// is the shape every test wants - start it, wait, assert on the bundle -
    /// so it stays, scoped to tests rather than deleted and re-written inline
    /// in each of them.
    #[cfg(test)]
    pub(crate) async fn run_to_completion(
        self: &Arc<Self>,
        profile: Profile,
        modules: Option<Vec<ModuleId>>,
        force: bool,
        cycle_target: Option<Duration>,
    ) -> Result<Arc<RunHandle>, RunError> {
        let handle = self.start(profile, modules, force, cycle_target)?;
        handle.wait_for_terminal().await;
        Ok(handle)
    }
}

/// Projects the inventory onto the facts modules are allowed to see.
///
/// `darcbench-modules` deliberately does not depend on `darcbench-inventory`:
/// a workload crate that can read `/proc` is a workload crate that can grow a
/// dependency on the machine it is measuring. The agent, which already
/// collected the inventory for the bundle, hands over the few numbers a module
/// needs to size its work - which is how the methodology's requirement that
/// "the cache topology captured in inventory is used to size them" is actually
/// satisfied.
fn machine_facts(inventory: &Inventory, scratch: Option<&StatePath>) -> MachineFacts {
    // The largest reported cache is the last level, whatever it is called: a
    // machine with no L3 must size against its L2 rather than against nothing.
    let last_level_cache_bytes = inventory
        .cpu
        .caches
        .iter()
        .map(|cache| cache.size_bytes)
        .max()
        .filter(|bytes| *bytes > 0);
    let l2_cache_bytes = inventory
        .cpu
        .caches
        .iter()
        .find(|cache| cache.level == "L2")
        .map(|cache| cache.size_bytes)
        .filter(|bytes| *bytes > 0);

    MachineFacts {
        last_level_cache_bytes,
        l2_cache_bytes,
        // `MemAvailable`, not `MemFree`: the point is what a fresh allocation
        // can actually get without pushing the host into reclaim.
        available_bytes: Some(inventory.memory.available_bytes).filter(|bytes| *bytes > 0),
        numa_nodes: inventory.cpu.numa_nodes,
        // Measured on the filesystem that will actually hold the scratch files,
        // which on a real host is often not the one holding `/`. Unknown stays
        // `None` and every consumer treats that as unsafe, never as unlimited.
        free_scratch_bytes: scratch.and_then(|dir| {
            darcbench_inventory::storage::StorageInfo::available_bytes_for_or_ancestor(
                dir.as_path(),
            )
        }),
    }
}

fn score_event(card: &ScoreCard, provisional: bool) -> ScoreEvent {
    ScoreEvent {
        scoring_model: card.scoring_model.clone(),
        provisional,
        total: card.total,
        categories: card
            .categories
            .iter()
            .map(|c| CategoryScore {
                key: c.key.key().to_string(),
                label: c.label.clone(),
                score: c.score,
                weight: c.weight,
            })
            .chain(card.facets.iter().map(|(key, score)| CategoryScore {
                key: key.clone(),
                label: key.replace('_', " "),
                score: *score,
                weight: 0.0,
            }))
            .collect(),
        uncalibrated: card.uncalibrated,
    }
}

#[cfg(test)]
// In tests, `unwrap`/`expect` panicking *is* the failure signal.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic_in_result_fn)]
mod tests {
    use super::*;

    fn manager() -> Arc<RunManager> {
        let dir = std::env::temp_dir().join(format!(
            "darcbench-runner-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let key = Arc::new(AgentKey::generate().expect("keygen"));
        Arc::new(RunManager::new(dir, key))
    }

    /// The cheapest module set that still exercises the whole pipeline.
    ///
    /// Tests about the *orchestrator* - event ordering, artifacts, replay -
    /// do not need every registered module to run, and running them all would
    /// make the suite scale with the module catalogue rather than with what is
    /// under test. `a_full_quick_run_produces_a_signed_bundle` is the one test
    /// that deliberately runs the real profile.
    fn one_module() -> Option<Vec<ModuleId>> {
        Some(vec![ModuleId::new("cpu.mixed").expect("id")])
    }

    /// The profile's module set is asserted without running it.
    ///
    /// Running every registered module here would make the unit suite scale
    /// with the module catalogue, and it does so at debug-build speed:
    /// `memory.bandwidth` sizes its working set from the real machine, so an
    /// unoptimised full quick run took over eight minutes on a four-core host.
    /// `scripts/e2e.sh` runs the real profile end to end against a release
    /// build, which is where a full-profile check belongs - and where the
    /// numbers it produces are meaningful, since a debug bundle is never
    /// comparable anyway.
    #[test]
    fn the_quick_profile_resolves_to_every_implemented_module() {
        let manager = manager();
        let modules: Vec<String> = manager
            .registry()
            .modules_for_profile(Profile::Quick)
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(
            modules,
            vec!["cpu.mixed", "memory.bandwidth", "storage.mixed"],
            "cheapest and least invasive first: compute, then memory, then the module that writes"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_full_quick_run_produces_a_signed_bundle() {
        let manager = manager();
        let handle = manager
            .run_to_completion(Profile::Quick, one_module(), true, None)
            .await
            .expect("run should start");

        assert_eq!(handle.state(), RunState::Completed);

        let bundle = handle.bundle().expect("bundle");
        assert_eq!(bundle.modules.len(), 1);
        assert_eq!(bundle.modules[0].metrics.len(), 10, "cpu.mixed");
        bundle
            .verify_signature()
            .expect("the bundle must be signed and verifiable");

        // Compute score exists; total is not standard because only one
        // category was measured.
        assert!(bundle
            .scores
            .category(darcbench_scoring::CategoryKey::Compute)
            .is_some());
        assert!(!bundle.scores.total_is_standard);
        assert!(
            bundle.scores.uncalibrated,
            "the shipped model is not calibrated"
        );
        assert_eq!(bundle.verdict.state, ResultState::Partial);
        assert!(!bundle.verdict.state.is_rankable());

        let _ = std::fs::remove_dir_all(&manager.state_dir);
    }

    /// The endurance profile is the only one that repeats, and repeating is the
    /// entire mechanism: a decline that takes forty minutes to appear cannot be
    /// seen by measuring harder for three.
    ///
    /// Run with the shortest permitted target against one cheap module, so the
    /// test exercises the cycle loop, the cycle tagging and the retention
    /// computation without spending an hour. The minimum-cycles rule is what
    /// makes that possible: the run completes two cycles even though the target
    /// elapses during the first.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_cycling_run_repeats_its_modules_and_measures_what_it_retained() {
        let manager = manager();
        let handle = manager
            .run_to_completion(
                Profile::Endurance,
                one_module(),
                true,
                Some(Duration::from_secs(60)),
            )
            .await
            .expect("run should start");

        assert_eq!(handle.state(), RunState::Completed);
        let bundle = handle.bundle().expect("bundle");

        assert!(
            bundle.modules.len() >= MIN_ENDURANCE_CYCLES as usize,
            "a cycling run must complete at least {MIN_ENDURANCE_CYCLES} cycles, got {}",
            bundle.modules.len()
        );
        let cycles: Vec<u32> = bundle.modules.iter().map(|m| m.cycle).collect();
        assert_eq!(
            cycles,
            (0..bundle.modules.len() as u32).collect::<Vec<_>>(),
            "cycles must be tagged in order; without the tag the same module id \
             appears repeatedly with no way to compare the passes"
        );

        let sustained = bundle
            .scores
            .sustained
            .as_ref()
            .expect("a cycling run must report what it retained");
        assert_eq!(sustained.cycles, bundle.modules.len());
        assert_eq!(
            sustained.scored_cycle,
            bundle.modules.len() as u32 - 1,
            "the published figures are the sustained ones, not the opening burst"
        );
        assert!(
            sustained.retention > 0.0 && sustained.retention.is_finite(),
            "got {}",
            sustained.retention
        );
        assert!(!sustained.by_metric.is_empty());

        let diagnosis = bundle
            .sustained_diagnosis
            .as_ref()
            .expect("a cycling run must explain its retention, including when nothing went wrong");
        assert!(!diagnosis.explanation.is_empty());

        let _ = std::fs::remove_dir_all(&manager.state_dir);
    }

    /// Every other profile runs once, and must be completely unaffected by the
    /// machinery above.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_non_cycling_run_stays_a_single_pass_with_no_retention_claim() {
        let manager = manager();
        let handle = manager
            .run_to_completion(Profile::Quick, one_module(), true, None)
            .await
            .expect("run should start");

        let bundle = handle.bundle().expect("bundle");
        assert_eq!(bundle.modules.len(), 1);
        assert_eq!(bundle.modules[0].cycle, 0);
        assert!(
            bundle.scores.sustained.is_none(),
            "a run that was never given time to decline has not shown that it would not"
        );
        assert!(bundle.sustained_diagnosis.is_none());
        assert!(bundle.run.stopped_because.is_none());

        let _ = std::fs::remove_dir_all(&manager.state_dir);
    }

    /// The watchdog's runtime ceiling must sit above what was asked for, or an
    /// ordinary cycle overshoot would be killed as a hang.
    #[test]
    fn the_runtime_ceiling_leaves_room_for_an_ordinary_overshoot() {
        let handle = RunHandle::new(
            RunId::try_new().expect("id"),
            Profile::Endurance,
            vec![],
            Some(Duration::from_secs(3600)),
        );
        let ceiling = hard_runtime_ceiling(&handle);
        assert!(
            ceiling > Duration::from_secs(3600),
            "a run may overshoot its last cycle boundary without being killed for it"
        );
        assert!(
            ceiling <= Duration::from_secs(u64::from(ENDURANCE_MAX_MINUTES) * 60 + 3600),
            "and it may never exceed the absolute ceiling"
        );
    }

    /// The thermal guard must not fire on a spike, and must not fire at the
    /// temperature where a healthy machine throttles - throttling is the
    /// measurement, not a fault.
    #[test]
    fn the_thermal_guard_needs_a_sustained_condition_not_a_spike() {
        let handle = RunHandle::new(
            RunId::try_new().expect("id"),
            Profile::Endurance,
            vec![],
            Some(Duration::from_secs(3600)),
        );
        let hot = |c: f64| TelemetrySnapshot {
            cpu_temp_c: Some(c),
            ..Default::default()
        };

        // A machine throttling hard at 95 C is working as designed.
        let mut watchdog = Watchdog::for_scope(Scope::BareMetal);
        for _ in 0..(THERMAL_ABORT_SAMPLES * 4) {
            assert!(
                watchdog.check(&handle, &hot(95.0)).is_none(),
                "throttling is the finding this profile exists to record"
            );
        }

        // A single spike past the threshold is not a condition.
        let mut watchdog = Watchdog::for_scope(Scope::BareMetal);
        assert!(watchdog.check(&handle, &hot(101.0)).is_none());
        assert_eq!(watchdog.consecutive_hot, 1);
        // ...and anything cooler resets the tally rather than accumulating.
        assert!(watchdog.check(&handle, &hot(60.0)).is_none());
        assert_eq!(watchdog.consecutive_hot, 0);

        // Sustained, it stops the run and says why.
        let mut watchdog = Watchdog::for_scope(Scope::BareMetal);
        let mut fired = None;
        for _ in 0..THERMAL_ABORT_SAMPLES {
            fired = watchdog.check(&handle, &hot(101.0));
        }
        let Some(WatchdogVerdict::Abort(warning)) = fired else {
            panic!("a sustained over-temperature must stop the run")
        };
        assert_eq!(warning.code, WarningCode::ThermalThrottle);
        assert!(warning.message.contains("watchdog"));
    }

    /// The load ceiling must ignore the load the benchmark itself creates.
    ///
    /// This is the whole difficulty of the guard: a benchmark saturates the
    /// machine on purpose, so any rule written against total CPU use or the
    /// load average would stop every healthy run on its first sample.
    #[test]
    fn the_load_ceiling_ignores_the_benchmarks_own_load() {
        let handle = watchdog_handle();
        let mut watchdog = Watchdog::for_scope(Scope::BareMetal);
        let saturated = TelemetrySnapshot {
            cpu_busy_pct: 100.0,
            cpu_external_busy_pct: 0.0,
            load1: 64.0,
            ..Default::default()
        };
        for _ in 0..(EXTERNAL_LOAD_ABORT_SAMPLES * 2) {
            assert!(
                watchdog.check(&handle, &saturated).is_none(),
                "a fully loaded machine running only this benchmark is the normal case"
            );
        }
        assert!(handle.take_contention().is_none());
    }

    /// Sustained competition degrades the module in flight; a brief spike does
    /// not.
    #[test]
    fn external_load_degrades_only_when_it_is_sustained() {
        let handle = watchdog_handle();
        let mut watchdog = Watchdog::for_scope(Scope::BareMetal);
        let contended = external(EXTERNAL_LOAD_WARN_PCT + 5.0);

        for _ in 0..(EXTERNAL_LOAD_WARN_SAMPLES - 1) {
            assert!(
                watchdog.check(&handle, &contended).is_none(),
                "a short burst of unrelated work is not a contaminated measurement"
            );
        }
        assert!(
            handle.take_contention().is_none(),
            "nothing may be charged to the module before the condition is met"
        );

        let fired = watchdog.check(&handle, &contended);
        let Some(WatchdogVerdict::Degrade(warning)) = fired else {
            panic!("sustained competition must degrade the measurement")
        };
        assert_eq!(warning.code, WarningCode::ExternalLoad);
        assert!(
            warning.code.degrades_result(),
            "a contaminated window cannot be reported as a clean measurement"
        );

        // Announced once, but recorded on every sample, so a stretch that
        // outlasts one module degrades all of them.
        assert!(handle.take_contention().is_some());
        for _ in 0..10 {
            assert!(
                watchdog.check(&handle, &contended).is_none(),
                "the operator is told once, not once a second"
            );
        }
        assert!(
            handle.take_contention().is_some(),
            "the next module's window is contended too and must be marked"
        );
    }

    /// Enough competition for long enough is a stopping condition, not a
    /// warning: the numbers cannot describe the machine, and the machine is
    /// evidently wanted for something else.
    #[test]
    fn the_load_ceiling_stops_a_run_the_machine_has_been_taken_back_from() {
        let handle = watchdog_handle();
        let mut watchdog = Watchdog::for_scope(Scope::BareMetal);
        let heavy = external(EXTERNAL_LOAD_ABORT_PCT + 10.0);

        let mut aborted = None;
        for _ in 0..EXTERNAL_LOAD_ABORT_SAMPLES {
            if let Some(WatchdogVerdict::Abort(warning)) = watchdog.check(&handle, &heavy) {
                aborted = Some(warning);
            }
        }
        let warning = aborted.expect("sustained heavy competition must stop the run");
        assert_eq!(warning.code, WarningCode::ExternalLoad);
        assert!(warning.message.contains("ceiling"));

        // One sample below the ceiling clears the tally: a backup window is not
        // a machine that has been reassigned.
        let mut watchdog = Watchdog::for_scope(Scope::BareMetal);
        for i in 0..(EXTERNAL_LOAD_ABORT_SAMPLES * 3) {
            let sample = if i % (EXTERNAL_LOAD_ABORT_SAMPLES - 1) == 0 {
                external(0.0)
            } else {
                heavy.clone()
            };
            assert!(
                !matches!(
                    watchdog.check(&handle, &sample),
                    Some(WatchdogVerdict::Abort(_))
                ),
                "the ceiling is a sustained condition, not a running total"
            );
        }
    }

    /// Inside a container `/proc/stat` usually describes the host, so every
    /// other tenant would read as external load. The guard declares itself
    /// absent rather than firing on evidence it cannot trust.
    #[test]
    fn the_load_ceiling_does_not_run_where_its_evidence_is_untrustworthy() {
        let handle = watchdog_handle();
        let mut watchdog = Watchdog::for_scope(Scope::Container);
        let heavy = external(90.0);
        for _ in 0..(EXTERNAL_LOAD_ABORT_SAMPLES * 2) {
            assert!(watchdog.check(&handle, &heavy).is_none());
        }
        assert!(handle.take_contention().is_none());

        // ...and it does run on a VM, where /proc/stat describes the guest the
        // operator actually rented.
        let mut watchdog = Watchdog::for_scope(Scope::VirtualMachine);
        for _ in 0..EXTERNAL_LOAD_WARN_SAMPLES {
            watchdog.check(&handle, &heavy);
        }
        assert!(handle.take_contention().is_some());
    }

    /// A guard that was never armed and a guard that never fired produce the
    /// same bundle unless the absence is written down. It is the difference
    /// between "nothing competed with this run" and "nobody was watching".
    #[test]
    fn a_guard_that_could_not_be_armed_says_so_in_the_bundle() {
        let absent = Watchdog::for_scope(Scope::Container).disclosures();
        // A cgroup quota below the host's CPU count is the same situation
        // reached by evidence rather than by a label, and it is the case scope
        // detection misses: `/sys` is mounted inside a container, so DMI reads
        // fine and the scope comes back `BareMetal`.
        let quota_bound = Watchdog::new(HostView {
            scope: Scope::BareMetal,
            cgroup_cpu_limit: Some(2.0),
            logical_cpus: 64,
        });
        assert_eq!(
            quota_bound.disclosures().len(),
            1,
            "a run entitled to 2 of 64 CPUs cannot tell a co-tenant from a competitor"
        );
        assert!(
            Watchdog::new(HostView {
                scope: Scope::BareMetal,
                cgroup_cpu_limit: Some(64.0),
                logical_cpus: 64,
            })
            .disclosures()
            .is_empty(),
            "a quota equal to the machine confines nothing"
        );
        assert_eq!(absent.len(), 1, "{absent:?}");
        assert!(
            absent[0].contains("load ceiling") && absent[0].contains("container"),
            "the disclosure must name the guard and the reason: {}",
            absent[0]
        );

        for scope in [Scope::BareMetal, Scope::VirtualMachine, Scope::Unknown] {
            assert!(
                Watchdog::for_scope(scope).disclosures().is_empty(),
                "{scope:?} enforces the ceiling and must claim nothing about it"
            );
        }
    }

    fn watchdog_handle() -> Arc<RunHandle> {
        RunHandle::new(
            RunId::try_new().expect("id"),
            Profile::Endurance,
            vec![],
            Some(Duration::from_secs(3600)),
        )
    }

    fn external(pct: f64) -> TelemetrySnapshot {
        TelemetrySnapshot {
            cpu_busy_pct: 100.0,
            cpu_external_busy_pct: pct,
            ..Default::default()
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn event_stream_is_gapless_and_terminates() {
        let manager = manager();
        let handle = manager
            .run_to_completion(Profile::Quick, one_module(), true, None)
            .await
            .expect("run");

        let events = handle.events_since(None).expect("events");
        assert!(events.len() > 20, "only {} events", events.len());
        for (index, event) in events.iter().enumerate() {
            assert_eq!(event.seq, index as u64, "sequence numbers must be gapless");
            assert_eq!(event.protocol, PROTOCOL_VERSION);
            assert_eq!(event.run_id, handle.id);
        }
        assert!(events.first().is_some_and(|e| e.kind() == "run.created"));
        assert!(events.last().is_some_and(|e| e.event.is_stream_terminal()));

        // The ordered lifecycle a client depends on.
        let kinds: Vec<&str> = events.iter().map(|e| e.kind()).collect();
        for expected in [
            "run.created",
            "run.preflight.started",
            "run.preflight.completed",
            "module.started",
            "module.sample",
            "module.completed",
            "score.provisional",
            "score.final",
            "run.completed",
        ] {
            assert!(
                kinds.contains(&expected),
                "missing `{expected}` in {kinds:?}"
            );
        }

        let _ = std::fs::remove_dir_all(&manager.state_dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn only_one_run_may_be_active_at_a_time() {
        let manager = manager();
        let first = manager
            .start(Profile::Quick, None, true, None)
            .expect("first run");
        let second = manager.start(Profile::Quick, None, true, None);
        assert!(matches!(second, Err(RunError::AlreadyRunning(_))));

        first.cancel();
        while !first.state().is_terminal() {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        // Once the first is terminal, a new run is allowed again.
        let third = manager.start(Profile::Quick, None, true, None);
        assert!(third.is_ok());
        if let Ok(handle) = third {
            handle.cancel();
            while !handle.state().is_terminal() {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        }

        let _ = std::fs::remove_dir_all(&manager.state_dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn cancellation_produces_a_terminal_invalid_bundle() {
        let manager = manager();
        let handle = manager
            .start(Profile::Quick, None, true, None)
            .expect("run");

        // Let the run get past preflight and into real work.
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        handle.cancel();

        let deadline = Instant::now() + std::time::Duration::from_secs(30);
        while !handle.state().is_terminal() && Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }

        assert_eq!(handle.state(), RunState::Cancelled);
        let bundle = handle
            .bundle()
            .expect("a cancelled run still produces a bundle");
        assert_eq!(bundle.verdict.state, ResultState::Invalid);
        assert!(bundle
            .verdict
            .reasons
            .contains(&darcbench_protocol::VerdictReason::Interrupted));
        // Cancellation must not corrupt the event stream.
        let events = handle.events_since(None).expect("events");
        assert!(events.last().is_some_and(|e| e.event.is_stream_terminal()));

        let _ = std::fs::remove_dir_all(&manager.state_dir);
    }

    /// A run that reports itself terminal must already have its artifacts on
    /// disk.
    ///
    /// Regression: the terminal state was published before `events.ndjson` was
    /// written, so anything that waits for completion and then reads the run
    /// directory - `darcbench run`, and the Phase 5 uploader - could observe a
    /// finished run with a missing event log.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn artifacts_are_written_under_the_run_directory() {
        let manager = manager();
        let handle = manager
            .run_to_completion(Profile::Quick, one_module(), true, None)
            .await
            .expect("run");

        let dir = manager.state_dir.join("runs").join(handle.id.as_str());
        for name in ["bundle.json", "report.html", "events.ndjson"] {
            let path = dir.join(name);
            assert!(path.exists(), "{} was not written", path.display());
            assert!(std::fs::metadata(&path)
                .map(|m| m.len() > 0)
                .unwrap_or(false));
        }

        // The persisted bundle must verify after being read back from disk.
        let raw = std::fs::read_to_string(dir.join("bundle.json")).expect("read");
        let reloaded: Bundle = serde_json::from_str(&raw).expect("parse");
        reloaded
            .verify_signature()
            .expect("persisted bundle must verify");

        let _ = std::fs::remove_dir_all(&manager.state_dir);
    }

    /// A completed run must be listable without opening its bundle, and the
    /// index must agree with what is on disk.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_completed_run_is_indexed_and_survives_a_new_manager() {
        let manager = manager();
        let handle = manager
            .run_to_completion(Profile::Quick, one_module(), true, None)
            .await
            .expect("run");

        let indexed = manager
            .index()
            .get(handle.id.as_str())
            .expect("index query")
            .expect("the run that just finished must be in the index");
        assert_eq!(indexed.profile, Profile::Quick.as_str());
        assert_eq!(
            indexed.bundle_digest,
            handle.bundle().and_then(|b| b.digest().ok()).unwrap(),
            "the index must name the bundle it indexed"
        );

        // The point of `reconcile`: a second process, with its own index,
        // rebuilds the same history from the bundles alone. This is also the
        // regression the merged run list depends on - a fresh `serve` used to
        // report zero runs next to a directory full of them.
        let second = Arc::new(RunManager::new(
            manager.state_dir.clone(),
            manager.key.clone(),
        ));
        assert!(
            second.list().is_empty(),
            "a new manager has executed no runs of its own"
        );
        second.reconcile_index();
        let rebuilt = second
            .index()
            .list(10)
            .expect("list")
            .into_iter()
            .map(|run| run.run_id)
            .collect::<Vec<_>>();
        assert_eq!(rebuilt, vec![handle.id.to_string()]);

        let _ = std::fs::remove_dir_all(&manager.state_dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn persisted_event_log_contains_the_terminal_event() {
        // Regression: the event log used to be written inside `persist`, which
        // runs before `run.completed` is emitted, so the file on disk was
        // missing the terminal event and a consumer replaying from it could
        // never tell the run had finished.
        let manager = manager();
        let handle = manager
            .run_to_completion(Profile::Quick, one_module(), true, None)
            .await
            .expect("run");
        let path = manager
            .state_dir
            .join("runs")
            .join(handle.id.as_str())
            .join("events.ndjson");

        let raw = std::fs::read_to_string(&path).expect("event log");
        let events: Vec<darcbench_protocol::Envelope> = raw
            .lines()
            .map(|line| serde_json::from_str(line).expect("valid envelope"))
            .collect();

        assert!(!events.is_empty());
        for (index, event) in events.iter().enumerate() {
            assert_eq!(event.seq, index as u64, "persisted log must be gapless");
        }
        let last = events.last().expect("last");
        assert!(
            last.event.is_stream_terminal(),
            "persisted log must end with a terminal event, ended with `{}`",
            last.kind()
        );

        // `final_seq` must account for every event, including itself.
        let in_memory = handle.events_since(None).expect("events");
        assert_eq!(
            events.len(),
            in_memory.len(),
            "disk log must match the in-memory stream"
        );

        let _ = std::fs::remove_dir_all(&manager.state_dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn replay_returns_only_newer_events() {
        let manager = manager();
        let handle = manager
            .run_to_completion(Profile::Quick, one_module(), true, None)
            .await
            .expect("run");
        let all = handle.events_since(None).expect("all");
        let after_five = handle.events_since(Some(5)).expect("subset");
        assert_eq!(after_five.len(), all.len() - 6);
        assert!(after_five.iter().all(|e| e.seq > 5));

        let _ = std::fs::remove_dir_all(&manager.state_dir);
    }

    /// The single-run guard must hold under concurrent starts.
    ///
    /// Regression: the check used a read lock that was released before the
    /// write lock that inserted the handle, so two simultaneous requests could
    /// both see an idle agent and both spawn a CPU-saturating benchmark.
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn concurrent_starts_cannot_both_win_the_single_run_slot() {
        let manager = manager();
        let mut tasks = Vec::new();
        for _ in 0..16 {
            let manager = manager.clone();
            tasks.push(tokio::task::spawn_blocking(move || {
                manager.start(Profile::Quick, None, true, None).is_ok()
            }));
        }

        let mut accepted = 0;
        for task in tasks {
            if task.await.unwrap_or(false) {
                accepted += 1;
            }
        }
        assert_eq!(
            accepted, 1,
            "exactly one concurrent start may be accepted, got {accepted}"
        );
        assert_eq!(
            manager.list().len(),
            1,
            "a rejected start must not leave a run behind"
        );

        if let Some(active) = manager.active() {
            active.cancel();
            while !active.state().is_terminal() {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        }
        let _ = std::fs::remove_dir_all(&manager.state_dir);
    }

    #[test]
    fn unknown_modules_are_rejected_before_a_run_is_created() {
        let manager = manager();
        let bogus = vec![ModuleId::new("nope.nope").expect("id")];
        let result = manager.start(Profile::Custom, Some(bogus), true, None);
        assert!(matches!(result, Err(RunError::UnknownModules(_))));
        assert!(
            manager.list().is_empty(),
            "a rejected request must not create a run"
        );
    }

    /// Subscribing before snapshotting the backlog must leave no gap.
    ///
    /// Regression: the SSE handler took the backlog first and subscribed
    /// second, so an event emitted in between reached neither. Near the end of
    /// a run the lost event is typically `run.completed`, and the dashboard
    /// would sit at "running" forever.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn subscribe_then_backlog_covers_every_event() {
        let handle = RunHandle::new(RunId::try_new().expect("id"), Profile::Quick, vec![], None);

        for _ in 0..5 {
            handle.emit(Event::Heartbeat(Heartbeat {
                state: RunState::Running,
                last_seq: 0,
            }));
        }

        // --- the broken order, to prove the ordering is what matters -------
        // Snapshot first, subscribe second: an event emitted in the window
        // between them lands in neither, and is lost with no way to detect it.
        let stale_backlog = handle.events_since(None).expect("backlog");
        handle.emit(Event::Heartbeat(Heartbeat {
            state: RunState::Running,
            last_seq: 5,
        }));
        let mut late_subscriber = handle.subscribe();

        let mut lost = collect(&stale_backlog, &mut late_subscriber);
        lost.sort_unstable();
        assert_eq!(
            lost,
            (0..5).collect::<Vec<u64>>(),
            "precondition: the backlog-then-subscribe order really does drop seq 5"
        );

        // --- the order the handler actually uses ---------------------------
        let mut live = handle.subscribe();
        let backlog = handle.events_since(None).expect("backlog");

        handle.emit(Event::RunCompleted(RunCompleted {
            state: RunState::Completed,
            verdict: darcbench_protocol::Verdict {
                state: ResultState::Local,
                reasons: vec![],
                validator_version: "test".into(),
            },
            duration_ms: 1,
            modules_completed: 0,
            modules_failed: 0,
            final_seq: 7,
        }));

        let mut seen = collect(&backlog, &mut live);
        seen.sort_unstable();
        assert_eq!(
            seen,
            (0..7).collect::<Vec<u64>>(),
            "backlog plus live stream must cover every sequence number exactly once"
        );
    }

    /// Mirrors the SSE handler's merge: replay the backlog, then take live
    /// events strictly newer than the last replayed one.
    fn collect(
        backlog: &[darcbench_protocol::Envelope],
        live: &mut tokio::sync::broadcast::Receiver<darcbench_protocol::Envelope>,
    ) -> Vec<u64> {
        let highest_replayed = backlog.last().map(|e| e.seq);
        let mut seen: Vec<u64> = backlog.iter().map(|e| e.seq).collect();
        while let Ok(envelope) = live.try_recv() {
            if highest_replayed.is_none_or(|seq| envelope.seq > seq) {
                seen.push(envelope.seq);
            }
        }
        seen
    }

    /// The audit log survives a run longer than the replay buffer.
    ///
    /// Regression, and one the endurance profile is the first thing to reach:
    /// `events.ndjson` and `events_digest` were both derived from the bounded
    /// replay buffer. Once a run emits more than `REPLAY_BUFFER` events the
    /// buffer evicts its prefix, so the log written to disk began part-way
    /// through the run - at a nonzero sequence number - while `event_count`
    /// went on reporting the complete total. A quick run never got close;
    /// an hour of 1 Hz telemetry passes it comfortably.
    #[test]
    fn the_event_log_is_complete_even_past_the_replay_window() {
        let handle = RunHandle::new(
            RunId::try_new().expect("id"),
            Profile::Endurance,
            vec![],
            None,
        );

        let overflow = REPLAY_BUFFER + 500;
        for _ in 0..overflow {
            handle.emit(Event::Heartbeat(Heartbeat {
                state: RunState::Running,
                last_seq: 0,
            }));
        }

        // The replay buffer is *supposed* to have dropped its prefix.
        let replayable = handle.events_since(None).expect("events");
        assert_eq!(replayable.len(), REPLAY_BUFFER);
        assert!(handle.truncated.load(Ordering::Relaxed));

        // The record must not have.
        let record = handle.record.lock().expect("record");
        let lines: Vec<&[u8]> = record
            .ndjson
            .split(|b| *b == b'\n')
            .filter(|l| !l.is_empty())
            .collect();
        assert_eq!(
            lines.len(),
            overflow,
            "the permanent record must hold every event, not the last {REPLAY_BUFFER}"
        );

        let envelopes: Vec<Envelope> = lines
            .iter()
            .map(|line| serde_json::from_slice(line).expect("valid envelope"))
            .collect();
        assert_eq!(
            envelopes.first().map(|e| e.seq),
            Some(0),
            "the log must start at the beginning of the run, not part-way through it"
        );
        for (index, envelope) in envelopes.iter().enumerate() {
            assert_eq!(envelope.seq, index as u64, "the record must be gapless");
        }
        assert_eq!(
            handle.last_seq() + 1,
            envelopes.len() as u64,
            "`event_count` must describe the log that was actually written"
        );
    }

    /// Concurrent emitters must never reorder the stream.
    ///
    /// Regression: the sequence number was allocated with a `fetch_add` outside
    /// the replay-buffer lock, so the module thread and the 1 Hz telemetry task
    /// could interleave between allocating `seq` and appending. The buffer,
    /// `events.ndjson` and the live SSE stream could then all carry a lower
    /// sequence number after a higher one - and because every client folds the
    /// stream by `seq` and ignores anything not newer than what it has, the
    /// reordered event is silently dropped rather than merely displayed oddly.
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn concurrent_emitters_produce_a_totally_ordered_stream() {
        let handle = RunHandle::new(RunId::try_new().expect("id"), Profile::Quick, vec![], None);

        let mut tasks = Vec::new();
        for _ in 0..8 {
            let handle = handle.clone();
            tasks.push(tokio::task::spawn_blocking(move || {
                for _ in 0..128 {
                    handle.emit(Event::Heartbeat(Heartbeat {
                        state: RunState::Running,
                        last_seq: 0,
                    }));
                }
            }));
        }
        for task in tasks {
            let _ = task.await;
        }

        let events = handle.events_since(None).expect("events");
        assert_eq!(events.len(), 8 * 128);
        for (index, event) in events.iter().enumerate() {
            assert_eq!(
                event.seq, index as u64,
                "the replay buffer must be gapless and strictly ascending"
            );
        }
    }

    /// Eviction keeps the buffer ordered, bounded, and honest about the gap.
    #[test]
    fn the_replay_buffer_evicts_oldest_first_and_refuses_a_lost_position() {
        let handle = RunHandle::new(RunId::try_new().expect("id"), Profile::Quick, vec![], None);
        for _ in 0..(REPLAY_BUFFER + 64) {
            handle.emit(Event::Heartbeat(Heartbeat {
                state: RunState::Running,
                last_seq: 0,
            }));
        }

        let events = handle.events_since(None).expect("events");
        assert_eq!(events.len(), REPLAY_BUFFER, "the buffer must stay bounded");
        assert_eq!(
            events.first().map(|e| e.seq),
            Some(64),
            "the oldest events, not the newest, must be the ones dropped"
        );
        assert!(events.windows(2).all(|w| w[0].seq < w[1].seq));

        // A client resuming from a position that has fallen out must be told to
        // refetch rather than handed a gap it cannot detect.
        assert!(
            handle.events_since(Some(10)).is_none(),
            "an evicted resume point must produce `replay_unavailable`"
        );
        // The boundary case: seq 63 is gone, but the client's next needed event
        // (64) is still the oldest one buffered, so the replay is complete.
        assert!(handle.events_since(Some(63)).is_some());

        let tail = handle.events_since(Some(4000)).expect("tail");
        assert!(tail.iter().all(|e| e.seq > 4000));
    }

    /// `Last-Event-ID` is caller-supplied, so the resume arithmetic must not
    /// overflow: `u64::MAX + 1` panics a debug build and wraps in a release one.
    #[test]
    fn a_hostile_last_event_id_cannot_overflow_the_resume_check() {
        let handle = RunHandle::new(RunId::try_new().expect("id"), Profile::Quick, vec![], None);
        for _ in 0..(REPLAY_BUFFER + 1) {
            handle.emit(Event::Heartbeat(Heartbeat {
                state: RunState::Running,
                last_seq: 0,
            }));
        }
        // Nothing after u64::MAX exists, so an empty replay is the honest
        // answer; the point of the test is that it returns at all.
        assert_eq!(
            handle.events_since(Some(u64::MAX)).map(|e| e.len()),
            Some(0)
        );
    }

    /// A run with nothing to run is refused rather than started empty.
    ///
    /// This used to be asserted against `Profile::WebOnly`, which resolved to
    /// no modules while `web.static` did not exist. It does now, so the case is
    /// made with an explicitly empty module list - the only way left to ask for
    /// a run of nothing, and the one an API caller can actually produce.
    #[test]
    fn a_run_with_no_modules_is_rejected() {
        let manager = manager();
        assert!(matches!(
            manager.start(Profile::Custom, Some(vec![]), true, None),
            Err(RunError::NoModules(Profile::Custom))
        ));
    }

    /// The web profile runs the web modules, in the order the registry declares.
    #[test]
    fn the_web_profile_resolves_to_the_web_modules() {
        let manager = manager();
        let modules = manager
            .registry()
            .modules_for_profile(Profile::WebOnly)
            .into_iter()
            .map(|id| id.to_string())
            .collect::<Vec<_>>();
        assert_eq!(
            modules,
            vec![
                "web.static".to_string(),
                "php.runtime".to_string(),
                "node.runtime".to_string()
            ]
        );
        // The runtime modules execute an interpreter the operator installed,
        // and most machines have neither. A standard run must not be downgraded
        // for that; see docs/adr/0013-executing-a-discovered-runtime.md.
        for runtime in ["php.runtime", "node.runtime"] {
            assert!(!manager
                .registry()
                .modules_for_profile(Profile::Standard)
                .iter()
                .any(|id| id.as_str() == runtime));
        }
    }

    /// Only a profile that cycles has a duration to override.
    ///
    /// This is the property the API and CLI guards enforce; pinning it here
    /// means a future profile that starts cycling gets the override for free,
    /// and one that does not cannot be turned into an all-day workload by
    /// attaching a duration to it.
    #[test]
    fn only_endurance_has_a_duration_to_override() {
        for profile in [
            Profile::Quick,
            Profile::Standard,
            Profile::Deep,
            Profile::ReadOnly,
            Profile::WebOnly,
            Profile::Custom,
        ] {
            assert!(
                profile.cycle_target_minutes().is_none(),
                "{profile} does not cycle, so a duration override must be refused for it"
            );
        }
        assert_eq!(
            Profile::Endurance.cycle_target_minutes(),
            Some(darcbench_protocol::ENDURANCE_DEFAULT_MINUTES)
        );
    }
}
