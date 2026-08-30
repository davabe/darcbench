# ADR-0016 — DARC-REF-C1, the client reference, and the honesty it is held to

**Status:** Accepted
**Date:** 2026-08-29
**Amended:** 2026-08-30 - the allocator is recorded, not eliminated; see "The allocator row"
**Phase:** 2
**Supersedes:** nothing
**Related:** [ADR-0015](0015-two-product-lines-one-engine.md), [ADR-0007](0007-scoring-versioning.md),
[SCORING-SYSTEM §3](../SCORING-SYSTEM.md), [CALIBRATION-RUNBOOK](../CALIBRATION-RUNBOOK.md)

## Context

The client line ([ADR-0015](0015-two-product-lines-one-engine.md)) needs its own
normalisation anchor. DARC-REF-1 is a Hetzner AX52-class rack server chosen
because it is the median dedicated server people actually buy. Normalising a
laptop against it would produce a technically valid and practically absurd
number.

The same reasoning that produced DARC-REF-1 applies: anchor on a machine people
really own, so the score carries meaning without a lookup table.

## Decision

**DARC-REF-C1** is the client reference specification.

| Component | Specification |
|---|---|
| CPU | AMD Ryzen 9 9900X (Zen 5, 12C/24T) |
| GPU | NVIDIA GeForce RTX 5080 |
| Memory | *to be recorded from the calibration host* |
| Storage | *to be recorded from the calibration host* |
| OS | *see "The reference OS must be named" below* |

The blank rows are blank on purpose. Inventing them would put fabricated numbers
into the anchor every client score derives from, which is the exact failure
`reference::provisional_reference` is flagged against today. They are filled from
the real machine at characterisation time, not from a spec sheet.

**Anchor value: 1000**, matching DARC-REF-1. A 2400 machine is 2.4x the
reference, in both product lines. Keeping the scales dimensionally identical is
what lets the two scores sit next to each other without a footnote, even though
they are never directly comparable.

### One machine is not a calibration

The calibration procedure this project wrote for itself
([SCORING-SYSTEM §3.1](../SCORING-SYSTEM.md)) requires **three physically
distinct hosts from at least two vendors**, medians of per-host medians, and
rejection of any metric whose across-host CV exceeds 5%. A single machine on a
desk does not meet that bar, and
[COMMERCIAL-STRATEGY](../COMMERCIAL-STRATEGY.md) names methodological
credibility as the moat: *slowly earned, instantly lost.*

The resolution is the one already in use for DARC-REF-1: **a reference is a
specification, not a serial number.**

1. Characterise the single available 9900X host. Write the measured values into
   the client reference profile with `calibrated: false`.
2. The client model stays `dcs/0.1.0-dev` while that flag is false. Every score
   it produces is flagged `uncalibrated`, exactly as the server line is today.
3. Before `dcs/1.0.0`: replicate on two further hosts matching the
   specification, apply the CV gate, publish the raw calibration bundles.

**The single host unblocks all development. It does not unblock a 1.0.** That
sentence is the decision; the rest is procedure.

### The reference OS must be named

DARC-REF-1 names *Debian 12, kernel 6.1 LTS, performance governor, default
mitigations* — because the OS is part of what was measured. DARC-REF-C1 must do
the same. If the anchor is characterised under Windows, then Windows is the
calibration OS and every Linux and macOS client score is measured against a
Windows-calibrated anchor.

That is defensible, but only if it is stated. It also promotes the cross-OS
delta from a curiosity to **the mechanism by which a Mac score relates to the
anchor at all**, which is why the characterisation below runs on more than one
OS from the start.

### First calibration step: cross-OS characterisation on the anchor host

Before the 9900X becomes an anchor, it has to be characterised — including how
it behaves under each OS. Same silicon, separate boots (not a VM, and not WSL2:
both virtualise and confound the measurement), the five existing kernels through
the real harness, one bundled allocator on every target so the allocator is not
a free variable.

Known sources of cross-OS divergence, and the disposition of each:

| Source | Disposition |
|---|---|
| Allocator (glibc vs Windows heap vs magazine_malloc) | **Record, then eliminate only if it proves large.** Revised 2026-08-30 - see below |
| Toolchain (`target-cpu`, `opt-level`, `lto`, `codegen-units`) | **Eliminate.** Pin identically across targets, verify in CI |
| Thread placement (P/E cores, macOS QoS classes) | **Control.** Set affinity and QoS explicitly. Homogeneous on the 9900X; not on Intel clients or Apple Silicon |
| Timer resolution and cost | **Validate.** Re-check `MIN_REP_MS` per OS |
| Filesystem (NTFS vs APFS vs ext4) | **Report.** This is a real property of the machine, not noise |

#### The allocator row, revised 2026-08-30

This ADR said *eliminate: bundle one allocator on all targets*. Building the
instrument showed the price. Every production-grade replacement - mimalloc,
rpmalloc, snmalloc - is C or C++, and this workspace is deliberately pure Rust
apart from `rusqlite`; the `rustls` dependency comment records `aws-lc-sys`
being rejected for needing `cmake` alone. Bundling would put a C toolchain
requirement on the one crate that must build on MSVC, macOS and Linux, to remove
a variable nobody has yet shown to be large.

So the allocator is **recorded** rather than eliminated: `darcbench-characterise`
carries the full target triple, which names the allocator and the CRT, and the
allocation-heavy metrics are identified so its contribution shows up in the
results instead of hiding in them. If the residual on those metrics is large,
bundling becomes the round-two experiment and the C dependency is then worth its
price, because it buys a known quantity rather than a guess.

The other three rows stand as written. See
[CHARACTERISATION-RUNBOOK](../CHARACTERISATION-RUNBOOK.md).

For what remains after that, the disposition is **disclose, never correct**. An
OS correction factor would look like fudging and would cost more credibility
than it buys. Instead, OS joins the comparability tuple in
[SCORING-SYSTEM §6](../SCORING-SYSTEM.md) as rule 7, displayed and never silently
pooled — the same treatment execution scope already gets under rule 6.

Publishing the measured cross-OS delta is also a competitive position: the
incumbents assert cross-platform comparability without publishing the residual.

### The anchor is NVIDIA silicon

Normalising AMD, Intel and Apple GPUs against an RTX 5080 is acceptable in the
same way that DARC-REF-1 anchoring CPU work on AMD silicon is — *provided the
workloads do not favour the anchor's vendor.* It makes vendor-neutral GPU API
selection a requirement rather than a preference, which is
[ADR-0018](0018-gpu-compute-api.md).

## Consequences

- Two references and two calibrations to maintain, each with its own runbook.
- Client scores are `uncalibrated` until three hosts exist. This must be as loud
  in the client UI as it is in the server reports.
- The characterisation output is the first real dataset of the client line and
  should be published with the raw bundles, not summarised.

## Revisit if

The 9900X/RTX 5080 class stops being representative of the machine the client
audience owns, or cross-OS residual after the eliminations above is large enough
that one anchor cannot honestly serve three platforms.
