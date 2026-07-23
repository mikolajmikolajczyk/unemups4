---
id: TASK-179
title: >-
  gnm/gpu: Celeste menu loses the 3D mountain — bloom RT composited as an opaque
  replace instead of an additive add
status: To Do
assignee: []
created_date: '2026-07-20 10:47'
updated_date: '2026-07-20 12:53'
labels:
  - gpu
  - gnm
  - celeste
  - retail
  - bloom
  - blend
dependencies: []
priority: high
ordinal: 183000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
On the Celeste main menu (CLIMB / Options / Credits) the 3D mountain background is missing; the final frame shows only a dark gradient plus the menu text. **ROOT-CAUSED (2026-07-20): we never read `SPI_PS_INPUT_CNTL`, so pixel-shader attribute slots are routed to the wrong vertex-shader export parameters.** The bloom blur's PS reads its UV from `attr0`, and its draw programs `SPI_PS_INPUT_CNTL_0.OFFSET = 1` — meaning slot 0 must read VS parameter 1. The recompiler assumes the identity mapping and hands it parameter 0, the vertex COLOUR, which is a constant `0xFFFFFFFF` across the quad. Constant UV means the sampler reads a single texel, so the blur outputs a constant and the bloom RT never contains the scene. Compositing that empty RT over the frame with the guest's premultiplied blend then wipes the mountain. The two composite draws use OFFSET 0, so they work by coincidence — that asymmetry (same source RT, same shader, one draw fine and one not) is what made this take so long.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 Root-caused with evidence: PS attribute slots are routed by `SPI_PS_INPUT_CNTL_n.OFFSET`, which we never read — measured OFFSET=1 on both blur draws vs 0 on both composites
- [x] #2 PS input slot `n` resolves to location `SPI_PS_INPUT_CNTL_n.OFFSET` instead of `n`, and the shader cache key includes that mapping (the same PS binary under a different mapping is a different module)
- [ ] #3 Celeste's menu renders the 3D mountain behind the menu text — maintainer's live oracle — and the studio splash + title screen stay correct (no regression)
- [x] #4 A regression test pins the non-identity mapping: a PS reading `attr0` under `OFFSET=1` must sample the VS's parameter 1
- [ ] #5 build + cargo test + clippy clean; diagnostic probes and experiment knobs either removed or left env-gated with zero cost when off
<!-- AC:END -->



## Notes

Session 2026-07-20. Investigation only — no fix landed for this bug. Everything below is
MEASURED unless marked as inference.

### The frame (our DAG, matches the RenderDoc capture 1:1)

Captures: `/home/mikolaj/renderdoc_captures/20072026_{1,2}.rdc` (frame 2244, 24 draws /
24 passes). RenderDoc groups passes by target, so "Colour Pass #4" is the VIDEOOUT pass,
not an RT — a point that mislead the first pass of this investigation.

```
RT_A 0x9b00e0000 (1920x1088)  fill (0,0,0,1) + 8 scene draws   <- the mountain
RT_B 0x9b1418000 (1024x576)   fill (0,0,0,0) + sample RT_A     <- horizontal blur
RT_C 0x9b1658000 (1024x576)   fill (0,0,0,0) + sample RT_B     <- vertical blur
videoout (1920x1080)          fill (0,0,0,1) x2
                              sample RT_A   EID 278            <- mountain lands, VISIBLE
                              sample RT_C   EID 290            <- WIPES the frame to black
                              + GUI atlases, then a gradient
```

The three RT bases and the 2:1:1 draw ratio match the three bounded `IT_ACQUIRE_MEM`
coherency ranges decoded from the real-PS4 oracle (doc-6 Entry 23) — independent
confirmation that these are the RTs the title means to round-trip.

### Measurements

- **Wipe arithmetic.** `CB_BLEND0_CONTROL = 0x45010501` on BOTH videoout composites
  (decoded: src=ONE, dst=ONE_MINUS_SRC_ALPHA, comb=ADD, enable=1). With src=(rgb, a=1),
  `dst = rgb + dst*(1-1) = rgb`. RT_C is near-black, so the frame goes black. With a=0 the
  same blend is `dst = rgb + dst`, i.e. additive — which is what a bloom needs.
