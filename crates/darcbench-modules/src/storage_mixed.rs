//! `storage.mixed` - the Phase 2 storage module.
//!
//! # What it measures
//!
//! | Metric | Unit | What it predicts |
//! |---|---|---|
//! | `sequential_read.qd1` | MiB/s | Streaming a large file: backups, media, log shipping |
//! | `sequential_write.qd1` | MiB/s | Bulk ingest, restores, image writes |
//! | `random_read_4k.qd1` | IOPS | One request at a time: an index probe with nothing to overlap it |
//! | `random_read_4k.qd16` | IOPS | A busy server with many requests in flight |
//! | `random_write_4k.qd1` | IOPS | Synchronous small writes |
//! | `random_write_4k.qd16` | IOPS | Concurrent small writes |
//! | `random_mixed_4k.qd16` | IOPS | 70/30 read/write, the shape a database under load actually makes |
//! | `latency_read_4k.p99` | ms | The slow 1% of reads - what a user notices |
//! | `latency_write_4k.p99` | ms | The slow 1% of writes |
//! | `latency_fsync.mean` | ms | Durability: what every database commit pays |
//!
//! Queue depth is reported in the metric key rather than blended away, because
//! the two answers are genuinely different: a device can be excellent at
//! QD16 and mediocre at QD1, and a single-threaded application only ever feels
//! the second one. Depths are 1 and 16 rather than a vanity 256 -
//! `docs/BENCHMARK-METHODOLOGY.md` calls for realistic depths, and a web server
//! does not run 256 outstanding I/Os.
//!
//! # Why this is a native implementation and not an fio adapter
//!
//! `docs/ROADMAP.md` lists "fio adapter" against this deliverable, and fio's
//! *methodology* is what this module follows: `direct=1` to bypass the page
//! cache, a warm-up before recording, realistic queue depths. What it does not
//! do is shell out to fio, because the product bible makes a single static
//! binary a hard requirement - `scp` it to a server and run it, no package
//! manager. A storage module that only worked where fio happened to be
//! installed would leave the Storage category empty on most hosts, and every
//! run `Partial` - which is the exact condition Phase 2 exists to escape. An
//! fio adapter remains worth adding later as a cross-check and as a route to
//! io_uring-quality queue depths; it is an enrichment, not the foundation.
//!
//! # Safety
//!
//! This is the first module that writes, so the rules in
//! `docs/BENCHMARK-METHODOLOGY.md` are enforced mechanically rather than by
//! convention:
//!
//! * **Regular files only, never a block device.** The path is composed by the
//!   agent's `StatePath` and this module appends one fixed, compile-time name
//!   to it. The file is opened `O_NOFOLLOW`, so a symlink planted at that path
//!   cannot redirect the open at a device, and the open file is then asserted
//!   to be a regular file before a single byte is written.
//! * **Only under the DARCBench state directory.** The module has no other
//!   path and constructs none.
//! * **Bounded by free space.** The fixture is a fraction of what is actually
//!   free, and unknown free space is treated as unsafe rather than unlimited.
//! * **Cleanup on every exit path.** The fixture removes itself on `Drop`, so
//!   an error, a panic or a cancellation all leave the disk as they found it. A
//!   stale fixture from a previous crashed run is removed before a new one is
//!   created.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::Instant;

use darcbench_protocol::metrics::{Direction, Metric, Warning, WarningCode};
use darcbench_protocol::stats::{outlier_indices, summarize};
use darcbench_protocol::ModuleId;

use crate::harness::{calibrate_with, time_reps};
use crate::module::{
    BenchmarkModule, ModuleError, ModuleManifest, ModuleOutput, ModuleParams, ModuleReporter,
    SafetyClass,
};
use crate::workloads::{SplitMix64, CORPUS_SEED};

/// Workload-definition version. Major bump = results are not comparable.
pub const VERSION: &str = "1.0.0";

/// The module's identifier, validated against the [`ModuleId`] grammar by a
/// unit test in this file.
pub const MODULE_ID: &str = "storage.mixed";

/// The one and only file name this module ever creates. A compile-time
/// constant, appended to an already-validated directory: there is no string
/// from any caller anywhere in the path.
const FIXTURE_NAME: &str = "storage-mixed.fixture";

/// Alignment for `O_DIRECT`, which requires the buffer address, the file offset
/// and the transfer length to be multiples of the logical block size. 4096
/// satisfies every device DARCBench targets; 512-byte-sector devices accept it
/// too, since it is a multiple of their requirement.
const ALIGN: usize = 4096;

/// Small-I/O block size. 4 KiB is the unit databases, filesystems and page
/// caches all work in, so it is the size whose latency a real workload feels.
const SMALL_BLOCK: usize = 4096;

/// Large-I/O block size for the streaming workloads.
const LARGE_BLOCK: usize = 1 << 20;

/// Preferred fixture size. Large enough that random offsets span a meaningful
/// range of the device rather than one flash die.
const PREFERRED_FIXTURE_BYTES: u64 = 2 << 30;

/// Smallest fixture that still means anything. Below this the offsets are so
/// clustered that the result describes a cache, not a device.
const MIN_FIXTURE_BYTES: u64 = 64 << 20;

/// Share of free space the fixture may occupy.
///
/// The agent's preflight already refuses a run that would not leave a 2 GiB
/// margin; this is the module declining to be the reason a disk fills up even
/// when preflight said yes.
const FREE_SPACE_SHARE: f64 = 0.125;

/// Queue depth for the concurrent shapes.
const DEEP_QUEUE: usize = 16;

/// Read share of the mixed workload. 70/30 is the conventional OLTP-ish mix and
/// is what makes this metric different from running the two separately: a
/// device's read path and write path contend.
const MIXED_READ_SHARE: u64 = 70;

/// Assumed throughput used only to *estimate* write volume before the run, for
/// the preflight wear disclosure. Deliberately generous, because an estimate
/// that understates flash wear is worse than one that overstates it.
const ASSUMED_WRITE_MIB_S: u64 = 600;

// ---------------------------------------------------------------------------
// Access patterns
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Access {
    SequentialRead,
    SequentialWrite,
    RandomRead,
    RandomWrite,
    RandomMixed,
    Fsync,
}

impl Access {
    fn writes(self) -> bool {
        matches!(
            self,
            Self::SequentialWrite | Self::RandomWrite | Self::RandomMixed | Self::Fsync
        )
    }

    fn block_bytes(self) -> usize {
        match self {
            Self::SequentialRead | Self::SequentialWrite => LARGE_BLOCK,
            _ => SMALL_BLOCK,
        }
    }
}

/// One measured workload: an access pattern at a stated queue depth.
struct Workload {
    access: Access,
    queue_depth: usize,
    /// Primary metric key.
    key: &'static str,
    unit: &'static str,
    direction: Direction,
    label: &'static str,
    /// Secondary tail-latency metric derived from the same repetitions.
    ///
    /// Only the QD1 shapes carry one: at depth 16 a per-operation latency is
    /// dominated by queueing behind the other fifteen, which is a real effect
    /// but not the device's response time. Reporting it under a name that says
    /// "latency" would invite exactly the wrong reading.
    latency_key: Option<&'static str>,
}

