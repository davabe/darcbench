//! The two-machine load-generation session.
//!
//! Every HTTP measurement DARCBench takes so far runs the load generator and
//! the origin in the same process on the same machine. That is the safe
//! default and it is what `docs/THREAT-MODEL.md` T-AMPLIFY requires, but on
//! loopback it costs accuracy: serving a 1 KiB object takes microseconds, so
//! the generator's own per-request work is comparable to the work being
//! measured, and the two compete for the same cores. `web.static` had to drop
//! its latency phase from the conventional 70% of capacity to a quarter of it
//! and publish the share it actually offered, because one machine cannot be
//! asked for 170% of itself.
//!
//! The fix is to put the generator on a second machine. This module is the
//! wire contract that makes that possible without turning the agent into a
//! tool for loading somebody else's server.
//!
//! # The consent problem
//!
//! T-AMPLIFY is permanent and reads *"HTTP load generation targets only a
//! server the agent started"*. A mode that accepts a host and a port appears
//! to break it: nothing stops a binary from opening a socket to an arbitrary
//! address, and no amount of protocol design can make a general-purpose
//! computer unable to send HTTP requests.
//!
//! So the property this module actually provides is narrower, and stating it
//! precisely matters more than stating it grandly:
//!
//! > The generator will not begin a measurement, and will not produce a
//! > result, unless the peer proves it is a DARCBench target that was
//! > deliberately started to be loaded, by an operator who transported a
//! > 256-bit secret out of band.
//!
//! A stranger's web server cannot satisfy that, because a stranger's web
//! server does not implement [`SessionOffer`] and does not know the token. The
//! handshake is therefore a *consent* mechanism, not an *access control* one:
//! it does not stop a determined attacker from writing their own load
//! generator, it stops this one from being usable as theirs, and it stops an
//! operator from accidentally pointing a benchmark at production.
//!
//! # The fabrication problem
//!
//! The generator is now a different machine, run by whoever holds the token,
//! and it reports numbers about a machine it does not own. A result bundle
//! that simply believed it would be worthless: the interesting figure is
//! throughput, and throughput is exactly what a dishonest generator would
//! inflate.
//!
//! So the target does not take the generator's word for it. The origin counts
//! every request it answers, and [`SessionReport::reconcile`] compares the
//! generator's claim against that count. A generator that claims more work
//! than the origin performed is rejected outright; one that claims materially
//! less is rejected too, because the extra requests came from somewhere and a
//! measurement taken while a third party was also loading the origin is not a
//! measurement of anything.
//!
//! That is one honest bound, and it is worth being clear about what it does
//! not cover: it proves the *count* of requests, not the *timing*. A
//! generator that really issued a million requests and then reported a
//! flattering latency distribution for them would pass. Latency figures from
//! an external generator are therefore trusted exactly as far as the operator
//! running it is - which, since both machines are the operator's own, is
//! usually far enough, and is the reason this mode is opt-in and disclosed in
//! the bundle rather than the default.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::ProtocolError;

/// Version of the external-session wire contract.
///
/// Format matches the rest of the crate: `darcbench.external/<major>`. Both
/// ends refuse a major they do not speak rather than guessing, because the
/// failure mode of guessing is a benchmark that reports numbers from a
/// half-understood conversation.
pub const EXTERNAL_PROTOCOL_VERSION: &str = "darcbench.external/1";

/// Bytes of entropy in a session token.
const TOKEN_BYTES: usize = 32;

/// Longest session an operator can ask for.
///
/// Four hours. Long enough for an endurance run with a wide margin, short
/// enough that a token pasted into a terminal and forgotten is not a listener
/// left open on a datacentre network for a week. The target enforces it; the
/// generator only reads what it was told.
pub const MAX_SESSION_SECS: u64 = 4 * 60 * 60;

/// Shortest session worth starting.
pub const MIN_SESSION_SECS: u64 = 30;

/// Tolerance on the reconciliation between claimed and served request counts.
///
/// Not zero, and the reason is not slack. The generator counts a request as
/// completed when it has read the whole body; the origin counts it when it
/// begins writing the response. At the moment a phase ends there are
/// necessarily a few requests the origin has counted and the generator has not
/// yet finished reading, bounded by the number of connections in flight - the
/// origin's own ceiling on those is 512.
///
/// A bound proportional to the total would grow with the run and let a large
/// run hide a large lie, so this is an absolute count rather than a
/// percentage.
///
/// **It is a bound on requests in flight and on nothing else.** Everything a
/// generator issues that it does not report lands in the same gap and is
/// indistinguishable from a third party - which is why
/// [`ShapeReport::requests_warmup`] exists and why a generator that discards a
/// phase must report it anyway. Widening this constant to cover unreported
/// work would be widening the hole a fabricated report walks through.
pub const RECONCILE_SLACK_REQUESTS: u64 = 1024;

