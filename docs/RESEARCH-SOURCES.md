# Research sources

All sources were accessed on **2026-08-03** unless stated otherwise. Market data
in particular changes constantly: hosting prices, plan specifications and CPU
generations move on a timescale of weeks, so every claim derived from a vendor
page is date-stamped and must be re-verified before it is used in any published
comparison.

Nothing in this repository reproduces a proprietary workload, scoring formula or
dataset. Competitor research informed *what problems to solve* and *what
mistakes to avoid*; the DARCBench methodology, workloads and scoring model are
original. See `docs/COMPETITIVE-ANALYSIS.md` for the analysis and
`LICENSE-STRATEGY.md` for the licensing position.

## How to read this file

Each entry records the source, who publishes it, when it was accessed, which
DARCBench decision it supports, and what it does **not** establish. That last
column matters: several of these sources are secondary, and a benchmark suite
that cites a blog post as if it were a specification has no business asking
anyone to trust its numbers.

---

## Benchmark methodology and scoring

### SPEC CPU2017 — overview and run rules
- **Publisher:** Standard Performance Evaluation Corporation
- **URL:** <https://www.spec.org/cpu2017/Docs/overview.html>
- **Accessed:** 2026-08-03
- **Supports:** ADR-0007 (scoring versioning); the decision to aggregate
  normalised ratios with a **geometric mean**, and to normalise against a
  **fixed reference machine** rather than a rolling best-known result. SPEC's
  stated rationale — that the geometric mean weights low results more heavily
  and so discourages optimising a single component — is the same property
  DARCBench needs. Also supports the "median or slower of two runs" style of
  conservative reportable-result rule reflected in
  `docs/BENCHMARK-METHODOLOGY.md`.
- **Does not establish:** that a geometric mean is *sufficient* protection
  against an unbalanced machine. Our own unit test
  (`one_catastrophic_category_cannot_be_hidden`) showed it is not, which is why
  DARCBench adds an explicit weak-link cap on top. See `docs/SCORING-SYSTEM.md`.
- **Limitation:** SPEC CPU is licensed commercial software with strict
  publication rules. DARCBench does not use, redistribute or imitate SPEC
  workloads; only the published aggregation *methodology* informed our design.

### Geekbench 6 — benchmark internals
- **Publisher:** Primate Labs
- **URLs:** <https://www.geekbench.com/doc/geekbench6-benchmark-internals.pdf>,
  <https://support.primatelabs.com/discussions/geekbench/84940-inquiry-about-geekbench-6-scoring-calculation-methodology>
- **Accessed:** 2026-08-03
- **Supports:** the two-level aggregation shape in `docs/SCORING-SYSTEM.md`
  (geometric mean within a group, weighted combination across groups), and the
  decision to publish a **reference-anchored** score. Geekbench 6 anchors a Dell
  Precision 3460 (Core i7-12700) at 2500; DARCBench anchors DARC-REF-1 at 1000
  for the reasons given in `model.rs::REFERENCE_ANCHOR`.
- **Also supports:** the separate reporting of single-core and multi-core
  results, which `cpu.mixed` implements as its `single` and `multi` shapes.
- **Does not establish:** any DARCBench weighting. Geekbench's 65/35 integer /
  floating-point split is theirs and is not copied.

### Chips and Cheese — "Evaluating Geekbench 6"
- **Author:** Chester Lam
- **URL:** <https://chipsandcheese.com/p/evaluating-geekbench-6>
- **Accessed:** 2026-08-03
- **Supports:** the caution in `docs/BENCHMARK-METHODOLOGY.md` that short
  workloads with small working sets can under-represent server behaviour.
- **Limitation:** secondary analysis, not a primary specification. Used as a
  prompt to think, not as an authority.

### PassMark PerformanceTest — CPU test information
- **Publisher:** PassMark Software
- **URLs:** <https://www.cpubenchmark.net/cpu_test_info.html>,
  <https://www.passmark.com/support/performancetest_faq/understanding-results.php>
