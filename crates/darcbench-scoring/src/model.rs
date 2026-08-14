//! Aggregation: raw metrics -> normalised ratios -> module, facet, category,
//! composite and total scores.

use std::collections::BTreeMap;

use darcbench_protocol::metrics::{ModuleStatus, WarningCode};
use darcbench_protocol::stats::{geometric_mean, weighted_geometric_mean};
use darcbench_protocol::{Direction, ModuleResult, Profile};
use serde::{Deserialize, Serialize};

use crate::reference::{provisional_reference, Facet, ReferenceProfile};
use crate::SCORING_MODEL_VERSION;

/// Score assigned to DARC-REF-1 in every category and in the total.
///
/// 1000 was chosen over a larger anchor (Geekbench-style 2500, PassMark-style
/// five figures) because it makes the ratio legible without a lookup table: a
/// 2400 machine is 2.4x the reference, full stop. A large anchor implies a
/// precision this model does not have.
pub const REFERENCE_ANCHOR: f64 = 1000.0;

/// Top-level score categories.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CategoryKey {
    Compute,
    Memory,
    Storage,
    Network,
    Web,
    Database,
    Deployment,
}

impl CategoryKey {
    pub const ALL: [CategoryKey; 7] = [
        Self::Compute,
        Self::Memory,
        Self::Storage,
        Self::Network,
        Self::Web,
        Self::Database,
        Self::Deployment,
    ];

    pub fn key(self) -> &'static str {
        match self {
            Self::Compute => "compute",
            Self::Memory => "memory",
            Self::Storage => "storage",
            Self::Network => "network",
            Self::Web => "web",
            Self::Database => "database",
            Self::Deployment => "deployment",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Compute => "Compute Score",
            Self::Memory => "Memory Score",
            Self::Storage => "Storage Score",
            Self::Network => "Network Score",
            Self::Web => "Web Score",
            Self::Database => "Database Score",
            Self::Deployment => "Deployment Score",
        }
    }

    /// Weight inside the DARCBench Total Score for the standard profile.
    ///
    /// Rationale (`docs/SCORING-SYSTEM.md`): the total is meant to predict how
    /// a machine feels as a *web hosting server*, so storage and the web
    /// workloads together outweigh raw compute, and network is deliberately
    /// capped at 8% so a 10 Gbit/s port cannot buy a good score for a machine
    /// with a slow disk.
    pub fn standard_weight(self) -> f64 {
        match self {
            Self::Compute => 0.26,
            Self::Memory => 0.12,
            Self::Storage => 0.20,
            Self::Network => 0.08,
            Self::Web => 0.18,
            Self::Database => 0.12,
            Self::Deployment => 0.04,
        }
    }

    /// Categories a standard, rankable total score requires.
    pub fn required_for_standard_total() -> [CategoryKey; 5] {
        [
            Self::Compute,
            Self::Memory,
            Self::Storage,
            Self::Network,
            Self::Web,
        ]
    }
}

/// A workload-oriented composite: a re-weighting of the same category scores
/// for a specific way of using a server.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompositeKey {
    WordpressHosting,
    PhpCommerce,
    NodeNextjs,
    DatabaseServer,
    StaticMediaServer,
    BuildServer,
    GeneralPurposeVps,
}

impl CompositeKey {
    pub const ALL: [CompositeKey; 7] = [
        Self::WordpressHosting,
        Self::PhpCommerce,
        Self::NodeNextjs,
        Self::DatabaseServer,
        Self::StaticMediaServer,
        Self::BuildServer,
        Self::GeneralPurposeVps,
    ];