- **RT contents** (`UNEMUPS4_RT_READBACK=1`, mean over a 16x16 grid across the central 80%
  of each RT): RT_A `[36,57,86,255]`, RT_B `[8,16,33,255]`, RT_C `[2,4,8,255]`. Alpha is
  **255 on all three**.
- **Blur constant buffer** (PS CB, first 4 dwords as floats): stage 1 `[1/960, 0, 0.75,
  -2.083]`, stage 2 `[0, 1/540, 0.75, -2.083]`. A separable Gaussian, horizontal then
  vertical, texel step matching the 960x540 viewport. **Read correctly.**
- **Blur shader** (dumped from the live scene, `/tmp/blur_9afae4f00.bin`, 396 bytes):
  5 `image_sample` taps weighted 0.2/0.3/0.2/0.15/0.15 = **1.0**, then a radial term
  `v20 = 0.75*dist + 0.75` applied to all four channels, then
  `v_cvt_pkrtz_f16_f32` x2 + `exp mrt0 ... compr`. Traced channel by channel through the
  register permutation: the exported ALPHA is the blurred SOURCE alpha.
- The ~4x brightness drop per stage is most likely BY DESIGN, not a defect: `cbf[3] =
  -2.083` reads as a bloom extraction threshold (`max(0, colour - threshold)`), which on a
  dark scene legitimately yields a near-black glow. (Inference, not proven.)

### The open contradiction — start here

Alpha flows: RT_A is cleared to (0,0,0,1) → scene draws premultiplied keep alpha 1 → the
blur exports the blurred source alpha → RT_B/RT_C alpha = 1 → the composite replaces.
By that chain **real hardware would wipe too**, and it does not. One link is wrong.

Cheapest next probes, in order:
1. In RenderDoc, view the ALPHA channel of RT_A at EID 278 and of RT_C at EID 290 (the `A`
   button in the Texture Viewer). If RT_A's alpha is not uniformly 1, the chain breaks
   there and the RT clear / scene alpha writes are the suspect.
2. If the alpha really is 1 everywhere, the divergence is inside the blur shader's
   execution: run it through the differential harness (`crates/gcn/src/interp.rs` vs the
   recompiled SPIR-V, `crates/gpu/src/bin/diff_harness.rs`) on identical inputs to find the
   instruction where results part.

### Ruled out — do NOT re-chase (each was measured, not argued)

- **RT-as-texture registry misses.** It resolves: 1484 RT binds in the sampled window; the
  consumer rebuilds the producer's exact cache key, so it is a clean hit with no upload.
- **GUI overdraw.** The mountain dies at EID 290, BEFORE any GUI draw.
- **`Draw(3,1)` losing its texture.** Those shaders are legitimate constant-colour fills
  (`s_buffer_load_dwordx4` + `exp mrt0`); they declare no sampler because they need none.
- **Colour write mask.** Celeste programs `CB_TARGET_MASK = 0xffff` on every draw. (We WERE
  hardcoding RGBA and ignoring the register — fixed separately in `e060725`, but it is not
  this bug.)
- **A lost blend-state change.** `CB_BLEND0_CONTROL` is identical (0x45010501) on both the
  mountain and the bloom composite, so we are not missing a register write.
- **`s_buffer_load` offset semantics.** The immediate is a dword index and is indexed as
  one.
- **VOP3 `neg`/`abs` modifiers.** Decoded AND applied (`apply_mods`).
- **RT format / channel order.** RT images and their render passes are both
  `R8G8B8A8_UNORM`; `ColorFormat::R8G8B8A8Unorm` is used consistently on both sides.
- **sRGB/UNORM conversion.** Numerically excluded: 71 would map to 5 (sRGB→linear) or 148
  (linear→sRGB); measured 34.
- **Pass ordering / loadOp.** Only the first videoout pass CLEARs; every later one LOADs
  (task-149/152 latch), so nothing downstream erases the frame. Image layouts are barriered
  COLOR→SHADER_READ after every RT write.

