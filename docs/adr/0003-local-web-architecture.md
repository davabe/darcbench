# ADR-0003: The dashboard is a single-page app embedded in the agent binary

**Status:** Accepted · **Date:** 2026-08-03

## Context

The operator is usually on a different machine from the server, reaching it over
SSH. They need a real-time view. The server may already run nginx, Apache,
Plesk or cPanel on ports 80 and 443, and DARCBench must not disturb any of it.

## Decision

The agent embeds the built SPA in its own binary and serves it from a
DARCBench-owned port, bound to `127.0.0.1` by default, behind a 256-bit token.

`crates/darcbench-agent/build.rs` walks `apps/web/dist` and emits an
`include_bytes!` table. When that directory does not exist, the table is empty
and the agent serves a small built-in console instead — so a Rust-only checkout,
a minimal container image and a distribution package all still produce a working
agent.

Authentication has two capability levels: the token in an `Authorization` header
may mutate; the token in a session cookie or query string is read-only. That is
the CSRF defence, and it is why `EventSource` (which cannot set headers) is
allowed to authenticate with a cookie.

## Alternatives

**Serve from an existing web server.** Rejected as the default: it requires
editing someone's production vhost. Offered later as an explicit, previewed,
backed-up, roll-back-able opt-in (`docs/INSTALLER-AND-DISCOVERY.md`).

**Ship assets on disk next to the binary.** Rejected: it turns a one-file `scp`
into a deployment.

**Terminal UI only.** Rejected: real-time charts, comparisons and shareable
reports are core product value, and a TUI over a laggy SSH session is worse than
a browser for exactly the moment that matters.

**No auth on loopback.** Rejected. On a shared host, "loopback" includes every
other tenant.

## Consequences

- Rebuilding the UI requires rebuilding the agent. Acceptable; `pnpm dev` proxies
  to a running agent for UI work.
- Binary size grows by roughly the bundle size (~220 KB today).
- CSP can stay at `script-src 'self'` with no inline-script exemption, because
  even the fallback console loads its script from its own route.
