//! `database.cache` - in-memory key-value throughput.
//!
//! # What it measures
//!
//! | Metric | Unit | What it predicts |
//! |---|---|---|
//! | `get.throughput` | ops/s | Cache reads: a session lookup, a rendered fragment, a rate-limit counter |
//! | `set.throughput` | ops/s | Cache writes: populating a page cache, storing a session |
//! | `incr.throughput` | ops/s | Read-modify-write on one key: counters, rate limiters, the contended case |
//! | `pipelined.throughput` | ops/s | The same GETs sixteen at a time - what batching is worth on this machine |
//! | `roundtrip.unloaded_mean` | ms | Round-trip on an otherwise idle server: the floor under every cache hit |
//! | `roundtrip.unloaded_max` | ms | The worst round-trip observed while idle - scheduler and allocator jitter |
//!
//! # There is no latency-under-load metric here, on purpose
//!
//! This is the decision worth reading before anything else in this file.
//!
//! `redis-benchmark` is **closed-loop**: each client sends a command, waits for
//! the reply, sends the next. There is no `--rate`, no scheduled arrival, no
//! equivalent of `pgbench`'s throttled mode. That is exactly the shape
//! `docs/adr/0012-load-generation.md` exists to reject - when the server slows,
//! the generator slows with it, the queue that would form in production never
//! forms, and the recorded latencies belong to a server that was politely never
//! overloaded.
//!
//! `redis-benchmark` *does* print `p50`, `p95` and `p99` columns, and they are
//! easy to parse and would look authoritative in a report. They are not
//! reported here. A percentile taken under a closed loop is a percentile of a
//! different experiment, and publishing one beside `web.static`'s
//! coordinated-omission-corrected figures would invite exactly the comparison
//! that makes it wrong.
//!
//! So this module reports:
//!
//! * **throughput under saturation**, which is what a closed loop measures
//!   correctly - "how many operations can this machine do" has no schedule to
//!   fall behind; and
//! * **round-trip latency on an idle server** (`redis-cli --latency`), which is
//!   a floor rather than a distribution, and is named `unloaded` in the metric
//!   key so it cannot be read as anything else.
//!
//! An honest gap is better than a confident number from the wrong experiment.
//! Closing it needs an open-model client speaking RESP, which belongs with the
//! same work that would let `database.oltp` measure MariaDB.
//!
//! # It never touches a cache on this machine
//!
//! `docs/THREAT-MODEL.md` T-DB, exactly as in [`crate::database_oltp`]: no
//! host, port, socket or password setting exists, and no code path could use
//! one. The module asks [`crate::container`] for a sandboxed Valkey and
//! measures that, or it reports itself as **not measured**.
//!
//! That matters more here than it looks. A Redis on a production host is
//! usually the session store, and `redis-benchmark` writes keys - a run pointed
//! at one would evict live sessions to make room for its own. There is no
//! fallback path for that to happen through.
//!
//! # Persistence is disclosed, never changed
//!
//! `appendonly` and `save` decide whether the dataset survives a restart, and
//! turning them off raises write throughput. The image's defaults are used and
//! recorded, for the same reason `database.oltp` records `fsync`: a benchmark
//! that quietly disabled durability publishes a number no production system can
//! reproduce.
//!
//! The data directory is a tmpfs, and that is disclosed too - an RDB snapshot
//! is still written, but to RAM, so these figures describe the server and the
//! CPU rather than the disk.

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
pub const MODULE_ID: &str = "database.cache";

/// The allow-list key for the image this module needs.
const IMAGE_KEY: &str = "valkey";

/// Concurrent clients.
///
/// Fifty is `redis-benchmark`'s own default and is enough to keep a
/// single-threaded event loop busy on any machine this runs on. More would
/// measure how well the *client* multiplexes, which is not the question.
const CLIENTS: &str = "50";

/// Requests per measured command.
///
/// A hundred thousand: long enough that connection setup and the first
/// allocation are a rounding error, short enough that the whole module fits in
/// the time budget its manifest declares. A duration-based bound would make two
/// machines do different amounts of work, which is fine for a rate and not fine
/// for the key-space behaviour below.
const REQUESTS: &str = "100000";

/// Distinct keys the workload touches.
///
/// Without this, `redis-benchmark` hammers a single key - which measures one
/// hash bucket and a CPU cache line rather than a data structure. Ten thousand
/// keys at the value size below is a few megabytes: comfortably resident, which
/// is correct, because a cache that has started swapping is not a cache.
const KEYSPACE: &str = "10000";

