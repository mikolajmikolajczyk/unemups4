---
id: TASK-118
title: 'refactor: typed SceKernelStat struct + SyncManager owns guest sync objects'
status: To Do
assignee: []
created_date: '2026-07-14 20:18'
updated_date: '2026-07-14 20:33'
labels:
  - refactor
  - tech-debt
dependencies:
  - TASK-113.3
priority: low
ordinal: 122000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Code-review altitude/reuse findings (task-113.3). (1) sceKernelStat fills hardcoded magic byte offsets (0x08 st_mode, 0x48 st_size, ...) + a literal 120-byte zero; unverified, and fields beyond what we fill (st_mtim/st_ino for Mono's assembly cache) are wrong/zero with no compile-time signal. Define #[repr(C)] SceKernelStat (FreeBSD-derived) with size/offset static asserts, shared by stat/fstat/lstat. (2) libkernel/sema.rs built a parallel process-global semaphore registry instead of extending kernel/sync.rs SyncManager (which owns mutex/cond/rwlock). Similarly scePthreadMutexInit pokes an hle_alloc'd guest object into the slot while SyncManager keys the host lock by slot addr (split-brain). Fix: SyncManager owns the guest-visible sync-object lifecycle (mutex/cond/rwlock/sema) — one create path, guest object + host primitive at one layer/key. read_cstr is also duplicated inline in pthread.rs vs the fs.rs helper.
<!-- SECTION:DESCRIPTION:END -->
