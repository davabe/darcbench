//! The standalone run index: SQLite over the bundles on disk.
//!
//! # What this is, and what it is not
//!
//! [ADR-0005](../../../docs/adr/0005-database-and-storage.md) is explicit about
//! the hierarchy: *"Bundles are the source of truth in both modes. A database is
//! an index over them, never the only copy."* Nothing here is authoritative.
//! Every row can be rebuilt from `<state>/runs/<run_id>/bundle.json` by
//! [`RunIndex::reconcile`], and a corrupt or deleted database costs a rebuild,
//! not a result.
//!
//! That hierarchy is why the index is allowed to fail quietly. A run that
//! completed, scored and signed has done its job; failing it afterwards because
//! an index write went wrong would destroy a real measurement to protect a
//! cache of it.
//!
//! # Why a database at all
//!
//! Phase 1 listed runs by scanning the directory and parsing every
//! `bundle.json` in full - a complete inventory, every metric and every
//! per-repetition sample - to read four fields. That is fine into the hundreds
//! and hopeless for the fleet views Phase 7 wants. It also could not answer the
//! question Phase 2 is actually for: *how does this run compare with that one*,
//! metric by metric, without opening both bundles.
//!
//! # Retention
//!
//! Pruning is an explicit command, never a background sweep, and it refuses to
//! delete `Invalid` runs. `docs/DATA-MODEL.md`: *"Invalid results are kept.
//! Deleting them would hide evidence, and the reason a run failed is often more
//! informative than the run succeeding would have been."*

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use darcbench_protocol::metrics::Direction;
use darcbench_protocol::{ResultState, RunId};
use darcbench_report::Bundle;
use rusqlite::{Connection, OptionalExtension};

/// Schema version, stored in SQLite's own `user_version` pragma.
///
/// A database written by a newer agent is not opened by an older one: the
/// alternative is an older binary silently reading columns that have since
/// changed meaning. Rebuilding from the bundles is cheap, so refusing is
/// affordable in a way that guessing is not.
const SCHEMA_VERSION: i64 = 3;

/// File name under the state directory. Sibling of `runs/`, not inside it, so
/// the run directories stay exactly what the on-disk layout documents.
pub(crate) const INDEX_FILE: &str = "index.db";

#[derive(Debug, thiserror::Error)]
pub(crate) enum IndexError {
    #[error("run index: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("run index: {0}")]
    Io(#[from] std::io::Error),
    #[error(
        "run index: {unreadable} row(s) could not be read, so the retention window cannot be \
         computed from a complete list. Nothing was deleted. The index is rebuilt from the \
         bundles at every start, so restarting the agent and retrying is the fix."
    )]
    PartialView { unreadable: usize },
    #[error(
        "run index at {path} was written by a newer agent (schema {found}, this build \
         understands {expected}). Delete it to have this agent rebuild it from the bundles, \
         which remain the source of truth."
    )]
    FutureSchema {
        path: PathBuf,
        found: i64,
        expected: i64,
    },
}

/// One row of the run list: everything `darcbench status` and
/// `GET /api/v1/runs` need, without opening a bundle.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct IndexedRun {
    pub(crate) run_id: String,
    pub(crate) profile: String,
    /// Lifecycle state as recorded in the bundle: `Completed`, `Cancelled`,
    /// `Failed`. Carried because a run stopped by the watchdog and a run that
    /// finished are different facts, and reporting every historical run as
    /// "completed" would erase exactly the distinction `stopped_because` exists
    /// to preserve.
    pub(crate) run_state: String,
    /// Why the run stopped early, when it did.
    pub(crate) stopped_because: Option<String>,
    pub(crate) result_state: ResultState,
    pub(crate) finished_at: chrono::DateTime<chrono::Utc>,
    pub(crate) duration_ms: u64,
    pub(crate) total_score: Option<f64>,
    pub(crate) total_is_standard: bool,
    pub(crate) scoring_model: String,
    pub(crate) agent_version: String,
    pub(crate) build_target: String,
    /// Digest of the machine's performance-relevant inventory. Two runs that
    /// share it were taken on what looks like the same machine, which is the
    /// precondition for comparing them at all.
    pub(crate) environment_digest: String,
    pub(crate) bundle_digest: String,
    pub(crate) modules: Vec<String>,
}

/// One metric compared across two runs.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct MetricDelta {
    pub(crate) module: String,
    pub(crate) metric_key: String,
    pub(crate) unit: String,
    pub(crate) baseline: f64,
    pub(crate) candidate: f64,
    /// Candidate relative to baseline, **direction-adjusted**: above 1.0 always
    /// means the candidate is better, whether the metric counts throughput or
    /// latency. A raw ratio would report a doubled fsync latency as a 2x
    /// improvement, which is the one reading a comparison must never allow.
    pub(crate) ratio: f64,
}

/// The result of comparing two runs.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Comparison {
    pub(crate) baseline: IndexedRun,
    pub(crate) candidate: IndexedRun,
    pub(crate) metrics: Vec<MetricDelta>,
    /// Metrics present in one run and not the other, named rather than dropped.
    ///
    /// A comparison that silently ignores what it could not line up looks
    /// complete while describing a subset, and the usual cause - a module that
    /// failed, or a version whose metric set changed - is the more interesting
    /// finding.
    pub(crate) unmatched: Vec<String>,
    /// True when the two runs disagree about anything that makes their numbers
    /// non-comparable. The comparison is still produced; it is labelled.
    pub(crate) comparable: bool,
    pub(crate) incomparable_reasons: Vec<String>,
}

/// What a prune would do, or did.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct PruneOutcome {
    pub(crate) removed: Vec<String>,
    /// Runs the policy selected but that could not be removed, with the reason.
    ///
    /// Collected rather than raised, because a prune that stops on the first
    /// failure has already deleted everything before it and then reports only
    /// the error - leaving the operator with no record of what went and no way
    /// to reconstruct it. One unreadable directory must not cost the account of
    /// the other 199.
    pub(crate) failed: Vec<(String, String)>,
    /// Runs the policy selected but that are retained anyway because they are
    /// `Invalid`.
    pub(crate) retained_as_evidence: Vec<String>,
    pub(crate) bytes_freed: u64,
}

/// Which runs a prune may remove.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct RetentionPolicy {
    /// Remove runs that finished more than this many days ago.
    pub(crate) older_than_days: Option<u32>,
    /// Keep at most this many runs, newest first.
    pub(crate) keep_last: Option<usize>,
}

impl RetentionPolicy {
    /// True when the policy would select nothing, whatever is on disk.
    ///
    /// A prune with no policy deleting everything would be the worst possible
    /// default for a command whose mistakes are not recoverable.
    pub(crate) fn is_empty(&self) -> bool {
        self.older_than_days.is_none() && self.keep_last.is_none()
    }

    /// True when the policy would select *every* run.
    ///
    /// `--keep-last 0` and `--older-than-days 0` are each one keystroke from a
    /// sensible value and each mean "delete all of it". Given that this
    /// operation has no undo, they get the same refusal an absent policy does
    /// rather than being honoured as an unusually decisive instruction.
    pub(crate) fn selects_everything(&self) -> bool {
        self.keep_last == Some(0) || self.older_than_days == Some(0)
    }
}

pub(crate) struct RunIndex {
    /// `None` only for [`RunIndex::unavailable`].
    connection: Option<Mutex<Connection>>,
    path: PathBuf,
}

impl std::fmt::Debug for RunIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunIndex")
            .field("path", &self.path)
            .finish()
    }
}

