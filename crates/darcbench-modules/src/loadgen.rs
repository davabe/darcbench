//! The open-model load generator, and the saturation detection that decides
//! whether anything it measured is worth reporting.
//!
//! This is Phase 3's foundation: every HTTP module drives its target through
//! here rather than writing its own request loop, for the same reason
//! [`crate::harness`] owns calibration - what follows is measurement policy,
//! not workload detail, and a module that quietly diverged from it would
//! produce numbers that look comparable and are not.
//!
//! # Why open-model, and what closed-loop gets wrong
//!
//! The obvious way to load a server is a pool of workers each looping "send a
//! request, wait for the response, send the next one". That is a **closed**
//! model, and it has a defect that is not obvious and is not small: the offered
//! load depends on the server's own response time. When the server slows down,
//! the generator slows down with it and stops asking for the load it was
//! configured to produce. The queue that would form in production never forms,
//! so the latencies the generator records are the latencies of a server that
//! was politely never overloaded.
//!
//! `docs/BENCHMARK-METHODOLOGY.md` requires the alternative: *"use an
//! open-model, constant-rate generator and measure latency from when a request
//! should have been sent, not when it was - the coordinated-omission correction
//! described by Gil Tene in [wrk2](https://github.com/giltene/wrk2)"*.
//!
//! So this generator fixes the schedule in advance. Request `i` is *due* at
//! `start + i / rate`, whatever happened to requests `0..i`. Nothing about the
//! server's behaviour can change when the next request is owed.
//!
//! # Coordinated omission, concretely
//!
//! Suppose a server serving 1000 requests/s stalls for one second. A closed
//! generator with 10 workers records ten requests of ~1000 ms and then carries
//! on: 10 slow samples out of 1000, a p99 that barely moves. But 1000 requests
//! were *owed* during that second, and the last of them waited a full second.
//! The stall is real, it is what a user experienced, and the closed generator
//! recorded a tenth of a percent of it.
//!
//! The correction is to measure each request from when it was **due**, not from
//! when it was sent. This module records both:
//!
//! * [`LoadOutcome::service_ms`] - completion minus actual send. What the
//!   server took. Useful, and the number every naive tool reports.
//! * [`LoadOutcome::response_ms`] - completion minus *due* time. What a client
//!   waiting on a schedule experienced, including any time the request spent
//!   owed but unsent.
//!
//! They are equal on an unsaturated system and diverge exactly when queueing
//! begins. Publishing only the first is how a benchmark reports a tail latency
//! that no user has ever observed.
//!
//! # Why saturation is decided by the schedule, not by CPU
//!
//! The methodology asks for generator-side CPU utilisation as the saturation
//! signal. This module records it - see [`LoadOutcome::generator_cpu_pct`] -
//! but does not decide on it, because CPU is a proxy for the thing and the
//! thing is directly observable: *did the generator issue the load it promised,
//! on time?*
//!
//! A generator can be far from CPU-bound and still fall behind, on a system
//! whose connections are all waiting; and it can be near its CPU ceiling and
//! still hold the schedule perfectly. Deciding on the proxy would produce both
//! errors. Missing the schedule cannot be argued with: if request 40,000 went
//! out two seconds late, no latency recorded after it describes the load the
//! run claims to have offered.
//!
//! CPU is still recorded, and it is what tells an operator *why* a saturated
//! run saturated - whether to raise the concurrency, or to reach for the
//! external-generator mode.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use darcbench_protocol::metrics::{Warning, WarningCode};

/// One unit of work the generator can drive.
///
/// Deliberately not an HTTP client. The generator's contract is about *when*
/// requests happen, and keeping it ignorant of what a request is makes it
/// testable against a synthetic target whose latency can be dictated - which is
/// the only way to prove that saturation detection fires when it should, since
/// a real server that cooperates by being exactly slow enough does not exist.
pub trait LoadTarget: Sync {
    /// Issues one request and returns the bytes received.
    ///
    /// `worker` identifies the calling thread, so an implementation can hold
    /// one connection per worker without any locking of its own.
    fn request(&self, worker: usize) -> Result<u64, String>;
}

