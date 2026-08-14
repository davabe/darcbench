//! The loopback HTTP origin that `web.static` loads.
//!
//! # Why DARCBench ships a web server at all
//!
//! `docs/THREAT-MODEL.md` (T-AMPLIFY) is permanent and binding: *"A benchmark
//! suite that lets you point a load generator at an arbitrary URL is a DDoS
//! tool with a scoring model. […] HTTP load generation targets **only** a
//! server the agent started on loopback. There will be no 'benchmark this URL'
//! feature."*
//!
//! That constraint has to be satisfied structurally, not by validation, so this
//! module *is* the target. Nothing here accepts a host, a URL or a path from a
//! caller: the origin binds a port the OS assigns, serves a fixed set of
//! generated objects, and is destroyed when the run that created it ends. There
//! is no configuration surface through which it could be pointed anywhere,
//! because it does not point - it listens for the length of one run. That
//! remains true of [`Bind::External`] below: the only thing an operator can
//! choose is which of *their own* interfaces this listener answers on.
//!
//! ADR-0012 records the second half of the argument. Serving the objects from
//! inside the agent means `web.static` measures *the machine's* HTTP
//! capability - syscalls, loopback, TLS, TCP - under a server that is identical
//! on every host, rather than measuring the operator's nginx configuration. It
//! also means the generator and the target share the machine, so every module
//! built on this origin must disclose that its number is a floor, not an
//! estimate.
//!
//! # Listening beyond loopback, and what makes that acceptable
//!
//! [`Bind::External`] lets the origin answer on one of the host's own network
//! interfaces, so the load can come from a second machine. That mode exists
//! because a local injector and the origin compete for the same cores: on
//! loopback, serving a 1 KiB object costs microseconds, the generator's own
//! per-request work is comparable to it, and `web.static` had to drop its
//! latency phase to a quarter of measured capacity because one machine cannot
//! be asked for 170% of itself.
//!
//! It is not a relaxation of T-AMPLIFY, which is about what this software can
//! be pointed *at*. Nothing in this module gained the ability to send a
//! request anywhere. What it gained is the ability to *receive* them from
//! somewhere other than this host, and that is gated three ways:
//!
//! * **It is never the default.** [`Bind::Loopback`] is what every existing
//!   module uses and what [`Default`] produces; external listening requires an
//!   explicit address from an operator.
//! * **Every request must carry the session token**
//!   ([`darcbench_protocol::external::SessionToken`]), a 256-bit secret the
//!   target prints once for a human to carry to the generator machine. A
//!   request without it is answered `401` and the connection is closed; it
//!   never reaches the body table.
//! * **Unauthorised requests are counted separately** ([`Origin::refused`])
//!   and never counted as served, so the reconciliation that makes an external
//!   result trustworthy is not corrupted by whoever else finds the port, and
//!   the bundle can still disclose that someone did.
//!
//! The listener is short-lived, on a port the OS assigns, on an address the
//! operator named, protected by a secret they generated. That is the whole of
//! the exposure, and it is stated here rather than left to be inferred.
//!
//! # What it is, deliberately
//!
//! A minimal HTTP/1.1 server: request line, headers, `200` or `404`, keep-alive
//! by default, optional TLS. It exists to be *cheap and uniform*, so that what
//! the load generator measures is the machine and not this code. Every design
//! choice below follows from that:
//!
//! * **Bodies are generated once, at [`Origin::start`], and served from
//!   memory.** Generating per request would put a PRNG in the measured path and
//!   `web.static` would be reporting SplitMix64 throughput. They come from the
//!   same fixed-seed generator as every other DARCBench corpus
//!   ([`crate::workloads::SplitMix64`]), so a 65,536-byte object is the same
//!   65,536 bytes on every machine and in every release of workload version 1 -
//!   and, being pseudo-random, is not something a compressing proxy or a
//!   filesystem could quietly turn into a much smaller transfer.
//! * **One allocation, not one per size.** Every object is a prefix of a single
//!   master buffer, so configuring `[1 KiB, 64 KiB, 1 MiB]` costs 1 MiB and not
//!   1.06 MiB, and the small objects are guaranteed to be prefixes of the large
//!   one rather than merely deterministic.
//! * **No `Date` header.** HTTP/1.1 says a server SHOULD send one; it costs a
//!   clock read on every response, nothing in this system reads it, and the
//!   only client this origin will ever have is the generator two threads away.
//!   Recorded here rather than left as an oversight.
//! * **A thread per connection.** The simplest thing that is correct. It is
//!   also honest about the cost of a connection, which is part of what
//!   `web.static` is measuring, and it is bounded - see
//!   [`MAX_LIVE_CONNECTIONS`].
//!
//! # This is a server, so it is written like one
//!
//! Even on loopback, an unbounded read from a socket is not acceptable. The
//! request line and header block together may not exceed [`MAX_HEAD_BYTES`];
//! past that the connection is refused and closed rather than buffered. Reads
//! are chunked, so the buffer can overshoot that bound by at most one
//! [`READ_CHUNK`] and never more. A half-sent request cannot hold a connection
//! slot indefinitely ([`HEAD_DEADLINE`]), an idle keep-alive connection cannot
//! hold one forever ([`IDLE_DEADLINE`]), and a client that stops reading cannot
//! pin a writer forever ([`WRITE_TIMEOUT`]).
//!
//! Nothing from a request is ever used to build a filesystem path. There is no
//! filesystem in this module at all: a request target either names a
//! byte count that was configured before the listener existed, or it is a 404.
//!
//! # Shutdown, and why it is done this way
//!
//! `std::net::TcpListener::accept` blocks with no portable timeout, which is
//! the whole difficulty of [`Drop`]. Two remedies exist:
//!
//! 1. Put the listener in non-blocking mode and poll. Simple, but it adds up to
//!    the poll interval of latency to every accepted connection - and accept
//!    latency is part of what an HTTP benchmark is measuring, so this would
//!    corrupt the measurement to simplify the teardown.
//! 2. Set a shutdown flag and open one connection to our own address to wake
//!    the accept loop. Costs nothing while running.
//!
//! This module uses (2). The accept thread checks the flag immediately after
//! every `accept`, so the wake-up connection is never served; it is closed and
//! the loop exits. Connection threads are owned by the accept thread, which
//! joins them all before returning, so joining the single accept handle in
//! `Drop` joins everything.
//!
//! Connection threads are woken by a read timeout of [`IDLE_POLL`] rather than
//! by anything cleverer: they wake, see the flag, and close. That puts a
//! `IDLE_POLL`-sized floor under teardown latency, which is the price of not
//! keeping a registry of live sockets to shut down individually. A connection
//! blocked *writing* to a client that has stopped reading can delay teardown by
//! up to [`WRITE_TIMEOUT`]; that is the one unbounded-in-practice case, and it
//! is bounded by the socket timeout rather than left to chance.
//!
//! # What this origin deliberately does not do
//!
//! **HTTP/2 and HTTP/3.** Out of reach without adding `h2` and a QUIC stack.
//! ADR-0012 declares them unmeasured rather than approximated, in the same
//! terms as packet loss in `network.transfer`.
//!
//! **Request bodies.** Nothing this origin measures needs one, and accepting a
//! body means either buffering it (an unbounded read wearing a polite name) or
//! streaming it to nowhere. A request that carries `Content-Length` or
//! `Transfer-Encoding` is refused with `400` and the connection is closed,
//! because the alternative - ignoring the header - would leave the body bytes
//! in the stream to be misparsed as the next request.
//!
//! **Compression, ranges, conditional requests, virtual hosts.** Every one of
//! them is a feature of somebody's *web server configuration*, which ADR-0012
//! is explicit about not measuring.
//!
//! **Serving anything from disk.** Bodies are generated. There is no document
//! root to traverse, which is the only completely reliable defence against
//! traversal.

use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use darcbench_protocol::external::SessionToken;
use rustls_pki_types::CertificateDer;

use crate::workloads::{SplitMix64, CORPUS_SEED};

/// Prefix of every object path. `path_for(65536)` is `/o/65536`.
///
/// Short on purpose: at a hundred thousand requests a second the request line
/// is a measurable fraction of the bytes on the wire, and a benchmark should
/// not spend them on a decorative URL.
const OBJECT_PREFIX: &str = "/o/";

/// Largest request line plus header block this origin will read.
///
/// 8 KiB is what nginx and Apache default to, so a request this origin refuses
/// is a request a real server would also refuse. The number matters less than
/// the fact that there is one: without it, a single client could make the
/// origin buffer until the machine ran out of memory, on a benchmark tool whose
/// entire promise is that it does not damage the host it runs on.
const MAX_HEAD_BYTES: usize = 8 * 1024;

/// Bytes requested per socket read.
///
/// Also the exact amount by which the head buffer may overshoot
/// [`MAX_HEAD_BYTES`] before the bound is noticed, which is why it is small.
const READ_CHUNK: usize = 1024;