### Two defects found on the side (not this bug, worth their own fixes)

1. **`crates/gcn/src/disasm.rs` silently drops VOP3 `neg`/`abs` modifiers** (zero mentions
   in the file). Disassembly therefore prints wrong operand signs — it made a symmetric
   5-tap blur read as one-sided and cost a false hypothesis here.
2. Our own PM4 emitter writes `CB_SHADER_MASK`, but nothing in `derive.rs` ever read it or
   `CB_TARGET_MASK` until `e060725`.

### Method notes (two of my own measurements were wrong — both flattered a hypothesis)

- A one-shot-per-address shader dump fires on the FIRST occurrence, which is the splash —
  not the scene under investigation. Window every dump on the flip range (the window title
  carries the flip number).
- Comparing a fixed BYTE OFFSET across buffers of different widths compares different
  scene positions: offset 1181696 is the centre of a 1024-wide RT but the top-right corner
  of a 1920-wide one. Sample a normalised position, and prefer a grid mean over a single
  texel when the quantity of interest is brightness.
- The decisive evidence throughout was the maintainer's RenderDoc per-draw input/output,
  not code reading. For a compositing bug, get that FIRST.

### Diagnostics left in the working tree (uncommitted, `crates/gnm/src/exec.rs`)

`UNEMUPS4_DRAWTEX_TRACE=1` now also logs, per draw: the target (`videoout` or `rt:<base>`),
`CB_TARGET_MASK`, `CB_BLEND0_CONTROL`, the sampled RT's grid-mean RGBA (needs
`UNEMUPS4_RT_READBACK=1`), the PS constant buffer's first 4 floats, and a `NOSAMPLER` line
for draws whose PS declares no sampler. `UNEMUPS4_DUMP_PS=<flip>` dumps PS GCN code from
that flip onward. Decide whether to keep these env-gated or drop them when this task closes.

## Update — corrections and the leading hypothesis (same session)

### Two corrections to the notes above

1. **The ~4x per-stage brightness drop is CORRECT behaviour, not a defect.** The blur's
   radial term uses `s16 = CB[2] = 0.75` (measured), and with the VOP3 sign modifiers the
   disassembler does not print, it scales by `(1 - 0.75) = 0.25`. Measured RT_B→RT_C ratio:
   **0.25 / 0.25 / 0.24** — a match to two decimals. The first stage reads 0.22/0.28/0.38
   because it also rescales 1920→1024, so the sampled regions only roughly correspond.
   Our recompiler executes this faithfully (`apply_mods` applies `neg`). **Drop "brightness
   loss" as a symptom — the ONLY anomaly is the alpha.**
2. **Withdraw the "`cbf[3] = -2.083` is a bloom extraction threshold" reading.** It was
   speculation and it is wrong: this shader loads only `CB[0]`, `CB[1]` and `CB[2]`. It
   never touches `CB[3]`.

### RT_A's alpha is genuinely 255 (maintainer, RenderDoc)

Viewing the alpha channel alone at the mountain composite shows solid **white** — alpha is
1 across the target, and with all channels enabled the mountain is there and correct. So
the alpha chain is NOT broken at the source: RT_A really does carry alpha = 1, exactly as
the `(0,0,0,1)` clear plus premultiplied scene draws imply.

That kills the idea that some upstream link corrupts the alpha, and forces the question to
its real form: *if the guest's own data says alpha = 1, why does hardware not wipe?*

### Leading hypothesis — `CB_SHADER_MASK` is never read

`CB_SHADER_MASK` (`R_02823C`, per-MRT **output-component** mask) is defined in
`pm4/opcodes.rs` and is even written by our own PM4 emitter — but **nothing in `derive.rs`
ever reads it**. It is the complement of `CB_TARGET_MASK` (fixed in `e060725`): TARGET_MASK
says which components the colour block may write, SHADER_MASK says which ones the pixel
shader actually exports. A component the shader does not export is not stored.

