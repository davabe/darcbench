//! OS, kernel, virtualization and execution-scope detection.

use serde::{Deserialize, Serialize};

use crate::{parse_kv, read_file, read_parse, Gap, Sensitive};

/// What the measurement actually describes.
///
/// This is the single most important honesty field in the whole inventory. A
/// benchmark run inside a 1-vCPU container on a 128-core host measures the
/// container, and reporting it as a machine score would be a lie. Scope is
/// carried into every report and every leaderboard entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    /// Physical machine, no hypervisor detected.
    BareMetal,
    /// Full virtual machine.
    VirtualMachine,
    /// Container or otherwise cgroup-constrained namespace.
    Container,
    /// Detection was inconclusive.
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Platform {
    pub os: String,
    pub distribution: Option<String>,
    pub distribution_version: Option<String>,
    pub kernel_release: Option<String>,
    pub architecture: String,
    pub scope: Scope,
    /// Hypervisor family when detectable: `kvm`, `vmware`, `microsoft`, `xen`,
    /// `oracle`, `qemu`, `lxc`, ...
    pub virtualization: Option<String>,
    /// How the virtualization verdict was reached, so it can be audited.
    pub virtualization_evidence: Vec<String>,
    pub container_runtime: Option<String>,
    /// Effective CPU limit from cgroup v2 `cpu.max`, in whole CPUs.
    pub cgroup_cpu_limit: Option<f64>,
    pub cgroup_mem_limit_bytes: Option<u64>,
    pub uptime_seconds: Option<u64>,
    pub load1: Option<f64>,
    pub load5: Option<f64>,
    pub load15: Option<f64>,
    /// True when the process runs as uid 0.
    pub running_as_root: bool,
    pub hostname: Sensitive<String>,
    /// DMI system vendor / product, useful for identifying cloud platforms.
    /// Serial numbers and UUIDs from DMI are deliberately never collected.
    pub dmi_vendor: Option<String>,
    pub dmi_product: Option<String>,
    /// Cloud platform inferred from DMI only. DARCBench never queries a cloud
    /// metadata endpoint: those responses carry credentials, and an SSRF-style
    /// read of `169.254.169.254` is not something a benchmark should perform.
    /// See `docs/THREAT-MODEL.md` (T-SSRF-METADATA).
    pub cloud_hint: Option<String>,
    pub security_modules: Vec<String>,
}

impl Platform {
    pub fn collect(gaps: &mut Vec<Gap>) -> Self {
        let os_release = read_file("/etc/os-release").unwrap_or_default();
        let os_fields = parse_kv(&os_release, '=');
        let field = |key: &str| -> Option<String> {
            os_fields
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.trim_matches('"').to_string())
        };

        let kernel_release = read_file("/proc/sys/kernel/osrelease").map(|s| s.trim().to_string());
        if kernel_release.is_none() {
            gaps.push(Gap {
                field: "platform.kernel_release".into(),
                reason: "/proc/sys/kernel/osrelease unreadable".into(),
            });
        }

