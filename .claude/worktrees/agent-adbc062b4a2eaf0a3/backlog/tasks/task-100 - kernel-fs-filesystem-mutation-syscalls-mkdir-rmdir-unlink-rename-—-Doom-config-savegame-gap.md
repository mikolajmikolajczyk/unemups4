---
id: TASK-100
title: >-
  kernel/fs: filesystem-mutation syscalls (mkdir/rmdir/unlink/rename) — Doom
  config/savegame gap
status: Done
assignee: []
created_date: '2026-07-13 09:36'
updated_date: '2026-07-13 10:06'
labels:
  - real-software
  - doom
  - syscalls
  - filesystem
dependencies: []
ordinal: 99000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
First real-software gap surfaced by the doomgeneric PS4 port (~/src/ps4doom). Doom boots through Z_Init + zone alloc on the x86jit JIT backend, then dies: it calls mkdir (M_MakeDirectory for config/saves) which the linker only TRAP-STUBS (ID 0xc0000001, aborts) — siblings rename/unlink/rmdir are trap-stubbed too. Implement these 4 as real fs syscalls so Doom proceeds to WAD load + first render.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 mkdir/rmdir/unlink/rename implemented end-to-end: Kernel trait (core/kernel.rs) + FileSystem methods (kernel/fs.rs, path-translated + '..'-rejecting) + bridge impl + #[ps4_syscall] handlers (libs/libkernel/fs.rs) with names registered so the linker resolves them instead of trap-stubbing
- [x] #2 Config/savegame writes land in a writable host sandbox (Doom's config dir is CWD-relative '.', which no current mount matches — decide: map guest CWD to a writable host dir, or handle mkdir('.')→EEXIST; Doom's M_MakeDirectory tolerates EEXIST)
- [x] #3 ps4doom.elf runs further than before in unemups4 (past 'Using . for configuration and saves' → into WAD load / render); report the NEXT gap
- [x] #4 Existing 6 example ELFs still match baselines (scripts/run_examples.sh); build 0, clippy -D warnings 0, fmt clean
<!-- AC:END -->

## Implementation Notes

Landed in merge e3ee133 (feat 1d3a6d8). Four-layer fs plumbing: `Kernel` trait (core/kernel.rs) → `FileSystem::{mkdir,rmdir,unlink,rename}` + `resolve()` + `io_errno()` (kernel/fs.rs) → bridge delegates → `#[ps4_syscall]` handlers with bare libc names registered (libs/libkernel/fs.rs), so the linker resolves them instead of trap-stubbing. `mkdir(".")` → EEXIST (Doom tolerates); CWD-relative paths anchor under /app0; `open()` switched translate→resolve. Review fixes: EFAULT guard returns -14 (not +14); io_errno remaps ELOOP/ENAMETOOLONG + defaults EIO; `mkdir("")` → ENOENT. Follow-ups filed: task-101 (open() positive-errno sign bug, pre-existing), task-102 (canonicalize sandbox containment vs symlink escape).

**RESULT:** Doom (Freedoom) boots to its main loop and RENDERS the title screen through the full pipeline (x86jit JIT → guest framebuffer → videoout → present), PNG-oracle verified. Colors show an R↔B/hue shift (separate ps4doom-shim swizzle issue). 6/6 example baselines still match.
