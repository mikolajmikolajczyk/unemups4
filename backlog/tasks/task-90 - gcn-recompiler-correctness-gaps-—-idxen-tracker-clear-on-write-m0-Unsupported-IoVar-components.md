---
id: TASK-90
title: >-
  gcn: recompiler correctness gaps — idxen tracker clear-on-write, m0
  Unsupported, IoVar components
status: Done
assignee: []
created_date: '2026-07-12 13:10'
updated_date: '2026-07-12 13:24'
labels:
  - gpu
  - gcn
dependencies: []
priority: medium
ordinal: 89000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Code-review round-8 of task-89 (recompiler hardening). All 3 corpus shaders still recompile + pass spirv-val (independently verified) and every finding is corpus-UNEXERCISED — these are latent silent-divergence gaps in task-89's NEW code for non-corpus shaders, fixed before task-41's diff harness exercises them. All in crates/gcn (recompile.rs primarily). ITEMS: #1 HIGH (recompile.rs vertex_index_regs, emit_vop1/2/3 store paths ~774/853/931): the idxen vertex-index tracker propagates through v_mov but VOP1-cvt/VOP2/VOP3 writes to a tracked reg NEVER remove it from vertex_index_regs. A VS that clobbers a tracked reg via arithmetic (v_add v0,v0,k) then does idxen v0 leaves v0 stale-tracked, so the fetch reads gl_VertexIndex (the UNMODIFIED launch index) instead of the computed value -> silent divergence from the interp (which reads the actual VGPR). FIX: on EVERY VGPR write that is NOT a v_mov-from-a-tracked-reg (all store_reg_f32/store_reg_bits paths in emit_vop1 non-mov, emit_vop2, emit_vop3, cvt), REMOVE the dst reg from vertex_index_regs. Also track VOP3-encoded v_mov if it is in the subset, or accept it lowers to Unsupported. Prefer the simplest correct: clear-on-any-non-mov-write. Add a unit test: a synthetic Inst stream v_add v0 then idxen v0 -> the recompiler either emits Unsupported OR reads the modified value (NOT gl_VertexIndex), matching the interp. #2 MED (recompile.rs s_mov m0 + special_bits M0 ~683/719): task-89 made s_mov m0 a no-op that no longer evaluates ssrc0 (swallowing an out-of-range-SGPR error) AND an m0 SOURCE read loads the uninitialized M0_SLOT -> 0 silently. FIX: an m0 source read returns RecompileError::Unsupported (not a silent 0); s_mov m0 still bounds-validates ssrc0 (evaluate-and-discard, or an explicit bounds check) so a malformed SGPR field errors. #3 MED (recompile.rs read_ps_input IoVar + lib.rs): IoVar.components is coalesced to the max channel read (3 for .xyz) but the emitted SPIR-V Input variable is ALWAYS vec4 -> a provider (task-42) that sizes the VS param output from IoVar.components could emit a vec3 output against the vec4 PS input = spirv-val interface mismatch. FIX: document that interpolant IoVars are always vec4 in SPIR-V and components is channels-USED (provider MUST emit vec4 at the Location), OR set interpolant components to 4. Make the contract unambiguous for task-42. #4 LOW (recompile.rs ensure_vs_buffer ~1475): components frozen at the first MUBUF fetch count; a 2nd MUBUF with a different count leaves io_buffers[0].components under-reported. Record the MAX component count across fetches. #5 LOW (recompile.rs IoLayout ~189): the uses_num_records_push_constant bool DUPLICATES the PushConstantRole::NumRecords entry in push_constants (two sources of truth -> drift). Remove the bool; derive it (push_constants.iter().any(role==NumRecords)) at call sites (or make it a method). #7 LOW (recompile.rs Recompiler::new ~409): vertex_index_regs is seeded with v0 for ALL stages incl PS; seed it only for the Vertex stage (harmless today, wrong semantics). LOW CLEANUP: gl_VertexIndex-vs-interp draw-mode assumption (first_vertex+lane sequential = vkCmdDraw firstVertex; NOT vkCmdDrawIndexed) -> document the assumption on the emit site + IoLayout for task-42/53; the spirv-val/disasm test temp-file cleanup leaks on a write/launch panic (best-effort remove is fine but note it); declared_capabilities load_words .expect could name the shader on parse failure. Keep ps4-gcn Vulkan-free, no task-NN refs in new comments (conventions.md:20). All 3 corpus goldens MUST stay byte-identical (these paths are unexercised by the corpus, so no golden should change).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 any VGPR write other than v_mov-from-a-tracked-reg removes the dst from vertex_index_regs; a synthetic 'modify a tracked reg then idxen it' stream yields the modified value or Unsupported, never the stale gl_VertexIndex (unit test); matches the interp
- [ ] #2 an m0 source read returns RecompileError::Unsupported (no silent 0); s_mov m0 bounds-validates ssrc0
- [ ] #3 the IoVar.components-vs-vec4 interpolant contract is unambiguous for task-42 (documented or components=4); ensure_vs_buffer records max components; the uses_num_records bool is removed/derived
- [ ] #4 vertex_index_regs seeded only for VS; gl_VertexIndex draw-mode assumption documented; all 3 corpus goldens byte-identical; build/test/clippy/fmt green; ps4-gcn Vulkan-free
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
DONE 2026-07-12 (fix/task-90 @<prior-history>, merged). Round-8 review of task-89; corpus-unexercised silent-divergence gaps, all crates/gcn, ALL 3 goldens byte-identical (verified vs 0afd2ef). #1 idxen clear-on-write (HIGH): clear-on-any-non-mov-write. emit_vop1 captures src_is_tracked_index BEFORE mutation (v_mov vN,vM from tracked vM inserts vN else removes; self-move v0,v0 preserved), 4 cvt arms remove dst; emit_vop2/vop3 remove dst after resolving n. Test arithmetic_write_untracks_vertex_index_so_idxen_is_not_stale (v_add v0,v0,v0 then idxen v0 → RecompileError::Unsupported{offset:1}, never stale gl_VertexIndex). #2 m0: special_bits(M0)→Unsupported (was silent uninit-slot load); removed dead M0_SLOT const; s_mov m0 → new validate_scalar_src (bounds-checks SGPR ssrc0, accepts inline/literal, rejects m0-source+unmodeled specials) emitting NO SPIR-V (corpus s_mov m0,s0 golden unchanged, malformed SGPR now errors). #3 IoVar contract CHOSEN=document (not components=4): kept components=channels-used; doc on IoVar.components + read_ps_input note — every Location interface var ALWAYS vec4 in SPIR-V, provider MUST emit vec4 (narrower output fails spirv-val). #4 ensure_vs_buffer widens io_buffers[0].components to max across MUBUF. #5 removed uses_num_records_push_constant bool → IoLayout::uses_num_records() derived from push_constants. #6 vertex_index_regs seeded only for ShaderStage::Vertex. #7 documented sequential-draw (vkCmdDraw firstVertex NOT vkCmdDrawIndexed) assumption on emit site + IoLayout; cap-test load_words names shader on parse fail. TASK-41/42 MUST HONOR: (a) provider emits vec4 output per Location matching PS interpolant input (components=channels-used only); (b) VS driven by sequential vkCmdDraw (firstVertex-seeded gl_VertexIndex) not vkCmdDrawIndexed, matching interp first_vertex+lane; (c) idxen on arithmetic-clobbered reg → Unsupported = deferred not broken. 39 tests, Vulkan-free. Combined gate: 32 suites, oracle 6/6.
<!-- SECTION:NOTES:END -->
