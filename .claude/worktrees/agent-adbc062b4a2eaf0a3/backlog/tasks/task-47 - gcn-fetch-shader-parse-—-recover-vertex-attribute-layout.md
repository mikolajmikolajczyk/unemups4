---
id: TASK-47
title: 'gcn: fetch-shader parse — recover vertex-attribute layout'
status: Done
assignee: []
created_date: '2026-07-11 12:55'
updated_date: '2026-07-13 00:46'
labels:
  - gpu
  - gcn
dependencies:
  - TASK-38
  - TASK-42
  - TASK-45
priority: medium
ordinal: 46000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Parse the fetch-shader convention (small GCN subroutine pointed at by user SGPRs that buffer_load_formats each attribute into agreed VGPRs before s_setpc back): walk with P4-03 decoder to recover attribute→V#-slot→VGPR mapping, merged with .sb VertexInputSemantic. Synthetic corpus deliberately avoids fetch shaders, so this lands after the triangle milestone but before freegnm/psbc or retail. Spec: shadPS4 fetch-shader parsing, GPCS4. Does NOT execute the fetch shader on GPU (replaced by Vulkan vertex input / recompiled loads).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 headless: hand-assembled fetch shader (added to P4-02 corpus) parses to correct attribute table
- [x] #2 headless: VS resolve through GcnShaderProvider consumes the table, produces vertex-input state without executing the fetch blob
- [x] #3 headless: non-conforming fetch code defers cleanly
<!-- AC:END -->



## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Add fetch_shader.rs in ps4-gcn: walk Decoded stream (s_load_dwordx4 V#→SGPR, buffer_load_format_* idxen from V#-SGPR→VGPR, stop at s_setpc/s_swappc/s_endpgm) building FetchAttribute{semantic-slot, vsharp_sgpr, dest_vgpr, format(components)}; non-conforming→None. Add hand-assembled fetch_vs corpus (.s/.code.bin/.sb) + independent expected-table test. Wire GcnShaderProvider AC#2: merge parsed table with .sb VertexInputSemantic into vertex-input state without executing blob. Keep gcn/gnm Vulkan-free; bounded reads only.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Done (uncommitted, awaiting orchestrator review).

Parser: `crates/gcn/src/fetch_shader.rs` — `parse_fetch_shader(&[Decoded]) -> Option<FetchShaderLayout>`. Walks the P4-03-decoded stream: `s_load_dwordx4` records SGPR-quad ← (desc-ptr SGPR, dword-offset*4 bytes); each `buffer_load_format_{x,xy,xyz,xyzw}` (idxen, srsrc naming a loaded V#) yields one `FetchAttribute{vsharp_sgpr, dest_vgpr, components, desc_ptr_sgpr, desc_offset_bytes}`; stops at `s_setpc_b64`/`s_swappc_b64`. Non-conforming (unmodeled inst, non-idxen, store, fetch from unloaded SGPR, no return, no attrs) → `None`. Added `S_SETPC_B64=0x20`/`S_SWAPPC_B64=0x21` to opcodes (llvm-mc-verified).

AC#2 merge: `crates/gnm/src/shader/fetch.rs` — `resolve_fetch_vertex_input(addr,len,bounded_reader,&Semantics)` reads the fetch blob through the bounded seam (untrusted addr), decodes, parses, and merges the parsed table with `.sb` VertexInputSemantic by dest-VGPR → `VertexInputState{ResolvedAttribute{semantic, vsharp_sgpr, dest_vgpr, components, ...}}`. Wired onto `GcnShaderProvider::resolve_fetch_vertex_input` (bounded seam, ShaderUnsupported defer). Does NOT execute the blob.

Corpus: hand-assembled `crates/gcn/tests/corpus/fetch_vs.{s,code.bin}` (real GFX7 bytes, llvm-mc bonaire). NOT in the s_endpgm-terminated CORPUS array; excluded from the data-driven differential harness via a `fetch_` name skip (it's a callee, not a runnable VS). Expected attribute table + semantics hand-reasoned in-test, independent of the parser.

Gates (worktree /home/mikolaj/src/unemups4-task47): release build ok; ps4-gcn 49 pass/2 ign; ps4-gnm+ps4-core 178 pass; clippy --all-targets --all-features -D warnings exit 0 / 0 lints; fmt --check clean; run_examples.sh check 6/6.

Note for reviewer: `GcnShaderProvider::resolve_fetch_vertex_input` is a new public entry point, not yet called from the draw path (the display-side vertex-input wiring is task-52/53). This task recovers the table only, per scope.
<!-- SECTION:NOTES:END -->
