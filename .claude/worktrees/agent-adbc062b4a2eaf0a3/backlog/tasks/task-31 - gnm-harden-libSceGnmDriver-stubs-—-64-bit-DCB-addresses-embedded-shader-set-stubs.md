---
id: TASK-31
title: >-
  gnm: harden libSceGnmDriver stubs — 64-bit DCB addresses + embedded-shader set
  stubs
status: Done
assignee: []
created_date: '2026-07-10 21:42'
updated_date: '2026-07-11 05:24'
labels:
  - gnm
  - gpu
dependencies:
  - TASK-20
priority: high
ordinal: 31000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
FOLLOW-UP surfaced by task-22 (hand-written PM4 ELF). Two concrete gaps in task-20's libSceGnmDriver stubs that bite any realistic corpus / real game: (1) the submit stub reads the guest DCB/CCB command-buffer address arrays as 32-bit GPU addresses and truncates — OpenOrbis malloc returns pointers ABOVE 4 GB, so a malloc'd command buffer (what real games and normal homebrew use) decodes as all-zeros; task-22 had to force its DCB into a static <4GB global to work around this. Fix: read the full 64-bit address (PS4 Gnm command-buffer submit takes 64-bit VA arrays; confirm the ABI against shadPS4 gnmdriver sceGnmSubmit* + the OpenOrbis GnmDriver.h prototype) so identity-mapped guest pointers survive. (2) sceGnmSetEmbeddedVsShader / sceGnmSetEmbeddedPsShader are NOT registered by task-20, so calling them traps 'missing symbol' — Tier B of task-22 had to hand-emit their PM4 instead of calling them. Register these (and audit the rest of the embedded-shader / set-shader surface) as log-and-return-success stubs, so an embedded-shader guest boots; they are also a prerequisite for task-24 (phase-3.5 embedded-shader draw). NON-GOAL: PM4 execution / actual shader binding (task-24); no Vulkan.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 submit stub reads 64-bit DCB/CCB addresses; a malloc'd (>4GB) command buffer decodes correctly (test with a high-address buffer); task-22's static-buffer workaround becomes unnecessary
- [x] #2 sceGnmSetEmbeddedVsShader / sceGnmSetEmbeddedPsShader (+ audited neighbours) registered as stubs; a guest calling them does not trap missing-symbol
- [x] #3 six examples byte-identical; ps4-gnm stays Vulkan-free; clippy -D warnings + fmt + cargo test clean
<!-- AC:END -->

## Notes

2026-07-11 (worktree, not committed). Done: (1) submit handler now reads DCB/CCB
address arrays as u64 (new `read_u64_array`); size arrays stay u32. Added
`submit_handler_preserves_high_addresses` test with >4GB pointers (0x4_0021_4000 /
0x5_00AB_0000) proving no truncation. (2) Registered 9 set-shader stubs in
libscegnmdriver: sceGnmSetEmbeddedVsShader (NID +AFvOEXrKJk), SetEmbeddedPsShader
(X9Omw9dwv5M), plus SetVs/Ps/Cs/Es/Gs/Hs/LsShader; extended the NID-resolves test.
Serialized the two global-driver submit tests via a test-local mutex (they share
the process-global driver() OnceLock). Verify: build/test/clippy/fmt clean; oracle
`run_examples.sh check` diff is byte-identical with vs without these changes (only
the pre-existing task-20 Gnm-load line + headless-Vulkan line). ps4-gnm still has
no ash/winit. Next: task-24 (PM4 exec / shader binding). Blocker: none.
