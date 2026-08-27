//! `web.static` - the Phase 3 web module.
//!
//! # What it measures
//!
//! | Metric | Unit | What it predicts |
//! |---|---|---|
//! | `requests.small_keepalive` | req/s | Small-object serving on a warm connection: an API, a cached page |
//! | `connections.plaintext` | conn/s | Cost of a new TCP connection per request - a client that will not keep alive |
//! | `connections.tls` | conn/s | The same with a full TLS handshake: what every fresh HTTPS visitor costs |
//! | `throughput.medium` | MiB/s | 64 KiB objects: a typical page's assets |
//! | `throughput.large` | MiB/s | 1 MiB objects: images, downloads, media |
//! | `latency.small_mean` | ms | Response time at a load the machine can actually sustain |
//! | `latency.small_p99` | ms | The slow 1% - what a user notices |
//!
//! # The origin is ours, and that is deliberate
//!
//! `docs/THREAT-MODEL.md` T-AMPLIFY is permanent: *"HTTP load generation
//! targets **only** a server the agent started on loopback. There will be no
//! 'benchmark this URL' feature."* So this module starts its own origin, loads
//! it, and destroys it inside the run.
//!
//! That is not merely a safety constraint, it is the right measurement. A run
//! against the operator's own nginx would measure their configuration - worker
//! counts, buffer sizes, sendfile, the modules they compiled in - which is
//! worth knowing and is not something a *machine* score can be built from. The
//! same argument the methodology makes about WordPress: "WordPress performance"
//! without a cache disclosure is meaningless. Here every machine runs the same
//! server, so a difference between two machines is a difference between two
//! machines.
//!
//! # What it deliberately does not measure
//!
//! - **HTTP/2 and HTTP/3.** Both need a protocol stack this workspace does not
//!   have, and approximating them over HTTP/1.1 would be a guess wearing a
//!   precise name. Declared unmeasured, in the same terms as packet loss in
//!   `network.transfer`.
//! - **Compression.** Serving `Content-Encoding: gzip` would measure this
//!   machine's deflate throughput, which `cpu.mixed` already measures directly
//!   and scores under Compute. Measuring it again here would count the same CPU
//!   twice in two categories.
//! - **The operator's web server.** See above.
//!
//! # The generator shares the machine with the target
//!
//! Unavoidable for a local injector, and it means every number here is a
//! **floor**: the CPU the generator used was CPU the origin did not get. That
//! is what the external-generator mode on the roadmap is for, and it is why
//! generator saturation degrades the result rather than being tolerated -
//! see [`crate::loadgen`] and `docs/adr/0012-load-generation.md`.

use std::cell::RefCell;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use darcbench_protocol::metrics::{Direction, Metric, Warning, WarningCode};
use darcbench_protocol::stats::{outlier_indices, summarize};
use darcbench_protocol::ModuleId;

use crate::harness::time_reps;
use crate::loadgen::{self, LoadPlan, LoadTarget};
use crate::module::{
    BenchmarkModule, ModuleError, ModuleManifest, ModuleOutput, ModuleParams, ModuleReporter,
    SafetyClass,
};
use crate::web_origin::{Origin, OriginConfig};

/// Workload-definition version. Major bump = results are not comparable.
pub const VERSION: &str = "1.0.0";

/// The module's identifier, validated against the [`ModuleId`] grammar by a
/// unit test in this file.
pub const MODULE_ID: &str = "web.static";

/// Object sizes served, in bytes.
///
/// Three, spanning the range a web server actually sees: an API response, a
/// page asset, and a download. One size would answer one question and be
/// generalised to all three by whoever read it.
const SMALL_BYTES: usize = 1024;
const MEDIUM_BYTES: usize = 64 * 1024;
const LARGE_BYTES: usize = 1024 * 1024;

/// Concurrent connections the generator opens.
///
/// Tied to the machine's CPU count rather than fixed, because the question is
/// what *this* machine serves and a fixed four connections would leave a
/// 64-core host idle. Bounded at 64: past that the measurement is dominated by
/// context switching between connections on both sides of a loopback socket,
/// which is not a property of the machine anyone is buying.
const MAX_CONNECTIONS: usize = 64;

/// How much of a response may arrive before the client gives up looking for
/// the end of its headers.
///
/// Matches the origin's own [`crate::web_origin::MAX_HEAD_BYTES`] on requests.
/// It bounds a peer that never terminates its headers; it is *not* a bound on
/// how much of a response may be buffered, and confusing the two is what the
/// comment in [`read_response`] is about.
const MAX_RESPONSE_HEAD_BYTES: usize = 8 * 1024;

/// How many more connections the latency phase opens than the capacity probe.
///
/// The capacity probe is a tight loop: take a connection, issue, repeat. The
/// open-model phase does strictly more per request - it computes a due time,
/// sleeps to it, and records three timings - so a worker there sustains a lower
/// rate than a worker in the probe does. Scheduling the latency phase at 70% of
/// measured capacity across the *same* number of connections therefore starved
/// every worker and reported the run saturated, which was true and was the
/// generator's own fault.
///
/// Two, not more: every extra worker is a thread that must be woken on time,
/// and on a machine with few cores over-subscribing them adds exactly the
/// wake-up jitter the schedule is trying to avoid. The saturation detector is
/// what catches the case where two is not enough - it is not being tuned away
/// here, it is being given a generator that can do its job.
const LATENCY_CONNECTION_FACTOR: usize = 2;