const WORKLOADS: &[Workload] = &[
    Workload {
        access: Access::SequentialRead,
        queue_depth: 1,
        key: "sequential_read.qd1",
        unit: "MiB/s",
        direction: Direction::HigherIsBetter,
        label: "Sequential read",
        latency_key: None,
    },
    Workload {
        access: Access::SequentialWrite,
        queue_depth: 1,
        key: "sequential_write.qd1",
        unit: "MiB/s",
        direction: Direction::HigherIsBetter,
        label: "Sequential write",
        latency_key: None,
    },
    Workload {
        access: Access::RandomRead,
        queue_depth: 1,
        key: "random_read_4k.qd1",
        unit: "IOPS",
        direction: Direction::HigherIsBetter,
        label: "Random 4K read (QD1)",
        latency_key: Some("latency_read_4k.p99"),
    },
    Workload {
        access: Access::RandomRead,
        queue_depth: DEEP_QUEUE,
        key: "random_read_4k.qd16",
        unit: "IOPS",
        direction: Direction::HigherIsBetter,
        label: "Random 4K read (QD16)",
        latency_key: None,
    },
    Workload {
        access: Access::RandomWrite,
        queue_depth: 1,
        key: "random_write_4k.qd1",
        unit: "IOPS",
        direction: Direction::HigherIsBetter,
        label: "Random 4K write (QD1)",
        latency_key: Some("latency_write_4k.p99"),
    },
    Workload {
        access: Access::RandomWrite,
        queue_depth: DEEP_QUEUE,
        key: "random_write_4k.qd16",
        unit: "IOPS",
        direction: Direction::HigherIsBetter,
        label: "Random 4K write (QD16)",
        latency_key: None,
    },
    Workload {
        access: Access::RandomMixed,
        queue_depth: DEEP_QUEUE,
        key: "random_mixed_4k.qd16",
        unit: "IOPS",
        direction: Direction::HigherIsBetter,
        label: "Random 4K mixed 70/30 (QD16)",
        latency_key: None,
    },
    Workload {
        access: Access::Fsync,
        queue_depth: 1,
        key: "latency_fsync.mean",
        unit: "ms",
        direction: Direction::LowerIsBetter,
        label: "fsync latency",
        latency_key: None,
    },
];

/// Tail-latency percentile reported by the QD1 shapes.
const TAIL_PERCENTILE: f64 = 99.0;

/// Fewest latency samples a percentile is reported from. Below this a "p99" is
/// one or two observations wearing a statistical name.
const MIN_LATENCY_SAMPLES: usize = 100;

// ---------------------------------------------------------------------------
// Aligned buffer
// ---------------------------------------------------------------------------

/// A buffer whose start address is aligned to [`ALIGN`].
///
/// `O_DIRECT` rejects an unaligned user buffer with `EINVAL`. The workspace
/// forbids `unsafe_code`, so there is no allocator call available to request
/// alignment directly: over-allocating and taking an aligned subslice is the
/// portable way to get there, and costs one extra page per buffer.
struct AlignedBuffer {
    storage: Vec<u8>,
    offset: usize,
    len: usize,
}

impl AlignedBuffer {
    fn new(len: usize, fill: u8) -> Self {
        let storage = vec![fill; len + ALIGN];
        // Reading the address as an integer is safe; nothing is dereferenced,
        // and the allocation is never resized afterwards so it cannot move.
        let addr = storage.as_ptr() as usize;
        let offset = (ALIGN - (addr % ALIGN)) % ALIGN;
        Self {
            storage,
            offset,
            len,
        }
    }

    fn as_slice(&self) -> &[u8] {
        &self.storage[self.offset..self.offset + self.len]
    }

    fn as_mut_slice(&mut self) -> &mut [u8] {
        &mut self.storage[self.offset..self.offset + self.len]
    }

    /// True when the usable slice really starts on an [`ALIGN`] boundary.
    fn is_aligned(&self) -> bool {
        self.as_slice().as_ptr() as usize % ALIGN == 0
    }
}

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

/// The test file, and the promise to remove it.
///
/// `Drop` is what makes the cleanup guarantee hold on *every* exit path: an
/// early return, a cancellation, a propagated I/O error and a panic all unwind
/// through it. Explicit cleanup at the end of `run` would cover only the happy
/// path, which is the one that never needed it.
struct Fixture {
    path: PathBuf,
    file: File,
    bytes: u64,
    /// False when the filesystem refused `O_DIRECT` and the module fell back to
    /// buffered I/O. The result is then describing the page cache as much as
    /// the device, and says so.
    direct: bool,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        remove_fixture(&self.path);
    }
}

fn remove_fixture(path: &Path) {
    if let Err(error) = std::fs::remove_file(path) {
        // Nothing useful can be done here, but leaving gigabytes behind
        // silently would be worse than a log line.
        if error.kind() != io::ErrorKind::NotFound {
            eprintln!(
                "darcbench: could not remove the storage fixture at {}: {error}",
                crate::runtime_exec::elide_home(path)
            );
        }
    }
}

/// Removes a path on drop unless it is disarmed first.
///
/// [`Fixture`]'s own `Drop` cannot cover the window between the file appearing
/// on disk and the `Fixture` being constructed - and that window contains the
/// expensive part, filling gigabytes. A `fill` that fails on ENOSPC part-way
/// through, which is what happens when a co-tenant fills the disk during a long
/// run, would otherwise leave everything it had written behind until some later
/// storage run's stale-fixture sweep. The manifest promises removal "on every
/// exit path including errors and cancellation", and this is what makes the
/// error paths part of that promise.
struct PathGuard {
    path: PathBuf,
    armed: bool,
}

impl PathGuard {
    fn arm(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    /// Hands ownership of the file over to something else that will remove it.
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for PathGuard {
    fn drop(&mut self) {
        if self.armed {
            remove_fixture(&self.path);
        }
    }
}

impl Fixture {
    /// Creates the fixture under an already-validated scratch directory.
    fn create(scratch: &Path, bytes: u64) -> io::Result<Self> {
        std::fs::create_dir_all(scratch)?;
        let path = scratch.join(FIXTURE_NAME);

        // A fixture left by a crashed run is stale by definition: it may be the
        // wrong size and its contents are unknown. Remove it rather than
        // reusing it.
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }

        // `O_NOFOLLOW`: if anything has planted a symlink at this path between
        // the removal above and now, the open fails rather than following it -
        // which is the difference between writing to our own scratch file and
        // writing to whatever the link points at.
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
            .open(&path)?;

        // The file exists from here on, so every failure below has to remove
        // it. `Fixture::drop` cannot: there is no `Fixture` yet.
        let mut guard = PathGuard::arm(path.clone());

        // Belt and braces: assert the thing now open really is a regular file.
        // "Never a raw block device, ever, by any flag" is an absolute rule, so
        // it gets an assertion and not just a careful path.
        let kind = file.metadata()?.file_type();
        if !kind.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "storage fixture is not a regular file; refusing to write",
            ));
        }

        fill(&mut file, bytes)?;
        drop(file);

        // Reopen for measurement, requesting O_DIRECT so reads and writes go to
        // the device instead of the page cache. Several filesystems - tmpfs,
        // some overlay and network mounts - refuse it, so the fallback is
        // buffered I/O with the result marked as describing the cache too.
        let (file, direct) = match Self::open_measuring(&path, true) {
            Ok(file) => (file, true),
            Err(_) => (Self::open_measuring(&path, false)?, false),
        };

        // The `Fixture` about to be returned owns the removal from now on.
        guard.disarm();
        Ok(Self {
            path,
            file,
            bytes,
            direct,
        })
    }

    fn open_measuring(path: &Path, direct: bool) -> io::Result<File> {
        let mut flags = rustix::fs::OFlags::NOFOLLOW;
        if direct {
            flags |= rustix::fs::OFlags::DIRECT;
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(flags.bits() as i32)
            .open(path)?;
        if direct {
            // Opening can succeed on a filesystem that then rejects the first
            // aligned transfer, so prove O_DIRECT works before trusting it.
            let mut probe = AlignedBuffer::new(ALIGN, 0);
            file.read_at(probe.as_mut_slice(), 0)?;
        }
        Ok(file)
    }
}

