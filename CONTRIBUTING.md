# Contributing to DARCBench

## The one rule

**A change that affects measurement or scoring needs a test that would fail
without it.**

Two bugs found during Phase 1 illustrate why. A test asserting that no single
catastrophic subsystem can be hidden proved the weighted geometric mean was
insufficient and forced the weak-link cap into existence. A test asserting that a
bundle still verifies after a disk round-trip caught `serde_json` parsing floats
one ULP off, which would have made signature verification intermittently fail in
production. Neither was found by reading the code.

## Setup

```bash
git clone https://github.com/davabe/darcbench && cd darcbench
rustup show                              # installs the pinned toolchain
(cd apps/web && pnpm install)            # pnpm >= 10 self-selects the pinned version
```

**Always run pnpm from `apps/web`.** `pnpm --dir apps/web ...` from the
repository root looks equivalent and is not: both Corepack and pnpm's own
version manager read `packageManager` from the *invocation* working directory,
and Corepack never sees `--dir` at all. The root has no `package.json`, so a
Corepack-managed setup quietly runs its default pnpm instead of the pinned one.
Measured with competing pins, `pnpm --dir web install` from a parent directory
ran pnpm 10.33.0 under Corepack and 11.20.0 under a plain pnpm binary — the same
command, two different versions, which is the exact failure the pin exists to
prevent.

Do not install a toolchain by hand. Both are pinned and both pins are enforced
by a CI step that fails when the running version is not the pinned one:

| | Pinned in | Version |
|---|---|---|
| Rust | `rust-toolchain.toml` | 1.97.1 |
| pnpm | `packageManager` in `apps/web/package.json` | 11.20.0 |

This is not bureaucracy — version skew has broken this project twice. A clippy
lint that exists in 1.97 but not 1.94 turned CI red after a green local run, and
a build-script policy that pnpm 10 treats as a warning but pnpm 11 treats as a
hard error broke `install && build` on a contributor's machine while CI stayed
green. The pin is *not* the MSRV: `rust-version = "1.82"` in `Cargo.toml` is the
minimum a consumer must be able to build with; the pin is the one version we all
develop against.

### Bumping a pin

Rust: edit `channel` in `rust-toolchain.toml`, run the full "before you push"
list, and fix any newly-fired lints **in the same commit**. The `lint-latest` CI
job runs clippy and rustfmt on current stable without blocking a merge, so new
lints are visible before a bump instead of after it.

pnpm: edit `packageManager`, then re-run `install && build` from a clean
`node_modules` — the failure mode being guarded against only appears on a clean
install. Verify on the *outgoing* pinned version too, since contributors who have
not pulled yet are still on it.

## Before you push

```bash
cargo fmt --all
cargo clippy --all-targets --all-features -- -D warnings
cargo test --workspace --release
(cd apps/web && pnpm build)              # runs tsc --noEmit
./scripts/check-links.sh                 # no compiler needed; run it first
./scripts/e2e.sh
```

`--release` matters: `cpu.mixed` calibration in a debug build wastes minutes, and
debug builds are not comparable anyway.

## Code standards

**Rust**
- `unsafe` is forbidden workspace-wide. If you think you need it, open an issue
  first.
- `unwrap`/`expect` are lint-warned. Where one is genuinely correct, add
  `#[allow]` **with a comment explaining why it cannot fire**, and a test that
  proves it.
- Errors are typed with `thiserror`. Never `Box<dyn Error>` across a public API.
- Every public item has a doc comment explaining *why*, not restating the
  signature.
- No stringly-typed payloads. Every event body and warning is a named type.
- No global mutable state.

**TypeScript**
- `strict`, `noUncheckedIndexedAccess`, `noUnusedLocals` on.
- No `any`. No `dangerouslySetInnerHTML`.
- The protocol types in `apps/web/src/types.ts` mirror the Rust types by hand.
  If you add an event kind in Rust, add it there and to `EVENT_KINDS`.

**Comments** explain reasoning, tradeoffs and non-obvious constraints. Do not
narrate what the next line does.

## Adding a benchmark module

See [docs/BENCHMARK-MODULE-SPEC.md](docs/BENCHMARK-MODULE-SPEC.md). In short:

1. Implement `BenchmarkModule`, including honest `limitations`.
2. Add reference anchors in `darcbench-scoring::reference` — in the *same*
   change. Anchors for a module that does not exist are fabricated data, and a
   test forbids them.
3. Register it in `Registry::builtin()`.
4. Write `benchmarks/<category>/<id>.json`.
5. Tests: manifest well-formedness, a full run, cancellation responsiveness,
   corpus determinism.

Your module must respond to `reporter.is_cancelled()` between repetitions and
clean up on every exit path, including errors.

## Changing scoring

1. Read [docs/SCORING-SYSTEM.md](docs/SCORING-SYSTEM.md) and
   [ADR-0007](docs/adr/0007-scoring-versioning.md).
2. Bump `SCORING_MODEL_VERSION` correctly — patch means *provably no score
   changes*.
3. Update the worked examples in the doc.
4. Add a test for the property you are changing.

Scores are never changed silently. That is a project commitment, not a
preference.

## Commits and PRs

Conventional commits: `feat(scoring): add weak-link cap`,
`fix(agent): reject percent-encoded traversal`. A PR should explain **why**,
list what was tested, and call out anything a reviewer should look at
sceptically. Small PRs get reviewed; large ones get postponed.

## What will be pushed back on

- A measurement change with no test.
- A `TODO` in place of an implementation. Either build it or don't merge it.
- Placeholder security ("we'll add auth later").
- Fabricated reference values or example results presented as real.
- Documentation that contradicts the code.
- A new dependency without a stated reason.
- Anything that makes the agent modify configuration outside its state
  directory.

## Reporting bugs

Include the output of `darcbench doctor --json` and, if a run is involved, the
`bundle.json`. Check it for anything you would rather not share first — it is
redacted by default, but read it anyway.

Security issues go to `security@getdarc.com`, not the issue tracker. See
[SECURITY.md](SECURITY.md).
