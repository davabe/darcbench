//! The two-machine load-generation session, as running code.
//!
//! [`darcbench_protocol::external`] is the wire contract and the reasoning
//! behind it - why consent rather than access control, and why the target does
//! not believe the generator. This module is the two ends of that contract:
//! [`TargetSession`] on the machine being measured, [`Driver`] on the machine
//! producing the load.
//!
//! # The shape of a session
//!
//! ```text
//!   target machine                          generator machine
//!   --------------                          -----------------
//!   darcbench web-target --bind 10.0.0.5
//!     starts the origin (token-gated)
//!     starts the control listener
//!     prints ONE ticket string  ────────►   operator carries it by hand
//!                                           darcbench web-drive --ticket ...
//!                                             SessionOffer::accept
//!                                             load, through crate::loadgen
//!     origin counts what it answered  ◄───    POST /report
//!     SessionReport::reconcile
//!     the run's numbers, or a refusal
//! ```
//!
//! # Why the offer is carried by hand and not fetched
//!
//! The obvious design gives the target a `GET /session` endpoint the generator
//! calls with its token. That endpoint would be a thing on the network that
//! answers questions about the session, and every such thing is a surface: it
//! can be probed, it can be timed, its error messages distinguish "wrong token"
//! from "no session here".
//!
//! The offer is not secret and it is not large. Printing it once, as part of
//! the same string that carries the token, removes the endpoint entirely - the
//! operator was already going to copy a secret between two machines, and the
//! offer rides along at no extra cost to them. What is left on the network is
//! one endpoint that accepts one document at the end of the run, which is the
//! smallest control plane this feature can have.
//!
//! It also makes a class of mistake impossible: a generator cannot be pointed
//! at an address it was not given an offer for, because there is nowhere to
//! ask.
//!
//! # Why the control listener is separate from the origin
//!
//! [`crate::web_origin`] parses a request target into a byte count and refuses
//! anything carrying a body. That is a real property - it is what makes path
//! traversal not arise for that module rather than merely be handled by it -
//! and accepting a JSON document would end it.
//!
//! So the report goes to a second listener, on the same interface, on its own
//! OS-assigned port, sharing the origin's TLS configuration so that a session
//! the operator asked to encrypt is encrypted on both channels. The origin
//! stays a thing that serves bytes.

use std::io::{Read, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use darcbench_protocol::external::{
    AcceptedSession, GeneratorIdentity, LatencySummary, LoadRequest, Refusal, ReportRejected,
    SessionOffer, SessionReport, SessionToken, ShapeReport, EXTERNAL_PROTOCOL_VERSION,
    MAX_SESSION_SECS, MIN_SESSION_SECS,
};

use crate::loadgen::{self, LoadOutcome, LoadPlan};
use crate::web_origin::{Bind, Origin, OriginConfig, OriginError};
use crate::web_static::{self, Reuse};

/// Longest control request this listener will read, head and body together.
///
/// A report is a few hundred bytes per shape and there are five shapes. Sixty
/// four kilobytes is two orders of magnitude of headroom and is still a bound,
/// which is the point: this listener is reachable by anyone who finds the port,
/// and an unbounded read from a socket is not something to write twice.
const MAX_CONTROL_BYTES: usize = 64 * 1024;

/// How long the control listener will wait for a peer to finish a request.
const CONTROL_DEADLINE: Duration = Duration::from_secs(30);

/// How long the generator will wait for the target to accept its report.
///
/// Longer than the read deadline, because the target reconciles before it
/// answers and a generator that gave up early would lose the verdict on a run
/// that had already succeeded.
const REPORT_TIMEOUT: Duration = Duration::from_secs(60);

/// The path the report is posted to. One endpoint, and this is it.
const REPORT_PATH: &str = "/report";

/// The header carrying the session token, matching the origin's.
const TOKEN_HEADER: &str = "x-darcbench-session";

// ---------------------------------------------------------------------------
// The ticket
// ---------------------------------------------------------------------------

/// Everything the generator needs, in one string a human can carry.
///
/// Serialised as JSON and hex-encoded rather than printed as JSON directly.
/// The reason is mundane and matters: a human copying this out of a terminal
/// must get all of it or none of it, and a blob with no spaces or newlines
/// survives a double-click, a chat window and an SSH scrollback in a way a
/// pretty-printed object does not.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Ticket {
    /// Where the origin listens.
    pub origin: SocketAddr,
    /// Where the report is posted.
    pub control: SocketAddr,
    /// The secret. Both channels require it.
    ///
    /// Serialised as hex through [`SessionToken`]'s printed form, so a ticket
    /// in a log is as bad as a token in a log - which is why neither this type
    /// nor the token prints itself in `Debug`.
    #[serde(with = "token_hex")]
    pub token: SessionToken,
    /// What the target is offering.
    pub offer: SessionOffer,
}

/// Serialises a [`SessionToken`] through its hex form.
mod token_hex {
    use darcbench_protocol::external::SessionToken;
    use serde::{Deserialize, Deserializer, Serializer};
    use std::str::FromStr;

    pub(super) fn serialize<S: Serializer>(
        token: &SessionToken,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&token.to_hex())
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<SessionToken, D::Error> {
        let raw = String::deserialize(deserializer)?;
        SessionToken::from_str(&raw).map_err(serde::de::Error::custom)
    }
}

