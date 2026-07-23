---
id: TASK-195
title: >-
  gnm/diag: snapshot deferred draws lose the exact unsupported instruction —
  surface offset+decoded-inst+shader in draws.json (x86jit-style coverage)
status: Done
assignee: []
created_date: '2026-07-21 11:12'
updated_date: '2026-07-21 17:44'
labels:
  - gnm
  - gpu
  - diag
  - recompiler
  - dx
dependencies: []
priority: high
ordinal: 200000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
When a draw defers with unsupported-gcn-shader the snapshot records only the constant category string (draws.json deferred_draws entries are all nulls + reason='unsupported-gcn-shader'), so finding WHICH GCN instruction the recompiler bailed on needs grepping /tmp/unemups4.log for 'unsupported instruction at dword offset N: Vop2 {...}'. The information already EXISTS end-to-end and is just discarded at two seams: (1) RecompileError::UnsupportedInst { inst: Box<Inst>, offset } (crates/gcn/src/recompile.rs:419) carries the full decoded instruction + dword offset, and defer_reason() (crates/gnm/src/shader/gcn.rs:581) already formats it human-readably; but resolve_shader_pair returns the bare enum ShaderPairResolution::NeedsGcn (crates/gnm/src/exec.rs ~501) which DROPS the RecompileError. (2) defer_draw(what, count, reason: &'static str) (exec.rs:1779) -> record_deferred -> DeferredRecord (crates/gnm/src/snapshot.rs:380) only accept/store a &'static str.\n\nGoal (parity with the x86jit model where iced-x86 decodes everything and the unlifted instruction is visible): the GCN decoder already decodes the whole shader; the lifter recompiles a subset; surface the exact gap in the snapshot instead of the log. \n\nPart 1 (must): thread the detail up. Make ShaderPairResolution::NeedsGcn carry the reason (the RecompileError or its defer_reason String + which stage VS/PS + the shader address/hash). Widen defer_draw / record_deferred / DeferredRecord to store: reason string (offset + decoded instruction, e.g. 'VOP2 op:5 V_SUBREV_F32 at dword 51'), the failing stage, and the shader id. Serialize into draws.json deferred_draws (fill the currently-null index/kind/count/reason/detail/shader fields) and one line in summary.txt.\n\nPart 2 (optional, higher leverage): a per-shader opcode COVERAGE list — since the decoder yields every Inst, record for a deferred shader the set of opcodes present and flag lifted vs unlifted, so after fixing the first missing op you can see how many siblings remain (avoids one-wall-at-a-time). Gate behind the snapshot arm like the rest.\n\nOracle: re-capture a Celeste in-game snapshot with an unsupported shader (before task-194 lands, or any title with a lift gap) and confirm draws.json names the exact instruction + offset + shader, no log grep needed.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 ShaderPairResolution::NeedsGcn carries the RecompileError/defer_reason + stage + shader id up to the deferral site (no longer a bare variant)
- [x] #2 draws.json deferred_draws entries name the exact failing instruction (decoded opcode + mnemonic + dword offset), the stage, and the shader address/hash — not a constant string with nulls; summary.txt shows it too
- [ ] #3 Part 2 (if done): a deferred shader's opcode coverage (present opcodes flagged lifted/unlifted) appears in the snapshot
- [x] #4 build + cargo test -p ps4-gnm + clippy clean; headless oracle baselines unaffected (deferral detail only populates when the snapshot is armed)
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Done 2026-07-21. Part 1 delivered (deferred draws now name the exact failing instruction + stage + shader id in draws.json/summary.txt, armed-gated). AC#3 (Part 2, opcode lifted/unlifted coverage) deliberately SKIPPED per the brief's skip-if-it-risks-Part-1 guidance: it needs knowledge living inside the recompiler and would double-decode on the armed path. Part 1 is what paid off — it named V_CMP_EQ_I32 for task-197 in one run.
<!-- SECTION:NOTES:END -->
