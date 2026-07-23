---
id: TASK-231
title: >-
  gcn/decoder: VOP3b reads scalar dest SDST from wrong bit field ([7:0] vs
  CI-ISA [14:8])
status: To Do
assignee: []
created_date: '2026-07-23 09:09'
labels:
  - bug
dependencies: []
ordinal: 236000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
VOP3b-encoded ops (VOPC integer compares promoted into VOP3) write a scalar destination. decode_vop3 reads SDST from bits [7:0], but CI-ISA (Sea Islands ISA) §13.3 VOP3b places SDST at [14:8]; [7:0] is VDST. Bit-position bug surfaced by the GPU provenance-citation audit (the reworded comment already states the true [14:8] layout and points here).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 decode_vop3 reads VOP3b SDST from bits [14:8] per CI-ISA §13.3
- [ ] #2 a real llvm-mc VOP3b (gfx700) encoding decodes its scalar dest to the correct SGPR
- [ ] #3 regression test pins the fix to the CI-ISA field / an llvm-mc encoding
<!-- AC:END -->
