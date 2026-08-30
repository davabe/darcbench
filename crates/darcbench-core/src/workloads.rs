//! The individual CPU workloads.
//!
//! # Design rules every workload here follows
//!
//! * **Deterministic input.** All corpora come from a fixed-seed SplitMix64
//!   generator, so the same DARCBench version measures the same bytes on every
//!   machine. Random inputs would add run-to-run variance that has nothing to
//!   do with the hardware.
//! * **Work is reported, not time.** Each workload returns the number of *work
//!   units* it completed (bytes hashed, elements sorted, floating point
//!   operations). The caller divides by elapsed time. This is what lets the
//!   harness calibrate iteration counts per machine while keeping results
//!   comparable.
//! * **Nothing may be optimised away.** Every result is passed through
//!   `std::hint::black_box`. Without it, LLVM is entitled to delete an entire
//!   workload whose output is unused, and the "benchmark" would measure an
//!   empty loop.
//! * **No allocation inside the timed region** where it can be avoided, so the
//!   allocator is not the thing under test.

use std::hint::black_box;

use sha2::{Digest, Sha256};

/// SplitMix64. Chosen because it is a handful of instructions, has no
/// dependency, and produces the identical stream on every platform and
/// architecture - which is exactly what a reproducible corpus needs.
#[derive(Debug, Clone, Copy)]
pub struct SplitMix64(u64);

impl SplitMix64 {
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

/// Fixed seed for every DARCBench corpus in workload version 1.
///
/// Changing this value changes every corpus and therefore invalidates
/// comparability with prior results, so it may only change alongside a major
/// workload version bump.
pub const CORPUS_SEED: u64 = 0x0DA2_C8E0_C4A1_5EED;

/// A workload: fixed setup, then a repeatable timed body.
pub trait Workload: Send + Sync {
    /// Stable metric key suffix, e.g. `crypto_sha256`.
    fn key(&self) -> &'static str;
    fn label(&self) -> &'static str;
    /// Unit of `work_units / second`, e.g. `MiB/s`.
    fn unit(&self) -> &'static str;
    /// Divisor applied to work units before dividing by seconds, so the
    /// reported number is in the workload's declared unit.
    fn unit_scale(&self) -> f64;
    /// Work units completed by a single iteration.
    fn units_per_iteration(&self) -> f64;
    /// Runs `iterations` iterations. Returns a value derived from the work so
    /// the optimiser cannot elide it.
    fn execute(&self, iterations: u64) -> u64;
}

// --------------------------------------------------------------------------
// crypto_sha256
// --------------------------------------------------------------------------

/// SHA-256 over a fixed buffer. Stands in for TLS handshakes, content hashing,
/// integrity checks and password-adjacent work - all of which a web server
/// does constantly.
#[derive(Debug)]
pub struct CryptoSha256 {
    buffer: Vec<u8>,
}

impl CryptoSha256 {
    pub const BUFFER_BYTES: usize = 1 << 20; // 1 MiB

    pub fn new() -> Self {
        let mut rng = SplitMix64::new(CORPUS_SEED);
        let mut buffer = vec![0u8; Self::BUFFER_BYTES];
        for chunk in buffer.chunks_mut(8) {
            let bytes = rng.next_u64().to_le_bytes();
            chunk.copy_from_slice(&bytes[..chunk.len()]);
        }
        Self { buffer }
    }
}

impl Default for CryptoSha256 {
    fn default() -> Self {
        Self::new()
    }
}

impl Workload for CryptoSha256 {
    fn key(&self) -> &'static str {
        "crypto_sha256"
    }
    fn label(&self) -> &'static str {
        "SHA-256 hashing"
    }
    fn unit(&self) -> &'static str {
        "MiB/s"
    }
    fn unit_scale(&self) -> f64 {
        1024.0 * 1024.0
    }
    fn units_per_iteration(&self) -> f64 {
        Self::BUFFER_BYTES as f64
    }

    fn execute(&self, iterations: u64) -> u64 {
        let mut acc = 0u64;
        for _ in 0..iterations {
            let digest = Sha256::digest(black_box(&self.buffer));
            acc = acc.wrapping_add(u64::from_le_bytes([
                digest[0], digest[1], digest[2], digest[3], digest[4], digest[5], digest[6],
                digest[7],
            ]));
        }
        black_box(acc)
    }
}

