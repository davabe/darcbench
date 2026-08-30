//! `memory.bandwidth` - the Phase 2 memory module.
//!
//! # What it measures
//!
//! Seven access patterns, each in a single-threaded and a multi-threaded shape
//! except for latency:
//!
//! | Metric | Unit | What it predicts |
//! |---|---|---|
//! | `sequential_read` | MiB/s | Scans: log processing, full-table reads, template rendering |
//! | `sequential_write` | MiB/s | Buffer fills, response assembly, page-cache population |
//! | `sequential_copy` | MiB/s | `memcpy` between buffers, which is most of what a web server does with bytes |
//! | `triad` | MiB/s | Multi-stream traffic; the pattern that actually saturates a memory controller |
//! | `random_read` | MiB/s | Hash lookups, index probes, interpreter object graphs |
//! | `cache_read` | MiB/s | The same scan when the working set fits in cache |
//! | `latency_random` | ns | Pointer chasing: linked structures, B-tree descent, PHP arrays |
//!
//! Reporting `cache_read` next to `sequential_read` is the point of including
//! it: the ratio between them is the cliff a workload falls off when its
//! working set stops fitting in cache, and it varies enormously between a
//! machine with 96 MiB of L3 and a shared instance with 4 MiB.
//!
//! # The trap this module exists to avoid
//!
//! `docs/BENCHMARK-METHODOLOGY.md`: *"measuring the page cache and calling it
//! memory bandwidth. Working sets must exceed last-level cache by a documented
//! multiple, buffers must be touched before timing, and the cache topology
//! captured in inventory is used to size them."*
//!
//! So, concretely:
//!
//! * The working set a shape streams is [`LLC_MULTIPLE`] times the last-level
//!   cache the host reports, so even a perfect LRU can retain only a fraction
//!   of it. For the single-threaded shape that is one thread's buffer; for the
//!   multi-threaded shape it is the aggregate across threads, because the
//!   threads share that cache and evict each other. See [`Plan`].
//! * Every buffer is written once before any timing, so page faults, zero-page
//!   sharing and NUMA first-touch placement are paid for outside the
//!   measurement.
//! * Buffers are allocated once per shape and reused across repetitions, so
//!   allocation never lands inside a timed region.
//! * When the host reports no cache topology, the module falls back to a
//!   documented default **and says so** in its context and warnings, rather
//!   than quietly measuring cache.
//!
//! # Safety
//!
//! The module writes nothing, opens no socket and spawns no process, but it
//! does allocate. DARCBench is designed to run on machines that are already
//! serving customers, and pushing a live host into swap to measure its memory
//! bandwidth would be an outage that also produces a number describing the swap
//! device. Total allocation is therefore capped at
//! [`MEMORY_BUDGET_FRACTION`] of *available* memory, and the working set is
//! shrunk - with a `MemoryPressure` warning - rather than the budget exceeded.

use std::hint::black_box;
use std::time::Instant;

use darcbench_protocol::metrics::{Direction, Metric, Warning, WarningCode};
use darcbench_protocol::stats::{outlier_indices, summarize};
use darcbench_protocol::ModuleId;

use crate::harness::{calibrate_with, time_reps};
use crate::module::{
    BenchmarkModule, MachineFacts, ModuleError, ModuleManifest, ModuleOutput, ModuleParams,
    ModuleReporter, SafetyClass,
};
use crate::workloads::{SplitMix64, CORPUS_SEED};

/// Workload-definition version. Major bump = results are not comparable.
pub const VERSION: &str = "1.0.0";

/// The module's identifier, validated against the [`ModuleId`] grammar by a
/// unit test in this file.
pub const MODULE_ID: &str = "memory.bandwidth";

/// Multiple of last-level cache that a DRAM working set must exceed.
///
/// Four is the documented multiple required by the methodology: large enough
/// that a perfectly-behaved LRU cache can retain at most a quarter of the
/// stream, small enough to stay affordable on a modest instance.
pub const LLC_MULTIPLE: u64 = 4;

/// Working set assumed when the host reports no cache topology at all - common
/// inside containers and on some hypervisors. Chosen to exceed the largest L3
/// found on mainstream server parts, so the fallback errs towards measuring
/// DRAM rather than cache.
pub const FALLBACK_LLC_BYTES: u64 = 32 << 20;

/// Floor on the per-thread DRAM working set for the single-threaded shape.
const MIN_WORKING_SET: u64 = 32 << 20;

/// Ceiling on the per-thread DRAM working set.
///
/// A stream this large is unambiguously in DRAM on any machine DARCBench
/// targets, and the ceiling keeps the peak allocation bounded independently of
/// what cache size the host claims - some hypervisors report the whole host's
/// L3 to a two-vCPU guest.
const MAX_WORKING_SET: u64 = 512 << 20;

/// Multiple of *private* L2 that each thread's stream must exceed.
///
/// The aggregate rule alone is not sufficient for the multi-threaded shape: on
/// a very wide machine, dividing the aggregate across threads can leave each
/// thread with a slice that fits in its own private L2, at which point the
/// thread never reaches the shared cache or DRAM at all.
const L2_MULTIPLE: u64 = 4;

/// Largest share of *available* memory this module will allocate in total.
pub const MEMORY_BUDGET_FRACTION: f64 = 0.25;

/// Working-set multiple below which a DRAM figure is no longer credible: the
/// cache would hold too much of it. Reaching this triggers a warning and a
/// `Degraded` result rather than a quiet lie.
const MIN_CREDIBLE_MULTIPLE: u64 = 2;

/// Cache-resident working set, when L2 is unknown. Small enough to sit inside
/// the L2 of anything DARCBench runs on.
const FALLBACK_CACHE_BYTES: u64 = 256 << 10;

/// Bytes per payload element. Every streaming kernel here walks `u64`.
const ELEM: u64 = 8;

/// Bytes per permutation entry. `u32` indices halve the index stream's own
/// memory traffic and still cover working sets up to 32 GiB.
const INDEX_BYTES: u64 = 4;

/// Triad is the heaviest pattern: three live buffers at once.
const PEAK_BUFFERS: u64 = 3;

/// Scalar for the Triad kernel. Any value works; a non-trivial one keeps the
/// multiply from being folded away.
const TRIAD_SCALAR: u64 = 3;

/// Dependent loads one latency pass performs.
///
/// The chase is *chunked* rather than traversing its whole cycle. At DRAM
/// latency a full traversal of a working set sized for bandwidth takes tens of
/// seconds - one 512 MiB cycle at 100 ns a step is over ten - which cannot be
/// calibrated down to a repetition target and would make a "quick" profile take
/// the better part of an hour on its own.
///
/// The cursor persists across passes *and* across repetitions, so successive
/// chunks keep advancing through the cycle instead of re-walking the prefix the
/// last chunk just pulled into cache. Every load stays a cold, dependent miss
/// scattered across the full working set, which is what the measurement needs;
/// visiting every slot is not.
const LATENCY_STEPS_PER_PASS: u64 = 1 << 16;

