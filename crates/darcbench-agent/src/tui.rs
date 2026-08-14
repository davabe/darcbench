//! The live terminal dashboard for `darcbench run`.
//!
//! Before this existed, `darcbench run` printed a two-line header and then
//! nothing at all until the bundle was ready - several minutes of silence on a
//! `quick` profile and well over an hour on `endurance`. The information was
//! never missing: the run already emits a complete, ordered event stream for
//! the browser dashboard over SSE. Only the terminal was not reading it. This
//! module subscribes to that same stream and folds it into a live view, so the
//! CLI and the web UI are two renderings of one source of truth rather than
//! two implementations of the same idea.
//!
//! # Why the frame rate is low, and why that is not a style choice
//!
//! This process is part of the system under test. `docs/BENCHMARK-METHODOLOGY.md`
//! caps telemetry sampling at 1 Hz for exactly this reason, and
//! `apps/web/src/styles.css` refuses to animate its radar on the same grounds:
//! *"CPU spent on decoration by a page whose whole job is not to disturb the
//! machine under it."* A terminal UI is cheaper than a browser but it is not
//! free, so the render loop here is bounded three ways:
//!
//! 1. Redraws happen on a fixed [`FRAME`] tick, never per event. A `deep`
//!    profile emits thousands of `module.sample` events; drawing on each one
//!    would tie render cost to workload throughput, which is precisely
//!    backwards - the faster the machine, the more the observer would steal.
//! 2. Between ticks, events are folded into plain state. That fold is a few
//!    map writes and a bounded push; it does no formatting and no drawing.
//! 3. Every history buffer is bounded ([`HISTORY`], [`TELEMETRY_HISTORY`],
//!    [`LOG_LINES`]), so cost per frame is constant for a two-minute run and a
//!    twelve-hour one alike.
//!
//! The result is a redraw of one screen of cells about fifteen times a second,
//! which is small next to a workload deliberately saturating every core - but
//! "small" is a claim, so it is also *disclosed* rather than assumed: the
//! dashboard is off unless stdout is a terminal, and `--no-tui` turns it off
//! for anyone who would rather spend nothing at all.
//!
//! # When it is off
//!
//! Never when `--json` is in effect: that output is a single well-formed
//! document on stdout and nothing may interleave with it. Never when stdout is
//! not a terminal, so a redirect or a CI log gets [`crate::follow`]'s plain
//! line-oriented progress instead of escape sequences. Never under `--no-color`
//! or `TERM=dumb`, both of which say the terminal cannot render this.

use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::io::Write;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use darcbench_protocol::metrics::ModuleStatus;
use darcbench_protocol::{Envelope, Event, ModuleId, Profile, RunState};
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{
    self, Event as TermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Cell, Gauge, Paragraph, Row, Table};
use ratatui::{Frame, Terminal};

use crate::runner::RunHandle;

/// Redraw interval. See the module docstring for why this is a budget rather
/// than a preference.
const FRAME: Duration = Duration::from_millis(66);

/// Samples kept per metric for its sparkline.
const HISTORY: usize = 64;
/// Telemetry points kept. At the 1 Hz emission rate this is two minutes.
const TELEMETRY_HISTORY: usize = 120;
/// Log lines retained for the activity pane.
const LOG_LINES: usize = 200;

/// Cells one inline sparkline occupies.
const SPARK_CELLS: usize = 22;

/// The eight block glyphs a sparkline is drawn from, lowest first.
const SPARK_GLYPHS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// Braille spinner frames for the module in flight.
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// The palette is lifted from `apps/web/src/styles.css` on purpose: the same
/// product should not have two visual identities depending on whether you are
/// looking at it through a browser or a terminal. The tokens keep their web
/// names so the two files can be diffed by eye.
mod palette {
    use ratatui::style::Color;

    pub(super) const CYAN: Color = Color::Rgb(0x22, 0xe0, 0xff);
    pub(super) const BLUE: Color = Color::Rgb(0x3b, 0x7d, 0xff);
    pub(super) const VIOLET: Color = Color::Rgb(0x9b, 0x6b, 0xff);
    pub(super) const OK: Color = Color::Rgb(0x3d, 0xdc, 0x97);
    pub(super) const WARN: Color = Color::Rgb(0xff, 0xb5, 0x47);
    pub(super) const BAD: Color = Color::Rgb(0xff, 0x5c, 0x73);
    pub(super) const DIM: Color = Color::Rgb(0x93, 0xa1, 0xbd);
    pub(super) const LINE: Color = Color::Rgb(0x1d, 0x27, 0x40);
    pub(super) const FG: Color = Color::Rgb(0xe8, 0xee, 0xf8);
}

/// Whether a live dashboard should be drawn for this invocation.
///
/// `decorated` is the same signal the rest of the CLI styles with - colour
/// enabled, not `--json`, stdout a terminal - so the dashboard can never appear
/// somewhere plain text would not already have been coloured. `TERM=dumb` is
/// rejected on top of that: it is the terminal telling us it cannot do this.
pub(crate) fn should_render(decorated: bool, no_tui: bool) -> bool {
    if no_tui || !decorated {
        return false;
    }
    if matches!(
        std::env::var("TERM").as_deref(),
        Ok("dumb") | Ok("") | Err(std::env::VarError::NotPresent)
    ) {
        return false;
    }
    // A terminal that will not say how big it is gets plain output. Drawing
    // into an unknown viewport is how the blank screen below happened.
    match ratatui::crossterm::terminal::size() {
        Ok((width, height)) => fits(width, height),
        Err(_) => false,
    }
}

/// Smallest terminal the dashboard is willing to draw into.
///
/// 80x24 is the classic default and roughly what this layout needs: the
/// vertical constraints alone reserve 22 rows before any pane gets a second
/// line of content.
const MIN_WIDTH: u16 = 80;
const MIN_HEIGHT: u16 = 24;

