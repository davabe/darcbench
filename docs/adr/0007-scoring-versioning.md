# ADR-0007: The scoring model is versioned separately from the agent, over retained raw evidence

**Status:** Accepted · **Date:** 2026-08-03

## Context

Scoring will change: weights will be refined, the reference will be calibrated,
categories will be added. Every benchmark product that has changed scoring
silently has damaged its own credibility.

## Decision

1. The scoring model has its own semantic version (`dbs/MAJOR.MINOR.PATCH`),
   independent of the agent version, recorded in every score event and bundle.
2. `ScoringModel::score_run` is a **pure function** of raw metrics. No I/O, no
   clock, no state. A test asserts determinism.
3. Raw metrics are retained in full, per repetition, in physical units. Any
   historical run can be rescored under any model without re-running it.
4. Two results are comparable only when the model version, reference profile,
   profile, module major versions, build profile and scope all match.
5. An uncalibrated model carries `-dev` and sets `uncalibrated: true`. A test
   fails the build if that flag is cleared while the reference is uncalibrated.

Version semantics: **patch** = no score changes (proven against a stored
corpus); **minor** = new categories or metrics, scores may move, historical runs
rescored with both values retained; **major** = weights or aggregation change,
historical runs rescored and shown alongside their original score, never
silently replaced.

## Alternatives

**One version for everything.** Rejected: an agent bug fix should not invalidate
comparability, and a scoring change should not be hidden inside a patch release.

**Freeze scoring at 1.0 forever.** Rejected: the model is explicitly not
calibrated yet, and pretending otherwise would be the dishonesty this ADR exists
to prevent.

## Consequences

- The bundle must carry everything scoring needs. It does.
- Leaderboards are partitioned by model version, and cross-version comparison is
  refused rather than approximated.
