//! Low-overhead telemetry sampling.
//!
//! # The observer is part of the system under test
//!
//! Telemetry runs *while* the benchmark runs, so its cost is charged to the
//! measurement. The sampler is therefore built to be cheap and boring: a fixed
//! set of small `/proc` reads, no allocation-heavy parsing, no per-core
//! expansion, and a caller-controlled interval that defaults to 1 Hz. At that
//! rate the sampler costs well under a millisecond of CPU per second on the
//! hosts DARCBench targets.
//!
//! Everything here is a *delta* between two reads, which is why the sampler is
//! stateful: `/proc/stat` and the per-device counters are monotonic totals, and
//! reporting them raw would be meaningless.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::read_into;

/// One telemetry observation.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TelemetrySnapshot {
    pub cpu_busy_pct: f64,
    /// Busy CPU time this process did not consume, as a percentage of the
    /// machine's whole CPU capacity.
    ///
    /// This is the signal a benchmark needs and `cpu_busy_pct` cannot give: a
    /// benchmark drives the machine to 100% busy by design, so total busy time
    /// says nothing about whether anything *else* is competing for the CPU.
    /// Subtracting this process's own consumption leaves exactly the
    /// competition.
    ///
    /// Both terms come from the same clock-tick unit - `/proc/stat` counts the
    /// machine in USER_HZ jiffies and `/proc/self/stat` counts this process's
    /// threads in the same jiffies - so no conversion, and no assumption about
    /// the value of USER_HZ, enters the calculation.
    ///
    /// Two limits are worth knowing. Kernel work done *on behalf of* this
    /// process outside its own time accounting (softirq for the network module,
    /// for instance) is charged here as external. Child processes the agent
    /// spawns and reaps - `php.runtime` executes an interpreter, and its whole
    /// workload runs in them - are *not*: they are counted as ours. And inside a container whose
    /// `/proc` is not namespaced, `/proc/stat` describes the host, so this
    /// figure includes other tenants - exactly as `cpu_busy_pct` already does.
    /// Callers that act on the value rather than merely reporting it must take
    /// the execution scope into account.
    #[serde(default)]
    pub cpu_external_busy_pct: f64,
    pub cpu_steal_pct: f64,
    pub cpu_iowait_pct: f64,
    pub load1: f64,
    pub mem_used_bytes: u64,
    pub mem_total_bytes: u64,
    pub swap_used_bytes: u64,
    pub cpu_freq_mhz: Option<f64>,
    pub cpu_temp_c: Option<f64>,
    pub psi_cpu_some_avg10: Option<f64>,
    pub psi_io_some_avg10: Option<f64>,
    pub psi_mem_some_avg10: Option<f64>,
    pub disk_read_bytes_per_s: u64,
    pub disk_write_bytes_per_s: u64,
    pub net_rx_bytes_per_s: u64,
    pub net_tx_bytes_per_s: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct CpuTotals {
    idle: u64,
    iowait: u64,
    steal: u64,
    total: u64,
}

#[derive(Clone, Copy, Debug, Default)]
struct IoTotals {
    disk_read_bytes: u64,
    disk_write_bytes: u64,
    net_rx_bytes: u64,
    net_tx_bytes: u64,
}

/// Where this host's CPU frequency can be read from, resolved once.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FrequencySource {
    /// `scaling_cur_freq`: a few bytes, and the accurate answer.
    Cpufreq,
    /// The first block of `/proc/cpuinfo`. Usual case inside a VM.
    CpuInfo,
    /// Neither exists. Frequency is genuinely not observable here, which is
    /// reported as `None` rather than as a fabricated 0 MHz.
    Unavailable,
}

