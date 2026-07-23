---
id: TASK-194
title: >-
  gcn: recompiler drops Celeste layer-composite shaders — missing native VOP2
  float ALU (V_SUBREV/ADD/SUB/MUL_F32) + 2 VOP1 ops, scene renders near-black
status: Done
assignee: []
created_date: '2026-07-21 11:04'
updated_date: '2026-07-21 17:44'
labels:
  - gcn
  - gpu
  - celeste
  - recompiler
  - retail
dependencies: []
priority: high
ordinal: 199000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
In-game Celeste renders near-black: only faint parallax tree outlines + snow show, the lit gameplay layers are missing (maintainer: game runs underneath, layers composite in wrong order/not at all). Root cause is NOT missing registers — the GPU snapshots (gpu-snapshots/frame-030xx) show 6 per-frame DrawIndexOffset count=6 full-screen quads (the layer-composite/blit passes) DEFERRED with reason unsupported-gcn-shader; they never reach the backend so those layers never composite. The recompiler bails on three GCN opcodes (from /tmp/unemups4.log, 18390 hits total): (1) 11608x Vop2 op:5 = V_SUBREV_F32 (0x05); (2) 2902x Vop1 op:34 = V_CEIL_F32 (0x22); (3) 2902x Vop1 op:14 (0x0E, src0=InlineInt(1) — confirm mnemonic vs llvm-mc/Mesa, likely a cvt). crates/gcn/src/recompile.rs emit_vop2 (line ~1943) only handles LSHLREV/LSHRREV/AND/ADD_I32/CNDMASK/CVT_PKRTZ and relies on VOP3-reencoded forms for float ALU (V_MUL_F32 0x108 etc.) — but Celeste emits the float ops in their NATIVE VOP2 encoding (0x03 ADD_F32, 0x04 SUB_F32, 0x05 SUBREV_F32, 0x08 MUL_F32, 0x1F MAC_F32), which fall through to the unsupported-instruction error. Fix: add the native VOP2 float-ALU family + the two VOP1 ops to the emitters (mirror the existing arm style; V_SUBREV_F32 = src1-src0; MAC_F32 = fma into vdst). Opcode consts live in crates/gcn/src/opcodes.rs (vop2/vop1 modules). Oracle: the 6 unsupported-gcn-shader deferrals per frame -> 0 (draws.json), the composite draws render, and the scene brightens to the correct layered image (maintainer live PNG oracle). Same class as task-184 (an unmodelled shader feature swallows a whole visible layer).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 emit_vop2 handles the native VOP2 float ALU family: V_ADD_F32(0x03), V_SUB_F32(0x04), V_SUBREV_F32(0x05), V_MUL_F32(0x08), V_MAC_F32(0x1F)
- [x] #2 emit_vop1 handles V_CEIL_F32(0x22) and VOP1 op 0x0E (mnemonic confirmed against llvm-mc/Mesa)
- [x] #3 the per-frame unsupported-gcn-shader deferrals drop to 0 (verified in a fresh gpu-snapshot draws.json) and Celeste's in-game scene composites correctly — maintainer live oracle
- [x] #4 build + cargo test -p ps4-gcn + clippy clean; no regression in existing shader tests
<!-- AC:END -->