    pub fn key(self) -> &'static str {
        match self {
            Self::WordpressHosting => "wordpress_hosting",
            Self::PhpCommerce => "php_commerce",
            Self::NodeNextjs => "node_nextjs",
            Self::DatabaseServer => "database_server",
            Self::StaticMediaServer => "static_media_server",
            Self::BuildServer => "build_server",
            Self::GeneralPurposeVps => "general_purpose_vps",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::WordpressHosting => "WordPress Hosting",
            Self::PhpCommerce => "PHP Commerce",
            Self::NodeNextjs => "Node / Next.js",
            Self::DatabaseServer => "Database Server",
            Self::StaticMediaServer => "Static / Media Server",
            Self::BuildServer => "Build Server",
            Self::GeneralPurposeVps => "General Purpose VPS",
        }
    }

    /// `(category, weight)` pairs. Weights sum to 1.0 per composite.
    pub fn weights(self) -> &'static [(CategoryKey, f64)] {
        use CategoryKey as C;
        match self {
            Self::WordpressHosting => &[
                (C::Compute, 0.20),
                (C::Storage, 0.20),
                (C::Web, 0.30),
                (C::Database, 0.25),
                (C::Memory, 0.05),
            ],
            Self::PhpCommerce => &[
                (C::Compute, 0.25),
                (C::Storage, 0.15),
                (C::Web, 0.25),
                (C::Database, 0.30),
                (C::Memory, 0.05),
            ],
            Self::NodeNextjs => &[
                (C::Compute, 0.40),
                (C::Memory, 0.15),
                (C::Web, 0.30),
                (C::Storage, 0.10),
                (C::Deployment, 0.05),
            ],
            Self::DatabaseServer => &[
                (C::Storage, 0.35),
                (C::Database, 0.35),
                (C::Memory, 0.20),
                (C::Compute, 0.10),
            ],
            Self::StaticMediaServer => &[
                (C::Network, 0.40),
                (C::Storage, 0.30),
                (C::Web, 0.20),
                (C::Compute, 0.10),
            ],
            Self::BuildServer => &[
                (C::Compute, 0.45),
                (C::Storage, 0.25),
                (C::Memory, 0.20),
                (C::Deployment, 0.10),
            ],
            Self::GeneralPurposeVps => &[
                (C::Compute, 0.30),
                (C::Memory, 0.15),
                (C::Storage, 0.25),
                (C::Network, 0.15),
                (C::Web, 0.15),
            ],
        }
    }
}

/// A configured, versioned scoring model.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScoringModel {
    pub version: String,
    pub reference: ReferenceProfile,
    /// Worst-case multiplicative penalty applied to the total for an unstable
    /// machine. 0.10 means a maximally unstable machine loses 10% of its total.
    pub stability_penalty_span: f64,
    /// Coefficient of variation at which the stability index reaches 0.
    pub cv_ceiling: f64,
    /// Weak-link cap factor: the total may not exceed this multiple of the
    /// weakest measured category. See [`apply_weak_link_cap`].
    pub weak_link_cap_factor: f64,
}

impl ScoringModel {
    /// The model implemented by this build.
    pub fn current() -> Self {
        Self {
            version: SCORING_MODEL_VERSION.to_string(),
            reference: provisional_reference(),
            stability_penalty_span: 0.10,
            cv_ceiling: 0.20,
            weak_link_cap_factor: 4.0,
        }
    }

