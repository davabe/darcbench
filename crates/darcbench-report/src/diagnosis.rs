//! Why a long run slowed down.
//!
//! # A number, then a reason
//!
//! `darcbench-scoring`'s retention says *how much* a machine lost across an
//! endurance run. This says *why*, by putting that loss next to the telemetry
//! taken while it happened. The two answers have completely different
//! consequences for the reader:
//!
//! | Cause | What the buyer should do |
//! |---|---|
//! | Burst credits exhausted | Expect baseline, not the benchmark. Price the larger instance. |
//! | Thermal or power throttling | The cooling is the limit, not the silicon. Fixable on your own hardware. |
//! | Noisy neighbour | The host is oversubscribed. Nothing about the plan will fix it. |
//! | Undiagnosed | Something slowed down and this run cannot say what. |
//!
//! The fourth row is the one that keeps the other three honest. It is easy to
//! write a classifier that always names a cause, and the result would be a
//! guess wearing the authority of a measurement. When the evidence does not
//! separate the hypotheses, this says so.
//!
//! # The signatures
//!
//! `docs/MARKET-RESEARCH.md` supplies the distinguishing observation, and it is
//! the whole reason these three can be told apart at all:
//!
//! > Once credits are exhausted, the instance is throttled to baseline — which
//! > the guest observes as **high steal time**, not as reduced clock speed.
//!
//! So:
//!
//! * **Thermal / power throttling** is a *frequency* fall. The guest is still
//!   getting its full share of a CPU that has slowed down. Temperature usually
//!   rises with it, though a machine already at its limit when the run started
//!   may show a flat, high temperature instead - so temperature corroborates
//!   and never decides on its own.
//! * **Burst-credit exhaustion** is a *steal* rise with the clock unchanged.
//!   The CPU is as fast as ever; the guest is being given less of it. It
//!   arrives as a step - fine, fine, fine, then baseline - because a credit
//!   balance runs out at a moment rather than degrading.
//! * **A noisy neighbour** is steal that is *high and erratic throughout*
//!   rather than low then high. Contention comes and goes with somebody else's
//!   workload, so the give-away is variance, not a trend.
//!
//! # What this deliberately does not claim
//!
//! Nothing here identifies a neighbour, a hypervisor or a provider. A guest
//! cannot see outside itself, and the honest limit of these signals is "the
//! time this machine was not given" - not who took it.

use darcbench_inventory::TelemetrySnapshot;
use serde::{Deserialize, Serialize};

/// Percentage points of steal time that count as a real rise between the start
/// of a run and its end.
///
/// Five, because ordinary background scheduling on a busy hypervisor moves
/// steal by a point or two, and a threshold that trips on that would name a
/// cause for every run on every shared host.
const STEAL_RISE_PP: f64 = 5.0;

/// Steal time above which a host is contended, regardless of any trend.
const STEAL_HIGH_PCT: f64 = 5.0;

/// Fractional clock fall that counts as throttling.
///
/// Five per cent. Below that, the reading is as likely to be an artefact of
/// which core the sampler happened to read as it is a real change.
const FREQUENCY_DROP: f64 = 0.05;

/// Standard deviation of steal, in percentage points, above which contention is
/// erratic rather than steady.
const STEAL_VOLATILITY_PP: f64 = 3.0;

/// Temperature rise, in degrees, that corroborates a thermal explanation.
const TEMP_RISE_C: f64 = 5.0;

/// What made a run slow down.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SustainedCause {
    /// Throughput held. Nothing to explain.
    Sustained,
    /// The clock fell: cooling or power delivery is the limit.
    ThermalThrottling,
    /// Steal rose while the clock held: a credit balance ran out.
    BurstCreditExhaustion,
    /// Steal was high and erratic: the host is contended.
    NoisyNeighbour,
    /// Throughput fell and the telemetry does not explain it.
    Undiagnosed,
}

impl SustainedCause {
    pub fn label(self) -> &'static str {
        match self {
            Self::Sustained => "Performance sustained",
            Self::ThermalThrottling => "Thermal or power throttling",
            Self::BurstCreditExhaustion => "Burst credits exhausted",
            Self::NoisyNeighbour => "Noisy neighbour / host contention",
            Self::Undiagnosed => "Decline not explained by telemetry",
        }
    }
}

