---
id: TASK-171
title: >-
  gnm/gpu: Celeste TITLE screen shows atlas-blit + garbage band + flicker —
  render-target / post-processing handling (revealed once textures render)
status: Done
assignee: []
created_date: '2026-07-18 08:50'
updated_date: '2026-07-19 22:18'
labels:
  - gnm
  - gpu
  - celeste
  - retail
  - render-target
dependencies: []
priority: medium
ordinal: 175000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Once textures render (task-157 EOP fix), the Celeste TITLE screen (mountain + CELESTE logo + parallax) shows heavy artifacts the white-dummy era hid: (a) the FULL texture atlas blitted across the top of the screen (button glyphs, hearts, icons in a grid), (b) a horizontal GARBAGE/noise band (uninitialized memory rendered), (c) intermittent FLICKER between correct and garbage frames. The correct scene (pink mountain + gold CELESTE logo + snow + gradient) renders underneath. Celeste's title uses render-to-texture (mountain parallax, bloom, the postcard transition) then composites — so this is render-target handling: we likely present an RT's raw/uninitialized memory, fail to clear an RT, or an RT aliases the atlas. Related backlog task-56 (RT-as-texture host aliasing + opt-in readback). Distinct from the splash (which is clean post-fix) and the intro loop. METHOD: run to the title screen, capture the flicker sequence (good vs garbage frame), correlate garbage frames to the DCB/present/RT state; check RT alloc/clear/present-buffer-selection and whether the timing (pipelined EOP) desyncs RT sync. Real-PS4 scraper for the title-screen DCB reference.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 The title-screen atlas-blit + garbage band + flicker root-caused to render-target / present handling
- [ ] #2 Title screen renders the composited scene cleanly (no atlas-blit, no garbage, no flicker) — PNG/live oracle
<!-- AC:END -->



## Notes

<!-- SECTION:NOTES:BEGIN -->
### Live screenshot evidence (2026-07-18, maintainer, main @ task-169, 27 FPS)
The TITLE screen renders CELESTE + mountain correctly BUT with a full-screen mess overlaid:
- **The entire glyph/texture atlas is drawn onscreen** across the top ~40% — controller/keyboard button glyphs (A/B/X/Y…), the colored collectible hearts, mountain icons, UI thumbnails — i.e. the atlas is being rasterized as geometry onto the final framebuffer.
- **A garbage character-band** ("¿GNkX)(KSRTTL…") = the font atlas's glyph map decoded as a text draw.
- **Real title elements present** (CELESTE ×2, mountain, PICO-8, postcards, "CELESTIAL RESORT HOTEL", snow bokeh) but **scenes overlap and flicker**; textures that should cross-fade smoothly instead flicker; the snow flickers.
- Progress vs before: the studio-splash intro ("Matt Makes Games") is now STABLE (task-169 + task-172 confirmed faithful content) — this title-screen breakage is a SEPARATE, later-scene bug.

**Read:** draws that on real HW target OFFSCREEN render-targets (or are composited/cross-faded through an RT) are all landing on the swapchain framebuffer at once → the atlas/intermediate content is presented raw instead of sampled into the composite. This is the RT-as-texture / post-processing handling gap (relates task-56 §8.5 RT-as-texture host aliasing + readback, and task-163 "content renders offscreen but never reaches the swapchain correctly"). The flicker = double-buffered RT contents alternating / not being sampled as a texture between passes.

**Next (when picked up):** capture the title-screen submit sequence (UNEMUPS4_DUMP_SUBMIT + PM4 trace), identify the render-target set/bind (CB_COLOR / RT base) vs the final flip surface, and check whether we (a) allocate + honor offscreen RTs, (b) alias an RT as a sampled texture for the next pass, (c) present the correct final surface. The real-PS4 scraper (task-168/172) can capture the real title-screen DCB/RT sequence as the ground-truth reference. Likely shares root with task-163.

