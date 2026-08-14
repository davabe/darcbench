# ADR-0004: Server-Sent Events, not WebSocket

**Status:** Accepted · **Date:** 2026-08-03

## Context

A run emits an ordered stream of events for minutes to hours. The browser may
reconnect mid-run. The connection may traverse an SSH tunnel, a corporate proxy
or a hosting panel's reverse proxy.

## Decision

Server-Sent Events over HTTP. Control actions (start, cancel) are ordinary HTTP
requests. Each event carries a monotonic `seq`, sent as the SSE `id:` field.

## Rationale

1. **The traffic is one-directional.** Agent to browser, overwhelmingly. A
   bidirectional transport buys nothing here.
2. **Replay is built into the standard.** The browser reconnects on its own with
   `Last-Event-ID`; the agent replays from its buffer. With WebSocket, we would
   implement reconnection, resume and heartbeat ourselves — and get them subtly
   wrong.
3. **It survives middleboxes.** WebSocket upgrades are broken by more proxies
   than a long-lived HTTP response is, and DARCBench is specifically designed to
   be reachable through whatever proxy is already in front of the server.
4. **Backpressure has an honest answer.** A lagging consumer ends the stream
   rather than being silently truncated, forcing a reconnect that can actually
   recover the gap.

## Alternatives

**WebSocket.** Rejected above. Reconsider if bidirectional low-latency control
becomes necessary — for instance interactive load-generator steering.

**Long polling.** Rejected: worse latency and more connection churn for no
compatibility gain over SSE.

**gRPC streaming.** Rejected for the browser leg; browsers need grpc-web and a
proxy. Reconsider for the agent-to-control-plane leg in Phase 5.

## Consequences

- The replay buffer is bounded (4096 events). A client that falls further behind
  is told to refetch rather than handed an undetectable gap.
- SSE is text; every event is JSON. Fine at our volumes, and worth the
  debuggability.
- TypeScript event types are hand-maintained against the Rust enum. A CI parity
  check fails if a kind exists in Rust but not in `EVENT_KINDS`. Generation from
  the Rust types is deferred until the protocol stops moving.
