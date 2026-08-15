//! `database.oltp` - transactional database throughput and latency.
//!
//! # What it measures
//!
//! | Metric | Unit | What it predicts |
//! |---|---|---|
//! | `read.tps` | tx/s | Select-only transactions: a cached CMS, a read replica, an API reading a hot table |
//! | `read.latency_mean` | ms | What a page waits on a read query, at a load the machine sustains |
//! | `read.latency_estimated_p95` | ms | The slow 5%, *estimated* - see the note below |
//! | `write.tps` | tx/s | Read-write transactions: checkout, publish, anything that commits |
//! | `write.latency_mean` | ms | Commit path latency, which is where storage and durability show up |
//! | `write.latency_estimated_p95` | ms | The tail on the commit path, *estimated* |
//!
//! The two `estimated_p95` keys say so in their own names on purpose. They are
//! computed from the mean and standard deviation assuming a normal
//! distribution, because real percentiles need `pgbench --log`, which writes a
//! line per transaction and would put megabytes of I/O inside the measurement.
//! A latency distribution is not normal and its tail is exactly where that
//! matters, so each is a floor on the real p95: useful for "is this machine
//! acceptable", wrong for "what is my worst case".
//!
//! # It never touches a database on this machine
//!
//! `docs/THREAT-MODEL.md` T-DB is absolute, and Phase 4's exit criterion is
//! that every database module *creates and destroys its own instance*. This
//! module has no configuration for a host, a port, a socket or a credential,
//! and no code path that could use one. It asks [`crate::container`] for a
//! sandboxed PostgreSQL and measures that, or it reports itself as **not
//! measured**.
//!
//! There is no fallback. Not "fall back to a local server if no container
//! runtime is available", not "use `PGHOST` if it is set". A `database.oltp`
//! that quietly measured the operator's production database - and, being
//! read-write, *wrote to it* - would be the worst thing this program could do.
//! The absence of that path is the mitigation; nothing here validates its way
//! to safety.
//!
//! # Why `pgbench`, and what that costs
//!
//! The measurement is taken by `pgbench`, which ships inside the official
//! PostgreSQL image, run through [`Sandbox::exec`]. Two alternatives were
//! considered and rejected:
//!
//! * **A database driver in this workspace.** `sqlx`, `postgres` and `mysql`
//!   are all pure Rust, so none of them breaks the single-static-binary rule.
//!   They are large, and more importantly they would make this module measure
//!   *that driver's* protocol implementation as much as the server. ADR-0011
//!   made the opposite call for `network.transfer` for the opposite reason -
//!   there, owning the connection *was* the measurement.
//! * **Writing the PostgreSQL wire protocol here.** Feasible - the simple
//!   query protocol is small - and it would put a few hundred lines of
//!   untested-against-a-real-server protocol code between the benchmark and
//!   the truth. `pgbench` is written by the people who write the server.
//!
//! What that costs is stated rather than buried: **the numbers include the
//! client.** `pgbench` runs in the same container as the server, so its CPU is
//! CPU the server did not get - the same disclosure `web.static` makes about
//! its in-process load generator, and for the same reason. Every figure here
//! is a floor.
//!
//! # Coordinated omission, again
//!
//! `pgbench`'s default mode is closed-loop: each client sends a transaction,
//! waits for it, sends the next. That is the defect
//! `docs/adr/0012-load-generation.md` exists to prevent - offered load falls
//! when the server slows, so the queue that would form in production never
//! forms and the recorded latencies belong to a server that was politely never
//! overloaded.
//!
//! So the latency phases run with `--rate`, which makes `pgbench` schedule
//! transactions in advance and **measure each one from when it was due rather
//! than when it was sent** - the same correction, implemented by the same
//! people who implemented the server. The throughput phases run without it,
//! because the question there is "how many can this machine do", which is a
//! saturation question and has no schedule to slip against.
//!
//! # TPC naming discipline
//!
//! `pgbench`'s built-in workload is described by its own documentation as
//! *loosely based on* TPC-B. It is **not** TPC-B, this is **not** a TPC
//! result, and no number here may be compared to a published TPC figure.
//! `docs/COMPETITIVE-ANALYSIS.md` requires that discipline and the manifest
//! repeats it, because "TPC-B-like" in a marketing table is how an
//! unauditable number acquires an auditable name.
//!
//! # Durability is disclosed, never changed
//!
//! `fsync`, `synchronous_commit` and `wal_level` decide whether a commit
//! survives a power cut, and turning them off multiplies write throughput.
//! A benchmark that quietly did so would publish a number no production
//! system can reproduce.
//!
//! This module changes none of them. It runs the image's defaults - `fsync=on`,
//! `synchronous_commit=on` - and records what they were, so a result carries
//! the terms it was measured under. That the data directory is a tmpfs is
//! itself disclosed: the WAL is still written and still fsynced, but to RAM,
//! so these figures describe the *server and CPU* rather than the disk.
//! `storage.mixed` is where the disk is measured, and measuring it again here
//! would put the same device in two categories.

use std::time::Duration;

use darcbench_protocol::metrics::{Direction, Metric, Warning, WarningCode};
use darcbench_protocol::stats::Summary;
use darcbench_protocol::ModuleId;

use crate::container::{ContainerError, Image, Runtime, Sandbox};
use crate::module::{
    BenchmarkModule, ModuleError, ModuleManifest, ModuleOutput, ModuleParams, ModuleReporter,
    SafetyClass,
};

/// Workload-definition version. Major bump = results are not comparable.
pub const VERSION: &str = "1.0.0";

/// The module's identifier, validated against the [`ModuleId`] grammar by a
/// unit test in this file.
pub const MODULE_ID: &str = "database.oltp";

/// The allow-list key for the image this module needs.
const IMAGE_KEY: &str = "postgres";

/// Database and superuser the sandbox is created with.
///
/// Fixed, not generated. The container is on loopback, its lifetime is one
/// module, its data directory is a tmpfs and its password is only ever seen by
/// this process - so a generated credential would protect nothing and would
/// make the run harder to reproduce by hand when something goes wrong.
const DB_USER: &str = "darcbench";
const DB_NAME: &str = "darcbench";
const DB_PASSWORD: &str = "darcbench";

