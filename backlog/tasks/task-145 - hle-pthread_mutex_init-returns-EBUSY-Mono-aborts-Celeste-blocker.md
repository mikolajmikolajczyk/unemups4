---
id: TASK-145
title: 'hle: pthread_mutex_init returns EBUSY -> Mono aborts (Celeste blocker)'
status: Done
assignee: []
created_date: '2026-07-16 13:35'
updated_date: '2026-07-16 13:52'
labels:
  - hle
  - kernel
  - celeste
  - retail
  - bug
dependencies: []
priority: high
ordinal: 151000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by task-144 recon. Celeste's Mono runtime calls pthread_mutex_init and our HLE returns EBUSY (16, 'Device busy'): 'mono_os_mutex_init: pthread_mutex_init failed "Device busy" (16)' -> Mono calls abort() -> traps our int-0x44 abort stub (the 'vector 68 @0x982e26' fault). This aborts the process, a Celeste bring-up blocker distinct from the libfmod VMASKMOVPS x86jit gap (x86jit TASK-259). EBUSY from pthread_mutex_init classically means re-initializing an already-initialized/live mutex, or a bad/unsupported mutex attr. Investigate our scePthreadMutexInit / the libc pthread_mutex_init shim (crates/libs/src/libkernel/pthread.rs + any libc-side mutex path): why does it return EBUSY here? Likely causes to check: (a) we treat a re-init of a statically-initialized (PTHREAD_MUTEX_INITIALIZER-style) or already-seen mutex slot as EBUSY when we should re-init/succeed; (b) the HLE object-arena slot for the mutex is already occupied (task-115 arena) and we map that to EBUSY; (c) a mutex attr (type/robust/pshared) we don't handle maps to EBUSY instead of succeeding. Mono initializes many mutexes early; one path returns EBUSY. Fix so a valid pthread_mutex_init succeeds (returns 0) and Mono proceeds. Assets gitignored, never commit; RUST_LOG=warn,ps4_kernel=info to see the mutex path. NOTE: with the new guest_hexdump reporter (task-144) + the abort-stub log, the failing call site is self-describing.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 Root-caused: why our pthread_mutex_init/scePthreadMutexInit returns EBUSY for Mono's init call (re-init handling / arena slot / unhandled attr)
- [x] #2 Fix lands: a valid pthread_mutex_init returns 0 (success); Mono no longer aborts on mono_os_mutex_init
- [x] #3 Live: Celeste re-run past the abort — report the next wall (expected: the libfmod VMASKMOVPS x86jit gap TASK-259, and/or actual geometry once both clear); PNG dumped for the orchestrator
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Done 2026-07-16 (merge 8c2065a). Root cause = hypothesis (a): SyncManager::mutex_init (crates/kernel/src/sync.rs) returned Ok(16)/EBUSY on map.contains_key(addr) — but mutex_lock LAZILY inserts an entry for any never-init'd handle, so a lock-before-init or a legit re-init spuriously saw the entry -> EBUSY -> Mono mono_os_mutex_init treats it fatal -> abort() -> int-0x44 'vector 68' trap (task-144). Fix: always insert a fresh HostMutex on the addr (detect re-init via the returned Option), return Ok(0) — real pthread/libthr re-init-of-unlocked semantics; common case is an unheld lazy placeholder / destroyed-reused slot. +16/-7, no guest deref (lockdown untouched). LIVE: Mono abort GONE, Celeste runs 30s+ into gameplay ASSET STREAMING (3-CelestialResort.bin, madeline portrait atlases) + 35 GPU submit/present/draw. Next wall = libfmod VMASKMOVPS (x86jit TASK-259, USER lands) on audio thread; PNG still black (geometry behind FMOD). EBUSY origin was crates/kernel not crates/libs (task guessed libs); libs/pthread.rs forwarded correctly.
<!-- SECTION:NOTES:END -->