// ---------------------------------------------------------------------------
// Access patterns
// ---------------------------------------------------------------------------

/// One memory access pattern.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Access {
    SequentialRead,
    SequentialWrite,
    SequentialCopy,
    Triad,
    RandomRead,
    CacheRead,
    LatencyRandom,
}

impl Access {
    /// Every pattern, in execution order.
    const ALL: [Access; 7] = [
        Self::SequentialRead,
        Self::SequentialWrite,
        Self::SequentialCopy,
        Self::Triad,
        Self::RandomRead,
        Self::CacheRead,
        Self::LatencyRandom,
    ];

    fn key(self) -> &'static str {
        match self {
            Self::SequentialRead => "sequential_read",
            Self::SequentialWrite => "sequential_write",
            Self::SequentialCopy => "sequential_copy",
            Self::Triad => "triad",
            Self::RandomRead => "random_read",
            Self::CacheRead => "cache_read",
            Self::LatencyRandom => "latency_random",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::SequentialRead => "Sequential read",
            Self::SequentialWrite => "Sequential write",
            Self::SequentialCopy => "Sequential copy",
            Self::Triad => "Triad (a = b + k*c)",
            Self::RandomRead => "Random read",
            Self::CacheRead => "Cache-resident read",
            Self::LatencyRandom => "Random access latency",
        }
    }

    fn unit(self) -> &'static str {
        match self {
            Self::LatencyRandom => "ns",
            _ => "MiB/s",
        }
    }

    fn direction(self) -> Direction {
        match self {
            // A latency measurement is inverted exactly once, in the scoring
            // crate. Nothing here may look at this beyond reporting it.
            Self::LatencyRandom => Direction::LowerIsBetter,
            _ => Direction::HigherIsBetter,
        }
    }

    /// Latency is a property of one dependent access chain. Running it on every
    /// core measures contention between cores, which is a real effect but a
    /// different quantity - reporting it under the same name would blur two
    /// things a reader needs kept apart.
    fn has_multi_shape(self) -> bool {
        self != Self::LatencyRandom
    }

    /// Payload buffers this pattern needs live at once.
    ///
    /// The pointer chase needs none: it walks its permutation array and never
    /// touches a payload buffer, so allocating one would be working set that is
    /// paid for and never read.
    fn buffers(self) -> u64 {
        match self {
            Self::LatencyRandom => 0,
            Self::SequentialCopy => 2,
            Self::Triad => 3,
            _ => 1,
        }
    }

    /// Entries in this pattern's index permutation, for a working set of
    /// `bytes`.
    ///
    /// The permutation is `u32`, so covering a given working set with the chase
    /// takes twice as many entries as a `u64` payload buffer would. Sizing it
    /// as though the entries were 8 bytes wide - the mistake this method exists
    /// to prevent - gives the chase half the working set it was supposed to
    /// have, which on a machine with a large last-level cache is enough for it
    /// to partly fit and report a latency the hardware cannot deliver.
    fn permutation_entries(self, bytes: u64) -> usize {
        match self {
            Self::LatencyRandom => (bytes / INDEX_BYTES).max(1) as usize,
            // The gather indexes a `u64` payload buffer, so there is one entry
            // per payload element.
            Self::RandomRead => (bytes / ELEM).max(1) as usize,
            _ => 0,
        }
    }

    /// True when the pattern deliberately fits inside cache.
    fn is_cache_resident(self) -> bool {
        self == Self::CacheRead
    }

    /// Bytes of memory traffic one pass over a `bytes`-sized working set
    /// generates.
    ///
    /// Read-modify-write patterns count both directions, as STREAM does, so a
    /// `sequential_copy` figure is directly comparable with a published STREAM
    /// Copy number rather than being half of one.
    fn bytes_per_pass(self, bytes: u64) -> f64 {
        let streams = match self {
            Self::SequentialRead | Self::SequentialWrite | Self::RandomRead | Self::CacheRead => 1,
            // one read + one write
            Self::SequentialCopy => 2,
            // two reads + one write
            Self::Triad => 3,
            // Not a bandwidth figure; reported in nanoseconds per load.
            Self::LatencyRandom => 1,
        };
        (bytes * streams) as f64
    }
}

// ---------------------------------------------------------------------------
// Sizing
// ---------------------------------------------------------------------------

/// The working-set sizing decision, recorded in the bundle so a reader can see
/// exactly what was measured and why.
///
/// # Why the two shapes get different sizes
///
/// The requirement is that the working set a shape streams exceeds last-level
/// cache by [`LLC_MULTIPLE`]. For the single-threaded shape that is one thread's
/// buffer. For the multi-threaded shape the threads share the last-level cache
/// and evict each other, so it is the *aggregate* across threads - which is the
/// same accounting STREAM uses when it splits one array across OpenMP threads.
///
/// Sizing the multi shape per-thread instead is not merely wasteful, it is
/// wrong in a way that matters: a 64-core server with a 256 MiB L3 would need
/// 64 GiB of buffers to satisfy the rule per thread, the memory budget would
/// cut that down, and the run would then be flagged cache-contaminated even
/// though its 20 GiB aggregate stream obviously never fit in cache. Exactly the
/// large dedicated servers DARCBench exists to measure would be the ones
/// reported as unmeasurable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Plan {
    /// Threads the multi-threaded shape will use.
    pub threads: u64,
    /// Working set for the single-threaded shape, in bytes.
    pub single_bytes: u64,
    /// Per-thread working set for the multi-threaded shape, in bytes.
    pub multi_bytes: u64,
    /// Per-thread cache-resident working set, in bytes.
    pub cache_bytes: u64,
    /// Last-level cache the sizing was based on.
    pub llc_bytes: u64,
    /// True when the host reported no cache topology and a default was used.
    pub llc_assumed: bool,
    /// True when the memory budget forced the single-threaded working set below
    /// [`MIN_CREDIBLE_MULTIPLE`] times the last-level cache.
    pub single_contaminated: bool,
    /// The same, for the multi-threaded shape's aggregate working set.
    pub multi_contaminated: bool,
    /// Peak bytes the module will have allocated at once.
    pub peak_alloc_bytes: u64,
}