/// Whether a viewport is big enough to be worth drawing into.
///
/// This exists because of a failure that every unit test passed through. The
/// dashboard was driven against a real run on a pty created without a window
/// size - which reports 0x0 - and every `Layout` split resolved to a zero-sized
/// rect. The result was not a cramped dashboard or an error: it was an
/// alternate screen that stayed completely blank for the whole run, which is
/// strictly worse than the silence this module was written to replace, because
/// the operator cannot even see the scrollback. Rendering tests could not catch
/// it because they hand `render` a viewport that is correct by construction.
///
/// Below this, the run falls back to [`crate::follow`]'s plain progress lines.
fn fits(width: u16, height: u16) -> bool {
    width >= MIN_WIDTH && height >= MIN_HEIGHT
}

/// One module's row in the dashboard.
#[derive(Clone, Debug)]
struct ModuleRow {
    id: ModuleId,
    phase: Phase,
    progress: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Queued,
    Preparing,
    Warmup,
    Running,
    Done(ModuleStatus),
}

impl Phase {
    fn label(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Preparing => "preparing",
            Self::Warmup => "warm-up",
            Self::Running => "running",
            Self::Done(ModuleStatus::Completed) => "completed",
            Self::Done(ModuleStatus::Degraded) => "degraded",
            Self::Done(ModuleStatus::Failed) => "failed",
            Self::Done(ModuleStatus::Cancelled) => "cancelled",
            Self::Done(ModuleStatus::Skipped) => "skipped",
        }
    }

    fn colour(self) -> Color {
        match self {
            Self::Queued => palette::DIM,
            Self::Preparing | Self::Warmup => palette::VIOLET,
            Self::Running => palette::CYAN,
            Self::Done(ModuleStatus::Completed) => palette::OK,
            Self::Done(ModuleStatus::Degraded) | Self::Done(ModuleStatus::Skipped) => palette::WARN,
            Self::Done(ModuleStatus::Failed) | Self::Done(ModuleStatus::Cancelled) => palette::BAD,
        }
    }

    fn is_active(self) -> bool {
        matches!(self, Self::Preparing | Self::Warmup | Self::Running)
    }
}

/// A live metric and the tail of its samples.
#[derive(Clone, Debug, Default)]
struct MetricTrack {
    value: f64,
    unit: String,
    rep: u32,
    history: VecDeque<f64>,
    /// Monotonic stamp of the last sample folded into this track.
    ///
    /// The pane is smaller than the metric set - a single module can report
    /// thirteen - so something has to be left out. Ordering by this and showing
    /// the newest means what is on screen is what is currently moving. The
    /// obvious alternative, the `BTreeMap`'s own key order, is alphabetical:
    /// stable, but it would pin the same arbitrary handful in view for the
    /// whole run and hide the metric actually being measured.
    updated: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Severity {
    Info,
    Warning,
    Error,
}

/// Everything the dashboard draws, folded from the event stream.
///
/// This is deliberately a plain fold with no rendering in it, mirroring
/// `apps/web/src/useRunStream.ts`. Keeping the fold pure is what lets the
/// render tick be independent of event arrival rate.
struct View {
    profile: Profile,
    modules: Vec<ModuleRow>,
    metrics: BTreeMap<String, MetricTrack>,
    telemetry: VecDeque<TelemetryPoint>,
    log: VecDeque<(Severity, String)>,
    categories: Vec<(String, f64)>,
    /// Eased values chasing `categories`, so a score arriving in one event
    /// counts up rather than snapping. Purely presentational.
    eased: BTreeMap<String, f64>,
    total: Option<f64>,
    uncalibrated: bool,
    scores_final: bool,
    risk: Option<String>,
    state: RunState,
    started: Instant,
    cancelling: bool,
    frame: u64,
    /// Highest sequence number folded, so the backlog and the live stream can
    /// overlap without double-counting.
    last_seq: Option<u64>,
    /// Counts folded samples, so `MetricTrack::updated` can order by recency.
    metric_clock: u64,
}

#[derive(Clone, Copy, Debug, Default)]
struct TelemetryPoint {
    cpu_busy: f64,
    cpu_external: f64,
    cpu_steal: f64,
    mem_used: u64,
    mem_total: u64,
    load1: f64,
    freq_mhz: Option<f64>,
    temp_c: Option<f64>,
}

impl View {
    fn new(profile: Profile) -> Self {
        Self {
            profile,
            modules: Vec::new(),
            metrics: BTreeMap::new(),
            telemetry: VecDeque::new(),
            log: VecDeque::new(),
            categories: Vec::new(),
            eased: BTreeMap::new(),
            total: None,
            uncalibrated: true,
            scores_final: false,
            risk: None,
            state: RunState::Created,
            started: Instant::now(),
            cancelling: false,
            frame: 0,
            last_seq: None,
            metric_clock: 0,
        }
    }

    fn note(&mut self, severity: Severity, text: String) {
        if self.log.len() >= LOG_LINES {
            self.log.pop_front();
        }
        self.log.push_back((severity, text));
    }

    fn row_mut(&mut self, id: &ModuleId) -> Option<&mut ModuleRow> {
        self.modules.iter_mut().find(|row| &row.id == id)
    }

