//! CPU topology, frequency and capability discovery.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::{read_file, read_parse, Gap};

/// Instruction-set extensions DARCBench cares about, because each materially
/// changes the performance of a workload the suite runs.
const TRACKED_FLAGS: &[&str] = &[
    // x86-64
    "sse4_2",
    "avx",
    "avx2",
    "avx512f",
    "avx512vl",
    "aes",
    "sha_ni",
    "vaes",
    "pclmulqdq",
    "bmi2",
    "hypervisor",
    "constant_tsc",
    "tsc_reliable",
    "nonstop_tsc",
    // aarch64 (`Features` in /proc/cpuinfo)
    "asimd",
    "aes",
    "sha2",
    "sha3",
    "crc32",
    "sve",
    "sve2",
    "atomics",
];

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CpuInfo {
    pub vendor: Option<String>,
    pub model: Option<String>,
    pub family: Option<String>,
    pub stepping: Option<String>,
    pub microcode: Option<String>,
    /// Logical CPUs visible to this process.
    pub logical_cpus: usize,
    /// Distinct physical cores, derived from `core_id`/`physical_package_id`.
    pub physical_cores: Option<usize>,
    pub sockets: Option<usize>,
    pub numa_nodes: Option<usize>,
    /// True when logical CPUs outnumber physical cores.
    pub smt_enabled: Option<bool>,
    pub base_mhz: Option<f64>,
    pub max_mhz: Option<f64>,
    pub min_mhz: Option<f64>,
    pub governor: Option<String>,
    pub scaling_driver: Option<String>,
    /// Cache sizes in bytes, keyed by `L1d`, `L1i`, `L2`, `L3`.
    pub caches: Vec<CacheLevel>,
    pub instruction_sets: Vec<String>,
    /// True when the kernel reports any vulnerability mitigation as active;
    /// mitigations materially change syscall-heavy performance.
    pub mitigations: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CacheLevel {
    pub level: String,
    pub size_bytes: u64,
    /// Number of logical CPUs sharing this cache instance.
    pub shared_by: Option<usize>,
}

impl CpuInfo {
    pub fn collect(gaps: &mut Vec<Gap>) -> Self {
        let cpuinfo = read_file("/proc/cpuinfo").unwrap_or_default();
        let first_block: Vec<(String, String)> =
            crate::parse_kv(cpuinfo.split("\n\n").next().unwrap_or_default(), ':');
        let field = |key: &str| -> Option<String> {
            first_block
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.clone())
        };

        let logical_cpus = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or_else(|_| {
                gaps.push(Gap {
                    field: "cpu.logical_cpus".into(),
                    reason: "available_parallelism unavailable, falling back to /proc/cpuinfo"
                        .into(),
                });
                cpuinfo
                    .lines()
                    .filter(|l| l.starts_with("processor"))
                    .count()
                    .max(1)
            });

        let (physical_cores, sockets) = topology(logical_cpus);
        let numa_nodes = count_numa_nodes();

        let model = field("model name")
            .or_else(|| field("Model"))
            .or_else(|| field("Hardware"))
            .or_else(|| field("CPU part"));

        let instruction_sets = collect_flags(&first_block);

        Self {
            vendor: field("vendor_id").or_else(|| field("CPU implementer")),
            model,
            family: field("cpu family"),
            stepping: field("stepping"),
            microcode: field("microcode"),
            logical_cpus,
            physical_cores,
            sockets,
            numa_nodes,
            smt_enabled: physical_cores.map(|c| logical_cpus > c),
            base_mhz: field("cpu MHz").and_then(|v| v.parse().ok()),
            max_mhz: read_parse::<f64>("/sys/devices/system/cpu/cpu0/cpufreq/cpuinfo_max_freq")
                .map(|khz| khz / 1000.0),
            min_mhz: read_parse::<f64>("/sys/devices/system/cpu/cpu0/cpufreq/cpuinfo_min_freq")
                .map(|khz| khz / 1000.0),
            governor: read_file("/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor")
                .map(|s| s.trim().to_string()),
            scaling_driver: read_file("/sys/devices/system/cpu/cpu0/cpufreq/scaling_driver")
                .map(|s| s.trim().to_string()),
            caches: collect_caches(),
            instruction_sets,
            mitigations: collect_mitigations(),
        }
    }

    /// Reads the current frequency of CPU 0, in MHz.
    ///
    /// Used during a run to detect a machine dropping clocks under sustained
    /// load. Returns `None` on platforms that do not expose cpufreq (common
    /// inside VMs, where the guest cannot see the real clock at all).
    ///
    /// Callers that sample this repeatedly should resolve the source once
    /// instead - see [`crate::telemetry::TelemetrySampler`].
    pub fn current_mhz() -> Option<f64> {
        if let Some(khz) = read_parse::<f64>(SCALING_CUR_FREQ) {
            return Some(khz / 1000.0);
        }
        cpuinfo_mhz()
    }
}

