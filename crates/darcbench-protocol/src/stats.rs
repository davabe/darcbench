//! Statistics used to turn raw samples into a reportable metric.
//!
//! Rationale for each choice is in `docs/BENCHMARK-METHODOLOGY.md`. Summary:
//!
//! * **Median, not mean**, is the headline estimator. Benchmark sample
//!   distributions on shared infrastructure are right-skewed: a single
//!   steal-time spike inflates a mean but barely moves a median.
//! * **Coefficient of variation** is reported so a reader can see instability
//!   instead of only a point estimate. A high CV is a *result*, not noise to
//!   be hidden - on a VPS it is often the most interesting number on the page.
//! * **Bootstrap-free CI**: with n in the 5..15 range we report a
//!   median-rank-based non-parametric interval, which makes no normality
//!   assumption. For n < 6 no interval is reported rather than reporting a
//!   meaningless one.

use serde::{Deserialize, Serialize};

/// Descriptive statistics over a set of repetition samples.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Summary {
    pub n: usize,
    pub min: f64,
    pub max: f64,
    pub mean: f64,
    pub median: f64,
    pub stddev: f64,
    /// Coefficient of variation (stddev / mean). `None` when the mean is zero.
    pub cv: Option<f64>,
    /// Non-parametric ~95% confidence interval for the median, when `n >= 6`.
    pub ci95: Option<(f64, f64)>,
    /// Median absolute deviation: the median of `|value - median|`.
    ///
    /// The robust counterpart to [`Summary::stddev`], and the one
    /// [`Summary::is_unstable`] asks. A standard deviation is moved a long way
    /// by one distant sample; a MAD is not moved at all until half of them
    /// move, which is the property that matters when the number being judged is
    /// a median.
    ///
    /// `None` for fewer than three samples, where a median of deviations says
    /// nothing. Skipped when absent so that adding it left the canonical form -
    /// and therefore the signature - of every bundle written before it
    /// unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mad: Option<f64>,
}

/// Computes descriptive statistics. Returns `None` for an empty sample set;
/// callers must treat that as a module failure, never as a zero score.
impl Summary {
    /// Half-width of the median's confidence interval, relative to the median.
    ///
    /// "How well determined is the number this metric reports?" - which is the
    /// question that matters, because the number it reports is the median.
    /// `None` when there is no interval (`n < 6`) or the median is zero.
    pub fn relative_ci(&self) -> Option<f64> {
        let (lo, hi) = self.ci95?;
        if self.median.abs() <= f64::EPSILON {
            return None;
        }
        Some(((hi - lo) / 2.0 / self.median).abs())
    }

    /// Spread of the bulk of the repetitions, relative to the median, on the
    /// same scale as [`Summary::cv`].
    ///
    /// The `1.4826` is what puts it on that scale: for normally distributed
    /// samples `1.4826 * MAD` estimates the standard deviation, so this and the
    /// coefficient of variation agree on clean data and diverge exactly when
    /// there are outliers. That is what lets one bound be compared against
    /// both, with no second threshold to keep in step.
    ///
    /// `None` before three samples, or when the median is zero.
    pub fn relative_mad(&self) -> Option<f64> {
        let mad = self.mad?;
        if self.median.abs() <= f64::EPSILON {
            return None;
        }
        Some((1.4826 * mad / self.median).abs())
    }