/// What the generator was asked to produce.
#[derive(Clone, Copy, Debug)]
pub struct LoadPlan {
    /// Requests per second the generator will *owe*, whatever the target does.
    pub rate_per_s: f64,
    /// How long to keep issuing them.
    pub duration: Duration,
    /// Workers, and therefore the maximum number of requests that can be in
    /// flight at once.
    ///
    /// This is the one closed-model element that survives, and it has to: an
    /// unbounded open model on a stalled server allocates threads until the
    /// machine dies, which is not an acceptable thing for a benchmark to do to
    /// somebody else's server. Reaching the bound is itself a saturation
    /// signal rather than a silent cap - see [`Saturation::AllWorkersBusy`].
    pub workers: usize,
    /// Requests issued and discarded before recording starts.
    ///
    /// Connection setup, TLS handshakes and a cold accept path are real costs
    /// but they are not steady-state serving, and a run that folded them into
    /// its latency distribution would report a machine that is slower than it
    /// is at the job it will actually do.
    pub warmup: u64,
}

/// Slowest and fastest schedules the generator will accept.
///
/// A plan outside this range is a caller bug rather than a measurement, and the
/// bounds exist so it becomes a clamped, reported one instead of a panic:
/// `Duration::from_secs_f64` aborts on infinity and on NaN, and a rate of zero
/// produces both. A benchmark that kills the agent because a rate was
/// mis-computed would be a worse failure than any number it could have
/// produced.
const MIN_RATE_PER_S: f64 = 0.001;
const MAX_RATE_PER_S: f64 = 10_000_000.0;

impl LoadPlan {
    /// The rate actually used, with a nonsensical one brought into range.
    fn rate(&self) -> f64 {
        if !self.rate_per_s.is_finite() {
            return MIN_RATE_PER_S;
        }
        self.rate_per_s.clamp(MIN_RATE_PER_S, MAX_RATE_PER_S)
    }

    /// Total requests the schedule owes, excluding warm-up.
    fn scheduled(&self) -> u64 {
        let count = self.rate() * self.duration.as_secs_f64();
        // Floored at one: a plan that owes no requests at all would produce an
        // empty outcome, and an empty outcome reads as a clean measurement.
        if count.is_finite() && count >= 1.0 {
            count as u64
        } else {
            1
        }
    }

    fn period(&self) -> Duration {
        Duration::from_secs_f64(1.0 / self.rate())
    }
}

/// Why a run's numbers cannot be trusted, when they cannot.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Saturation {
    /// The generator held its schedule. The measurement stands.
    #[default]
    None,
    /// Requests went out materially later than they were due.
    ///
    /// The direct evidence that the injector, not the target, was the
    /// bottleneck. Everything measured after the slip began describes a load
    /// the run did not actually offer.
    ScheduleSlip,
    /// Fewer requests were completed than the plan owed.
    ///
    /// Distinct from a slip: a run can hold its per-request schedule and still
    /// finish short, when the target stops answering at all.
    RateShortfall,
    /// Every worker was busy every time a request came due.
    ///
    /// The concurrency bound, not the machine, decided the offered load. The
    /// result describes `workers` concurrent clients rather than the rate that
    /// was asked for.
    AllWorkersBusy,
}

impl Saturation {
    pub fn is_saturated(self) -> bool {
        self != Self::None
    }
}

/// Fraction by which the achieved rate may fall short of the plan before the
/// run is declared saturated.
///
/// Five percent. A schedule executed by an OS scheduler will never be exact,
/// and a bound tight enough to catch every millisecond would fail every run on
/// a busy machine; a bound loose enough to tolerate a tenth of the load would
/// not be a bound. Five percent is comfortably outside scheduler jitter at the
/// rates these modules use and comfortably inside the point where a latency
/// distribution starts describing a different experiment.
const RATE_SHORTFALL_TOLERANCE: f64 = 0.05;

/// Mean lateness, as a multiple of the inter-request period, above which the
/// generator is judged not to have kept its schedule.
///
/// Expressed relative to the period rather than in milliseconds because the
/// same absolute lateness means opposite things at 10 req/s and at 10,000: one
/// is rounding, the other is a backlog. Half a period of *mean* lateness means
/// the generator is, on average, half a request behind - which is where the
/// recorded distribution stops describing the schedule it claims.
const SLIP_TOLERANCE_PERIODS: f64 = 0.5;