/// Session time reserved for everything that is not a measured phase.
///
/// Connecting, warming up, and posting the report at the end. Ten seconds,
/// which is generous against a report that is a few kilobytes and mean against
/// a session that is minutes long. It exists because a budget that spent the
/// whole session on measurement would expire during the submission - losing a
/// run that had already succeeded, which is the worst moment to lose one.
pub const SESSION_OVERHEAD_MS: u64 = 10_000;

/// Shortest phase worth measuring.
///
/// A second. Below this a phase is measuring connection setup and scheduler
/// wake-up rather than a server, so a session too short to give every phase
/// this much is refused rather than clamped down to it. Granting a token phase
/// would produce a number, and the number would be about nothing - which is the
/// failure this whole crate is arranged against.
pub const MIN_PHASE_MS: u64 = 1_000;

// ---------------------------------------------------------------------------
// The token
// ---------------------------------------------------------------------------

/// The shared secret that makes a session a session.
///
/// Generated by the target, printed once for a human to carry to the generator
/// machine, and required on every request the target will answer. It is the
/// whole of the consent mechanism described in the module documentation.
///
/// `Debug` is implemented by hand and prints nothing but a marker. A token in
/// a log line is a token in a log aggregator, and this type exists to be
/// carried through code that logs liberally.
#[derive(Clone, PartialEq, Eq)]
pub struct SessionToken([u8; TOKEN_BYTES]);

impl SessionToken {
    /// A fresh token from the OS entropy source.
    pub fn try_new() -> Result<Self, ProtocolError> {
        let mut raw = [0u8; TOKEN_BYTES];
        getrandom::getrandom(&mut raw)
            .map_err(|error| ProtocolError::InvalidId(format!("entropy unavailable: {error}")))?;
        Ok(Self(raw))
    }

    /// Compares without an early exit on the first differing byte.
    ///
    /// The claim here is deliberately modest. The loop has no data-dependent
    /// branch and no data-dependent memory access, which is what removes the
    /// obvious timing signal; it is not a proof, because nothing short of an
    /// audited constant-time primitive is, and this workspace is not going to
    /// grow a dependency for it.
    ///
    /// The property the design actually rests on is that the token is 256
    /// random bits and a wrong one closes the connection. Extracting it a byte
    /// at a time from network timing would need an implausible number of
    /// probes against a listener that exists for the length of one benchmark
    /// run - and an attacker who can already run that many probes against the
    /// operator's machine has better things to do.
    #[must_use]
    pub fn matches(&self, other: &Self) -> bool {
        let mut difference = 0u8;
        for (a, b) in self.0.iter().zip(other.0.iter()) {
            difference |= a ^ b;
        }
        difference == 0
    }

    /// The value a human copies between machines.
    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }
}

impl FromStr for SessionToken {
    type Err = ProtocolError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let raw = hex::decode(s.trim())
            .map_err(|_| ProtocolError::InvalidId("token is not hexadecimal".to_string()))?;
        let sized: [u8; TOKEN_BYTES] = raw.as_slice().try_into().map_err(|_| {
            ProtocolError::InvalidId(format!(
                "token must be {TOKEN_BYTES} bytes ({} hex characters), got {}",
                TOKEN_BYTES * 2,
                raw.len()
            ))
        })?;
        Ok(Self(sized))
    }
}

impl fmt::Debug for SessionToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SessionToken(<redacted>)")
    }
}

// ---------------------------------------------------------------------------
// The offer
// ---------------------------------------------------------------------------

/// What the target announces about itself when a generator presents a valid
/// token.
///
/// This is the only thing the generator learns before it starts, and it is
/// deliberately everything it needs: what is served, on what terms, and until
/// when. A generator that had to discover the object sizes by probing would be
/// making requests before the handshake concluded, which is the thing the
/// handshake exists to prevent.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SessionOffer {
    /// Always [`EXTERNAL_PROTOCOL_VERSION`]. Checked first, before anything
    /// else in this struct is believed.
    pub protocol: String,
    /// Build of the agent on the target, so a mismatch is visible in the
    /// bundle rather than surfacing as an unexplained difference.
    pub agent_version: String,
    /// Identifies this session in the target's logs and in the report.
    pub session_id: String,
    /// Object sizes the origin will serve, in bytes.
    pub object_sizes: Vec<u64>,
    /// Whether the origin terminates TLS.
    pub tls: bool,
    /// Hex DER of the origin's certificate, when `tls`.
    ///
    /// The certificate is self-signed and per-run, so no CA can vouch for it
    /// and the generator must pin it. The whole certificate rather than a
    /// digest of it, because pinning by digest needs a custom `rustls`
    /// verifier while pinning by certificate is a root store containing
    /// exactly one entry - which is stronger (exact bytes, not a hash) and is
    /// the code path the in-process modules already use. A few hundred bytes
    /// in a ticket a human pastes once is not a cost worth optimising.
    pub certificate_der: Option<String>,
    /// How long the session has left, from the moment the offer was written.
    ///
    /// Relative on purpose. The two machines have independent clocks, and an
    /// absolute timestamp would make a session fail or overrun by whatever
    /// their skew happens to be. A duration is interpreted against the
    /// receiver's own monotonic clock, which is the only clock either end can
    /// trust.
    pub expires_in_ms: u64,
    /// Longest phase the target will accept.
    pub max_duration_ms: u64,
    /// Highest request rate the target will accept being asked for.
    pub max_rate_per_s: f64,
    /// Highest number of concurrent connections the target will accept.
    pub max_workers: u32,
}