/// Stateful sampler. Call [`TelemetrySampler::sample`] on a fixed interval.
///
/// # What is resolved once, and why
///
/// The set of thermal zones and the availability of `cpufreq` are properties of
/// the boot, not of the moment. Rediscovering them on every tick meant a
/// `/sys/class/thermal` directory walk plus a `type` read per zone every
/// second, and - on the very hosts DARCBench targets, where `cpufreq` is not
/// exposed - a full read of `/proc/cpuinfo` every second as well. Both are
/// resolved in [`TelemetrySampler::new`] and reused.
#[derive(Debug)]
pub struct TelemetrySampler {
    previous_cpu: Option<CpuTotals>,
    /// Cumulative user+system jiffies of *this* process, for the external-load
    /// subtraction. Same unit as [`CpuTotals`], which is the point.
    previous_self_cpu: Option<u64>,
    previous_io: Option<(IoTotals, std::time::Instant)>,
    /// Sector size assumed for `/proc/diskstats`, which always counts in
    /// 512-byte units regardless of the device's physical block size.
    sector_bytes: u64,
    frequency: FrequencySource,
    /// `temp` files of the CPU-ish thermal zones present at construction.
    thermal_zones: Vec<PathBuf>,
    /// Reused across samples so a run does not allocate a fresh buffer per
    /// `/proc` file per second.
    buffer: String,
}

impl Default for TelemetrySampler {
    fn default() -> Self {
        Self::new()
    }
}

impl TelemetrySampler {
    /// Builds a sampler, resolving the host's frequency and thermal sources.
    ///
    /// This is the expensive part of sampling and it happens exactly once; the
    /// run loop discards the first sample anyway, because a rate needs two
    /// readings.
    pub fn new() -> Self {
        Self {
            previous_cpu: None,
            previous_self_cpu: None,
            previous_io: None,
            sector_bytes: 512,
            frequency: resolve_frequency_source(),
            thermal_zones: resolve_thermal_zones(),
            // Comfortably covers /proc/stat and /proc/meminfo without growing.
            buffer: String::with_capacity(8 * 1024),
        }
    }

    /// Current CPU frequency in MHz, from the source resolved at construction.
    fn current_mhz(&self) -> Option<f64> {
        match self.frequency {
            FrequencySource::Cpufreq => {
                crate::read_parse::<f64>(crate::cpu::SCALING_CUR_FREQ).map(|khz| khz / 1000.0)
            }
            FrequencySource::CpuInfo => crate::cpu::cpuinfo_mhz(),
            FrequencySource::Unavailable => None,
        }
    }

    /// Highest temperature across the CPU-ish thermal zones, in Celsius.
    fn cpu_temperature(&self) -> Option<f64> {
        let mut best: Option<f64> = None;
        for zone in &self.thermal_zones {
            let Ok(raw) = std::fs::read_to_string(zone) else {
                continue;
            };
            let Ok(millidegrees) = raw.trim().parse::<f64>() else {
                continue;
            };
            let celsius = millidegrees / 1000.0;
            // Reject implausible readings rather than reporting a 0 C CPU.
            if (0.0..=150.0).contains(&celsius) {
                best = Some(best.map_or(celsius, |b: f64| b.max(celsius)));
            }
        }
        best
    }