- **Accessed:** 2026-08-03
- **Supports:** the decision **against** a weighted harmonic mean of
  sub-test scores (PassMark's V9 CPUMark approach) as our primary aggregate,
  and against averaging community submissions into a single per-model figure.
  Both make results easier to read and harder to defend; DARCBench keeps every
  run's raw samples and never averages across machines.
- **Does not establish:** anything about server or web workloads. PassMark's
  suite is desktop-oriented.

### Phoronix Test Suite — features and result validation
- **Publisher:** Phoronix Media / phoronix-test-suite maintainers
- **URLs:** <https://www.phoronix-test-suite.com/?k=features>,
  <https://github.com/phoronix-test-suite/phoronix-test-suite>
- **Accessed:** 2026-08-03
- **Supports:** the **dynamic repetition** design in
  `docs/BENCHMARK-METHODOLOGY.md`. PTS re-runs a test when the standard
  deviation between runs exceeds a configurable threshold (default 3.5%), up to
  a bounded multiple of the base run count. DARCBench adopts the idea and the
  bound, but reports the variance rather than only using it as a stopping
  condition — see "Variance is a result, not noise".
- **Does not establish:** DARCBench's specific CV thresholds (0.15 module
  warning, 0.25 validation failure), which are our own and are documented as
  provisional pending calibration data.

### wrk2 — constant-throughput load generation and coordinated omission
- **Author:** Gil Tene
- **URL:** <https://github.com/giltene/wrk2>
- **Accessed:** 2026-08-03
- **Supports:** the requirement in `docs/BENCHMARK-METHODOLOGY.md` that every
  future HTTP-facing module (Phase 3+) must use an **open-model, constant-rate**
  generator and measure latency from the time a request *should* have been sent.
  A closed-loop generator that waits for the previous response silently omits
  exactly the outliers a hosting customer cares about.
- **Also supports:** recording full latency distributions (HdrHistogram-style)
  rather than a mean, reflected in the `Summary` type carrying percentiles and
  the plan for p99/p99.9 in storage and web modules.

### Grafana k6 — open vs closed models, and injector sizing
- **Publisher:** Grafana Labs
- **URLs:** <https://grafana.com/docs/k6/latest/using-k6/scenarios/concepts/open-vs-closed/>,
  <https://grafana.com/blog/2020/03/03/open-source-load-testing-tool-review/>
- **Accessed:** 2026-08-03
- **Supports:** the `GeneratorSaturated` warning code in
  `darcbench-protocol::WarningCode` and the rule that a run whose load
  generator saturated is not a valid result. k6's guidance to leave headroom on
  the injector is the operational form of the same rule.
- **Does not establish:** which generator DARCBench will use in Phase 3. That
  decision is deferred with stated criteria in `docs/ROADMAP.md`.

### fio — flexible I/O tester documentation
- **Publisher:** Jens Axboe / fio maintainers
- **URL:** <https://fio.readthedocs.io/en/latest/fio_doc.html>
- **Accessed:** 2026-08-03
- **Supports:** the storage module design in
  `docs/BENCHMARK-METHODOLOGY.md`: `direct=1` to bypass the page cache,
  explicit ramp time before recording, realistic queue depths rather than
  vanity depths, tail-percentile reporting, and separate fsync-latency
  measurement. Also supports the SSD-preconditioning caveat: without it, a
  short random-write test measures the drive's SLC cache, not the drive.
- **Limitation:** fio is GPL-2.0. DARCBench will invoke it as an external tool
  under an explicit dependency declaration, never vendor or link it. See
  `docs/BENCHMARK-MODULE-SPEC.md`.

### Arm Learning Path — benchmarking block storage with fio
- **Publisher:** Arm
- **URL:** <https://learn.arm.com/learning-paths/servers-and-cloud-computing/disk-io-benchmark/using-fio/>
- **Accessed:** 2026-08-03
- **Supports:** the aarch64 storage-testing notes in the Phase 2 backlog.

### YABS (Yet-Another-Bench-Script)
- **Author:** masonr and contributors
- **URL:** <https://github.com/masonr/yet-another-bench-script>
- **Accessed:** 2026-08-03
- **Supports:** the product positioning in `docs/PRODUCT-BIBLE.md`. YABS
  demonstrates the real demand — one command, no setup, no root, works on every
  common distribution — and simultaneously demonstrates the ceiling: it
  orchestrates three third-party tools and prints their output, with no scoring
  model, no result integrity, no web workloads and no way to compare two runs
  statistically. DARCBench's usability bar is YABS; its rigour bar is not.
- **Note:** YABS wraps Geekbench, whose licence governs redistribution of that
  binary. DARCBench does not bundle third-party benchmark binaries.