/// Connections served concurrently before new ones are closed on accept.
///
/// This is a *self-defence* limit, not a tuning knob: the load generator on the
/// other end of these sockets is code in this same binary, and a bug there that
/// opened connections in a loop must not be able to make the origin spawn
/// threads until the host falls over. 512 is far above any concurrency
/// `web.static` will configure and far below anything that threatens a machine
/// able to run a benchmark at all. Over the limit the connection is closed
/// immediately rather than queued, so the generator observes the refusal as a
/// connection error and reports it, instead of seeing latency that came from
/// this module rather than from the machine.
pub const MAX_LIVE_CONNECTIONS: usize = 512;

/// Largest object the origin will materialise.
///
/// The bodies live in memory for the life of the run, so an object size is a
/// resident-memory commitment. Refusing at `start()` turns a typo in a module's
/// size table into an error before anything is allocated, rather than into an
/// OOM on the operator's production server.
pub const MAX_OBJECT_BYTES: usize = 64 << 20;

/// Read timeout on a connection socket, and therefore how often a connection
/// thread notices that the origin is shutting down.
const IDLE_POLL: Duration = Duration::from_millis(200);

/// How long a *partially* received request head may take to complete.
///
/// Distinct from [`IDLE_DEADLINE`] because the two are different situations: a
/// keep-alive connection sitting idle between requests is behaving correctly,
/// and one that has sent half a request line and stopped is not.
const HEAD_DEADLINE: Duration = Duration::from_secs(10);

/// How long an idle keep-alive connection may hold a slot with no request.
const IDLE_DEADLINE: Duration = Duration::from_secs(60);

/// Write timeout on a connection socket.
///
/// A client that opens a connection, asks for a 1 MiB object and then stops
/// reading would otherwise block a connection thread until the peer went away,
/// which is also the one way teardown could take longer than [`IDLE_POLL`].
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// Timeout on the self-connection that wakes the accept loop during teardown.
const WAKE_TIMEOUT: Duration = Duration::from_secs(1);

/// Seed for the object corpus.
///
/// Derived from [`CORPUS_SEED`] the same way every other DARCBench corpus is,
/// so it inherits the rule that changing it invalidates comparability and may
/// only happen with a major workload version bump.
const ORIGIN_SEED: u64 = CORPUS_SEED ^ 0x5EB0;

const STATUS_OK: &str = "200 OK";
const STATUS_BAD_REQUEST: &str = "400 Bad Request";
const STATUS_NOT_FOUND: &str = "404 Not Found";
const STATUS_METHOD_NOT_ALLOWED: &str = "405 Method Not Allowed";
const STATUS_HEAD_TOO_LARGE: &str = "431 Request Header Fields Too Large";
const STATUS_UNAUTHORIZED: &str = "401 Unauthorized";

/// The header carrying the session token on a token-gated origin.
///
/// Not `Authorization`. That header has a scheme grammar, a long history of
/// proxies and libraries handling it specially, and a habit of appearing in
/// access logs; a bespoke name has none of those and cannot be confused for a
/// credential to anything else.
const TOKEN_HEADER: &str = "x-darcbench-session";

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Which interface the origin answers on.
///
/// Not a `SocketAddr`, and not a string. The port is never a caller's choice -
/// see [`Origin::start`] - so a type that could carry one would be a type
/// someone eventually sets. What is left is a binary decision plus, in one
/// case, an address, which is exactly the decision an operator gets to make.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Bind {
    /// `127.0.0.1`. The default, and what every in-process module uses.
    #[default]
    Loopback,
    /// One of this host's own interfaces, so a second machine can generate the
    /// load. Requires a token; see [`OriginConfig::token`].
    ///
    /// A specific address, never `0.0.0.0`. Two reasons, and both are checked
    /// in [`Origin::start`] rather than documented and hoped for: listening on
    /// every interface exposes the origin on networks the operator was not
    /// thinking about, and the TLS certificate needs an IP SAN, which a
    /// wildcard bind cannot supply. A certificate for `0.0.0.0` would parse,
    /// look right, and fail verification at the far end.
    External(IpAddr),
}

/// What the origin will serve for the life of one run.
#[derive(Debug, Clone, Default)]
pub struct OriginConfig {
    /// Body sizes in bytes the origin will serve, e.g. `[1024, 65536, 1048576]`.
    ///
    /// Duplicates and ordering are irrelevant; the origin sorts and deduplicates
    /// them. Every size costs resident memory until the [`Origin`] is dropped,
    /// bounded by [`MAX_OBJECT_BYTES`] on the largest.
    pub object_sizes: Vec<usize>,
    /// When true the origin terminates TLS with a per-run self-signed cert.
    pub tls: bool,
    /// Which interface to answer on. Defaults to [`Bind::Loopback`].
    pub bind: Bind,
    /// The secret every request must carry, when there is one.
    ///
    /// `None` on loopback: the only client is this process, two threads away,
    /// and a token would protect nothing while adding a comparison to the
    /// measured path. Required by [`Bind::External`], and
    /// [`Origin::start`] refuses the combination of an external bind and no
    /// token rather than starting an open listener on somebody's network.
    pub token: Option<SessionToken>,
}

/// A running HTTP origin. Dropping it shuts the origin down.
#[derive(Debug)]
pub struct Origin {
    address: SocketAddr,
    shared: Arc<Shared>,
    /// `None` only after [`Drop`] has taken it.
    accept: Option<JoinHandle<()>>,
    certificate: Option<CertificateDer<'static>>,
}

impl Origin {
    /// Binds port zero on the configured interface, starts the accept loop on
    /// background threads, and returns once the origin is ready to serve.
    ///
    /// "Ready" is not a promise this function has to work for: the listener is
    /// bound and listening on the *calling* thread before the accept thread is
    /// spawned, so a connection made the instant this returns is already
    /// sitting in the kernel's backlog. Binding here rather than in the thread
    /// is also what lets a bind failure be returned instead of logged.
    ///
    /// The port is always `0` - the OS assigns a free one. There is no
    /// configurable port, both because a benchmark that races whatever the
    /// operator already has listening is a benchmark that damages their machine
    /// (`docs/INSTALLER-AND-DISCOVERY.md`), and because a fixed port would be a
    /// value someone could eventually be tempted to make configurable.
    pub fn start(config: OriginConfig) -> Result<Self, OriginError> {
        // Before anything is allocated or generated, because the whole point
        // of this check is that the listener never comes into existence.
        let listen_ip = match config.bind {
            Bind::Loopback => IpAddr::V4(Ipv4Addr::LOCALHOST),
            Bind::External(ip) => {
                if config.token.is_none() {
                    return Err(OriginError::ExternalWithoutToken);
                }
                if is_wildcard(ip) {
                    return Err(OriginError::WildcardBind);
                }
                // A loopback address here is deliberately *not* an error. It
                // is a listener on loopback that demands a token, which is
                // what the caller asked for and is strictly more restrictive
                // than `Bind::Loopback`. Allowing it is what lets a test - and
                // an operator rehearsing the two-machine flow on one machine -
                // exercise the authenticated path without a second interface.
                ip
            }
        };

        let mut sizes = config.object_sizes;
        sizes.sort_unstable();
        sizes.dedup();
        let largest = sizes.last().copied().unwrap_or(0);
        if largest > MAX_OBJECT_BYTES {
            return Err(OriginError::ObjectTooLarge {
                requested: largest,
                limit: MAX_OBJECT_BYTES,
            });
        }

        // Generated before the listener exists, so that no request can ever
        // arrive while a body is half-built and so that the cost of building
        // them is outside anything the module times.
        let bodies = Bodies {
            master: generate_master(largest),
            sizes,
        };

        let (tls, certificate) = if config.tls {
            let (config, certificate) = server_tls(listen_ip)?;
            (Some(config), Some(certificate))
        } else {
            (None, None)
        };

        let listener =
            TcpListener::bind(SocketAddr::new(listen_ip, 0)).map_err(OriginError::Bind)?;
        let address = listener.local_addr().map_err(OriginError::Bind)?;

        let shared = Arc::new(Shared {
            shutdown: AtomicBool::new(false),
            served: AtomicU64::new(0),
            refused: AtomicU64::new(0),
            unmatched: AtomicU64::new(0),
            bodies,
            tls,
            token: config.token,
        });

        let accept_shared = Arc::clone(&shared);
        let accept = std::thread::Builder::new()
            .name("darcbench-origin".to_string())
            .spawn(move || accept_loop(listener, accept_shared))
            .map_err(OriginError::Spawn)?;

        Ok(Self {
            address,
            shared,
            accept: Some(accept),
            certificate,
        })
    }

    /// The bound address, with the OS-assigned port.
    pub fn address(&self) -> SocketAddr {
        self.address
    }