/// The telemetry evidence a diagnosis rests on, published so it can be argued
/// with.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SustainedEvidence {
    /// Mean steal time over the opening third of the run, in per cent.
    pub steal_pct_opening: f64,
    /// Mean steal time over the closing third.
    pub steal_pct_closing: f64,
    /// Standard deviation of steal across the whole run, in percentage points.
    pub steal_pct_stddev: f64,
    /// Fractional fall in mean clock between the opening and closing thirds.
    /// `None` when the host exposes no frequency at all, which is usual in a VM.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frequency_drop: Option<f64>,
    /// Rise in mean temperature between the windows, in degrees.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature_rise_c: Option<f64>,
    /// Telemetry samples the windows were taken from.
    pub samples: usize,
}

/// Why a long run slowed down, with the evidence.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SustainedDiagnosis {
    pub cause: SustainedCause,
    /// One sentence an operator can act on.
    pub explanation: String,
    pub evidence: SustainedEvidence,
}

/// Mean of the finite values a projection yields, or `None` if there are none.
fn mean<F>(samples: &[TelemetrySnapshot], project: F) -> Option<f64>
where
    F: Fn(&TelemetrySnapshot) -> Option<f64>,
{
    let values: Vec<f64> = samples
        .iter()
        .filter_map(&project)
        .filter(|v| v.is_finite())
        .collect();
    if values.is_empty() {
        return None;
    }
    Some(values.iter().sum::<f64>() / values.len() as f64)
}

fn stddev<F>(samples: &[TelemetrySnapshot], project: F) -> Option<f64>
where
    F: Fn(&TelemetrySnapshot) -> Option<f64> + Copy,
{
    let values: Vec<f64> = samples
        .iter()
        .filter_map(project)
        .filter(|v| v.is_finite())
        .collect();
    if values.len() < 2 {
        return None;
    }
    let mean = values.iter().sum::<f64>() / values.len() as f64;
    let variance =
        values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / (values.len() as f64 - 1.0);
    Some(variance.sqrt())
}

/// Reads the telemetry series into the summary the classifier works from.
fn gather(samples: &[TelemetrySnapshot]) -> SustainedEvidence {
    let window = (samples.len() / 3).max(1);
    let opening = &samples[..window.min(samples.len())];
    let closing = &samples[samples.len().saturating_sub(window)..];

    let frequency_drop = match (
        mean(opening, |s| s.cpu_freq_mhz),
        mean(closing, |s| s.cpu_freq_mhz),
    ) {
        (Some(first), Some(last)) if first > 0.0 => Some(1.0 - (last / first)),
        _ => None,
    };
    let temperature_rise_c = match (
        mean(opening, |s| s.cpu_temp_c),
        mean(closing, |s| s.cpu_temp_c),
    ) {
        (Some(first), Some(last)) => Some(last - first),
        _ => None,
    };

    SustainedEvidence {
        steal_pct_opening: mean(opening, |s| Some(s.cpu_steal_pct)).unwrap_or(0.0),
        steal_pct_closing: mean(closing, |s| Some(s.cpu_steal_pct)).unwrap_or(0.0),
        steal_pct_stddev: stddev(samples, |s| Some(s.cpu_steal_pct)).unwrap_or(0.0),
        frequency_drop,
        temperature_rise_c,
        samples: samples.len(),
    }
}

