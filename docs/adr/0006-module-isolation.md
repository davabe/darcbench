# ADR-0006: Compile-time module registry, with tiered isolation as modules grow

**Status:** Accepted · **Date:** 2026-08-03

## Context

Modules are the extension point. They are also the obvious attack surface: a
module system that loads code from disk turns the agent into a loader running as
root on a production server.

## Decision

**Phase 1–4: no dynamic loading.** `Registry::builtin()` is a compile-time table
mapping `ModuleId` to a Rust implementation. An API caller supplies a string;
the registry resolves it or rejects it. There is no path from a request to a
process.

Isolation is tiered by what a module actually needs:

| Tier | Used by | Mechanism |
|---|---|---|
| In-process | CPU, memory | Native Rust, no isolation needed — no external input |
| Subprocess | storage (fio), network | Fixed argv, no shell, rlimits, timeout, working directory under the state dir |
| Container | databases, WordPress, deployment | Pinned OCI image by digest, no host network, resource limits, destroyed after the run |

Native adapters are preferred where containerisation would distort the
measurement — measuring disk I/O through an overlay filesystem measures the
overlay.

**Phase 8, if third-party modules ship:** signed manifests, integrity hashes,
declared resource bounds, execution under seccomp + namespaces + cgroups, and
**they may never contribute to the official total score.**

## Rationale for the score exclusion

A total score whose inputs anyone can extend is not a score anyone can trust.
Third-party modules can run, can produce their own named scores, and their runs
are labelled `Custom`. Relaxing this requires a curated, signed module registry
with review — which is a product, not a feature.

## Consequences

- Adding a module requires a release. Acceptable, and it keeps the allow-list
  auditable.
- Container-based modules add a Docker/Podman dependency, declared in the
  manifest and checked during preflight.
