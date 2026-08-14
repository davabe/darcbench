# DARCBench Product Bible

The document everything else defers to. If another document contradicts this one,
this one is right and the other is a bug.

## What DARCBench is

A benchmarking platform for servers — dedicated, VPS, cloud instances, bare
metal, homelabs, development machines and web hosting infrastructure — that
produces a recognisable score, separated category scores, reproducible raw
measurements, and results a stranger can verify.

The comparison people reach for is 3DMark or Geekbench, for servers. That is
useful shorthand and slightly wrong: those products answer "how fast is this
chip". DARCBench answers **"how good is this machine at hosting things, and can
I trust the answer".**

## The problem

Someone is choosing between a €13/month Contabo VPS with 8 vCPU and a €64/month
Hetzner AX52 dedicated server. Today they can:

- read marketing copy, which is not evidence;
- run `yabs.sh`, which gives three tools' output and no way to weigh them;
- run `fio` themselves, and discover they measured the page cache;
- find a benchmark someone posted, with no environment snapshot, no variance,
  no timestamp and no way to tell if the numbers were edited.

None of these answers the actual question, and the last one is worse than
nothing because it looks like an answer.

## Non-negotiable principles

**1. Never publish a number we cannot defend.** `Partial`, `Invalid` and
`Custom` exist so the suite never has to round a broken run up to a score. A
missing category produces no total, not an optimistic one.

**2. Raw measurements are the product; scores are an opinion.** Every repetition
is retained in physical units. Scores are a pure function of them and are
recomputable under any model version, forever.

**3. Variance is a result.** On shared infrastructure, spread is often the most
useful number on the page. It is measured, published, and factored into the
Stability Score. Outliers are flagged, never deleted.

**4. Safe on a production machine.** No configuration modified, no port taken
over, no production database touched, no raw device written. Preflight
classifies risk, shows what the run costs, and refuses when it should.

**5. Verifiable, with honest limits.** Signed bundles plus server-side score
recomputation. And a plain statement of what that does *not* prove: an operator
who controls the machine can fabricate results, so a locally-signed bundle never
exceeds `SelfReported`.

**6. Neutrality is the asset.** The moment a provider can buy a better score,
every score is worthless. See [COMMERCIAL-STRATEGY.md](COMMERCIAL-STRATEGY.md).

## Users

| Who | Wants | Gets |
|---|---|---|
| **Buyer** evaluating a VPS | "Is this worth €13?" | Quick profile, workload composites, variance, provider comparison |
| **Hosting provider** | Prove the hardware is good | Verified results, fleet benchmarking, published methodology |
| **SRE / platform team** | Detect regressions | Scheduled runs, historical comparison, alerts |
| **Agency** picking hosting for a client | "Will WooCommerce be fast?" | PHP Commerce and WordPress Hosting composites |
| **Homelab operator** | Compare to real servers | Free standalone mode, no account |
| **Journalist / reviewer** | Defensible comparisons | Reproducible methodology, raw data, citable formulas |

## What DARCBench measures

Compute, memory, storage, network, static web serving, PHP, Node.js, databases,
WordPress, container deployment, and sustained behaviour over hours. Details and
status in [BENCHMARK-METHODOLOGY.md](BENCHMARK-METHODOLOGY.md) and
[ROADMAP.md](ROADMAP.md).

The distinguishing choices:

- **Single-core and multi-core are always separate.** A 32-vCPU shared instance
  and a 4-core high-frequency machine can have the same aggregate throughput and
  feel completely different to host a site on.
- **Network is capped at 8% of the total.** A 10 Gbit/s port must not buy a good
  score for a machine with a slow disk.
- **Sustained behaviour is a first-class result.** Burst credits, thermal
  throttling and noisy neighbours are exactly what a point-estimate benchmark
  misses, and exactly what the buyer will experience.

## What DARCBench will never do

Permanent constraints, not backlog items:

- Benchmark an arbitrary user-supplied URL. That is a DDoS tool with a scoring
  model.
- Query a cloud metadata endpoint. Those responses carry credentials.
- Modify web server, panel or firewall configuration.
- Touch a production database, or test a raw block device.
- Serve an unauthenticated dashboard.
- Let a hand-picked module set claim a standard score.
- Build invasive hardware fingerprinting to catch cheaters.

## Brand

```
DARC//BENCH
Deployment · Application · Runtime · Compute
Tombatossals Softworks LLC
```

Dark, precise, high-contrast. Electric cyan, cobalt blue, controlled violet as
accents — never as the sole carrier of meaning. Professional instrumentation,
not a gaming product. The tone is a measuring instrument that tells you when it
is uncertain.

## Success

**Year 1:** the tool people are told to run when they ask "is this VPS any
good"; a calibrated scoring model; a public leaderboard with results that
survive scrutiny.

**Year 3:** a score providers quote because buyers ask for it; the benchmark a
journalist cites; an independent methodology nobody can accuse of being for
sale.

**The failure mode to avoid:** becoming another number generator. If DARCBench
publishes a score that turns out to be indefensible, the correct response is to
invalidate it publicly, not to explain it away.
