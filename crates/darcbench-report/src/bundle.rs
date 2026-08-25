//! The result bundle format (`darcbench.bundle/1`).

use darcbench_inventory::Inventory;
use darcbench_protocol::{
    ModuleRef, Profile, ResultState, RunId, RunState, Verdict, BUNDLE_SCHEMA_VERSION,
    PROTOCOL_VERSION,
};
use darcbench_scoring::ScoreCard;
use serde::{Deserialize, Serialize};

use crate::signing::{AgentKey, Signature, SigningError};

/// Identity of the software that produced a bundle.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BundleMeta {
    pub schema: String,
    pub protocol: String,
    pub agent_version: String,
    /// Target triple the agent was built for. Part of comparability: the same
    /// workload compiled for a different target is not the same workload.
    pub build_target: String,
    /// `release` or `debug`. A debug-built agent must never produce a
    /// comparable score, and validation enforces that.
    pub build_profile: String,
    pub generated_at: chrono::DateTime<chrono::Utc>,
}

impl BundleMeta {
    pub fn new(agent_version: &str) -> Self {
        Self {
            schema: BUNDLE_SCHEMA_VERSION.to_string(),
            protocol: PROTOCOL_VERSION.to_string(),
            agent_version: agent_version.to_string(),
            build_target: format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS),
            build_profile: if cfg!(debug_assertions) {
                "debug"
            } else {
                "release"
            }
            .to_string(),
            generated_at: chrono::Utc::now(),
        }
    }
}

/// The run's own facts.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RunRecord {
    pub run_id: RunId,
    pub profile: Profile,
    pub state: RunState,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub finished_at: chrono::DateTime<chrono::Utc>,
    pub duration_ms: u64,
    pub modules: Vec<ModuleRef>,
    /// Digest of the environment snapshot at run start.
    pub environment_digest: String,
    /// Digest over the full ordered event stream, so a consumer can prove the
    /// events they were shown match the bundle they were given.
    pub events_digest: String,
    pub event_count: u64,
    /// Why the watchdog stopped the run early, if it did.
    ///
    /// A run that ends short is `Cancelled` and `Interrupted` either way, and
    /// without this a watchdog abort would be indistinguishable from an
    /// operator pressing stop. The two mean entirely different things to
    /// whoever reads the result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stopped_because: Option<String>,
    /// Safety guards that could not be enforced on this host, named.
    ///
    /// A guard that is armed and never fires and a guard that was never armed
    /// produce identical bundles otherwise, and they are not the same claim.
    /// The runtime load ceiling is the first entry: inside a container without
    /// a namespaced `/proc`, `/proc/stat` describes the host, so enforcing it
    /// would abort a correctly-behaving run for other tenants' work.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub guards_not_enforced: Vec<String>,
    /// Comparability facts a module declared and this bundle does not carry.
    ///
    /// Each entry is `module_id/key`. Empty is the healthy state and the
    /// common one.
    ///
    /// # Why this is recorded rather than assumed
    ///
    /// Every module's manifest carries a `comparability` list: the facts that
    /// must match for two of its results to mean the same thing - a PHP
    /// version, a scale factor, a storage driver. The list is how a reader, or
    /// a comparison, knows when a difference is the machine and when it is the
    /// measurement.
    ///
    /// It was documentation that nothing checked. An audit of a live bundle
    /// found that most declared keys resolved to nothing at all:
    /// `cpu.mixed` declared `params.threads` and recorded `threads`;
    /// `database.oltp` declared `postgres_image` and recorded the version but
    /// never the image; `deployment.container` declared `storage_driver`,
    /// which is the single fact that decides whether two of its numbers may be
    /// compared, and recorded nothing of the kind.
    ///
    /// So the list is now resolved against the bundle when the bundle is
    /// written, and whatever does not resolve is named here. The same choice
    /// `ScoreCard::unreferenced_metrics` makes about a metric with no anchor:
    /// surfaced rather than dropped, because a promise that quietly does not
    /// hold is worse than one that was never made.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub comparability_not_recorded: Vec<String>,
}