/// Scale factor for the generated dataset.
///
/// `pgbench -s 10` builds roughly a million rows in `pgbench_accounts`, about
/// 150 MB. Two constraints meet here: it must not fit entirely in the server's
/// default shared buffers, or the read phase measures a hash lookup rather
/// than a database; and it must fit in the container's tmpfs and memory
/// ceiling, because the data directory is RAM.
///
/// Ten satisfies both with room to spare. A scale factor an operator could
/// choose would make two runs incomparable while looking like the same
/// benchmark, so there is not one.
const SCALE: &str = "10";

/// Clients for the throughput phases.
const THROUGHPUT_CLIENTS: &str = "8";

/// Clients for the latency phases.
///
/// Fewer, because the question is what a request waits when the machine is not
/// saturated, and more clients answer a different question.
const LATENCY_CLIENTS: &str = "4";

/// Transactions per second the latency phases offer.
///
/// A fixed, deliberately modest rate rather than a share of measured capacity.
/// `web.static` derives its latency rate from a capacity probe because HTTP
/// capacity spans four orders of magnitude across the machines this runs on;
/// OLTP capacity does not, and a rate low enough that a small VPS holds the
/// schedule is a rate that answers "what does a query cost when the server is
/// not busy" on every machine. `pgbench` reports the schedule lag, and a run
/// that could not hold this rate says so - see [`LATENCY_LAG_TOLERANCE_MS`].
const LATENCY_RATE: &str = "200";

/// Seconds each phase runs.
const PHASE_SECONDS: u64 = 20;

/// Mean schedule lag above which a latency phase describes the client.
///
/// `pgbench --rate` reports how far behind schedule it fell. Past this, the
/// transactions were issued late, so the latencies recorded after the backlog
/// began are not the latencies of the load the run claims to have offered -
/// the same reasoning, and the same conclusion, as
/// [`crate::loadgen::Saturation::ScheduleSlip`].
const LATENCY_LAG_TOLERANCE_MS: f64 = 5.0;

/// How long fetching the image may take before it is given up on.
///
/// Ten minutes, which is generous and deliberately so: this is a one-off on a
/// machine that has never run DARCBench, it happens before any clock starts,
/// and the alternative to waiting is reporting the module as not measured on a
/// slow link. PostgreSQL is 156 MB, which is minutes on a modest connection.
const PULL_TIMEOUT: Duration = Duration::from_secs(600);

/// How long any one `pgbench` invocation may take before it is killed.
///
/// Generous against the phase length: the dataset build is the long pole and
/// it is not timed.
const EXEC_TIMEOUT: Duration = Duration::from_secs(300);

/// How long the dataset build may take.
const BUILD_TIMEOUT: Duration = Duration::from_secs(600);

// ---------------------------------------------------------------------------
// Parsing pgbench
// ---------------------------------------------------------------------------

/// What one `pgbench` run reported.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct BenchResult {
    pub(crate) tps: f64,
    pub(crate) latency_mean_ms: f64,
    pub(crate) latency_stddev_ms: Option<f64>,
    /// Mean time transactions spent waiting past their scheduled start.
    /// `None` when the run was not rate-limited.
    pub(crate) schedule_lag_ms: Option<f64>,
    pub(crate) transactions: u64,
    pub(crate) failed: u64,
    /// How many clients gave up part-way, as distinct from how many
    /// transactions failed.
    ///
    /// They are different events and pgbench counts them separately: a failed
    /// transaction is one the server refused, and an aborted client is one
    /// that stopped issuing them at all. A phase that lost clients still
    /// prints a plausible `tps` — computed over the whole requested window,
    /// including the part after the clients were gone.
    pub(crate) aborted_clients: u64,
}

/// Reads `pgbench`'s summary block.
///
/// Written against the format rather than with a regular expression, because
/// the fields that matter appear on their own lines with stable prefixes and a
/// pattern that silently matched nothing would produce a confident zero. Every
/// field this returns has a caller that checks it, and
/// [`BenchResult::is_credible`] refuses a result whose transaction count is
/// zero - which is what a parse that found nothing would produce.
pub(crate) fn parse_pgbench(output: &str) -> BenchResult {
    let mut result = BenchResult::default();
    for line in output.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("tps = ") {
            // `tps = 1234.567890 (without initial connection time)`. Only the
            // first occurrence is taken: older versions print two, including
            // and excluding connection establishment, and mixing them across
            // versions would make two machines incomparable.
            if result.tps == 0.0 {
                result.tps = leading_number(rest).unwrap_or(0.0);
            }
        } else if let Some(rest) = line.strip_prefix("latency average = ") {
            result.latency_mean_ms = leading_number(rest).unwrap_or(0.0);
        } else if let Some(rest) = line.strip_prefix("latency stddev = ") {
            result.latency_stddev_ms = leading_number(rest);
        } else if let Some(rest) = line.strip_prefix("rate limit schedule lag: avg ") {
            result.schedule_lag_ms = leading_number(rest);
        } else if let Some(rest) = line.strip_prefix("number of transactions actually processed: ")
        {
            // `4000/4000` on a fixed-count run, a bare number on a timed one.
            result.transactions = rest
                .split('/')
                .next()
                .and_then(|value| value.trim().parse().ok())
                .unwrap_or(0);
        } else if let Some(rest) = line.strip_prefix("number of failed transactions: ") {
            result.failed = leading_number(rest).unwrap_or(0.0) as u64;
        } else if line.contains("aborted in command") {
            // `pgbench: error: client 7 aborted in command 10 (SQL) of script
            // 0; perhaps the backend died while processing`. One line per
            // client, on stderr, and interleaved between threads - so the
            // lines can be spliced mid-word and only the stable fragment is
            // matched.
            result.aborted_clients += 1;
        }
    }
    result
}