/// Lateness below which nothing is a signal, whatever the period.
///
/// The tolerances above are expressed relative to the inter-request period,
/// which is right until the period gets small. At 18,000 requests a second the
/// period is 54 microseconds, half a period is 27, and *no* general-purpose
/// scheduler places a thread within 27 microseconds of when it asked to run.
/// Without a floor the detector fires on the operating system's own timing
/// granularity and declares every high-rate phase saturated - which it did, on
/// a four-core host, at under nine percent of the machine's measured capacity.
///
/// A millisecond is comfortably above ordinary wake-up jitter and comfortably
/// below any lateness that changes what a latency distribution describes. It
/// makes the detector unable to see sub-millisecond scheduling noise, which is
/// correct: it cannot distinguish that from the thing it is looking for.
const SCHEDULER_NOISE_FLOOR_MS: f64 = 1.0;

/// What one load phase produced.
#[derive(Clone, Debug, Default)]
pub struct LoadOutcome {
    /// Requests the plan owed.
    pub scheduled: u64,
    /// Warm-up requests that returned successfully.
    ///
    /// Not a measurement - warm-up timings are discarded on purpose. It exists
    /// because the origin answered these and an external session's
    /// reconciliation counts what the origin answered, so a generator that
    /// claimed its *planned* warm-up rather than its achieved one would be
    /// claiming responses that were never sent.
    pub warmup_completed: u64,
    /// Requests that completed successfully.
    pub completed: u64,
    /// Requests that returned an error, with the first few reasons kept.
    pub errors: u64,
    pub error_examples: Vec<String>,
    pub bytes: u64,
    /// Completion minus actual send, per request. What the target took.
    pub service_ms: Vec<f64>,
    /// Completion minus *due* time, per request. What a client on a schedule
    /// experienced. This is the coordinated-omission-corrected series and it is
    /// the one a latency metric must be built from.
    pub response_ms: Vec<f64>,
    /// Actual send minus due time, per request: how late the generator was.
    pub slip_ms: Vec<f64>,
    /// Requests actually completed per second over the measured window.
    pub achieved_rate_per_s: f64,
    /// CPU the agent process consumed during the phase, as a percentage of one
    /// core. Above `100 * cores` is impossible; near it means the injector had
    /// no headroom left.
    ///
    /// Recorded as context, not as the saturation criterion - see the module
    /// documentation for why.
    pub generator_cpu_pct: f64,
    /// Times the generator wanted a worker and every one was busy.
    pub worker_starvation: u64,
    pub saturation: Saturation,
}

impl LoadOutcome {
    /// The warning a saturated run must carry, if it is saturated.
    ///
    /// [`WarningCode::GeneratorSaturated`] is in the degrading set, so this is
    /// what makes the methodology's rule - *"emit `GeneratorSaturated`, which
    /// invalidates the result, if the injector runs out of headroom"* - hold
    /// mechanically rather than by a module remembering to check.
    pub fn warning(&self, metric_key: Option<String>) -> Option<Warning> {
        let detail = match self.saturation {
            Saturation::None => return None,
            Saturation::ScheduleSlip => format!(
                "requests went out {:.1} ms late on average against a {:.2} ms schedule, so the \
                 load offered after the backlog began is not the load this run claims",
                mean(&self.slip_ms),
                1000.0 / self.achieved_rate_per_s.max(f64::MIN_POSITIVE),
            ),
            Saturation::RateShortfall => format!(
                "the generator completed {:.0} of the {} requests it owed ({:.0}/s achieved)",
                self.completed as f64, self.scheduled, self.achieved_rate_per_s,
            ),
            Saturation::AllWorkersBusy => format!(
                "every connection was busy on {} occasions when a request came due, so the \
                 concurrency limit and not the machine decided the offered load",
                self.worker_starvation,
            ),
        };
        Some(Warning {
            code: WarningCode::GeneratorSaturated,
            message: format!(
                "The load generator, not the system under test, was the bottleneck: {detail}. The \
                 measurements are kept as evidence and the result is degraded, because a latency \
                 distribution recorded while the injector could not keep up describes the \
                 injector. Generator CPU during the phase was {:.0}% of one core. Raising the \
                 connection count may help; a machine that outruns a local injector needs the \
                 external-generator mode.",
                self.generator_cpu_pct,
            ),
            metric_key,
        })
    }
}

