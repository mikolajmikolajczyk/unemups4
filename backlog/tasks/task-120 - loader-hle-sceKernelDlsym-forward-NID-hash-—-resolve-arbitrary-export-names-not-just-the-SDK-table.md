---
id: TASK-120
title: >-
  loader/hle: sceKernelDlsym forward NID hash — resolve arbitrary export names,
  not just the SDK table
status: To Do
assignee: []
created_date: '2026-07-15 06:53'
labels:
  - hle
  - loader
  - retail
dependencies:
  - TASK-29
ordinal: 124000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
sceKernelDlsym (Process::module_dlsym) currently derives a name's NID only via ps4_syscalls::SyscallId::from_symbol_name(name), i.e. the build-time SDK name→NID table. A dlsym for any export name NOT in that table (an app-specific or C++-mangled interop symbol a retail .prx exports) returns -ENOENT even when the module genuinely exports it under NID(name). Real PS4 sceKernelDlsym computes the forward NID hash of ANY name (SHA-1(name || salt=518D64A635DED8C1E6B039B1C3E55230) -> Sony base64, first 8 bytes) and looks that up. Implement the forward hash (crates/loader/src/nid.rs already carries the Sony base64 alphabet + salt in its doc-comment; only encode_id exists there today, the forward hash was deliberately deferred to the ps4-syscalls build-time table) so module_dlsym resolves arbitrary names. Currently MASKED: the observed Celeste dlsym misses (_ZN5Audio11SoundSystem*) were genuinely optional probes the scePlayStation4.prx build does not export, so the runtime tolerated them — but the next dlsym for a real non-SDK export will silently fail.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 module_dlsym resolves an export by hashing an arbitrary name (not in the SDK table) to its NID and matching the module's NID-keyed export map
- [ ] #2 forward NID hash lives in crates/loader/src/nid.rs (SHA-1(name||salt) -> Sony base64), unit-tested against at least one known name->NID pair from the generated ps4-syscalls table
- [ ] #3 from_symbol_name stays as a fast path; the forward hash is the fallback for names the SDK table lacks
- [ ] #4 no crypto/keys — a SHA-1 name hash is not decryption (doc-3 K1); reuse an existing SHA-1 dep, do not vendor one gratuitously
<!-- AC:END -->
