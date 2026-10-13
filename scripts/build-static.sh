#!/usr/bin/env bash
# Builds the static musl binary that field runs and calibration use.
#
# `docs/CALIBRATION-RUNBOOK.md` requires the static musl build - a glibc binary
# built on a modern distribution will not start on Debian 12, and a benchmark
# that has to be compiled per host has its compiler version inside the
# measurement. This script is how that binary is produced, so it is produced the
# same way every time by anyone.
#
# The obstacle is that `rusqlite` compiles SQLite from C, so a *C* cross
# compiler targeting musl is needed. Zig ships one in a single tarball that
# needs no root, which is why it is used here rather than `musl-tools`: a
# release must be buildable by a contributor who cannot install packages, and
# pinning the compiler pins the generated code for the C half of the binary.
#
# Usage:  scripts/build-static.sh [--check] [--target <triple>]
#           --check   verify the toolchain and exit without building
#           --target  x86_64-unknown-linux-musl (default) or
#                     aarch64-unknown-linux-musl
#
# Both targets are built from an x86-64 host with the same zig. arm64 is a
# cross build rather than a native one on purpose: one pinned compiler for the C
# half of every published binary, instead of a second toolchain whose version
# would have to be pinned and checked separately.
set -euo pipefail

CHECK_ONLY=0
TARGET="x86_64-unknown-linux-musl"
while [ $# -gt 0 ]; do
    case "$1" in
        --check) CHECK_ONLY=1 ;;
        --target)
            [ $# -ge 2 ] || { echo "--target needs a value" >&2; exit 2; }
            TARGET="$2"
            shift
            ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
    shift
done

case "$TARGET" in
    x86_64-unknown-linux-musl) ARCH="x86_64" ;;
    aarch64-unknown-linux-musl) ARCH="aarch64" ;;
    *) echo "unsupported target: $TARGET (x86_64- or aarch64-unknown-linux-musl)" >&2; exit 2 ;;
esac
# zig's spelling of the same target, and the env-var spelling cargo and cc-rs
# derive from the rust triple.
ZIG_TARGET="$ARCH-linux-musl"
TRIPLE_ENV="$(echo "$TARGET" | tr 'a-z-' 'A-Z_')"
TRIPLE_CC="$(echo "$TARGET" | tr '-' '_')"

ZIG_VERSION="0.16.0"
ZIG_SHA256="70e49664a74374b48b51e6f3fdfbf437f6395d42509050588bd49abe52ba3d00"
ZIG_URL="https://ziglang.org/download/${ZIG_VERSION}/zig-x86_64-linux-${ZIG_VERSION}.tar.xz"

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# Kept out of `target/`, which `cargo clean` empties - re-downloading a 52 MB
# toolchain because someone cleaned a build directory is a poor trade.
TOOLS="${DARCBENCH_TOOLCHAIN_DIR:-$ROOT/.toolchain}"
ZIG_DIR="$TOOLS/zig-x86_64-linux-$ZIG_VERSION"
SHIM_DIR="$TOOLS/shim"
# cc-rs derives this name from the target triple and will accept nothing else.
SHIM="$SHIM_DIR/$ARCH-linux-musl-gcc"

say() { printf '\033[1m%s\033[0m\n' "$*"; }

if ! rustup target list --installed 2>/dev/null | grep -qx "$TARGET"; then
    say "installing the $TARGET std"
    rustup target add "$TARGET"
fi

if [ ! -x "$ZIG_DIR/zig" ]; then
    say "fetching zig $ZIG_VERSION"
    mkdir -p "$TOOLS"
    curl -fSL --retry 3 "$ZIG_URL" -o "$TOOLS/zig.tar.xz"
    # Checked before it is unpacked, let alone executed. A toolchain is the most
    # sensitive thing this repository downloads: it decides what every published
    # binary contains.
    echo "$ZIG_SHA256  $TOOLS/zig.tar.xz" | sha256sum -c - || {
        rm -f "$TOOLS/zig.tar.xz"
        echo "zig tarball failed its checksum; refusing to unpack it" >&2
        exit 1
    }
    tar -xf "$TOOLS/zig.tar.xz" -C "$TOOLS"
    rm -f "$TOOLS/zig.tar.xz"
fi

mkdir -p "$SHIM_DIR"
cat > "$SHIM" <<SHIM_BODY
#!/usr/bin/env python3
"""cc-rs calls this expecting gcc; it gets \`zig cc\` targeting musl.

The \`--target=<rust triple>\` that cc-rs passes is stripped: zig spells targets
its own way and rejects \`x86_64-unknown-linux-gnu\` outright.
"""
import subprocess, sys
args = [a for a in sys.argv[1:] if not a.startswith("--target=")]
sys.exit(subprocess.call(
    ["$ZIG_DIR/zig", "cc", "-target", "$ZIG_TARGET"] + args))
SHIM_BODY
chmod +x "$SHIM"

# Proves the shim links a static binary before a five-minute build depends on
# it. A toolchain that is subtly wrong is much cheaper to find here.
probe="$(mktemp -d)"
trap 'rm -rf "$probe"' EXIT
printf 'int main(void){return 0;}\n' > "$probe/probe.c"
"$SHIM" "$probe/probe.c" -o "$probe/probe"
file "$probe/probe" | grep -q "statically linked" || {
    echo "the shim produced a dynamically linked binary; the toolchain is wrong" >&2
    exit 1
}
say "toolchain ok: zig $("$ZIG_DIR/zig" version), static link confirmed"

if [ "$CHECK_ONLY" = 1 ]; then
    exit 0
fi

# `link-self-contained=no` because rust ships its own musl startup objects and
# zig ships musl too: with both, `_start_c` is defined twice and the link fails.
export PATH="$SHIM_DIR:$PATH"
export "CC_${TRIPLE_CC}=$SHIM"
export "CARGO_TARGET_${TRIPLE_ENV}_RUSTFLAGS=-C link-self-contained=no -C linker=$SHIM"

say "building $TARGET"
# `--locked`: a static binary is what gets published and what calibration runs,
# so it is built from the committed lockfile, never from whatever the registry
# would resolve today.
cargo build --release --locked --target "$TARGET" -p darcbench-agent

BIN="$ROOT/target/$TARGET/release/darcbench"
file "$BIN" | grep -q "statically linked" || {
    echo "the built binary is not statically linked" >&2
    exit 1
}
# A cross build that silently produced a host binary would pass every other
# check here and fail only on the machine it was shipped to.
case "$ARCH" in
    x86_64) EXPECT_ARCH="x86-64" ;;
    aarch64) EXPECT_ARCH="ARM aarch64" ;;
esac
file "$BIN" | grep -q "$EXPECT_ARCH" || {
    echo "the built binary is not $EXPECT_ARCH: $(file -b "$BIN")" >&2
    exit 1
}
# The hash goes in the bundle as `agent_build_hash`, so it is the thing that
# says two results came from the same code. Printed here so a release never has
# to guess it.
say "built $(stat -c%s "$BIN") bytes"
sha256sum "$BIN"
# The binary is only run when it can be: an arm64 build on an x86-64 host is
# checked by `file` above and executed by whoever has the hardware (or qemu).
if [ "$ARCH" = "$(uname -m)" ]; then
    "$BIN" --version
fi