### SHARPENED symptom (2026-07-18, maintainer live, main @ task-173 S#-wrap fix)
After the S#-wrap fix the STATIC studio splash now renders CLEANLY and correctly ("Matt Makes Games Inc. / presents" — smooth gold→purple gradient, snow bokeh, crisp outlined text; the backdrop no longer tiles — screenshot Image#17). So the per-scene rendering is now good. **The remaining bug is the SCENE TRANSITION / crossfade:** when the splash text CHANGES (studio → "a game by…" credits → CELESTE title), the scenes "przeskakuje jedno w drugie" (snap/blend between scenes instead of a smooth crossfade) AND **the background sometimes flickers to BLACK.**
**Read:** Celeste cross-fades between splash screens THROUGH render-targets (render scene A + scene B to offscreen RTs, blend by alpha to the framebuffer). The "black flicker" is the strong clue — a render-target that should hold the previous/next scene for the crossfade is momentarily BLACK: either we clear it at the wrong time, don't preserve its contents across the pass, or present an un-rendered/empty RT for a frame. The "jump one into the other" = the crossfade alpha-blend not compositing (hard scene swap instead of dissolve). This is the same RT/compositing gap as the title-screen atlas-splatter, seen at the TRANSITION boundary. **First debug step:** UNEMUPS4_DUMP_SUBMIT across a transition; find the RT allocations (CB_COLOR base) for the two scenes + the blend/present draw that samples them; check clear vs load-op and whether we alias the RT as a sampled texture for the blend. Real-PS4 scraper (task-172 VBUF + DCB) can capture the real transition's RT/blend sequence as ground truth.
### ROOT CAUSE — airtight, evidence-pinned (2026-07-18, opus headless — worktree agent-aa461f7fd09ecf828)
The title/gameplay scene renders through OFFSCREEN render-targets (splash = direct scanout, confirmed doc-6 Entry 18; title = multi-pass RT). Decoded title render graph (frame 900): draws 0-8 → RT `0x9b00e0000` (1920×1088, mountain/parallax), draws 9-12 → bloom RTs `0x9b1418000`/`0x9b1658000` (1024×576 half-res), draws 13-16 → SCANOUT0 sampling all three RTs (composite), draws 17-22 → SCANOUT0 sampling atlas (CELESTE logo/UI). Composite T# bases exactly match RT bases → our RT-as-texture lookup (task-56) recognizes them; that path is CORRECT.

**The break:** the RT-PRODUCER draws (the big 31785- and 100254-vertex mountain/parallax geometry into `0x9b00e0000`, and the bloom-RT fills) all **DEFER** in our executor because their shaders declare **BOTH a VS constant buffer AND a PS constant buffer**, which collide on our single `set0/bind2` const-storage slot — the doc-6 Entry 10 "strict-or-defer" dual-CB limit (`crates/gnm/src/exec.rs:506-513`). Because `register_render_target` runs BEFORE the defer check (`exec.rs:456`) and a deferred draw's already-pushed cmds are never rolled back (`exec.rs:267-275`), each RT is ALLOCATED (`CreateRenderTarget`) but **never rendered into** → the composite/present draws sample undefined/partially-rendered RT memory → **black background, garbage bands, stale-VRAM "atlas" splatter**, and the crossfade "snaps" because the RTs it blends hold garbage. Confirmed via `RUST_LOG=ps4_gnm=debug` trace: every large Offscreen producer draw logs DEFER "both VS and PS declare a constant buffer"; only tiny draws (count 3/6/30) + the scanout UI/logo draws record — hence CELESTE logo + UI appear correct ON TOP of the broken background.

**Our RT model (for reference):** target class in `crates/gnm/src/derive.rs:88-142` (Videoout iff CB_COLOR0_BASE exactly matches a display buffer via `crates/gpu/src/lib.rs:27-32`, else Offscreen); producer `register_render_target` `exec.rs:456-458`/`1171-1210`; consumer `bind_render_target_as_texture` `exec.rs:1218-1249`; registry `state.rs:44-85` (exact base + containment, never evicts); backend passes `crates/gpu/src/backend.rs:1042-1259` (a SetRenderTarget with no following draw leaves the RT UNDEFINED). One shared host `texture_image` for all scanout (`submit_flip` backend.rs:1604-1615 uses flip index only for R/B swap) — fine for direct-scanout splash.

**NOT implicated:** pipelined-EOP (task-157) does not desync RT read-after-write (ordered behind fences); macro-tiling (all title textures tile=8 LinearAligned, have a detiler). **Separate/minor:** the splash-to-splash "crossfade snap" scenes are direct-scanout with NO RTs and NO dual-CB defer → not this bug; whether we dissolve vs discrete-swap there needs the visual oracle (likely minor).