/// Whether a declared comparability key resolves to a fact this bundle carries.
///
/// A key is a dotted path, and it is looked for in three places in this order:
///
/// * `module.*` - the module reference. `module.version` is the workload
///   version, which is the fact that matters most and the one every module
///   declares.
/// * `agent.*` - the bundle's own metadata: `agent.build_target`,
///   `agent.build_profile`, `agent.version`.
/// * anything else - the module's `context` first, then the environment
///   inventory. Both are walked by dotted path, so `plan.single_bytes` reaches
///   into a `plan` object and `cpu.architecture` reaches into the inventory.
///
/// The context is searched before the environment on purpose: a module that
/// records its own view of a machine fact is recording what it *used*, and
/// that is the fact the comparison needs. The inventory is what the machine
/// said about itself, which is not always the same thing.
pub fn comparability_key_resolves(
    key: &str,
    module: &darcbench_protocol::ModuleResult,
    meta: &BundleMeta,
    environment: &serde_json::Value,
) -> bool {
    if let Some(rest) = key.strip_prefix("module.") {
        return match rest {
            "version" => !module.module.version.is_empty(),
            "id" => true,
            _ => false,
        };
    }
    if let Some(rest) = key.strip_prefix("agent.") {
        return matches!(rest, "build_target" | "build_profile" | "version")
            && !meta.agent_version.is_empty();
    }
    let context = serde_json::Value::Object(module.context.clone().into_iter().collect());
    walk(&context, key).is_some() || walk(environment, key).is_some()
}

/// Follows a dotted path into a JSON value.
///
/// Tries the whole remaining key at each level before descending, so a literal
/// key containing a dot - and several modules have one - is found rather than
/// mistaken for a path.
fn walk<'a>(value: &'a serde_json::Value, key: &str) -> Option<&'a serde_json::Value> {
    if let Some(found) = value.get(key) {
        return Some(found);
    }
    let (head, rest) = key.split_once('.')?;
    walk(value.get(head)?, rest)
}

/// Every declared comparability fact this bundle does not carry, as
/// `module_id/key`.
///
/// `declared` is the manifest list for each module id, which the caller has
/// because it has the registry and this crate deliberately does not.
pub fn comparability_not_recorded(
    modules: &[darcbench_protocol::ModuleResult],
    meta: &BundleMeta,
    environment: &serde_json::Value,
    declared: &dyn Fn(&darcbench_protocol::ModuleId) -> Vec<String>,
) -> Vec<String> {
    let mut missing = Vec::new();
    for module in modules {
        // A module that failed recorded no context, and reporting every one of
        // its declared keys as missing would bury the real cases in noise from
        // a module that never ran.
        if module.metrics.is_empty() {
            continue;
        }
        for key in declared(&module.module.id) {
            if !comparability_key_resolves(&key, module, meta, environment) {
                missing.push(format!("{}/{key}", module.module.id));
            }
        }
    }
    missing.sort();
    missing.dedup();
    missing
}

/// Aggregated telemetry over the measured window.
///
/// Summarised rather than shipped raw: a 60-minute endurance run at 1 Hz is
/// 3600 samples per field, which does not belong in a shareable bundle. The
/// full series stays in the run directory.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TelemetrySummary {
    pub samples: usize,
    pub cpu_busy_pct_mean: f64,
    /// Peak CPU use by work that was not this benchmark, as a percentage of the
    /// machine.
    ///
    /// The evidence behind `WarningCode::ExternalLoad` and behind the run being
    /// stopped by the load ceiling, so it belongs in the shareable bundle: a
    /// reader deciding whether to trust a degraded result needs to see how much
    /// competition there was, not only that there was some.
    #[serde(default)]
    pub cpu_external_busy_pct_max: f64,
    pub cpu_steal_pct_max: f64,
    pub cpu_steal_pct_mean: f64,
    pub cpu_iowait_pct_mean: f64,
    pub load1_max: f64,
    pub mem_used_bytes_max: u64,
    pub swap_used_bytes_max: u64,
    pub cpu_freq_mhz_first: Option<f64>,
    pub cpu_freq_mhz_last: Option<f64>,
    pub cpu_temp_c_max: Option<f64>,
    pub psi_cpu_some_avg10_max: Option<f64>,
}