If Celeste programs `CB_SHADER_MASK` = RGB-only for the blur passes, alpha is never written
into RT_B/RT_C, so they keep the **alpha 0** their `(0,0,0,0)` clear left — and the
premultiplied composite becomes `dst = rgb + dst`, an additive glow. The mountain survives.

Every measured value fits this and nothing contradicts it:

| measured | fits |
|---|---|
| RT_A cleared `(0,0,0,1)` opaque | the scene target *should* be opaque |
| RT_B / RT_C cleared `(0,0,0,0)` transparent | the game deliberately wants alpha 0 there |
| blur shader exports the blurred source alpha | true, but the mask would discard it |
| our RT_B / RT_C carry alpha 255 | because we ignore the mask and store all four |

**Verify before implementing:** log `CB_SHADER_MASK` per draw and confirm it is RGB-only
(e.g. `0x7`) on the two blur draws while the scene draws use all components. If it reads
all-enabled everywhere, this hypothesis is dead too and it must be recorded as such.

Implementation, if confirmed: derive the effective write mask as
`CB_TARGET_MASK & CB_SHADER_MASK` for MRT0 and carry it on `BlendKey::write_mask`
(the plumbing from `e060725` already exists — only the derivation changes).

### On the differential harness — do NOT reach for it here

`crates/gpu/src/bin/diff_harness.rs` runs our CPU interpreter as the oracle against our
recompiled SPIR-V on a real device. It compares **our code against our code**: if both
share a wrong assumption it passes and teaches nothing, and it says nothing about what real
PS4 hardware does. It is the right tool for "is the recompiler faithful to the interpreter",
which is not the open question — the shader translation is behaving correctly here.

### `CB_SHADER_MASK` hypothesis — REFUTED

Measured: `CB_SHADER_MASK = 0xF` on **every** draw of the affected frame, blur passes
included. Together with `CB_TARGET_MASK = 0xffff` that means the guest asks for all four
components to be exported AND written, so alpha 255 in the bloom RTs is what its own
register state calls for. Neither mask is the cause. (The `e060725` TARGET_MASK fix remains
correct on its own merits — we were ignoring a register — it is simply not this bug.)

Where that leaves the contradiction: every input we can measure — the clears, the blend
control, both masks, the blur shader's dataflow, and RT_A's alpha confirmed white in
RenderDoc — says hardware should wipe the frame exactly like we do. It does not. So the
divergence is in something we have NOT yet measured, not in the values above.

### Still unmeasured, in priority order

1. **`CB_COLOR_CONTROL` (`R_028808`) is defined in `pm4/opcodes.rs`, named in TWO doc
   comments in `derive.rs` as part of blend derivation — and never actually read.** It
   carries `MODE` (0 = `CB_DISABLE`, i.e. no colour writes at all) and `ROP3` (0xCC = plain
   copy; other values are logic ops). The same "defined, emitted, never read" shape that
   `CB_TARGET_MASK` and `CB_SHADER_MASK` turned out to have. If the bloom composite is
   programmed with a non-default mode or ROP, we are ignoring the instruction.
2. **The composite quad's GEOMETRY.** We have only ever checked its vertex COUNT (6), never
   its positions. If the RT_C composite should cover a small region and our vertex fetch
   makes it full-screen, that alone explains the wipe without any blend/alpha defect. The
   dynamic vertex ring is a known-fragile area here (task-178 cache staleness, x86jit
   task-275 dirty tracking), so a stale or mis-based quad is plausible.

### `CB_COLOR_CONTROL` — REFUTED (and a reasoning error corrected)

Measured: **Celeste never programs `CB_COLOR_CONTROL` at all** (`None` — the shadow
register is unset for the whole run). It cannot be the cause. It remains a real fidelity
gap that we neither read it nor seed a hardware default for it, but it is not this bug.

**Correction to everything above that blamed RT_C's stored alpha.** In the blend equation,
`src` is the COMPOSITE draw's fragment-shader output — not the sampled texel. RT_C's alpha
of 255 only forces a replace if the composite shader passes that alpha through. That
shader was never examined: only the blur shaders were dumped. A bloom composite typically
exports `(glow * intensity, 0)` — alpha 0 — which makes the premultiplied blend additive
regardless of what alpha sits in the texture.

