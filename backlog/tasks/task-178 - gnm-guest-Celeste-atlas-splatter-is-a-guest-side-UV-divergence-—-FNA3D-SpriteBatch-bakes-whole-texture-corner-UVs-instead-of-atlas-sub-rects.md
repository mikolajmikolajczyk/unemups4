---
id: TASK-178
title: >-
  gnm/gpu: Celeste title-screen frame-alternating garbage — double-buffered
  PRESENT/DETILE (UV/base-vertex/geometry all refuted by oracle)
status: Done
assignee: []
created_date: '2026-07-19 12:32'
updated_date: '2026-07-19 22:09'
labels:
  - celeste
  - guest
  - mono
  - fna3d
  - retail
dependencies: []
priority: high
ordinal: 182000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Real-PS4 GNM oracle (3000-flip scrape at ~/celeste-scrape-oracle, decoded with the uvdump host bin) proved the atlas-splatter is NOT a GPU/GNM/RT/texture/recompiler bug: for the SAME UI sprite draw (frame 1713 drawIndexed(930), bound to the 4096x4096 UI atlas), real HW's vertex buffer holds ALL FRACTIONAL atlas sub-rect UVs (correct sprites) while our guest's byte-identical-layout vertex buffer holds whole-texture CORNER UVs (0,0)-(1,1) -> each quad stretches the whole atlas = splatter. Texture binding (same atlas), vertex attribute layout/fetch (uv@offset16 stride24), verbatim SSBO upload, and the recompiler are all CONFIRMED correct on both sides. The corner UVs are literally in our guest's memory (B2). Root cause is upstream of the GPU: a managed-runtime (CPU) divergence in the per-sprite atlas source-rect -> UV computation. FNA3D SpriteBatch bakes uv = sourceRect / textureSize; real Celeste supplies each glyph's atlas sub-rect, our guest computes sourceRect = whole texture (the FNA3D sourceRectangle==null / default path). Investigate the guest-side sprite source-rect lookup / SpriteBatch UV math / atlas-metadata parse (Celeste Atlas data) under Mono-JIT + x86jit + FNA3D-HLE for a known glyph. NOT task-171 (GPU/RT, now exonerated); task-174 dual-CB is correct and irrelevant to this artifact. Splatter is cosmetic-but-large, does not block playability (Celeste is playable with pad).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 The specific guest value/path that yields sourceRect=whole-texture (vs the real atlas sub-rect) for the UI sprite batch is identified with evidence (trace of the guest source-rect lookup / SpriteBatch UV math / atlas-metadata parse for a known glyph)
- [ ] #2 Root cause is pinned to a concrete mechanism (atlas-metadata decode, an FNA3D-HLE value, a texture-dimension the game divides by, or a Mono/x86jit execution divergence) with a fix
- [ ] #3 Fix makes the UI sprite draw emit fractional atlas sub-rect UVs matching real HW; the atlas-splatter is gone — live/PNG oracle; build+test+clippy clean
<!-- AC:END -->

## Notes

### Investigation verdict (2026-07-19) — FS/asset + atlas-parse EXONERATED; it's a draw-time managed (Mono-AOT/x86jit) execution divergence
**Premise stands (do NOT re-question): the splatter is REAL and PRESENT on main HEAD** — maintainer's RenderDoc capture (frame 1713) + live run confirm it, ~frame 1700, auto-reached. (One agent wrongly concluded "resolved by task-171/174" by misclassifying the small-sprite splatter draws as fullscreen RT composites — that verdict is REJECTED.)

Narrowed with runtime evidence, corner UVs (0,0)-(1,1) trace to a **whole-texture parent `MTexture` source-rect authored at draw time**, NOT wrong data:
- **The `.meta` is byte-perfect over our FS** (`UNEMUPS4_META_TRACE=1`, crates/kernel/src/fs.rs): guest reads `Gui.meta` sequentially, 15751 bytes = exact file size, byte-identical to the real file; both `Gui0.data`/`Gui1.data` sources resolve → the 403-entry sub-texture table was consumed with correct byte alignment (a desync would ENOENT a garbage path).
- **The parse decodes clean** (`Monocle.Atlas.ReadAtlasData`, format 5: `{name; i16 x,y,w,h,offX,offY,realW,realH}`; ClipRect=(x,y,w,h)). Host oracle `scripts/decode_meta.py` decodes to offset==filelen, all ClipRects fractional over 4096 (logo=(2312,2002,1116,898), title=(2047,1719,1457,282), dot=(20,0,14,14)) — none whole-atlas. `Atlas.get_Item` throws on miss (no whole-texture fallback), so it isn't a failed lookup.
- Clean corner UVs (exactly the parent rect, not garbage >1) rule out a byte-desynced parse or a wrong 16-bit load → **the ClipRect values are correct; the divergence is that the draw uses the whole-texture PARENT rect instead of the child sub-rect.**

**So it is NOT an emulator FS/asset/parse bug.** It's a managed Mono-AOT-on-x86jit execution divergence in the per-sprite draw path (the child `MTexture.ClipRect` read yields the parent's whole-atlas rect, or the wrong MTexture is drawn). **AC #1 met.** AC #2/#3 open.