    /// Path serving a body of exactly `bytes`, e.g. `/o/65536`. `None` for a
    /// size that was not configured.
    ///
    /// Callers should route through this rather than formatting the path
    /// themselves: it is the one place that knows a size is actually being
    /// served, so a module that asks for a size it never configured gets
    /// `None` at request-building time instead of a run full of 404s that
    /// still produce plausible-looking throughput numbers.
    pub fn path_for(&self, bytes: usize) -> Option<String> {
        self.shared
            .bodies
            .sizes
            .binary_search(&bytes)
            .ok()
            .map(|_| format!("{OBJECT_PREFIX}{bytes}"))
    }

    /// DER of the self-signed certificate, for the client to trust. `None` when
    /// `tls` is false.
    ///
    /// This is the *whole* trust story: the certificate is generated at
    /// `start()`, lives in memory, is handed to exactly one client - this
    /// agent's own load generator, in this process - and is gone when the run
    /// ends. It is never written to disk and never goes near the host trust
    /// store, so a DARCBench run cannot leave the machine trusting anything it
    /// did not trust before.
    pub fn certificate_der(&self) -> Option<CertificateDer<'static>> {
        self.certificate.clone()
    }

    /// The TLS configuration this origin terminates with, for a second
    /// listener that must present the same certificate.
    ///
    /// Exists for the session control channel, which is a different port on
    /// the same interface for the same session. Handing it the *same*
    /// configuration rather than generating a second certificate means a
    /// generator that has pinned the origin's certificate can reach the
    /// control channel with the pin it already has, and an operator who asked
    /// for an encrypted session gets one on both channels rather than on the
    /// one they were thinking about.
    pub fn tls_config(&self) -> Option<Arc<rustls::ServerConfig>> {
        self.shared.tls.clone()
    }

    /// Requests answered with an object since start.
    ///
    /// Counts a request only when the origin actually served a configured
    /// body. A 404, a 405, an unauthorised request or a malformed head is not
    /// here - see [`Self::unmatched`] and [`Self::refused`].
    ///
    /// That exclusion is the point rather than a detail. A caller comparing
    /// this against the number of requests it issued is asserting that the
    /// origin really did the work, and a 404 is 46 bytes and no work: counting
    /// it here would let a generator request a size this origin does not serve,
    /// report the results as 1 MiB transfers, and reconcile perfectly against
    /// a machine that did essentially nothing.
    ///
    /// The counter is incremented *before* the response is written, which is
    /// the only ordering that makes it usable: a client holding response `N` is
    /// then guaranteed to observe at least `N` here. Incrementing afterwards
    /// reads better and turns every such assertion into a race the caller
    /// loses whenever the connection thread is descheduled between the write
    /// and the increment. The cost is that a response whose write fails
    /// mid-body is still counted, which is the smaller inaccuracy: it
    /// overcounts a connection that broke rather than undercounting one that
    /// worked.
    pub fn served(&self) -> u64 {
        self.shared.served.load(Ordering::Acquire)
    }

    /// Requests rejected because they did not carry the session token.
    ///
    /// Always zero on a tokenless origin. Kept apart from [`Self::served`] for
    /// a specific reason: `served` is what the generator's claims are
    /// reconciled against, and folding a port scanner's probes into it would
    /// turn any passing stranger into an invalidated benchmark run. A refused
    /// request costs a parse and a 46-byte response; it did not compete with
    /// the measurement for anything worth counting.
    ///
    /// It is still worth reporting. A non-zero count means the origin's port
    /// was found by something that was not the generator, which an operator
    /// reading the bundle should know even though it did not change the
    /// numbers.
    pub fn refused(&self) -> u64 {
        self.shared.refused.load(Ordering::Acquire)
    }

    /// Authorised requests the origin had no object for: 404 and 405.
    ///
    /// Should be zero on any run driven by a DARCBench generator, which asks
    /// only for sizes the session offered. A non-zero count means the client
    /// and the origin disagree about what is being served, which is worth
    /// seeing rather than folding into a throughput figure.
    pub fn unmatched(&self) -> u64 {
        self.shared.unmatched.load(Ordering::Acquire)
    }
}

impl Drop for Origin {
    fn drop(&mut self) {
        self.shared.shutdown.store(true, Ordering::SeqCst);

        // Wake the blocking `accept`. The flag is already visible, so this
        // connection is closed rather than served. Retried a few times because
        // the whole teardown hangs if the wake-up is lost, and cheap because a
        // connection to a closed loopback port is refused immediately rather
        // than timing out - so the retries cost nothing in the case where the
        // accept loop has already exited on its own.
        for _ in 0..3 {
            match TcpStream::connect_timeout(&self.address, WAKE_TIMEOUT) {
                Ok(stream) => {
                    let _ = stream.shutdown(Shutdown::Both);
                    break;
                }
                Err(_) => continue,
            }
        }

        if let Some(accept) = self.accept.take() {
            // The accept thread joins every connection thread before it
            // returns, so this one join is the whole shutdown.
            let _ = accept.join();
        }
    }
}

/// Why an origin could not be started.
#[derive(Debug, thiserror::Error)]
pub enum OriginError {
    #[error("could not bind the origin: {0}")]
    Bind(#[source] std::io::Error),
    #[error(
        "an origin listening beyond loopback must have a session token; refusing to start an \
         unauthenticated listener on a network interface"
    )]
    ExternalWithoutToken,
    #[error(
        "an external origin must name one interface rather than binding all of them: a wildcard \
         address exposes the listener on networks the operator was not thinking about, and the \
         TLS certificate needs an IP SAN a wildcard cannot supply"
    )]
    WildcardBind,
    #[error("could not start the origin's accept loop: {0}")]
    Spawn(#[source] std::io::Error),
    #[error("could not build the origin's TLS configuration: {0}")]
    Tls(String),
    #[error(
        "object size {requested} exceeds the origin's {limit}-byte ceiling; \
         bodies are held in memory for the life of the run"
    )]
    ObjectTooLarge { requested: usize, limit: usize },
}

// ---------------------------------------------------------------------------
// Bodies
// ---------------------------------------------------------------------------

/// The objects, as one buffer plus the set of lengths that may be taken from
/// its front.
#[derive(Debug)]
struct Bodies {
    /// Pseudo-random bytes, as long as the largest configured object.
    master: Vec<u8>,
    /// Configured sizes, sorted and deduplicated so lookup is a binary search
    /// and `path_for` and the request parser agree by construction.
    sizes: Vec<usize>,
}

impl Bodies {
    fn body(&self, bytes: usize) -> Option<&[u8]> {
        self.sizes
            .binary_search(&bytes)
            .ok()
            .and_then(|_| self.master.get(..bytes))
    }
}

/// Builds the master buffer.
///
/// Pseudo-random rather than a repeated pattern, so that nothing between the
/// origin and the generator - a compressing intermediary, a filesystem, a
/// hypervisor's page deduplication - can turn a 1 MiB transfer into a much
/// smaller one and flatter the machine. Deterministic rather than random, so
/// the same DARCBench version serves the same bytes everywhere.
fn generate_master(bytes: usize) -> Vec<u8> {
    let mut rng = SplitMix64::new(ORIGIN_SEED);
    let mut buffer = vec![0u8; bytes];
    for chunk in buffer.chunks_mut(8) {
        let word = rng.next_u64().to_le_bytes();
        chunk.copy_from_slice(&word[..chunk.len()]);
    }
    buffer
}

// ---------------------------------------------------------------------------
// TLS
// ---------------------------------------------------------------------------

/// Generates the per-run certificate and the `rustls` server configuration.
fn server_tls(
    listen_ip: IpAddr,
) -> Result<(Arc<rustls::ServerConfig>, CertificateDer<'static>), OriginError> {
    // `CertificateParams::new` turns a string that parses as an IP address into
    // an **IP SAN**, which is what a client connecting to an address rather
    // than a name needs. A DNS SAN of "127.0.0.1" would parse, look right, and
    // fail verification, because a client presenting an IP server name never
    // matches a DNS name.
    //
    // The SAN is the address actually bound, not a constant. An external
    // origin whose certificate named loopback would hand every generator a
    // certificate that cannot verify - and the failure would arrive as a TLS
    // error in the middle of a benchmark rather than at start-up.
    let mut params = rcgen::CertificateParams::new(vec![listen_ip.to_string()])
        .map_err(|error| OriginError::Tls(error.to_string()))?;
    let mut name = rcgen::DistinguishedName::new();
    name.push(rcgen::DnType::CommonName, "darcbench benchmark origin");
    params.distinguished_name = name;

    let key = rcgen::KeyPair::generate().map_err(|error| OriginError::Tls(error.to_string()))?;
    let certificate = params
        .self_signed(&key)
        .map_err(|error| OriginError::Tls(error.to_string()))?;
    let certificate_der = certificate.der().clone();
    let key_der = rustls_pki_types::PrivateKeyDer::Pkcs8(
        rustls_pki_types::PrivatePkcs8KeyDer::from(key.serialize_der()),
    );

    // The provider is named rather than taken from the compile-time default.
    // ADR-0011 chose `ring` over `aws-lc-rs` because this workspace has no C
    // toolchain requirement, and the default is decided by which rustls
    // features happen to be unified across whatever crates are in one cargo
    // invocation - a condition that can change without anyone touching this
    // file. Naming it makes the ADR's decision hold locally.
    let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|error| OriginError::Tls(error.to_string()))?
    .with_no_client_auth()
    .with_single_cert(vec![certificate_der.clone()], key_der)
    .map_err(|error| OriginError::Tls(error.to_string()))?;

    // Without this, a client that offers ALPN gets no protocol back and has to
    // guess; with it, the one protocol this origin speaks is stated. Session
    // resumption is left at rustls' default because whether a handshake resumes
    // is the *client's* choice, and the client here is the load generator,
    // which is the component that should decide what it is measuring.
    config.alpn_protocols = vec![b"http/1.1".to_vec()];

    Ok((Arc::new(config), certificate_der))
}

