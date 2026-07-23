---
id: TASK-184
title: >-
  gnm/gcn: Celeste menu scene is uniformly over-blurred — the bloom chain
  carries the whole scene, not just its bright parts
status: Done
assignee: []
created_date: '2026-07-20 13:28'
updated_date: '2026-07-23 18:41'
labels:
  - gpu
  - gnm
  - gcn
  - celeste
  - retail
  - bloom
dependencies: []
priority: high
ordinal: 188000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
With task-179 (PS attribute routing) and task-180 (RT content extent) landed, Celeste's menu renders the 3D mountain full-screen — but the entire 3D scene is soft, as if the blurred copy were being shown instead of merely added to. Real-hardware reference: the scene is SHARP, and the bloom is a modest glow around bright areas only. GUI layers (CLIMB/Options/Credits text, snow particles) stay sharp in our output, so the defect is confined to the path that produces or composites the 3D scene, not to presentation as a whole. Leading hypothesis: the bright-pass/threshold that should restrict what enters the bloom chain is not being applied, so the bloom RT holds a blurred copy of the WHOLE scene and adding it over the sharp composite washes everything out. Alternatives not yet excluded: the sharp scene composite (draw 3 of the per-frame chain) being skipped or overwritten; the additive composite using the wrong blend weight; the bloom RT being sampled with wrong filtering when upscaled 1024->1920. DO NOT use the CLIMB title's colour as a signal — the selected menu entry pulses/animates, so its hue differs between captures for reasons that have nothing to do with this bug.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 Root-caused with evidence naming which stage of the bloom chain carries the whole scene instead of its bright parts (or refuting that framing)
- [ ] #2 Celeste's menu 3D scene renders SHARP with only a bright-area glow, matching the console reference — maintainer's live oracle
- [ ] #3 The CLIMB title text renders green, not yellow
- [x] #4 build + cargo test + clippy clean
- [x] #5 Root-caused with evidence naming which stage of the bloom chain carries the whole scene instead of its bright parts (or refuting that framing)
- [ ] #6 Celeste's menu 3D scene renders SHARP with only a bright-area glow, matching the console reference — maintainer's live oracle
- [x] #7 build + cargo test + clippy clean
<!-- AC:END -->



## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Session 2026-07-20. Snapshot-driven, no new ad-hoc probes on `exec.rs`.

### The frame, verified from the captures (16 frames, all identical)

`gpu-snapshots/frame-015{14..21}` and `frame-017{18..25}` agree field-for-field on the
whole chain. Nothing here is frame-dependent, so a single frame is representative.

```
 0     fill  RT_A 0x9b00e0000 1920x1080  blend off  PS const = [0,0,0,1]   <- OPAQUE
 1-8   scene RT_A                        blend 0x45010501
 9     fill  RT_B 0x9b1418000  960x540   blend off  PS const = [0,0,0,0]   <- transparent
10     RT_A -> RT_B   blend 0x45010501  PS 0x9afae4f00  PS const [1/960, 0,     0.75, -2.0835]
11     fill  RT_C 0x9b1658000  960x540   blend off  PS const = [0,0,0,0]
12     RT_B -> RT_C   blend 0x45010501  PS 0x9afae4f00  PS const [0,     1/540, 0.75, -2.0835]
13,14  fills videoout                    blend off  PS const = [0,0,0,1]
15     RT_A -> videoout  blend 0x45010501  PS 0x98166f700
16     RT_C -> videoout  blend 0x45010501  PS 0x98166f700
17-22  GUI
```

### THE FINDING — the blur PS is not a plain blur, it carries a radial attenuation

`0x9afae4f00` decoded from its raw dwords (the in-tree disassembler does NOT print VOP3
`neg`/`abs` modifiers, which is why previous reads of this shader missed the sign of half
its operands — see "disassembler gap" below). Its true dataflow:

```
uv      = attr0.xy
taps    = uv-4s, uv-2s, uv, uv+2s, uv+4s        (s = const.xy, one axis per pass)
blur    = 0.15*T(-4s) + 0.2*T(-2s) + 0.3*T(0) + 0.2*T(+2s) + 0.15*T(+4s)   [sum = 1.0]
d       = length(uv - 0.5)
v20     = const.z * (1 - d)                      = 0.75*(1-d)      [v_mad_f32 s16, -v12, s16]
out.rgba = blur.rgba * (1 - v20) = blur.rgba * (0.25 + 0.75*d)     [v_mac_f32 v20, -v, dst]
```

The attenuation is applied to **all four channels including ALPHA**, and it is applied in
BOTH passes. So, taking RT_A.a = 1 (confirmed by the `[0,0,0,1]` fill above and by the
maintainer in RenderDoc):

| screen position | `d` | per-pass factor | RT_C alpha after two passes |
|---|---|---|---|
| centre  | 0.0   | 0.25 | **0.0625** |
| edge midpoint | 0.5 | 0.625 | 0.39 |
| corner  | 0.707 | 0.78 | 0.61 |

This is the missing piece of task-179's central contradiction ("every input we can measure
says hardware should wipe the frame exactly like we do"). It does not wipe, because the
guest's own blur shader attenuates alpha down to ~6% where the mountain is. The
premultiplied composite at draw 16 is therefore ~94% additive at screen centre — a glow —
and only becomes a partial replace toward the edges. **No bright-pass / threshold exists
anywhere in this chain** (the task title's framing is wrong: the bloom deliberately carries
the whole scene, attenuated radially, not just its bright parts).

### The handed-down hypothesis: right in shape, wrong in mechanism

