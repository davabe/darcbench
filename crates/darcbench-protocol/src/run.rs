//! Run lifecycle, profiles, and the result-trust model.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::ids::{ModuleId, RunId};
use crate::ProtocolError;

/// Lifecycle state of a run inside the agent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    Created,
    Preflight,
    Running,
    Finalizing,
    Completed,
    Failed,
    Cancelled,
}

impl RunState {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

/// A named, versioned bundle of modules and run parameters.
///
/// Profiles are the unit of comparability: two runs may only be compared
/// directly when their profile *and* scoring model version match. See
/// `docs/BENCHMARK-METHODOLOGY.md` section "Comparability rules".
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Profile {
    Quick,
    Standard,
    Deep,
    Endurance,
    ReadOnly,
    WebOnly,
    Custom,
}

impl Profile {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Quick => "quick",
            Self::Standard => "standard",
            Self::Deep => "deep",
            Self::Endurance => "endurance",
            Self::ReadOnly => "read_only",
            Self::WebOnly => "web_only",
            Self::Custom => "custom",
        }
    }

    /// Whether runs of this profile are eligible for the public leaderboard.
    ///
    /// `Custom` never is: a user-selected module subset is not comparable with
    /// anything, and letting it onto a leaderboard is the single easiest way to
    /// game a benchmark suite.
    /// Whether a run of this profile may claim a comparable total score.
    ///
    /// `ReadOnly` is deliberately excluded alongside `Custom`. It exists so an
    /// operator can measure a machine they are unwilling to write to, which is
    /// a legitimate and useful thing to want - but a run that cannot measure
    /// write throughput, write latency or fsync cost has not measured storage,
    /// and `docs/BENCHMARK-METHODOLOGY.md` is explicit that a read-only storage
    /// profile "is **not** treated as equivalent to a full storage score".
    /// Enforcing that here makes it structural: the verdict comes out `Custom`
    /// and no amount of downstream wiring can promote it.
    pub fn is_standard(self) -> bool {
        !matches!(self, Self::Custom | Self::ReadOnly)
    }

    /// Human-facing estimate used by the preflight screen.
    pub fn nominal_duration_minutes(self) -> (u32, u32) {
        match self {
            Self::Quick => (3, 6),
            Self::Standard => (10, 20),
            Self::Deep => (30, 60),
            Self::Endurance => (ENDURANCE_DEFAULT_MINUTES, ENDURANCE_MAX_MINUTES),
            Self::ReadOnly => (4, 8),
            Self::WebOnly => (8, 15),
            Self::Custom => (0, 0),
        }
    }

    /// How long a run of this profile keeps repeating its module set.
    ///
    /// `None` means one pass and stop, which is every profile but one. Endurance
    /// is the exception by definition: what it measures is what happens *after*
    /// the first few minutes, so it repeats the whole module set in cycles until
    /// the target elapses.
    pub fn cycle_target_minutes(self) -> Option<u32> {
        matches!(self, Self::Endurance).then_some(ENDURANCE_DEFAULT_MINUTES)
    }
}

/// How long an endurance run lasts unless the operator overrides it.
///
/// One hour, which is the low end of the range `nominal_duration_minutes`
/// has always advertised, and it is chosen from the phenomenon rather than
/// from taste. `docs/MARKET-RESEARCH.md` calls burstable instances "the single
/// strongest argument for the endurance profile", and a T-series instance at
/// full load takes tens of minutes to spend a credit balance it accumulated
/// over hours. A twenty-minute endurance run would measure the credits and
/// call it the machine - the exact error the profile exists to prevent.
///
/// It is a constant rather than a preference because profile is the unit of
/// comparability: two endurance runs of different lengths measure different
/// amounts of decline, and averaging them would be meaningless. An operator who
/// wants a different length gets one, and gets a `Custom` verdict with it.
pub const ENDURANCE_DEFAULT_MINUTES: u32 = 60;

/// Hard ceiling on any run's wall clock, override included.
///
/// Twenty-four hours matches the documented upper bound of the endurance range.
/// It exists because this software runs on servers belonging to other people:
/// a mistyped override must not be able to hold a production machine at full
/// load for a week.
pub const ENDURANCE_MAX_MINUTES: u32 = 24 * 60;

/// Shortest override worth accepting.
///
/// Below a few minutes the cycles are too short and too few for retention to
/// mean anything, and the run would publish a decline figure derived from two
/// samples taken a minute apart. Kept low enough to stay testable.
pub const ENDURANCE_MIN_MINUTES: u32 = 2;

