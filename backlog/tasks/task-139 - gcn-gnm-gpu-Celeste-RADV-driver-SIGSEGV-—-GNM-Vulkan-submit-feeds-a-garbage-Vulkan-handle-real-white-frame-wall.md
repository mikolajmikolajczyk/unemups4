---
id: TASK-139
title: >-
  gcn/gnm/gpu: Celeste RADV driver SIGSEGV — GNM->Vulkan submit feeds a garbage
  Vulkan handle (real white-frame wall)
status: Done
assignee: []
created_date: '2026-07-16 12:11'
updated_date: '2026-07-16 12:46'
labels:
  - gnm
  - gpu
  - gcn
  - celeste
  - retail
  - bug
dependencies: []
priority: high
ordinal: 145000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
The REAL Celeste (CUSA11302) white-frame blocker, precisely attributed by task-137 coredump analysis. Celeste boots Mono + submits GNM PM4, but the process host-SIGSEGVs (rc=139, SEGV_MAPERR) with the faulting RIP inside /usr/lib/libvulkan_radeon.so (RADV driver) on the MAIN PRESENT THREAD, ~65ms after sceGnmSubmitAndFlipCommandBuffers count=2. Faulting instr 'cmpl $0x3b9ce510,0xb0(%rax)', %rax=0x38 — 0x3b9ce510 is a RADV-internal sentinel; RADV is validating a GARBAGE Vulkan handle we handed it. NOT guest code, NOT the Graphics:: dlsym path (task-137 ruled that out — its trap-stub never fires; managed code checks ENOENT). So: our GNM->Vulkan submission builds a malformed/garbage Vulkan object (pipeline/descriptor/image/handle) that crashes RADV during submit or present.

PRIME LEAD (from task-135): every Celeste IT_DMA_DATA is memory->REGISTER (DAS=1, dst ~0x3022c register offset, byte_count 92..196) and we DEFER register-space DMA. If those DMAs are how Celeste programs its draw context/GPU registers (vertex/const/RT state), deferring them leaves the draw state incomplete, so the pipeline/binding we synthesize is garbage -> RADV rejects/segfaults. Investigate whether mem->register IT_DMA_DATA must be modeled as writes into our shadow register file (crates/gnm), and whether a specific Vulkan object in the submit/present path (crates/gpu backend / vulkan.rs) is malformed.

APPROACH: run Celeste under Vulkan validation (VK_LAYER_KHRONOS_validation=1) and/or RADV_DEBUG (e.g. RADV_DEBUG=llvm as a known ACO-avoidance, plus hang/syncshaders for object errors) to catch the bad object BEFORE the raw segfault, so we get a validation message naming the handle/object instead of a driver crash. Then trace it back: is it a null/garbage VkImage/VkBuffer/VkPipeline from an incomplete draw, a descriptor pointing at an unbacked resource, or a flip target that was never created? Cross-reference the mem->register DMA lead. Assets at /home/mikolaj/PS4/CUSA11302 gitignored, NEVER commit. RUST_LOG=warn,ps4_gnm=info (firehose kills runs). PNG oracle for any frame claim (logs lie).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 The RADV/libvulkan_radeon.so host SIGSEGV during Celeste's GNM submit/present is root-caused: the specific malformed Vulkan object (or the missing draw state that produces it) is named, with evidence from a validation-layer run
- [x] #2 Fix lands so Celeste no longer crashes RADV at submit/present (validation-clean, or the malformed object is no longer built); re-run reaches the next wall
- [ ] #3 If the cause is the mem->register IT_DMA_DATA deferral (task-135 lead): mem->register DMA is modeled as shadow-register writes (or the correct handling determined), and the draw state is complete
- [ ] #4 Live: Celeste re-run past this crash; PNG dumped (UNEMUPS4_DUMP_PNG) for the orchestrator to Read — report whether any geometry appears
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Done 2026-07-16 (merge 1beccfb). Root cause via VK_LAYER_KHRONOS_validation: VUID-07988 — PS s_buffer_load emits set0/bind2 STORAGE_BUFFER in FRAGMENT SPIR-V but exec only harvested const_buffers from VS + backend hardcoded that descriptor stage_flags=VERTEX -> layout mismatch -> vkCreateGraphicsPipelines returns garbage handle -> RADV segfaults at present. Fix: derive_draw_state harvests CB from whichever stage declares it (VS|PS), tracks Stage, reads V# from declaring stage SGPR block, both-stages-declare defers (single set0/bind2 slot, strict-or-defer); CreatePipeline carries const_storage_fragment; backend sets stage_flags FRAGMENT/VERTEX. RADV crash CLEARED (rc 139->124, survives, VUID-07988 count 0). Frame now clean BLACK (no crash, no geometry). NOT the mem->register DMA lead (unrelated). 289 tests green. Method lesson: run under validation layer to NAME the bad object before the raw driver segfault. AC#3/#4 (geometry) blocked on NEXT wall -> task-141.
<!-- SECTION:NOTES:END -->