So the question is now precise and local: **what alpha does the composite PS export, and
does our recompilation of it agree?** This is the only remaining hypothesis that explains
the contradiction without assuming hardware behaves differently on identical inputs.

### Separate bug found on the way: offscreen RT width is the PITCH, not the surface width

`derive_target` sets `width = pitch` for offscreen targets, but pitch is alignment-padded:
measured, the blur targets have viewport `(0, 540, 960, -540)` while we create the RT as
**1024x576**, and the scene target has viewport 1920x1080 against a **1920x1088** RT. The
guest's UVs assume the content width (960 / 1080), so sampling `[0,1]` reads ~6% past the
content into never-written padding — the composited image is slightly scaled and carries a
border. Not the cause of the wipe, but a genuine correctness defect worth its own task:
the RT's sampled extent should come from the surface/viewport extent, not the padded pitch.

### The composite shader, the vertex colour, and where the chain must break

The composite PS (dumped from the live scene) is a plain textured quad:

```
image_sample v[2:5], ...            ; texture RGBA
v_interp_p1/p2 attr0.x/y/z/w        ; VERTEX COLOUR RGBA
v_mul_f32 v1, v2, v8                ; R_tex * R_vcol
v_mul_f32 v2, v7, v3                ; G
v_mul_f32 v3, v6, v4                ; B
v_mul_f32 v0, v0, v5                ; A_tex * A_vcol      <-- decides add vs replace
exp mrt0, ...
```

So `out_alpha = vertex_alpha * texture_alpha`. Both composites (mountain and bloom) share
this shader AND the same blend (0x45010501) — they differ only in vertex colour and source
texture.

- **Vertex colour: REFUTED as the cause.** Measured first vertex of both composites, stride
  24 with streams at +0/+12/+16 (pos/colour/uv): colour = `ff ff ff ff` for **both**. So
  `vcol.a = 1`, and `out_alpha = 1 * tex.a`.
- **Reference (maintainer, real Celeste):** the menu background is the **sharp** mountain
  with a soft glow — i.e. RT_A as the base layer PLUS an additive bloom. The replace is
  therefore wrong and the bloom composite must be additive.

Chaining those: additive requires `out_alpha = 0`, `vcol.a = 1`, therefore **RT_C's alpha
must be 0**, therefore RT_B's and RT_A's must be 0 (the blur passes alpha through and the
premultiplied blend accumulates it). **Our RT_A carries alpha = 255.**

Consistency check that supports this rather than merely assuming it: if RT_A's alpha were
0, the MOUNTAIN composite would also be additive — and since videoout is cleared to opaque
black first, `mountain + black = mountain`, visually identical. So alpha 0 is correct for
BOTH composites while alpha 1 only happens to look right for the first. That also explains
why the mountain looks fine despite carrying the same defect: its error is invisible
against a black destination.

### Prime suspect: the RT_A clear's constant colour

Our RT_A alpha of 255 traces to the guest fill draw whose PS constant buffer we read as
`[0,0,0,1]`. This is the ONE link never validated. The measured pattern is suspicious —
all four fills run the SAME shader (`0x981400000`) and differ only in the constant buffer:

```
fill RT_A      -> [0, 0, 0, 1]
fill RT_B      -> [0, 0, 0, 0]
fill RT_C      -> [0, 0, 0, 0]
fill videoout  -> [0, 0, 0, 1]
```

A wrong offset or a stale copy would produce exactly this: some values right, some not.
Constant buffers are a documented weak spot here — task-178 measured 1433 ConstBuf
STALE-HITs and the force-re-upload workaround is still load-bearing because x86jit dirty
tracking is dead (x86jit task-275, `take_dirty` returned 0 ranges on 3709/3709 submits).

**Next measurement (running):** dump 16 floats of the PS constant buffer for the fill draws
instead of 4, plus the resolved range (addr/size). If `[0,0,0,0]` sits a few dwords from
where we read, we are reading at the wrong offset. If the buffer genuinely holds
`[0,0,0,1]` at offset 0, the clear is faithful and the divergence is further upstream —
in which case the next thing to question is whether the ALPHA channel should be written by
the scene draws at all.