/// Writes `bytes` of non-zero data, so the file is fully allocated.
///
/// A sparse file would let reads be answered from a hole without the device
/// being touched at all, which would report a storage speed the machine does
/// not have.
fn fill(file: &mut File, bytes: u64) -> io::Result<()> {
    let mut rng = SplitMix64::new(CORPUS_SEED ^ 0x5707_4A6E);
    let mut block = vec![0u8; LARGE_BLOCK];
    for chunk in block.chunks_mut(8) {
        let value = rng.next_u64().to_le_bytes();
        chunk.copy_from_slice(&value[..chunk.len()]);
    }

    let mut written = 0u64;
    while written < bytes {
        let take = ((bytes - written) as usize).min(block.len());
        file.write_at(&block[..take], written)?;
        written += take as u64;
    }
    // Durable before measuring, so the first timed workload is not competing
    // with writeback of the fixture it is about to read.
    file.sync_all()
}

// ---------------------------------------------------------------------------
// Sizing
// ---------------------------------------------------------------------------

/// The fixture size decision, recorded so a reader can see what was measured.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Plan {
    pub fixture_bytes: u64,
    /// True when free space, not the preferred size, chose the fixture.
    pub constrained: bool,
}

impl Plan {
    /// Sizes the fixture against the free space actually available.
    ///
    /// Unknown free space yields the minimum, never the preferred size: the
    /// same rule preflight applies when it treats an unknown disk as unsafe.
    pub fn for_free_space(free_bytes: Option<u64>) -> Self {
        let Some(free) = free_bytes.filter(|f| *f > 0) else {
            return Self {
                fixture_bytes: MIN_FIXTURE_BYTES,
                constrained: true,
            };
        };
        let affordable = (free as f64 * FREE_SPACE_SHARE) as u64;
        // `MIN_FIXTURE_BYTES <= PREFERRED_FIXTURE_BYTES` is a compile-time
        // invariant, so the clamp cannot invert.
        const _: () = assert!(MIN_FIXTURE_BYTES <= PREFERRED_FIXTURE_BYTES);
        let fixture_bytes = affordable.clamp(MIN_FIXTURE_BYTES, PREFERRED_FIXTURE_BYTES)
            // Whole large blocks, so every offset the workloads compute is
            // aligned and in range.
            & !(LARGE_BLOCK as u64 - 1);
        Self {
            fixture_bytes: fixture_bytes.max(LARGE_BLOCK as u64),
            constrained: affordable < PREFERRED_FIXTURE_BYTES,
        }
    }

    /// True when there is not even room for the minimum fixture.
    pub fn fits_in(self, free_bytes: Option<u64>) -> bool {
        match free_bytes {
            Some(free) => self.fixture_bytes < free,
            None => false,
        }
    }
}

// ---------------------------------------------------------------------------
// Execution
// ---------------------------------------------------------------------------

/// What one timed repetition performed.
struct Completed {
    ops: u64,
    bytes: u64,
    /// Per-operation latencies in milliseconds, only when asked for.
    latencies: Vec<f64>,
}

/// Runs `ops` operations of `workload` against the fixture.
///
/// Queue depth is reached with one thread per outstanding operation, each doing
/// synchronous positional I/O. That is `fio`'s `numjobs` model rather than its
/// `iodepth` model: without `io_uring` or `libaio` - neither of which is
/// reachable from safe Rust - concurrent threads are how a device is given more
/// than one request to work on, and the device sees the same thing either way.
/// It is charged honestly: thread spawn is inside the timed region.
fn run_ops(
    fixture: &Fixture,
    workload: &Workload,
    ops: u64,
    capture_latency: bool,
    seed: u64,
) -> io::Result<Completed> {
    let threads = workload.queue_depth.max(1);
    let block = workload.access.block_bytes();
    let per_thread = ops.div_ceil(threads as u64).max(1);
    // Offsets are drawn from whole blocks inside the fixture.
    let blocks = (fixture.bytes / block as u64).max(1);

    let results: Vec<io::Result<Completed>> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..threads)
            .map(|thread| {
                let thread_seed = seed ^ (thread as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
                scope.spawn(move || {
                    thread_ops(
                        fixture,
                        workload,
                        per_thread,
                        blocks,
                        capture_latency,
                        thread_seed,
                    )
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| {
                handle
                    .join()
                    .unwrap_or_else(|_| Err(io::Error::other("a storage worker thread panicked")))
            })
            .collect()
    });

    let mut total = Completed {
        ops: 0,
        bytes: 0,
        latencies: Vec::new(),
    };
    for result in results {
        let part = result?;
        total.ops += part.ops;
        total.bytes += part.bytes;
        total.latencies.extend(part.latencies);
    }
    Ok(total)
}

fn thread_ops(
    fixture: &Fixture,
    workload: &Workload,
    ops: u64,
    blocks: u64,
    capture_latency: bool,
    seed: u64,
) -> io::Result<Completed> {
    let block = workload.access.block_bytes();
    let mut buffer = AlignedBuffer::new(block, 0xA5);
    if !buffer.is_aligned() {
        return Err(io::Error::other(
            "could not obtain an aligned buffer for direct I/O",
        ));
    }
    let mut rng = SplitMix64::new(seed);
    let mut latencies = if capture_latency {
        Vec::with_capacity(ops as usize)
    } else {
        Vec::new()
    };

    // Sequential shapes start at a per-thread stripe so concurrent threads do
    // not all replay the same offsets.
    let mut cursor = seed % blocks;
    let mut bytes = 0u64;

    for _ in 0..ops {
        let offset = match workload.access {
            Access::SequentialRead | Access::SequentialWrite => {
                let at = cursor * block as u64;
                cursor = (cursor + 1) % blocks;
                at
            }
            _ => (rng.next_u64() % blocks) * block as u64,
        };

        let started = capture_latency.then(Instant::now);
        match workload.access {
            Access::SequentialRead | Access::RandomRead => {
                fixture.file.read_exact_at(buffer.as_mut_slice(), offset)?;
            }
            Access::SequentialWrite | Access::RandomWrite => {
                fixture.file.write_all_at(buffer.as_slice(), offset)?;
            }
            Access::RandomMixed => {
                if rng.next_u64() % 100 < MIXED_READ_SHARE {
                    fixture.file.read_exact_at(buffer.as_mut_slice(), offset)?;
                } else {
                    fixture.file.write_all_at(buffer.as_slice(), offset)?;
                }
            }
            Access::Fsync => {
                // The durability cost, measured the way a database pays it:
                // one small write followed by the barrier that makes it
                // survive a power cut. `sync_data` rather than `sync_all`
                // because a database fsyncs data, not metadata, on the commit
                // path.
                fixture.file.write_all_at(buffer.as_slice(), offset)?;
                fixture.file.sync_data()?;
            }
        }
        if let Some(started) = started {
            latencies.push(started.elapsed().as_secs_f64() * 1000.0);
        }
        bytes += block as u64;
    }

    Ok(Completed {
        ops,
        bytes,
        latencies,
    })
}

/// The `p`-th percentile of `values`, by nearest rank.
///
/// Nearest-rank rather than an interpolating definition: a latency percentile
/// should be a latency that actually occurred, not an average of two that
/// straddle it.
fn percentile(values: &mut [f64], p: f64) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(f64::total_cmp);
    let rank = ((p / 100.0) * values.len() as f64).ceil() as usize;
    values
        .get(rank.saturating_sub(1).min(values.len() - 1))
        .copied()
}