impl TelemetrySummary {
    /// Builds a summary from a telemetry series.
    pub fn from_samples(samples: &[darcbench_inventory::TelemetrySnapshot]) -> Self {
        if samples.is_empty() {
            return Self::default();
        }
        // The CPU percentages are aggregated over the ticks that actually read
        // `/proc/stat`. A failed read leaves placeholder zeroes in the snapshot,
        // and averaging those in would report a machine as quieter than it was
        // observed to be - on the same figures that justify a degraded verdict.
        // Everything else in a snapshot comes from its own file and is
        // unaffected, so only these four are filtered.
        let cpu: Vec<&darcbench_inventory::TelemetrySnapshot> =
            samples.iter().filter(|s| !s.cpu_unavailable).collect();
        // Both fold to 0.0 rather than to the identity when no tick was
        // readable: `fold(NEG_INFINITY, max)` over an empty set yields
        // `-inf`, which DCJ/1 refuses to canonicalise, so a host that could
        // not read `/proc/stat` once would produce a bundle that cannot be
        // signed.
        let cpu_mean = |f: fn(&darcbench_inventory::TelemetrySnapshot) -> f64| -> f64 {
            if cpu.is_empty() {
                return 0.0;
            }
            cpu.iter().map(|s| f(s)).sum::<f64>() / cpu.len() as f64
        };
        let cpu_max = |f: fn(&darcbench_inventory::TelemetrySnapshot) -> f64| -> f64 {
            cpu.iter().map(|s| f(s)).fold(0.0_f64, f64::max)
        };

        let max = |f: fn(&darcbench_inventory::TelemetrySnapshot) -> f64| -> f64 {
            samples.iter().map(f).fold(f64::NEG_INFINITY, f64::max)
        };

        Self {
            samples: samples.len(),
            cpu_busy_pct_mean: cpu_mean(|s| s.cpu_busy_pct),
            cpu_external_busy_pct_max: cpu_max(|s| s.cpu_external_busy_pct),
            cpu_steal_pct_max: cpu_max(|s| s.cpu_steal_pct),
            cpu_steal_pct_mean: cpu_mean(|s| s.cpu_steal_pct),
            cpu_iowait_pct_mean: cpu_mean(|s| s.cpu_iowait_pct),
            load1_max: max(|s| s.load1),
            mem_used_bytes_max: samples.iter().map(|s| s.mem_used_bytes).max().unwrap_or(0),
            swap_used_bytes_max: samples.iter().map(|s| s.swap_used_bytes).max().unwrap_or(0),
            cpu_freq_mhz_first: samples.first().and_then(|s| s.cpu_freq_mhz),
            cpu_freq_mhz_last: samples.last().and_then(|s| s.cpu_freq_mhz),
            cpu_temp_c_max: samples
                .iter()
                .filter_map(|s| s.cpu_temp_c)
                .fold(None, |acc: Option<f64>, v| {
                    Some(acc.map_or(v, |a| a.max(v)))
                }),
            psi_cpu_some_avg10_max: samples
                .iter()
                .filter_map(|s| s.psi_cpu_some_avg10)
                .fold(None, |acc: Option<f64>, v| {
                    Some(acc.map_or(v, |a| a.max(v)))
                }),
        }
    }

    /// Fractional clock drop between the first and last observation.
    ///
    /// A sustained drop over a long run is the signature of thermal or power
    /// throttling on bare metal, and of burst-credit exhaustion on cloud VMs.
    pub fn frequency_drop(&self) -> Option<f64> {
        match (self.cpu_freq_mhz_first, self.cpu_freq_mhz_last) {
            (Some(first), Some(last)) if first > 0.0 => Some(1.0 - (last / first)),
            _ => None,
        }
    }
}