/// Value size in bytes.
///
/// Small on purpose. A cache workload is dominated by round-trips and command
/// dispatch, not by copying; a large value would turn this into a memory
/// bandwidth measurement, which `memory.bandwidth` already makes directly and
/// scores under Memory. Measuring it again here would count the same silicon in
/// two categories.
const VALUE_BYTES: &str = "64";

/// Depth of the pipelined phase.
///
/// Sixteen. The point is the *difference* from the unpipelined phases: it is
/// how much of a cache's cost is syscalls and round-trips rather than the data
/// structure, which is the single most actionable thing an operator can learn
/// here - it is the difference between "buy a faster machine" and "batch your
/// calls".
const PIPELINE: &str = "16";

/// How many round-trip samples the unloaded latency probe takes.
const LATENCY_SECONDS: &str = "5";

/// How long fetching the image may take before it is given up on.
///
/// Ten minutes, which is generous and deliberately so: this is a one-off on a
/// machine that has never run DARCBench, it happens before any clock starts,
/// and the alternative to waiting is reporting the module as not measured on a
/// slow link.
const PULL_TIMEOUT: Duration = Duration::from_secs(600);

/// How long any one measurement may take before it is killed.
const EXEC_TIMEOUT: Duration = Duration::from_secs(180);

// ---------------------------------------------------------------------------
// Parsing redis-benchmark
// ---------------------------------------------------------------------------

/// One command's result from `redis-benchmark --csv`.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct CommandResult {
    pub(crate) name: String,
    pub(crate) ops_per_second: f64,
}

/// Reads the CSV `redis-benchmark` prints.
///
/// The format has grown columns across versions - modern builds emit
/// `"test","rps","avg_latency_ms",...` with a header row, older ones emit
/// `"GET","123456.78"` with none. Only the first two fields are read, because
/// they are the two that have never moved, and the latency columns are
/// deliberately ignored: see the module documentation for why a closed-loop
/// percentile is not published here.
///
/// A header row is recognised and skipped by its literal first field rather
/// than by position, so a future version that adds a row before it does not
/// silently turn the header into a result.
pub(crate) fn parse_csv(output: &str) -> Vec<CommandResult> {
    let mut results = Vec::new();
    for line in output.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut fields = line.split(',').map(|field| field.trim().trim_matches('"'));
        let Some(name) = fields.next() else {
            continue;
        };
        if name.eq_ignore_ascii_case("test") {
            continue;
        }
        let Some(rps) = fields.next().and_then(|value| value.parse::<f64>().ok()) else {
            // A line that is not a result row - a warning, a blank, a banner.
            // Skipped rather than counted as zero, which would read as a
            // command the server could not perform at all.
            continue;
        };
        if rps.is_finite() && rps > 0.0 {
            results.push(CommandResult {
                name: name.to_string(),
                ops_per_second: rps,
            });
        }
    }
    results
}

/// What `redis-cli --latency` reported.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct LatencyProbe {
    pub(crate) min_ms: f64,
    pub(crate) max_ms: f64,
    pub(crate) mean_ms: f64,
    pub(crate) samples: u64,
}

/// Reads whichever of `redis-cli --latency`'s two output formats arrived.
///
/// # There are two, and this program only ever sees the second
///
/// Attached to a terminal the tool redraws one line in place and prints
/// `min: 0, max: 3, avg: 0.09 (1234 samples)`. Attached to a pipe it prints
/// neither the labels nor the redraw — just `0 2 0.23 471`, once, on exit.
///
/// Every invocation from this module goes through [`Sandbox::exec`], which
/// captures stdout, so the labelled format is the one it will *never* get.
/// This parser was written against it anyway, and the result was a probe that
/// returned zero samples on every run on every machine — found by driving the
/// module against a real daemon, because there is no way to find it without
/// one. A fixture written from the documentation agrees with a parser written
/// from the documentation.
///
/// The labelled form is still parsed, and deliberately first: it costs four
/// lines, and someone will eventually run this attached to a terminal.
///
/// Written against the format rather than with a regular expression, for the
/// same reason as the pgbench parser: a pattern that silently matched nothing
/// would produce a confident zero, and a round-trip of zero milliseconds reads
/// as an extraordinarily fast machine rather than as a probe that did not run.
/// [`LatencyProbe::is_credible`] is what catches that — and it is what turned
/// this defect into a withheld metric rather than a fabricated one.
pub(crate) fn parse_latency(output: &str) -> LatencyProbe {
    let mut probe = LatencyProbe::default();
    // The tool redraws one line in place; the last complete one is the result.
    if let Some(line) = output
        .lines()
        .map(str::trim)
        .rfind(|line| line.contains("avg:"))
    {
        for part in line.split(',') {
            let part = part.trim();
            if let Some(rest) = part.strip_prefix("min:") {
                probe.min_ms = leading_number(rest).unwrap_or(0.0);
            } else if let Some(rest) = part.strip_prefix("max:") {
                probe.max_ms = leading_number(rest).unwrap_or(0.0);
            } else if let Some(rest) = part.strip_prefix("avg:") {
                probe.mean_ms = leading_number(rest).unwrap_or(0.0);
                if let Some(open) = rest.find('(') {
                    probe.samples = leading_number(&rest[open + 1..]).unwrap_or(0.0) as u64;
                }
            }
        }
        return probe;
    }

    // The piped format: `min max avg samples`, and nothing else on the line.
    // Requiring exactly four fields is what keeps this from matching a stray
    // line of a banner or an error - a looser match here would resurrect the
    // confident zero the labelled parser was careful to avoid.
    let Some(fields) = output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|line| {
            line.split_whitespace()
                .map(str::parse::<f64>)
                .collect::<Result<Vec<f64>, _>>()
        })
        .filter_map(Result::ok)
        .rfind(|fields| fields.len() == 4)
    else {
        return probe;
    };
    probe.min_ms = fields[0];
    probe.max_ms = fields[1];
    probe.mean_ms = fields[2];
    // Negative or fractional sample counts are not a thing the tool emits, and
    // `as u64` on a negative float saturates to zero rather than wrapping - so
    // a malformed line lands on "not credible" rather than on a huge count.
    probe.samples = if fields[3] >= 0.0 {
        fields[3] as u64
    } else {
        0
    };
    probe
}

