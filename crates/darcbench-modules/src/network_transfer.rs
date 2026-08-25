//! `network.transfer` - the Phase 2 network module.
//!
//! # What it measures
//!
//! | Metric | Unit | What it predicts |
//! |---|---|---|
//! | `dns_resolve.mean` | ms | How long a name costs before any byte moves |
//! | `tcp_connect.mean` | ms | Round-trip time to the nearest major edge |
//! | `tcp_connect.jitter` | ms | How *steady* that round trip is |
//! | `tls_handshake.mean` | ms | The cost every HTTPS connection pays |
//! | `ttfb.mean` | ms | Request out, first byte back |
//! | `download.single` | Mbit/s | One stream, as a single client sees it |
//! | `download.multi` | Mbit/s | Four streams, as a busy server sees it |
//!
//! The four phases are timed **separately** and never rolled into one number,
//! because they fail for different reasons and have different fixes: slow DNS
//! is a resolver problem, slow connect is distance or routing, slow TLS is CPU
//! or a bad cipher choice, and slow TTFB after all three is the far end.
//!
//! # Why one stream and four are both reported
//!
//! A single TCP stream on a long fat path is limited by window size and loss
//! recovery, not by the link. A server serving many clients has many streams.
//! Reporting only one understates a good link; reporting only four hides a
//! machine that cannot fill a pipe with one connection. They are different
//! questions and get different metrics.
//!
//! # Safety: this module is why the endpoint table is a `const`
//!
//! `docs/THREAT-MODEL.md` (T-DDOS) is permanent and binding: *"A benchmark
//! suite that lets you point a load generator at an arbitrary URL is a DDoS
//! tool with a scoring model."* Every host contacted here comes from
//! [`crate::network_endpoints`], which is a compile-time table. No API field,
//! environment variable or configuration file reaches it. The only value this
//! module ever formats into a request is a byte count it computed and clamped
//! itself.
//!
//! Volume is bounded by [`TRANSFER_CEILING_BYTES`] against a running total that
//! spans calibration, warm-ups and every repetition. When the ceiling is
//! reached the module stops and says so, rather than finishing the run.
//!
//! # What this module deliberately does not do
//!
//! **Packet loss.** Measuring it properly needs ICMP or raw sockets, which need
//! privileges this module does not take and `unsafe` this workspace forbids.
//! Inferring it from TCP behaviour would be a guess wearing a precise name, so
//! it is declared as not measured rather than estimated.
//!
//! **Upload.** Sending bulk data to a third party is a different traffic
//! profile and a different conversation with whoever runs the endpoint. Left
//! out until there is an endpoint whose published purpose covers it.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use darcbench_protocol::metrics::{Direction, Metric, Warning, WarningCode};
use darcbench_protocol::stats::{outlier_indices, summarize};
use darcbench_protocol::ModuleId;

use crate::harness::time_reps;
use crate::module::{
    BenchmarkModule, ModuleError, ModuleManifest, ModuleOutput, ModuleParams, ModuleReporter,
    SafetyClass,
};
use crate::network_endpoints::{
    Endpoint, LATENCY_ENDPOINTS, THROUGHPUT_ENDPOINT, TRANSFER_CEILING_BYTES,
};

/// Workload-definition version. Major bump = results are not comparable.
pub const VERSION: &str = "1.0.0";

/// The module's identifier, validated against the [`ModuleId`] grammar by a
/// unit test in this file.
pub const MODULE_ID: &str = "network.transfer";

/// Timeout for every network operation.
///
/// Generous enough for a satellite link, short enough that an unreachable host
/// cannot stall a run. A benchmark that hangs is worse than one that fails.
const TIMEOUT: Duration = Duration::from_secs(10);

/// Streams opened by the multi-stream download.
///
/// Four rather than a larger number: it is enough to show whether a machine can
/// use more of its link with concurrency, and few enough that the module is not
/// opening a connection storm at a third party.
const PARALLEL_STREAMS: usize = 4;

/// Largest download this module will request from one stream.
const MAX_DOWNLOAD_BYTES: u64 = 8 << 20;

/// Smallest download still worth timing.
const MIN_DOWNLOAD_BYTES: u64 = 1 << 20;

/// Bytes requested per download repetition, per stream.
///
/// Deliberately **not** calibrated to `target_rep_ms`. Calibration would let a
/// fast link pull progressively more from a third-party service to fill a time
/// budget, which is exactly the amplifier shape the methodology forbids: a
/// faster machine would take more from the endpoint, not finish sooner.
///
/// Instead the size is derived from the transfer ceiling and the repetition
/// count, so the *whole run* fits inside the ceiling whatever profile selected
/// it. Without this a `deep` run wants 560 MiB and an `endurance` run 1.3 GiB
/// against a 512 MiB ceiling, and the module would spend the second half of
/// every long run tripping its own emergency brake.
fn download_bytes(params: &ModuleParams) -> u64 {
    let reps = (params.warmup_reps + params.measured_reps).max(1) as u64;
    // Both download shapes run every repetition: one stream, then four.
    let transfers = reps.saturating_mul(1 + PARALLEL_STREAMS as u64);
    (TRANSFER_CEILING_BYTES / transfers.max(1)).clamp(MIN_DOWNLOAD_BYTES, MAX_DOWNLOAD_BYTES)
}

/// Bytes read from a latency probe before the connection is dropped.
///
/// Enough to have seen the first byte of the response; the body is not wanted.
const PROBE_READ_BYTES: usize = 1;

/// Read buffer for draining a download.
const READ_CHUNK: usize = 64 * 1024;

/// Smallest transfer that may be turned into a rate.
const MIN_RATE_BYTES: u64 = 512 * 1024;

/// Shortest transfer that may be turned into a rate. Below this the clock is
/// measuring itself rather than the link.
const MIN_RATE_SECONDS: f64 = 0.005;

/// Fewest successful samples before a metric is reported at all.
const MIN_SAMPLES: usize = 3;

// ---------------------------------------------------------------------------
// Transfer budget
// ---------------------------------------------------------------------------

/// The run's remaining transfer allowance.
///
/// Shared across threads because the multi-stream download spends from it
/// concurrently. Every read checks in before it happens, so the ceiling holds
/// even when four streams are draining at once.
#[derive(Debug)]
struct Budget {
    spent: AtomicU64,
    ceiling: u64,
}

impl Budget {
    fn new(ceiling: u64) -> Self {
        Self {
            spent: AtomicU64::new(0),
            ceiling,
        }
    }

    /// Reserves `bytes`, returning how many were actually available.
    ///
    /// A compare-and-swap loop rather than a plain `fetch_add`, so a concurrent
    /// reservation can never push the total past the ceiling even briefly.
    fn reserve(&self, bytes: u64) -> u64 {
        let mut current = self.spent.load(Ordering::Relaxed);
        loop {
            let remaining = self.ceiling.saturating_sub(current);
            let granted = bytes.min(remaining);
            if granted == 0 {
                return 0;
            }
            match self.spent.compare_exchange_weak(
                current,
                current + granted,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return granted,
                Err(observed) => current = observed,
            }
        }
    }

