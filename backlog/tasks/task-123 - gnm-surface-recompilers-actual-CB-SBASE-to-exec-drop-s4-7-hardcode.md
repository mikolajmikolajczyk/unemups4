---
id: TASK-123
title: 'gnm: surface recompiler''s actual CB SBASE to exec (drop s[4:7] hardcode)'
status: Done
assignee: []
created_date: '2026-07-16 06:16'
updated_date: '2026-07-16 10:32'
labels:
  - from-code-review
  - gnm
dependencies: []
ordinal: 129000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Code-review finding (exec.rs derive_const_buffer): the executor decodes the constant-buffer V# from a HARDCODED user-SGPR quad s[4:7] (CONST_BUFFER_SBASE_SGPR=4), but the recompiler resolves the s_buffer_load SBASE symbolically and never surfaces which SGPR pair it actually used. A retail VS whose CB SBASE != s[4:7] recompiles fine but the executor reads the wrong V#: if the garbage decodes non-null and hits mapped memory, the cache uploads the WRONG bytes and binds them (wrong transform → off-screen/NaN geometry), no defer. CB contents ARE bounded (BoundedMem) so no crash — this is silent wrong output. Fix: record the resolved SBASE (and count) in GcnResources and have derive_const_buffer read from it instead of the constant.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 recompiler records the CB SBASE SGPR index (and dword extent) it resolved, exposed on the resolution struct
- [ ] #2 derive_const_buffer reads the CB V# from that recorded SGPR, not a hardcoded s[4:7]
- [ ] #3 a VS with CB SBASE != s[4:7] binds the correct V# (add corpus coverage)
<!-- AC:END -->