impl LatencyProbe {
    /// Whether this describes a probe that actually ran.
    ///
    /// A mean of zero with no samples is a parse that found nothing, and
    /// reporting it would put an extraordinarily fast machine in a comparison.
    pub(crate) fn is_credible(&self) -> bool {
        self.samples > 0 && self.mean_ms.is_finite() && self.max_ms >= self.mean_ms
    }
}

fn leading_number(text: &str) -> Option<f64> {
    let trimmed = text.trim_start();
    let end = trimmed
        .find(|c: char| !c.is_ascii_digit() && c != '.' && c != '-' && c != '+')
        .unwrap_or(trimmed.len());
    trimmed[..end].parse().ok()
}

// ---------------------------------------------------------------------------
// The module
// ---------------------------------------------------------------------------

/// One measured command: what to ask `redis-benchmark` for, and what to call it.
struct Shape {
    /// The `-t` argument.
    command: &'static str,
    /// Metric key.
    key: &'static str,
    label: &'static str,
    /// Pipeline depth, when this shape batches.
    pipeline: Option<&'static str>,
}

const SHAPES: &[Shape] = &[
    Shape {
        command: "get",
        key: "get.throughput",
        label: "GET",
        pipeline: None,
    },
    Shape {
        command: "set",
        key: "set.throughput",
        label: "SET",
        pipeline: None,
    },
    Shape {
        command: "incr",
        key: "incr.throughput",
        label: "INCR",
        pipeline: None,
    },
    Shape {
        command: "get",
        key: "pipelined.throughput",
        label: "GET, pipelined 16 deep",
        pipeline: Some(PIPELINE),
    },
];

pub struct DatabaseCache {
    manifest: ModuleManifest,
}

impl Default for DatabaseCache {
    fn default() -> Self {
        Self::new()
    }
}