    /// Folds one envelope. Out-of-order and replayed envelopes are ignored by
    /// sequence number, which is the same rule the browser client applies.
    fn fold(&mut self, envelope: &Envelope) {
        if let Some(seen) = self.last_seq {
            if envelope.seq <= seen {
                return;
            }
        }
        self.last_seq = Some(envelope.seq);

        match &envelope.event {
            Event::RunCreated(created) => {
                self.profile = created.profile;
                self.modules = created
                    .modules
                    .iter()
                    .map(|module| ModuleRow {
                        id: module.id.clone(),
                        phase: Phase::Queued,
                        progress: 0.0,
                    })
                    .collect();
                self.state = RunState::Preflight;
            }
            Event::PreflightCompleted(preflight) => {
                self.risk = Some(format!("{:?}", preflight.risk));
                self.state = if preflight.passed {
                    RunState::Running
                } else {
                    RunState::Failed
                };
                for finding in &preflight
                    .findings
                    .iter()
                    .filter(|f| f.blocking)
                    .collect::<Vec<_>>()
                {
                    self.note(
                        Severity::Error,
                        format!("{}: {}", finding.check, finding.message),
                    );
                }
            }
            Event::ModuleQueued(life) => {
                if let Some(row) = self.row_mut(&life.module.id) {
                    row.phase = Phase::Queued;
                }
            }
            Event::ModulePreparing(life) => {
                if let Some(row) = self.row_mut(&life.module.id) {
                    row.phase = Phase::Preparing;
                }
            }
            Event::ModuleWarmup(life) => {
                if let Some(row) = self.row_mut(&life.module.id) {
                    row.phase = Phase::Warmup;
                }
            }
            Event::ModuleStarted(life) => {
                if let Some(row) = self.row_mut(&life.module.id) {
                    row.phase = Phase::Running;
                }
                self.state = RunState::Running;
            }
            Event::ModuleSample(sample) => {
                if let Some(row) = self.row_mut(&sample.module) {
                    row.progress = sample.module_progress.clamp(0.0, 1.0);
                    if row.phase == Phase::Preparing {
                        row.phase = if sample.warmup {
                            Phase::Warmup
                        } else {
                            Phase::Running
                        };
                    }
                }
                // Warm-up samples move the progress bar - they are real work and
                // hiding them makes the bar stall - but they are never charted,
                // because they are explicitly excluded from the result.
                if sample.warmup {
                    return;
                }
                self.metric_clock = self.metric_clock.wrapping_add(1);
                let stamp = self.metric_clock;
                let track = self.metrics.entry(sample.metric_key.clone()).or_default();
                track.value = sample.value;
                track.unit.clone_from(&sample.unit);
                track.rep = sample.rep;
                track.updated = stamp;
                if track.history.len() >= HISTORY {
                    track.history.pop_front();
                }
                track.history.push_back(sample.value);
            }
            Event::ModuleTelemetry(telemetry) => {
                if self.telemetry.len() >= TELEMETRY_HISTORY {
                    self.telemetry.pop_front();
                }
                self.telemetry.push_back(TelemetryPoint {
                    cpu_busy: telemetry.cpu_busy_pct,
                    cpu_external: telemetry.cpu_external_busy_pct,
                    cpu_steal: telemetry.cpu_steal_pct,
                    mem_used: telemetry.mem_used_bytes,
                    mem_total: telemetry.mem_total_bytes,
                    load1: telemetry.load1,
                    freq_mhz: telemetry.cpu_freq_mhz,
                    temp_c: telemetry.cpu_temp_c,
                });
            }
            Event::ModuleWarning(warning) => {
                self.note(
                    Severity::Warning,
                    format!("{}: {}", warning.module, warning.warning.message),
                );
            }
            Event::ModuleCompleted(completed) => {
                let result = &completed.result;
                if let Some(row) = self.row_mut(&result.module.id) {
                    row.phase = Phase::Done(result.status);
                    row.progress = 1.0;
                }
                self.note(
                    match result.status {
                        ModuleStatus::Completed => Severity::Info,
                        ModuleStatus::Failed | ModuleStatus::Cancelled => Severity::Error,
                        _ => Severity::Warning,
                    },
                    format!("{} {:?}", result.module.id, result.status),
                );
            }
            Event::ModuleFailed(failed) => {
                if let Some(row) = self.row_mut(&failed.module.id) {
                    row.phase = Phase::Done(ModuleStatus::Failed);
                }
                self.note(
                    Severity::Error,
                    format!("{}: {}", failed.module.id, failed.error),
                );
            }
            Event::ModuleCancelled(life) => {
                if let Some(row) = self.row_mut(&life.module.id) {
                    row.phase = Phase::Done(ModuleStatus::Cancelled);
                }
            }
            Event::ScoreProvisional(score) | Event::ScoreFinal(score) => {
                self.categories = score
                    .categories
                    .iter()
                    .map(|category| (category.label.clone(), category.score))
                    .collect();
                self.total = score.total;
                self.uncalibrated = score.uncalibrated;
                self.scores_final = !score.provisional;
            }
            Event::RunCompleted(completed) => {
                self.state = completed.state;
            }
            Event::RunInvalidated(_) => {
                self.state = RunState::Failed;
            }
            _ => {}
        }
    }

    /// Advances the presentational easing by one frame.
    ///
    /// A first-order lag rather than a timed tween: it needs no per-value start
    /// time, it is stable if a score changes mid-animation, and it settles
    /// visually within about half a second at [`FRAME`].
    fn tick(&mut self) {
        self.frame = self.frame.wrapping_add(1);
        for (label, target) in &self.categories {
            let current = self.eased.entry(label.clone()).or_insert(0.0);
            *current += (target - *current) * 0.18;
        }
    }

    fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    fn overall_progress(&self) -> f64 {
        if self.modules.is_empty() {
            return 0.0;
        }
        let sum: f64 = self.modules.iter().map(|row| row.progress).sum();
        (sum / self.modules.len() as f64).clamp(0.0, 1.0)
    }
}

/// Renders `values` as block glyphs, most recent on the right.
///
/// The scale is per-call rather than global: these lines sit beside their own
/// printed value and exist to show *shape* - is it steady, is it decaying - not
/// magnitude, which the number already gives exactly.
fn sparkline(values: &VecDeque<f64>, cells: usize) -> String {
    if values.is_empty() {
        return " ".repeat(cells);
    }
    let tail: Vec<f64> = values
        .iter()
        .rev()
        .take(cells)
        .rev()
        .copied()
        .filter(|v| v.is_finite())
        .collect();
    if tail.is_empty() {
        return " ".repeat(cells);
    }
    let min = tail.iter().copied().fold(f64::INFINITY, f64::min);
    let max = tail.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let span = max - min;
    let mut out = String::with_capacity(cells);
    for _ in tail.len()..cells {
        out.push(' ');
    }
    for value in &tail {
        // A flat series is drawn mid-height rather than at the floor: a run of
        // identical values is steady, not zero, and drawing it as an empty
        // trough reads as a stall.
        let level = if span <= f64::EPSILON {
            3
        } else {
            (((value - min) / span) * 7.0).round().clamp(0.0, 7.0) as usize
        };
        out.push(SPARK_GLYPHS[level.min(SPARK_GLYPHS.len() - 1)]);
    }
    out
}

fn format_elapsed(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    let (hours, minutes, seconds) = (seconds / 3600, (seconds % 3600) / 60, seconds % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes:02}:{seconds:02}")
    }
}

