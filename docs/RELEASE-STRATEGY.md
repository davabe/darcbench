# Release strategy

## Versioned artifacts

Four things version independently, on purpose:

| Artifact | Format | Changes when |
|---|---|---|
| Agent | `0.1.0` | Any code change |
| Event protocol | `darcbench.events/1` | Breaking wire change (major only) |
| Bundle schema | `darcbench.bundle/1` | Breaking result-format change |
| Scoring model | `dbs/0.2.0-dev` | Any change to how scores are computed |

An agent bug fix must not invalidate comparability, and a scoring change must not
hide inside a patch release. See [ADR-0007](adr/0007-scoring-versioning.md).

**Every model version this project has published stays recomputable.**
`ScoringModel::for_version` selects the model a bundle declares, not whichever
is newest, because recomputing the score from the raw metrics is the anti-tamper
check and it needs the model that produced the score. Verifying only against the
current model would make every bundle ever signed unverifiable the moment the
model moved — which is when the evidence matters most. `dbs/0.1.0-dev` is kept
as a diff from the current anchors rather than a second copy of them.

An unknown model is still fatal. Knowing an older model and declining to check
it are different things, and only the second lets arbitrary numbers become
rankable by naming a model nobody implements.

## Channels

| Channel | Audience | Bar |
|---|---|---|
| `nightly` | Developers | CI green |
| `beta` | Early adopters | CI green + e2e on two architectures |
| `stable` | Everyone | Beta soak + no open critical issues |

Results from `nightly` and `beta` agents are accepted but never promoted above
`SelfReported`.

## Pre-1.0 policy

The agent is `0.x`. Breaking changes are possible in minor releases and are
listed in `CHANGELOG.md`. The protocol and bundle schema are already at `/1` and
will not break without a major bump — consumers can rely on them before the agent
stabilises.

**`1.0` requires:** calibrated scoring (`dbs/1.0.0`, `calibrated: true`); Phase
2–4 modules complete; an external security audit; arm64 release parity; and a
stable, documented API.

## Pipeline

```
fmt ─► clippy -D warnings ─► test --release ─► web build ─► e2e
   └─► security scan ─► dependency audit ─► SBOM
                                             └─► build matrix ─► sign ─► publish
```

**Build matrix:** linux/amd64 and linux/arm64, gnu and musl. Static musl builds
are the default download — one file, no libc version to match.

The amd64 musl build is produced by
[`scripts/build-static.sh`](../scripts/build-static.sh), which pins the C
cross-compiler by checksum. Pinning it matters beyond reproducibility: SQLite is
compiled from C into the same process the benchmark measures, so the compiler is
part of the measurement in the same way the libc is. arm64 is not wired up yet.

**Reproducible builds** (Phase 8): `--locked`, no build timestamps. Two
independent builds of a tag must produce identical binaries, so the build-hash
attestation that gates `Verified` results means something.

The toolchain half of that is already in place, because it was needed for
development correctness before it was needed for reproducibility: Rust is pinned
in `rust-toolchain.toml` and pnpm in the `packageManager` field of
`apps/web/package.json`, and CI fails if the running version is not the pinned
one. A release tag therefore already names its exact toolchain. What Phase 8 adds
is the rest — `--locked` everywhere, timestamp elimination, and a rebuild job
that diffs two independent builds of the same tag.

**Signing** (Phase 8): every artifact and container image signed; checksums and
signatures published; SBOM per release.

## Artifacts

Standalone `.tar.gz` per target · `.deb` · `.rpm` · container image (multi-arch,
distroless) · install script · SBOM · checksums · signatures.

## Compatibility guarantees

| Between | Guarantee |
|---|---|
| Agent ↔ bundle | An agent reads any bundle with a schema major it knows |
| Agent ↔ protocol | Additive fields never break a consumer; consumers must ignore unknown fields and kinds |
| Agent ↔ control plane | The control plane supports the current agent major and the one before |
| Scoring model ↔ bundle | Any bundle is rescoreable under any model that knows its metrics |

An agent more than one major behind is asked to upgrade before uploading, and is
told exactly why.

## Self-update safety (Phase 8)

Download to a temporary path → verify signature → verify checksum → atomic
rename → run `doctor` → roll back on failure. **Never** update while a run is in
progress. Update is opt-in; the agent does not check for updates on its own,
because a benchmark tool making unexpected outbound connections is a surprise
nobody wants on a production host.

## Database migrations (Phase 5)

Forward-only, reviewed, tested against a production-shaped dataset, applied
before the new version serves traffic. Every migration must be safe to run while
the previous version is still running — no long table locks, no destructive
column drops in the same release that stops writing them.

## Deprecation

Announce in `CHANGELOG.md` → keep working for at least two minor releases → warn
at runtime for at least one → remove in a major release. A protocol event kind is
never removed within a major version; it is only ever stopped being emitted.
