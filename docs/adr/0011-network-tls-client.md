# ADR-0011: rustls with the ring provider and the host trust store, for the network module

**Status:** Accepted · **Date:** 2026-08-06

## Context

`network.transfer` (Phase 2) has to report DNS resolution, TCP connect, **TLS
negotiation**, TTFB and download throughput as separate numbers. That rules out
a high-level HTTP client, which hides exactly the phase boundaries the module
exists to measure, and it rules out plain HTTP, because no public measurement
endpoint serves it any more.

So the module needs a TLS implementation it can drive itself: connect a
`TcpStream`, time the handshake, write a minimal HTTP/1.1 request, time to first
byte, then drain the body and time the transfer.

Three constraints pull on that choice:

1. `unsafe_code = "forbid"` across the workspace.
2. `deny.toml` allows only permissive licences — Apache-2.0, MIT, BSD-2/3-Clause,
   ISC, Unicode-3.0, Zlib.
3. The product ships as a single static binary that is copied to a server and
   run. Nothing may be needed at runtime.

Before this change the workspace was pure Rust and needed no C toolchain to
build, which is a property worth naming even though it is not a stated
requirement.

## Decision

Add **`rustls` 0.23 with `default-features = false` and the `ring` provider**,
plus **`rustls-native-certs`** for the trust anchors.

Two sub-decisions that are easy to get wrong by accepting defaults:

**`ring`, not `aws-lc-rs`.** `aws-lc-rs` is rustls' default provider and pulls
in `aws-lc-sys`, which requires **cmake** at build time. `ring` needs only `cc`.
Both compile into the binary, so the static-binary property is unaffected
either way; the difference is entirely in what a contributor must have installed.

**`rustls-native-certs`, not `webpki-roots`.** Two independent reasons, either
sufficient. `webpki-roots` is **MPL-2.0**, which is outside the `deny.toml`
allow-list. And a tool that runs on someone else's server should trust the CAs
*that machine's administrator* trusts — including a corporate internal CA —
rather than a snapshot of Mozilla's bundle frozen at the time the binary was
compiled. Reading the host trust store is also the behaviour an operator would
expect from anything else on their machine.

## Consequences

- Building DARCBench now requires a C compiler for `ring`'s assembly. It did
  not before. This is a build-time cost only; there is no new runtime
  dependency and the binary stays statically linked.
- Seventeen transitive crates are added. Every licence was checked against
  `deny.toml`: `rustls` (Apache-2.0 OR ISC OR MIT), `ring` (Apache-2.0 AND ISC),
  `rustls-webpki` (ISC), `rustls-pki-types` (MIT OR Apache-2.0),
  `rustls-native-certs` (Apache-2.0 OR ISC OR MIT), `subtle` (BSD-3-Clause),
  `untrusted` (ISC), `zeroize` (Apache-2.0 OR MIT), `openssl-probe`
  (MIT OR Apache-2.0).
- A host with an empty or unreadable trust store cannot run the network module.
  That is reported as a precondition failure rather than worked around by
  falling back to a bundled set — silently trusting different roots than the
  administrator configured would be worse than not running.

## Alternatives considered

**A high-level client (`ureq`, `reqwest`).** Rejected: they collapse DNS, TCP,
TLS and TTFB into one call, and those four numbers are the deliverable. `reqwest`
would additionally drag async and a second runtime into a crate that runs inside
`spawn_blocking`.

**`native-tls` / OpenSSL.** Rejected: a runtime dependency on the host's OpenSSL
directly contradicts the single-static-binary requirement, and the version
present would silently change what "TLS handshake time" means between machines.

**Ship without TLS — DNS, TCP connect and jitter only.** Genuinely tempting,
because it needs no new dependency at all and those three are useful. Rejected
because the deliverable is called `network.transfer`: throughput is the number
buyers actually compare, and a Network category without it would be a latency
category wearing the wrong name.

**A pure-Rust crypto provider (`rustls-rustcrypto`).** Rejected: not recommended
for production by its own authors, and its constant-time properties are less
scrutinised than ring's. Revisit if it matures — it would restore the
no-C-toolchain property.

## Revisit when

- `rustls-rustcrypto` or an equivalent pure-Rust provider becomes production
  recommended.
- Reproducible builds (Phase 8) turn out to be materially harder because of
  `ring`'s assembly.
- The module needs HTTP/2 or HTTP/3 timing, at which point a protocol library
  becomes worth its weight and this decision should be re-taken as a whole.
