# ADR-0005: Filesystem now, SQLite standalone, PostgreSQL and object storage for the control plane

**Status:** Accepted · **Date:** 2026-08-03

## Context

Standalone mode must work with zero setup on a machine that may have no database
at all. The control plane must handle many agents, historical comparison and
large raw artifacts.

## Decision

**Standalone, Phase 1:** a plain filesystem layout,
`<state>/runs/<run_id>/{bundle.json,report.html,events.ndjson}`. The run index is
a directory scan.

**Standalone, Phase 2:** SQLite (bundled, no system dependency) for the run
index, comparisons and history. Bundles stay as files — a benchmark result
should remain readable with `cat` and `jq` after the tool that made it is gone.

**Control plane, Phase 5:** PostgreSQL for entities and derived scores; S3-
compatible object storage for immutable raw bundles and event streams.

## Rationale

Deferring SQLite is not laziness: an index over a handful of directories does
not need a database, and the migration is bounded because `RunManager` is the
only code that touches persistence. Adding a C dependency and a schema before
there is a query to run would be architecture for its own sake.

PostgreSQL over MySQL for the control plane: better JSONB, better partial
indexes for leaderboard queries, and a stronger story for the analytical queries
provider comparisons need.

## Consequences

- Bundles are the source of truth in both modes. A database is an index over
  them, never the only copy.
- Retention and pruning policy is required before Phase 2 ships; see
  `docs/BACKLOG.md`.

## Phase 2 implementation notes

*Added 2026-08-09, when the standalone half of this decision was implemented.
The decision is unchanged; these record what carrying it out settled.*

- The index lives at `<state>/index.db`, a sibling of `runs/` rather than
  something inside it, so the documented run-directory layout is untouched.
- **Disposability is enforced, not merely intended.** `reconcile` runs at every
  startup: it indexes bundles the database does not know about and forgets runs
  whose directory has gone. Deleting `index.db` therefore costs a rebuild and
  nothing else, and an operator clearing space with `rm -rf` does not end up
  with a list of runs that no longer exist. An index that cannot be opened at
  all degrades to an in-memory one; the agent still runs benchmarks.
- Index writes are best-effort and never fail a run. A run that completed,
  scored and signed has done its job, and failing it afterwards because a cache
  of its metadata would not write gets the hierarchy above exactly backwards.
- **`rusqlite` with `bundled` is the workspace's only C dependency**, which is
  the specific cost this ADR anticipated with "adding a C dependency". It is
  paid here because the amalgamation ships inside the crate, so no host needs
  `libsqlite3` and the single-static-binary promise in the product bible
  survives. `cargo deny` passes unchanged: `rusqlite` and `libsqlite3-sys` are
  both MIT, already on the allow-list.
- The schema version lives in SQLite's own `user_version` pragma. A database
  written by a newer agent is **refused** rather than half-read, with an error
  saying to delete it — safe advice precisely because the bundles are the source
  of truth.
