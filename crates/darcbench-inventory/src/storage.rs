//! Block devices, filesystems and the storage context a result depends on.
//!
//! Two identical NVMe drives produce very different fio numbers depending on
//! filesystem, mount options, RAID layer and whether the volume is
//! network-attached. Capturing that context is what makes a storage score
//! comparable at all.

use serde::{Deserialize, Serialize};

use crate::{read_file, read_parse, Gap};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageInfo {
    pub devices: Vec<BlockDevice>,
    pub mounts: Vec<Mount>,
    /// True when any software RAID, LVM, ZFS or network-storage indicator was
    /// found. Such a stack must be disclosed alongside any storage score.
    pub complex_stack: bool,
    pub stack_indicators: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockDevice {
    pub name: String,
    pub model: Option<String>,
    pub size_bytes: u64,
    /// `false` for SSD/NVMe, `true` for spinning media.
    pub rotational: Option<bool>,
    /// `nvme`, `sata`/`scsi`, `virtio`, `mmc`, `md`, `dm`, or `unknown`.
    pub transport: String,
    pub scheduler: Option<String>,
    pub queue_depth: Option<u32>,
    pub physical_block_size: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mount {
    pub source: String,
    pub target: String,
    pub fstype: String,
    pub options: Vec<String>,
    /// True for filesystems whose performance is not the local disk's:
    /// nfs, cifs, ceph, glusterfs, and friends.
    pub network_backed: bool,
}

const NETWORK_FILESYSTEMS: &[&str] = &[
    "nfs",
    "nfs4",
    "cifs",
    "smb3",
    "ceph",
    "glusterfs",
    "lustre",
    "9p",
    "sshfs",
    "fuse.sshfs",
];

/// Pseudo-filesystems that carry no storage-performance meaning.
const PSEUDO_FILESYSTEMS: &[&str] = &[
    "proc",
    "sysfs",
    "devpts",
    "devtmpfs",
    "cgroup",
    "cgroup2",
    "securityfs",
    "pstore",
    "bpf",
    "tracefs",
    "debugfs",
    "configfs",
    "fusectl",
    "mqueue",
    "hugetlbfs",
    "binfmt_misc",
    "autofs",
    "nsfs",
    "ramfs",
    "efivarfs",
    "selinuxfs",
];

impl StorageInfo {
    pub fn collect(gaps: &mut Vec<Gap>) -> Self {
        let devices = collect_devices();
        if devices.is_empty() {
            gaps.push(Gap {
                field: "storage.devices".into(),
                reason: "/sys/block empty or unreadable (common inside containers)".into(),
            });
        }
        let mounts = collect_mounts();

        let mut indicators = Vec::new();
        if read_file("/proc/mdstat").is_some_and(|s| s.lines().any(|l| l.starts_with("md"))) {
            indicators.push("software RAID (mdraid) active".into());
        }
        if devices.iter().any(|d| d.transport == "dm") {
            indicators.push("device-mapper / LVM present".into());
        }
        if mounts.iter().any(|m| m.fstype == "zfs") {
            indicators.push("ZFS filesystem present".into());
        }
        if mounts.iter().any(|m| m.fstype == "btrfs") {
            indicators.push("Btrfs filesystem present".into());
        }
        if mounts.iter().any(|m| m.network_backed) {
            indicators.push("network-attached filesystem present".into());
        }
        if devices.iter().any(|d| d.transport == "virtio") {
            indicators.push("virtio block device (virtualised storage path)".into());
        }

        Self {
            complex_stack: !indicators.is_empty(),
            stack_indicators: indicators,
            devices,
            mounts,
        }
    }

    /// Bytes available to an unprivileged writer on the filesystem containing
    /// `path`.
    ///
    /// Used by the storage safety guard, which refuses to start a write test
    /// that could fill a production disk. Uses `f_bavail` (space available to
    /// non-root) rather than `f_bfree`, so a run started as root cannot eat
    /// into the reserved-blocks margin that keeps a full filesystem
    /// recoverable.
    ///
    /// `None` means "could not determine", which callers MUST treat as unsafe
    /// rather than as unlimited.
    pub fn available_bytes_for(path: &std::path::Path) -> Option<u64> {
        let stat = rustix::fs::statvfs(path).ok()?;
        stat.f_bavail.checked_mul(stat.f_frsize)
    }

    /// Free space for `path`, falling back to its nearest existing ancestor.
    ///
    /// `statvfs` needs a path that exists, and the directory a run is about to
    /// write into frequently does not yet - the state directory is created on
    /// first use. Reporting `None` there is not merely unhelpful: every caller
    /// correctly treats unknown free space as unsafe, so a first run on a fresh
    /// install would be refused for want of a directory it was about to create.
    /// An ancestor is on the same filesystem, so its answer is the right one.
    pub fn available_bytes_for_or_ancestor(path: &std::path::Path) -> Option<u64> {
        let mut candidate = Some(path);
        while let Some(current) = candidate {
            if let Some(bytes) = Self::available_bytes_for(current) {
                return Some(bytes);
            }
            candidate = current.parent();
        }
        None
    }

    /// Finds the mount point that actually backs `path`, so a report can say
    /// *which* filesystem a storage number describes.
    pub fn mount_for(&self, path: &std::path::Path) -> Option<&Mount> {
        let target = path.canonicalize().ok()?;
        self.mounts
            .iter()
            .filter(|m| target.starts_with(&m.target))
            .max_by_key(|m| m.target.len())
    }
}

fn collect_devices() -> Vec<BlockDevice> {
    let Ok(entries) = std::fs::read_dir("/sys/block") else {
        return Vec::new();
    };
    let mut devices: Vec<BlockDevice> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();
            // Skip loop, ram and zram devices: they are not the machine's storage.
            if name.starts_with("loop") || name.starts_with("ram") || name.starts_with("zram") {
                return None;
            }
            let base = format!("/sys/block/{name}");
            // sysfs reports size in 512-byte sectors regardless of the device's
            // real block size.
            let sectors: u64 = read_parse(&format!("{base}/size")).unwrap_or(0);

            Some(BlockDevice {
                model: read_file(&format!("{base}/device/model"))
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty()),
                size_bytes: sectors.saturating_mul(512),
                rotational: read_parse::<u8>(&format!("{base}/queue/rotational")).map(|v| v == 1),
                transport: classify_transport(&name, &base),
                scheduler: read_file(&format!("{base}/queue/scheduler"))
                    .and_then(|s| active_scheduler(s.trim())),
                queue_depth: read_parse(&format!("{base}/queue/nr_requests")),
                physical_block_size: read_parse(&format!("{base}/queue/physical_block_size")),
                name,
            })
        })
        .collect();
    devices.sort_by(|a, b| a.name.cmp(&b.name));
    devices
}

