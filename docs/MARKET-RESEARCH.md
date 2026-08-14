# Hosting market research

**Captured 2026-08-03.** Prices, plans and CPU generations change on a timescale
of weeks. Every figure here is a snapshot and must be re-verified before use in
any published comparison. Sources and their limitations are in
[RESEARCH-SOURCES.md](RESEARCH-SOURCES.md).

## Evidence quality

Being explicit about this, because a market document that implies uniform rigour
is misleading:

| Depth | Providers |
|---|---|
| **Vendor page captured in full** | DigitalOcean (complete pricing matrix) |
| **Vendor pages / press releases** | Hetzner (AX line), OVHcloud (Advance, Scale 2026) |
| **Secondary sources only** | Contabo, RackNerd, netcup, Oracle Cloud free tier |
| **Not captured this pass** | Vultr (HTTP 403 to automated retrieval), Linode/Akamai, AWS, Azure, GCP, Lightsail, Scaleway, IONOS, Leaseweb, Hivelocity, NOCIX, Wholesale Internet, HostHatch, Equinix Metal |

Segment analysis below is **structural reasoning** about how these product
classes behave, not a claim of measured performance. DARCBench has measured no
provider. That is the point of building it.

## Segments and what they demand of a benchmark

### Low-cost high-density VPS
*Contabo-class: ~€4.50/mo for 4 vCPU / 8 GB; ~€13/mo for 8 vCPU / 16 GB / 400 GB NVMe*

Deliberate oversubscription. Headline vCPU counts are real allocations, not real
capacity. Widely reported peak-hour contention.

**What a benchmark must expose:** steal time as its own series; coefficient of
variation, not just a median; sustained versus burst behaviour; scaling
efficiency, which collapses when vCPUs share physical cores. A single-number
benchmark run at 3 a.m. flatters this segment enormously.

### Premium and dedicated-vCPU cloud
*DigitalOcean CPU-Optimized: 8 GiB / 4 vCPU / 50 GB / 5,000 GiB at $84.00/mo;
General Purpose: 8 GiB / 2 vCPU at $63.00/mo; Basic shared: 1 GiB / 1 vCPU at $6.00/mo*

Dedicated-core plans should hold their performance under load; shared plans
should not. That difference is precisely what `cpu.mixed`'s scaling efficiency
and the endurance profile are built to show.

**What a benchmark must expose:** whether "dedicated" is actually dedicated;
storage that is network-attached versus local NVMe — a distinction the storage
category will surface and that a naive `dd` test misses entirely.

### Burstable / credit-based instances
*AWS T-series and equivalents*

Baseline performance plus a credit balance. Once credits are exhausted, the
instance is throttled to baseline — which the guest observes as **high steal
time**, not as reduced clock speed.

**What a benchmark must expose:** this is the single strongest argument for the
endurance profile. A 3-minute benchmark on a T-series instance measures the
credit balance, not the instance. DARCBench treats a throughput decline across a
long run as a first-class finding.

### Modern Ryzen dedicated servers
*Hetzner AX52: Ryzen 7 7700, 8C/16T Zen 4, 64 GB DDR5, 2 × 1 TB Gen4 NVMe.
AX102: Ryzen 9 7950X3D, 16C/32T, 128 GB DDR5 ECC, 2 × 1.92 TB NVMe*

Very high single-thread performance, excellent price/performance for web
hosting, no oversubscription.

This class is the shape of **DARC-REF-1**, because it is close to the median
modern web hosting dedicated server actually sold — which makes a score of 1000
mean something intuitive rather than being an arbitrary index.

The AX102's asymmetric topology (3D V-Cache on one CCD, higher clocks on the
other) is a good stress test for a benchmark's honesty: a suite that reports one
blended CPU number will describe neither half of that chip correctly.

### EPYC and Xeon Scalable dedicated servers
*OVHcloud Advance 2026: EPYC 4245P 6C/12T, 32–256 GB, NVMe, 1–5 Gbps public,
25 Gbps private, from $134/mo. Scale 2026: EPYC 9005, very high core counts,
up to 3 TB DDR5*

Many cores, moderate per-core clocks, large memory capacity, NUMA above one
socket.

**What a benchmark must expose:** the single-core / multi-core split. A 96-core
EPYC will dominate multi-core and can lose to a Ryzen desktop-derived chip on
the single-threaded path that determines PHP request latency. Reporting only a
combined figure would recommend the wrong machine for WordPress hosting.

Public bandwidth varying from 1 to 5 Gbps *within one vendor's range* is the
direct justification for capping the network category at 8% of the total.

### ARM instances
*Ampere Altra, AWS Graviton, Oracle Ampere A1*

Strong performance per watt and per euro; different instruction-set
availability; some workloads are dramatically better or worse than the x86
equivalent.

Note on volatility: Oracle's Always Free Ampere allowance was reportedly reduced
from 4 OCPU / 24 GB to 2 OCPU / 12 GB effective 2026-06-15, apparently without
public announcement (secondary sources; see RESEARCH-SOURCES.md). Whether or not
the detail is exact, the lesson holds: **entitlements change silently**, so a
benchmark result is only meaningful with a timestamp and a captured environment
snapshot. DARCBench records both in every bundle.

### Storage- and network-oriented servers
*DigitalOcean Storage-Optimized: 64 GiB / 8 vCPU / 1,170 GB NVMe at $524.00/mo*

**What a benchmark must expose:** sustained versus burst write behaviour;
whether short random-write tests are measuring the SSD's SLC cache rather than
the drive; transfer quotas, which are a real constraint the CPU score cannot see.

## Marketing claims a benchmark should verify

| Claim | How DARCBench tests it |
|---|---|
| "Dedicated vCPU" | Scaling efficiency; steal time under sustained load |
| "NVMe SSD" | Storage transport in inventory; fsync and tail latency (Phase 2) |
| "Unmetered bandwidth" | Sustained versus burst network throughput (Phase 2) |
| "Enterprise hardware" | ECC presence, CPU model, mitigations, RAID stack |
| "Optimised for WordPress" | WordPress Origin / Cached / Database / Admin scores (Phase 4) |
| "10 Gbit port" | Negotiated link speed versus achieved throughput |
| "High-frequency CPU" | Single-core score; frequency drift across the run |

## How this drives the design

1. **Oversubscription is the dominant real-world failure**, so variance,
   steal time and endurance are first-class rather than optional.
2. **Provider ranges span an order of magnitude on network**, so network is
   capped in the total score.
3. **Web hosting is dominated by single-thread performance and storage
   latency**, so compute and storage carry 46% of the standard total between
   them and both single-core and multi-core are always shown.
4. **Entitlements and plans change without notice**, so every result carries a
   timestamp, an environment snapshot and a digest.
5. **No provider's own numbers are used anywhere.** Quoting vendor benchmarks as
   a baseline would defeat the entire premise.

## Open work

Primary-source capture for the providers marked "not captured", a repeatable
scraping and dating process for plan metadata, and a provider/plan taxonomy in
the control plane so results can be grouped without conflating a shared-vCPU
plan with a dedicated one. Tracked in [BACKLOG.md](BACKLOG.md).
