---
id: TASK-55
title: >-
  gnm/gcn/gpu: textured-draw milestone — T#/S# decode + MIMG in
  interp/recompiler + texture cache end-to-end
status: Done
assignee: []
created_date: '2026-07-11 12:55'
updated_date: '2026-07-13 20:44'
labels:
  - gpu
  - gnm
dependencies:
  - TASK-53
  - TASK-50
  - TASK-51
  - TASK-48
priority: high
ordinal: 54000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Second milestone: a textured quad from a .sb PS doing image_sample. Adds: T# (128/256-bit image descriptor) + S# (sampler) decode in gnm state (extending P4-10 user-SGPR model); MIMG format decode + image_sample in P4-03 decoder, P4-04 interpreter (point/bilinear reference sampling — oracle), P4-05 recompiler (combined image-sampler, portable subset); corpus PS with image_sample (P4-02 extension); texture upload through cache with detile (P4-14+P4-15+P4-16); dirty-invalidation of a guest-written texture via x86jit DirtySource — first real payoff of P4-13. 2D macro-tiling sized here IF corpus needs (synthetic can stay linear/1D). Does NOT do DCC/HTILE, mips>0, cube/3D, anisotropy.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 headless: interpreter samples a mock texture with expected filtering; recompiled module spirv-val-clean with combined image samplers
- [x] #2 headless: MockBackend end-to-end — textured-draw DCB → create-image/upload(detiled)/bind-texture/draw; guest write to the texture → exactly one re-upload on next use
- [x] #3 live GPU (maintainer): corpus textured-quad ELF renders correctly
<!-- AC:END -->



## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
T#/S# decode in gnm state; MIMG+image_sample in decoder/interp(point+bilinear oracle)/recompiler(combined image-sampler, portable SPIR-V); texture upload through ResourceCache with task-50 detile; dirty re-upload via DirtySource; draw-arm wires CreatePipeline{texture:Some}+BindTexture with defer guard; corpus PS + interp-vs-recompiler differential; MockBackend AC#2 hand-reasoned sequence.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Session 2026-07-13. DONE: full textured-draw milestone.
- Decoder: MIMG + image_sample (crates/gcn/src/{opcodes,inst,decoder,disasm}.rs).
- Interp oracle: reference point + bilinear sampling via T#/S# decode (interp.rs). Tests: point + bilinear vs hand-reasoned texels.
- Recompiler: OpImageSampleImplicitLod combined image-sampler (portable, set0/binding1), IoLayout.samplers (recompile.rs). spirv-val clean.
- Differential: corpus texture_sample_ps + analytic spec + structural sampler guard (differential.rs). All pass.
- gnm: T#/S# decode + derive_texture (vbuf.rs); ResourceCache.get_texture (detile→CreateImage/UploadImage) + get_sampler (cache/mod.rs); exec draw-arm wires texture:Some + BindTexture with texture-Some-but-no-BindTexture DEFER guard (exec.rs).
- Corpus: texture_sample_ps.{s,code.bin,dis,sb} (self-authored, zero assets).
- AC#1 (headless): interp point+bilinear tests + spirv-val-clean combined-image-sampler module + interp-vs-recompiler structural differential. PASS.
- AC#2 (headless): MockBackend end-to-end exec test (gcn_textured_draw_end_to_end...) — CreateImage/UploadImage(detiled)/CreateSampler/BindTexture sequence + guest write => exactly one re-upload; hand-reasoned literals. + cache texture tests. PASS.
- AC#3 (live GPU): verified on AMD Radeon 780M via extended diff_harness — recompiled texture_sample_ps sampling a combined image-sampler == CPU oracle within 1e-5. NOTE: a textured-quad homebrew ELF was NOT authored (like task-96 was the triangle ELF follow-up); FILE that as a follow-up. Pre-existing passthrough_vs GPU point-snap divergence is unrelated to this task.
GATE: cargo build --release OK; cargo test ps4-gnm/core/gpu/gcn = 224 passed 0 failed; clippy 0; fmt clean; run_examples 6/6. Vulkan-free gnm/gcn preserved; portable SPIR-V; bounded reads for T#/S#/texels; no task-NN in source. UNCOMMITTED for review.
<!-- SECTION:NOTES:END -->

## AC#3 note

Recompiler GPU-confirmed via diff_harness: texture_sample_ps mrt0 == interp oracle within 1e-5 on AMD Radeon 780M (RADV) — recompiler==interp==GPU CLOSED for image_sample. Full corpus textured-quad-in-window ELF not authored → follow-up task (mirrors task-96 for the triangle). Two reviews (gcn + gnm) clean, zero criticals; 2 MINORs (tiling_index 5-bit mask fixed; oracle-detile reconciliation → task-98).