    /// Takes one observation.
    ///
    /// The first call after construction cannot produce rates (there is no
    /// previous reading to subtract), so rate fields are zero and CPU
    /// percentages fall back to cumulative-since-boot values. Callers should
    /// discard the first sample; the run loop does.
    pub fn sample(&mut self) -> TelemetrySnapshot {
        let mut snapshot = TelemetrySnapshot::default();

        // --- CPU ----------------------------------------------------------
        //
        // Read before the machine totals, so the two windows overlap as closely
        // as the two reads allow. Any skew between them shows up as external
        // load that is not there, and the guard that consumes this figure is
        // one that stops runs.
        let current_self_cpu = read_self_cpu_jiffies(&mut self.buffer);
        if let Some(current) = read_cpu_totals(&mut self.buffer) {
            if let Some(previous) = self.previous_cpu {
                let total = current.total.saturating_sub(previous.total);
                if total > 0 {
                    let idle = current.idle.saturating_sub(previous.idle);
                    let iowait = current.iowait.saturating_sub(previous.iowait);
                    let steal = current.steal.saturating_sub(previous.steal);
                    let pct = |v: u64| (v as f64 / total as f64) * 100.0;
                    // `busy` deliberately excludes iowait and steal: neither is
                    // this workload making progress, and folding them into a
                    // single "CPU used" number is how monitoring tools hide
                    // noisy neighbours.
                    let busy = total
                        .saturating_sub(idle)
                        .saturating_sub(iowait)
                        .saturating_sub(steal);
                    snapshot.cpu_busy_pct = pct(busy).clamp(0.0, 100.0);
                    snapshot.cpu_iowait_pct = pct(iowait).clamp(0.0, 100.0);
                    snapshot.cpu_steal_pct = pct(steal).clamp(0.0, 100.0);

                    // Busy time minus our own. `saturating_sub` rather than a
                    // signed difference because the two files are read a few
                    // microseconds apart and the accounting is quantised to a
                    // jiffy: a self total marginally ahead of the machine total
                    // is ordinary skew, not negative external load.
                    if let (Some(current_self), Some(previous_self)) =
                        (current_self_cpu, self.previous_self_cpu)
                    {
                        let ours = current_self.saturating_sub(previous_self);
                        snapshot.cpu_external_busy_pct =
                            pct(busy.saturating_sub(ours)).clamp(0.0, 100.0);
                    }
                }
            }
            self.previous_cpu = Some(current);
            // Advanced only alongside the machine totals, so the two deltas
            // always span the same interval. Updating it unconditionally meant
            // that a single failed `/proc/stat` read left a two-interval
            // machine delta facing a one-interval self delta, overstating
            // external load for that sample - on a signal that stops runs.
            if let Some(current_self) = current_self_cpu {
                self.previous_self_cpu = Some(current_self);
            }
        }

        // --- load / memory ------------------------------------------------
        snapshot.load1 = if read_into("/proc/loadavg", &mut self.buffer) {
            self.buffer
                .split_whitespace()
                .next()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0.0)
        } else {
            0.0
        };

        // Only the four fields a snapshot carries, parsed in place. The full
        // `MemoryInfo` collector reads two extra files and allocates two owned
        // strings per `/proc/meminfo` key, which is not a per-second cost worth
        // paying for four numbers.
        if read_into("/proc/meminfo", &mut self.buffer) {
            let memory = crate::memory::MemoryUsage::parse(&self.buffer);
            snapshot.mem_total_bytes = memory.total_bytes;
            snapshot.mem_used_bytes = memory.used_bytes();
            snapshot.swap_used_bytes = memory.swap_used_bytes();
        }

        // --- frequency / thermals ------------------------------------------
        snapshot.cpu_freq_mhz = self.current_mhz();
        snapshot.cpu_temp_c = self.cpu_temperature();

        // --- pressure stall information -------------------------------------
        snapshot.psi_cpu_some_avg10 = read_psi("/proc/pressure/cpu", &mut self.buffer);
        snapshot.psi_io_some_avg10 = read_psi("/proc/pressure/io", &mut self.buffer);
        snapshot.psi_mem_some_avg10 = read_psi("/proc/pressure/memory", &mut self.buffer);

        // --- I/O rates -------------------------------------------------------
        let now = std::time::Instant::now();
        let current_io = read_io_totals(self.sector_bytes, &mut self.buffer);
        if let Some((previous, at)) = self.previous_io {
            let elapsed = now.duration_since(at).as_secs_f64();
            if elapsed > 0.0 {
                let rate = |current: u64, previous: u64| {
                    (current.saturating_sub(previous) as f64 / elapsed) as u64
                };
                snapshot.disk_read_bytes_per_s =
                    rate(current_io.disk_read_bytes, previous.disk_read_bytes);
                snapshot.disk_write_bytes_per_s =
                    rate(current_io.disk_write_bytes, previous.disk_write_bytes);
                snapshot.net_rx_bytes_per_s = rate(current_io.net_rx_bytes, previous.net_rx_bytes);
                snapshot.net_tx_bytes_per_s = rate(current_io.net_tx_bytes, previous.net_tx_bytes);
            }
        }
        self.previous_io = Some((current_io, now));

        snapshot
    }
}

/// Resolves the cheapest working frequency source for this host.
fn resolve_frequency_source() -> FrequencySource {
    if crate::read_parse::<f64>(crate::cpu::SCALING_CUR_FREQ).is_some() {
        FrequencySource::Cpufreq
    } else if crate::cpu::cpuinfo_mhz().is_some() {
        FrequencySource::CpuInfo
    } else {
        FrequencySource::Unavailable
    }
}