impl DatabaseCache {
    pub fn new() -> Self {
        // Justified `expect`: `MODULE_ID` is a compile-time constant whose
        // conformance to the `ModuleId` grammar is asserted by a unit test in
        // this file, so this cannot fail in a built binary.
        #[allow(clippy::expect_used)]
        let id = ModuleId::new(MODULE_ID).expect("MODULE_ID is a valid module id");
        Self {
            manifest: ModuleManifest {
                id,
                version: VERSION.into(),
                title: "In-memory cache".into(),
                purpose: "Measure key-value throughput against a Valkey instance this agent \
                          creates in a container and destroys when the module ends - never \
                          against a cache already on this machine."
                    .into(),
                safety_class: SafetyClass::ProvisionsServices,
                dependencies: vec![
                    "A container runtime (Docker or Podman) whose daemon is reachable".into(),
                    // The ceiling the isolation tier applies, not what a
                    // 10,000-key cache needs, which is a rounding error beside
                    // it. Preflight is where an operator agrees to the largest
                    // amount this could take, not the likeliest.
                    "1 GiB of free memory, which is the sandbox's ceiling rather than what this \
                     workload uses"
                        .into(),
                ],
                max_bytes_written: 0,
                // The image, once, on a machine that does not already have it.
                // See `database.oltp` for why this is no longer zero.
                max_network_bytes: 17_456_505,
                cleanup: "The container is removed when the module ends, including on failure. \
                          Anything a crashed run leaves behind carries this agent's label and is \
                          removed at the start of the next run."
                    .into(),
                validation: vec![
                    "A command that reported no operations per second is withheld rather than \
                     published as a zero, which would read as a server that could not perform it \
                     at all."
                        .into(),
                    "The unloaded round-trip probe is withheld unless it collected samples; a \
                     mean of zero from a parse that found nothing would read as an \
                     extraordinarily fast machine."
                        .into(),
                    "An image that is not pinned to a digest is not run at all.".into(),
                ],
                limitations: vec![
                    "There is no latency-under-load metric. redis-benchmark is closed-loop and \
                     has no rate limiter, so its p50/p95/p99 columns are percentiles of an \
                     experiment where offered load fell whenever the server slowed. They are \
                     parsed by nothing here and published nowhere."
                        .into(),
                    "`roundtrip.unloaded_*` is measured on an otherwise idle server. It is a \
                     floor under every cache hit, not a distribution, and the metric key says \
                     `unloaded` so it cannot be read as one."
                        .into(),
                    "redis-benchmark shares the container with the server, so its CPU is CPU the \
                     server did not get. Every figure here is a floor."
                        .into(),
                    "The data directory is a tmpfs. An RDB snapshot is still written, but to \
                     RAM, so these figures describe the server and the CPU rather than the disk."
                        .into(),
                    "Redis proper is not measured. Valkey is the fork the major distributions \
                     and cloud providers moved to after the 2024 licence change, and running \
                     both would double the runtime to measure two servers that are, at this \
                     workload, the same program."
                        .into(),
                ],
                comparability: vec![
                    "valkey_image".into(),
                    "valkey_version".into(),
                    "clients".into(),
                    "value_bytes".into(),
                    "keyspace".into(),
                    "appendonly".into(),
                    "save".into(),
                    "io_threads".into(),
                    "data_directory_is_tmpfs".into(),
                ],
                stability_cv_bound: 0.15,
            },
        }
    }

    /// The `redis-benchmark` argument vector for one shape.
    ///
    /// A pure function, for the same reason `container::run_args` is one: it
    /// decides what is measured and where it is measured against, and both are
    /// testable without a server.
    fn shape_args(&self, shape: &Shape) -> Vec<String> {
        let mut args: Vec<String> = vec![
            "redis-benchmark".into(),
            // The container's own loopback. redis-benchmark and the server are
            // in the same network namespace.
            "-h".into(),
            "127.0.0.1".into(),
            "-c".into(),
            CLIENTS.into(),
            "-n".into(),
            REQUESTS.into(),
            "-d".into(),
            VALUE_BYTES.into(),
            // Spread over a key space, so this measures a data structure
            // rather than one hash bucket and a CPU cache line.
            "-r".into(),
            KEYSPACE.into(),
            "-t".into(),
            shape.command.into(),
            // Machine-readable. The human format redraws in place and would
            // have to be parsed out of carriage returns.
            "--csv".into(),
        ];
        if let Some(depth) = shape.pipeline {
            args.push("-P".into());
            args.push(depth.into());
        }
        args
    }

    /// The unloaded round-trip probe.
    fn latency_args(&self) -> Vec<String> {
        vec![
            "redis-cli".into(),
            "-h".into(),
            "127.0.0.1".into(),
            "--latency".into(),
            "-i".into(),
            LATENCY_SECONDS.into(),
        ]
    }
}

impl BenchmarkModule for DatabaseCache {
    fn manifest(&self) -> &ModuleManifest {
        &self.manifest
    }

    /// redis-benchmark and the server both run in a container this process did
    /// not fork, so none of their CPU can be attributed to this run.
    fn workload_runs_outside_this_process(&self) -> bool {
        true
    }

    /// The sandbox's ceiling, for the same reason `database.oltp` declares it:
    /// a container's memory is memory on the host, and preflight is where the
    /// operator agrees to it.
    ///
    /// The same figure as `database.oltp` even though a 10,000-key cache is
    /// far smaller than a pgbench dataset. The budget is a property of the
    /// isolation tier, not of the workload, and quoting the tier's ceiling is
    /// the honest answer to "how much of my machine could this take".
    fn estimated_peak_memory_bytes(&self, _params: &ModuleParams) -> u64 {
        crate::container::sandbox_memory_budget_bytes()
    }