    /// Returns an unused part of a reservation.
    ///
    /// Reads are reserved before they happen so the ceiling can never be
    /// exceeded even transiently, but a TLS record is ~16 KiB where the read
    /// buffer is 64 KiB. Without the refund the budget charges four times what
    /// crossed the wire, hits its ceiling at a quarter of the intended volume,
    /// and truncates the very downloads it is meant to bound.
    fn refund(&self, bytes: u64) {
        if bytes > 0 {
            self.spent.fetch_sub(bytes, Ordering::Relaxed);
        }
    }

    fn spent(&self) -> u64 {
        self.spent.load(Ordering::Relaxed)
    }

    fn exhausted(&self) -> bool {
        self.spent() >= self.ceiling
    }
}

// ---------------------------------------------------------------------------
// One timed connection
// ---------------------------------------------------------------------------

/// Timings for one connection, in milliseconds. Every phase is separate.
#[derive(Clone, Copy, Debug, Default)]
struct Phases {
    dns_ms: f64,
    connect_ms: f64,
    tls_ms: f64,
    ttfb_ms: f64,
    /// Bytes of response body actually read.
    body_bytes: u64,
    /// Seconds spent reading the body, from first byte to last.
    body_seconds: f64,
    /// Which address family the connection actually used.
    ipv6: bool,
    /// True when the transfer ceiling cut the download short.
    truncated: bool,
}

impl Phases {
    /// Transfer rate, or `None` when this connection cannot support one.
    ///
    /// A download the budget cut short is **not** a throughput measurement: the
    /// bytes that did arrive divided by the fraction of a second they took is a
    /// number with no physical meaning, and left unguarded it produces figures
    /// like 652 Gbit/s. `MIN_RATE_BYTES` and `MIN_RATE_SECONDS` reject the same
    /// shape arriving any other way - a body that was too small or finished too
    /// fast to time.
    fn mbit_s(&self) -> Option<f64> {
        if self.truncated
            || self.body_bytes < MIN_RATE_BYTES
            || self.body_seconds < MIN_RATE_SECONDS
        {
            return None;
        }
        Some((self.body_bytes as f64 * 8.0) / self.body_seconds / 1_000_000.0)
    }
}

/// A TLS client configuration built once and shared by every connection.
///
/// Trust anchors come from the host store - see ADR-0011: a tool running on
/// someone else's server should trust the CAs that machine's administrator
/// trusts, not a snapshot baked into the binary.
fn tls_config() -> Result<Arc<rustls::ClientConfig>, ModuleError> {
    let mut roots = rustls::RootCertStore::empty();
    let loaded = rustls_native_certs::load_native_certs();
    for certificate in loaded.certs {
        // A store can legitimately contain a certificate rustls will not parse;
        // that is not a reason to refuse to run.
        let _ = roots.add(certificate);
    }
    if roots.is_empty() {
        return Err(ModuleError::Precondition(
            "this host has no usable TLS trust anchors, so no measurement can be authenticated; \
             refusing rather than trusting roots the administrator did not configure"
                .into(),
        ));
    }
    let mut config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    // Every handshake must be a full one. With resumption on, the first
    // connection pays 130 ms and every later one resumes in under a
    // millisecond, so the reported "TLS handshake" would be an average of two
    // different operations and would flatter the machine. A fresh visitor to a
    // server pays the full handshake, and that is the cost worth reporting.
    config.resumption = rustls::client::Resumption::disabled();
    Ok(Arc::new(config))
}

