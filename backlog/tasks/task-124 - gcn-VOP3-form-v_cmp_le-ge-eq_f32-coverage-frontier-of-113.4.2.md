---
id: TASK-124
title: 'gcn: VOP3-form v_cmp_le/ge/eq_f32 coverage (frontier of 113.4.2)'
status: Done
assignee: []
created_date: '2026-07-16 06:16'
updated_date: '2026-07-16 07:31'
labels:
  - from-code-review
  - gcn
dependencies: []
ordinal: 130000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Code-review finding (interp.rs:851, opcodes.rs vop3 module): the VOP3-encoded compare path wires only V_CMP_LT_F32/V_CMP_GT_F32; the vop3 opcode module defines only those two (LT=0x001, GT=0x004). eval_f32_compare and the standalone VOPC path support all five (LT/EQ/LE/GT/GE), but a VOP3-form v_cmp_le/ge/eq_f32 (used when the compare writes a non-VCC SGPR pair or carries modifiers) falls through to UnsupportedInst on both sides → the shader defers. Asymmetric coverage gap that blocks otherwise-supported retail shaders. Follow the 113.4.2 discipline: add vop3 consts EQ=0x002/LE=0x003/GE=0x006, extend the interp allowlist + compare branch, mirror in recompile emit, add a corpus shader + differential + decode goldens.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 opcodes::vop3 defines V_CMP_EQ_F32=0x002, V_CMP_LE_F32=0x003, V_CMP_GE_F32=0x006 (RE-verified vs llvm-mc)
- [x] #2 interp + recompile handle all five VOP3-form f32 compares; corpus shader exercises le/ge/eq
- [x] #3 differential + decode goldens committed for the new shader
<!-- AC:END -->
