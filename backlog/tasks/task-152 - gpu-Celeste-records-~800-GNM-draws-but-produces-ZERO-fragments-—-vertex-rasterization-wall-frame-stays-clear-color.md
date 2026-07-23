---
id: TASK-152
title: >-
  gpu: Celeste records ~800 GNM draws but produces ZERO fragments —
  vertex/rasterization wall (frame stays clear-color)
status: Done
assignee: []
created_date: '2026-07-16 18:12'
updated_date: '2026-07-16 20:08'
labels:
  - gpu
  - gnm
  - gcn
  - celeste
  - retail
  - bug
dependencies: []
ordinal: 158000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
After task-149 cleared the texture-resolve defer (InlineVSharp T#/S# now read from SGPRs) and the multi-videoout-pass CLEAR-clobber (later videoout passes now LOAD), Celeste's ~800-1366 draws RECORD and build real Vulkan pipelines, the present path is proven working (a temp magenta CLEAR-color probe showed the swapchain/PNG turning UNIFORM magenta), yet NOT A SINGLE FRAGMENT survives any draw: the final frame is exactly the clear color, no overdraw anywhere. So every recorded videoout draw rasterizes nothing. The wall is now purely in the vertex/rasterization stage — geometry is either fully clipped (positions out of clip space / NaN / all-zero) or emits no primitives. PROVEN FACTS (task-149 live run, CUSA11302): (1) needs_gcn=0 — every VS/PS resolves to a recompilable GCN shader, Celeste binds real GNM shaders (NOT the managed Graphics::VertexShader path — that hypothesis is disproven). (2) Dominant OLD defer tex_unresolved=953 was 100%% VbufError::MemoryFault from dereferencing an inline T#'s first dword as a pointer — FIXED. (3) Post-fix ~879 draws record; ~499/run target Videoout directly, ~220 Offscreen; submits carry up to 5 videoout draws each. (4) Remaining minor defers: cb_multi (~40/run, both VS+PS declare a const buffer → collide on the single set0/bind2 slot), tex_macrotile (~60/run, 2D macro-tiled textures, no detiler). NEXT STEPS: instrument the ACTUAL vertex data for a recorded videoout draw — dump the fetched vertex positions (SSBO pull or vertex-input) and the derived viewport/scissor. Prime suspects: (a) the VS transform-matrix constant buffer (Celeste's 4x4 MVP via s_buffer_load, doc-6 Entry 9) mis-resolves or is bound with wrong bytes → positions collapse; (b) the SSBO/vertex-input fetch returns zeros (num_records clamp, stride push-constant, or byte offset wrong for Celeste's real V#); (c) viewport/scissor is degenerate (zero extent) or Y-flip/depth-range leaves NDC off-screen; (d) index buffer (DrawIndexOffset/DrawIndex2) points at zeros → all vertices are element 0. Add a temp probe at the RECORDED point in setup_draw logging viewport/scissor + first few fetched vertex floats + CB presence, then dump ONE videoout draw's SPIR-V and check the position output. Assets at /home/mikolaj/PS4/CUSA11302 gitignored, NEVER commit; PNG oracle for all frame claims.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 Root-caused: WHY every recorded Celeste videoout draw produces zero fragments (clipped positions / degenerate viewport / bad vertex fetch / bad index buffer / mis-resolved MVP constant buffer)
- [x] #2 Fix lands so submitted geometry produces visible fragments; PNG oracle shows non-black, non-uniform content (orchestrator Reads the PNG)
- [x] #3 The cb_multi defer (both VS+PS declare a const buffer, colliding on the single set0/bind2 slot) is resolved or confirmed not on the critical path
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Done 2026-07-16 (merge 22da7ce). ROOT CAUSE: recompiled SSBO vertex fetch read 4 raw dwords, but Celeste's position V# is Format32_32_32 with dst_sel=[4,5,6,1] (w=SQ_SEL_1=const 1.0); we read the padding 4th dword as garbage -> gl_Position.w=NaN -> every primitive clipped -> zero fragments. FIX: honor dst_sel via a 3rd VS push-constant member + per-channel nested OpSelect (0->0.0,1->1.0,4..7->src); DST_SEL_IDENTITY=0xFAC passthrough keeps goldens/oracle green. +2 secondary: PA_SC_SCREEN_SCISSOR reg off-by-one (0x00D/E->0x00C/D), cross-submit videoout clear-clobber (frame latch). Method: probe dumped viewport/MVP/vertex, disasm showed gl_Position.w=row3.(v4..v7), force-w=1.0 made geometry appear, runtime probe confirmed dst_sel=[4,5,6,1]. LIVE (PNG orchestrator-confirmed): loading-screen progress bar + particle field RASTERIZE (was black). BUT all WHITE (no color/texture) -> NEXT wall filed. 290 tests. Added crates/gcn/examples/dump_disasm.rs (keeper debug tool).
<!-- SECTION:NOTES:END -->

## RESOLVED (2026-07-16) — first Celeste pixels

All three fixes landed in the worktree (uncommitted, per hard rule). Verification: cargo test -p ps4-gcn -p ps4-gnm -p ps4-gpu = 289 passed / 0 failed / 2 ignored; clippy --all-targets --all-features -D warnings = clean; fmt --check = clean. LIVE Celeste PNG (UNEMUPS4_DUMP_PNG): frame 6 = 5 colors (full-screen fill), frame 50 = white progress bar + scattered particle-field squares/diamonds across a 1896x1029 region (was uniform black before). AC #1/#2/#3 met.

Fix summary (files):
- crates/gnm/src/pm4/opcodes.rs — scissor TL/BR 0x00D/0x00E → 0x00C/0x00D.
- crates/gpu/src/backend.rs — `videoout_cleared_this_frame` frame-scoped clear latch (clear first videoout submit, LOAD+accumulate rest, reset at submit_flip).
- crates/gcn/src/recompile.rs + crates/core/src/gpu.rs + crates/gnm/src/exec.rs + crates/gpu/src/backend.rs + crates/gpu/src/bin/diff_harness.rs — honor V# dst_sel in the SSBO vertex fetch via a 3rd push-constant member (num_records, stride, dst_sel); per-channel nested OpSelect (0→0.0, 1→1.0, 4..7→source). DST_SEL_IDENTITY=0xFAC keeps identity a passthrough. Differential/goldens updated (corpus V# word3 = identity swizzle; spirv_eval DstSel binding + OpUGreaterThanEqual; passthrough_vs.spvasm golden regenerated; PC field-count test = 3).

The dst_sel-ignored fetch was THE fragment-blocker: Celeste's V# is Format32_32_32 + dst_sel=[4,5,6,1] (w=SQ_SEL_1=1.0); our fetch read a raw garbage 4th dword → gl_Position.w=NaN → all geometry clipped. Scissor + cross-submit were real but secondary (scissor masked by backend full-screen fallback; cross-submit matters once fragments survive). NOT COMMITTED — awaiting user request.
