---
id: TASK-189
title: 'gcn: disassembler prints a spurious third source on two-source VOP3 forms'
status: To Do
assignee: []
created_date: '2026-07-20 18:39'
updated_date: '2026-07-20 18:40'
labels:
  - gcn
  - disasm
  - diagnostics
dependencies: []
priority: medium
ordinal: 193000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
The disassembler renders a third source operand on VOP3 instructions that only take two — for example "v_mac_f32 v7, s1, v0, s0" and "v_cvt_pkrtz_f16_f32 v0, v4, 1.0, s0". The trailing operand is whatever happens to sit in the src2 field, which for a two-source form is not an operand at all. Found while implementing task-188.

Same family as task-182 (VOP3 neg/abs were dropped from the printed text) and the same cost: disassembly is a primary reverse-engineering tool here, and text that misrepresents an instruction gets acted on. Twice in one day a wrong disassembly sent an investigation down a false path — once making a symmetric blur kernel read as one-sided, once forcing raw dwords to be hand-decoded because the printed form could not be trusted. This defect is milder, since a spurious extra operand is easier to notice than a missing sign, but it is the same class: the renderer does not know the operand count of what it is printing.

The fix presumably means the printer consulting the opcode true source count rather than always emitting three. Worth checking whether the same gap exists for one-source VOP3 forms.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A two-source VOP3 prints exactly two sources; a one-source form prints one
- [ ] #2 Tests cover a two-source form (e.g. v_mac_f32) and a one-source form, asserting the rendered text
- [ ] #3 The source count comes from the opcode rather than being special-cased per instruction, so the next opcode added cannot reintroduce this
- [ ] #4 build + cargo test + clippy clean
<!-- AC:END -->