## Experiment knobs — the productive turn

Reading code and adding probes had stalled (see the refutation list); what moved this
forward was the maintainer's suggestion to add **knobs they could flip and judge by eye**.
Three runs with knobs produced more than a dozen instrumented runs had. Keep this approach
for compositing bugs: the eye is a faster oracle than a readback, and it does not lie.

All are env-gated, default OFF, zero cost when unset (`OnceLock`-cached lookups):

| knob | effect | observed |
|---|---|---|
| `UNEMUPS4_X_SKIP_BLOOM=1` | drop the composite that samples an RT smaller than its target | **correct picture** — mountain, castle, clouds, essentially the reference |
| `UNEMUPS4_X_ADDITIVE=1` | force dst factor ONE on any draw sampling an RT | white blow-out over a region **exactly 960/1024 = 93.75%** of the width |
| `UNEMUPS4_X_RT_ALPHA_MASK=1` | never write alpha into offscreen RTs | no change |
| `UNEMUPS4_X_RT_CLEAR_ALPHA0=1` | clear RTs to transparent instead of opaque black | region turns from white to a smooth gradient; still no mountain |
| `UNEMUPS4_X_FULL_BARRIER=1` | ALL_COMMANDS→ALL_COMMANDS barrier after every pass | **no change** |
| `UNEMUPS4_X_PASS_TRACE=1` | log which map each sampled id resolved from + the pass's target RT | see below |

`SKIP_BLOOM` producing the correct picture is the key anchor: **everything upstream of the
bloom composite works.** The defect is confined to the bloom chain's content or its
composite.

`ADDITIVE`'s white region ending at exactly 93.75% is an independent visual confirmation of
the RT-width-is-pitch bug recorded above (content 960 wide, image 1024 wide).

`RT_ALPHA_MASK` doing nothing is explained: with alpha never written it retains whatever our
FIRST-USE CLEAR left, and that clear is `[0,0,0,1]`. The knob locks in alpha 1 rather than
removing it — a badly designed knob, not evidence against the alpha theory.

## Refuted by experiment (not by argument)

- **Synchronisation / barriers.** A maximally conservative full barrier after every pass
  changes nothing.
- **Wrong image bound.** `PASS_TRACE` shows the complete, correct DAG with zero
  `UNRESOLVED`:
  `1156(RT_A)→1187(RT_B)`, `1187(RT_B)→1194(RT_C)`, `1156→videoout`, `1194→videoout`,
  all `from=RT`, 211 of each per run.
- **Vertex UVs / geometry.** Six vertices dumped and decoded (24-byte record =
  `pos.x, pos.y, pos.z, colour, u, v`): a proper 960x540 quad with UVs spanning 0..1
  `(0,0) (1,0) (0,1) (1,1)`. Nothing constant, nothing degenerate.
- **Shader lowering drops the samples.** The generated SPIR-V has all 5
  `OpImageSampleImplicitLod`; each is `OpCompositeExtract`ed into its four channel
  variables and those variables feed the final composite chain. The radial term is applied
  to all four channels (four `OpFNegate`/`OpFMul`/`OpFAdd` groups) and the export builds
  `OpCompositeConstruct %v4float` from exactly those. `OpFNegate` being present also
  settles that VOP3 `neg` modifiers work — earlier that was only inference.

## Retractions (claims I made that the evidence killed)

- **"The blur loses ~4x brightness per stage."** Built on `UNEMUPS4_RT_READBACK` texel
  reads. `ADDITIVE` blowing the screen to white proves the bloom RT is BRIGHT, not near
  black. **The RT readback path returns values that do not match what the GPU holds** —
  treat it as untrustworthy until someone verifies it, and do not rebuild an argument on
  it. It misled this investigation more than once.
- **"RT_C is a featureless radial gradient, so the sample returns a constant."** That read
  the on-screen result of a replace-composite; the purple gradient in the final frame comes
  from the LAST draw (EID 362, an additive gradient texture), not from RT_C.
