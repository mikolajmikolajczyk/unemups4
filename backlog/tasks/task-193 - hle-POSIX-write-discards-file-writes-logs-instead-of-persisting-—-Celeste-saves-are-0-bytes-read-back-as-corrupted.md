---
id: TASK-193
title: >-
  hle: POSIX write() discards file writes (logs instead of persisting) — Celeste
  saves are 0 bytes, read back as corrupted
status: Done
assignee: []
created_date: '2026-07-21 10:49'
updated_date: '2026-07-21 10:55'
labels:
  - hle
  - kernel
  - celeste
  - fs
  - savedata
dependencies: []
priority: high
ordinal: 198000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
sys_write (crates/libs/src/libkernel/mod.rs, id SYS_WRITE, names write/_write) sends fd 1/2 to stdout but for every other fd only logs the payload as [FILE] write and returns len WITHOUT writing the file. Celeste's save-and-quit writes 0.celeste and settings.celeste via POSIX write(fd,...); the 14493-byte save XML goes to the log and the on-disk file stays 0 bytes, so on reload the game sees an empty file and reports the save Corrupted. The file backend already persists correctly: fs::write / k.file_write (crates/kernel/src/fs.rs:453) writes straight to the host File (handles fd 1/2 stdout too, no BufWriter). Fix: route sys_write non-stdio fds through k.file_write like sce_kernel_write does; keep a fallback log for fds the backend does not know (Err EBADF) so stray debug writes still surface. Note sceKernelWrite (fs.rs) is correct and unaffected — the bug is specifically the POSIX write alias. Oracle: after save-and-quit, /home/mikolaj/PS4/CUSA11302/savedata/SAVEDATA00/0.celeste is non-zero and the game reloads the save instead of Corrupted (maintainer live).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 POSIX write() to an open file fd persists to the host file (non-zero on disk), not just a log line
- [x] #2 Celeste save-and-quit then reload restores the save instead of showing Corrupted (maintainer live oracle)
- [x] #3 stray writes to unknown/non-file fds still fall back gracefully; build + cargo test + clippy clean
<!-- AC:END -->
