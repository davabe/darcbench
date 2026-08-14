# ADR-0001: The agent and its workloads are written in Rust

**Status:** Accepted · **Date:** 2026-08-03

## Context

The agent runs on machines it does not own, often production web servers, often
as root. It has to be trivial to install, cheap to run, and it has to *be* the
measurement instrument: the workloads themselves execute inside it.

Requirements: single-file distribution; no runtime dependency that could collide
with the customer's PHP/Python/Node; predictable timing with no collector
pauses; memory safety; cross-compilation to linux/amd64 and linux/arm64; a
credible HTTP server and TLS story.

## Decision

Rust, for both the agent and the first-party workloads. `unsafe_code = "forbid"`
across the workspace. Release profile pinned in the workspace manifest so
workloads are never built at a different optimisation level than the one results
were calibrated at.

## Alternatives

**Go.** Genuinely close, and better at cross-compilation ergonomics. Rejected
primarily because of the garbage collector: a collection pause inside a timed
repetition is measurement error attributed to the machine under test. A
benchmark suite whose own runtime introduces stop-the-world pauses of
unpredictable duration has a credibility problem it cannot argue its way out of,
and GOGC tuning is a workaround, not an answer. Secondary: a Go binary is
larger, and `unsafe` has no equivalent lint boundary.

**C or C++.** Best possible control over timing. Rejected on memory safety: this
process parses `/proc`, handles network input and runs as root on other people's
production servers.

**Python or Node.** Rejected outright. Interpreter version becomes a
confounding variable, and the install story ("first, ensure Python 3.11") is
exactly the friction the product exists to avoid.

**Rust agent orchestrating external binaries** (`fio`, `sysbench`). Rejected as
the *primary* design because it reproduces the weakness of existing shell-script
suites: results depend on whichever version of the tool the distribution shipped.
Adopted as a *secondary* mechanism where a native reimplementation would be
worse than the mature tool (fio for storage), always under an explicit declared
dependency with the tool version recorded in the bundle.

## Consequences

- Compile times are a real cost for contributors. Mitigated by small crates and
  a workspace that allows testing one crate at a time.
- The Rust ecosystem for hosting-panel interaction is thin. Irrelevant: we
  deliberately never interact with panels beyond read-only detection.
- Workloads compile to the same code on every target, so the compiler is a
  controlled variable rather than an uncontrolled one.

## Revisit if

Cross-compiling to a target we need becomes materially harder in Rust than in
Go, or a workload appears that genuinely cannot be expressed safely.
