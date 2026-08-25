<div align="center">

# DARC//BENCH

**Deployment · Application · Runtime · Compute**

A benchmark suite for real servers — dedicated, VPS, cloud, bare metal, homelab
and web hosting — that measures what actually makes a server good at hosting
things, and refuses to publish a number it cannot defend.

*Tombatossals Softworks LLC*

</div>

---

## Status

**Phase 1 complete; Phase 2 in progress. Working end to end, honest about what
it is not.**

| | |
|---|---|
| ✅ Working | Agent, embedded dashboard, live event streaming, cancellation, `cpu.mixed`, `memory.bandwidth`, `storage.mixed` and `network.transfer` modules, scoring pipeline, signed result bundles, HTML/JSON/NDJSON reports, tamper detection |
| ⚠️ Not calibrated | The scoring model `dbs/0.1.0-dev` has **not** been calibrated against reference hardware. Raw measurements are real; scores are development output |
| ⏳ Not built yet | Web, PHP, Node.js, database, WordPress and deployment modules; the control plane |

Every score this build produces is flagged `uncalibrated` and every run is
`Partial`, because four of five required categories are implemented. That is the
correct answer, not a limitation we are hiding.

`network.transfer` is the only module that contacts anything outside the
machine. It reaches a compile-time list of public measurement endpoints, sends
them nothing about you or your machine, is bounded by a transfer ceiling the
module enforces, and names both the volume and every operator in preflight
before the run starts. The `quick` profile excludes it, so a first run on an
unfamiliar server opens no outbound connection at all.

---

## Why this exists

The server benchmarking landscape is split between two things that do not meet.

On one side, excellent single-purpose tools — `fio`, `sysbench`, `wrk`,
`iperf3` — that measure one subsystem precisely and leave you to interpret the
numbers. On the other, shell scripts that run three of those tools and print
their output, which is genuinely useful for a quick sanity check and cannot tell
you whether a €40/month VPS will host your WooCommerce store.

Neither answers the question people actually have: **is this server good, for
what I am going to do with it, and can I trust the answer?**

DARCBench is built around four commitments:

1. **A defensible methodology.** Calibrated iteration counts, warm-up
   repetitions, medians over means, published variance, non-parametric
   confidence intervals, flagged-not-deleted outliers.
2. **Raw measurements are the product.** Scores are derived and recomputable.
   Every repetition is kept, in physical units. A new scoring model can rescore
   every historical run without re-running a single benchmark.
3. **Results you can check.** Bundles are Ed25519-signed over canonical JSON.
   The server never trusts a bundle's own verdict — it recomputes every score
   from the raw metrics. Edit a score and both checks fail.
4. **Safe on a production machine.** No configuration is ever modified. No port
   is ever taken over. No production database is ever touched. Preflight
   classifies the risk, shows what the run will cost, and refuses when it should.

---

## Quick start

**Requires:** Linux (x86-64 or arm64). Node.js 22 and pnpm ≥ 10 are optional —
without them the agent serves a built-in console instead of the React dashboard.

The toolchains are pinned, so you do not choose versions:

| | Pinned in | Version |
|---|---|---|
| Rust | [`rust-toolchain.toml`](rust-toolchain.toml) | 1.97.1 — `rustup` installs and selects it automatically |
| pnpm | `packageManager` in [`apps/web/package.json`](apps/web/package.json) | 11.20.0 — pnpm ≥ 10 self-selects it |

Both pins are verified in CI: a job fails if the running version is not the
pinned one. The Rust pin is *not* the MSRV — `rust-version = "1.82"` in
`Cargo.toml` remains the minimum a consumer needs. The pin is the single version
we all build, lint and test with, so nobody sees diagnostics nobody else sees.

> **On dependency build scripts.** [`apps/web/pnpm-workspace.yaml`](apps/web/pnpm-workspace.yaml)
> declares `allowBuilds: { esbuild: false }`. esbuild's postinstall script is
> deliberately **not** run: esbuild 0.28 ships its native binary through
> platform-specific optional dependencies, so the build works without it, and
> not executing third-party install scripts is the more conservative
> supply-chain position. The declaration is not optional — with the decision left
> undeclared, pnpm 11 exits non-zero with `ERR_PNPM_IGNORED_BUILDS` and breaks
> `install && build`. Use `allowBuilds` here rather than
> `pnpm.ignoredBuiltDependencies` in `package.json`: the latter is read by
> pnpm 10 only and silently ignored by pnpm 11.

```bash
git clone https://github.com/davabe/darcbench && cd darcbench

# Optional: build the full dashboard (skip for a Rust-only build)
(cd apps/web && pnpm install && pnpm build)

cargo build --release          # binary at target/release/darcbench
```

> **Run pnpm from `apps/web`, not `pnpm --dir apps/web` from the root.** Both
> Corepack and pnpm's own version manager resolve `packageManager` from the
> *invocation* working directory; Corepack never sees `--dir` at all. From the
> repository root — which has no `package.json` — a Corepack-managed setup
> silently runs whatever pnpm it defaults to instead of the pinned one, which
> defeats the pin. The subshell keeps your own shell where it was.

### Check the machine before touching it

```bash
./target/release/darcbench doctor
```

```
DARCBench doctor

  agent            0.1.0
  bundled web UI   yes
  scope            VirtualMachine
  cpu              Intel(R) Xeon(R) Processor @ 2.80GHz
  logical cpus     4
  memory           15.7 GiB
  risk class       HeavyLoad
  estimated run    ~1 min (quick profile)
  disk writes      2 GiB fixture, ~11 GiB written in total

  PASS ready to benchmark
```