fn classify_transport(name: &str, base: &str) -> String {
    if name.starts_with("nvme") {
        return "nvme".into();
    }
    if name.starts_with("vd") {
        return "virtio".into();
    }
    if name.starts_with("md") {
        return "md".into();
    }
    if name.starts_with("dm-") {
        return "dm".into();
    }
    if name.starts_with("mmcblk") {
        return "mmc".into();
    }
    if std::path::Path::new(&format!("{base}/device/vendor")).exists() {
        return "scsi".into();
    }
    "unknown".into()
}

/// `/sys/block/*/queue/scheduler` looks like `none [mq-deadline] kyber`.
fn active_scheduler(raw: &str) -> Option<String> {
    raw.split_whitespace()
        .find(|t| t.starts_with('[') && t.ends_with(']'))
        .map(|t| t.trim_matches(['[', ']']).to_string())
        .or_else(|| raw.split_whitespace().next().map(str::to_string))
}

fn collect_mounts() -> Vec<Mount> {
    let Some(raw) = read_file("/proc/self/mounts") else {
        return Vec::new();
    };
    raw.lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let source = fields.next()?.to_string();
            let target = fields.next()?.replace("\\040", " ");
            let fstype = fields.next()?.to_string();
            let options: Vec<String> = fields.next()?.split(',').map(str::to_string).collect();
            if PSEUDO_FILESYSTEMS.contains(&fstype.as_str()) || fstype.starts_with("cgroup") {
                return None;
            }
            let network_backed = NETWORK_FILESYSTEMS.contains(&fstype.as_str());
            Some(Mount {
                source,
                target,
                fstype,
                options,
                network_backed,
            })
        })
        .collect()
}

#[cfg(test)]
// In tests, `unwrap`/`expect` panicking *is* the failure signal.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic_in_result_fn)]
mod tests {
    use super::*;

    #[test]
    fn storage_collects_without_panicking() {
        let mut gaps = Vec::new();
        let s = StorageInfo::collect(&mut gaps);
        // Mounts should always include at least the root filesystem.
        assert!(
            s.mounts.iter().any(|m| m.target == "/"),
            "no root mount found"
        );
    }

    #[test]
    fn pseudo_filesystems_are_excluded() {
        let mounts = collect_mounts();
        assert!(!mounts.iter().any(|m| m.fstype == "proc"));
        assert!(!mounts.iter().any(|m| m.fstype == "sysfs"));
        assert!(!mounts.iter().any(|m| m.fstype.starts_with("cgroup")));
    }

    #[test]
    fn scheduler_parsing_picks_the_active_one() {
        assert_eq!(
            active_scheduler("none [mq-deadline] kyber").as_deref(),
            Some("mq-deadline")
        );
        assert_eq!(active_scheduler("[none]").as_deref(), Some("none"));
        assert_eq!(active_scheduler("none").as_deref(), Some("none"));
    }

    #[test]
    fn transport_classification() {
        assert_eq!(classify_transport("nvme0n1", "/nonexistent"), "nvme");
        assert_eq!(classify_transport("vda", "/nonexistent"), "virtio");
        assert_eq!(classify_transport("md0", "/nonexistent"), "md");
        assert_eq!(classify_transport("sda", "/nonexistent"), "unknown");
    }

    #[test]
    fn available_space_is_readable_and_bounded() {
        let free = StorageInfo::available_bytes_for(std::path::Path::new("/tmp"))
            .expect("statvfs on /tmp should succeed");
        assert!(free > 0, "no free space reported on /tmp");
    }

    #[test]
    fn available_space_is_none_for_a_missing_path() {
        assert!(StorageInfo::available_bytes_for(std::path::Path::new(
            "/definitely/not/a/real/path/darcbench"
        ))
        .is_none());
    }

    #[test]
    fn mount_for_resolves_the_longest_prefix() {
        let mut gaps = Vec::new();
        let s = StorageInfo::collect(&mut gaps);
        let m = s
            .mount_for(std::path::Path::new("/tmp"))
            .expect("a mount backs /tmp");
        assert!(std::path::Path::new("/tmp").starts_with(&m.target));
    }

    #[test]
    fn network_filesystems_are_flagged() {
        let m = Mount {
            source: "10.0.0.1:/export".into(),
            target: "/mnt/data".into(),
            fstype: "nfs4".into(),
            options: vec!["rw".into()],
            network_backed: NETWORK_FILESYSTEMS.contains(&"nfs4"),
        };
        assert!(m.network_backed);
    }
}
