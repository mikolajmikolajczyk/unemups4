---
id: TASK-68
title: >-
  gnm/cpu: boot-time sentinel watch mitigation for x86jit watch-count 0->nonzero
  race (x86jit-217)
status: Done
assignee: []
created_date: '2026-07-11 18:18'
updated_date: '2026-07-13 19:21'
labels:
  - gpu
  - gnm
  - cpu
dependencies: []
priority: medium
ordinal: 67000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Fable Runda-1 review SHOULD-FIX #2 — emulator-side mitigation for x86jit-217 (filed in x86jit backlog @ HEAD; UNLANDED). x86jit's JIT store-watch gate is a per-run snapshot: a watch_range() that transitions watch_count 0->nonzero while another vCPU is mid-run in JIT'd code loses that vCPU's stores to the newly-watched range until its next run boundary (crates/cpu/src/dirty.rs:32-42 forwards to the affected x86jit facility). Single-threaded corpus is safe; multi-threaded (Bloodborne) is not — task-49/55 could silently miss a texture/vertex re-upload (the worst debugging class: stale cache, no error anywhere). LOCAL MITIGATION (independent of the x86jit fix): install ONE permanent sentinel watch at boot BEFORE guest threads start, so watch_count is never 0 and the live per-page check is always armed. Cost = the per-page check on JIT stores even when the cache has nothing watched — MEASURE against the task-204 zero-cost goal before committing; if the cost is unacceptable, prefer waiting on x86jit-217 (kick running vCPUs on 0->nonzero) instead. SEQUENCING: either (a) land x86jit-217 + bump the rev pin (preferred, no local cost), or (b) ship the boot sentinel here if the pin bump is not yet available. Verify the chosen fix with a multi-threaded repro: thread A JIT-loops storing to R with no watches at its run start, thread B watches R mid-run, assert take_dirty_ranges sees A's stores.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 a multi-threaded repro (A JIT-stores to R with watch_count==0 at its run start; B watches R mid-run) reports A's stores via the DirtySource — currently FAILS, fix makes it pass
- [x] #2 chosen path documented: x86jit-217 pin-bump (preferred) OR boot sentinel watch with a measured cost note vs task-204
- [x] #3 single-threaded corpus + oracle 6/6 unaffected; no regression in task-48 dirty_source test
<!-- AC:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Resolve via preferred path (a): x86jit-217 (78f2834, JIT store-watch gate reads watch_count live) is already an ancestor of the pinned rev f32bb87 — no pin bump, no local sentinel mitigation (zero local cost, meets task-204). Add a multi-threaded repro at the VmDirtySource seam mirroring x86jit's own test. Verify dirty_source + 6 examples unaffected.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Done 2026-07-13. Path (a): x86jit-217 (78f2834) already ancestor of pinned f32bb87 — no bump, no local sentinel (zero cost, task-204). Added multi-threaded repro jit_store_seen_when_watched_mid_run_by_another_thread at VmDirtySource seam (hits>0 guards false-green); passes with the fix. dirty_source (task-48) test + full ps4-cpu suite + 6 example baselines all clean.
<!-- SECTION:NOTES:END -->