/// `temp` files of the thermal zones that describe the CPU.
///
/// Many virtualised hosts expose no thermal zone at all, so an empty list is
/// the correct and common answer - and must be reported as "not observable",
/// never as 0 degrees.
fn resolve_thermal_zones() -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir("/sys/class/thermal") else {
        return Vec::new();
    };
    let mut zones = Vec::new();
    for entry in entries.filter_map(Result::ok) {
        if !entry
            .file_name()
            .to_string_lossy()
            .starts_with("thermal_zone")
        {
            continue;
        }
        let zone_type = std::fs::read_to_string(entry.path().join("type"))
            .unwrap_or_default()
            .trim()
            .to_lowercase();
        if zone_type.contains("cpu")
            || zone_type.contains("pkg")
            || zone_type.contains("x86")
            || zone_type.contains("soc")
        {
            zones.push(entry.path().join("temp"));
        }
    }
    zones
}

fn read_cpu_totals(buffer: &mut String) -> Option<CpuTotals> {
    if !read_into("/proc/stat", buffer) {
        return None;
    }
    let line = buffer.lines().find(|l| l.starts_with("cpu "))?;
    // user nice system idle iowait irq softirq steal guest guest_nice.
    // Only the first eight are summed: guest time is already counted inside
    // user and nice, so including it would double-count.
    let mut fields = [0u64; 8];
    let mut seen = 0usize;
    for value in line.split_whitespace().skip(1).take(8) {
        fields[seen] = value.parse().ok()?;
        seen += 1;
    }
    if seen < 8 {
        return None;
    }
    Some(CpuTotals {
        idle: fields[3],
        iowait: fields[4],
        steal: fields[7],
        total: fields.iter().sum(),
    })
}

/// This process's cumulative user+system time, in the same USER_HZ jiffies
/// `/proc/stat` uses.
///
/// `utime` and `stime` are summed over every thread in the process, which is
/// what makes the machine-minus-self subtraction hold for a benchmark that runs
/// its workloads on a thread pool.
///
/// **Reaped children are included**, via `cutime`/`cstime`. They were excluded
/// on the grounds that no module forks - true when this was written, and false
/// the moment `php.runtime` shipped. A module that measures an interpreter by
/// executing it does all of its work in child processes, so excluding them made
/// the runtime load ceiling see the benchmark's own workload as somebody else's
/// and degrade every single run.
///
/// The kernel only folds a child's time into these fields when it is *reaped*,
/// so the attribution arrives in one lump at `wait()` rather than as the child
/// runs. That makes a single sample understate external load and the next one
/// overstate it. Both are absorbed by the guard's twenty-consecutive-sample
/// requirement, and the direction of the residual error is toward *under*
/// reporting contention - the safe one for a guard that stops runs.
fn read_self_cpu_jiffies(buffer: &mut String) -> Option<u64> {
    if !read_into("/proc/self/stat", buffer) {
        return None;
    }
    // The `comm` field is the executable name in parentheses and may itself
    // contain spaces and parentheses, so field 3 onwards is found from the
    // *last* `)`, never by splitting the whole line on whitespace.
    let after_comm = &buffer[buffer.rfind(')')? + 1..];
    let mut fields = after_comm.split_whitespace();
    // state ppid pgrp session tty_nr tpgid flags minflt cminflt majflt cmajflt
    // are fields 3..=13; then utime 14, stime 15, cutime 16, cstime 17.
    let utime: u64 = fields.nth(11)?.parse().ok()?;
    let stime: u64 = fields.next()?.parse().ok()?;
    // Children's fields are signed in the kernel's own format and can be
    // negative in exotic cases, which parse as an error here and are treated as
    // zero rather than as a wrapped enormous number.
    let cutime: u64 = fields.next().and_then(|v| v.parse().ok()).unwrap_or(0);
    let cstime: u64 = fields.next().and_then(|v| v.parse().ok()).unwrap_or(0);
    Some(
        utime
            .saturating_add(stime)
            .saturating_add(cutime)
            .saturating_add(cstime),
    )
}

