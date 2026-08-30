# Licensing strategy

**Decision:** open core. Apache-2.0 for everything an operator runs; AGPL-3.0
for the hosted control plane; CC BY 4.0 for the methodology.

Full reasoning and rejected alternatives: [ADR-0009](docs/adr/0009-licensing.md).

## What is licensed how

| Component | Licence |
|---|---|
| `crates/darcbench-protocol` | Apache-2.0 |
| `crates/darcbench-inventory` | Apache-2.0 |
| `crates/darcbench-scoring` | Apache-2.0 |
| `crates/darcbench-core` | Apache-2.0 |
| `crates/darcbench-modules` | Apache-2.0 |
| `crates/darcbench-report` | Apache-2.0 |
| `crates/darcbench-agent` | Apache-2.0 |
| `apps/web` | Apache-2.0 |
| `apps/control-plane` (Phase 5) | AGPL-3.0, commercial available |
| Benchmark methodology and scoring formulas | CC BY 4.0 |

## Why

**Apache-2.0 on everything an operator runs.** A benchmark nobody can run freely
does not become a standard, and a standard is the only thing worth owning here.
Apache removes every objection a hosting provider's legal team could raise about
deploying the agent across their fleet, and its explicit patent grant matters for
a project publishing a methodology.

**AGPL on the control plane.** This is the open-core boundary. The value of the
hosted product is *operating* it — the leaderboard, the verification service, the
accumulated corpus — not the source. AGPL means a competitor cannot run a closed
fork as a service, while anyone can self-host for their own fleet. For that
promise to be real, self-hosting must be practical, which is why ADR-0010 forbids
depending on any managed service.

**CC BY on the methodology.** A benchmark whose scoring is a black box is a
marketing instrument. Publishing the formulas separately means anyone can
reimplement the model and check our arithmetic — and that verifiability is
precisely what makes the scores worth anything.

## What you may do

- Run the agent anywhere, including commercially, including as a hosting
  provider on your own fleet.
- Modify it, fork it, embed it, redistribute it.
- Implement the scoring model yourself and check our numbers.
- Self-host the control plane for your own infrastructure.

## What triggers the AGPL

Offering a **modified** control plane as a network service. Then the AGPL source
obligation applies, or you take a commercial licence. Running an unmodified
self-hosted control plane for your own organisation triggers nothing.

## Third-party licences

- **fio** (GPL-2.0) — invoked as a declared external dependency in Phase 2.
  Never vendored, never linked. GPL obligations do not propagate to a program
  that executes another program.
- **sysbench** (GPL-2.0) — same position, if used at all.
- **No proprietary benchmark binaries are bundled**, ever. That includes
  Geekbench, which some comparable tools redistribute.
- Rust and npm dependency licences are audited in CI; an SBOM ships with every
  release from Phase 8.

## Trademarks

"DARCBench", "DARC//BENCH" and the Tombatossals Softworks name and marks are not
licensed by the code licences above. A fork may use the code; it may not present
itself as DARCBench. This exists so that a "DARCBench score" means one thing.

## Contributions

Contributions are accepted under the licence of the file being modified. There
is no CLA — it is friction for contributors, and we would rather have the
contributions. The consequence is that relicensing would require contributor
consent, which is an acceptable constraint.
