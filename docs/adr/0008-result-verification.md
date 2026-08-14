# ADR-0008: Ed25519 over canonical JSON, plus server-side score recomputation

**Status:** Accepted · **Date:** 2026-08-03

## Context

Results will be published and compared. Someone will try to fake one. A
verification scheme that overpromises is worse than none, because it invites
trust it cannot support.

## Decision

**Signing.** Ed25519 over DARCBench Canonical JSON (DCJ/1): sorted object keys,
no insignificant whitespace, non-finite numbers rejected, and — normatively —
**correctly-rounding decimal parsing**. The signature covers everything except
itself, including the verdict, so a downgraded verdict cannot be quietly
upgraded.

**Recomputation.** The server never trusts a bundle's own scores or verdict. It
recomputes every score from the raw metrics with the named model and rejects a
mismatch. Editing a score without editing the metrics is caught by
recomputation; editing the metrics breaks the signature.

**Verification tiers.** A locally-signed bundle can never exceed
`SelfReported`. `Validated` requires server-side recomputation to pass.
`Verified` additionally requires a server-issued nonce, a run token, and an agent
build hash matching a published release. `Official` requires
DARCBench-controlled provisioning. Only `Validated`, `Verified` and `Official`
are rankable.

## Why Ed25519

Small keys and signatures, no parameter choices to get wrong, deterministic
nonces so no RNG is needed at signing time, and constant-time implementations
are the norm. For an artifact that must be verifiable years from now by a
browser, a CLI and a server, "no knobs" is the feature.

## Why DCJ/1 and not RFC 8785

DCJ/1 agrees with JCS on key ordering and whitespace but does not implement JCS
number serialisation in full. Claiming compliance we have not verified would be
worse than documenting the difference.

**The correctly-rounding requirement is not theoretical.** During implementation
we found that `serde_json` without its `float_roundtrip` feature parses some
decimals one ULP away from the value that was written — so a signed bundle
written to disk and read back failed its own signature check. The feature is now
required workspace-wide and `signature_survives_a_disk_roundtrip` is the
regression test.

## Accepted residual risk

An operator who controls the machine can patch the agent and sign fabricated
numbers with their own key. This is unfixable without hardware attestation,
which DARCBench will not require. It is handled by classification, not
prevention — which is exactly what the tier ladder is for. We explicitly reject
invasive hardware fingerprinting as a countermeasure.