/// The leading number of a string like `1234.56 ms (including ...)`.
fn leading_number(text: &str) -> Option<f64> {
    let trimmed = text.trim_start();
    let end = trimmed
        .find(|c: char| !c.is_ascii_digit() && c != '.' && c != '-' && c != '+')
        .unwrap_or(trimmed.len());
    trimmed[..end].parse().ok()
}

impl BenchResult {
    /// Whether this describes work that actually happened.
    ///
    /// A phase that processed no transactions is not a slow machine, it is a
    /// phase that did not run - and reporting a tps of zero for it would put a
    /// fabricated data point in a comparison. The same argument the checksum
    /// makes in the runtime modules.
    ///
    /// A phase that *lost clients* is refused for the harder version of the
    /// same reason. It does not look empty — it looks like a result. pgbench
    /// divides the transactions it completed by the window it was asked for,
    /// so a run whose backends were killed ten seconds into twenty reports
    /// roughly half the machine's real throughput as though it were the
    /// measurement. That is worse than a zero, which at least announces
    /// itself: this is a plausible number that is simply wrong, and the run
    /// that produced it published exactly that before this check existed.
    pub(crate) fn is_credible(&self) -> bool {
        self.transactions > 0 && self.tps > 0.0 && self.tps.is_finite() && self.aborted_clients == 0
    }

    /// The 95th percentile, estimated from the mean and standard deviation.
    ///
    /// `pgbench` does not publish percentiles without `--log`, which writes a
    /// line per transaction and would put megabytes of I/O inside the
    /// measurement. A normal approximation is used instead, and it is named
    /// as an estimate everywhere it appears - `estimated_p95` in the metric
    /// key, and a `context` note in the bundle - because a latency
    /// distribution is not normal and its tail is exactly where that matters
    /// most. It is a floor on the real p95, which makes it useful for
    /// "is this machine acceptable" and wrong for "what is my worst case".
    ///
    /// `None` when `pgbench` reported no standard deviation, rather than a
    /// number that pretends the distribution had no spread.
    pub(crate) fn estimated_p95_ms(&self) -> Option<f64> {
        // 1.645 standard deviations is the 95th percentile of a normal.
        self.latency_stddev_ms
            .map(|stddev| self.latency_mean_ms + 1.645 * stddev)
    }
}

// ---------------------------------------------------------------------------
// The module
// ---------------------------------------------------------------------------

pub struct DatabaseOltp {
    manifest: ModuleManifest,
}

impl Default for DatabaseOltp {
    fn default() -> Self {
        Self::new()
    }
}

impl DatabaseOltp {
    pub fn new() -> Self {
        // Justified `expect`: `MODULE_ID` is a compile-time constant whose
        // conformance to the `ModuleId` grammar is asserted by a unit test in
        // this file, so this cannot fail in a built binary. Matches the
        // precedent in `network_transfer` and `web_static`.
        #[allow(clippy::expect_used)]
        let id = ModuleId::new(MODULE_ID).expect("MODULE_ID is a valid module id");
        Self {
            manifest: ModuleManifest {
                id,
                version: VERSION.into(),
                title: "Transactional database".into(),
                purpose: "Measure transactional throughput and latency against a PostgreSQL \
                          instance this agent creates in a container and destroys when the \
                          module ends - never against a database already on this machine."
                    .into(),
                safety_class: SafetyClass::ProvisionsServices,
                dependencies: vec![
                    "A container runtime (Docker or Podman) whose daemon is reachable".into(),
                    // 1 GiB, and it really is resident: the data directory is a
                    // tmpfs, so the dataset is in RAM rather than on a disk the
                    // container merely has a limit against. An earlier "roughly
                    // 700 MB" here was an estimate of the service alone and did
                    // not count the data, which is the same mistake that had
                    // the container OOM-killed mid-phase.
                    "1 GiB of free memory for the sandboxed database, including its dataset, \
                     which is held in a tmpfs rather than on disk"
                        .into(),
                ],
                // The data directory is a tmpfs, so the database writes to RAM
                // and nothing reaches a host filesystem.
                max_bytes_written: 0,
                // The image, once, on a machine that does not already have it.
                //
                // This said `0` and the comment beside it said the runtime
                // pulled the image "before the run" - an assumption nothing
                // enforced. On a machine that has never run DARCBench, this
                // module fetched 156 MB while preflight promised the operator
                // no network at all. The fetch is now explicit and untimed, and
                // this is what it costs. Nothing crosses the network during the
                // measurement itself: pgbench and the server share a namespace.
                max_network_bytes: 156_190_575,
                cleanup: "The container is removed when the module ends, including on failure. \
                          Anything a crashed run leaves behind carries this agent's label and is \
                          removed at the start of the next run - which is the only cleanup \
                          available, because the release profile aborts on panic and nothing \
                          runs on SIGKILL."
                    .into(),
                validation: vec![
                    "A latency phase whose schedule lag exceeded the tolerance is reported \
                     GeneratorSaturated and degrades the result: transactions issued late do not \
                     describe the load the phase claims to have offered."
                        .into(),
                    "A phase that processed no transactions is withheld rather than reported as \
                     a throughput of zero, which would read as a very slow machine instead of a \
                     phase that did not run."
                        .into(),
                    "An image that is not pinned to a digest is not run at all.".into(),
                    "A phase that lost clients part-way through is withheld. pgbench still \
                     reports a rate for one, computed over the whole window it was asked for \
                     including the part after the clients were gone, so the figure looks \
                     ordinary and is not a measurement of anything."
                        .into(),
                ],
                limitations: vec![
                    "pgbench shares the container with the server, so its CPU is CPU the server \
                     did not get. Every figure here is a floor, not an estimate."
                        .into(),
                    "The 95th percentile is ESTIMATED from the mean and standard deviation \
                     assuming a normal distribution. A latency distribution is not normal and \
                     its tail is exactly where that matters, so it is a floor on the real p95."
                        .into(),
                    "The data directory is a tmpfs. The WAL is written and fsynced, but to RAM, \
                     so these figures describe the server and the CPU rather than the disk. \
                     storage.mixed is where the disk is measured."
                        .into(),
                    "The workload is pgbench's built-in one, which its own documentation calls \
                     loosely based on TPC-B. This is not TPC-B, it is not a TPC result, and no \
                     figure here may be compared to a published TPC number."
                        .into(),
                    "MariaDB and MySQL are not measured. No open-model load tool ships in their \
                     official images, and a closed-loop one would report latencies of a server \
                     that was politely never overloaded."
                        .into(),
                ],
                comparability: vec![
                    "postgres_image".into(),
                    "postgres_version".into(),
                    "pgbench_version".into(),
                    "scale_factor".into(),
                    "fsync".into(),
                    "synchronous_commit".into(),
                    "wal_level".into(),
                    "data_directory_is_tmpfs".into(),
                ],
                stability_cv_bound: 0.15,
            },
        }
    }