/// How many times the latency phase may halve its offered rate looking for one
/// the generator can actually hold.
///
/// A local injector on a small machine cannot offer 70% of what that machine
/// can serve: the generator and the origin are competing for the same cores,
/// and the generator does strictly more work per request than the origin does.
/// On a four-core host offering 140,000 requests a second, it saturates - and
/// it is right to say so.
///
/// The wrong response would be to loosen the detector until it stopped
/// complaining, which is how a benchmark ends up publishing a latency
/// distribution that describes its own injector. The right one is to offer a
/// rate the generator can hold and *say what fraction of capacity that was*, so
/// the number means something exact rather than something flattering. Four
/// halvings reach 2% of capacity; below that the phase is not measuring
/// anything about load and the saturation warning stands.
const LATENCY_RATE_ATTEMPTS: usize = 5;

/// Share of measured capacity the latency phase starts from.
///
/// Latency at capacity is meaningless - it is queueing delay, and it grows
/// without bound as the offered load approaches what the machine can serve. The
/// number an operator can act on is latency at a load the machine comfortably
/// sustains, and the conventional headroom figure for that is 70%.
///
/// This is a quarter, and the difference is the whole reason the
/// external-generator mode exists. `capacity` is measured by a tight closed
/// loop that does almost nothing per request; the open-model generator computes
/// a due time, sleeps to it and records three timings, and on loopback - where
/// serving a 1 KiB object costs microseconds - that overhead is *comparable to
/// the work being measured*. Offering 70% of capacity therefore asks one
/// machine for roughly 170% of it, and no machine has that.
///
/// A quarter is a starting point rather than a promise: the search below halves
/// it until the generator can hold the schedule, and the share actually offered
/// is published in the module context so the latency figure says exactly what
/// load it describes. Against an external injector the full 70% becomes
/// reachable, and this constant is what that deliverable will change.
const LATENCY_LOAD_SHARE: f64 = 0.25;

/// Seconds of load per repetition when the profile has not said otherwise.
const DEFAULT_PHASE_MS: u64 = 300;

// ---------------------------------------------------------------------------
// The client
// ---------------------------------------------------------------------------

/// How a connection is used, which is one of the things being measured.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reuse {
    /// One connection serves many requests. What a browser and every sane
    /// client does.
    KeepAlive,
    /// A new connection per request, so the measurement is connection setup
    /// rather than serving.
    PerRequest,
}

/// An HTTP/1.1 client driven by hand, one connection slot per worker.
///
/// Written out rather than taken from a crate for the same reason
/// `network.transfer` drives its own connection (ADR-0011): a client that
/// manages its own pool would decide the thing being measured - when a
/// connection is reused, when it is closed, how many are open - and those are
/// the measurement.
pub(crate) struct HttpClient {
    address: SocketAddr,
    path: String,
    expected_bytes: usize,
    reuse: Reuse,
    tls: Option<Arc<rustls::ClientConfig>>,
    /// One slot per worker. Each worker touches only its own index, so the
    /// mutex is uncontended and exists to satisfy the borrow checker rather
    /// than to synchronise anything.
    slots: Vec<Mutex<Option<Connection>>>,
    /// The session token, on a token-gated origin.
    ///
    /// Pre-rendered as the whole header line rather than as the token, so the
    /// hot path concatenates a string it was handed instead of formatting one
    /// per request. `None` for the in-process origin, which has no gate.
    session_header: Option<String>,
}

pub(crate) enum Connection {
    Plain(TcpStream),
    Tls(Box<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>),
}

impl Connection {
    fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()> {
        match self {
            Self::Plain(stream) => stream.write_all(buf),
            Self::Tls(stream) => stream.write_all(buf),
        }
    }

    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::Plain(stream) => stream.read(buf),
            Self::Tls(stream) => stream.read(buf),
        }
    }
}

impl HttpClient {
    pub(crate) fn new(
        address: SocketAddr,
        path: String,
        expected_bytes: usize,
        reuse: Reuse,
        tls: Option<Arc<rustls::ClientConfig>>,
        workers: usize,
    ) -> Self {
        Self {
            address,
            path,
            expected_bytes,
            reuse,
            tls,
            slots: (0..workers).map(|_| Mutex::new(None)).collect(),
            session_header: None,
        }
    }

    /// Presents `token` on every request, for an external, token-gated origin.
    pub(crate) fn with_session_token(mut self, token: String) -> Self {
        self.session_header = Some(format!("x-darcbench-session: {token}\r\n"));
        self
    }

