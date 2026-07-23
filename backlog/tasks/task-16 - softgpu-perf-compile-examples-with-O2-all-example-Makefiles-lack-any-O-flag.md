---
id: TASK-16
title: >-
  softgpu perf: compile examples with -O2 (all example Makefiles lack any -O
  flag)
status: Done
assignee: []
created_date: '2026-07-10 09:28'
updated_date: '2026-07-10 13:45'
labels:
  - perf
  - examples
dependencies: []
priority: high
ordinal: 16000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
ps4-softgpu runs ~34 fps; its 1920x1080 fill loop (examples/ps4-softgpu/ps4-softgpu/main.cpp:42-44, 2M u32 stores = 8.3MB per frame) is built at -O0 (Makefile:31 CFLAGS has no -O flag), so the guest executes tens of millions of unoptimized instructions per frame through the JIT. -O2 lets clang vectorize the fill to SSE stores — likely the single biggest fps win for this test. ALL six example Makefiles lack -O (ps4-fs, ps4-helloworld, ps4-mmap, ps4-softgpu, ps4-thread-testing, ps4-tls). Caveat: .elf files are committed — rebuilding changes guest binaries, so oracle baselines (scripts/run_examples.sh) may need refresh; verify none of the tests depend on -O0 codegen quirks.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 -O2 added to CFLAGS in all six examples/*/Makefile
- [x] #2 Rebuilt .elf files committed; scripts/run_examples.sh oracle still green (baselines refreshed if output legitimately changed)
- [x] #3 ps4-softgpu fps measured before/after on the same machine and recorded in task notes
<!-- AC:END -->



## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. Add -O2 to CFLAGS in all 6 examples/*/Makefile (single identical CFLAGS line each).
2. Rebuild each ELF with the OpenOrbis toolchain (extracted from /home/mikolaj/src/ps4labs/ps4sdk/toolchain-llvm-18.tar.gz to scratch; system clang/ld.lld in devshell) via OO_PS4_TOOLCHAIN, replicating the Makefile compile+link (compile main.cpp -O2, link with crt1.o + link.x + libs). Copy resulting <proj>.elf to committed path (note: helloworld commits as hello_world.elf).
3. Verify ELFs valid; run oracle run_examples.sh check -- guest stdout must match baselines modulo known headless Vulkan env line. If any semantic divergence, STOP and record for maintainer (do not overwrite baselines).
4. FPS before/after: cannot measure headless (no Vulkan driver); hand live ps4-softgpu FPS run to maintainer, record mechanism in notes.
5. Commit rebuilt binaries (watch pre-commit large-file hook).
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Session 2026-07-10.

TOOLCHAIN: OO_PS4_TOOLCHAIN was UNSET, but a full OpenOrbis PS4Toolchain ships (unextracted) in /home/mikolaj/src/ps4labs/ps4sdk/toolchain-llvm-18.tar.gz. Extracted to scratch; builds with the devShell's system clang++ 22.1.6 / ld.lld (exactly as 'make' would in this devShell). So rebuild WAS possible.

DONE:
- AC#1: -O2 added to CFLAGS in all six examples/*/Makefile.
- All six ELFs rebuilt at -O2 and committed (compile main.cpp -O2, link with crt1.o + link.x + per-example LIBS; helloworld commits as hello_world.elf).
- softgpu -O2 win confirmed at the binary level: the 2M-iteration drawTarget[i]=bgColor fill loop (main.cpp:42-44) is now SSE-vectorized (XMM store count 6 -> 20 vs the -O0 original).

LOADER FIX (necessary companion): modern ld.lld defaults to a SEPARATE read-only PT_LOAD (page-aligned 0x1000) for .rodata. The emulator loader (crates/loader/src/linker.rs:83-91) uses raw p_vaddr with only 0x1000 size-rounding, so that extra segment maps into an overlapping range -> 'Memory collision', and the rebuilt ELF refuses to load. Added --no-rosegment to LDFLAGS in all six Makefiles (with an explanatory comment) to keep .rodata in the R+E text segment, restoring the originally-committed 3-LOAD-segment layout. The originally-committed ELFs were linked without a separate rosegment (older LLVM default).

AC#2 ORACLE: scripts/run_examples.sh check — across all six examples the ONLY divergence is the known headless env line 'Failed to initialize Vulkan: Unable to find a Vulkan driver' (environmental, present before my changes too). Every guest-visible stdout line is byte-identical. ONE legitimate, -O2-caused change: ps4-tls 'Applied 29 relocations' -> 'Applied 28 relocations' (loader INFO log, NOT guest stdout; guest tests all still PASS). Refreshed ONLY that single line in scripts/baselines/ps4-tls.txt (did NOT run capture mode, which would poison baselines with this env's Vulkan-failure line). No other baseline touched.

AC#3 FPS (before/after) — NOT DONE HERE, HANDED TO MAINTAINER: headless devShell has no Vulkan driver, so ps4-softgpu FPS cannot be measured. Maintainer to run 'cargo run --release -p unemups4 -- examples/ps4-softgpu/ps4-softgpu.elf' under UNEMUPS4_BACKEND=jit on a machine with a Vulkan driver, before/after this commit, and record FPS. Expected: the vectorized fill loop cuts per-frame guest instruction count sharply (biggest single fps lever per task-16 rationale). Baseline before was ~34 fps.

Status left In Progress for maintainer to set Done after merge + live FPS measurement.

2026-07-10 (maintainer live verification): ps4-softgpu under UNEMUPS4_BACKEND=jit on a real Vulkan driver, combined with task-17: **34 fps → 60 fps (vsync cap)**, smooth, no artifacts. AC#3 ticked; merged to main; Done.
<!-- SECTION:NOTES:END -->