        let hostname = read_file("/proc/sys/kernel/hostname")
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|| "unknown".to_string());

        let (scope, virtualization, container_runtime, evidence) = detect_scope();

        let (load1, load5, load15) = read_file("/proc/loadavg")
            .and_then(|s| {
                let mut parts = s.split_whitespace();
                Some((
                    parts.next()?.parse().ok()?,
                    parts.next()?.parse().ok()?,
                    parts.next()?.parse().ok()?,
                ))
            })
            .map(|(a, b, c): (f64, f64, f64)| (Some(a), Some(b), Some(c)))
            .unwrap_or((None, None, None));

        let uptime_seconds = read_file("/proc/uptime")
            .and_then(|s| s.split_whitespace().next()?.parse::<f64>().ok())
            .map(|v| v as u64);

        let dmi_vendor = read_file("/sys/class/dmi/id/sys_vendor").map(|s| s.trim().to_string());
        let dmi_product = read_file("/sys/class/dmi/id/product_name").map(|s| s.trim().to_string());
        let cloud_hint = cloud_hint_from_dmi(dmi_vendor.as_deref(), dmi_product.as_deref());

        Self {
            os: "linux".to_string(),
            distribution: field("ID"),
            distribution_version: field("VERSION_ID"),
            kernel_release,
            architecture: std::env::consts::ARCH.to_string(),
            scope,
            virtualization,
            virtualization_evidence: evidence,
            container_runtime,
            cgroup_cpu_limit: cgroup_cpu_limit(),
            cgroup_mem_limit_bytes: cgroup_memory_limit(),
            uptime_seconds,
            load1,
            load5,
            load15,
            running_as_root: is_root(),
            hostname: Sensitive::new(hostname),
            dmi_vendor,
            dmi_product,
            cloud_hint,
            security_modules: detect_security_modules(),
        }
    }
}

/// Detects execution scope without shelling out.
///
/// Evidence is collected and returned rather than reduced to a bare verdict,
/// because "this is a VM" is a claim a reader may reasonably want to check.
fn detect_scope() -> (Scope, Option<String>, Option<String>, Vec<String>) {
    let mut evidence = Vec::new();
    let mut container_runtime = None;

    // --- container -------------------------------------------------------
    if std::path::Path::new("/.dockerenv").exists() {
        evidence.push("/.dockerenv present".into());
        container_runtime = Some("docker".to_string());
    }
    if std::path::Path::new("/run/.containerenv").exists() {
        evidence.push("/run/.containerenv present".into());
        container_runtime.get_or_insert_with(|| "podman".to_string());
    }
    if let Some(cgroup) = read_file("/proc/1/cgroup") {
        for (marker, runtime) in [
            ("docker", "docker"),
            ("containerd", "containerd"),
            ("kubepods", "kubernetes"),
            ("lxc", "lxc"),
        ] {
            if cgroup.contains(marker) {
                evidence.push(format!("/proc/1/cgroup mentions `{marker}`"));
                container_runtime.get_or_insert_with(|| runtime.to_string());
            }
        }
    }
    if let Some(env) = read_file("/proc/1/environ") {
        if env.contains("container=lxc") {
            evidence.push("pid 1 environment declares container=lxc".into());
            container_runtime.get_or_insert_with(|| "lxc".to_string());
        }
    }

    // --- hypervisor ------------------------------------------------------
    let mut hypervisor = None;
    if let Some(cpuinfo) = read_file("/proc/cpuinfo") {
        if cpuinfo
            .lines()
            .any(|l| l.starts_with("flags") && l.contains(" hypervisor"))
        {
            evidence.push("CPUID hypervisor flag set".into());
            hypervisor = Some("unknown".to_string());
        }
    }
    let vendor = read_file("/sys/class/dmi/id/sys_vendor")
        .unwrap_or_default()
        .to_lowercase();
    let product = read_file("/sys/class/dmi/id/product_name")
        .unwrap_or_default()
        .to_lowercase();
    for (needle, name) in [
        ("kvm", "kvm"),
        ("qemu", "qemu"),
        ("vmware", "vmware"),
        ("virtualbox", "virtualbox"),
        ("innotek", "virtualbox"),
        ("xen", "xen"),
        ("microsoft corporation", "hyperv"),
        ("bochs", "qemu"),
        ("openstack", "kvm"),
        ("amazon ec2", "nitro"),
        ("google", "gce"),
        ("alibaba", "kvm"),
        ("digitalocean", "kvm"),
        ("scaleway", "kvm"),
        ("hetzner", "kvm"),
        ("oracle", "kvm"),
    ] {
        if vendor.contains(needle) || product.contains(needle) {
            evidence.push(format!("DMI identifies `{needle}`"));
            hypervisor = Some(name.to_string());
            break;
        }
    }
    if read_file("/sys/hypervisor/type").is_some() {
        evidence.push("/sys/hypervisor/type present".into());
        hypervisor.get_or_insert_with(|| "xen".to_string());
    }

    let scope = if container_runtime.is_some() {
        Scope::Container
    } else if hypervisor.is_some() {
        Scope::VirtualMachine
    } else if let Some(named) = dmi_identity(&vendor, &product) {
        // The evidence for "physical" is negative - no hypervisor marker fired -
        // and it is only worth anything if the DMI we read actually describes a
        // machine. Recording it is not decoration: every other branch here
        // cites its reason, and a scope that names itself without one is the
        // thing `scope_detection_is_evidence_backed` exists to catch.
        evidence.push(format!(
            "DMI identifies `{named}` and no container or hypervisor marker was found"
        ));
        Scope::BareMetal
    } else {
        Scope::Unknown
    };

    (scope, hypervisor, container_runtime, evidence)
}