impl Plan {
    /// Chooses working-set sizes for `threads` threads on this machine.
    ///
    /// Two constraints pull against each other: the working set must be a
    /// documented multiple of last-level cache to mean anything, and the total
    /// allocation must stay well inside available memory so the module is safe
    /// on a machine that is already serving traffic. When they cannot both be
    /// satisfied the budget wins - a benchmark that swaps measures the swap
    /// device - and the compromise is recorded rather than hidden.
    pub fn for_machine(facts: &MachineFacts, threads: usize) -> Self {
        let threads = (threads.max(1) as u64).max(1);
        let llc_assumed = facts.last_level_cache_bytes.is_none();
        let llc = facts
            .last_level_cache_bytes
            .filter(|v| *v > 0)
            .unwrap_or(FALLBACK_LLC_BYTES);
        let l2 = facts
            .l2_cache_bytes
            .filter(|v| *v > 0)
            .unwrap_or(FALLBACK_CACHE_BYTES);

        // Unknown available memory is treated as constrained, not as unlimited
        // - the same rule preflight applies to unknown free disk space.
        let budget = facts
            .available_bytes
            .map(|available| (available as f64 * MEMORY_BUDGET_FRACTION) as u64)
            .unwrap_or_else(|| {
                MIN_WORKING_SET.saturating_mul(threads.saturating_mul(PEAK_BUFFERS))
            });
        // Triad is the heaviest pattern: three live buffers per thread.
        let affordable =
            |thread_count: u64| budget / thread_count.saturating_mul(PEAK_BUFFERS).max(1);

        let aggregate_target = llc.saturating_mul(LLC_MULTIPLE);

        let single_bytes = align_down(
            aggregate_target
                .clamp(MIN_WORKING_SET, MAX_WORKING_SET)
                .min(affordable(1))
                .max(page_floor()),
        );

        // The aggregate target divided across threads, but never so thin that a
        // thread's slice fits inside its own private L2.
        let multi_bytes = align_down(
            (aggregate_target / threads)
                .max(l2.saturating_mul(L2_MULTIPLE))
                .min(MAX_WORKING_SET)
                .min(affordable(threads))
                .max(page_floor()),
        );

        let credible = llc.saturating_mul(MIN_CREDIBLE_MULTIPLE);
        let single_contaminated = single_bytes < credible;
        let multi_contaminated = multi_bytes.saturating_mul(threads) < credible;

        // The cache-resident set targets half of L2: large enough to be a real
        // stream, small enough that it cannot escape into DRAM.
        let cache_bytes = align_down(
            (l2 / 2)
                .clamp(page_floor(), single_bytes.min(multi_bytes))
                .max(page_floor()),
        );

        Self {
            threads,
            single_bytes,
            multi_bytes,
            cache_bytes,
            llc_bytes: llc,
            llc_assumed,
            single_contaminated,
            multi_contaminated,
            peak_alloc_bytes: single_bytes.saturating_mul(PEAK_BUFFERS).max(
                multi_bytes
                    .saturating_mul(PEAK_BUFFERS)
                    .saturating_mul(threads),
            ),
        }
    }

    /// Working set one thread streams, for this access pattern and shape.
    fn bytes(self, access: Access, threads: usize) -> u64 {
        if access.is_cache_resident() {
            self.cache_bytes
        } else if threads <= 1 {
            self.single_bytes
        } else {
            self.multi_bytes
        }
    }
}

/// Smallest working set the module will ever allocate: one page of `u64`s.
fn page_floor() -> u64 {
    4096
}

/// Rounds down to a whole page, so a working set never straddles a partial one.
fn align_down(bytes: u64) -> u64 {
    let aligned = bytes & !(4096 - 1);
    aligned.max(4096)
}

// ---------------------------------------------------------------------------
// Per-thread state
// ---------------------------------------------------------------------------

/// One thread's buffers, allocated and first-touched before any timing.
///
/// Reused across every repetition of a shape: a half-gigabyte allocation plus
/// its page faults runs to hundreds of milliseconds, which would swamp a 200 ms
/// repetition if it happened inside one.
struct WorkingSet {
    a: Vec<u64>,
    b: Vec<u64>,
    c: Vec<u64>,
    /// A single cycle visiting every slot exactly once, for the pointer chase
    /// and the random gather. `u32` indices halve the index stream's own
    /// memory traffic and cover working sets up to 32 GiB of `u64`.
    permutation: Vec<u32>,
    /// Where the pointer chase has reached. Persists across passes and
    /// repetitions so the chase keeps advancing through the cycle rather than
    /// re-walking a prefix that is already in cache.
    cursor: u32,
}

impl WorkingSet {
    /// Allocates and first-touches everything `access` needs to stream `bytes`.
    ///
    /// `seed` differs per thread so that concurrent threads do not walk
    /// identical permutations in lockstep, which would let them share cache
    /// lines and flatter the random results.
    fn prepare(access: Access, bytes: u64, seed: u64) -> Self {
        let elements = (bytes / ELEM).max(1) as usize;
        let buffers = access.buffers();

        // `vec![v; n]` with a non-zero value writes every element, so the pages
        // are faulted in and NUMA-placed here rather than during a timed pass.
        // A zero fill would let the kernel hand back shared zero pages and the
        // first timed read would pay for the faults.
        let fill = |count: u64, value: u64| {
            if buffers >= count {
                vec![value; elements]
            } else {
                Vec::new()
            }
        };

        Self {
            a: fill(1, 1),
            b: fill(2, 2),
            c: fill(3, 3),
            permutation: match access.permutation_entries(bytes) {
                0 => Vec::new(),
                entries => permutation(entries, seed),
            },
            cursor: 0,
        }
    }

    /// Dependent loads one pass of the pointer chase performs.
    ///
    /// Only meaningful for [`Access::LatencyRandom`]; zero when this working set
    /// carries no permutation to chase.
    fn chase_steps(&self) -> u64 {
        if self.permutation.is_empty() {
            0
        } else {
            LATENCY_STEPS_PER_PASS
        }
    }
}

/// A random permutation of `0..len` as a single cycle.
///
/// Built with a Sattolo shuffle rather than Fisher-Yates: Sattolo produces a
/// permutation with exactly one cycle of full length, which is what a pointer
/// chase needs. A Fisher-Yates shuffle can decompose into several short cycles,
/// and a chase that falls into a short one would sit in cache and report a
/// latency several times better than the machine can actually deliver.
fn permutation(len: usize, seed: u64) -> Vec<u32> {
    let mut order: Vec<u32> = (0..len as u32).collect();
    if len < 2 {
        return order;
    }
    let mut rng = SplitMix64::new(seed);
    // Sattolo: index i is swapped with a strictly lower index.
    for i in (1..len).rev() {
        let j = (rng.next_u64() % i as u64) as usize;
        order.swap(i, j);
    }
    // `order` is now a cycle in "visit order" form; turn it into next-pointers
    // so a chase is a single dependent load per step.
    let mut next = vec![0u32; len];
    for window in order.windows(2) {
        next[window[0] as usize] = window[1];
    }
    if let (Some(last), Some(first)) = (order.last(), order.first()) {
        next[*last as usize] = *first;
    }
    next
}

// ---------------------------------------------------------------------------
// Kernels
// ---------------------------------------------------------------------------