- **"The ~0.25 factor is intentional."** Speculation about modifiers the disassembler does
  not print. The SPIR-V now confirms the negation exists, but the brightness claim it
  supported came from the untrustworthy readback and is withdrawn with it.

## Open question, one RenderDoc click

At **EID 230** (Colour Pass #2, the `RT_A → RT_B` blur, `DrawIndexed(6,1)`) the capture
shows two things that cannot both be RT_B: the main Texture Viewer reports
`2D Color Attachment 42381 - 1024x576` and is **black**, while the Outputs panel thumbnail
(`FS 0 / res86`) shows **the scene**. Which one is RT_B after this draw decides the split:

- **RT_B holds the scene** → the first blur works; the failure is the second stage
  (`RT_B → RT_C`) or the composite.
- **RT_B is black** → the first blur already fails, despite correct binding, barriers, UVs,
  geometry and a structurally correct SPIR-V — which would leave the shader's *inputs*
  (descriptor contents, sampler, the T#/S# actually bound) as the last unexamined layer.

## ROOT CAUSE FOUND — `SPI_PS_INPUT_CNTL` is never read, so PS attribute routing is wrong

The maintainer's RenderDoc read at EID 230 (the `RT_A → RT_B` blur) split it open: the
draw's **input is the correct scene texture** while its **output is a constant colour**. A
correctly-bound texture that samples to a constant means the UV reaching the sampler is
constant — even though the vertex buffer's UVs were measured correct.

The two shaders read UV from DIFFERENT attribute slots:

```
BLUR      v_interp_p1_f32 v16, v0, attr0.x     ; UV from attr0
COMPOSITE v_interp_p1_f32 v2,  v0, attr1.x     ; UV from attr1
          v_interp_p1_f32 v8,  v0, attr0.x     ; COLOUR from attr0
```

On GCN, which VS export parameter feeds PS attribute slot `n` is programmed by
`SPI_PS_INPUT_CNTL_n.OFFSET` (bits [4:0], `R_028644`+n). **That register appears nowhere in
the codebase.** `recompile.rs` assumes the identity mapping on both sides: VS
`ExportTarget::Param(n)` → location `n`, and a PS `attr` → location `attr`.

Measured per draw (`psin[0..3]`, one frame of the affected scene):

| draw | `psin[0].OFFSET` |
|---|---|
| blur 1 — RT_A → RT_B | **1** |
| blur 2 — RT_B → RT_C | **1** |
| composite — RT_A → videoout | 0 |
| composite — RT_C → videoout | 0 |

So the blur's `attr0` must read VS **parameter 1** (the UV). We feed it parameter 0 — the
vertex COLOUR, which is a constant `0xFFFFFFFF` across the quad. Constant UV → one texel
sampled → constant output → a bloom RT with no scene in it. The composites program OFFSET 0
and therefore work by coincidence, which is exactly the asymmetry that misdirected this
investigation for so long: same source RT, same composite shader, one draw fine and one not.

This also explains why every other layer checked out — binding, barriers, vertex UVs,
geometry and the generated SPIR-V were all correct. The defect is one level above them, in
how VS outputs are routed to PS inputs.

### Fix shape (not implemented — needs a design decision)

PS input slot `n` must resolve to location `SPI_PS_INPUT_CNTL_n.OFFSET`, not `n`. Two
consequences that make this more than a one-liner:

1. The recompiler does not currently see context registers; the mapping has to be threaded
   into `recompile()` (alongside the stage) and applied where PS input variables get their
   `Location` decoration.
2. **The shader cache key must include the mapping.** The same PS binary under a different
   `SPI_PS_INPUT_CNTL` is a different SPIR-V module — without this, the first variant is
   cached and handed to the second, reproducing this very bug in a harder-to-see form.

### Pattern worth a follow-up of its own

This is the FOURTH register in this investigation that exists in hardware, changes results,
and was never read: `CB_TARGET_MASK` (fixed in `e060725`), `CB_SHADER_MASK`,
`CB_COLOR_CONTROL`, and now `SPI_PS_INPUT_CNTL`. The first three turned out harmless HERE;
the fourth is the bug. Worth a dedicated audit of which pipeline-affecting registers the
derivation still ignores, rather than discovering them one wall at a time.

## Spun-off tasks

The investigation surfaced four defects that are independent of this bug and are filed
separately so they do not get lost in this file:

- **task-180** — offscreen RT extent is the padded PITCH, not the surface width (confirmed
  visually: the affected region ends at exactly 960/1024 of the screen).
- **task-181** — `UNEMUPS4_RT_READBACK` reports values that contradict the rendered image.
  It misled this investigation more than once; do not build an argument on it until fixed.
- **task-182** — the disassembler drops VOP3 `neg`/`abs` modifiers, so printed operand
  signs are wrong. Cost a false hypothesis here.
- **task-183** — audit which pipeline-affecting registers the derivation still ignores.
  Four turned up in this one investigation; finding them one wall at a time is expensive.

## FIX LANDED — confirmed by the maintainer's live oracle (2026-07-20)

The `SPI_PS_INPUT_CNTL` routing fix works. Celeste's menu now renders the 3D mountain
behind CLIMB / Options / Credits, with the bloom, the snow and the fog bank all present.
Root cause confirmed end-to-end: it was the PS attribute routing, exactly as diagnosed.

Shape of what landed (nothing committed yet — the maintainer commits):

| file | change |
|---|---|
| `crates/gcn/src/recompile.rs` | `PsInputMap` (`Default` = identity, `OFFSET` masked to 5 bits), `recompile_with()`, `read_ps_input` decorates `Location = map.location_for(attr)` |
| `crates/gnm/src/state.rs` | `gcn_ref_from_regs` snapshots `SPI_PS_INPUT_CNTL_0..31` for the pixel stage; an unwritten slot stays identity |
| `crates/gnm/src/shader/source.rs` | `ShaderRef::GcnBinary` gains `ps_input_map` |
| `crates/gnm/src/derive.rs` | `shader_hash` mixes the map, so `PipelineKey` re-keys on a routing change |
| `crates/gnm/src/shader/gcn.rs` | the map is part of `ShaderCacheKey` |

`recompile()` is kept as an identity-mapping wrapper over `recompile_with()`. That is not
just churn-avoidance: the differential harness diffs against `interp.rs`, which has no
notion of routing, so identity is the CORRECT default on that path.

**`ps_inputs` had to be re-keyed from `attr` to the RESOLVED LOCATION.** Under a
non-identity map two distinct attribute slots can resolve to the same location (unused
slots commonly read `OFFSET=0`), and two Input variables sharing one `Location` is invalid
SPIR-V. A test covers the aliased case with `spirv-val`.

Verification: `cargo build --release` clean, `cargo clippy --all-targets --all-features -D
warnings` clean, `cargo test -p ps4-gcn` 87 passed, `cargo test -p ps4-gnm` 209 passed / 7
failed — the same 7 pre-existing `cache::tests` failures that are red on main since
`5af099d`, confirmed by stashing the change and re-running. No new failures.

### Deliberately NOT done

`shader_hash` now mixes all 32 slots on the per-draw path (~256 FNV byte-rounds per GCN
shader). Packing the map into 4 `u64`s was proposed and **declined** — no optimization
until at least three titles run; optimizing against one game is overfitting.

### Remaining on this task

- AC#3 is only half-confirmed: the mountain is there, but the composited scene does not
  cover the full screen — content ends at roughly 87.8% of the width and 89.4% of the
  height. The two fractions differ, so this is NOT a uniform scale; it looks like
  **task-180** (offscreen RT extent taken as the padded pitch rather than the surface
  width), which this same investigation turned up. Chase it there, not here.
- The splash + title-screen no-regression half of AC#3 still needs a look.
- AC#5: the diagnostic probes and `UNEMUPS4_X_*` knobs are still in the working tree by
  request — they stay until the fix is confirmed, then get removed or left zero-cost
  env-gated.
