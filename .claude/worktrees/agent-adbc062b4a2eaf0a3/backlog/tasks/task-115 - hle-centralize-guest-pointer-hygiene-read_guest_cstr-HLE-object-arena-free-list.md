---
id: TASK-115
title: >-
  hle: centralize guest-pointer hygiene (read_guest_cstr) + HLE object-arena
  free-list
status: To Do
assignee: []
created_date: '2026-07-14 20:18'
updated_date: '2026-07-14 20:33'
labels:
  - hle
  - bug
  - tech-debt
dependencies:
  - TASK-113.3
priority: medium
ordinal: 119000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Code-review altitude finding (task-113.3). is_guest_ptr is applied ad-hoc in a few handlers while ~11 others (sceKernelOpen path, condattr, thread names, mutex_ptr slot deref, etc.) still deref guest pointers unguarded, and sceKernelStat uses a raw '< 0x10000' literal (inconsistent). Under JIT identity-map a junk guest ptr = host SIGSEGV, not a guest fault, so this is whack-a-mole. Fix: a single read_guest_cstr(ptr)->Option<String> (range-check via GUEST_BASE/DEFAULT_SPAN + bounded NUL scan) used by ALL handlers; route the sceKernelStat check through is_guest_ptr; guard scePthreadMutexInit's *(mutex_ptr) slot deref. ALSO: the HLE object bump-arena (ps4_core::kernel hle_alloc, 1 MiB, ~16k objs, NO free) leaks and, when exhausted, returns 0 -> scePthreadMutexInit leaves the slot null -> guest null-deref under heavy mutex/cond churn. Add a free path (mutex_destroy -> hle_free) or a slab/free-list keyed to object lifecycle.
<!-- SECTION:DESCRIPTION:END -->
