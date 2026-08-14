//! Benchmark modules.
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
//! The [`registry`] maps an allow-listed [`ModuleId`] to a Rust implementation.
//! There is no code path from an HTTP request to a shell, which is what makes
//! it safe to expose a start-a-run button to a browser at all.
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
pub mod cpu_mixed;
pub mod database_cache;
pub mod database_oltp;
pub mod deployment_container;
pub mod external_session;
mod harness;
pub mod loadgen;
pub mod memory_bandwidth;
pub mod module;
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
pub mod workloads;

pub use module::{
    BenchmarkModule, MachineFacts, ModuleError, ModuleManifest, ModuleOutput, ModuleParams,
    ModuleReporter, SafetyClass,
};
pub use registry::Registry;