/// Runs `passes` passes of `access` over `state`, returning a value derived
/// from the work so nothing can be optimised away.
///
/// Every kernel is written as a plain loop over slices. Hand-written SIMD would
/// measure what a specialist can extract rather than what a normal optimised
/// program achieves, which is the same choice `cpu.mixed` makes and is
/// disclosed as a limitation in both.
fn execute(access: Access, state: &mut WorkingSet, passes: u64) -> u64 {
    let mut acc = 0u64;
    for pass in 0..passes {
        match access {
            Access::SequentialRead | Access::CacheRead => {
                let mut sum = 0u64;
                for value in black_box(&state.a).iter() {
                    sum = sum.wrapping_add(*value);
                }
                acc = acc.wrapping_add(sum);
            }
            Access::SequentialWrite => {
                // The stored value varies per pass so the compiler cannot hoist
                // the fill out of the loop.
                let value = pass | 1;
                for slot in black_box(&mut state.a).iter_mut() {
                    *slot = value;
                }
                acc = acc.wrapping_add(state.a[0]);
            }
            Access::SequentialCopy => {
                let (dst, src) = (&mut state.a, &state.b);
                dst.copy_from_slice(black_box(src));
                acc = acc.wrapping_add(dst[0]);
            }
            Access::Triad => {
                let (dst, b, c) = (&mut state.a, &state.b, &state.c);
                for ((slot, x), y) in dst.iter_mut().zip(b.iter()).zip(c.iter()) {
                    *slot = x.wrapping_add(TRIAD_SCALAR.wrapping_mul(*y));
                }
                acc = acc.wrapping_add(dst[0]);
            }
            Access::RandomRead => {
                // Independent gathers: the index stream is sequential, the
                // payload access is scattered, and the loads do not depend on
                // each other, so this measures random-access *throughput*
                // rather than latency. `latency_random` is the dependent one.
                let a = black_box(&state.a);
                let mut sum = 0u64;
                for index in black_box(&state.permutation).iter() {
                    sum = sum.wrapping_add(a[*index as usize]);
                }
                acc = acc.wrapping_add(sum);
            }
            Access::LatencyRandom => {
                // A single dependent chain: each load's address comes from the
                // previous load's result, so nothing can be prefetched or
                // overlapped and the timing is load-to-use latency.
                let mut cursor = state.cursor;
                {
                    let next = black_box(&state.permutation);
                    if next.is_empty() {
                        continue;
                    }
                    for _ in 0..LATENCY_STEPS_PER_PASS {
                        cursor = next[cursor as usize];
                    }
                }
                // Carried forward so the next chunk continues the cycle.
                state.cursor = cursor;
                acc = acc.wrapping_add(cursor as u64);
            }
        }
    }
    black_box(acc)
}

/// Executes one repetition of a shape and returns
/// `(value_in_the_metric_unit, wall_clock_ms)`.
///
/// `states` holds one prepared working set per thread, so the timed region
/// contains only memory traffic.
fn time_shape(access: Access, plan: &Plan, states: &mut [WorkingSet], passes: u64) -> (f64, f64) {
    let bytes = plan.bytes(access, states.len());
    let threads = states.len().max(1) as u64;
    // Read before the timed region so the accounting never depends on state
    // the kernels may have touched.
    let chase_steps = states.first().map(WorkingSet::chase_steps).unwrap_or(0);

    let start = Instant::now();
    if states.len() <= 1 {
        if let Some(state) = states.first_mut() {
            execute(access, state, passes);
        }
    } else {
        std::thread::scope(|scope| {
            for state in states.iter_mut() {
                scope.spawn(move || execute(access, state, passes));
            }
        });
    }
    let elapsed = start.elapsed();
    let seconds = elapsed.as_secs_f64().max(f64::EPSILON);

    let value = if access == Access::LatencyRandom {
        // Nanoseconds per dependent load, across every load performed. The step
        // count comes from the permutation itself rather than from the working
        // set size, so the two can never drift apart.
        let accesses = (chase_steps * passes * threads) as f64;
        (seconds * 1e9) / accesses.max(1.0)
    } else {
        let traffic = access.bytes_per_pass(bytes) * passes as f64 * threads as f64;
        traffic / (1024.0 * 1024.0) / seconds
    };
    (value, elapsed.as_secs_f64() * 1000.0)
}

// ---------------------------------------------------------------------------
// The module
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct MemoryBandwidth {
    manifest: ModuleManifest,
}

impl Default for MemoryBandwidth {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryBandwidth {
    pub fn new() -> Self {
        // Justified `expect`: `MODULE_ID` is a compile-time constant whose
        // validity under the ModuleId grammar is asserted by a unit test in
        // this file. There is no runtime input here to fail on.
        #[allow(clippy::expect_used)]
        let id = ModuleId::new(MODULE_ID).expect("MODULE_ID is a valid module id");
        Self {
            manifest: ModuleManifest {
                id,
                version: VERSION.to_string(),
                title: "Memory bandwidth and latency".to_string(),
                purpose: "Measure sequential, streaming and random memory throughput plus \
                          random-access latency, at working sets sized from the host's own \
                          cache topology so the result describes DRAM rather than cache."
                    .to_string(),
                safety_class: SafetyClass::ComputeIntensive,
                dependencies: vec![],
                max_bytes_written: 0,
                max_network_bytes: 0,
                cleanup: "None required: the module allocates only heap memory, which is \
                          released when it returns, including on cancellation."
                    .to_string(),
                validation: vec![
                    "Every access pattern must produce at least 5 measured repetitions."
                        .to_string(),
                    "Repetitions shorter than 20 ms are rejected as timer-noise dominated."
                        .to_string(),
                    "A shape whose working set falls below twice the last-level cache is \
                     reported as cache-contaminated and downgrades the result to Degraded."
                        .to_string(),
                    "Coefficient of variation above 0.15 raises a high-variance warning and \
                     downgrades the result to Degraded."
                        .to_string(),
                ],
                limitations: vec![
                    "Working sets are sized from the cache topology the host reports. Inside a \
                     container or a hypervisor that exports none, a documented default is used \
                     instead and the result records that it was assumed."
                        .to_string(),
                    "Thread placement is left to the operating system, so on a multi-socket \
                     machine the multi-threaded figures reflect default first-touch NUMA \
                     placement rather than an isolated per-node measurement. Binding threads to \
                     nodes needs privileges this module deliberately does not take."
                        .to_string(),
                    "The kernels are plain loops: no hand-written SIMD and no explicit \
                     non-temporal stores, so they measure what a normal optimised program \
                     achieves, including whatever the compiler auto-vectorises. `sequential_copy` \
                     is the exception - it delegates to the platform `memcpy`, which is what real \
                     software does and which may use non-temporal stores or other tricks the \
                     other kernels do not. That is why it can report more total traffic than a \
                     plain read of the same buffer."
                        .to_string(),
                    "Copy and Triad count traffic in both directions, as STREAM does, so their \
                     figures are roughly double and triple a one-way read of the same buffer."
                        .to_string(),
                    "Random-access latency includes TLB misses, which is what a real workload \
                     experiences but makes the figure sensitive to huge-page configuration."
                        .to_string(),
                ],
                comparability: vec![
                    "module.version".to_string(),
                    // `platform.architecture`, not `cpu.architecture`: the inventory puts it
                    // there, and the name it was declared under for two phases resolved to
                    // nothing at all.
                    "platform.architecture".to_string(),
                    "agent.build_target".to_string(),
                    // The key the context actually carries. `params.threads` was the name
                    // of the input rather than of the recorded fact.
                    "threads".to_string(),
                    "plan.single_bytes".to_string(),
                    "plan.multi_bytes_per_thread".to_string(),
                    "plan.llc_bytes".to_string(),
                ],
                // Memory bandwidth is physically steadier than mixed CPU work,
                // so the bound is tighter. `docs/BENCHMARK-METHODOLOGY.md`
                // targets < 0.05 for a good run and 0.15 as the acceptable
                // ceiling; this is that ceiling.
                stability_cv_bound: 0.15,
            },
        }
    }
}

impl BenchmarkModule for MemoryBandwidth {
    fn manifest(&self) -> &ModuleManifest {
        &self.manifest
    }