/// Explains a run's retention using the telemetry taken alongside it.
///
/// `retention` is the fraction of opening performance still present at the end,
/// from `darcbench_scoring::sustained`. `None` means the run did not cycle and
/// there is nothing to explain.
pub fn diagnose(
    retention: Option<f64>,
    declined: bool,
    samples: &[TelemetrySnapshot],
) -> Option<SustainedDiagnosis> {
    let retention = retention?;
    let evidence = gather(samples);

    // A machine that held its throughput still gets a diagnosis, because
    // "sustained" is a finding: it is the one an operator hoped for, and
    // publishing it only in the bad case would make its absence ambiguous.
    if !declined {
        return Some(SustainedDiagnosis {
            cause: SustainedCause::Sustained,
            explanation: format!(
                "Throughput held across the run: {:.0}% of the opening figure was still there at \
                 the end. Whatever this machine did in the first minutes, it kept doing.",
                retention * 100.0
            ),
            evidence,
        });
    }

    let steal_rise = evidence.steal_pct_closing - evidence.steal_pct_opening;
    let clock_fell = evidence
        .frequency_drop
        .is_some_and(|drop| drop >= FREQUENCY_DROP);
    let lost_pct = (1.0 - retention) * 100.0;

    // Order matters, and it is the order of how decisive each signal is.
    //
    // A falling clock is checked first because it is the only one of the three
    // that is unambiguous: the CPU really did slow down, and no amount of steal
    // time can produce that reading. Steal is checked second and only with the
    // clock steady, because that pairing is what separates "given less CPU"
    // from "given a slower CPU" - the distinction the market research says is
    // the whole point.
    let (cause, explanation) = if clock_fell {
        let drop = evidence.frequency_drop.unwrap_or(0.0) * 100.0;
        let heat = match evidence.temperature_rise_c {
            Some(rise) if rise >= TEMP_RISE_C => {
                format!(" Package temperature rose {rise:.0} C over the same window.")
            }
            _ => String::new(),
        };
        (
            SustainedCause::ThermalThrottling,
            format!(
                "Throughput fell {lost_pct:.0}% and the CPU clock fell {drop:.0}% with it, so the \
                 processor itself slowed down.{heat} On hardware you own this is a cooling or \
                 power-delivery limit rather than a limit of the silicon, and it is usually \
                 fixable."
            ),
        )
    } else if steal_rise >= STEAL_RISE_PP {
        (
            SustainedCause::BurstCreditExhaustion,
            format!(
                "Throughput fell {lost_pct:.0}% while the CPU clock held steady and steal time \
                 rose from {:.1}% to {:.1}%. That is the signature of a burstable instance \
                 spending its credit balance: the processor is as fast as it ever was, and this \
                 guest is simply being given less of it. Expect the later figures, not the \
                 opening ones, and treat a short benchmark of this machine as a measurement of \
                 its credits.",
                evidence.steal_pct_opening, evidence.steal_pct_closing
            ),
        )
    } else if evidence.steal_pct_stddev >= STEAL_VOLATILITY_PP
        || evidence.steal_pct_closing >= STEAL_HIGH_PCT
    {
        (
            SustainedCause::NoisyNeighbour,
            format!(
                "Throughput fell {lost_pct:.0}% with steal time averaging {:.1}% and swinging by \
                 {:.1} points across the run. Erratic rather than rising steal is contention on \
                 the host: another tenant's workload arriving and leaving. No plan change on this \
                 machine fixes it.",
                evidence.steal_pct_closing, evidence.steal_pct_stddev
            ),
        )
    } else {
        (
            SustainedCause::Undiagnosed,
            format!(
                "Throughput fell {lost_pct:.0}% over the run, and the telemetry does not explain \
                 it: the clock held, steal time stayed low and steady. Storage is the usual \
                 remaining candidate - an SLC cache filling up or a burst IOPS allowance running \
                 out - so compare the per-metric retention to see whether the loss is confined to \
                 the disk. Stated as unexplained rather than attributed to a cause this run did \
                 not observe."
            ),
        )
    };

    Some(SustainedDiagnosis {
        cause,
        explanation,
        evidence,
    })
}

#[cfg(test)]
// In tests, `unwrap`/`expect` panicking *is* the failure signal.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic_in_result_fn)]
mod tests {
    use super::*;

    fn sample(steal: f64, freq: Option<f64>, temp: Option<f64>) -> TelemetrySnapshot {
        TelemetrySnapshot {
            cpu_busy_pct: 100.0,
            cpu_external_busy_pct: 0.0,
            cpu_steal_pct: steal,
            cpu_iowait_pct: 0.0,
            load1: 4.0,
            mem_used_bytes: 0,
            mem_total_bytes: 0,
            swap_used_bytes: 0,
            cpu_freq_mhz: freq,
            cpu_temp_c: temp,
            psi_cpu_some_avg10: None,
            psi_io_some_avg10: None,
            psi_mem_some_avg10: None,
            cpu_unavailable: false,
            disk_read_bytes_per_s: 0,
            disk_write_bytes_per_s: 0,
            net_rx_bytes_per_s: 0,
            net_tx_bytes_per_s: 0,
        }
    }

    /// Half the samples at the opening condition, half at the closing one.
    fn series(open: TelemetrySnapshot, close: TelemetrySnapshot) -> Vec<TelemetrySnapshot> {
        let mut out = vec![open; 30];
        out.extend(std::iter::repeat_n(close, 30));
        out
    }

    #[test]
    fn a_run_that_did_not_cycle_is_not_diagnosed() {
        assert!(diagnose(None, false, &[]).is_none());
    }