impl RunIndex {
    /// Opens - creating and migrating if needed - the index at `path`.
    pub(crate) fn open(path: PathBuf) -> Result<Self, IndexError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let connection = Connection::open(&path)?;
        Self::prepare(&connection, &path)?;
        Ok(Self {
            connection: Some(Mutex::new(connection)),
            path,
        })
    }

    /// An index that exists only for the life of the process.
    ///
    /// Used by the tests, and as the fallback when the on-disk index cannot be
    /// opened: a degraded agent that can still list the runs it executed itself
    /// is better than one that refuses to start.
    pub(crate) fn in_memory() -> Result<Self, IndexError> {
        let connection = Connection::open_in_memory()?;
        let path = PathBuf::from(":memory:");
        Self::prepare(&connection, &path)?;
        Ok(Self {
            connection: Some(Mutex::new(connection)),
            path,
        })
    }

    /// An index that answers "nothing" to everything.
    ///
    /// The last resort when SQLite cannot be used at all. Every read path
    /// already treats an empty index as "no history recorded", so this needs no
    /// special handling anywhere else - which is the point: a benchmark must
    /// still run on a host where the index will not open.
    pub(crate) fn unavailable() -> Self {
        Self {
            connection: None,
            path: PathBuf::from("<unavailable>"),
        }
    }

    /// The open connection, or `None` when there is no usable index.
    ///
    /// A poisoned mutex is also `None`: it means another thread panicked
    /// mid-write, and the bundles - which are the source of truth - are
    /// unaffected, so the honest answer is that the index cannot serve this
    /// call rather than a panic in a second thread.
    fn connection(&self) -> Option<std::sync::MutexGuard<'_, Connection>> {
        self.connection.as_ref()?.lock().ok()
    }

    fn prepare(connection: &Connection, path: &Path) -> Result<(), IndexError> {
        // WAL so a `serve` process writing a run does not block a `status`
        // command reading one. `NORMAL` rather than `FULL`: the bundles are the
        // durable copy, and an index that loses its last write to a power cut
        // is repaired by `reconcile` on the next start.
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "NORMAL")?;
        connection.pragma_update(None, "foreign_keys", true)?;
        // Two processes share this file by design - a `serve` recording a run
        // while a CLI reads one - and WAL only removes *reader*-writer
        // contention. Every CLI read command is a writer, because it reconciles
        // first. Without a busy handler SQLite returns SQLITE_BUSY immediately,
        // and since index writes are deliberately best-effort that failure is
        // silent: the run just never appears in the history. Five seconds is far
        // longer than any write here takes and far shorter than a human waits.
        connection.busy_timeout(std::time::Duration::from_secs(5))?;

        let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version > SCHEMA_VERSION {
            return Err(IndexError::FutureSchema {
                path: path.to_path_buf(),
                found: version,
                expected: SCHEMA_VERSION,
            });
        }
        if version == SCHEMA_VERSION {
            return Ok(());
        }
        if version > 0 {
            // Migration is a rebuild. Everything here is derived from the
            // bundles, so dropping and re-reconciling is both the simplest
            // migration and the one that cannot produce a half-converted row -
            // and it costs a directory scan on one startup. A real `ALTER
            // TABLE` path becomes worth writing when the index holds something
            // the bundles do not, which is never, by design.
            connection.execute_batch(
                "BEGIN;
                 DROP TABLE IF EXISTS metrics;
                 DROP TABLE IF EXISTS categories;
                 DROP TABLE IF EXISTS runs;
                 COMMIT;",
            )?;
        }

        connection.execute_batch(
            "BEGIN;
             CREATE TABLE IF NOT EXISTS runs (
                 run_id             TEXT PRIMARY KEY,
                 profile            TEXT NOT NULL,
                 run_state          TEXT NOT NULL,
                 result_state       TEXT NOT NULL,
                 started_at         TEXT NOT NULL,
                 finished_at        TEXT NOT NULL,
                 duration_ms        INTEGER NOT NULL,
                 total_score        REAL,
                 total_is_standard  INTEGER NOT NULL,
                 scoring_model      TEXT NOT NULL,
                 uncalibrated       INTEGER NOT NULL,
                 agent_version      TEXT NOT NULL,
                 build_target       TEXT NOT NULL,
                 build_profile      TEXT NOT NULL,
                 environment_digest TEXT NOT NULL,
                 bundle_digest      TEXT NOT NULL,
                 modules            TEXT NOT NULL,
                 stopped_because    TEXT,
                 -- Identity of the bundle file this row was built from, so
                 -- `reconcile` can tell a row that is merely known from a row
                 -- that is still true. Size and mtime rather than a digest,
                 -- because a digest costs parsing every bundle on every
                 -- startup, which is the scan the index exists to replace.
                 bundle_size        INTEGER NOT NULL DEFAULT 0,
                 bundle_mtime       INTEGER NOT NULL DEFAULT 0
             );
             CREATE INDEX IF NOT EXISTS runs_by_finish ON runs (finished_at DESC);
             CREATE INDEX IF NOT EXISTS runs_by_machine ON runs (environment_digest);
             CREATE TABLE IF NOT EXISTS metrics (
                 run_id     TEXT NOT NULL REFERENCES runs(run_id) ON DELETE CASCADE,
                 module     TEXT NOT NULL,
                 cycle      INTEGER NOT NULL,
                 metric_key TEXT NOT NULL,
                 value      REAL NOT NULL,
                 unit       TEXT NOT NULL,
                 direction  TEXT NOT NULL,
                 cv         REAL,
                 samples    INTEGER NOT NULL,
                 PRIMARY KEY (run_id, module, cycle, metric_key)
             );
             CREATE TABLE IF NOT EXISTS categories (
                 run_id TEXT NOT NULL REFERENCES runs(run_id) ON DELETE CASCADE,
                 key    TEXT NOT NULL,
                 label  TEXT NOT NULL,
                 score  REAL NOT NULL,
                 weight REAL NOT NULL,
                 -- Comma-separated module ids that produced this category, as
                 -- the score card publishes them. Stored so a comparison can
                 -- see that two same-named categories were computed from
                 -- different workloads without opening either bundle.
                 modules TEXT NOT NULL DEFAULT '',
                 PRIMARY KEY (run_id, key)
             );
             COMMIT;",
        )?;
        connection.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        Ok(())
    }

    /// Indexes one bundle, replacing any previous row for the same run.
    ///
    /// Replacing rather than refusing, because a rescore under a new model
    /// produces a new bundle for a run that already exists, and the index has
    /// to follow the file rather than argue with it.
    pub(crate) fn record(&self, bundle: &Bundle, source: &Path) -> Result<(), IndexError> {
        self.record_with_source(bundle, Some(source))
    }

    /// [`Self::record`], additionally stamping the row with the identity of the
    /// file it came from so [`Self::reconcile`] can detect it going stale.
    fn record_with_source(&self, bundle: &Bundle, source: Option<&Path>) -> Result<(), IndexError> {
        let (bundle_size, bundle_mtime) = source.map_or((0, 0), file_identity);
        // A missing or poisoned index is not a failed run: the bundle is
        // already on disk, and `reconcile` picks it up on the next start.
        let Some(mut connection) = self.connection() else {
            return Ok(());
        };
        let transaction = connection.transaction()?;
        let model = darcbench_scoring::ScoringModel::current();
        let run_id = bundle.run.run_id.as_str();
        let digest = bundle.digest().unwrap_or_default();
        let modules: Vec<String> = bundle
            .run
            .modules
            .iter()
            .map(|m| format!("{}@{}", m.id, m.version))
            .collect();

        transaction.execute(
            "INSERT OR REPLACE INTO runs (
                 run_id, profile, run_state, result_state, started_at, finished_at, duration_ms,
                 total_score, total_is_standard, scoring_model, uncalibrated, agent_version,
                 build_target, build_profile, environment_digest, bundle_digest, modules,
                 stopped_because,
                 bundle_size, bundle_mtime
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16,
                       ?17, ?18, ?19, ?20)",
            rusqlite::params![
                run_id,
                bundle.run.profile.as_str(),
                format!("{:?}", bundle.run.state),
                result_state_key(bundle.verdict.state),
                bundle.run.started_at.to_rfc3339(),
                bundle.run.finished_at.to_rfc3339(),
                bundle.run.duration_ms,
                bundle.scores.total,
                bundle.scores.total_is_standard,
                bundle.scores.scoring_model,
                bundle.scores.uncalibrated,
                bundle.meta.agent_version,
                bundle.meta.build_target,
                bundle.meta.build_profile,
                bundle.run.environment_digest,
                digest,
                serde_json::to_string(&modules).unwrap_or_else(|_| "[]".into()),
                bundle.run.stopped_because,
                bundle_size,
                bundle_mtime,
            ],
        )?;

        // Rewritten wholesale rather than merged: a replaced bundle may have
        // fewer metrics than the one before it, and leaving the difference
        // behind would invent measurements the run does not contain.
        transaction.execute("DELETE FROM metrics WHERE run_id = ?1", [run_id])?;
        transaction.execute("DELETE FROM categories WHERE run_id = ?1", [run_id])?;

        for module in &bundle.modules {
            for metric in &module.metrics {
                transaction.execute(
                    "INSERT OR REPLACE INTO metrics
                     (run_id, module, cycle, metric_key, value, unit, direction, cv, samples)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    rusqlite::params![
                        run_id,
                        module.module.id.as_str(),
                        module.cycle,
                        metric.key,
                        metric.value,
                        metric.unit,
                        direction_key(anchor_direction(
                            &model,
                            module.module.id.as_str(),
                            &metric.key,
                            metric.direction,
                        )),
                        metric.summary.cv,
                        metric.summary.n as i64,
                    ],
                )?;
            }
        }
        for category in &bundle.scores.categories {
            transaction.execute(
                "INSERT OR REPLACE INTO categories (run_id, key, label, score, weight, modules)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![
                    run_id,
                    format!("{:?}", category.key),
                    category.label,
                    category.score,
                    category.weight,
                    category.modules.join(","),
                ],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    /// The most recent runs, newest first.
    pub(crate) fn list(&self, limit: usize) -> Result<Vec<IndexedRun>, IndexError> {
        Ok(self.list_rows(limit)?.0)
    }

    /// The most recent runs, plus how many rows could not be read.
    ///
    /// `list` drops an unreadable row, which is right for a history someone is
    /// looking at: one bad row should not blank the page. It is wrong for
    /// anything that *acts* on position in the result, because a dropped row
    /// silently shifts everything after it - `prune` selecting by index into
    /// this list would then keep one run fewer than `--keep-last` asked for,
    /// and deleting a benchmark result is not undoable.
    ///
    /// So the count is returned rather than discarded, and callers that cannot
    /// tolerate a partial view check it. No row can fail to convert against the
    /// current schema; the asymmetry is removed anyway, because "no trigger
    /// exists today" is not a property that survives a schema change.
    fn list_rows(&self, limit: usize) -> Result<(Vec<IndexedRun>, usize), IndexError> {
        let Some(connection) = self.connection() else {
            return Ok((Vec::new(), 0));
        };
        let mut statement = connection.prepare(&format!(
            "SELECT {COLUMNS} FROM runs ORDER BY finished_at DESC LIMIT ?1"
        ))?;
        let rows = statement.query_map([limit as i64], row_to_run)?;
        let mut runs = Vec::new();
        let mut unreadable = 0usize;
        for row in rows {
            match row {
                Ok(run) => runs.push(run),
                Err(_) => unreadable += 1,
            }
        }
        Ok((runs, unreadable))
    }

    pub(crate) fn get(&self, run_id: &str) -> Result<Option<IndexedRun>, IndexError> {
        let Some(connection) = self.connection() else {
            return Ok(None);
        };
        let mut statement =
            connection.prepare(&format!("SELECT {COLUMNS} FROM runs WHERE run_id = ?1"))?;
        Ok(statement.query_row([run_id], row_to_run).optional()?)
    }

    pub(crate) fn count(&self) -> Result<usize, IndexError> {
        let Some(connection) = self.connection() else {
            return Ok(0);
        };
        let count: i64 = connection.query_row("SELECT COUNT(*) FROM runs", [], |row| row.get(0))?;
        Ok(count.max(0) as usize)
    }

    /// Compares two indexed runs metric by metric.
    ///
    /// Only cycle 0 is compared. An endurance run's later cycles measure a
    /// machine that has been under load for an hour, and lining cycle 7 of one
    /// run up against cycle 2 of another would compare two different questions;
    /// what a *cycling* run retained is already its own published number.
    pub(crate) fn compare(
        &self,
        baseline_id: &str,
        candidate_id: &str,
    ) -> Result<Option<Comparison>, IndexError> {
        let (Some(baseline), Some(candidate)) = (self.get(baseline_id)?, self.get(candidate_id)?)
        else {
            return Ok(None);
        };

        let baseline_metrics = self.metrics_of(baseline_id)?;
        let candidate_metrics = self.metrics_of(candidate_id)?;

        let mut metrics = Vec::new();
        let mut unmatched = Vec::new();
        for (key, left) in &baseline_metrics {
            let Some(right) = candidate_metrics.get(key) else {
                unmatched.push(format!("{key} (baseline only)"));
                continue;
            };
            // A ratio needs both sides positive: a zero baseline has no
            // multiple, and a negative value is not a measurement.
            if left.value <= 0.0 || right.value <= 0.0 {
                unmatched.push(format!("{key} (not a positive measurement in both runs)"));
                continue;
            }
            let ratio = match left.direction {
                Direction::HigherIsBetter => right.value / left.value,
                Direction::LowerIsBetter => left.value / right.value,
            };
            if !ratio.is_finite() {
                unmatched.push(format!("{key} (ratio is not finite)"));
                continue;
            }
            metrics.push(MetricDelta {
                module: left.module.clone(),
                metric_key: left.metric_key.clone(),
                unit: left.unit.clone(),
                baseline: left.value,
                candidate: right.value,
                ratio,
            });
        }
        for key in candidate_metrics.keys() {
            if !baseline_metrics.contains_key(key) {
                unmatched.push(format!("{key} (candidate only)"));
            }
        }
        unmatched.sort();

        let mut incomparable_reasons = comparability_gaps(&baseline, &candidate);
        // A category computed from different workloads is not the same
        // measurement, however identical the two machines are. The Web category
        // is the live case: `php.runtime` is scored into it and only exists on a
        // host with PHP installed, so two `web` runs on one machine can produce
        // Web scores from different baskets. Reported alongside the other
        // reasons rather than as a separate concept, because the consequence is
        // the same - the numbers do not line up - and a caller that renders one
        // renders the other.
        let baseline_baskets = self.baskets_of(baseline_id)?;
        let candidate_baskets = self.baskets_of(candidate_id)?;
        for (key, before) in &baseline_baskets {
            let Some(after) = candidate_baskets.get(key) else {
                continue;
            };
            if before == after {
                continue;
            }
            // An empty basket means the bundle predates the field, not that the
            // category was computed from no modules. Absence of the record is
            // not evidence of a difference, and reporting it as one would put
            // "computed from nothing" in front of an operator comparing a run
            // made last month with one made today - which is both false and the
            // most common comparison there is. The same discipline the rest of
            // this codebase applies to unmeasured facts.
            if before.is_empty() || after.is_empty() {
                continue;
            }
            let named = |ids: &std::collections::BTreeSet<String>| {
                ids.iter().cloned().collect::<Vec<_>>().join(", ")
            };
            incomparable_reasons.push(format!(
                "the {key} category was computed from different modules: {} then {}",
                named(before),
                named(after),
            ));
        }

        Ok(Some(Comparison {
            comparable: incomparable_reasons.is_empty(),
            incomparable_reasons,
            baseline,
            candidate,
            metrics,
            unmatched,
        }))
    }

    /// Category baskets of one run, keyed by category.
    ///
    /// The module ids come back as a set rather than the stored string, because
    /// the question a comparison asks is which workloads differ, and ordering
    /// is an artefact of storage.
    fn baskets_of(
        &self,
        run_id: &str,
    ) -> Result<BTreeMap<String, std::collections::BTreeSet<String>>, IndexError> {
        let Some(connection) = self.connection() else {
            return Ok(BTreeMap::new());
        };
        let mut statement =
            connection.prepare("SELECT key, modules FROM categories WHERE run_id = ?1")?;
        let rows = statement.query_map([run_id], |row| {
            let key: String = row.get(0)?;
            let modules: String = row.get(1)?;
            Ok((key, modules))
        })?;
        Ok(rows
            .filter_map(Result::ok)
            .map(|(key, modules)| {
                (
                    key,
                    modules
                        .split(',')
                        .filter(|m| !m.is_empty())
                        .map(str::to_string)
                        .collect(),
                )
            })
            .collect())
    }

    /// Cycle-0 metrics of one run, keyed `<module>/<metric_key>`.
    fn metrics_of(&self, run_id: &str) -> Result<BTreeMap<String, IndexedMetric>, IndexError> {
        let Some(connection) = self.connection() else {
            return Ok(BTreeMap::new());
        };
        let mut statement = connection.prepare(
            "SELECT module, metric_key, value, unit, direction FROM metrics
             WHERE run_id = ?1 AND cycle = 0",
        )?;
        let rows = statement.query_map([run_id], |row| {
            let module: String = row.get(0)?;
            let metric_key: String = row.get(1)?;
            let direction: String = row.get(4)?;
            Ok(IndexedMetric {
                module,
                metric_key,
                value: row.get(2)?,
                unit: row.get(3)?,
                direction: if direction == "lower_is_better" {
                    Direction::LowerIsBetter
                } else {
                    Direction::HigherIsBetter
                },
            })
        })?;
        Ok(rows
            .filter_map(Result::ok)
            .map(|m| (format!("{}/{}", m.module, m.metric_key), m))
            .collect())
    }

    /// Rebuilds the index from the bundles on disk.
    ///
    /// Runs at startup, and is what makes the database disposable: it indexes
    /// every bundle it does not already know about, and forgets every run whose
    /// directory has gone. Both halves matter - the first recovers from a
    /// crash or a deleted database, the second from somebody clearing space
    /// with `rm -rf`, which is a thing operators do and should not be punished
    /// for with a list full of runs that no longer exist.
    pub(crate) fn reconcile(&self, runs_dir: &Path) -> Result<ReconcileOutcome, IndexError> {
        let mut outcome = ReconcileOutcome::default();

        let mut on_disk: Vec<String> = Vec::new();
        if let Ok(entries) = std::fs::read_dir(runs_dir) {
            for entry in entries.filter_map(Result::ok) {
                let name = entry.file_name().to_string_lossy().to_string();
                // Parsed rather than trusted: a directory whose name is not a
                // run id was not written by this agent, and indexing it would
                // put an unvalidated string into a primary key.
                if name.parse::<RunId>().is_err() {
                    continue;
                }
                if !entry.path().join("bundle.json").is_file() {
                    // A run that was interrupted before it wrote a bundle. Not
                    // an error and not indexable: there is nothing to index.
                    continue;
                }
                on_disk.push(name);
            }
        }

        for run_id in &on_disk {
            let path = runs_dir.join(run_id).join("bundle.json");
            // Known is not the same as still true. A row is refreshed when the
            // file behind it has changed size or mtime, so a rewritten bundle -
            // a rescore, or an edit by hand - does not leave the index serving
            // a digest and a result state that no longer describe it. `prune`
            // reads `result_state` from the row, so a stale row is not merely
            // cosmetic: it decides what gets deleted.
            if self.is_current(run_id, &path)? {
                continue;
            }
            match std::fs::read(&path)
                .ok()
                .and_then(|raw| serde_json::from_slice::<Bundle>(&raw).ok())
            {
                Some(bundle) => {
                    self.record_with_source(&bundle, Some(&path))?;
                    outcome.indexed.push(run_id.clone());
                }
                // A bundle this build cannot parse is left alone rather than
                // deleted: it is still the operator's evidence, and a newer or
                // older agent may well read it.
                None => outcome.unreadable.push(run_id.clone()),
            }
        }

        let known: Vec<String> = self
            .list(usize::MAX)?
            .into_iter()
            .map(|run| run.run_id)
            .collect();
        for run_id in known {
            if on_disk.contains(&run_id) {
                continue;
            }
            // Re-checked against the filesystem rather than against the
            // snapshot taken at the top of this function. Indexing the bundles
            // above can take seconds on a large directory, and another process
            // finishing a run in that window would otherwise have its row
            // deleted by this one.
            if runs_dir.join(&run_id).join("bundle.json").is_file() {
                continue;
            }
            self.forget(&run_id)?;
            outcome.forgotten.push(run_id);
        }
        Ok(outcome)
    }

    /// True when the indexed row was built from the file that is there now.
    fn is_current(&self, run_id: &str, bundle_path: &Path) -> Result<bool, IndexError> {
        let Some(connection) = self.connection() else {
            return Ok(false);
        };
        let stored: Option<(i64, i64)> = connection
            .query_row(
                "SELECT bundle_size, bundle_mtime FROM runs WHERE run_id = ?1",
                [run_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some(stored) = stored else {
            return Ok(false);
        };
        // A row stamped (0, 0) carries no identity, so nothing about it can be
        // compared to the file on disk. It is treated as **stale** and re-read.
        //
        // It used to be treated as current, on the reasoning that `record`
        // wrote it straight from the bundle it had just persisted. That is true
        // at the instant of writing and false forever afterwards: the row then
        // claimed to be current for the life of the index, so a bundle edited
        // or replaced later kept serving its old verdict, metrics and digest -
        // and `prune`, which refuses to delete an `Invalid` run, would consult
        // that stale row and delete a bundle that had since become invalid.
        //
        // `record` now stamps the real identity, so this branch only sees rows
        // written by an older build. Re-reading one costs a parse once.
        if stored == (0, 0) {
            return Ok(false);
        }
        Ok(stored == file_identity(bundle_path))
    }

    fn forget(&self, run_id: &str) -> Result<(), IndexError> {
        let Some(connection) = self.connection() else {
            return Ok(());
        };
        // `ON DELETE CASCADE` plus `foreign_keys = ON` takes the metric and
        // category rows with it.
        connection.execute("DELETE FROM runs WHERE run_id = ?1", [run_id])?;
        Ok(())
    }

    /// Applies a retention policy.
    ///
    /// `dry_run` selects and reports without touching anything, which is the
    /// mode this command should usually be run in first: deleting a benchmark
    /// result is not undoable, and the bundles are the only copy.
    pub(crate) fn prune(
        &self,
        runs_dir: &Path,
        policy: RetentionPolicy,
        dry_run: bool,
    ) -> Result<PruneOutcome, IndexError> {
        let mut outcome = PruneOutcome::default();
        if policy.is_empty() {
            return Ok(outcome);
        }

        let (all, unreadable) = self.list_rows(usize::MAX)?;
        if unreadable > 0 {
            // Refuse rather than delete from a view known to be incomplete.
            // The window is positional, so an invisible row moves it, and the
            // operation has no undo. Rebuilding the index is one restart away.
            return Err(IndexError::PartialView { unreadable });
        }
        let cutoff = policy
            .older_than_days
            .map(|days| chrono::Utc::now() - chrono::Duration::days(i64::from(days)));

        for (position, run) in all.iter().enumerate() {
            let too_old = cutoff.is_some_and(|cutoff| run.finished_at < cutoff);
            // `all` is newest first, so everything at or past `keep_last` is
            // outside the window.
            let beyond_window = policy.keep_last.is_some_and(|keep| position >= keep);
            if !(too_old || beyond_window) {
                continue;
            }
            if run.result_state == ResultState::Invalid {
                // DATA-MODEL.md: invalid results are retained. The reason a run
                // failed is often more informative than the run succeeding
                // would have been, and a retention policy that quietly deletes
                // the failures leaves a history that only ever went well.
                outcome.retained_as_evidence.push(run.run_id.clone());
                continue;
            }
            let directory = runs_dir.join(&run.run_id);
            let bytes = directory_bytes(&directory);
            if !dry_run {
                if let Err(error) = std::fs::remove_dir_all(&directory) {
                    if error.kind() != std::io::ErrorKind::NotFound {
                        outcome.failed.push((run.run_id.clone(), error.to_string()));
                        continue;
                    }
                }
                // The row goes only once the directory is actually gone, so a
                // failure leaves the run listed rather than invisible-but-present.
                if let Err(error) = self.forget(&run.run_id) {
                    outcome.failed.push((run.run_id.clone(), error.to_string()));
                    continue;
                }
            }
            outcome.bytes_freed += bytes;
            outcome.removed.push(run.run_id.clone());
        }
        Ok(outcome)
    }
}

/// What a [`RunIndex::reconcile`] pass changed.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct ReconcileOutcome {
    pub(crate) indexed: Vec<String>,
    pub(crate) forgotten: Vec<String>,
    pub(crate) unreadable: Vec<String>,
}

impl ReconcileOutcome {
    pub(crate) fn is_noop(&self) -> bool {
        self.indexed.is_empty() && self.forgotten.is_empty() && self.unreadable.is_empty()
    }
}

#[derive(Clone, Debug)]
struct IndexedMetric {
    module: String,
    metric_key: String,
    value: f64,
    unit: String,
    direction: Direction,
}

const COLUMNS: &str = "run_id, profile, result_state, finished_at, duration_ms, total_score, \
                       total_is_standard, scoring_model, agent_version, build_target, \
                       environment_digest, bundle_digest, modules, run_state, stopped_because";

fn row_to_run(row: &rusqlite::Row<'_>) -> rusqlite::Result<IndexedRun> {
    let finished: String = row.get(3)?;
    let modules: String = row.get(12)?;
    Ok(IndexedRun {
        run_id: row.get(0)?,
        profile: row.get(1)?,
        run_state: row.get(13)?,
        stopped_because: row.get(14)?,
        result_state: result_state_from_key(&row.get::<_, String>(2)?),
        finished_at: chrono::DateTime::parse_from_rfc3339(&finished)
            .map(|t| t.with_timezone(&chrono::Utc))
            .unwrap_or_else(|_| chrono::DateTime::UNIX_EPOCH),
        duration_ms: row.get::<_, i64>(4)?.max(0) as u64,
        total_score: row.get(5)?,
        total_is_standard: row.get(6)?,
        scoring_model: row.get(7)?,
        agent_version: row.get(8)?,
        build_target: row.get(9)?,
        environment_digest: row.get(10)?,
        bundle_digest: row.get(11)?,
        modules: serde_json::from_str(&modules).unwrap_or_default(),
    })
}

/// Everything that makes two runs' numbers not directly comparable.
///
/// Reported rather than enforced. An operator comparing a run from before a
/// kernel upgrade with one from after is doing something legitimate and
/// interesting; what they must not do is read the difference as the machine
/// changing when it was the measurement that changed.
fn comparability_gaps(baseline: &IndexedRun, candidate: &IndexedRun) -> Vec<String> {
    let mut gaps = Vec::new();
    if baseline.environment_digest != candidate.environment_digest {
        gaps.push(
            "These runs were taken on machines whose performance-relevant inventory differs \
             (CPU, memory, topology or storage stack). The difference below includes the \
             difference between the machines."
                .to_string(),
        );
    }
    if baseline.scoring_model != candidate.scoring_model {
        gaps.push(format!(
            "Scored by different models ({} and {}). Raw metrics remain comparable; the scores \
             do not.",
            baseline.scoring_model, candidate.scoring_model
        ));
    }
    if baseline.profile != candidate.profile {
        gaps.push(format!(
            "Different profiles ({} and {}), so the two runs did not measure the same set of \
             work.",
            baseline.profile, candidate.profile
        ));
    }
    // Finding worth stating on the comparison rather than in a doc comment: the
    // metric rows come from cycle 0 and the totals beside them come from the
    // last complete cycle, so on a cycling run the two halves of the output
    // describe different moments. Reading flat metric rows under a large score
    // gap as "the totals are noise" is exactly the wrong conclusion.
    if baseline.profile == "endurance" || candidate.profile == "endurance" {
        gaps.push(
            "One of these runs cycled. The metric rows below are its opening cycle, while the              total scores are taken from its last complete one - so a flat row under a moved              total is the machine declining over the run, not noise. What a cycling run retained              is its own published Sustained Performance Score."
                .to_string(),
        );
    }
    if baseline.build_target != candidate.build_target {
        gaps.push(format!(
            "Different build targets ({} and {}).",
            baseline.build_target, candidate.build_target
        ));
    }
    if baseline.agent_version != candidate.agent_version {
        gaps.push(format!(
            "Different agent versions ({} and {}); a workload may have changed between them.",
            baseline.agent_version, candidate.agent_version
        ));
    }
    gaps
}

/// Size and modification time of a file, as the cheapest available statement
/// that it is the same file it was.
///
/// Not a digest: hashing every bundle at every startup is precisely the scan
/// the index replaces. Not a proof either - a rewrite that preserves both is
/// possible - but a bundle is rewritten only by this agent, and it is the
/// bundle rather than the index that anything authoritative reads.
fn file_identity(path: &Path) -> (i64, i64) {
    let Ok(meta) = std::fs::metadata(path) else {
        return (0, 0);
    };
    let mtime = meta
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|since| since.as_nanos() as i64)
        .unwrap_or(0);
    (meta.len() as i64, mtime)
}

