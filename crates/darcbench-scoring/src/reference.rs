//! The reference profile.
//!
//! # DARC-REF-1
//!
//! DARCBench normalises every raw measurement against a *specified* reference
//! machine rather than against the best machine seen so far. A moving reference
//! silently rewrites history every time a faster CPU ships; a fixed reference
//! keeps a 2026 score meaningful in 2030.
//!
//! DARC-REF-1 is specified as:
//!
//! | Component | Specification |
//! |---|---|
//! | CPU | 8 physical cores / 16 threads, x86-64-v3, ~3.8 GHz sustained all-core |
//! | Memory | 64 GB DDR5-4800 ECC, dual channel |
//! | Storage | 2x NVMe PCIe 4.0 datacenter SSD, software RAID 1, ext4, `relatime` |
//! | Network | 1 Gbit/s symmetric, unmetered |
//! | OS | Debian 12, kernel 6.1 LTS, `performance` governor, mitigations default |
//!
//! That profile was chosen because it is close to the median *modern web
//! hosting dedicated server* actually sold today (the Hetzner AX52 class of
//! machine: Ryzen 7 7700, 64 GB DDR5, 2x1 TB Gen4 NVMe, per
//! <https://www.hetzner.com/pressroom/neue-dedicated-server-2023/>, accessed
//! 2026-08-03). Anchoring on a machine people really buy means a score of 1000
//! carries an intuitive meaning - "as fast as a normal good web server" -
//! rather than being an arbitrary index.
//!
//! # Status of the numbers below
//!
//! The values are **declared targets, not measurements**. Publishing invented
//! numbers as if they had been measured would poison every score derived from
//! them, so they are flagged: [`ReferenceProfile::calibrated`] is `false` and
//! the model version carries `-dev`. `docs/SCORING-SYSTEM.md` specifies the
//! calibration run that replaces them.

use std::collections::BTreeMap;

use darcbench_protocol::Direction;
use serde::{Deserialize, Serialize};

use crate::model::CategoryKey;

/// One normalisation anchor: the value DARC-REF-1 is expected to produce for a
/// specific metric, plus how that metric rolls up.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReferencePoint {
    /// Value DARC-REF-1 achieves, in the metric's own unit.
    pub value: f64,
    pub direction: Direction,
    /// Relative weight inside its module. Weights need not sum to 1; they are
    /// normalised at aggregation time.
    pub weight: f64,
    pub category: CategoryKey,
    /// Optional sub-aggregate this metric feeds, e.g. `single_core`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub facet: Option<Facet>,
}

/// Cross-cutting aggregates that are reported as their own public scores.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Facet {
    SingleCore,
    MultiCore,
}

impl Facet {
    pub fn label(self) -> &'static str {
        match self {
            Self::SingleCore => "Single-Core Score",
            Self::MultiCore => "Multi-Core Score",
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            Self::SingleCore => "single_core",
            Self::MultiCore => "multi_core",
        }
    }
}

/// The full set of normalisation anchors for a scoring model version.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReferenceProfile {
    /// Name of the reference specification, e.g. `DARC-REF-1`.
    pub name: String,
    /// `false` while the values are declared targets rather than measurements.
    pub calibrated: bool,
    /// Keyed by `"<module_id>/<metric_key>"`.
    pub points: BTreeMap<String, ReferencePoint>,
}

impl ReferenceProfile {
    pub fn get(&self, module_id: &str, metric_key: &str) -> Option<&ReferencePoint> {
        self.points.get(&format!("{module_id}/{metric_key}"))
    }
}

fn point(value: f64, weight: f64, category: CategoryKey, facet: Option<Facet>) -> ReferencePoint {
    ReferencePoint {
        value,
        direction: Direction::HigherIsBetter,
        weight,
        category,
        facet,
    }
}

/// A lower-is-better anchor, for latency metrics.
fn latency_point(
    value: f64,
    weight: f64,
    category: CategoryKey,
    facet: Option<Facet>,
) -> ReferencePoint {
    ReferencePoint {
        value,
        direction: Direction::LowerIsBetter,
        weight,
        category,
        facet,
    }
}