/// Why a generator refused to proceed against a peer.
///
/// Every variant is a refusal to *measure*, not a warning on a measurement.
/// There is no degraded path here: a generator that could not establish what
/// it is talking to has nothing to report.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    #[error(
        "the peer did not answer with a DARCBench session offer, so it is not a target this \
         agent started and will not be loaded: {detail}"
    )]
    NotATarget { detail: String },
    #[error(
        "the target speaks {found} and this build speaks {expected}; refusing rather than \
         guessing at a half-understood conversation"
    )]
    UnsupportedProtocol { found: String, expected: String },
    #[error("the session expired before the generator was ready")]
    Expired,
    #[error(
        "the session has {remaining_ms} ms left, which across {phases} phase(s) leaves \
         {per_phase_ms} ms each - below the {MIN_PHASE_MS} ms floor. A phase that short measures \
         connection setup and scheduler wake-up rather than a server, so this is refused rather \
         than run: start the target again with a longer --minutes"
    )]
    SessionTooShort {
        remaining_ms: u64,
        phases: u32,
        per_phase_ms: u64,
    },
    #[error("the target offered no object sizes, so there is nothing to request")]
    NothingServed,
    #[error("the target declared TLS without a certificate fingerprint to pin")]
    UnpinnableCertificate,
    #[error("the target declared a nonsensical ceiling: {detail}")]
    ImplausibleOffer { detail: String },
    #[error(
        "refusing to start: {detail}. A load plan this agent cannot make sense of is a typo, and \
         the safe reading of a typo is not \"as fast as the other machine will allow\""
    )]
    ImplausibleRequest { detail: String },
}

/// What a generator wants to do, before the target's ceilings are applied.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LoadRequest {
    pub rate_per_s: f64,
    /// Duration of **one** phase.
    pub duration_ms: u64,
    pub workers: u32,
    /// How many phases the generator intends to run back to back.
    ///
    /// Without this the expiry clamp below checks one phase against the whole
    /// remaining session, and a generator running four of them sequentially
    /// overruns: a one-minute ticket driven at thirty seconds a shape schedules
    /// two minutes of work, so the target shuts down around the third shape and
    /// the report has nowhere to go. The session's lifetime is a budget shared
    /// by every phase, and only the generator knows how many it will run.
    pub phases: u32,
}

/// A load plan both ends have agreed to, and the reasons it differs from what
/// was asked for.
#[derive(Clone, Debug, PartialEq)]
pub struct AcceptedSession {
    pub session_id: String,
    pub object_sizes: Vec<u64>,
    pub tls: bool,
    pub certificate_der: Option<String>,
    pub granted: LoadRequest,
    /// Human-readable notes on every ceiling that bound.
    ///
    /// Carried into the bundle. A run that silently got a tenth of the load it
    /// asked for and reported the resulting latency as if it were the load
    /// requested is the same defect as coordinated omission, arriving by a
    /// different route.
    pub clamps: Vec<String>,
}