/// Formats a measured value at a readable width.
///
/// Benchmark metrics span from sub-millisecond latencies to gigabytes per
/// second in the same table, so a fixed precision is wrong at one end or the
/// other.
fn format_value(value: f64) -> String {
    let magnitude = value.abs();
    if magnitude >= 100_000.0 {
        format!("{value:.0}")
    } else if magnitude >= 1000.0 {
        format!("{value:.1}")
    } else if magnitude >= 1.0 {
        format!("{value:.2}")
    } else {
        format!("{value:.4}")
    }
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Guards the terminal mode so it is restored on every exit path.
///
/// The release profile sets `panic = "abort"`, so there is no unwinding and a
/// `Drop` guard alone would not survive a panic. The panic hook installed
/// alongside it is therefore not belt-and-braces: it is the only thing standing
/// between a bug in here and a terminal left in raw mode with no echo, which an
/// operator has to blindly type `reset` into.
struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> Result<Self> {
        enable_raw_mode()?;
        let mut stdout = std::io::stdout();
        execute!(stdout, EnterAlternateScreen)?;

        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore();
            previous(info);
        }));

        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore();
    }
}

/// Puts the terminal back. Every failure here is ignored on purpose: this runs
/// on the way out, including out of a panic, and there is nowhere left to
/// report to. Doing as much of it as possible beats stopping at the first
/// error and leaving the rest undone.
fn restore() {
    let mut stdout = std::io::stdout();
    let _ = execute!(stdout, LeaveAlternateScreen);
    let _ = disable_raw_mode();
    let _ = stdout.flush();
}

/// What ended the dashboard.
pub(crate) enum Outcome {
    /// The run reached a terminal state on its own.
    Finished,
    /// The operator asked to stop; the run was told to cancel.
    Cancelled,
}

/// Drives the dashboard until the run finishes or the operator cancels.
pub(crate) async fn drive(handle: Arc<RunHandle>) -> Result<Outcome> {
    // Subscribe *before* taking the backlog. The reverse order has a window
    // between the snapshot and the subscription in which an event is delivered
    // to nobody, and `runner.rs` carries a regression test
    // (`subscribe_then_backlog_covers_every_event`) for exactly that bug in the
    // SSE handler. Overlap is harmless because the fold discards by sequence
    // number; a gap is not recoverable.
    let mut events = handle.subscribe();
    let backlog = handle.events_since(None).unwrap_or_default();

    let mut view = View::new(handle.profile);
    for envelope in &backlog {
        view.fold(envelope);
    }

    let guard = TerminalGuard::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(std::io::stdout()))?;
    terminal.clear()?;

    let mut keys = key_reader();
    let mut ticker = tokio::time::interval(FRAME);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // The tick drives everything; events are drained into the fold inside it.
    //
    // The obvious shape - selecting on `events.recv()` and the tick as peers -
    // does not work here, and biasing it towards events makes it worse. A
    // module under measurement emits samples continuously, so the receive
    // branch is ready almost every time the loop polls, and the tick branch is
    // the only one that draws. The dashboard would stop redrawing exactly when
    // the run got busy, which is when somebody is watching it. Draining with
    // `try_recv` on the tick instead bounds the work per frame to one frame's
    // worth of events and makes redraw rate independent of event rate, which is
    // what the module docstring promises.
    let outcome = loop {
        tokio::select! {
            Some(key) = keys.recv() => {
                if is_quit(&key) && !view.cancelling {
                    view.cancelling = true;
                    view.note(
                        Severity::Warning,
                        "cancelling - finishing the module in flight".into(),
                    );
                    handle.cancel();
                }
            }

            _ = ticker.tick() => {
                let stream = drain(&mut events, &handle, &mut view);
                view.tick();

                if view.state.is_terminal() || handle.state().is_terminal() {
                    if !view.state.is_terminal() {
                        view.state = handle.state();
                    }
                    // Draw the settled state before leaving, so the last thing
                    // on screen is the finished run and not the frame before it.
                    terminal.draw(|frame| render(frame, &view))?;
                    break if view.cancelling {
                        Outcome::Cancelled
                    } else {
                        Outcome::Finished
                    };
                }

                if stream == Stream::Closed {
                    terminal.draw(|frame| render(frame, &view))?;
                    break Outcome::Finished;
                }

                terminal.draw(|frame| render(frame, &view))?;
            }
        }
    };

    drop(terminal);
    drop(guard);
    Ok(outcome)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stream {
    Open,
    Closed,
}

/// Folds every event available right now, without waiting for more.
///
/// Returns once the channel is empty, so the caller can draw one frame that
/// reflects everything that has arrived rather than one event out of a burst.
fn drain(
    events: &mut tokio::sync::broadcast::Receiver<Envelope>,
    handle: &RunHandle,
    view: &mut View,
) -> Stream {
    use tokio::sync::broadcast::error::TryRecvError;

    loop {
        match events.try_recv() {
            Ok(envelope) => view.fold(&envelope),
            Err(TryRecvError::Empty) => return Stream::Open,
            // Lagged means events were dropped between this receiver and the
            // sender, and they are not coming back. The replay buffer is the
            // only complete source left; re-folding from it is idempotent
            // because `View::fold` discards by sequence number. Carrying on
            // without it would leave a permanently wrong view.
            Err(TryRecvError::Lagged(_)) => {
                for envelope in handle.events_since(view.last_seq).unwrap_or_default() {
                    view.fold(&envelope);
                }
            }
            Err(TryRecvError::Closed) => return Stream::Closed,
        }
    }
}

