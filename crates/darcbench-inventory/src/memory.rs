//! Memory capacity, swap and pressure.

use serde::{Deserialize, Serialize};

use crate::{parse_kv, read_file, Gap};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryInfo {
    pub total_bytes: u64,
    /// `MemAvailable`, the only field that actually predicts what a new
    /// allocation can get. `MemFree` is famously misleading on a warm host.
    pub available_bytes: u64,
    pub free_bytes: u64,
    pub buffers_bytes: u64,
    pub cached_bytes: u64,
    pub swap_total_bytes: u64,
    pub swap_free_bytes: u64,
    pub hugepages_total: u64,
    /// `vm.swappiness`, which changes how a memory benchmark behaves.
    pub swappiness: Option<u32>,
    /// True when the host exposes PSI, enabling real memory-pressure detection.
    pub psi_available: bool,
}

impl MemoryInfo {
    pub fn collect(gaps: &mut Vec<Gap>) -> Self {
        let raw = read_file("/proc/meminfo").unwrap_or_else(|| {
            gaps.push(Gap {
                field: "memory".into(),
                reason: "/proc/meminfo unreadable".into(),
            });
            String::new()
        });
        let fields = parse_kv(&raw, ':');
        let get = |key: &str| -> u64 {
            fields
                .iter()
                .find(|(k, _)| k == key)
                .and_then(|(_, v)| v.split_whitespace().next()?.parse::<u64>().ok())
                .map(|kib| kib * 1024)
                .unwrap_or(0)
        };
        let count = |key: &str| -> u64 {
            fields
                .iter()
                .find(|(k, _)| k == key)
                .and_then(|(_, v)| v.split_whitespace().next()?.parse::<u64>().ok())
                .unwrap_or(0)
        };

        Self {
            total_bytes: get("MemTotal"),
            available_bytes: get("MemAvailable"),
            free_bytes: get("MemFree"),
            buffers_bytes: get("Buffers"),
            cached_bytes: get("Cached"),
            swap_total_bytes: get("SwapTotal"),
            swap_free_bytes: get("SwapFree"),
            hugepages_total: count("HugePages_Total"),
            swappiness: crate::read_parse("/proc/sys/vm/swappiness"),
            psi_available: std::path::Path::new("/proc/pressure/memory").exists(),
        }
    }

    pub fn used_bytes(&self) -> u64 {
        self.total_bytes.saturating_sub(self.available_bytes)
    }

    pub fn swap_used_bytes(&self) -> u64 {
        self.swap_total_bytes.saturating_sub(self.swap_free_bytes)
    }
}

/// Just the memory fields telemetry reports, parsed without allocating.
///
/// [`MemoryInfo::collect`] turns every `/proc/meminfo` key into two owned
/// strings and reads two further files. Doing that once a second, for the four
/// numbers a telemetry snapshot actually carries, was the bulk of the sampler's
/// allocation traffic.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MemoryUsage {
    pub total_bytes: u64,
    pub available_bytes: u64,
    pub swap_total_bytes: u64,
    pub swap_free_bytes: u64,
}

impl MemoryUsage {
    /// Parses the fields of interest out of `/proc/meminfo` contents.
    pub fn parse(meminfo: &str) -> Self {
        let mut out = Self::default();
        for line in meminfo.lines() {
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            let slot = match key {
                "MemTotal" => &mut out.total_bytes,
                "MemAvailable" => &mut out.available_bytes,
                "SwapTotal" => &mut out.swap_total_bytes,
                "SwapFree" => &mut out.swap_free_bytes,
                _ => continue,
            };
            if let Some(kib) = value
                .split_whitespace()
                .next()
                .and_then(|v| v.parse::<u64>().ok())
            {
                *slot = kib.saturating_mul(1024);
            }
        }
        out
    }

    pub fn used_bytes(&self) -> u64 {
        self.total_bytes.saturating_sub(self.available_bytes)
    }

    pub fn swap_used_bytes(&self) -> u64 {
        self.swap_total_bytes.saturating_sub(self.swap_free_bytes)
    }
}

#[cfg(test)]
// In tests, `unwrap`/`expect` panicking *is* the failure signal.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic_in_result_fn)]
mod tests {
    use super::*;

    #[test]
    fn memory_collects_sane_values() {
        let mut gaps = Vec::new();
        let m = MemoryInfo::collect(&mut gaps);
        assert!(m.total_bytes > 0, "gaps: {gaps:?}");
        assert!(m.available_bytes <= m.total_bytes);
        assert!(m.used_bytes() <= m.total_bytes);
    }

    #[test]
    fn usage_parsing_matches_the_full_collector() {
        let raw = read_file("/proc/meminfo").expect("/proc/meminfo");
        let usage = MemoryUsage::parse(&raw);
        let full = MemoryInfo::collect(&mut Vec::new());

        // The cheap per-sample path must agree with the full collector on
        // every field it claims to provide; a faster wrong number is worse
        // than a slow right one.
        assert_eq!(usage.total_bytes, full.total_bytes);
        assert_eq!(usage.swap_total_bytes, full.swap_total_bytes);
        assert!(usage.total_bytes > 0);
    }

    #[test]
    fn usage_parsing_ignores_junk_and_reports_kib_as_bytes() {
        let usage = MemoryUsage::parse(
            "MemTotal:       16266152 kB\n\
             MemAvailable:    8000000 kB\n\
             not a key value line\n\
             SwapTotal:        102396 kB\n\
             SwapFree:          102396 kB\n\
             Cached:          1234 kB\n",
        );
        assert_eq!(usage.total_bytes, 16_266_152 * 1024);
        assert_eq!(usage.available_bytes, 8_000_000 * 1024);
        assert_eq!(usage.used_bytes(), (16_266_152 - 8_000_000) * 1024);
        assert_eq!(usage.swap_used_bytes(), 0);
    }

    #[test]
    fn usage_parsing_of_an_empty_file_yields_zeroes_not_a_panic() {
        assert_eq!(MemoryUsage::parse(""), MemoryUsage::default());
    }

    #[test]
    fn swap_used_never_underflows() {
        let m = MemoryInfo {
            total_bytes: 1,
            available_bytes: 0,
            free_bytes: 0,
            buffers_bytes: 0,
            cached_bytes: 0,
            swap_total_bytes: 0,
            // Deliberately inconsistent input, as can happen when the two
            // fields are read a moment apart.
            swap_free_bytes: 500,
            hugepages_total: 0,
            swappiness: None,
            psi_available: false,
        };
        assert_eq!(m.swap_used_bytes(), 0);
    }
}