/// How much the last third of the measured repetitions gave up against the
/// first third.
///
/// `docs/BENCHMARK-METHODOLOGY.md`: *"short random-write tests on consumer SSDs
/// measure the SLC cache, not the drive. Steady-state behaviour must be
/// reported separately from burst."* A full preconditioning pass is not
/// affordable inside a benchmark run, but the *drift* within one is cheap to
/// measure and is the signal that the cache ran out - so it is reported instead
/// of the burst number being quietly published as the drive's speed.
/// The ratio is **direction-adjusted**, so it always means "fraction of the
/// opening performance still there at the end", whatever the metric counts. A
/// bare `last / first` only means that for a throughput metric; on
/// `latency_fsync.mean`, where the numbers grow as the device gets worse, it
/// inverts. An fsync that degraded from 0.4 ms to 1.6 ms - exactly the SLC
/// cache filling that this check exists to catch - would score 4.0 and pass
/// silently, while a device that *warmed up* would score 0.25 and be degraded
/// for improving. `darcbench-scoring::sustained` makes the same adjustment for
/// the same reason.
fn steady_state_ratio(measured: &[f64], direction: Direction) -> Option<f64> {
    // Four is the floor at which "first part versus last part" means anything.
    // It used to be six, which quietly disabled the whole signal on the `quick`
    // profile - the one most people run - because that profile measures five
    // repetitions. A methodology requirement that silently does not apply to
    // the default profile is not a requirement.
    if measured.len() < 4 {
        return None;
    }
    let third = (measured.len() / 3).max(1);
    let median = |slice: &[f64]| -> Option<f64> {
        let mut v = slice.to_vec();
        v.sort_by(f64::total_cmp);
        v.get(v.len() / 2).copied()
    };
    let first = median(&measured[..third])?;
    let last = median(&measured[measured.len() - third..])?;
    // Both ends must be positive: the inverted direction divides by `last`, and
    // a zero or a NaN on either side makes the ratio meaningless rather than
    // merely extreme.
    if first <= 0.0 || last <= 0.0 || first.is_nan() || last.is_nan() {
        return None;
    }
    let retained = match direction {
        Direction::HigherIsBetter => last / first,
        Direction::LowerIsBetter => first / last,
    };
    retained.is_finite().then_some(retained)
}

/// Below this the run visibly slowed as it went: an SLC cache filling up, a
/// thermal limit, or a burst allowance running out.
const STEADY_STATE_FLOOR: f64 = 0.70;

/// Operations run and discarded before calibrating each workload.
///
/// Enough to clear the queue and let the device settle into the access pattern
/// about to be measured, cheap enough not to be a measurable part of the run's
/// flash wear.
const RAMP_OPS: u64 = 64;

// ---------------------------------------------------------------------------
// The module
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct StorageMixed {
    manifest: ModuleManifest,
}

impl Default for StorageMixed {
    fn default() -> Self {
        Self::new()
    }
}

impl StorageMixed {
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
                title: "Mixed storage workloads".to_string(),
                purpose: "Measure sequential and 4K random read and write throughput at realistic \
                          queue depths, tail latency, and fsync durability cost, against a \
                          regular file opened with direct I/O so the result describes the device \
                          rather than the page cache."
                    .to_string(),
                safety_class: SafetyClass::WritesTemporaryFiles,
                dependencies: vec![],
                max_bytes_written: PREFERRED_FIXTURE_BYTES,
                max_network_bytes: 0,
                cleanup: "The single fixture file is removed when the module returns, on every \
                          exit path including errors and cancellation, and a fixture left behind \
                          by a crashed run is removed before a new one is created."
                    .to_string(),
                validation: vec![
                    "Every workload must produce at least 5 measured repetitions.".to_string(),
                    "Repetitions shorter than 20 ms are rejected as timer-noise dominated."
                        .to_string(),
                    "A filesystem that refuses direct I/O downgrades the result: the figures then \
                     describe the page cache as well as the device."
                        .to_string(),
                    "Throughput falling below 70% of its opening level across the repetitions is \
                     reported as a steady-state failure, not averaged away."
                        .to_string(),
                    "Coefficient of variation above 0.20 raises a high-variance warning and \
                     downgrades the result to Degraded."
                        .to_string(),
                ],
                limitations: vec![
                    "Measures the filesystem and the storage stack as configured, not the bare \
                     device: RAID, LVM, ZFS, virtio and the mount options all sit in the path and \
                     are recorded in the environment snapshot for that reason."
                        .to_string(),
                    "Queue depth is reached with one thread per outstanding operation rather than \
                     an asynchronous submission queue, so a small amount of scheduling overhead is \
                     charged to the result at depth 16."
                        .to_string(),
                    "No SSD preconditioning is performed. A drive that has been idle will report \
                     its burst behaviour in the first repetitions; the steady-state ratio is \
                     published so that is visible rather than hidden, but a full preconditioning \
                     pass would take hours and write far more than a benchmark should."
                        .to_string(),
                    "The fixture is a fraction of free space, so a nearly-full disk is measured \
                     over a smaller offset range than an empty one. The size used is recorded."
                        .to_string(),
                    "fsync latency depends on whether the device or its controller honours cache \
                     flushes. A drive that lies about flushing will report an excellent number \
                     here, and no benchmark run from userspace can detect that."
                        .to_string(),
                ],
                comparability: vec![
                    "module.version".to_string(),
                    "agent.build_target".to_string(),
                    // Recorded by this module rather than read from the inventory: the
                    // inventory lists devices, and what decides whether two storage results
                    // mean the same thing is the filesystem the fixture was written *on*.
                    "filesystem".to_string(),
                    "plan.fixture_bytes".to_string(),
                    "plan.direct_io".to_string(),
                ],
                // Storage is physically noisier than CPU or memory: background
                // writeback, garbage collection and neighbouring tenants all
                // land here. `docs/BENCHMARK-METHODOLOGY.md` targets < 0.10 for
                // a good run and 0.20 as the acceptable ceiling.
                stability_cv_bound: 0.20,
            },
        }
    }
}

impl BenchmarkModule for StorageMixed {
    fn manifest(&self) -> &ModuleManifest {
        &self.manifest
    }

    fn estimated_duration_s(&self, params: &ModuleParams) -> u64 {
        let shapes = WORKLOADS.len() as u64;
        let reps = (params.warmup_reps + params.measured_reps) as u64;
        let measuring = (shapes * (reps + 2) * params.target_rep_ms).div_ceil(1000);
        // Plus creating and flushing the fixture before anything is measured.
        let plan = Plan::for_free_space(params.facts.free_scratch_bytes);
        let fill_s = (plan.fixture_bytes / (1 << 20)).div_ceil(ASSUMED_WRITE_MIB_S);
        measuring + fill_s
    }

    fn estimated_write_volume_bytes(&self, params: &ModuleParams) -> u64 {
        let plan = Plan::for_free_space(params.facts.free_scratch_bytes);
        let reps = (params.warmup_reps + params.measured_reps + 2) as u64;
        let write_shapes = WORKLOADS.iter().filter(|w| w.access.writes()).count() as u64;
        // Each write repetition runs for roughly `target_rep_ms`, so the volume
        // is that duration times an assumed rate. Random and fsync shapes write
        // far less than this; overstating is the safe direction for a wear
        // disclosure.
        let per_rep = (ASSUMED_WRITE_MIB_S << 20) * params.target_rep_ms / 1000;
        plan.fixture_bytes + write_shapes * reps * per_rep
    }

