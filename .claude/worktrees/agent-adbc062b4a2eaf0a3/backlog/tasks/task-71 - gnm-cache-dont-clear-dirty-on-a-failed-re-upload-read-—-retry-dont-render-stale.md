---
id: TASK-71
title: >-
  gnm/cache: don't clear dirty on a failed re-upload read — retry, don't render
  stale
status: Done
assignee: []
created_date: '2026-07-12 06:01'
updated_date: '2026-07-12 06:13'
labels:
  - gpu
  - gnm
dependencies: []
priority: high
ordinal: 70000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Code-review finding #2 (correctness). ResourceCache::get dirty-hit path (cache/mod.rs:239-245): calls reupload then unconditionally sets entry.dirty=false. But Self::upload (cache/mod.rs:298) is 'if let Ok(bytes)=mem.read_bytes {...}' — on Err the backend.upload never runs. So if the guest remaps/frees the backing range between submits and read_bytes fails, the backend buffer keeps STALE bytes, the entry is marked CLEAN, and it is never retried → the GPU silently draws wrong/old data with no error. FIX: only clear dirty when the upload actually happened (thread a bool/Result out of upload); on a read failure keep the entry dirty (so the next submit retries) and log once. Same guard on the first-use copy path (if the initial upload's read fails, don't record the entry as clean-and-ready — either skip the entry or mark it dirty so a later get retries).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 dirty-hit where mem.read_bytes fails leaves entry.dirty=true (retried next get), backend NOT told it's clean — unit test with a MockMem that fails reads
- [ ] #2 dirty-hit where read succeeds re-uploads and clears dirty (existing behavior preserved)
- [ ] #3 first-use where the initial read fails does not leave a clean entry backing a never-uploaded buffer
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
DONE 2026-07-12 (fix/task-71 @ adb5188, merged f0b084d). upload/reupload now return bool (true iff read_bytes ok AND backend.upload ran). Dirty-hit clears entry.dirty ONLY when re-upload happened; on read failure leaves dirty=true (retries next get) + logs once via tracing::debug!. First-use choice: dirty-insert (entry created+watched but dirty=true on failed initial read) — keeps get() contract simplest, retry rides existing dirty machinery. Import path/keying/drain_dirty/ranges_overlap untouched (that's task-72). New FailMem mock + 3 tests: dirty_hit_failed_read_stays_dirty_and_retries, dirty_hit_successful_read_reuploads_and_clears, first_use_failed_read_inserts_dirty_and_retries. Verify: gnm+core+gpu 94 pass, clippy 0, fmt clean, gnm Vulkan-free. Combined main gate: 28 suites, oracle 6/6.
<!-- SECTION:NOTES:END -->
