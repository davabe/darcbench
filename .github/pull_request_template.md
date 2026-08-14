## What and why

<!-- What changes, and the reason. Link the issue or ADR if there is one. -->

## Does this change measurement or scoring?

- [ ] No
- [ ] Yes — and there is a test that would fail without this change

If yes, state which property changed and whether the scoring model version was
bumped (see [ADR-0007](../docs/adr/0007-scoring-versioning.md)). Scores are never
changed silently.

## Tested

```
cargo fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --workspace --release
(cd apps/web && pnpm build)
./scripts/e2e.sh
```

<!-- Paste anything notable, especially a test that now fails on main. -->

## Safety review

- [ ] No new filesystem write outside the state directory
- [ ] No new external process, URL or hostname accepted from a caller
- [ ] No new unauthenticated endpoint
- [ ] No existing system configuration modified
- [ ] Identifying values still redact by default

## Anything a reviewer should look at sceptically

<!-- Be specific. "Nothing" is a valid answer for a small change. -->
