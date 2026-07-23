---
id: TASK-196
title: >-
  gnm/gcn/diag: dump DEFERRED shaders' .sb + name the full instruction on
  InvalidOperand — finish x86jit-parity so a failed recompile is fully
  inspectable
status: Done
assignee: []
created_date: '2026-07-21 11:33'
updated_date: '2026-07-21 17:44'
labels:
  - gnm
  - gcn
  - gpu
  - diag
  - recompiler
  - dx
dependencies: []
priority: high
ordinal: 201000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
task-195 made deferred draws name their failing instruction, but a shader that FAILS to recompile is still not dumped (dump_shader runs only on success), and RecompileError::InvalidOperand names only the operand, not the containing instruction. Concretely: Celeste in-game frame 1858 has exactly ONE remaining deferral — PS 0x9afae5a00 (#0x7220397693965fd8) into rt 0x9afb58000 (the yellow background RT) — 'invalid operand at dword offset 28: Sgpr(0) (not a vector destination)'. We cannot see WHICH instruction that is because the .sb was never dumped and the error omits the opcode. This blocks fixing the last composite (likely the missing color/blue layer that leaves the sky yellow). Two enhancements: (1) dump the raw .sb (+ .txt in-tree disasm) for a shader that DEFERS on recompile failure, keyed the same as successful dumps (shaders/<stage>-<hash>.sb), gated on the armed snapshot — the failing shader is exactly the one you want to read. (2) enrich RecompileError::InvalidOperand to carry the full decoded Inst (mirror UnsupportedInst { inst, offset }) so the log / draws.json 'instruction' field names the opcode at the failing offset, not just the operand. This is the x86jit-parity completion: a failed recompile becomes fully inspectable (bytes + named instruction) from the snapshot, saving a re-dump-and-guess loop on every future shader wall. Oracle: after a re-dump, shaders/ contains ps-7220397693965fd8.sb and draws.json's deferred instruction string names the opcode at dword 28.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 a shader that defers on RecompileError dumps its raw .sb (+ .txt disasm) under shaders/<stage>-<hash>, same keying as successful dumps, only when the snapshot is armed
- [x] #2 RecompileError::InvalidOperand carries the decoded instruction; the log and draws.json deferred 'instruction' field name the full instruction (opcode + operands) at the failing dword offset
- [x] #3 build + cargo test -p ps4-gcn + -p ps4-gnm + clippy clean; unarmed/headless path unaffected
<!-- AC:END -->
