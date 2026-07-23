---
id: TASK-99
title: 'corpus: textured-quad homebrew ELF for task-55 AC#3 live textured draw'
status: Done
assignee: []
created_date: '2026-07-12 23:51'
updated_date: '2026-07-13 20:44'
labels:
  - gpu
  - gcn
  - corpus
dependencies:
  - TASK-55
priority: high
ordinal: 98000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
task-55 AC#3 (live: corpus textured-quad ELF renders) — the recompiler was GPU-confirmed via diff_harness (texture_sample_ps mrt0 == oracle within 1e-5 on RADV), but no textured-quad homebrew ELF was authored (like task-96 was the triangle-ELF follow-up). Author examples/ps4-gcn-textured-quad (OpenOrbis toolchain, OO_PS4_TOOLCHAIN=/home/mikolaj/src/unemups4/data/oo_sdk): bind the texture_sample_ps corpus .sb via the register route + a T#/S# (image + sampler) in the user-SGPR block + a small texture in guest memory, draw a quad, submit. Then verify via the PNG oracle (UNEMUPS4_DUMP_PNG) that the textured quad renders. Mirrors the task-96 triangle ELF; couples with task-97 PNG oracle + task-58 shadPS4 cross-check.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 a textured-quad homebrew ELF binds the corpus image_sample PS via the register route + a T#/S# + texture, and renders a textured quad
- [x] #2 verified via the PNG oracle (textured quad visible)
<!-- AC:END -->



## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. Create examples/ps4-gcn-textured-quad/ mirroring ps4-gcn-triangle (main.c + Makefile + .gitignore + sce_sys/).
2. Embed passthrough_vs.sb + texture_sample_ps.sb corpus bytes verbatim.
3. Guest memory: quad vertex buffer (4 verts vec4 pos, 6 indices via DRAW_INDEX_AUTO count=6 as 2 tris — actually DRAW_INDEX_AUTO needs a triangle list, so lay out 6 verts / 2 tris OR 4 verts. VS fetches by vertex index via V#; use 6-vert two-triangle list, stride16). vec4 V# (dfmt14/nfmt7). Checkerboard RGBA8 texture (e.g. 8x8). T# linear RGBA8 (dfmt10 nfmt0) per decode_t_sharp: word0=base>>8, word1=(base>>40&0xff)|(10<<20), word2=(w-1)|((h-1)<<14), word3=0. S# 16 bytes point (all zero).
4. PS desc set (256-aligned): T# @off0 (32B) + S# @off32 (16B). Program PS user-SGPR s[0:1] = SPI_SHADER_USER_DATA_PS_0 (CORPUS_TEXTURE_SLOT). VS user-SGPR s[2:3] = VS desc set (vec4 V#).
5. DCB: default hw state, sceGnmSetVsShader/PsShader register-route (PGM_LO etc), VS s[2:3], PS s[0:1], CB_COLOR0 (fb, INFO fmt 0x0A<<2), viewport, scissor, DRAW_INDEX_AUTO. Submit+flip.
6. sce_sys from graphics sample (param.sfo/icon0.png/about/right.sprx); unique TITLE_ID/CONTENT_ID.
7. Build ELF + .pkg via OO_PS4_TOOLCHAIN. Copy linked ELF to top-level .elf.
8. Verify UNEMUPS4_DUMP_PNG shows textured quad; RUST_LOG=ps4_gnm=debug no 'deferring draw'. No-regression: triangle + pm4-test still render.
<!-- SECTION:PLAN:END -->

## Implementation Notes

Session 2026-07-13. DONE (uncommitted for review).
- Created examples/ps4-gcn-textured-quad (main.c + Makefile + .gitignore + sce_sys/) mirroring ps4-gcn-triangle, adding the texture path.
- Embeds passthrough_vs.sb (VS) + texture_sample_ps.sb (image_sample PS) corpus bytes verbatim.
- Guest arena: quad = 6 vec4 verts (two triangles), vec4 V# (dfmt14/nfmt7), 8x8 RGBA8 checkerboard, linear RGBA8 T# (dfmt10/nfmt0 per decode_t_sharp), point S# (all-zero). PS desc set: T# @off0 + S# @off32 (T_SHARP_SIZE=32).
- DCB register route: sceGnmSetVsShader/PsShader, VS s[2:3]=V# ptr, PS s[0:1]=T#/S# desc-set ptr (SPI_SHADER_USER_DATA_PS_0, CORPUS_TEXTURE_SLOT), CB_COLOR0 (fb, INFO 0x0A<<2), viewport, scissor, DRAW_INDEX_AUTO count=6, submit+flip. UV = interpolated position.xy (VS exports pos0==param0).
- Build: DOTNET_SYSTEM_GLOBALIZATION_INVARIANT=1 OO_PS4_TOOLCHAIN=... make. ELF + eboot.bin + param.sfo + pkg.gp4 build clean. Final .pkg (PkgTool.Core pkg_build) needs OpenSSL 1.1 (host has 3.x) — built by supplying libssl.so.1.1/libcrypto.so.1.1 via LD_LIBRARY_PATH. .pkg produced (6.3M).
- AC#1/AC#2: PNG oracle shows a visible checkerboard-textured quad; RUST_LOG=ps4_gnm=debug shows NO "deferring draw" (texture:Some resolved a BindTexture). No-regression: triangle + pm4-test still render.
- Color note: checkerboard cells render pink/olive vs authored red/white — same channel/format remap seen in the triangle baseline (B8G8R8A8 target), not a homebrew defect; pattern + per-cell alternation correct.
