# Architecture Decision Records

Each ADR states a decision, the forces behind it, the alternatives considered
and what would make us revisit it. An ADR is never deleted; it is superseded.

| ADR | Decision | Status |
|---|---|---|
| [0001](0001-agent-language.md) | Agent and workloads in Rust | Accepted |
| [0002](0002-repository-structure.md) | Single monorepo, Cargo + pnpm workspaces | Accepted |
| [0003](0003-local-web-architecture.md) | Embedded SPA served by the agent, token auth | Accepted |
| [0004](0004-realtime-transport.md) | Server-Sent Events, not WebSocket | Accepted |
| [0005](0005-database-and-storage.md) | Files now, SQLite standalone, PostgreSQL + object storage for the control plane | Accepted |
| [0006](0006-module-isolation.md) | Compile-time module registry; tiered isolation as modules grow | Accepted |
| [0007](0007-scoring-versioning.md) | Versioned scoring model over retained raw evidence | Accepted |
| [0008](0008-result-verification.md) | Ed25519 over canonical JSON, plus server-side recomputation | Accepted |
| [0009](0009-licensing.md) | Apache-2.0 core, AGPL-3.0 control plane (open core) | Accepted |
| [0010](0010-control-plane-deployment.md) | Containerised stateless control plane, deferred to Phase 5 | Accepted |
| [0011](0011-network-tls-client.md) | rustls with the ring provider and the host trust store, for the network module | Accepted |
| [0012](0012-load-generation.md) | In-process open-model load generator; saturation judged by the schedule, not by CPU | Accepted |
| [0013](0013-executing-a-discovered-runtime.md) | Runtime modules execute the operator's interpreter, under a path allow-list and a safe-path check | Accepted |