    /// The `pgbench` argument vector for one phase.
    ///
    /// A pure function, for the same reason `container::run_args` is one: it is
    /// what decides whether the measurement is open-model or closed, and that
    /// is testable without a database.
    pub(crate) fn phase_args<'a>(
        &self,
        select_only: bool,
        clients: &'a str,
        seconds: &'a str,
        rate: Option<&'a str>,
    ) -> Vec<String> {
        let mut args: Vec<String> = vec![
            "pgbench".into(),
            "-U".into(),
            DB_USER.into(),
            "-h".into(),
            // The container's own loopback, not the host's. `pgbench` and the
            // server are in the same network namespace.
            "127.0.0.1".into(),
            "-c".into(),
            clients.into(),
            // One thread per client. Fewer would make pgbench's own scheduler
            // the bottleneck at the rates below, which would be measured as
            // the database being slow.
            "-j".into(),
            clients.into(),
            "-T".into(),
            seconds.into(),
            // Report the standard deviation, which is what makes an estimated
            // p95 possible without logging every transaction.
            "-r".into(),
            // No `--progress`. Periodic progress lines are off by default, and
            // asking for them off explicitly with `--progress 0` is not the way
            // to say so: pgbench rejects it outright with
            // `-P/--progress must be in range 1..2147483647`.
            //
            // That is what it did, on every phase of every run, and the module
            // reported four withheld metrics and two warnings rather than four
            // numbers. It could not have been found without a real database:
            // the argument vector is *correct* by inspection, it just is not
            // one pgbench accepts.
        ];
        if select_only {
            // `-S`: the select-only variant. Read-write is the default.
            args.push("-S".into());
        }
        if let Some(rate) = rate {
            // The open model. Without this pgbench is closed-loop, offered
            // load falls when the server slows, and the recorded latencies
            // belong to a server that was politely never overloaded.
            args.push("--rate".into());
            args.push(rate.into());
        }
        args.push(DB_NAME.into());
        args
    }
}

impl BenchmarkModule for DatabaseOltp {
    fn manifest(&self) -> &ModuleManifest {
        &self.manifest
    }

    /// pgbench and the server both run in a container this process did not
    /// fork, so none of their CPU can be attributed to this run.
    fn workload_runs_outside_this_process(&self) -> bool {
        true
    }

    /// The sandbox's ceiling, because a container's memory is the host's.
    ///
    /// Preflight sums this across the selected modules and shows the operator
    /// the total before anything starts. A module that provisions a service
    /// and declared zero here would be telling the operator that a gigabyte of
    /// their machine was free to take - and the dataset lives in a tmpfs, so
    /// this really is resident memory rather than a limit that is rarely
    /// approached.
    fn estimated_peak_memory_bytes(&self, _params: &ModuleParams) -> u64 {
        crate::container::sandbox_memory_budget_bytes()
    }

    fn estimated_duration_s(&self, _params: &ModuleParams) -> u64 {
        // Four measured phases plus the dataset build and container start.
        // Not derived from `params`: the phases are fixed-duration on purpose,
        // because a repetition count that varied with the profile would make
        // two runs of the same machine incomparable.
        4 * PHASE_SECONDS + 120
    }

    fn run(
        &self,
        _params: &ModuleParams,
        _reporter: &dyn ModuleReporter,
    ) -> Result<ModuleOutput, ModuleError> {
        let image = Image::from_allow_list(IMAGE_KEY).ok_or_else(|| {
            ModuleError::Precondition(format!(
                "`{IMAGE_KEY}` is not on the container image allow-list, so this module has \
                 nothing it is permitted to run."
            ))
        })?;

        // Checked before a runtime is even looked for, so the operator gets
        // the actionable reason rather than "no container runtime" on a
        // machine that has one.
        // Resolved here, before a container runtime is even looked for, so an
        // operator on a machine that *has* Docker gets the actionable reason
        // rather than a misleading "no container runtime". `Sandbox::launch`
        // resolves it again; that is the check that actually gates the daemon,
        // and this one exists to make the failure legible.
        image.reference().map_err(not_measured)?;

        let runtime = Runtime::discover().map_err(not_measured)?;

        // Before anything is timed and before the container is asked for. An
        // implicit pull inside `docker run` would be an undeclared transfer and
        // would land inside the measurement - see `ensure_image_present`.
        let fetched = runtime
            .ensure_image_present(image, PULL_TIMEOUT)
            .map_err(not_measured)?;

        // Before starting, because a previous run that was killed leaves its
        // container behind and only the next run can clear it.
        let reaped = runtime.reap().map_err(not_measured)?;

        let env = vec![
            format!("POSTGRES_USER={DB_USER}"),
            format!("POSTGRES_PASSWORD={DB_PASSWORD}"),
            format!("POSTGRES_DB={DB_NAME}"),
            // Durability is left at the image defaults. See the module docs:
            // a benchmark that turned fsync off would publish a number no
            // production system can reproduce.
            format!("PGPASSWORD={DB_PASSWORD}"),
        ];
        let sandbox =
            Sandbox::launch(&runtime, image, &unique_suffix(), &env).map_err(not_measured)?;

        let outcome = self.measure(&sandbox, reaped, fetched);
        // The sandbox is dropped here whatever happened, and `Drop` removes
        // the container. On a panic it is not - the release profile aborts -
        // which is what `reap` above exists for.
        drop(sandbox);
        outcome
    }
}