/// Finds how many requests per second the target can actually serve.
///
/// # Why this one is closed-loop, and why that is not a contradiction
///
/// Everything above argues that a closed-loop generator produces untrustworthy
/// *latency*. It does. It is also the right tool for measuring *capacity*, and
/// the two are different measurements: workers looping flat out ask the machine
/// for as much as it will give and report what it gave, which is exactly the
/// question. Coordinated omission is a defect in a latency distribution, and
/// this function does not produce one - it returns a single rate and no
/// latencies at all, so there is nothing for the omission to distort.
///
/// The two belong together. A rate has to come from somewhere before an
/// open-model phase can be scheduled at it, and picking one out of the air
/// would measure whatever that guess happened to be: too low and the machine
/// looks idle, too high and every run reports itself saturated. So capacity is
/// measured first and the open-model phases are scheduled at a fraction of it -
/// the same shape as `harness::calibrate_with`, which sizes a repetition before
/// timing it, for the same reason.
///
/// Returns requests per second, counting only successful ones.
pub fn measure_capacity(target: &dyn LoadTarget, workers: usize, duration: Duration) -> f64 {
    let workers = workers.max(1);
    let served = AtomicU64::new(0);
    let deadline = Instant::now() + duration;
    let started = Instant::now();

    std::thread::scope(|scope| {
        for worker in 0..workers {
            let served = &served;
            scope.spawn(move || {
                while Instant::now() < deadline {
                    if target.request(worker).is_ok() {
                        served.fetch_add(1, Ordering::Relaxed);
                    }
                }
            });
        }
    });

    let elapsed = started.elapsed().as_secs_f64().max(f64::MIN_POSITIVE);
    served.load(Ordering::Relaxed) as f64 / elapsed
}

/// Runs one load phase against `target`.
///
/// Blocking, and expected to be called from a module's own thread - the same
/// contract every other module here follows.
pub fn run(target: &dyn LoadTarget, plan: &LoadPlan) -> LoadOutcome {
    let scheduled = plan.scheduled();
    let period = plan.period();
    let workers = plan.workers.max(1);

    // --- warm-up ---------------------------------------------------------
    //
    // Run flat out rather than on the schedule: the point is to have
    // connections open and caches warm before the clock that matters starts,
    // not to measure anything.
    let mut warmup_completed: u64 = 0;
    for index in 0..plan.warmup {
        // Counted, not discarded. The result of a warm-up request is not a
        // measurement, but *whether it succeeded* is evidence: an external
        // session reconciles the generator's successful-request count against
        // the origin's own, and a warm-up the origin never served is a request
        // the generator must not claim. Reporting `plan.warmup` unconditionally
        // made one refused connection reject an otherwise valid run.
        if target.request((index as usize) % workers).is_ok() {
            warmup_completed += 1;
        }
    }

    let shared = Arc::new(Shared {
        next: AtomicU64::new(0),
        scheduled,
        starvation: AtomicU64::new(0),
        // One buffer per worker, merged after the phase.
        //
        // A single shared `Mutex<Samples>` taken once per request put a global
        // lock in the hot path, and at the rates a loopback origin reaches -
        // over a hundred thousand requests a second - the generator spent more
        // time contending on it than issuing requests. It then reported itself
        // saturated, which was true and was its own fault: an injector that
        // cannot schedule the load it was asked for measures the injector.
        per_worker: (0..workers)
            .map(|_| Mutex::new(Samples::with_capacity(scheduled as usize / workers + 1)))
            .collect(),
    });

    // A worker is "starved" only when it is late by more than a period *and*
    // by more than the scheduler can resolve. See `SCHEDULER_NOISE_FLOOR_MS`.
    let starvation_threshold =
        period.max(Duration::from_secs_f64(SCHEDULER_NOISE_FLOOR_MS / 1000.0));

    let cpu_before = process_cpu_seconds();
    let started = Instant::now();

    std::thread::scope(|scope| {
        for worker in 0..workers {
            let shared = Arc::clone(&shared);
            scope.spawn(move || {
                loop {
                    let index = shared.next.fetch_add(1, Ordering::Relaxed);
                    if index >= shared.scheduled {
                        return;
                    }
                    // The whole open model, in one line: when this request is
                    // owed is a function of its index and nothing else. No
                    // outcome of any earlier request can move it.
                    let due = started + period.mul_f64(index as f64);
                    let now = Instant::now();
                    if now < due {
                        std::thread::sleep(due - now);
                    } else if now.duration_since(due) > starvation_threshold {
                        // Late by more than a period *and* by more than the
                        // scheduler's own resolution before the request is even
                        // sent: this worker was still busy when its turn came
                        // round, which is the concurrency bound binding rather
                        // than the target being slow.
                        shared.starvation.fetch_add(1, Ordering::Relaxed);
                    }

                    let sent = Instant::now();
                    let outcome = target.request(worker);
                    let done = Instant::now();

                    let Some(Ok(mut samples)) =
                        shared.per_worker.get(worker).map(|slot| slot.lock())
                    else {
                        // A poisoned lock means this worker panicked earlier.
                        // Stop rather than continue producing samples nothing
                        // will read.
                        return;
                    };
                    samples
                        .slip_ms
                        .push(millis(sent.saturating_duration_since(due)));
                    match outcome {
                        Ok(bytes) => {
                            samples.bytes += bytes;
                            samples.service_ms.push(millis(done - sent));
                            // Measured from `due`, never from `sent`. This is
                            // the coordinated-omission correction and it is the
                            // reason this generator exists.
                            samples
                                .response_ms
                                .push(millis(done.saturating_duration_since(due)));
                        }
                        Err(error) => {
                            samples.errors += 1;
                            if samples.error_examples.len() < 3 {
                                samples.error_examples.push(error);
                            }
                        }
                    }
                }
            });
        }
    });

    let elapsed = started.elapsed();
    let cpu_used = process_cpu_seconds() - cpu_before;

    let mut samples = Samples::with_capacity(scheduled as usize);
    for slot in &shared.per_worker {
        let part = match slot.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        };
        samples.merge(part);
    }

    let seconds = elapsed.as_secs_f64().max(f64::MIN_POSITIVE);
    let mut outcome = LoadOutcome {
        scheduled,
        warmup_completed,
        completed: samples.response_ms.len() as u64,
        errors: samples.errors,
        error_examples: samples.error_examples,
        bytes: samples.bytes,
        achieved_rate_per_s: samples.response_ms.len() as f64 / seconds,
        generator_cpu_pct: (cpu_used / seconds) * 100.0,
        worker_starvation: shared.starvation.load(Ordering::Relaxed),
        service_ms: samples.service_ms,
        response_ms: samples.response_ms,
        slip_ms: samples.slip_ms,
        saturation: Saturation::None,
    };
    outcome.saturation = classify(&outcome, plan);
    outcome
}

