//! Records the target triple the crate was compiled for.
//!
//! `std::env::consts` gives `ARCH` and `OS` but not the libc, so a build for
//! `x86_64-unknown-linux-musl` and one for `x86_64-unknown-linux-gnu` both
//! reported `x86_64-linux` and were treated as comparable. They are not: musl
//! and glibc differ in allocator and in `memcpy`, which is exactly what
//! `cpu.mixed` and `memory.bandwidth` measure. Since static musl is the default
//! download and building from source gives gnu, that difference would have been
//! invisible in every comparison and in calibration itself.
//!
//! Cargo sets `TARGET` for build scripts and nowhere else, which is why this
//! file exists at all.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".to_string());
    println!("cargo:rustc-env=DARCBENCH_TARGET={target}");
}