/// A complete, self-contained result.
///
/// Field order in the struct is irrelevant: canonicalisation sorts keys, which
/// is what the signature covers.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Bundle {
    pub meta: BundleMeta,
    pub run: RunRecord,
    /// Environment snapshot. Serialised under the ambient redaction policy, so
    /// a bundle written for sharing has identifying fields already removed.
    pub environment: Inventory,
    /// Raw, immutable evidence.
    pub modules: Vec<darcbench_protocol::ModuleResult>,
    /// Derived. Always recomputable from `modules` + the named scoring model.
    pub scores: ScoreCard,
    pub verdict: Verdict,
    pub telemetry: TelemetrySummary,
    /// Why a cycling run slowed down, if it did.
    ///
    /// Derived from `scores.sustained` and the telemetry series together, and
    /// recorded rather than recomputed on read because the full series does not
    /// travel with the bundle - `TelemetrySummary` exists precisely so an hour
    /// of 1 Hz samples does not. Absent for every profile that does not cycle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sustained_diagnosis: Option<crate::diagnosis::SustainedDiagnosis>,
    /// Present once the bundle has been signed. Excluded from the signed bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<Signature>,
}

impl Bundle {
    /// The subset of the bundle a signature covers: everything except the
    /// signature itself.
    ///
    /// # Why the redaction policy is pinned here
    ///
    /// `Inventory` contains [`Sensitive`](darcbench_inventory::redact::Sensitive)
    /// fields whose `Serialize` consults a *thread-local* policy. Left ambient,
    /// the bytes a signature covers would depend on which thread signed and on
    /// whatever `with_policy` scope happened to enclose it - so a bundle signed
    /// under one policy and verified under another would fail its own signature
    /// check with nothing wrong in it. Nothing exports bundles under `Reveal`
    /// today, which is the only reason this has never bitten; pinning removes
    /// the possibility rather than relying on it staying true.
    ///
    /// `Redact` rather than `Reveal` because a bundle is the artifact meant to
    /// travel, and because a redacted value deserialises back to the literal
    /// redacted string - so a bundle written either way re-serialises to the
    /// same signed bytes.
    fn signable(&self) -> serde_json::Value {
        darcbench_inventory::redact::with_policy(
            darcbench_inventory::redact::RedactionPolicy::Redact,
            || self.signable_under_current_policy(),
        )
    }

    fn signable_under_current_policy(&self) -> serde_json::Value {
        serde_json::json!({
            "meta": self.meta,
            "run": self.run,
            "environment": self.environment,
            "modules": self.modules,
            "scores": self.scores,
            "verdict": self.verdict,
            "telemetry": self.telemetry,
            "sustained_diagnosis": self.sustained_diagnosis,
        })
    }

    /// Signs the bundle in place.
    pub fn sign(&mut self, key: &AgentKey) -> Result<(), SigningError> {
        let signature = key.sign(&self.signable())?;
        self.signature = Some(signature);
        Ok(())
    }

    /// Verifies the bundle's own signature.
    pub fn verify_signature(&self) -> Result<(), SigningError> {
        let Some(signature) = &self.signature else {
            return Err(SigningError::BadSignature);
        };
        crate::signing::verify(&self.signable(), signature)
    }

    /// SHA-256 over the canonical form of the *whole* bundle, signature
    /// included. This is the identifier used for deduplication and replay
    /// detection.
    pub fn digest(&self) -> Result<String, crate::canonical::CanonicalError> {
        crate::canonical::canonical_digest(self)
    }

    /// Result state a purely local bundle is entitled to claim.
    ///
    /// Never better than [`ResultState::SelfReported`], and only that once
    /// signed. Anything stronger requires a server that issued a nonce.
    pub fn local_result_state(&self) -> ResultState {
        if self.verdict.state == ResultState::Invalid {
            return ResultState::Invalid;
        }
        match self.signature {
            Some(_) => ResultState::SelfReported,
            None => ResultState::Local,
        }
    }
}

#[cfg(test)]
// In tests, `unwrap`/`expect` panicking *is* the failure signal.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic_in_result_fn)]
mod comparability_tests {
    use super::*;
    use darcbench_protocol::{ModuleId, ModuleRef};

