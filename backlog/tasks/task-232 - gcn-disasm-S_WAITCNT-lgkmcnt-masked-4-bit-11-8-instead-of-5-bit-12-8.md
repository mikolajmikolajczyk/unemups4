---
id: TASK-232
title: 'gcn/disasm: S_WAITCNT lgkmcnt masked 4-bit [11:8] instead of 5-bit [12:8]'
status: To Do
assignee: []
created_date: '2026-07-23 09:09'
labels:
  - bug
dependencies: []
ordinal: 237000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
waitcnt_operand masks lgkmcnt as (simm16>>8)&0xF = bits [11:8] (4-bit), dropping bit 12. CI-ISA SOPP 0xC S_WAITCNT defines lgkmcnt as simm16[12:8] (5-bit) on CIK/Liverpool, so lgkmcnt in 16..31 renders wrong. Golden-text-only impact (disasm rendering). Surfaced by the GPU provenance-citation audit.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 lgkmcnt uses the 5-bit [12:8] field (max 0x1F)
- [ ] #2 lgkmcnt(16) and lgkmcnt(31) render correctly
- [ ] #3 test derived from the CI-ISA SOPP field definition
<!-- AC:END -->
