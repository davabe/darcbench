# Fixtures

Deterministic test data.

## Present

Benchmark corpora are **generated, not stored**. Every workload builds its input
from `CORPUS_SEED` via SplitMix64 at construction time, so the corpus is
identical on every machine and every run without shipping binary blobs in the
repository. A reference-vector test pins the generator: changing it changes every
corpus and requires a major workload version bump.

## Planned

| Fixture | Phase | Purpose |
|---|---|---|
| `bundles/` — valid, tampered, truncated, wrong-version | 2 | Validation and verification tests |
| `inventories/` — captured `/proc` and `/sys` trees from varied hosts | 2 | Test detection without needing the hardware |
| `wordpress/` — deterministic post, page, comment and media generator | 4 | Reproducible CMS workload |
| `oltp/` — schema and seed data for the database module | 4 | Reproducible OLTP workload |

Captured inventories will be **redacted before commit** — no hostnames, MAC
addresses, IPs or serial numbers. See [../docs/PRIVACY.md](../docs/PRIVACY.md).
