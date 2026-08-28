# Field evidence

What DARCBench has actually been run on, and what those runs proved or
disproved. Everything here is a measurement, not a design intention: this file
exists so that claims about the scoring model can be checked against numbers
somebody can re-derive, and so that a finding is not rediscovered twice.

[SCORING-SYSTEM.md](SCORING-SYSTEM.md) says what the model is meant to do;
[CALIBRATION-RUNBOOK.md](CALIBRATION-RUNBOOK.md) says how to gather the data
that would calibrate it. This says what the data gathered so far shows.

---

## Corpus 2026-08 — three hosts, `quick` profile

Three bare-metal hosts, one `quick` run each, all from the same static musl
binary (`sha256:2e8308c5…e248a`), so `build_target` and `agent_build_hash`
are identical across the three and the numbers are comparable to each other.

| | H1 | H2 | H3 |
|---|---|---|---|
| CPU | Xeon E-2274G, 4c/8t, 4.9 GHz boost | Xeon E5-1620 v2, 4c/8t, 3.9 GHz | 2 × Xeon E5-2699 v3, 36c/72t, 3.6 GHz |
| Released | 2019 | 2013 | 2014 |
| Memory | 64 GiB | 32 GiB | 32 GiB, 2 NUMA nodes |
| Storage | 2 × Samsung MZQLB960 NVMe, md RAID | Intel S3500 SATA SSD | TME38MB256 NVMe |
| OS | Ubuntu 24.04, k6.8 | Ubuntu 26.04, k7.0 | Ubuntu 24.04, k6.8 |
| Governor | `performance` | `schedutil` | `performance` |
| load1 at start | 0.00 | 1.46 | 1.12 |
| Board DMI | ASRock Rack E3C246D4U2-2T | Supermicro X9SRE | `Default string` |

**This is not a calibration set, and none of it is used as one.** The runbook
asks for three hosts of one specification, ten `deep` runs each. This is three
different machine classes, one `quick` run each. It cannot produce a per-host
median, it measures three of five categories, and its hosts are eleven years
apart. Nothing below changes an anchor value.

What it can do is what a heterogeneous corpus is *better* at than a
same-specification one: it separates properties of the scoring model from
properties of a machine, because a defect that appears identically on a 2013
Sandy Bridge-EP and a 2019 Coffee Lake and a dual-socket Haswell is not a
property of any of them.

### What it proved

Every bundle verified on a machine that did not produce it: signature valid,
scores recomputed from raw metrics, verdict reproduced. Three hosts, two of
them not the author's, an inventory the verifier had never seen. The
end-to-end chain — collect, measure, score, canonicalise, sign, transport,
verify — works on foreign hardware.

The facet split is doing its job. H1, the 4.9 GHz quad, scores single-core 854
and multi-core 265. H3, the 72-thread dual socket, scores 577 and 816. A single
total would have called these machines nearly equal — 721 against 732 — which
they are not for any actual workload. The facets say why.

`latency_fsync.mean` discriminates exactly what it was built to discriminate:
0.021 ms on H1's enterprise NVMe with power-loss protection, 0.098 ms on H2's
SATA SSD, 0.908 ms on H3's consumer NVMe. A 44× spread on the one storage
metric that cannot be faked by a cache.

`direct_io: true` on all three. The storage figures are device figures.

### What it disproved

#### 1. The reference anchors are not merely uncalibrated, they are incoherent

If the anchors were coherent, a host's measured/anchor ratios would cluster:
one number saying how much faster or slower than DARC-REF-1 that machine is.
They do not. Within a single host the ratios span **76×, 94× and 20×**.

Dividing each ratio by its own host's median isolates the anchor from the
machine. A metric that agrees with the rest of its host reads 1.00:

| metric | H1 | H2 | H3 | geo-mean |
|---|---|---|---|---|
| `cpu.mixed/crypto_sha256.multi` | 0.06 | 0.06 | 0.21 | **0.09** |
| `cpu.mixed/crypto_sha256.single` | 0.21 | 0.26 | 0.13 | **0.19** |
| `cpu.mixed/compress_deflate.multi` | 0.16 | 0.17 | 0.58 | **0.25** |
| `cpu.mixed/compress_deflate.single` | 0.41 | 0.54 | 0.25 | **0.38** |
| `storage.mixed/sequential_read.qd1` | 0.60 | 0.25 | 0.69 | 0.47 |
| … 19 metrics between 0.6 and 1.5 … | | | | |
| `storage.mixed/random_write_4k.qd1` | 2.78 | 2.35 | 2.32 | **2.48** |
| `cpu.mixed/float_matmul.single` | 3.08 | 3.68 | 1.75 | **2.71** |
| `cpu.mixed/integer_sort.single` | 4.65 | 5.78 | 2.50 | **4.06** |