/// The provisional DARC-REF-1 anchors shipped with `dbs/0.1.0-dev`.
///
/// Only modules this build actually implements are populated: inventing anchors
/// for a module that does not exist yet would be fabricated data, and a test
/// below forbids it. Anchors are added in the same change that adds a module.
pub fn provisional_reference() -> ReferenceProfile {
    let mut points = BTreeMap::new();

    // --- cpu.mixed -------------------------------------------------------
    // Single-thread anchors: one core of DARC-REF-1 at its sustained boost.
    points.insert(
        "cpu.mixed/crypto_sha256.single".into(),
        point(1_900.0, 1.0, CategoryKey::Compute, Some(Facet::SingleCore)),
    );
    points.insert(
        "cpu.mixed/compress_deflate.single".into(),
        point(85.0, 1.0, CategoryKey::Compute, Some(Facet::SingleCore)),
    );
    points.insert(
        "cpu.mixed/json_roundtrip.single".into(),
        point(9_500.0, 1.0, CategoryKey::Compute, Some(Facet::SingleCore)),
    );
    points.insert(
        "cpu.mixed/integer_sort.single".into(),
        point(24.0, 1.0, CategoryKey::Compute, Some(Facet::SingleCore)),
    );
    points.insert(
        "cpu.mixed/float_matmul.single".into(),
        point(4_800.0, 1.0, CategoryKey::Compute, Some(Facet::SingleCore)),
    );

    // Multi-thread anchors: all 16 threads of DARC-REF-1, throughput style.
    points.insert(
        "cpu.mixed/crypto_sha256.multi".into(),
        point(24_000.0, 1.0, CategoryKey::Compute, Some(Facet::MultiCore)),
    );
    points.insert(
        "cpu.mixed/compress_deflate.multi".into(),
        point(1_050.0, 1.0, CategoryKey::Compute, Some(Facet::MultiCore)),
    );
    points.insert(
        "cpu.mixed/json_roundtrip.multi".into(),
        point(118_000.0, 1.0, CategoryKey::Compute, Some(Facet::MultiCore)),
    );
    points.insert(
        "cpu.mixed/integer_sort.multi".into(),
        point(280.0, 1.0, CategoryKey::Compute, Some(Facet::MultiCore)),
    );
    points.insert(
        "cpu.mixed/float_matmul.multi".into(),
        point(58_000.0, 1.0, CategoryKey::Compute, Some(Facet::MultiCore)),
    );

    // --- memory.bandwidth --------------------------------------------------
    //
    // DARC-REF-1 specifies 64 GB of dual-channel DDR5-4800, whose theoretical
    // ceiling is ~76.8 GB/s. Real streaming kernels reach roughly 55-65% of a
    // theoretical peak, so the multi-threaded anchors sit near 45 GB/s and the
    // single-threaded ones near the ~20 GB/s a single core can pull before it
    // runs out of outstanding misses rather than out of controller bandwidth.
    //
    // Memory metrics deliberately carry **no facet**. `single_core` and
    // `multi_core` exist to stop core count alone buying a good score
    // (`docs/SCORING-SYSTEM.md`, constraint 4) and are read as compute scores;
    // blending DRAM latency into a published "Single-Core Score" would change
    // what an already-shipped number means. The single and multi shapes remain
    // separate metrics inside the Memory category, so nothing is lost.
    //
    // Weights: random access and latency are weighted above streaming because
    // they are what a database, a PHP interpreter or a template engine actually
    // does. `cache_read` is weighted lowest - it is reported to show the
    // cache/DRAM cliff, and a machine with a small L3 should not be punished
    // for it twice.
    points.insert(
        "memory.bandwidth/sequential_read.single".into(),
        point(18_000.0, 1.0, CategoryKey::Memory, None),
    );
    points.insert(
        "memory.bandwidth/sequential_read.multi".into(),
        point(42_000.0, 1.0, CategoryKey::Memory, None),
    );
    points.insert(
        "memory.bandwidth/sequential_write.single".into(),
        point(12_000.0, 1.0, CategoryKey::Memory, None),
    );
    points.insert(
        "memory.bandwidth/sequential_write.multi".into(),
        point(28_000.0, 1.0, CategoryKey::Memory, None),
    );
    // Copy and Triad count traffic in both directions, STREAM-style, so their
    // anchors are correspondingly larger than a one-way read of the same data.
    points.insert(
        "memory.bandwidth/sequential_copy.single".into(),
        point(16_000.0, 1.0, CategoryKey::Memory, None),
    );
    points.insert(
        "memory.bandwidth/sequential_copy.multi".into(),
        point(38_000.0, 1.0, CategoryKey::Memory, None),
    );
    points.insert(
        "memory.bandwidth/triad.single".into(),
        point(17_000.0, 1.0, CategoryKey::Memory, None),
    );
    points.insert(
        "memory.bandwidth/triad.multi".into(),
        point(40_000.0, 1.0, CategoryKey::Memory, None),
    );
    points.insert(
        "memory.bandwidth/random_read.single".into(),
        point(1_800.0, 1.5, CategoryKey::Memory, None),
    );
    points.insert(
        "memory.bandwidth/random_read.multi".into(),
        point(9_000.0, 1.5, CategoryKey::Memory, None),
    );
    points.insert(
        "memory.bandwidth/cache_read.single".into(),
        point(80_000.0, 0.5, CategoryKey::Memory, None),
    );
    points.insert(
        "memory.bandwidth/cache_read.multi".into(),
        point(500_000.0, 0.5, CategoryKey::Memory, None),
    );
    // The only lower-is-better anchor in the profile. Inverted exactly once,
    // in `model::normalise`.
    points.insert(
        "memory.bandwidth/latency_random.single".into(),
        latency_point(85.0, 1.5, CategoryKey::Memory, None),
    );

    // --- storage.mixed -----------------------------------------------------
    //
    // DARC-REF-1 specifies two PCIe 4.0 datacenter NVMe drives in software
    // RAID 1 on ext4. Sequential reads come off both mirrors, so they roughly
    // double a single drive; sequential writes go to both and do not. The
    // random figures are the ones that matter for hosting, and the QD1 numbers
    // are deliberately far below the QD16 ones - a single-threaded application
    // never sees the queued figure, and a scoring model that only anchored the
    // queued one would rank a device by a number its users never experience.
    //
    // Weights follow that reasoning: random I/O and durability outweigh
    // streaming, because a web server almost never streams a whole file but
    // pays fsync on every write its database commits.
    points.insert(
        "storage.mixed/sequential_read.qd1".into(),
        point(3_500.0, 0.75, CategoryKey::Storage, None),
    );
    points.insert(
        "storage.mixed/sequential_write.qd1".into(),
        point(1_800.0, 0.75, CategoryKey::Storage, None),
    );
    points.insert(
        "storage.mixed/random_read_4k.qd1".into(),
        point(18_000.0, 1.5, CategoryKey::Storage, None),
    );
    points.insert(
        "storage.mixed/random_read_4k.qd16".into(),
        point(230_000.0, 1.0, CategoryKey::Storage, None),
    );
    points.insert(
        "storage.mixed/random_write_4k.qd1".into(),
        point(30_000.0, 1.5, CategoryKey::Storage, None),
    );
    points.insert(
        "storage.mixed/random_write_4k.qd16".into(),
        point(180_000.0, 1.0, CategoryKey::Storage, None),
    );
    points.insert(
        "storage.mixed/random_mixed_4k.qd16".into(),
        point(160_000.0, 1.25, CategoryKey::Storage, None),
    );
    points.insert(
        "storage.mixed/latency_read_4k.p99".into(),
        latency_point(0.12, 1.25, CategoryKey::Storage, None),
    );
    points.insert(
        "storage.mixed/latency_write_4k.p99".into(),
        latency_point(0.10, 1.25, CategoryKey::Storage, None),
    );
    points.insert(
        "storage.mixed/latency_fsync.mean".into(),
        latency_point(0.05, 1.5, CategoryKey::Storage, None),
    );

    // --- network.transfer ---------------------------------------------------
    //
    // DARC-REF-1 specifies a 1 Gbit/s symmetric unmetered port in a European
    // datacenter. A single TCP stream on such a link reaches roughly 800 Mbit/s
    // once window scaling settles; four streams get close to line rate. The
    // latency anchors assume a major CDN edge a few milliseconds away, which is
    // what a European datacenter actually has.
    //
    // Note what these anchors are *not*: they are not a measure of the
    // internet. They describe how well a machine reaches a nearby major edge.
    // `docs/PRODUCT-BIBLE.md` caps the whole Network category at 8% of the
    // total for exactly this reason - "a 10 Gbit/s port must not buy a good
    // score for a machine with a slow disk" - so the weights below only
    // distribute that 8%.
    //
    // Throughput carries the most weight, then time to first byte: those are
    // what a visitor to a site hosted on the machine actually experiences. DNS
    // is weighted lowest because it says more about the resolver configured on
    // the box than about the machine or its link.
    points.insert(
        "network.transfer/download.single".into(),
        point(800.0, 1.5, CategoryKey::Network, None),
    );
    points.insert(
        "network.transfer/download.multi".into(),
        point(940.0, 1.5, CategoryKey::Network, None),
    );
    points.insert(
        "network.transfer/ttfb.mean".into(),
        latency_point(25.0, 1.0, CategoryKey::Network, None),
    );
    points.insert(
        "network.transfer/tcp_connect.mean".into(),
        latency_point(5.0, 1.0, CategoryKey::Network, None),
    );
    points.insert(
        "network.transfer/tcp_connect.jitter".into(),
        latency_point(0.5, 0.75, CategoryKey::Network, None),
    );
    points.insert(
        "network.transfer/tls_handshake.mean".into(),
        latency_point(10.0, 0.75, CategoryKey::Network, None),
    );
    points.insert(
        "network.transfer/dns_resolve.mean".into(),
        latency_point(5.0, 0.5, CategoryKey::Network, None),
    );

    // --- web.static ---------------------------------------------------------
    //
    // These describe a machine serving HTTP over its own loopback interface,
    // against an origin DARCBench starts. That makes them a measurement of the
    // machine's HTTP stack - syscall cost, scheduler behaviour under many
    // connections, TCP on loopback, and for the TLS shape its asymmetric crypto
    // throughput. It is deliberately *not* a measurement of the operator's web
    // server or of their link; `network.transfer` covers the second and nothing
    // covers the first, by design.
    //
    // The figures assume DARC-REF-1's core count with a thread-per-connection
    // origin, which is what this module ships. They are declared targets, and
    // like every other anchor here they are what calibration will replace.
    //
    // The shape of the numbers matters more than their absolute values, and it
    // is the part a reader can sanity-check: keep-alive must be several times
    // a bare TCP connection, and a TCP connection must be several times a TLS
    // handshake, because each adds a strictly larger fixed cost. An anchor set
    // that did not have that shape would be wrong however well its individual
    // numbers were chosen.
    points.insert(
        "web.static/requests.small_keepalive".into(),
        point(120_000.0, 1.5, CategoryKey::Web, None),
    );
    points.insert(
        "web.static/connections.plaintext".into(),
        point(25_000.0, 0.75, CategoryKey::Web, None),
    );
    // A TLS handshake is dominated by one asymmetric operation, which is the
    // most CPU-expensive thing a web server does per visitor. It is weighted as
    // heavily as bulk throughput because it is the cost that decides how a site
    // behaves when a flash crowd arrives with cold connections.
    points.insert(
        "web.static/connections.tls".into(),
        point(3_000.0, 1.0, CategoryKey::Web, None),
    );
    points.insert(
        "web.static/throughput.medium".into(),
        point(3_000.0, 1.0, CategoryKey::Web, None),
    );
    points.insert(
        "web.static/throughput.large".into(),
        point(5_000.0, 0.75, CategoryKey::Web, None),
    );
    points.insert(
        "web.static/latency.small_mean".into(),
        latency_point(0.5, 1.0, CategoryKey::Web, None),
    );
    // The tail carries more weight than the mean, because the tail is what a
    // visitor notices and the mean is what a dashboard reports. It is measured
    // from when each request was *due* rather than when it was sent, so it
    // includes queueing the machine caused - see `darcbench-modules::loadgen`.
    points.insert(
        "web.static/latency.small_p99".into(),
        latency_point(2.0, 1.25, CategoryKey::Web, None),
    );

    // --- php.runtime --------------------------------------------------------
    //
    // PHP is single-threaded per request, so every figure here is about one
    // core - which is exactly why the market research insists both single- and
    // multi-core are always shown: *"a 96-core EPYC will dominate multi-core and
    // can lose to a Ryzen desktop-derived chip on the single-threaded path that
    // determines PHP request latency."* These anchors describe that path.
    //
    // They are scored into Web rather than Compute on purpose. `cpu.mixed`
    // already measures what the silicon does with hand-written Rust; what this
    // measures is what an *interpreter* does on it, which is a different
    // property of the same machine - branch prediction and cache behaviour
    // under a bytecode dispatch loop - and it is the one that decides how a PHP
    // site feels.
    //
    // Password hashing is weighted highest of the workloads because it is the
    // one an operator can neither cache nor optimise away, and because it is
    // what decides how many sign-ins a machine survives. Cold start is weighted
    // with it: on a host without a warm worker pool it is paid on every single
    // request.
    // 62/s is ~16 ms per hash, which is what bcrypt cost 8 - 256 key-setup
    // rounds - costs on any current x86 core. An earlier 300/s implied 3.3 ms,
    // which is roughly cost *6*: the anchor described a different workload from
    // the one the module runs, and because this metric carries the heaviest
    // weight in the module and the module is half the Web basket, it multiplied
    // every machine's Web score by about 0.875. Serial integer work like this
    // barely varies between current cores, so the anchor should be close to
    // what any of them measures.
    points.insert(
        "php.runtime/hash.password".into(),
        point(62.0, 1.25, CategoryKey::Web, None),
    );
    points.insert(
        "php.runtime/json.encode".into(),
        point(900_000.0, 1.0, CategoryKey::Web, None),
    );
    points.insert(
        "php.runtime/json.decode".into(),
        point(700_000.0, 1.0, CategoryKey::Web, None),
    );
    points.insert(
        "php.runtime/array.ops".into(),
        point(400_000.0, 1.0, CategoryKey::Web, None),
    );
    points.insert(
        "php.runtime/template.render".into(),
        point(800_000.0, 1.0, CategoryKey::Web, None),
    );
    points.insert(
        "php.runtime/hash.sha256".into(),
        point(140_000.0, 0.75, CategoryKey::Web, None),
    );
    // PHP CLI start-up is dominated by dynamic linking and per-extension MINIT
    // rather than by core speed, so a stock distro build sits at 25-40 ms
    // almost everywhere and a faster CPU moves it very little. An anchor of 20
    // baked in a minimal-extension build that nobody serves sites with.
    points.insert(
        "php.runtime/startup.cold".into(),
        latency_point(30.0, 1.25, CategoryKey::Web, None),
    );

    // --- node.runtime -------------------------------------------------------
    //
    // Like PHP, every figure here is one core: Node's concurrency story for a
    // web server is one process per core behind a load balancer, so what
    // matters per-request is the single-threaded path. What this measures that
    // `cpu.mixed` does not is what a JIT does on this silicon - V8's inline
    // caches and its garbage collector behave differently from hand-written
    // Rust on the same machine, and it is the JIT's behaviour that decides how
    // a Node service feels.
    //
    // Grounded in measurements taken from the shipped workload rather than
    // guessed, then raised modestly for DARC-REF-1's newer core. They remain
    // declared targets, and calibration replaces them like every other anchor.
    //
    // Two carry the most weight. `module.load` is the cold-start cost of a
    // dependency tree, which every serverless invocation and every deploy pays
    // and which no cache removes; `async.fileio` is the event loop, which is
    // what a Node service is actually made of between the CPU work.
    points.insert(
        "node.runtime/json.stringify".into(),
        point(450_000.0, 1.0, CategoryKey::Web, None),
    );
    points.insert(
        "node.runtime/json.parse".into(),
        point(600_000.0, 1.0, CategoryKey::Web, None),
    );
    points.insert(
        "node.runtime/ssr.render".into(),
        point(1_000_000.0, 1.0, CategoryKey::Web, None),
    );
    points.insert(
        "node.runtime/crypto.hash".into(),
        point(110_000.0, 0.75, CategoryKey::Web, None),
    );
    points.insert(
        "node.runtime/async.fileio".into(),
        point(6_000.0, 1.25, CategoryKey::Web, None),
    );
    points.insert(
        "node.runtime/module.load".into(),
        point(260.0, 1.25, CategoryKey::Web, None),
    );
    // Node starts slower than PHP - a bigger binary, V8's snapshot to
    // deserialise - and like PHP it is dominated by process and runtime set-up
    // rather than by core speed, so it barely moves between machines.
    points.insert(
        "node.runtime/startup.cold".into(),
        latency_point(90.0, 1.0, CategoryKey::Web, None),
    );

    // --- database.oltp --------------------------------------------------------
    //
    // These are the first anchors written with a measurement in front of them.
    // Every other block here was derived from published figures; this one was
    // extrapolated from `database.oltp` actually running, on 2026-08-14. That
    // does not make them calibrated - DARC-REF-1 is still a machine nobody has
    // run this on - but the shape and the ratios come from the workload rather
    // than from an estimate of it.
    //
    // **The observing machine was two vCPUs of a Ryzen 9 9900X**, and the
    // asymmetry is what makes the extrapolation possible in both directions.
    // Its cores are *faster* than DARC-REF-1's 7700, and it has two of them
    // against sixteen threads. So the observations are not uniformly low: they
    // are low where the workload wanted parallelism and roughly right, or
    // better than the reference, where it wanted one fast core.
    //
    // Observed: read 20,900-49,300 tx/s, write ~3,000 tx/s, read latency
    // 0.74 ms and write 1.53 ms at an offered 200 tx/s.
    //
    // The latency anchors sit *below* those observations rather than being
    // scaled up from them. A latency phase runs four clients and four pgbench
    // threads beside the server, which is nine runnable processes on two
    // cores - so most of that 0.74 ms is scheduler queueing rather than query
    // cost, and a machine with threads to spare removes it. Anchoring at
    // 0.4 ms assumes about half of it was contention.
    //
    // The throughput anchors go up, but by much less than the eightfold the
    // core count suggests, for two reasons in the module. `pgbench` runs
    // *inside the container*, so half of any added parallelism goes to the
    // client rather than the server. And the write path is bounded by
    // something core count does not help at all: pgbench's built-in workload
    // updates `pgbench_branches`, of which scale 10 has ten rows, so eight
    // clients contend on ten rows and the ceiling is lock throughput.
    //
    // The wide read spread is a property of the observing machine and is
    // recorded because an anchor drawn from an unstable observation should say
    // that it was one.
    points.insert(
        "database.oltp/write.tps".into(),
        point(10_000.0, 1.5, CategoryKey::Database, None),
    );
    points.insert(
        "database.oltp/read.tps".into(),
        point(100_000.0, 1.25, CategoryKey::Database, None),
    );
    points.insert(
        "database.oltp/write.latency_mean".into(),
        latency_point(0.8, 1.0, CategoryKey::Database, None),
    );
    points.insert(
        "database.oltp/read.latency_mean".into(),
        latency_point(0.4, 1.0, CategoryKey::Database, None),
    );
    // Both `estimated_p95` anchors carry half the weight of the means they are
    // derived from, because that is what they are: a normal approximation from
    // a mean and a standard deviation, not an observed percentile. The module
    // says so in the metric key. A tail estimate should not move a category
    // score as much as a measurement does.
    points.insert(
        "database.oltp/write.latency_estimated_p95".into(),
        latency_point(2.0, 0.5, CategoryKey::Database, None),
    );
    points.insert(
        "database.oltp/read.latency_estimated_p95".into(),
        latency_point(1.0, 0.5, CategoryKey::Database, None),
    );

    // --- database.cache -------------------------------------------------------
    //
    // Valkey serves commands from a single thread, so unlike every other
    // anchor in the Database category these track per-core speed and barely
    // move with core count. What core count does change is whether the
    // benchmark client starves the server, because it shares the container:
    // the observing machine's 88,900 GET/s was two cores split between a
    // single-threaded server and a fifty-connection client.
    //
    // That observing machine had the *faster* core of the two, so the doubling
    // below is not a per-core speed correction - it is the client no longer
    // taking half the machine. Which is why these multiply by about two where
    // the OLTP read anchor multiplies by rather more: there, added threads go
    // to a server that can use them; here they only stop the client stealing
    // from one that cannot.
    //
    // The consequence worth stating is that this module scores a 96-core
    // machine and an 8-core one almost identically, and that is correct rather
    // than a defect. It is the market research's own point about single-thread
    // paths, arriving in the Database category instead of the Web one.
    points.insert(
        "database.cache/get.throughput".into(),
        point(180_000.0, 1.5, CategoryKey::Database, None),
    );
    points.insert(
        "database.cache/set.throughput".into(),
        point(170_000.0, 1.0, CategoryKey::Database, None),
    );
    points.insert(
        "database.cache/incr.throughput".into(),
        point(180_000.0, 0.75, CategoryKey::Database, None),
    );
    // Pipelining at depth 16 amortises the syscall and round-trip cost that
    // dominates the three above, so it lands an order of magnitude higher and
    // measures something different: how fast the machine moves data once the
    // per-command overhead is taken away.
    points.insert(
        "database.cache/pipelined.throughput".into(),
        point(1_500_000.0, 1.0, CategoryKey::Database, None),
    );
    // The floor under every cache hit, and weighted accordingly: an
    // application that reaches its cache twenty times to build a page pays
    // this twenty times, and no amount of throughput headroom reduces it.
    points.insert(
        "database.cache/roundtrip.unloaded_mean".into(),
        latency_point(0.08, 1.25, CategoryKey::Database, None),
    );
    // The worst single round trip seen while idle - scheduler and allocator
    // jitter. Weighted lowest in the module because it is one observation
    // rather than a distribution, and a single outlier is exactly what it is
    // made of.
    points.insert(
        "database.cache/roundtrip.unloaded_max".into(),
        latency_point(1.0, 0.5, CategoryKey::Database, None),
    );

    ReferenceProfile {
        name: "DARC-REF-1".into(),
        calibrated: false,
        points,
    }
}

