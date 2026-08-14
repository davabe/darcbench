# Competitive analysis

Accessed 2026-08-03. Sources in [RESEARCH-SOURCES.md](RESEARCH-SOURCES.md).

**No proprietary algorithm, workload, asset or scoring formula has been copied.**
This analysis informed *what problems to solve* and *what mistakes to avoid*.

## Summary

| Product | Measures well | Weak for our purpose | Lesson taken |
|---|---|---|---|
| **SPEC CPU2017** | Rigorous, audited, fixed reference machine | Licensed, CPU-only, strict publication rules, nothing web | Geometric mean over normalised ratios; fixed reference; conservative reportable-result rules |
| **Geekbench 6** | Cross-platform, well-documented internals, clear reference anchor | Desktop/mobile framing, short workloads, proprietary, no server or web workloads | Two-level aggregation; reference anchor; separate single- and multi-core |
| **PassMark** | Broad, huge comparison corpus | Weighted harmonic mean is hard to defend; averages submissions per CPU model, destroying per-machine context; desktop-oriented | **Avoided:** never average across machines; never hide per-run variance |
| **3DMark / PCMark** | Superb presentation; strong brand | Not server relevant | Presentation matters; a benchmark people enjoy reading gets run |
| **Phoronix Test Suite / OpenBenchmarking** | Enormous test corpus, dynamic repetition on high stddev, open | Result quality varies by test; no single defensible composite; heavy install | Re-run on high variance — but **publish** the variance rather than only using it as a stopping rule |
| **UnixBench** | Historic reference | Ancient workloads, meaningless composite index | **Avoided:** an index nobody can explain is worse than no index |
| **YABS** | Genuinely great UX: one command, no root, no setup, works everywhere | Wraps three tools and prints output; no scoring model, no integrity, no web workloads, no statistics | **Usability bar.** DARCBench must be this easy and far more rigorous |
| **`bench.sh`** | Instant, ubiquitous | `dd`-based disk numbers are close to meaningless | **Avoided:** never ship a measurement we know is wrong because it is fast |
| **sysbench** | Good OLTP and CPU primitives | Primitives, not a suite; no scoring; easy to misconfigure | Useful as a possible component, never as the product |
| **fio** | The definitive storage tool | Enormous configuration surface; easy to measure the page cache by accident | Adopt as a declared dependency; encode the correct configuration so users cannot get it wrong |
| **stress-ng** | Excellent stressor coverage | A stressor is not a benchmark; no comparable scoring | Useful for endurance and thermal work |
| **wrk / wrk2** | wrk2 solves coordinated omission correctly | C, no structured output, single-endpoint | **Methodology adopted:** open model, constant rate, latency from intended send time |
| **ApacheBench** | Ubiquitous | Single-threaded, closed loop, coordinated omission, misleading percentiles | **Avoided entirely** |
| **Siege** | Simple | Same closed-loop problems | Avoided |
| **k6** | Scriptable, open model, good docs on injector sizing | JS runtime is heavier than the target sometimes needs | Adopt the guidance: the injector must never be the bottleneck |
| **vegeta** | Constant-rate, efficient, good latency reporting | Library-shaped, no scoring | Strong candidate as a Phase 3 component |
| **iperf3** | The standard for raw throughput | Needs a cooperating endpoint; not application-layer | Adopt for raw network; separate clearly from HTTP throughput |
| **Speedtest CLI / Cloudflare Speed Test** | Real internet paths, many endpoints | Single endpoint ≠ universal capacity; TOS constraints | Use within policy; **never** claim one endpoint represents network capability |
| **Redis benchmark** | Simple, canonical | Trivially misconfigured (pipelining) | Record configuration in the bundle or the number is meaningless |
| **pgbench** | Canonical PostgreSQL workload | Default scale factor is far too small; measures cache | Sizing must exceed shared_buffers by a documented multiple |
| **HammerDB** | Serious OLTP, correct about TPC naming | Heavy, complex setup | **Adopt the naming discipline**: derived workload, distinct name, distinct metric, never claim tpmC |
| **TPC-C / TPC-H** | The rigorous standard | Trademarked; audit required; unusable unofficially | Follow the HammerDB precedent exactly |
| **VPSBenchmarks** | Real longitudinal VPS data, provider comparison | Closed methodology, limited providers, no self-service verification | **Closest competitor in intent.** Differentiate on open methodology and verifiable results |
| **Cloud Spectator** | Professional cloud benchmarking reports | Commercial, closed, paid | The neutrality question is the whole game — see COMMERCIAL-STRATEGY.md |
| **ServerScope** | Right idea, right era | Effectively defunct | The space is open, and the reason it is open is worth understanding |

## Patterns worth naming

**Everyone measures CPU; almost nobody measures hosting.** The gap between "this
CPU scores X" and "this machine will serve your WooCommerce store in Y ms" is
where DARCBench lives.

**Statistical rigour and usability are treated as opposites.** SPEC is rigorous
and unusable; YABS is usable and not rigorous. Nothing forces that trade-off
except effort.

**Nobody solves result integrity.** Every open suite emits editable text. If
DARCBench does one novel thing, signed bundles plus server-side score
recomputation is it.

**Closed-loop load generation is still everywhere.** ApacheBench and Siege are
still recommended daily, and both systematically hide the tail latency that
determines whether a site feels fast.

**Composite indexes get abandoned rather than versioned.** UnixBench's index
survives as a number nobody can explain. The answer is not "no composite" — it
is a versioned, published, recomputable one.

## Mistakes DARCBench must not repeat

1. A composite score with no published formula.
2. Averaging across machines and losing per-run context.
3. `dd` as a storage benchmark.
4. Closed-loop load generation with percentile claims.
5. Hiding variance behind a single number.
6. Changing scoring silently between releases.
7. Benchmarking the page cache and calling it storage or memory.
8. Publishing a workload composite computed from a fragment of its inputs.
9. Letting a hand-picked module set claim a standard score.
10. Trusting a client-supplied score without recomputing it.

## Legal position

- No SPEC or TPC workload is used, reimplemented or redistributed.
- `TPC-C` and `tpmC` are never used; derived workloads get distinct names and
  distinct metrics.
- No proprietary benchmark binary is bundled.
- GPL tools (fio, sysbench) are invoked as declared external dependencies, never
  vendored or linked.
- The DARCBench methodology and scoring formulas are published under CC BY 4.0
  so they can be audited and cited independently.
