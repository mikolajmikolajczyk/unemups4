---
id: decision-6
title: >-
  Phase 4 architecture: GCN decode→interp→recompiler, resource cache, state
  model
date: '2026-07-11 12:56'
status: proposed
---
## Context

Phase 4 turns the PM4 pipeline into a real GPU path: decode guest GCN shaders,
run them for correctness, recompile them to portable SPIR-V, model the render
state, and cache guest-memory-backed resources. This spans crates gcn, gnm,
gpu, core and converges on two runnable milestones (a real-shader triangle,
then a textured quad). Spec = doc-4 §4/§5/§8/§C3/C4/C7/C8/C9 + doc-3.

## Decision

Lock: phase-4 is 21 tasks in 5 sub-phases (4a decode+interp / 4b recompiler /
4c state / 4d cache / 4e integration) converging on P4-18 (real-shader triangle
keystone) then P4-20 (textured draw). Spec = doc-4 §4/§5/§8/§C3/C4/C7/C8/C9 +
doc-3. Key commitments:

- CPU wave64 interpreter (P4-04) is the differential ORACLE, built BEFORE the
  recompiler (P4-05) and never discarded (decision-3). Recompiler coverage
  grows per-milestone in lockstep with the interp.
- All shader binds route through ShaderProvider chain [Embedded, Gcn];
  recompiled SPIR-V stays MoltenVK/Metal-portable (decision-3), spirv-val-gated.
- Resource cache (doc-4 §8) uses the x86jit watch_range/take_dirty_ranges
  DirtySource (pinned 26bc5ec); sequencing buffers→textures→RT-readback (§8.6).
- Executor/gcn stay Vulkan-free; SPIR-V + texture data cross the existing
  BackendCmd/RunCommandList channel (BackendCmd loses Copy, carries Arc<[u32]>
  on pipeline-cache miss).
- ORACLE HIERARCHY (added 2026-07-11, maintainer): ground-truth tiers, strongest
  first — (0) **real PS4 hardware = the ULTIMATE oracle** (capture actual
  framebuffers/behavior on-device; nothing overrides it). BLOCKED: maintainer must
  acquire a console — deferred like task-29 until hardware is on hand; file a
  capture-reference task then. (1) **shadPS4** = the external proxy we use NOW
  (below). (2) our **interp** = internal consistency only. As soon as a real PS4 is
  available, its captures supersede shadPS4 for any shader/scene where they differ.
- EXTERNAL ORACLE (added 2026-07-11, maintainer): the interp-vs-recompiler
  differential proves INTERNAL consistency only — a shared misread of a GCN
  encoding (or a wrong hand-authored golden) passes both. So phase-4 correctness
  is ALSO cross-validated against **shadPS4** (a mature GCN→SPIR-V PS4 emulator),
  at two tiers: **Tier 1 black-box** (task-58) — the same guest ELF renders
  equivalently on shadPS4 and unemups4 (framebuffer compare; manual gate first,
  then tolerance-diff); **Tier 3 decoder cross-check** (task-57) — our GCN
  disassembly matches shadPS4's for the corpus. Tier 2 (shadPS4's translator as
  reference SPIR-V) is deferred. shadPS4 is built locally + gitignored (like
  data/oo_sdk); the checks skip cleanly when absent. This makes the corpus ELFs a
  real target we don't control the "right answer" for.
- CONFIRMED SCOPING: (1) corpus = self-authored synthetic .sb (llvm-mc) that MUST
  be GENUINELY runnable on an external emulator (real OrbShdr bytes/addresses/
  vertex buffer, not markers) PLUS **freegnm's triangle vendored (MIT/UNLICENSE)
  as the shared cross-emulator corpus** (promoted from "later stretch" — it is a
  known-good homebrew proven on shadPS4). psbc = later stretch; Bloodborne .sb =
  local-oracle-only, never committed. (2) tiling = linear + 1D-thin only; 2D-macro
  + DCC/HTILE deferred (fields carried). (3) spirv-tools vendored as dev-dep. (4)
  wave64+EXEC from day one; interp gate = triangle-subset goldens.

## Consequences

NON-GOALS in phase 4: compute/ACE async queues, tessellation/GS execution,
DCC/HTILE decode, 2D macro-tiling, mips/cube/3D/anisotropy, per-title config,
structured-CFG recompilation beyond simple branches.