#[cfg(test)]
// In tests, `unwrap`/`expect` panicking *is* the failure signal.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic_in_result_fn)]
mod tests {
    use super::*;

    #[test]
    fn every_shipped_anchor_is_positive_and_finite() {
        for (key, p) in provisional_reference().points {
            assert!(
                p.value > 0.0 && p.value.is_finite(),
                "{key} has a non-positive anchor"
            );
            assert!(p.weight > 0.0, "{key} has a non-positive weight");
        }
    }

    /// Every metric `web.static` ships must have an anchor, or it scores
    /// nothing and lands in `unreferenced_metrics` - which is visible, but only
    /// to somebody reading the bundle.
    #[test]
    fn the_web_category_has_anchors_for_every_shipped_metric() {
        let reference = provisional_reference();
        for key in [
            "requests.small_keepalive",
            "connections.plaintext",
            "connections.tls",
            "throughput.medium",
            "throughput.large",
            "latency.small_mean",
            "latency.small_p99",
        ] {
            let anchor = reference
                .get("web.static", key)
                .unwrap_or_else(|| panic!("web.static/{key} has no anchor"));
            assert_eq!(anchor.category, CategoryKey::Web, "{key}");
            assert!(anchor.facet.is_none(), "{key}");
        }
    }

    #[test]
    fn the_database_category_has_anchors_for_every_shipped_metric() {
        let reference = provisional_reference();
        for (module, keys) in [
            (
                "database.oltp",
                [
                    "read.tps",
                    "write.tps",
                    "read.latency_mean",
                    "write.latency_mean",
                    "read.latency_estimated_p95",
                    "write.latency_estimated_p95",
                ]
                .as_slice(),
            ),
            (
                "database.cache",
                [
                    "get.throughput",
                    "set.throughput",
                    "incr.throughput",
                    "pipelined.throughput",
                    "roundtrip.unloaded_mean",
                    "roundtrip.unloaded_max",
                ]
                .as_slice(),
            ),
        ] {
            for key in keys {
                let anchor = reference
                    .get(module, key)
                    .unwrap_or_else(|| panic!("{module}/{key} has no anchor"));
                assert_eq!(anchor.category, CategoryKey::Database, "{module}/{key}");
                assert!(anchor.facet.is_none(), "{module}/{key}");
            }
        }
    }