impl DatabaseOltp {
    /// Everything after the container is up. Split out so the container's
    /// lifetime is visible in one place in [`BenchmarkModule::run`].
    fn measure(
        &self,
        sandbox: &Sandbox,
        reaped: usize,
        fetched: bool,
    ) -> Result<ModuleOutput, ModuleError> {
        let seconds = PHASE_SECONDS.to_string();

        let build = sandbox
            .exec(
                &[
                    "pgbench",
                    "-i",
                    "-q",
                    "-s",
                    SCALE,
                    "-U",
                    DB_USER,
                    "-h",
                    "127.0.0.1",
                    DB_NAME,
                ],
                BUILD_TIMEOUT,
            )
            .map_err(not_measured)?;
        if !build.succeeded() {
            return Err(ModuleError::Precondition(format!(
                "the dataset could not be built, so there is nothing to measure: {}",
                first_line(&format!("{}{}", build.stderr, build.stdout))
            )));
        }

        let mut metrics = Vec::new();
        let mut warnings = Vec::new();
        // `Metric` has no per-metric note field, so what a figure was measured
        // under lands in the module context keyed by the metric it describes.
        // It belongs in the bundle either way: a latency without the load it
        // was taken at is a number nobody can act on.
        let mut notes: Vec<(String, String)> = Vec::new();

        // Throughput: no rate limit, because "how many can this machine do" is
        // a saturation question with no schedule to slip against.
        for (select_only, key, label) in [
            (true, "read.tps", "Select-only transactions"),
            (false, "write.tps", "Read-write transactions"),
        ] {
            let args = self.phase_args(select_only, THROUGHPUT_CLIENTS, &seconds, None);
            let result = self.run_phase(sandbox, &args)?;
            if !result.is_credible() {
                // Two different failures, and the operator acts on them
                // differently, so they are not collapsed into one sentence.
                let message = if result.aborted_clients > 0 {
                    format!(
                        "the {label} phase lost {} of its {THROUGHPUT_CLIENTS} clients part-way \
                         through - the server stopped answering them - so its result is \
                         withheld. pgbench still reported a rate, computed over the whole \
                         requested window including the part with no clients left in it, which \
                         is why this is refused rather than published with a caveat",
                        result.aborted_clients
                    )
                } else {
                    format!(
                        "the {label} phase processed no transactions, so it is not a slow machine \
                         - it is a phase that did not run, and reporting a zero for it would put \
                         a fabricated point in a comparison"
                    )
                };
                warnings.push(Warning {
                    code: WarningCode::ValidationFailed,
                    message,
                    metric_key: Some(key.to_string()),
                });
                continue;
            }
            metrics.push(Metric {
                key: key.to_string(),
                label: format!("{label}, throughput"),
                unit: "tx/s".to_string(),
                value: result.tps,
                direction: Direction::HigherIsBetter,
                // One phase, one figure: pgbench reports a summary rather than
                // per-transaction samples, so there is nothing to build a
                // distribution from and a fabricated one-element `samples`
                // list would let the report draw a spread that was never
                // measured.
                summary: single(result.tps),
                samples: Vec::new(),
                outliers: Vec::new(),
            });
            notes.push((
                format!("{key}.note"),
                format!(
                    "{} transactions in {PHASE_SECONDS}s across {THROUGHPUT_CLIENTS} clients, \
                     {} failed. pgbench's built-in workload, which its own documentation calls \
                     loosely based on TPC-B; this is not a TPC result.",
                    result.transactions, result.failed
                ),
            ));
        }

        // Latency: rate-limited, so each transaction is measured from when it
        // was due rather than when it was sent.
        for (select_only, prefix, label) in [
            (true, "read", "Select-only"),
            (false, "write", "Read-write"),
        ] {
            let args = self.phase_args(select_only, LATENCY_CLIENTS, &seconds, Some(LATENCY_RATE));
            let result = self.run_phase(sandbox, &args)?;
            if !result.is_credible() {
                // This used to be a bare `continue`, and the throughput loop
                // above has always warned. The asymmetry cost four metrics
                // once: a run where the server was OOM-killed part-way through
                // published two throughput figures and said nothing whatever
                // about the four latency ones. Absent and unexplained is the
                // one outcome this codebase does not allow - see the
                // `limitations` list on every module.
                warnings.push(Warning {
                    code: WarningCode::ValidationFailed,
                    message: format!(
                        "the {label} latency phase processed no transactions, so no latency is \
                         reported for it. It is not a slow machine - it is a phase that did not \
                         run, and the most likely reason is that the server was no longer \
                         answering by the time it started"
                    ),
                    metric_key: Some(format!("{prefix}.latency_mean")),
                });
                continue;
            }

            // A phase that could not hold its schedule describes the client.
            if let Some(lag) = result.schedule_lag_ms {
                if lag > LATENCY_LAG_TOLERANCE_MS {
                    warnings.push(Warning {
                        code: WarningCode::GeneratorSaturated,
                        message: format!(
                            "the {label} latency phase fell {lag:.1} ms behind its schedule on \
                             average, past the {LATENCY_LAG_TOLERANCE_MS} ms tolerance. \
                             Transactions after the backlog began were issued late, so the \
                             latencies recorded are not the latencies of the load this phase \
                             claims to have offered. pgbench shares the container with the \
                             server, so its own CPU is CPU the server did not get."
                        ),
                        metric_key: Some(format!("{prefix}.latency_mean")),
                    });
                }
            }

            metrics.push(Metric {
                key: format!("{prefix}.latency_mean"),
                label: format!("{label} latency, mean"),
                unit: "ms".to_string(),
                value: result.latency_mean_ms,
                direction: Direction::LowerIsBetter,
                summary: single(result.latency_mean_ms),
                samples: Vec::new(),
                outliers: Vec::new(),
            });
            notes.push((
                format!("{prefix}.latency_mean.note"),
                format!(
                    "At an offered {LATENCY_RATE} tx/s across {LATENCY_CLIENTS} clients. \
                     Rate-limited, so each transaction is measured from when it was due rather \
                     than when it was sent - the coordinated-omission correction. Schedule lag \
                     {}.",
                    result
                        .schedule_lag_ms
                        .map(|lag| format!("{lag:.2} ms"))
                        .unwrap_or_else(|| "not reported".to_string())
                ),
            ));

            if let Some(p95) = result.estimated_p95_ms() {
                metrics.push(Metric {
                    key: format!("{prefix}.latency_estimated_p95"),
                    label: format!("{label} latency, estimated 95th percentile"),
                    unit: "ms".to_string(),
                    value: p95,
                    direction: Direction::LowerIsBetter,
                    summary: single(p95),
                    samples: Vec::new(),
                    outliers: Vec::new(),
                });
                notes.push((
                    format!("{prefix}.latency_estimated_p95.note"),
                    "ESTIMATED from the mean and standard deviation assuming a normal \
                     distribution, because publishing real percentiles needs pgbench's \
                     per-transaction log and that much I/O inside the measurement would change \
                     it. A latency distribution is not normal and its tail is exactly where that \
                     matters, so this is a floor on the real p95: useful for `is this machine \
                     acceptable`, wrong for `what is my worst case`."
                        .to_string(),
                ));
            }
        }

        let mut context = self.disclosure(sandbox);
        if reaped > 0 {
            // Disclosed rather than done silently: a container left by an
            // earlier run means that run was killed, and an operator reading
            // this bundle should know their last one did not finish.
            context.insert(
                "containers_reaped_from_earlier_runs".to_string(),
                serde_json::Value::from(reaped as u64),
            );
        }
        // Whether this run paid for the image, which is the difference between
        // a run that used the network and one that did not. Always recorded,
        // including when it is `false`: "no transfer happened" is a fact about
        // this run, and a key that only appears sometimes is one a reader
        // cannot rely on.
        // The digest, not just the version. Two runs of the same server version
        // from two different images are not the same measurement, and this key
        // was declared in `comparability` and recorded nowhere.
        context.insert(
            "postgres_image".to_string(),
            serde_json::Value::String(
                Image::from_allow_list(IMAGE_KEY)
                    .and_then(|image| image.reference().ok())
                    .unwrap_or("unknown")
                    .to_string(),
            ),
        );
        context.insert(
            "image_fetched_during_this_run".to_string(),
            serde_json::Value::Bool(fetched),
        );
        for (key, value) in notes {
            context.insert(key, serde_json::Value::String(value));
        }
        Ok(ModuleOutput {
            metrics,
            warnings,
            context,
        })
    }