    fn connect(&self) -> Result<Connection, String> {
        let stream = TcpStream::connect_timeout(&self.address, Duration::from_secs(5))
            .map_err(|error| format!("connect: {error}"))?;
        // Nagle would batch the request write behind the previous response and
        // turn a latency measurement into a measurement of Nagle.
        stream
            .set_nodelay(true)
            .map_err(|error| format!("nodelay: {error}"))?;
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .map_err(|error| format!("timeout: {error}"))?;

        let Some(config) = &self.tls else {
            return Ok(Connection::Plain(stream));
        };
        let server = rustls_pki_types::ServerName::IpAddress(self.address.ip().into());
        let connection = rustls::ClientConnection::new(Arc::clone(config), server)
            .map_err(|error| format!("tls setup: {error}"))?;
        Ok(Connection::Tls(Box::new(rustls::StreamOwned::new(
            connection, stream,
        ))))
    }
}

impl LoadTarget for HttpClient {
    fn request(&self, worker: usize) -> Result<u64, String> {
        let slot = self
            .slots
            .get(worker % self.slots.len().max(1))
            .ok_or_else(|| "no connection slot".to_string())?;
        let mut held = slot.lock().map_err(|_| "connection slot poisoned")?;

        if held.is_none() {
            *held = Some(self.connect()?);
        }
        let connection = held.as_mut().ok_or("connection slot empty")?;

        // `Connection: close` on the per-request shape, so the *server* also
        // tears the connection down. Leaving it to the client alone would
        // measure a machine that keeps accumulating sockets in TIME_WAIT while
        // pretending each request paid for its own setup.
        let close = self.reuse == Reuse::PerRequest;
        let request = format!(
            "GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: darcbench/{VERSION}\r\n\
             Connection: {}\r\n{}\r\n",
            self.path,
            // The bound address rather than a constant. An external origin
            // reached at 10.0.0.5 and told `Host: 127.0.0.1` is being sent a
            // request that names somebody else's machine; nothing here reads
            // it today, and a proxy in front of one eventually would.
            self.address,
            if close { "close" } else { "keep-alive" },
            self.session_header.as_deref().unwrap_or(""),
        );

        let outcome = read_response(connection, request.as_bytes(), self.expected_bytes);
        // A failed connection is never reused: the next request on it would
        // fail for a reason that has nothing to do with the machine.
        if outcome.is_err() || close {
            *held = None;
        }
        outcome
    }
}

/// Writes a request and reads exactly one response, returning its body length.
///
/// Deliberately strict about `Content-Length`. A body short of what the origin
/// promised is a truncated read, and counting it as a served request would
/// report a machine as fast because it failed quickly - the same failure mode
/// the network module's rate guard exists to prevent.
fn read_response(
    connection: &mut Connection,
    request: &[u8],
    expected_bytes: usize,
) -> Result<u64, String> {
    connection
        .write_all(request)
        .map_err(|error| format!("write: {error}"))?;

    let mut buffer = vec![0u8; 16 * 1024];
    let mut head = Vec::with_capacity(512);
    let mut body_seen = 0usize;
    let mut header_end = None;

    loop {
        let read = connection
            .read(&mut buffer)
            .map_err(|error| format!("read: {error}"))?;
        if read == 0 {
            return Err("connection closed before the response was complete".into());
        }
        let chunk = &buffer[..read];

        match header_end {
            Some(_) => body_seen += read,
            None => {
                head.extend_from_slice(chunk);
                // The search runs BEFORE the size bound, and the order is the
                // whole of this arm's correctness. A read returns up to 16 KiB
                // and a response is a small head followed by the body, so the
                // first read of a 64 KiB or 1 MiB object routinely delivers
                // the head plus thousands of bytes of body in one chunk.
                // Checking the length first counted those body bytes as
                // header, tripped the 8 KiB bound, and failed the request -
                // with the header end sitting a hundred bytes into the buffer.
                //
                // Found by the external generator, which offers a fixed rate
                // to every shape and so drove the large-object shapes far
                // harder than `web.static`'s own capacity-derived rate ever
                // did. It was never external-only: it depended on how much of
                // a response one `read` happened to return, so on loopback it
                // was a rare failure and at volume it was every request.
                if let Some(position) = find_header_end(&head) {
                    header_end = Some(position);
                    body_seen = head.len() - position;
                } else if head.len() > MAX_RESPONSE_HEAD_BYTES {
                    return Err(format!(
                        "no end of headers in the first {MAX_RESPONSE_HEAD_BYTES} bytes"
                    ));
                }
            }
        }

        if let Some(position) = header_end {
            let status = status_code(&head[..position])
                .ok_or("response did not begin with an HTTP/1.x status line")?;
            if status != 200 {
                return Err(format!("origin answered HTTP {status}"));
            }
            if body_seen >= expected_bytes {
                return Ok(body_seen as u64);
            }
        }
    }
}

fn find_header_end(buffer: &[u8]) -> Option<usize> {
    buffer
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|position| position + 4)
}

fn status_code(head: &[u8]) -> Option<u16> {
    let line = head.split(|byte| *byte == b'\r').next()?;
    let text = std::str::from_utf8(line).ok()?;
    let mut parts = text.split_whitespace();
    if !parts.next()?.starts_with("HTTP/1.") {
        return None;
    }
    parts.next()?.parse().ok()
}