    /// Whether these repetitions were too unsteady for the value to be compared
    /// against another machine's.
    ///
    /// The coefficient of variation alone was answering a different question.
    /// It is `stddev / mean`, so a single slow repetition out of eleven moves it
    /// enormously - while the median the metric actually reports does not move
    /// at all. Measured on the published corpus: `latency_read_4k.p99` came back
    /// with a CV of 137% and a median determined to within 5.3%, and that run
    /// was downgraded to `Partial` on the strength of the 137%.
    ///
    /// So a metric is unstable only when *both* say so: the spread is wide and
    /// the median is genuinely poorly determined. A wide spread around a
    /// well-determined median is a distribution with a tail, which is a fact
    /// about the device or the network and not a reason to refuse the
    /// measurement.
    ///
    /// This is deliberately a narrowing: every metric it clears was already
    /// being flagged, and each is cleared for the stated reason that the
    /// reported value is solid. It never flags anything the CV bound did not.
    ///
    /// The interval needs `n >= 6`, so short profiles fall back to the CV
    /// alone: the old behaviour, applied where there is not enough evidence to
    /// do better rather than everywhere.
    ///
    /// Checked against the corpus in `docs/FIELD-EVIDENCE.md`: it clears
    /// `latency_read_4k.p99` (CV 137%, CI 5.3%) and `throughput.medium`
    /// (CV 23%, CI 1.7%), and still flags `ttfb.mean` on a host where the whole
    /// distribution is wide (CV 53%, CI 56%) rather than one repetition being
    /// slow.
    pub fn is_unstable(&self, bound: f64) -> bool {
        let Some(cv) = self.cv else {
            return false;
        };
        if cv <= bound {
            return false;
        }
        // `relative_mad` first, and the interval only as a fallback for bundles
        // written before the MAD existed.
        //
        // The interval turned out to be a poor robust statistic at the sample
        // counts the profiles actually use. `median_ci95` trims
        // `floor(n/2 - 0.98*sqrt(n))` from each end, which is *zero* at n = 6
        // and n = 7 - so for a `standard` run the interval is min to max, wider
        // than the CV, and clears nothing. It only begins trimming at n = 8 and
        // does real work at the eleven repetitions `deep` uses. Measured:
        // `triad.single` over seven repetitions had a CV of 36%, an interval of
        // 42%, and a relative MAD of 0.3% - one wild sample around a median
        // that barely moved.
        //
        // The MAD needs three samples, so it is available to every profile.
        match self.relative_mad().or_else(|| self.relative_ci()) {
            Some(relative) => relative > bound,
            None => true,
        }
    }
}