### HammerDB — TPROC-C and TPC derivation rules
- **Publisher:** HammerDB project / TPC
- **URLs:** <https://www.hammerdb.com/docs/ch03s02.html>,
  <https://www.hammerdb.com/docs/ch11s01.html>
- **Accessed:** 2026-08-03
- **Supports:** the naming and claims policy for the Phase 4 database modules.
  `TPC-C` and `tpmC` are registered trademarks of the TPC Council and may only
  be used for officially audited results. HammerDB's response — a derived
  workload under a distinct name (`TPROC-C`) with a distinct metric (`NOPM`) —
  is the pattern DARCBench will follow. Our OLTP module will be named
  `database.oltp` and report `dbtx/s`; it will never claim tpmC.
- **Does not establish:** that our workload will be TPC-C-derived at all. That
  is an open Phase 4 decision recorded in `docs/BACKLOG.md`.

## Hosting market

> Every figure below is a snapshot. Treat any price older than a few weeks as
> unverified. `docs/MARKET-RESEARCH.md` explains how these segments map to
> DARCBench modules and scores.

### DigitalOcean Droplet pricing
- **URL:** <https://www.digitalocean.com/pricing/droplets>
- **Accessed:** 2026-08-03
- **Data used:** the full published matrix of Basic (shared vCPU),
  General Purpose, CPU-Optimized, Memory-Optimized and Storage-Optimized plans,
  with vCPU / RAM / SSD / transfer and monthly USD price. Example anchors:
  Basic 1 GiB / 1 vCPU / 25 GB / 1,000 GiB at **$6.00/mo**; CPU-Optimized
  8 GiB / 4 vCPU / 50 GB at **$84.00/mo**; Storage-Optimized 64 GiB / 8 vCPU /
  1,170 GB at **$524.00/mo**.
- **Supports:** the shared-vCPU vs dedicated-vCPU distinction that the
  `cpu.mixed` scaling-efficiency metric is designed to expose, and the
  price-per-score analysis deferred to the control plane.

### Hetzner — dedicated server line (AX series)
- **URLs:** <https://www.hetzner.com/pressroom/neue-dedicated-server-2023/>,
  <https://www.hetzner.com/pressroom/new-amd-ryzen-7950-server/>,
  <https://www.hetzner.com/dedicated-rootserver/ax102/>
- **Accessed:** 2026-08-03
- **Data used:** AX52 — AMD Ryzen 7 7700 (Zen 4, 8C/16T), 64 GB DDR5,
  2 × 1 TB Gen4 NVMe. AX102 — Ryzen 9 7950X3D (16C/32T, 3D V-Cache on one CCD),
  128 GB DDR5 ECC, 2 × 1.92 TB datacenter Gen4 NVMe; launch price quoted at
  €109/mo + €39 setup in the April 2023 press release.
- **Supports:** the **DARC-REF-1 reference specification** in
  `crates/darcbench-scoring/src/reference.rs`. The AX52 class was chosen as the
  shape of the reference machine because it is close to the median modern web
  hosting dedicated server actually sold, which makes a score of 1000 mean
  something intuitive.
- **Does not establish:** any performance figure. The DARC-REF-1 anchor values
  are declared targets awaiting measurement on physical hardware; they are not
  Hetzner benchmark results, and no such results are claimed anywhere in this
  repository.
- **Caveat:** press-release pricing from 2023 is stale for any commercial
  purpose. Re-verify before use.

### OVHcloud — bare metal ranges
- **URLs:** <https://us.ovhcloud.com/bare-metal/prices/>,
  <https://www.ovhcloud.com/en/bare-metal/scale/>
- **Accessed:** 2026-08-03
- **Data used:** the Advance and Scale 2026 generations; Advance-1 2026 with
  AMD EPYC 4245P (6C/12T), 32–256 GB, NVMe, 25 Gbps private / 1–5 Gbps public,
  quoted from $134/mo with a setup fee; Scale 2026 on EPYC 9005 with very high
  core counts and up to 3 TB DDR5.
- **Supports:** the network-category weight cap in `docs/SCORING-SYSTEM.md`.
  Public bandwidth varies by an order of magnitude across a single vendor's
  range, so letting network throughput dominate a general score would rank
  machines by their uplink rather than their usefulness.