fn directory_bytes(path: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| entry.metadata().ok())
        .map(|meta| if meta.is_dir() { 0 } else { meta.len() })
        .sum()
}

/// The direction the *reference profile* declares for a metric, falling back to
/// the bundle's own claim only where there is no anchor.
///
/// The same rule the scoring model follows, and for the same reason: the
/// reference profile is this build's own data, and the bundle is written by the
/// machine under test. Trusting `metric.direction` here would have undone one
/// commit later exactly what removing it from `normalise` achieved - a bundle
/// relabelling `latency_fsync.mean` as higher-is-better would score 0.01x
/// correctly while `compare` rendered the same degradation as `+300%`, in the
/// one table an operator reads to decide whether something regressed.
///
/// The fallback is not a hole. A metric with no anchor contributes to no score
/// either (it is listed in `unreferenced_metrics`), so all that is at stake is
/// how its comparison row is oriented, and the bundle's claim is the only
/// statement about it that exists.
fn anchor_direction(
    model: &darcbench_scoring::ScoringModel,
    module_id: &str,
    metric_key: &str,
    declared: Direction,
) -> Direction {
    model
        .reference
        .get(module_id, metric_key)
        .map(|anchor| anchor.direction)
        .unwrap_or(declared)
}

fn direction_key(direction: Direction) -> &'static str {
    match direction {
        Direction::HigherIsBetter => "higher_is_better",
        Direction::LowerIsBetter => "lower_is_better",
    }
}

