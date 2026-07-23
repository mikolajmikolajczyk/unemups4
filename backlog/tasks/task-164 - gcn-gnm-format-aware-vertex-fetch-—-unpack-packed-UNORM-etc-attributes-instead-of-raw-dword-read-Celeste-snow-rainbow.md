---
id: TASK-164
title: >-
  gcn/gnm: format-aware vertex fetch — unpack packed UNORM/etc attributes
  instead of raw-dword read (Celeste snow rainbow)
status: Done
assignee: []
created_date: '2026-07-17 16:55'
updated_date: '2026-07-17 17:38'
labels:
  - gcn
  - gnm
  - gpu
  - celeste
  - retail
  - vertex-fetch
dependencies: []
priority: high
ordinal: 170000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
ROOT CAUSE (task-163 snow investigation, confirmed): Celeste's SpriteBatch vertex color is a packed RGBA8_UNORM attribute (V# dfmt=Format8_8_8_8 nfmt=Unorm, stream1/set0-bind3, offset +12 in a 24-byte interleaved vertex) — 4 bytes = ONE 32-bit dword. GCN buffer_load_format_xyzw on an _8_8_8_8/unorm V# unpacks that ONE dword into 4 normalized floats (byte/255). But our vertex fetch is FORMAT-BLIND: recompile.rs::fetch_buffer_component (~L2614/2671) loads  (=4) SEPARATE raw dwords and bitcasts each u32->f32, and interp.rs::exec_mubuf (~L1634) mirrors that. So for the color it reads dwords at bytes [12..28): ch0=packed-RGBA8-dword-as-f32 (garbage), ch1=uv.x, ch2=uv.y, ch3=next-vertex-pos.x -> the visible position-dependent UV rainbow instead of the intended ~white per-vertex color. decode_v_sharp CORRECTLY extracts dfmt/nfmt (Mesa-verified) — the bug is purely that the fetch ignores them. Wrong stream/offset/dst_sel all RULED OUT (color swizzle identity [4,5,6,7], so interp==recompile both wrong -> task-122 differential passes while the frame is wrong). FIX (cross-crate, per snow agent's scoping): (1) thread dfmt/nfmt as new per-stream push-constant members (recompile.rs PC block ~L487 PC_MEMBERS_PER_STREAM, ensure_pc_block, load helpers); (2) in fetch_buffer_component emit unpack/normalize — for packed _8_8_8_8 unorm read ONE dword at element base, extract byte src_comp, /255.0; keep raw-read for float32; MoltenVK-portable branchless (nested OpSelect like the dst_sel style); handle the other common packed formats too (2_10_10_10, 16_16 unorm/float, snorm/uint/sint per nfmt); (3) mirror bit-exactly in interp.rs::exec_mubuf so interp==recompile; (4) plumb the format onto BackendCmd::BindStorageBuffer (exec.rs ~L794) + the gpu backend push-constant write; (5) regenerate task-122 differential goldens. AC: Celeste snow renders ~white (not rainbow) per PNG oracle; interp==recompile holds; goldens updated. Relates task-140/155 (per-stream push constants), task-98/122 (differential), the Mesa audit (decode confirmed correct).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 Vertex fetch unpacks packed formats (RGBA8_UNORM + common others) per the V# dfmt/nfmt instead of raw-dword bitcast
- [x] #2 interp==recompile holds (task-122 differential goldens regenerated)
- [x] #3 Celeste snow renders ~white per-vertex color, not the UV-debug rainbow (PNG oracle)
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
MERGED 3407210. Format-aware vertex fetch: threads V# dfmt/nfmt as per-stream push constant, branchless SPIR-V unpack in fetch_buffer_component (8-bit + 16-bit unorm/snorm/uint/sint/half; float32 bit-identical), interp.rs mirrors bit-for-bit (convert_packed_int), goldens regenerated, new differential test proves interp==recompile AND byte/255. 290 gcn/gnm + 41 gpu/core tests pass, clippy/fmt clean. PNG ORACLE (orchestrator read frames 8/20): Celeste sprite color is now WHITE/GRAY with per-particle alpha variation, NOT the rainbow — CONFIRMED FIXED. 2_10_10_10 falls back to raw (follow-up, DataFormat doesn't model it). Residual (separate): the visible content still binds the 2x1 white-dummy (hard white squares + a solid white BAR = the logo quad rendered white-dummy instead of the real 1500x199 logo texture) — the sprite SHAPE/logo needs the provenance fix (why the presented draw resolves the degenerate placeholder T# instead of the real atlas), which is the re-sharpened task-157 question.
<!-- SECTION:NOTES:END -->
