---
id: TASK-58
title: >-
  test: shadPS4 black-box framebuffer cross-validation + freegnm shared corpus
  (Tier-1 external oracle)
status: To Do
assignee: []
created_date: '2026-07-11 13:54'
updated_date: '2026-07-12 18:47'
labels:
  - gpu
dependencies:
  - TASK-96
priority: medium
ordinal: 57000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
End-to-end external ground truth: run the SAME guest ELF on shadPS4 AND unemups4 and compare the rendered framebuffer, so our whole GCN pipeline (parse→decode→recompile→draw, task-53) is validated against a mature emulator, not only our own interp oracle (which risks 'works only for us'). Two corpora: (a) our Tier-C synthetic ELF (task-54), which MUST be genuinely shadPS4-runnable — real OrbShdr .sb bytes at real addresses + a real vertex buffer, NOT the marker addresses task-24 used for embedded shaders; (b) freegnm's triangle (MIT/UNLICENSE) vendored as the shared cross-emulator corpus — a known-good homebrew proven on shadPS4. shadPS4 built locally, gitignored. Start with a MANUAL 'renders the same on both' gate for the keystone milestones (task-53 triangle, task-55 textured), then automate framebuffer capture + tolerance-based compare (resolution/timing differences need an epsilon). NON-GOAL: pixel-exact match (different backends); shadPS4 recompiler internals (deferred Tier 2).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 freegnm triangle vendored (MIT/UNLICENSE, attributed) + builds/runs; used as the shared corpus for the cross-emulator compare
- [ ] #2 The Tier-C synthetic ELF (task-54) runs on shadPS4 (real .sb/addresses/vertex buffer) — proven by rendering there, not just on unemups4
- [ ] #3 A documented compare procedure (manual eyeball gate first) confirms task-53's triangle renders equivalently on shadPS4 and unemups4; maintainer-run (both need a GPU)
- [ ] #4 (stretch) automated framebuffer capture + tolerance-diff harness over the shared corpus
<!-- AC:END -->

## Notes

Cross-check target (maintainer 2026-07-12): the task-96 corpus GCN .sb triangle ELF (examples/ps4-gcn-triangle) is intended as the shared cross-emulator corpus for this task — the SAME .elf must render the triangle in both unemups4 and shadPS4, and their framebuffers compared. task-96 is being authored as genuine shadPS4-runnable homebrew (real-GNM PM4 + real .sb format, not emulator-specific leniency); its report will list any emulator-vs-real-GNM divergence it had to choose — those are the first things to verify here. Depends on task-96 landing.

## shadPS4 cross-check progress (2026-07-13)

The ps4-gcn-textured-quad homebrew (task-99) is now GENUINELY shadPS4-runnable — all loader/API/flip blockers cleared over several iterations (verified live in shadPS4 v0.16.1): (1) bundled sce_module/{libc,libSceFios2}.prx; (2) videoout init matched to OpenOrbis graphics.cpp — sceVideoOutOpen bus MAIN=0 (was 1, the assertion), real OrbisVideoOutBufferAttribute; (3) prepareFlip PM4 packet appended to the DCB tail (header 0xC03E1000 IT_NOP + tag PrepareFlip 0x68750777) that shadPS4 PatchFlipRequest scans for; (4) flip loop that re-arms the prepareFlip tag each frame (shadPS4 patches it to PatchedFlip 0x68750776 in-place on consume). shadPS4 now runs the homebrew's render loop continuously, no assertions. BUT the window is still BLACK — the pixel render is a shadPS4-INTERNAL matter, NOT the recompiler (which is GPU-validated via diff_harness mrt0==oracle). Suspects for the black: framebuffer tilingMode (RegisterBuffers logs tilingMode=1 — verify LINEAR encoding), CB_COLOR0 base vs the registered videoout buffer (0x200000000) aliasing in shadPS4, or shadPS4's own GCN recompiler on our .sb. NEXT: enable shadPS4 shouldDumpShaders / readbackLinearImages to tell present-issue vs recompiler; or accept unemups4+diff_harness as the validation and treat shadPS4 framebuffer-compare as blocked on shadPS4-internal render. All conformance fixes are real-GNM-accurate + committed + pushed; unemups4 unregressed.
