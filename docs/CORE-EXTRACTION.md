# Extracting `darcbench-core`

The mechanical plan for the crate boundary decided in
[ADR-0015](adr/0015-two-product-lines-one-engine.md): one measurement engine,
shared by the server and client product lines, with the compiler enforcing that
server workloads cannot leak into a client build.

**Run this on the Linux development host** ([DEVELOPMENT-HOST.md](DEVELOPMENT-HOST.md)).
Every step below has a verification gate, and the gates are the point: this is a
refactor of a well-tested crate and nothing here should be taken on trust.

## Why before the repository split

Splitting repositories first is the failure mode. Three repositories kept in
sync by hand diverge silently, and comparability is the product. Extract the
core inside the monorepo, prove it with CI on three platforms, and the eventual
split becomes mechanical. ADR-0015 records the trigger that fires it.

## What moves

Traced through every `use` and `crate::` reference in the crate. The compiler is
the final authority; this is the starting hypothesis, not a promise.

| File | Depends on | Moves |
|---|---|---|
| `module.rs` | protocol, serde, serde_json, thiserror | ✅ |
| `harness.rs` | protocol, `crate::module` | ✅ |
| `workloads.rs` | sha2, flate2, serde_json | ✅ |
| `cpu_mixed.rs` | protocol, `crate::{harness, module, workloads}` | ✅ |
| `memory_bandwidth.rs` | protocol, `crate::{harness, module, workloads}` | ✅ |

Everything else stays in `darcbench-modules`: `storage_mixed`, `network_*`,
`web_*`, `php_runtime`, `node_runtime`, `runtime_exec`, `database_*`,
`wordpress_*`, `deployment_container`, `container`, `loadgen`,
`external_session`, and `registry`.

No cycles: `module` depends on nothing internal, `harness` on `module`, the two
workload modules on both plus `workloads`.

### `darcbench-core` manifest

```toml
[dependencies]
darcbench-protocol.workspace = true
flate2.workspace = true
serde.workspace = true
serde_json.workspace = true
sha2.workspace = true
thiserror.workspace = true
```

Note what is **absent**: `rustix`, `rcgen`, `rustls`, `rustls-native-certs`,
`rustls-pki-types`, `chrono`, `hex`. Dropping `rustix` is the load-bearing one —
its `OFlags::DIRECT` is a Linux-only constant and is the single reason
`darcbench-modules` cannot compile on macOS today.

## Visibility and re-exports

**`harness` must become `pub` in core.** It is currently `mod harness;` (private)
and is used by eight modules on both sides of the new boundary: `cpu_mixed`,
`memory_bandwidth`, `loadgen`, `network_transfer`, `node_runtime`,
`php_runtime`, `storage_mixed`, `web_static`.

**`darcbench-modules` re-exports core so no downstream crate changes.** The agent
reaches for all of these today and must keep compiling untouched:

```rust
pub use darcbench_core::{module, harness, workloads, cpu_mixed, memory_bandwidth};
pub use darcbench_core::module::{
    BenchmarkModule, MachineFacts, ModuleError, ModuleManifest, ModuleOutput,
    ModuleParams, ModuleReporter, SafetyClass,
};
```

Both the type re-exports (`darcbench_modules::ModuleParams`) and the module path
(`darcbench_modules::module::ModuleReporter`) are used in the agent, so both
forms are needed.

`registry.rs` stays in `darcbench-modules` and imports `CpuMixed` and
`MemoryBandwidth` from core. It is the allow-list boundary described in
[ADR-0006](adr/0006-module-isolation.md) and belongs with the server line's
module set; the client line gets its own registry when it exists.

## Steps and gates

1. **Create the crate, move the five files, fix imports.**
   Gate: `cargo check -p darcbench-core`

2. **Add the re-exports to `darcbench-modules`; delete the moved files.**
   Gate: `cargo check --workspace` — with **zero changes to `darcbench-agent`**.
   If the agent needs an edit, the re-export surface is incomplete. Fix the
   re-exports, not the agent.

3. **Full suite.**
   Gates: `cargo clippy --all-targets --all-features -- -D warnings`,
   `cargo test --workspace --release`, `cargo fmt --all -- --check`,
   `./scripts/check-links.sh`, and the e2e job's manifest-vs-registry parity
   check.

4. **Prove portability.** Add `-p darcbench-core` to `PORTABLE_CRATES` in the
   `cross-platform` job in [ci.yml](../.github/workflows/ci.yml). Green on
   `windows-latest` and `macos-latest` is the actual deliverable of this
   exercise — the crate boundary asserted by the module tree, confirmed by two
   compilers on two operating systems.

5. **Record it.** CHANGELOG entry naming the boundary and what it makes possible.

## The ratchet

`PORTABLE_CRATES` in the `cross-platform` CI job lists what is portable today.
It starts at `darcbench-protocol` and `darcbench-scoring`, gains
`darcbench-core` at step 4, and gains `darcbench-report` once `HostFacts` lands
and `darcbench-inventory` is no longer `/proc` and `/sys` behind a hard
dependency. **The list only grows.** Removing an entry is a deliberate act that
has to be argued for.

## What this does not do

- It does not make `darcbench-modules` or `darcbench-agent` build on Windows.
  They are the server line and stay Linux-only ([ROADMAP](ROADMAP.md),
  *Explicitly deferred*).
- It does not add the client scoring model, the second reference profile, the
  GPU module or the `HostFacts` trait. Those are separate pieces of work that
  this boundary is the precondition for.
- It changes no measurement and no score. A run before and after must produce
  identical numbers, and the stored-bundle rescoring test is what proves it.