    fn run(
        &self,
        params: &ModuleParams,
        reporter: &dyn ModuleReporter,
    ) -> Result<ModuleOutput, ModuleError> {
        let Some(scratch) = params.scratch_dir.as_deref() else {
            return Err(ModuleError::Precondition(
                "no scratch directory was provided, and this module will not choose one".into(),
            ));
        };

        let plan = Plan::for_free_space(params.facts.free_scratch_bytes);
        if !plan.fits_in(params.facts.free_scratch_bytes) {
            return Err(ModuleError::Precondition(format!(
                "a {} MiB fixture does not fit in the free space reported for {}",
                plan.fixture_bytes >> 20,
                crate::runtime_exec::elide_home(scratch)
            )));
        }
        if reporter.is_cancelled() {
            return Err(ModuleError::Cancelled);
        }

        // From here on the fixture exists, and its `Drop` owns removing it.
        let fixture = Fixture::create(scratch, plan.fixture_bytes)
            .map_err(|e| ModuleError::Precondition(format!("could not create the fixture: {e}")))?;

        let mut metrics = Vec::new();
        let mut warnings = Vec::new();
        let mut calibration = serde_json::Map::new();
        let mut steady_state = serde_json::Map::new();

        if !fixture.direct {
            let warning = Warning {
                code: WarningCode::ValidationFailed,
                message: "This filesystem refused direct I/O, so reads and writes went through \
                          the page cache. The figures describe the cache as much as the device \
                          and must not be compared with a run that reached the disk."
                    .to_string(),
                metric_key: None,
            };
            reporter.warn(warning.clone());
            warnings.push(warning);
        }

        let total_units = WORKLOADS.len() as f64;
        let mut completed_units = 0.0f64;

        for (index, workload) in WORKLOADS.iter().enumerate() {
            if reporter.is_cancelled() {
                return Err(ModuleError::Cancelled);
            }
            let seed = CORPUS_SEED ^ (index as u64).wrapping_mul(0x2545_F491_4F6C_DD1D);
            let value_of = |done: &Completed, seconds: f64| -> f64 {
                match workload.access {
                    Access::SequentialRead | Access::SequentialWrite => {
                        done.bytes as f64 / (1024.0 * 1024.0) / seconds
                    }
                    // fsync is reported as the cost of one barrier.
                    Access::Fsync => seconds * 1000.0 / done.ops.max(1) as f64,
                    _ => done.ops as f64 / seconds,
                }
            };

            // Settle before calibrating. The previous workload leaves dirty
            // pages and an in-flight queue behind, and the first probe would
            // otherwise be charged for flushing them - which is not a
            // measurement of this workload at all. It poisons calibration
            // rather than just adding noise: one slow probe makes the search
            // extrapolate *downwards*, and an `fsync` shape calibrated from a
            // probe that absorbed the previous workload's writeback ends up
            // asking for a single operation per repetition. This is the ramp
            // period `docs/BENCHMARK-METHODOLOGY.md` calls for, and it is why
            // fio has one too.
            if let Err(error) = fixture.file.sync_all() {
                return Err(ModuleError::Workload(format!(
                    "{}: could not quiesce before calibrating: {error}",
                    workload.key
                )));
            }
            if let Err(error) = run_ops(&fixture, workload, RAMP_OPS, false, seed) {
                return Err(ModuleError::Workload(format!(
                    "{}: ramp failed: {error}",
                    workload.key
                )));
            }
            if reporter.is_cancelled() {
                return Err(ModuleError::Cancelled);
            }

            let mut io_error: Option<io::Error> = None;
            let ops = calibrate_with(params.target_rep_ms, reporter, |ops| {
                let started = Instant::now();
                match run_ops(&fixture, workload, ops, false, seed) {
                    Ok(_) => started.elapsed().as_secs_f64() * 1000.0,
                    Err(error) => {
                        io_error = Some(error);
                        // Report the target so the search stops immediately;
                        // the error is surfaced below.
                        params.target_rep_ms as f64
                    }
                }
            })?;
            if let Some(error) = io_error {
                return Err(ModuleError::Workload(format!("{}: {error}", workload.key)));
            }
            calibration.insert(
                format!("{}.operations", workload.key),
                serde_json::Value::from(ops),
            );

            // The QD1 shapes also produce a tail-latency metric, captured from
            // the same repetitions rather than by running the workload twice -
            // which would double both the time and the flash wear.
            let capture = workload.latency_key.is_some();
            let tail_per_rep = std::cell::RefCell::new(Vec::<f64>::new());
            let sample_counts = std::cell::RefCell::new(Vec::<usize>::new());
            let failure = std::cell::RefCell::new(Option::<io::Error>::None);

            let outcome = time_reps(
                params,
                reporter,
                workload.key,
                workload.unit,
                completed_units,
                total_units,
                |rep| {
                    let started = Instant::now();
                    let done = match run_ops(&fixture, workload, ops, capture, seed ^ rep as u64) {
                        Ok(done) => done,
                        Err(error) => {
                            *failure.borrow_mut() = Some(error);
                            return (0.0, 0.0);
                        }
                    };
                    let elapsed = started.elapsed();
                    let seconds = elapsed.as_secs_f64().max(f64::EPSILON);
                    if capture && rep >= params.warmup_reps {
                        let mut latencies = done.latencies.clone();
                        sample_counts.borrow_mut().push(latencies.len());
                        if let Some(tail) = percentile(&mut latencies, TAIL_PERCENTILE) {
                            tail_per_rep.borrow_mut().push(tail);
                        }
                    }
                    (value_of(&done, seconds), elapsed.as_secs_f64() * 1000.0)
                },
            )?;
            if let Some(error) = failure.into_inner() {
                return Err(ModuleError::Workload(format!("{}: {error}", workload.key)));
            }
            warnings.extend(outcome.warnings);

            let summary = summarize(&outcome.measured)
                .ok_or_else(|| ModuleError::NoSamples(workload.key.to_string()))?;

            if let Some(cv) = summary.cv {
                if summary.is_unstable(self.manifest.stability_cv_bound) {
                    let warning = Warning {
                        code: WarningCode::HighVariance,
                        message: format!(
                            "`{}` varied by {:.1}% between repetitions (bound {:.0}%). On shared \
                             storage this usually means a neighbouring tenant or background \
                             garbage collection rather than a measurement fault.",
                            workload.key,
                            cv * 100.0,
                            self.manifest.stability_cv_bound * 100.0
                        ),
                        metric_key: Some(workload.key.to_string()),
                    };
                    reporter.warn(warning.clone());
                    warnings.push(warning);
                }
            }

            // Steady state, for the shapes that write.
            if workload.access.writes() {
                if let Some(ratio) = steady_state_ratio(&outcome.measured, workload.direction) {
                    steady_state.insert(workload.key.to_string(), serde_json::json!(ratio));
                    if ratio < STEADY_STATE_FLOOR {
                        let warning = Warning {
                            code: WarningCode::ValidationFailed,
                            message: format!(
                                "`{}` finished at {:.0}% of the throughput it started at. That is \
                                 the shape of an SLC cache filling up, a burst allowance running \
                                 out or a thermal limit - the opening figure is not this device's \
                                 sustained speed.",
                                workload.key,
                                ratio * 100.0
                            ),
                            metric_key: Some(workload.key.to_string()),
                        };
                        reporter.warn(warning.clone());
                        warnings.push(warning);
                    }
                }
            }

            metrics.push(Metric {
                key: workload.key.to_string(),
                label: workload.label.to_string(),
                unit: workload.unit.to_string(),
                direction: workload.direction,
                value: summary.median,
                outliers: outlier_indices(&outcome.measured, 3.5),
                summary,
                samples: outcome.samples,
                measures_dispersion: false,
                tail_quantile: false,
            });

            if let Some(latency_key) = workload.latency_key {
                let tails = tail_per_rep.into_inner();
                let counts = sample_counts.into_inner();
                let thin = counts.iter().any(|n| *n < MIN_LATENCY_SAMPLES);
                match (summarize(&tails), thin) {
                    (Some(tail_summary), false) => {
                        // An erratic tail is a fact about the device, and this
                        // is the only place that knows enough to say so: the
                        // percentile, how many operations fed it, and that it
                        // is an order statistic rather than a mean.
                        //
                        // It is reported and not enforced. A tail quantile
                        // drifts between repetitions on a machine behaving
                        // perfectly, because a handful of slow operations
                        // decide it - so a bound on its variation would
                        // disqualify hardware rather than describe it. The
                        // widest bound in the suite, at three times the
                        // throughput bound, because even that is generous.
                        if let Some(cv) = tail_summary.cv {
                            if cv > self.manifest.stability_cv_bound * 3.0 {
                                warnings.push(Warning {
                                    // Informational, not `HighVariance`.
                                    // `WarningCode::degrades_result` is true for high
                                    // variance, so raising that code here would degrade
                                    // the module and land the run back in `Partial` by a
                                    // second route - undoing the exemption two fields up.
                                    code: WarningCode::Informational,
                                    message: format!(
                                        "`{latency_key}` varied by {:.0}% between repetitions. \
                                         The slow 1% of operations on this device is not \
                                         reproducible, which is itself a finding about the \
                                         device - worn flash, an unstable controller, or a \
                                         neighbour competing for it. The run stays \
                                         comparable: a tail quantile is not evidence about \
                                         the machine's steadiness the way a throughput \
                                         median is.",
                                        cv * 100.0
                                    ),
                                    metric_key: Some(latency_key.to_string()),
                                });
                            }
                        }
                        metrics.push(Metric {
                            key: latency_key.to_string(),
                            label: format!("{} p{TAIL_PERCENTILE:.0} latency", workload.label),
                            unit: "ms".to_string(),
                            direction: Direction::LowerIsBetter,
                            value: tail_summary.median,
                            outliers: outlier_indices(&tails, 3.5),
                            summary: tail_summary,
                            samples: Vec::new(),
                            measures_dispersion: false,
                            tail_quantile: true,
                        });
                    }
                    _ => {
                        // A percentile computed from a handful of observations
                        // is a number wearing a statistical name. Reporting the
                        // absence is honest; reporting the number would not be.
                        warnings.push(Warning {
                            code: WarningCode::ValidationFailed,
                            message: format!(
                                "`{latency_key}` was not reported: a repetition completed fewer \
                                 than {MIN_LATENCY_SAMPLES} operations, which is too few for a \
                                 p{TAIL_PERCENTILE:.0} to mean anything."
                            ),
                            metric_key: Some(latency_key.to_string()),
                        });
                    }
                }
            }

            completed_units += 1.0;
        }

        let mut context = serde_json::Map::new();
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
                "fixture_bytes": plan.fixture_bytes,
                "constrained_by_free_space": plan.constrained,
                "direct_io": fixture.direct,
                "small_block_bytes": SMALL_BLOCK,
                "large_block_bytes": LARGE_BLOCK,
                "deep_queue_depth": DEEP_QUEUE,
                "mixed_read_share_pct": MIXED_READ_SHARE,
            }),
        );
        // The single most important comparability fact this module has, and it
        // was declared and never recorded. ROADMAP.md names the hazard - "storage
        // behaviour varies across kernel versions, filesystems and whether
        // O_DIRECT is honoured at all" - and says it is mitigated by recording
        // the storage stack. The inventory records *devices*; what decides
        // whether two of these results mean the same thing is the filesystem
        // the fixture was written on, and nothing was recording that.
        context.insert(
            "filesystem".into(),
            serde_json::Value::from(filesystem_of(scratch)),
        );
        context.insert("calibration".into(), serde_json::Value::Object(calibration));
        context.insert(
            "steady_state_ratio".into(),
            serde_json::Value::Object(steady_state),
        );

        Ok(ModuleOutput {
            metrics,
            warnings,
            context,
        })
    }
}

