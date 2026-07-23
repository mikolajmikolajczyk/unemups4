---
id: TASK-38
title: 'gcn: GCN ISA decoder + disassembler (triangle subset)'
status: Done
assignee: []
created_date: '2026-07-11 12:53'
updated_date: '2026-07-12 09:57'
labels:
  - gpu
  - gcn
dependencies:
  - TASK-37
  - TASK-80
priority: medium
ordinal: 37000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
First real content of crates/gcn. Instruction-word decode for the corpus subset — SOP1/SOP2/SOPK/SOPC/SOPP, SMRD, VOP1/VOP2/VOP3/VOPC, VINTRP, MUBUF, EXP — into typed Inst enum (opcode, Operand::{Sgpr,Vgpr,Const,Literal,…}), plus a text disassembler for traces/golden tests (mirrors pm4 trace.rs). Spec: AMD SI/CI ISA manual; shadPS4/GPCS4 decoders cross-ref. Unknown encodings → Inst::Unknown(raw), logged, never fatal (PM4-decoder discipline). Does NOT execute; does NOT cover MIMG/MTBUF/DS/FLAT yet (later corpus). ps4-core-only dep.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 headless: decodes every instr in P4-02 corpus with golden disasm snapshots
- [ ] #2 headless: total decoder — arbitrary dword streams never panic; unknown ops = Unknown, walk continues
- [ ] #3 headless: 64-bit literals/VOP3 second-dword handled (off-by-one-dword trap)
- [ ] #4 cargo build -p ps4-gcn has no ash/winit
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
DONE 2026-07-12 (feat/task-38 @ 0955acb, merged). crates/gcn/src/: operand.rs (Operand::{Sgpr,Vgpr,Special(SpecialReg vcc/m0/exec/scc),InlineInt,InlineFloat,Literal,Raw}; single decode_src(field) for the 9-bit SI/CI source field — SGPR 0-103, special, inline int 0..64/-1..-16, inline float ±0.5..±4.0, literal=255, VGPR 256-511, total/never-panics), inst.rs (Inst per encoding class + Decoded{inst,size_dwords} for exact PC advance; ExportTarget), decoder.rs (dispatch by high-bit prefix: SOP1/SOP2/SOPK/SOPC/SOPP/SMRD/VOP1/VOP2/VOP3/VOPC/VINTRP/MUBUF/EXP; VOP2 tested LAST so it can't swallow VOP1/VOPC), disasm.rs (mirrors pm4/trace.rs — total, mnemonic()→class_0xNN fallback, Unknown→<unknown 0xN>, s[lo:hi]/v[lo:hi] spans, s_waitcnt vmcnt/expcnt/lgkmcnt), opcodes.rs (per-class name tables mirroring pm4::opcodes). AC#1: corpus_disasm_matches_golden — decodes each .code.bin, no Unknown, consumed==size, matches committed tests/corpus/*.dis (SPOT-CHECKED by orchestrator: passthrough_vs.dis == .s exactly). AC#2: never_panics_on_garbage (2000 xorshift + zeros/ones/0xDEADBEEF, no panic, consumed==input; env UNEMUPS4_GCN_TRACE). AC#3: multi_dword_advances_pc (VOP1+literal 2-dword, synthetic VOP3 2-dword; tail-truncation clamps size to remaining, no over-read). AC#4: gcn Vulkan-free (deps ps4-core + tracing workspace). 16 tests. UNSURE/spot-check-later (all decode the ENCODING CLASS correctly, only specific opcode NUMBERS unexercised — corpus has none): VOP3 opcode consts (v_mad_f32=0x141 etc.), VOP2 specific ops, v_madmk/madak inline-literal 2-dword advance. Added tracing dep (workspace, Vulkan-free, matches pm4 env-gated-trace idiom; flag if prefer eprintln). Combined gate: 30 suites, oracle 6/6. FOUNDATION for task-39 (interp) + task-40 (recompiler) — they consume Inst/Operand.
<!-- SECTION:NOTES:END -->