"Alpha out of the bloom chain decides add-vs-replace" is CORRECT and is confirmed by the
composite PS `0x98166f700`, which is `out = texture * vertex_colour` per channel, alpha
included — with vertex colour measured `ffffffff` for both composites (task-179), so
`out.a = RT_C.a` exactly.

But "if our bloom RT carries alpha ~1, draw 16 replaces" is not a defect of ours *by
construction*: the guest shader intends alpha well below 1. The question is not "why is the
alpha 1" but "why is the guest's attenuation not reaching our RT".

### Refuted BY MEASUREMENT this session

- **"Our GCN->SPIR-V drops the VOP3 `neg` modifiers / mis-lowers the vignette."** Refuted.
  Recompiled `0x9afae4f00` offline and read the emitted SPIR-V (`spirv-val` clean, 2176
  words): `%340 = OpFMul %334 %338` where `%338 = OpFNegate %335`, `%341 = OpFAdd %340 %339`
  — `v0*(1 - v20)` exactly, emitted for all four channels. The radial term, its `OpExtInst
  Sqrt`, and the negation are all present and correct.
- **"The `s_buffer_load` offset is misread."** Refuted. Raw SMRD dword `0xc2080d02` decodes
  OP=8 (`S_BUFFER_LOAD_DWORD`), SDST=16, SBASE field 6 (= s[12:13]), IMM=1, OFFSET=2. Our
  decoder and `emit_s_buffer_load` both treat the SI/CI immediate as a DWORD index, and the
  SPIR-V indexes `%uint_2` — i.e. `const.z = 0.75`. Correct.