/// True for the keys that mean "stop": `q`, `Esc`, `Ctrl-C`.
///
/// `Ctrl-C` is handled here rather than left to the signal handler because raw
/// mode means the terminal no longer generates SIGINT for it - the key arrives
/// as an ordinary key event, and an operator pressing it and seeing nothing
/// happen would reach for `kill`, which skips the cleanup entirely.
fn is_quit(key: &KeyEvent) -> bool {
    if key.kind != KeyEventKind::Press {
        return false;
    }
    matches!(key.code, KeyCode::Char('q') | KeyCode::Esc)
        || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
}

/// Reads key events on a dedicated thread.
///
/// `event::read` blocks, so it cannot run on a runtime worker. The thread ends
/// when the receiver is dropped: the send fails and the loop breaks, which
/// happens within one poll interval of the dashboard exiting.
fn key_reader() -> tokio::sync::mpsc::UnboundedReceiver<KeyEvent> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    std::thread::spawn(move || loop {
        match event::poll(Duration::from_millis(100)) {
            Ok(true) => match event::read() {
                Ok(TermEvent::Key(key)) => {
                    if tx.send(key).is_err() {
                        break;
                    }
                }
                Ok(_) => {}
                Err(_) => break,
            },
            Ok(false) => {
                if tx.is_closed() {
                    break;
                }
            }
            Err(_) => break,
        }
    });
    rx
}

// --- rendering -------------------------------------------------------------

fn render(frame: &mut Frame<'_>, view: &View) {
    let area = frame.area();
    let layout = Layout::vertical([
        Constraint::Length(3), // header
        Constraint::Length(3), // overall progress
        Constraint::Min(6),    // modules + telemetry
        Constraint::Min(5),    // metrics
        Constraint::Length(4), // scores
        Constraint::Length(1), // footer
    ])
    .split(area);

    render_header(frame, layout[0], view);
    render_progress(frame, layout[1], view);

    let middle = Layout::horizontal([Constraint::Percentage(52), Constraint::Percentage(48)])
        .split(layout[2]);
    render_modules(frame, middle[0], view);
    render_telemetry(frame, middle[1], view);

    render_metrics(frame, layout[3], view);
    render_scores(frame, layout[4], view);
    render_footer(frame, layout[5], view);
}

fn panel(title: &str) -> Block<'_> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(palette::LINE))
        .title(Span::styled(
            format!(" {title} "),
            Style::default()
                .fg(palette::DIM)
                .add_modifier(Modifier::BOLD),
        ))
}

fn render_header(frame: &mut Frame<'_>, area: Rect, view: &View) {
    let spinner = if view.state.is_terminal() {
        "●"
    } else {
        SPINNER[(view.frame as usize / 3) % SPINNER.len()]
    };

    let state_colour = match view.state {
        RunState::Completed => palette::OK,
        RunState::Failed => palette::BAD,
        RunState::Cancelled => palette::WARN,
        _ => palette::CYAN,
    };

    let mut spans = vec![
        Span::styled(
            "DARC",
            Style::default()
                .fg(palette::CYAN)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("//", Style::default().fg(palette::VIOLET)),
        Span::styled(
            "BENCH",
            Style::default()
                .fg(palette::FG)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("   "),
        Span::styled(spinner, Style::default().fg(state_colour)),
        Span::raw(" "),
        Span::styled(
            format!("{:?}", view.state).to_lowercase(),
            Style::default().fg(state_colour),
        ),
        Span::raw("   "),
        Span::styled("profile ", Style::default().fg(palette::DIM)),
        Span::styled(view.profile.to_string(), Style::default().fg(palette::FG)),
    ];

    if let Some(risk) = &view.risk {
        spans.push(Span::raw("   "));
        spans.push(Span::styled("risk ", Style::default().fg(palette::DIM)));
        spans.push(Span::styled(
            risk.to_lowercase(),
            Style::default().fg(palette::WARN),
        ));
    }

    spans.push(Span::raw("   "));
    spans.push(Span::styled(
        format_elapsed(view.elapsed()),
        Style::default()
            .fg(palette::FG)
            .add_modifier(Modifier::BOLD),
    ));

    frame.render_widget(
        Paragraph::new(Line::from(spans)).block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(palette::LINE)),
        ),
        area,
    );
}

fn render_progress(frame: &mut Frame<'_>, area: Rect, view: &View) {
    let ratio = view.overall_progress();
    let done = view
        .modules
        .iter()
        .filter(|row| matches!(row.phase, Phase::Done(_)))
        .count();

    // The bar's colour sweeps while work is in flight and settles when it is
    // not, so a stalled run is visibly different from a slow one without
    // needing a number to be read.
    let colour = if view.state.is_terminal() {
        palette::OK
    } else {
        let phase = (view.frame / 6) % 3;
        [palette::CYAN, palette::BLUE, palette::VIOLET][phase as usize]
    };

    frame.render_widget(
        Gauge::default()
            .block(panel(&format!(
                "progress  {done}/{} modules",
                view.modules.len()
            )))
            .gauge_style(Style::default().fg(colour))
            .ratio(ratio.clamp(0.0, 1.0))
            .label(Span::styled(
                format!("{:.0}%", ratio * 100.0),
                Style::default()
                    .fg(palette::FG)
                    .add_modifier(Modifier::BOLD),
            )),
        area,
    );
}

fn render_modules(frame: &mut Frame<'_>, area: Rect, view: &View) {
    let width = area.width.saturating_sub(4) as usize;
    let bar_cells = width.saturating_sub(30).clamp(6, 24);

    let rows: Vec<Row<'_>> = view
        .modules
        .iter()
        .map(|module| {
            let marker = if module.phase.is_active() {
                SPINNER[(view.frame as usize / 3) % SPINNER.len()]
            } else if matches!(module.phase, Phase::Done(_)) {
                "✓"
            } else {
                "·"
            };

            let filled = (module.progress * bar_cells as f64).round() as usize;
            let filled = filled.min(bar_cells);
            let bar: String = "█".repeat(filled) + &"░".repeat(bar_cells - filled);

            Row::new(vec![
                Cell::from(Span::styled(
                    marker.to_string(),
                    Style::default().fg(module.phase.colour()),
                )),
                Cell::from(Span::styled(
                    module.id.to_string(),
                    Style::default().fg(if module.phase.is_active() {
                        palette::FG
                    } else {
                        palette::DIM
                    }),
                )),
                Cell::from(Span::styled(
                    bar,
                    Style::default().fg(module.phase.colour()),
                )),
                Cell::from(Span::styled(
                    module.phase.label().to_string(),
                    Style::default().fg(module.phase.colour()),
                )),
            ])
        })
        .collect();

    frame.render_widget(
        Table::new(
            rows,
            [
                Constraint::Length(1),
                Constraint::Min(14),
                Constraint::Length(bar_cells as u16),
                Constraint::Length(10),
            ],
        )
        .column_spacing(1)
        .block(panel("modules")),
        area,
    );
}

