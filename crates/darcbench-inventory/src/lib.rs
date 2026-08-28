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
    ///
    /// It must cover every subsystem that is *scored*, and for a long time it
    /// covered three of the five. Storage and network were absent, and that was
    /// found the way such things are: one machine's NIC renegotiated from 1000
    /// to 100 Mbit/s between runs, `download.single` fell from 305 to 76
    /// Mbit/s, a fourfold move in a scored category, and the digest came out
    /// byte-identical. Two runs that the digest called the same environment
    /// differed by a factor of four in Network.
    ///
    /// What goes in is chosen to be stable across two runs on an unchanged
    /// machine, which is the only property that makes the digest usable:
    ///
    /// * The link speed of the *primary* interface only. Secondary interfaces
    ///   flap, containers come and go with veth pairs, and none of that is what
    ///   `network.transfer` measures over.
    /// * Block devices by model, size, transport and rotational flag, sorted by
    ///   name. Not free space, which is volatile by definition, and not the
    ///   scheduler, which an administrator may retune without the hardware
    ///   changing - though that is a judgement call rather than an obvious one.
    /// * The root filesystem type, because ext4 and ZFS are not the same
    ///   machine as far as `storage.mixed` is concerned.
    ///
    /// Device models are already in the visible column of `docs/PRIVACY.md`, so
    /// including them keeps the digest publishable.
    pub fn performance_digest(&self) -> String {
        use sha2::{Digest, Sha256};

        let link_speed_mbps = self
            .network
            .primary_interface
            .as_ref()
            .and_then(|name| self.network.interfaces.iter().find(|i| &i.name == name))
            .and_then(|i| i.speed_mbps);

        let mut devices: Vec<_> = self
            .storage
            .devices
            .iter()
            .map(|d| {
                serde_json::json!({
                    "name": d.name,
                    "model": d.model,
                    "size_bytes": d.size_bytes,
                    "rotational": d.rotational,
                    "transport": d.transport,
                })
            })
            .collect();
        // Enumeration order is not guaranteed to be stable, and an unstable
        // digest is worse than an incomplete one.
        devices.sort_by(|a, b| a["name"].to_string().cmp(&b["name"].to_string()));

        let root_fstype = self
            .storage
            .mounts
            .iter()
            .find(|m| m.target == "/")
            .map(|m| m.fstype.clone());

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
            "link_speed_mbps": link_speed_mbps,
            "storage_devices": devices,
            "root_fstype": root_fstype,
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

    /// The digest must cover every subsystem that is scored.
    ///
    /// Found in the field: a machine's NIC renegotiated from 1000 to 100
    /// Mbit/s between two runs, `download.single` fell from 305 to 76 Mbit/s -
    /// a fourfold move in a scored category - and the digest was byte-identical
    /// across the two. The digest existed precisely to say "this is no longer
    /// the same environment" and said the opposite.
    #[test]
    fn a_change_to_any_scored_subsystem_moves_the_digest() {
        use crate::network::Interface;
        use crate::storage::BlockDevice;

        let mut inv = Inventory::collect();
        // A known interface and device, so the assertions do not depend on what
        // the host running the tests happens to have.
        inv.network.primary_interface = Some("eth0".into());
        inv.network.interfaces = vec![Interface {
            name: "eth0".into(),
            speed_mbps: Some(1000),
            mtu: Some(1500),
            operstate: "up".into(),
            virtual_device: false,
            mac: Sensitive::new("00:00:00:00:00:00".into()),
        }];
        inv.storage.devices = vec![BlockDevice {
            name: "nvme0n1".into(),
            model: Some("SAMSUNG MZQLB960".into()),
            size_bytes: 960_197_124_096,
            rotational: Some(false),
            transport: "nvme".into(),
            scheduler: Some("none".into()),
            queue_depth: Some(1023),
            physical_block_size: Some(512),
        }];
        let before = inv.performance_digest();

        // The exact change that was missed.
        inv.network.interfaces[0].speed_mbps = Some(100);
        assert_ne!(
            before,
            inv.performance_digest(),
            "a link renegotiated to a tenth of its speed is a different environment"
        );
        inv.network.interfaces[0].speed_mbps = Some(1000);
        assert_eq!(before, inv.performance_digest(), "and back again");

        // Storage is the same class of omission.
        inv.storage.devices[0].model = Some("A DIFFERENT DISK".into());
        assert_ne!(
            before,
            inv.performance_digest(),
            "swapping the disk is a different environment"
        );
        inv.storage.devices[0].model = Some("SAMSUNG MZQLB960".into());
        assert_eq!(before, inv.performance_digest());

        // Things that must *not* move it, because they change without the
        // machine changing.
        inv.network.interfaces[0].operstate = "down".into();
        inv.network.interfaces[0].mtu = Some(9000);
        assert_eq!(
            before,
            inv.performance_digest(),
            "operstate and MTU are not what the storage or network modules measure over"
        );
    }
}
