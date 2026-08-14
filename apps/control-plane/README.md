# DARCBench control plane

**Not started. Phase 5.** This directory is a placeholder so the repository
layout matches the architecture; there is no code here yet, deliberately.

## Why it does not exist yet

Standalone mode is fully useful without it, and building storage for results the
benchmark modules cannot yet produce would be designing against imagined data.
The shape is decided in [ADR-0010](../../docs/adr/0010-control-plane-deployment.md)
so that nothing built in Phases 1-4 blocks it.

## What it will be

- **Stateless HTTP service in Rust**, reusing `darcbench-scoring` and
  `darcbench-report` unchanged. That reuse is the point: the server recomputes
  scores with exactly the code the agent used, so a mismatch means tampering
  rather than a version skew.
- **PostgreSQL** for entities and derived scores; **S3-compatible object
  storage** for immutable raw bundles.
- **PostgreSQL-backed queue** for validation and rescoring. Rescoring a corpus
  under a new scoring model is a batch job, not a request.
- **OpenTelemetry** traces and metrics.
- **Container images**, deployable to any orchestrator, with no managed-service
  dependency - the AGPL promise in [ADR-0009](../../docs/adr/0009-licensing.md)
  is meaningless if self-hosting is impractical.

## The security property that shapes it

**The agent initiates every connection.** The control plane never connects
inward to an agent. A remote run is a job the agent picks up, not a command
pushed to it - which is what keeps "no unauthenticated remote command execution"
true by construction even in connected mode.

## Licence

AGPL-3.0, unlike the Apache-2.0 agent. See
[LICENSE-STRATEGY.md](../../LICENSE-STRATEGY.md).