    fn estimated_duration_s(&self, params: &ModuleParams) -> u64 {
        let shapes: u64 = Access::ALL
            .iter()
            .map(|a| if a.has_multi_shape() { 2 } else { 1 })
            .sum();
        let reps = (params.warmup_reps + params.measured_reps) as u64;
        // Plus roughly two repetitions per shape for calibration, and a
        // second per shape for allocating and first-touching the buffers.
        (shapes * (reps + 2) * params.target_rep_ms).div_ceil(1000) + shapes
    }

    fn estimated_peak_memory_bytes(&self, params: &ModuleParams) -> u64 {
        Plan::for_machine(&params.facts, params.effective_threads()).peak_alloc_bytes
    }

    fn run(
        &self,
        params: &ModuleParams,
        reporter: &dyn ModuleReporter,
    ) -> Result<ModuleOutput, ModuleError> {
        let threads = params.effective_threads();
        let plan = Plan::for_machine(&params.facts, threads);

        let mut metrics = Vec::new();
        let mut warnings = Vec::new();
        let mut calibration = serde_json::Map::new();

        if plan.llc_assumed {
            warnings.push(Warning {
                code: WarningCode::ValidationFailed,
                message: format!(
                    "This host reports no cache topology, so the working set was sized from a \
                     {} MiB assumed last-level cache. The figures are still real measurements, \
                     but whether they describe DRAM or cache cannot be confirmed from here.",
                    FALLBACK_LLC_BYTES >> 20
                ),
                metric_key: None,
            });
        }
        for (shape, working_set, contaminated) in [
            ("single", plan.single_bytes, plan.single_contaminated),
            (
                "multi",
                plan.multi_bytes.saturating_mul(plan.threads),
                plan.multi_contaminated,
            ),
        ] {
            if !contaminated {
                continue;
            }
            let warning = Warning {
                code: WarningCode::MemoryPressure,
                message: format!(
                    "The {shape}-threaded shape could only be given a {} MiB working set against \
                     a {} MiB last-level cache. Below {MIN_CREDIBLE_MULTIPLE}x cache the stream \
                     partly fits, so these figures sit between cache and DRAM and must not be \
                     compared with a run on a machine that had room for a full one.",
                    working_set >> 20,
                    plan.llc_bytes >> 20,
                ),
                metric_key: None,
            };
            reporter.warn(warning.clone());
            warnings.push(warning);
        }

        let total_units: f64 = Access::ALL
            .iter()
            .map(|a| if a.has_multi_shape() { 2.0 } else { 1.0 })
            .sum();
        let mut completed_units = 0.0f64;

        for access in Access::ALL {
            let shapes: &[(&str, usize)] = if access.has_multi_shape() {
                &[("single", 1), ("multi", 0)]
            } else {
                &[("single", 1)]
            };

            for (shape_name, shape_threads) in shapes {
                if reporter.is_cancelled() {
                    return Err(ModuleError::Cancelled);
                }
                let shape_threads = if *shape_threads == 0 {
                    threads
                } else {
                    *shape_threads
                };
                let metric_key = format!("{}.{shape_name}", access.key());
                let working_set = plan.bytes(access, shape_threads);

                // Allocated and first-touched once, outside every timed region.
                let mut states: Vec<WorkingSet> = (0..shape_threads)
                    .map(|thread| {
                        WorkingSet::prepare(
                            access,
                            working_set,
                            CORPUS_SEED ^ ((thread as u64).wrapping_mul(0x9E37_79B9)),
                        )
                    })
                    .collect();

                if reporter.is_cancelled() {
                    return Err(ModuleError::Cancelled);
                }

                let passes = calibrate_with(params.target_rep_ms, reporter, |passes| {
                    time_shape(access, &plan, &mut states, passes).1
                })?;
                calibration.insert(
                    format!("{metric_key}.passes"),
                    serde_json::Value::from(passes),
                );

                let outcome = time_reps(
                    params,
                    reporter,
                    &metric_key,
                    access.unit(),
                    completed_units,
                    total_units,
                    |_| time_shape(access, &plan, &mut states, passes),
                )?;
                warnings.extend(outcome.warnings);

                let summary = summarize(&outcome.measured)
                    .ok_or_else(|| ModuleError::NoSamples(metric_key.clone()))?;

                if let Some(cv) = summary.cv {
                    if summary.is_unstable(self.manifest.stability_cv_bound) {
                        let warning = Warning {
                            code: WarningCode::HighVariance,
                            message: format!(
                                "`{metric_key}` varied by {:.1}% between repetitions (bound \
                                 {:.0}%). For memory this usually means a noisy neighbour on the \
                                 same memory controller rather than a measurement fault.",
                                cv * 100.0,
                                self.manifest.stability_cv_bound * 100.0
                            ),
                            metric_key: Some(metric_key.clone()),
                        };
                        reporter.warn(warning.clone());
                        warnings.push(warning);
                    }
                }

                metrics.push(Metric {
                    label: format!("{} ({shape_name})", access.label()),
                    unit: access.unit().to_string(),
                    direction: access.direction(),
                    value: summary.median,
                    outliers: outlier_indices(&outcome.measured, 3.5),
                    summary,
                    samples: outcome.samples,
                    key: metric_key,
                    measures_dispersion: false,
                    tail_quantile: false,
                });

                completed_units += 1.0;
            }
        }

        let mut context = serde_json::Map::new();
        context.insert("threads".into(), serde_json::Value::from(threads));
        context.insert("workload_version".into(), serde_json::Value::from(VERSION));
        context.insert(
            "build_target".into(),
            serde_json::Value::from(format!(
                "{}-{}",
                std::env::consts::ARCH,
                std::env::consts::OS
            )),
        );
        context.insert(
            "plan".into(),
            serde_json::json!({
                "single_bytes": plan.single_bytes,
                "multi_bytes_per_thread": plan.multi_bytes,
                "multi_bytes_aggregate": plan.multi_bytes.saturating_mul(plan.threads),
                "cache_bytes": plan.cache_bytes,
                "llc_bytes": plan.llc_bytes,
                "llc_assumed": plan.llc_assumed,
                "llc_multiple": LLC_MULTIPLE,
                "l2_multiple": L2_MULTIPLE,
                "single_contaminated": plan.single_contaminated,
                "multi_contaminated": plan.multi_contaminated,
                "peak_alloc_bytes": plan.peak_alloc_bytes,
                "memory_budget_fraction": MEMORY_BUDGET_FRACTION,
            }),
        );
        context.insert(
            "numa_nodes".into(),
            match params.facts.numa_nodes {
                Some(nodes) => serde_json::Value::from(nodes),
                None => serde_json::Value::Null,
            },
        );
        context.insert("calibration".into(), serde_json::Value::Object(calibration));

        // The cache/DRAM cliff: how much faster the same scan is when it fits.
        if let Some(ratio) = cache_cliff(&metrics) {
            context.insert("cache_cliff".into(), serde_json::json!(ratio));
        }

        if params.facts.numa_nodes.is_some_and(|nodes| nodes > 1) {
            let warning = Warning {
                code: WarningCode::Informational,
                message: format!(
                    "This machine has {} NUMA nodes. Threads were placed by the operating \
                     system, so the multi-threaded figures include whatever cross-node traffic \
                     the default policy produced. They are a fair description of what an \
                     unpinned workload gets, not of the hardware's best case.",
                    params.facts.numa_nodes.unwrap_or_default()
                ),
                metric_key: None,
            };
            reporter.warn(warning.clone());
            warnings.push(warning);
        }

        Ok(ModuleOutput {
            metrics,
            warnings,
            context,
        })
    }
}

/// `cache_read.single` divided by `sequential_read.single`.
///
/// The ratio a workload's performance falls by when its working set stops
/// fitting in cache. Reported as context rather than scored: it is a property
/// of the cache hierarchy, and a machine with a small L3 should not be punished
/// twice for it.
fn cache_cliff(metrics: &[Metric]) -> Option<f64> {
    let cached = metrics.iter().find(|m| m.key == "cache_read.single")?;
    let dram = metrics.iter().find(|m| m.key == "sequential_read.single")?;
    (dram.value > 0.0).then(|| cached.value / dram.value)
}

#[cfg(test)]
// In tests, `unwrap`/`expect` panicking *is* the failure signal.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic_in_result_fn)]
mod tests {
    use super::*;
    use crate::module::NullReporter;
    use darcbench_protocol::Profile;
    use std::collections::BTreeSet;

