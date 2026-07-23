---
id: decision-6
title: >-
  GCN shader architecture: decode→interp→recompiler, resource cache, state model
date: '2026-07-11 12:56'
status: accepted
---
## Context

Turning the PM4 pipeline into a real GPU path: decode guest GCN shaders, run them
for correctness, recompile them to portable SPIR-V, model the render state, and
cache guest-memory-backed resources. This spans crates gcn, gnm, gpu, core.
Spec = doc-2 §4/§5/§8/§C3/C4/C7/C8/C9.

## Decision

The GPU path is built as decode → CPU wave interpreter → GCN→SPIR-V
recompiler, with a guest-side resource cache and a shadow-register state model.
Key commitments:

- The CPU wave64 interpreter is the differential correctness ORACLE. It is
  built BEFORE the recompiler and never discarded (decision-3). Recompiler
  coverage grows in lockstep with the interpreter, cross-checked against it.
- All shader binds route through a ShaderProvider chain [Embedded, Gcn];
  recompiled SPIR-V stays MoltenVK/Metal-portable (decision-3), spirv-val-gated.
- The resource cache (doc-2 §8) uses the x86jit watch_range/take_dirty_ranges
  DirtySource; sequencing buffers→textures→RT-readback (§8.6).
- Executor/gcn stay Vulkan-free; SPIR-V + texture data cross the existing
  BackendCmd/RunCommandList channel (BackendCmd carries Arc<[u32]> on
  pipeline-cache miss).
- ORACLE HIERARCHY (maintainer), strongest first: (0) **real PS4 hardware = the
  ULTIMATE oracle** — capture actual framebuffers/behavior on-device; nothing
  overrides it. Deferred until the maintainer has a console on hand. (1) **real
  PS4 command-stream/framebuffer captures** = the external proxy used in the
  meantime. (2) our **interpreter** = internal consistency only. As soon as live
  on-device capture is available, it supersedes replayed captures for any
  shader/scene where they differ.
- EXTERNAL ORACLE (maintainer): the interp-vs-recompiler differential proves
  INTERNAL consistency only — a shared misread of a GCN encoding (or a wrong
  hand-authored golden) passes both. So correctness is ALSO cross-validated
  against the real PS4 console capture (ground-truth framebuffers and command
  streams) two ways: a **black-box** check — the same guest ELF renders
  equivalently to the console's captured framebuffer (manual gate first, then
  tolerance-diff) — and a **decoder cross-check** — our GCN disassembly matches
  the reference **llvm-mc / AMD Sea Islands ISA** disassembly for the corpus. A
  reference SPIR-V translator tier is deferred. Captures are stored locally +
  gitignored (like data/oo_sdk); the checks skip cleanly when absent. This makes
  the corpus ELFs a real target we don't control the "right answer" for.
- SCOPING: (1) the corpus is self-authored synthetic .sb (llvm-mc) that MUST be
  GENUINELY runnable real GCN (real OrbShdr bytes/addresses/vertex buffer, not
  markers), PLUS **freegnm's triangle vendored (MIT/UNLICENSE) as the shared
  cross-checkable corpus** (a known-good homebrew triangle). psbc is a later
  stretch; Bloodborne .sb is local-oracle-only, never committed. (2) tiling =
  linear + 1D-thin only; 2D-macro + DCC/HTILE deferred (fields carried). (3)
  spirv-tools vendored as dev-dep. (4) wave64+EXEC from day one; interp gate =
  triangle-subset goldens.

## Consequences

NON-GOALS: compute/ACE async queues, tessellation/GS execution, DCC/HTILE
decode, 2D macro-tiling, mips/cube/3D/anisotropy, per-title config,
structured-CFG recompilation beyond simple branches.