/// A client config that trusts exactly the origin's own certificate.
///
/// Not the host trust store, which is the opposite of what `network.transfer`
/// does and correct for the opposite reason: there the peer is a public service
/// and the machine's administrator decides who to trust, here the peer is a
/// certificate this process generated seconds ago for a listener on loopback.
/// Trusting anything else would be strictly worse, and disabling verification
/// would leave the handshake measuring something no real client performs.
pub(crate) fn tls_config(
    certificate: rustls_pki_types::CertificateDer<'static>,
) -> Result<Arc<rustls::ClientConfig>, ModuleError> {
    let mut roots = rustls::RootCertStore::empty();
    roots.add(certificate).map_err(|error| {
        ModuleError::Precondition(format!("origin certificate rejected: {error}"))
    })?;
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(Arc::new(config))
}

// ---------------------------------------------------------------------------
// The module
// ---------------------------------------------------------------------------

/// One measured shape: an object size, a connection policy and a protocol.
struct Shape {
    key: &'static str,
    label: &'static str,
    unit: &'static str,
    bytes: usize,
    reuse: Reuse,
    tls: bool,
    /// True when the metric is bytes per second rather than requests per
    /// second. A 1 MiB object served 400 times a second is a bandwidth figure;
    /// reporting it as "400" next to a small-object figure of 90,000 would
    /// invite the reading that the machine got slower with bigger files.
    throughput: bool,
}

const SHAPES: &[Shape] = &[
    Shape {
        key: "requests.small_keepalive",
        label: "Small objects, keep-alive",
        unit: "req/s",
        bytes: SMALL_BYTES,
        reuse: Reuse::KeepAlive,
        tls: false,
        throughput: false,
    },
    Shape {
        key: "connections.plaintext",
        label: "New connection per request",
        unit: "conn/s",
        bytes: SMALL_BYTES,
        reuse: Reuse::PerRequest,
        tls: false,
        throughput: false,
    },
    Shape {
        key: "connections.tls",
        label: "New TLS connection per request",
        unit: "conn/s",
        bytes: SMALL_BYTES,
        reuse: Reuse::PerRequest,
        tls: true,
        throughput: false,
    },
    Shape {
        key: "throughput.medium",
        label: "64 KiB objects",
        unit: "MiB/s",
        bytes: MEDIUM_BYTES,
        reuse: Reuse::KeepAlive,
        tls: false,
        throughput: true,
    },
    Shape {
        key: "throughput.large",
        label: "1 MiB objects",
        unit: "MiB/s",
        bytes: LARGE_BYTES,
        reuse: Reuse::KeepAlive,
        tls: false,
        throughput: true,
    },
];

pub struct WebStatic {
    manifest: ModuleManifest,
}

impl Default for WebStatic {
    fn default() -> Self {
        Self::new()
    }
}

impl WebStatic {
    pub fn new() -> Self {
        // Justified `expect`: `MODULE_ID` is a compile-time constant whose
        // validity under the ModuleId grammar is asserted by a unit test in
        // this file. There is no runtime input here to fail on.
        #[allow(clippy::expect_used)]
        let id = ModuleId::new(MODULE_ID).expect("MODULE_ID is a valid module id");
        Self {
            manifest: ModuleManifest {
                id,
                version: VERSION.into(),
                title: "Static object serving".into(),
                purpose: "Measure how many HTTP requests this machine serves per second, at three \
                          object sizes, with and without connection reuse, over plaintext and \
                          TLS - against an origin the agent starts on loopback and destroys when \
                          the run ends."
                    .into(),
                // The module starts a service of its own, which is exactly what
                // this class describes. It is the first module to use it.
                safety_class: SafetyClass::ProvisionsServices,
                dependencies: vec![],
                max_bytes_written: 0,
                // Loopback only. Nothing leaves the machine, which is why this
                // is zero rather than the volume the loopback socket carries:
                // the field bounds what the operator's link pays for.
                max_network_bytes: 0,
                cleanup: "The origin listener and its connections are closed when the module \
                          returns, on every path including cancellation. Nothing is written to \
                          disk and no configuration is read or altered."
                    .into(),
                validation: vec![
                    "A phase whose load generator could not hold its schedule is reported \
                     GeneratorSaturated and degrades the result: a latency distribution recorded \
                     while the injector was behind describes the injector."
                        .into(),
                    "Every metric needs at least three successful repetitions; below that it is \
                     withheld rather than reported from noise."
                        .into(),
                    "A response shorter than the origin's declared Content-Length is an error, \
                     never a fast request."
                        .into(),
                ],
                limitations: vec![
                    "The load generator shares the machine with the origin it measures, so every \
                     figure here is a floor: the CPU the generator used is CPU the origin did not \
                     get. An external-generator mode is on the roadmap for machines fast enough \
                     to outrun a local injector."
                        .into(),
                    "HTTP/1.1 only. HTTP/2 and HTTP/3 need protocol stacks this build does not \
                     carry, and approximating them over HTTP/1.1 would be a guess wearing a \
                     precise name."
                        .into(),
                    "Compression is not measured. Serving gzip would measure this machine's \
                     deflate throughput, which `cpu.mixed` already measures and scores under \
                     Compute; counting it again here would charge the same CPU to two categories."
                        .into(),
                    "The origin is DARCBench's own, not the operator's web server. That is a \
                     permanent constraint (docs/THREAT-MODEL.md, T-AMPLIFY) and it is also what \
                     makes two machines comparable - a run against somebody's nginx measures \
                     their configuration."
                        .into(),
                    "Loopback has no packet loss, no reordering and an MTU unlike any real \
                     network. This measures the machine's HTTP stack, never its connectivity; \
                     `network.transfer` measures the second."
                        .into(),
                ],
                comparability: vec![
                    "module.version".into(),
                    "agent.build_target".into(),
                    "web.connections".into(),
                    "web.object_sizes".into(),
                ],
                // The methodology's `web.*` row: warn above 0.20.
                stability_cv_bound: 0.20,
            },
        }
    }