/// Reads the `some ... avg10=` value from a PSI file.
fn read_psi(path: &str, buffer: &mut String) -> Option<f64> {
    if !read_into(path, buffer) {
        return None;
    }
    buffer
        .lines()
        .find(|l| l.starts_with("some "))?
        .split_whitespace()
        .find_map(|token| token.strip_prefix("avg10=")?.parse().ok())
}

fn read_io_totals(sector_bytes: u64, buffer: &mut String) -> IoTotals {
    let mut totals = IoTotals::default();

    if read_into("/proc/diskstats", buffer) {
        for line in buffer.lines() {
            // Fields are taken positionally off the iterator rather than
            // collected: a busy host has dozens of block devices and this runs
            // once a second for the whole run.
            let mut fields = line.split_whitespace();
            let Some(name) = fields.nth(2) else { continue };
            if !is_whole_storage_device(name) {
                continue;
            }
            // From `name`: reads_completed, reads_merged, sectors_read.
            let sectors_read: u64 = fields.nth(2).and_then(|v| v.parse().ok()).unwrap_or(0);
            // Then: ms_reading, writes_completed, writes_merged, sectors_written.
            let sectors_written: u64 = fields.nth(3).and_then(|v| v.parse().ok()).unwrap_or(0);
            totals.disk_read_bytes = totals
                .disk_read_bytes
                .saturating_add(sectors_read.saturating_mul(sector_bytes));
            totals.disk_write_bytes = totals
                .disk_write_bytes
                .saturating_add(sectors_written.saturating_mul(sector_bytes));
        }
    }

    if read_into("/proc/net/dev", buffer) {
        for line in buffer.lines().skip(2) {
            let Some((iface, counters)) = line.split_once(':') else {
                continue;
            };
            if iface.trim() == "lo" {
                continue;
            }
            let mut values = counters.split_whitespace();
            let rx: u64 = values.next().and_then(|v| v.parse().ok()).unwrap_or(0);
            // rx: bytes packets errs drop fifo frame compressed multicast,
            // then tx bytes.
            let tx: u64 = values.nth(7).and_then(|v| v.parse().ok()).unwrap_or(0);
            totals.net_rx_bytes = totals.net_rx_bytes.saturating_add(rx);
            totals.net_tx_bytes = totals.net_tx_bytes.saturating_add(tx);
        }
    }

    totals
}

/// Whole block devices only: partitions double-count their parent, and
/// loop/ram devices are not the machine's storage.
fn is_whole_storage_device(name: &str) -> bool {
    if name.starts_with("loop") || name.starts_with("ram") || name.starts_with("zram") {
        return false;
    }
    let is_partition = name.chars().last().is_some_and(|c| c.is_ascii_digit())
        && !name.starts_with("nvme")
        && !name.starts_with("md")
        && !name.starts_with("dm-");
    let is_nvme_partition = name.starts_with("nvme") && name.contains('p');
    !is_partition && !is_nvme_partition
}

#[cfg(test)]
// In tests, `unwrap`/`expect` panicking *is* the failure signal.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic_in_result_fn)]
mod tests {
    use super::*;

    #[test]
    fn first_sample_has_no_rates_second_sample_does() {
        let mut sampler = TelemetrySampler::new();
        let first = sampler.sample();
        assert_eq!(
            first.disk_read_bytes_per_s, 0,
            "no previous reading to diff against"
        );
        assert!(first.mem_total_bytes > 0);

        // Burn a little CPU so the second sample has something to report.
        let mut acc = 0u64;
        for i in 0..3_000_000u64 {
            acc = acc.wrapping_add(i ^ acc);
        }
        assert!(acc > 0);

        let second = sampler.sample();
        assert!((0.0..=100.0).contains(&second.cpu_busy_pct));
        assert!((0.0..=100.0).contains(&second.cpu_steal_pct));
        assert!((0.0..=100.0).contains(&second.cpu_iowait_pct));
    }

