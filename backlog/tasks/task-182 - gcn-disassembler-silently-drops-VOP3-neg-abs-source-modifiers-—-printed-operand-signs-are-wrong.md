---
id: TASK-182
title: >-
  gcn: disassembler silently drops VOP3 neg/abs source modifiers — printed
  operand signs are wrong
status: Done
assignee: []
created_date: '2026-07-20 12:30'
updated_date: '2026-07-23 18:44'
labels:
  - gcn
  - disasm
  - diagnostics
dependencies: []
priority: medium
ordinal: 186000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
crates/gcn/src/disasm.rs never prints the VOP3 neg/abs modifiers (zero mentions in the file), although the decoder reads them (decoder.rs, w1 bits [31:29] / w0 bits [10:8]) and the recompiler applies them (apply_mods). Disassembly therefore shows wrong operand signs. Concretely during task-179 this made a symmetric 5-tap Gaussian read as one-sided (all taps at +2/+4, the negated ones invisible) and a scale-down term read as a scale-up, costing a false hypothesis and a stretch of wasted analysis. Disassembly is a primary reverse-engineering tool here, so silently lying about signs is expensive.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 VOP3 neg/abs modifiers are printed (e.g. -v1, |v2|) so the disassembly matches the encoding
- [x] #2 A test disassembles an instruction with neg and abs set and asserts the rendered text
- [ ] #3 1
- [ ] #4 2
<!-- AC:END -->