    /// Scores a run.
    ///
    /// Pure: identical inputs always yield identical outputs, which is what
    /// makes server-side score recomputation a usable anti-tamper check.
    pub fn score_run(&self, profile: Profile, modules: &[ModuleResult]) -> ScoreCard {
        // `(ratio, weight)` pairs bucketed as they are produced. The previous
        // shape - one flat list, then a filtering pass per category and per
        // facet - walked every metric nine times and kept a formatted key for
        // each one that nothing ever read.
        let mut by_category: BTreeMap<CategoryKey, Vec<(f64, f64)>> = BTreeMap::new();
        let mut by_facet: BTreeMap<Facet, Vec<(f64, f64)>> = BTreeMap::new();
        let mut unreferenced: Vec<String> = Vec::new();
        let mut cvs: Vec<f64> = Vec::new();
        let mut instability_flags = 0usize;

        // A cycling profile is scored from its **last complete cycle**, and the
        // decline is published separately as retention. Averaging a machine's
        // burst throughput together with its post-throttling throughput
        // produces a number that describes neither of them, and the one an
        // operator has to live with is the second. For every other profile
        // there is exactly one cycle and this selects all of it, which is why
        // nothing below had to change.
        let sustained = crate::sustained::analyse(modules);
        let scored_cycle = crate::sustained::scoring_cycle(modules);

        for module in modules {
            // A failed or cancelled module contributes nothing. Its metrics are
            // still retained in the bundle as evidence; they just cannot be
            // scored, because a partial workload is not the workload.
            if !matches!(
                module.status,
                ModuleStatus::Completed | ModuleStatus::Degraded
            ) {
                continue;
            }
            if module.cycle != scored_cycle {
                continue;
            }
            let module_id = module.module.id.as_str();
            for metric in &module.metrics {
                let Some(reference) = self.reference.get(module_id, &metric.key) else {
                    unreferenced.push(format!("{module_id}/{}", metric.key));
                    continue;
                };
                if let Some(cv) = metric.summary.cv {
                    cvs.push(cv);
                }
                // The **anchor's** direction, not the metric's. The bundle is
                // the operator's to write; the reference profile is this
                // scoring model's own, and it is what the server recomputes
                // from. Trusting `metric.direction` meant a bundle could
                // relabel `latency_fsync.mean` as higher-is-better and have a
                // 5 ms fsync normalise to 100x instead of 0.01 - reproduced
                // exactly by the server, because it read the same tampered
                // field, so recomputation agreed and the run validated. It also
                // let a merely *buggy* agent score a mislabelled metric upside
                // down with nothing to catch it.
                let Some(ratio) = normalise(metric.value, reference.value, reference.direction)
                else {
                    unreferenced.push(format!("{module_id}/{}", metric.key));
                    continue;
                };
                by_category
                    .entry(reference.category)
                    .or_default()
                    .push((ratio, reference.weight));
                if let Some(facet) = reference.facet {
                    by_facet
                        .entry(facet)
                        .or_default()
                        .push((ratio, reference.weight));
                }
            }
            instability_flags += module
                .warnings
                .iter()
                .filter(|w| {
                    matches!(
                        w.code,
                        WarningCode::HighVariance
                            | WarningCode::StealTimeObserved
                            | WarningCode::ThermalThrottle
                            | WarningCode::FrequencyDrop
                            // The one signal here that is actually raised at
                            // runtime today. Without it a run contaminated by
                            // a co-resident service throughout would be
                            // rendered as "no instability flags" while every
                            // module it touched was degraded.
                            | WarningCode::ExternalLoad
                    )
                })
                .count();
        }

        // --- categories --------------------------------------------------
        // Emitted in `CategoryKey::ALL` order rather than map order, so the
        // card's category list stays stable regardless of the order metrics
        // happened to arrive in. Recomputation compares it positionally.
        let mut categories: Vec<CategoryOutcome> = Vec::new();
        for key in CategoryKey::ALL {
            let Some(pairs) = by_category.get(&key) else {
                continue;
            };
            if pairs.is_empty() {
                continue;
            }
            if let Some(gm) = weighted_geometric_mean(pairs) {
                categories.push(CategoryOutcome {
                    key,
                    label: key.label().to_string(),
                    score: gm * REFERENCE_ANCHOR,
                    weight: key.standard_weight(),
                    metric_count: pairs.len(),
                });
            }
        }

        // --- facets ------------------------------------------------------
        let mut facets: BTreeMap<String, f64> = BTreeMap::new();
        for facet in [Facet::SingleCore, Facet::MultiCore] {
            let Some(pairs) = by_facet.get(&facet) else {
                continue;
            };
            if let Some(gm) = weighted_geometric_mean(pairs) {
                facets.insert(facet.key().to_string(), gm * REFERENCE_ANCHOR);
            }
        }

        // --- stability ---------------------------------------------------
        let median_cv = median(&mut cvs);
        let stability_index = match median_cv {
            Some(cv) => (1.0 - (cv / self.cv_ceiling)).clamp(0.0, 1.0),
            // No variance information at all is not evidence of stability.
            None => 0.5,
        };
        let stability_score = stability_index * REFERENCE_ANCHOR;
        let stability_multiplier =
            (1.0 - self.stability_penalty_span) + self.stability_penalty_span * stability_index;

        // --- total -------------------------------------------------------
        let present: BTreeMap<CategoryKey, f64> =
            categories.iter().map(|c| (c.key, c.score)).collect();
        let missing_required: Vec<CategoryKey> = CategoryKey::required_for_standard_total()
            .into_iter()
            .filter(|k| !present.contains_key(k))
            .collect();

        // The total is computed over the categories that are present. When
        // required categories are missing it is still shown (a partial run is
        // useful) but `total_is_standard` is false and the run must be reported
        // as Partial rather than ranked.
        let total_pairs: Vec<(f64, f64)> = categories
            .iter()
            .map(|c| (c.score / REFERENCE_ANCHOR, c.weight))
            .collect();
        let uncapped_total = weighted_geometric_mean(&total_pairs)
            .map(|gm| gm * REFERENCE_ANCHOR * stability_multiplier);
        let category_scores: Vec<f64> = categories.iter().map(|c| c.score).collect();
        let total = uncapped_total
            .map(|t| apply_weak_link_cap(t, &category_scores, self.weak_link_cap_factor));
        let weak_link_applied = matches!(
            (uncapped_total, total),
            (Some(u), Some(t)) if t < u - 1e-9
        );
        let balance_index = weakest(&category_scores).and_then(|min| {
            geometric_mean(&category_scores).map(|gm| if gm > 0.0 { min / gm } else { 0.0 })
        });

        // --- composites --------------------------------------------------
        let composites = CompositeKey::ALL
            .into_iter()
            .filter_map(|composite| {
                let pairs: Vec<(f64, f64)> = composite
                    .weights()
                    .iter()
                    .filter_map(|(cat, w)| present.get(cat).map(|s| (*s / REFERENCE_ANCHOR, *w)))
                    .collect();
                let covered: f64 = composite
                    .weights()
                    .iter()
                    .filter(|(cat, _)| present.contains_key(cat))
                    .map(|(_, w)| *w)
                    .sum();
                // Refuse to publish a workload composite from a fragment of its
                // inputs; below 60% weight coverage the number is noise.
                if covered < 0.60 {
                    return None;
                }
                weighted_geometric_mean(&pairs).map(|gm| CompositeOutcome {
                    key: composite,
                    label: composite.label().to_string(),
                    score: gm * REFERENCE_ANCHOR,
                    weight_coverage: covered,
                })
            })
            .collect();

        // --- efficiency ---------------------------------------------------
        // Performance per logical CPU, expressed against the reference's own
        // per-thread performance. This is what separates a 4-thread
        // high-frequency machine from a 32-thread machine that is merely wide.
        let efficiency = match (facets.get("multi_core"), facets.get("single_core")) {
            (Some(multi), Some(single)) if *single > 0.0 => {
                Some(geometric_mean(&[*multi, *single]).unwrap_or(*single))
            }
            _ => None,
        };

        ScoreCard {
            scoring_model: self.version.clone(),
            reference_profile: self.reference.name.clone(),
            uncalibrated: !self.reference.calibrated,
            profile,
            total,
            total_is_standard: missing_required.is_empty() && profile.is_standard(),
            categories,
            facets,
            composites,
            stability_score,
            stability_index,
            stability_multiplier,
            uncapped_total,
            weak_link_applied,
            balance_index,
            efficiency_score: efficiency,
            median_cv,
            instability_flags,
            missing_required_categories: missing_required,
            unreferenced_metrics: unreferenced,
            sustained,
        }
    }
}

