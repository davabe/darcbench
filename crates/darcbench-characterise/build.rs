//! Records the target triple this binary was compiled for.
//!
//! The same reason `darcbench-report` has one: `std::env::consts` gives `ARCH`
//! and `OS` but not the libc or the toolchain, so `x86_64-pc-windows-msvc` and
//! `x86_64-pc-windows-gnu` would both report `x86_64-windows` and be compared
//! as if they were the same machine. They differ in allocator and in CRT, which
//! is a large part of what this binary exists to measure.
//!
//! Cargo sets `TARGET` for build scripts and nowhere else, which is why this
//! file exists at all.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".to_string());
    println!("cargo:rustc-env=DARCBENCH_TARGET={target}");
}