    #[test]
    fn a_machine_that_held_up_is_told_so() {
        let samples = series(
            sample(0.0, Some(3800.0), Some(55.0)),
            sample(0.0, Some(3800.0), Some(56.0)),
        );
        let d = diagnose(Some(0.99), false, &samples).expect("diagnosis");
        assert_eq!(d.cause, SustainedCause::Sustained);
    }

    /// The bare-metal signature: the clock falls and the heat rises.
    #[test]
    fn a_falling_clock_is_thermal_throttling() {
        let samples = series(
            sample(0.0, Some(4000.0), Some(52.0)),
            sample(0.0, Some(2800.0), Some(95.0)),
        );
        let d = diagnose(Some(0.68), true, &samples).expect("diagnosis");
        assert_eq!(d.cause, SustainedCause::ThermalThrottling);
        assert!(
            d.explanation.contains("temperature") || d.explanation.contains("C over"),
            "the corroborating heat must appear in the explanation: {}",
            d.explanation
        );
        assert!(d.evidence.frequency_drop.expect("drop") > 0.25);
    }

    /// The burstable-instance signature, and the one the market research calls
    /// the strongest argument for the profile existing.
    #[test]
    fn rising_steal_with_a_steady_clock_is_burst_credit_exhaustion() {
        let samples = series(
            sample(0.2, Some(2900.0), None),
            sample(62.0, Some(2900.0), None),
        );
        let d = diagnose(Some(0.38), true, &samples).expect("diagnosis");
        assert_eq!(d.cause, SustainedCause::BurstCreditExhaustion);
        assert!(d.explanation.contains("credit"));
        assert!(d.evidence.steal_pct_closing > d.evidence.steal_pct_opening);
    }

    /// A guest with no `cpufreq` at all - the usual case on a VPS - must still
    /// reach the steal-based diagnosis rather than falling through to
    /// undiagnosed for want of a frequency reading.
    #[test]
    fn a_guest_without_a_frequency_reading_is_still_diagnosable() {
        let samples = series(sample(0.5, None, None), sample(40.0, None, None));
        let d = diagnose(Some(0.5), true, &samples).expect("diagnosis");
        assert_eq!(d.cause, SustainedCause::BurstCreditExhaustion);
        assert!(d.evidence.frequency_drop.is_none());
    }

    /// Contention is erratic, not monotone. Steal that swings without rising
    /// must not be reported as a credit balance running out.
    #[test]
    fn erratic_steal_is_a_noisy_neighbour_not_a_credit_balance() {
        let mut samples = Vec::new();
        for i in 0..60 {
            // Oscillates between quiet and heavily contended throughout, so the
            // opening and closing means are the same and only the variance is
            // large.
            let steal = if i % 2 == 0 { 1.0 } else { 18.0 };
            samples.push(sample(steal, Some(2900.0), None));
        }
        let d = diagnose(Some(0.7), true, &samples).expect("diagnosis");
        assert_eq!(d.cause, SustainedCause::NoisyNeighbour);
        assert!(d.evidence.steal_pct_stddev > STEAL_VOLATILITY_PP);
    }

    /// The rule that keeps the other three honest.
    #[test]
    fn an_unexplained_decline_is_named_unexplained() {
        let samples = series(
            sample(0.1, Some(2900.0), Some(50.0)),
            sample(0.1, Some(2900.0), Some(50.0)),
        );
        let d = diagnose(Some(0.55), true, &samples).expect("diagnosis");
        assert_eq!(
            d.cause,
            SustainedCause::Undiagnosed,
            "a classifier that always names a cause is a guess with a measurement's authority"
        );
        // It must still be useful: point at the remaining candidate.
        assert!(d.explanation.contains("SLC") || d.explanation.contains("disk"));
    }

    /// Thermal beats steal when both are present: only one of them can make a
    /// clock reading fall.
    #[test]
    fn a_falling_clock_outranks_rising_steal() {
        let samples = series(
            sample(0.0, Some(4000.0), Some(50.0)),
            sample(30.0, Some(2000.0), Some(90.0)),
        );
        let d = diagnose(Some(0.4), true, &samples).expect("diagnosis");
        assert_eq!(d.cause, SustainedCause::ThermalThrottling);
    }

    /// No telemetry at all must not panic or fabricate evidence.
    #[test]
    fn an_empty_telemetry_series_yields_an_undiagnosed_decline() {
        let d = diagnose(Some(0.5), true, &[]).expect("diagnosis");
        assert_eq!(d.cause, SustainedCause::Undiagnosed);
        assert_eq!(d.evidence.samples, 0);
        assert!(d.evidence.frequency_drop.is_none());
    }
}