    /// Small, fast parameters that still exercise the full
    /// size -> allocate -> calibrate -> warm up -> measure -> summarise path.
    fn fast_params() -> ModuleParams {
        ModuleParams {
            warmup_reps: 1,
            measured_reps: 5,
            target_rep_ms: 25,
            threads: 2,
            // A small but realistic machine. `available_bytes` is deliberately
            // modest so the budget, not the ideal size, picks the working set:
            // it keeps the suite fast while still exercising the constrained
            // path that real shared instances take.
            facts: MachineFacts {
                last_level_cache_bytes: Some(512 << 10),
                l2_cache_bytes: Some(128 << 10),
                available_bytes: Some(128 << 20),
                numa_nodes: Some(1),
                free_scratch_bytes: None,
            },
            scratch_dir: None,
        }
    }

    #[test]
    fn module_id_constant_satisfies_the_grammar() {
        assert!(
            ModuleId::new(MODULE_ID).is_ok(),
            "MODULE_ID `{MODULE_ID}` violates the ModuleId grammar; the constructor would panic"
        );
    }

    #[test]
    fn manifest_is_well_formed() {
        let module = MemoryBandwidth::new();
        let m = module.manifest();
        assert_eq!(m.id.as_str(), MODULE_ID);
        assert_eq!(m.version, VERSION);
        assert_eq!(
            m.max_bytes_written, 0,
            "memory.bandwidth must never write to disk"
        );
        assert_eq!(m.max_network_bytes, 0);
        assert_eq!(m.safety_class, SafetyClass::ComputeIntensive);
        assert!(!m.validation.is_empty());
        assert!(
            !m.limitations.is_empty(),
            "a module must disclose its limitations"
        );
        assert!(m.stability_cv_bound > 0.0);
    }

    #[test]
    fn metric_keys_are_unique_and_in_the_reference_alphabet() {
        let mut keys = Vec::new();
        for access in Access::ALL {
            keys.push(format!("{}.single", access.key()));
            if access.has_multi_shape() {
                keys.push(format!("{}.multi", access.key()));
            }
        }
        let unique: BTreeSet<&String> = keys.iter().collect();
        assert_eq!(keys.len(), unique.len(), "duplicate metric key");
        for access in Access::ALL {
            let key = access.key();
            let mut chars = key.chars();
            assert!(chars.next().is_some_and(|c| c.is_ascii_lowercase()));
            assert!(
                chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
                "`{key}` must match [a-z][a-z0-9_]*"
            );
        }
    }

    /// One full run, checked for shape: every metric present, every metric
    /// carrying its warm-ups and its measured repetitions, and the units and
    /// directions the scoring crate will rely on.
    #[test]
    fn a_full_run_produces_every_metric_with_the_declared_shape() {
        let module = MemoryBandwidth::new();
        let output = module
            .run(&fast_params(), &NullReporter::default())
            .expect("run");

        // 6 patterns x 2 shapes, plus latency single-only.
        assert_eq!(output.metrics.len(), 13);
        for metric in &output.metrics {
            assert!(
                metric.value > 0.0 && metric.value.is_finite(),
                "{} produced {}",
                metric.key,
                metric.value
            );
            assert_eq!(metric.summary.n, 5, "{} measured reps", metric.key);
            assert_eq!(metric.samples.len(), 6, "5 measured + 1 warm-up");
            assert_eq!(metric.samples.iter().filter(|s| s.warmup).count(), 1);

            // Latency is the one inverted metric. Getting this wrong would
            // rank the slowest machine first, so it is asserted per metric
            // rather than spot-checked.
            let (expected_direction, expected_unit) = if metric.key.starts_with("latency_") {
                (Direction::LowerIsBetter, "ns")
            } else {
                (Direction::HigherIsBetter, "MiB/s")
            };
            assert_eq!(metric.direction, expected_direction, "{}", metric.key);
            assert_eq!(metric.unit, expected_unit, "{}", metric.key);
        }

        let keys: Vec<&str> = output.metrics.iter().map(|m| m.key.as_str()).collect();
        for expected in [
            "sequential_read.single",
            "sequential_read.multi",
            "sequential_write.multi",
            "sequential_copy.single",
            "triad.multi",
            "random_read.single",
            "cache_read.single",
            "latency_random.single",
        ] {
            assert!(keys.contains(&expected), "missing `{expected}`");
        }
        assert!(
            !keys.contains(&"latency_random.multi"),
            "latency has no multi shape; a threaded latency figure measures contention"
        );
    }