**FIX direction (NOT implemented):** lift the strict-or-defer limit so a draw can carry BOTH a VS-CB and a PS-CB — two distinct const-storage descriptor slots (keep VS-CB at set0/bind2, add a second binding for PS-CB) threaded through the pipeline layout + `BindConstBuffer`, instead of deferring at `exec.rs:506-513`. Celeste's real scene shaders (VS transform matrix + PS color-grade/lighting) exceed exactly this. This is the doc-6 Entry 10 limitation coming due — likely its own task (executor capability). **Smallest confirming experiment (before the full fix):** env-gated — when both CBs present, bind only the VS CB and DROP the PS CB (instead of deferring the whole draw); if the title background stops being black/garbage (RTs fill with the mountain, even if PS color-grade is wrong) → confirms the defer is the cause. Then PNG/maintainer oracle for pixel-correctness.

**Instrumentation (uncommitted, worktree agent-aa461f7fd09ecf828):** `tools/ps4-gnm-scrape/host/src/bin/rtmap.rs` (offline DCB RT-classification analyzer: per-draw scanout/offscreen class, RT-as-texture detection, tiling defer flag) + Cargo `[[bin]]`. Capture artifacts under `$HOME/.cache/celeste171*`. No runtime code modified. **AC #1 (root-caused) satisfied.**

### The FIX direction landed — two-slot dual-CB implemented (2026-07-19)
The "FIX direction (NOT implemented)" above is now IMPLEMENTED under **task-174** (uncommitted, worktree agent-a1e2f32826dc3ec1c): VS-CB stays set0/bind2, PS-CB moves to set0/bind6, two distinct STORAGE_BUFFER descriptors threaded through recompiler + pipeline layout + `BindConstBuffer`; the `exec.rs` dual-CB defer is removed. build/clippy/test green; headless smoke shows no regression (no Vulkan-validation error on reachable draws). **This task's AC #2 (clean title composite) still needs the maintainer's LIVE eyes** — the dual-CB RT scene is the interactive title/menu, unreachable headlessly (guest loops in intro/asset-load without controller input; ~11 draws, none dual-CB). Validate live once at the title. See task-174 notes for the full file-by-file breakdown + the doc-6-entry recommendation.
### RenderDoc capture root-cause — atlas-splatter is a UV/texcoord defect, NOT position/RT (2026-07-19, opus headless — worktree agent-a6e8ecc3c416f1e60)

Analyzed the maintainer's RenderDoc capture `~/renderdoc_captures/celeste_frame_wrong.rdc` (frame 1713) with the RenderDoc python replay API. NB the Arch `renderdoc` package ships no python bindings and `renderdoccmd` has no `python` subcommand — the working path is `QT_QPA_PLATFORM=offscreen qrenderdoc --python <script.py>` (Vulkan replay needs no display; qrenderdoc embeds the `renderdoc` module). Scripts + PNG dumps live under this worktree's scratchpad.