// ---------------------------------------------------------------------------
// Accept loop
// ---------------------------------------------------------------------------

/// State shared between the accept thread and every connection thread.
#[derive(Debug)]
struct Shared {
    shutdown: AtomicBool,
    served: AtomicU64,
    refused: AtomicU64,
    unmatched: AtomicU64,
    bodies: Bodies,
    tls: Option<Arc<rustls::ServerConfig>>,
    /// `None` means every request is served. See [`OriginConfig::token`].
    token: Option<SessionToken>,
}

impl Shared {
    /// The status for a head this origin could not parse, and the counter
    /// increment that goes with it.
    ///
    /// On a tokenless origin the diagnostic status is kept: the only client is
    /// this process, `400` and `431` say which bound was hit, and there is no
    /// adversary to keep them from. On a gated one they collapse to `401`,
    /// because an unparseable head is an unauthenticated peer by construction
    /// and it should learn nothing this origin does not have to tell it.
    fn refuse_status(&self, diagnostic: &'static str) -> &'static str {
        if self.token.is_none() {
            return diagnostic;
        }
        self.refused.fetch_add(1, Ordering::Release);
        STATUS_UNAUTHORIZED
    }
}

fn accept_loop(listener: TcpListener, shared: Arc<Shared>) {
    // The accept thread owns its children. Nothing else has to know how many
    // connection threads exist or how to wait for them, which is what makes
    // `Origin::drop` a single join.
    let mut connections: Vec<JoinHandle<()>> = Vec::new();

    loop {
        match listener.accept() {
            Ok((stream, _peer)) => {
                // Checked before anything is spawned, so the teardown wake-up
                // connection is never served.
                if shared.shutdown.load(Ordering::SeqCst) {
                    let _ = stream.shutdown(Shutdown::Both);
                    break;
                }
                reap(&mut connections);
                if connections.len() >= MAX_LIVE_CONNECTIONS {
                    let _ = stream.shutdown(Shutdown::Both);
                    continue;
                }
                let connection_shared = Arc::clone(&shared);
                match std::thread::Builder::new()
                    .name("darcbench-origin-conn".to_string())
                    .spawn(move || connection(stream, &connection_shared))
                {
                    Ok(handle) => connections.push(handle),
                    // The stream is dropped with the closure, so the peer sees
                    // a closed connection rather than a hang. Out of threads is
                    // the machine's problem to report, not this module's to
                    // retry into.
                    Err(_) => continue,
                }
            }
            Err(_) => {
                if shared.shutdown.load(Ordering::SeqCst) {
                    break;
                }
                // A transient accept error (EMFILE, ECONNABORTED) must not turn
                // into a hot loop that competes with the benchmark for CPU.
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }

    for handle in connections {
        let _ = handle.join();
    }
}

/// Joins the connection threads that have already finished.
///
/// `Vec::retain` would *drop* the handles, which detaches the threads instead
/// of joining them - harmless, since they have exited, but it makes "no leaked
/// threads" a claim about timing rather than a fact.
fn reap(connections: &mut Vec<JoinHandle<()>>) {
    let mut index = 0;
    while index < connections.len() {
        if connections[index].is_finished() {
            let _ = connections.swap_remove(index).join();
        } else {
            index += 1;
        }
    }
}

// ---------------------------------------------------------------------------
// Connection
// ---------------------------------------------------------------------------

fn connection(stream: TcpStream, shared: &Shared) {
    // The read timeout is what lets this thread notice shutdown; without it a
    // keep-alive connection sitting idle would block teardown until its peer
    // happened to do something. If it cannot be set, this connection could
    // outlive the origin, so it is closed rather than served.
    if stream.set_read_timeout(Some(IDLE_POLL)).is_err() {
        return;
    }
    let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
    // Nagle would hold a small response back waiting for more data to coalesce
    // with, adding up to 40 ms to a latency figure that is meant to describe
    // the machine.
    let _ = stream.set_nodelay(true);

    match &shared.tls {
        Some(config) => {
            let Ok(session) = rustls::ServerConnection::new(Arc::clone(config)) else {
                return;
            };
            let mut tls = rustls::StreamOwned::new(session, stream);
            let _ = serve(&mut tls, shared);
            // A `close_notify` distinguishes an orderly end from a truncation
            // attack, and a client that checks for one - as it should - would
            // otherwise report every completed connection as an error.
            tls.conn.send_close_notify();
            let _ = tls.flush();
        }
        None => {
            let mut stream = stream;
            let _ = serve(&mut stream, shared);
        }
    }
}

/// One request, as understood.
#[derive(Debug)]
struct Request {
    /// False for any method other than `GET`.
    get: bool,
    /// The object size named by the target, if the target named one at all.
    object: Option<usize>,
    /// Whether this response must be the last on the connection.
    close: bool,
    /// The value of [`TOKEN_HEADER`], parsed, if the request carried a
    /// well-formed one.
    ///
    /// Parsed here rather than compared here: `parse_head` has no access to
    /// the session and knowing whether a token is *correct* is not its job.
    /// A malformed value becomes `None` and is refused exactly like an absent
    /// one, so a client cannot learn anything from the difference.
    token: Option<SessionToken>,
}

/// What reading a request head produced.
#[derive(Debug)]
enum Head {
    Request(Request),
    /// Syntactically unusable, or carrying a body this origin will not read.
    Malformed,
    /// Larger than [`MAX_HEAD_BYTES`].
    Oversized,
    /// The peer closed, a deadline expired, or the origin is shutting down.
    /// In every case there is nobody left to answer.
    Finished,
}

/// The request loop. Runs until the connection ends for any reason.
fn serve<S: Read + Write>(stream: &mut S, shared: &Shared) -> std::io::Result<()> {
    // Reused across requests: a keep-alive connection that served ten thousand
    // requests should not have allocated ten thousand buffers.
    let mut buffer: Vec<u8> = Vec::with_capacity(READ_CHUNK);

    loop {
        match read_head(stream, &mut buffer, shared)? {
            Head::Finished => return Ok(()),
            // Both of these are answered and then closed. Answering is a
            // courtesy; closing is the point, because after a head we could not
            // parse we no longer know where the next request begins.
            //
            // On a gated origin both become a `401` and both are counted in
            // `refused`, which is not cosmetic. A head this origin could not
            // parse cannot have carried a valid token - so the peer is
            // unauthenticated by construction, and answering it `400` or `431`
            // told a scanner it had found an HTTP server while leaving no
            // trace in the counter the bundle discloses. Most scanner traffic
            // is exactly this shape: a TLS ClientHello at a plaintext port, a
            // chunked request, a bare probe.
            Head::Oversized => {
                let _ = write_response(
                    stream,
                    shared.refuse_status(STATUS_HEAD_TOO_LARGE),
                    &[],
                    true,
                );
                return Ok(());
            }
            Head::Malformed => {
                let _ = write_response(stream, shared.refuse_status(STATUS_BAD_REQUEST), &[], true);
                return Ok(());
            }
            Head::Request(request) => {
                // Before the method, before the target, before the body table.
                // An unauthorised request must not be able to learn which
                // object sizes exist, or even whether this is an origin that
                // would have served it, from the status code it gets back.
                if !authorised(&request, shared) {
                    shared.refused.fetch_add(1, Ordering::Release);
                    let _ = write_response(stream, STATUS_UNAUTHORIZED, &[], true);
                    return Ok(());
                }
                let (status, body) = if !request.get {
                    (STATUS_METHOD_NOT_ALLOWED, &[][..])
                } else {
                    match request.object.and_then(|bytes| shared.bodies.body(bytes)) {
                        Some(body) => (STATUS_OK, body),
                        None => (STATUS_NOT_FOUND, &[][..]),
                    }
                };
                // Before the write, not after. See [`Origin::served`].
                //
                // A response that carried an object and one that did not go to
                // different counters. They used to share one, on the reasoning
                // that a 404 is a response - which is true, and is the wrong
                // thing to count for a benchmark. A 404 is 46 bytes and no
                // work, so a generator that requested a size the origin does
                // not serve and reported the results as 1 MiB transfers would
                // have reconciled perfectly against a machine that did
                // essentially nothing.
                if status == STATUS_OK {
                    shared.served.fetch_add(1, Ordering::Release);
                } else {
                    shared.unmatched.fetch_add(1, Ordering::Release);
                }
                write_response(stream, status, body, request.close)?;
                if request.close {
                    return Ok(());
                }
            }
        }
    }
}

/// Whether `ip` means "every interface".
///
/// `IpAddr::is_unspecified` is not enough. It answers for `0.0.0.0` and `::`
/// and nothing else, so the IPv4-mapped any-address `::ffff:0.0.0.0` returns
/// false - and a dual-stack Linux kernel binding it treats it as `INADDR_ANY`,
/// which is every IPv4 interface on the machine. That is precisely the outcome
/// [`OriginError::WildcardBind`] exists to prevent, arriving through a
/// spelling the guard did not recognise. It would also have minted a
/// certificate whose IP SAN is `::ffff:0.0.0.0`, matching no address any
/// client will ever connect to.
///
/// So a v4-mapped address is unwrapped and asked again, rather than trusted to
/// be a different thing because it is written differently.
fn is_wildcard(ip: IpAddr) -> bool {
    if ip.is_unspecified() {
        return true;
    }
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().is_some_and(|v4| v4.is_unspecified()),
        IpAddr::V4(_) => false,
    }
}

/// Whether this request may be answered with anything but a `401`.
///
/// Always true on a tokenless origin, which is every in-process module: the
/// only client is this process, two threads away, and putting a comparison in
/// the measured path to protect against it would cost something and protect
/// nothing.
fn authorised(request: &Request, shared: &Shared) -> bool {
    let Some(expected) = &shared.token else {
        return true;
    };
    request
        .token
        .as_ref()
        .is_some_and(|presented| expected.matches(presented))
}

/// Reads and parses one request head, leaving any bytes beyond it in `buffer`.
///
/// Leaving the remainder is not pipelining support so much as pipelining
/// *safety*: a client that writes two requests into one packet must not have
/// its second request silently discarded, because the resulting "the server
/// answered fewer requests than I sent" is exactly the shape of a saturation
/// signal and would be read as one.
fn read_head<S: Read>(
    stream: &mut S,
    buffer: &mut Vec<u8>,
    shared: &Shared,
) -> std::io::Result<Head> {
    let started = Instant::now();
    let mut scanned = 0usize;
    let mut chunk = [0u8; READ_CHUNK];

    loop {
        if let Some(end) = find_head_end(buffer, &mut scanned) {
            let head = parse_head(&buffer[..end]);
            buffer.drain(..end + 4);
            return Ok(match head {
                Some(request) => Head::Request(request),
                None => Head::Malformed,
            });
        }
        // Checked before the next read, so the buffer can exceed the bound by
        // at most the last chunk read into it and never grows again after that.
        if buffer.len() >= MAX_HEAD_BYTES {
            return Ok(Head::Oversized);
        }
        if shared.shutdown.load(Ordering::Relaxed) {
            return Ok(Head::Finished);
        }
        // Checked on every pass, not only when a read would have blocked.
        //
        // It used to live inside the retryable-error arm alone, which meant a
        // peer that produced *any* byte inside every poll interval never
        // reached it: the deadline was documented, tested against a silent
        // client, and unenforced against a slow one. Eight kilobytes at one
        // byte per timeout window is about twenty-six minutes of a connection
        // slot, per connection, renewable - and none of it needs a token,
        // because the head is never finished so the gate is never reached.
        //
        // The bound has to be here, at the top of the loop, because "how long
        // has this connection been sending me a head" is a property of the
        // loop and not of any one read's outcome.
        if !buffer.is_empty() && started.elapsed() >= HEAD_DEADLINE {
            return Ok(Head::Oversized);
        }

        match stream.read(&mut chunk) {
            Ok(0) => return Ok(Head::Finished),
            Ok(read) => buffer.extend_from_slice(&chunk[..read]),
            Err(error) if retryable(&error) => {
                if shared.shutdown.load(Ordering::Relaxed) {
                    return Ok(Head::Finished);
                }
                let deadline = if buffer.is_empty() {
                    IDLE_DEADLINE
                } else {
                    HEAD_DEADLINE
                };
                if started.elapsed() > deadline {
                    return Ok(Head::Finished);
                }
            }
            Err(error) => return Err(error),
        }
    }
}

/// A socket timeout or an interrupted syscall, not a broken connection.
fn retryable(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::WouldBlock
            | std::io::ErrorKind::TimedOut
            | std::io::ErrorKind::Interrupted
    )
}

/// Offset of the `\r\n\r\n` that ends the head, resuming from `scanned`.
///
/// The resume point is what keeps a client that sends the head one byte at a
/// time from costing O(n²) comparisons. It is safe to keep across reads because
/// the loop only advances past positions where a full four-byte window was
/// available to compare.
fn find_head_end(buffer: &[u8], scanned: &mut usize) -> Option<usize> {
    while *scanned + 4 <= buffer.len() {
        if &buffer[*scanned..*scanned + 4] == b"\r\n\r\n" {
            return Some(*scanned);
        }
        *scanned += 1;
    }
    None
}

/// Parses a complete head. `None` means "do not attempt to serve this".
fn parse_head(head: &[u8]) -> Option<Request> {
    // Non-UTF-8 in a head is either a broken client or someone probing; either
    // way it is rejected here rather than being handled byte-wise below.
    let head = std::str::from_utf8(head).ok()?;
    let mut lines = head.split("\r\n");

    let mut fields = lines.next()?.split(' ');
    let method = fields.next()?;
    let target = fields.next()?;
    let version = fields.next()?;
    if fields.next().is_some() {
        return None;
    }

    // HTTP/1.0 defaults to closing; HTTP/1.1 defaults to keeping the connection
    // open. Getting this backwards for 1.0 would make the origin hold a slot
    // for a client that has already stopped listening.
    let mut close = match version {
        "HTTP/1.1" => false,
        "HTTP/1.0" => true,
        _ => return None,
    };
    let mut token: Option<SessionToken> = None;

    for line in lines {
        // A continuation line. Deprecated by RFC 7230 and refused rather than
        // interpreted, because the only reason to send one here is to find out
        // what this parser does with it.
        if line.starts_with(' ') || line.starts_with('\t') {
            return None;
        }
        let (name, value) = line.split_once(':')?;
        let value = value.trim();
        if name.eq_ignore_ascii_case("connection") {
            // Token list, so `keep-alive, upgrade` has to be examined rather
            // than compared.
            for token in value.split(',') {
                let token = token.trim();
                if token.eq_ignore_ascii_case("close") {
                    close = true;
                } else if token.eq_ignore_ascii_case("keep-alive") {
                    close = false;
                }
            }
        } else if name.eq_ignore_ascii_case("content-length") {
            // A body would sit in the stream after this head and be misparsed
            // as the next request. See the module docs: refused, not ignored.
            if value != "0" {
                return None;
            }
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            return None;
        } else if name.eq_ignore_ascii_case(TOKEN_HEADER) {
            // Last one wins, and a duplicate header is not an error. The
            // comparison downstream is against a single 256-bit secret, so a
            // client sending two values gets one chance at it either way.
            token = value.parse().ok();
        }
    }

    Some(Request {
        get: method == "GET",
        object: object_size(target),
        close,
        token,
    })
}

/// The object size a request target names, if it names one.
///
/// This is the *only* thing derived from a request, and it is a number. There
/// is no string here that could become a path, a host or an argument - which is
/// what makes the traversal question in `docs/THREAT-MODEL.md` (T-PATH) not
/// arise for this module rather than merely be handled by it.
fn object_size(target: &str) -> Option<usize> {
    let path = target.split('?').next()?;
    let digits = path.strip_prefix(OBJECT_PREFIX)?;
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    // One canonical spelling per object. Without this, `/o/01024` and `/o/1024`
    // would be the same object under two URLs, and a cache or a proxy anywhere
    // in a future path would treat them as two.
    if digits.len() > 1 && digits.starts_with('0') {
        return None;
    }
    digits.parse::<usize>().ok()
}

/// Writes a complete response.
///
/// `Content-Length` on every response, including the empty ones, because a
/// framing that depends on connection close is not keep-alive-correct and the
/// generator would see a 404 as the end of the connection.
fn write_response<S: Write>(
    stream: &mut S,
    status: &str,
    body: &[u8],
    close: bool,
) -> std::io::Result<()> {
    let connection = if close { "close" } else { "keep-alive" };
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nContent-Type: application/octet-stream\r\n\
         Connection: {connection}\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    if !body.is_empty() {
        stream.write_all(body)?;
    }
    stream.flush()
}

#[cfg(test)]
// In tests, `unwrap`/`expect` panicking *is* the failure signal.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic_in_result_fn)]
mod tests {
    use super::*;

