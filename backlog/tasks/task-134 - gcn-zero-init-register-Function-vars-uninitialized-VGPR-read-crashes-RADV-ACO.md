---
id: TASK-134
title: >-
  gcn: zero-init register Function vars (uninitialized-VGPR read crashes
  RADV-ACO)
status: Done
assignee: []
created_date: '2026-07-16 08:18'
updated_date: '2026-07-16 09:43'
labels:
  - from-celeste
  - gcn
  - bug
dependencies: []
ordinal: 140000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Celeste bring-up finding (doc-6 Entry 11, task-113.4.1 Celeste-wall agent): the intermittent executor SIGSEGV is a RADV ACO shader-compiler crash inside vkCreateGraphicsPipelines on Celeste's recompiled VS/PS — NOT a bounded-seam/race bug (both disproven by audit). The recompiled SPIR-V passes spirv-val and compiles clean under RADV_DEBUG=llvm, but ACO segfaults because a recompiled VS reads an UNINITIALIZED Function uint: the recompiler creates Function-storage OpVariable register slots (u32+f32 pair) and a shader that reads a register never written in that shader gets an undefined-value read, which ACO mishandles (heap/ASLR-sensitive → intermittent, masked by logging). Fix: give every register Function OpVariable (VGPR/SGPR u32+f32 views, and any predicate/m0 var) a zero initializer at declaration (OpVariable ... %const_0), so an unwritten register reads 0 — defined, ACO-safe, and matches real GCN power-on/defined-behavior expectations + the existing m0_ptr zero-init pattern. Verify: recompiled Celeste VS/PS compile under default RADV-ACO without SIGSEGV; interp oracle already defaults unwritten regs to 0 so the differential/value-oracle (task-122) stays green.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 every register (VGPR/SGPR) Function OpVariable is emitted with a zero (const 0) initializer
- [x] #2 recompiled shaders no longer read an uninitialized Function variable (spirv-val + a check that no OpVariable lacks an initializer on the value path)
- [x] #3 differential + CPU-SPIR-V value oracle (task-122) stay green (interp already zero-defaults regs)
- [x] #4 Celeste VS/PS build under default RADV-ACO without the vkCreateGraphicsPipelines SIGSEGV (RADV_DEBUG=llvm workaround no longer needed)
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
AC#4 CONFIRMED via live smoke 2026-07-16: 3/3 clean Celeste runs under DEFAULT RADV (RADV_DEBUG unset) — exit 137 (SIGKILL at timeout = survived), 0 crash markers (no SIGSEGV/ACO/vkCreateGraphicsPipelines/panic), guest reaches GPU PM4 submit + steady-state. Previously ~1/2 runs SIGSEGV'd. The zero-init register vars fix cleared the RADV-ACO uninitialized-Function-read crash. Guest now runs indefinitely submitting frames. Next visible walls: (1) game dlsym's its own native Graphics::GraphicsSystem::*/Graphics::Texture::*/Graphics::RenderTarget::Init2D C++ symbols -> not found (new missing-symbol class, may gate real rendering); (2) unhandled PM4 IT_DMA_DATA(0x50) + IT_INDEX_BUFFER_SIZE(0x13); (3) task-56 steps 3-4 (RT-as-texture backend + multi-pass) for visible pixels.
<!-- SECTION:NOTES:END -->