    #[test]
    fn a_database_read_is_cheaper_than_a_write_and_a_cache_hit_is_cheaper_than_both() {
        // The ordering, not the numbers. A read that anchored slower than a
        // write, or a cache round trip anchored slower than a SQL query, would
        // score every machine against a relationship no database produces -
        // and unlike a value that is merely off, a wrong ordering cannot be
        // corrected by calibration. It would take a *faster* machine to look
        // worse on the metric that was inverted.
        let reference = provisional_reference();
        let anchor = |module: &str, key: &str| {
            reference
                .get(module, key)
                .unwrap_or_else(|| panic!("{module}/{key}"))
                .value
        };
        assert!(
            anchor("database.oltp", "read.tps") > anchor("database.oltp", "write.tps"),
            "select-only must anchor above read-write: no commit path is cheaper than none"
        );
        assert!(
            anchor("database.oltp", "read.latency_mean")
                < anchor("database.oltp", "write.latency_mean")
        );
        assert!(
            anchor("database.cache", "roundtrip.unloaded_mean")
                < anchor("database.oltp", "read.latency_mean"),
            "a cache exists because it is faster than the database it fronts"
        );
        assert!(
            anchor("database.cache", "pipelined.throughput")
                > anchor("database.cache", "get.throughput"),
            "pipelining removes the per-command round trip; it cannot be slower"
        );
        // And every p95 estimate must sit above the mean it is derived from,
        // since it is that mean plus 1.645 standard deviations.
        for prefix in ["read", "write"] {
            assert!(
                anchor("database.oltp", &format!("{prefix}.latency_estimated_p95"))
                    > anchor("database.oltp", &format!("{prefix}.latency_mean")),
                "{prefix}: a p95 below its own mean is not reachable"
            );
        }
    }

