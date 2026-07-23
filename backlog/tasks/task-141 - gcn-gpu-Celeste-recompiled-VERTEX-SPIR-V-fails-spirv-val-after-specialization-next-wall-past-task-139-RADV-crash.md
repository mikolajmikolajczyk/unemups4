---
id: TASK-141
title: >-
  gcn/gpu: Celeste recompiled VERTEX SPIR-V fails spirv-val after specialization
  (next wall past task-139 RADV crash)
status: Done
assignee: []
created_date: '2026-07-16 12:40'
updated_date: '2026-07-16 13:15'
labels:
  - gcn
  - gpu
  - celeste
  - retail
  - bug
dependencies:
  - TASK-130
ordinal: 147000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Exposed by task-139 fix (which cleared the RADV SIGSEGV from a garbage pipeline layout). With Celeste (CUSA11302) now surviving GNM submit, running under VK_LAYER_KHRONOS_validation shows the recompiled VERTEX shader module fails spirv-val: VUID-VkShaderModuleCreateInfo-pCode-08737 (vkCreateShaderModule: 'spirv-val produced an error') and VUID-VkPipelineShaderStageCreateInfo-pSpecializationInfo-06849 ('After specialization was applied, VkShaderModule produces a spirv-val error, stage VK_SHADER_STAGE_VERTEX_BIT'). So vkCreateGraphicsPipelines still yields no usable pipeline -> no geometry (frame BLACK, PNG oracle). The failing module is the VS with the task-128 stride specialization constant. Repro: VK_LAYER_PATH=<steam validation layer> VK_INSTANCE_LAYERS=VK_LAYER_KHRONOS_validation RUST_LOG=warn,ps4_gnm=info LD_LIBRARY_PATH=/usr/lib run eboot.bin. Investigate: dump the offending VS SPIR-V, run spirv-val standalone to get the exact rule violated; likely the recompiler emits invalid SPIR-V for one of Celeste's VS instruction shapes or the stride spec-constant folds to an illegal constant. crates/gcn recompile.rs + the spec-constant path (task-128). NOT the layout/descriptor path (task-139, fixed). Assets gitignored, never commit; PNG oracle for frame claims.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 The exact spirv-val rule the recompiled VS violates is identified (dump the SPIR-V + run spirv-val standalone)
- [x] #2 Root-caused: is it a recompiler codegen bug or the stride spec-constant folding to an illegal constant?
- [x] #3 Fix lands; the VS module passes spirv-val (validation-clean), vkCreateGraphicsPipelines yields a usable pipeline
- [x] #4 Live: Celeste re-run, PNG dumped, report whether geometry appears
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Done 2026-07-16 (merge 014e76e). Root cause = codegen bug NOT stride spec-constant: 'ID decorated with ArrayStride multiple times' — ensure_const_buffer + ensure_vs_buffer each build a structurally-identical OpTypeStruct{runtimearray uint} + each emit ArrayStride/Block/Offset; rspirv dedups the type to one id -> decorated twice -> illegal. Only fires when a VS has BOTH const-buffer AND vertex-fetch SSBO (Celeste's does; single-SSBO corpus never did). Fix: memoize dword_ssbo_block, decorate once, both paths reuse. recompile.rs only +47/-34, golden byte-identical, spirv-val green on all 3 Celeste modules, 315 tests. VUID-08737/06849 CLEARED. Does NOT close 140/128. PNG STILL BLACK (orchestrator Read it, 100% #000000): guest now hits a CPU-side wall -> x86jit UnknownInstruction at guest 0xd5bc00 (SIGILL vector 68 at 0x982e26) BEFORE geometry draws. Next = characterize that opcode + file x86jit lift task (task-144).
<!-- SECTION:NOTES:END -->