    fn connections(&self, params: &ModuleParams) -> usize {
        params.effective_threads().clamp(2, MAX_CONNECTIONS)
    }

    fn phase_duration(&self, params: &ModuleParams) -> Duration {
        Duration::from_millis(params.target_rep_ms.max(DEFAULT_PHASE_MS))
    }
}

impl BenchmarkModule for WebStatic {
    fn manifest(&self) -> &ModuleManifest {
        &self.manifest
    }

    fn estimated_duration_s(&self, params: &ModuleParams) -> u64 {
        let reps = u64::from(params.warmup_reps + params.measured_reps);
        let phase_s = self.phase_duration(params).as_secs_f64();
        // Every shape, plus the latency phase, plus a generous allowance for
        // connection setup on the per-request shapes. Rounded up: preflight may
        // be pessimistic and may never be optimistic.
        let shapes = SHAPES.len() as u64 + 1;
        ((reps * shapes) as f64 * phase_s * 1.5).ceil() as u64 + 2
    }

    fn estimated_peak_memory_bytes(&self, params: &ModuleParams) -> u64 {
        // The origin holds one copy of each object body, and each connection
        // holds a 16 KiB read buffer.
        let bodies = (SMALL_BYTES + MEDIUM_BYTES + LARGE_BYTES) as u64;
        bodies + (self.connections(params) as u64 * 16 * 1024)
    }