The extremes agree across three unrelated machines, which is what makes this a
statement about the anchors rather than about the hardware. `crypto_sha256.multi`
and `integer_sort.single` are **45× apart in relative error**, so the Compute
score today is dominated by whichever anchor happens to be most wrong.

Most of this is the ordinary uncalibrated gap and calibration fixes it. One
part is not — see below.

#### 2. `crypto_sha256` is substantially an instruction-set detector

The `crypto_sha256.single` anchor is 1900 MiB/s. The three hosts measured 289,
192 and 206 MiB/s. None of them reports `sha_ni`; DARC-REF-1's Ryzen 7 7700
has it, and `sha2` dispatches to the hardware path when it is present.

So the ~7× gap is not a calibration error and **re-measuring the anchor on real
DARC-REF-1 hardware will not close it** — the reference has the instruction
too. It lands as a flat ~7× penalty on every pre-Ice-Lake Intel part, inside a
metric weighted like any other, in a category weighted 0.26.

This is a real property of the silicon and it is scored as measured. What was
wrong is that nothing in the bundle said which path had run, so a reader could
not distinguish a slow CPU from a CPU missing one instruction. `cpu.mixed` now
records `isa_dispatch` in its context and warns when the extensions are absent.

Whether one instruction should carry that much of Compute is a question about
the metric's weight, and it is on the roadmap as one. It is not a question the
calibration run answers.

#### 3. A tail quantile was making healthy runs unrankable

H2's `storage.mixed` came back `ExcessiveVariance`, and `ExcessiveVariance`
means `Partial`, and `Partial` is not rankable. The metric responsible was
`latency_write_4k.p99`, which varied 65% between repetitions — while every
throughput metric in the same module held within 4.4%.

A p99 is estimated from the slowest 1% of a finite sample: a handful of
operations decide it, and it moves between repetitions on a machine behaving
perfectly. Its coefficient of variation is not evidence about the machine,
which is the only thing the validator's bound is entitled to conclude from it.
The module knew this and never judged the metric; the validator applied a
blanket bound to everything and did.

This is the same defect as `tcp_connect.jitter`, in a second place, for a
different underlying reason — so the fix is a second declared property,
`tail_quantile`, and one function in the validator that decides exemption, so
the next such metric is added beside its reasoning rather than as another
clause. `storage.mixed` now remarks on an erratic tail informationally, which
keeps the observation without disqualifying the device. An erratic write tail
is worth knowing about precisely on the tired hardware this would have refused
to rank.

The three bundles above still report the old verdict when re-verified, and
correctly so: the exemption is a property the producing module declares, and a
bundle written before the property existed does not declare it. A re-run
produces the corrected verdict.

#### 4. Signatures froze the metric schema, and nobody had said so

Signature verification re-serialises the *parsed* bundle and compares canonical
bytes. Any field that always appears therefore changes the canonical form of
every bundle written before it existed and invalidates every signature already
issued — silently, at the moment the field is added.

Before this corpus there were no bundles in the field and the constraint was
invisible. There are now. `tail_quantile` is declared
`skip_serializing_if = "std::ops::Not::not"` for that reason, which is why all
three bundles still verify against the binary that added it, and a test in
`darcbench-protocol` pins the property so the next added field cannot quietly
break it.

### What it left open

**A `quick` run is `Partial` by construction.** `Quick` deliberately omits
`network.transfer` — it is the first thing anyone runs on a machine they are
evaluating, and keeping it egress-free is a deliberate choice — and it does not
include `web.static`. Both are required categories, so all three runs are
`Partial` and none is rankable. That is honest, and it is also the entire
first-run experience for a project whose point is a public leaderboard. Whether
there should be an egress-free rankable profile is a design question this
corpus raises and does not answer.