/// Per-CPU current frequency, in kHz. Cheap: a handful of bytes.
pub(crate) const SCALING_CUR_FREQ: &str = "/sys/devices/system/cpu/cpu0/cpufreq/scaling_cur_freq";

/// Enough of `/proc/cpuinfo` to cover CPU 0's block on any realistic host.
const CPUINFO_FIRST_BLOCK: usize = 4096;

/// The `cpu MHz` value from CPU 0's `/proc/cpuinfo` block.
///
/// Reads only the first block: the value is per-CPU and the caller wants CPU 0,
/// so rendering every core's block to find it is wasted kernel work.
pub(crate) fn cpuinfo_mhz() -> Option<f64> {
    parse_cpuinfo_mhz(&crate::read_file_prefix(
        "/proc/cpuinfo",
        CPUINFO_FIRST_BLOCK,
    )?)
}

fn parse_cpuinfo_mhz(cpuinfo: &str) -> Option<f64> {
    cpuinfo
        .lines()
        .find(|l| l.starts_with("cpu MHz"))
        .and_then(|l| l.split(':').nth(1)?.trim().parse().ok())
}

/// Derives physical core and socket counts from sysfs topology.
fn topology(logical_cpus: usize) -> (Option<usize>, Option<usize>) {
    let mut cores = BTreeSet::new();
    let mut packages = BTreeSet::new();
    for cpu in 0..logical_cpus {
        let base = format!("/sys/devices/system/cpu/cpu{cpu}/topology");
        let package: Option<i32> = read_parse(&format!("{base}/physical_package_id"));
        let core: Option<i32> = read_parse(&format!("{base}/core_id"));
        if let (Some(package), Some(core)) = (package, core) {
            cores.insert((package, core));
            packages.insert(package);
        }
    }
    if cores.is_empty() {
        // Common in containers and some hypervisors: topology is not exported.
        return (None, None);
    }
    (Some(cores.len()), Some(packages.len()))
}

fn count_numa_nodes() -> Option<usize> {
    let entries = std::fs::read_dir("/sys/devices/system/node").ok()?;
    let count = entries
        .filter_map(Result::ok)
        .filter(|e| {
            e.file_name().to_string_lossy().starts_with("node")
                && e.file_name().to_string_lossy()[4..]
                    .chars()
                    .all(|c| c.is_ascii_digit())
        })
        .count();
    (count > 0).then_some(count)
}