    fn estimated_duration_s(&self, _params: &ModuleParams) -> u64 {
        // Four throughput shapes plus the latency probe and container start.
        // Fixed rather than derived from `params`: a repetition count that
        // varied with the profile would make two runs of the same machine
        // incomparable.
        90
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
        // Resolved before a runtime is looked for, so an operator on a machine
        // that *has* Docker gets the actionable reason rather than a
        // misleading "no container runtime".
        image.reference().map_err(not_measured)?;

        let runtime = Runtime::discover().map_err(not_measured)?;

        // Before anything is timed and before the container is asked for. An
        // implicit pull inside `docker run` would be an undeclared transfer and
        // would land inside the measurement - see `ensure_image_present`.
        let fetched = runtime
            .ensure_image_present(image, PULL_TIMEOUT)
            .map_err(not_measured)?;
        let reaped = runtime.reap().map_err(not_measured)?;

        // No environment at all. Valkey's image needs no credential to start,
        // and the container is on loopback for the length of one module - so a
        // password here would protect nothing and would be one more value in
        // an argument vector.
        let sandbox =
            Sandbox::launch(&runtime, image, &unique_suffix(), &[]).map_err(not_measured)?;
        let outcome = self.measure(&sandbox, reaped, fetched);
        drop(sandbox);
        outcome
    }
}

impl DatabaseCache {
    fn measure(
        &self,
        sandbox: &Sandbox,
        reaped: usize,
        fetched: bool,
    ) -> Result<ModuleOutput, ModuleError> {
        let mut metrics = Vec::new();
        let mut warnings = Vec::new();

        for shape in SHAPES {
            let args = self.shape_args(shape);
            let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
            let output = sandbox
                .exec(&borrowed, EXEC_TIMEOUT)
                .map_err(not_measured)?;
            let results = parse_csv(&format!("{}\n{}", output.stdout, output.stderr));

            // `-t get` can print more than one row on some versions. The row
            // whose name matches the command is taken rather than the first,
            // so a version that adds a summary line does not get measured
            // instead of the command.
            let found = results
                .iter()
                .find(|result| result.name.eq_ignore_ascii_case(shape.command))
                .or_else(|| results.first());

            match found {
                Some(result) => metrics.push(Metric {
                    key: shape.key.into(),
                    label: format!("{}, throughput", shape.label),
                    unit: "ops/s".into(),
                    value: result.ops_per_second,
                    direction: Direction::HigherIsBetter,
                    summary: single(result.ops_per_second),
                    samples: Vec::new(),
                    outliers: Vec::new(),
                }),
                None => warnings.push(Warning {
                    code: WarningCode::ValidationFailed,
                    message: format!(
                        "the {} phase reported no operations per second, so it is withheld \
                         rather than published as a zero - which would read as a server that \
                         could not perform the command at all rather than a phase that did not \
                         run.",
                        shape.label
                    ),
                    metric_key: Some(shape.key.to_string()),
                }),
            }
        }

        // The unloaded round-trip. Last, so nothing this module started is
        // still loading the server while it runs - the whole point of the
        // metric is that the server is idle.
        let args = self.latency_args();
        let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
        let probe = sandbox
            .exec(&borrowed, EXEC_TIMEOUT)
            .map(|output| parse_latency(&format!("{}\n{}", output.stdout, output.stderr)))
            .unwrap_or_default();

        if probe.is_credible() {
            metrics.push(Metric {
                key: "roundtrip.unloaded_mean".into(),
                label: "Round-trip on an idle server, mean".into(),
                unit: "ms".into(),
                value: probe.mean_ms,
                direction: Direction::LowerIsBetter,
                summary: single(probe.mean_ms),
                samples: Vec::new(),
                outliers: Vec::new(),
            });
            metrics.push(Metric {
                key: "roundtrip.unloaded_max".into(),
                label: "Round-trip on an idle server, worst observed".into(),
                unit: "ms".into(),
                value: probe.max_ms,
                direction: Direction::LowerIsBetter,
                summary: single(probe.max_ms),
                samples: Vec::new(),
                outliers: Vec::new(),
            });
        } else {
            warnings.push(Warning {
                code: WarningCode::ValidationFailed,
                message: "the unloaded round-trip probe collected no samples, so it is withheld. \
                          A mean of zero from a parse that found nothing would read as an \
                          extraordinarily fast machine."
                    .into(),
                metric_key: Some("roundtrip.unloaded_mean".into()),
            });
        }

        let mut context = self.disclosure(sandbox, &probe);
        if reaped > 0 {
            // Disclosed rather than done silently: a container left by an
            // earlier run means that run was killed, and an operator reading
            // this bundle should know their last one did not finish.
            context.insert(
                "containers_reaped_from_earlier_runs".into(),
                serde_json::Value::from(reaped as u64),
            );
        }
        // Whether this run paid for the image. Always recorded, including when
        // it is `false`: a key that only appears sometimes is one a reader
        // cannot rely on.
        context.insert(
            "image_fetched_during_this_run".into(),
            serde_json::Value::Bool(fetched),
        );

        Ok(ModuleOutput {
            metrics,
            warnings,
            context,
        })
    }

