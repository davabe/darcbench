# ADR-0015 — Two product lines, one measurement engine, two scoring models

**Status:** Accepted
**Date:** 2026-08-29
**Phase:** 2 (structural), delivered across 3-6
**Supersedes:** nothing
**Amends:** [ADR-0002](0002-repository-structure.md) (adds a split trigger)
**Related:** [ADR-0007](0007-scoring-versioning.md), [ADR-0016](0016-client-reference-darc-ref-c1.md),
[SCORING-SYSTEM](../SCORING-SYSTEM.md), [COMPETITIVE-ANALYSIS](../COMPETITIVE-ANALYSIS.md)

## Context

DARCBench was built to answer one question: *is this server good for hosting
things, and can I trust the answer?* Everything encodes that thesis. Storage,
web, database and deployment carry 54% of the standard total. A rankable total
requires a `web` category, so a machine with no web server cannot produce one.
DARC-REF-1 is a rack dedicated server. The weak-link cap exists because a
hosting box is as good as its worst subsystem.

We are now also entering the client-device market — the ground Geekbench and
PassMark hold — across Windows, macOS and Linux. That raises a structural
question that has to be answered before any code moves: **one product on three
platforms, three products, or something else?**

The market evidence is unambiguous. Geekbench 6 anchors **one** baseline across
macOS, Windows, Linux, Android and iOS, and comparing an iPhone to a workstation
*is* its product. What it separates is installers, store listings and UI shells —
not the measurement core. PassMark, whose per-platform versions diverge in
coverage, has the correspondingly weaker cross-platform story and the weaker
methodological reputation.

The lesson: cross-platform benchmarks that share a core and a baseline become
standards. Ones that fork per platform become three niche tools. Comparability
is not a feature of the product; it *is* the product.

## Decision

**Two product lines. One measurement engine. Two scoring models.**

1. **One engine, enforced by a crate boundary.** A new `darcbench-core` holds
   the portable measurement engine: `module`, `harness`, `workloads`,
   `cpu.mixed`, `memory.bandwidth`, and the GPU module when it lands. It has no
   OS-specific dependency. `darcbench-modules` keeps the server workloads
   (`php.runtime`, `node.runtime`, `web.*`, `database.*`, `wordpress.*`,
   `deployment.container`, `storage.mixed`, `network.transfer`, `proxy`,
   `runtime_exec`), depends on core, and re-exports it so the agent is
   unaffected.

   The boundary is **product line, not OS family.** `#[cfg(unix)]` is the wrong
   tool: macOS is unix and is a *client* target, so a `cfg(unix)` gate would
   wrongly enable the WordPress module on a MacBook. A crate boundary states the
   real distinction and the compiler enforces it.

2. **Two scores over one body of evidence.** `dbs/x.y.z` (server) and
   `dcs/x.y.z` (client). Both are pure functions of the same retained raw
   metrics, differing only in reference profile and category weights. This costs
   almost nothing because [ADR-0007](0007-scoring-versioning.md) already
   separated evidence from score: one run's bundle can be scored under both
   models without re-running anything. `Facet::{SingleCore, MultiCore}` and
   `CompositeKey` already exist and carry over unchanged.

3. **`HostFacts` as a trait.** The `/proc`+`/sys` collectors in
   `darcbench-inventory` move behind a trait with one implementation per
   platform. The seam already exists — `read_file` is the only reader and
   parsing is already separated and tested against strings.

4. **The repository stays a monorepo for now.** See the trigger below.

## Alternatives

**Three forks, one repository per OS.** Rejected. It destroys cross-platform
comparability, which is the only thing that makes a client benchmark worth
building, and it does so silently: three copies of `workloads.rs` diverge within
months and nothing fails loudly when they do. It would also contradict this
project's central commitment — a number it cannot defend.

**One score for both lines.** Rejected. The hosting weights and the weak-link cap
actively mis-rank a laptop: a fast ultrabook with no web server would score
`Partial` forever, and a machine with modest storage would be capped for a
reason that does not apply to a client device. Two theses need two models.

**Split the repositories now.** Rejected for this phase, on ADR-0002's own
argument: it would force a release cycle for every protocol change and make an
atomic protocol-plus-consumers change impossible. We are about to enter the
highest-churn period in the project's history — a second reference profile, a
new scoring model, a GPU module and the core extraction all touch protocol,
scoring and modules together. A version boundary through the middle of that is
the most expensive possible moment to introduce one.

**Split trigger, so this is a decision and not a deferral:** split when
`darcbench-core` has produced a *calibrated* `dcs/1.0.0` **and** a platform
product is ready to ship a signed installer. Target shape at that point:

```
darcbench-core          workloads · harness · protocol · scoring · report · references
   ├── darcbench-linux     server line; /proc+/sys host layer
   ├── darcbench-windows   client line; WMI/PDH host layer
   └── darcbench-macos     client line; sysctl/IOKit host layer
```

## Consequences

- The compiler, not a convention, keeps server workloads out of the client
  product. A client build that pulls in `php.runtime` fails to link.
- CI grows a cross-platform check whose crate list is a **ratchet**: each crate
  that becomes portable is added and never removed. That job is what makes the
  extraction verifiable rather than asserted.
- `darcbench-report` is not portable yet — it depends on `darcbench-inventory`.
  It joins the ratchet after `HostFacts` lands.
- Two scoring models means two calibrations, two references, and two sets of
  comparability rules to maintain. That cost is real and is the price of the
  market.
- The moat in [COMMERCIAL-STRATEGY](../COMMERCIAL-STRATEGY.md) is unchanged and
  arguably strengthened: open core, signed results and a public corpus is a
  sharper weapon against a proprietary incumbent in the client market than it
  was in the server one.

## Revisit if

Cross-OS variance on the shared kernels proves large enough that one baseline
cannot honestly serve three platforms — in which case the client line needs a
per-OS displayed dimension at minimum, and possibly per-OS references. The
measurement that decides this is named in [ADR-0016](0016-client-reference-darc-ref-c1.md).
