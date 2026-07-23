---
id: TASK-22
title: 'examples: hand-written PM4 test ELF'
status: Done
assignee: []
created_date: '2026-07-10 18:24'
updated_date: '2026-07-10 21:42'
labels:
  - gnm
  - gpu
  - examples
dependencies:
  - TASK-20
priority: medium
ordinal: 22000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Phase 2 / doc-2 D1 corpus (see decision-3). OpenOrbis ships no working native Gnm 3D sample and no shader compiler / .sb blobs (doc-3), so the Gnm test vehicle is a HAND-WRITTEN PM4 example ELF built with OpenOrbis `GnmDriver.h` (per-call PM4 packet builders) + the `libSceGnmDriver.so` stub (202 symbols), matching §7-Q2 (hand-written PM4 ELFs are the phase-2 corpus; captured buffers + RenderDoc later).

Build it as TWO TIERS in one example (or two adjacent examples), both submitting via raw `sceGnmSubmit*` calls (the NIDs stubbed in task-20):

- **Tier A — trace/present, NO shader**: `sceGnmDrawInitDefaultHardwareState350` → clear color → `sceGnmSubmitAndFlipCommandBuffers`. A pure PM4 command stream with no draw, no shader — exercises task-21's decoder and the phase-3 present/clear subset. This is the minimum viable corpus and does NOT depend on any shader work.
- **Tier B — real draw with EMBEDDED shaders, still NO .sb blob**: add `sceGnmSetEmbeddedVsShader(0)` + `sceGnmSetEmbeddedPsShader(1)` (firmware-embedded fullscreen-quad VS + R/G-export PS per doc-3 — need no shader binary) + `sceGnmDrawIndexAuto`. This is the first corpus that draws real geometry, and crucially needs NO GCN interpreter: the emulator recognizes the embedded shader IDs and substitutes hardcoded host VS/PS (that's the separate phase-3.5 task). Arbitrary `.sb` shaders (freegnm triangle) are a later, GCN-dependent corpus — out of scope here.

Build with the OpenOrbis toolchain exactly like the other examples: -O2 CFLAGS and --no-rosegment LDFLAGS (the loader needs .rodata in the R+E text segment — task-16's loader note; committed .elf must be a 3-LOAD-segment layout). Follow the existing examples/ Makefile pattern (e.g. examples/ps4-softgpu/Makefile). Toolchain lives at ~/src/ps4labs/ps4sdk (not vendored).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 New examples/<name> builds with the OpenOrbis toolchain like the other examples (-O2 CFLAGS, --no-rosegment LDFLAGS), committing a loadable .elf (3-LOAD-segment layout)
- [x] #2 Tier A: initDefaultHardwareState350 → clear → SubmitAndFlip, a shader-free PM4 stream, submitted via raw sceGnmSubmit* (task-20 stubs)
- [x] #3 Tier A running under the emulator produces a PM4 trace (task-21) matching the crafted packet stream
- [x] #4 Tier B: adds SetEmbeddedVsShader(0) + SetEmbeddedPsShader(1) + DrawIndexAuto — a real draw using firmware-embedded shaders, NO .sb blob (consumed by the phase-3.5 embedded-shader draw path); traces correctly
<!-- AC:END -->

## Implementation Notes

Session 2026-07-10. Done. New example: `examples/ps4-pm4-test/` (Makefile + `.gitignore` + `ps4-pm4-test/main.c` + committed `ps4-pm4-test.elf`), one ELF, two tiers submitted as two flips. No emulator (crates/*) changes; NOT added to `run_examples.sh` (leaves the six-example oracle untouched).

**Toolchain.** `OO_PS4_TOOLCHAIN` = the *extracted* OpenOrbis toolchain from `~/src/ps4labs/ps4sdk/toolchain-llvm-18.tar.gz` (path `OpenOrbis/PS4Toolchain`). Note: the `~/src/ps4labs/ps4sdk` git checkout has an empty `lib/` (no crt1.o / no `.so` stubs) — the built toolchain (crt1.o, `libSceGnmDriver.so`, `link.x`, identical `include/`) ships only inside that tarball; extract it and point `OO_PS4_TOOLCHAIN` at `.../OpenOrbis/PS4Toolchain`. System `clang`/`ld.lld` (LLVM). Build:
- `clang --target=x86_64-pc-freebsd12-elf -O2 -fPIC -funwind-tables -c -isysroot $OO -isystem $OO/include -o main.o main.c`
- `ld.lld main.o -o ps4-pm4-test.elf -m elf_x86_64 -pie --script $OO/link.x --eh-frame-hdr --no-rosegment -L$OO/lib -lc -lkernel -lSceVideoOut -lSceGnmDriver -lc++ $OO/lib/crt1.o`

**readelf -l:** 3 LOAD segments (`R E` text carries `.rodata` via `--no-rosegment`, then two `RW`) — matches siblings. ELF64 DYN/PIE x86-64.

**PM4 trace (`UNEMUPS4_PM4_TRACE=1`), matched to the crafted stream:**
- Tier A (dcb 36 B, 3 pkts): `IT_CLEAR_STATE`, `IT_CONTEXT_CONTROL`, `IT_SET_CONTEXT_REG reg=0xa2b0` (CB_COLOR0_CLEAR clear color) — via `sceGnmSubmitAndFlipCommandBuffers` + `sceGnmSubmitDone`.
- Tier B (dcb 76 B, 6 pkts): + `IT_SET_SH_REG reg=0x2c48` (embedded VS pgm), `IT_SET_SH_REG reg=0x2c08` (embedded PS pgm), `IT_SET_CONTEXT_REG reg=0xa1c5` (PS R/G col format), `IT_DRAW_INDEX_AUTO`. `[GNM] sceGnmDrawIndexAuto count=3` also logged. Guest exits cleanly.

**Two emulator-forced deviations (documented in main.c):**
1. DCB lives in a **static/global** buffer, not `malloc`. OpenOrbis malloc returns a >4 GB host pointer; task-20's submit stub reads the guest dcb-address array as **32-bit** GPU addresses (`u32* dcb_gpu_addrs[]`), truncating a >4 GB pointer. A global lands <4 GB (~0x41xxxx) so its low 32 bits == the full identity-mapped address and the decoder reads the real buffer. (Alternative fix would be task-20 reading 64-bit addrs — out of scope; not touched.)
2. The OpenOrbis `GnmDriver.h` per-call builders are HLE stubs here (task-20) that write NO PM4, so the DCB is **hand-emitted** with the exact IT_* packets those builders emit on real HW. `sceGnmDrawInitDefaultHardwareState350` and `sceGnmDrawIndexAuto` ARE stubbed, so they are still called (NID + `[GNM]` log exercised). `sceGnmSetEmbeddedVsShader`/`PsShader` are **NOT stubbed by task-20** — calling them traps "missing symbol". Per the "don't modify the emulator" constraint they are not called; their embedded-shader PM4 (VS/PS SH-register writes) is hand-emitted instead, so Tier B is a genuine embedded-shader draw at the PM4 level with no `.sb` blob. Follow-up worth filing: task-20 does not stub SetEmbeddedVs/PsShader (nor 64-bit submit addrs).

**Worktree note:** this worktree was cut from `fd6f5d8`, which predates task-20 (present on `main`). Rebased onto `main` (`c72943e`) so the libSceGnmDriver stubs + decoder are present; example + task-22 edits carried across cleanly. `cargo build --release` green; `ps4-gnm`/`ps4-libs` tests pass.

Next: none (ready for review + merge). Blocker: none.