    #[test]
    fn the_php_runtime_has_anchors_for_every_shipped_metric() {
        let reference = provisional_reference();
        for key in [
            "json.encode",
            "json.decode",
            "array.ops",
            "template.render",
            "hash.sha256",
            "hash.password",
            "startup.cold",
        ] {
            let anchor = reference
                .get("php.runtime", key)
                .unwrap_or_else(|| panic!("php.runtime/{key} has no anchor"));
            assert_eq!(anchor.category, CategoryKey::Web, "{key}");
            assert!(anchor.facet.is_none(), "{key}");
        }
        // bcrypt at cost 8 is thousands of times more expensive than encoding a
        // small object. An anchor set that did not reflect that would score a
        // machine against a relationship the workload cannot produce.
        let rate = |key: &str| {
            reference
                .get("php.runtime", key)
                .map(|a| a.value)
                .unwrap_or(0.0)
        };
        assert!(rate("json.encode") > rate("hash.password") * 1000.0);
        assert!(rate("hash.sha256") > rate("hash.password"));
    }

    #[test]
    fn the_node_runtime_has_anchors_for_every_shipped_metric() {
        let reference = provisional_reference();
        for key in [
            "json.stringify",
            "json.parse",
            "ssr.render",
            "crypto.hash",
            "async.fileio",
            "module.load",
            "startup.cold",
        ] {
            let anchor = reference
                .get("node.runtime", key)
                .unwrap_or_else(|| panic!("node.runtime/{key} has no anchor"));
            assert_eq!(anchor.category, CategoryKey::Web, "{key}");
            assert!(anchor.facet.is_none(), "{key}");
        }
        // Requiring a 64-module tree is thousands of times more expensive than
        // serialising one small object, and a filesystem round trip through the
        // event loop is far more expensive than either JSON path. An anchor set
        // without that shape would score machines against a relationship the
        // workload cannot produce.
        let rate = |key: &str| {
            reference
                .get("node.runtime", key)
                .map(|a| a.value)
                .unwrap_or(0.0)
        };
        assert!(rate("json.stringify") > rate("module.load") * 1000.0);
        assert!(rate("json.parse") > rate("async.fileio") * 10.0);
    }

