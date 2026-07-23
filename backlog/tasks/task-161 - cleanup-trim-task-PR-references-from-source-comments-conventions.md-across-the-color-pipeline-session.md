---
id: TASK-161
title: >-
  cleanup: trim task/PR references from source comments (conventions.md) across
  the color-pipeline session
status: To Do
assignee: []
created_date: '2026-07-17 11:32'
labels:
  - cleanup
  - review
  - conventions
dependencies: []
priority: low
ordinal: 167000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
backlog/docs/conventions.md forbids referencing the current task/fix/PR in source comments ('added for X', 'handles case from #123' belong in the commit message). This session's GPU/color-pipeline commits saturated comments with 'task-153/154/155', 'TEST (white-dummy hypothesis)', 'the pre-task-153 bug', 'FNA/XNA SpriteBatch...' narration across crates/gnm/src/exec.rs, crates/gnm/src/cache/mod.rs (get_white_dummy + white_dummy field doc still labels itself a TEST/hypothesis in shipped public API), crates/core/src/tiling.rs, crates/gnm/src/derive.rs, crates/gpu/src/backend.rs. Trim to why-not-what, drop the task tags + hypothesis framing (keep the mechanism explanation where the why is non-obvious). Also rename/re-doc get_white_dummy so a shipped cache method isn't self-labeled a 'TEST hypothesis'. Non-blocking cleanup.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Source comments in the session's changed files no longer reference task numbers / 'TEST hypothesis'; get_white_dummy doc reflects settled behavior
<!-- AC:END -->