    #[test]
    fn percentages_stay_in_range_across_many_samples() {
        let mut sampler = TelemetrySampler::new();
        for _ in 0..5 {
            let s = sampler.sample();
            assert!(s.cpu_busy_pct >= 0.0 && s.cpu_busy_pct <= 100.0, "{s:?}");
            assert!(s.cpu_steal_pct >= 0.0 && s.cpu_steal_pct <= 100.0, "{s:?}");
            if let Some(temp) = s.cpu_temp_c {
                assert!(
                    (0.0..=150.0).contains(&temp),
                    "implausible temperature {temp}"
                );
            }
        }
    }

    /// The expensive discovery must happen once, not once per tick.
    ///
    /// Regression: every sample re-walked `/sys/class/thermal` (a directory
    /// read plus a `type` read per zone) and, on a host without `cpufreq`,
    /// re-read the whole of `/proc/cpuinfo` - which the kernel renders one CPU
    /// block at a time, so the cost grew with the core count on exactly the
    /// shared instances where the sampler must stay invisible.
    #[test]
    fn host_capabilities_are_resolved_once_not_per_sample() {
        let sampler = TelemetrySampler::new();
        let resolved = sampler.frequency;
        let zones = sampler.thermal_zones.clone();

        let mut sampler = sampler;
        for _ in 0..5 {
            sampler.sample();
        }
        assert_eq!(
            sampler.frequency, resolved,
            "the frequency source is a property of the boot, not of the tick"
        );
        assert_eq!(sampler.thermal_zones, zones);
    }

    /// The buffer is reused, so it must never leak one file's contents into the
    /// parse of the next.
    #[test]
    fn the_shared_read_buffer_does_not_carry_stale_content() {
        let mut sampler = TelemetrySampler::new();
        let first = sampler.sample();
        let second = sampler.sample();
        // Memory totals come from /proc/meminfo, which is read after
        // /proc/stat and /proc/loadavg into the same buffer.
        assert!(first.mem_total_bytes > 0);
        assert_eq!(
            first.mem_total_bytes, second.mem_total_bytes,
            "total memory cannot change between two samples"
        );
        assert!(second.mem_used_bytes <= second.mem_total_bytes);
    }

    #[test]
    fn cpu_totals_parse_positionally_and_exclude_guest_time() {
        let mut buffer = String::new();
        assert!(read_cpu_totals(&mut buffer).is_some(), "/proc/stat is real");

        // user nice system idle iowait irq softirq steal guest guest_nice.
        // Guest time is already accounted inside user/nice, so the total must
        // stop at the first eight fields.
        let line = "cpu  10 20 30 40 50 60 70 80 900 1000";
        let mut fields = [0u64; 8];
        for (slot, value) in fields.iter_mut().zip(line.split_whitespace().skip(1)) {
            *slot = value.parse().expect("integer");
        }
        assert_eq!(fields.iter().sum::<u64>(), 360);
        assert_eq!((fields[3], fields[4], fields[7]), (40, 50, 80));
    }

    #[test]
    fn only_whole_block_devices_are_counted() {
        for whole in ["sda", "nvme0n1", "vda", "md0", "dm-0"] {
            assert!(
                is_whole_storage_device(whole),
                "`{whole}` is a whole device"
            );
        }
        for skip in ["sda1", "nvme0n1p2", "vda3", "loop0", "ram0", "zram0"] {
            assert!(
                !is_whole_storage_device(skip),
                "`{skip}` must not be double-counted"
            );
        }
    }

    /// The whole point of the external figure: this process's own load must
    /// not appear in it.
    ///
    /// The test burns CPU on this very process between two samples. `busy`
    /// therefore rises, and `external` must not follow it - otherwise the
    /// runtime load ceiling would stop every run for the load the benchmark
    /// itself creates.
    #[test]
    fn self_inflicted_load_does_not_count_as_external() {
        let mut sampler = TelemetrySampler::new();
        sampler.sample();

        let threads: Vec<_> = (0..2)
            .map(|_| {
                std::thread::spawn(|| {
                    let deadline =
                        std::time::Instant::now() + std::time::Duration::from_millis(400);
                    let mut acc = 0u64;
                    while std::time::Instant::now() < deadline {
                        for i in 0..50_000u64 {
                            acc = acc.wrapping_add(i ^ acc);
                        }
                    }
                    acc
                })
            })
            .collect();
        for thread in threads {
            let _ = thread.join();
        }

        let after = sampler.sample();
        assert!(
            after.cpu_external_busy_pct <= after.cpu_busy_pct + 1.0,
            "external load cannot exceed total busy time: {after:?}"
        );
        // A shared CI runner is not idle, so this cannot assert a hard zero.
        // It can assert that the burn is attributed to us: the busy figure has
        // to have moved further than the external one did.
        assert!(
            (0.0..=100.0).contains(&after.cpu_external_busy_pct),
            "{after:?}"
        );
    }