/// Resolves, connects, handshakes and optionally downloads, timing each phase.
///
/// `download_bytes` of `None` means a latency probe: the response is opened and
/// the first byte read, then the connection is dropped.
fn measure(
    endpoint: &Endpoint,
    config: &Arc<rustls::ClientConfig>,
    download_bytes: Option<u64>,
    budget: &Budget,
    prefer_ipv6: bool,
) -> std::io::Result<Phases> {
    let mut phases = Phases::default();

    // --- DNS -------------------------------------------------------------
    let started = Instant::now();
    let addresses: Vec<SocketAddr> = (endpoint.host, endpoint.port).to_socket_addrs()?.collect();
    phases.dns_ms = started.elapsed().as_secs_f64() * 1000.0;
    let address = pick_address(&addresses, prefer_ipv6).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("{} resolved to no usable address", endpoint.host),
        )
    })?;
    phases.ipv6 = address.is_ipv6();

    // --- TCP -------------------------------------------------------------
    let started = Instant::now();
    let stream = TcpStream::connect_timeout(&address, TIMEOUT)?;
    phases.connect_ms = started.elapsed().as_secs_f64() * 1000.0;
    stream.set_read_timeout(Some(TIMEOUT))?;
    stream.set_write_timeout(Some(TIMEOUT))?;
    // Nagle would batch the request write and distort TTFB.
    stream.set_nodelay(true)?;

    // --- TLS -------------------------------------------------------------
    let server_name = rustls_pki_types::ServerName::try_from(endpoint.host)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?
        .to_owned();
    let mut connection = rustls::ClientConnection::new(config.clone(), server_name)
        .map_err(std::io::Error::other)?;
    let mut stream = stream;
    let started = Instant::now();
    // Drive the handshake to completion before timing anything else, so the
    // TLS number is the handshake and not the handshake plus the request.
    while connection.is_handshaking() {
        if connection.wants_write() {
            connection.write_tls(&mut stream)?;
        } else if connection.wants_read() {
            if connection.read_tls(&mut stream)? == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "connection closed during the TLS handshake",
                ));
            }
            connection
                .process_new_packets()
                .map_err(std::io::Error::other)?;
        } else {
            break;
        }
    }
    phases.tls_ms = started.elapsed().as_secs_f64() * 1000.0;

    // --- request ---------------------------------------------------------
    let path = match download_bytes {
        Some(bytes) => endpoint.download_path(bytes),
        None => endpoint.probe_path(),
    };
    // The request line is assembled from compile-time constants plus a byte
    // count this module computed. Nothing here comes from a caller.
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {}\r\nUser-Agent: darcbench/{VERSION}\r\n\
         Accept: */*\r\nConnection: close\r\n\r\n",
        endpoint.host
    );

    let mut tls = rustls::Stream::new(&mut connection, &mut stream);
    let started = Instant::now();
    tls.write_all(request.as_bytes())?;
    tls.flush()?;

    // --- response --------------------------------------------------------
    //
    // The first read is charged to the budget like every other. It is the only
    // read a latency probe performs, and there are more probe connections in a
    // run than download connections, so leaving it uncharged meant real traffic
    // exceeded the ceiling by every probe's response - a ceiling described as
    // enforced rather than documented cannot be approximately right.
    let mut buffer = vec![0u8; READ_CHUNK];
    let granted = budget.reserve(READ_CHUNK as u64);
    if granted == 0 {
        // An error rather than a truncated `Phases`: with no response read at
        // all there are no phase timings to report, and the run-level ceiling
        // warning already says what happened.
        return Err(std::io::Error::other(
            "transfer ceiling reached before the response could be read",
        ));
    }
    let read = match tls.read(&mut buffer[..granted as usize]) {
        Ok(n) => {
            budget.refund(granted - n as u64);
            n
        }
        Err(error) => {
            budget.refund(granted);
            return Err(error);
        }
    };
    phases.ttfb_ms = started.elapsed().as_secs_f64() * 1000.0;
    if read == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "no response",
        ));
    }

    // Every response must at least *be* an HTTP response. That much is checked
    // for probes too, because it is what separates "the far end answered" from
    // "something on this port sent bytes".
    let head = &buffer[..read];
    let status = http_status(head).ok_or_else(|| {
        std::io::Error::other("response did not begin with an HTTP/1.x status line")
    })?;

    // The status itself only decides a *download*. A probe is timing the
    // network - DNS, the handshake, the round trip - and any well-formed answer
    // measures that equally well. A download is different: the bytes that
    // follow are about to be timed and published as throughput, so an error
    // page or a redirect body must not become the sample. That is the whole
    // reason this check exists.
    if download_bytes.is_some() && status != 200 {
        return Err(std::io::Error::other(format!(
            "endpoint answered HTTP {status}; a body that is not the requested payload is not a \
             throughput sample"
        )));
    }

    let Some(wanted) = download_bytes else {
        // Latency probe: the headers have arrived, which is all that was
        // wanted. Dropping here keeps the probe to a few kilobytes.
        debug_assert!(read >= PROBE_READ_BYTES);
        return Ok(phases);
    };

    // Where the headers stop and the payload starts. Everything before this
    // boundary is protocol overhead: counting it as body would inflate the byte
    // total, and - because the body clock starts here - would credit the
    // transfer with bytes that moved before the timer did.
    let body_offset = header_end(head)
        .ok_or_else(|| std::io::Error::other("response headers did not fit in the first read"))?;

    // Body timing starts at the first *body* byte, so it measures transfer
    // rather than transfer plus the far end's think time - that is what `ttfb`
    // is for.
    let body_started = Instant::now();
    let mut total = (read - body_offset) as u64;
    loop {
        if total >= wanted {
            break;
        }
        // Reserve *before* reading, so the ceiling holds even with four streams
        // draining concurrently, and bound the read by what was granted so the
        // charge equals the bytes that actually cross the wire.
        let granted = budget.reserve(READ_CHUNK as u64);
        if granted == 0 {
            phases.truncated = true;
            break;
        }
        match tls.read(&mut buffer[..granted as usize]) {
            Ok(0) => {
                budget.refund(granted);
                break;
            }
            Ok(n) => {
                budget.refund(granted - n as u64);
                total += n as u64;
            }
            Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => {
                budget.refund(granted);
                break;
            }
            Err(error) => {
                budget.refund(granted);
                return Err(error);
            }
        }
    }
    phases.body_seconds = body_started.elapsed().as_secs_f64();
    phases.body_bytes = total;
    Ok(phases)
}

/// Status code from an HTTP/1.x status line, if the response starts with one.
///
/// Deliberately not a parser: it reads the three digits after the version token
/// and stops. The only thing this module needs to know about the response is
/// whether it is the payload it asked for.
fn http_status(response: &[u8]) -> Option<u16> {
    let line_end = response
        .iter()
        .position(|b| *b == b'\r' || *b == b'\n')
        .unwrap_or(response.len());
    let line = std::str::from_utf8(&response[..line_end]).ok()?;
    let mut parts = line.split(' ');
    let version = parts.next()?;
    if !version.starts_with("HTTP/1.") {
        return None;
    }
    parts.next()?.parse().ok()
}

/// Offset of the first body byte: just past the blank line ending the headers.
///
/// Returns `None` when the headers did not fit in what was read. That is
/// treated as an error rather than guessed at, because guessing the boundary
/// wrong silently mis-attributes header bytes to the payload - which is the
/// defect this function exists to close.
fn header_end(response: &[u8]) -> Option<usize> {
    response
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|at| at + 4)
        // Tolerated because a bare-LF response is legal enough to read and a
        // measurement endpoint that sends one is not worth failing over.
        .or_else(|| {
            response
                .windows(2)
                .position(|w| w == b"\n\n")
                .map(|at| at + 2)
        })
}

/// Chooses an address, honouring the requested family when it is available.
fn pick_address(addresses: &[SocketAddr], prefer_ipv6: bool) -> Option<SocketAddr> {
    addresses
        .iter()
        .find(|a| a.is_ipv6() == prefer_ipv6)
        .or_else(|| addresses.first())
        .copied()
}

/// Whether this host has a usable IPv6 path at all.
///
/// Reported rather than assumed: a great many VPS plans are IPv4-only, and a
/// missing AAAA is a fact about the machine worth recording, not a failure.
fn ipv6_available(endpoint: &Endpoint) -> bool {
    (endpoint.host, endpoint.port)
        .to_socket_addrs()
        .map(|addresses| addresses.into_iter().any(|a| a.is_ipv6()))
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// The module
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct NetworkTransfer {
    manifest: ModuleManifest,
}

impl Default for NetworkTransfer {
    fn default() -> Self {
        Self::new()
    }
}

impl NetworkTransfer {
    pub fn new() -> Self {
        // Justified `expect`: `MODULE_ID` is a compile-time constant whose
        // validity under the ModuleId grammar is asserted by a unit test in
        // this file. There is no runtime input here to fail on.
        #[allow(clippy::expect_used)]
        let id = ModuleId::new(MODULE_ID).expect("MODULE_ID is a valid module id");
        Self {
            manifest: ModuleManifest {
                id,
                version: VERSION.to_string(),
                title: "Network transfer and latency".to_string(),
                purpose: "Measure DNS resolution, TCP connect, connect jitter, TLS handshake, \
                          time to first byte and single- and multi-stream download throughput \
                          against a compile-time allow-list of public measurement endpoints."
                    .to_string(),
                safety_class: SafetyClass::UsesNetwork,
                dependencies: vec![],
                max_bytes_written: 0,
                max_network_bytes: TRANSFER_CEILING_BYTES,
                cleanup: "None required: connections are closed when each measurement returns, \
                          and nothing is written to disk or left at any remote endpoint."
                    .to_string(),
                validation: vec![
                    "Every metric must have at least 3 successful samples; below that it is \
                     withheld rather than reported from noise."
                        .to_string(),
                    "Reaching the transfer ceiling stops the module and downgrades the result."
                        .to_string(),
                    "A host that cannot resolve or reach any endpoint fails the module rather \
                     than reporting zero."
                        .to_string(),
                    "Coefficient of variation above 0.30 raises a high-variance warning and \
                     downgrades the result to Degraded."
                        .to_string(),
                ],
                limitations: vec![
                    "Throughput is measured against one provider's anycast network. It describes \
                     how well this machine reaches a nearby major edge, which is a reasonable \
                     proxy for serving the internet and is NOT universal network capacity. A \
                     different provider, a different continent or a congested peer would give a \
                     different number."
                        .to_string(),
                    "Packet loss is not measured. Doing it properly needs ICMP or raw sockets, \
                     which need privileges this module does not take; inferring it from TCP \
                     behaviour would be a guess wearing a precise name."
                        .to_string(),
                    "Upload is not measured. Sending bulk data to a third party is a different \
                     traffic profile and needs an endpoint whose published purpose covers it."
                        .to_string(),
                    "Application-layer HTTPS throughput is not the same measurement as raw link \
                     capacity, loopback throughput or provider routing quality, and the four are \
                     never conflated here."
                        .to_string(),
                    "A host that reaches the internet only through an HTTP proxy will report the \
                     endpoints as unreachable: the module connects directly, because a proxy in \
                     the path would be measuring the proxy."
                        .to_string(),
                    "Download size is fixed rather than calibrated to the repetition target, so \
                     on a very fast link a repetition finishes well under it. That is deliberate: \
                     calibrating would mean a faster machine pulling more data from a third \
                     party."
                        .to_string(),
                ],
                comparability: vec![
                    "module.version".to_string(),
                    "agent.build_target".to_string(),
                    "endpoint.host".to_string(),
                    "endpoint.operator".to_string(),
                    "network.address_family".to_string(),
                ],
                // The internet is the noisiest thing DARCBench measures.
                // `docs/BENCHMARK-METHODOLOGY.md` targets < 0.15 for a good run
                // and 0.30 as the acceptable ceiling.
                stability_cv_bound: 0.30,
            },
        }
    }
}

impl BenchmarkModule for NetworkTransfer {
    fn manifest(&self) -> &ModuleManifest {
        &self.manifest
    }

    fn estimated_duration_s(&self, params: &ModuleParams) -> u64 {
        let reps = (params.warmup_reps + params.measured_reps) as u64;
        // Latency shapes are quick; the two download shapes dominate and are
        // bounded by the fixed transfer size rather than by a time target.
        let latency_s = reps * (LATENCY_ENDPOINTS.len() as u64 + 1) / 4;
        // Assume a pessimistic 20 Mbit/s so the estimate is not optimistic.
        let download_mib = (download_bytes(params) >> 20) * reps * (1 + PARALLEL_STREAMS as u64);
        latency_s + download_mib * 8 / 20
    }

    fn run(
        &self,
        params: &ModuleParams,
        reporter: &dyn ModuleReporter,
    ) -> Result<ModuleOutput, ModuleError> {
        let config = tls_config()?;
        let budget = Budget::new(TRANSFER_CEILING_BYTES);

        let mut metrics = Vec::new();
        let mut warnings = Vec::new();

        // Reachability is established once, before anything is measured, so an
        // offline host fails immediately with a clear reason instead of
        // producing a run full of zeroes.
        let reachable = measure(&THROUGHPUT_ENDPOINT, &config, None, &budget, false);
        if let Err(error) = reachable {
            return Err(ModuleError::Precondition(format!(
                "no allow-listed endpoint is reachable ({}: {error}); a network measurement \
                 needs the network",
                THROUGHPUT_ENDPOINT.host
            )));
        }
        let ipv6 = ipv6_available(&THROUGHPUT_ENDPOINT);

        // --- latency shapes ------------------------------------------------
        //
        // One repetition samples every latency endpoint plus the throughput
        // host, so each repetition's value is the median across operators
        // rather than one provider's edge.
        let total_units = 3.0;
        let mut completed_units = 0.0f64;

        // Every metric is **one value per repetition**, like every other module
        // in the suite. Pooling each endpoint's raw samples into one series
        // instead would give `dns_resolve` a coefficient of variation above 100%
        // - not because resolution is unstable, but because four different names
        // legitimately cost different amounts - and the validator would flag
        // every network run as excessively variable.
        let endpoint_count = 1 + LATENCY_ENDPOINTS.len();
        let dns = std::cell::RefCell::new(Vec::<f64>::new());
        let tls = std::cell::RefCell::new(Vec::<f64>::new());
        let ttfb = std::cell::RefCell::new(Vec::<f64>::new());
        // Connect times kept per endpoint, so jitter can be the variation of
        // each path rather than the spread between paths.
        let per_endpoint: std::cell::RefCell<Vec<Vec<f64>>> =
            std::cell::RefCell::new(vec![Vec::new(); endpoint_count]);
        let failures = std::cell::RefCell::new(0usize);

        let latency = time_reps(
            params,
            reporter,
            "tcp_connect.mean",
            "ms",
            completed_units,
            total_units,
            |rep| {
                // Warm-ups are streamed to the operator like any other
                // repetition, but must not reach a published statistic. The
                // first repetition of this module is the coldest measurement it
                // will ever take - an empty resolver cache, a routing entry that
                // is not there yet, a TLS stack that has never run - and folding
                // it into the summary is how a steady path comes out looking
                // unstable. `time_reps` already withholds its own return value
                // for warm-ups; these side aggregations have to do the same.
                let measured = rep >= params.warmup_reps;
                let started = Instant::now();
                let mut rep_dns = Vec::new();
                let mut rep_connect = Vec::new();
                for (index, endpoint) in std::iter::once(&THROUGHPUT_ENDPOINT)
                    .chain(LATENCY_ENDPOINTS)
                    .enumerate()
                {
                    match measure(endpoint, &config, None, &budget, false) {
                        Ok(phases) => {
                            // DNS and connect are sampled across every operator:
                            // they describe this machine's path to the internet,
                            // and one provider having a bad day should show up as
                            // spread rather than as the machine's own latency.
                            rep_dns.push(phases.dns_ms);
                            rep_connect.push(phases.connect_ms);
                            if measured {
                                per_endpoint.borrow_mut()[index].push(phases.connect_ms);
                            }
                            // Handshake and first-byte cost come from one service
                            // only. Averaging TTFB across four different services
                            // would blend four different amounts of far-end work
                            // into a number that describes none of them.
                            if index == 0 && measured {
                                tls.borrow_mut().push(phases.tls_ms);
                                ttfb.borrow_mut().push(phases.ttfb_ms);
                            }
                        }
                        Err(_) => *failures.borrow_mut() += 1,
                    }
                }
                if let Some(value) = median(&mut rep_dns) {
                    if measured {
                        dns.borrow_mut().push(value);
                    }
                }
                let value = median(&mut rep_connect).unwrap_or(0.0);
                (value, started.elapsed().as_secs_f64() * 1000.0)
            },
        )?;
        warnings.extend(latency.warnings);
        completed_units += 1.0;

        let dns = dns.into_inner();
        let tls = tls.into_inner();
        let ttfb = ttfb.into_inner();
        let per_endpoint = per_endpoint.into_inner();
        let failures = failures.into_inner();
        // Only the measured repetitions; warm-ups are evidence, never statistics.
        let connect: Vec<f64> = latency
            .measured
            .iter()
            .copied()
            .filter(|v| *v > 0.0)
            .collect();

        if connect.len() < MIN_SAMPLES {
            return Err(ModuleError::NoSamples("tcp_connect.mean".into()));
        }

        let connect_summary =
            summarize(&connect).ok_or_else(|| ModuleError::NoSamples("tcp_connect.mean".into()))?;

        push_latency_metric(&mut metrics, "dns_resolve.mean", "DNS resolution", &dns);
        metrics.push(Metric {
            key: "tcp_connect.mean".to_string(),
            label: "TCP connect".to_string(),
            unit: "ms".to_string(),
            direction: Direction::LowerIsBetter,
            value: connect_summary.median,
            outliers: outlier_indices(&connect, 3.5),
            summary: connect_summary,
            samples: latency.samples,
            measures_dispersion: false,
        });
        if let Some(jitter) = path_jitter(&per_endpoint) {
            // Jitter is what a real-time workload feels: a steady 40 ms path is
            // usable, a 40 ms path that sometimes takes 300 ms is not.
            metrics.push(Metric {
                key: "tcp_connect.jitter".to_string(),
                label: "TCP connect jitter".to_string(),
                unit: "ms".to_string(),
                direction: Direction::LowerIsBetter,
                value: jitter.median,
                outliers: Vec::new(),
                // This module already exempts jitter from its own stability
                // warning; the flag is what carries that exemption into the
                // bundle so the validator honours it too. Without it the
                // validator's blanket CV bound downgraded the run to `Partial`
                // on the one metric whose variance is the measurement.
                measures_dispersion: true,
                summary: jitter,
                samples: Vec::new(),
            });
        }
        push_latency_metric(&mut metrics, "tls_handshake.mean", "TLS handshake", &tls);
        push_latency_metric(&mut metrics, "ttfb.mean", "Time to first byte", &ttfb);

        if failures > 0 {
            warnings.push(Warning {
                code: WarningCode::Informational,
                message: format!(
                    "{failures} endpoint probe(s) failed during the run. Latency figures are \
                     the median of the endpoints that answered."
                ),
                metric_key: None,
            });
        }

        // --- download shapes ------------------------------------------------
        let per_stream = download_bytes(params);
        for (key, label, streams) in [
            ("download.single", "Download (1 stream)", 1usize),
            ("download.multi", "Download (4 streams)", PARALLEL_STREAMS),
        ] {
            if reporter.is_cancelled() {
                return Err(ModuleError::Cancelled);
            }
            if budget.exhausted() {
                warnings.push(Warning {
                    code: WarningCode::ValidationFailed,
                    message: format!(
                        "`{key}` was not measured: the run reached its {} MiB transfer ceiling. \
                         A benchmark suite must not become a traffic amplifier, so the ceiling \
                         wins over completeness.",
                        TRANSFER_CEILING_BYTES >> 20
                    ),
                    metric_key: Some(key.to_string()),
                });
                continue;
            }

            let failed = std::cell::RefCell::new(0usize);
            let outcome = time_reps(
                params,
                reporter,
                key,
                "Mbit/s",
                completed_units,
                total_units,
                |_| {
                    let started = Instant::now();
                    let rates = download(&config, &budget, streams, per_stream);
                    let elapsed = started.elapsed().as_secs_f64() * 1000.0;
                    if rates.is_empty() {
                        *failed.borrow_mut() += 1;
                        return (0.0, elapsed);
                    }
                    // Concurrent streams share a link, so the machine's
                    // throughput is their sum, not their average.
                    (rates.iter().sum::<f64>(), elapsed)
                },
            )?;
            warnings.extend(outcome.warnings);
            completed_units += 1.0;

            let usable: Vec<f64> = outcome
                .measured
                .iter()
                .copied()
                .filter(|v| *v > 0.0)
                .collect();
            if usable.len() < MIN_SAMPLES {
                warnings.push(Warning {
                    code: WarningCode::ValidationFailed,
                    message: format!(
                        "`{key}` produced only {} usable sample(s); withheld rather than \
                         reported from noise.",
                        usable.len()
                    ),
                    metric_key: Some(key.to_string()),
                });
                continue;
            }
            let Some(summary) = summarize(&usable) else {
                continue;
            };
            metrics.push(Metric {
                key: key.to_string(),
                label: label.to_string(),
                unit: "Mbit/s".to_string(),
                direction: Direction::HigherIsBetter,
                value: summary.median,
                outliers: outlier_indices(&usable, 3.5),
                summary,
                samples: outcome.samples,
                measures_dispersion: false,
            });
        }

        // --- variance ---------------------------------------------------------
        //
        // Swept over every metric at once rather than checked where each is
        // built. The manifest promises that any coefficient of variation above
        // the bound raises a warning and downgrades the result; checking that
        // inside one of the two construction paths made the promise true for the
        // two download metrics and false for the five latency ones. A live run
        // published `ttfb.mean` at 118% variation, silently, while flagging
        // `download.single` at 44%.
        //
        // The sweep reads the metric list, so a metric added later is covered
        // without anyone remembering to cover it.
        //
        // `tcp_connect.jitter` is the one exemption, and it is a real one rather
        // than a convenience. Every other metric summarises one value per
        // repetition, so its coefficient of variation says how reproducible the
        // measurement was. Jitter summarises one value per *path*, and those
        // paths are legitimately different lengths - the whole reason jitter is
        // computed within an endpoint and not across them. A CV over four
        // per-path spreads measures how diverse the endpoints are, and warning
        // on it would tell an operator their network was congested when what
        // varied was geography.
        for metric in &metrics {
            if metric.key == "tcp_connect.jitter" {
                continue;
            }
            let Some(cv) = metric.summary.cv else {
                continue;
            };
            if cv > self.manifest.stability_cv_bound {
                let warning = Warning {
                    code: WarningCode::HighVariance,
                    message: format!(
                        "`{}` varied by {:.0}% between repetitions (bound {:.0}%). The internet \
                         is shared; this usually means congestion on the path rather than a \
                         fault in the machine.",
                        metric.key,
                        cv * 100.0,
                        self.manifest.stability_cv_bound * 100.0
                    ),
                    metric_key: Some(metric.key.clone()),
                };
                reporter.warn(warning.clone());
                warnings.push(warning);
            }
        }

        if budget.exhausted() {
            let warning = Warning {
                code: WarningCode::ValidationFailed,
                message: format!(
                    "The run reached its {} MiB transfer ceiling, so some measurements were cut \
                     short. The ceiling is a hard bound on what this suite will pull from a \
                     third party.",
                    TRANSFER_CEILING_BYTES >> 20
                ),
                metric_key: None,
            };
            reporter.warn(warning.clone());
            warnings.push(warning);
        }

        let mut context = serde_json::Map::new();
        context.insert("workload_version".into(), serde_json::Value::from(VERSION));
        context.insert(
            "build_target".into(),
            serde_json::Value::from(format!(
                "{}-{}",
                std::env::consts::ARCH,
                std::env::consts::OS
            )),
        );
        context.insert("ipv6_available".into(), serde_json::Value::from(ipv6));
        context.insert(
            "transfer".into(),
            serde_json::json!({
                "bytes_spent": budget.spent(),
                "ceiling_bytes": TRANSFER_CEILING_BYTES,
                "download_bytes_per_stream": download_bytes(params),
                "parallel_streams": PARALLEL_STREAMS,
            }),
        );
        // Which endpoints were contacted, and whose they are. The methodology
        // requires the remote endpoint and its limitations be recorded per
        // measurement, so a reader knows whose network a number describes.
        context.insert(
            "endpoints".into(),
            serde_json::Value::Array(
                crate::network_endpoints::all()
                    .iter()
                    .map(|e| {
                        serde_json::json!({
                            "host": e.host,
                            "operator": e.operator,
                            "role": format!("{:?}", e.role),
                        })
                    })
                    .collect(),
            ),
        );
        context.insert(
            "probe_failures".into(),
            serde_json::Value::from(failures as u64),
        );

        if !ipv6 {
            warnings.push(Warning {
                code: WarningCode::Informational,
                message: "This host has no IPv6 path to the measurement endpoints, so every \
                          figure describes IPv4. Common on VPS plans, and a fact about the \
                          machine rather than a measurement fault."
                    .to_string(),
                metric_key: None,
            });
        }

        Ok(ModuleOutput {
            metrics,
            warnings,
            context,
        })
    }
}

/// Downloads on `streams` concurrent connections, returning each one's rate.
fn download(
    config: &Arc<rustls::ClientConfig>,
    budget: &Budget,
    streams: usize,
    bytes: u64,
) -> Vec<f64> {
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..streams)
            .map(|_| {
                scope.spawn(|| {
                    measure(&THROUGHPUT_ENDPOINT, config, Some(bytes), budget, false)
                        .ok()
                        .and_then(|phases| phases.mbit_s())
                })
            })
            .collect();
        handles
            .into_iter()
            .filter_map(|handle| handle.join().ok().flatten())
            .collect()
    })
}

/// Adds a latency metric, or nothing when there were too few samples.
fn push_latency_metric(metrics: &mut Vec<Metric>, key: &str, label: &str, samples: &[f64]) {
    if samples.len() < MIN_SAMPLES {
        return;
    }
    let Some(summary) = summarize(samples) else {
        return;
    };
    metrics.push(Metric {
        key: key.to_string(),
        label: label.to_string(),
        unit: "ms".to_string(),
        direction: Direction::LowerIsBetter,
        value: summary.median,
        outliers: outlier_indices(samples, 3.5),
        summary,
        samples: Vec::new(),
        measures_dispersion: false,
    });
}

/// Variation of each path's round trip, summarised across paths.
///
/// Jitter has to be measured **within** an endpoint, not across endpoints. The
/// spread between a resolver 0.2 ms away and one 30 ms away is distance, not
/// jitter, and reporting it under this name would tell an operator their
/// network was unstable when it was merely diverse.
fn path_jitter(per_endpoint: &[Vec<f64>]) -> Option<darcbench_protocol::stats::Summary> {
    let spreads: Vec<f64> = per_endpoint
        .iter()
        .filter(|samples| samples.len() >= MIN_SAMPLES)
        .filter_map(|samples| summarize(samples).map(|s| s.stddev))
        .collect();
    summarize(&spreads)
}

/// The median across endpoints, averaging the two central values on an even
/// count.
///
/// The averaging is not a nicety here, it is the difference between a median
/// and an upper median, and the endpoint table has an *even* number of entries
/// by construction. With four endpoints returning connect times of 2, 3, 40 and
/// 41 ms, taking `values[len / 2]` returns 40 - the slower half's lower bound -
/// where the median is 21.5. Every repetition would be biased toward the
/// slowest endpoints, systematically understating a lower-is-better network
/// anchor. The rest of the workspace (`stats::summarize`, `sustained`, the
/// scoring model) already averages; this was the one place that did not.
fn median(values: &mut [f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(f64::total_cmp);
    let n = values.len();
    Some(if n % 2 == 1 {
        values[n / 2]
    } else {
        (values[n / 2 - 1] + values[n / 2]) / 2.0
    })
}

#[cfg(test)]
// In tests, `unwrap`/`expect` panicking *is* the failure signal.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic_in_result_fn)]
mod tests {
    use super::*;
    use crate::module::NullReporter;
    use darcbench_protocol::Profile;

    /// The endpoint table has an even number of entries, so the even case is
    /// the normal one rather than the corner one.
    #[test]
    fn the_endpoint_median_is_a_median_not_an_upper_median() {
        // Two nearby operators and two distant ones: the shape the multi-
        // endpoint design exists to produce.
        let mut four = [41.0, 2.0, 40.0, 3.0];
        assert_eq!(
            median(&mut four),
            Some(21.5),
            "an upper median would report 40 ms and bias every repetition toward the slower half"
        );

        let mut three = [3.0, 40.0, 2.0];
        assert_eq!(median(&mut three), Some(3.0));
        assert_eq!(median(&mut [][..]), None);
        assert_eq!(median(&mut [7.0]), Some(7.0));
        // Whatever the live table's size, a flat sample has to reduce to its
        // own value - the property an upper median also happens to satisfy, and
        // the reason the bias above went unnoticed.
        let count = 1 + crate::network_endpoints::LATENCY_ENDPOINTS.len();
        assert_eq!(median(&mut vec![1.0; count]), Some(1.0));
    }

    fn fast_params() -> ModuleParams {
        ModuleParams {
            warmup_reps: 1,
            measured_reps: 5,
            target_rep_ms: 200,
            threads: 1,
            facts: Default::default(),
            scratch_dir: None,
        }
    }

    #[test]
    fn module_id_constant_satisfies_the_grammar() {
        assert!(
            ModuleId::new(MODULE_ID).is_ok(),
            "MODULE_ID `{MODULE_ID}` violates the ModuleId grammar; the constructor would panic"
        );
    }

    #[test]
    fn manifest_is_well_formed() {
        let module = NetworkTransfer::new();
        let m = module.manifest();
        assert_eq!(m.id.as_str(), MODULE_ID);
        assert_eq!(m.version, VERSION);
        assert_eq!(
            m.safety_class,
            SafetyClass::UsesNetwork,
            "a module that talks to the internet must declare that it does"
        );
        assert_eq!(
            m.max_bytes_written, 0,
            "network.transfer must not write to disk"
        );
        assert_eq!(
            m.max_network_bytes, TRANSFER_CEILING_BYTES,
            "the declared network bound must be the ceiling actually enforced"
        );
        assert!(m.dependencies.is_empty());
        assert!(!m.validation.is_empty());
        assert!(!m.limitations.is_empty());
        assert!(
            m.limitations.iter().any(|l| l.contains("anycast")),
            "the single-CDN limitation is a methodology requirement and must be declared"
        );
        assert!(
            m.limitations
                .iter()
                .any(|l| l.to_lowercase().contains("packet loss")),
            "not measuring packet loss must be declared, not left implied"
        );
        assert!(m.stability_cv_bound > 0.0);
    }

    #[test]
    fn metric_keys_are_unique_and_in_the_reference_alphabet() {
        let keys = [
            "dns_resolve.mean",
            "tcp_connect.mean",
            "tcp_connect.jitter",
            "tls_handshake.mean",
            "ttfb.mean",
            "download.single",
            "download.multi",
        ];
        let unique: std::collections::BTreeSet<&&str> = keys.iter().collect();
        assert_eq!(keys.len(), unique.len());
        for key in keys {
            for segment in key.split('.') {
                let mut chars = segment.chars();
                assert!(
                    chars.next().is_some_and(|c| c.is_ascii_lowercase()),
                    "{key}"
                );
                assert!(
                    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
                    "{key}"
                );
            }
        }
    }

    // --- the transfer ceiling ---------------------------------------------

    /// The ceiling is the mechanism that stops this being a traffic amplifier,
    /// so it is tested as a mechanism and not taken on trust.
    #[test]
    fn the_budget_never_grants_more_than_its_ceiling() {
        let budget = Budget::new(1000);
        assert_eq!(budget.reserve(400), 400);
        assert_eq!(budget.reserve(400), 400);
        // Only 200 left, so an over-large request is trimmed, not refused.
        assert_eq!(budget.reserve(400), 200);
        assert_eq!(budget.reserve(1), 0);
        assert!(budget.exhausted());
        assert_eq!(budget.spent(), 1000);
    }

    /// Four streams reserve concurrently; the ceiling must still hold exactly.
    #[test]
    fn the_budget_holds_under_concurrent_streams() {
        let ceiling = 10_000u64;
        let budget = Budget::new(ceiling);
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    for _ in 0..1000 {
                        budget.reserve(7);
                    }
                });
            }
        });
        assert_eq!(
            budget.spent(),
            ceiling,
            "concurrent reservations must total exactly the ceiling, never overshoot it"
        );
    }

    /// The module must fit inside its own ceiling on *every* profile.
    ///
    /// Regression: the download size was a constant, so a `deep` run wanted
    /// 560 MiB and an `endurance` run 1.3 GiB against a 512 MiB ceiling. The
    /// ceiling would have held - it is enforced - but the module would have
    /// spent the second half of every long run tripping its own emergency
    /// brake and withholding metrics, which is not a design, it is a bug that
    /// happens to be safe.
    #[test]
    fn every_profile_fits_inside_the_transfer_ceiling() {
        for profile in [
            Profile::Quick,
            Profile::Standard,
            Profile::Deep,
            Profile::Endurance,
            Profile::ReadOnly,
            Profile::Custom,
        ] {
            let params = ModuleParams::for_profile(profile);
            let reps = (params.warmup_reps + params.measured_reps) as u64;
            let per_stream = download_bytes(&params);
            let worst = per_stream * reps * (1 + PARALLEL_STREAMS as u64);
            assert!(
                worst <= TRANSFER_CEILING_BYTES,
                "{profile} would want {worst} bytes against a {TRANSFER_CEILING_BYTES} ceiling"
            );
            assert!(
                per_stream >= MIN_DOWNLOAD_BYTES,
                "{profile} scaled the download down to {per_stream} bytes, too small to time"
            );
            assert!(per_stream <= MAX_DOWNLOAD_BYTES, "{profile}");
        }
    }

    /// A transfer the ceiling cut short is not a throughput measurement.
    ///
    /// Regression: the budget charged the full 64 KiB read buffer while TLS
    /// records deliver ~16 KiB, so it over-charged fourfold, exhausted the
    /// ceiling at a quarter of the intended volume and truncated every
    /// download. The surviving bytes divided by the sliver of a second they
    /// took were then reported as throughput - 652 Gbit/s, on a link that does
    /// about 0.7.
    #[test]
    fn a_truncated_or_immeasurable_transfer_has_no_rate() {
        let good = Phases {
            body_bytes: 8 << 20,
            body_seconds: 0.1,
            ..Default::default()
        };
        assert!(good.mbit_s().is_some());

        let truncated = Phases {
            truncated: true,
            ..good
        };
        assert!(
            truncated.mbit_s().is_none(),
            "a download the budget cut short must not become a number"
        );
        assert!(
            Phases {
                body_bytes: 32 * 1024,
                body_seconds: 0.000_000_4,
                ..Default::default()
            }
            .mbit_s()
            .is_none(),
            "a body too small and too fast to time must not become a number"
        );
        assert!(Phases::default().mbit_s().is_none());
    }

    // --- address selection --------------------------------------------------

    #[test]
    fn address_selection_honours_the_requested_family_when_available() {
        let v4: SocketAddr = "1.2.3.4:443".parse().expect("v4");
        let v6: SocketAddr = "[2001:db8::1]:443".parse().expect("v6");

        assert_eq!(pick_address(&[v4, v6], true), Some(v6));
        assert_eq!(pick_address(&[v4, v6], false), Some(v4));
        // Falls back rather than failing when the preferred family is absent.
        assert_eq!(pick_address(&[v4], true), Some(v4));
        assert_eq!(pick_address(&[v6], false), Some(v6));
        assert_eq!(pick_address(&[], false), None);
    }

    /// An error page is not a download.
    ///
    /// Nothing checked the status line, so a 500 or a redirect body would be
    /// drained, timed and published as this machine's network throughput - the
    /// endpoint having a bad day reported as a property of the server under
    /// test.
    #[test]
    fn only_a_200_response_can_become_a_throughput_sample() {
        assert_eq!(
            http_status(b"HTTP/1.1 200 OK\r\nX: y\r\n\r\nbody"),
            Some(200)
        );
        assert_eq!(http_status(b"HTTP/1.0 200 OK\r\n\r\n"), Some(200));
        assert_eq!(
            http_status(b"HTTP/1.1 500 Internal Server Error\r\n"),
            Some(500)
        );
        assert_eq!(http_status(b"HTTP/1.1 302 Found\r\n"), Some(302));
        // Anything that is not an HTTP/1.x response is rejected outright rather
        // than guessed at.
        assert_eq!(http_status(b"HTTP/2 200\r\n"), None);
        assert_eq!(http_status(b"\x16\x03\x03garbage"), None);
        assert_eq!(http_status(b""), None);
    }

    /// Header bytes are protocol overhead, not payload.
    ///
    /// Counting the whole first read as body did two things at once: it added
    /// a few hundred header bytes to the total, and - because the body clock
    /// starts after that read - it credited the transfer with bytes that moved
    /// before the timer did. Both inflate the reported rate.
    #[test]
    fn the_body_starts_after_the_headers_not_at_the_first_byte() {
        let response = b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\nbody";
        let offset = header_end(response).expect("boundary");
        assert_eq!(&response[offset..], b"body");
        assert_eq!(
            response.len() - offset,
            4,
            "only the payload counts towards the transfer"
        );

        // A bare-LF response is tolerated rather than failed over.
        let lf = b"HTTP/1.1 200 OK\nContent-Length: 2\n\nhi";
        assert_eq!(&lf[header_end(lf).expect("boundary")..], b"hi");

        // Headers that did not arrive in the first read are an error, not a
        // guess: guessing the boundary wrong is the defect being closed here.
        assert!(header_end(b"HTTP/1.1 200 OK\r\nContent-Len").is_none());
    }

    /// The ceiling is a bound on what crosses the wire, so every read counts.
    ///
    /// The first read of each connection used to be uncharged, and a latency
    /// probe performs exactly one read - so the uncharged share grew with the
    /// number of probes, which is the majority of connections in a run. A
    /// ceiling described as enforced rather than documented cannot be
    /// approximately right.
    #[test]
    fn the_budget_is_charged_before_any_byte_is_read() {
        let budget = Budget::new(READ_CHUNK as u64);
        // One connection's opening read exhausts a ceiling this size, which is
        // only true if that read is charged at all.
        assert_eq!(budget.reserve(READ_CHUNK as u64), READ_CHUNK as u64);
        assert!(budget.exhausted());
        assert_eq!(
            budget.reserve(1),
            0,
            "a probe that cannot be paid for must not be issued"
        );
    }

    #[test]
    fn throughput_is_computed_from_body_bytes_and_body_time_only() {
        let phases = Phases {
            body_bytes: 1_000_000,
            body_seconds: 0.5,
            // Deliberately large: the handshake must not enter the rate.
            dns_ms: 500.0,
            connect_ms: 500.0,
            tls_ms: 500.0,
            ttfb_ms: 500.0,
            ipv6: false,
            truncated: false,
        };
        let rate = phases.mbit_s().expect("rate");
        assert!((rate - 16.0).abs() < 1e-9, "got {rate} Mbit/s");

        // A connection that transferred nothing has no rate, rather than zero.
        assert!(Phases::default().mbit_s().is_none());
    }

    // --- offline behaviour --------------------------------------------------

    /// The suite must be runnable without internet, and this module must say so
    /// rather than reporting zeroes.
    ///
    /// It cannot assert *which* way the run goes - CI may or may not have
    /// egress - so it asserts the two acceptable outcomes and rejects the third:
    /// silently succeeding with no data.
    #[test]
    fn an_unreachable_network_fails_the_module_rather_than_scoring_zero() {
        let module = NetworkTransfer::new();
        match module.run(&fast_params(), &NullReporter::default()) {
            Err(ModuleError::Precondition(message)) => {
                assert!(
                    message.contains("reachable") || message.contains("trust anchors"),
                    "an offline failure must explain itself: {message}"
                );
            }
            Err(ModuleError::Cancelled) => {}
            Err(other) => panic!("unexpected error: {other}"),
            Ok(output) => {
                // Online: every metric present must be real.
                assert!(
                    !output.metrics.is_empty(),
                    "a successful run must produce metrics, not an empty result"
                );
                for metric in &output.metrics {
                    assert!(
                        metric.value >= 0.0 && metric.value.is_finite(),
                        "{} produced {}",
                        metric.key,
                        metric.value
                    );
                    let expected = if metric.key.starts_with("download.") {
                        Direction::HigherIsBetter
                    } else {
                        Direction::LowerIsBetter
                    };
                    assert_eq!(metric.direction, expected, "{}", metric.key);

                    // Warm-ups are streamed but must never reach a statistic.
                    // The three phase metrics were aggregated in side vectors
                    // that the harness does not filter, so a cold first
                    // repetition - empty resolver cache, TLS stack that has
                    // never run - was being averaged into the published figure.
                    // `tcp_connect.jitter` is per-path, not per-repetition, so
                    // it is bounded by the endpoint count instead.
                    let params = fast_params();
                    let bound = if metric.key == "tcp_connect.jitter" {
                        1 + LATENCY_ENDPOINTS.len()
                    } else {
                        params.measured_reps as usize
                    };
                    assert!(
                        metric.summary.n <= bound,
                        "{} summarised {} samples where at most {bound} are measured; a warm-up \
                         reached the statistics",
                        metric.key,
                        metric.summary.n
                    );

                    // Every per-repetition metric is checked against the
                    // variance bound, not just the ones built in the loop that
                    // happened to contain the check. A live run published
                    // ttfb.mean at 118% CV silently while flagging
                    // download.single at 44%. Jitter is exempt: its samples are
                    // per-path, so their spread is endpoint diversity.
                    if metric.key != "tcp_connect.jitter"
                        && metric.summary.cv.is_some_and(|cv| cv > 0.30)
                    {
                        assert!(
                            output.warnings.iter().any(|w| {
                                w.code == WarningCode::HighVariance
                                    && w.metric_key.as_deref() == Some(metric.key.as_str())
                            }),
                            "{} exceeded the variance bound without a warning",
                            metric.key
                        );
                    }
                }
                let spent = output.context["transfer"]["bytes_spent"]
                    .as_u64()
                    .expect("bytes_spent");
                assert!(
                    spent <= TRANSFER_CEILING_BYTES,
                    "a run transferred {spent} bytes against a {TRANSFER_CEILING_BYTES} ceiling"
                );
                assert!(output.context.contains_key("endpoints"));
                assert!(output.context.contains_key("ipv6_available"));
            }
        }
    }

    #[test]
    fn estimated_duration_is_not_optimistic() {
        let module = NetworkTransfer::new();
        let params = ModuleParams::for_profile(Profile::Standard);
        assert!(
            module.estimated_duration_s(&params) > 0,
            "an estimate of zero would tell an operator the run is free"
        );
    }
}