- **"The bloom RTs are cleared opaque."** Refuted. Draws 9 and 11 fill them with
  `[0,0,0,0]`; only RT_A and videoout are filled `[0,0,0,1]`. (Also kills the reading of
  `UNEMUPS4_X_RT_CLEAR_ALPHA0` as evidence about the guest's intent.)
- **"The guest programs a different blend for draw 16 than for draw 15."** Not observed:
  `0x45010501` with write mask `0xf` on both, in all 16 captures.
- **"The blur weights are wrong / the kernel is asymmetric."** Refuted; weights are
  `.15/.2/.3/.2/.15`, taps symmetric at `+-2s` and `+-4s`, sum exactly 1.0.
- **The 4th PS constant (`-2.083496`) is never read by this shader.** Only `.x`, `.y`
  (texel step, one axis per pass) and `.z` (0.75) are loaded.

### Where that leaves it

Every static link in the chain checks out: guest data (const buffers, clears, blend, masks),
our decode, our SPIR-V. If the attenuation reached the render target, the picture would be
sharp at centre and progressively softer toward the edges — a *vignette-shaped* softness.
The reported symptom is **uniform** softness, and the only free parameter that turns
6% into ~100% *uniformly* is the radial term collapsing to 1, i.e. `const.z` arriving as 0
at the shader. That points at the runtime delivery of the PS scalar constant buffer
(set 0 / binding 6, FRAGMENT) for a pipeline that ALSO binds a sampler and a VS constant
buffer — a combination the clear PS (`0x981400000`, const buffer but no sampler) does not
exercise, and the clear PS demonstrably works. Not proven; stated as the next thing to test,
not as a finding.

### Snapshot extensions landed (task-185 tool, gaps closed)

1. **Guest T#/S# on the RT-as-texture path.** `TextureSource::RenderTarget` now carries the
   resolved `TextureBindingRange` alongside the registered RT; the bind path ignores it
   exactly as before. `sampled.t_sharp`/`s_sharp` are now populated for RT binds too, plus a
   new `descriptor_honoured` flag so a recorded descriptor can never be mistaken for one
   that was applied. This makes visible what the guest ASKED for (format, extent, filter)
   versus the RGBA8 + linear/repeat we substitute.
2. **Per-draw PS attribute routing.** `ps.ps_input_map` is the `SPI_PS_INPUT_CNTL_n`-derived
   attr-slot -> VS-param map the draw resolved. This is genuinely invisible today:
   `registers.json` is the END-OF-FRAME register file, so it only ever shows the last draw's
   routing. It matters acutely for `0x9afae4f00`, which derives BOTH its sample coordinate
   and its radial scalar from `attr0` — an off-by-one routing there changes the picture
   without changing any other recorded field.

### Disassembler gap (recorded, not fixed — out of scope here)

`ps4_gcn::disasm` prints VOP3 operands without their `neg`/`abs` modifiers, so
`v_mad_f32 v1, 2.0, -s13, v17` prints as `... 2.0, s13, v17`. Every negative tap offset and
the entire sign of the radial term are invisible in its output. task-179's reading of this
same shader was done from that output. The decoder and recompiler are correct; only the text
rendering drops the modifiers. Worth its own task.

### The one decisive observation to make next (maintainer, one RenderDoc click)

At draw 16 (the `RT_C -> videoout` composite), view **RT_C's ALPHA channel alone**.

- Alpha near-black in the middle, brightening smoothly toward the edges (a radial ramp
  roughly 0.06 -> 0.6) => the guest's attenuation IS reaching the render target, the bloom
  chain is faithful, and the uniform softness must come from somewhere else entirely
  (this section's whole line of reasoning is then dead and should be recorded as such).
- Alpha uniformly white => the attenuation is being lost between the SPIR-V (verified
  correct) and the pixels, which puts the PS scalar constant buffer's runtime delivery
  (set 0 / binding 6, FRAGMENT, on a pipeline that also binds a sampler and a VS constant
  buffer) as the prime and near-only suspect.

Everything else in this task hangs off which of those two it is, and no amount of further
static reading can decide it.

## Session 2 — RenderDoc result, a corrected prediction, and a real defect found

### The snapshot gap reported by the coordinator does not exist

Constant-buffer contents were ALREADY recorded, and have been since task-185: they live in
the per-draw `const_buffers` array, not in `buffers` (which holds vertex/index V# ranges
only). `frame-01610` — captured with the session-1 extensions — reads, for the blur passes:

```
draw 10  CB pixel  0x90244893c  size 16  dumped 16  truncated false  read_failed false
         floats [0.0010416667, 0, 0.75, -2.083496]        (= 1/960, 0, 0.75, ...)
draw 12  CB pixel  0x902449144  size 16  dumped 16  truncated false  read_failed false
         floats [0, 0.0018518518, 0.75, -2.083496]        (= 0, 1/540, 0.75, ...)
```

Bounded read, explicit `truncated` and `read_failed` flags: exactly the discipline asked
for. No re-capture was needed and none was taken.

**So: `const.x` and `const.z` in GUEST MEMORY are 1/960 and 0.75 — precisely what the guest
intended.** The "constant buffer delivering the wrong dwords" theory is dead AT THE SOURCE.

### Correction to the refined prediction

The proposed refinement — that `const.z = 0` yields `0.25 + 0*d = 0.25` and therefore could
not produce a full replace — substitutes into an already-specialised expression. The general
form is:

```
factor = 1 - const.z * (1 - d)
```

`0.25 + 0.75*d` IS that expression with `const.z = 0.75` already substituted. Setting
`const.z = 0` gives `factor = 1 - 0 = 1.0`, uniformly, over the whole quad. `const.x` never
enters the factor at all — it is only the texel step. So the session-1 prediction stands
unchanged and needs no extra condition: **`const.z` arriving as 0 is on its own sufficient
for the observed outright replace.**

It also predicts the *character* of the blur, which is a sharper test. With `const.x/.y`
also zero (an all-zero SSBO), all five taps collapse onto the same `uv` and the weights
still sum to 1.0, so each pass degenerates into a pure resample: RT_A 1920x1080 -> RT_B
960x540 -> composited back up to 1920x1080. That is a MILD, uniform softness — a
half-resolution image shown at full size. A correctly-delivered constant buffer would give a
5-tap gaussian spanning `+-4/960` (~+-8 source pixels), which is a much wider, obviously
smeary blur. The two are trivially distinguishable by eye on RT_C.

### Defect found and FIXED — undefined-descriptor UB on exactly this draw shape

`crates/gpu/src/backend.rs`: `HostPipeline::needs_const` was a **bool** built as
`const_storage.is_some() || const_storage_fragment.is_some()`, and the per-draw guard tested
it against `!const_binds.is_empty()`. A pipeline declaring BOTH constant buffers therefore
passed the guard with only ONE resolved, recorded the draw, and left the other descriptor
**unwritten** — undefined-descriptor UB. On a typical driver an unwritten storage-buffer
descriptor reads as zeros, which is exactly the `const.z = 0` failure predicted above.

Draws 10 and 12 are the only draws in the frame that declare both (`const_storage`
set0/bind2 VERTEX + `const_storage_fragment` set0/bind6 FRAGMENT) alongside a sampler — and
they are the two blur passes. The clear PS (`0x981400000`) declares a PS constant buffer but
no VS one, which is why it was never affected and its `[0,0,0,1]` fill always worked.

`needs_storage` had the identical bool-vs-count shape and the same consequence for the
3-stream vertex pull (task-153), so both are now counts:

```rust
needs_const:   u32 = const_storage.is_some() as u32 + const_storage_fragment.is_some() as u32
needs_storage: u32 = vertex_storage.len() as u32
guard: has_const < needs_const  ->  drop the draw (and now WARN with needs/got)
```

Covered by `draw_guards_ok_requires_every_declared_binding_not_just_one`.

**This is NOT yet proven to be the cause.** It is a genuine defect, it sits on exactly the
suspect path, and its failure mode matches the prediction — but whether the PS const bind is
actually being dropped at runtime is unmeasured. If it IS firing, the fix changes the symptom
rather than curing it: the blur draws will now DEFER (with a `[GPU] dropping draw: pipeline
needs constant buffers but not all of their V# resources resolved` warning naming needs=2
got=1), which removes the bloom composite entirely and should produce the sharp mountain —
the same picture `UNEMUPS4_X_SKIP_BLOOM=1` produced in task-179. **Watch the log for that
warning on the next run: its presence or absence is the measurement.**

### Second defect recorded (real, probably not this bug) — the S# is discarded on the RT path

Now visible thanks to the session-1 extension. `descriptor_honoured: false` on draws 10, 12,
15 and 16, and the guest's S# differs by role:

| draws | guest S# | we substitute |
|---|---|---|
| 10, 12 (blur) | `bilinear: false`, clamp X/Y `ClampToEdge` | Linear + **Repeat** |
| 15, 16 (composite) | `bilinear: true`, clamp X/Y `ClampToEdge` | Linear + **Repeat** |

Two independent errors: the filter is wrong on the blur passes (guest asked POINT), and the
wrap mode is wrong on ALL FOUR (guest asked ClampToEdge, we use Repeat — so every tap that
runs off an edge wraps to the opposite side instead of clamping). Worth its own task.

Note these COMPOUND with the hypothesis above: under an all-zero constant buffer the blur
degenerates to a resample, and it is precisely our forced LINEAR filter that turns that
resample into something *soft* rather than *aliased*. Both defects are needed to produce the
exact picture observed.

### Independent confirmation of task-180

The guest's own T# — a source that is not the viewport — reports RT_A as 1920x1080 pitch
1920, and both bloom targets as 960x540 with pitch **1024**. The content extent really is
960, not the padded pitch, so task-180's viewport-derived extent is right.

### The one observation that now decides it

On the capture already in hand (frame #1715, EID 290), look at **RT_C itself**:

- RT_C is a *mild* half-resolution-looking copy of the scene, and its ALPHA is uniformly
  white => the constant buffer is arriving as zeros at the shader, the guard defect above is
  firing, and the fix should cure it.
- RT_C is a *wide, smeary* gaussian, and its alpha is a radial ramp (dark centre, brighter
  edges) => the constants ARE arriving, the attenuation IS being applied, the guard defect is
  a real but unrelated bug, and this whole line of reasoning is dead — record it as such and
  start again from the composite.


## Session 3 — const-delivery hypothesis DEAD; live-module verification closed

### `needs_const` guard: real defect, NOT this bug

Measured: 73788-line run log contains ZERO occurrences of "dropping draw" (any variant),
against plenty of other WARN lines — so the absence is a measurement, not a log-level
artefact. On draws 10 and 12 `has_const == needs_const`: both descriptors resolve, both are
written, and `const.z = 0.75` reaches the shader. **The const-delivery hypothesis is dead.**
The bool->count fix stays on its own merits (an unwritten descriptor is UB waiting to
happen) but it is not the cause and must not be reported as one.

### The hole in session 1's verification — found, checked, CLOSED

Correctly identified: session 1 verified the SPIR-V from an OFFLINE `recompile()` with the
DEFAULT identity `PsInputMap`, while the snapshot records `ps_input_map[0] = 1` on draws 10
and 12 (identity everywhere else in the frame). Since the map is part of the shader cache
key, that is a different module — session 1 verified a module that does not run.

Redone with `recompile_with()` under the recorded map and diffed against the identity build.
**The two modules differ in exactly one word:**

```
16c16
<   OpDecorate %40 Location 0
---
>   OpDecorate %40 Location 1
```

Everything else is byte-identical, and the live module is `spirv-val` clean. So every
conclusion session 1 drew about the dataflow — the 5-tap kernel, the `Sqrt`, the
`OpFNegate`/`OpFMul`/`OpFAdd` radial term on all four channels — holds for the module that
actually executes.

The routing ALSO cannot selectively break the radial term, and this is structural rather
than a matter of luck: `%40` is the module's **only** `Input` variable, and all four
`OpLoad`s — the ones feeding the tap coordinates AND the one feeding `length(uv - 0.5)` —
read from it. Both consumers are fed by the same load of the same variable. A routing error
moves both together or neither; it cannot produce a correct blur with a missing attenuation.

### Everything in the chain is now verified. Nothing is left to read.

| link | status | how |
|---|---|---|
| guest constant data (1/960, 0, 0.75) | correct | 17 captures, bounded read, `read_failed: false` |
| constants delivered to the shader | correct | guard never fires in a 73788-line log |
| our lowering of the blur PS | correct | SPIR-V read; live module diffed to 1 decoration |
| PS attribute routing | cannot be the cause | single `Input` var feeds both consumers |
| bloom RT clears `(0,0,0,0)` | correct | draws 9/11, blend off, mask `0xf` |
| blend on draws 15 and 16 | identical `0x45010501` | 17 captures |
| `CB_SHADER_MASK` / `CB_TARGET_MASK` / `CB_COLOR_CONTROL` | not the cause | task-179 |
| composite vertex colour `ffffffff` | not the cause | task-179 |
| RT DAG / barriers | correct | task-179 `PASS_TRACE`, `X_FULL_BARRIER` |
| draw 15 output SHARP | confirmed | RenderDoc EID 278 |
| **RT_C's actual alpha** | **NEVER OBSERVED** | — |

Every input to RT_C's alpha is verified correct and the shader computing it is verified
correct, yet EID 290 demonstrably replaces. The single quantity the entire argument rests on
has only ever been INFERRED. That is the hole, and no further static reading can close it —
I am not going to reach for another mechanism just to have one.

### Exactly what to look at (capture already exists: Frame #1715)

Select **EID 290**, open its **Inputs**, pick the sampled texture — the bloom target
`0x9b1658000`, 960x540 — and view it two ways.

**(a) Alpha channel alone.** The prediction is precise and falsifiable:

| position | predicted alpha | 8-bit |
|---|---|---|
| screen centre | 0.0625 | ~16 |
| edge midpoint | 0.39 | ~99 |
| corner | 0.61 | ~155 |

- A radial ramp near those values => the attenuation IS reaching the RT, the bloom chain is
  faithful, and the replace comes from the composite/blend stage instead. Everything in this
  task's reasoning about the bloom chain is then dead and should be recorded as such.
- Uniformly white (~255) => the attenuation is lost between a verified-correct shader and
  the stored texel, which puts the export path, the offscreen render pass, or the blend on
  draws 10/12 in frame — none of which has been examined yet.

**(b) RGB.** Is it a WIDE, smeary gaussian (taps spanning ~+-8 source pixels), or a MILD
half-resolution-looking copy? That independently reports whether `const.x` (the texel step)
is reaching the shader, using the picture rather than a descriptor as the witness.


## Session 4 — bloom-chain COMPUTATION line is DEAD; both write-path leads are clean negatives

**RT_C alpha at EID 290 is uniformly WHITE (maintainer, RenderDoc).** The predicted radial
ramp (0.06 centre / 0.39 edge / 0.61 corner) is not there. The bloom-chain-computation line
of investigation — everything in sessions 1-3 that reasoned from "the attenuation should
reach the RT" — is hereby **DEAD**. It was correct about what the shader computes and wrong
about what lands.

### Lead 1 — offscreen RT image format / view swizzle: CLEAN. Defect is at WRITE, not creation.

Established, not assumed:

- `create_render_target` (`backend.rs`) calls `vk_color_format(ColorFormat::R8G8B8A8Unorm)`
  -> `vk::Format::R8G8B8A8_UNORM`. The format has genuine 8-bit alpha storage.
- `create_render_target_image` (`vulkan.rs`) builds the view with
  `..Default::default()` for `components`, i.e. `ComponentMapping` all
  `COMPONENT_SWIZZLE_IDENTITY` (the zero value). `aspect_mask: COLOR`, one mip, one layer.
  **No swizzle substitutes ONE for alpha**, on the attachment view or the sampled view —
  and they are the same view object (`bind_texture_or_rt` resolves an RT id to
  `CacheRenderTarget.view`).
- The offscreen render pass attachment (`create_rt_target`) is `EMBEDDED_TARGET_FORMAT` =
  `R8G8B8A8_UNORM`, `store_op: STORE`. Alpha is stored, not discarded.

So the image genuinely HAS alpha and the stored bytes really are `0xFF`. **The defect is at
write.**

### Lead 2 — MRT export lowering for alpha: CLEAN.

In the module that actually runs (`ps_input_map[0] = 1`, diffed in session 3):

```
%387 = OpVariable %_ptr_Output_v4float Output      ; Location 0, FOUR components
%385 = OpCompositeConstruct %v4float %379 %380 %383 %384
       OpStore %387 %385
```

All four components reach the fragment output in a single store. `IoLayout.outputs` reports
`components: 4`. Nothing in the export path forces, masks or substitutes alpha; the COMPR
handling round-trips correctly (`PackHalf2x16` of `(v0,v1)` and `(v2,v3)` then
`UnpackHalf2x16`, matching `v_cvt_pkrtz_f16_f32` + `exp compr` operand order — verified
register-by-register). `blend_attachment_state` derives `color_write_mask` from
`CB_TARGET_MASK` = `0xf` -> `R|G|B|A`. **Alpha survives the export.**

### Also closed on the way: constant-buffer CONTENTS (not just delivery)

Session 3's measurement proved a VkBuffer was BOUND at binding 6; it did not prove the
buffer's bytes were current, which is exactly the task-178 stale-hit shape. Checked:
`cache/mod.rs` classifies `ResLayout::ConstBuf` as `dynamic_copy` and **force-re-uploads on
every hit** (the task-178 workaround for dead x86jit dirty tracking). So the SSBO holds the
guest bytes. `const.z = 0.75` genuinely reaches the shader. That gap is closed too.

### The contradiction, stated precisely

RGB and alpha are produced by the SAME four `v_mac_f32` instructions from the SAME `v20`:

```
v0 = v20*(-v0) + v0     ; R
v1 = v20*(-v1) + v1     ; G
v3 = v20*(-v3) + v3     ; A
v2 = v20*(-v2) + v2     ; B
```

**It is not structurally possible for the radial term to be applied to RGB but not to
alpha.** Yet EID 290's output is a recognisable blur of the scene (so RT_C.rgb IS a correct
blur, so the sample coordinate and therefore `uv` and therefore `d <= 0.707` are right)
while RT_C.a is 1 (which needs `f = 1`, i.e. `d = 1` everywhere). Those two observations
cannot both be products of this shader.

So one of them is not describing what we think it is. **I am not going to invent a third
mechanism to reconcile them.** The way out is one more observation, and it is cheap.

### The observation that splits it (same capture, Frame #1715)

Two adjacent EIDs, both already in the capture:

1. **RT_C alpha immediately after the CLEAR draw** (our draw 11 — the `DrawIndexAuto`
   count=3 into `0x9b1658000`, blend off, PS `0x981400000`, PS const `[0,0,0,0]`).
   - alpha already 1 here => **the guest's clear is not landing in the RT**, and the alpha
     we see is accumulated, not computed. Frame-over-frame accumulation under
     premultiplied-over (`a <- s + a(1-s)`) converges to 1 for any `s > 0`, and RGB
     converges to a BRIGHTENED blur — which would also explain task-179's white blow-out
     under `X_ADDITIVE`. The hunt then moves to why the clear does not land.
   - alpha 0 here => the clear works, and the very next draw is the culprit.
2. **RT_C alpha immediately after the BLUR draw** (our draw 12). If (1) is 0 and (2) is 1,
   the divergence is inside a single draw whose shader provably cannot do that, which would
   point at the blend/attachment state actually programmed for that pass rather than at
   anything computational.

3. Still unanswered from session 3 and worth one glance while there: **RT_C's RGB** — a
   WIDE smeary gaussian (taps ~+-8 source pixels) or a MILD half-resolution-looking copy?
   That independently reports whether `const.x` reaches the shader, using the picture rather
   than a descriptor as the witness.

<!-- SECTION:NOTES:END -->

## DECISIVE — the guest's clear does not land (2026-07-20, maintainer, Frame #1740)

RenderDoc, Colour Pass #3 (EID 232-251), **EID 238** — the fill/clear draw into RT_C
(`vkCmdDraw(3,1)`, our draw 11; `blend enable=false`, PS `0x981400000`, constant buffer
`[0,0,0,0]`). **Its output still shows the blurred scene.** The target is 960x540 and the
mountain is plainly visible in it after the draw that is supposed to zero it.

So the clear is submitted (RenderDoc shows the draw) but does not clear.

**This reconciles the contradiction that stalled the investigation.** RGB and alpha come from
the same four `v_mac_f32` instructions on the same `v20`, so the radial attenuation cannot hit
one and miss the other — yet EID 290 showed a recognisable blur with alpha at 1. It can, if
the alpha is not COMPUTED per frame but ACCUMULATED across frames: under premultiplied-over
into a target that is never zeroed, `a ← s + a(1-s)` converges to 1 for any `s > 0`, and RGB
converges to a brightened blur that still looks like a blur. Both observations are then
consistent with a correct shader and a broken clear.

It also retro-explains task-179's `UNEMUPS4_X_ADDITIVE=1` blowing the screen white.

### Dead as of this observation

The entire bloom-chain-computation line: the constants, their delivery, the lowering, the
attribute routing, the export path, and the render-target format/view. All were verified
correct, and this explains why they could be correct while the picture was wrong.

### Next question — why does a submitted, blend-disabled, full-screen fill not write?

Candidates, none yet tested:
- the fill draw's VS (`0x981400100`) producing a degenerate or off-screen triangle, so nothing
  is rasterised — check the Mesh Viewer at EID 238;
- viewport/scissor for that draw not covering the target;
- the colour write mask or depth/stencil state rejecting the fragments;
- the render pass being `LOAD` (EID 233 confirms `vkCmdBeginRenderPass(Load)`) combined with
  the fill never writing, so the previous frame's contents simply survive.

Note the draw IS recorded in our snapshot (draws 9, 11, 13, 14) and is NOT in the deferred
list, so it is not being dropped by our guards — it reaches Vulkan and produces nothing.

## ROOT CAUSE — the fill VS never reads the vertex index, so its triangle is degenerate

Found by reading the SPIR-V the snapshot tool dumps (`shaders/vs-4eb8fe0d640b94e5.spv`),
i.e. the module that actually ran — not the disassembly, which carries a task-182 warning
that it drops VOP3 modifiers and would not have settled this.

```
OpEntryPoint Vertex %11 "main" %gl_Position %70        <- no gl_VertexIndex in the interface
%15 = OpVariable %_ptr_Function_uint Function %uint_0  <- VGPR0 modelled as a local, init 0
%16 = OpLoad %uint %15
%17 = OpBitwiseAnd %uint %uint_1 %16
```

On GCN, `v0` in a vertex shader IS the vertex index (launch ABI). We model it as a
function-local initialised to zero and never bind the builtin, so all three vertices of the
fill draw evaluate at index 0 and collapse onto `(-1,-1,0,1)`. A zero-area triangle
rasterises nothing: **the guest's full-screen fill writes nothing, ever.**

The guest shader is the standard index-derived full-screen triangle:
`x = (idx & 1) * 2 - 1`, `y = (idx & ~1) - 1`.

### Why this produced exactly the observed picture

RT_B and RT_C are never cleared. Their render pass is `LOAD` (RenderDoc EID 233), so nothing
else zeroes them either. Under the guest's premultiplied-over blend, `a <- s + a(1-s)`
converges to 1 across frames for any `s > 0`, and RGB converges to a brightened blur that
still reads as a blur. The bloom composite then arrives with alpha ~1 and REPLACES the sharp
scene instead of adding a glow.

**Videoout hides the same defect** because its render pass uses `loadOp Clear` (EID 253) —
Vulkan clears it for us, so the broken fill there costs nothing visible. That asymmetry is
why the bug looked like a bloom problem rather than a fill problem.

### Why every earlier hypothesis was correct and still wrong

The constants, their delivery, the lowering, the attribute routing, the export path and the
render-target format/view were all verified correct — and all of them were. The defect was
never in the bloom chain's computation; it was that the target it accumulates into is never
reset. An investigation anchored on "the attenuation is being lost" could not find it,
because nothing was being lost.

### Fix shape

`crates/gcn/src/recompile.rs` already models this: `vertex_index: Option<spirv::Word>` plus
tracking of "VGPRs currently known to carry the launch vertex index". The fetch-shader VS
path establishes it. A VS that reads `v0` DIRECTLY — no `s_swappc`, no fetch shader, which is
exactly this fill shader — never seeds it. Seed VGPR0 from `gl_VertexIndex` at VS entry.

Mind the documented DRAW-MODE ASSUMPTION at `recompile.rs:351`: a recompiled VS fetches by
`gl_VertexIndex`, which is correct for non-indexed `vkCmdDraw` but index-buffer driven for
`vkCmdDrawIndexed`. The fill draws are `DrawIndexAuto` (non-indexed), so they are the safe
case, but the seeding must not silently change behaviour for the indexed path.

## FIX (2026-07-20) — VS launch ABI: `v0` now resolves to `gl_VertexIndex` on a direct read

`crates/gcn/src/recompile.rs`. The recompiler already tracked "VGPRs currently known to
carry the launch vertex index" (`vertex_index_regs`, seeded with `v0` for a VS) but only
CONSULTED that set at the `idxen` MUBUF vertex fetch. A VS that reads `v0` as a plain ALU
source — no fetch shader, exactly the index-derived full-screen-fill shape — fell through
to the generic register model and read the zero-initialized `v0` slot.

The fix extends the existing mechanism rather than adding a second one: the tracker is now
consulted at the two register-read chokepoints, `load_reg_u32` / `load_reg_f32` (via
`launch_vertex_index`), so ANY read of a still-tracked reg resolves to `gl_VertexIndex`.
The idxen path is unchanged (it already resolved through the same set). The f32 view
bit-reinterprets the index, matching the oracle's `read_f32_lane`. `vertex_index_regs` is
empty for a Fragment shader, so this is inert outside a VS.

Also closed a hole the change widened: the MUBUF fetch destination did not untrack, so a
`buffer_load_format_x v0, v0, …` would have kept `v0` resolving to `gl_VertexIndex` on
every later read.

Draw modes: `DrawIndexAuto` → `vkCmdDraw(count, 1, 0, 0)`, so `gl_VertexIndex` = 0,1,2 —
what the guest's `v0` holds. `DrawIndex2` → `vkCmdDrawIndexed`, where `gl_VertexIndex` is
index-buffer driven; that is ALSO what GCN's VGT delivers in `v0` for an indexed draw, so
the change is correct there too and alters nothing about the existing indexed path. The
DRAW-MODE ASSUMPTION comment (now at `recompile.rs:351`) is about agreement with the
*interp oracle* (which has no index buffer), not hardware; it was extended to say the same
rule covers direct `v0` reads.

Interpreter: NO gap. `interp.rs` already seeds `st.vgprs[0][lane] = first_vertex + lane`
(`LaunchAbi::Vertex`). The recompiler was the asymmetric side; the two now agree.

### Tests

- `crates/gcn/tests/corpus/index_tri_vs.{s,code.bin}` — new self-authored corpus VS (no
  vertex buffer, no fetch shader) deriving position from `v0` arithmetically. Assembled
  with llvm-mc and byte-verified against its `-show-encoding` output (NOT a blanket
  `regen.sh` run — see task-190 drift). No `.sb`/`.dis` (same as `nonstd_stride_vs`).
- `differential.rs::recompiled_spirv_matches_oracle` — analytic spec for `index_tri_vs`
  with `first_vertex: 1` (so ignoring the index cannot pass by coincidence). Interp oracle
  vs CPU-evaluated recompiled SPIR-V, bit-for-bit.
- `recompile.rs::vs_reading_v0_directly_binds_gl_vertex_index` — module-level assertion:
  `gl_VertexIndex` is decorated, IS in the `OpEntryPoint` interface, is loaded, and its
  load feeds the `OpBitwiseAnd`. Module level because the whole bug was invisible in a
  casual disassembly read.

Both tests verified non-vacuous: with the fix reverted, the differential fails
`index_tri_vs: lane 0 Pos(0) ch0 — CPU-SPIR-V -1 != oracle 1` and the module test fails on
the missing builtin.

`cargo build --release`, `cargo test --workspace` (514 passed / 0 failed), `cargo clippy
--all-targets --all-features -- -D warnings`, `cargo fmt` all clean.

### NOT verified — maintainer's eyes required

Nobody has seen Celeste's menu with this fix; the emulator was not run. What should change
on screen if it worked: the guest's full-screen fill draws (0, 9, 11, 13, 14) now actually
write, so the bloom targets RT_B/RT_C are cleared to transparent each frame instead of
accumulating alpha to saturation. The 3D mountain scene should render SHARP, with the
bloom present only as a modest glow around bright areas — not as a blurred copy replacing
the scene. If the scene is still uniformly soft, this was a real bug but not the whole one.

## Session 5 — the fill VS fix was incomplete, and there was a SECOND fill defect

The task-184 launch-ABI fix was real and reached the GPU, but the fill draws still wrote
nothing. Two further defects, both required. Both fixed; both regression-tested
non-vacuously.

### Refuted BY MEASUREMENT this session

- **"The VERTEX constant buffer lands at set0/binding 6 instead of the PIXEL one."**
  REFUTED, from the pictures. `frame-01579` RT_C fits a five-tap vertical blur of RT_B at
  ±2/±4 rows with **RMSE 1.62/255**; the alternatives fit far worse:

  | candidate kernel | implied `const` | RMSE |
  |---|---|---|
  | vertical ±2, ±4 (intended) | pixel CB, `const.y = 1/540` | **1.62** |
  | identity (no blur) | all-zero SSBO | 3.31 |
  | horizontal ±2, ±4 | — | 3.07 |
  | horizontal ±4, ±8 | vertex CB, `const.x = 1/480`, `const.y = 0` | 3.76 |

  Under the vertex CB, draw 12's vertical pass would have `const.y = 0` and blur
  HORIZONTALLY. It does not. Dwords 0 and 1 of the PIXEL buffer demonstrably reach
  binding 6.

- **The stale comment at `exec.rs:566` is a doc bug, not a live signal.** It is superseded
  in place by the task-174 paragraph directly beneath it (lines 573-578). Verified through
  the whole chain: the recompiler assigns binding by `self.stage` (`PS_CONST_BUFFER_BINDING
  = 6` for Fragment, `CONST_BUFFER_BINDING = 2` otherwise); `vs_const`/`ps_const` each
  carry their OWN `(ConstBufferBinding, range)` pair; `BindConstBuffer` uses `cb.binding`
  from that pair; the backend writes one descriptor per bind at its own binding. There is
  no site where the two can swap and none where binding 6 is written twice.
  Draws 10/12 are not deferred and the frame's deferred list is empty — as the coordinator
  observed, correctly, but the comment was simply stale.

- **"RT_C's RGB tells you whether the clear landed."** REFUTED as a method. Under
  premultiplied-over into a never-zeroed target the fixed point is `dst = f·blur/f = blur`
  — a *correct* blur, with no brightening or saturation. "Clear works + attenuation lost"
  and "clear fails + attenuation applied" predict the SAME RGB and the same alpha of 1.
  Session 4's reconciliation was right for a reason it did not state.

### ROOT CAUSE 1 — the launch index resolved on the FIRST read only

Read out of the module that actually ran (`gpu-snapshots/frame-01579/shaders/vs-4eb8fe0d640b94e5.spv`,
394 words = post-fix):

```
%16 = OpLoad %uint %gl_VertexIndex        ; v_and_b32 v1, 1, v0   -> RESOLVED
%17 = OpBitwiseAnd %uint %uint_1 %16
%25 = OpLoad %uint %24                    ; v_and_b32 v0, -2, v0  -> reads a
%26 = OpBitwiseAnd %uint %uint_4294967294 %25   ;   zero-initialized Function slot
```

`gl_VertexIndex` was declared, decorated, in the entry-point interface and loaded — and
the Y coordinate of all three vertices was still pinned to -1. Zero-area triangle again.

Mechanism: every ALU emitter untracks its destination as a launch-index carrier BEFORE
evaluating its source operands (`emit_vop2` line 1932, and the same shape in VOP1/VOP3),
and a tracked register lives ONLY in `vertex_index_regs` — its slot still holds the zero
initializer, because the index is materialized on demand at each read. An in-place update
(`dst == src`) therefore reads zero.

`index_tri_vs`, the corpus shader added for the previous fix, writes a DIFFERENT VGPR on
its second read of `v0`, so the differential harness was silent about the shape that
actually shipped.

**Fix** (`crates/gcn/src/recompile.rs`): `untrack_vertex_index(n)` spills the builtin into
the register slot before removing it from the tracker, so the generic path is correct at
every later read including the clobbering instruction's own source. Used at all eleven
untrack sites plus the MUBUF fetch destination. `V_MOV_B32` was reordered to evaluate its
source first, then untrack, then store (a self-move must read the index).

### ROOT CAUSE 2 — `VGT_PRIMITIVE_TYPE` = RECTLIST was never modelled

From the snapshot's per-draw register delta (uconfig `0xC242`), `frame-01579`:

| draws | `VGT_PRIMITIVE_TYPE` |
|---|---|
| 0, 9, 11, 13, 14 (the fills) | `0x11` = `DI_PT_RECTLIST` |
| everything else | `0x04` = `DI_PT_TRILIST` |

The repo had ZERO references to `VGT_PRIMITIVE_TYPE`; topology was hardcoded
`TRIANGLE_LIST` at `backend.rs:2565`. A GCN rect list takes three vertices per
RECTANGLE — `p0`, `p1`, `p2` name three corners, hardware synthesizes the fourth as
`p2 + p1 - p0`. Celeste's fill VS emits `(-1,-1)`, `(1,-1)`, `(-1,1)`: as a rect list the
full screen, as a triangle list the lower-left HALF. So even with root cause 1 fixed the
clears would have covered half their target.

`registers.json` shows only `0x04` because it is written at end of frame — the same
end-of-frame blindness task-179 hit. The per-draw delta is what made it visible.

**Fix**: `PipelineKey` gained `topology`, derived from `VGT_PRIMITIVE_TYPE`; a rect list
builds a triangle-STRIP pipeline and a non-indexed rect draw is issued with FOUR vertices,
whose strip triangles `(v0,v1,v2)` and `(v1,v2,v3)` tile the same parallelogram. The
fourth vertex comes from the stream at index 3 rather than the hardware's corner
synthesis; the two coincide for the index-derived fill idiom (index 3 → `(1,1)`, exactly
`p2 + p1 - p0`) and diverge for a vertex-fetched rect list — documented on the enum. Every
other primitive type keeps triangle list, so this cannot regress a title that never sets
the register. Indexed rect lists are out of scope (none observed; they would need the
expansion in the index buffer).

### Snapshot extension

`draws.json` now records the per-draw `topology`. It was the one field that distinguished
a rect fill from a 3-vertex triangle draw and it appeared nowhere in the file.

### Tests

- `crates/gcn/tests/corpus/index_tri_inplace_vs.{s,code.bin}` — new self-authored corpus
  VS whose body is **byte-identical** to Celeste's fill VS (`v_and_b32 v1, 1, v0` /
  `v_and_b32 v0, -2, v0` / `v_mad_u32_u24` / `v_add_i32` / two `v_cvt_f32_i32`). Assembled
  with llvm-mc and byte-compared against the dumped `.sb`; NOT a blanket `regen.sh` run
  (task-190).
- `differential.rs` — analytic spec with `first_vertex: 1`. Reverting the spill fails with
  `lane 1 Pos(0) ch1 — CPU-SPIR-V -1 != oracle 1`.
- `recompile.rs::every_read_of_v0_resolves_not_only_the_first` — a def-use walk asserting
  the `-2`-masked `OpBitwiseAnd` consumes a value originating from `gl_VertexIndex`,
  directly or through the spill. The pre-existing
  `vs_reading_v0_directly_binds_gl_vertex_index` passes on the broken module, which is why
  this one is separate.
- `exec.rs::rectlist_draw_is_issued_as_a_four_vertex_strip` + the triangle-list
  counterpart; `derive.rs::rectlist_primitive_type_selects_triangle_strip_and_rekeys`.

All three new tests verified non-vacuous by reverting each fix in turn.

`cargo build --release`, `cargo test --workspace` (518 passed / 0 failed), `cargo clippy
--all-targets --all-features -- -D warnings`, `cargo fmt` all clean.

### NOT verified — maintainer's eyes required

The emulator was not run. What should change on screen: the guest's full-screen fills
(draws 0, 9, 11, 13, 14) now rasterize the whole rectangle, so RT_B/RT_C are cleared to
transparent black each frame instead of accumulating alpha to saturation. The bloom
composite at draw 16 then arrives with a radial alpha ramp (~0.06 at screen centre, ~0.6
at the corners) and ADDS a glow instead of replacing the frame. **The 3D mountain should
be SHARP, with bloom only around bright areas.**

If it is still uniformly soft: the next thing to check is RT_C's alpha channel at the
composite — a radial ramp means the bloom chain is finally faithful and the remaining
softness is elsewhere; still-uniform white means a third defect keeps the fill from
landing, and the Mesh Viewer at the fill draw (does it now cover the whole target?) splits
those two.