### Contabo — high-density VPS
- **URLs:** <https://1vps.com/review-contabo/>, <https://www.vpsbenchmarks.com/compare/contabo_vs_racknerd>
- **Accessed:** 2026-08-03
- **Supports:** the noisy-neighbour and oversubscription detection requirements:
  steal-time sampling, coefficient-of-variation reporting, and the endurance
  profile. The segment exists, is popular, and is precisely where a single
  point-estimate benchmark misleads buyers most.
- **Limitation:** **secondary sources.** These are review and comparison sites,
  not vendor specifications. They are cited as evidence that the *phenomenon* is
  widely reported, not as authoritative performance data. No number from them is
  used in DARCBench.

### Oracle Cloud — Always Free Ampere A1
- **URLs:** <https://www.infoq.com/news/2026/07/oracle-cloud-free-tier-limits/>,
  <https://terminalbytes.com/oracle-cloud-free-tier-changes-2026/>
- **Accessed:** 2026-08-03
- **Data used:** reports that the Always Free Ampere A1 allowance was reduced
  from 4 OCPU / 24 GB to 2 OCPU / 12 GB effective 2026-06-15, apparently without
  a public announcement.
- **Supports:** the arm64 support requirement in `docs/ROADMAP.md` (Phase 8) and
  the argument in `docs/PRODUCT-BIBLE.md` that hosting entitlements change
  silently, so a benchmark result is only meaningful with a timestamp and a
  captured environment snapshot.
- **Limitation:** **secondary sources reporting an unannounced change.** Treated
  as a signal, not as fact. No DARCBench behaviour depends on it.

### AWS burstable instances and CPU steal
- **URLs:** <https://repost.aws/questions/QU2Ss4A4MMSFarFCBgr3dWuQ/how-to-resolve-very-high-cpu-steal-time-on-a-t3-large-instance-without-using-dedicated-and-reserved-instances>,
  <https://www.percona.com/blog/choose-your-ec2-instance-type-wisely-on-aws/>
- **Accessed:** 2026-08-03
- **Supports:** the interpretation of steal time in
  `darcbench-inventory::telemetry` and in `docs/BENCHMARK-METHODOLOGY.md`. On a
  T-series instance, steal is not (only) a noisy neighbour — it is the
  instance being throttled to its baseline after exhausting CPU credits.
  DARCBench therefore reports steal as a first-class telemetry series and
  frequency/throughput drift over the run, rather than collapsing "CPU used"
  into one number that hides it.
- **Limitation:** AWS re:Post is a community forum; the Percona article is a
  vendor blog. Both are widely corroborated but neither is a specification.

---

## Sources deliberately **not** used

- **TPC official result databases.** Publishing or deriving comparisons from
  audited TPC results carries trademark and fair-use obligations DARCBench is
  not in a position to meet.
- **SPEC published result databases.** Same reasoning.
- **Proprietary scoring formulas** from any commercial benchmark. None were
  reverse-engineered, and none are implemented.
- **Any provider's own published benchmark numbers.** Vendor-run benchmarks are
  a marketing artifact. DARCBench's entire premise is measuring them
  independently; quoting them as a baseline would defeat that.

## Gaps in current research

Stated plainly, because a research document that implies completeness is worse
than one that admits its edges:

1. **No DARC-REF-1 hardware has been measured.** Every reference anchor is a
   declared target. This is the single largest open item and blocks a calibrated
   scoring release. See `docs/ROADMAP.md` Phase 2.
2. **Hosting coverage is uneven.** DigitalOcean's pricing matrix was captured in
   full from the vendor's own page; Hetzner and OVHcloud from vendor pages and
   press releases; Contabo, netcup, RackNerd, Vultr, Linode/Akamai, Azure, GCP,
   Lightsail, Scaleway, IONOS, Leaseweb, Hivelocity, NOCIX, Wholesale Internet,
   HostHatch and Equinix Metal were **not** captured at primary-source depth in
   this pass. `docs/MARKET-RESEARCH.md` marks which segments are evidenced and
   which are structural reasoning.
3. **Vultr's pricing page returned HTTP 403** to automated retrieval on
   2026-08-03, so no Vultr figures are recorded.
4. **No independent replication.** Every methodology decision here is reasoned
   from published sources; none has yet been validated against a controlled
   experiment on real hardware. Phase 2 exists to fix that.