**Nothing gates the governor or pre-existing load.** H2 ran with `schedutil`
and load1 1.46 — the exact conditions the runbook forbids — and both facts are
faithfully recorded in the bundle while nothing acts on either. Its scores are
depressed by an unknown amount that no reader can separate from the hardware.
Preflight warns above `BUSY_LOAD_PER_CPU`; 1.46 across 8 CPUs is below it, and
the governor is not checked at all.

**`scope: unknown` on a bare-metal host.** H3's DMI reads `Default string`, a
placeholder DARCBench correctly refuses to treat as identification, so it
declines to call the machine bare metal. That is the right call for an
unattested field. Whether `unknown` should be rankable, and on what evidence, is
open.

**Thin calibration on the largest host.** H3's `memory.bandwidth` sized
`random_read.single` at 1 pass per repetition and `triad.multi` at 5, against
7 and 45 on H1. A repetition with a single pass has no internal averaging, and
H3 is also the host whose `memory.bandwidth` tripped the variance bound. A
minimum-passes floor is worth investigating.

---

## Corpus 2026-08b — the same three hosts, `deep` profile

The same three machines, `deep` instead of `quick`, run as an ordinary user
rather than root. All three completed, ~5 minutes each, and all three verified.

The important structural difference: **all five required categories were
measured**, so `total_is_standard` is true and
`missing_required_categories` is empty. These are the first runs that produce
a standard total. They are still not rankable, for reasons below that are
about the suite rather than the hardware.

| | H1 E-2274G | H2 E5-1620 v2 | H3 2×E5-2699 v3 |
|---|---|---|---|
| Total | 1185 | 649 | 1146 |
| Compute | 474 | 340 | 716 |
| Memory | 842 | 534 | 616 |
| Storage | 1184 | 417 | 978 |
| Network | 2666 | 1135 | 486 |
| Web | 3951 | 2496 | 6400 |
| single-core facet | 853 | 582 | 570 |
| multi-core facet | 263 | 198 | 900 |
| balance index | 0.34 | 0.46 | 0.46 |

### What it disproved

#### 5. `network.transfer` disqualified every run it completed

All three hosts came back `Partial` with the same two reasons, and one of them
was the module failing itself for succeeding.

`download_bytes` deliberately sizes every transfer so the whole run fits
*exactly* inside the 512 MiB ceiling — that is what stops a fast link pulling
more from a third party than a slow one. The module then warned on
`budget.exhausted()`, which is `spent() >= ceiling`, with the text "some
measurements were cut short". Measured on all three hosts: `bytes_spent` equal
to `ceiling_bytes` **to the byte**, all seven metrics present, nothing skipped.

`ValidationFailed` degrades a module, a degraded module makes the run
`Partial`, and `Partial` is not rankable. `network.transfer` is in both `deep`
and `standard`, so **no profile could produce a rankable result at all** — the
quick corpus could not see this because `Quick` omits the module.

The warning now fires only when the ceiling actually stopped a shape from being
measured, which each affected metric already reported by name. The volume that
crossed the wire is disclosed in `context.transfer`, where it always belonged.

Verified against the reproduced condition rather than the unit test alone: a
`deep` run after the fix reported `bytes_spent` 536870912 against a
`ceiling_bytes` of 536870912 — the same exact exhaustion — and the warning did
not fire.

That leaves `ttfb.mean` as the **only** thing between a `standard` or `deep`
run and rankability, and it is marginal rather than reliable: the same machine
produced a `standard` run with `network.transfer` completed and no warnings at
all, then a `deep` run at exactly 30.0% against the 30% bound. Whether a run is
rankable currently turns on whether one request to a public CDN stalled. See
the open question below.

#### 6. The agent created a scratch directory its own check refused

`node.runtime` failed on H2 — a host with Node.js installed — with "the scratch
directory `/home/ubuntu/.local/state/darcbench/scratch` is group-writable (mode
775)".

Nobody made it 775. `create_dir_all` applies the process umask, Ubuntu ships
002 for a user with a private group, and the security check immediately below
the creation refused what the creation had just produced. The check is right —
whoever can write that directory chooses what the interpreter executes — so the
fix is on the creation side: `DirBuilder` with an explicit `0700`, which the
umask cannot loosen.

A pre-existing scratch from an older agent is still refused rather than
repaired. Tightening the mode would close the window from now on but says
nothing about what was planted while it stood open, and an interpreter reads
more from its working directory than the one script that function writes. The
refusal now names the command that clears it.

#### 7. The Web anchors are uniformly 3–6× too low