**Next probe (splits wrong-value-read vs wrong-object; then confirms x86jit vs managed):** `UNEMUPS4_VBUF_TRACE=1` at frame-1713 `drawIndexed(930)`, cross-ref per-quad UVs vs the `decode_meta.py` ClipRects; hook the guest `SpriteBatch.Draw sourceRectangle` / `MTexture.ClipRect` getter callsite (or a targeted x86jit trace of it). If a specific x86-64 instruction/lift is shown to diverge (differential vs Unicorn) → file to the **x86jit backlog** (do not edit x86jit here). Cosmetic — does NOT block playability (Celeste is playable with the pad).

**Diagnostics landed for this (env-gated, zero-cost):** `UNEMUPS4_META_TRACE` (fs.rs), `scripts/decode_meta.py` (host .meta oracle), `UNEMUPS4_VBUF_TRACE` (already on main). Related: task-171/174 (GPU, done + exonerated for this artifact).

### RenderDoc pass-graph analysis (2026-07-19) — compositing EXONERATED; NOT cosmetic; sharper signature
Full frame-1713 pass graph decoded from the capture (24 draws):
- Scene renders into offscreen RT `37568` (1920×1088 UNORM) — correct night-mountain. Bloom chain 37568→37722→37754.
- Videoout composite into RT `126` (1920×1080 SRGB, the presented target): **EID 187 samples RT 37568 (the SCENE), fullscreen, One/InvSrcAlpha → the good scene IS composited in correctly**; bloom composited; **EID 247 samples the 4096² atlas (img 20330), 155 UI sprite quads**; present EID 285 blits RT 126 → Swapchain 96.
- **The compositing DAG is CORRECT and the good scene reaches the final image.** No RT-alias, no wrong-texture composite, no mis-selected flip buffer. The hypothesis "a composite pass binds the atlas where it should bind the scene RT" is DISPROVEN by the capture. This also closes task-171's residual question (EID 247 is a single atlas draw; the composite quads 187/199 are separate draws sampling RT 37568 — no emulator dynamic-buffer index leak). **There is NO emulator-side GPU/compositing/texture-binding bug** — the emulator faithfully renders wrong guest data.
- **Obscurer = EID 247:** 155 sprite quads each with UV span uSpan=vSpan=1.0 (whole atlas), scattered across the screen, **2 of them FULLSCREEN [-1,1]²** at α up to 1.0, non-square quads stretch the atlas → skewed. This paints whole-atlas copies (flying + skewed) over the scene = the maintainer's live observation.

**Correction to earlier notes: NOT merely cosmetic — the two fullscreen whole-atlas quads genuinely COVER the scene → Celeste is not visually playable. Priority bumped to high.**

**Sharper signature for AC#2 (supersedes "whole-texture parent rect"):** the wrong UVs are not literally (0,0)-(1,1); the invariant is **sourceRect.Width == texture.Width AND sourceRect.Height == texture.Height** (span 1.0), with the per-sprite OFFSET (x,y) varying and CORRECT. So the guest gets each glyph's sourceRectangle OFFSET right but its WIDTH/HEIGHT equal to the full texture dimensions. Chase where `MTexture.ClipRect.Width/Height` (or the SpriteBatch `sourceRectangle` W/H) becomes the atlas dimensions instead of the glyph's — a draw-time managed (Mono-AOT/x86jit/FNA3D) divergence; no GPU/recompiler change fixes it and faking UVs emulator-side is wrong HLE. Decisive probe: hook the guest `MTexture.ClipRect` getter / `SpriteBatch.Draw` sourceRect at the frame-1713 drawIndexed(930) callsite and read the actual W/H, then trace where ClipRect.W/H is set (expected glyph size vs observed texture size); if a specific x86-64 lift diverges, file to x86jit backlog with a minimal repro.

### TIEBREAKER (2026-07-19, opus) — (A) CONFIRMED, (B) emulator-RT-binding hypothesis REFUTED on every point; decisive same-draw tuple ours-vs-real-HW completed end-to-end
A later reviewer challenged (A) with hypothesis (B): that the obscuring LARGE/fullscreen quads sample an OFFSCREEN SCENE RT (real bases 0x2bd4e0000-class, tiling-14) on real HW, and that OUR RT-as-texture exact-base match (state.rs `lookup`) / Offscreen classification fails for a tiling-14 RGBA8 RT → the composite quad falls to the plain-texture path and binds the atlas. This tiebreaker completes the one check neither prior pass finished: the **(draw, bound texture base+dims+tiling, UVs of ALL 4 verts of the specific obscuring quads)** tuple on BOTH sides.

