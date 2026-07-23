---
id: TASK-197
title: >-
  gcn: recompiler rejects VOP3-encoded VOPC integer compares writing an
  SGPR-pair (V_CMP_EQ_I32) — last Celeste composite defers, sky stays yellow
status: Done
assignee: []
created_date: '2026-07-21 11:50'
updated_date: '2026-07-21 17:44'
labels:
  - gcn
  - gpu
  - celeste
  - recompiler
  - retail
dependencies: []
priority: high
ordinal: 202000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
The one remaining Celeste in-game deferral (now fully visible thanks to task-196): PS 0x9afae5a00 #0x7220397693965fd8 into rt 0x9afb58000 (the yellow background RT). Exact instruction at dword 28: Vop3 { op: 130, vdst: Sgpr(0), src0: InlineInt(1), src1: Vgpr(6), src2: Sgpr(0) } -> 'invalid operand: Sgpr(0) not a vector destination'. op 130 = 0x82 = V_CMP_EQ_I32 (GCN VOPC integer compare, VOP3B-encoded, writing the 64-bit lane mask to the SGPR pair s[0:1]). The .sb disasm (gpu-snapshots/shaders/ps-7220397693965fd8.sb / .txt) shows the pattern: three compares 'vop3_0x82 s[0:1],1,v6' / 's[2:3],2,v6' / 's[4:5],3,v6' feeding v_cndmask_b32 selects — a switch/case colour selector; its absence leaves the sky RT yellow.\n\nRoot: emit_vopc (crates/gcn/src/recompile.rs ~2371) ALREADY stores a compare result to VCC OR an SGPR pair (PredKey::SgprPair(n), pred_dst resolver), but a VOP3-encoded VOPC arrives as Inst::Vop3{op:130} and is dispatched to emit_vop3 (~2095), which has no compare arm and falls through to the generic 'not a vector destination' rejection. Fix: in emit_vop3, detect ops in the VOPC compare range and route them into the existing predicate/compare machinery, using the VOP3 vdst as the SGPR-pair predicate destination (VOP3B). Implement the integer compare family i32 (V_CMP_{LT,EQ,LE,GT,NE,GE}_I32 = 0x81..0x86, plus F/T if trivial) and the u32 family (0xC1..0xC6) since a sibling will wall next; name the opcodes in opcodes.rs (vopc mod currently only names the F32 compares 0x01..0x06 — add I32/U32; the VOP3 form uses op+0x80/0xC0 style numbering, CONFIRM exact values via llvm-mc / AMD GCN ISA). Reuse emit_vopc's bool-per-lane compute + pred store so a later v_cndmask reading s0/s2/s4 lowers to OpSelect exactly as the VCC path does. PROVENANCE: derive opcodes ONLY from AMD GCN ISA / Mesa / llvm-mc.\n\nOracle: the last unsupported-gcn-shader deferral drops to 0 in a fresh gpu-snapshot draws.json, and Celeste's background composites to the correct (deep blue/navy night sky, white moon) instead of yellow — maintainer live PNG oracle. Same class as task-194 (a missing shader op swallows a visible layer).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 emit_vop3 routes VOP3-encoded VOPC integer compares (V_CMP_*_I32 0x81..0x86, +U32 0xC1..0xC6) into the compare/predicate path, storing the mask to the VOP3B SGPR-pair destination; opcodes named in opcodes.rs, values confirmed via llvm-mc/AMD ISA
- [x] #2 PS 0x9afae5a00 recompiles (no InvalidOperand); a fresh gpu-snapshot shows 0 unsupported-gcn-shader deferrals in-game
- [x] #3 Celeste background composites correctly (deep-blue night sky, white not yellow moon) — maintainer live oracle
- [x] #4 build + cargo test -p ps4-gcn + clippy clean; new unit test recompiles the v_cmp_eq_i32->cndmask select pattern without error
<!-- AC:END -->