/// Stored as the protocol's own wire name, via the protocol's own serialiser.
///
/// A hand-written match here would be a second copy of the state list that
/// compiles until somebody adds a variant, and then classifies it by hand -
/// which is how an index quietly disagrees with the bundles it indexes.
fn result_state_key(state: ResultState) -> String {
    serde_json::to_value(state)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_else(|| "invalid".into())
}

fn result_state_from_key(key: &str) -> ResultState {
    // Unknown states read as `Invalid` rather than as the most generous option.
    // A row this build does not understand has not been validated by anything
    // it can check.
    serde_json::from_value(serde_json::Value::String(key.to_string()))
        .unwrap_or(ResultState::Invalid)
}

#[cfg(test)]
// In tests, `unwrap`/`expect` panicking *is* the failure signal.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic_in_result_fn)]
mod tests {
    use super::*;
    use darcbench_protocol::metrics::{Metric, ModuleResult, ModuleStatus};
    use darcbench_protocol::stats::Summary;
    use darcbench_protocol::{ModuleId, ModuleRef, Profile, RunState, Verdict};
    use darcbench_report::bundle::{BundleMeta, RunRecord, TelemetrySummary};

    fn bundle(total: Option<f64>, state: ResultState, metrics: Vec<Metric>) -> Bundle {
        let now = chrono::Utc::now();
        let module = ModuleRef {
            id: ModuleId::new("cpu.mixed").unwrap(),
            version: "1.0.0".into(),
        };
        let mut meta = BundleMeta::new("0.1.0-test");
        meta.build_profile = "release".into();
        let mut scores = darcbench_scoring::ScoringModel::current().score_run(Profile::Quick, &[]);
        scores.total = total;
        Bundle {
            meta,
            run: RunRecord {
                run_id: RunId::try_new().unwrap(),
                profile: Profile::Quick,
                state: RunState::Completed,
                started_at: now,
                finished_at: now,
                duration_ms: 1234,
                modules: vec![module.clone()],
                environment_digest: "sha256:machine".into(),
                events_digest: "sha256:events".into(),
                event_count: 5,
                stopped_because: None,
                guards_not_enforced: vec![],
                comparability_not_recorded: vec![],
            },
            environment: darcbench_inventory::Inventory::collect(),
            modules: vec![ModuleResult {
                module,
                status: ModuleStatus::Completed,
                cycle: 0,
                started_at: now,
                finished_at: now,
                duration_ms: 1234.0,
                metrics,
                warnings: vec![],
                error: None,
                context: Default::default(),
            }],
            scores,
            verdict: Verdict {
                state,
                reasons: vec![],
                validator_version: "dbv/0.1.0".into(),
            },
            telemetry: TelemetrySummary::default(),
            sustained_diagnosis: None,
            signature: None,
        }
    }