**OUR side (RenderDoc `celeste_frame_wrong.rdc`, frame 1713):**
- Frame pass graph (24 draws): scene→RT 37568 (1920×1088); bloom→37722/37754; composites→RT 126: **EID187 samples RT 37568 (scene), EID199 samples 37754 (bloom), EID211 samples 37811** — our RT-as-texture path WORKS (these correctly bind RTs, not the atlas); sprite passes EID223/235/247/259/271 bind plain content textures; present EID285→swapchain 96.
- Obscurer **EID247 = drawIndexed(930) = 155 quads, bound sampled texture = res 20330 = 4096×4096 atlas** (single texture for the whole draw). Full per-quad decode: **151/155 quads are whole-texture CORNER UV (span 1.0)**, only 4 are FRAC (recurring fullscreen composites q0/9/63/117, uv≈[-0.415,0.584] span 0.999). The LARGE obscurers are genuine corner-UV quads: **q10/q64/q118 = 1500×199 px** at x[209,1709]y[447,646], **q11/q65/q119 = 775×395 px** at x[572,1347]y[351,746], all uv[0,1]×[0,1]. EID235 binds res 52497 (2048²) and EID259 binds res 152 (134×126) — each with the SAME large corner quads → the obscurers span multiple sprite batches, each stretching its own bound plain texture, none of them an RT.
- ⇒ Directly REFUTES (B)'s "first-4-verts" methodology critique: the corner-UV finding is 151/155 quads decoded per-quad, NOT the leading fullscreen composite (which are the 4 FRAC quads).

