---
id: TASK-57
title: 'test: GCN decoder cross-validation vs llvm-mc disassembly (external oracle)'
status: To Do
assignee: []
created_date: '2026-07-11 13:54'
updated_date: '2026-07-23 10:17'
labels:
  - test
dependencies:
  - TASK-38
priority: medium
ordinal: 56000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Our differential oracle (interpreter vs recompiler, task-39/41) proves INTERNAL consistency, not EXTERNAL correctness — a shared misread of a GCN encoding passes both, and our self-authored golden 'known answers' could themselves be wrong. Add an external ground-truth check against llvm-mc (LLVM's AMDGPU assembler/disassembler, gfx700 = Sea Islands = Liverpool): assemble each mnemonic and compare the encoding bytes (and/or disassemble bytes) against our task-38 decoder over the synthetic corpus (task-37). llvm-mc is an independent implementation of the AMD GCN ISA, so agreement proves our decode tracks the hardware. LARGELY SATISFIED by the provenance-audit witness tests gcn_opcodes_match_amd_oracle (all 120 op numbers vs llvm-mc gfx700 encoding fields) and decoder_fields_match_amd_oracle (one real llvm-mc encoding per format); this task extends that to the full corpus disassembly. NON-GOAL: a full reference SPIR-V pipeline.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Our decoder's disassembly of every corpus shader matches llvm-mc's gfx700 decode per-instruction (committed golden fixture, or a harness invoking llvm-mc over the corpus blobs)
- [ ] #2 A divergence (opcode/operand mismatch) fails the test with a clear per-instruction diff
- [ ] #3 llvm-mc is invoked as an external tool; the check skips cleanly when llvm-mc is absent, never breaks CI
<!-- AC:END -->