    /// The anchor set has to have the right *shape*, independently of whether
    /// the individual numbers survive calibration.
    ///
    /// Each step adds a strictly larger fixed cost per request: reusing a
    /// connection is cheapest, opening a TCP connection costs more, and adding
    /// a TLS handshake costs more again. An anchor set that inverted any of
    /// those would score a machine on a relationship that cannot exist, and the
    /// error would be invisible in the aggregate.
    #[test]
    fn the_web_anchors_have_a_physically_possible_shape() {
        let reference = provisional_reference();
        let rate = |key: &str| {
            reference
                .get("web.static", key)
                .map(|anchor| anchor.value)
                .unwrap_or_else(|| panic!("{key}"))
        };
        assert!(rate("requests.small_keepalive") > rate("connections.plaintext"));
        assert!(rate("connections.plaintext") > rate("connections.tls"));

        let mean = reference
            .get("web.static", "latency.small_mean")
            .unwrap()
            .value;
        let p99 = reference
            .get("web.static", "latency.small_p99")
            .unwrap()
            .value;
        assert!(p99 > mean, "a 99th percentile below the mean is impossible");
    }

    #[test]
    fn lookup_uses_module_scoped_keys() {
        let r = provisional_reference();
        assert!(r.get("cpu.mixed", "crypto_sha256.single").is_some());
        // A metric key from another module must not collide.
        assert!(r.get("php.wordpress", "crypto_sha256.single").is_none());
    }