**REAL HW (oracle `/home/mikolaj/celeste-scrape-oracle/frame001713_*`, decoded via host `decode` T# + stride-24 vertex scan):** real-HW frame-1713 PS textures decode to a set that maps 1:1 to ours — **atlas 0x2978a9700 4096² tiling-8** (=our res 20330), 0x2b020d100 2048² tiling-8 (=52497), 0x292cce900 134×126 tiling-8 (=152), 0x2924c0000 1922×1082 tiling-8 (=179), and the tiling-14 RTs **0x2bd4e0000 1920×1080** + **0x2be818000/0x2bea58000 960×540** (=our RTs 37568/37754). Scanning EVERY real-HW frame-1713 vertex buffer for large quads: **every corner-UV quad on real HW is FULLSCREEN (960×540 or 1920×1080, col ffffffff/33333333) and belongs to the RT composites; there is NO 1500×199 or 775×395 corner quad anywhere.** Real HW's large quads are all FRACTIONAL (799×670, 762×757, 1796×703, 256×223 sprite @ uv[0.259,0.322]). The atlas draw on real HW is all-fractional sub-rects.

**DECISION-TREE OUTCOME:** OURS binds the atlas (res 20330, 4096²) for EID247 AND **real HW ALSO binds the atlas (0x2978a9700, 4096², tiling-8) for the same-signature draw, with FRACTIONAL UVs** ⇒ **(A) CONFIRMED**. (B) is refuted point-by-point: (1) real HW binds the SAME atlas we do for the sprite draw — not an RT; (2) our RT-as-texture binding SUCCEEDS (EID187 samples RT 37568 = real HW's 0x2bd4e0000 tiling-14 RT), so the alleged tiling-14 lookup-miss does not occur; (3) (B) correctly identified 0x2bd4e0000 tiling-14 as a real RT, but that RT is corner-sampled by the **separate fullscreen scene composite** (which we replicate correctly), NOT by the obscuring quads; (4) the obscuring 1500×199/775×395 corner quads DO NOT EXIST on real HW — they are guest-generated geometry unique to our run.

**SHARPER SIGNATURE (refines AC#2 — divergence is broader than UV width/height):** not only are the atlas-draw UVs whole-texture corners (span 1.0), the obscuring quads' POSITIONS/SIZES also diverge — the 1500×199 and 775×395 quads have no real-HW counterpart (real HW's atlas sprites are ~256×223, fractional). And the divergence is SELECTIVE: our EID223 (the CELESTE logo, 256×223 @ uv[0.259,0.322]) renders CORRECTLY, byte-matching real HW's logo quad, while EID247/235/259 (menu/UI atlas batches) are corrupt. So a subset of SpriteBatch entries (whole vertex records: pos+size+uv) are wrong while others are right — pointing at a managed-runtime (Mono-AOT / x86jit / FNA3D-HLE) execution divergence in the sprite-build for specific UI batches, not a uniform UV-scale drop. Next guest-side step unchanged: instrument/diff the SpriteBatch build (or x86jit lift) for a corrupt EID247 sprite vs the correct EID223 logo sprite. **Emulator GPU/GNM/RT/texture-binding is correct; no emulator fix applies. No runtime code changed in this pass** (throwaway host `decode`/scan + RenderDoc scripts only).


### x86jit differential pass (2026-07-19, opus/agent) — REPRODUCED on HEAD under BOTH backends; localized to shared-LIFT-or-input (NOT cranelift codegen); exact instruction NOT yet pinned (needs a store-watchpoint chain)

**Setup.** Worktree synced to main a88c27c. Built + patchelf'd; ran Celeste headless. Added a throwaway env-gated flip-window filter to `dump_vbuf_probe` (`UNEMUPS4_DUMP_VBUF_MIN`/`_MAX`, inclusive) so a run reaching the frame-1713 scene dumps ONLY that window (crates/gnm/src/exec.rs, uncommitted, zero-cost off). x86jit worktree at `~/src/x86jit-celeste-fix` branch `celeste-fix-task178` off the pinned rev **942d253** (what unemups4's Cargo pins).

**REPRODUCED on HEAD (JIT).** `UNEMUPS4_DUMP_VBUF=... MIN=1710 MAX=1716` → the vs12=0x6f8/ps12=0x6f7 UI-sprite draws carry mid-size **whole-texture (u[0,1] v[0,1]) quads** (e.g. 523x138, 817x305, 422x152, 1038x615) that do NOT exist on real HW. Decoded oracle `frame001713_*` (stride-24): every real-HW full-span quad is either a fullscreen RT composite (960x540 / 1920x1080) or a <=24px solid dot; the CELESTE logo is the correct fractional (191,159) 256x223 uv[0.259,0.322]. Real HW has ZERO mid-size whole-texture atlas quads. Premise reconfirmed live.

**Per-sprite SELECTIVITY confirmed at the vertex level.** One corrupt batch (flip1710 draw10, 151 quads) decodes as a quad-class sequence `fff` + ~99 `o`(ok-fractional) + a CONTIGUOUS block of **26 `C`(corrupt whole-texture)** + ~21 `o`. So within ONE SpriteBatch the per-sprite vertex-fill runs 148x and is CORRECT for 122 sprites — the fill code is NOT the bug. The 26 corrupt sprites are a **contiguous run = one game object / draw group**, and their WHOLE vertex records are wrong: UV is bit-exact (0,0)-(1,1) AND the four positions form a thin/skewed (not axis-aligned) quad → pos+uv both diverge. Bit-exact 0/1 UV on a pow2 (4096) atlas = sourceRect == whole texture (parent MTexture rect) instead of the child glyph sub-rect. Divergence is UPSTREAM of the fill, in that object's per-sprite source-rect setup.

**MonoGame vertex-fill decompiled** (MonoGame.Framework.dll via monodis): span-exactly-1.0 UV is produced ONLY by the `sourceRectangle == null` path (constants Vector2.Zero/One) OR by a non-null sourceRect whose W==texW,H==texH on a pow2 texture (texW*(1/texW)=1.0 exact). UV math = SSE scalar mul by a precomputed reciprocal; the rotation Set overload adds Math.Sin/Cos (double, libm). So the corrupt sprites either pass sourceRectangle==null, or their ClipRect == the whole atlas.

**KEY LOCALIZATION — JIT vs INTERPRETER.** `UNEMUPS4_BACKEND=interp` reaches flip 1710 (~2.8 flips/s, ~10 min) and **REPRODUCES the identical corruption class** — interp flip1716 shows the exact **1500x199 and 775x395** whole-atlas quads from the maintainer's RenderDoc capture. Because x86jit's interpreter and cranelift JIT both consume the SAME lifted IR, JIT==interp rigorously means the fault is in the **shared LIFT stage OR in the INPUT** to the guest code — it is **NOT** a cranelift-codegen-only bug and NOT an interp-execution-only bug. (Exact corrupt quad SIZES differ jit-vs-interp — jit: 523x138…, interp: 1500x199… — because the attract-scene animation is at a slightly different phase per backend; the corruption CLASS is deterministic, the specific geometry tracks scene timing.)

**What this leaves (honest):** the bug is real, deterministic, and backend-independent, but the evidence does NOT (yet) reduce it to a single divergent instruction. Two live possibilities remain, distinguishable only by running the offending guest block under a reference CPU (x86jit interp-vs-Native/Unicorn): (a) a shared x86jit **LIFT** bug on some instruction that only this object's data-path hits (siblings take a different path), or (b) a deterministic managed/HLE **input** divergence that the guest then computes faithfully. Cannot claim (a) over (b) without the block capture. NOTE: libunicorn is MISSING via pkg-config on this box; the x86jit differential oracle here is **NativeOracle** (fork + real host CPU, `x86jit-tests` builds clean without the `unicorn` feature) — the only oracle that also validates VEX/AVX.

**Concrete next step to pin it (documented, not yet done — exceeds one session):** the corrupt vs6f8 vertex buffers at flip 1710-1716 all live in the reused dynamic-buffer ring **0x9afd50000–0x9afd7a030** (stable across frames; the corrupt nr=606 draw base 0x9afd79eb0 binds tex 0x9b00e0000). Add an env-gated store-watchpoint to x86jit's `Memory::write` (memory.rs:951) / interp store path stamping the current guest RIP, keyed on that address range, run interp to 1710 → the store RIP is the FNA3D SetData memcpy; its source register gives the managed `_vertexArray`; iterate the watchpoint up the chain (_vertexArray ← SpriteBatchItem[] ← Set ← the per-object source-rect setup) to the guest RIP that first writes the whole-texture rect. Capture that block's bytes + register/memory state → `VectorInput`/`Vector::asm` → `compare(NativeOracle, InterpreterOracle)` (x86jit-tests) decides lift-bug (a) vs input (b) and, if (a), names the instruction.

**Artifacts (unemups4 worktree, uncommitted):** `scratch/analyze_dump.py` (quad classifier), `scratch/dump_jit/` + `scratch/dump_interp/` (frame-1710-1716 vertex dumps, both backends), the `dump_vbuf_probe` flip-window filter. **No emulator behavior changed** (diagnostic-only). x86jit worktree `~/src/x86jit-celeste-fix` (branch celeste-fix-task178) ready but UNMODIFIED. AC#1 reconfirmed live + AC#2 narrowed (shared-lift-or-input, not codegen); AC#2 concrete-instruction + AC#3 fix still open.

### === CURRENT STATE / NEXT SESSION START HERE (2026-07-19) — timing lead, maintainer's eyes are ground truth ===

**PROCESS CORRECTION (critical): the maintainer's LIVE eyes are the oracle; headless agent conclusions that contradict them are WRONG.** Multiple agents this session declared the splatter "resolved / transient / cosmetic / title renders clean" — ALL contradicted by the maintainer's live run and REJECTED. Per [[playable-needs-visual-oracle]]. Do NOT relay "it's fixed/gone/transient" unless the maintainer's eyes confirm.

**What is SOLID (evidence-backed):**
- The atlas-splatter is GUEST-side; the emulator (GPU/RT/compositing/FS) faithfully renders wrong guest vertex data (proven 3× independently: compositing DAG correct, RT-as-texture works, real HW binds correctly).
- **x86jit is RULED OUT — airtight.** NativeOracle differential on a real host CPU (Ryzen 7 7840HS, AVX2): 228,729 vector + 114,073 scalar-arith ops replayed, ZERO divergences; JIT==interp. NOT a lift/codegen bug. The verdict is INPUT: the guest is FED wrong data and computes faithfully.
- The corrupt sprites sample a **1500×199 texture WHOLE** (texel 1/1500 at guest RIP 0x203adc3 `mulss xmm0,[r15+0x64]`, descriptor 0x9c59bc530+0x60/64) instead of an animated SLICE. Tex base **0x9afc28000 = the "Matt Makes Games Inc. presents" intro text card** (DUMP_TEX content), NOT the Gui atlas. **Prior work MISIDENTIFIED this base as the 4096² Gui atlas (decode.rs `ATLAS_BASE_HEX`) — that identification is WRONG.** It's Celeste's TEXT-REVEAL effect drawing the whole text instead of animated slices → smear.

**MAINTAINER'S LIVE GROUND TRUTH (authoritative, overrides agent "transient" claims):** nothing renders cleanly persistently; the 2D-mountain splash appears correctly only ONCE IN A WHILE, and BEFORE it appears everything is smeared/broken. **Maintainer's hypothesis: a TIMING problem (again).** They have been right about timing repeatedly (task-169 clock delta-cap, task-170 intro-loop). Take timing as the LEAD.

**TIMING LEAD:** the reveal's per-sprite sourceRect degenerates to the FULL texture (span 1.0) instead of an animated band/slice because the reveal's animation-progress `t` (= f(time/delta)) is wrong/unstable — most frames smeared, occasionally correct = unstable progress. Likely a wrong time/delta INPUT the reveal reads (clock.rs consumers, sceKernelGetProcessTimeCounter / clock_gettime), possibly a RENDER-phase residual of task-169, OR an FNA3D sourceRectangle-null default when the band count/height computes to 0/full.

**NON-BLIND METHOD (do NOT work blind / do NOT trust agent guesses — anchor to ground truth):**
- **Real-PS4 GNM oracle = the reference:** `~/celeste-scrape-oracle/` (3000 contiguous flips of real Celeste, DCB + KIND_VBUF content, gitignored). Flip-by-flip diff OUR per-flip state (animation-buffer content, draw signature, the reveal slice dims) vs real HW's SAME flip → the timing/animation-progress divergence shows directly. If our reveal is at the wrong progress at a given logical flip vs real HW → timing confirmed with a hard reference.
- `UNEMUPS4_CLOCKLOG` (clock.rs) — what time values the guest reads. `UNEMUPS4_VBUF_TRACE` + `_MIN/_MAX` window — the reveal draws. `UNEMUPS4_DUMP_TEX` — texture identity. `UNEMUPS4_META_TRACE` — asset reads.
- Decompiled Celeste.exe / MonoGame IL (monodis) — the reveal-effect logic (how the slice rect is computed from `t`).
- Pinpoint hooks handed forward: compute instr 0x203adc3; per-texture descriptor 0x9c59bc530 (+0x60/64 = texel); slice-dim integers (199/1500) in rax/rdi — store-watchpoint back to where the slice dim is set full-vs-band, and correlate to the time/delta input that drives it.

**Next step:** ground-truth flip-by-flip timing diff (ours vs the real-PS4 oracle) + the guest time-read log for the reveal frames, to pin WHERE our animation-progress diverges from real HW. Do it transparently (show the numbers), not as a black-box agent verdict.

### === NEXT SESSION — START HERE: TOP-DOWN via ILSpy (the plan the maintainer approved 2026-07-19) ===
Stop reverse-engineering from GPU output (bottom-up = slow, many probes). For a managed-runtime game go **TOP-DOWN: read the game's own code.**

**Concrete plan (do this first next session):**
1. Get a READABLE C# decompile of Celeste (not raw monodis IL): `ilspycmd` / ILSpy on `/home/mikolaj/PS4/CUSA11302/Celeste.exe` (+ MonoGame.Framework.dll). Install via `dotnet tool install -g ilspycmd` or `nix`. Output C# source.
2. Find the **studio-intro / text-reveal effect** (the "Matt Makes Games Inc. presents" card, tex 0x9afc28000 = 1500x199, and the "a game by …" credits card 0x9afae7100 = 775x395). It's a glitch/strip reveal. Read the class that draws it — how it computes the **number of horizontal strips / the per-strip sourceRectangle** and WHICH INPUT drives it (a reveal progress `t`, a glitch amount, a time/delta, a random seed, or an `Ease`/timer).
3. That input is what our emulator supplies wrong. Check it: is it time/delta (→ clock, relates task-169), a random seed, or a value from an HLE we stub? Fix the input emulator-side; the strips reappear.
4. Cross-check with MonoGame source (github.com/MonoGame/MonoGame) for the SpriteBatch/source-rect semantics the reveal relies on.

**Why this bug, precisely (established, do not redo):** the reveal on real HW draws the card as MANY wide-short strips (glitch), each a thin sourceRect slice — the oracle geometry shows ~100+ wide-short big quads/flip (1920x267, 1286x206, 1229x129, 1204x174, 1150x349, ...). OUR side draws the card as **ONE whole quad** (`UNEMUPS4_DRAWTEX_TRACE` shows `PLAIN base=0x9afc28000 count=6` every frame — one quad, whole-UV). Our GNM handles multi-quad draws fine (EID247 = drawIndexed 930 = 155 quads), so we do NOT truncate — the GUEST issued count=6. So the reveal's strip-count came out 1 on our emu vs N on real HW → a guest-side computation fed a wrong INPUT (verdict INPUT; x86jit ruled out airtight: 228,729 vector + 114,073 scalar ops replayed on a real Ryzen, zero divergence). The reveal draws whole-card-fullscreen every frame = animation stuck at "1 whole strip".

**Ruled out this session (do NOT rechase):** x86jit lift (differential), GPU/RT/compositing (3x — DAG correct, RT-as-texture works via render_targets.lookup), texture-cache staleness (UNEMUPS4_TEXCACHE_TRACE: zero STALE-HIT, only FIRST — dirty-tracking OK), videoout LOAD-accumulate as the root (force-clear removed content, not the cause), FS/.meta parse (byte-perfect).

**Uncommitted working-tree diagnostics (keep or revert as needed):** `crates/gnm/src/cache/mod.rs` (`UNEMUPS4_TEXCACHE_TRACE` content-hash staleness probe), `crates/gnm/src/exec.rs` (`UNEMUPS4_DRAWTEX_TRACE` PLAIN/RT + count per draw). Committed diagnostics on main: EXECTRACE, VBUF_TRACE(+_MIN/_MAX window), META_TRACE, decode_meta.py, uvdump, scraper MAX_FLIPS=3000. Oracle geometry-layout tool: `scratchpad/oracle_geom2.py` (VBUF positions → per-flip quad-layout SVG; no textures captured so pixel-image can't be reconstructed, only geometry).

**Methodology reference:** see doc-4 "Differential bisection + top-down" section (added this session).

### === SESSION 2026-07-19b — MAJOR CORRECTION: it's a frame-alternating PRESENT/DETILE bug, NOT a UV/base-vertex bug ===

**The whole prior diagnosis (UV divergence / whole-texture source-rect / base-vertex) is REFUTED. Read this before touching anything above.**

**Method failure that wasted the first half of the session:** analysis ran on the WRONG frames. Timeouts of 30–75 s only reached flip ~599, which is the EARLY flicker phase (flip 0–800: not smooth, black frames, screen flickers) — BEFORE the persistent corruption. Everything looked "byte-identical to real HW / correct" because it WAS the clean phase. Lesson (reinforce [[playable-needs-visual-oracle]]): ask the maintainer WHEN/WHERE the artifact occurs BEFORE instrumenting; put the flip number in the window title (done: `crates/gpu/src/display.rs` now prints `flip N`); window every probe on that range.

**The real corrupt window (maintainer-confirmed):** the CELESTE title screen (pink mountain + logo + gradient + snow + close-X), persistent from ~flip 1500 until pad input; also bad flip 800–1000. It **alternates frame-by-frame between a correct title frame and a garbage frame** (same screen, no input). Garbage frame = left ~1/4 is the gradient with horizontal BANDING lines; middle-right ~3/4 is the whole 4096² atlas laid out as a dense grid of tiny sprites + vertical-stripe artifacts. (Two maintainer screenshots captured; treat those as ground truth, not the flip number on them.)

**What the data proves (flip 800–1000, windowed probes):**
- Every flip has the SAME 5 draws with IDENTICAL texture bindings: `PLAIN 0x9afc28000 1500×199`, `0x9afae7100 775×395`, `0x9858ce900 134×126 count=300`, `0x9850c0000 1922×1082`, `0x98a4a9700 4096² count=504/510`. No per-flip variation in bindings, dims, dfmt(10)/tiling(8)/pitch.
- The 4096² atlas draw (count=504/510 = 84/85 quads): **UVs are fractional (correct atlas sub-rects), positions are stable screen-space** (first quad (572.5,400)→(599.5,434), a 27×34px sprite) on EVERY flip. So this draw renders correct sprites every frame — it is NOT the garbage.
- Descriptor-set pointer (VS user-SGPR slot 2) **ping-pongs every flip**: `42234532 ↔ 29651612` = two descriptor sets = classic DOUBLE-BUFFERING. But the V#s/data both sets point at are consistent/correct at decode time.
- So: draw list + textures + vertex UVs + vertex positions are all identical/correct every flip, yet the render alternates good/garbage. **The divergence is therefore NOT in geometry, UVs, texture binding, or base-vertex — it is in PRESENT / RT / DETILE state on alternate (double-buffered) frames.** Garbage image signature (gradient horizontal banding + atlas-as-grid) = an image read with the WRONG PITCH/STRIDE (task-155-class shear) on one of the two buffers.

**base-vertex hypothesis — DEFINITIVELY refuted by the real oracle (do NOT rechase):** the count=300 UI draw's vertex buffer has whole-quad CORNER UVs at record 0 and fractional at record 200. I hypothesised a dropped base-vertex (we fetch base+0, real HW base+200). WRONG. The real-PS4 oracle (`~/celeste-scrape-oracle/frame000000_sub0_flip_dcb.bin` + `buf03`) shows: `VGT_INDX_OFFSET (ctx 0xA102) = 0`, `DRAW_INDEX_OFFSET_2 = [max=300, index_offset=0, count=300]`, index buffer 0-based (0..199, SpriteBatcher pattern), and the oracle's OWN vertex buffer has the SAME corners@0 / fractional@200 layout. So real HW ALSO fetches base+0 = corners, IDENTICAL to us. Our vertex fetch is correct; base-vertex is 0 on both sides; the corners are legitimate whole-texture sprites both sides draw. `base+200` is just the next batch in the shared MonoGame dynamic ring (`SetUserVertexBuffer`, base advances per draw, base+num_records·stride = ring end = const).

**Mesa register reference (from maintainer):** GFX6 3D reg indices — `index_offset(VGT_INDX_OFFSET)=0xA102`, `index_base_address=0xA1F9`, `num_instances(uconfig)=0xC24D`, `primitive_type=0xC242`. Two takeaways: (1) VGT_INDX_OFFSET is the base-vertex register and it's 0 here (confirmed via oracle) → base-vertex genuinely unused. (2) Other implementations SEED the register file with hardware defaults; WE start with an empty sparse bank (unset = None) — a real gap worth fixing (may affect other rendering) but NOT the cause of this alternation.

**Our GPU submission path IS correct here:** CCB is decoded+walked (pm4/decode.rs:248 appends CCB packets), the 3 interleaved vertex V#s (pos@0, color@12, uv@16, stride 24) decode right, vertex data is snapshotted into a Vulkan copy at decode time (ResourceCache) so there is no CPU↔GPU race on vertex bytes.

**NEXT STEP (start here next session):** instrument the PRESENT / videoout / flip-buffer path per-flip — which flip buffer (buf_idx) is presented, its base/pitch/tiling, and whether the detile alternates between two buffers with different pitch/stride. This is the common cause of BOTH symptoms (0–800 black-frame flicker AND 800–1000 alternating garbage). Look at `crates/gpu/src/backend.rs` present + the videoout submit (buf_idx, `submit_and_flip`) and the framebuffer detile/pitch. Relates [[gpu-completion-timing-gotcha]] (task-157) and task-149/152 videoout accumulate.

**Uncommitted working-tree diagnostics added this session (all env-gated, zero-cost off; keep or revert):**
- `crates/gpu/src/display.rs`: flip number in window title.
- `crates/gnm/src/exec.rs`: `UNEMUPS4_VBUF_SPAN=1` → `vbuf_span_scan` (full-span corner-vs-fractional UV + pos4 positions + uds dump + `BV_HUNT`/`BV_REGS` register scans, windowed via `UNEMUPS4_DUMP_VBUF_MIN/MAX`); `idx_value_probe` (index-buffer values); `DRAWTEX` extended with full T# dims (W/H/dfmt/nfmt/tiling/pitch) + flip + window.
- `crates/gnm/src/vbuf.rs`: `resolve_slot` descriptor-set memory-window dump (`DESC_DUMP`).
- `crates/gnm/src/state.rs`: `RegFile::scan_value` (find register holding a value).
- flake.nix: added `pkgs.ilspycmd` (decompiled Celeste.exe + MonoGame + Sce.PlayStation4.dll to scratchpad).

**Oracle facts learned:** scrape covers frames 0..2111 (2112 frames), DCB+vertex/index/V#-set/shader-code/const-buffer buffers per frame — but NOT T#/S# or texture bytes (the small `buf00/02/09` are shader CODE, not descriptors). Frame numbers DRIFT vs our flip numbers (our flip 873 ≠ oracle frame 873 — different draw mix), so align by content/role, not by number.

### === RESOLVED 2026-07-20 — root cause: buffer-cache STALE-HIT (missed dirty-tracking) ===

**Root cause (maintainer's own hypothesis, confirmed):** PS4 games reuse the same guest memory for dynamic buffers every frame; our resource cache served STALE bytes because the dirty flag was never set. Two compounding bugs:

1. **Double-drain of the dirty source** (`libscegnmdriver/submit.rs`): `DirtySource::take_dirty()` is a DRAINING read. The submit path called `gcn.drain_dirty(ds)` then `resources.drain_dirty(ds)` — each internally `take_dirty()`d — so the SECOND consumer (the resource cache) always got an EMPTY set and never marked buffer entries dirty. Fixed: drain ONCE, feed both via new `apply_dirty(&[(u64,u64)])` methods on `ResourceCache` and `GcnShaderProvider` (drain_dirty kept as a thin wrapper for tests).

2. **x86jit dirty-tracking returns nothing anyway** (the deeper cause): even with one drain, `VmDirtySource::take_dirty()` returned **0 ranges** every submit (`UNEMUPS4_DIRTY_TRACE`) — x86jit's watched-range facility (`watch_range`/`note_watched_write`/`take_dirty_ranges`, x86jit-core `memory.rs`) does NOT report the MonoGame `DynamicVertexBuffer.SetData` / projection-const rewrites. So the buffer cache clean-hit served stale bytes (`UNEMUPS4_TEXCACHE_TRACE` buffer STALE-HIT probe: **66625 stale hits**, VertexBuf rings + ConstBuf projection matrices). This is why `UNEMUPS4_DIRTY=always` (force AlwaysDirty) rendered clean — it re-uploads every submit, sidestepping the broken tracking.

**Emulator-side fix (landed, default):** in `ResourceCache::get`, dynamic COPY-path buffers (`VertexBuf`/`IndexBuf`/`ConstBuf`, not imported/zero-copy) now force a re-upload on every hit instead of trusting the dirty flag — because the dirty source under-reports their rewrites. Textures/RTs keep the incremental dirty path (large, not rewritten per frame). Result: STALE-HIT → **0**, Celeste title screen renders clean, framerate acceptable. Confirmed live by the maintainer.

**Follow-up (proper fix, restores incremental perf):** the real fix is x86jit reporting these writes so we can drop the always-reupload workaround. Filed in the **x86jit backlog** (dirty-tracking gap: `watch_range`d dynamic buffers written by JIT/interp guest code not surfaced by `take_dirty_ranges`). When it lands: remove the `dynamic_copy` force-reupload in `ResourceCache::get`, run with `UNEMUPS4_TEXCACHE_TRACE=1`, confirm 0 STALE-HIT. The buffer STALE-HIT probe + `store_upload_hash` are KEPT in the tree for that verification; the flip# in the window title (`display.rs`) is kept too.

**Process lesson (reinforce [[playable-needs-visual-oracle]]):** the maintainer flagged "games reuse memory for different textures, need cache invalidation" TWICE early on; I chased base-vertex / present-path / detile for a very long time before testing the cache-staleness hypothesis directly. TRUST THE MAINTAINER'S DOMAIN HYPOTHESIS EARLY — test it before the exotic ones. Also: never analyze the wrong frames — the flicker phase (flip 0–800) looked "correct/oracle-identical" only because the corruption starts later (~flip 300 when the 2nd dynamic buffer appears); put flip# in the title and window every probe.

**New opcode noted (not this task):** `[GNM] unhandled PM4 opcode 0x58 (IT_ACQUIRE_MEM)` — implement later.

**Throwaway probes removed in cleanup:** VBUF_SPAN/BV_HUNT/BV_REGS/DESC_DUMP/idx_value_probe/CB_TRACE/PRESENT_TRACE/SUBMIT/DIRTY_TRACE, `RegFile::scan_value`, ilspycmd in flake, DRAWTEX dim-extension. KEPT: the fix, the buffer STALE-HIT probe (`UNEMUPS4_TEXCACHE_TRACE`) + `store_upload_hash`, flip# in window title.

## Follow-up CLOSED 2026-07-20 — the workaround is gone

This task's note said: "When the x86jit fix lands, drop the force-reupload and verify 0
STALE-HIT via `UNEMUPS4_TEXCACHE_TRACE`." Done, in `ad0d4bd`.

x86jit `873563f` ("watched-page tracking must span the whole guest address space") fixed the
root cause on its side: `watch_page` had been sized with the CODE-page sizing, capped at
`CODE_WINDOW` (4 GiB), while our GPU buffers sit around 41 GiB in the direct-memory heap — so
every `watch_range` silently no-opped. Pin bumped `2f9372a` → `873563f`; the force-re-upload
branch is removed from `ResourceCache::get` and dynamic copy buffers take the incremental
dirty path again.

Verified, not assumed: `[BUFCACHE STALE-HIT]` = **0** over a 75-second run with real submits
and draws (it read 66625 before the workaround was introduced), and the maintainer confirmed
no visual regression — a broken dirty path would show as overlapping textures immediately.

**The seven `cache::tests` failures were caused BY this workaround, not merely coexisting
with it.** They assert that a clean hit emits no commands, which force-re-upload violates by
construction. Removing it took `cargo test --workspace` to 494 passed / 0 failed — the first
fully green run since `5af099d`. Worth remembering: a persistent known-red suite is a claim
to re-examine, not a constant to design around.