    fn run(
        &self,
        params: &ModuleParams,
        reporter: &dyn ModuleReporter,
    ) -> Result<ModuleOutput, ModuleError> {
        let connections = self.connections(params);
        let phase = self.phase_duration(params);
        let mut metrics = Vec::new();
        let mut warnings = Vec::new();

        // Two origins, because a listener is either plaintext or TLS and both
        // are being measured. They are separate ports on the same loopback
        // interface serving identical bodies, so the only difference between a
        // plaintext number and a TLS one is the TLS.
        //
        // Both live for the whole module and are dropped on every exit path,
        // including the `?` returns below and cancellation - which is what
        // makes the manifest's cleanup promise hold without a single explicit
        // teardown call.
        let sizes = vec![SMALL_BYTES, MEDIUM_BYTES, LARGE_BYTES];
        let start = |tls: bool| {
            Origin::start(OriginConfig {
                object_sizes: sizes.clone(),
                tls,
                // In-process, on loopback, for the length of this module. The
                // only client is the generator two threads away, so there is
                // nothing a token would keep out and it would put a comparison
                // in the measured path.
                ..OriginConfig::default()
            })
            .map_err(|error| {
                ModuleError::Precondition(format!(
                    "could not start the loopback origin this module measures: {error}. The agent \
                     binds 127.0.0.1 on an OS-assigned port and touches no existing service, so \
                     this usually means loopback networking is unavailable in this sandbox."
                ))
            })
        };
        let plain_origin = start(false)?;
        let tls_origin = start(true)?;

        let tls = tls_config(tls_origin.certificate_der().ok_or_else(|| {
            ModuleError::Precondition(
                "the origin produced no certificate for its TLS listener".into(),
            )
        })?)?;

        // Shapes plus one latency phase.
        let total_units = SHAPES.len() as f64 + 1.0;
        let mut completed_units = 0.0;
        let mut small_capacity: Option<f64> = None;

        for shape in SHAPES {
            if reporter.is_cancelled() {
                return Err(ModuleError::Cancelled);
            }
            let origin = if shape.tls {
                &tls_origin
            } else {
                &plain_origin
            };
            let path = origin.path_for(shape.bytes).ok_or_else(|| {
                ModuleError::Workload(format!("origin serves no {} B object", shape.bytes))
            })?;
            let client = HttpClient::new(
                origin.address(),
                path,
                shape.bytes,
                shape.reuse,
                shape.tls.then(|| Arc::clone(&tls)),
                connections,
            );

            let outcome = time_reps(
                params,
                reporter,
                shape.key,
                shape.unit,
                completed_units,
                total_units,
                |_rep| {
                    let started = Instant::now();
                    let rate = loadgen::measure_capacity(&client, connections, phase);
                    let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
                    let value = if shape.throughput {
                        // Requests per second times the object size, in MiB/s.
                        rate * shape.bytes as f64 / (1024.0 * 1024.0)
                    } else {
                        rate
                    };
                    (value, elapsed_ms)
                },
            )?;
            warnings.extend(outcome.warnings.clone());
            completed_units += 1.0;

            let usable: Vec<f64> = outcome
                .measured
                .iter()
                .copied()
                .filter(|v| *v > 0.0)
                .collect();
            if shape.key == "requests.small_keepalive" && !usable.is_empty() {
                small_capacity = median(&usable);
            }
            match summarize(&usable) {
                Some(summary) if usable.len() >= 3 => metrics.push(Metric {
                    key: shape.key.into(),
                    label: shape.label.into(),
                    value: summary.median,
                    unit: shape.unit.into(),
                    direction: Direction::HigherIsBetter,
                    outliers: outlier_indices(&usable, 3.5),
                    summary,
                    samples: outcome.samples,
                    measures_dispersion: false,
                    tail_quantile: false,
                }),
                _ => warnings.push(Warning {
                    code: WarningCode::ValidationFailed,
                    message: format!(
                        "`{}` produced {} usable repetitions, below the three needed to report a \
                         value; it is withheld rather than published from noise.",
                        shape.key,
                        usable.len()
                    ),
                    metric_key: Some(shape.key.to_string()),
                }),
            }
        }

        // --- latency, under a load the machine can actually sustain ---------
        //
        // Everything above is capacity, measured closed-loop. Latency needs the
        // open model and a rate below capacity - see `loadgen`'s documentation
        // for why each is the right tool for its own question.
        let Some(capacity) = small_capacity.filter(|c| *c > 0.0) else {
            warnings.push(Warning {
                code: WarningCode::ValidationFailed,
                message: "no small-object capacity was measured, so there is no rate at which to \
                          offer a latency phase; the latency metrics are withheld."
                    .into(),
                metric_key: None,
            });
            return finish(metrics, warnings, params, connections, 0.0, 0.0, reporter);
        };

        let path = plain_origin
            .path_for(SMALL_BYTES)
            .ok_or_else(|| ModuleError::Workload("origin serves no small object".into()))?;
        let client = HttpClient::new(
            plain_origin.address(),
            path,
            SMALL_BYTES,
            Reuse::KeepAlive,
            None,
            (connections * LATENCY_CONNECTION_FACTOR).min(256),
        );
        let latency_connections = (connections * LATENCY_CONNECTION_FACTOR).min(256);
        let mut plan = LoadPlan {
            rate_per_s: capacity * LATENCY_LOAD_SHARE,
            duration: phase,
            workers: latency_connections,
            warmup: latency_connections as u64,
        };

        // Search downwards for a rate this generator can actually schedule. The
        // relationship to the phases below is the one `harness::calibrate_with`
        // has with the repetitions it sizes: find the parameter first, then
        // measure at it.
        //
        // The probe runs for a *full* phase rather than a fraction of one. A
        // short probe was cheaper and useless: it cleared at a rate the real
        // phases then saturated at, because thirty milliseconds is mostly
        // start-up transient and says little about what a schedule holds over
        // ten times as long. Paying four phases once per run is the price of
        // the answer being about the same thing.
        let probe = plan;
        for attempt in 0..LATENCY_RATE_ATTEMPTS {
            if reporter.is_cancelled() {
                return Err(ModuleError::Cancelled);
            }
            let outcome = loadgen::run(
                &client,
                &LoadPlan {
                    rate_per_s: plan.rate_per_s,
                    ..probe
                },
            );
            if !outcome.saturation.is_saturated() {
                break;
            }
            if attempt + 1 < LATENCY_RATE_ATTEMPTS {
                plan.rate_per_s /= 2.0;
            }
        }
        let offered_share = plan.rate_per_s / capacity;

        // Both latency metrics come from the same phases, so p99 is
        // accumulated alongside the mean rather than by running the load twice.
        let p99_samples: RefCell<Vec<f64>> = RefCell::new(Vec::new());
        let saturated: RefCell<Option<Warning>> = RefCell::new(None);
        let outcome = time_reps(
            params,
            reporter,
            "latency.small_mean",
            "ms",
            completed_units,
            total_units,
            |rep| {
                let started = Instant::now();
                let load = loadgen::run(&client, &plan);
                let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
                // Warm-ups are excluded from the side aggregations too.
                // `time_reps` withholds only its own return value, so anything
                // accumulated here has to apply the same rule or a warm-up
                // would silently enter the published statistics.
                let measured = rep >= params.warmup_reps;
                if measured {
                    if let Some(p99) = percentile(&load.response_ms, 99.0) {
                        p99_samples.borrow_mut().push(p99);
                    }
                    if let Some(warning) = load.warning(Some("latency.small_mean".into())) {
                        *saturated.borrow_mut() = Some(warning);
                    }
                }
                let mean = if load.response_ms.is_empty() {
                    0.0
                } else {
                    load.response_ms.iter().sum::<f64>() / load.response_ms.len() as f64
                };
                (mean, elapsed_ms)
            },
        )?;
        warnings.extend(outcome.warnings.clone());
        completed_units += 1.0;
        let _ = completed_units;

        push_latency(
            &mut metrics,
            &mut warnings,
            "latency.small_mean",
            "Mean response time under load",
            &outcome.measured,
            outcome.samples,
        );
        let p99 = p99_samples.into_inner();
        push_latency(
            &mut metrics,
            &mut warnings,
            "latency.small_p99",
            "99th percentile response time under load",
            &p99,
            vec![],
        );

        // The saturation warning is raised once for the phase rather than once
        // per repetition, and it goes to both channels: the reporter so the
        // console shows it live, and the output so the bundle carries it and
        // the module is degraded.
        if let Some(warning) = saturated.into_inner() {
            reporter.warn(warning.clone());
            warnings.push(warning);
        }

        finish(
            metrics,
            warnings,
            params,
            connections,
            plan.rate_per_s,
            offered_share,
            reporter,
        )
    }
}