fn render_telemetry(frame: &mut Frame<'_>, area: Rect, view: &View) {
    let Some(latest) = view.telemetry.back().copied() else {
        frame.render_widget(
            Paragraph::new(Span::styled(
                "  waiting for the first sample…",
                Style::default().fg(palette::DIM),
            ))
            .block(panel("host telemetry")),
            area,
        );
        return;
    };

    let cells = (area.width as usize)
        .saturating_sub(28)
        .clamp(6, SPARK_CELLS);
    let series = |project: fn(&TelemetryPoint) -> f64| -> VecDeque<f64> {
        view.telemetry.iter().map(project).collect()
    };

    let mem_pct = if latest.mem_total > 0 {
        (latest.mem_used as f64 / latest.mem_total as f64) * 100.0
    } else {
        0.0
    };

    // External CPU is the one line here that can invalidate the run, so it is
    // coloured by threshold rather than by category: anything sustained above a
    // few percent is competition for the measurement, which is why the watchdog
    // reads this field. See `TelemetryEvent::cpu_external_busy_pct`.
    let external_colour = if latest.cpu_external > 5.0 {
        palette::BAD
    } else if latest.cpu_external > 1.0 {
        palette::WARN
    } else {
        palette::OK
    };
    let steal_colour = if latest.cpu_steal > 2.0 {
        palette::BAD
    } else {
        palette::DIM
    };

    let mut lines = vec![
        telemetry_line(
            "cpu",
            &sparkline(&series(|p| p.cpu_busy), cells),
            &format!("{:>5.1}%", latest.cpu_busy),
            palette::CYAN,
        ),
        telemetry_line(
            "external",
            &sparkline(&series(|p| p.cpu_external), cells),
            &format!("{:>5.1}%", latest.cpu_external),
            external_colour,
        ),
        telemetry_line(
            "steal",
            &sparkline(&series(|p| p.cpu_steal), cells),
            &format!("{:>5.1}%", latest.cpu_steal),
            steal_colour,
        ),
        telemetry_line(
            "memory",
            &sparkline(&series(|p| p.mem_used as f64), cells),
            &format!("{:>5.1}%", mem_pct),
            palette::BLUE,
        ),
        telemetry_line(
            "load1",
            &sparkline(&series(|p| p.load1), cells),
            &format!("{:>6.2}", latest.load1),
            palette::VIOLET,
        ),
    ];

    let mut trailer = vec![
        Span::styled("  mem ", Style::default().fg(palette::DIM)),
        Span::styled(
            format!(
                "{} / {}",
                format_bytes(latest.mem_used),
                format_bytes(latest.mem_total)
            ),
            Style::default().fg(palette::FG),
        ),
    ];
    if let Some(freq) = latest.freq_mhz {
        trailer.push(Span::styled("   freq ", Style::default().fg(palette::DIM)));
        trailer.push(Span::styled(
            format!("{freq:.0} MHz"),
            Style::default().fg(palette::FG),
        ));
    }
    if let Some(temp) = latest.temp_c {
        trailer.push(Span::styled("   temp ", Style::default().fg(palette::DIM)));
        trailer.push(Span::styled(
            format!("{temp:.0}°C"),
            Style::default().fg(if temp > 85.0 {
                palette::BAD
            } else {
                palette::FG
            }),
        ));
    }
    lines.push(Line::from(trailer));

    // Wrapped, because the trailer carries however many of memory, frequency
    // and temperature this host actually exposes. Unwrapped it clipped
    // mid-word at the panel edge on a narrow terminal, which reads as a
    // rendering bug rather than as a line that ran out of room.
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(ratatui::widgets::Wrap { trim: false })
            .block(panel("host telemetry")),
        area,
    );
}

fn telemetry_line<'a>(label: &'a str, spark: &str, value: &str, colour: Color) -> Line<'a> {
    Line::from(vec![
        Span::styled(format!("  {label:<9}"), Style::default().fg(palette::DIM)),
        Span::styled(spark.to_string(), Style::default().fg(colour)),
        Span::raw(" "),
        Span::styled(
            value.to_string(),
            Style::default()
                .fg(palette::FG)
                .add_modifier(Modifier::BOLD),
        ),
    ])
}