    fn module(context: serde_json::Value) -> darcbench_protocol::ModuleResult {
        let object = match context {
            serde_json::Value::Object(map) => map.into_iter().collect(),
            _ => Default::default(),
        };
        darcbench_protocol::ModuleResult {
            module: ModuleRef {
                id: ModuleId::new("cpu.mixed").unwrap(),
                version: "1.0.1".into(),
            },
            status: darcbench_protocol::metrics::ModuleStatus::Completed,
            cycle: 0,
            started_at: chrono::Utc::now(),
            finished_at: chrono::Utc::now(),
            duration_ms: 1.0,
            metrics: vec![],
            warnings: vec![],
            error: None,
            context: object,
        }
    }

    #[test]
    fn a_declared_key_resolves_only_if_the_bundle_carries_it() {
        let result = module(serde_json::json!({
            "threads": 8,
            "plan": { "single_bytes": 1024 },
            // A literal key containing a dot, which several modules have. It
            // must be found as itself rather than walked into as a path.
            "shape.single.threads": 1,
        }));
        let meta = BundleMeta::new("0.1.0");
        let environment = serde_json::json!({
            "platform": { "architecture": "x86_64" },
            "cpu": { "model": "example" },
        });
        let resolves = |key: &str| comparability_key_resolves(key, &result, &meta, &environment);

        // Module context, flat and by path.
        assert!(resolves("threads"));
        assert!(resolves("plan.single_bytes"));
        assert!(resolves("shape.single.threads"));
        // The environment, by path.
        assert!(resolves("platform.architecture"));
        // Bundle-level facts.
        assert!(resolves("module.version"));
        assert!(resolves("agent.build_target"));

        // And the four shapes of the defect this exists to catch: a key that
        // names the input rather than the recorded fact, one under the wrong
        // root, one that is simply absent, and one that names a namespace with
        // no leaf.
        assert!(!resolves("params.threads"));
        assert!(!resolves("cpu.architecture"));
        assert!(!resolves("storage_driver"));
        assert!(!resolves("agent.nonsense"));
    }

    #[test]
    fn a_module_that_produced_nothing_is_not_reported_as_missing_everything() {
        // A module that failed recorded no context. Listing every key it
        // declared would bury the real cases under noise from one that never
        // ran, and the reason it failed is already on the result.
        let mut failed = module(serde_json::json!({}));
        failed.status = darcbench_protocol::metrics::ModuleStatus::Failed;
        let meta = BundleMeta::new("0.1.0");
        let missing = comparability_not_recorded(
            std::slice::from_ref(&failed),
            &meta,
            &serde_json::json!({}),
            &|_| vec!["threads".to_string(), "plan.single_bytes".to_string()],
        );
        assert!(missing.is_empty(), "{missing:?}");
    }

    #[test]
    fn what_is_missing_is_named_with_the_module_that_promised_it() {
        let mut ran = module(serde_json::json!({ "threads": 8 }));
        ran.metrics.push(darcbench_protocol::metrics::Metric {
            key: "x".into(),
            label: "x".into(),
            unit: "x".into(),
            value: 1.0,
            direction: darcbench_protocol::Direction::HigherIsBetter,
            summary: darcbench_protocol::stats::summarize(&[1.0]).unwrap(),
            samples: vec![],
            outliers: vec![],
        });
        let meta = BundleMeta::new("0.1.0");
        let missing = comparability_not_recorded(
            std::slice::from_ref(&ran),
            &meta,
            &serde_json::json!({}),
            &|_| vec!["threads".to_string(), "params.threads".to_string()],
        );
        assert_eq!(missing, vec!["cpu.mixed/params.threads".to_string()]);
    }
}

#[cfg(test)]
// In tests, `unwrap`/`expect` panicking *is* the failure signal.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic_in_result_fn)]
mod tests {