/// Decides whether the generator kept its promise.
///
/// Ordered most-specific first, so the warning names the thing that actually
/// went wrong rather than the most general symptom it caused. A generator that
/// ran out of workers also slips and also falls short; saying "every connection
/// was busy" tells an operator to raise the connection count, and saying "the
/// rate fell short" does not.
fn classify(outcome: &LoadOutcome, plan: &LoadPlan) -> Saturation {
    // A single starvation event is one unlucky scheduling moment. Starving on a
    // material share of the schedule is the bound binding.
    if outcome.worker_starvation as f64 > outcome.scheduled as f64 * RATE_SHORTFALL_TOLERANCE {
        return Saturation::AllWorkersBusy;
    }
    let period_ms = plan.period().as_secs_f64() * 1000.0;
    let slip_tolerance = (period_ms * SLIP_TOLERANCE_PERIODS).max(SCHEDULER_NOISE_FLOOR_MS);
    if mean(&outcome.slip_ms) > slip_tolerance {
        return Saturation::ScheduleSlip;
    }
    let owed = outcome.scheduled as f64;
    if (outcome.completed as f64) < owed * (1.0 - RATE_SHORTFALL_TOLERANCE) {
        return Saturation::RateShortfall;
    }
    Saturation::None
}

struct Shared {
    next: AtomicU64,
    scheduled: u64,
    starvation: AtomicU64,
    per_worker: Vec<Mutex<Samples>>,
}

#[derive(Clone, Default)]
struct Samples {
    service_ms: Vec<f64>,
    response_ms: Vec<f64>,
    slip_ms: Vec<f64>,
    errors: u64,
    error_examples: Vec<String>,
    bytes: u64,
}

impl Samples {
    /// Pre-allocated to the whole schedule.
    ///
    /// A `Vec` growing under a lock held by every worker would make the
    /// generator's own allocator a source of the jitter it is trying to
    /// measure - and the size is known in advance, so there is no reason to
    /// discover it.
    fn with_capacity(scheduled: usize) -> Self {
        Self {
            service_ms: Vec::with_capacity(scheduled),
            response_ms: Vec::with_capacity(scheduled),
            slip_ms: Vec::with_capacity(scheduled),
            ..Default::default()
        }
    }