**Decisive finding: the sprite POSITIONS are correct; the sprite UVs are wrong.** For the representative splatter draw **EID 247 `vkCmdDrawIndexed(930)`** (Colour Pass #4 → RT `ResourceId::126`), and confirmed across the pass:
- **Post-VS `gl_Position` is correct screen NDC.** Guest vertex records are `{posX_px, posY_px, z, packedColor, u, v}` (stride **24 B**). Positions are in PIXEL space (e.g. 1632, 404); the VS applies a pixel→NDC transform (from the bind2 const buffer) and post-VS lands at the right NDC (1632px → NDC ~0.70, matches). Position/transform WORKS.
- **Post-VS UV (Location 1) collapses to full-atlas quad corners `(0,0),(1,0),(0,1),(1,1)` for essentially every sprite** (only the first quad emits a fractional sub-rect). The bound fragment texture is the **full 4096² UI atlas** (`ResourceId::20330`); the PS samples it directly at the interpolated UV with **no** sub-rect transform and **no** PS const block. So every sprite samples the ENTIRE atlas → the "atlas-splatter" (every glyph/heart/gem/button/postcard drawn at once). Verified visually: dumped RT 126 + swapchain 96 to PNG — the on-screen result is the atlas content blitted across the frame over the (correctly-rendered) mountain/stars scene.
- The atlas sub-rect (uvmin/uvmax, e.g. -0.4152..0.5838 / -0.4612..0.537) exists in the bound const buffer but is **never applied to the UV** — the recompiled sprite VS emits `UV_out = pulled_vertex_attribute` verbatim (SPIR-V: `_1178 = (_893,_982,0,0)`, straight from the fetch, no scale-bias/MAD).

**NOT the cause (ruled out with capture evidence):** (1) position/MVP — correct. (2) The vertex-fetch push constant `uniforms23` is the per-stream `{num_records, stride, dst_sel, format}` metadata (decoded: 3 streams, each num_records=1188, **stride=24** — matches the record) — correct, not a mat4. (3) The dual-CB two-slot change (task-174) — the bind2 transform yields correct positions, so it is working; and Colour Pass #1 renders the real scene (mountain/sky/crystals) correctly into the offscreen RT. **The dual-CB fix did its job: it un-deferred the RT-producer draws — the earlier "stale-VRAM splatter" theory above is now SUPERSEDED (the RTs DO render post-fix). Verdict: KEEP the dual-CB change; reverting would re-break scene rendering. The atlas-splatter is a SEPARATE, now-revealed UV bug in the UI sprite pass.**

**Root cause (capture-proven):** the UI sprite VS outputs raw quad-corner texcoords instead of the sprite's atlas sub-rectangle UV, so each sprite samples the whole atlas. **Two candidate mechanisms remain (need one more datum to decide):**
- (A) the recompiler DROPPED the guest VS's UV sub-rect scale-bias (`uv = uvmin + corner·(uvmax−uvmin)` from the const buffer). Against this: the recompiler is faithful (it *errors* on unsupported ops rather than silently dropping), and it emits the position mat4 fine.
- (B) the emulator STAGES the UV vertex stream wrong (base/offset/dst_sel for the texcoord stream), so the fetch pulls a corner/normalized field instead of the real atlas-UV field. Favoured by the logic that a faithful recompiler + correct-on-real-HW guest ⇒ the divergence must be in emulator-supplied DATA (stream/V# staging in `crates/gnm/src/exec.rs`), not the SPIR-V.

**Code locations (on MAIN — see wrinkle below):** GCN→SPIR-V vertex fetch + VS param-export of the texcoord in `crates/gcn/src/recompile.rs` (MUBUF fetch, per-stream stride/dst_sel/format via push constant); provider-side per-stream V#→push-constant staging in `crates/gnm/src/exec.rs`. The PS (`ResourceId::126` pass) samples the atlas at the interpolated UV with no correction, so the fix must land the correct atlas UV in the VS output.

**Needs the maintainer's live eyes / next decisive step:** disambiguate (A) vs (B) with the real-PS4 GNM scrape (task-168/172) for THIS draw — dump the guest VS `.sb` bytecode and the guest vertex streams for the sprite pass; if the real per-vertex texcoord is fractional (atlas sub-rect) → (B), our stream staging mis-routes it; if it is `(0,1)` corners + the guest VS has a sub-rect MAD → (A), recompiler dropped it. Then confirm any fix visually (the title/menu sprite pass is input-gated, unreachable headlessly).

**WRINKLE (important):** this diagnosis worktree (`agent-a6e8ecc3c416f1e60`) is checked out at an OLD ancestor (`5cf0840`, recompile.rs 1905 lines, single vertex buffer) that PREDATES the multi-vertex-stream + dual-CB work. The capture reflects **main's** recompiler (`c075e3a`, recompile.rs 3829 lines, multi-stream bind0/2/3/4). All code line-references above are to **main**, not this worktree's stale checkout. No code was modified in this pass (diagnosis only; env-off path N/A).

### A-vs-B RESOLVED FROM THE CAPTURE — it is (B) data, NOT (A) recompiler (2026-07-19, follow-up, same worktree)

Disambiguated A vs B entirely from the .rdc (no PS4 scrape needed), per coordinator. Corrected an earlier binding-map error: `access.index` is a sequential index, not the SPIR-V binding — the real map for the splatter VS is bind0/bind2/bind3/bind4 = res 37385 / **52713** / 37388 / 37391. **bind2 (res 52713) is a CLEAN pixel→NDC ortho matrix** (`[2/1920,0,0,-1.001][0,-2/1080,0,1.001][0,0,1,0][0,0,0,1]`) — the "garbage 4.8e30 matrix" in the first pass was a misread vertex stream. Position transform is correct and correctly bound.

**Decisive contrast (identical shader, different result):**
- **EID 223 (renders correctly):** VS=`res 245`, PS=`res 246`, FS texture = **4096² atlas `res 20330`**, post-VS UV = **fractional sub-rect** `u[0.259,0.322] v[0.228,0.282]` → correct atlas sprite (a 256×223px element, drawn 9× as an outline).
- **EID 247 (splatter):** VS=`res 245`, PS=`res 246` (SAME shaders), FS texture = **same 4096² atlas `res 20330`**, post-VS UV = **exact quad corners `(0,0),(1,0),(0,1),(1,1)`** for ~154/155 quads → every sprite samples the WHOLE atlas. The visible splatter comes from this draw's LARGE corner-UV quads (e.g. 1500×199 and 775×395 px) each stretching the full atlas across the screen; EID 235/259 share the pattern.
- Both draws use the SAME push-constant vertex format (per-stream `{num_records, stride=24, dst_sel, format}`), so the fetch path is byte-for-byte identical.

**Why it is (B) and not (A):**
1. The recompiled VS's texcoord output is a pure pass-through of the pulled attribute (SPIR-V `_1178 = (_893,_982,0,0)` straight from the fetch — no scale-bias/MAD). It is NOT a MAD-with-wrong-operands; there is no dropped transform.
2. The IDENTICAL shader renders EID 223 correctly when the buffer holds real fractional atlas UVs — so the recompiler + vertex-fetch are proven correct. A recompiler bug (A) would break 223 too.
3. **Within EID 247, sprite 0/9 are fullscreen quads whose UVs ARE fractional atlas coords (-0.415..0.584) and correctly sample an atlas sub-region** — proving the bound atlas (res 20330) is the INTENDED texture for this draw. Therefore the corner-UV sprites in the same draw with the same texture must want atlas sub-rects too → their UVs are wrong DATA, not a wrong texture and not a wrong shader.
4. Raw guest-buffer decode confirms it: all three bound vertex streams (bind0/3/4 = one interleaved buffer, attr offsets 0/12/16, stride 24) contain, for sprites 1+, exact `0.0`/`1.0` corner floats at the UV offset and **no fractional atlas sub-rect anywhere** in the vertex data or the const buffer. Clean 0/1 (not garbage) rules out a stride/offset misread — the buffer genuinely holds corners.

**VERDICT: (A) recompiler is RULED OUT. The defect is (B): the UV vertex DATA fed to the splatter draws is quad-corners `(0,0)-(1,1)` where the guest's real atlas sub-rect UVs should be.** Texture binding is correct (atlas, proven by the fractional-UV quads in the same draw); position/transform is correct; shader is correct.

**Code locus:** `crates/gnm/src/exec.rs` — the per-draw vertex-buffer / V# staging that resolves and binds each vertex stream (base address / stride / format) for these draws, and/or the GNM path that reconstructs the vertex buffer the guest generated. NOT `crates/gcn/src/recompile.rs` (exonerated).

**One residual split the capture cannot make (both inside path (B), both outside the recompiler):** whether the corner-UV buffer is (B1) our executor binding the wrong buffer/base for the UV stream (or repacking it and losing the real UVs), or (B2) the guest itself emitting corner-UV vertices because an UPSTREAM emulation step that should have produced the atlas-UV vertex data (a prior copy/compute/sprite-build) ran wrong. Deciding B1 vs B2 needs an `exec.rs` VBUF trace (log the guest V# base/stride/num_records we bind for EID 247 and verify against the DCB) OR the real-PS4 VBUF scrape (task-172) for this exact draw: if the real PS4 vertex buffer at that address holds fractional atlas UVs → B1 (we bind/stage the wrong bytes); if it holds corners → B2 (upstream guest-vertex-gen divergence). Either way the recompiler is not implicated and the dual-CB change (task-174) is confirmed IRRELEVANT to this artifact (KEEP it — bind2 ortho is correct and Pass#1 scene renders).


### B1-vs-B2 decided: B2 confirmed LIVE (our guest memory holds corner UVs) + real-PS4 diff harness ready (2026-07-19)

Resolved the one residual split above. Two deliverables:

**(A) OUR emulator's UVs, headless (the reference to diff the scrape against).** Added an env-gated VBUF trace to `crates/gnm/src/exec.rs` (`UNEMUPS4_VBUF_TRACE=1`, zero-cost default): for each draw it logs the resolved vertex V# `{base, stride, num_records, dfmt/nfmt/dst_sel}`, the RAW guest bytes at the V# base, and the offset-16 UV (stride 24) of the first four vertices, tagged with the sampled T# base. Because our executor uploads the vertex SSBO VERBATIM from the V# base (no repack), the raw bytes ARE what Vulkan/RenderDoc sees — so corners in guest memory ⇒ B2, not B1. Run: build release, `patchelf --set-interpreter /usr/lib64/ld-linux-x86-64.so.2 --set-rpath /usr/lib target/release/unemups4`, then `timeout 160 env LD_LIBRARY_PATH= UNEMUPS4_VBUF_TRACE=1 RUST_LOG=info ./target/release/unemups4 /home/mikolaj/PS4/CUSA11302/eboot.bin`. Reached flip 4390 (well past the ~1700 broken scene); 154718 trace lines, ~138k of them stride 24.

Smoking-gun line — a slot-0 sprite draw binding the **UI atlas 0x9afc28000** (the known 4096² atlas base), stride 24, Format32_32_32/Float:
```
flip=1 slot=0 tex=0x9afc28000 base=0x9afd53b20 stride=24 num_records=1796 dfmt=Format32_32_32 nfmt=Float dst_sel=[4,5,6,1]
  uv=(0.0000,0.0000)(1.0000,0.0000)(0.0000,1.0000)(1.0000,1.0000)
  raw0=[00 00 52 43  00 00 e0 43  00 00 00 00  d4 d4 d4 d4  00 00 00 00  00 00 00 00]
```
raw0 offset 16..24 = `00 00 00 00 00 00 00 00` → u=0.0, v=0.0 literally in guest memory (posX=210.0, posY=448.0, color=0xd4d4d4d4). The corner pattern `(0,0)(1,0)(0,1)(1,1)` occurs 21654x across stride-24 draws (1574x on the UI atlas specifically). CONTRAST — a correct sprite with fractional sub-rect UVs (different texture 0x98a4a9700), same 24B record:
```
flip=1584 slot=0 tex=0x98a4a9700 base=0x9afd7a030 stride=24 num_records=590 ... uv=(0.0496,0.0659)(0.0645,0.0659)(0.0496,0.0808)(0.0645,0.0808)
  raw0=[00 20 dd 44  00 40 68 44  00 00 00 00  ff ff ff ff  00 00 4b 3d  00 00 87 3d]   (u=0x3d4b0000=0.0496, v=0x3d870000=0.0659)
```
=> **B2 confirmed live: the guest's OWN vertex memory holds quad-corner UVs for the splatter sprites** (the executor stages them faithfully; it is NOT binding the wrong buffer/base — B1 ruled out). The recompiler stays exonerated; task-174 dual-CB stays IRRELEVANT to this artifact.

**(B) Real-PS4 UV decoder for the scrape (proves whether real HW diverges from us).** New host bin `tools/ps4-gnm-scrape/host/src/bin/uvdump.rs` (built + clippy green). It reads a `receiver` dump dir, scans every DCB for V#s to build `base -> {(stride,num_records)}` (same `decode_v_sharp` window scan as `vref`), then for each KIND_VBUF content file whose correlated stride is 24 decodes the offset-16 UV of the first ~8 vertices and flags CORNERS (0/1) vs FRACTIONAL. Falls back to an assumed stride 24 (`24?`) when a VBUF base has no correlated V# but its length divides by 24. Run after scraping the same auto-reached scene (no input needed; frame window ~1600-1800):
```
cd tools/ps4-gnm-scrape/host && cargo run --bin uvdump -- <dump_dir>     # e.g. ./dumps ; add --all to also list non-24 buffers
```

**Final-diff recipe:** run `uvdump` on the real-PS4 scrape of the same sprite draw — **FRACTIONAL there => managed-runtime (Mono) divergence confirmed** (real Celeste emits atlas sub-rects where our guest emitted corners; the bug is upstream in our vertex generation / managed execution); **CORNERS there => NOT a divergence** (real Celeste emits corners too, so the artifact's cause is elsewhere in the render path, not our sprite-UV data).

### REAL-PS4 ORACLE DECIDES IT: real HW binds the SAME 4096² atlas but with FRACTIONAL UVs — the splatter is a guest-side UV-DATA divergence, not a texture/RT/scissor bug (2026-07-19, opus, worktree agent-ad008ae1e345e29d5)

Decoded the permanent real-PS4 scrape (`/home/mikolaj/celeste-scrape-oracle/`, 2112 flip DCBs + KIND_VBUF content) for the auto-reached title scene (frames 1700/1713) with a new throwaway host bin `tools/ps4-gnm-scrape/host/src/bin/spritetex.rs` (per-draw texture T# + all inline V# candidates; built on MAIN, clippy-clean) plus offline python quad classifiers. The scrape captures DCBs + vertex/const KIND_VBUF buffers (NOT index buffers).

**Frame structure (real HW):** one flip DCB per frame, all draws in it. Frame 1713 has 23 draws; draws 17–22 are the UI/atlas sprite passes. Every draw's only *inline* VS-user-data V# (slot4, stride16, nrec4) is the **MVP ortho matrix const-buffer** (decodes as a bogus V#, ignore it); the real vertex stream is a V# **table** pointed to by VS user-data **slot2** → an interleaved stride-24 sprite record with three attributes **pos@0, color@12, uv@16** — byte-for-byte the SAME layout our executor fetches (`uv` at offset 16, stride 24). Confirmed on draw20's table (0x20f563c9c): attr bases 0x2bd152800/…80c/…810 = offsets 0/12/16.

**THE decisive same-draw comparison (both are frame 1713, `vkCmdDrawIndexed(930)` = 155 quads, bound to the 4096² atlas):**
- **Real HW draw20** binds atlas T# base **0x2978a9700, 4096×4096** (spritetex). Its vertex buffer **0x2bd152800** (500 quads) laid out in draw/append order: **quads 0..154 = ALL FRACTIONAL** sub-rect atlas UVs (e.g. v0 pos (191,159) uv (0.259,0.228), a 256×223 UI sprite; col 0xff……). The draw's 930 indices draw exactly that first contiguous 155-quad run → correct atlas sprites, **no splatter**. The buffer's later quads: a block of **zero-color (col=0) corner-UV quads** (invisible) at 155..203, and **BIG fullscreen corner-UV quads** (960×540 / 1920×1080, col=0xffffffff) at 288..292 — those are appended by *other* draws whose index range binds a **1920×1080 / 960×540 RT** (tiling 14), i.e. correct fullscreen composites where corner UVs (0..1) sample the whole RT.
- **OUR draw** = the maintainer's RenderDoc EID247 (frame 1713, drawIndexed 930): binds the **SAME 4096² atlas** (res 20330) but post-VS UVs are **quad corners (0,0)(1,0)(0,1)(1,1)** for ~154/155 quads, including LARGE corner quads (1500×199, 775×395) → each stretches the whole atlas across the screen = the reported splatter. (Headless can't re-reach the title — input-gated — so EID247 stays the authoritative OUR-side title datum; our headless dump only reaches intro/loading, whose flip counter does not align with the real-HW frame counter.)

**VERDICT — the task's leading hypothesis is FALSE, and the alternative is now pinned:**
1. **Texture binding is CORRECT on both sides** — real HW binds the 4096² atlas for this corner/sprite draw, exactly like us. So it is NOT "real HW binds a small per-sprite texture where we bind the atlas." The "small-texture-vs-atlas" T#/identity/cache theory is ruled out.
2. **Vertex attribute layout & fetch are CORRECT** — UV @ offset 16, stride 24, identical on real HW and in our executor; we read the right bytes.
3. **Recompiler stays exonerated** (prior passes; the identical shader renders fractional-UV quads correctly).
4. **The divergence is the UV VERTEX DATA itself:** for the identical logical atlas draw, real HW's vertex buffer holds **fractional atlas sub-rects**, ours holds **whole-texture (0,1) corners**. Our upload is verbatim (B2), so the corners are literally in our guest's memory. This is the task's decision-tree branch "real HW ALSO binds the atlas ⇒ the difference is elsewhere" — and "elsewhere" is now *specifically the guest-produced UV field*, NOT RT target / scissor / viewport / blend (the real HW atlas draw carries fractional UVs in the same buffer field with no special masking — it just samples sub-rects).

**ROOT CAUSE (as pinned as the scrape allows): a GUEST-SIDE / managed-runtime (CPU) divergence in the per-sprite atlas source-rect → UV computation.** Celeste/FNA3D's SpriteBatch bakes `uv = sourceRect / textureSize` into each vertex; real Celeste bakes fractional atlas sub-rects, our guest bakes whole-texture (0..1) corners — i.e. our guest is computing `sourceRect = whole texture` (the FNA3D "sourceRectangle == null / default" path) where real Celeste supplies the atlas glyph rect. This is upstream of the GPU entirely — an x86jit / Mono-JIT / FNA3D-HLE / atlas-metadata (Sprites/packed-atlas lookup) execution divergence — **NOT a GNM/GPU/render-target/texture bug.** task-171's RT/post-processing framing (and the earlier "corner UVs are faithful" read from coarse whole-buffer uvdump counts) is **superseded**: the coarse uvdump counted whole buffers as corner/fractional, but per-*quad* the real atlas sprites are fractional and only the invisible (col=0) and RT-composite (corner-correct) quads are corners.

**FIX DIRECTION:** move this off task-171 (a GPU/RT task) to a **guest-execution / managed-runtime** investigation. Next decisive step is guest-side, not GPU: trace the FNA3D `SpriteBatch` UV math or the atlas source-rect lookup in the guest (where does `sourceRect` become the whole texture?) — e.g. compare the guest's computed source rectangles / atlas metadata to the real ones, or diff the CPU execution around the sprite-build for a known glyph. The GPU crates need no change for this artifact; keep task-174 dual-CB (it correctly un-defers the RT producers and is irrelevant to this UV artifact).

**Residual honesty:** the one thing the scrape alone cannot fully close is whether every one of our EID247 corner quads is our *guest* writing corners (B2) vs our emulator mis-indexing a shared dynamic buffer so RT-composite (corner) quads leak into the atlas draw's range (would need OUR title-scene index buffer, input-gated). The weight of evidence favors guest-data divergence: verbatim upload + byte-identical attribute layout + real HW's atlas draw range being cleanly all-fractional while ours is corners. **Throwaway artifact:** `tools/ps4-gnm-scrape/host/src/bin/spritetex.rs` + its `[[bin]]` in that Cargo.toml (uncommitted, on MAIN; safe to delete). No runtime/GPU code changed.


### TIEBREAKER (2026-07-19, opus) — (A) guest-side UV/geometry divergence CONFIRMED; (B) "emulator RT-as-texture binding bug" REFUTED; task-171 (GPU/RT) stays EXONERATED for this artifact
Resolved the disagreement between task-178 (A: guest-side) and a challenger hypothesis (B: our RT-as-texture lookup fails for tiling-14 RTs → obscuring composite quads fall to the atlas). Completed the decisive **(draw, bound texture base+dims+tiling, per-quad UVs)** comparison ours-vs-real-HW that neither prior pass finished. Full evidence in task-178 Notes; summary for the GPU/RT angle here:

- **Our RT-as-texture path WORKS.** In the RenderDoc capture (frame 1713), the fullscreen composites EID187/199/211 correctly sample RTs 37568/37754/37811 — i.e. `RenderTargetRegistry::lookup` (state.rs, exact-base+containment) and Offscreen classification (derive.rs) succeed for these RTs. So (B)'s premise — that a tiling-14 RGBA8 RT lookup MISSES and the quad falls to the plain-texture/atlas path (exec.rs plain-texture bind) — does not occur.
- **Real HW's tiling-14 RTs are exactly the ones (B) named, and we handle them.** Real-HW frame-1713 T# decode: 0x2bd4e0000 1920×1080 tiling-14 (scene RT) + 0x2be818000/0x2bea58000 960×540 tiling-14 (bloom) map 1:1 to our RTs 37568/37754. (B) correctly spotted these RTs but wrongly inferred the OBSCURING quads sample them; they are sampled by the separate fullscreen composites we already render correctly.
- **The obscuring draw binds the atlas on BOTH sides.** Our EID247 (drawIndexed 930) binds res 20330 = 4096² atlas; real HW's same-signature draw binds atlas 0x2978a9700 = 4096² tiling-8. Same texture. Real HW's atlas draw carries FRACTIONAL sub-rect UVs (e.g. 256×223 sprite @ uv[0.259,0.322]); ours carries whole-texture CORNER UVs (span 1.0) for 151/155 quads, incl. large obscurers 1500×199 and 775×395 that **have no real-HW counterpart at all**. ⇒ decision tree: OURS atlas + real-HW atlas(fractional) = **(A)**.

**Verdict: (A). The atlas-splatter is a guest-side / managed-runtime (Mono-AOT / x86jit / FNA3D-HLE) execution divergence in the per-sprite vertex build for specific UI batches — the emulator faithfully renders wrong guest data. There is NO emulator GPU/GNM/RT/texture-binding/scissor defect for this artifact.** task-171's RT/post-processing framing is fully superseded (the dual-CB fix, task-174, correctly un-defers the RT producers and is irrelevant to this UV artifact — KEEP it). Follow-up belongs to the guest-execution investigation (task-178), not here. No runtime/GPU code changed in this pass.

<!-- SECTION:NOTES:END -->

### Closed 2026-07-20 — resolved by the task-178 fix
The "atlas-blit + garbage band + flicker" on the Celeste title screen was the
frame-alternating garbage root-caused in task-178: the resource cache served STALE
bytes for the MonoGame dynamic vertex ring + per-frame projection const buffers
(dirty-tracking under-reported the guest rewrites; see task-178 for the full
writeup and the x86jit follow-up, x86jit task-273). With that fix (single dirty
drain + force-re-upload of dynamic copy buffers) the title screen renders clean —
confirmed live by the maintainer. No separate render-target / post-processing work
was needed.