impl FromStr for Profile {
    type Err = ProtocolError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().replace('-', "_").as_str() {
            "quick" => Ok(Self::Quick),
            "standard" => Ok(Self::Standard),
            "deep" => Ok(Self::Deep),
            "endurance" => Ok(Self::Endurance),
            "read_only" | "readonly" => Ok(Self::ReadOnly),
            "web_only" | "web" => Ok(Self::WebOnly),
            "custom" => Ok(Self::Custom),
            other => Err(ProtocolError::InvalidId(format!(
                "unknown profile `{other}`"
            ))),
        }
    }
}

impl fmt::Display for Profile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How much a consumer of a result is entitled to trust it.
///
/// The ladder is deliberately conservative: an agent running on a machine its
/// operator fully controls can never be more than `SelfReported`, because the
/// operator can patch the binary. Higher tiers require evidence produced
/// somewhere the operator does not control.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResultState {
    /// Never left the machine. No claims made.
    Local,
    /// Uploaded with a valid agent signature, nothing else checked.
    SelfReported,
    /// Server recomputed every score from raw metrics and all invariants held.
    Validated,
    /// Validated, plus the run carried a server-issued nonce and run token, and
    /// the agent build hash matched a published release.
    Verified,
    /// Verified, plus executed under DARCBench-controlled provisioning.
    Official,
    /// Failed validation. Retained (deletion would hide evidence) but excluded
    /// from every aggregate.
    Invalid,
    /// Some required modules did not complete; subscores may still be shown.
    Partial,
    /// Non-standard module set or parameters. Never comparable, never ranked.
    Custom,
}

impl ResultState {
    /// Whether this state may appear in leaderboards and provider aggregates.
    pub fn is_rankable(self) -> bool {
        matches!(self, Self::Validated | Self::Verified | Self::Official)
    }
}

/// The outcome of server-side (or local) validation of a run.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Verdict {
    pub state: ResultState,
    pub reasons: Vec<VerdictReason>,
    /// Version of the validation ruleset that produced this verdict.
    pub validator_version: String,
}

/// A single, typed reason a run was downgraded or invalidated.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerdictReason {
    RequiredModuleMissing {
        module: ModuleId,
    },
    ModuleFailed {
        module: ModuleId,
    },
    ExcessiveVariance {
        module: ModuleId,
    },
    /// A module ran, produced real measurements, and declared them not
    /// comparable - a working set the machine could not afford, a repetition
    /// too short to trust, a validation condition the module itself defined.
    ///
    /// Distinct from [`Self::ExcessiveVariance`] because a reader has to act on
    /// them differently: high variance means run it again on a quieter machine,
    /// while a declared validation failure means this machine cannot produce a
    /// comparable number for that module at all.
    ModuleDegraded {
        module: ModuleId,
        reason: String,
    },
    LoadGeneratorSaturated {
        module: ModuleId,
    },
    EnvironmentChangedMidRun,
    ClockAnomaly,
    ErrorRateExceeded,
    InsufficientDiskSpace,
    Interrupted,
    MissingMetadata {
        field: String,
    },
    CustomProfile,
    IncompatibleBenchmarkVersion {
        found: String,
        expected: String,
    },
    SignatureInvalid,
    ScoreRecomputationMismatch,
    ReplayDetected,
}

/// Compact description of a run, used by list endpoints and the CLI.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RunSummary {
    pub run_id: RunId,
    pub profile: Profile,
    pub state: RunState,
    pub created_at: chrono::DateTime<chrono::Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<chrono::DateTime<chrono::Utc>>,
    pub modules: Vec<ModuleId>,
    /// Fraction in `[0, 1]`. Derived from completed modules, not from time.
    pub progress: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_score: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_state: Option<ResultState>,
}

#[cfg(test)]
// In tests, `unwrap`/`expect` panicking *is* the failure signal.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic_in_result_fn)]
mod tests {
    use super::*;

    #[test]
    fn custom_profile_is_never_standard() {
        assert!(!Profile::Custom.is_standard());
        assert!(Profile::Standard.is_standard());
    }

    #[test]
    fn profile_parses_common_spellings() {
        assert_eq!(
            "read-only".parse::<Profile>().expect("p"),
            Profile::ReadOnly
        );
        assert_eq!("WEB".parse::<Profile>().expect("p"), Profile::WebOnly);
        assert!("nonsense".parse::<Profile>().is_err());
    }

    #[test]
    fn only_server_checked_states_are_rankable() {
        assert!(!ResultState::Local.is_rankable());
        assert!(!ResultState::SelfReported.is_rankable());
        assert!(!ResultState::Custom.is_rankable());
        assert!(!ResultState::Invalid.is_rankable());
        assert!(!ResultState::Partial.is_rankable());
        assert!(ResultState::Validated.is_rankable());
        assert!(ResultState::Official.is_rankable());
    }

    #[test]
    fn terminal_states() {
        assert!(RunState::Completed.is_terminal());
        assert!(RunState::Cancelled.is_terminal());
        assert!(!RunState::Running.is_terminal());
    }
}