impl SessionOffer {
    /// Decides whether to talk to this peer at all, and on what terms.
    ///
    /// Order matters and is not arbitrary: the protocol version is checked
    /// before any other field is read, because a field's meaning is defined by
    /// the version that declared it.
    pub fn accept(&self, wanted: LoadRequest) -> Result<AcceptedSession, Refusal> {
        if self.protocol != EXTERNAL_PROTOCOL_VERSION {
            return Err(Refusal::UnsupportedProtocol {
                found: self.protocol.clone(),
                expected: EXTERNAL_PROTOCOL_VERSION.to_string(),
            });
        }
        if self.expires_in_ms == 0 {
            return Err(Refusal::Expired);
        }
        if self.object_sizes.is_empty() {
            return Err(Refusal::NothingServed);
        }
        if self.tls && self.certificate_der.is_none() {
            return Err(Refusal::UnpinnableCertificate);
        }
        if !self.max_rate_per_s.is_finite() || self.max_rate_per_s <= 0.0 {
            return Err(Refusal::ImplausibleOffer {
                detail: format!("max rate {}", self.max_rate_per_s),
            });
        }
        if self.max_duration_ms == 0 || self.max_workers == 0 {
            return Err(Refusal::ImplausibleOffer {
                detail: format!(
                    "max duration {} ms, max workers {}",
                    self.max_duration_ms, self.max_workers
                ),
            });
        }

        let mut clamps = Vec::new();
        let mut granted = wanted;

        // Refused, not clamped, and specifically not clamped *upward*. This
        // used to substitute the target's ceiling, so an operator who typed a
        // rate wrong got the most aggressive load the peer would permit. For a
        // tool whose threat model is about not overloading machines, that is
        // failing open in the wrong direction; a typo should stop the run.
        if !granted.rate_per_s.is_finite() || granted.rate_per_s <= 0.0 {
            return Err(Refusal::ImplausibleRequest {
                detail: format!("a rate of {} is not a rate", granted.rate_per_s),
            });
        }
        if granted.rate_per_s > self.max_rate_per_s {
            clamps.push(format!(
                "requested {:.0} req/s, target permits {:.0}",
                granted.rate_per_s, self.max_rate_per_s
            ));
            granted.rate_per_s = self.max_rate_per_s;
        }

        if granted.duration_ms > self.max_duration_ms {
            clamps.push(format!(
                "requested {} ms, target permits {}",
                granted.duration_ms, self.max_duration_ms
            ));
            granted.duration_ms = self.max_duration_ms;
        }
        if granted.duration_ms == 0 {
            clamps.push("requested a zero-length phase; using the target's ceiling".to_string());
            granted.duration_ms = self.max_duration_ms;
        }

        if granted.workers > self.max_workers {
            clamps.push(format!(
                "requested {} connections, target permits {}",
                granted.workers, self.max_workers
            ));
            granted.workers = self.max_workers;
        }
        if granted.workers == 0 {
            clamps.push("requested zero connections; using one".to_string());
            granted.workers = 1;
        }

        // The session must outlast *every* phase plus the overhead between
        // them, or the target stops answering half way through and the
        // shortfall is recorded as the machine being slow. Checked after the
        // duration clamp, so it sees the real length.
        let phases = granted.phases.max(1);
        granted.phases = phases;
        // Reserved for what the phases themselves do not account for:
        // connecting, warming up, and posting the report at the end. A budget
        // that spent the whole session on measurement would expire during the
        // submission, which loses a run that had already succeeded.
        let budget = self
            .expires_in_ms
            .saturating_sub(SESSION_OVERHEAD_MS)
            .max(1);
        let per_phase = budget / u64::from(phases);
        if per_phase < MIN_PHASE_MS {
            return Err(Refusal::SessionTooShort {
                remaining_ms: self.expires_in_ms,
                phases,
                per_phase_ms: per_phase,
            });
        }
        if per_phase < granted.duration_ms {
            clamps.push(format!(
                "session has {} ms left and the generator will run {phases} phase(s), so each \
                 gets {per_phase} ms rather than the {} ms requested",
                self.expires_in_ms, granted.duration_ms
            ));
            granted.duration_ms = per_phase;
        }

        Ok(AcceptedSession {
            session_id: self.session_id.clone(),
            object_sizes: self.object_sizes.clone(),
            tls: self.tls,
            certificate_der: self.certificate_der.clone(),
            granted,
            clamps,
        })
    }
}

// ---------------------------------------------------------------------------
// The report
// ---------------------------------------------------------------------------

/// A latency distribution, reduced to what a metric needs.
///
/// Summarised on the generator rather than shipped raw: an endurance phase at
/// ten thousand requests a second is tens of millions of samples, and posting
/// them across a network to compute percentiles that the generator has already
/// computed would be a large transfer to arrive at the same numbers.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct LatencySummary {
    pub count: u64,
    pub mean_ms: f64,
    pub p50_ms: f64,
    pub p90_ms: f64,
    pub p99_ms: f64,
    pub max_ms: f64,
}

/// What one load phase produced, as the generator saw it.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ShapeReport {
    /// Matches the shape name the target's module knows, e.g. `small.keepalive`.
    pub shape: String,
    pub object_bytes: u64,
    pub requests_scheduled: u64,
    pub requests_completed: u64,
    pub requests_failed: u64,
    /// Requests issued before recording began, and discarded.
    ///
    /// Reported rather than dropped, and counted by [`Self::claimed`]. The
    /// origin answered them like any other, so a warm-up the report did not
    /// mention would appear on the target as load from a third party and
    /// reject an entirely honest run - blaming a stranger who was never there.
    #[serde(default)]
    pub requests_warmup: u64,
    pub error_examples: Vec<String>,
    pub bytes: u64,
    pub offered_rate_per_s: f64,
    pub achieved_rate_per_s: f64,
    /// Completion minus actual send. What the target took.
    pub service_ms: LatencySummary,
    /// Completion minus *due* time: the coordinated-omission-corrected series,
    /// and the one any latency metric must be built from.
    pub response_ms: LatencySummary,
    /// `None` when the generator held its schedule.
    pub saturation: Option<String>,
    pub generator_cpu_pct: f64,
}

/// Who generated the load, for the record.
///
/// A slow external generator produces a low number for a fast target, and an
/// operator reading the bundle a year later needs to be able to tell which
/// machine the figure describes. None of this is trusted for anything; it is
/// disclosure.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct GeneratorIdentity {
    pub agent_version: String,
    pub os: String,
    pub arch: String,
    pub cpus: u32,
}