pub fn summarize(samples: &[f64]) -> Option<Summary> {
    if samples.is_empty() || samples.iter().any(|v| !v.is_finite()) {
        return None;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

    let n = sorted.len();
    let mean = sorted.iter().sum::<f64>() / n as f64;
    let median = median_of_sorted(&sorted);
    // Sample standard deviation (Bessel-corrected); 0.0 for n == 1.
    let stddev = if n > 1 {
        (sorted.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / (n as f64 - 1.0)).sqrt()
    } else {
        0.0
    };
    let cv = if mean.abs() > f64::EPSILON {
        Some(stddev / mean)
    } else {
        None
    };

    // Median of the absolute deviations from the median. Computed from the
    // already-sorted copy, so this costs one more sort of the same length.
    let mad = if n >= 3 {
        let mut deviations: Vec<f64> = sorted.iter().map(|v| (v - median).abs()).collect();
        deviations.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        Some(median_of_sorted(&deviations))
    } else {
        None
    };

    Some(Summary {
        n,
        min: sorted[0],
        max: sorted[n - 1],
        mean,
        median,
        stddev,
        cv,
        mad,
        ci95: median_ci95(&sorted),
    })
}

fn median_of_sorted(sorted: &[f64]) -> f64 {
    let n = sorted.len();
    if n % 2 == 1 {
        sorted[n / 2]
    } else {
        (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0
    }
}

/// Order-statistic (sign-test) confidence interval for the median.
///
/// The interval is `[x_(k), x_(n+1-k)]` where `k` is the largest index such
/// that the binomial tail probability is <= 0.025. We approximate `k` with the
/// standard normal formula `k = floor(n/2 - 0.98 * sqrt(n))`, valid enough for
/// the n we use; below n = 6 no interval exists at 95% and we return `None`.
fn median_ci95(sorted: &[f64]) -> Option<(f64, f64)> {
    let n = sorted.len();
    if n < 6 {
        return None;
    }
    let nf = n as f64;
    let k = (nf / 2.0 - 0.98 * nf.sqrt()).floor();
    let lo = k.max(0.0) as usize;
    let hi = n.saturating_sub(lo + 1);
    if lo >= hi {
        return None;
    }
    Some((sorted[lo], sorted[hi]))
}

/// Geometric mean of strictly positive values.
///
/// Computed in log space to avoid overflow when combining many normalised
/// ratios. Returns `None` if the slice is empty or contains a non-positive
/// value - a zero-valued sub-score must fail the run, not silently annihilate
/// the aggregate.
pub fn geometric_mean(values: &[f64]) -> Option<f64> {
    // Finiteness is checked first so the `<= 0.0` comparison is only ever
    // applied to a comparable value; `NaN <= 0.0` is false and would otherwise
    // slip a NaN into the logarithm below.
    if values.is_empty() || values.iter().any(|v| !v.is_finite() || *v <= 0.0) {
        return None;
    }
    let log_sum: f64 = values.iter().map(|v| v.ln()).sum();
    Some((log_sum / values.len() as f64).exp())
}

/// Weighted geometric mean. `pairs` is `(value, weight)`; weights must be
/// positive and are normalised internally.
pub fn weighted_geometric_mean(pairs: &[(f64, f64)]) -> Option<f64> {
    if pairs.is_empty() {
        return None;
    }
    let total_weight: f64 = pairs.iter().map(|(_, w)| *w).sum();
    if !total_weight.is_finite() || total_weight <= 0.0 {
        return None;
    }
    let mut log_sum = 0.0;
    for (value, weight) in pairs {
        if !value.is_finite() || *value <= 0.0 || !weight.is_finite() || *weight <= 0.0 {
            return None;
        }
        log_sum += weight * value.ln();
    }
    Some((log_sum / total_weight).exp())
}

/// Modified-Z-score outlier detection based on the median absolute deviation.
///
/// MAD is used instead of stddev because stddev is itself dragged by the
/// outlier it is supposed to detect. Returns the indices judged to be outliers
/// at `threshold` (3.5 is the conventional value).
///
/// DARCBench **flags** outliers; it does not silently delete them. Dropping
/// samples that a hypervisor genuinely produced would turn a noisy-neighbour
/// signal into a clean lie.
pub fn outlier_indices(samples: &[f64], threshold: f64) -> Vec<usize> {
    if samples.len() < 4 {
        return Vec::new();
    }
    let mut sorted = samples.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let med = median_of_sorted(&sorted);

    let mut deviations: Vec<f64> = samples.iter().map(|v| (v - med).abs()).collect();
    deviations.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mad = median_of_sorted(&deviations);
    if mad <= f64::EPSILON {
        return Vec::new();
    }
    samples
        .iter()
        .enumerate()
        .filter(|(_, v)| (0.6745 * (*v - med) / mad).abs() > threshold)
        .map(|(i, _)| i)
        .collect()
}

#[cfg(test)]
// In tests, `unwrap`/`expect` panicking *is* the failure signal.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic_in_result_fn)]
mod tests {

    /// A wide spread around a well-determined median is not instability.
    ///
    /// Every case below is taken from the published corpus, so this is a
    /// regression test against measured hardware rather than a constructed
    /// example. See `corpus/2026-08/` and `docs/FIELD-EVIDENCE.md`.
    #[test]
    fn stability_asks_about_the_value_the_metric_reports() {
        // `storage.mixed/latency_read_4k.p99` on the E5-1620 v2: eleven
        // repetitions, two of them slow. The CV came out at 137% and the run
        // was downgraded to `Partial` - while the median it reports was
        // determined to within 5.3%.
        let tail = Summary {
            n: 11,
            min: 0.10,
            max: 4.0,
            mean: 0.55,
            median: 0.19,
            stddev: 0.755,
            cv: Some(1.374),
            // The bulk sits within 0.008 ms of the median: 1.4826 * 0.008 /
            // 0.19 is 6.2%, well under a 20% bound.
            mad: Some(0.008),
            ci95: Some((0.18, 0.20)),
        };
        assert!(tail.cv.expect("cv") > 0.20, "the CV really is that wide");
        assert!(
            tail.relative_ci().expect("ci") < 0.06,
            "and the median really is that solid"
        );
        assert!(
            !tail.is_unstable(0.20),
            "a solid median with a long tail must not be called unstable"
        );

        // `network.transfer/ttfb.mean` on the E-2274G: no single outlier, the
        // whole distribution is wide. That is genuine irreproducibility and
        // must still be caught.
        let spread = Summary {
            n: 11,
            min: 29.31,
            max: 128.87,
            mean: 60.8,
            median: 53.91,
            stddev: 32.30,
            cv: Some(0.531),
            // Wide throughout, not one slow request: 1.4826 * 12.0 / 53.91 is
            // 33%, over a 30% bound, so the MAD agrees with the CV here.
            mad: Some(12.0),
            ci95: Some((23.9, 83.9)),
        };
        assert!(
            spread.relative_ci().expect("ci") > 0.30,
            "the median is genuinely poorly determined"
        );
        assert!(
            spread.is_unstable(0.30),
            "a wide distribution must still be flagged"
        );

        // A steady metric is untouched.
        let steady = Summary {
            n: 11,
            min: 288.9,
            max: 289.6,
            mean: 289.0,
            median: 289.0,
            stddev: 0.30,
            cv: Some(0.001),
            mad: Some(0.20),
            ci95: Some((288.9, 289.1)),
        };
        assert!(!steady.is_unstable(0.20));
    }

    /// The MAD is what makes the rule work at the sample counts really used.
    ///
    /// The confidence interval it replaced trims
    /// `floor(n/2 - 0.98*sqrt(n))` from each end, which is zero at `n = 6` and
    /// `n = 7`. A `standard` run measures seven repetitions, so its interval was
    /// min-to-max - wider than the CV, clearing nothing - and a `quick` run's
    /// five got no interval at all. The rule only did real work on `deep`.
    ///
    /// Every case here is measured, from five runs on one host.
    #[test]
    fn the_robust_spread_is_available_to_every_profile() {
        // `memory.bandwidth/triad.single`, a `standard` run: seven repetitions,
        // one of them wild. CV 36.0%, interval 42.1%, relative MAD 0.3%.
        let one_wild = summarize(&[
            12_398.0, 12_401.0, 12_405.0, 12_410.0, 12_412.0, 12_418.0, 30_100.0,
        ])
        .expect("summary");
        assert_eq!(one_wild.n, 7);
        assert!(
            one_wild.cv.expect("cv") > 0.30,
            "the CV really is that wide"
        );
        assert!(
            one_wild.relative_ci().expect("interval") >= one_wild.cv.expect("cv"),
            "and at n = 7 the interval is no help: it spans min to max"
        );
        assert!(
            one_wild.relative_mad().expect("mad") < 0.02,
            "while the bulk of the repetitions barely moved"
        );
        assert!(
            !one_wild.is_unstable(0.15),
            "one wild repetition around a steady median is not instability"
        );

        // Five repetitions - a `quick` run - get no interval at all, and the
        // MAD still works.
        let short = summarize(&[100.0, 101.0, 99.0, 100.5, 260.0]).expect("summary");
        assert!(short.relative_ci().is_none(), "no interval below n = 6");
        assert!(short.relative_mad().expect("mad") < 0.05);
        assert!(!short.is_unstable(0.15));

        // Genuinely spread stays flagged. Shaped after
        // `memory.bandwidth/sequential_write.multi`, which over eleven
        // repetitions had a CV of 17.2% and a relative MAD of 19.0%: no
        // outlier, the whole distribution is wide, and both measures say so.
        let spread = summarize(&[
            14_937.0, 15_850.0, 16_762.0, 17_675.0, 18_587.0, 19_500.0, 20_413.0, 21_325.0,
            22_238.0, 23_150.0, 24_063.0,
        ])
        .expect("summary");
        assert!(
            spread.cv.expect("cv") > 0.15,
            "the constructed spread must clear the bound the assertion uses"
        );
        assert!(spread.relative_mad().expect("mad") > 0.15);
        assert!(
            spread.is_unstable(0.15),
            "a distribution that is wide throughout must still be flagged"
        );
    }

    /// On clean data the two measures agree, which is what lets one bound judge
    /// both.
    ///
    /// `1.4826 * MAD` estimates the standard deviation for normal samples, so a
    /// steady metric is not quietly given a looser test than it had before.
    #[test]
    fn the_robust_spread_and_the_coefficient_of_variation_agree_on_clean_data() {
        // Symmetric, no outliers, spread about 10% of the median.
        let clean = summarize(&[90.0, 95.0, 97.0, 100.0, 103.0, 105.0, 110.0]).expect("summary");
        let cv = clean.cv.expect("cv");
        let mad = clean.relative_mad().expect("mad");
        assert!(
            (cv - mad).abs() < 0.05,
            "cv {cv} and relative mad {mad} should be close on clean data"
        );
        assert!(clean.is_unstable(0.02), "both are over a tight bound");
        assert!(!clean.is_unstable(0.30), "and both under a loose one");
    }

    /// Below `n = 6` there is no interval, so the CV decides alone.
    ///
    /// The old behaviour, kept where there is not enough evidence to do better
    /// rather than kept everywhere. A short profile must not become *harder* to
    /// flag than a long one just because it collected fewer repetitions.
    #[test]
    fn without_an_interval_the_coefficient_of_variation_still_decides() {
        let short = Summary {
            n: 5,
            min: 1.0,
            max: 9.0,
            mean: 4.0,
            median: 3.0,
            stddev: 3.2,
            cv: Some(0.80),
            // Samples 1, 9, 2, 8, 3: median 3, deviations 2, 6, 1, 5, 0, so the
            // MAD is 2 and the relative MAD ~99%. Spread throughout, not one
            // outlier - correctly still unstable.
            mad: Some(2.0),
            ci95: None,
        };
        assert!(short.relative_ci().is_none());
        assert!(
            short.is_unstable(0.20),
            "no interval means the CV is all there is, and it is over the bound"
        );

        // A metric with no CV at all - a zero mean - is not evidence of
        // instability either way.
        let empty = Summary {
            n: 11,
            min: 0.0,
            max: 0.0,
            mean: 0.0,
            median: 0.0,
            stddev: 0.0,
            cv: None,
            mad: None,
            ci95: None,
        };
        assert!(!empty.is_unstable(0.20));
    }

    /// The rule may only ever remove flags, never add one.
    ///
    /// This is what makes it safe to apply to eight modules and the validator
    /// at once: it is a narrowing of an existing bound, so no run that passed
    /// before can start failing.
    #[test]
    fn the_rule_is_strictly_a_narrowing_of_the_coefficient_of_variation_bound() {
        let mut checked = 0;
        for n in [5usize, 6, 11, 30] {
            for spread in [0.0, 0.05, 0.2, 0.9, 3.0] {
                for width in [0.0, 0.01, 0.5, 2.0] {
                    let median = 10.0;
                    let summary = Summary {
                        n,
                        min: median - spread * median,
                        max: median + spread * median,
                        mean: median,
                        median,
                        stddev: spread * median,
                        cv: Some(spread),
                        // Swept independently of the CV so the narrowing
                        // property is tested where they disagree, which is the
                        // only case that can go wrong.
                        mad: Some(width * median / 1.4826),
                        ci95: if n >= 6 {
                            Some((median - width * median, median + width * median))
                        } else {
                            None
                        },
                    };
                    for bound in [0.15, 0.20, 0.30] {
                        let cv_alone = summary.cv.is_some_and(|cv| cv > bound);
                        if summary.is_unstable(bound) {
                            assert!(
                                cv_alone,
                                "flagged something the CV bound did not: n={n} \
                                 spread={spread} width={width} bound={bound}"
                            );
                        }
                        checked += 1;
                    }
                }
            }
        }
        assert!(checked > 100, "the sweep must actually cover something");
    }
    use super::*;

    fn approx(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-9, "{a} != {b}");
    }

    #[test]
    fn summary_basic() {
        let s = summarize(&[10.0, 12.0, 11.0, 13.0, 9.0]).expect("summary");
        assert_eq!(s.n, 5);
        approx(s.median, 11.0);
        approx(s.mean, 11.0);
        approx(s.min, 9.0);
        approx(s.max, 13.0);
        // sample stddev of {9,10,11,12,13} = sqrt(10/4) = sqrt(2.5)
        approx(s.stddev, 2.5f64.sqrt());
        assert!(
            s.ci95.is_none(),
            "n=5 must not produce a 95% median interval"
        );
    }

    #[test]
    fn summary_even_count_median() {
        let s = summarize(&[1.0, 2.0, 3.0, 4.0]).expect("summary");
        approx(s.median, 2.5);
    }

    #[test]
    fn summary_rejects_empty_and_nan() {
        assert!(summarize(&[]).is_none());
        assert!(summarize(&[1.0, f64::NAN]).is_none());
        assert!(summarize(&[1.0, f64::INFINITY]).is_none());
    }

    #[test]
    fn ci_appears_at_n6() {
        let s = summarize(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]).expect("summary");
        assert!(s.ci95.is_some());
    }

    #[test]
    fn geometric_mean_matches_known_value() {
        approx(geometric_mean(&[1.0, 4.0]).expect("gm"), 2.0);
        approx(geometric_mean(&[2.0, 8.0]).expect("gm"), 4.0);
        assert!(geometric_mean(&[1.0, 0.0]).is_none());
        assert!(geometric_mean(&[]).is_none());
    }

    #[test]
    fn geometric_mean_punishes_a_single_catastrophic_subsystem() {
        // This is the core scoring property: one near-zero subsystem must drag
        // the aggregate down hard, which an arithmetic mean would not do.
        let balanced = geometric_mean(&[1.0, 1.0, 1.0, 1.0]).expect("gm");
        let lopsided = geometric_mean(&[3.9, 0.1, 1.0, 1.0]).expect("gm");
        assert!(lopsided < balanced, "{lopsided} should be below {balanced}");
    }

    #[test]
    fn weighted_geometric_mean_reduces_to_unweighted() {
        let plain = geometric_mean(&[2.0, 8.0]).expect("gm");
        let weighted = weighted_geometric_mean(&[(2.0, 1.0), (8.0, 1.0)]).expect("wgm");
        approx(plain, weighted);
    }

    #[test]
    fn aggregation_rejects_nan_rather_than_propagating_it() {
        // NaN compares false against every ordering operator, so a naive
        // `value > 0.0` guard would let it through and poison the logarithm.
        assert!(geometric_mean(&[1.0, f64::NAN]).is_none());
        assert!(weighted_geometric_mean(&[(1.0, 1.0), (f64::NAN, 1.0)]).is_none());
        assert!(weighted_geometric_mean(&[(1.0, f64::NAN)]).is_none());
        assert!(weighted_geometric_mean(&[(1.0, f64::INFINITY)]).is_none());
        assert!(weighted_geometric_mean(&[(1.0, -1.0)]).is_none());
    }

    #[test]
    fn weighted_geometric_mean_respects_weights() {
        let w = weighted_geometric_mean(&[(1.0, 9.0), (100.0, 1.0)]).expect("wgm");
        assert!(w < 2.0, "heavy weight on 1.0 should dominate, got {w}");
    }

    #[test]
    fn outliers_detected_but_not_removed() {
        let samples = [100.0, 101.0, 99.0, 100.5, 12.0, 100.2];
        let idx = outlier_indices(&samples, 3.5);
        assert_eq!(idx, vec![4]);
        // Summary still contains all six samples.
        assert_eq!(summarize(&samples).expect("s").n, 6);
    }

    #[test]
    fn outliers_none_when_stable() {
        assert!(outlier_indices(&[10.0, 10.1, 9.9, 10.05, 10.02], 3.5).is_empty());
    }
}
