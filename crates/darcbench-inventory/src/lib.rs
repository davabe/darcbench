//! Read-only system inventory and telemetry.
//!
//! # Guarantees
//!
//! * **Nothing in this crate writes to the system.** No configuration is
//!   touched, no service restarted, no file created outside a caller-supplied
//!   path. Discovery must be safe to run on a live production host.
//! * **No shell.** Every fact is read from `/proc`, `/sys` or a small set of
//!   well-known files. There is no string interpolation into a command line
//!   anywhere in this crate, which removes an entire injection class.
//! * **Privacy by construction.** Values that identify a machine or its owner
//!   (hostname, serial numbers, MAC and IP addresses, cloud instance ids) are
//!   captured into fields marked [`Sensitive`] and are redacted by default on
//!   any code path that leaves the machine. See `docs/PRIVACY.md`.

pub mod cpu;
pub mod memory;
pub mod network;
pub mod platform;
pub mod redact;
pub mod software;
pub mod storage;
pub mod telemetry;

use serde::{Deserialize, Serialize};

pub use redact::{RedactionPolicy, Sensitive};
pub use telemetry::{TelemetrySampler, TelemetrySnapshot};

/// A complete point-in-time description of the machine under test.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Inventory {
    pub schema: String,
    pub collected_at: chrono::DateTime<chrono::Utc>,
    pub platform: platform::Platform,
    pub cpu: cpu::CpuInfo,
    pub memory: memory::MemoryInfo,
    pub storage: storage::StorageInfo,
    pub network: network::NetworkInfo,
    pub software: software::SoftwareInfo,
    /// Facts the collector could not determine, with the reason. Reported
    /// rather than defaulted, because a silently-zero core count would corrupt
    /// scoring.
    pub gaps: Vec<Gap>,
}

pub const INVENTORY_SCHEMA: &str = "darcbench.inventory/1";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Gap {
    pub field: String,
    pub reason: String,
}

impl Inventory {
    /// Collects the full inventory. Never fails as a whole: individual
    /// collectors degrade into [`Inventory::gaps`] entries.
    pub fn collect() -> Self {
        let mut gaps = Vec::new();
        let platform = platform::Platform::collect(&mut gaps);
        let cpu = cpu::CpuInfo::collect(&mut gaps);
        let memory = memory::MemoryInfo::collect(&mut gaps);
        let storage = storage::StorageInfo::collect(&mut gaps);
        let network = network::NetworkInfo::collect(&mut gaps);
        let software = software::SoftwareInfo::collect(&mut gaps);

        Self {
            schema: INVENTORY_SCHEMA.to_string(),
            collected_at: chrono::Utc::now(),
            platform,
            cpu,
            memory,
            storage,
            network,
            software,
            gaps,
        }
    }

    /// Stable digest over the *performance-relevant* subset of the inventory.
    ///
    /// Used to detect a machine changing materially mid-run (a hot-added vCPU,
    /// a live migration to different hardware). Deliberately excludes volatile
    /// values such as free memory, and excludes identifying values so the
    /// digest itself is safe to publish.
    pub fn performance_digest(&self) -> String {
        use sha2::{Digest, Sha256};
        let material = serde_json::json!({
            "cpu_model": self.cpu.model,
            "physical_cores": self.cpu.physical_cores,
            "logical_cpus": self.cpu.logical_cpus,
            "sockets": self.cpu.sockets,
            "numa_nodes": self.cpu.numa_nodes,
            "flags": self.cpu.instruction_sets,
            "mem_total": self.memory.total_bytes,
            "kernel": self.platform.kernel_release,
            "virtualization": self.platform.virtualization,
            "scope": self.platform.scope,
            "cgroup_cpu_limit": self.platform.cgroup_cpu_limit,
            "cgroup_mem_limit_bytes": self.platform.cgroup_mem_limit_bytes,
        });
        let bytes = serde_json::to_vec(&material).unwrap_or_default();
        format!("sha256:{}", hex::encode(Sha256::digest(bytes)))
    }
}