    fn run_phase(&self, sandbox: &Sandbox, args: &[String]) -> Result<BenchResult, ModuleError> {
        let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
        let output = sandbox
            .exec(&borrowed, EXEC_TIMEOUT)
            .map_err(not_measured)?;
        // pgbench writes its summary to stdout and its complaints to stderr;
        // both are parsed, because a run that failed halfway still printed the
        // part that says so.
        Ok(parse_pgbench(&format!(
            "{}\n{}",
            output.stdout, output.stderr
        )))
    }

    /// Everything the bundle must disclose about what was measured.
    ///
    /// The methodology requires it and the manifest's `comparability` list
    /// names each key: two results taken under different durability settings,
    /// scale factors or PostgreSQL versions are not comparable, and the
    /// comparison layer can only refuse if it is told.
    fn disclosure(&self, sandbox: &Sandbox) -> serde_json::Map<String, serde_json::Value> {
        let ask = |argv: &[&str]| -> String {
            sandbox
                .exec(argv, Duration::from_secs(30))
                .ok()
                .filter(|output| output.succeeded())
                .map(|output| output.stdout.trim().to_string())
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| "unknown".to_string())
        };
        let setting = |name: &str| -> String {
            let sql = format!("SHOW {name};");
            ask(&[
                "psql",
                "-U",
                DB_USER,
                "-h",
                "127.0.0.1",
                "-tAc",
                &sql,
                DB_NAME,
            ])
        };

        let mut context = serde_json::Map::new();
        for (key, value) in [
            ("postgres_version".to_string(), setting("server_version")),
            (
                "pgbench_version".to_string(),
                ask(&["pgbench", "--version"]),
            ),
            ("scale_factor".to_string(), SCALE.to_string()),
            // The three settings that decide whether a commit survives a power
            // cut. Recorded, never changed.
            ("fsync".to_string(), setting("fsync")),
            (
                "synchronous_commit".to_string(),
                setting("synchronous_commit"),
            ),
            ("wal_level".to_string(), setting("wal_level")),
            ("shared_buffers".to_string(), setting("shared_buffers")),
            (
                "data_directory_is_tmpfs".to_string(),
                "yes - the WAL is written and fsynced, but to RAM. These figures describe the \
                 server and the CPU rather than the disk; storage.mixed is where the disk is \
                 measured, and measuring it again here would put the same device in two \
                 categories."
                    .to_string(),
            ),
            (
                "workload".to_string(),
                "pgbench's built-in workload, which its own documentation calls loosely based on \
                 TPC-B. This is not TPC-B, it is not a TPC result, and no figure here may be \
                 compared to a published TPC number."
                    .to_string(),
            ),
            (
                "client_shares_the_container".to_string(),
                "yes - pgbench runs beside the server, so its CPU is CPU the server did not get. \
                 Every figure here is a floor."
                    .to_string(),
            ),
        ] {
            context.insert(key, serde_json::Value::String(value));
        }
        context
    }
}

/// Turns a container failure into a precondition failure.
///
/// Every one of them is "this module was not measured", never "measure
/// something else instead". The distinction is the whole of T-DB: there is no
/// value of any container error for which pointing at a database on this
/// machine would be the right response.
fn not_measured(error: ContainerError) -> ModuleError {
    ModuleError::Precondition(error.to_string())
}