With Web and Network measured for the first time, the anchor-coherence table
extends to all five categories. Host medians are sane — 0.92×, 0.53× and 0.90×
of DARC-REF-1, which is about right for a 2019 quad, a 2013 quad and a 2014
dual-socket. The spread within each host is 171×, 73× and 136×.

| metric | H1 | H2 | H3 | geo-mean |
|---|---|---|---|---|
| `cpu.mixed/crypto_sha256.multi` | 0.05 | 0.06 | 0.19 | **0.08** |
| `cpu.mixed/crypto_sha256.single` | 0.17 | 0.22 | 0.12 | **0.16** |
| `cpu.mixed/compress_deflate.multi` | 0.12 | 0.17 | 0.56 | **0.22** |
| `cpu.mixed/compress_deflate.single` | 0.31 | 0.39 | 0.24 | **0.31** |
| … 24 metrics between 0.39 and 2.3 … | | | | |
| `web.static/requests.small_keepalive` | 1.80 | 1.95 | 8.67 | **3.13** |
| `cpu.mixed/integer_sort.single` | 3.60 | 4.19 | 2.40 | **3.31** |
| `web.static/connections.tls` | 4.32 | 4.35 | 4.58 | **4.42** |
| `web.static/throughput.medium` | 2.91 | 3.17 | 12.60 | **4.88** |
| `web.static/throughput.large` | 3.77 | 4.07 | 16.46 | **6.33** |

`connections.tls` is the cleanest signal in the corpus: 4.32, 4.35, 4.58 on
three unrelated machines. Agreement that tight across a decade of hardware is
not three machines behaving alike, it is one anchor being about 4.4× too low.

`throughput.large` and `throughput.medium` disagree between hosts as well as
with the anchor — 3.77 and 16.46 — which is the loopback-memcpy shape already
recorded under "known conditions" in the runbook. They need the metric looked
at, not just the anchor moved.

The whole Web module reading 3–6× high explains Web scoring 2496–6400 against a
1000 reference on machines that are otherwise at 0.5–0.9× of it.

**No anchor value is changed on this evidence either.** Three `deep` runs, one
per machine class, is still not a calibration set. What this buys is a ranked
list of which anchors to check first and what to expect.

### What it left open

**A tail dominates `ttfb.mean`, and the stability check is not robust to it.**
All three hosts exceeded the 30% bound — 53%, 79%, 79%. The distributions are
tight with one long-tailed exception: H3 ran 46–61 ms with a single 271 ms
repetition out of eleven; H2 32–45 ms with a single 201 ms. The module's own
MAD detector flags those samples, and the metric's reported *value* is the
median, which is robust. Its *stability verdict* is a mean-based coefficient of
variation, which is not — so one slow repetition out of eleven, on a public
CDN, degrades the module. Whether a robust dispersion measure should judge a
robustly-estimated metric is a decision that changes which runs are rankable
and what `median_cv` feeds into `stability_multiplier`, so it is on the roadmap
rather than changed here.

**`web.static` loopback bounds.** Two of three hosts exceeded the module's 20%
bound on a loopback HTTP measurement — `throughput.medium` at 23%,
`connections.plaintext` at 26%. Same question as above, different subsystem.

**Six of eleven modules need software the host may not have.** `php.runtime`,
`node.runtime`, `database.oltp`, `database.cache`, `wordpress.site` and
`deployment.container` all failed on all three hosts for want of PHP, Node or a
container runtime. That is the modules being honest rather than a defect — but
it means a `deep` run on a bare host measures five modules and reports six
failures, and a failed module makes the run `Partial`. The runbook now lists
the prerequisites.

`standard` already answers most of this: it is `cpu.mixed`,
`memory.bandwidth`, `storage.mixed`, `network.transfer` and `web.static` — the
five required categories and nothing else — and it omits the interpreter
modules deliberately, so that a machine without PHP is not told its missing PHP
is a fault. It is the rankable profile for a bare server. What remains is that
the calibration runbook asks for `deep`, which on an unprovisioned host reports
six failures for software nobody promised.

### Reproducing this

The analysis is arithmetic over the bundles and needs no special tooling: for
each anchored metric take `measured / anchor` (inverted where
`direction` is `lower_is_better`), divide by the host's median ratio, and
compare across hosts.

```bash
for b in run_*/bundle.json; do darcbench verify "$b"; done
```