/// The DMI identity, if the firmware actually programmed one.
///
/// `!vendor.is_empty()` is not that test. SMBIOS ships placeholder strings and
/// whitebox and OEM builders routinely leave them in: the machine this was
/// found on reports `Default string` for both `sys_vendor` and `product_name`.
/// Those fields are present, non-empty, and say nothing - so the old condition
/// classified such a host as bare metal on the strength of being able to open
/// `/sys/class/dmi/id` at all, which every VM can do too.
///
/// Treating them as absent moves that host to `Unknown`, which is what the
/// evidence supports. Nothing is lost by the change: `Scope::Unknown` arms the
/// runtime load ceiling exactly as `BareMetal` does - only `Container` disarms
/// it - so this alters what the bundle *claims*, not what the run *enforces*.
fn dmi_identity<'a>(vendor: &'a str, product: &'a str) -> Option<&'a str> {
    /// Lowercased, because both inputs already are.
    const PLACEHOLDERS: &[&str] = &[
        "default string",
        "to be filled by o.e.m.",
        "to be filled by oem",
        "system manufacturer",
        "system product name",
        "system name",
        "not specified",
        "not applicable",
        "unknown",
        "none",
        "o.e.m.",
        "oem",
        "empty",
        "n/a",
        "chassis manufacturer",
    ];
    let informative = |value: &str| {
        let value = value.trim();
        !value.is_empty() && !PLACEHOLDERS.contains(&value)
    };
    [vendor, product].into_iter().find(|v| informative(v))
}

/// cgroup v2 `cpu.max` -> whole CPUs, or v1 quota/period.
fn cgroup_cpu_limit() -> Option<f64> {
    if let Some(v2) = read_file("/sys/fs/cgroup/cpu.max") {
        let mut parts = v2.split_whitespace();
        let quota = parts.next()?;
        let period: f64 = parts.next()?.parse().ok()?;
        if quota == "max" {
            return None;
        }
        let quota: f64 = quota.parse().ok()?;
        if period > 0.0 {
            return Some(quota / period);
        }
    }
    let quota: i64 = read_parse("/sys/fs/cgroup/cpu/cpu.cfs_quota_us")?;
    let period: i64 = read_parse("/sys/fs/cgroup/cpu/cpu.cfs_period_us")?;
    (quota > 0 && period > 0).then(|| quota as f64 / period as f64)
}

fn cgroup_memory_limit() -> Option<u64> {
    if let Some(v2) = read_file("/sys/fs/cgroup/memory.max") {
        let v2 = v2.trim();
        if v2 != "max" {
            return v2.parse().ok();
        }
        return None;
    }
    let v1: u64 = read_parse("/sys/fs/cgroup/memory/memory.limit_in_bytes")?;
    // cgroup v1 signals "unlimited" with a sentinel close to u64::MAX.
    (v1 < (1u64 << 62)).then_some(v1)
}