    const KIB: usize = 1024;

    /// A deliberately dumb HTTP client: it does exactly what it is told, so a
    /// test can tell it to do something a real client would not.
    struct Client<S> {
        stream: S,
        buffer: Vec<u8>,
    }

    impl<S: Read + Write> Client<S> {
        fn new(stream: S) -> Self {
            Self {
                stream,
                buffer: Vec::new(),
            }
        }

        fn get(&mut self, path: &str) -> std::io::Result<(u16, Vec<u8>)> {
            self.send(path, false)
        }

        fn send(&mut self, path: &str, close: bool) -> std::io::Result<(u16, Vec<u8>)> {
            let connection = if close { "close" } else { "keep-alive" };
            let request =
                format!("GET {path} HTTP/1.1\r\nHost: origin\r\nConnection: {connection}\r\n\r\n");
            self.stream.write_all(request.as_bytes())?;
            self.stream.flush()?;
            self.read_response()
        }

        fn read_response(&mut self) -> std::io::Result<(u16, Vec<u8>)> {
            let mut scanned = 0usize;
            let end = loop {
                if let Some(end) = find_head_end(&self.buffer, &mut scanned) {
                    break end;
                }
                self.fill()?;
            };
            let head =
                String::from_utf8(self.buffer[..end].to_vec()).map_err(std::io::Error::other)?;
            self.buffer.drain(..end + 4);

            let mut lines = head.split("\r\n");
            let status_line = lines.next().unwrap();
            let status: u16 = status_line.split(' ').nth(1).unwrap().parse().unwrap();
            let mut length = 0usize;
            for line in lines {
                let (name, value) = line.split_once(':').unwrap();
                if name.eq_ignore_ascii_case("content-length") {
                    length = value.trim().parse().unwrap();
                }
            }

            while self.buffer.len() < length {
                self.fill()?;
            }
            let body: Vec<u8> = self.buffer.drain(..length).collect();
            Ok((status, body))
        }