// --------------------------------------------------------------------------
// compress_deflate
// --------------------------------------------------------------------------

/// DEFLATE compression of a semi-compressible corpus. This is the single most
/// common CPU cost in web serving after TLS: every `Content-Encoding: gzip`
/// response pays it.
#[derive(Debug)]
pub struct CompressDeflate {
    corpus: Vec<u8>,
}

impl CompressDeflate {
    pub const CORPUS_BYTES: usize = 512 * 1024;
    /// Level 6 is zlib's default and what nginx/Apache ship with, so it is what
    /// a real server actually spends its cycles on.
    pub const LEVEL: u32 = 6;

    pub fn new() -> Self {
        // A realistic corpus: repeated English-like tokens with structure, so
        // the compressor does real match-finding. Pure random data would make
        // this a memcpy benchmark; pure zeroes would make it a no-op.
        const WORDS: &[&str] = &[
            "the",
            "server",
            "response",
            "cache",
            "header",
            "content",
            "encoding",
            "gzip",
            "request",
            "database",
            "query",
            "index",
            "user",
            "session",
            "template",
            "render",
            "static",
            "asset",
            "bundle",
            "compression",
            "latency",
            "throughput",
            "benchmark",
        ];
        let mut rng = SplitMix64::new(CORPUS_SEED ^ 0x5151);
        let mut corpus = Vec::with_capacity(Self::CORPUS_BYTES + 64);
        while corpus.len() < Self::CORPUS_BYTES {
            let word = WORDS[(rng.next_u64() % WORDS.len() as u64) as usize];
            corpus.extend_from_slice(word.as_bytes());
            corpus.push(if rng.next_u64().is_multiple_of(12) {
                b'\n'
            } else {
                b' '
            });
        }
        corpus.truncate(Self::CORPUS_BYTES);
        Self { corpus }
    }
}

impl Default for CompressDeflate {
    fn default() -> Self {
        Self::new()
    }
}

impl Workload for CompressDeflate {
    fn key(&self) -> &'static str {
        "compress_deflate"
    }
    fn label(&self) -> &'static str {
        "DEFLATE compression (level 6)"
    }
    fn unit(&self) -> &'static str {
        "MiB/s"
    }
    fn unit_scale(&self) -> f64 {
        1024.0 * 1024.0
    }
    fn units_per_iteration(&self) -> f64 {
        Self::CORPUS_BYTES as f64
    }

    fn execute(&self, iterations: u64) -> u64 {
        use flate2::write::DeflateEncoder;
        use flate2::Compression;
        use std::io::Write;

        let mut acc = 0u64;
        for _ in 0..iterations {
            let mut encoder = DeflateEncoder::new(
                Vec::with_capacity(Self::CORPUS_BYTES / 2),
                Compression::new(Self::LEVEL),
            );
            // A write failure to an in-memory Vec is not a benchmark result;
            // the accumulator simply records that nothing was produced.
            if encoder.write_all(black_box(&self.corpus)).is_err() {
                continue;
            }
            match encoder.finish() {
                Ok(out) => acc = acc.wrapping_add(out.len() as u64),
                Err(_) => continue,
            }
        }
        black_box(acc)
    }
}

// --------------------------------------------------------------------------
// json_roundtrip
// --------------------------------------------------------------------------

/// Serialise then parse a realistic API document. Every JSON API, every
/// server-rendered page fetching structured data, and every log pipeline pays
/// this cost.
#[derive(Debug)]
pub struct JsonRoundtrip {
    document: serde_json::Value,
    serialized: String,
}

impl JsonRoundtrip {
    pub const RECORDS: usize = 64;