    #[test]
    fn anchors_exist_only_for_implemented_modules() {
        // Guards against the temptation to pre-populate anchors for modules
        // that have never been run - those numbers would be fabrications.
        //
        // "Implemented" here means registered and runnable, not merely written.
        // The two database modules existed in this crate's sibling for two
        // commits before they earned a line below: their images were unpinned,
        // so they could not run, so anchoring them would have been anchoring an
        // idea. They were added when the digests were pinned and both modules
        // had produced metrics on real hardware.
        const IMPLEMENTED: [&str; 9] = [
            "cpu.mixed/",
            "memory.bandwidth/",
            "storage.mixed/",
            "network.transfer/",
            "web.static/",
            "php.runtime/",
            "node.runtime/",
            "database.oltp/",
            "database.cache/",
        ];
        let r = provisional_reference();
        for key in r.points.keys() {
            assert!(
                IMPLEMENTED.iter().any(|prefix| key.starts_with(prefix)),
                "{key}: anchors may only ship alongside an implemented module"
            );
        }
    }

    /// Every inverted anchor is inverted on purpose.
    ///
    /// A throughput anchor accidentally marked `LowerIsBetter` would rank the
    /// slowest machine first in that metric, and the error would be invisible
    /// in every aggregate - the total still looks like a plausible number.
    ///
    /// The list below is exhaustive rather than inferred. An earlier version of
    /// this test matched on `key.contains("latency")`, which held right up until
    /// the network anchors landed: `ttfb.mean`, `tcp_connect.mean` and
    /// `dns_resolve.mean` are latency measurements whose keys never say so, and
    /// the heuristic would have demanded they be scored upside down. A rule that
    /// silently gets the answer wrong is worse than a list someone has to edit,
    /// so adding an anchor here is a deliberate act.
    #[test]
    fn every_inverted_anchor_is_declared() {
        const LOWER_IS_BETTER: &[&str] = &[
            "memory.bandwidth/latency_random.single",
            "network.transfer/dns_resolve.mean",
            "network.transfer/tcp_connect.jitter",
            "network.transfer/tcp_connect.mean",
            "network.transfer/tls_handshake.mean",
            "network.transfer/ttfb.mean",
            "storage.mixed/latency_fsync.mean",
            "storage.mixed/latency_read_4k.p99",
            "storage.mixed/latency_write_4k.p99",
            "node.runtime/startup.cold",
            "php.runtime/startup.cold",
            "web.static/latency.small_mean",
            "web.static/latency.small_p99",
            // The database latencies. Four of the six say `latency` in the key
            // and two say `roundtrip`, which is the point the comment above
            // makes: a heuristic would have caught the first four and scored a
            // cache's round trip upside down.
            "database.oltp/read.latency_mean",
            "database.oltp/read.latency_estimated_p95",
            "database.oltp/write.latency_mean",
            "database.oltp/write.latency_estimated_p95",
            "database.cache/roundtrip.unloaded_mean",
            "database.cache/roundtrip.unloaded_max",
        ];

        let declared: std::collections::BTreeSet<&str> = LOWER_IS_BETTER.iter().copied().collect();
        let actual: std::collections::BTreeSet<String> = provisional_reference()
            .points
            .into_iter()
            .filter(|(_, p)| p.direction == Direction::LowerIsBetter)
            .map(|(key, _)| key)
            .collect();

        let actual_refs: std::collections::BTreeSet<&str> =
            actual.iter().map(String::as_str).collect();
        assert_eq!(
            actual_refs, declared,
            "an anchor changed direction without the decision being recorded here"
        );
    }