/// Reads a file, returning `None` for any error. Inventory collection must
/// never abort because one `/proc` entry is unreadable in a container.
pub(crate) fn read_file(path: &str) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

/// Reads a file into a caller-owned buffer, returning `false` on any error.
///
/// Telemetry re-reads the same handful of `/proc` files once a second for the
/// entire length of a run. Reusing one buffer keeps that from allocating a
/// fresh `String` per file per second - the sampler is part of the system under
/// test, so its allocation traffic is charged to the measurement.
pub(crate) fn read_into(path: &str, buffer: &mut String) -> bool {
    use std::io::Read;
    buffer.clear();
    std::fs::File::open(path)
        .and_then(|mut file| file.read_to_string(buffer))
        .is_ok()
}

/// Reads at most `limit` bytes from the start of a file.
///
/// `/proc/cpuinfo` is rendered lazily by the kernel, one CPU block at a time,
/// so the cost of reading it to EOF scales with the core count. Where only the
/// first block is needed, bounding the read makes it constant instead: on a
/// 64-vCPU host the full file is ~90 KiB of kernel-side formatting for a value
/// that appears in the first 500 bytes.
pub(crate) fn read_file_prefix(path: &str, limit: usize) -> Option<String> {
    use std::io::Read;
    let mut buffer = vec![0u8; limit];
    let mut file = std::fs::File::open(path).ok()?;
    let read = file.read(&mut buffer).ok()?;
    buffer.truncate(read);
    String::from_utf8(buffer).ok()
}

/// Reads a file and parses its trimmed contents.
pub(crate) fn read_parse<T: std::str::FromStr>(path: &str) -> Option<T> {
    read_file(path)?.trim().parse().ok()
}

/// Parses a `key: value` style `/proc` file into pairs.
pub(crate) fn parse_kv(contents: &str, sep: char) -> Vec<(String, String)> {
    contents
        .lines()
        .filter_map(|line| {
            let (k, v) = line.split_once(sep)?;
            Some((k.trim().to_string(), v.trim().to_string()))
        })
        .collect()
}

#[cfg(test)]
// In tests, `unwrap`/`expect` panicking *is* the failure signal.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic_in_result_fn)]
mod tests {
    use super::*;

    #[test]
    fn parse_kv_handles_proc_style_files() {
        let pairs = parse_kv(
            "MemTotal:       16266152 kB\nMemFree:  100 kB\ngarbage\n",
            ':',
        );
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[0], ("MemTotal".into(), "16266152 kB".into()));
    }

    #[test]
    fn collect_never_panics_and_reports_gaps() {
        let inv = Inventory::collect();
        assert_eq!(inv.schema, INVENTORY_SCHEMA);
        // On any Linux host the logical CPU count must be discoverable.
        assert!(inv.cpu.logical_cpus >= 1, "gaps: {:?}", inv.gaps);
        assert!(inv.memory.total_bytes > 0, "gaps: {:?}", inv.gaps);
    }

    #[test]
    fn performance_digest_is_stable_and_prefixed() {
        let inv = Inventory::collect();
        let a = inv.performance_digest();
        let b = inv.performance_digest();
        assert_eq!(a, b);
        assert!(a.starts_with("sha256:"));
    }

    #[test]
    fn performance_digest_ignores_volatile_and_identifying_values() {
        let mut inv = Inventory::collect();
        let before = inv.performance_digest();
        inv.memory.available_bytes = inv.memory.available_bytes.saturating_sub(1_000_000);
        inv.platform.hostname = Sensitive::new("a-totally-different-host".to_string());
        assert_eq!(before, inv.performance_digest());

        // A real hardware change must move it.
        inv.cpu.logical_cpus += 1;
        assert_ne!(before, inv.performance_digest());
    }
}
