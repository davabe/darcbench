//! The portable DARCBench measurement engine.
//!
//! # What lives here, and why it is a crate
//!
//! Everything in this crate is free of any operating-system dependency: the
//! module contract, the timing harness, the workload definitions, and the two
//! modules built purely on them. It is shared unchanged by both product lines -
//! the server agent and the client application - because a benchmark whose
//! engine differs per platform cannot compare across platforms, and comparing
//! across platforms is the product ([ADR-0015](../../../docs/adr/0015-two-product-lines-one-engine.md)).
//!
//! The boundary is a **crate**, not a `#[cfg(unix)]`. The distinction being
//! drawn is *product line*, not OS family: macOS is unix and is a client
//! target, so a `cfg(unix)` gate would compile the WordPress module for a
//! MacBook. A crate boundary states what is actually meant, and the compiler
//! enforces it - a client build that reaches for a server workload does not
//! link.
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
//! A registry maps an allow-listed `ModuleId` to a Rust implementation. There is
//! no code path from an HTTP request to a shell, which is what makes it safe to
//! expose a start-a-run button to a browser at all. The registry itself lives
//! with each product line, since the two ship different sets of modules.
//!
//! # Adding a module here
//!
//! 1. Implement [`BenchmarkModule`].
//! 2. Add reference anchors for its metrics in `darcbench-scoring`.
//! 3. Register it in the consuming product line's registry.
//! 4. Write the manifest file under `benchmarks/<category>/<id>.json`.
//!
//! Steps 1-3 are enforced together: a module whose metrics have no anchors
//! shows up in `ScoreCard::unreferenced_metrics` and is visible immediately.
//!
//! A module that needs a process, a socket or a filesystem beyond a scratch
//! file does not belong here. It belongs to a product line.

pub mod cpu_mixed;
pub mod harness;
pub mod memory_bandwidth;
pub mod module;
pub mod workloads;

pub use module::{
    BenchmarkModule, MachineFacts, ModuleError, ModuleManifest, ModuleOutput, ModuleParams,
    ModuleReporter, SafetyClass,
};