/// Applies the variance sweep and builds the output.
///
/// Swept over the finished metric list rather than checked where each metric is
/// built, for the reason `network.transfer` records: the manifest promises that
/// any coefficient of variation above the bound raises a warning, and checking
/// that inside one of two construction paths made the promise true for some
/// metrics and false for others.
#[allow(clippy::too_many_arguments)]
fn finish(
    metrics: Vec<Metric>,
    mut warnings: Vec<Warning>,
    params: &ModuleParams,
    connections: usize,
    offered_rate: f64,
    offered_share: f64,
    reporter: &dyn ModuleReporter,
) -> Result<ModuleOutput, ModuleError> {
    const BOUND: f64 = 0.20;
    for metric in &metrics {
        if let Some(cv) = metric.summary.cv {
            if cv > BOUND {
                let warning = Warning {
                    code: WarningCode::HighVariance,
                    message: format!(
                        "`{}` varied by {:.0}% across repetitions, above this module's {:.0}% \
                         bound. On a loopback HTTP measurement that usually means the machine was \
                         doing something else at the same time.",
                        metric.key,
                        cv * 100.0,
                        BOUND * 100.0
                    ),
                    metric_key: Some(metric.key.clone()),
                };
                reporter.warn(warning.clone());
                warnings.push(warning);
            }
        }
    }

    let mut context = serde_json::Map::new();
    context.insert("workload_version".into(), VERSION.into());
    context.insert(
        "build_target".into(),
        format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS).into(),
    );
    context.insert("connections".into(), (connections as u64).into());
    context.insert(
        "object_sizes".into(),
        serde_json::json!([SMALL_BYTES, MEDIUM_BYTES, LARGE_BYTES]),
    );
    context.insert("protocol".into(), "HTTP/1.1".into());
    context.insert(
        "origin".into(),
        "in-process, 127.0.0.1, OS-assigned port, destroyed with the module".into(),
    );
    context.insert(
        "latency_load_share_target".into(),
        serde_json::json!(LATENCY_LOAD_SHARE),
    );
    context.insert(
        "latency_load_share".into(),
        serde_json::json!(offered_share),
    );
    context.insert(
        "latency_offered_rate_per_s".into(),
        serde_json::json!(offered_rate),
    );
    context.insert(
        "measured_reps".into(),
        serde_json::json!(params.measured_reps),
    );

    Ok(ModuleOutput {
        metrics,
        warnings,
        context,
    })
}

fn push_latency(
    metrics: &mut Vec<Metric>,
    warnings: &mut Vec<Warning>,
    key: &str,
    label: &str,
    samples: &[f64],
    raw: Vec<darcbench_protocol::metrics::MetricSample>,
) {
    let usable: Vec<f64> = samples.iter().copied().filter(|v| *v > 0.0).collect();
    match summarize(&usable) {
        Some(summary) if usable.len() >= 3 => metrics.push(Metric {
            key: key.into(),
            label: label.into(),
            value: summary.median,
            unit: "ms".into(),
            direction: Direction::LowerIsBetter,
            outliers: outlier_indices(&usable, 3.5),
            summary,
            samples: raw,
            measures_dispersion: false,
            tail_quantile: false,
        }),
        _ => warnings.push(Warning {
            code: WarningCode::ValidationFailed,
            message: format!(
                "`{key}` produced {} usable repetitions, below the three needed to report a \
                 value; it is withheld rather than published from noise.",
                usable.len()
            ),
            metric_key: Some(key.to_string()),
        }),
    }
}

fn median(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let n = sorted.len();
    Some(if n % 2 == 1 {
        sorted[n / 2]
    } else {
        (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0
    })
}

/// Nearest-rank percentile over an unsorted slice.
fn percentile(values: &[f64], percent: f64) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let rank = ((percent / 100.0) * sorted.len() as f64).ceil() as usize;
    sorted
        .get(rank.saturating_sub(1).min(sorted.len() - 1))
        .copied()
}

#[cfg(test)]
// In tests, `unwrap`/`expect` panicking *is* the failure signal.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic_in_result_fn)]
mod tests {
    use super::*;
    use crate::module::NullReporter;
    use darcbench_protocol::Profile;

    fn fast_params() -> ModuleParams {
        // The shortest phase the harness will accept, so the whole module runs
        // in a few seconds rather than the tens a real profile takes.
        let mut params = ModuleParams::for_profile(Profile::Quick);
        params.warmup_reps = 0;
        params.measured_reps = 3;
        params.target_rep_ms = 60;
        params.threads = 4;
        params
    }

    #[test]
    fn module_id_constant_satisfies_the_grammar() {
        assert!(ModuleId::new(MODULE_ID).is_ok());
        assert_eq!(WebStatic::new().manifest().id.as_str(), MODULE_ID);
    }

