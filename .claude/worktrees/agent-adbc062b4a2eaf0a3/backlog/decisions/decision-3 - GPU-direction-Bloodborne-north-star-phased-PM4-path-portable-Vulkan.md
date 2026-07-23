---
id: decision-3
title: 'GPU direction: Bloodborne north star, phased PM4 path, portable Vulkan'
date: '2026-07-10 18:21'
status: accepted
---

- **Status:** Accepted
- **Deciders:** Mikołaj Mikołajczyk

## Context

`backlog/docs/doc-2 - GPU-approach-research.md` surveyed what PS4 software needs
from the GPU stack, how existing emulators (shadPS4, GPCS4, fpPS4, Kyty, RPCSX)
build theirs, and the Rust ecosystem available. Its findings, taken as settled:

- The only interceptable surface is **libSceGnmDriver** submits carrying PM4
  command buffers — Gnm/Gnmx are statically linked into the game, so there are
  no high-level Gnm API calls to hook. "Gnm API-level HLE" is a dead end.
- Shaders arrive as **precompiled GCN machine code**, never source. Any GPU tier
  past framebuffer-only must handle GCN ISA (translate, interpret, or match).
- The one proven blueprint is **HLE-parse-PM4 + GCN→SPIR-V recompiler over
  Vulkan**. shadPS4's recompiler alone is ~35k LOC.
- unemups4's identity mapping (guest ptr == host ptr) means a PM4 parser or
  shader decoder can read command buffers and shader binaries straight out of
  guest memory with no translation layer.

doc-2 closed with a recommendation (Option A now, Option D incrementally,
Option C — the full recompiler — recorded as *permanently deferred*) and §7 open
questions. The maintainer has now answered §7 and set an **overriding goal** that
reframes the recommendation: the ceiling is not educational Tier A/B — it is
**Tier C, running Bloodborne**, as a multi-year north star. This decision records
the direction so future sessions don't re-litigate it.

## Decision

**North star: run Bloodborne** (Tier C commercial game). The phased path from
doc-2 stands — each phase must visibly work on its own — but the phases now
continue *through* a full GCN→SPIR-V recompiler rather than stopping short of it.
The recompiler is a late, mandatory phase, not a rejected option.

Concretely:

1. **Tiered phase path (doc-2 Option A → D → C), each phase demonstrable.**
   - Phase 1 — finish Tier A (videoout): task-18 (done), flip events → equeues,
     real flip counters, buffer-attribute honoring.
   - Phase 2 — Gnm boots and traces (doc-2 D1): stub the GnmDriver NIDs so Gnm
     homebrew boots; add a headless PM4 Type-3 trace decoder; hand-written PM4
     test ELF as the corpus.
   - Phase 3 — present/sync PM4 subset (D2): SubmitAndFlip → flip path, EOP
     events → equeues, CPDMA copies under identity mapping.
   - Phase 4+ — shaders (D3) then the **full GCN→SPIR-V recompiler** (D4 and
     beyond), the long pole toward Tier C.

2. **CPU shader interpreter first, recompiler second — interpreter as the
   correctness oracle.** This deliberately mirrors the project's proven
   interp→JIT pattern from the x86jit CPU migration: a from-scratch GCN
   interpreter establishes correctness and stays as the reference; the
   recompiler chases speed; differential tests run guest shaders through both
   and compare. The interpreter is never thrown away — it is the oracle.

3. **Graphics API: ash/Vulkan stays primary — and must remain swappable.**
   Vulkan (raw ash 0.38) is the backend. New constraint: the Vulkan layer must
   stay portable to **MoltenVK** (macOS, arriving in a few months) and
   potentially **native Metal**. Policy: **prefer the Vulkan portability subset;
   any non-portable extension must be gated behind a capability check with a
   graceful fallback.** Precedent already in-tree: task-18's
   `VK_EXT_external_memory_host` zero-copy import is *not* supported by MoltenVK,
   and its existing staging-copy fallback is exactly the required
   gate-and-fallback pattern — every future non-portable path follows it.

## Alternatives considered

- **wgpu instead of ash** (doc-2 §4): rejected. wgpu interposes naga validation
  on the SPIR-V path (a known friction point for machine-generated,
  unstructured GCN control flow) and does not expose
  `VK_EXT_external_memory_host` (task-18). ash + rspirv is the lower-friction
  path for emitting shaders we generate ourselves; portability is bought via the
  Vulkan portability subset + MoltenVK, not via wgpu.

- **"Gnm API-level HLE" (intercept high-level Gnm/shader calls)** (doc-2 §5
  Option B): rejected as infeasible — Gnm is statically linked (no calls cross
  the boundary) and shaders are precompiled GCN (no high-level form to
  intercept). Recorded so it is not re-attempted.

- **Permanently deferring the full recompiler** (doc-2 §6 recommendation, Option
  C): **rejected / un-deferred.** doc-2 recommended parking Option C in
  `deferred.md` with "revisit: never, absent a change of mission." The Bloodborne
  north star *is* that change of mission. The recompiler is now a scheduled late
  phase, not a permanent deferral, and must not be recorded as deferred.

## Consequences

- Positive: a single durable direction; each phase ships something visible;
  the interp-as-oracle discipline gives the shader work a correctness anchor
  the same way x86jit's did; the portability policy keeps the mac/Metal door
  open from day one instead of being retrofitted.
- Positive: doc-2's dead ends (wgpu, API-level Gnm) are on record, saving future
  sessions the re-derivation.
- Negative / accepted cost: the north star is multi-year and, taken to the end,
  *becomes the project* — it sits in tension with the "lightweight, educational,
  not a faithful reimplementation" ethos. Mitigated by hard phase gates: every
  phase is independently useful, so stalling before Tier C still leaves a
  working, demonstrable artifact (Gnm homebrew boots + traces; present/sync
  works; first triangles draw).
- Non-GPU work is required for Bloodborne but out of scope here: FSELF loading of
  decrypted dumps, AJM audio, much broader libkernel, savedata. These are named
  as **future, unscheduled workstreams** in doc-2 — not tasks yet.

## Trigger to revisit

If the project's mission narrows back to educational Tier A/B (dropping the
Bloodborne north star), re-defer the full recompiler and record Option C in
`deferred.md`. If macOS/Metal support is dropped, relax the portability policy.