        fn fill(&mut self) -> std::io::Result<()> {
            let mut chunk = [0u8; 8192];
            match self.stream.read(&mut chunk)? {
                0 => Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "origin closed the connection",
                )),
                read => {
                    self.buffer.extend_from_slice(&chunk[..read]);
                    Ok(())
                }
            }
        }
    }

    fn connect(origin: &Origin) -> Client<TcpStream> {
        let stream = TcpStream::connect_timeout(&origin.address(), Duration::from_secs(5)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        Client::new(stream)
    }

    fn plain_origin(sizes: &[usize]) -> Origin {
        Origin::start(OriginConfig {
            object_sizes: sizes.to_vec(),
            tls: false,
            ..OriginConfig::default()
        })
        .unwrap()
    }

    /// A token-gated origin, bound to loopback so the test needs no second
    /// interface. `Bind::External` on a loopback address is authenticated
    /// listening on loopback, which is the authorised path with none of the
    /// exposure.
    fn gated_origin(sizes: &[usize], token: SessionToken) -> Origin {
        Origin::start(OriginConfig {
            object_sizes: sizes.to_vec(),
            tls: false,
            bind: Bind::External(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            token: Some(token),
        })
        .unwrap()
    }

    impl<S: Read + Write> Client<S> {
        /// A GET carrying an arbitrary extra header, so a test can present a
        /// wrong token, a malformed one, or none at all.
        fn get_with(
            &mut self,
            path: &str,
            header: Option<&str>,
        ) -> std::io::Result<(u16, Vec<u8>)> {
            let extra = header
                .map(|value| format!("{value}\r\n"))
                .unwrap_or_default();
            let request = format!(
                "GET {path} HTTP/1.1\r\nHost: origin\r\nConnection: keep-alive\r\n{extra}\r\n"
            );
            self.stream.write_all(request.as_bytes())?;
            self.stream.flush()?;
            self.read_response()
        }
    }

    // -----------------------------------------------------------------------
    // The token-gated, externally-bindable origin
    // -----------------------------------------------------------------------

    #[test]
    fn an_external_origin_without_a_token_never_starts_listening() {
        // The check that matters most in this file: the failure mode it
        // prevents is an unauthenticated benchmark origin on somebody's
        // datacentre network, and it must happen before a listener exists
        // rather than be caught by a caller who remembered to look.
        let error = Origin::start(OriginConfig {
            object_sizes: vec![KIB],
            tls: false,
            bind: Bind::External(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))),
            token: None,
        })
        .unwrap_err();
        assert!(
            matches!(error, OriginError::ExternalWithoutToken),
            "{error}"
        );
    }

    #[test]
    fn an_external_origin_refuses_to_bind_every_interface() {
        for wildcard in [
            IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED),
        ] {
            let error = Origin::start(OriginConfig {
                object_sizes: vec![KIB],
                tls: false,
                bind: Bind::External(wildcard),
                token: Some(SessionToken::try_new().unwrap()),
            })
            .unwrap_err();
            assert!(
                matches!(error, OriginError::WildcardBind),
                "{wildcard}: {error}"
            );
        }
    }

    #[test]
    fn a_request_with_the_session_token_is_served_normally() {
        let token = SessionToken::try_new().unwrap();
        let origin = gated_origin(&[KIB], token.clone());
        let mut client = connect(&origin);
        let header = format!("{TOKEN_HEADER}: {}", token.to_hex());
        let (status, body) = client.get_with("/o/1024", Some(&header)).unwrap();
        assert_eq!(status, 200);
        assert_eq!(body.len(), KIB);
        assert_eq!(origin.refused(), 0);
        assert_eq!(origin.served(), 1);
    }

    #[test]
    fn a_request_without_a_token_is_refused_and_never_counted_as_served() {
        // `served` is what an external generator's claims are reconciled
        // against. Counting a stranger's probe in it would turn any passing
        // port scanner into an invalidated benchmark run.
        let origin = gated_origin(&[KIB], SessionToken::try_new().unwrap());
        let mut client = connect(&origin);
        assert_eq!(client.get_with("/o/1024", None).unwrap().0, 401);
        assert_eq!(origin.served(), 0);
        assert_eq!(origin.refused(), 1);
    }

    #[test]
    fn a_wrong_token_and_a_malformed_one_are_refused_identically() {
        // A client must not be able to learn from the status code whether it
        // got the shape of the secret right.
        let origin = gated_origin(&[KIB], SessionToken::try_new().unwrap());
        let wrong = SessionToken::try_new().unwrap().to_hex();
        for value in [wrong.as_str(), "not-hex", "", "ab"] {
            let mut client = connect(&origin);
            let header = format!("{TOKEN_HEADER}: {value}");
            assert_eq!(
                client.get_with("/o/1024", Some(&header)).unwrap().0,
                401,
                "presented {value:?}"
            );
        }
        assert_eq!(origin.served(), 0);
        assert_eq!(origin.refused(), 4);
    }

    #[test]
    fn an_unauthorised_request_cannot_learn_which_objects_exist() {
        // The gate runs before the body table, so a 404 and a 200 look the
        // same to a client that has not authenticated. Without this ordering
        // an unauthenticated stranger could map the origin's configuration by
        // reading status codes.
        let origin = gated_origin(&[KIB], SessionToken::try_new().unwrap());
        for path in ["/o/1024", "/o/999999", "/nonsense"] {
            let mut client = connect(&origin);
            assert_eq!(client.get_with(path, None).unwrap().0, 401, "{path}");
        }
    }

    #[test]
    fn a_refused_request_ends_the_connection() {
        // Otherwise a client with no token holds one of the origin's bounded
        // connection slots for as long as it likes, guessing.
        let origin = gated_origin(&[KIB], SessionToken::try_new().unwrap());
        let mut client = connect(&origin);
        assert_eq!(client.get_with("/o/1024", None).unwrap().0, 401);
        assert!(
            client.get_with("/o/1024", None).is_err(),
            "the origin kept the connection open after refusing a request"
        );
    }

    #[test]
    fn a_wildcard_written_as_an_ipv4_mapped_address_is_still_a_wildcard() {
        // `IpAddr::is_unspecified` answers for `0.0.0.0` and `::` and nothing
        // else, so `::ffff:0.0.0.0` walked past the guard - and a dual-stack
        // kernel binding it treats it as INADDR_ANY, every IPv4 interface on
        // the machine. Exactly the outcome the guard exists to prevent,
        // arriving through a spelling it did not recognise.
        let mapped: IpAddr = "::ffff:0.0.0.0".parse().unwrap();
        assert!(!mapped.is_unspecified(), "the premise of this test changed");
        assert!(is_wildcard(mapped));

        let error = Origin::start(OriginConfig {
            object_sizes: vec![KIB],
            tls: false,
            bind: Bind::External(mapped),
            token: Some(SessionToken::try_new().unwrap()),
        })
        .unwrap_err();
        assert!(matches!(error, OriginError::WildcardBind), "{error}");

        // And a real address written the same way is still allowed.
        assert!(!is_wildcard("::ffff:127.0.0.1".parse::<IpAddr>().unwrap()));
    }

    #[test]
    fn a_head_that_never_ends_cannot_hold_a_slot_by_trickling() {
        // The deadline used to be checked only when a read would have blocked,
        // so a peer producing any byte inside every poll window never reached
        // it: 8 KiB at one byte per window is about twenty-six minutes of a
        // connection slot, renewable, with no token needed - the head is never
        // finished, so the gate is never reached.
        //
        // Driven through `serve` directly rather than over a socket, so the
        // test asserts the loop's bound rather than waiting out a real
        // deadline.
        struct Trickle {
            sent: usize,
        }
        impl Read for Trickle {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                // One byte at a time, forever, never a terminator.
                self.sent += 1;
                buffer[0] = b'x';
                Ok(1)
            }
        }
        impl Write for Trickle {
            fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
                Ok(buffer.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let origin = gated_origin(&[KIB], SessionToken::try_new().unwrap());
        let mut stream = Trickle { sent: 0 };
        // Returns rather than hanging: the byte bound stops it even without
        // the clock, and the clock stops it even without the byte bound.
        serve(&mut stream, &origin.shared).unwrap();
        assert!(
            stream.sent <= MAX_HEAD_BYTES + READ_CHUNK,
            "read {} bytes looking for a head",
            stream.sent
        );
        assert_eq!(origin.refused(), 1, "an unparseable head must be counted");
    }

    #[test]
    fn a_gated_origin_answers_a_malformed_probe_with_401_and_counts_it() {
        // Most scanner traffic is not a well-formed HTTP request. Answering it
        // `400` told the scanner it had found an HTTP server and left no trace
        // in the counter the bundle discloses.
        let origin = gated_origin(&[KIB], SessionToken::try_new().unwrap());
        for probe in [
            "GET /o/1024 HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n",
            "GET /o/1024 HTTP/9.9\r\nHost: x\r\n\r\n",
            "GET /o/1024 HTTP/1.1\r\nHost: x\r\n \tfolded\r\n\r\n",
            "\r\n\r\n",
        ] {
            let mut stream =
                TcpStream::connect_timeout(&origin.address(), Duration::from_secs(5)).unwrap();
            stream.write_all(probe.as_bytes()).unwrap();
            stream.flush().unwrap();
            let mut answer = String::new();
            let _ = stream.read_to_string(&mut answer);
            assert!(answer.starts_with("HTTP/1.1 401"), "{probe:?}: {answer}");
        }
        assert_eq!(origin.refused(), 4);
        assert_eq!(origin.served(), 0);
    }

    #[test]
    fn a_tokenless_origin_keeps_its_diagnostic_statuses() {
        // The in-process modules have no adversary and `400` says which bound
        // was hit. Collapsing every failure to `401` there would trade a
        // useful diagnostic for protection against nobody.
        let origin = plain_origin(&[KIB]);
        let mut stream =
            TcpStream::connect_timeout(&origin.address(), Duration::from_secs(5)).unwrap();
        stream
            .write_all(b"GET /o/1024 HTTP/9.9\r\nHost: x\r\n\r\n")
            .unwrap();
        stream.flush().unwrap();
        let mut answer = String::new();
        let _ = stream.read_to_string(&mut answer);
        assert!(answer.starts_with("HTTP/1.1 400"), "{answer}");
        assert_eq!(origin.refused(), 0);
    }

    #[test]
    fn a_404_is_not_counted_as_a_request_the_origin_served() {
        // A 404 is 46 bytes and no work. Counting it in `served` let a
        // generator request a size this origin does not serve, report the
        // results as 1 MiB transfers, and reconcile perfectly against a
        // machine that did essentially nothing.
        let origin = plain_origin(&[KIB]);
        let mut client = connect(&origin);
        assert_eq!(client.get("/o/999999").unwrap().0, 404);
        assert_eq!(client.get("/o/1024").unwrap().0, 200);
        assert_eq!(origin.served(), 1);
        assert_eq!(origin.unmatched(), 1);
    }

    #[test]
    fn a_tokenless_origin_serves_a_request_that_carries_a_token_anyway() {
        // The in-process modules must be entirely unaffected by this feature.
        let origin = plain_origin(&[KIB]);
        let mut client = connect(&origin);
        let header = format!(
            "{TOKEN_HEADER}: {}",
            SessionToken::try_new().unwrap().to_hex()
        );
        assert_eq!(client.get_with("/o/1024", Some(&header)).unwrap().0, 200);
        assert_eq!(origin.refused(), 0);
    }

    #[test]
    fn an_external_tls_origin_certifies_the_address_it_actually_bound() {
        // A certificate naming loopback on an externally-bound origin would
        // fail verification at the far end - in the middle of a benchmark,
        // rather than at start-up.
        let ip = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let origin = Origin::start(OriginConfig {
            object_sizes: vec![KIB],
            tls: true,
            bind: Bind::External(ip),
            token: Some(SessionToken::try_new().unwrap()),
        })
        .unwrap();
        assert_eq!(origin.address().ip(), ip);
        assert!(origin.certificate_der().is_some());
    }

    #[test]
    fn the_origin_binds_loopback_on_an_os_assigned_port() {
        let origin = plain_origin(&[KIB]);
        let address = origin.address();
        assert!(
            address.ip().is_loopback(),
            "T-AMPLIFY makes loopback-only structural, not a default: bound {address}"
        );
        assert_ne!(address.port(), 0, "the OS should have assigned a real port");
    }

    #[test]
    fn the_origin_serves_exactly_the_number_of_bytes_the_path_names() {
        let sizes = [0, 1, KIB, 64 * KIB, 1024 * KIB];
        let origin = plain_origin(&sizes);
        let mut client = connect(&origin);

        for size in sizes {
            let path = origin.path_for(size).unwrap();
            let (status, body) = client.get(&path).unwrap();
            assert_eq!(status, 200, "{path}");
            assert_eq!(
                body.len(),
                size,
                "{path} served {} bytes; a size that is off by even one makes every \
                 throughput number wrong by a factor nobody would notice",
                body.len()
            );
        }
        assert_eq!(origin.served(), sizes.len() as u64);
    }

    #[test]
    fn the_same_object_is_byte_identical_on_every_request() {
        let origin = plain_origin(&[64 * KIB]);
        let path = origin.path_for(64 * KIB).unwrap();

        let mut client = connect(&origin);
        let (_, first) = client.get(&path).unwrap();
        let (_, second) = client.get(&path).unwrap();
        // A different connection, in case anything ever became per-connection.
        let mut other = connect(&origin);
        let (_, third) = other.get(&path).unwrap();

        assert_eq!(first, second);
        assert_eq!(first, third);
    }

    #[test]
    fn the_bodies_are_not_trivially_compressible() {
        // A body of repeated bytes would let anything in the path - a proxy, a
        // filesystem, a hypervisor deduplicating pages - turn a 1 MiB transfer
        // into a much smaller one, and the machine would look faster than it is.
        let master = generate_master(256 * KIB);
        let mut encoder =
            flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(&master).unwrap();
        let compressed = encoder.finish().unwrap();
        let ratio = compressed.len() as f64 / master.len() as f64;
        assert!(
            ratio > 0.95,
            "the object corpus compresses to {ratio:.2} of its size, so a compressing \
             intermediary could quietly shrink the transfer being measured"
        );
    }

    #[test]
    fn the_corpus_is_the_same_on_every_machine_and_every_run() {
        // Pins the actual bytes, not just self-consistency: a change to the
        // seed or the fill order would otherwise pass every other test here and
        // silently break comparability with results already published.
        let master = generate_master(16);
        assert_eq!(
            master,
            SplitMix64::new(ORIGIN_SEED)
                .next_u64()
                .to_le_bytes()
                .into_iter()
                .chain({
                    let mut rng = SplitMix64::new(ORIGIN_SEED);
                    rng.next_u64();
                    rng.next_u64().to_le_bytes()
                })
                .collect::<Vec<u8>>()
        );
        // Small objects are prefixes of large ones, which is what the single
        // master buffer buys and what a future reader would otherwise have to
        // rediscover from the implementation.
        assert_eq!(generate_master(1024), generate_master(4096)[..1024]);
    }

    #[test]
    fn one_connection_serves_many_requests_when_keep_alive_is_left_alone() {
        let origin = plain_origin(&[KIB]);
        let path = origin.path_for(KIB).unwrap();
        let mut client = connect(&origin);

        // If the origin closed after the first response, the second request
        // would fail rather than merely be slower - so eight successes on one
        // stream is the assertion, and `served` confirms the origin agrees
        // about how many requests happened.
        for request in 0..8 {
            let (status, body) = client.get(&path).unwrap();
            assert_eq!(status, 200, "request {request} on a reused connection");
            assert_eq!(body.len(), KIB);
        }
        assert_eq!(origin.served(), 8);
    }

    #[test]
    fn a_connection_close_request_is_the_last_one_on_that_connection() {
        let origin = plain_origin(&[KIB]);
        let path = origin.path_for(KIB).unwrap();
        let mut client = connect(&origin);

        let (status, body) = client.send(&path, true).unwrap();
        assert_eq!(status, 200);
        assert_eq!(body.len(), KIB);

        // The response was complete and correct; what must follow is the end of
        // the connection, not another response.
        let after = client.send(&path, true);
        assert!(
            after.is_err(),
            "the origin kept serving a connection the client asked it to close"
        );
        assert_eq!(origin.served(), 1);
    }

    #[test]
    fn an_unknown_path_is_a_404_that_the_connection_survives() {
        let origin = plain_origin(&[KIB]);
        let mut client = connect(&origin);

        for path in [
            "/",
            "/o/",
            "/o/999",
            "/o/01024",
            "/o/abc",
            "/../../etc/passwd",
        ] {
            let (status, body) = client.get(path).unwrap();
            assert_eq!(status, 404, "{path}");
            assert!(body.is_empty(), "{path}");
        }

        // Keep-alive correctness after a 404 is the part that is easy to get
        // wrong: a zero-length body with no `Content-Length` looks identical to
        // a connection the server intends to close.
        let path = origin.path_for(KIB).unwrap();
        let (status, body) = client.get(&path).unwrap();
        assert_eq!(status, 200);
        assert_eq!(body.len(), KIB);
    }

    #[test]
    fn an_oversized_request_head_is_refused_without_unbounded_buffering() {
        let origin = plain_origin(&[KIB]);
        let stream = TcpStream::connect_timeout(&origin.address(), Duration::from_secs(5)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();

        // A megabyte of request line with no terminator in sight. The origin
        // must stop at MAX_HEAD_BYTES; if it buffers what it is given, this is
        // the shape of the request that eventually takes the host down.
        let mut stream = stream;
        let filler = vec![b'A'; 4096];
        let _ = stream.write_all(b"GET /o/");
        for _ in 0..256 {
            // Writes fail once the origin has closed on us, which is the
            // outcome under test rather than a problem.
            if stream.write_all(&filler).is_err() {
                break;
            }
        }
        let _ = stream.flush();

        // Whatever it answers, the connection has to end - promptly, and by the
        // origin's decision rather than by the client giving up. The read
        // timeout above is what turns "hangs" into a failure.
        let mut sink = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            match stream.read(&mut chunk) {
                Ok(0) => break,
                // A reset is the normal outcome: the origin answers and closes
                // while unread request bytes are still queued.
                Err(_) => break,
                Ok(read) => sink.extend_from_slice(&chunk[..read]),
            }
            assert!(
                sink.len() < 64 * KIB,
                "the origin answered a refused request at length"
            );
        }
        assert_eq!(
            origin.served(),
            0,
            "a request that was never understood must not be counted as served"
        );

        // And the origin is still a working server afterwards.
        let mut client = connect(&origin);
        let path = origin.path_for(KIB).unwrap();
        assert_eq!(client.get(&path).unwrap().0, 200);
    }

    #[test]
    fn a_request_carrying_a_body_is_refused_rather_than_misparsed() {
        let origin = plain_origin(&[KIB]);
        let mut stream =
            TcpStream::connect_timeout(&origin.address(), Duration::from_secs(5)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        stream
            .write_all(b"GET /o/1024 HTTP/1.1\r\nHost: origin\r\nContent-Length: 5\r\n\r\nhello")
            .unwrap();

        let mut client = Client::new(stream);
        let (status, _) = client.read_response().unwrap();
        assert_eq!(
            status, 400,
            "ignoring the body would leave `hello` in the stream to be read as the next \
             request line"
        );
    }

    #[test]
    fn a_method_other_than_get_is_refused() {
        let origin = plain_origin(&[KIB]);
        let mut stream =
            TcpStream::connect_timeout(&origin.address(), Duration::from_secs(5)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        stream
            .write_all(b"POST /o/1024 HTTP/1.1\r\nHost: origin\r\n\r\n")
            .unwrap();
        let mut client = Client::new(stream);
        assert_eq!(client.read_response().unwrap().0, 405);
    }

    #[test]
    fn path_for_refuses_a_size_the_origin_was_never_asked_to_serve() {
        let origin = plain_origin(&[KIB, 64 * KIB]);
        assert_eq!(origin.path_for(KIB).as_deref(), Some("/o/1024"));
        assert_eq!(origin.path_for(64 * KIB).as_deref(), Some("/o/65536"));
        assert_eq!(
            origin.path_for(2048),
            None,
            "a module asking for a size it never configured should find out at \
             request-building time, not from a run full of 404s"
        );
    }

    #[test]
    fn an_object_larger_than_the_ceiling_is_refused_before_anything_is_allocated() {
        let error = Origin::start(OriginConfig {
            object_sizes: vec![MAX_OBJECT_BYTES + 1],
            tls: false,
            ..OriginConfig::default()
        })
        .expect_err("a size above the ceiling must not be allocated");
        assert!(
            matches!(error, OriginError::ObjectTooLarge { .. }),
            "{error}"
        );
    }

    #[test]
    fn a_tls_client_that_trusts_only_the_run_certificate_gets_a_body() {
        let origin = Origin::start(OriginConfig {
            object_sizes: vec![64 * KIB],
            tls: true,
            ..OriginConfig::default()
        })
        .unwrap();

        // The trust store contains the run's certificate and nothing else: no
        // host roots, no bundled roots. A handshake that succeeds here proves
        // the origin presented that exact certificate with an IP SAN a client
        // connecting to 127.0.0.1 can verify.
        let mut roots = rustls::RootCertStore::empty();
        roots.add(origin.certificate_der().unwrap()).unwrap();
        let config = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();

        let name = rustls_pki_types::ServerName::try_from("127.0.0.1")
            .unwrap()
            .to_owned();
        let session = rustls::ClientConnection::new(Arc::new(config), name).unwrap();
        let stream = TcpStream::connect_timeout(&origin.address(), Duration::from_secs(5)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();

        let mut client = Client::new(rustls::StreamOwned::new(session, stream));
        let path = origin.path_for(64 * KIB).unwrap();
        let (status, body) = client.get(&path).unwrap();
        assert_eq!(status, 200);
        assert_eq!(body.len(), 64 * KIB);

        // Keep-alive has to work over TLS too, or the TLS numbers would be
        // handshake numbers.
        let (status, body) = client.get(&path).unwrap();
        assert_eq!(status, 200);
        assert_eq!(body.len(), 64 * KIB);
        assert_eq!(origin.served(), 2);

        // Same bytes as the plaintext origin: TLS must not be measuring a
        // different object.
        assert_eq!(body, generate_master(64 * KIB));
    }

    #[test]
    fn a_plaintext_origin_has_no_certificate() {
        assert!(plain_origin(&[KIB]).certificate_der().is_none());
    }

    #[test]
    fn dropping_the_origin_releases_the_port() {
        let address = {
            let origin = plain_origin(&[KIB]);
            let address = origin.address();
            // Leave a live keep-alive connection behind, because a shutdown
            // that only works for an idle origin is a shutdown that will hang
            // the first time a run ends under load.
            let mut client = connect(&origin);
            let path = origin.path_for(KIB).unwrap();
            assert_eq!(client.get(&path).unwrap().0, 200);
            address
        };

        assert!(
            TcpStream::connect_timeout(&address, Duration::from_secs(2)).is_err(),
            "the origin's port is still accepting connections after the Origin was dropped"
        );
        // And the address is genuinely free again, not merely refusing.
        drop(TcpListener::bind(address).expect("the released port should be bindable again"));
    }

    #[test]
    fn the_origin_can_be_shared_across_the_threads_that_will_load_it() {
        // The load generator drives this from many threads at once; a type that
        // was accidentally not `Sync` would fail far from here.
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Origin>();
    }

    #[test]
    fn a_target_is_parsed_as_a_number_and_never_as_a_path() {
        assert_eq!(object_size("/o/1024"), Some(1024));
        assert_eq!(object_size("/o/1024?cache=0"), Some(1024));
        assert_eq!(object_size("/o/0"), Some(0));
        assert_eq!(object_size("/o/01024"), None);
        assert_eq!(object_size("/o/+1024"), None);
        assert_eq!(object_size("/o/../../etc/passwd"), None);
        assert_eq!(object_size("/o/%2e%2e%2fetc"), None);
        assert_eq!(object_size("/etc/passwd"), None);
        assert_eq!(object_size("/o/"), None);
        assert_eq!(
            object_size(&format!("/o/{}", u128::from(u64::MAX) + 1)),
            None,
            "a size that does not fit a usize must not wrap into one that does"
        );
    }
}