fn render_metrics(frame: &mut Frame<'_>, area: Rect, view: &View) {
    if view.metrics.is_empty() {
        frame.render_widget(
            Paragraph::new(Span::styled(
                "  no measured samples yet - warm-up is not charted",
                Style::default().fg(palette::DIM),
            ))
            .block(panel("live metrics")),
            area,
        );
        return;
    }

    let capacity = (area.height as usize).saturating_sub(2);
    // Borders, the four fixed columns and the four inter-column gaps. Drawing a
    // fixed-width sparkline into a flexible column would either clip it or
    // leave a gap, so the glyph count is derived from the space it actually got.
    let spark_cells = (area.width as usize)
        .saturating_sub(2 + 30 + 12 + 8 + 8 + 4)
        .clamp(6, HISTORY);

    // Newest-first, then alphabetical as the tie-break so the order is stable
    // across frames while nothing is arriving - a table that reshuffled itself
    // every tick would be unreadable.
    let mut ordered: Vec<(&String, &MetricTrack)> = view.metrics.iter().collect();
    ordered.sort_by(|(left_key, left), (right_key, right)| {
        right
            .updated
            .cmp(&left.updated)
            .then_with(|| left_key.cmp(right_key))
    });

    let rows: Vec<Row<'_>> = ordered
        .into_iter()
        .take(capacity)
        .map(|(key, track)| {
            Row::new(vec![
                Cell::from(Span::styled(key.clone(), Style::default().fg(palette::FG))),
                Cell::from(Span::styled(
                    format_value(track.value),
                    Style::default()
                        .fg(palette::CYAN)
                        .add_modifier(Modifier::BOLD),
                )),
                Cell::from(Span::styled(
                    track.unit.clone(),
                    Style::default().fg(palette::DIM),
                )),
                Cell::from(Span::styled(
                    sparkline(&track.history, spark_cells),
                    Style::default().fg(palette::BLUE),
                )),
                Cell::from(Span::styled(
                    format!("rep {}", track.rep),
                    Style::default().fg(palette::DIM),
                )),
            ])
        })
        .collect();

    frame.render_widget(
        Table::new(
            rows,
            // The slack goes to the sparkline, not to the key. `Min` is the
            // column that grows, and putting it on the key opened a gutter of
            // whitespace between a metric and its own value on a wide terminal
            // while the chart stayed stubbornly narrow.
            [
                Constraint::Length(30),
                Constraint::Length(12),
                Constraint::Length(8),
                Constraint::Min(12),
                Constraint::Length(8),
            ],
        )
        .column_spacing(1)
        .block(panel(&format!("live metrics  ({})", view.metrics.len()))),
        area,
    );
}

fn render_scores(frame: &mut Frame<'_>, area: Rect, view: &View) {
    if view.categories.is_empty() {
        let waiting = if view.uncalibrated {
            "  scores appear as modules complete - this model is uncalibrated"
        } else {
            "  scores appear as modules complete"
        };
        frame.render_widget(
            Paragraph::new(Span::styled(waiting, Style::default().fg(palette::DIM)))
                .block(panel("scores")),
            area,
        );
        return;
    }

    let mut spans = vec![Span::raw("  ")];
    for (label, _) in &view.categories {
        let shown = view.eased.get(label).copied().unwrap_or(0.0);
        spans.push(Span::styled(
            format!("{label} "),
            Style::default().fg(palette::DIM),
        ));
        spans.push(Span::styled(
            format!("{shown:.0}  "),
            Style::default()
                .fg(palette::CYAN)
                .add_modifier(Modifier::BOLD),
        ));
    }

    let mut lines = vec![Line::from(spans)];
    let mut totals = vec![Span::raw("  ")];
    totals.push(Span::styled("total ", Style::default().fg(palette::DIM)));
    totals.push(Span::styled(
        view.total
            .map(|value| format!("{value:.0}"))
            .unwrap_or_else(|| "n/a".into()),
        Style::default()
            .fg(palette::FG)
            .add_modifier(Modifier::BOLD),
    ));
    totals.push(Span::styled(
        if view.scores_final {
            "   final"
        } else {
            "   provisional"
        },
        Style::default().fg(if view.scores_final {
            palette::OK
        } else {
            palette::WARN
        }),
    ));
    if view.uncalibrated {
        totals.push(Span::styled(
            "   uncalibrated model",
            Style::default().fg(palette::WARN),
        ));
    }
    lines.push(Line::from(totals));

    frame.render_widget(Paragraph::new(lines).block(panel("scores")), area);
}

fn render_footer(frame: &mut Frame<'_>, area: Rect, view: &View) {
    let mut spans = Vec::new();
    if view.cancelling {
        spans.push(Span::styled(
            " cancelling - the module in flight is being stopped ",
            Style::default().fg(palette::WARN),
        ));
    } else if view.state.is_terminal() {
        spans.push(Span::styled(" done ", Style::default().fg(palette::OK)));
    } else {
        spans.push(Span::styled(" q ", Style::default().fg(palette::FG)));
        spans.push(Span::styled("cancel   ", Style::default().fg(palette::DIM)));
    }

    if let Some((severity, text)) = view.log.back() {
        spans.push(Span::styled(
            text.clone(),
            Style::default().fg(match severity {
                Severity::Info => palette::DIM,
                Severity::Warning => palette::WARN,
                Severity::Error => palette::BAD,
            }),
        ));
    }

    frame.render_widget(
        Paragraph::new(Line::from(spans)).alignment(Alignment::Left),
        area,
    );
}

#[cfg(test)]
// In tests, `unwrap`/`expect` panicking *is* the failure signal.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic_in_result_fn)]
mod tests {
    use super::*;

    #[test]
    fn sparkline_pads_to_the_requested_width() {
        let values: VecDeque<f64> = [1.0, 2.0, 3.0].into_iter().collect();
        assert_eq!(sparkline(&values, 8).chars().count(), 8);
        assert_eq!(sparkline(&VecDeque::new(), 8).chars().count(), 8);
    }

    #[test]
    fn sparkline_keeps_the_most_recent_samples() {
        let values: VecDeque<f64> = (0..100).map(f64::from).collect();
        let drawn = sparkline(&values, 4);
        // The last sample is the maximum, so it must render as a full block.
        assert!(drawn.ends_with('█'), "got {drawn}");
    }

    /// A flat series must not read as a stall. Drawing identical values at the
    /// glyph floor made a perfectly steady metric look like a dead one.
    #[test]
    fn a_flat_series_is_drawn_mid_height_not_at_the_floor() {
        let values: VecDeque<f64> = [5.0; 6].into_iter().collect();
        let drawn = sparkline(&values, 6);
        assert!(!drawn.contains('▁'), "got {drawn}");
        assert!(drawn.chars().all(|c| c == '▄'), "got {drawn}");
    }

    #[test]
    fn non_finite_samples_are_dropped_rather_than_scaling_everything_to_nothing() {
        let values: VecDeque<f64> = [1.0, f64::NAN, 2.0, f64::INFINITY].into_iter().collect();
        let drawn = sparkline(&values, 4);
        assert_eq!(drawn.chars().count(), 4);
        assert!(!drawn.contains('\u{0}'));
    }