impl Ticket {
    /// The single string an operator carries between machines.
    pub fn encode(&self) -> Result<String, SessionError> {
        let json =
            serde_json::to_vec(self).map_err(|error| SessionError::Ticket(error.to_string()))?;
        Ok(hex::encode(json))
    }

    /// The inverse. Tolerates the whitespace a terminal paste adds.
    pub fn decode(encoded: &str) -> Result<Self, SessionError> {
        let raw = hex::decode(encoded.trim())
            .map_err(|_| SessionError::Ticket("ticket is not a DARCBench ticket".to_string()))?;
        serde_json::from_slice(&raw)
            .map_err(|error| SessionError::Ticket(format!("ticket is unreadable: {error}")))
    }
}

/// A ticket never prints its contents, because one of them is the token.
impl std::fmt::Display for Ticket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Ticket(origin {}, <secret>)", self.origin)
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("{0}")]
    Ticket(String),
    #[error("could not start the benchmark origin: {0}")]
    Origin(#[from] OriginError),
    #[error("could not start the session control listener: {0}")]
    Control(#[source] std::io::Error),
    #[error("the session length must be between {MIN_SESSION_SECS} and {MAX_SESSION_SECS} seconds, not {0}")]
    SessionLength(u64),
    #[error("the generator refused to load this peer: {0}")]
    Refused(#[from] Refusal),
    #[error("the target refused the report: {0}")]
    Rejected(#[from] ReportRejected),
    #[error("could not reach the target's control listener: {0}")]
    Transport(String),
    #[error("the session expired before the run finished")]
    Expired,
}

// ---------------------------------------------------------------------------
// The target
// ---------------------------------------------------------------------------

/// What a completed session produced, on the target.
#[derive(Debug, Clone)]
pub struct SessionResult {
    pub report: SessionReport,
    /// The origin's own count, which the report was reconciled against.
    pub served: u64,
    /// Requests that reached the origin without the token.
    ///
    /// Never zero-checked, never fatal, always reported. A non-zero count means
    /// the port was found by something that was not the generator, which an
    /// operator should know even though it did not change the numbers.
    pub refused: u64,
}

/// The machine being measured: an origin, a control listener, and a ticket.
///
/// `Debug` is written out rather than derived. Deriving would be safe today -
/// [`SessionToken`] redacts itself - but it would make the safety of printing a
/// session depend on a field of a field several types away, and that is exactly
/// the kind of property that stops holding when somebody adds a field.
pub struct TargetSession {
    origin: Arc<Origin>,
    control_address: SocketAddr,
    ticket: Ticket,
    deadline: Instant,
    inbox: Arc<Mutex<Option<SessionReport>>>,
    stop: Arc<AtomicBool>,
    control: Option<std::thread::JoinHandle<()>>,
}

impl TargetSession {
    /// Starts the origin and the control listener on `ip`, and mints a ticket.
    ///
    /// Both listeners take an OS-assigned port. Neither is configurable, for
    /// the reason [`Origin::start`] gives: a benchmark that races whatever the
    /// operator already has listening is a benchmark that damages the machine
    /// it was asked to measure.
    pub fn start(
        ip: IpAddr,
        object_sizes: Vec<usize>,
        tls: bool,
        ttl: Duration,
    ) -> Result<Self, SessionError> {
        let seconds = ttl.as_secs();
        if !(MIN_SESSION_SECS..=MAX_SESSION_SECS).contains(&seconds) {
            return Err(SessionError::SessionLength(seconds));
        }

        let token = SessionToken::try_new()
            .map_err(|error| SessionError::Ticket(format!("no entropy for a token: {error}")))?;
        let session_id = darcbench_protocol::RunId::try_new()
            .map_err(|error| SessionError::Ticket(format!("no entropy for a session id: {error}")))?
            .as_str()
            .to_string();

        let origin = Arc::new(Origin::start(OriginConfig {
            object_sizes: object_sizes.clone(),
            tls,
            bind: Bind::External(ip),
            token: Some(token.clone()),
        })?);

        // Bound before the ticket is minted, so the ticket cannot name a port
        // nothing is listening on.
        let listener = TcpListener::bind(SocketAddr::new(ip, 0)).map_err(SessionError::Control)?;
        let control_address = listener.local_addr().map_err(SessionError::Control)?;

        let offer = SessionOffer {
            protocol: EXTERNAL_PROTOCOL_VERSION.to_string(),
            agent_version: env!("CARGO_PKG_VERSION").to_string(),
            session_id,
            object_sizes: object_sizes.iter().map(|&size| size as u64).collect(),
            tls,
            certificate_der: origin
                .certificate_der()
                .map(|der| hex::encode(der.as_ref())),
            expires_in_ms: ttl.as_millis().min(u128::from(u64::MAX)) as u64,
            max_duration_ms: ttl.as_millis().min(u128::from(u64::MAX)) as u64,
            max_rate_per_s: MAX_OFFERED_RATE_PER_S,
            max_workers: MAX_OFFERED_WORKERS,
        };

        let ticket = Ticket {
            origin: origin.address(),
            control: control_address,
            token: token.clone(),
            offer,
        };

        let inbox = Arc::new(Mutex::new(None));
        let stop = Arc::new(AtomicBool::new(false));
        let control = {
            let inbox = Arc::clone(&inbox);
            let stop = Arc::clone(&stop);
            let tls = origin.tls_config();
            std::thread::Builder::new()
                .name("darcbench-session-control".to_string())
                .spawn(move || control_loop(listener, token, inbox, stop, tls))
                .map_err(SessionError::Control)?
        };

        Ok(Self {
            origin,
            control_address,
            ticket,
            deadline: Instant::now() + ttl,
            inbox,
            stop,
            control: Some(control),
        })
    }

    pub fn ticket(&self) -> &Ticket {
        &self.ticket
    }

    pub fn origin_address(&self) -> SocketAddr {
        self.origin.address()
    }

    pub fn control_address(&self) -> SocketAddr {
        self.control_address
    }

    /// Requests the origin has answered so far.
    pub fn served(&self) -> u64 {
        self.origin.served()
    }

    /// Requests that reached the origin without the session token.
    pub fn refused(&self) -> u64 {
        self.origin.refused()
    }

    /// Blocks until a report arrives and reconciles, or the session expires.
    ///
    /// `poll` is how often the deadline is checked. It bounds how long after
    /// expiry this returns, and nothing else: a report that arrives is picked
    /// up on the next tick.
    pub fn wait(&self, poll: Duration) -> Result<SessionResult, SessionError> {
        loop {
            if let Some(report) = self.take_report() {
                // Read after the report is in hand. The origin's counter is
                // monotonic, so reading it later can only ever include *more*
                // requests - which fails the reconciliation in the safe
                // direction, toward "somebody else was here", rather than
                // toward accepting a claim the origin cannot support.
                let served = self.origin.served();
                report.reconcile(&self.ticket.offer.session_id, served)?;
                return Ok(SessionResult {
                    report,
                    served,
                    refused: self.origin.refused(),
                });
            }
            if Instant::now() >= self.deadline {
                return Err(SessionError::Expired);
            }
            std::thread::sleep(poll.min(Duration::from_secs(1)));
        }
    }

    fn take_report(&self) -> Option<SessionReport> {
        self.inbox.lock().ok().and_then(|mut held| held.take())
    }
}

impl Drop for TargetSession {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wake the blocking accept, exactly as the origin does. The flag is
        // already visible, so this connection is closed rather than read.
        if let Ok(stream) =
            TcpStream::connect_timeout(&self.control_address, Duration::from_secs(1))
        {
            let _ = stream.shutdown(Shutdown::Both);
        }
        if let Some(control) = self.control.take() {
            let _ = control.join();
        }
    }
}

/// Highest rate a target will let a generator ask for.
///
/// Not a capability claim - no machine serves five million requests a second -
/// but a bound on what a generator may *schedule*. It exists so a mistyped
/// rate becomes a clamp with a note in the bundle rather than a generator
/// allocating a schedule it can never execute.
const MAX_OFFERED_RATE_PER_S: f64 = 5_000_000.0;

/// Highest connection count a target will let a generator open.
///
/// Each one is a thread on the generator and a connection slot on the origin,
/// and the origin's own ceiling is 512. Well below it, because a generator
/// that opens every slot leaves none for the shape that measures connection
/// setup.
const MAX_OFFERED_WORKERS: u32 = 256;

// ---------------------------------------------------------------------------
// The control listener
// ---------------------------------------------------------------------------

fn control_loop(
    listener: TcpListener,
    token: SessionToken,
    inbox: Arc<Mutex<Option<SessionReport>>>,
    stop: Arc<AtomicBool>,
    tls: Option<Arc<rustls::ServerConfig>>,
) {
    while let Ok((stream, _peer)) = listener.accept() {
        if stop.load(Ordering::SeqCst) {
            let _ = stream.shutdown(Shutdown::Both);
            return;
        }
        // Serially, on this thread. One report per session means concurrency
        // here would buy nothing and would add a way for a peer to occupy
        // several threads by opening several connections and going quiet.
        let _ = stream.set_read_timeout(Some(CONTROL_DEADLINE));
        let _ = stream.set_write_timeout(Some(CONTROL_DEADLINE));
        match &tls {
            None => {
                let mut stream = stream;
                serve_control(&mut stream, &token, &inbox);
            }
            Some(config) => {
                match rustls::ServerConnection::new(Arc::clone(config)) {
                    Ok(connection) => {
                        let mut stream = rustls::StreamOwned::new(connection, stream);
                        serve_control(&mut stream, &token, &inbox);
                    }
                    // A handshake this end could not even set up. Nothing to
                    // say to the peer that it could read.
                    Err(_) => continue,
                }
            }
        }
    }
}

/// Reads one request, answers it, and closes. Never keep-alive.
///
/// One report per session, so a connection that has been answered has nothing
/// left to do, and closing is one fewer thing to bound.
fn serve_control<S: Read + Write>(
    stream: &mut S,
    token: &SessionToken,
    inbox: &Mutex<Option<SessionReport>>,
) {
    let (status, body) = match read_control_request(stream) {
        Err(status) => (status, String::new()),
        Ok(request) => handle_control_request(&request, token, inbox),
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

struct ControlRequest {
    post: bool,
    path: String,
    token: Option<SessionToken>,
    body: Vec<u8>,
}

/// Reads a complete request, or the status to answer with instead.
fn read_control_request<S: Read>(stream: &mut S) -> Result<ControlRequest, &'static str> {
    let mut buffer = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];

    // The head.
    let head_end = loop {
        if let Some(index) = find_double_crlf(&buffer) {
            break index;
        }
        if buffer.len() >= MAX_CONTROL_BYTES {
            return Err("431 Request Header Fields Too Large");
        }
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return Err("400 Bad Request"),
            Ok(read) => buffer.extend_from_slice(&chunk[..read]),
        }
    };

    // Copied out of `buffer` rather than borrowed from it: the body is drained
    // from the same buffer below, and a parser that borrowed the head would
    // make that a borrow-checker argument instead of a straight read.
    let head = String::from_utf8(buffer[..head_end].to_vec()).map_err(|_| "400 Bad Request")?;
    let mut lines = head.split("\r\n");
    let mut fields = lines.next().ok_or("400 Bad Request")?.split(' ');
    let method = fields.next().ok_or("400 Bad Request")?;
    let path = fields.next().ok_or("400 Bad Request")?.to_string();

    let mut token = None;
    let mut length = 0usize;
    for line in lines {
        // Continuation lines are refused rather than interpreted, for the same
        // reason the origin refuses them: the only reason to send one is to
        // find out what this parser does with it.
        if line.starts_with(' ') || line.starts_with('\t') {
            return Err("400 Bad Request");
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err("400 Bad Request");
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("content-length") {
            length = value.parse().map_err(|_| "400 Bad Request")?;
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            // No chunked decoding here. A body this listener cannot measure in
            // advance is a body it cannot bound.
            return Err("400 Bad Request");
        } else if name.eq_ignore_ascii_case(TOKEN_HEADER) {
            token = value.parse().ok();
        }
    }

    // Checked against the bound *before* reading, so an oversized
    // `Content-Length` is refused rather than read.
    if length > MAX_CONTROL_BYTES {
        return Err("413 Payload Too Large");
    }

    let mut body: Vec<u8> = buffer.drain(head_end + 4..).collect();
    while body.len() < length {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return Err("400 Bad Request"),
            Ok(read) => body.extend_from_slice(&chunk[..read]),
        }
    }
    body.truncate(length);

    Ok(ControlRequest {
        post: method == "POST",
        path,
        token,
        body,
    })
}

fn handle_control_request(
    request: &ControlRequest,
    token: &SessionToken,
    inbox: &Mutex<Option<SessionReport>>,
) -> (&'static str, String) {
    // Before the path, so an unauthenticated peer cannot distinguish a live
    // session's control listener from any other socket by which paths exist.
    let authorised = request
        .token
        .as_ref()
        .is_some_and(|presented| token.matches(presented));
    if !authorised {
        return ("401 Unauthorized", json_error("unauthorized"));
    }
    if !request.post || request.path != REPORT_PATH {
        return ("404 Not Found", json_error("no such endpoint"));
    }

    let report: SessionReport = match serde_json::from_slice(&request.body) {
        Ok(report) => report,
        Err(error) => return ("400 Bad Request", json_error(&error.to_string())),
    };

    let Ok(mut held) = inbox.lock() else {
        return (
            "500 Internal Server Error",
            json_error("session state lost"),
        );
    };
    if held.is_some() {
        // A session accepts one report. A second would either overwrite a
        // reconciled result or race the reconciliation, and neither is a thing
        // to allow for the convenience of a generator that sent twice.
        return (
            "409 Conflict",
            json_error("this session already has a report"),
        );
    }
    *held = Some(report);
    ("202 Accepted", "{\"accepted\":true}".to_string())
}

fn json_error(message: &str) -> String {
    // Built with serde rather than by formatting, so a message containing a
    // quote produces valid JSON instead of a document the peer cannot parse.
    serde_json::json!({ "error": message }).to_string()
}

fn find_double_crlf(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

// ---------------------------------------------------------------------------
// The generator
// ---------------------------------------------------------------------------

/// One shape the driver will measure. Mirrors `web.static`'s table.
#[derive(Debug, Clone, Copy)]
pub struct DriveShape {
    pub key: &'static str,
    pub bytes: usize,
    pub reuse: Reuse,
}

/// The machine producing the load.
///
/// `Debug` written out for the same reason as [`TargetSession`]'s.
pub struct Driver {
    ticket: Ticket,
    accepted: AcceptedSession,
}

impl Driver {
    /// Decides whether to talk to this peer at all.
    ///
    /// Nothing in this type opens a socket to the origin until this has
    /// returned `Ok`. That ordering is the consent property from
    /// [`darcbench_protocol::external`]: a peer that did not produce a
    /// recognisable offer is never loaded, because there is no path here that
    /// reaches the load generator without passing through
    /// [`SessionOffer::accept`].
    pub fn open(ticket: Ticket, wanted: LoadRequest) -> Result<Self, SessionError> {
        let accepted = ticket.offer.accept(wanted)?;
        Ok(Self { ticket, accepted })
    }

    pub fn accepted(&self) -> &AcceptedSession {
        &self.accepted
    }

    /// Runs one shape and returns what the generator saw.
    pub fn measure(&self, shape: DriveShape) -> Result<ShapeReport, SessionError> {
        let size = shape.bytes as u64;
        if !self.accepted.object_sizes.contains(&size) {
            return Err(SessionError::Transport(format!(
                "the target does not serve {size}-byte objects; it offered {:?}",
                self.accepted.object_sizes
            )));
        }

        let tls = match &self.accepted.certificate_der {
            None => None,
            Some(hex_der) => {
                let der = hex::decode(hex_der).map_err(|_| {
                    SessionError::Transport("the target's certificate is unreadable".to_string())
                })?;
                Some(
                    web_static::tls_config(rustls_pki_types::CertificateDer::from(der)).map_err(
                        |error| SessionError::Transport(format!("certificate refused: {error}")),
                    )?,
                )
            }
        };

        let workers = self.accepted.granted.workers as usize;
        let client = web_static::HttpClient::new(
            self.ticket.origin,
            format!("/o/{}", shape.bytes),
            shape.bytes,
            shape.reuse,
            tls,
            workers,
        )
        .with_session_token(self.ticket.token.to_hex());

        let plan = LoadPlan {
            rate_per_s: self.accepted.granted.rate_per_s,
            duration: Duration::from_millis(self.accepted.granted.duration_ms),
            workers,
            warmup: workers as u64,
        };
        let outcome = loadgen::run(&client, &plan);
        Ok(shape_report(shape, &plan, &outcome))
    }

    /// Posts a finished report and returns once the target has accepted it.
    pub fn submit(&self, shapes: Vec<ShapeReport>) -> Result<(), SessionError> {
        let report = SessionReport {
            protocol: EXTERNAL_PROTOCOL_VERSION.to_string(),
            session_id: self.accepted.session_id.clone(),
            generator: identity(),
            shapes,
            clamps: self.accepted.clamps.clone(),
        };
        let body = serde_json::to_vec(&report)
            .map_err(|error| SessionError::Transport(error.to_string()))?;
        post_report(&self.ticket, &body)
    }
}

fn identity() -> GeneratorIdentity {
    GeneratorIdentity {
        agent_version: env!("CARGO_PKG_VERSION").to_string(),
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        cpus: std::thread::available_parallelism()
            .map(|count| count.get() as u32)
            .unwrap_or(0),
    }
}

fn shape_report(shape: DriveShape, plan: &LoadPlan, outcome: &LoadOutcome) -> ShapeReport {
    ShapeReport {
        shape: shape.key.to_string(),
        object_bytes: shape.bytes as u64,
        requests_scheduled: outcome.scheduled,
        requests_completed: outcome.completed,
        requests_failed: outcome.errors,
        // The count the generator *achieved*, never the count it planned.
        // `successful_requests` puts this on the strict upper bound against
        // what the origin served, so claiming a warm-up the origin never
        // answered - one refused connection is enough - rejects an otherwise
        // valid run and blames a third party who was never there.
        requests_warmup: outcome.warmup_completed,
        error_examples: outcome.error_examples.clone(),
        bytes: outcome.bytes,
        offered_rate_per_s: plan.rate_per_s,
        achieved_rate_per_s: outcome.achieved_rate_per_s,
        service_ms: summarise(&outcome.service_ms),
        response_ms: summarise(&outcome.response_ms),
        saturation: outcome
            .warning(None)
            .map(|warning| warning.message)
            .filter(|_| outcome.saturation.is_saturated()),
        generator_cpu_pct: outcome.generator_cpu_pct,
    }
}

/// Reduces a sample series to what a metric needs.
///
/// Sorts a copy rather than the caller's data: the outcome is the run's
/// evidence and reordering it in place would silently destroy the arrival
/// order anything else might want to read.
fn summarise(samples: &[f64]) -> LatencySummary {
    if samples.is_empty() {
        return LatencySummary::default();
    }
    let mut sorted: Vec<f64> = samples.iter().copied().filter(|v| v.is_finite()).collect();
    if sorted.is_empty() {
        return LatencySummary::default();
    }
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let count = sorted.len();
    let at = |percent: f64| -> f64 {
        // Nearest-rank, clamped. The index is bounded rather than trusted to
        // land in range, because a percentile of 100 on a one-element series
        // would otherwise index past the end.
        let rank = ((percent / 100.0) * count as f64).ceil() as usize;
        sorted[rank.saturating_sub(1).min(count - 1)]
    };
    LatencySummary {
        count: count as u64,
        mean_ms: sorted.iter().sum::<f64>() / count as f64,
        p50_ms: at(50.0),
        p90_ms: at(90.0),
        p99_ms: at(99.0),
        max_ms: sorted[count - 1],
    }
}

/// Sends the report over the same transport the origin uses.
fn post_report(ticket: &Ticket, body: &[u8]) -> Result<(), SessionError> {
    let stream = TcpStream::connect_timeout(&ticket.control, REPORT_TIMEOUT)
        .map_err(|error| SessionError::Transport(format!("connect: {error}")))?;
    stream
        .set_read_timeout(Some(REPORT_TIMEOUT))
        .and_then(|()| stream.set_write_timeout(Some(REPORT_TIMEOUT)))
        .map_err(|error| SessionError::Transport(format!("timeout: {error}")))?;

    let request = format!(
        "POST {REPORT_PATH} HTTP/1.1\r\nHost: {}\r\n{TOKEN_HEADER}: {}\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        ticket.control,
        ticket.token.to_hex(),
        body.len()
    );

    let status = match &ticket.offer.certificate_der {
        None => {
            let mut stream = stream;
            exchange(&mut stream, request.as_bytes(), body)?
        }
        Some(hex_der) => {
            let der = hex::decode(hex_der)
                .map_err(|_| SessionError::Transport("certificate unreadable".to_string()))?;
            let config = web_static::tls_config(rustls_pki_types::CertificateDer::from(der))
                .map_err(|error| SessionError::Transport(format!("certificate: {error}")))?;
            let server = rustls_pki_types::ServerName::IpAddress(ticket.control.ip().into());
            let connection = rustls::ClientConnection::new(config, server)
                .map_err(|error| SessionError::Transport(format!("tls: {error}")))?;
            let mut stream = rustls::StreamOwned::new(connection, stream);
            exchange(&mut stream, request.as_bytes(), body)?
        }
    };

    match status {
        202 => Ok(()),
        401 => Err(SessionError::Transport(
            "the target refused the session token".to_string(),
        )),
        409 => Err(SessionError::Transport(
            "the target already has a report for this session".to_string(),
        )),
        other => Err(SessionError::Transport(format!(
            "the target answered {other} to the report"
        ))),
    }
}

/// Writes a request and reads back the status code.
fn exchange<S: Read + Write>(
    stream: &mut S,
    head: &[u8],
    body: &[u8],
) -> Result<u16, SessionError> {
    stream
        .write_all(head)
        .and_then(|()| stream.write_all(body))
        .and_then(|()| stream.flush())
        .map_err(|error| SessionError::Transport(format!("write: {error}")))?;

    let mut buffer = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    loop {
        if let Some(end) = buffer.iter().position(|&byte| byte == b'\r') {
            let line = std::str::from_utf8(&buffer[..end])
                .map_err(|_| SessionError::Transport("unreadable status line".to_string()))?;
            return line
                .split(' ')
                .nth(1)
                .and_then(|code| code.parse().ok())
                .ok_or_else(|| SessionError::Transport(format!("unreadable status: {line}")));
        }
        if buffer.len() > 1024 {
            return Err(SessionError::Transport(
                "the peer did not send an HTTP status line".to_string(),
            ));
        }
        match stream.read(&mut chunk) {
            Ok(0) => {
                return Err(SessionError::Transport(
                    "the target closed without answering".to_string(),
                ))
            }
            Ok(read) => buffer.extend_from_slice(&chunk[..read]),
            Err(error) => return Err(SessionError::Transport(format!("read: {error}"))),
        }
    }
}

/// The shapes an external run measures. The same five `web.static` does.
pub const DRIVE_SHAPES: &[DriveShape] = &[
    DriveShape {
        key: "requests.small_keepalive",
        bytes: 1024,
        reuse: Reuse::KeepAlive,
    },
    DriveShape {
        // Not `connections.plaintext`, which is what `web.static` calls its
        // in-process equivalent. A session has one origin and it is either
        // plaintext or TLS for all of it, so naming the transport in the shape
        // would have labelled a TLS measurement "plaintext" whenever the
        // operator passed --tls. The transport is recorded once, on the
        // session, where it is true.
        key: "connections.per_request",
        bytes: 1024,
        reuse: Reuse::PerRequest,
    },
    DriveShape {
        key: "throughput.medium",
        bytes: 64 * 1024,
        reuse: Reuse::KeepAlive,
    },
    DriveShape {
        key: "throughput.large",
        bytes: 1024 * 1024,
        reuse: Reuse::KeepAlive,
    },
];

impl std::fmt::Debug for TargetSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TargetSession")
            .field("origin", &self.origin.address())
            .field("control", &self.control_address)
            .field("session", &self.ticket.offer.session_id)
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for Driver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Driver")
            .field("origin", &self.ticket.origin)
            .field("session", &self.accepted.session_id)
            .field("granted", &self.accepted.granted)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    const LOOPBACK: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);
    const KIB: usize = 1024;

    fn target(tls: bool) -> TargetSession {
        TargetSession::start(
            LOOPBACK,
            vec![KIB],
            tls,
            Duration::from_secs(MIN_SESSION_SECS),
        )
        .unwrap()
    }

    /// A modest plan. The point of these tests is the protocol, not the
    /// machine, so every one of them asks for load two orders of magnitude
    /// below what a loopback origin serves - a test that flakes under load
    /// tells you nothing about the thing it was written for.
    fn modest() -> LoadRequest {
        LoadRequest {
            rate_per_s: 200.0,
            duration_ms: 300,
            workers: 4,
            phases: 1,
        }
    }

    #[test]
    fn a_session_runs_end_to_end_and_reconciles() {
        // The whole feature in one test: mint a ticket, carry it, refuse to
        // proceed without an offer, load the origin, post the report, and have
        // the target check the claim against what it actually served.
        let target = target(false);
        let carried = target.ticket().encode().unwrap();

        let driver = Driver::open(Ticket::decode(&carried).unwrap(), modest()).unwrap();
        let shape = DriveShape {
            key: "requests.small_keepalive",
            bytes: KIB,
            reuse: Reuse::KeepAlive,
        };
        let report = driver.measure(shape).unwrap();
        assert!(report.requests_completed > 0, "{report:?}");
        driver.submit(vec![report]).unwrap();

        let result = target.wait(Duration::from_millis(50)).unwrap();
        assert_eq!(result.report.shapes.len(), 1);
        assert!(result.served >= result.report.claimed_requests());
        assert_eq!(result.refused, 0);
    }

    #[test]
    fn a_session_runs_end_to_end_over_tls() {
        // The control channel shares the origin's certificate, so a generator
        // that pinned the origin can post its report with the pin it has.
        let target = target(true);
        let driver = Driver::open(target.ticket().clone(), modest()).unwrap();
        let report = driver
            .measure(DriveShape {
                key: "requests.small_keepalive",
                bytes: KIB,
                reuse: Reuse::KeepAlive,
            })
            .unwrap();
        assert!(report.requests_completed > 0, "{report:?}");
        driver.submit(vec![report]).unwrap();
        assert!(target.wait(Duration::from_millis(50)).is_ok());
    }

    #[test]
    fn the_generator_refuses_a_peer_whose_protocol_it_does_not_speak() {
        // The consent property: there is no path from `Driver` to the load
        // generator that does not pass through `SessionOffer::accept`, so a
        // peer that did not produce a recognisable offer is never loaded.
        let target = target(false);
        let mut ticket = target.ticket().clone();
        ticket.offer.protocol = "darcbench.external/99".to_string();
        let error = Driver::open(ticket, modest()).unwrap_err();
        assert!(
            matches!(
                error,
                SessionError::Refused(Refusal::UnsupportedProtocol { .. })
            ),
            "{error}"
        );
        // And the origin served nothing, because there is no path from
        // `Driver::open` to the load generator that skips the refusal.
        assert_eq!(target.served(), 0);
    }

    #[test]
    fn a_report_without_the_token_is_refused_by_the_control_listener() {
        let target = target(false);
        let mut ticket = target.ticket().clone();
        ticket.token = SessionToken::try_new().unwrap();
        let driver = Driver::open(ticket, modest()).unwrap();
        let error = driver.submit(Vec::new()).unwrap_err();
        assert!(
            matches!(&error, SessionError::Transport(message) if message.contains("token")),
            "{error}"
        );
    }

    #[test]
    fn a_session_accepts_one_report_and_not_two() {
        // A second report would either overwrite a reconciled result or race
        // the reconciliation.
        let target = target(false);
        let driver = Driver::open(target.ticket().clone(), modest()).unwrap();
        let report = driver
            .measure(DriveShape {
                key: "requests.small_keepalive",
                bytes: KIB,
                reuse: Reuse::KeepAlive,
            })
            .unwrap();
        driver.submit(vec![report.clone()]).unwrap();
        let error = driver.submit(vec![report]).unwrap_err();
        assert!(
            matches!(&error, SessionError::Transport(message) if message.contains("already")),
            "{error}"
        );
    }

    #[test]
    fn a_generator_that_inflates_its_numbers_is_caught_by_the_target() {
        // The anti-fabrication property, exercised against a real origin
        // rather than a hand-built report: the driver measures honestly, the
        // number is then multiplied by a thousand, and the target rejects it
        // because a response the generator read is a response the origin sent.
        let target = target(false);
        let driver = Driver::open(target.ticket().clone(), modest()).unwrap();
        let mut report = driver
            .measure(DriveShape {
                key: "requests.small_keepalive",
                bytes: KIB,
                reuse: Reuse::KeepAlive,
            })
            .unwrap();
        report.requests_completed = report.requests_completed.saturating_mul(1000) + 1000;
        driver.submit(vec![report]).unwrap();

        let error = target.wait(Duration::from_millis(50)).unwrap_err();
        assert!(
            matches!(
                error,
                SessionError::Rejected(ReportRejected::ClaimExceedsServed { .. })
            ),
            "{error}"
        );
    }

    #[test]
    fn a_third_party_loading_the_origin_invalidates_the_session() {
        // Not a fabrication - the generator is honest here. The origin served
        // requests the report cannot account for, so the numbers describe this
        // machine serving two clients rather than one.
        let target = target(false);
        let driver = Driver::open(target.ticket().clone(), modest()).unwrap();
        let report = driver
            .measure(DriveShape {
                key: "requests.small_keepalive",
                bytes: KIB,
                reuse: Reuse::KeepAlive,
            })
            .unwrap();

        // Someone else, with the token, doing more than the allowance.
        let interloper = web_static::HttpClient::new(
            target.origin_address(),
            "/o/1024".to_string(),
            KIB,
            Reuse::KeepAlive,
            None,
            1,
        )
        .with_session_token(target.ticket().token.to_hex());
        for _ in 0..(darcbench_protocol::external::RECONCILE_SLACK_REQUESTS + 64) {
            crate::loadgen::LoadTarget::request(&interloper, 0).unwrap();
        }

        driver.submit(vec![report]).unwrap();
        let error = target.wait(Duration::from_millis(50)).unwrap_err();
        assert!(
            matches!(
                error,
                SessionError::Rejected(ReportRejected::UnaccountedLoad { .. })
            ),
            "{error}"
        );
    }

    #[test]
    fn requests_that_never_authenticated_are_disclosed_and_not_fatal() {
        // A port scanner must not be able to invalidate somebody's benchmark,
        // and must not be invisible either.
        let target = target(false);
        let driver = Driver::open(target.ticket().clone(), modest()).unwrap();
        let report = driver
            .measure(DriveShape {
                key: "requests.small_keepalive",
                bytes: KIB,
                reuse: Reuse::KeepAlive,
            })
            .unwrap();

        let stranger = web_static::HttpClient::new(
            target.origin_address(),
            "/o/1024".to_string(),
            KIB,
            Reuse::PerRequest,
            None,
            1,
        );
        for _ in 0..2000 {
            let _ = crate::loadgen::LoadTarget::request(&stranger, 0);
        }

        driver.submit(vec![report]).unwrap();
        let result = target.wait(Duration::from_millis(50)).unwrap();
        assert!(result.refused >= 2000, "refused {}", result.refused);
    }

    #[test]
    fn a_ticket_round_trips_and_never_prints_its_secret() {
        let target = target(false);
        let ticket = target.ticket();
        let carried = ticket.encode().unwrap();
        assert!(!carried.contains(' '), "a ticket must survive a paste");

        let back = Ticket::decode(&format!("  {carried}\n")).unwrap();
        assert_eq!(back.origin, ticket.origin);
        assert!(back.token.matches(&ticket.token));

        let rendered = format!("{ticket} {ticket:?}");
        assert!(
            !rendered.contains(&ticket.token.to_hex()[..8]),
            "a ticket printed its token: {rendered}"
        );
    }

    #[test]
    fn a_session_shorter_or_longer_than_the_bounds_is_refused() {
        for seconds in [0, MIN_SESSION_SECS - 1, MAX_SESSION_SECS + 1] {
            let error =
                TargetSession::start(LOOPBACK, vec![KIB], false, Duration::from_secs(seconds))
                    .unwrap_err();
            assert!(
                matches!(error, SessionError::SessionLength(_)),
                "{seconds}s: {error}"
            );
        }
    }

    #[test]
    fn a_driver_refuses_a_size_the_target_never_offered() {
        // Otherwise the run is a phase of 404s, which produce plausible-looking
        // throughput for no work.
        let target = target(false);
        let driver = Driver::open(target.ticket().clone(), modest()).unwrap();
        let error = driver
            .measure(DriveShape {
                key: "throughput.large",
                bytes: 1024 * 1024,
                reuse: Reuse::KeepAlive,
            })
            .unwrap_err();
        assert!(
            matches!(&error, SessionError::Transport(message) if message.contains("does not serve")),
            "{error}"
        );
    }

    #[test]
    fn the_control_listener_refuses_a_body_larger_than_its_bound() {
        // Checked against the declared length before a byte of it is read.
        let target = target(false);
        let mut stream = TcpStream::connect(target.control_address()).unwrap();
        let request = format!(
            "POST /report HTTP/1.1\r\nHost: x\r\n{TOKEN_HEADER}: {}\r\n\
             Content-Length: {}\r\n\r\n",
            target.ticket().token.to_hex(),
            MAX_CONTROL_BYTES + 1
        );
        stream.write_all(request.as_bytes()).unwrap();
        stream.flush().unwrap();
        let mut answer = String::new();
        let _ = stream.read_to_string(&mut answer);
        assert!(answer.starts_with("HTTP/1.1 413"), "{answer}");
    }

    #[test]
    fn the_control_listener_answers_an_unknown_endpoint_identically_to_a_wrong_token() {
        // Not identically in text - the point is that neither reveals whether
        // a session is live here. A peer without the token gets 401 whatever
        // path it asks for, so it cannot enumerate endpoints.
        let target = target(false);
        for path in ["/report", "/session", "/", "/../etc/passwd"] {
            let mut stream = TcpStream::connect(target.control_address()).unwrap();
            let request = format!("POST {path} HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\n\r\n");
            stream.write_all(request.as_bytes()).unwrap();
            stream.flush().unwrap();
            let mut answer = String::new();
            let _ = stream.read_to_string(&mut answer);
            assert!(answer.starts_with("HTTP/1.1 401"), "{path}: {answer}");
        }
    }

    #[test]
    fn a_percentile_of_a_single_sample_does_not_index_past_the_end() {
        let summary = summarise(&[7.0]);
        assert_eq!(summary.count, 1);
        assert_eq!(summary.p99_ms, 7.0);
        assert_eq!(summary.max_ms, 7.0);
        assert_eq!(summarise(&[]).count, 0);
        // Non-finite samples cannot reach a percentile, because a sort that
        // sees a NaN has no defined order to produce.
        assert_eq!(summarise(&[f64::NAN, f64::INFINITY]).count, 0);
    }

    #[test]
    fn summarising_does_not_reorder_the_runs_evidence() {
        let samples = vec![9.0, 1.0, 5.0];
        let copy = samples.clone();
        let summary = summarise(&samples);
        assert_eq!(samples, copy, "the sample series was sorted in place");
        assert_eq!(summary.p50_ms, 5.0);
    }
}
