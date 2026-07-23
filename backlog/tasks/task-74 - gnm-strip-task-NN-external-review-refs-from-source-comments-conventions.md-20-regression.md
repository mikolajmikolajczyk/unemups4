---
id: TASK-74
title: >-
  gnm: strip task-NN + external-review refs from source comments
  (conventions.md:20 regression)
status: Done
assignee: []
created_date: '2026-07-12 06:01'
updated_date: '2026-07-12 06:23'
labels:
  - gpu
  - gnm
  - chore
dependencies: []
priority: low
ordinal: 73000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Code-review finding #9 (conventions). backlog/docs/conventions.md line 20: 'Don't reference the current task / fix / PR ("added for X", "handles case from #123") — that belongs in the commit message, not the source file.' task-64 (Done) swept these clean; Runda-2 re-introduced them. Occurrences to strip (keep the surrounding explanatory prose, keep doc-N/decision-N/section cites which ARE allowed): task-NN in crates/gnm/src/state.rs (lines 28,75,92,142,363), crates/gnm/src/pm4/emit.rs:1, crates/gnm/src/exec.rs (100,141,483), crates/gnm/src/cache/mod.rs:30, crates/core/src/gpu.rs:72; 'Fable #3'/'Fable phase-4 review #3' external-review tags in crates/gnm/src/cache/mod.rs (lines 10,183,186,248) and crates/core/src/gpu.rs. Reword to state the invariant/rationale directly without the ticket reference. RUN THIS LAST (after task-69/71 and any other Runda-3 fixes merge) — it touches comments in files those tasks also change, so doing it last avoids merge conflicts. Verify: git grep -nE 'task-[0-9]|Fable #' crates/ returns only allowed doc/decision cites (or empty).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 git grep -nE 'task-[0-9]+' over crates/*/src returns no task-NN references in source comments (doc-N/decision-N cites may remain)
- [ ] #2 no 'Fable #'/'Fable phase-4 review' external-review tags remain in source
- [ ] #3 the explanatory rationale is preserved (reworded, not deleted); build/clippy/fmt green
<!-- AC:END -->
