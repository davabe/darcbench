# Cross-OS characterisation runbook

How to measure what the same machine does under two operating systems, and what
to conclude from the difference.

This is the first step of client-line calibration, not calibration itself. It
produces raw measurements and a residual. It produces no score.

**Why it exists:** [ADR-0016](adr/0016-client-reference-darc-ref-c1.md) commits
to disclosing the cross-OS delta rather than correcting it. That commitment was
unmeasurable when it was written — `darcbench-agent` is the server line and is
Linux-only, and `darcbench-core` is a library with no entry point.
`darcbench-characterise` is the instrument that closes the gap.

---

## 0. What you need, and what will not do

**One physical machine, booted natively into each operating system.** For
DARC-REF-C1 that is the Ryzen 9 9900X / RTX 5080 host.

**A VM will not do. WSL2 will not do.** Both virtualise the thing being
measured, so the delta you get is the hypervisor's, not the operating system's.
Two drives, or a dual boot, or one drive swapped — anything where the silicon is
identical and the kernel is not.

**Nothing else running.** No browser, no IDE, no indexer, no antivirus scan. The
agent has a runtime load ceiling that would catch a busy machine; this binary
does not, because it deliberately has no `/proc` dependency. That check is your
job here.

**Same power and thermal conditions.** Mains power, same ambient temperature,
same case fans. Laptop on battery is a different machine.

---

## 1. Build

On each operating system, from a clean checkout of the same commit:

```bash
cargo build --release -p darcbench-characterise
```

`rust-toolchain.toml` pins the compiler, so `rustup` installs and selects the
same version on both. That pin is load-bearing here: two rustc versions produce
different code, and a delta measured across two compilers measures the
compilers.

**Release only.** A debug build runs several times slower and is not comparable
to anything — the same rule as comparability rule 5 in
[SCORING-SYSTEM](SCORING-SYSTEM.md).

Record `rustc --version` on both. They must match.

---

## 2. Run

```bash
./target/release/darcbench-characterise --label linux --profile deep --passes 5 \
  > linux.csv 2> linux.ndjson
```

and, after rebooting into the other system:

```bash
./target/release/darcbench-characterise --label windows --profile deep --passes 5 \
  > windows.csv 2> windows.ndjson
```

**Leave the machine alone while it runs.** Do not use it, do not let it sleep.

`--passes 5` runs the whole suite five times. That is not decoration: a cross-OS
delta means nothing without the within-OS spread to compare it against, and more
repetitions inside one pass would not give that — they widen one sample of one
machine state. Each pass re-calibrates, re-warms and re-schedules.

Take a break of at least ten minutes between passes if the machine runs hot. The
binary does not enforce one.

---

## 3. What you have

`*.csv` — one row per repetition:

```
label,target,os,arch,profile,pass,module,module_version,metric,unit,direction,rep,warmup,value,duration_ms
```

`*.ndjson` — provenance, one object per module per pass: the calibrated work
sizes, the thread count, the ISA dispatch the run actually took, and any
warnings. **Two CSVs with similar numbers and different calibrations are not the
same measurement**, and this is what makes that visible. Check it before
comparing anything.

---

## 4. Analysis

Drop the warm-ups first — `warmup=true` rows are streamed as evidence and are
never scored.

**Per-OS headline:** for each `(os, module, metric)`, the median of `value`
across every measured row of every pass.

**Within-OS spread:** for each `(os, module, metric)`, take the median per pass,
then the coefficient of variation of those per-pass medians. This is the number
that says whether a delta is real.

**The delta:** `windows_median / linux_median - 1`, per metric.

**The decision rule.** A delta is only a finding if it is larger than the
within-OS spread on both sides. A 4% delta between two systems that each vary by
6% pass-to-pass is noise, and reporting it as an OS effect would be exactly the
kind of unfounded number this project exists not to publish.

Expect the three metrics to behave differently, and treat that as signal:

| Metric family | What a delta there probably means |
|---|---|
| `crypto_sha256`, `float_matmul` | Almost pure compute. A delta here points at codegen or thread placement, not at the OS |
| `compress_deflate`, `json_roundtrip`, `integer_sort` | Allocation-heavy. A delta here is the allocator (see below) |
| `memory.bandwidth` | Page size, huge pages and NUMA policy. The most OS-sensitive of the set |

---

## 5. The allocator — a revision of ADR-0016

ADR-0016 lists the allocator as a variable to **eliminate**, by bundling one
across all targets. `darcbench-characterise` does not do that, and the reason is
a trade the ADR did not weigh.

Every production-grade replacement allocator — mimalloc, rpmalloc, snmalloc — is
C or C++. The workspace is deliberately pure Rust apart from `rusqlite`, and the
`rustls` dependency comment records that `aws-lc-sys` was rejected precisely for
needing `cmake`. Adding a C toolchain requirement to the one crate that must
build on MSVC, on macOS and on Linux would put build friction on exactly the
platforms this work just made viable, to remove a variable nobody has yet shown
to be large.

**So the allocator is a recorded variable, not an eliminated one.** The `target`
column carries the full triple — `x86_64-pc-windows-msvc` versus
`x86_64-unknown-linux-gnu` — which names the allocator and the CRT, and the
allocation-heavy metrics are called out above so its contribution is visible in
the results rather than hidden in them.

**If the residual on the allocation-heavy metrics is large, bundling becomes the
round-two experiment** and the C dependency is then worth its price, because at
that point it buys a known quantity instead of a guess. That is the order the
evidence justifies.

---

## 6. What to do with the result

1. **Fill in DARC-REF-C1.** The blank memory and storage rows in
   [ADR-0016](adr/0016-client-reference-darc-ref-c1.md) get the real values from
   the host, and the specification names the OS it was characterised under.
2. **Publish the residual.** Not as a correction factor — as measured evidence,
   alongside the raw CSVs. The incumbents assert cross-platform comparability
   without publishing what is left over; this is the position that differs.
3. **Add rule 7 to comparability.** OS joins the tuple in
   [SCORING-SYSTEM §6](SCORING-SYSTEM.md), displayed and never silently pooled,
   the same treatment execution scope gets under rule 6.
4. **Three hosts before `dcs/1.0.0`.** This runbook characterises one machine.
   The three-host rule in [SCORING-SYSTEM §3.1](SCORING-SYSTEM.md) still gates a
   calibrated model, and the client reference stays `calibrated: false` until it
   is satisfied.

---

## 7. Known limits of this instrument

- **`MachineFacts` is left at its default on every platform**, so
  `memory.bandwidth` sizes its working set from an assumed cache rather than the
  real one. This is deliberate — populating it would feed the two runs different
  inputs and fold the difference between two inventory implementations into a
  number reported as an OS difference — but it means the absolute figures here
  are not comparable with a full agent run. The delta is unaffected.
- **No thermal or load guard.** The agent's watchdog is `/proc`-based and does
  not exist here. A thermally throttled or contended run looks like a slow
  operating system.
- **Two modules only.** `storage.mixed` needs `O_DIRECT` and `network.transfer`
  reaches the internet; neither belongs in a cross-OS compute comparison, and
  neither is in the portable engine.
- **This is one machine.** Everything here describes DARC-REF-C1's silicon under
  two kernels. It says nothing about how any other machine behaves.