    pub fn new() -> Self {
        let mut rng = SplitMix64::new(CORPUS_SEED ^ 0x7777);
        let records: Vec<serde_json::Value> = (0..Self::RECORDS)
            .map(|i| {
                serde_json::json!({
                    "id": i,
                    "sku": format!("SKU-{:08X}", rng.next_u64() & 0xFFFF_FFFF),
                    "title": "Deterministic product record for benchmark corpus",
                    "price_cents": rng.next_u64() % 100_000,
                    "in_stock": rng.next_u64().is_multiple_of(2),
                    "tags": ["hosting", "server", "benchmark"],
                    "attributes": {
                        "weight_g": rng.next_u64() % 5_000,
                        "rating": (rng.next_u64() % 500) as f64 / 100.0,
                        "warehouse": format!("eu-{}", rng.next_u64() % 8),
                    }
                })
            })
            .collect();
        let document = serde_json::json!({ "records": records, "page": 1, "total": Self::RECORDS });
        let serialized = serde_json::to_string(&document).unwrap_or_default();
        Self {
            document,
            serialized,
        }
    }
}

impl Default for JsonRoundtrip {
    fn default() -> Self {
        Self::new()
    }
}

impl Workload for JsonRoundtrip {
    fn key(&self) -> &'static str {
        "json_roundtrip"
    }
    fn label(&self) -> &'static str {
        "JSON serialise + parse"
    }
    fn unit(&self) -> &'static str {
        "ops/s"
    }
    fn unit_scale(&self) -> f64 {
        1.0
    }
    fn units_per_iteration(&self) -> f64 {
        1.0
    }

    fn execute(&self, iterations: u64) -> u64 {
        let mut acc = 0u64;
        for _ in 0..iterations {
            let text = serde_json::to_string(black_box(&self.document)).unwrap_or_default();
            acc = acc.wrapping_add(text.len() as u64);
            match serde_json::from_str::<serde_json::Value>(black_box(&self.serialized)) {
                Ok(value) => {
                    acc = acc.wrapping_add(value["total"].as_u64().unwrap_or(0));
                }
                Err(_) => continue,
            }
        }
        black_box(acc)
    }
}

// --------------------------------------------------------------------------
// integer_sort
// --------------------------------------------------------------------------

/// Sorting a large integer array: branchy, cache-hostile integer work with a
/// memory access pattern that punishes small caches. Represents ORDER BY,
/// index maintenance and log processing.
#[derive(Debug)]
pub struct IntegerSort {
    master: Vec<u64>,
}

impl IntegerSort {
    pub const ELEMENTS: usize = 1 << 18; // 262144 elements = 2 MiB

    pub fn new() -> Self {
        let mut rng = SplitMix64::new(CORPUS_SEED ^ 0x1234);
        Self {
            master: (0..Self::ELEMENTS).map(|_| rng.next_u64()).collect(),
        }
    }
}

impl Default for IntegerSort {
    fn default() -> Self {
        Self::new()
    }
}

impl Workload for IntegerSort {
    fn key(&self) -> &'static str {
        "integer_sort"
    }
    fn label(&self) -> &'static str {
        "Integer sort"
    }
    fn unit(&self) -> &'static str {
        "Melem/s"
    }
    fn unit_scale(&self) -> f64 {
        1_000_000.0
    }
    fn units_per_iteration(&self) -> f64 {
        Self::ELEMENTS as f64
    }

    fn execute(&self, iterations: u64) -> u64 {
        // Allocated once, outside the per-iteration work: sorting requires
        // unsorted input, so the refill is genuinely part of the workload, but
        // the allocation is not.
        let mut scratch = vec![0u64; Self::ELEMENTS];
        let mut acc = 0u64;
        for _ in 0..iterations {
            scratch.copy_from_slice(black_box(&self.master));
            scratch.sort_unstable();
            acc = acc.wrapping_add(scratch[Self::ELEMENTS / 2]);
        }
        black_box(acc)
    }
}

// --------------------------------------------------------------------------
// float_matmul
// --------------------------------------------------------------------------