    fn merge(&mut self, other: Self) {
        self.service_ms.extend(other.service_ms);
        self.response_ms.extend(other.response_ms);
        self.slip_ms.extend(other.slip_ms);
        self.errors += other.errors;
        self.bytes += other.bytes;
        for example in other.error_examples {
            if self.error_examples.len() < 3 {
                self.error_examples.push(example);
            }
        }
    }
}

fn millis(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

fn mean(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.iter().sum::<f64>() / values.len() as f64
}

/// This process's cumulative CPU time in seconds, generator included.
///
/// The agent's whole process rather than the generator's threads alone: Rust's
/// standard library does not expose thread ids, so per-thread accounting would
/// mean walking `/proc/self/task` and matching threads by name. During a load
/// phase the generator dominates this figure, and the number is reported as
/// context rather than used as the saturation criterion, so the extra precision
/// would buy nothing.
///
/// Returns 0.0 where `/proc` is not readable, which makes the CPU figure absent
/// rather than fabricated.
fn process_cpu_seconds() -> f64 {
    let Ok(raw) = std::fs::read_to_string("/proc/self/stat") else {
        return 0.0;
    };
    // `comm` may contain spaces and parentheses, so fields are counted from the
    // last `)`, never by splitting the whole line.
    let Some(position) = raw.rfind(')') else {
        return 0.0;
    };
    let mut fields = raw[position + 1..].split_whitespace();
    // state ppid pgrp session tty_nr tpgid flags minflt cminflt majflt cmajflt
    // are fields 3..=13; utime is 14 and stime is 15.
    let Some(utime) = fields.nth(11).and_then(|v| v.parse::<f64>().ok()) else {
        return 0.0;
    };
    let Some(stime) = fields.next().and_then(|v| v.parse::<f64>().ok()) else {
        return 0.0;
    };
    // USER_HZ is 100 on every Linux target this agent builds for. Getting it
    // from `sysconf` would need libc, which this workspace does without; the
    // consequence of being wrong is a mis-scaled context figure, not a wrong
    // verdict, because nothing decides on this value.
    (utime + stime) / 100.0
}

#[cfg(test)]
// In tests, `unwrap`/`expect` panicking *is* the failure signal.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic_in_result_fn)]
mod tests {
    use super::*;

    /// A target whose latency the test dictates, which is the only way to prove
    /// saturation detection fires: a real server that cooperates by being
    /// exactly slow enough does not exist.
    struct FixedLatency {
        per_request: Duration,
        served: AtomicU64,
    }

    impl FixedLatency {
        fn new(per_request: Duration) -> Self {
            Self {
                per_request,
                served: AtomicU64::new(0),
            }
        }
    }

    impl LoadTarget for FixedLatency {
        fn request(&self, _worker: usize) -> Result<u64, String> {
            std::thread::sleep(self.per_request);
            self.served.fetch_add(1, Ordering::Relaxed);
            Ok(64)
        }
    }

    /// Rates here are deliberately far below what the machine can do.
    ///
    /// These tests run alongside the rest of the suite, which saturates every
    /// core - so a plan with only a little headroom flakes, and a flaky test on
    /// a saturation detector is worse than no test: it trains everyone to
    /// re-run it. A 20 ms period tolerates the tens of milliseconds of sleep
    /// overshoot a fully loaded scheduler produces; the *saturated* cases below
    /// are over-subscribed by a factor of ten or more, so they cannot flake in
    /// the other direction either.
    fn plan(rate: f64, millis: u64, workers: usize) -> LoadPlan {
        LoadPlan {
            rate_per_s: rate,
            duration: Duration::from_millis(millis),
            workers,
            warmup: 0,
        }
    }

    /// The baseline: a target comfortably faster than the schedule must produce
    /// an unsaturated run, or every saturation verdict below is meaningless.
    #[test]
    fn a_target_that_keeps_up_is_not_saturated() {
        // 50 requests/s against a 1 ms target with 16 workers: three orders of
        // magnitude of headroom.
        let target = FixedLatency::new(Duration::from_millis(1));
        let outcome = run(&target, &plan(50.0, 800, 16));

        assert_eq!(outcome.saturation, Saturation::None, "{outcome:?}");
        assert!(outcome.warning(None).is_none());
        assert_eq!(outcome.completed, outcome.scheduled);
        assert_eq!(outcome.errors, 0);
        assert!(outcome.bytes > 0);
    }

    /// The exit criterion of this deliverable, stated as a test: a saturated
    /// generator provably invalidates a result.
    #[test]
    fn a_saturated_generator_degrades_the_result() {
        // Two workers asked for 500 requests/s against a target taking 50 ms:
        // 40 req/s is the most this configuration can produce, so the generator
        // cannot possibly keep its promise.
        let target = FixedLatency::new(Duration::from_millis(50));
        let outcome = run(&target, &plan(500.0, 400, 2));

        assert!(
            outcome.saturation.is_saturated(),
            "an injector that cannot issue a tenth of its schedule must say so: {outcome:?}"
        );
        let warning = outcome
            .warning(Some("web.static/latency.p99".into()))
            .expect("a saturated outcome must produce a warning");
        assert_eq!(warning.code, WarningCode::GeneratorSaturated);
        assert!(
            warning.code.degrades_result(),
            "the methodology requires that a saturated generator invalidates the result"
        );
        assert_eq!(
            warning.metric_key.as_deref(),
            Some("web.static/latency.p99")
        );
        assert!(warning.message.contains("bottleneck"));
    }

    /// The reason this generator exists. A stall must show up in the corrected
    /// series even though the requests that observed it directly are few.
    #[test]
    fn coordinated_omission_is_corrected_for() {
        /// Stalls once, part-way through, for far longer than the period.
        struct StallsOnce {
            served: AtomicU64,
            stall_at: u64,
        }
        impl LoadTarget for StallsOnce {
            fn request(&self, _worker: usize) -> Result<u64, String> {
                let index = self.served.fetch_add(1, Ordering::Relaxed);
                if index == self.stall_at {
                    std::thread::sleep(Duration::from_millis(400));
                } else {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Ok(1)
            }
        }

        let target = StallsOnce {
            served: AtomicU64::new(0),
            stall_at: 10,
        };
        // One worker, so the stall genuinely blocks the schedule behind it.
        let outcome = run(&target, &plan(50.0, 1200, 1));

        let worst_service = outcome.service_ms.iter().cloned().fold(0.0, f64::max);
        let worst_response = outcome.response_ms.iter().cloned().fold(0.0, f64::max);
        assert!(
            worst_service > 350.0,
            "the request that hit the stall must see it: {worst_service}"
        );
        assert!(
            worst_response >= worst_service,
            "a request measured from when it was due can never look faster than one measured \
             from when it was sent"
        );

        // The point: requests that were merely *owed* during the stall carry
        // it too. Counting how many samples exceed 100 ms separates the two
        // series - the service series sees one, the corrected series sees the
        // whole backlog.
        let slow_service = outcome.service_ms.iter().filter(|ms| **ms > 100.0).count();
        let slow_response = outcome.response_ms.iter().filter(|ms| **ms > 100.0).count();
        assert!(
            slow_response > slow_service,
            "the corrected series must carry the queue the stall created, not only the one \
             request that observed it: service {slow_service}, response {slow_response}"
        );
    }

    /// The schedule must be a property of the index, not of what the target did
    /// to earlier requests. This is what makes the model open.
    #[test]
    fn the_schedule_does_not_move_when_the_target_slows_down() {
        let fast = FixedLatency::new(Duration::from_millis(1));
        let slow = FixedLatency::new(Duration::from_millis(8));
        // Enough workers that neither run starves; the rate is what is being
        // held constant.
        let fast_outcome = run(&fast, &plan(50.0, 800, 16));
        let slow_outcome = run(&slow, &plan(50.0, 800, 16));

        assert_eq!(
            fast_outcome.scheduled, slow_outcome.scheduled,
            "a slower target must not be asked for fewer requests - that is exactly the closed \
             model's defect"
        );
        assert!(!fast_outcome.saturation.is_saturated(), "{fast_outcome:?}");
        assert!(
            !slow_outcome.saturation.is_saturated(),
            "an eight-fold slower target that still fits inside the concurrency budget is a \
             valid measurement, not a saturated one: {slow_outcome:?}"
        );
        // ...and the eight-fold difference is visible where it belongs.
        assert!(mean(&slow_outcome.service_ms) > mean(&fast_outcome.service_ms) * 3.0);
    }

    /// The concurrency bound binding is its own diagnosis, because its remedy
    /// is different: raise the connection count rather than reach for another
    /// machine.
    #[test]
    fn running_out_of_workers_is_reported_as_such() {
        let target = FixedLatency::new(Duration::from_millis(40));
        let outcome = run(&target, &plan(400.0, 400, 1));
        assert_eq!(
            outcome.saturation,
            Saturation::AllWorkersBusy,
            "{outcome:?}"
        );
        let warning = outcome.warning(None).expect("warning");
        assert!(
            warning.message.contains("connection"),
            "the message must point at the remedy: {}",
            warning.message
        );
    }

    /// A target that fails must not be counted as having served anything, and
    /// the reason must survive to the operator.
    #[test]
    fn errors_are_counted_and_explained_rather_than_averaged_away() {
        struct AlwaysFails;
        impl LoadTarget for AlwaysFails {
            fn request(&self, _worker: usize) -> Result<u64, String> {
                Err("connection refused".into())
            }
        }
        let outcome = run(&AlwaysFails, &plan(50.0, 400, 4));
        assert_eq!(outcome.completed, 0);
        assert!(outcome.errors > 0);
        assert_eq!(outcome.bytes, 0);
        assert!(outcome.service_ms.is_empty(), "a failure is not a latency");
        assert_eq!(
            outcome.saturation,
            Saturation::RateShortfall,
            "a run that completed nothing cannot be reported as a clean measurement"
        );
        assert_eq!(outcome.error_examples.len(), 3, "kept, and bounded");
        assert_eq!(outcome.error_examples[0], "connection refused");
    }

    /// Degenerate plans must not produce a division by zero or an empty
    /// outcome that reads as a successful measurement.
    #[test]
    fn a_degenerate_plan_cannot_masquerade_as_a_measurement() {
        let target = FixedLatency::new(Duration::from_micros(100));
        for bad in [
            plan(0.0, 100, 4),
            plan(f64::NAN, 100, 4),
            plan(100.0, 0, 4),
            plan(100.0, 100, 0),
        ] {
            let outcome = run(&target, &bad);
            assert!(outcome.scheduled >= 1, "{bad:?}");
            assert!(outcome.achieved_rate_per_s.is_finite(), "{outcome:?}");
            assert!(outcome.generator_cpu_pct.is_finite(), "{outcome:?}");
        }
    }

    /// Capacity must reflect what the target can actually do, and must scale
    /// with the concurrency it is given when the target is not the limit.
    #[test]
    fn capacity_measures_what_the_target_can_serve() {
        let target = FixedLatency::new(Duration::from_millis(10));
        // One worker against a 10 ms target can serve about 100/s.
        let single = measure_capacity(&target, 1, Duration::from_millis(500));
        assert!(
            (40.0..160.0).contains(&single),
            "a 10 ms target on one connection is ~100/s, got {single}"
        );

        // Four connections against a target that sleeps rather than computes
        // should multiply, because sleeping is not a resource they contend for.
        let four = measure_capacity(&target, 4, Duration::from_millis(500));
        assert!(
            four > single * 2.0,
            "concurrency must raise capacity when the target is not the bottleneck: \
             {single} -> {four}"
        );
    }

    #[test]
    fn generator_cpu_is_recorded_and_plausible() {
        let target = FixedLatency::new(Duration::from_millis(1));
        let outcome = run(&target, &plan(50.0, 600, 8));
        assert!(
            outcome.generator_cpu_pct >= 0.0 && outcome.generator_cpu_pct.is_finite(),
            "{}",
            outcome.generator_cpu_pct
        );
        // Sleeping workers are not busy workers: a generator driving a target
        // that does nothing but sleep cannot have saturated a core.
        // A generous ceiling: process CPU is counted in 10 ms jiffies over a
        // sub-second window, so the quantisation error is worth a whole core.
        assert!(
            outcome.generator_cpu_pct < 100.0 * (num_cpus_upper_bound() + 1.0),
            "{} exceeds what this machine has",
            outcome.generator_cpu_pct
        );
    }

    fn num_cpus_upper_bound() -> f64 {
        std::thread::available_parallelism()
            .map(|n| n.get() as f64)
            .unwrap_or(1.0)
    }
}