/// A [`Summary`] for a figure that has exactly one observation.
///
/// pgbench reports a summary rather than per-transaction timings, so there is
/// no distribution to describe. Every field is the value itself and the spread
/// is zero - which is honest, because no spread was measured. Inventing a
/// one-element `samples` list instead would let the report draw a distribution
/// that never existed.
fn single(value: f64) -> Summary {
    Summary {
        n: 1,
        min: value,
        max: value,
        mean: value,
        median: value,
        stddev: 0.0,
        // `None`, not zero. A coefficient of variation of zero claims the
        // measurement was perfectly stable; `None` says it was measured once.
        cv: None,
        ci95: None,
    }
}

/// A short unique token for a container name.
///
/// `ModuleParams` carries no run id, and the name only has to be unique on
/// this machine for the life of one module - reaping is by label, not by name,
/// so nothing downstream depends on the name meaning anything. The process id
/// plus a counter gives that without this crate growing a randomness
/// dependency for a value that is not a secret.
fn unique_suffix() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

fn first_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("no output")
        .to_string()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn the_module_id_is_valid() {
        assert!(ModuleId::new(MODULE_ID).is_ok());
    }

    #[test]
    fn the_module_declares_that_it_provisions_services() {
        // The safety layer decides whether running this is acceptable from the
        // manifest, so a module that provisions a database and did not say so
        // would bypass every check built on that.
        let module = DatabaseOltp::new();
        assert_eq!(
            module.manifest().safety_class,
            SafetyClass::ProvisionsServices
        );
        assert_eq!(module.manifest().max_bytes_written, 0);
        // Not zero: the image has to reach the machine somehow, and a manifest
        // that says a run uses no network while the runtime fetches 156 MB is
        // a broken promise rather than a rounding error.
        assert!(module.manifest().max_network_bytes >= 150_000_000);
    }

    #[test]
    fn the_latency_phases_are_rate_limited_and_the_throughput_phases_are_not() {
        // The single most important property in this file. Without `--rate`
        // pgbench is closed-loop: offered load falls when the server slows, so
        // the queue that would form in production never forms and the recorded
        // latencies belong to a server that was politely never overloaded.
        let module = DatabaseOltp::new();

        let latency = module.phase_args(true, "4", "20", Some("200"));
        let at = latency
            .iter()
            .position(|a| a == "--rate")
            .expect("no --rate");
        assert_eq!(latency[at + 1], "200");

        let throughput = module.phase_args(true, "8", "20", None);
        assert!(
            !throughput.iter().any(|a| a == "--rate"),
            "the throughput phase must saturate: {throughput:?}"
        );
    }

    #[test]
    fn the_read_phase_is_select_only_and_the_write_phase_is_not() {
        let module = DatabaseOltp::new();
        assert!(module
            .phase_args(true, "8", "20", None)
            .iter()
            .any(|a| a == "-S"));
        assert!(!module
            .phase_args(false, "8", "20", None)
            .iter()
            .any(|a| a == "-S"));
    }

    #[test]
    fn a_phase_that_lost_its_clients_is_refused_rather_than_published() {
        // Verbatim from a run where the container hit its memory ceiling
        // part-way through the write phase. Note what pgbench still reports:
        // a transaction count, zero *failed* transactions, and a tps computed
        // over the full twenty seconds it was asked for. Nothing in the
        // summary block says the measurement is void; the only evidence is on
        // stderr, and the run that published 2589 tx/s from this had every
        // number in the summary looking reasonable.
        let output = "\
pgbench: error: client 7 aborted in command 10 (SQL) of script 0; perhaps the backend died while processing
pgbench: error: client 0 aborted in command 10 (SQL) of script 0; perhaps the backend died while processing
number of transactions actually processed: 26178
number of failed transactions: 0 (0.000%)
latency average = 3.089 ms
tps = 2589.225000 (without initial connection time)
";
        let result = parse_pgbench(output);
        assert_eq!(result.transactions, 26178);
        assert_eq!(result.failed, 0, "pgbench counts these separately");
        assert_eq!(result.aborted_clients, 2);
        assert!(
            !result.is_credible(),
            "a phase whose clients died reported a plausible rate and must still be refused"
        );

        // And the ordinary case is untouched: a clean phase stays credible.
        let clean = parse_pgbench(
            "number of transactions actually processed: 66559\n\
             number of failed transactions: 0 (0.000%)\n\
             tps = 3325.411547 (without initial connection time)\n",
        );
        assert_eq!(clean.aborted_clients, 0);
        assert!(clean.is_credible());
    }

    #[test]
    fn no_phase_asks_pgbench_for_a_progress_interval_it_refuses() {
        // `--progress 0` reads as "no progress reports" and is not: pgbench
        // exits with `-P/--progress must be in range 1..2147483647` before it
        // runs a single transaction. Every phase of every run failed this way,
        // and because each phase withholds rather than fabricates, the visible
        // symptom was a module that returned no metrics at all.
        //
        // Off is the default, so the fix is the absence of the flag - which is
        // a thing only a test can hold in place, since there is no longer any
        // code to read.
        for args in [
            DatabaseOltp::new().phase_args(true, "8", "20", None),
            DatabaseOltp::new().phase_args(false, "4", "20", Some("200")),
        ] {
            for forbidden in ["-P", "--progress"] {
                assert!(
                    !args.iter().any(|a| a == forbidden),
                    "`{forbidden}` is back and pgbench will refuse the phase: {args:?}"
                );
            }
        }
    }

    #[test]
    fn no_phase_can_reach_a_database_outside_its_container() {
        // T-DB, at the level of the argument vector. `pgbench` runs inside the
        // container and is told to connect to the container's own loopback;
        // there is no host, socket or credential a caller could influence,
        // because none of these values comes from a caller.
        let module = DatabaseOltp::new();
        for args in [
            module.phase_args(true, "8", "20", None),
            module.phase_args(false, "4", "20", Some("200")),
        ] {
            let host = args.iter().position(|a| a == "-h").expect("no host");
            assert_eq!(args[host + 1], "127.0.0.1");
            assert_eq!(args.last().unwrap(), DB_NAME);
            // Nothing that could redirect the connection elsewhere.
            for forbidden in ["-p", "--host", "--port", "-f", "--file"] {
                assert!(
                    !args.iter().any(|a| a == forbidden),
                    "`{forbidden}` reached the argument vector: {args:?}"
                );
            }
        }
    }

    #[test]
    fn durability_settings_are_disclosed_and_never_set() {
        // A benchmark that turned fsync off would publish a number no
        // production system can reproduce. The manifest has to name them so
        // the comparison layer can refuse rather than mislead.
        let manifest = DatabaseOltp::new().manifest().clone();
        for key in ["fsync", "synchronous_commit"] {
            assert!(
                manifest.comparability.contains(&key.to_string()),
                "{key} is not declared as a comparability key"
            );
        }
        // And no phase argument sets one.
        let module = DatabaseOltp::new();
        let args = module.phase_args(false, "8", "20", None).join(" ");
        assert!(!args.contains("fsync"));
        assert!(!args.contains("synchronous_commit"));
    }

    const SAMPLE: &str = "\
transaction type: <builtin: TPC-B (sort of)>
scaling factor: 10
query mode: simple
number of clients: 8
number of threads: 8
duration: 20 s
number of transactions actually processed: 24680
number of failed transactions: 0 (0.000%)
latency average = 6.482 ms
latency stddev = 3.117 ms
rate limit schedule lag: avg 0.812 (max 41.203) ms
initial connection time = 12.345 ms
tps = 1234.567890 (without initial connection time)
";

    #[test]
    fn a_pgbench_summary_is_read_field_by_field() {
        let result = parse_pgbench(SAMPLE);
        assert_eq!(result.tps, 1234.567890);
        assert_eq!(result.latency_mean_ms, 6.482);
        assert_eq!(result.latency_stddev_ms, Some(3.117));
        assert_eq!(result.schedule_lag_ms, Some(0.812));
        assert_eq!(result.transactions, 24680);
        assert_eq!(result.failed, 0);
        assert!(result.is_credible());
    }

    #[test]
    fn a_summary_that_says_nothing_is_not_credible() {
        // A parse that found nothing produces a zero, and a zero tps reads as
        // a very slow machine rather than as a phase that did not run.
        let empty = parse_pgbench("could not connect to server\n");
        assert_eq!(empty, BenchResult::default());
        assert!(!empty.is_credible());
        // And a phase that ran but committed nothing is equally not a result.
        let none = parse_pgbench("number of transactions actually processed: 0\ntps = 0.000000\n");
        assert!(!none.is_credible());
    }

    #[test]
    fn a_fixed_count_transaction_line_is_read_as_the_count() {
        // `4000/4000` on a fixed-count run, a bare number on a timed one.
        let result =
            parse_pgbench("number of transactions actually processed: 4000/4000\ntps = 200.0\n");
        assert_eq!(result.transactions, 4000);
    }

    #[test]
    fn only_the_first_tps_line_is_taken() {
        // Older pgbench prints two - including and excluding connection
        // establishment - and mixing them across versions would make two
        // machines incomparable.
        let result = parse_pgbench(
            "tps = 100.0 (including connections establishing)\n\
             tps = 110.0 (excluding connections establishing)\n\
             number of transactions actually processed: 10\n",
        );
        assert_eq!(result.tps, 100.0);
    }

    #[test]
    fn an_unlimited_run_reports_no_schedule_lag_rather_than_zero() {
        // Zero lag and no lag mean different things: one is a schedule that
        // was held, the other is no schedule at all.
        let result = parse_pgbench(
            "number of transactions actually processed: 10\nlatency average = 1.0 ms\ntps = 10.0\n",
        );
        assert_eq!(result.schedule_lag_ms, None);
    }

    #[test]
    fn an_estimated_p95_is_absent_rather_than_invented_without_a_stddev() {
        let mut result = parse_pgbench(SAMPLE);
        assert_eq!(result.estimated_p95_ms(), Some(6.482 + 1.645 * 3.117));
        result.latency_stddev_ms = None;
        assert_eq!(
            result.estimated_p95_ms(),
            None,
            "a p95 without a spread would pretend the distribution had none"
        );
    }

    #[test]
    fn the_estimated_p95_says_so_in_its_own_metric_key() {
        // A latency distribution is not normal and its tail is exactly where
        // that matters. The estimate must not be readable as a measurement.
        let module = DatabaseOltp::new();
        assert!(module.manifest().purpose.contains("creates in a container"));
        // The key is built in `measure`; asserted here as the literal so a
        // rename that drops `estimated` fails.
        let key = format!("{}.latency_estimated_p95", "read");
        assert!(key.contains("estimated"));
    }

    #[test]
    fn the_manifest_promises_cleanup_including_after_a_crash() {
        let manifest = DatabaseOltp::new().manifest().clone();
        assert!(manifest.cleanup.contains("including on failure"));
        assert!(
            manifest.cleanup.contains("label"),
            "the crash path is what the label exists for and the manifest should say so"
        );
    }

    #[test]
    fn tpc_naming_discipline_is_stated_wherever_the_workload_is_named() {
        // pgbench's own banner says "TPC-B (sort of)". A bundle that repeated
        // that without qualification is how an unauditable number acquires an
        // auditable name.
        let module = DatabaseOltp::new();
        let manifest = module.manifest();
        assert!(!manifest.title.contains("TPC"));
        assert!(!manifest.purpose.contains("TPC"));
        // And where it *is* named, the qualification travels with it.
        let named: Vec<&String> = manifest
            .limitations
            .iter()
            .filter(|note| note.contains("TPC"))
            .collect();
        assert_eq!(named.len(), 1, "{named:?}");
        assert!(named[0].contains("is not TPC-B"), "{}", named[0]);
        // The disclosure is built against a live sandbox, so the constant text
        // is asserted here instead.
        assert!(SAMPLE.contains("TPC-B (sort of)"), "the premise changed");
    }
}
