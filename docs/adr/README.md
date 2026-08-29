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
| [0014](0014-reverse-proxy-integration.md) | Reverse-proxy integration writes an inert file, never a live one | Accepted |
| [0015](0015-two-product-lines-one-engine.md) | Two product lines, one measurement engine, two scoring models | Accepted |
| [0016](0016-client-reference-darc-ref-c1.md) | DARC-REF-C1 as the client reference; one host calibrates a `-dev` model, never a 1.0 | Accepted |
| [0017](0017-engine-shell-process-separation.md) | The client UI is a separate process, silent while measuring | Accepted |
| [0018](0018-gpu-compute-api.md) | GPU API: wgpu on native backends, pending a backend-variance measurement | **Proposed** |