    /// Everything the bundle must disclose about what was measured.
    fn disclosure(
        &self,
        sandbox: &Sandbox,
        probe: &LatencyProbe,
    ) -> serde_json::Map<String, serde_json::Value> {
        let config = |name: &str| -> String {
            sandbox
                .exec(
                    &["redis-cli", "-h", "127.0.0.1", "config", "get", name],
                    Duration::from_secs(30),
                )
                .ok()
                .filter(|output| output.succeeded())
                // `CONFIG GET` answers with the name and then the value; the
                // value is the second line.
                .and_then(|output| {
                    output
                        .stdout
                        .lines()
                        .nth(1)
                        .map(|value| value.trim().to_string())
                })
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| "unknown".to_string())
        };

        let version = sandbox
            .exec(
                &["redis-cli", "-h", "127.0.0.1", "info", "server"],
                Duration::from_secs(30),
            )
            .ok()
            .and_then(|output| {
                output
                    .stdout
                    .lines()
                    .find_map(|line| {
                        line.trim()
                            .strip_prefix("valkey_version:")
                            .map(str::to_string)
                    })
                    .or_else(|| {
                        output.stdout.lines().find_map(|line| {
                            line.trim()
                                .strip_prefix("redis_version:")
                                .map(str::to_string)
                        })
                    })
            })
            .unwrap_or_else(|| "unknown".to_string());

        let mut context = serde_json::Map::new();
        for (key, value) in [
            ("valkey_version".to_string(), version),
            ("clients".to_string(), CLIENTS.to_string()),
            ("requests_per_command".to_string(), REQUESTS.to_string()),
            ("value_bytes".to_string(), VALUE_BYTES.to_string()),
            ("keyspace".to_string(), KEYSPACE.to_string()),
            ("pipeline_depth".to_string(), PIPELINE.to_string()),
            // The two settings that decide whether the dataset survives a
            // restart. Recorded, never changed.
            ("appendonly".to_string(), config("appendonly")),
            ("save".to_string(), config("save")),
            ("maxmemory".to_string(), config("maxmemory")),
            ("maxmemory_policy".to_string(), config("maxmemory-policy")),
            // Valkey 8 can serve I/O on several threads, which changes what
            // these numbers mean by a large factor.
            ("io_threads".to_string(), config("io-threads")),
            (
                "roundtrip_probe_samples".to_string(),
                probe.samples.to_string(),
            ),
            (
                "data_directory_is_tmpfs".to_string(),
                "yes - an RDB snapshot is still written, but to RAM. These figures describe the \
                 server and the CPU rather than the disk."
                    .to_string(),
            ),
            (
                "latency_under_load".to_string(),
                "not measured. redis-benchmark is closed-loop and has no rate limiter, so its \
                 percentile columns describe an experiment in which offered load fell whenever \
                 the server slowed. Publishing one beside a coordinated-omission-corrected \
                 figure would invite exactly the comparison that makes it wrong."
                    .to_string(),
            ),
            (
                "client_shares_the_container".to_string(),
                "yes - redis-benchmark runs beside the server, so its CPU is CPU the server did \
                 not get. Every figure here is a floor."
                    .to_string(),
            ),
        ] {
            context.insert(key, serde_json::Value::String(value));
        }
        context
    }
}

/// A [`Summary`] for a figure that has exactly one observation.
///
/// `redis-benchmark` reports a rate rather than per-operation timings, so there
/// is no distribution to describe. `cv` is `None` rather than zero: zero claims
/// the measurement was perfectly stable, `None` says it was measured once.
fn single(value: f64) -> Summary {
    Summary {
        n: 1,
        min: value,
        max: value,
        mean: value,
        median: value,
        stddev: 0.0,
        cv: None,
        ci95: None,
    }
}

/// Turns a container failure into a precondition failure.
///
/// Every one of them is "this module was not measured", never "measure
/// something else instead". A Redis on a production host is usually the session
/// store, and this workload writes keys - so there is no value of any container
/// error for which pointing at a cache on this machine would be acceptable.
fn not_measured(error: ContainerError) -> ModuleError {
    ModuleError::Precondition(error.to_string())
}

