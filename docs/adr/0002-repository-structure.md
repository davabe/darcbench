# ADR-0002: Single monorepo with Cargo and pnpm workspaces

**Status:** Accepted · **Date:** 2026-08-03
**Amended by:** [ADR-0015](0015-two-product-lines-one-engine.md) — the decision stands, with a concrete split trigger

## Context

The protocol is shared by the agent, the browser and (later) the control plane.
The scoring model must run identically in the agent and on the server. A
protocol change that lands in one place and not the others produces results that
disagree about their own meaning.

## Decision

One repository. A Cargo workspace under `crates/`, a pnpm workspace under
`apps/`. Protocol, scoring and reporting are separate crates with a strictly
downward dependency direction: `protocol` depends on nothing internal, `scoring`
only on `protocol`, the agent on everything, and nothing on the agent.

## Alternatives

**Multi-repo with published crates.** Rejected for this phase. It would force a
release cycle for every protocol change and make an atomic
protocol-plus-consumers change impossible. Reconsider once the protocol is
stable and third parties depend on it.

**One giant crate.** Rejected: the control plane needs `scoring` and `report`
without pulling in a CLI, an HTTP server and a tokio runtime.

## Consequences

- CI builds everything on every change. Acceptable at this size.
- The scoring model is reusable by a server that never runs a benchmark, which
  is what makes server-side recomputation possible.
- The web UI's TypeScript protocol types are hand-maintained against the Rust
  types, with a CI parity check. Generation is deferred until the protocol
  settles (see ADR-0004).

## Revisit if

Third parties begin depending on `darcbench-protocol` as a published crate, or
the control plane grows a team that needs an independent release cadence.

**Amended 2026-08-29 by [ADR-0015](0015-two-product-lines-one-engine.md).** The
client product line raised the multi-repo question again and this decision was
re-affirmed for the current phase, on the argument above: a second reference
profile, a second scoring model, a GPU module and the `darcbench-core`
extraction all touch protocol, scoring and modules together, so a version
boundary through the middle of them is at its most expensive right now.

ADR-0015 replaces "reconsider once the protocol is stable" with a testable
trigger: **split when `darcbench-core` has produced a calibrated `dcs/1.0.0`
and a platform product is ready to ship a signed installer.** Until both hold,
one repository.
