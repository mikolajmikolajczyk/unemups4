---
id: decision-3
title: 'GPU direction: Bloodborne north star, phased PM4 path, portable Vulkan'
date: '2026-07-10 18:21'
status: accepted
---

- **Status:** Accepted
- **Deciders:** Mikołaj Mikołajczyk

## Context

Settled findings about what PS4 software needs from the GPU stack:

- The only interceptable surface is **libSceGnmDriver** submits carrying PM4
  command buffers — Gnm/Gnmx are statically linked into the game, so there are
  no high-level Gnm API calls to hook. "Gnm API-level HLE" is a dead end.
- Shaders arrive as **precompiled GCN machine code**, never source. Any GPU
  path past framebuffer-only must handle GCN ISA (translate, interpret, or
  match).
- The proven blueprint is **HLE-parse-PM4 + GCN→SPIR-V recompiler over
  Vulkan**. Such a recompiler alone is tens of thousands of lines.
- unemups4's identity mapping (guest ptr == host ptr) means a PM4 parser or
  shader decoder can read command buffers and shader binaries straight out of
  guest memory with no translation layer.

The maintainer set an **overriding goal**: the ceiling is not educational
homebrew — it is a **full commercial game, running Bloodborne**, as a
multi-year north star. This decision records the direction so future sessions
don't re-litigate it.

## Decision

**North star: run Bloodborne** (a full commercial game). The GPU path is
built incrementally — each increment must visibly work on its own — and it
continues *through* a full GCN→SPIR-V recompiler rather than stopping short of
it. The recompiler is a late, **mandatory** phase, not a rejected option.

1. **A complete PM4 + GCN→SPIR-V recompiler is the target.** The path runs
   from software-framebuffer output (videoout) through the PM4 present/sync
   subset and shaders to the full recompiler — the long pole toward running a
   commercial title.

2. **CPU shader interpreter first, recompiler second — interpreter as the
   correctness oracle.** This deliberately mirrors the project's proven
   interp→JIT pattern from the x86jit CPU migration: a from-scratch GCN
   interpreter establishes correctness and stays as the reference; the
   recompiler chases speed; differential tests run guest shaders through both
   and compare. The interpreter is never thrown away — it is the oracle.

3. **Graphics API: ash/Vulkan stays primary — and must remain swappable.**
   Vulkan (raw ash 0.38) is the backend. The Vulkan layer must stay portable
   to **MoltenVK** (macOS) and potentially **native Metal**. Policy: **prefer
   the Vulkan portability subset; any non-portable extension must be gated
   behind a capability check with a graceful fallback.** Precedent already
   in-tree: `VK_EXT_external_memory_host` zero-copy import is *not* supported
   by MoltenVK, and its existing staging-copy fallback is exactly the required
   gate-and-fallback pattern — every future non-portable path follows it.

## Alternatives considered

- **wgpu instead of ash**: rejected. wgpu interposes naga validation on the
  SPIR-V path (a known friction point for machine-generated, unstructured GCN
  control flow) and does not expose `VK_EXT_external_memory_host`. ash + rspirv
  is the lower-friction path for emitting shaders we generate ourselves;
  portability is bought via the Vulkan portability subset + MoltenVK, not via
  wgpu.

- **"Gnm API-level HLE" (intercept high-level Gnm/shader calls)**: rejected as
  infeasible — Gnm is statically linked (no calls cross the boundary) and
  shaders are precompiled GCN (no high-level form to intercept). Recorded so it
  is not re-attempted.

- **Permanently deferring the full recompiler**: **rejected / un-deferred.**
  Parking it as "revisit: never, absent a change of mission" no longer holds —
  the Bloodborne north star *is* that change of mission. The recompiler is a
  scheduled, mandatory phase, not a permanent deferral, and must not be
  recorded as deferred.

## Consequences

- Positive: a single durable direction; each increment ships something
  visible; the interp-as-oracle discipline gives the shader work a correctness
  anchor the same way x86jit's did; the portability policy keeps the mac/Metal
  door open from day one instead of being retrofitted.
- Positive: the dead ends (wgpu, API-level Gnm) are on record, saving future
  sessions the re-derivation.
- **The north-star capability is now proven for one title.** Celeste — a
  commercial title — runs the full GCN→SPIR-V recompiler end-to-end to
  gameplay. The complete PM4 + recompiler + portable-Vulkan stack works on a
  real shipped game; Bloodborne is a harder one of the same kind, not a
  different bet.
- Negative / accepted cost: the north star is multi-year and, taken to the end,
  *becomes the project* — it sits in tension with the "lightweight, educational,
  not a faithful reimplementation" ethos. Mitigated because each increment is
  independently useful, so stalling short of Bloodborne still leaves a working,
  demonstrable artifact.
- Non-GPU work is required for Bloodborne but out of scope here: FSELF loading
  of decrypted dumps, AJM audio, much broader libkernel, savedata. These are
  named as **future, unscheduled workstreams** — not tasks yet.

## Trigger to revisit

If the project's mission narrows back to educational homebrew (dropping the
Bloodborne north star), re-defer the full recompiler. If macOS/Metal support is
dropped, relax the portability policy.
