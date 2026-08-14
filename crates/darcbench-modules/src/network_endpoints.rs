//! The network endpoint allow-list.
//!
//! # This table is a security boundary
//!
//! `docs/THREAT-MODEL.md` (T-DDOS) is binding and permanent:
//!
//! > A benchmark suite that lets you point a load generator at an arbitrary URL
//! > is a DDoS tool with a scoring model. [...] network endpoints come from a
//! > compile-time allow-list [...] There will be no "benchmark this URL"
//! > feature. This is a permanent product constraint, not a backlog item.
//!
//! So this file is a `const` table and nothing else. There is no constructor
//! that takes a host, no environment variable, no configuration file and no API
//! field anywhere in DARCBench that reaches it. Adding an endpoint is a code
//! change, a review and a release - which is the point, because it forces
//! somebody to answer the question below for every host added.
//!
//! # What has to be true of an endpoint before it goes in this table
//!
//! 1. **The operator publishes it for measurement, or the traffic is
//!    negligible.** A throughput endpoint must be a service whose stated
//!    purpose is speed measurement. A timing-only probe is a TCP and TLS
//!    handshake - a few kilobytes - against infrastructure built to absorb
//!    public request volume.
//! 2. **The volume is bounded and stated.** [`TRANSFER_CEILING_BYTES`] is
//!    enforced by the module, not merely documented.
//! 3. **The reason it is here is written down.** Each entry carries its
//!    operator, its purpose and why the traffic is acceptable, in this file,
//!    next to the entry.

/// Hard ceiling on bytes this module may transfer in a single run.
///
/// Enforced by the module against a running total, so it holds across
/// calibration, warm-ups and every repetition together. The methodology's
/// requirement is blunt - *"a benchmark suite must not become a traffic
/// amplifier"* - and a documented intention is not a ceiling. This is.
pub const TRANSFER_CEILING_BYTES: u64 = 512 << 20;

// Bounded at compile time rather than in a test, because these are properties of
// a constant and there is no reason to let a build that violates them exist at
// all. Moving the ceiling outside this window is a decision somebody has to take
// deliberately, here, by editing the bound as well as the value.
//
// A single 1 Gbit/s link saturated for a minute moves about 7 GiB, so an upper
// bound of one gigabyte keeps a whole run to a few seconds of one connection -
// a rounding error against what a public measurement service handles. The lower
// bound exists because a ceiling too small to fund four concurrent streams would
// silently turn every download into a truncated transfer, which the module
// correctly refuses to convert into a rate: the Network category would go quiet
// rather than go wrong, but it would still go quiet.
const _: () = assert!(
    TRANSFER_CEILING_BYTES <= 1 << 30,
    "a benchmark that can pull a gigabyte per run from a third party is an amplifier"
);
const _: () = assert!(
    TRANSFER_CEILING_BYTES >= 64 << 20,
    "a ceiling this low cannot fund a credible multi-stream measurement"
);

/// What an endpoint is used for, which decides how much traffic it sees.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// Full phase timing plus bulk download. Only for services whose published
    /// purpose is measurement.
    Throughput,
    /// DNS, TCP connect and TLS handshake only - a few kilobytes. Used so that
    /// latency and jitter are not a single operator's anycast network.
    LatencyProbe,
}

/// One allow-listed endpoint.
#[derive(Clone, Copy, Debug)]
pub struct Endpoint {
    /// Hostname. A compile-time constant, never assembled from anything.
    pub host: &'static str,
    pub port: u16,
    /// Request path. For [`Role::Throughput`] it must accept a byte count.
    pub path: &'static str,
    /// Who runs it. Recorded per measurement so a reader knows whose network
    /// a number describes.
    pub operator: &'static str,
    pub role: Role,
    /// Why this host is in the table at all.
    pub justification: &'static str,
}

impl Endpoint {
    /// The throughput request path for `bytes`.
    ///
    /// The only formatting this module ever does into a request line, and the
    /// only input is a `u64` this module computed and clamped itself. No
    /// caller-supplied value reaches a URL anywhere in DARCBench.
    pub fn download_path(&self, bytes: u64) -> String {
        format!("{}{bytes}", self.path)
    }

    /// The path a timing-only probe requests.
    ///
    /// A [`Role::Throughput`] endpoint's `path` is a prefix expecting a byte
    /// count, so using it unmodified sends `/__down?bytes=` with nothing after
    /// the `=`. That is a malformed request, and Cloudflare rightly answers
    /// `400` - which went unnoticed for as long as nothing checked the status,
    /// because a rejection still costs a full DNS, TCP, TLS and round trip and
    /// so still produced plausible-looking timings. It was measuring how fast
    /// the endpoint says no.
    ///
    /// Zero bytes rather than one: it is a valid request for the smallest
    /// possible payload, so the response is a real `200` on the same code path
    /// a download uses, and it adds nothing to the transfer ceiling.
    pub fn probe_path(&self) -> String {
        match self.role {
            Role::Throughput => self.download_path(0),
            Role::LatencyProbe => self.path.to_string(),
        }
    }
}