    #[test]
    fn manifest_is_well_formed() {
        let module = WebStatic::new();
        let manifest = module.manifest();
        assert_eq!(manifest.version, VERSION);
        assert_eq!(manifest.safety_class, SafetyClass::ProvisionsServices);
        assert_eq!(
            manifest.max_bytes_written, 0,
            "the module writes nothing to disk"
        );
        assert_eq!(
            manifest.max_network_bytes, 0,
            "loopback traffic costs the operator's link nothing"
        );
        assert!(!manifest.limitations.is_empty());
        assert!(!manifest.validation.is_empty());
        assert!(manifest.stability_cv_bound > 0.0);
        assert!(
            manifest.dependencies.is_empty(),
            "the origin is in-process; nothing external is required"
        );
        // The three limitations that are load-bearing rather than decorative.
        let text = manifest.limitations.join(" ");
        assert!(
            text.contains("floor"),
            "the shared-machine floor must be stated"
        );
        assert!(text.contains("HTTP/2"));
        assert!(text.contains("Compression"));
    }

    #[test]
    fn metric_keys_are_unique_and_shaped_like_the_others() {
        let mut keys: Vec<&str> = SHAPES.iter().map(|s| s.key).collect();
        keys.push("latency.small_mean");
        keys.push("latency.small_p99");
        let unique: std::collections::BTreeSet<&&str> = keys.iter().collect();
        assert_eq!(unique.len(), keys.len(), "duplicate metric key");
        for key in &keys {
            assert!(
                key.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '_'),
                "`{key}` is not in the reference alphabet"
            );
        }
    }

    #[test]
    fn a_full_run_produces_every_metric_and_discloses_its_origin() {
        let module = WebStatic::new();
        let params = fast_params();
        let output = module
            .run(&params, &NullReporter::default())
            .expect("the module must run against its own loopback origin");

        for shape in SHAPES {
            let metric = output
                .metric(shape.key)
                .unwrap_or_else(|| panic!("missing `{}`", shape.key));
            assert!(metric.value > 0.0, "{}: {}", shape.key, metric.value);
            assert_eq!(metric.unit, shape.unit);
            assert_eq!(metric.direction, Direction::HigherIsBetter);
        }
        for key in ["latency.small_mean", "latency.small_p99"] {
            let metric = output
                .metric(key)
                .unwrap_or_else(|| panic!("missing `{key}`"));
            assert!(metric.value > 0.0, "{key}: {}", metric.value);
            assert_eq!(metric.direction, Direction::LowerIsBetter);
        }

        // Keep-alive must beat reconnecting. If it does not, the connection
        // policy is not doing what the metric names claim, and the two numbers
        // are measuring the same thing under different labels.
        let keepalive = output.metric_value("requests.small_keepalive").unwrap();
        let reconnect = output.metric_value("connections.plaintext").unwrap();
        assert!(
            keepalive > reconnect,
            "reusing a connection must beat opening one per request: {keepalive} vs {reconnect}"
        );
        // The TLS shape is measured but not *ordered* against the plaintext one
        // here. It should be slower, and on a quiet machine it is - but both
        // shapes are dominated by connection setup and teardown on loopback,
        // and a modern handshake is small enough next to that for the two to
        // land within noise of each other on a busy four-core CI host. An
        // assertion that fails on a loaded machine would be a worse test than
        // no assertion: it would train everyone to re-run it. The relationship
        // *is* asserted where it belongs, on the reference anchors, which are
        // declared rather than measured.
        assert!(output.metric_value("connections.tls").unwrap() > 0.0);

        assert_eq!(output.context["protocol"], "HTTP/1.1");
        assert!(output.context["origin"]
            .as_str()
            .unwrap()
            .contains("127.0.0.1"));
        assert!(output.context["connections"].as_u64().unwrap() >= 2);
    }

    #[test]
    fn cancellation_is_honoured_before_any_load_is_offered() {
        let module = WebStatic::new();
        let reporter = NullReporter::default();
        reporter.cancel();
        assert!(matches!(
            module.run(&fast_params(), &reporter),
            Err(ModuleError::Cancelled)
        ));
    }

    #[test]
    fn the_duration_estimate_is_not_optimistic() {
        let module = WebStatic::new();
        let params = ModuleParams::for_profile(Profile::Standard);
        let estimate = module.estimated_duration_s(&params);
        let phases =
            (SHAPES.len() + 1) as u64 * u64::from(params.warmup_reps + params.measured_reps);
        let floor = phases * params.target_rep_ms / 1000;
        assert!(
            estimate >= floor,
            "estimate {estimate}s is below the {floor}s the phases alone take"
        );
    }

    #[test]
    fn percentile_and_median_agree_with_their_definitions() {
        let values: Vec<f64> = (1..=100).map(|v| v as f64).collect();
        assert_eq!(percentile(&values, 99.0), Some(99.0));
        assert_eq!(percentile(&values, 100.0), Some(100.0));
        assert_eq!(percentile(&[], 50.0), None);
        assert_eq!(median(&[3.0, 1.0, 2.0]), Some(2.0));
        assert_eq!(median(&[4.0, 1.0, 3.0, 2.0]), Some(2.5));
        assert_eq!(median(&[]), None);
    }
}
