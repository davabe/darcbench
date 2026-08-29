# ADR-0018 — GPU API selection: wgpu, pending a backend-variance measurement

**Status:** Proposed — not Accepted. The experiment in "What would settle this" has not been run
**Date:** 2026-08-29
**Phase:** 3 (client line)
**Supersedes:** nothing
**Related:** [ADR-0015](0015-two-product-lines-one-engine.md), [ADR-0016](0016-client-reference-darc-ref-c1.md),
[ADR-0017](0017-engine-shell-process-separation.md), [SCORING-SYSTEM §6](../SCORING-SYSTEM.md)

## Context

The client line cannot compete without GPU measurement. Geekbench 6 has GPU
Compute; 3DMark is nothing else. A client benchmark with no GPU category is not
in the market.

The choice of API is not an implementation detail here, for three reasons:

1. **It is cross-vendor or it is worthless.** NVIDIA, AMD, Intel and Apple all
   have to be measurable on the same scale.
2. **The anchor is NVIDIA silicon** ([ADR-0016](0016-client-reference-darc-ref-c1.md)).
   Any workload that favours the anchor's vendor turns the reference into a
   thumb on the scale, and that is precisely the accusation this project cannot
   afford.
3. **It interacts with the one-baseline commitment.** A translation layer in one
   platform's path is a performance delta attributable to software, sitting
   inside a number we present as a property of hardware.

## Decision (proposed)

**wgpu**, targeting native DirectX 12, Vulkan and Metal backends — subject to the
measurement below.

The reasoning is that one Rust codebase reaches all three platforms on native
backends, and the same API serves the compute tests, the graphics render test and
the visual demo that [ADR-0017](0017-engine-shell-process-separation.md) makes
the spectacle of the product. One dependency for three jobs across three
platforms.

**API is a displayed dimension of the score, never silently pooled.** GPU results
carry the backend they were produced on, and results from different backends are
not presented as directly comparable. This is the established industry treatment
— Geekbench reports GPU Compute tagged by API — and it is the same rule this
project already applies to execution scope under
[SCORING-SYSTEM §6](../SCORING-SYSTEM.md) rule 6, and to OS under the rule 7
added by [ADR-0016](0016-client-reference-darc-ref-c1.md).

## Alternatives

**Native backends written separately (D3D12 + Metal + Vulkan).** Maximum control,
no intermediate layer, and no shared abstraction whose overhead has to be
characterised. This is the fallback if the measurement below goes against wgpu,
and it is the only alternative not rejected outright. Cost: three
implementations, which is exactly the shape [ADR-0015](0015-two-product-lines-one-engine.md)
spent its argument avoiding elsewhere.

**Vulkan everywhere, via MoltenVK on macOS.** Rejected. MoltenVK is a translation
layer onto Metal. Its overhead would appear in macOS GPU scores as a hardware
difference, poisoning exactly the cross-platform comparability that justifies the
whole client line.

**CUDA.** Rejected outright. NVIDIA-only in a cross-vendor benchmark is
disqualifying on its own; combined with an NVIDIA anchor it is the worst
available combination and would be read, correctly, as a rigged reference.

**OpenCL.** Rejected. Deprecated on macOS since 2018. Choosing a
deprecated API for a product line that must last is choosing a rewrite.

## What would settle this

wgpu is an abstraction over three backends, and abstractions have variance. The
question that decides this ADR is whether that variance is small enough to sit
inside a benchmark:

> On the anchor host, run identical compute kernels through wgpu's DX12 and
> Vulkan backends, and through hand-written native D3D12 and Vulkan, on the same
> GPU under the same OS. Measure the wgpu-versus-native overhead per backend, and
> the spread between backends.

If wgpu's overhead is consistent across backends, it is a constant that
normalisation absorbs and wgpu is Accepted. If it differs materially *between*
backends, wgpu is injecting a software delta into a hardware comparison and the
decision flips to native backends.

This is the same discipline [ADR-0016](0016-client-reference-darc-ref-c1.md)
applies to cross-OS variance: measure the residual before building on top of it,
and disclose what remains.

## Consequences

- GPU category weights, and whether GPU is required for a rankable client total,
  are deliberately **not** decided here. They belong to the client scoring model
  and depend on numbers that do not exist yet.
- Shader source is part of the workload definition and falls under the workload
  versioning rules: a shader change is a workload version change and breaks
  comparability, exactly like `CORPUS_SEED`.
- GPU telemetry needs a vendor-neutral path (DXGI adapter info, D3D queries).
  NVML would cover the anchor completely and no other vendor at all, which is
  the wrong dependency to build on.

## Revisit if

The measurement above goes against wgpu; a backend is deprecated; or the client
line needs a raster/graphics score distinct from compute, which may not share
this API decision.