/// Everything the generator sends back when it is done.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SessionReport {
    pub protocol: String,
    pub session_id: String,
    pub generator: GeneratorIdentity,
    pub shapes: Vec<ShapeReport>,
    /// Ceilings that bound, carried from [`AcceptedSession::clamps`] so the
    /// target's bundle records them without having to be told twice.
    pub clamps: Vec<String>,
}

/// Why a target threw a generator's report away.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ReportRejected {
    #[error("report speaks {found}, this target speaks {expected}")]
    UnsupportedProtocol { found: String, expected: String },
    #[error("report is for session {found}, this target is running {expected}")]
    WrongSession { found: String, expected: String },
    #[error("report contains no phases")]
    Empty,
    #[error(
        "the generator claims {claimed} completed requests but the origin answered only \
         {served}; a generator cannot have received more responses than were sent, so the \
         report is discarded rather than degraded"
    )]
    ClaimExceedsServed { claimed: u64, served: u64 },
    #[error(
        "the origin answered {served} requests but the generator claims only {claimed}, a gap of \
         {excess} against an in-flight allowance of {allowance}; the surplus came from somewhere \
         else, and a measurement taken while a third party was also loading the origin does not \
         describe this machine"
    )]
    UnaccountedLoad {
        claimed: u64,
        served: u64,
        /// `served - claimed`, reported raw.
        ///
        /// Not reduced by the allowance, because the two are not separable:
        /// some unknown part of this gap is requests still in flight and the
        /// rest is a third party, and subtracting the allowance would present
        /// a guess about the split as a fact.
        excess: u64,
        allowance: u64,
    },
}

impl SessionReport {
    /// Checks the generator's claims against what the origin actually did.
    ///
    /// `served` is the origin's own counter: requests it began writing a
    /// response to, over the whole session. This is the anti-fabrication
    /// property described in the module documentation, and it is the reason
    /// an external result is worth as much as a local one.
    ///
    /// The bound has two sides, and they count different things:
    ///
    /// * **Upper.** Successful requests, warm-up included, may not exceed
    ///   `served`. A response the generator read is a response the origin
    ///   sent, so this can only be exceeded by lying. Discarded flat.
    /// * **Lower.** `served` may not exceed everything the generator says it
    ///   attempted, plus an in-flight allowance. A surplus means something
    ///   else was talking to the origin, and the measurement then describes
    ///   this machine serving the benchmark *and* whatever found the port.
    ///
    /// Failures count toward the lower bound only. The asymmetry is
    /// deliberate: a request that failed may or may not have reached the
    /// origin, so a failure can legitimately exist on either end alone.
    /// Counting them toward the upper bound would reject an honest lossy run.
    ///
    /// `served` counts only requests the origin answered with a body. That is
    /// what closes the 404 route: a generator asking for a size the origin
    /// does not serve gets 46-byte responses for no work, and reporting them
    /// as 1 MiB transfers now fails the upper bound instead of reconciling
    /// against a machine that did nothing.
    pub fn reconcile(&self, expected_session: &str, served: u64) -> Result<(), ReportRejected> {
        if self.protocol != EXTERNAL_PROTOCOL_VERSION {
            return Err(ReportRejected::UnsupportedProtocol {
                found: self.protocol.clone(),
                expected: EXTERNAL_PROTOCOL_VERSION.to_string(),
            });
        }
        if self.session_id != expected_session {
            return Err(ReportRejected::WrongSession {
                found: self.session_id.clone(),
                expected: expected_session.to_string(),
            });
        }
        if self.shapes.is_empty() {
            return Err(ReportRejected::Empty);
        }

        // Upper bound. Every response the generator says it read is a
        // response the origin says it sent, so a successful count above the
        // origin's is impossible without lying.
        let successful = self.successful_requests();
        if successful > served {
            return Err(ReportRejected::ClaimExceedsServed {
                claimed: successful,
                served,
            });
        }

        // Lower bound. Everything the origin answered has to be accounted for
        // by something the generator says it attempted.
        //
        // Failures are on this side of the check and not the other, and the
        // asymmetry is the whole reason the bound has two sides. A request
        // that failed may or may not have reached the origin - a connection
        // reset before the write did not, a timeout after the response began
        // did - so a failure can legitimately exist on either end alone.
        // Counting failures toward the upper bound would reject an honest
        // lossy run; leaving them out of the lower bound would reject an
        // honest run whose failures the origin *did* answer.
        let claimed = self.claimed_requests();
        let excess = served.saturating_sub(claimed);
        if excess > RECONCILE_SLACK_REQUESTS {
            return Err(ReportRejected::UnaccountedLoad {
                claimed,
                served,
                excess,
                allowance: RECONCILE_SLACK_REQUESTS,
            });
        }
        Ok(())
    }