fn is_root() -> bool {
    // Read from /proc rather than linking libc, keeping the crate `unsafe`-free.
    read_file("/proc/self/status")
        .and_then(|status| {
            status
                .lines()
                .find(|l| l.starts_with("Uid:"))?
                .split_whitespace()
                .nth(1)?
                .parse::<u32>()
                .ok()
        })
        .map(|uid| uid == 0)
        .unwrap_or(false)
}

fn detect_security_modules() -> Vec<String> {
    let mut modules = Vec::new();
    if let Some(lsm) = read_file("/sys/kernel/security/lsm") {
        modules.extend(
            lsm.trim()
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
        );
    }
    if modules.is_empty() {
        if std::path::Path::new("/sys/fs/selinux").exists() {
            modules.push("selinux".into());
        }
        if std::path::Path::new("/sys/kernel/security/apparmor").exists() {
            modules.push("apparmor".into());
        }
    }
    modules
}

fn cloud_hint_from_dmi(vendor: Option<&str>, product: Option<&str>) -> Option<String> {
    let haystack = format!(
        "{} {}",
        vendor.unwrap_or_default().to_lowercase(),
        product.unwrap_or_default().to_lowercase()
    );
    for (needle, name) in [
        ("amazon ec2", "aws"),
        ("google", "gcp"),
        ("microsoft corporation", "azure"),
        ("digitalocean", "digitalocean"),
        ("hetzner", "hetzner"),
        ("scaleway", "scaleway"),
        ("openstack", "openstack"),
        ("vultr", "vultr"),
        ("oracle", "oci"),
        ("linode", "linode"),
    ] {
        if haystack.contains(needle) {
            return Some(name.to_string());
        }
    }
    None
}

#[cfg(test)]
// In tests, `unwrap`/`expect` panicking *is* the failure signal.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic_in_result_fn)]
mod tests {
    use super::*;

    #[test]
    fn platform_collects_on_this_host() {
        let mut gaps = Vec::new();
        let p = Platform::collect(&mut gaps);
        assert_eq!(p.os, "linux");
        assert!(!p.architecture.is_empty());
        assert!(p.load1.is_some(), "loadavg should be readable: {gaps:?}");
    }

    #[test]
    fn hostname_is_not_serialised_by_default() {
        let mut gaps = Vec::new();
        let p = Platform::collect(&mut gaps);
        let json = serde_json::to_value(&p).expect("ser");
        assert_eq!(json["hostname"], crate::redact::REDACTED);
    }

    #[test]
    fn scope_detection_is_evidence_backed() {
        let (scope, _, _, evidence) = detect_scope();
        if scope != Scope::Unknown {
            assert!(
                !evidence.is_empty(),
                "a non-Unknown scope must cite evidence"
            );
        }
    }

    #[test]
    fn unprogrammed_dmi_is_not_an_identity() {
        // The exact strings seen in the wild, and the reason the bare-metal
        // branch used to fire with nothing to show for it.
        assert_eq!(dmi_identity("default string", "default string"), None);
        assert_eq!(dmi_identity("", ""), None);
        assert_eq!(
            dmi_identity("to be filled by o.e.m.", "system product name"),
            None
        );
    }

    #[test]
    fn a_programmed_dmi_field_is_an_identity() {
        assert_eq!(dmi_identity("supermicro", "x11"), Some("supermicro"));
        // Either field alone is enough; a vendor that left only the product
        // name still told us something.
        assert_eq!(
            dmi_identity("default string", "poweredge r640"),
            Some("poweredge r640")
        );
    }

    #[test]
    fn cloud_hint_matching() {
        assert_eq!(
            cloud_hint_from_dmi(Some("Amazon EC2"), None).as_deref(),
            Some("aws")
        );
        assert_eq!(
            cloud_hint_from_dmi(Some("DigitalOcean"), Some("Droplet")).as_deref(),
            Some("digitalocean")
        );
        assert_eq!(cloud_hint_from_dmi(Some("Supermicro"), Some("X11")), None);
    }
}