/// The filesystem type a path is on, from `/proc/self/mountinfo`.
///
/// Read rather than asked, because asking means `statfs` and that means either
/// `libc` or `unsafe`, and this workspace forbids the second and has avoided
/// the first. `mountinfo` names the type directly and needs no lookup table
/// from a magic number.
///
/// The answer is the mount point with the **longest** prefix of the path, which
/// is what makes a scratch directory on its own mount report that mount rather
/// than `/`.
///
/// `"unknown"` when it cannot be determined, never a guess. A wrong filesystem
/// in a comparability key is worse than an absent one: it would let two results
/// from different stacks be compared as though they matched.
fn filesystem_of(path: &std::path::Path) -> String {
    let Ok(mountinfo) = std::fs::read_to_string("/proc/self/mountinfo") else {
        return "unknown".to_string();
    };
    let path = path.to_string_lossy();
    let mut best: Option<(usize, String)> = None;
    for line in mountinfo.lines() {
        // `36 35 98:0 /mnt1 /mnt2 rw,noatime - ext3 /dev/root rw,errors=continue`
        // The fields before ` - ` are variable in number; the filesystem type is
        // the first field after it, and the mount point is field five before it.
        let Some((before, after)) = line.split_once(" - ") else {
            continue;
        };
        let mount_point = before.split_whitespace().nth(4).unwrap_or_default();
        let fs_type = after.split_whitespace().next().unwrap_or_default();
        if mount_point.is_empty() || fs_type.is_empty() {
            continue;
        }
        // A prefix match on the string is not enough: `/var` is a prefix of
        // `/variable` and is not its mount point.
        let is_parent = path == mount_point
            || (path.starts_with(mount_point)
                && (mount_point == "/" || path.as_bytes().get(mount_point.len()) == Some(&b'/')));
        if is_parent
            && best
                .as_ref()
                .is_none_or(|(len, _)| mount_point.len() > *len)
        {
            best = Some((mount_point.len(), fs_type.to_string()));
        }
    }
    best.map(|(_, fs)| fs)
        .unwrap_or_else(|| "unknown".to_string())
}

#[cfg(test)]
// In tests, `unwrap`/`expect` panicking *is* the failure signal.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic_in_result_fn)]
mod tests {
    use super::*;
    use crate::module::{MachineFacts, NullReporter};
    use darcbench_protocol::Profile;

