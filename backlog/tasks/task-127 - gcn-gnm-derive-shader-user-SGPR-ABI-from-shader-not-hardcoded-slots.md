---
id: TASK-127
title: 'gcn/gnm: derive shader user-SGPR ABI from shader, not hardcoded slots'
status: Done
assignee: []
created_date: '2026-07-16 06:25'
updated_date: '2026-07-16 10:32'
labels:
  - from-audit
  - gnm
  - gcn
dependencies:
  - TASK-123
ordinal: 133000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Hardcode audit (game#2 risk, Tier-1 SILENT WRONG OUTPUT): the executor reads descriptor pointers from HARDCODED user-SGPR slots that only match how Celeste/MonoGame's gnmx happens to compile — CB V# at s[4:7] (exec.rs:926, CONST_BUFFER_SBASE_SGPR), descriptor-set ptr at s[2:3] (exec.rs:937, DESC_SET_USER_SGPR), texture T#/S# set ptr at s[0:1] (vbuf.rs:586, CORPUS_TEXTURE_SLOT). Also the provider picks bindings via samplers.first()/const_buffers.first() (exec.rs:434,458). A different game's shader places these in different SGPRs/bindings → executor reads the wrong register → garbage descriptor → off-screen/NaN geometry or wrong/missing texture, with NO crash and NO defer (bounded read masks it). Fix: the recompiler already resolves these symbolically — surface the actual SGPR slot (and binding index) each descriptor was resolved from, and have the executor read from that instead of a constant. Supersedes/absorbs task-123 (CB slice); this covers the whole SGPR-slot ABI family.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 recompiler records, per resolved descriptor, the user-SGPR slot + binding index it came from
- [ ] #2 executor reads CB / desc-set / texture descriptors from the recorded slots, no hardcoded s[4:7]/s[2:3]/s[0:1]
- [ ] #3 a shader with a different SGPR layout binds correct descriptors (corpus coverage)
<!-- AC:END -->