    #[test]
    fn an_unreadable_proc_stat_does_not_dilute_the_cpu_summary() {
        use darcbench_inventory::TelemetrySnapshot;
        let measured = TelemetrySnapshot {
            cpu_busy_pct: 90.0,
            cpu_external_busy_pct: 40.0,
            cpu_steal_pct: 10.0,
            cpu_iowait_pct: 4.0,
            load1: 8.0,
            ..Default::default()
        };
        // What `sample()` produces when `/proc/stat` cannot be read: the CPU
        // fields are placeholders and the snapshot says so.
        let unmeasured = TelemetrySnapshot {
            cpu_unavailable: true,
            load1: 8.0,
            ..Default::default()
        };

        let all = TelemetrySummary::from_samples(&[measured.clone(), unmeasured]);
        let only_measured = TelemetrySummary::from_samples(std::slice::from_ref(&measured));
        assert_eq!(
            all.cpu_busy_pct_mean, only_measured.cpu_busy_pct_mean,
            "a tick nobody measured must not halve the mean"
        );
        assert_eq!(
            all.cpu_external_busy_pct_max, 40.0,
            "the peak that justifies a degraded verdict must survive"
        );
        assert_eq!(all.samples, 2, "the sample count still counts every tick");
        // load1 comes from its own file and is unaffected by /proc/stat.
        assert_eq!(all.load1_max, 8.0);
    }

    #[test]
    fn a_run_with_no_readable_cpu_tick_still_canonicalises() {
        use darcbench_inventory::TelemetrySnapshot;
        let summary = TelemetrySummary::from_samples(&[TelemetrySnapshot {
            cpu_unavailable: true,
            ..Default::default()
        }]);
        // `fold(NEG_INFINITY, max)` over an empty set would put `-inf` here,
        // and DCJ/1 refuses to canonicalise a non-finite number - so the
        // bundle could not be signed at all.
        assert!(summary.cpu_external_busy_pct_max.is_finite());
        assert!(summary.cpu_steal_pct_max.is_finite());
        assert!(summary.cpu_busy_pct_mean.is_finite());
        assert!(crate::canonical::canonical_json(&summary).is_ok());
    }
    use super::*;
    use darcbench_inventory::TelemetrySnapshot;
    use darcbench_protocol::VerdictReason;

    fn sample_bundle() -> Bundle {
        let now = chrono::Utc::now();
        let inventory = Inventory::collect();
        Bundle {
            meta: BundleMeta::new("0.1.0-test"),
            run: RunRecord {
                run_id: RunId::try_new().expect("id"),
                profile: Profile::Quick,
                state: RunState::Completed,
                started_at: now,
                finished_at: now,
                duration_ms: 42,
                modules: vec![],
                environment_digest: inventory.performance_digest(),
                events_digest: "sha256:0".into(),
                event_count: 3,
                stopped_because: None,
                guards_not_enforced: vec![],
                comparability_not_recorded: vec![],
            },
            environment: inventory,
            modules: vec![],
            scores: darcbench_scoring::ScoringModel::current().score_run(Profile::Quick, &[]),
            verdict: Verdict {
                state: ResultState::Local,
                reasons: vec![VerdictReason::CustomProfile],
                validator_version: "0.1.0".into(),
            },
            telemetry: TelemetrySummary::default(),
            sustained_diagnosis: None,
            signature: None,
        }
    }

    #[test]
    fn bundle_roundtrips_through_json() {
        let bundle = sample_bundle();
        let text = serde_json::to_string(&bundle).expect("ser");
        let back: Bundle = serde_json::from_str(&text).expect("de");
        assert_eq!(back.run.run_id, bundle.run.run_id);
        assert_eq!(back.meta.schema, BUNDLE_SCHEMA_VERSION);
    }

    #[test]
    fn signing_covers_everything_except_the_signature() {
        let key = AgentKey::generate().expect("keygen");
        let mut bundle = sample_bundle();
        bundle.sign(&key).expect("sign");
        bundle.verify_signature().expect("verify");

        // Mutating any covered field must break the signature.
        let mut tampered = bundle.clone();
        tampered.run.duration_ms = 999_999;
        assert!(tampered.verify_signature().is_err());

        let mut rescored = bundle.clone();
        rescored.scores.total = Some(999_999.0);
        assert!(rescored.verify_signature().is_err());

        let mut reverdicted = bundle.clone();
        reverdicted.verdict.state = ResultState::Official;
        assert!(reverdicted.verify_signature().is_err());
    }