    /// A scratch directory that removes itself, so a failing test never leaves
    /// a fixture behind either.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "darcbench-storage-{name}-{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            ));
            Self(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Small, fast parameters that still exercise the whole path.
    fn fast_params(scratch: &Scratch) -> ModuleParams {
        ModuleParams {
            warmup_reps: 1,
            measured_reps: 5,
            target_rep_ms: 25,
            threads: 2,
            facts: MachineFacts {
                // Sized so the fixture lands on MIN_FIXTURE_BYTES.
                free_scratch_bytes: Some(1 << 30),
                ..Default::default()
            },
            scratch_dir: Some(scratch.0.clone()),
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
        let module = StorageMixed::new();
        let m = module.manifest();
        assert_eq!(m.id.as_str(), MODULE_ID);
        assert_eq!(m.version, VERSION);
        assert_eq!(
            m.safety_class,
            SafetyClass::WritesTemporaryFiles,
            "a module that writes must declare that it writes"
        );
        assert!(
            m.max_bytes_written > 0,
            "the disk guard needs a real upper bound"
        );
        assert_eq!(
            m.max_network_bytes, 0,
            "storage.mixed must not use the network"
        );
        assert!(
            m.dependencies.is_empty(),
            "the module must be self-contained"
        );
        assert!(!m.validation.is_empty());
        assert!(!m.limitations.is_empty());
        assert!(m.stability_cv_bound > 0.0);
    }

    #[test]
    fn metric_keys_are_unique_and_in_the_reference_alphabet() {
        let mut keys: Vec<&str> = Vec::new();
        for workload in WORKLOADS {
            keys.push(workload.key);
            if let Some(latency) = workload.latency_key {
                keys.push(latency);
            }
        }
        let unique: std::collections::BTreeSet<&&str> = keys.iter().collect();
        assert_eq!(keys.len(), unique.len(), "duplicate metric key");
        for key in keys {
            for segment in key.split('.') {
                let mut chars = segment.chars();
                assert!(
                    chars.next().is_some_and(|c| c.is_ascii_lowercase()),
                    "`{key}`: segment `{segment}` must start with a lowercase letter"
                );
                assert!(
                    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
                    "`{key}`: segment `{segment}` must match [a-z][a-z0-9_]*"
                );
            }
        }
    }

    // --- safety ------------------------------------------------------------

    /// The absolute rule: the module writes one file, under the directory it
    /// was given, and takes it away again.
    #[test]
    fn the_fixture_lives_only_under_the_scratch_directory_and_is_removed() {
        let scratch = Scratch::new("cleanup");
        let bytes = 1 << 20;
        let path = {
            let fixture = Fixture::create(&scratch.0, bytes).expect("fixture");
            assert_eq!(
                fixture.path.parent(),
                Some(scratch.0.as_path()),
                "the fixture must sit directly in the scratch directory"
            );
            assert_eq!(
                fixture.path.file_name().and_then(|n| n.to_str()),
                Some(FIXTURE_NAME)
            );
            assert!(fixture.path.is_file());
            assert_eq!(
                std::fs::metadata(&fixture.path).expect("metadata").len(),
                bytes,
                "the fixture must be fully allocated, not sparse"
            );
            fixture.path.clone()
        };
        assert!(
            !path.exists(),
            "dropping the fixture must remove it; a benchmark that leaves gigabytes behind is a \
             benchmark nobody runs twice"
        );
    }

    /// Cleanup must survive the unhappy paths, which are the ones that need it.
    #[test]
    fn the_fixture_is_removed_even_when_the_run_fails() {
        let scratch = Scratch::new("panic");
        let path = scratch.0.join(FIXTURE_NAME);

        let result = std::panic::catch_unwind(|| {
            let _fixture = Fixture::create(&scratch.0, 1 << 20).expect("fixture");
            panic!("simulated workload failure");
        });
        assert!(result.is_err(), "the panic must actually have happened");
        assert!(
            !path.exists(),
            "a fixture must not survive a panic; Drop is what makes the cleanup promise hold"
        );
    }

    /// The promise covers the window *before* the `Fixture` exists, too.
    ///
    /// `Drop` only runs on something that was constructed, and the expensive
    /// part - writing gigabytes - happens first. A `create` that fails on the
    /// way, which is what ENOSPC looks like when a co-tenant fills the disk
    /// mid-run, must not leave what it had already written behind.
    #[test]
    fn a_failed_create_leaves_nothing_behind() {
        let scratch = Scratch::new("create-fail");
        let path = scratch.0.join(FIXTURE_NAME);

        // Nothing on a real filesystem reliably fails `fill` on demand, so the
        // guard is exercised directly: it is the whole mechanism, and this
        // pins that it removes what it was armed on.
        std::fs::create_dir_all(&scratch.0).expect("scratch");
        std::fs::write(&path, b"partially written fixture").expect("write");
        {
            let _guard = PathGuard::arm(path.clone());
            assert!(path.exists());
        }
        assert!(
            !path.exists(),
            "an armed guard must remove the file it was armed on"
        );

        // ...and a disarmed guard must not, or a successful `create` would
        // delete the fixture it just built.
        std::fs::write(&path, b"handed over to the Fixture").expect("write");
        {
            let mut guard = PathGuard::arm(path.clone());
            guard.disarm();
        }
        assert!(path.exists(), "a disarmed guard must leave the file alone");
        let _ = std::fs::remove_file(&path);
    }

    /// A symlink planted at the fixture path must never redirect a write.
    ///
    /// This is the mechanism behind "never a raw block device, ever, by any
    /// flag". Two things enforce it together: the stale-fixture removal unlinks
    /// the *symlink itself* rather than its target, and `create_new` then
    /// refuses to proceed if anything - including a freshly re-planted link -
    /// reappeared at the path. Either a brand-new regular file is created, or
    /// nothing is.
    #[test]
    fn a_symlink_at_the_fixture_path_never_redirects_the_write() {
        let scratch = Scratch::new("symlink");
        std::fs::create_dir_all(&scratch.0).expect("scratch");
        let decoy = scratch.0.join("decoy-target");
        std::fs::write(&decoy, b"must not be overwritten").expect("decoy");
        std::os::unix::fs::symlink(&decoy, scratch.0.join(FIXTURE_NAME)).expect("symlink");

        {
            let fixture = Fixture::create(&scratch.0, 1 << 20).expect("fixture");
            assert!(
                fixture
                    .file
                    .metadata()
                    .expect("metadata")
                    .file_type()
                    .is_file(),
                "the measured handle must be a regular file"
            );
        }

        assert_eq!(
            std::fs::read(&decoy).expect("decoy still readable"),
            b"must not be overwritten",
            "the symlink target must be untouched: the link was unlinked, never followed"
        );
        assert_eq!(
            std::fs::metadata(&decoy).expect("decoy metadata").len(),
            "must not be overwritten".len() as u64
        );
    }

    /// The reopen between filling and measuring is guarded by `O_NOFOLLOW`.
    ///
    /// The fixture is created, filled, closed and reopened. If anything
    /// replaced it with a symlink in that window, following the link would put
    /// direct writes somewhere the module never chose - so the reopen refuses
    /// links outright.
    #[test]
    fn reopening_refuses_to_follow_a_symlink() {
        let scratch = Scratch::new("nofollow");
        std::fs::create_dir_all(&scratch.0).expect("scratch");
        let target = scratch.0.join("real-file");
        std::fs::write(&target, vec![0u8; ALIGN]).expect("target");
        let link = scratch.0.join("link-to-target");
        std::os::unix::fs::symlink(&target, &link).expect("symlink");

        for direct in [true, false] {
            let error = Fixture::open_measuring(&link, direct)
                .expect_err("O_NOFOLLOW must refuse a symlink");
            assert_eq!(
                error.raw_os_error(),
                Some(rustix::io::Errno::LOOP.raw_os_error()),
                "expected ELOOP from O_NOFOLLOW, got {error}"
            );
        }
        // The same open against the real path is fine, direct I/O permitting.
        assert!(Fixture::open_measuring(&target, false).is_ok());
    }

    #[test]
    fn a_module_with_no_scratch_directory_refuses_rather_than_choosing_one() {
        let module = StorageMixed::new();
        let params = ModuleParams::for_profile(Profile::Quick);
        assert!(params.scratch_dir.is_none());
        assert!(matches!(
            module.run(&params, &NullReporter::default()),
            Err(ModuleError::Precondition(_))
        ));
    }

    // --- sizing ------------------------------------------------------------

    #[test]
    fn unknown_free_space_is_treated_as_constrained_not_unlimited() {
        let unknown = Plan::for_free_space(None);
        assert_eq!(unknown.fixture_bytes, MIN_FIXTURE_BYTES);
        assert!(unknown.constrained);
        assert!(
            !unknown.fits_in(None),
            "a disk whose free space is unknown must never be written to"
        );
    }

    #[test]
    fn the_fixture_is_a_fraction_of_free_space_and_never_more_than_preferred() {
        let roomy = Plan::for_free_space(Some(1 << 40));
        assert_eq!(roomy.fixture_bytes, PREFERRED_FIXTURE_BYTES);
        assert!(!roomy.constrained);

        let tight = Plan::for_free_space(Some(4 << 30));
        assert!(
            tight.fixture_bytes as f64 <= (4u64 << 30) as f64 * FREE_SPACE_SHARE,
            "the fixture must stay inside its share of free space"
        );
        assert!(tight.constrained);

        // Every plan is a whole number of large blocks, so no offset the
        // workloads compute can run past the end of the file.
        for free in [None, Some(100 << 20), Some(4 << 30), Some(1u64 << 40)] {
            let plan = Plan::for_free_space(free);
            assert_eq!(plan.fixture_bytes % LARGE_BLOCK as u64, 0);
            assert!(plan.fixture_bytes >= LARGE_BLOCK as u64);
        }
    }

    #[test]
    fn preflight_estimates_cover_both_space_and_wear() {
        let module = StorageMixed::new();
        let scratch = Scratch::new("estimates");
        let params = fast_params(&scratch);
        assert!(module.estimated_duration_s(&params) > 0);
        // Wear must exceed the space bound: the fixture is written once to
        // create it and then rewritten by every write workload.
        assert!(
            module.estimated_write_volume_bytes(&params)
                > Plan::for_free_space(params.facts.free_scratch_bytes).fixture_bytes,
            "the wear estimate must account for more than just creating the fixture"
        );
    }

    // --- statistics --------------------------------------------------------

    #[test]
    fn percentiles_report_an_observation_not_an_interpolation() {
        let mut values: Vec<f64> = (1..=100).map(|v| v as f64).collect();
        assert_eq!(percentile(&mut values, 99.0), Some(99.0));
        assert_eq!(percentile(&mut values, 50.0), Some(50.0));
        assert_eq!(percentile(&mut values, 100.0), Some(100.0));
        assert_eq!(percentile(&mut [], 99.0), None);
        assert_eq!(percentile(&mut [7.0], 99.0), Some(7.0));
    }

    #[test]
    fn steady_state_detects_a_run_that_slowed_down() {
        // Opening fast, ending slow: an SLC cache running out.
        let burst = [1000.0, 1000.0, 990.0, 500.0, 300.0, 280.0];
        let ratio = steady_state_ratio(&burst, Direction::HigherIsBetter).expect("ratio");
        assert!(
            ratio < STEADY_STATE_FLOOR,
            "ratio {ratio} should trip the floor"
        );

        let steady = [500.0, 505.0, 498.0, 502.0, 499.0, 501.0];
        let ratio = steady_state_ratio(&steady, Direction::HigherIsBetter).expect("ratio");
        assert!((ratio - 1.0).abs() < 0.05, "steady run gave {ratio}");

        // Too few repetitions to say anything.
        assert!(steady_state_ratio(&[1.0, 2.0, 3.0], Direction::HigherIsBetter).is_none());

        // The `quick` profile measures five repetitions, and the signal has to
        // work there: it is the profile most people run. Regression - the floor
        // used to be six, so steady state was silently absent by default.
        let quick_burst = [1000.0, 900.0, 700.0, 400.0, 250.0];
        let ratio = steady_state_ratio(&quick_burst, Direction::HigherIsBetter)
            .expect("five reps must produce a ratio");
        assert!(
            ratio < STEADY_STATE_FLOOR,
            "five-rep ratio {ratio} should trip the floor"
        );
        assert!(
            steady_state_ratio(&[500.0, 500.0, 500.0, 500.0], Direction::HigherIsBetter).is_some()
        );
    }

    /// The one write-shaped workload whose numbers grow as the device gets
    /// worse. A direction-blind `last / first` reports its degradation as a
    /// 400% improvement and its improvement as a validation failure - both
    /// exactly backwards, and both silent.
    #[test]
    fn steady_state_is_direction_adjusted_for_fsync_latency() {
        // 0.4 ms becoming 1.6 ms is the SLC cache filling: the signal this
        // check exists to catch.
        let degrading = [0.4, 0.4, 0.42, 1.5, 1.6, 1.7];
        let ratio = steady_state_ratio(&degrading, Direction::LowerIsBetter).expect("ratio");
        assert!(
            ratio < STEADY_STATE_FLOOR,
            "fsync latency that quadrupled must read as retaining a quarter, got {ratio}"
        );

        // ...and a device that settles *faster* than it started has not
        // degraded, so it must not be reported as having failed validation.
        let improving = [1.6, 1.6, 1.5, 0.42, 0.4, 0.4];
        let ratio = steady_state_ratio(&improving, Direction::LowerIsBetter).expect("ratio");
        assert!(
            ratio > 1.0,
            "a device that got faster cannot be degraded for it, got {ratio}"
        );

        // The `writes()` set that gates the check really does include the
        // inverted metric, which is why the adjustment is not optional.
        assert!(Access::Fsync.writes());
        assert_eq!(
            WORKLOADS
                .iter()
                .find(|w| w.access == Access::Fsync)
                .map(|w| w.direction),
            Some(Direction::LowerIsBetter)
        );
    }

    #[test]
    fn aligned_buffers_really_are_aligned() {
        for len in [ALIGN, SMALL_BLOCK, LARGE_BLOCK] {
            let buffer = AlignedBuffer::new(len, 0);
            assert!(
                buffer.is_aligned(),
                "len {len} produced an unaligned buffer"
            );
            assert_eq!(buffer.as_slice().len(), len);
        }
    }

    // --- end to end --------------------------------------------------------

    /// The full path against a real filesystem.
    ///
    /// Whether this host offers `O_DIRECT` is not something the test can
    /// require - a container on overlayfs or tmpfs will not - so it asserts the
    /// module is *honest* about which mode it used rather than asserting a mode.
    #[test]
    fn a_full_run_produces_every_metric_and_discloses_its_io_mode() {
        let scratch = Scratch::new("full");
        let module = StorageMixed::new();
        let output = module
            .run(&fast_params(&scratch), &NullReporter::default())
            .expect("run");

        let keys: Vec<&str> = output.metrics.iter().map(|m| m.key.as_str()).collect();
        for expected in [
            "sequential_read.qd1",
            "sequential_write.qd1",
            "random_read_4k.qd1",
            "random_read_4k.qd16",
            "random_write_4k.qd1",
            "random_write_4k.qd16",
            "random_mixed_4k.qd16",
            "latency_fsync.mean",
        ] {
            assert!(keys.contains(&expected), "missing `{expected}` in {keys:?}");
        }

        for metric in &output.metrics {
            assert!(
                metric.value > 0.0 && metric.value.is_finite(),
                "{} produced {}",
                metric.key,
                metric.value
            );
            assert!(!metric.unit.is_empty());
            let expected = if metric.key.starts_with("latency_") {
                Direction::LowerIsBetter
            } else {
                Direction::HigherIsBetter
            };
            assert_eq!(metric.direction, expected, "{}", metric.key);
        }

        let plan = &output.context["plan"];
        let direct = plan["direct_io"].as_bool().expect("direct_io");
        assert!(plan["fixture_bytes"].as_u64().expect("bytes") >= MIN_FIXTURE_BYTES);
        if !direct {
            assert!(
                output
                    .warnings
                    .iter()
                    .any(|w| w.message.contains("direct I/O")),
                "a buffered fallback must be disclosed, not passed off as a device measurement"
            );
        }
        assert!(output.context.contains_key("calibration"));
        assert!(output.context.contains_key("steady_state_ratio"));

        // And the disk is left as it was found.
        assert!(!scratch.0.join(FIXTURE_NAME).exists());
    }

    #[test]
    fn cancellation_is_honoured_and_still_cleans_up() {
        let scratch = Scratch::new("cancel");
        let module = StorageMixed::new();
        let reporter = NullReporter::default();
        reporter.cancel();

        assert!(matches!(
            module.run(&fast_params(&scratch), &reporter),
            Err(ModuleError::Cancelled)
        ));
        assert!(
            !scratch.0.join(FIXTURE_NAME).exists(),
            "a cancelled run must not leave its fixture behind"
        );
    }
}