### Benchmark from the terminal

```bash
./target/release/darcbench run --profile quick
```

### Or open the dashboard

```bash
./target/release/darcbench serve
```

The agent prints a loopback URL containing a one-time token. From another
machine, forward the port rather than exposing it:

```bash
ssh -N -L 7842:127.0.0.1:7842 user@your-server
```

Then open the printed URL locally. **The dashboard is never available without
authentication, on any interface.**

### Verify a result

```bash
./target/release/darcbench verify ~/.local/state/darcbench/runs/run_*/bundle.json
```

```
  signature        valid
  score recompute  matches raw metrics
  verdict          Partial
```

Change one number in that file and both lines turn red.

---

## Commands

```
darcbench                     what this is, and what to run next
darcbench doctor              readiness and risk assessment
darcbench inspect             full system inventory as JSON
darcbench serve               local dashboard (loopback by default)
darcbench run --profile ...   benchmark from the terminal
darcbench status              recent runs
darcbench compare <a> <b>     two runs, metric by metric
darcbench report [run_id]     stored bundle, or --html
darcbench verify <bundle>     signature + score recomputation
darcbench prune               retention policy; reports unless --confirm
darcbench uninstall           reports what it would remove; --confirm to do it
```

Global: `--json`, `--no-color`, `--non-interactive`, `--home <dir>`,
`--log <level>`.

## Profiles

| Profile | Duration | Purpose | Status |
|---|---|---|---|
| `quick` | 3–6 min | First look at a VPS · makes no outbound connection | ✅ |
| `standard` | 10–20 min | The comparable score | ⏳ needs Phase 3 modules |
| `deep` | 30–60 min | Larger datasets, more repetitions | ⏳ |
| `endurance` | 1 h | Throttling, burst credits, noisy neighbours | ✅ |
| `read-only` | 4–8 min | Sensitive production hosts | ⏳ |
| `web` | 8–15 min | Static, TLS, PHP, Node, WordPress, DB | ⏳ |

A profile with no implemented modules says so and refuses to start, rather than
running a subset and calling it that profile.

`read-only` resolves to compute and memory only, and is structurally `Custom`:
a run that cannot measure write throughput, write latency or fsync cost must not
be able to claim a comparable total.

`endurance` is the only profile that repeats. It runs its module set in cycles
for an hour, then reports how much of the opening performance survived and
*why* it did not — thermal throttling, burst credits running out, a noisy
neighbour, or an honest "the telemetry does not explain this". A three-minute
benchmark of a burstable instance measures its credit balance rather than the
instance, and this is the profile that does not.

---

## Repository layout

```
crates/
  darcbench-protocol/   wire types: ids, metrics, statistics, events, run lifecycle
  darcbench-inventory/  read-only discovery, telemetry, privacy redaction
  darcbench-scoring/    the pure scoring model
  darcbench-modules/    module contract, workloads, allow-list registry
  darcbench-report/     bundles, canonical JSON, Ed25519 signing, validation, HTML
  darcbench-agent/      CLI, dashboard server, orchestration, preflight
apps/
  web/                  React + TypeScript dashboard, compiled into the binary
  control-plane/        Phase 5, not started
benchmarks/             machine-readable module manifests
docs/                   methodology, architecture, threat model, ADRs
scripts/ tests/         validation and end-to-end checks
```

## Documentation

**Start here**
[Product Bible](docs/PRODUCT-BIBLE.md) ·
[Architecture](docs/ARCHITECTURE.md) ·
[Roadmap](docs/ROADMAP.md) ·
[Public results](docs/PUBLIC-RESULTS.md) ·
[Calibration runbook](docs/CALIBRATION-RUNBOOK.md)

**The methodology**
[Benchmark methodology](docs/BENCHMARK-METHODOLOGY.md) ·
[Scoring system](docs/SCORING-SYSTEM.md) ·
[Module spec](docs/BENCHMARK-MODULE-SPEC.md) ·
[Research sources](docs/RESEARCH-SOURCES.md)

**Building on it**
[API](docs/API.md) ·
[Real-time protocol](docs/REALTIME-PROTOCOL.md) ·
[Data model](docs/DATA-MODEL.md)

**Operating it**
[Installer & discovery](docs/INSTALLER-AND-DISCOVERY.md) ·
[Operations](docs/OPERATIONS.md) ·
[Privacy](docs/PRIVACY.md) ·
[Threat model](docs/THREAT-MODEL.md)

**Decisions** — [ADR index](docs/adr/)

---

## What DARCBench will not do

Constraints, not backlog items:

- **Never benchmark an arbitrary URL.** A load generator pointed at a
  user-supplied target is a DDoS tool with a scoring model.
- **Never query a cloud metadata endpoint.** Those responses carry credentials.
- **Never modify web server, panel or firewall configuration.**
- **Never touch a production database.** Modules create and destroy their own.
- **Never test a raw block device.**
- **Never publish an unauthenticated dashboard.**
- **Never let a hand-picked module set claim a standard score.**

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). The bar for a change that affects
measurement or scoring is a test that would fail without it.

```bash
cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test --workspace --release
(cd apps/web && pnpm build)
./scripts/e2e.sh
```

## Security

Report vulnerabilities privately — see [SECURITY.md](SECURITY.md). Please do not
open public issues for security problems.

## Licence

Apache-2.0 for the agent, protocol, scoring model and modules; AGPL-3.0 for the
control plane. Rationale in [LICENSE-STRATEGY.md](LICENSE-STRATEGY.md).