/// Turns a raw value into a dimensionless ratio against the reference.
///
/// Lower-is-better metrics are inverted exactly once, here. No other code in
/// the crate is allowed to look at [`Direction`], which is what prevents the
/// classic double-inversion bug.
fn normalise(value: f64, reference: f64, direction: Direction) -> Option<f64> {
    if !value.is_finite() || !reference.is_finite() || reference <= 0.0 {
        return None;
    }
    let ratio = match direction {
        Direction::HigherIsBetter => value / reference,
        Direction::LowerIsBetter => {
            if value <= 0.0 {
                // A zero-latency measurement is a broken measurement, not an
                // infinitely fast machine.
                return None;
            }
            reference / value
        }
    };
    (ratio.is_finite() && ratio > 0.0).then_some(ratio)
}

/// Caps the total at `factor` times the weakest measured category.
///
/// # Why a weighted geometric mean is not enough
///
/// A geometric mean penalises imbalance, but only logarithmically. With the
/// standard weights, a machine at 4x reference in compute, memory, network and
/// web but at 0.02x in storage still aggregates to ~1.13x reference - i.e. a
/// server whose disk is fifty times slower than a normal one would be reported
/// as *above average*. That is precisely the failure mode the scoring
/// requirements forbid, and it is not a hypothetical: a cloud instance whose
/// burst credits are exhausted, or one on a degraded network-attached volume,
/// looks exactly like this.
///
/// The cap encodes an Amdahl-style observation: real server workloads are
/// pipelines, so a subsystem several times slower than the rest of the machine
/// dominates end-to-end time no matter how fast the other parts are.
///
/// `factor = 4.0` means "the machine as a whole may be claimed to be at most
/// four times as good as its worst measured part". It is high enough never to
/// touch a merely uneven machine (a 2x spread is untouched) and low enough to
/// stop a catastrophic one.
///
/// The cap is *reported*, never hidden: [`ScoreCard::uncapped_total`],
/// [`ScoreCard::weak_link_applied`] and [`ScoreCard::balance_index`] let a
/// reader see exactly what happened and why.
pub fn apply_weak_link_cap(total: f64, category_scores: &[f64], factor: f64) -> f64 {
    match weakest(category_scores) {
        Some(min) if factor > 0.0 => total.min(min * factor),
        _ => total,
    }
}