    #[test]
    fn elapsed_grows_an_hours_field_only_when_needed() {
        assert_eq!(format_elapsed(Duration::from_secs(0)), "00:00");
        assert_eq!(format_elapsed(Duration::from_secs(75)), "01:15");
        assert_eq!(format_elapsed(Duration::from_secs(3661)), "1:01:01");
    }

    #[test]
    fn values_are_formatted_at_a_readable_precision_across_the_whole_range() {
        assert_eq!(format_value(0.00123), "0.0012");
        assert_eq!(format_value(12.5), "12.50");
        assert_eq!(format_value(1234.5), "1234.5");
        assert_eq!(format_value(1_234_567.0), "1234567");
    }

    #[test]
    fn bytes_are_formatted_in_binary_units() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(2048), "2.0 KiB");
        assert_eq!(format_bytes(3 * 1024 * 1024 * 1024), "3.0 GiB");
    }

    /// The dashboard must never appear where plain text would not have been
    /// coloured, and never on a terminal that says it cannot render it.
    #[test]
    fn the_dashboard_is_off_wherever_decoration_is_off() {
        assert!(!should_render(false, false), "undecorated output");
        assert!(!should_render(true, true), "--no-tui");
    }

    /// Regression: a pty created without a window size reports 0x0, every
    /// layout rect collapsed, and the dashboard held a blank alternate screen
    /// for an entire run rather than falling back to plain progress.
    #[test]
    fn a_viewport_too_small_to_draw_is_refused() {
        assert!(!fits(0, 0), "an unsized pty must never be drawn into");
        assert!(!fits(200, 0));
        assert!(!fits(0, 60));
        assert!(!fits(79, 24), "one column short");
        assert!(!fits(80, 23), "one row short");
        assert!(fits(80, 24), "the classic default must be enough");
        assert!(fits(200, 60));
    }

    #[test]
    fn overall_progress_is_bounded_even_if_a_module_reports_nonsense() {
        let mut view = View::new(Profile::Quick);
        view.modules = vec![
            ModuleRow {
                id: ModuleId::new("cpu.mixed").unwrap(),
                phase: Phase::Running,
                progress: 5.0,
            },
            ModuleRow {
                id: ModuleId::new("memory.bandwidth").unwrap(),
                phase: Phase::Queued,
                progress: -1.0,
            },
        ];
        let progress = view.overall_progress();
        assert!(
            (0.0..=1.0).contains(&progress),
            "a Gauge ratio outside [0,1] panics ratatui; got {progress}"
        );
    }

    #[test]
    fn easing_converges_on_the_reported_score() {
        let mut view = View::new(Profile::Quick);
        view.categories = vec![("Compute".into(), 1000.0)];
        for _ in 0..120 {
            view.tick();
        }
        let shown = view.eased.get("Compute").copied().unwrap_or_default();
        assert!((shown - 1000.0).abs() < 1.0, "got {shown}");
    }

    /// A view with one of everything, for the rendering tests below.
    fn populated() -> View {
        let mut view = View::new(Profile::Quick);
        view.state = RunState::Running;
        view.risk = Some("HeavyLoad".into());
        view.modules = vec![
            ModuleRow {
                id: ModuleId::new("cpu.mixed").unwrap(),
                phase: Phase::Done(ModuleStatus::Completed),
                progress: 1.0,
            },
            ModuleRow {
                id: ModuleId::new("memory.bandwidth").unwrap(),
                phase: Phase::Running,
                progress: 0.62,
            },
            ModuleRow {
                id: ModuleId::new("storage.mixed").unwrap(),
                phase: Phase::Queued,
                progress: 0.0,
            },
        ];
        for (key, value, unit) in [
            ("crypto_sha256.single", 1842.5, "MiB/s"),
            ("latency_random.single", 0.0184, "ms"),
        ] {
            let track = view.metrics.entry(key.to_string()).or_default();
            track.value = value;
            track.unit = unit.to_string();
            track.rep = 4;
            track.history = (0..24)
                .map(|i| value * (1.0 + f64::from(i % 5) / 40.0))
                .collect();
        }
        for i in 0..30 {
            view.telemetry.push_back(TelemetryPoint {
                cpu_busy: 88.0 + f64::from(i % 7),
                cpu_external: 0.4,
                cpu_steal: 0.0,
                mem_used: 6_012_000_000,
                mem_total: 16_000_000_000,
                load1: 7.5,
                freq_mhz: Some(3600.0),
                temp_c: Some(64.0),
            });
        }
        view.categories = vec![("Compute".into(), 1024.0), ("Memory".into(), 968.0)];
        view.total = Some(996.0);
        for _ in 0..80 {
            view.tick();
        }
        view
    }

    fn draw(width: u16, height: u16) -> ratatui::buffer::Buffer {
        let view = populated();
        let mut terminal =
            Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| render(frame, &view)).unwrap();
        terminal.backend().buffer().clone()
    }

    fn as_text(buffer: &ratatui::buffer::Buffer) -> String {
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Renders a full frame and prints it. `cargo test -- --nocapture
    /// renders_a_frame` is the fastest way to look at the dashboard without
    /// starting a benchmark.
    #[test]
    fn renders_a_frame() {
        let buffer = draw(96, 30);
        let text = as_text(&buffer);
        println!("\n{text}\n");

        for expected in [
            "DARC",
            "BENCH",
            "cpu.mixed",
            "memory.bandwidth",
            "crypto_sha256.single",
            "host telemetry",
            "scores",
        ] {
            assert!(
                text.contains(expected),
                "frame is missing `{expected}`:\n{text}"
            );
        }
    }

    /// Ratatui panics on a `Gauge` ratio outside `[0, 1]` and on some zero-sized
    /// layouts, and a dashboard that aborts the process would take the run's
    /// unwritten bundle with it. Small and odd terminals must simply draw.
    #[test]
    fn renders_at_awkward_terminal_sizes_without_panicking() {
        for (width, height) in [(20, 10), (40, 12), (200, 60), (80, 24), (30, 8)] {
            let buffer = draw(width, height);
            assert_eq!(buffer.area.width, width);
            assert_eq!(buffer.area.height, height);
        }
    }
}
