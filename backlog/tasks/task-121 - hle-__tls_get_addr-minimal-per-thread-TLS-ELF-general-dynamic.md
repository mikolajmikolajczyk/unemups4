---
id: TASK-121
title: 'hle: __tls_get_addr + minimal per-thread TLS (ELF general-dynamic)'
status: Done
assignee: []
created_date: '2026-07-15 12:25'
updated_date: '2026-07-15 13:18'
labels:
  - retail
  - fase-3
  - hle
  - tls
dependencies: []
ordinal: 126000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Celeste's managed AOT code calls __tls_get_addr [NID vNe1w4diLCs], the ELF general-dynamic TLS resolver, on the main thread -> missing-symbol FATAL. The emulator currently has NO TLS infrastructure: the loader ignores PT_TLS and no fs_base is ever set (the guest reached the managed runtime using pthread-key TLS only). The eboot's own PT_TLS is EMPTY (FileSiz=0 MemSiz=0). Approach: minimal zero-init HLE first (smallest fix that breaks the wall), extend only if a module with initialized __thread data (tdata) surfaces.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 __tls_get_addr(tls_index* ti) is HLE'd: ti = { u64 ti_module; u64 ti_offset } read from the guest ptr; returns a valid guest address = per-thread TLS block base + ti_offset
- [x] #2 Each guest thread gets its own lazily-allocated TLS arena (allocated from the guest heap via the memory manager), so two threads see distinct TLS addresses for the same (module,offset); the same thread sees a STABLE address across calls
- [x] #3 The returned block is zero-initialized (correct for bss-only/empty TLS templates such as the eboot's); a KNOWN LIMITATION note documents that modules with initialized tdata are not yet template-copied
- [x] #4 Celeste's main thread advances PAST the __tls_get_addr FATAL on a smoke run (new wall surfaces further along, or it reaches directory enumeration and exercises sceKernelGetdents)
- [x] #5 Homebrew + Doom smoke: no regression (they don't use general-dynamic TLS, so behavior is unchanged)
<!-- AC:END -->