fn weakest(scores: &[f64]) -> Option<f64> {
    scores
        .iter()
        .copied()
        .filter(|v| v.is_finite())
        .fold(None, |acc: Option<f64>, v| {
            Some(match acc {
                Some(cur) => cur.min(v),
                None => v,
            })
        })
}

fn median(values: &mut [f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = values.len();
    Some(if n % 2 == 1 {
        values[n / 2]
    } else {
        (values[n / 2 - 1] + values[n / 2]) / 2.0
    })
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CategoryOutcome {
    pub key: CategoryKey,
    pub label: String,
    pub score: f64,
    pub weight: f64,
    pub metric_count: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CompositeOutcome {
    pub key: CompositeKey,
    pub label: String,
    pub score: f64,
    /// Fraction of the composite's declared weight actually backed by data.
    pub weight_coverage: f64,
}

/// The complete, recomputable output of the scoring model for one run.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScoreCard {
    pub scoring_model: String,
    pub reference_profile: String,
    /// True while the reference profile is uncalibrated. Consumers MUST render
    /// this prominently rather than presenting the numbers as final.
    pub uncalibrated: bool,
    pub profile: Profile,
    pub total: Option<f64>,
    /// False when the total was computed from an incomplete category set or a
    /// non-standard profile. Such a total is informative but never rankable.
    pub total_is_standard: bool,
    pub categories: Vec<CategoryOutcome>,
    /// `single_core` / `multi_core`.
    pub facets: BTreeMap<String, f64>,
    pub composites: Vec<CompositeOutcome>,
    pub stability_score: f64,
    pub stability_index: f64,
    pub stability_multiplier: f64,
    /// The total before the weak-link cap. Published so the cap is auditable.
    pub uncapped_total: Option<f64>,
    /// True when the weak-link cap actually reduced the total.
    pub weak_link_applied: bool,
    /// Weakest category divided by the geometric mean of all categories, in
    /// `(0, 1]`. 1.0 is a perfectly balanced machine.
    pub balance_index: Option<f64>,
    pub efficiency_score: Option<f64>,
    pub median_cv: Option<f64>,
    pub instability_flags: usize,
    pub missing_required_categories: Vec<CategoryKey>,
    /// Metrics that had no reference anchor. Surfaced instead of dropped, so a
    /// module that silently stopped contributing is visible.
    pub unreferenced_metrics: Vec<String>,
    /// How much of its opening performance the machine still had at the end.
    ///
    /// `None` unless the run cycled, which today means the `endurance` profile.
    /// Absent rather than 1.0: a run that was never given time to decline has
    /// not demonstrated that it would not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sustained: Option<crate::sustained::SustainedOutcome>,
}

impl ScoreCard {
    pub fn category(&self, key: CategoryKey) -> Option<f64> {
        self.categories
            .iter()
            .find(|c| c.key == key)
            .map(|c| c.score)
    }

    /// Sustained Performance Score, when the run measured one.
    pub fn sustained_score(&self) -> Option<f64> {
        self.sustained.as_ref().map(|s| s.score)
    }
}

#[cfg(test)]
// In tests, `unwrap`/`expect` panicking *is* the failure signal.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic_in_result_fn)]
mod tests {
    use super::*;
    use darcbench_protocol::metrics::{Metric, MetricSample};
    use darcbench_protocol::stats::summarize;
    use darcbench_protocol::{ModuleId, ModuleRef};

    fn metric(key: &str, value: f64, samples: &[f64]) -> Metric {
        Metric {
            key: key.to_string(),
            label: key.to_string(),
            unit: "u".into(),
            direction: Direction::HigherIsBetter,
            value,
            summary: summarize(samples).expect("summary"),
            samples: samples
                .iter()
                .enumerate()
                .map(|(i, v)| MetricSample {
                    rep: i as u32,
                    value: *v,
                    duration_ms: 100.0,
                    warmup: false,
                })
                .collect(),
            outliers: vec![],
        }
    }

    /// Anchors belonging to one module, with the module prefix stripped.
    ///
    /// Fixtures must filter: the reference profile carries anchors for every
    /// implemented module, and a `cpu.mixed` result built from all of them
    /// would carry metric keys no `cpu.mixed` anchor can match, quietly
    /// inflating `unreferenced_metrics`.
    fn anchors_for(module: &str) -> Vec<(String, crate::reference::ReferencePoint)> {
        let prefix = format!("{module}/");
        provisional_reference()
            .points
            .into_iter()
            .filter_map(|(key, point)| {
                key.strip_prefix(&prefix)
                    .map(|stripped| (stripped.to_string(), point))
            })
            .collect()
    }

    /// Builds a `cpu.mixed` result whose every metric is `factor` x reference.
    fn cpu_result_at(factor: f64, cv: f64) -> ModuleResult {
        let now = chrono::Utc::now();
        let metrics = anchors_for("cpu.mixed")
            .into_iter()
            .map(|(key, p)| {
                let v = p.value * factor;
                // Two samples straddling the mean produce the requested CV.
                let samples = [v * (1.0 - cv), v * (1.0 + cv), v, v, v, v];
                metric(&key, v, &samples)
            })
            .collect();
        ModuleResult {
            module: ModuleRef {
                id: ModuleId::new("cpu.mixed").expect("id"),
                version: "1.0.0".into(),
            },
            status: ModuleStatus::Completed,
            cycle: 0,
            started_at: now,
            finished_at: now,
            duration_ms: 1000.0,
            metrics,
            warnings: vec![],
            error: None,
            context: Default::default(),
        }
    }

    #[test]
    fn reference_performance_scores_the_anchor() {
        let model = ScoringModel::current();
        let card = model.score_run(Profile::Quick, &[cpu_result_at(1.0, 0.0)]);
        let compute = card.category(CategoryKey::Compute).expect("compute");
        assert!(
            (compute - REFERENCE_ANCHOR).abs() < 1e-6,
            "a machine matching DARC-REF-1 must score exactly {REFERENCE_ANCHOR}, got {compute}"
        );
        assert_eq!(
            card.facets.get("single_core").map(|v| v.round()),
            Some(1000.0)
        );
        assert_eq!(
            card.facets.get("multi_core").map(|v| v.round()),
            Some(1000.0)
        );
    }

    #[test]
    fn scores_are_monotonic_in_performance() {
        let model = ScoringModel::current();
        let slow = model.score_run(Profile::Quick, &[cpu_result_at(0.5, 0.0)]);
        let base = model.score_run(Profile::Quick, &[cpu_result_at(1.0, 0.0)]);
        let fast = model.score_run(Profile::Quick, &[cpu_result_at(2.0, 0.0)]);
        let c = |s: &ScoreCard| s.category(CategoryKey::Compute).expect("compute");
        assert!(c(&slow) < c(&base) && c(&base) < c(&fast));
        assert!(
            (c(&fast) / c(&base) - 2.0).abs() < 1e-6,
            "doubling perf must double the score"
        );
    }

    #[test]
    fn latency_metrics_are_inverted_exactly_once() {
        // Faster (lower) latency must produce a higher ratio.
        let fast = normalise(5.0, 10.0, Direction::LowerIsBetter).expect("ratio");
        let slow = normalise(20.0, 10.0, Direction::LowerIsBetter).expect("ratio");
        assert!(fast > slow, "lower latency must score higher");
        assert!((fast - 2.0).abs() < 1e-12);
        assert!((slow - 0.5).abs() < 1e-12);
        // A nonsensical zero latency is rejected rather than scored infinite.
        assert!(normalise(0.0, 10.0, Direction::LowerIsBetter).is_none());
    }

    #[test]
    fn instability_reduces_the_total_but_is_bounded() {
        let model = ScoringModel::current();
        let stable = model.score_run(Profile::Quick, &[cpu_result_at(1.0, 0.0)]);
        let jittery = model.score_run(Profile::Quick, &[cpu_result_at(1.0, 0.40)]);
        let (a, b) = (stable.total.expect("t"), jittery.total.expect("t"));
        assert!(
            b < a,
            "an unstable machine must not score the same as a stable one"
        );
        assert!(
            b >= a * (1.0 - model.stability_penalty_span) - 1e-9,
            "the stability penalty must stay bounded at {}",
            model.stability_penalty_span
        );
        assert!(jittery.stability_score < stable.stability_score);
    }

    #[test]
    fn partial_runs_are_not_standard() {
        let model = ScoringModel::current();
        let card = model.score_run(Profile::Standard, &[cpu_result_at(1.0, 0.0)]);
        assert!(
            !card.total_is_standard,
            "a compute-only run is not a standard total"
        );
        assert!(card
            .missing_required_categories
            .contains(&CategoryKey::Storage));
        assert!(card
            .missing_required_categories
            .contains(&CategoryKey::Network));
    }

    #[test]
    fn custom_profile_total_is_never_standard() {
        let model = ScoringModel::current();
        let card = model.score_run(Profile::Custom, &[cpu_result_at(1.0, 0.0)]);
        assert!(!card.total_is_standard);
    }

    #[test]
    fn failed_modules_contribute_nothing() {
        let model = ScoringModel::current();
        let mut failed = cpu_result_at(1.0, 0.0);
        failed.status = ModuleStatus::Failed;
        let card = model.score_run(Profile::Quick, &[failed]);
        assert!(card.category(CategoryKey::Compute).is_none());
        assert!(card.total.is_none());
    }

    #[test]
    fn composites_below_coverage_threshold_are_withheld() {
        let model = ScoringModel::current();
        let card = model.score_run(Profile::Quick, &[cpu_result_at(1.0, 0.0)]);
        // Only Compute exists; Build Server is 45% compute-weighted, which is
        // under the 60% coverage floor, so nothing may be published.
        assert!(
            card.composites.is_empty(),
            "composites must not be published from a single category: {:?}",
            card.composites
        );
    }

    #[test]
    fn score_card_is_deterministic() {
        let model = ScoringModel::current();
        let input = [cpu_result_at(1.37, 0.05)];
        let a = model.score_run(Profile::Quick, &input);
        let b = model.score_run(Profile::Quick, &input);
        assert_eq!(
            a, b,
            "scoring must be a pure function for recomputation to work"
        );
    }

    #[test]
    fn unreferenced_metrics_are_reported_not_swallowed() {
        let model = ScoringModel::current();
        let mut result = cpu_result_at(1.0, 0.0);
        result
            .metrics
            .push(metric("brand_new_workload.single", 5.0, &[5.0, 5.0]));
        let card = model.score_run(Profile::Quick, &[result]);
        assert_eq!(
            card.unreferenced_metrics,
            vec!["cpu.mixed/brand_new_workload.single"]
        );
    }

    #[test]
    fn standard_category_weights_sum_to_one() {
        let sum: f64 = CategoryKey::ALL.iter().map(|c| c.standard_weight()).sum();
        assert!(
            (sum - 1.0).abs() < 1e-9,
            "category weights must sum to 1.0, got {sum}"
        );
    }

    #[test]
    fn every_composite_weight_set_sums_to_one() {
        for composite in CompositeKey::ALL {
            let sum: f64 = composite.weights().iter().map(|(_, w)| *w).sum();
            assert!(
                (sum - 1.0).abs() < 1e-9,
                "{} weights sum to {sum}, expected 1.0",
                composite.key()
            );
        }
    }

    // --- weak-link cap ---------------------------------------------------

    /// A scoring model whose reference profile spreads the ten `cpu.mixed`
    /// metrics across the five required categories, so the full aggregation
    /// path (not just a helper) can be exercised with synthetic per-category
    /// performance before the storage/network/web modules exist.
    fn multi_category_model() -> ScoringModel {
        use crate::reference::ReferencePoint;
        let mut model = ScoringModel::current();
        let cats = [
            CategoryKey::Compute,
            CategoryKey::Memory,
            CategoryKey::Storage,
            CategoryKey::Network,
            CategoryKey::Web,
        ];
        // Only the `cpu.mixed` anchors, re-categorised. Including anchors from
        // other modules would produce metric keys the synthetic result cannot
        // match, so half the fixture would silently go unscored.
        let mut points = BTreeMap::new();
        for (i, (key, p)) in anchors_for("cpu.mixed").into_iter().enumerate() {
            points.insert(
                format!("cpu.mixed/{key}"),
                ReferencePoint {
                    value: p.value,
                    direction: p.direction,
                    weight: 1.0,
                    // Two metrics per category, deterministically assigned.
                    category: cats[i % cats.len()],
                    facet: p.facet,
                },
            );
        }
        model.reference.points = points;
        model
    }

    /// Builds a result where each metric is scaled by its category's factor.
    fn result_with_category_factors(
        model: &ScoringModel,
        factors: &[(CategoryKey, f64)],
    ) -> ModuleResult {
        let now = chrono::Utc::now();
        let metrics = model
            .reference
            .points
            .iter()
            .map(|(full_key, p)| {
                let key = full_key.trim_start_matches("cpu.mixed/");
                let factor = factors
                    .iter()
                    .find(|(c, _)| *c == p.category)
                    .map(|(_, f)| *f)
                    .unwrap_or(1.0);
                let v = p.value * factor;
                metric(key, v, &[v, v, v, v, v, v])
            })
            .collect();
        ModuleResult {
            module: ModuleRef {
                id: ModuleId::new("cpu.mixed").expect("id"),
                version: "1.0.0".into(),
            },
            status: ModuleStatus::Completed,
            cycle: 0,
            started_at: now,
            finished_at: now,
            duration_ms: 1000.0,
            metrics,
            warnings: vec![],
            error: None,
            context: Default::default(),
        }
    }

    #[test]
    fn one_catastrophic_category_cannot_be_hidden() {
        use CategoryKey as C;
        let model = multi_category_model();

        // Excellent everywhere, catastrophic storage: exactly the shape a
        // credit-exhausted cloud instance or a degraded network volume takes.
        let crippled = model.score_run(
            Profile::Standard,
            &[result_with_category_factors(
                &model,
                &[
                    (C::Compute, 4.0),
                    (C::Memory, 4.0),
                    (C::Storage, 0.02),
                    (C::Network, 4.0),
                    (C::Web, 4.0),
                ],
            )],
        );
        let reference_machine = model.score_run(
            Profile::Standard,
            &[result_with_category_factors(&model, &[])],
        );

        let bad = crippled.total.expect("total");
        let base = reference_machine.total.expect("total");

        // Without the cap, the weighted geometric mean alone would have put
        // this machine *above* reference. That regression must never return.
        assert!(
            crippled.uncapped_total.expect("uncapped") > base,
            "precondition: the geometric mean alone does hide the weakness \
             ({:?} vs {base})",
            crippled.uncapped_total
        );
        assert!(
            crippled.weak_link_applied,
            "the weak-link cap must have engaged"
        );
        assert!(
            bad < base * 0.15,
            "a machine with 50x-slow storage scored {bad}; reference is {base}"
        );
        assert!(crippled.balance_index.expect("balance") < 0.1);
    }

    #[test]
    fn weak_link_cap_leaves_merely_uneven_machines_alone() {
        use CategoryKey as C;
        let model = multi_category_model();
        // A 2x spread between best and worst is normal, not pathological.
        let card = model.score_run(
            Profile::Standard,
            &[result_with_category_factors(
                &model,
                &[
                    (C::Compute, 2.0),
                    (C::Memory, 1.5),
                    (C::Storage, 1.0),
                    (C::Network, 2.0),
                    (C::Web, 1.5),
                ],
            )],
        );
        assert!(
            !card.weak_link_applied,
            "a 2x spread must not trigger the cap"
        );
        assert_eq!(card.total, card.uncapped_total);
    }

    #[test]
    fn weak_link_cap_is_a_pure_min() {
        assert_eq!(apply_weak_link_cap(1000.0, &[500.0, 4000.0], 4.0), 1000.0);
        assert_eq!(apply_weak_link_cap(1000.0, &[100.0, 4000.0], 4.0), 400.0);
        // No categories: nothing to cap against, total passes through.
        assert_eq!(apply_weak_link_cap(1000.0, &[], 4.0), 1000.0);
    }

    #[test]
    fn balanced_machines_keep_a_balance_index_of_one() {
        let model = multi_category_model();
        let card = model.score_run(
            Profile::Standard,
            &[result_with_category_factors(&model, &[])],
        );
        let bi = card.balance_index.expect("balance");
        assert!(
            (bi - 1.0).abs() < 1e-9,
            "balance index should be 1.0, got {bi}"
        );
    }
}