    /// Facets are compute-shaped and must stay that way.
    ///
    /// `single_core` / `multi_core` exist to stop core count alone buying a
    /// good score, and are published as core-performance numbers. Folding
    /// memory metrics into them would silently change what an already-shipped
    /// score means.
    #[test]
    fn only_compute_anchors_carry_a_facet() {
        for (key, p) in provisional_reference().points {
            if p.facet.is_some() {
                assert_eq!(
                    p.category,
                    CategoryKey::Compute,
                    "{key} feeds a core facet but is not a compute metric"
                );
            }
        }
    }

    #[test]
    fn the_network_category_has_anchors_for_every_shipped_metric() {
        let r = provisional_reference();
        for key in [
            "download.single",
            "download.multi",
            "ttfb.mean",
            "tcp_connect.mean",
            "tcp_connect.jitter",
            "tls_handshake.mean",
            "dns_resolve.mean",
        ] {
            let point = r
                .get("network.transfer", key)
                .unwrap_or_else(|| panic!("no anchor for network.transfer/{key}"));
            assert_eq!(point.category, CategoryKey::Network);
        }
    }

    /// A multi-stream download cannot anchor below a single stream.
    ///
    /// If it did, a machine that uses its link better with concurrency - which
    /// is every machine - would score worse for doing so.
    #[test]
    fn the_multi_stream_anchor_is_at_least_the_single_stream_one() {
        let r = provisional_reference();
        let single = r
            .get("network.transfer", "download.single")
            .expect("single");
        let multi = r.get("network.transfer", "download.multi").expect("multi");
        assert!(multi.value >= single.value);
        // ...and neither may exceed the reference machine's stated link rate.
        assert!(
            multi.value <= 1000.0,
            "DARC-REF-1 has a 1 Gbit/s port; an anchor above line rate is fiction"
        );
    }

    #[test]
    fn the_storage_category_has_anchors_for_every_shipped_metric() {
        let r = provisional_reference();
        for key in [
            "sequential_read.qd1",
            "sequential_write.qd1",
            "random_read_4k.qd1",
            "random_read_4k.qd16",
            "random_write_4k.qd1",
            "random_write_4k.qd16",
            "random_mixed_4k.qd16",
            "latency_read_4k.p99",
            "latency_write_4k.p99",
            "latency_fsync.mean",
        ] {
            let point = r
                .get("storage.mixed", key)
                .unwrap_or_else(|| panic!("no anchor for storage.mixed/{key}"));
            assert_eq!(point.category, CategoryKey::Storage);
        }
    }

    /// A queued figure must anchor well above the unqueued one.
    ///
    /// If they were anchored alike, a device that is only good when it has
    /// sixteen requests to work on would score the same as one that answers
    /// the first request immediately - and only the second feels fast to a
    /// single-threaded application.
    #[test]
    fn queued_and_unqueued_anchors_are_not_interchangeable() {
        let r = provisional_reference();
        for base in ["random_read_4k", "random_write_4k"] {
            let qd1 = r.get("storage.mixed", &format!("{base}.qd1")).expect("qd1");
            let qd16 = r
                .get("storage.mixed", &format!("{base}.qd16"))
                .expect("qd16");
            assert!(
                qd16.value > qd1.value * 2.0,
                "{base}: a queued anchor of {} against an unqueued {} does not describe a real \
                 device",
                qd16.value,
                qd1.value
            );
        }
    }

    #[test]
    fn the_memory_category_has_anchors_for_every_shipped_metric() {
        let r = provisional_reference();
        for key in [
            "sequential_read.single",
            "sequential_read.multi",
            "sequential_write.single",
            "sequential_write.multi",
            "sequential_copy.single",
            "sequential_copy.multi",
            "triad.single",
            "triad.multi",
            "random_read.single",
            "random_read.multi",
            "cache_read.single",
            "cache_read.multi",
            "latency_random.single",
        ] {
            let point = r
                .get("memory.bandwidth", key)
                .unwrap_or_else(|| panic!("no anchor for memory.bandwidth/{key}"));
            assert_eq!(point.category, CategoryKey::Memory);
        }
    }
}