/// A short unique token for a container name. See `database_oltp`.
fn unique_suffix() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
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
        let module = DatabaseCache::new();
        assert_eq!(
            module.manifest().safety_class,
            SafetyClass::ProvisionsServices
        );
        assert_eq!(module.manifest().max_bytes_written, 0);
        assert!(module.manifest().max_network_bytes >= 17_000_000);
    }

    #[test]
    fn no_phase_can_reach_a_cache_outside_its_container() {
        // T-DB at the level of the argument vector. A Redis on a production
        // host is usually the session store and this workload writes keys, so
        // a run pointed at one would evict live sessions to make room.
        let module = DatabaseCache::new();
        let mut vectors: Vec<Vec<String>> = SHAPES.iter().map(|s| module.shape_args(s)).collect();
        vectors.push(module.latency_args());
        for args in vectors {
            let host = args.iter().position(|a| a == "-h").expect("no host");
            assert_eq!(args[host + 1], "127.0.0.1");
            for forbidden in ["-p", "--user", "-a", "--pass", "-u", "--cluster"] {
                assert!(
                    !args.iter().any(|a| a == forbidden),
                    "`{forbidden}` reached the argument vector: {args:?}"
                );
            }
        }
    }

    #[test]
    fn no_closed_loop_percentile_is_ever_asked_for_or_reported() {
        // The decision this module rests on. redis-benchmark prints p50/p95/p99
        // columns that are easy to parse and would look authoritative; they are
        // percentiles of an experiment in which offered load fell whenever the
        // server slowed.
        let module = DatabaseCache::new();
        let manifest = module.manifest();

        // Nothing in the metric vocabulary offers a loaded percentile.
        for shape in SHAPES {
            assert!(!shape.key.contains("p50"), "{}", shape.key);
            assert!(!shape.key.contains("p95"), "{}", shape.key);
            assert!(!shape.key.contains("p99"), "{}", shape.key);
            assert!(!shape.key.contains("latency"), "{}", shape.key);
        }
        // The only latency keys say `unloaded` in their own names.
        for key in ["roundtrip.unloaded_mean", "roundtrip.unloaded_max"] {
            assert!(key.contains("unloaded"));
        }
        // And the gap is declared rather than left to be noticed.
        assert!(
            manifest
                .limitations
                .iter()
                .any(|note| note.contains("closed-loop") && note.contains("published nowhere")),
            "the missing latency-under-load metric must be a declared limitation"
        );
    }

    #[test]
    fn the_parser_reads_only_the_two_columns_that_never_moved() {
        // Modern builds emit a header and eight columns; older ones emit two
        // and no header. Both must parse, and the latency columns must be
        // ignored rather than read into something.
        let modern = "\"test\",\"rps\",\"avg_latency_ms\",\"min_latency_ms\",\"p50_latency_ms\",\
                      \"p95_latency_ms\",\"p99_latency_ms\",\"max_latency_ms\"\n\
                      \"GET\",\"148588.41\",\"0.167\",\"0.032\",\"0.159\",\"0.255\",\"0.399\",\"1.911\"\n";
        let results = parse_csv(modern);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "GET");
        assert_eq!(results[0].ops_per_second, 148588.41);

        let old = "\"SET\",\"120481.93\"\n\"GET\",\"131233.60\"\n";
        let results = parse_csv(old);
        assert_eq!(results.len(), 2);
        assert_eq!(results[1].ops_per_second, 131233.60);
    }

    #[test]
    fn a_header_row_is_never_read_as_a_result() {
        // Recognised by its literal first field rather than by position, so a
        // future version that prints a banner first does not turn the header
        // into a measurement.
        let results = parse_csv("some banner line\n\"test\",\"rps\"\n\"GET\",\"100.0\"\n");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "GET");
    }

    #[test]
    fn a_command_that_reported_nothing_produces_no_result_rather_than_a_zero() {
        // A zero ops/s reads as a server that could not perform the command at
        // all, rather than as a phase that did not run.
        assert!(parse_csv("Error: Connection refused\n").is_empty());
        assert!(parse_csv("\"GET\",\"0\"\n").is_empty());
        assert!(parse_csv("").is_empty());
    }

    #[test]
    fn the_latency_probe_reads_the_last_complete_line() {
        // The tool redraws one line in place, so the output holds every
        // intermediate state and only the last one is the result.
        let output = "min: 0, max: 1, avg: 0.05 (100 samples)\n\
                      min: 0, max: 3, avg: 0.09 (4213 samples)\n";
        let probe = parse_latency(output);
        assert_eq!(probe.min_ms, 0.0);
        assert_eq!(probe.max_ms, 3.0);
        assert_eq!(probe.mean_ms, 0.09);
        assert_eq!(probe.samples, 4213);
        assert!(probe.is_credible());
    }

    #[test]
    fn the_latency_probe_reads_the_format_a_pipe_actually_gets() {
        // Verbatim from `docker exec valkey redis-cli --latency -i 5` with
        // stdout captured, which is the only way this module ever invokes it.
        // No labels, no redraw, one line: min, max, avg, samples.
        //
        // The parser was written from the documented format and returned zero
        // samples from this on every run on every machine. It was found by
        // running the module against a real daemon and not before, because a
        // fixture written from the documentation agrees with a parser written
        // from the documentation. This test is that fixture replaced with an
        // observation.
        let probe = parse_latency("0 2 0.23 471\n");
        assert_eq!(probe.min_ms, 0.0);
        assert_eq!(probe.max_ms, 2.0);
        assert_eq!(probe.mean_ms, 0.23);
        assert_eq!(probe.samples, 471);
        assert!(probe.is_credible());
    }

    #[test]
    fn a_bare_line_that_is_not_the_result_is_not_read_as_one() {
        // The piped format has no labels to key off, so the only thing
        // separating it from any other line is its shape. Four numbers and
        // nothing else - a looser match would put a version banner or a
        // partial line into a benchmark result.
        assert!(!parse_latency("0 2 0.23\n").is_credible());
        assert!(!parse_latency("0 2 0.23 471 88\n").is_credible());
        assert!(!parse_latency("Warning: 0 2 0.23 471\n").is_credible());
        // And a well-shaped line still has to describe a probe that ran.
        assert!(!parse_latency("0 0 0.00 0\n").is_credible());
        assert!(!parse_latency("0 2 0.23 -5\n").is_credible());
    }

    #[test]
    fn a_latency_probe_that_collected_nothing_is_not_credible() {
        // A mean of zero from a parse that found nothing would read as an
        // extraordinarily fast machine.
        assert!(!parse_latency("Could not connect\n").is_credible());
        assert!(!parse_latency("").is_credible());
        assert!(!parse_latency("min: 0, max: 0, avg: 0.00 (0 samples)\n").is_credible());
    }

    #[test]
    fn the_workload_spreads_over_a_key_space_and_stays_small() {
        // Without `-r` redis-benchmark hammers a single key, which measures one
        // hash bucket and a CPU cache line rather than a data structure. And a
        // large value would turn this into a memory bandwidth measurement,
        // which memory.bandwidth already makes directly and scores under
        // Memory.
        let module = DatabaseCache::new();
        let args = module.shape_args(&SHAPES[0]);
        let keyspace = args.iter().position(|a| a == "-r").expect("no key space");
        assert_eq!(args[keyspace + 1], KEYSPACE);
        assert!(VALUE_BYTES.parse::<usize>().unwrap() <= 1024);
    }

    #[test]
    fn exactly_one_shape_is_pipelined_and_it_says_so_in_its_key() {
        // The value of the pipelined phase is the difference from the others -
        // how much of a cache's cost is syscalls and round-trips rather than
        // the data structure. A reader has to be able to tell which is which.
        let module = DatabaseCache::new();
        let pipelined: Vec<&Shape> = SHAPES.iter().filter(|s| s.pipeline.is_some()).collect();
        assert_eq!(pipelined.len(), 1);
        assert!(pipelined[0].key.contains("pipelined"));

        let args = module.shape_args(pipelined[0]);
        let depth = args.iter().position(|a| a == "-P").expect("no pipeline");
        assert_eq!(args[depth + 1], PIPELINE);

        for shape in SHAPES.iter().filter(|s| s.pipeline.is_none()) {
            assert!(
                !module.shape_args(shape).iter().any(|a| a == "-P"),
                "{} must not be pipelined",
                shape.key
            );
        }
    }

    #[test]
    fn persistence_settings_are_disclosed_and_never_set() {
        // Turning off appendonly and save raises write throughput and publishes
        // a number no production system can reproduce.
        let manifest = DatabaseCache::new().manifest().clone();
        for key in ["appendonly", "save", "io_threads"] {
            assert!(
                manifest.comparability.contains(&key.to_string()),
                "{key} is not declared as a comparability key"
            );
        }
        let module = DatabaseCache::new();
        for shape in SHAPES {
            let args = module.shape_args(shape).join(" ");
            assert!(!args.contains("appendonly"));
            assert!(!args.contains("config"));
        }
    }

    #[test]
    fn the_manifest_says_redis_itself_is_not_measured() {
        // Valkey is the fork the distributions moved to. Measuring only one of
        // two near-identical servers is a choice, and a choice a reader should
        // find stated rather than infer from an absence.
        let manifest = DatabaseCache::new().manifest().clone();
        assert!(manifest
            .limitations
            .iter()
            .any(|note| note.contains("Redis proper is not measured")));
    }
}