    fn metric(key: &str, value: f64, direction: Direction) -> Metric {
        Metric {
            key: key.into(),
            label: key.into(),
            value,
            unit: "ops/s".into(),
            direction,
            summary: Summary {
                n: 5,
                ..Default::default()
            },
            samples: vec![],
            outliers: vec![],
            measures_dispersion: false,
            tail_quantile: false,
        }
    }

    #[test]
    fn a_recorded_bundle_comes_back_without_opening_the_file() {
        let index = RunIndex::in_memory().unwrap();
        let bundle = bundle(
            Some(742.0),
            ResultState::Partial,
            vec![metric("single", 100.0, Direction::HigherIsBetter)],
        );
        index
            .record(&bundle, std::path::Path::new("/nonexistent/bundle.json"))
            .unwrap();

        let listed = index.list(10).unwrap();
        assert_eq!(listed.len(), 1);
        let run = &listed[0];
        assert_eq!(run.run_id, bundle.run.run_id.as_str());
        assert_eq!(run.total_score, Some(742.0));
        assert_eq!(run.result_state, ResultState::Partial);
        assert_eq!(run.modules, vec!["cpu.mixed@1.0.0".to_string()]);
        assert_eq!(index.get(run.run_id.as_str()).unwrap().as_ref(), Some(run));
    }

