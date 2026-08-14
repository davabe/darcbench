# Tests

Most tests live next to the code they cover, which is where they belong in Rust.
This directory is for cross-cutting checks that need the whole workspace.

| What | Where |
|---|---|
| Unit and integration tests | `crates/*/src/**` (`#[cfg(test)]`) |
| End-to-end against the real binary | [`../scripts/e2e.sh`](../scripts/e2e.sh) |
| Rust/TypeScript protocol parity | [`../scripts/check-protocol-parity.sh`](../scripts/check-protocol-parity.sh) |
| Module manifests vs compiled registry | [`../scripts/check-manifests.sh`](../scripts/check-manifests.sh) |
| Browser verification | Playwright, manual today — see [../docs/TEST-STRATEGY.md](../docs/TEST-STRATEGY.md) |

Run everything:

```bash
cargo fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --workspace --release
(cd apps/web && pnpm build)
./scripts/check-protocol-parity.sh
./scripts/check-manifests.sh
./scripts/e2e.sh
```

Planned additions and known gaps are listed in
[../docs/TEST-STRATEGY.md](../docs/TEST-STRATEGY.md).