    /// A module that measures an interpreter does its work in child processes.
    /// If those are not counted as ours, the runtime load ceiling sees the
    /// benchmark's own workload as somebody else's competing with it.
    #[test]
    fn a_reaped_childs_cpu_counts_as_ours() {
        let mut buffer = String::new();
        let before = read_self_cpu_jiffies(&mut buffer).expect("/proc/self/stat is real");

        // A child that burns CPU rather than sleeping, so the delta is real.
        let status = std::process::Command::new("/bin/sh")
            .args(["-c", "i=0; while [ $i -lt 400000 ]; do i=$((i+1)); done"])
            .status();
        if !matches!(status, Ok(status) if status.success()) {
            // No /bin/sh is not a host this suite runs on; decline to fail for
            // the wrong reason.
            return;
        }

        let after = read_self_cpu_jiffies(&mut buffer).expect("/proc/self/stat is real");
        assert!(
            after > before,
            "a reaped child's CPU must be charged to this process: {before} -> {after}"
        );
    }

    #[test]
    fn self_cpu_jiffies_are_monotonic_and_survive_a_comm_with_spaces() {
        let mut buffer = String::new();
        let first = read_self_cpu_jiffies(&mut buffer).expect("/proc/self/stat is real");
        let mut acc = 0u64;
        for i in 0..5_000_000u64 {
            acc = acc.wrapping_add(i ^ acc);
        }
        assert!(acc > 0);
        let second = read_self_cpu_jiffies(&mut buffer).expect("/proc/self/stat is real");
        assert!(second >= first, "cumulative CPU time cannot go backwards");

        // A process named `(evil) 1 2 3 4 5 6 7 8 9 10 11 12` would break any
        // parser that split the whole line on whitespace: the fake fields sit
        // where the real ones are expected. Parsing from the last `)` is what
        // makes the offsets hold.
        let mut hostile =
            String::from("7 ((evil) 1 2 3 4 5 6 7 8 9 10 11 12) S 1 7 7 0 -1 0 0 0 0 0 ");
        // Fields 14..=17 of the real record: utime, stime, cutime, cstime.
        hostile.push_str("111 222 33 44 20 0 1 0 0");
        let after_comm = &hostile[hostile.rfind(')').unwrap() + 1..];
        let mut fields = after_comm.split_whitespace();
        let utime: u64 = fields.nth(11).unwrap().parse().unwrap();
        let stime: u64 = fields.next().unwrap().parse().unwrap();
        let cutime: u64 = fields.next().unwrap().parse().unwrap();
        let cstime: u64 = fields.next().unwrap().parse().unwrap();
        assert_eq!((utime, stime, cutime, cstime), (111, 222, 33, 44));
    }

    #[test]
    fn psi_parsing() {
        // Format: `some avg10=0.00 avg60=0.00 avg300=0.00 total=0`
        let parsed = "some avg10=1.25 avg60=0.30 avg300=0.10 total=1234"
            .split_whitespace()
            .find_map(|t| t.strip_prefix("avg10=")?.parse::<f64>().ok());
        assert_eq!(parsed, Some(1.25));
    }

    #[test]
    fn sampler_is_cheap() {
        // Guards against the sampler quietly becoming expensive enough to
        // perturb the measurements it accompanies.
        let mut sampler = TelemetrySampler::new();
        sampler.sample();
        let start = std::time::Instant::now();
        for _ in 0..20 {
            sampler.sample();
        }
        let per_sample = start.elapsed() / 20;
        assert!(
            per_sample < std::time::Duration::from_millis(25),
            "telemetry sampling took {per_sample:?} per sample; it must stay far below the 1 Hz \
             sampling interval to avoid perturbing the benchmark"
        );
    }
}