    /// Re-recording a run must replace it, not accumulate it. A rescore under a
    /// new model writes a new bundle for a run that already exists.
    #[test]
    fn recording_the_same_run_twice_replaces_rather_than_duplicates() {
        let index = RunIndex::in_memory().unwrap();
        let mut bundle = bundle(
            Some(100.0),
            ResultState::Local,
            vec![
                metric("a", 1.0, Direction::HigherIsBetter),
                metric("b", 2.0, Direction::HigherIsBetter),
            ],
        );
        index
            .record(&bundle, std::path::Path::new("/nonexistent/bundle.json"))
            .unwrap();

        bundle.scores.total = Some(200.0);
        bundle.modules[0].metrics.pop();
        index
            .record(&bundle, std::path::Path::new("/nonexistent/bundle.json"))
            .unwrap();

        assert_eq!(index.count().unwrap(), 1);
        assert_eq!(index.list(10).unwrap()[0].total_score, Some(200.0));
        assert_eq!(
            index.metrics_of(bundle.run.run_id.as_str()).unwrap().len(),
            1,
            "a metric the new bundle no longer carries must not survive in the index"
        );
    }

    /// The comparison's one non-obvious rule: a doubled latency is a
    /// regression, however the arithmetic falls out.
    #[test]
    fn comparison_ratios_are_direction_adjusted() {
        let index = RunIndex::in_memory().unwrap();
        let baseline = bundle(
            Some(500.0),
            ResultState::Local,
            vec![
                metric("throughput", 100.0, Direction::HigherIsBetter),
                metric("latency", 1.0, Direction::LowerIsBetter),
            ],
        );
        let mut candidate = bundle(
            Some(600.0),
            ResultState::Local,
            vec![
                metric("throughput", 200.0, Direction::HigherIsBetter),
                metric("latency", 2.0, Direction::LowerIsBetter),
            ],
        );
        candidate.run.environment_digest = baseline.run.environment_digest.clone();
        index
            .record(&baseline, std::path::Path::new("/nonexistent/bundle.json"))
            .unwrap();
        index
            .record(&candidate, std::path::Path::new("/nonexistent/bundle.json"))
            .unwrap();

        let comparison = index
            .compare(baseline.run.run_id.as_str(), candidate.run.run_id.as_str())
            .unwrap()
            .expect("both runs are indexed");

        let by_key: BTreeMap<_, _> = comparison
            .metrics
            .iter()
            .map(|d| (d.metric_key.as_str(), d.ratio))
            .collect();
        assert_eq!(
            by_key["throughput"], 2.0,
            "doubled throughput is twice as good"
        );
        assert_eq!(
            by_key["latency"], 0.5,
            "doubled latency is half as good, not twice as good"
        );
        assert!(
            comparison.comparable,
            "{:?}",
            comparison.incomparable_reasons
        );
    }

