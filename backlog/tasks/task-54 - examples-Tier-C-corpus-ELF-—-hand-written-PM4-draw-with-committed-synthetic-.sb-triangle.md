---
id: TASK-54
title: >-
  examples: Tier C corpus ELF — hand-written PM4 draw with committed synthetic
  .sb triangle
status: To Do
assignee: []
created_date: '2026-07-11 12:55'
updated_date: '2026-07-11 13:54'
labels:
  - gpu
  - examples
dependencies:
  - TASK-37
priority: medium
ordinal: 53000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Extend examples/ps4-pm4-test (rebuilt with installed data/oo_sdk) with a Tier C: embed P4-02 corpus VS/PS blobs as data, hand-write PM4 — default HW state, RT setup to videoout buffer, SET_SH_REG PGM_LO/HI binds (or sceGnmSetVsShader with hand-built regs), user-data V# for a small vertex buffer, DrawIndexAuto — then SubmitAndFlip. Entirely self-authored; freegnm/psbc NOT required (validation stretch). Buildable early (right after P4-02), usable headless for trace-driven dev before P4-18. Do NOT add Tier C to the 6-example oracle set.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 ELF builds via OO SDK Makefile pattern; committed source, gitignored binaries per existing example conventions
- [ ] #2 headless: under UNEMUPS4_PM4_TRACE=1 trace shows expected packet seq incl shader-bind register writes
- [ ] #3 headless-until-P4-18-then-live: before P4-18 the draw defers via the "needs GCN" path exercised end-to-end from a real guest; after P4-18 it renders
- [ ] #4 The Tier-C ELF is GENUINELY GNM-faithful — real .sb bytes + real addresses + a real vertex buffer (NO marker addresses like task-24's embedded path) — enabling the task-58 black-box framebuffer compare
<!-- AC:END -->