/// Endpoint used for bulk transfer.
///
/// Cloudflare's speed test service. This is the endpoint that
/// <https://speed.cloudflare.com> itself drives from a browser: its stated
/// purpose is measuring a client's connection, and `__down?bytes=N` returns N
/// bytes of padding for exactly that. Traffic is bounded by
/// [`TRANSFER_CEILING_BYTES`].
///
/// **It is one provider's anycast network.** That is a real limitation, not a
/// footnote: the number describes how well this machine reaches the nearest
/// Cloudflare edge, which is a good proxy for "how well does this server serve
/// the internet" and is *not* universal capacity. The module declares this in
/// its limitations and the report repeats it, which is what
/// `docs/BENCHMARK-METHODOLOGY.md` requires:
/// *"One CDN endpoint does not represent universal network capacity, and the
/// report says so."*
pub const THROUGHPUT_ENDPOINT: Endpoint = Endpoint {
    host: "speed.cloudflare.com",
    port: 443,
    path: "/__down?bytes=",
    operator: "Cloudflare",
    role: Role::Throughput,
    justification: "Public speed-test service; `__down` exists to return a requested number of \
                    bytes for client measurement. Volume bounded by the module's transfer ceiling.",
};

/// Endpoints probed for latency, jitter and handshake cost only.
///
/// Each sees a TCP handshake and a TLS handshake per sample - a few kilobytes
/// against anycast infrastructure built to serve public query volume at
/// internet scale. They exist so that latency and jitter are not measured
/// against a single operator: if one provider's edge is having a bad day, the
/// spread across three makes that visible instead of it looking like the
/// machine's own network.
pub const LATENCY_ENDPOINTS: &[Endpoint] = &[
    Endpoint {
        host: "one.one.one.one",
        port: 443,
        path: "/",
        operator: "Cloudflare",
        role: Role::LatencyProbe,
        justification: "Public DNS resolver on documented anycast addresses. Handshake-only \
                        probe; no queries are issued and no name is looked up through it.",
    },
    Endpoint {
        host: "dns.google",
        port: 443,
        path: "/",
        operator: "Google Public DNS",
        role: Role::LatencyProbe,
        justification: "Public DNS resolver, documented for unauthenticated public use. \
                        Handshake-only probe.",
    },
    Endpoint {
        host: "dns.quad9.net",
        port: 443,
        path: "/",
        operator: "Quad9",
        role: Role::LatencyProbe,
        justification: "Public DNS resolver run by a non-profit for open public use. \
                        Handshake-only probe.",
    },
];

/// Every endpoint this build can contact, for disclosure in reports.
pub fn all() -> Vec<Endpoint> {
    let mut endpoints = vec![THROUGHPUT_ENDPOINT];
    endpoints.extend_from_slice(LATENCY_ENDPOINTS);
    endpoints
}

#[cfg(test)]
// In tests, `unwrap`/`expect` panicking *is* the failure signal.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic_in_result_fn)]
mod tests {
    use super::*;

    /// The allow-list must stay an allow-list.
    ///
    /// Every entry is a compile-time constant with a stated operator and a
    /// stated justification. A blank justification means somebody added a host
    /// without answering why it is acceptable to send it traffic.
    #[test]
    fn every_endpoint_is_declared_and_justified() {
        for endpoint in all() {
            assert!(!endpoint.host.is_empty());
            assert!(
                !endpoint.operator.is_empty(),
                "{}: an endpoint must name its operator so a report can say whose network a \
                 number describes",
                endpoint.host
            );
            assert!(
                endpoint.justification.len() > 40,
                "{}: an endpoint needs a real justification, not a placeholder",
                endpoint.host
            );
            assert_eq!(
                endpoint.port, 443,
                "{}: every endpoint is HTTPS; a plaintext probe would measure a different thing",
                endpoint.host
            );
            // A hostname, not a URL. Anything with a scheme or a path in the
            // host field would mean somebody was assembling requests here.
            assert!(
                !endpoint.host.contains('/') && !endpoint.host.contains(':'),
                "{}: the host field is a hostname, not a URL",
                endpoint.host
            );
        }
    }

    #[test]
    fn only_the_declared_throughput_endpoint_carries_bulk_traffic() {
        assert_eq!(THROUGHPUT_ENDPOINT.role, Role::Throughput);
        for endpoint in LATENCY_ENDPOINTS {
            assert_eq!(
                endpoint.role,
                Role::LatencyProbe,
                "{}: bulk traffic goes to the speed-test service only",
                endpoint.host
            );
        }
        assert_eq!(
            all().iter().filter(|e| e.role == Role::Throughput).count(),
            1
        );
    }

    #[test]
    fn latency_probes_span_more_than_one_operator() {
        let operators: std::collections::BTreeSet<&str> =
            LATENCY_ENDPOINTS.iter().map(|e| e.operator).collect();
        assert!(
            operators.len() >= 2,
            "latency measured against a single operator is that operator's anycast network, \
             not this machine's connectivity"
        );
    }

    #[test]
    fn the_download_path_only_ever_carries_a_byte_count() {
        let path = THROUGHPUT_ENDPOINT.download_path(1_048_576);
        assert_eq!(path, "/__down?bytes=1048576");
        // No caller-supplied text can reach the request line: the only input is
        // an integer this module computed.
        assert!(!path.contains(' ') && !path.contains('\r') && !path.contains('\n'));
    }

    // The transfer ceiling's bounds are asserted at compile time next to the
    // constant itself, so a build that violates them does not exist. What the
    // ceiling does at run time - trimming an over-large request, holding exactly
    // under concurrent streams - is tested in `network_transfer.rs`, where the
    // budget that enforces it lives.
}