    /// A retention window computed from an incomplete list would delete the
    /// wrong run, so `prune` refuses instead.
    ///
    /// `list` drops a row it cannot read, which is right for a page someone is
    /// looking at and wrong for an operation with no undo: the window is
    /// positional, so an invisible row shifts it and `--keep-last 2` keeps one.
    #[test]
    fn prune_refuses_rather_than_delete_from_a_partial_view() {
        let index = RunIndex::in_memory().unwrap();
        let runs_dir = std::env::temp_dir().join(format!(
            "darcbench-partial-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));

        let mut ids = Vec::new();
        for _ in 0..3 {
            let b = bundle(
                Some(1000.0),
                ResultState::Local,
                vec![metric("m", 1.0, Direction::HigherIsBetter)],
            );
            let dir = runs_dir.join(b.run.run_id.as_str());
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("bundle.json"), b"{}").unwrap();
            index
                .record(&b, std::path::Path::new("/nonexistent/bundle.json"))
                .unwrap();
            ids.push(b.run.run_id.as_str().to_string());
        }

        // Make one row unreadable the way a schema change would: a value whose
        // type `row_to_run` cannot convert. SQLite's dynamic typing allows it.
        index
            .connection()
            .unwrap()
            .execute(
                "UPDATE runs SET duration_ms = 'not-a-number' WHERE run_id = ?1",
                [&ids[0]],
            )
            .unwrap();

        // The lenient path still works - a broken row must not blank the list.
        assert_eq!(
            index.list(usize::MAX).unwrap().len(),
            2,
            "list degrades to the rows it can read"
        );

        let outcome = index.prune(
            &runs_dir,
            RetentionPolicy {
                older_than_days: None,
                keep_last: Some(1),
            },
            false,
        );
        assert!(
            matches!(outcome, Err(IndexError::PartialView { unreadable: 1 })),
            "prune must refuse a partial view, got {outcome:?}"
        );
        for id in &ids {
            assert!(
                runs_dir.join(id).is_dir(),
                "a refused prune must delete nothing, but {id} is gone"
            );
        }

        let _ = std::fs::remove_dir_all(&runs_dir);
    }

    /// Two same-named categories built from different workloads are not the
    /// same measurement, and the comparison has to say so.
    ///
    /// The live case is Web: `php.runtime` is scored into it and exists only on
    /// a host with PHP installed, so two `web` runs on one machine can produce
    /// Web scores from different baskets while every other comparability fact
    /// matches.
    #[test]
    fn a_category_built_from_different_modules_is_not_comparable() {
        use darcbench_scoring::{CategoryKey, CategoryOutcome};

        let basket = |modules: &[&str]| CategoryOutcome {
            key: CategoryKey::Web,
            label: "Web".into(),
            score: 1000.0,
            weight: 0.2,
            metric_count: 4,
            modules: modules.iter().map(|m| m.to_string()).collect(),
        };

        let mut baseline = bundle(
            Some(1000.0),
            ResultState::Local,
            vec![metric("shared", 10.0, Direction::HigherIsBetter)],
        );
        baseline.scores.categories = vec![basket(&["php.runtime", "web.static"])];

        let mut candidate = bundle(
            Some(1000.0),
            ResultState::Local,
            vec![metric("shared", 10.0, Direction::HigherIsBetter)],
        );
        // Everything else about these two runs matches, so the basket is the
        // only thing that can make them incomparable.
        candidate.run.environment_digest = baseline.run.environment_digest.clone();
        candidate.scores.categories = vec![basket(&["web.static"])];

        let index = RunIndex::in_memory().unwrap();
        for b in [&baseline, &candidate] {
            index
                .record(b, std::path::Path::new("/nonexistent/bundle.json"))
                .unwrap();
        }

        let comparison = index
            .compare(baseline.run.run_id.as_str(), candidate.run.run_id.as_str())
            .unwrap()
            .unwrap();

        assert!(
            !comparison.comparable,
            "a differing basket must make the runs incomparable"
        );
        let reason = comparison
            .incomparable_reasons
            .iter()
            .find(|r| r.contains("Web"))
            .expect("the Web category must be named");
        assert!(
            reason.contains("php.runtime"),
            "the reason must name the workload that differs, not merely that one does: {reason}"
        );
        // The numbers are still produced. A comparison is labelled, never
        // withheld.
        assert_eq!(comparison.metrics.len(), 1);
    }

    /// A bundle written before the basket field existed records nothing, and
    /// "not recorded" must not be reported as "different".
    ///
    /// Otherwise every comparison between a run made before this field and one
    /// made after it - the most common comparison there is, for a while - would
    /// claim the categories were computed from different workloads, and say one
    /// of them used no modules at all.
    #[test]
    fn an_unrecorded_basket_is_not_a_difference() {
        use darcbench_scoring::{CategoryKey, CategoryOutcome};
        let mut baseline = bundle(
            Some(1000.0),
            ResultState::Local,
            vec![metric("shared", 10.0, Direction::HigherIsBetter)],
        );
        // As `#[serde(default)]` leaves it when an older bundle is read.
        baseline.scores.categories = vec![CategoryOutcome {
            key: CategoryKey::Web,
            label: "Web".into(),
            score: 1000.0,
            weight: 0.2,
            metric_count: 4,
            modules: vec![],
        }];
        let mut candidate = bundle(
            Some(1000.0),
            ResultState::Local,
            vec![metric("shared", 10.0, Direction::HigherIsBetter)],
        );
        candidate.run.environment_digest = baseline.run.environment_digest.clone();
        candidate.scores.categories = vec![CategoryOutcome {
            key: CategoryKey::Web,
            label: "Web".into(),
            score: 1000.0,
            weight: 0.2,
            metric_count: 4,
            modules: vec!["web.static".to_string()],
        }];

        let index = RunIndex::in_memory().unwrap();
        for b in [&baseline, &candidate] {
            index
                .record(b, std::path::Path::new("/nonexistent/bundle.json"))
                .unwrap();
        }
        let comparison = index
            .compare(baseline.run.run_id.as_str(), candidate.run.run_id.as_str())
            .unwrap()
            .unwrap();
        assert!(
            comparison.comparable,
            "an unrecorded basket must not be reported as a differing one: {:?}",
            comparison.incomparable_reasons
        );
    }

    /// Two runs whose baskets agree stay comparable.
    #[test]
    fn an_identical_basket_is_not_a_reason_to_refuse() {
        use darcbench_scoring::{CategoryKey, CategoryOutcome};
        let basket = CategoryOutcome {
            key: CategoryKey::Web,
            label: "Web".into(),
            score: 1000.0,
            weight: 0.2,
            metric_count: 4,
            modules: vec!["web.static".to_string()],
        };
        let mut baseline = bundle(
            Some(1000.0),
            ResultState::Local,
            vec![metric("shared", 10.0, Direction::HigherIsBetter)],
        );
        baseline.scores.categories = vec![basket.clone()];
        let mut candidate = bundle(
            Some(1000.0),
            ResultState::Local,
            vec![metric("shared", 12.0, Direction::HigherIsBetter)],
        );
        candidate.run.environment_digest = baseline.run.environment_digest.clone();
        candidate.scores.categories = vec![basket];

        let index = RunIndex::in_memory().unwrap();
        for b in [&baseline, &candidate] {
            index
                .record(b, std::path::Path::new("/nonexistent/bundle.json"))
                .unwrap();
        }
        let comparison = index
            .compare(baseline.run.run_id.as_str(), candidate.run.run_id.as_str())
            .unwrap()
            .unwrap();
        assert!(
            comparison.comparable,
            "{:?}",
            comparison.incomparable_reasons
        );
    }

    /// Metrics only one side has are named. A comparison that silently drops
    /// them looks complete while describing a subset.
    #[test]
    fn metrics_present_in_only_one_run_are_named_not_dropped() {
        let index = RunIndex::in_memory().unwrap();
        let baseline = bundle(
            None,
            ResultState::Local,
            vec![
                metric("shared", 10.0, Direction::HigherIsBetter),
                metric("baseline_only", 1.0, Direction::HigherIsBetter),
                metric("zero", 0.0, Direction::HigherIsBetter),
            ],
        );
        let mut candidate = bundle(
            None,
            ResultState::Local,
            vec![
                metric("shared", 20.0, Direction::HigherIsBetter),
                metric("candidate_only", 1.0, Direction::HigherIsBetter),
                metric("zero", 5.0, Direction::HigherIsBetter),
            ],
        );
        candidate.run.environment_digest = baseline.run.environment_digest.clone();
        index
            .record(&baseline, std::path::Path::new("/nonexistent/bundle.json"))
            .unwrap();
        index
            .record(&candidate, std::path::Path::new("/nonexistent/bundle.json"))
            .unwrap();

        let comparison = index
            .compare(baseline.run.run_id.as_str(), candidate.run.run_id.as_str())
            .unwrap()
            .unwrap();
        assert_eq!(comparison.metrics.len(), 1);
        assert_eq!(comparison.metrics[0].metric_key, "shared");
        assert_eq!(comparison.unmatched.len(), 3, "{:?}", comparison.unmatched);
        assert!(comparison
            .unmatched
            .iter()
            .any(|u| u.contains("baseline_only")));
        assert!(comparison
            .unmatched
            .iter()
            .any(|u| u.contains("candidate_only")));
        assert!(
            comparison.unmatched.iter().any(|u| u.contains("zero")),
            "a zero baseline has no ratio and must be said so rather than divided by"
        );
    }

    /// Different machines still compare, and say so.
    #[test]
    fn a_comparison_across_machines_is_produced_and_labelled() {
        let index = RunIndex::in_memory().unwrap();
        let baseline = bundle(
            Some(100.0),
            ResultState::Local,
            vec![metric("shared", 10.0, Direction::HigherIsBetter)],
        );
        let mut candidate = bundle(
            Some(200.0),
            ResultState::Local,
            vec![metric("shared", 20.0, Direction::HigherIsBetter)],
        );
        candidate.run.environment_digest = "sha256:a-different-machine".into();
        index
            .record(&baseline, std::path::Path::new("/nonexistent/bundle.json"))
            .unwrap();
        index
            .record(&candidate, std::path::Path::new("/nonexistent/bundle.json"))
            .unwrap();

        let comparison = index
            .compare(baseline.run.run_id.as_str(), candidate.run.run_id.as_str())
            .unwrap()
            .unwrap();
        assert!(!comparison.comparable);
        assert!(comparison.incomparable_reasons[0].contains("machines"));
        assert_eq!(
            comparison.metrics.len(),
            1,
            "the comparison is still produced; it is labelled, not withheld"
        );
    }

    #[test]
    fn comparing_an_unknown_run_is_none_not_an_error() {
        let index = RunIndex::in_memory().unwrap();
        assert!(index
            .compare("run_deadbeef", "run_cafebabe")
            .unwrap()
            .is_none());
    }

    /// The index is disposable: everything in it can be rebuilt from the
    /// bundles, which is what makes losing it a rebuild rather than a loss.
    #[test]
    fn reconcile_rebuilds_from_the_bundles_and_forgets_deleted_runs() {
        let root = std::env::temp_dir().join(format!(
            "darcbench-index-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        let runs = root.join("runs");
        std::fs::create_dir_all(&runs).unwrap();

        let bundle = bundle(
            Some(321.0),
            ResultState::Local,
            vec![metric("single", 5.0, Direction::HigherIsBetter)],
        );
        let dir = runs.join(bundle.run.run_id.as_str());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("bundle.json"),
            serde_json::to_vec(&bundle).unwrap(),
        )
        .unwrap();

        // A directory that is not a run id, and a run with no bundle yet: both
        // are ordinary and neither may end up in the index.
        std::fs::create_dir_all(runs.join("not-a-run-id")).unwrap();
        std::fs::create_dir_all(runs.join("run_00000000000000000000000000000000")).unwrap();

        let index = RunIndex::in_memory().unwrap();
        let outcome = index.reconcile(&runs).unwrap();
        assert_eq!(outcome.indexed, vec![bundle.run.run_id.to_string()]);
        assert_eq!(index.count().unwrap(), 1);
        assert_eq!(index.list(10).unwrap()[0].total_score, Some(321.0));

        // A second pass changes nothing: reconcile has to be idempotent, or
        // every startup would rewrite the whole index.
        assert!(index.reconcile(&runs).unwrap().is_noop());

        // An operator clearing space must not leave a list full of runs that no
        // longer exist.
        std::fs::remove_dir_all(&dir).unwrap();
        let outcome = index.reconcile(&runs).unwrap();
        assert_eq!(outcome.forgotten, vec![bundle.run.run_id.to_string()]);
        assert_eq!(index.count().unwrap(), 0);

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A live-recorded row must not claim to be current forever.
    #[test]
    fn a_bundle_replaced_after_it_was_recorded_is_re_read_not_trusted() {
        // The defect: `record` stored no file identity for a normally completed
        // run, and `is_current` read that missing identity as "current". The row
        // then described the bundle for the life of the index, so a bundle
        // edited or replaced later kept serving its old verdict and metrics -
        // and `prune`, which refuses to delete an `Invalid` run, would consult
        // that stale row and delete a bundle that had since become invalid.
        let root = std::env::temp_dir().join(format!(
            "darcbench-index-stale-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        let runs = root.join("runs");
        std::fs::create_dir_all(&runs).unwrap();

        let first = bundle(
            Some(100.0),
            ResultState::Local,
            vec![metric("single", 1.0, Direction::HigherIsBetter)],
        );
        let dir = runs.join(first.run.run_id.as_str());
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bundle.json");
        std::fs::write(&path, serde_json::to_vec(&first).unwrap()).unwrap();

        // Recorded the way a finished run is: straight from memory, with the
        // path of the file just written.
        let index = RunIndex::in_memory().unwrap();
        index.record(&first, &path).unwrap();
        assert_eq!(index.list(10).unwrap()[0].total_score, Some(100.0));

        // Nothing changed on disk, so reconciliation must not re-read it.
        assert!(
            index.reconcile(&runs).unwrap().is_noop(),
            "an unchanged bundle was re-read; reconcile is no longer idempotent"
        );

        // Now the bundle on disk is replaced, keeping the same run id.
        let mut replaced = bundle(
            Some(999.0),
            ResultState::Invalid,
            vec![metric("single", 2.0, Direction::HigherIsBetter)],
        );
        replaced.run.run_id = first.run.run_id.clone();
        // Written until its identity actually differs. Size alone would do
        // here, but a same-size edit within one mtime tick is exactly the case
        // a naive check misses, so the test does not rely on the timing.
        std::fs::write(&path, serde_json::to_vec_pretty(&replaced).unwrap()).unwrap();

        let outcome = index.reconcile(&runs).unwrap();
        assert!(
            !outcome.is_noop(),
            "the replaced bundle was not noticed: the row still claims to be current"
        );
        let row = &index.list(10).unwrap()[0];
        assert_eq!(row.total_score, Some(999.0), "the stale score survived");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Retention never deletes evidence of a failure, and never deletes
    /// anything at all without being told what to select.
    #[test]
    fn pruning_keeps_invalid_runs_and_refuses_an_empty_policy() {
        let root = std::env::temp_dir().join(format!(
            "darcbench-prune-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        let runs = root.join("runs");
        std::fs::create_dir_all(&runs).unwrap();

        let index = RunIndex::in_memory().unwrap();
        let mut ids = Vec::new();
        for (position, state) in [
            ResultState::Local,
            ResultState::Invalid,
            ResultState::Local,
            ResultState::Local,
        ]
        .into_iter()
        .enumerate()
        {
            let mut b = bundle(Some(1.0), state, vec![]);
            // Newest first in `list`, so descending timestamps make the order
            // deterministic regardless of how fast the loop runs.
            b.run.finished_at = chrono::Utc::now() - chrono::Duration::minutes(position as i64);
            let dir = runs.join(b.run.run_id.as_str());
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("bundle.json"), b"{}").unwrap();
            index
                .record(&b, std::path::Path::new("/nonexistent/bundle.json"))
                .unwrap();
            ids.push(b.run.run_id.to_string());
        }

        // No policy selects nothing, whatever is on disk.
        let nothing = index
            .prune(&runs, RetentionPolicy::default(), false)
            .unwrap();
        assert_eq!(nothing, PruneOutcome::default());
        assert_eq!(index.count().unwrap(), 4);

        // A dry run reports and touches nothing.
        let policy = RetentionPolicy {
            older_than_days: None,
            keep_last: Some(1),
        };
        let planned = index.prune(&runs, policy, true).unwrap();
        assert_eq!(planned.removed.len(), 2, "{planned:?}");
        assert_eq!(planned.retained_as_evidence, vec![ids[1].clone()]);
        assert_eq!(index.count().unwrap(), 4, "a dry run must delete nothing");
        assert!(runs.join(&ids[3]).exists());

        let done = index.prune(&runs, policy, false).unwrap();
        assert_eq!(done.removed, planned.removed);
        assert_eq!(index.count().unwrap(), 2, "the invalid run is kept");
        assert!(runs.join(&ids[0]).exists(), "the newest run is kept");
        assert!(
            runs.join(&ids[1]).exists(),
            "an invalid run's directory must survive too, not only its row"
        );
        assert!(!runs.join(&ids[3]).exists());

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A database from a future agent is refused rather than half-read.
    #[test]
    fn a_newer_schema_is_refused_not_guessed_at() {
        let connection = Connection::open_in_memory().unwrap();
        RunIndex::prepare(&connection, Path::new(":memory:")).unwrap();
        connection
            .pragma_update(None, "user_version", SCHEMA_VERSION + 1)
            .unwrap();
        let error = RunIndex::prepare(&connection, Path::new(":memory:")).unwrap_err();
        assert!(
            matches!(error, IndexError::FutureSchema { .. }),
            "got {error:?}"
        );
        assert!(error.to_string().contains("source of truth"));
    }
}
