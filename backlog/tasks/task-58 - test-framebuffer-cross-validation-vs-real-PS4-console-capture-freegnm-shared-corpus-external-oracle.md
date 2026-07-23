---
id: TASK-58
title: >-
  test: framebuffer cross-validation vs real PS4 console capture + freegnm
  shared corpus (external oracle)
status: To Do
assignee: []
created_date: '2026-07-11 13:54'
updated_date: '2026-07-23 10:19'
labels:
  - gpu
dependencies:
  - TASK-96
priority: medium
ordinal: 57000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
End-to-end external ground truth: validate our whole GCN pipeline (parse->decode->recompile->draw, task-53) against the REAL PS4 console, not only our own interp oracle (which risks 'works only for us'). The console capture (celeste-scrape-oracle: the real DCB/CCB Celeste submits per flip, streamed off hardware by the GoldHEN scraper, task-168) is the ground-truth command stream; dcbdump (tools/ps4-gnm-scrape/host) decodes it through our own clean PM4 decoder for a readable compare. Two corpora: (a) our Tier-C synthetic ELF (task-54) with real OrbShdr .sb bytes at real addresses + a real vertex buffer (real-GNM-conformant, not emulator-specific leniency); (b) freegnm's triangle (MIT/UNLICENSE) vendored as a shared known-good homebrew corpus. Start with a MANUAL 'renders correctly' gate (maintainer's eyes) for the keystone milestones (task-53 triangle, task-55 textured), then automate command-stream / framebuffer compare against the capture with a tolerance epsilon (resolution/timing need it). NON-GOAL: pixel-exact match (software vs hardware backends).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 freegnm triangle vendored (MIT/UNLICENSE, attributed) + builds/runs; used as the shared known-good corpus for the compare
- [ ] #2 The Tier-C synthetic ELF (task-54) is real-GNM-conformant (real .sb/addresses/vertex buffer, not emulator-specific leniency) — proven by rendering and by matching the console capture's command stream
- [ ] #3 A documented compare procedure (manual eyeball gate first) confirms task-53's triangle renders correctly (maintainer's eyes) and its command stream matches the console capture; maintainer-run
- [ ] #4 (stretch) automated framebuffer capture + tolerance-diff harness over the shared corpus
<!-- AC:END -->

## Notes

Cross-check target (maintainer 2026-07-12): the task-96 corpus GCN .sb triangle ELF (examples/ps4-gcn-triangle) is the shared corpus for this task — the same .elf renders the triangle in unemups4, and its command stream is compared against the real PS4 console capture (celeste-scrape-oracle) decoded with dcbdump. task-96 is authored as genuine real-GNM homebrew (real-GNM PM4 + real .sb format, not emulator-specific leniency); its report lists any real-GNM divergence it had to choose — those are the first things to verify here. Depends on task-96 landing.

## Real-GNM conformance progress (2026-07-13)

The ps4-gcn-textured-quad homebrew (task-99) was made genuinely real-GNM-conformant — loader/API/flip blockers cleared over several iterations: (1) bundled sce_module/{libc,libSceFios2}.prx; (2) videoout init per the OpenOrbis graphics sample — sceVideoOutOpen bus MAIN=0 (was 1, the assertion), a real OrbisVideoOutBufferAttribute; (3) a prepareFlip PM4 packet appended to the DCB tail (IT_NOP header 0xC03E1000 + tag 0x68750777) that a GNM host scans for; (4) a flip loop that re-arms the prepareFlip tag each frame (the host patches it to 0x68750776 in-place on consume). These are real-GNM submit/flip conventions, not emulator-specific. Validation now compares the homebrew's command stream against the real PS4 console capture (celeste-scrape-oracle, decoded with dcbdump) — the console is the stronger ground truth, and the recompiler is already GPU-validated via diff_harness (mrt0 == oracle). Open thread: framebuffer tilingMode encoding (RegisterBuffers logs tilingMode=1 — verify LINEAR) and CB_COLOR0 base vs the registered videoout buffer aliasing; confirm against the capture. unemups4 unregressed.