    /// Requests the generator says it received a complete response to.
    ///
    /// Warm-up counts: those requests succeeded and were discarded, not
    /// failed. Failures do not, for the reason [`Self::reconcile`] gives.
    #[must_use]
    pub fn successful_requests(&self) -> u64 {
        self.shapes.iter().fold(0u64, |total, shape| {
            total
                .saturating_add(shape.requests_completed)
                .saturating_add(shape.requests_warmup)
        })
    }

    /// Total requests the generator says it received a response to, saturating
    /// rather than wrapping.
    ///
    /// Saturation matters: these numbers come off the network from a machine
    /// this one does not control, and a set of shapes summing past `u64::MAX`
    /// would otherwise wrap to a small number and sail through
    /// [`Self::reconcile`] - which is precisely the arithmetic a fabricated
    /// report would want.
    #[must_use]
    pub fn claimed_requests(&self) -> u64 {
        self.shapes.iter().fold(0u64, |total, shape| {
            total
                .saturating_add(shape.requests_completed)
                .saturating_add(shape.requests_failed)
                .saturating_add(shape.requests_warmup)
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn offer() -> SessionOffer {
        SessionOffer {
            protocol: EXTERNAL_PROTOCOL_VERSION.to_string(),
            agent_version: "0.1.0".to_string(),
            session_id: "s-1".to_string(),
            object_sizes: vec![1024, 65536],
            tls: false,
            certificate_der: None,
            expires_in_ms: 600_000,
            max_duration_ms: 60_000,
            max_rate_per_s: 5000.0,
            max_workers: 64,
        }
    }

    fn wanted() -> LoadRequest {
        LoadRequest {
            rate_per_s: 1000.0,
            duration_ms: 10_000,
            workers: 16,
            phases: 1,
        }
    }

    #[test]
    fn a_token_round_trips_through_its_printed_form() {
        let token = SessionToken::try_new().unwrap();
        let printed = token.to_hex();
        assert_eq!(printed.len(), TOKEN_BYTES * 2);
        assert!(token.matches(&SessionToken::from_str(&printed).unwrap()));
    }

    #[test]
    fn two_fresh_tokens_differ() {
        let a = SessionToken::try_new().unwrap();
        let b = SessionToken::try_new().unwrap();
        assert!(!a.matches(&b));
    }

    #[test]
    fn a_token_never_prints_itself_in_debug_output() {
        // This type is carried through code that logs, and the whole consent
        // mechanism is the secrecy of these 32 bytes.
        let token = SessionToken::try_new().unwrap();
        let rendered = format!("{token:?}");
        assert_eq!(rendered, "SessionToken(<redacted>)");
        assert!(!rendered.contains(&token.to_hex()[..8]));
    }

    #[test]
    fn a_truncated_or_overlong_token_is_refused_rather_than_padded() {
        assert!(SessionToken::from_str("00").is_err());
        assert!(SessionToken::from_str(&"ab".repeat(TOKEN_BYTES + 1)).is_err());
        assert!(SessionToken::from_str("not hex at all").is_err());
    }

    #[test]
    fn a_token_tolerates_the_whitespace_a_terminal_paste_adds() {
        let token = SessionToken::try_new().unwrap();
        let pasted = format!("  {}\n", token.to_hex());
        assert!(token.matches(&SessionToken::from_str(&pasted).unwrap()));
    }

    #[test]
    fn a_plan_within_every_ceiling_is_granted_unchanged() {
        let accepted = offer().accept(wanted()).unwrap();
        assert_eq!(accepted.granted, wanted());
        assert!(accepted.clamps.is_empty(), "{:?}", accepted.clamps);
    }

    #[test]
    fn every_ceiling_that_binds_is_reported_rather_than_applied_silently() {
        // The defect this guards is coordinated omission arriving by another
        // route: a run that quietly got a tenth of the load it asked for and
        // reported the resulting latency as if it were the load requested.
        let accepted = offer()
            .accept(LoadRequest {
                rate_per_s: 50_000.0,
                duration_ms: 600_000,
                workers: 4096,
                phases: 1,
            })
            .unwrap();
        assert_eq!(accepted.granted.rate_per_s, 5000.0);
        assert_eq!(accepted.granted.duration_ms, 60_000);
        assert_eq!(accepted.granted.workers, 64);
        assert_eq!(accepted.clamps.len(), 3, "{:?}", accepted.clamps);
    }

    #[test]
    fn the_session_lifetime_is_a_budget_shared_by_every_phase() {
        // The defect this replaces: `accept` checked one phase against the
        // whole remaining session, so a generator running four of them back to
        // back inside a one-minute ticket scheduled two minutes of work. The
        // target shut down around the third shape and the report of the two
        // that had already succeeded had nowhere to go.
        let mut offer = offer();
        offer.expires_in_ms = 60_000;
        let accepted = offer
            .accept(LoadRequest {
                duration_ms: 30_000,
                phases: 4,
                ..wanted()
            })
            .unwrap();

        let expected = (60_000 - SESSION_OVERHEAD_MS) / 4;
        assert_eq!(accepted.granted.duration_ms, expected);
        // And the whole plan now fits, with the overhead still unspent.
        assert!(
            accepted.granted.duration_ms * 4 + SESSION_OVERHEAD_MS <= offer.expires_in_ms,
            "the budget still overruns"
        );
        assert!(accepted.clamps.iter().any(|note| note.contains("phase")));
    }

    #[test]
    fn a_plan_that_already_fits_the_budget_is_not_clamped() {
        let mut offer = offer();
        offer.expires_in_ms = 600_000;
        let accepted = offer
            .accept(LoadRequest {
                duration_ms: 10_000,
                phases: 4,
                ..wanted()
            })
            .unwrap();
        assert_eq!(accepted.granted.duration_ms, 10_000);
        assert!(accepted.clamps.is_empty(), "{:?}", accepted.clamps);
    }

    #[test]
    fn a_session_too_short_to_measure_is_refused_rather_than_clamped_to_nothing() {
        // The first version of the budget clamped to `max(1)`, which turns a
        // session with five seconds left into a one-millisecond phase. That
        // produces a number, and the number is about connection setup.
        let mut offer = offer();
        offer.expires_in_ms = 5_000;
        let outcome = offer.accept(wanted());
        assert!(
            matches!(outcome, Err(Refusal::SessionTooShort { .. })),
            "{outcome:?}"
        );

        // And the boundary: exactly enough for the floor is accepted.
        offer.expires_in_ms = SESSION_OVERHEAD_MS + MIN_PHASE_MS;
        let accepted = offer.accept(wanted()).unwrap();
        assert_eq!(accepted.granted.duration_ms, MIN_PHASE_MS);
    }

    #[test]
    fn the_protocol_version_is_checked_before_any_other_field_is_believed() {
        let mut offer = offer();
        offer.protocol = "darcbench.external/99".to_string();
        // Everything else about this offer is also invalid; the version must
        // still be the reported reason, because a field's meaning is defined
        // by the version that declared it.
        offer.object_sizes.clear();
        offer.max_rate_per_s = f64::NAN;
        assert!(matches!(
            offer.accept(wanted()),
            Err(Refusal::UnsupportedProtocol { .. })
        ));
    }

    #[test]
    fn a_peer_that_is_not_a_target_gets_no_load() {
        let mut offer = offer();
        offer.object_sizes.clear();
        assert_eq!(offer.accept(wanted()), Err(Refusal::NothingServed));
    }

    #[test]
    fn a_tls_target_with_nothing_to_pin_is_refused() {
        let mut offer = offer();
        offer.tls = true;
        offer.certificate_der = None;
        assert_eq!(offer.accept(wanted()), Err(Refusal::UnpinnableCertificate));
    }

    #[test]
    fn an_expired_session_is_refused_rather_than_run_briefly() {
        let mut offer = offer();
        offer.expires_in_ms = 0;
        assert_eq!(offer.accept(wanted()), Err(Refusal::Expired));
    }

    #[test]
    fn a_nonsensical_ceiling_is_refused_rather_than_clamped_to() {
        for broken in [f64::NAN, f64::INFINITY, 0.0, -1.0] {
            let mut offer = offer();
            offer.max_rate_per_s = broken;
            assert!(
                matches!(
                    offer.accept(wanted()),
                    Err(Refusal::ImplausibleOffer { .. })
                ),
                "accepted a max rate of {broken}"
            );
        }
    }

    #[test]
    fn a_nonsensical_request_stops_the_run_rather_than_running_flat_out() {
        // Two things at once. A rate of NaN would reach
        // `Duration::from_secs_f64` and abort the process, which is worse than
        // any number it could produce - and the earlier fix for that
        // substituted the *target's ceiling*, so a typo became the most
        // aggressive load the peer would permit. For a tool whose threat model
        // is about not overloading machines, that failed open in the wrong
        // direction.
        for broken in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 0.0, -5.0] {
            let outcome = offer().accept(LoadRequest {
                rate_per_s: broken,
                ..wanted()
            });
            assert!(
                matches!(outcome, Err(Refusal::ImplausibleRequest { .. })),
                "a rate of {broken} produced {outcome:?}"
            );
        }
    }

    #[test]
    fn a_failure_the_origin_never_saw_does_not_fail_the_upper_bound() {
        // A connection reset before the write reached the origin: the
        // generator counts a failure, the origin counted nothing. Rejecting
        // that would make every lossy-but-honest run unreportable.
        let report = report(1_000, 500);
        assert!(report.reconcile("s-1", 1_000).is_ok());
    }

    #[test]
    fn warm_up_requests_are_reported_and_counted() {
        // The origin answered them like any other request. A warm-up the
        // report did not mention appears on the target as load from a third
        // party and rejects an entirely honest run.
        let mut report = report(1_000, 0);
        report.shapes[0].requests_warmup = 4_000;
        assert_eq!(report.successful_requests(), 5_000);
        assert!(report.reconcile("s-1", 5_000).is_ok());

        report.shapes[0].requests_warmup = 0;
        assert!(matches!(
            report.reconcile("s-1", 5_000),
            Err(ReportRejected::UnaccountedLoad { .. })
        ));
    }

    fn report(completed: u64, failed: u64) -> SessionReport {
        SessionReport {
            protocol: EXTERNAL_PROTOCOL_VERSION.to_string(),
            session_id: "s-1".to_string(),
            generator: GeneratorIdentity {
                agent_version: "0.1.0".to_string(),
                os: "linux".to_string(),
                arch: "x86_64".to_string(),
                cpus: 8,
            },
            shapes: vec![ShapeReport {
                shape: "small.keepalive".to_string(),
                object_bytes: 1024,
                requests_scheduled: completed + failed,
                requests_completed: completed,
                requests_failed: failed,
                requests_warmup: 0,
                error_examples: Vec::new(),
                bytes: completed.saturating_mul(1024),
                offered_rate_per_s: 1000.0,
                achieved_rate_per_s: 995.0,
                service_ms: LatencySummary::default(),
                response_ms: LatencySummary::default(),
                saturation: None,
                generator_cpu_pct: 42.0,
            }],
            clamps: Vec::new(),
        }
    }

    #[test]
    fn an_honest_report_reconciles() {
        assert!(report(10_000, 5).reconcile("s-1", 10_005).is_ok());
    }

    #[test]
    fn failed_requests_count_toward_the_claim() {
        // The origin answered them - a 404 is a response - and excluding them
        // would leave exactly the gap a dishonest generator would want.
        let report = report(10_000, 500);
        assert_eq!(report.claimed_requests(), 10_500);
        assert!(report.reconcile("s-1", 10_500).is_ok());
    }

    #[test]
    fn a_generator_claiming_more_work_than_the_origin_did_is_discarded() {
        // The whole reason an external result is worth as much as a local one.
        assert!(matches!(
            report(1_000_000, 0).reconcile("s-1", 400_000),
            Err(ReportRejected::ClaimExceedsServed { .. })
        ));
    }

    #[test]
    fn in_flight_requests_at_the_end_of_a_phase_are_not_treated_as_a_lie() {
        // The origin counts a request when it starts writing; the generator
        // when it finishes reading. A handful is arithmetic, not fraud.
        assert!(report(10_000, 0).reconcile("s-1", 10_000 + 512).is_ok());
    }

    #[test]
    fn load_from_a_third_party_invalidates_the_measurement() {
        let outcome = report(10_000, 0).reconcile("s-1", 10_000 + RECONCILE_SLACK_REQUESTS + 1);
        assert!(
            matches!(
                outcome,
                Err(ReportRejected::UnaccountedLoad { excess, .. })
                    if excess == RECONCILE_SLACK_REQUESTS + 1
            ),
            "{outcome:?}"
        );
    }

    #[test]
    fn the_slack_is_absolute_so_a_long_run_cannot_hide_a_large_lie() {
        // A percentage tolerance would grow with the run. Ten million requests
        // with a percentage bound would swallow tens of thousands of unexplained
        // ones; the same absolute slack catches it.
        let outcome = report(10_000_000, 0).reconcile("s-1", 10_050_000);
        assert!(matches!(
            outcome,
            Err(ReportRejected::UnaccountedLoad { .. })
        ));
    }

    #[test]
    fn a_shape_total_that_would_overflow_saturates_instead_of_wrapping() {
        // These numbers arrive off the network from a machine this one does
        // not control. Wrapping would turn an absurd claim into a small one
        // that reconciles.
        let mut report = report(u64::MAX, 0);
        report.shapes.push(report.shapes[0].clone());
        assert_eq!(report.claimed_requests(), u64::MAX);
        assert!(matches!(
            report.reconcile("s-1", 10),
            Err(ReportRejected::ClaimExceedsServed { .. })
        ));
    }

    #[test]
    fn a_report_for_another_session_is_refused() {
        assert!(matches!(
            report(10, 0).reconcile("s-2", 10),
            Err(ReportRejected::WrongSession { .. })
        ));
    }

    #[test]
    fn an_empty_report_is_refused_rather_than_read_as_a_clean_run() {
        let mut report = report(10, 0);
        report.shapes.clear();
        assert_eq!(report.reconcile("s-1", 0), Err(ReportRejected::Empty));
    }

    #[test]
    fn the_offer_and_the_report_survive_a_json_round_trip() {
        // Both cross a network boundary; a field that serialises and does not
        // deserialise would fail at the far end of a benchmark run.
        let offer = offer();
        let encoded = serde_json::to_string(&offer).unwrap();
        assert_eq!(
            serde_json::from_str::<SessionOffer>(&encoded).unwrap(),
            offer
        );

        let report = report(7, 1);
        let encoded = serde_json::to_string(&report).unwrap();
        assert_eq!(
            serde_json::from_str::<SessionReport>(&encoded).unwrap(),
            report
        );
    }
}
