# ADR-0009: Apache-2.0 core, AGPL-3.0 control plane

**Status:** Accepted · **Date:** 2026-08-03

## Context

The agent must be adopted widely to be credible — a benchmark nobody runs is not
a standard. The business needs something defensible. A licence that blocks
adoption without creating a business is the worst of both.

## Decision

- **Apache-2.0** for `darcbench-protocol`, `darcbench-scoring`,
  `darcbench-modules`, `darcbench-inventory`, `darcbench-report`,
  `darcbench-agent` and the web UI.
- **AGPL-3.0** for `apps/control-plane` (Phase 5), with commercial licensing
  available.
- Benchmark **methodology and scoring formulas are public** under CC BY 4.0, so
  they can be cited and audited independently of the code.

## Rationale

Apache-2.0 on the agent maximises adoption and removes every objection a hosting
provider's legal team could raise about running it on their fleet. Its explicit
patent grant matters for a project publishing a methodology. Hosting providers
must be able to run and even embed the agent freely; that is how it becomes a
standard.

AGPL on the control plane is the open-core boundary. The value of the hosted
product is operating it — the leaderboard, the verification service, the data —
not the source. AGPL means a competitor cannot run a closed fork as a service,
while anyone can self-host for their own fleet.

Publishing the scoring formulas separately under CC BY is deliberate: a
benchmark whose scoring is a black box is a marketing instrument. Anyone must be
able to reimplement the model and check our arithmetic.

## Alternatives

**AGPL everywhere.** Rejected: it would deter exactly the hosting providers and
enterprises whose adoption the project needs.

**MIT everywhere.** Rejected: no patent grant, and no defensible business.

**BSL / SSPL.** Rejected: not OSI-approved, would be treated as proprietary by
most adopters, and would undermine the neutrality claim that gives the scores
value.

**Dual licence with a CLA.** Rejected for now — a CLA is friction for
contributors. Revisit if relicensing ever becomes necessary.

## Consequences

- Anyone may fork the agent, including a competitor. Accepted; the moat is the
  verified result corpus and the methodology's credibility, not the source.
- The AGPL boundary must be architecturally clean: the control plane depends on
  the Apache-licensed crates, never the reverse. Enforced by the dependency
  direction in ADR-0002.
