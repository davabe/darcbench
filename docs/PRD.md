# Product requirements

Requirements are numbered and testable. Status: ✅ implemented and tested,
⏳ planned with a phase, ⚠️ deliberately excluded.

## Operating modes

| # | Requirement | Status |
|---|---|---|
| M1 | Standalone mode is fully useful with no account and no contact with any DARCBench service | ✅ |
| M2 | The agent serves a browser dashboard reachable from another machine | ✅ (via tunnel) |
| M3 | Connected mode uploads signed bundles to an optional control plane | ⏳ Phase 5 |
| M4 | The agent never accepts unauthenticated remote command execution | ✅ |
| M5 | In connected mode the agent initiates every connection; the control plane never connects inward | ⏳ Phase 5, decided in ADR-0010 |

## Installation and discovery

| # | Requirement | Status |
|---|---|---|
| I1 | Single static binary, no runtime dependency | ✅ |
| I2 | Detects hosting panels, web servers, databases, container runtimes, firewalls, listening ports — read-only | ✅ |
| I3 | Produces a capability report before modifying anything | ✅ (`doctor`) |
| I4 | Binds loopback by default; never binds 80, 443 or another occupied/reserved port | ✅ |
| I5 | Never modifies panel, web server or firewall configuration | ✅ |
| I6 | Optional reverse-proxy integration generates, previews, validates, backs up and can roll back | ⏳ Phase 3 |
| I7 | Uninstall removes only what was created, and says what that is first | ✅ |
| I8 | Install script offers a verifiable alternative to piping into a shell | ⏳ Phase 8 |

## Safety

| # | Requirement | Status |
|---|---|---|
| S1 | Risk classified before every run: Safe / Moderate / Heavy / Production risk / Unsupported | ✅ |
| S2 | Estimated duration, bytes written and network transfer shown before starting | ✅ |
| S3 | Refuses to start when free space is insufficient; unknown free space is unsafe | ✅ |
| S4 | Detects production signals and raises the risk class | ✅ |
| S5 | `--force` overrides warnings, never blocking findings | ✅ |
| S6 | Cancellation always reaches a terminal state and produces a bundle | ✅ |
| S7 | Only one run at a time | ✅ |
| S8 | Never tests raw block devices | ✅ (no module can) |
| S9 | Never benchmarks an existing production database | ⏳ Phase 4, enforced by design |
| S10 | Watchdog, max runtime, thermal guard, load ceiling, transfer ceiling | ✅ |

## Measurement

| # | Requirement | Status |
|---|---|---|
| B1 | Warm-up repetitions, excluded from statistics but retained and flagged | ✅ |
| B2 | ≥5 measured repetitions in every profile | ✅ |
| B3 | Median headline, plus stddev, CV and non-parametric CI where n permits | ✅ |
| B4 | Outliers flagged, never silently removed | ✅ |
| B5 | Iteration counts calibrated so a repetition lands in a trustworthy duration | ✅ |
| B6 | Repetitions shorter than 20 ms rejected | ✅ |
| B7 | Deterministic corpora from a fixed seed | ✅ |
| B8 | Workloads cannot be optimised away | ✅ (black_box + scaling tests) |
| B9 | Telemetry sampled at ≤1 Hz to bound observer overhead | ✅ |
| B10 | Steal time, frequency drift, PSI and thermal data captured during the run | ✅ |
| B11 | Load generator saturation invalidates a web result | ⏳ Phase 3 |
| B12 | Page cache never mistaken for memory or storage bandwidth | ⏳ Phase 2, specified |

## Scoring

| # | Requirement | Status |
|---|---|---|
| C1 | Higher is always better | ✅ |
| C2 | Latency inverted exactly once | ✅ |
| C3 | One catastrophic subsystem cannot be hidden | ✅ (geometric mean + weak-link cap) |
| C4 | Core count alone cannot buy a good score | ✅ (facets + efficiency) |
| C5 | Network cannot dominate a compute-oriented score | ✅ (8% cap) |
| C6 | Scores versioned; never changed silently | ✅ |
| C7 | Raw measurements always accompany scores | ✅ |
| C8 | Any historical run rescoreable without re-running | ✅ (pure function) |
| C9 | Uncalibrated models declare themselves | ✅ (enforced by test) |
| C10 | Workload composites withheld below 60% input coverage | ✅ |

## Verification

| # | Requirement | Status |
|---|---|---|
| V1 | Bundles signed over a canonical serialisation | ✅ |
| V2 | Server recomputes every score from raw metrics | ✅ (`verify --json`, server path) |
| V3 | Tampering with a score or a metric is detected | ✅ |
| V4 | Result states: Local / SelfReported / Validated / Verified / Official / Invalid / Partial / Custom | ✅ |
| V5 | Only Validated, Verified and Official are rankable | ✅ |
| V6 | Nonce and run token prevent replay | ⏳ Phase 6 |
| V7 | Agent build hash checked against published releases for Verified | ⏳ Phase 6 |
| V8 | No invasive hardware fingerprinting | ⚠️ Deliberately excluded |

## Interface

| # | Requirement | Status |
|---|---|---|
| U1 | Live progress, telemetry, per-test results and provisional score in the browser | ✅ |
| U2 | Cancellation from the browser | ✅ |
| U3 | Raw metric explorer | ✅ |
| U4 | Final score presentation with result state | ✅ |
| U5 | HTML, JSON and NDJSON export | ✅ |
| U6 | Keyboard navigation, sufficient contrast, reduced motion, screen-reader labels, no colour-only meaning | ✅ |
| U7 | Usable over slow remote connections | ✅ (bounded buffers, coalesced samples) |
| U8 | Run-to-run and server-to-server comparison | ⏳ Phase 5 |
| U9 | Radar / balance visualisation | ⏳ Phase 2, once >1 category exists |
| U10 | Public share page | ⏳ Phase 6 |

## CLI

| # | Requirement | Status |
|---|---|---|
| L1 | `doctor`, `inspect`, `serve`, `run`, `status`, `report`, `verify`, `uninstall` | ✅ |
| L2 | `--json`, `--no-color`, `--non-interactive` on every command | ✅ |
| L3 | JSON output is a single clean document on stdout; logs go to stderr | ✅ |
| L4 | Meaningful exit codes (0 ok, 2 preflight blocked, 3 verification failed, 130 cancelled) | ✅ |
| L5 | `status` and `cancel` against a running agent | ⏳ Phase 2 (currently via the API) |

## Privacy

| # | Requirement | Status |
|---|---|---|
| P1 | Hostnames, MACs, IPs, serials and instance ids redacted by default | ✅ |
| P2 | Revealing is opt-in and refused on non-loopback binds | ✅ |
| P3 | DMI serial numbers and UUIDs never collected | ✅ |
| P4 | Run ids are random, not derived from host properties | ✅ |
| P5 | Public sharing is opt-in | ⏳ Phase 6 |
| P6 | Standalone mode makes no outbound connection | ✅ |
