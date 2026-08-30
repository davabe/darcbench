//! The server line's benchmark modules.
//!
//! # What is here and what is not
//!
//! The measurement engine - the module contract, the timing harness, the
//! workload definitions, `cpu.mixed` and `memory.bandwidth` - lives in
//! [`darcbench_core`] and is shared with the client product line. What is left
//! here is everything that needs an operating system: a process, a listening
//! socket, a container daemon, an interpreter the operator installed, or a
//! filesystem beyond a scratch file.
//!
//! That split is a crate boundary rather than a `#[cfg(unix)]` because the
//! distinction is *product line*, not OS family - see
//! [ADR-0015](../../../docs/adr/0015-two-product-lines-one-engine.md). A new
//! module belongs here only if it could not run on a laptop that has never
//! hosted anything.
//!
//! The engine is re-exported below, so `crate::harness`, `crate::module` and
//! `crate::workloads` resolve inside this crate exactly as they did before the
//! split, and so consumers of `darcbench_modules` are unaffected.
//!
//! # The module contract
//!
//! A module is a *versioned workload definition*, not a script. It declares up
//! front, in a [`ModuleManifest`], everything the agent and the safety layer
//! need to decide whether running it is acceptable: how long it takes, how many
//! bytes it writes, what it depends on, what it cleans up, and what would make
//! its results invalid.
//!
//! Crucially, a module never receives a command line and never constructs one.
//! The [`registry`] maps an allow-listed [`ModuleId`](darcbench_protocol::ModuleId)
//! to a Rust implementation. There is no code path from an HTTP request to a
//! shell, which is what makes it safe to expose a start-a-run button to a
//! browser at all.
//!
//! # Adding a module
//!
//! 1. Implement [`BenchmarkModule`].
//! 2. Add reference anchors for its metrics in `darcbench-scoring`.
//! 3. Register it in [`registry::builtin`].
//! 4. Write the manifest file under `benchmarks/<category>/<id>.json`.
//!
//! Steps 1-3 are enforced together: a module whose metrics have no anchors
//! shows up in `ScoreCard::unreferenced_metrics` and is visible immediately.

pub mod container;
pub mod database_cache;
pub mod database_oltp;
pub mod deployment_container;
pub mod external_session;
pub mod loadgen;
pub mod network_endpoints;
pub mod network_transfer;
pub mod node_runtime;
pub mod php_runtime;
pub mod registry;
pub mod runtime_exec;
pub mod storage_mixed;
pub mod web_origin;
pub mod web_static;
pub mod wordpress_fixture;
pub mod wordpress_site;

// The shared measurement engine, re-exported at this crate's root.
//
// This is load-bearing, not a convenience: it is what lets the seventeen module
// files above go on saying `crate::harness`, `crate::module` and
// `crate::workloads` after the engine moved to its own crate, and what keeps
// `darcbench_modules::module::ModuleReporter` working for the agent. Removing
// it is a breaking change to both.
pub use darcbench_core::{cpu_mixed, harness, memory_bandwidth, module, workloads};

pub use darcbench_core::module::{
    BenchmarkModule, MachineFacts, ModuleError, ModuleManifest, ModuleOutput, ModuleParams,
    ModuleReporter, SafetyClass,
};
pub use registry::Registry;