fn collect_flags(block: &[(String, String)]) -> Vec<String> {
    let raw = block
        .iter()
        .find(|(k, _)| k == "flags" || k == "Features")
        .map(|(_, v)| v.clone())
        .unwrap_or_default();
    let present: BTreeSet<&str> = raw.split_whitespace().collect();
    TRACKED_FLAGS
        .iter()
        .filter(|f| present.contains(**f))
        .map(|f| f.to_string())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn collect_caches() -> Vec<CacheLevel> {
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    for index in 0..8u32 {
        let base = format!("/sys/devices/system/cpu/cpu0/cache/index{index}");
        let Some(level) = read_file(&format!("{base}/level")).map(|s| s.trim().to_string()) else {
            continue;
        };
        let kind = read_file(&format!("{base}/type"))
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        let Some(size) = read_file(&format!("{base}/size")) else {
            continue;
        };
        let Some(bytes) = parse_size_kib(size.trim()) else {
            continue;
        };

        let label = match (level.as_str(), kind.as_str()) {
            ("1", "Data") => "L1d".to_string(),
            ("1", "Instruction") => "L1i".to_string(),
            (l, _) => format!("L{l}"),
        };
        if !seen.insert(label.clone()) {
            continue;
        }
        let shared_by =
            read_file(&format!("{base}/shared_cpu_list")).map(|s| count_cpu_list(s.trim()));
        out.push(CacheLevel {
            level: label,
            size_bytes: bytes,
            shared_by,
        });
    }
    out
}

/// Parses sysfs cache sizes like `32K`, `1280K`, `32768K`, `16M`.
fn parse_size_kib(s: &str) -> Option<u64> {
    let (digits, suffix) = s.split_at(s.find(|c: char| !c.is_ascii_digit())?);
    let value: u64 = digits.parse().ok()?;
    Some(match suffix {
        "K" | "KiB" | "k" => value * 1024,
        "M" | "MiB" | "m" => value * 1024 * 1024,
        "G" | "GiB" | "g" => value * 1024 * 1024 * 1024,
        _ => value,
    })
}

/// Counts CPUs in a sysfs list like `0-3,8,10-11`.
fn count_cpu_list(list: &str) -> usize {
    list.split(',')
        .filter_map(|part| {
            let part = part.trim();
            if part.is_empty() {
                return None;
            }
            match part.split_once('-') {
                Some((a, b)) => {
                    let a: usize = a.parse().ok()?;
                    let b: usize = b.parse().ok()?;
                    Some(b.saturating_sub(a) + 1)
                }
                None => part.parse::<usize>().ok().map(|_| 1),
            }
        })
        .sum()
}

fn collect_mitigations() -> Vec<String> {
    let Ok(entries) = std::fs::read_dir("/sys/devices/system/cpu/vulnerabilities") else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();
            let value = std::fs::read_to_string(entry.path()).ok()?;
            let value = value.trim();
            (!value.starts_with("Not affected")).then(|| format!("{name}: {value}"))
        })
        .collect()
}

#[cfg(test)]
// In tests, `unwrap`/`expect` panicking *is* the failure signal.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic_in_result_fn)]
mod tests {
    use super::*;

    #[test]
    fn cpu_collects_a_plausible_topology() {
        let mut gaps = Vec::new();
        let cpu = CpuInfo::collect(&mut gaps);
        assert!(cpu.logical_cpus >= 1);
        if let Some(cores) = cpu.physical_cores {
            assert!(
                cores <= cpu.logical_cpus,
                "physical cores cannot exceed logical CPUs"
            );
            assert!(cores >= 1);
        }
    }

    #[test]
    fn cache_size_parsing() {
        assert_eq!(parse_size_kib("32K"), Some(32 * 1024));
        assert_eq!(parse_size_kib("1280K"), Some(1280 * 1024));
        assert_eq!(parse_size_kib("16M"), Some(16 * 1024 * 1024));
        assert_eq!(parse_size_kib("garbage"), None);
    }

    #[test]
    fn cpu_list_counting() {
        assert_eq!(count_cpu_list("0-3"), 4);
        assert_eq!(count_cpu_list("0-3,8,10-11"), 7);
        assert_eq!(count_cpu_list("0"), 1);
        assert_eq!(count_cpu_list(""), 0);
    }

    #[test]
    fn only_tracked_flags_are_reported() {
        let block = vec![(
            "flags".to_string(),
            "fpu vme avx2 aes some_unknown_flag sha_ni".to_string(),
        )];
        let flags = collect_flags(&block);
        assert!(flags.contains(&"avx2".to_string()));
        assert!(flags.contains(&"aes".to_string()));
        assert!(!flags.contains(&"some_unknown_flag".to_string()));
        assert!(!flags.contains(&"fpu".to_string()));
    }
}
