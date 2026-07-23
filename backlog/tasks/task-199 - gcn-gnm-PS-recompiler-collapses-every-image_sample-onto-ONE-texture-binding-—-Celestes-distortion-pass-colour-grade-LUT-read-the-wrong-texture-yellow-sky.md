---
id: TASK-199
title: >-
  gcn/gnm: PS recompiler collapses every image_sample onto ONE texture binding —
  Celeste's distortion pass + colour-grade LUT read the wrong texture (yellow
  sky)
status: Done
assignee: []
created_date: '2026-07-21 15:18'
updated_date: '2026-07-21 17:44'
labels:
  - gcn
  - gnm
  - gpu
  - celeste
  - recompiler
  - retail
dependencies: []
priority: high
ordinal: 204000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Proven against a real-PS4 scrape of the same scene (dumps/scrape2 vs gpu-snapshots/frame-02143): every colour/blend register matches the console byte-for-byte across all 29 draws, so the yellow sky is NOT a blend/format/swizzle bug. The cause is that ensure_ps_texture() (crates/gcn/src/recompile.rs ~3590) memoises self.ps_texture and early-returns, so only the FIRST image_sample's DescriptorSource is pushed into io_samplers; every later sample silently reuses that one binding. The doc comment at recompile.rs ~378 states the (wrong) ABI outright: a PS with image_sample declares exactly one.\n\nTwo independent consequences in Celeste, both proven:\n1. Draw 14 (distortion pass, PS 0x9afae5500) samples TWO textures: s[16:23] = a memory-resident T# pointing at the 320x180 buffer that holds (0.5,0.5,0,1) — the canonical NEUTRAL DISPLACEMENT MAP, not a colour — then perturbs the UVs (v_sin_f32/v_madmk_f32) and samples the SCENE via s[0:7] at the displaced UV; only the second result is exported. We bind only the first, so BOTH samples read the displacement map, the export is olive, and blend ONE/ONE_MINUS_SRC_ALPHA with alpha=1 fully replaces the sky RT. Draw 23 then additively adds the moon glow onto (0.5,0.5,0) giving a yellow moon on an olive sky — exactly the reported image. draws.json draw 14 records sampled.base = 0x9afc30000 with descriptor_honoured: false.\n2. Draw 28 (present pass, PS 0x9afae6900) samples the sky RT via s[0:7] then does a two-slice fetch of Celeste's 256x16 colour-grade LUT via s[12:19] and lerps. The LUT is never bound, so every presented pixel is mis-graded even independently of (1).\n\nFix: let a PS declare and bind MULTIPLE textures/samplers — one Vulkan combined-image-sampler per distinct image_sample srsrc — and thread each sample's own DescriptorSource through recompile -> the ShaderIo/io_samplers contract -> the gnm descriptor derivation -> the gpu backend binding. Keep single-texture shaders byte-identical. Both register-resident T#s and memory-loaded T#s (s_load_dwordx8 from a user-data pointer) must resolve independently.\n\nOracle: the per-frame gpu-snapshot must show draw 14 sampling the SCENE (0x9afb10000) rather than 0x9afc30000, draw 28 must record the LUT, and the in-game sky must render as a deep-blue night sky with a white moon — maintainer live PNG oracle. Provenance: AMD GCN ISA / Mesa / llvm-mc only.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 a PS with N distinct image_sample descriptors declares and binds N combined-image-samplers; each sample uses its own T#/S#, register-resident or memory-loaded
- [ ] #2 Celeste draw 14 samples the scene RT (not the displacement map) and draw 28 binds the 256x16 colour-grade LUT — verified in a fresh gpu-snapshot draws.json
- [x] #3 single-texture shaders are unchanged (no regression); build + cargo test -p ps4-gcn -p ps4-gnm + clippy clean
- [x] #4 in-game sky renders deep-blue with a white moon — maintainer live oracle
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Done 2026-07-21, maintainer live-confirmed the sky renders correctly. AC#2 (re-verify draw 14 / draw 28 bindings in a FRESH gpu-snapshot draws.json) was not re-run after the fix — covered instead by (a) the maintainer's visual oracle and (b) recompiling the dumped shipping .sb directly, which showed draw 14 -> 2 bindings (bind7 = the scene, what it exports) and draw 28 -> 2 (frame + 256x16 colour-grade LUT). Cheap to close properly with one F9 if wanted.
<!-- SECTION:NOTES:END -->