    /// The measurements have to be physically plausible, and the sizing
    /// decision behind them has to be recorded.
    #[test]
    fn measurements_are_plausible_and_the_sizing_decision_is_recorded() {
        let module = MemoryBandwidth::new();
        let output = module
            .run(&fast_params(), &NullReporter::default())
            .expect("run");

        // A latency of a fraction of a nanosecond would mean the chase never
        // left L1 - which is exactly the failure this module exists to avoid.
        let latency = output
            .metric_value("latency_random.single")
            .expect("latency metric");
        assert!(
            latency > 1.0,
            "a {latency:.2} ns dependent load is an L1 hit, not a random access"
        );
        assert!(
            latency < 10_000.0,
            "{latency:.0} ns per access is not a memory system"
        );

        let plan = &output.context["plan"];
        assert_eq!(plan["llc_assumed"], false);
        assert_eq!(plan["llc_multiple"], LLC_MULTIPLE);
        assert_eq!(plan["single_contaminated"], false);
        assert_eq!(plan["multi_contaminated"], false);
        let single = plan["single_bytes"].as_u64().expect("single");
        let multi = plan["multi_bytes_per_thread"].as_u64().expect("multi");
        let cache = plan["cache_bytes"].as_u64().expect("cache");
        assert!(
            cache < single && cache < multi,
            "the cache-resident set ({cache}) must be smaller than both DRAM sets \
             ({single} single, {multi} multi)"
        );
        assert_eq!(output.context["threads"], 2);
        assert_eq!(output.context["numa_nodes"], 1);
        assert!(output.context.contains_key("calibration"));

        let cliff = output.context["cache_cliff"].as_f64().expect("cliff");
        assert!(cliff > 0.0 && cliff.is_finite());

        // Whether a cache-resident scan actually *is* faster is a property of
        // the hardware that only shows through an optimised build: unoptimised,
        // both scans are bound by the generated code rather than by memory, and
        // the ratio means nothing. The suite refuses to assert on a number the
        // build cannot support - the same refusal `validate` makes when it
        // marks any debug-profile bundle incomparable.
        if cfg!(debug_assertions) {
            println!("skipping the cache-cliff assertion: debug build, cliff was {cliff:.2}x");
        } else {
            assert!(
                cliff >= 1.0,
                "a cache-resident scan should not be slower than the same scan from DRAM; \
                 cliff was {cliff:.2}x"
            );
        }
    }

    #[test]
    fn cancellation_is_honoured_promptly() {
        let module = MemoryBandwidth::new();
        let reporter = NullReporter::default();
        reporter.cancel();
        let start = Instant::now();
        assert!(matches!(
            module.run(&fast_params(), &reporter),
            Err(ModuleError::Cancelled)
        ));
        assert!(
            start.elapsed() < std::time::Duration::from_secs(5),
            "cancellation took {:?}",
            start.elapsed()
        );
    }

    // --- sizing -----------------------------------------------------------

    fn facts(llc: u64, l2: u64, available: Option<u64>) -> MachineFacts {
        MachineFacts {
            last_level_cache_bytes: Some(llc),
            l2_cache_bytes: Some(l2),
            available_bytes: available,
            numa_nodes: Some(1),
            free_scratch_bytes: None,
        }
    }

    #[test]
    fn the_single_threaded_working_set_exceeds_cache_by_the_documented_multiple() {
        let plan = Plan::for_machine(&facts(32 << 20, 1 << 20, Some(64u64 << 30)), 8);
        assert!(
            plan.single_bytes >= (32 << 20) * LLC_MULTIPLE,
            "{} bytes against a 32 MiB cache",
            plan.single_bytes
        );
        assert!(!plan.single_contaminated);
        assert!(!plan.multi_contaminated);
        assert!(!plan.llc_assumed);
    }

    /// The multiple applies to what a shape actually streams.
    ///
    /// Regression: the working set was sized per thread for both shapes, so a
    /// large server needed threads x 4 x LLC of buffers to satisfy the rule.
    /// The memory budget cut that down and the run was then flagged
    /// cache-contaminated - meaning the big dedicated servers DARCBench exists
    /// to measure were exactly the ones reported as unmeasurable.
    #[test]
    fn a_wide_server_with_a_large_cache_is_measurable() {
        // 64 threads, 256 MiB L3, 256 GiB of memory: a current EPYC host.
        let plan = Plan::for_machine(&facts(256 << 20, 1 << 20, Some(256u64 << 30)), 64);

        assert!(
            !plan.multi_contaminated,
            "aggregate working set was {} MiB against a {} MiB cache",
            (plan.multi_bytes * plan.threads) >> 20,
            plan.llc_bytes >> 20
        );
        assert!(
            plan.multi_bytes * plan.threads >= plan.llc_bytes * MIN_CREDIBLE_MULTIPLE,
            "the aggregate stream must exceed cache"
        );
        // ...and no thread's slice may fit inside its own private L2.
        assert!(
            plan.multi_bytes >= (1 << 20) * L2_MULTIPLE,
            "per-thread slice of {} bytes would sit in L2",
            plan.multi_bytes
        );
        assert!(
            plan.peak_alloc_bytes as f64 <= (256u64 << 30) as f64 * MEMORY_BUDGET_FRACTION,
            "peak allocation {} exceeds the budget",
            plan.peak_alloc_bytes
        );
    }

    /// The budget must win over the ideal working set. A benchmark that pushes
    /// a live host into swap is an outage, and the number it produces describes
    /// the swap device.
    #[test]
    fn a_constrained_machine_gets_a_smaller_working_set_not_swap() {
        let plan = Plan::for_machine(&facts(32 << 20, 1 << 20, Some(512 << 20)), 16);
        assert!(
            plan.peak_alloc_bytes as f64 <= (512u64 << 20) as f64 * MEMORY_BUDGET_FRACTION,
            "peak allocation {} exceeds the budget",
            plan.peak_alloc_bytes
        );
        assert!(
            plan.single_contaminated,
            "a single-threaded working set this far below cache must be flagged, not published \
             as DRAM"
        );
    }

