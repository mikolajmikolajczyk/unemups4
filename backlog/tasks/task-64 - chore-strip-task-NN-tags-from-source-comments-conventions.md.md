---
id: TASK-64
title: 'chore: strip task-NN tags from source comments (conventions.md)'
status: Done
assignee: []
created_date: '2026-07-11 14:47'
updated_date: '2026-07-11 15:54'
labels:
  - chore
dependencies: []
priority: low
ordinal: 63000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
conventions.md forbids referencing the current task/fix/PR in source ('that belongs in the commit message, not the source file'). Recent GPU/loader code carries task-NN tags in doc-comments (e.g. crates/gnm/src/exec.rs:7 'Phase 3 (task-34)', state.rs:5 'Phase 3.5 (task-24)', many more). Strip the task-NN references; KEEP the doc-N/decision-N citations (they encode non-obvious invariants, defensible). Optionally trim what-not-why comment density where it just narrates the code. Do this now so phase-4 code doesn't compound the drift. Low-risk comment-only sweep.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 no 'task-NN' references remain in source comments (git grep clean); doc-N/decision-N citations retained
- [x] #2 comment-only change; zero behavior/test/oracle change
<!-- AC:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. Enumerate all task-NN refs via git grep across crates/**/*.rs + app/**/*.rs. 2. Classify each: comment (edit) vs string-literal/identifier (leave). 3. Strip task-NN from comments, minimal rephrase; keep doc-N/decision-N. 4. Verify: git grep clean of comment refs, doc/decision citations intact, build/test/clippy/fmt/run_examples 6/6.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Comment-only sweep complete. Removed 140 task-NN references from doc/line comments across 36 source files (crates/**/*.rs + app/**/*.rs), rephrasing each to read naturally. Kept all doc-N/decision-N citations (128 remain). DELIBERATELY LEFT one task-NN ref: crates/memory/src/vm_backend.rs:243 'guest? (task-11 class)",' — it is INSIDE a format! string literal (the UnmappedMemory fault-diagnostic the emulator prints), so editing it would change program output and break the run_examples oracle baselines. No identifiers contained 'task'. Verify: git grep task-NN over sources returns ONLY that string literal; diff is pure comment (no added non-comment line); cargo build 0 errors; cargo test 116 passed/3 ignored/0 failed; clippy -D warnings 0 errors; cargo fmt --check clean; run_examples.sh check = 6/6 baselines match. Left for maintainer to commit (no commit/push per instructions).
<!-- SECTION:NOTES:END -->