    #[test]
    fn signature_survives_a_disk_roundtrip() {
        // Regression test for the float canonicalisation hazard documented in
        // `crate::canonical`: without correctly-rounding float parsing, the
        // bundle read back from disk differs from the one written by one ULP
        // and fails its own signature check.
        let key = AgentKey::generate().expect("keygen");
        let mut bundle = sample_bundle();
        // Values with full f64 precision, of the kind a real throughput
        // measurement produces.
        bundle.scores.total = Some(1234.5678901234567);
        bundle.scores.stability_score = 987.6543210987654;
        bundle.telemetry.cpu_busy_pct_mean = 99.98765432109876;
        bundle.sign(&key).expect("sign");

        let text = serde_json::to_string(&bundle).expect("write");
        let reloaded: Bundle = serde_json::from_str(&text).expect("read");

        assert_eq!(
            reloaded.scores.total, bundle.scores.total,
            "float changed across the disk trip"
        );
        reloaded
            .verify_signature()
            .expect("a bundle must verify after being written and read back");
        assert_eq!(reloaded.digest().expect("d"), bundle.digest().expect("d"));
    }

    #[test]
    fn an_unsigned_bundle_fails_verification() {
        assert!(sample_bundle().verify_signature().is_err());
    }

    #[test]
    fn local_state_never_exceeds_self_reported() {
        let mut bundle = sample_bundle();
        assert_eq!(bundle.local_result_state(), ResultState::Local);

        let key = AgentKey::generate().expect("keygen");
        bundle.sign(&key).expect("sign");
        assert_eq!(bundle.local_result_state(), ResultState::SelfReported);
        assert!(!bundle.local_result_state().is_rankable());

        // Even a bundle that claims Official locally cannot earn it.
        bundle.verdict.state = ResultState::Invalid;
        assert_eq!(bundle.local_result_state(), ResultState::Invalid);
    }

    #[test]
    fn digest_is_stable_and_sensitive() {
        let bundle = sample_bundle();
        assert_eq!(bundle.digest().expect("d"), bundle.digest().expect("d"));
        let mut other = bundle.clone();
        other.run.event_count += 1;
        assert_ne!(bundle.digest().expect("d"), other.digest().expect("d"));
    }

    #[test]
    fn telemetry_summary_from_empty_series_is_zeroed_not_nan() {
        let summary = TelemetrySummary::from_samples(&[]);
        assert_eq!(summary.samples, 0);
        assert!(summary.cpu_busy_pct_mean.is_finite());
        assert!(summary.cpu_steal_pct_max.is_finite());
        assert!(summary.frequency_drop().is_none());
    }

    #[test]
    fn telemetry_summary_computes_means_and_maxima() {
        let samples = vec![
            TelemetrySnapshot {
                cpu_busy_pct: 50.0,
                cpu_steal_pct: 1.0,
                load1: 1.0,
                mem_used_bytes: 100,
                cpu_freq_mhz: Some(3800.0),
                cpu_temp_c: Some(50.0),
                ..Default::default()
            },
            TelemetrySnapshot {
                cpu_busy_pct: 90.0,
                cpu_steal_pct: 9.0,
                load1: 4.0,
                mem_used_bytes: 300,
                cpu_freq_mhz: Some(2850.0),
                cpu_temp_c: Some(72.0),
                ..Default::default()
            },
        ];
        let s = TelemetrySummary::from_samples(&samples);
        assert_eq!(s.samples, 2);
        assert!((s.cpu_busy_pct_mean - 70.0).abs() < 1e-9);
        assert!((s.cpu_steal_pct_max - 9.0).abs() < 1e-9);
        assert_eq!(s.load1_max, 4.0);
        assert_eq!(s.mem_used_bytes_max, 300);
        assert_eq!(s.cpu_temp_c_max, Some(72.0));
        // 3800 -> 2850 MHz is a 25% drop: the classic throttling signature.
        let drop = s.frequency_drop().expect("drop");
        assert!((drop - 0.25).abs() < 1e-9, "expected 25% drop, got {drop}");
    }

    #[test]
    fn bundle_records_the_build_profile() {
        let meta = BundleMeta::new("0.1.0");
        assert!(meta.build_profile == "debug" || meta.build_profile == "release");
        assert!(meta.build_target.contains(std::env::consts::ARCH));
    }
}