    #[test]
    fn the_ceiling_bounds_a_host_that_reports_an_absurd_cache() {
        // Some hypervisors report the whole host's L3 to a small guest.
        let plan = Plan::for_machine(&facts(2 << 30, 1 << 20, Some(64u64 << 30)), 4);
        assert_eq!(plan.single_bytes, MAX_WORKING_SET);
        assert!(plan.multi_bytes <= MAX_WORKING_SET);
        // The honest consequence: against a claimed 2 GiB cache neither shape
        // can prove it reached DRAM, and both say so.
        assert!(plan.single_contaminated);
    }

    #[test]
    fn unknown_available_memory_is_treated_as_constrained_not_unlimited() {
        let unknown = Plan::for_machine(&facts(32 << 20, 1 << 20, None), 8);
        let generous = Plan::for_machine(&facts(32 << 20, 1 << 20, Some(64u64 << 30)), 8);
        assert!(
            unknown.peak_alloc_bytes <= generous.peak_alloc_bytes,
            "not knowing how much memory is free must never buy a larger allocation"
        );
    }

    #[test]
    fn a_host_with_no_cache_topology_falls_back_and_says_so() {
        let plan = Plan::for_machine(
            &MachineFacts {
                available_bytes: Some(64u64 << 30),
                ..Default::default()
            },
            4,
        );
        assert!(plan.llc_assumed, "the fallback must be recorded");
        assert_eq!(plan.llc_bytes, FALLBACK_LLC_BYTES);
        assert!(!plan.single_contaminated);
    }

    #[test]
    fn every_working_set_is_page_aligned_and_non_empty() {
        for threads in [1usize, 3, 64, 256] {
            for available in [None, Some(16 << 20), Some(64 << 20), Some(256u64 << 30)] {
                let plan = Plan::for_machine(&facts(16 << 20, 512 << 10, available), threads);
                for (name, bytes) in [
                    ("single", plan.single_bytes),
                    ("multi", plan.multi_bytes),
                    ("cache", plan.cache_bytes),
                ] {
                    assert!(
                        bytes >= 4096,
                        "{threads} threads, {name}: {bytes} too small"
                    );
                    assert_eq!(
                        bytes % 4096,
                        0,
                        "{threads} threads, {name}: not page aligned"
                    );
                }
                assert!(plan.cache_bytes <= plan.single_bytes);
                assert!(plan.cache_bytes <= plan.multi_bytes);
                assert!(plan.bytes(Access::SequentialRead, 1) > 0);
                assert!(plan.bytes(Access::SequentialRead, threads) > 0);
                assert!(plan.bytes(Access::CacheRead, threads) > 0);
            }
        }
    }

    #[test]
    fn the_cache_resident_set_is_the_same_size_in_both_shapes() {
        let plan = Plan::for_machine(&facts(32 << 20, 1 << 20, Some(64u64 << 30)), 8);
        assert_eq!(
            plan.bytes(Access::CacheRead, 1),
            plan.bytes(Access::CacheRead, 8),
            "the cache-resident scan must measure the same working set in both shapes"
        );
    }

    // --- permutation ------------------------------------------------------

    /// A pointer chase is only a latency measurement if it visits everything.
    ///
    /// A Fisher-Yates shuffle can produce several short cycles; a chase that
    /// falls into one would stay in cache and report a latency several times
    /// better than the machine can deliver. Sattolo guarantees one full cycle.
    #[test]
    fn the_permutation_is_a_single_full_length_cycle() {
        for len in [2usize, 3, 17, 1024] {
            let next = permutation(len, CORPUS_SEED);
            assert_eq!(next.len(), len);

            let mut visited = vec![false; len];
            let mut cursor = 0u32;
            for _ in 0..len {
                assert!(
                    !visited[cursor as usize],
                    "len {len}: revisited slot {cursor} before the cycle closed"
                );
                visited[cursor as usize] = true;
                cursor = next[cursor as usize];
            }
            assert_eq!(cursor, 0, "len {len}: the cycle must close on its start");
            assert!(
                visited.iter().all(|v| *v),
                "len {len}: not every slot visited"
            );
        }
    }

    #[test]
    fn the_permutation_is_deterministic_but_differs_per_seed() {
        assert_eq!(permutation(256, 7), permutation(256, 7));
        assert_ne!(permutation(256, 7), permutation(256, 8));
        // Degenerate lengths must not panic.
        assert_eq!(permutation(0, 1), Vec::<u32>::new());
        assert_eq!(permutation(1, 1), vec![0u32]);
    }

    #[test]
    fn traffic_accounting_follows_the_stream_convention() {
        let bytes = 8192;
        let one_way = Access::SequentialRead.bytes_per_pass(bytes);
        assert_eq!(one_way, 8192.0);
        assert_eq!(Access::SequentialCopy.bytes_per_pass(bytes), one_way * 2.0);
        assert_eq!(Access::Triad.bytes_per_pass(bytes), one_way * 3.0);
    }

    /// The chase must span the whole working set it was given.
    ///
    /// Regression: the permutation was sized as though its entries were 8 bytes
    /// wide like the payload buffers, but it is a `u32` array - so the chase
    /// covered half the intended working set. On a machine with a large
    /// last-level cache that is enough for it to partly fit, and the reported
    /// latency would be one the hardware cannot actually deliver.
    #[test]
    fn the_pointer_chase_spans_its_whole_working_set() {
        let bytes = 8 << 20;
        let entries = Access::LatencyRandom.permutation_entries(bytes);
        assert_eq!(
            entries as u64 * INDEX_BYTES,
            bytes,
            "the chase must walk the full working set, not half of it"
        );

        let state = WorkingSet::prepare(Access::LatencyRandom, bytes, CORPUS_SEED);
        // The chase is chunked: a pass is a fixed number of dependent loads,
        // not a full traversal, so it can be calibrated to a repetition target.
        assert_eq!(state.chase_steps(), LATENCY_STEPS_PER_PASS);
        assert!(
            (entries as u64) > LATENCY_STEPS_PER_PASS,
            "the cycle must be far longer than one chunk, or the chase would \
             re-walk lines it just pulled into cache"
        );
        assert!(
            state.a.is_empty() && state.b.is_empty() && state.c.is_empty(),
            "the chase reads no payload buffer, so allocating one is working set \
             paid for and never touched"
        );

        // The gather does index a payload buffer, one entry per element.
        let gather = WorkingSet::prepare(Access::RandomRead, bytes, CORPUS_SEED);
        assert_eq!(gather.a.len(), (bytes / ELEM) as usize);
        assert_eq!(gather.permutation.len(), gather.a.len());
    }

    #[test]
    fn estimated_duration_is_not_optimistic() {
        let module = MemoryBandwidth::new();
        let params = ModuleParams::for_profile(Profile::Quick);
        assert!(module.estimated_duration_s(&params) > 0);
        // 13 shapes x (1 warmup + 5 measured + 2 calibration) x 200 ms, plus a
        // second per shape for allocation and first touch.
        assert_eq!(
            module.estimated_duration_s(&params),
            (13 * 8 * 200_u64).div_ceil(1000) + 13
        );
    }
}