/// Dense double-precision matrix multiply. The only pure floating-point
/// workload in the set; included because FP throughput varies enormously
/// between server CPU generations and is invisible to integer-only benchmarks.
#[derive(Debug)]
pub struct FloatMatmul {
    a: Vec<f64>,
    b: Vec<f64>,
}

impl FloatMatmul {
    pub const N: usize = 128;

    pub fn new() -> Self {
        let mut rng = SplitMix64::new(CORPUS_SEED ^ 0xABCD);
        let mut gen = || {
            // Values in [0.5, 1.5): away from zero and from denormals, which
            // would otherwise turn this into a microcode benchmark.
            0.5 + (rng.next_u64() >> 11) as f64 / (1u64 << 53) as f64
        };
        Self {
            a: (0..Self::N * Self::N).map(|_| gen()).collect(),
            b: (0..Self::N * Self::N).map(|_| gen()).collect(),
        }
    }
}

impl Default for FloatMatmul {
    fn default() -> Self {
        Self::new()
    }
}

impl Workload for FloatMatmul {
    fn key(&self) -> &'static str {
        "float_matmul"
    }
    fn label(&self) -> &'static str {
        "Double-precision matrix multiply"
    }
    fn unit(&self) -> &'static str {
        "MFLOP/s"
    }
    fn unit_scale(&self) -> f64 {
        1_000_000.0
    }
    fn units_per_iteration(&self) -> f64 {
        // One multiply and one add per inner-loop step.
        2.0 * (Self::N * Self::N * Self::N) as f64
    }

    fn execute(&self, iterations: u64) -> u64 {
        const N: usize = FloatMatmul::N;
        let mut c = vec![0.0f64; N * N];
        let mut acc = 0u64;
        for _ in 0..iterations {
            c.fill(0.0);
            // i-k-j ordering: `b` is walked contiguously in the inner loop,
            // which is the layout a real BLAS-free implementation would use.
            for i in 0..N {
                for k in 0..N {
                    let aik = self.a[i * N + k];
                    let brow = &self.b[k * N..k * N + N];
                    let crow = &mut c[i * N..i * N + N];
                    for j in 0..N {
                        crow[j] += aik * brow[j];
                    }
                }
            }
            acc = acc.wrapping_add(black_box(c[N / 2 * N + N / 2]).to_bits());
        }
        black_box(acc)
    }
}

/// The workload set of `cpu.mixed` version 1.
pub fn cpu_workloads() -> Vec<Box<dyn Workload>> {
    vec![
        Box::new(CryptoSha256::new()),
        Box::new(CompressDeflate::new()),
        Box::new(JsonRoundtrip::new()),
        Box::new(IntegerSort::new()),
        Box::new(FloatMatmul::new()),
    ]
}

#[cfg(test)]
// In tests, `unwrap`/`expect` panicking *is* the failure signal.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic_in_result_fn)]
mod tests {
    use super::*;

    #[test]
    fn splitmix_is_deterministic_and_platform_stable() {
        let mut a = SplitMix64::new(42);
        let mut b = SplitMix64::new(42);
        let first: Vec<u64> = (0..8).map(|_| a.next_u64()).collect();
        let second: Vec<u64> = (0..8).map(|_| b.next_u64()).collect();
        assert_eq!(first, second);
        // Reference vector for seed 0, so a refactor that changes the
        // generator - and therefore every corpus - fails loudly.
        let mut zero = SplitMix64::new(0);
        assert_eq!(zero.next_u64(), 16294208416658607535);
        assert_eq!(zero.next_u64(), 7960286522194355700);
    }

    #[test]
    fn corpora_are_identical_across_constructions() {
        assert_eq!(CryptoSha256::new().buffer, CryptoSha256::new().buffer);
        assert_eq!(CompressDeflate::new().corpus, CompressDeflate::new().corpus);
        assert_eq!(IntegerSort::new().master, IntegerSort::new().master);
        assert_eq!(FloatMatmul::new().a, FloatMatmul::new().a);
        assert_eq!(
            JsonRoundtrip::new().serialized,
            JsonRoundtrip::new().serialized
        );
    }

