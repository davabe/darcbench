# ADR-0010: Stateless containerised control plane, deferred to Phase 5

**Status:** Accepted · **Date:** 2026-08-03

## Context

The control plane adds accounts, history, comparison, fleets, verification and
leaderboards. It is also the component most likely to be built prematurely, and
the one whose absence blocks nothing in standalone mode.

## Decision

**Defer to Phase 5.** Standalone mode is fully useful without it, and building
it before the benchmark modules exist would mean designing storage for data we
cannot yet produce.

The shape is decided now so nothing built earlier blocks it:

- **Stateless HTTP service** in Rust (reusing `darcbench-scoring` and
  `darcbench-report` unchanged), horizontally scalable.
- **PostgreSQL** for entities and derived scores; **S3-compatible object
  storage** for immutable raw bundles.
- **Queue** (PostgreSQL-backed initially) for validation and rescoring jobs;
  rescoring a corpus under a new model is a batch job, not a request.
- **OpenTelemetry** traces and metrics.
- **Container images**, deployable to any orchestrator. No managed-service
  dependency that would prevent self-hosting, because the AGPL promise in
  ADR-0009 is meaningless if self-hosting is impractical.

**Agent-to-control-plane:** the agent initiates every connection. The control
plane never connects inward to an agent. Remote runs are a job the agent picks
up, not a command pushed to it — which is what keeps "no unauthenticated remote
command execution" true by construction even in connected mode.

## Consequences

- Result upload must be resumable; a bundle from a deep run can be large and
  server links are unreliable.
- Score recomputation must be a batch job from day one.
- Multi-tenancy and row-level authorisation must be designed in, not retrofitted.

## Revisit if

Phase 2–4 reveal a data volume or query shape PostgreSQL handles badly, or
providers demand on-premise verification, which would change the trust model.
