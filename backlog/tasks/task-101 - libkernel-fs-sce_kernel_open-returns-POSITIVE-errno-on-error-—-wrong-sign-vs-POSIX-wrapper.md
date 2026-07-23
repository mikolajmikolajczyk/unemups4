---
id: TASK-101
title: >-
  libkernel/fs: sce_kernel_open returns POSITIVE errno on error — wrong sign vs
  POSIX wrapper
status: Done
assignee: []
created_date: '2026-07-13 10:02'
updated_date: '2026-07-13 19:02'
labels:
  - real-software
  - doom
  - syscalls
  - filesystem
  - bug
dependencies: []
ordinal: 100000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
task-100 review (Finding 2) surfaced a pre-existing bug: sce_kernel_open returns 'Err(e) => e' (POSITIVE errno, e.g. +2 ENOENT) — contradicting its own comment 'Return a negative errno' and the OpenOrbis POSIX wrapper convention (ret<0 = error). A failed open therefore reads as a valid fd to the guest (e.g. +2 = stderr), corrupting stdio on the error path. The new fs-mutation syscalls (task-100) correctly return -e; open() was left as-is to keep task-100 scoped. Not hit by the 6 examples (they only open existing files) but Doom's config-read path can hit it. Fix open (and audit sibling handlers: read/write/lseek sign conventions) to negative-errno-on-error, add a unit test for the missing-file path.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 sce_kernel_open returns negative errno on failure; a guest open() of a missing file yields -1/negative, not a positive pseudo-fd
- [x] #2 Sibling fs handlers (read/write/close/lseek) audited for the same sign convention; unit test covers the error path
- [x] #3 6 example ELFs still match baselines
<!-- AC:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Fix positive-errno returns in fs syscall handlers: sce_kernel_open (null-path EFAULT + Err path) and sce_kernel_close both returned +errno → guest reads it as a valid fd. Negate to -errno. Audit siblings (read/write/writev/lseek/readv/mkdir/rmdir/unlink/rename already -e). Add fs backend error-path unit tests. Verify 6 example baselines unchanged.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Done 2026-07-13. Negated positive-errno returns in sce_kernel_open (null EFAULT + Err path) and sce_kernel_close; siblings already -e. Root-caused as the bug behind ps4doom's /tmp/doom.mid stdout spill. Added 4 fs backend error-path unit tests (all pass). 6 example baselines unchanged (run_examples.sh check: all match).
<!-- SECTION:NOTES:END -->