    #[test]
    fn compression_corpus_is_actually_compressible() {
        // If the corpus were incompressible, this would be a memcpy benchmark.
        let w = CompressDeflate::new();
        use flate2::write::DeflateEncoder;
        use flate2::Compression;
        use std::io::Write;
        let mut encoder = DeflateEncoder::new(Vec::new(), Compression::new(CompressDeflate::LEVEL));
        encoder.write_all(&w.corpus).expect("in-memory write");
        let compressed = encoder.finish().expect("finish");
        let ratio = compressed.len() as f64 / w.corpus.len() as f64;
        assert!(
            (0.15..0.75).contains(&ratio),
            "corpus compresses to {ratio:.2} of its size; the workload needs real match-finding"
        );
    }

    #[test]
    fn every_workload_does_measurable_work() {
        for workload in cpu_workloads() {
            let start = std::time::Instant::now();
            let result = workload.execute(1);
            let elapsed = start.elapsed();
            assert!(
                elapsed > std::time::Duration::from_micros(10),
                "{} finished in {elapsed:?}, which suggests it was optimised away",
                workload.key()
            );
            // The accumulator is data-dependent; a constant 0 would hint at
            // dead-code elimination.
            let _ = result;
            assert!(workload.units_per_iteration() > 0.0);
            assert!(workload.unit_scale() > 0.0);
        }
    }

    #[test]
    fn workload_keys_are_unique_and_snake_case() {
        let keys: Vec<&str> = cpu_workloads().iter().map(|w| w.key()).collect();
        let unique: std::collections::BTreeSet<&&str> = keys.iter().collect();
        assert_eq!(keys.len(), unique.len(), "duplicate workload key");
        for key in keys {
            // Metric keys become `<workload>.<shape>` and are looked up in the
            // scoring reference table, so they must stay in the restricted
            // `[a-z][a-z0-9_]*` alphabet.
            let mut chars = key.chars();
            assert!(
                chars.next().is_some_and(|c| c.is_ascii_lowercase()),
                "`{key}` must start with a lowercase letter"
            );
            assert!(
                chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
                "`{key}` must match [a-z][a-z0-9_]*"
            );
        }
    }

    /// The work must actually happen: a compiler that hoisted the loop would
    /// make every throughput figure in the suite a fiction.
    ///
    /// # Why the margin is enormous
    ///
    /// This started as 8 iterations against 4 with a 1.3x threshold, and it
    /// went red on a loaded CI runner - twice - because a single descheduled
    /// sample on the *smaller* side is enough to flatten a ratio that narrow.
    /// Best-of-five helped and did not fix it: the whole suite now spawns PHP
    /// and Node processes, so there is no quiet moment to take a best sample
    /// in.
    ///
    /// A sixteen-fold difference in work against a four-fold threshold has so
    /// much margin that no scheduling accident can close it, and it detects a
    /// hoisted loop exactly as well - better, in fact, since a hoisted loop
    /// gives a ratio near 1.0 whatever the multiplier. Widening the gap was
    /// always the right answer; tightening the sampling was treating the
    /// symptom.
    #[test]
    fn throughput_scales_with_iterations() {
        let workload = IntegerSort::new();
        let time_it = |n: u64| {
            let start = std::time::Instant::now();
            workload.execute(n);
            start.elapsed().as_secs_f64()
        };
        time_it(4); // warm up

        let small = time_it(4);
        let large = time_it(64);
        assert!(
            large > small * 4.0,
            "64 iterations ({large:.4}s) vs 4 ({small:.4}s) is suspiciously flat; sixteen times \
             the work must take more than four times the time unless the loop was hoisted"
        );
    }

    #[test]
    fn matmul_reports_correct_flop_count() {
        let w = FloatMatmul::new();
        assert_eq!(w.units_per_iteration(), 2.0 * 128.0 * 128.0 * 128.0);
    }
}
