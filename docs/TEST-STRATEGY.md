# Test strategy

**Today: 643 Rust tests across the workspace and 68 dashboard tests, all passing.** The rule for any change
affecting measurement or scoring is a test that would fail without it.

## Layers

| Layer | Where | What it proves |
|---|---|---|
| Unit | Alongside the code | Statistics, scoring arithmetic, parsers, path validation, auth |
| Property-ish | `stats`, `scoring` | Monotonicity, boundedness, determinism |
| Integration | `runner`, `server` | Full run lifecycle, event ordering, cancellation, persistence |
| Contract | `protocol`, `canonical` | Serialisation stability, forward compatibility, signature round-trip |
| End-to-end | `scripts/e2e.sh` | The real binary: doctor → run → verify → tamper detection |
| Reference | `scripts/check-links.sh` | Every path named in a document or a doc comment still exists |
| Dashboard | `apps/web/src/*.test.ts(x)` — Vitest, jsdom | The event fold, the API client, and what each panel says |
| Browser | Playwright (manual) | The dashboard renders, streams and completes a run |

## What is covered today

### Scoring correctness — the highest-value tests
- A machine matching DARC-REF-1 scores exactly 1000.
- Doubling performance doubles the score.
- Latency inverted exactly once; a zero-latency reading is rejected.
- **One catastrophic category cannot be hidden** — asserts the pre-cap aggregate
  would have exceeded reference, then asserts the cap engaged. This test found a
  real methodology gap during development.
- A 2× spread between categories does *not* trigger the cap.
- Instability lowers the total, bounded at 10%.
- Failed modules contribute nothing; partial and custom runs are never standard.
- Scoring is a pure function — identical inputs give identical outputs.
- Category weights and every composite weight set sum to 1.0.
- The shipped model is flagged uncalibrated (fails the build if cleared).

### Measurement integrity
- SplitMix64 pinned to a reference vector, so a change to the generator — and
  therefore every corpus — fails loudly.
- Corpora identical across constructions.
- Every workload takes measurable time (would catch dead-code elimination).
- Runtime scales with iteration count (would catch loop hoisting).
- The compression corpus actually compresses (15–75%), so it is not a memcpy
  benchmark.
- Calibration lands in the right order of magnitude.
- Telemetry sampling stays under 25 ms per sample.

### Safety and security
- Path traversal rejected in run ids, module ids, state paths and asset routes,
  including percent-encoded forms.
- Cookie and query auth cannot start or cancel a run (CSRF).
- Malformed `Authorization` schemes rejected.
- Unauthenticated requests get 401, cookie-mutation gets 403.
- Reserved and privileged ports refused.
- Force overrides warnings but never blocking findings.
- Production-looking machines classified `ProductionRisk`.
- Agent key persisted at mode 0600; truncated key rejected; never in `Debug`.

### Result integrity
- Sign/verify round-trip; tampering with any covered field breaks it.
- **A bundle written to disk and read back still verifies** — the regression
  test for the float-canonicalisation bug found during development.
- Server-side validation detects an edited score via recomputation.
- Canonical JSON: sorted keys, no whitespace, insertion-order independence.

### Privacy
- Redaction is the default; revealing is opt-in and scope-restored.
- Hostname and MAC are `[redacted]` in serialised output.
- The performance digest ignores volatile and identifying values but moves on a
  real hardware change.

### Lifecycle
- A full quick run produces a signed bundle with 10 metrics.
- The event stream is gapless, correctly ordered and terminates.
- Only one run at a time; a second is refused with 409.
- Cancellation produces a terminal `Invalid` bundle without corrupting the
  stream.
- Artifacts written and re-verifiable from disk.
- SSE replay returns exactly the events after a given sequence number.

### Reports
- HTML escaping neutralises injection from hostile inventory strings.
- Reports are self-contained — no `http://`, `https://` or `<script` anywhere.
- The uncalibrated banner is present.
- The fallback console has no inline script that CSP would block.

### Dashboard
Run in CI by the `Web UI` job with `pnpm test`, against the same Vite
configuration the embedded bundle is built with, and type-checked by
`tsc --noEmit` alongside the application code.
- **The event fold is idempotent.** A tail re-delivered after a reconnect
  changes nothing, so `Last-Event-ID` recovery cannot double-count.
- Warm-up samples are never charted; telemetry and the log stay bounded.
- Events are pooled and applied once per flush window, at once on a terminal
  event or a full pool — the observer-overhead rule, tested rather than assumed.
- The bearer token goes on every request, run ids are path-encoded, agent error
  codes survive to the UI, and the token leaves the URL only once the session
  exists.
- A comparison lists the largest regression first, never calls float noise a
  change, names what it could not line up, and discards the answer for a pair
  that is no longer selected.
- Category balance leaves facets out and recognises a total sitting on the
  weak-link cap.

Each of these was checked by breaking the code it guards and watching a test
fail.

## Gaps, stated plainly

| Gap | Plan |
|---|---|
| No `#[should_panic]`-style fuzzing of protocol decoding | Add `cargo-fuzz` for the event and bundle decoders in Phase 2 |
| Browser tests are manual | Automate Playwright in CI in Phase 2 |
| No arm64 test runs | Add a CI matrix once arm64 runners are available (Phase 8) |
| No root-vs-non-root matrix | Add a container-based matrix in Phase 2 |
| No low-disk / missing-dependency / occupied-port integration tests | Required before storage modules ship |
| No crash-recovery or stale-run tests | Required with the SQLite index (Phase 2) |
| No multi-machine reproducibility corpus | Blocked on calibration hardware |
| No slow-SSE-client test | Add a deliberately throttled consumer in Phase 2 |
| Coverage is not measured | Add `cargo-llvm-cov` in CI |

## Deterministic fixtures

Every corpus is generated from `CORPUS_SEED` via SplitMix64. Reference vectors
pin the generator. Where tests need timestamps or randomness they construct them
explicitly rather than reading the clock, so tests do not fail at midnight.

## Running

```bash
cargo fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --workspace --release
(cd apps/web && pnpm test && pnpm build)   # build includes tsc --noEmit
./scripts/e2e.sh
```

`--release` matters: `cpu.mixed` calibration in a debug build wastes minutes,
and debug builds are not comparable anyway.

## Acceptable variance per module

See [BENCHMARK-METHODOLOGY.md](BENCHMARK-METHODOLOGY.md) §8. Those figures are
targets to validate during Phase 2 calibration, not measured results.
