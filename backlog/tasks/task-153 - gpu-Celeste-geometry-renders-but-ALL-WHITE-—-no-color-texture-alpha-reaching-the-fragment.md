---
id: TASK-153
title: >-
  gpu: Celeste geometry renders but ALL WHITE — no color/texture/alpha reaching
  the fragment
status: Done
assignee: []
created_date: '2026-07-16 20:08'
updated_date: '2026-07-17 07:30'
labels:
  - gpu
  - gcn
  - gnm
  - celeste
  - retail
  - bug
dependencies: []
priority: high
ordinal: 159000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
After task-152 (dst_sel fix), Celeste's loading-screen geometry finally RASTERIZES (progress bar + particle field, PNG orchestrator-confirmed) — but every fragment is uniform WHITE on black: no color, no texture, no alpha. So the vertex/clip stage is now correct, but the FRAGMENT output is stuck at white. Investigate why the PS produces white instead of the sampled texture / vertex color / material constant. Candidates: (a) the sampled combined-image-sampler result isn't used by the PS output (texture bind resolves — task-149 fixed InlineVSharp T# — but is the SAMPLE actually feeding the color? check the recompiled PS: does it OpImageSampleImplicitLod and route the result to the output, or is the output a constant/uninit that reads white?); (b) the interpolated vertex color/UV (VS->PS varyings) isn't wired — if the PS reads an interpolant that our recompiler doesn't emit/link, it may default to 1.0 (white); (c) alpha/blend state leaves everything opaque white, or the texture data is all-white (macro-tiled textures still defer per task-149 tex_macrotile ~60/run -> if the bound texture is a fallback/white, output is white); (d) the MVP/vertex-color constant buffer feeds white. Method (proven this session): magenta-clear + force-constant-color isolation, dump the recompiled PS SPIR-V for a videoout draw, check the sample->output path + the varyings. PNG oracle for all frame claims (orchestrator Reads it). Assets gitignored. Relates to task-149 (tex_macrotile defers -> the actual gameplay textures may not be uploading), doc-6.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 Root-caused why fragments are white (sample-not-used / missing varying / white texture / blend)
- [x] #2 Fix: the PS outputs the real sampled texture / vertex color; PNG oracle shows colored (non-white) content
<!-- AC:END -->





## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Recon-first (opus lane af45ab1): dump recompiled PS SPIR-V + trace sample->output path + VS->PS varyings + tex_macrotile defer count. Root-cause which of (a) sample-not-used / (b) missing varying / (c) white macrotile texture / (d) const-buffer/blend, THEN implement smallest fix in a second lane. Golden+task-122 oracle must stay green; DST_SEL_IDENTITY 0xFAC passthrough must not regress. PNG oracle (orchestrator Reads) for the color verdict.
<!-- SECTION:PLAN:END -->

## Recon (lane af45ab1, 2026-07-16)
NOT the shader — recompiled PS `gcn_0x98166f700` (tex×vertexcolor, 2682 rendered draws, blend on) is provably correct: spirv-val clean, `OpImageSampleImplicitLod` -> 4× `OpFMul` (sample × attr0 vcol) -> store to Location-0 Output; both varyings (attr0 vcol, attr1 UV) emitted as Location inputs. Candidates (a) sample-not-used + (b) missing-varying MECHANICALLY EXCLUDED. Do NOT touch recompile.rs (task-122 interp==recompile golden + 0xFAC DST_SEL passthrough risk).
White comes from the INPUTS to that correct multiply:
  1. bound linear-texture texels (2682 rendered draws bound non-macrotiled textures; if texels wrong-detiled/white -> white flows through).
  2. fetch-spliced attr0 vertex color: paired VS `0x98166f800` opens `s_swappc_b64` (fetch-shader call); fetch body outside 64KB dump (UnrecognizedFetchBody), VS fails isolated recompile (UnsupportedInst Sop1 op:33). If fetch-supplied vcol defaults to 1.0 -> tex×1.0 = tex, but if BOTH ~1.0 -> white.
Defer taxonomy (60s run): 908× "2D macro-tiled (no detiler) — deferring draw" (tiling_index=8) = colored sprites VANISH (whole-draw defer, NOT white fallback); 677× "VS+PS both declare CB, collide set0/bind2" = content vanish; only 6 macrotile among the 2682 rendered. Red herring: exec.rs:1156 prints pre-fold key `texture:None`; real texture folded at exec.rs:612-627.
=> The white VISIBLE geometry = the 2682 linear-texture draws. The COLORED game art likely = the 908 macrotile-deferred draws. Next: probe lane to disambiguate texel-vs-vcol for the 2682, and confirm the 908 macrotile draws carry colored atlas data. Fix belongs in texture/detile bind path (crates/gnm exec.rs/derive.rs, ps4_core::tiling) or VS fetch-splice (shader/gcn.rs) — NOT recompile.rs. Secondary tasks: macrotile detiler + CB-collide dual-slot (relates task-150).

## Probe (lane ac2816d, 2026-07-16) — ROOT CAUSE CONFIRMED
Disambiguated live: (a) NO white linear texel — emit_image_upload/BindTexture fired ZERO times for sampled draws (no linear/thin1d texture ever bound). (b) vertex-color fetch HEALTHY — VS `0x98166f800` resolve_fetch_call recovers real V#s (attr0: vsharp_sgpr=8 dest_vgpr=4 comp=4 desc_offset=0; attr1 @16; attr2 UV @32), no UnrecognizedFetchBody, no default-1.0. (c) the colored art lives in MACRO-TILED textures: first deferred tex `base=0x9afc28000 w=1500 h=199 tiling_index=8`, raw bytes min=0 max=255 mean=17.6, 67 distinct RGB, R=G=B=A premult-grayscale coverage = real font/UI atlas. Orchestrator Read macro_alpha.png: structured glyph content + diagonal bank/pipe swizzle signature (NOT white, NOT garbage — real art scrambled by tiling).
=> White frame = 908 macro-tiled (tiling_index=8) draws DEFER WHOLE at exec.rs:589-597 (no macro-2D detiler) -> colored sprites/text never render -> only untextured geometry paints white. FIX = implement macro-2D bank/pipe swizzle detiler (clean-room from AMD GCN spec): TileKind::Macro2d in crates/core/src/tiling.rs + dispatch in crates/gnm/src/cache/tile.rs + remove the Macro2d->None defer. NOT recompile.rs, NOT texel-decode, NOT fetch-splice (all proven healthy). Impl lane a04bad8 in flight. Offline fixture: scratchpad/macro_raw.bin (w=1500 h=199 32bpp). Follow-ups (separate, content-suppressing not white-causing): 677 VS+PS CB-collide defers (relates task-150 2-slot CB model).

## Detiler MERGED (3d20861, 2026-07-16) — AC#1 done, AC#2 NOT yet
Lane a04bad8 corrected the root cause: tiling_index=8 is NOT 2D macro bank/pipe swizzle — it is GFX7 ARRAY_LINEAR_ALIGNED (row-major, row pitch padded to align(width,64) texels = 1536 for the 1500-wide 32bpp font atlas). Implemented TileKind::LinearAligned (idx 8; >=9 stays Macro2d/deferred), shared linear_aligned_pitch/linear_aligned_texel_offset helpers, cache detile/tile pitch-strip, matching oracle stride (interp.rs) so oracle==upload byte-for-byte (task-98/122), byte_span covers padded pitch. 316+202 tests green, clippy/fmt clean. Macro-tiled defer 908 -> 0 live. OFFLINE ORACLE PNG (orchestrator Read): font atlas straightens to crisp "Matt Makes Games Inc. presents" — detiler PROVEN correct.
BUT full-frame celeste153_after.png (orchestrator Read) STILL shows white progress-bar block + particle squares — NOT colored. So the detiler was necessary but not sufficient. Remaining suspects for the still-white frame: (1) the textured sprite draws that bind the now-detiled atlas may STILL defer on the 677x "VS+PS both declare a constant buffer, collide on set0/bind2" wall (relates task-150 2-slot CB model) -> texture ready but draw never issues; (2) captured frame may be an early loading frame before the splash/content draws; (3) the visible white blocks may be the 1557 untextured pkrtz draws (no sampler) whose solid color comes from a const-buffer/vertex-color that reads 1.0. NEXT: confirm whether the atlas-binding draws render or still defer (CB-collide), capture later frames to rule out timing, then fix the confirmed blocker.

## Probe (lane aa26bdb, 2026-07-16) — CB-collide DISPROVEN, deeper: fragment inputs not reaching PS
Per-shader ENTER->RENDER/DEFER counts (57s run): atlas sampler 0x98166f700 = 608 ENTER / 608 RENDER / 0 DEFER (tex=true cb=true) — the CB-collide does NOT eat the textured atlas sprites (task-150 is a LATER-scene concern: the 80 cb-collide defers hit different shaders 0x98166fc00 @21:27:31). pkrtz 0x981400000 = 301/301 render (tex=false, cb=true, Pixel-stage CB reads f32=[0,0,0,1]=opaque BLACK for 299/301). Draw TARGET split: atlas+pkrtz go to videoout (507+140) AND offscreen@0x982c48000 (163+138).
Frame timeline (592 per-flip PNGs, full-pixel scan): frames 0-2 black, 3-6 uniform YELLOW (255,255,0) guest clear, 7-591 pure black. Draws ran continuously (no hang) yet every presented frame black/debug-clear. => RENDER BLOCKER not capture-timing. (celeste153_after.png white-blocks was a DIFFERENT scene: loading screen.)
Two symptoms, both = fragment color INPUTS not reaching the PS (consistent with recon#1 proving the SPIR-V itself is correct):
  1. Atlas 0x98166f700 emits BLACK — splash is WHITE text, so the sample returns 0 at runtime (texture/sampler/UV binding delivers nothing, or vertex-color attr0 = 0). Texture IS bound (BindTexture fires, tex=true) so suspect S#/sampler config, UV interpolant (attr2), or attr0 vcol actual VALUES=0.
  2. pkrtz 0x981400000 ignores its (0,0,0,1) black Pixel-stage CB -> outputs WHITE. task-134 zero-inits regs so uninit=0=black; white=1.0 is a POSITIVE value => suspect the compr/pkrtz MRT export decode (exp...compr) OR the Pixel-stage CB descriptor not actually delivering to the PS.
NEXT: fragment-output isolation probes (doc-5 method) on the atlas draw — force PS out = red (pipeline/raster/present OK?), = sampled texel only (texture reaching fragment?), = vertexcolor only (attr0 non-zero?); + force pkrtz out = its CB value. Bisects which input fails. NOT task-150 (cb-collide disproven for splash).

## Bisection DONE (lane a2feeb5, orchestrator Read 4 PNGs, 2026-07-16)
Forced-output isolation on the atlas draw (frame 200, where forced-red fills the screen = atlas geometry is a fullscreen quad on-screen):
  red  -> fullscreen RED  => pipeline/raster/blend/present all WORK; bug is purely color inputs.
  vcol -> fullscreen YELLOW (1,1,0) => attr0 vertex-color reads NON-ZERO (works).
  texel-> BLACK => texture sample returns 0.
  uv   -> BLACK => attr2 UV interpolant reads ZERO.
ROOT CAUSE (color): UV (attr2) reads zero -> sample at texcoord (0,0) = atlas corner (transparent/black) -> black output. attr0 works, attr2 does not. The atlas VS uses a fetch shader recovering THREE SEPARATE V# descriptors (desc_offset 0/16/32 = attr0 color, attr1, attr2 UV) => MULTI-V# streams, not one interleaved buffer.
Two real bugs in the vertex-fetch path, both to fix:
  1. CONFIRMED (recompiler drops the fetch offset): crates/gcn/src/recompile.rs::emit_mubuf (2353)/fetch_buffer_component (2527) computes dword = index*stride/4 + src_comp with NO soffset+offset term (hardcodes offset:0 @2371); the oracle crates/gcn/src/interp.rs::exec_mubuf (~1629) adds + soff + offset. Corpus is all offset=0 so task-122 never caught it. Fix: thread soffset+offset into the fetch address, mirror the oracle; add a non-zero-offset differential golden.
  2. exec.rs::setup_draw SSBO path (~741-793) .find()s only the FIRST VertexBuf range and packs only ITS num_records/stride/dst_sel push-constants; recompile.rs::ensure_vs_buffer (~3237) caches one buffer for ALL MUBUF fetches. So attr2 (UV) fetches with attr0's params -> num_records clamp -> index 0 -> zeros. Fix: bind EACH attribute's V# as its own SSBO/stream with per-stream push-constants (num_records/stride/dst_sel/soffset/offset).
NEXT: implement multi-stream vertex fetch + emit_mubuf offset fix; verify task-122 oracle (interp==recompile) + existing goldens stay green + new multi-buffer/nonzero-offset golden; PNG oracle: the splash "Matt Makes Games" text must render (currently black).
Bisect PNGs: target/t153/{red,texel,vcol,uv}/frame_0200.png.

## Lane finding (multi-V# vertex layout — live dump 2026-07-16)
Celeste's atlas VS fetch recovers THREE distinct V# from the SAME descriptor set (sbase=s2) at desc_offset 0/16/32:
- attr0 srsrc=s8  desc_off=0  count=4  base=B+0   stride=24 dst_sel=[4,5,6,1]  (xyz pos, w=1)
- attr1 srsrc=s12 desc_off=16 count=4  base=B+12  stride=24 dst_sel=[4,5,6,7]  (color RGBA8)
- attr2 srsrc=s16 desc_off=32 count=2  base=B+16  stride=24 dst_sel=[4,5,0,1]  (UV, z=0/w=1)
All three MUBUF imm_offset=0, soffset=InlineInt(0). So it's an INTERLEAVED buffer: same 24-byte stride, DIFFERENT V# base per attribute (+0/+12/+16). Bug 2 (single shared SSBO bound to attr0) is the active cause: attr2 fetched attr0's base+params -> UV read xy-position bytes -> wrong -> black. Bug 1 (dropped imm_offset) is 0 here but still threaded for correctness. Fix: per-V# SSBO binding keyed by srsrc, each with its own base/num_records/stride/dst_sel push-constant group; route each MUBUF fetch to its srsrc's binding.

## Multi-stream fix MERGED (8ec13fc) — verified-correct but PNG STILL no color (2026-07-17)
Multi-stream vertex fetch merged: attr2 UV now fetches from its own V#/SSBO (per-stream bindings 0/3/4, per-stream push-constants), + emit_mubuf soffset/offset threaded. task-122 oracle + 2 new goldens with teeth (offset_fetch_vs, inline_multi_fetch_vs — the latter fails on stream-collapse with the exact zero-UV symptom). 322 tests green, clippy/fmt clean. Runtime: 305 draws now bind 3 streams.
PNG ORACLE (orchestrator ran full 641-flip capture UNEMUPS4_DUMP_PNG + Read frames): STILL no visible color. frame_0100 = full white; frame_0450 = white particle squares on black (loading-screen particle field). Every frame max=255 but sparse-white-on-black — the untextured UI (particles/progress) renders WHITE, the textured atlas content (splash text, colored sprites) is NOT visible on videoout. So the UV fix is a real correctness win at the shader/golden level but did not by itself produce visible color.
LEADS for the still-no-color wall (next investigation):
  (a) OFFSCREEN-RT COMPOSITE: earlier probe showed atlas+pkrtz draws split videoout (507+140) AND offscreen@0x982c48000 (163+138). The textured/colored content likely renders to the offscreen RT which is never composited/sampled back to videoout -> videoout shows only the untextured white UI draws. This is task-56 (RT-as-texture host aliasing + compositor) territory — the offscreen RT must be bound as a texture by the videoout composite draw.
  (b) pkrtz-WHITE: the untextured pkrtz draws (PS 0x981400000, Pixel-stage CB=(0,0,0,1) black) render WHITE not black -> the Pixel-stage const buffer is not reaching the fragment (compr-export decode, or CB descriptor not delivered to the PS). This makes the particles/progress white instead of their intended color.
Both are downstream of the (now-fixed) vertex fetch. AC#2 NOT met (no colored PNG). Recommend: investigate (a) offscreen-RT composite first (most likely gates all colored content) — relates task-56. Frames: scratchpad/frames/frame_0100.png (white), frame_0450.png (white particles).

## Recon offscreen-RT + composite (lane ad91aed, 2026-07-17)
Premise correction: offscreen RT bases are PER-RUN (this run: 0x982c48000 + 0x9b00e0000, both 1920x1088, fmt 0xa COLOR_8_8_8_8, tile_idx=14). BOTH recognized as RTs (CreateRenderTarget fires, 475 producer draws), scene's REAL large atlases (spans 0xcd0000/0x400000/0x830600, tiling_idx=8) sampled INTO the RTs. So the textured scene IS rendered — into offscreen RTs.
- Q2: videoout composite NEVER samples the RT (rt_hit 0/702). RT content produced, never consumed.
- Q3: RT never flipped. Only display buffer = degenerate 1x3 @0x981670000. Present blits the videoout target (separate NoColorBase surface).
- Q4: force-UV on videoout atlas draws = CLEAN RG GRADIENT across the central block + every particle (orchestrator Read png_uv/frame_0400.png) => multi-stream UV fix WORKS END-TO-END. force-texel = BLACK because 467/470 videoout composite samples bind a DEGENERATE 8-byte T# (span=0x8, tiling_idx=0), not the real atlas.
- Q5: pkrtz PS CB IS delivered (Pixel,16 resolves every draw). White = color-math over the CB, not a delivery gap. All Celeste PS export via compr/pkrtz (only 3 distinct PS: 2 no-sample + 1 sample).
Two intertwined remaining gaps (both downstream of the merged UV+detiler fixes):
  A. VIDEOOUT COMPOSITE 8-byte T#: the direct-to-videoout atlas draws (SpriteBatch solid-color primitives — particles/progress) sample an 8-byte placeholder T# -> texel black -> invisible. Likely the SpriteBatch 1x1 white-pixel texture reading BLACK instead of white. Concrete, likely-fixable (texel-decode / tiny-texture upload / sample). Fix candidate: derive_texture_binding / the tiny-T# upload path.
  B. RT-AS-TEXTURE CONSUME (task-56 AC#3): the real scene renders into offscreen RTs that nothing composites back to videoout (rt_hit 0) and that are never flipped. Celeste consumes its RT via a mechanism we don't model (a resolve/copy to a linear surface, or a compositor T# pointing at a resolved copy, not CB_COLOR0_BASE). This is the RT->screen end-to-end model — a larger design/RE effort.
NEXT: probe the 8-byte T# (gap A) — raw descriptor dims/format/base + guest texel bytes (white FF vs black 00) + uploaded-or-defaulted — to pin whether it's a fixable tiny-texture bug or the genuine placeholder that means gap B (RT-consume) is the only path to the scene. Bisect PNGs: probe56/png_uv (gradient), png_texel (black).

## FINAL diagnosis (lane a14aa4a, 2026-07-17) — gap A dead, gap B (RT-consume) is the only path
The 8-byte composite T# is a GENUINE degenerate/uninitialized inline descriptor, NOT fixable as a texture bug:
- raw T# (s0-s7, InlineVSharp{sgpr=0}): `?? 00100009 00000001 20077fac 00f3e000 0 0 0` -> decodes 2x1, base 0x901846d2400 UNMAPPABLE (bounded seam faults), byte_span=8 (2*1*4). word2=1 => 2x1 (not a 1x1 white pixel). Only s0 varies between the 467 draws; s1-s7 constant non-descriptor data.
- Q2: guest bytes at base = UNREADABLE (unmapped). Not white, not black — base points nowhere. Even dropping the HLE 48-bit base extension leaves the base outside guest range.
- Q4: IDENTICAL provenance (InlineVSharp{sgpr=0}) as the REAL atlas draws of the SAME PS 0x98166f700 (real T#: base 0x9afc28000 MAPS, 1500x199, dfmt=10, tile=8). So it's not "read from wrong place"; no valid descriptor hides at any alt SGPR offset. The recompiler correctly derives sgpr=0 (MIMG srsrc).
Conclusion: these 467 videoout composite draws have a genuinely garbage/uninitialized T# in their SGPRs — the real texture content does NOT exist as a bound sampled texture for them. The color lives ONLY in the offscreen RTs (0x982c48000/0x9b00e0000). Combined with rt_hit=0 (no draw samples an RT base) and RT-never-flipped: within our current model the RT content has NO route to the screen.
=> task-153's "why is the frame colorless" is now FULLY root-caused. The FIX is NOT in task-153's texture/vertex scope (both merged + verified: detiler 3d20861, multi-stream UV 8ec13fc, UV proven end-to-end via gradient). The fix is modeling how Celeste consumes its offscreen RTs back to videoout — a GPU RESOLVE/copy of CB_COLOR -> a linear texture the compositor samples, or a compositor T# we don't capture. This is task-56 AC#3 (RT-as-texture end-to-end) + RE of Celeste's specific RT->screen path. LARGER design effort, NOT a quick patch. Secondary safe hardening (optional): bind_texture (exec.rs:991)/derive_texture should defer a draw whose T# [base,base+span) fails the bounded seam rather than uploading from an unmapped base (prevents a garbage black sample; does not add color).
HANDOFF: task-153 AC#1 done (root-caused). AC#2 (colored PNG) blocked on the RT-consume model -> reassign to task-56 AC#3 / a new design task. task-153 texture+vertex work is complete + merged.

## RE BREAKTHROUGH (lane a9a3209 + orchestrator verify, 2026-07-17) — it's a VIDEOOUT bug, NOT RT-as-texture
Celeste uses DOUBLE-BUFFERED DIRECT SCANOUT, not render-to-texture-then-composite. sceVideoOutRegisterBuffers(handle=0, start=0, count=2) with list[0]=0x981670000, list[1]=0x982c48000. It renders the full scene directly into a scanout buffer (0x982c48000 = scanout buffer #1, which we misclassified as a private offscreen RT), then flips via sceGnmSubmitAndFlipCommandBuffers(..., buf_idx). NO GPU resolve/copy exists (all 2525 IT_DMA_DATA are mem->register; the 2x1 garbage T# is Celeste's dummy solid-color UI texture, a red herring). rt_hit=0 is EXPECTED — nothing samples an RT because Celeste never intends to. task-56 RT-as-texture machinery is correct but simply unexercised by Celeste; AC#3 stays open for a real 2-pass title.
THREE concrete videoout/flip bugs (all confirmed against the vendored SDK header data/oo_sdk/include/orbis/_types/video.h: OrbisVideoOutBufferAttribute { int32 format@0; int32 tmode@4; int32 aspect@8; u32 width@12; u32 height@16; u32 pixelPitch@20; u64 reserved[2]@24 }):
  FIX 1 (bridge.rs video_out_register_buffers ~297-353): reads only list[0] (memory.read::<u64>(ptr)), ignores count, never iterates -> buffer #1 (0x982c48000, the scene) never registered -> misclassified Offscreen. Fix: iterate i in 0..count, read list[i]=read::<u64>(ptr+i*8), register each at index start_index+i; skip zero/EFAULT entries without aborting.
  FIX 2 (libscevideoout/mod.rs:85): sce_video_out_set_buffer_attribute is a NO-OP STUB with the WRONG signature `(_handle:i32,_attr:u64,_val:u64)->i32{0}`. Real API = sceVideoOutSetBufferAttribute(attr*, u32 pixelFormat, u32 tilingMode, u32 aspectRatio, u32 width, u32 height, u32 pixelPitch) — 7 args — that FILLS *attr. Because we no-op it, the guest's attr struct is never populated -> read_videoout_attr@+12/+16 (correct per SDK) reads garbage (1 and 3) -> degenerate 1x3 display buffer = THE white-on-black cause. Fix: implement it to write the struct at the SDK offsets via the bounded seam (format@0, tmode@4, aspect@8, width@12, height@16, pixelPitch@20). ABI: attr=arg0(rdi), format=rsi, tmode=rdx, aspect=4th (R10 per SYSCALL RCX-clobber task-106), width=r8, height=r9, pixelPitch=7th(stack). read_videoout_attr already reads the correct offsets — keep it; optionally thread pixelPitch. (Supersedes the design agent's plausibility-gate heuristic — unnecessary once the writer is fixed.)
  FIX 3 (libscegnmdriver/submit.rs + core/gpu.rs + gpu/src/lib.rs::submit_and_flip:94): sce_gnm_submit_and_flip_command_buffers drops vo_handle(arg7)+buf_idx(arg8, stack), and GpuManager hardcodes submit_flip(0,0) -> always present index 0 (the mis-parsed 1x3), never index 1 (the scene). Fix: read the stack args (precedent: task-106 R10 4th-arg + Doom flipArg), thread buf_idx through PresentSink::submit_and_flip into submit_flip(vo_handle, buf_idx). Default (0,0) on stack-read failure so no regression.
Backend check: the display loop/present must resolve the flipped buffer's base (buffers[(handle,index)]) and blit THAT buffer, not a single fixed draw_target — if it presents one fixed image regardless of index, add per-buffer scanout images keyed by (handle,index). Verify.
Fix order (land+PNG-check each): FIX 2 (attr parse -> full-size buffer) -> FIX 1 (register buffer #1) -> FIX 3 (flip the right index = the payoff). Existing TargetKind::Videoout path then routes the scene to screen; no RT-as-texture needed. PNG oracle mandatory (orchestrator Reads). Examples (ps4-gcn-textured-quad uses SetBufferAttribute+flip) must not regress.

## Videoout direct-scanout fixes MERGED (ad7bb19, 2026-07-17) — white-on-black GONE, scene reaches scanout
3 fixes landed + verified: (1) register ALL buffers (0x982c48000 now videoout index 1), (2) implement sceVideoOutSetBufferAttribute (was no-op stub -> now writes 1920x1080 struct at SDK offsets), (3) thread flip buf_idx. Runtime trace confirms all 3 fire (both buffers register 1920x1080, buf_idx threaded, backend blits flipped index). 287 tests + clippy + fmt clean, ps4-gcn-textured-quad no regression.
PNG ORACLE (orchestrator Read, 753-frame run): the white-on-black bug is GONE. Presented frames now black-background with COLORED content reaching the scanout buffer (frame_0700 histogram: 2845 cyan (0,255,255) + 8 green px on black). BUT not yet a clean colored scene — REMAINING (separate present-path wall, filed as follow-up): (a) alpha=0 on every pixel (present blit / swapchain alpha handling); (b) R=0 in ALL pixels (only G/B channels -> a channel/pixel-format order issue in the present readback/scanout); (c) content is SPARSE + SCATTERED (2845 of 2M px) and STATIC frames 550-752 -> strongly suggests the scanout buffer is TILED (tile_idx=14 seen earlier) and presented as LINEAR -> scrambled/sparse (needs the macro-tile detile on present, the tile_idx>=9 Macro2d path we defer for textures — same gap, present side).
task-153's texture+vertex+scanout-plumbing scope is COMPLETE + merged (detiler 3d20861, multi-stream UV 8ec13fc, videoout scanout ad7bb19). The remaining colored-frame work is the PRESENT-PATH format/tiling residual -> task-154.
