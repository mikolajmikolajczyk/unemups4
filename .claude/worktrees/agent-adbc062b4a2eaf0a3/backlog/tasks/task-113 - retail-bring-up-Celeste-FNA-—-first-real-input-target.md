---
id: TASK-113
title: 'retail bring-up (epic): managed-runtime title on native host — umbrella'
status: To Do
assignee: []
created_date: '2026-07-13 21:49'
updated_date: '2026-07-14 08:28'
labels:
  - retail
  - loader
  - hle
  - epic
dependencies: []
priority: high
ordinal: 112000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Umbrella for bringing up the first RETAIL title (title-agnostic). Target class: a SELF-wrapped native host + N SELF-wrapped native .prx modules + managed assemblies (PE/.NET) executed by an AOT-compiled managed runtime, audio via a native middleware module, rendering via a platform-interop module -> GNM. Sequenced FASE 0..4 (children): 0 SELF unwrap (113.1) -> 1 multi-module load+link (task-29) -> 2 native runtime bring-up (113.3) -> 3 framework/platform interop, first GNM+audio (113.4) -> 4 correctness/playability (113.5). Diagnostics (113.2) lands early, parallel with FASE 0. Hard boundaries across all: ZERO crypto ever (console-side decryption only; unwrapping unencrypted SELF is container parsing, not decryption); game assets stay LOCAL + gitignored, NEVER committed (tests use synthetic fixtures); GPU/SPIR-V stays MoltenVK/Metal-portable; x86jit gaps go to the x86jit backlog (user lands, bump pin), never edit x86jit directly. Milestone-gated + pull-driven: FASE 2+ granular gaps are filed as the smoke loop surfaces them, not pre-filed.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Dumped Celeste eboot magic checked; if SELF-wrapped, a no-crypto unwrapper task is filed+landed and goblin ingests the ELF
- [ ] #2 Engine confirmed from the ELF import table (expected FNA: SDL2/FAudio)
- [ ] #3 Celeste ELF runs under unemups4 to its first concrete blocker, captured with RUST_LOG
- [ ] #4 Per-gap follow-up tasks filed from the first run's blockers
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Concrete first instance (2026-07-14): dump at /home/mikolaj/PS4/CUSA11302/ (LOCAL, gitignored, never commit). Engine identified from eboot strings (unencrypted SELF -> plaintext): MonoGame + Mono FULL-AOT (--full-aot 'Avoid JITting any code', MONO_AOT_FILE_FLAG_LLVM_ONLY, mono_eh_frame; build path Z:\work\gh\mono-ps4-alt) + FMOD (libfmod/libfmodstudio.prx) + SGen/Boehm GC. Managed side = PE/.NET (.exe/.dll magic 4D5A). All native (eboot + 6 .prx: scePlayStation4, libfmod, libfmodstudio, libc, libSceFios2) are SELF magic 4F153D1D, unencrypted. Correction: earlier 'FNA' call was WRONG — it is Mono full-AOT. GOOD: full-AOT => no guest-side JIT (x86jit runs native AOT code; no W^X/SMC mono-JIT to fight). eboot = 40MB.
<!-- SECTION:NOTES:END -->
