---
id: TASK-175
title: >-
  gpu: Celeste title screen renders R/B channel-swapped (warm palette) while
  splash is correct — format-aware present
status: Done
assignee: []
created_date: '2026-07-18 17:09'
updated_date: '2026-07-20 07:22'
labels:
  - gpu
  - celeste
  - color
  - present
  - retail
dependencies: []
priority: high
ordinal: 179000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Live oracle: the Celeste attract/title screen (CELESTE logo + 2D mountain) renders with a WARM palette (pink mountain, gold/purple bg, yellow logo) where the console capture shows the correct COOL palette (blue/purple mountain, navy bg, cyan logo). Swapping R<->B on the reference reproduces our exact palette -> this is a BGRA<->RGBA channel swap on this scene. CRITICAL: the studio splash (Matt Makes Games, gold gradient) renders with CORRECT colors, so the R/B handling DIFFERS between scenes and a global flip would break the splash. investigator noted submit_flip (crates/gpu/src/backend.rs:1604-1615) uses the flip index ONLY to pick the R/B swap. Root-cause WHY the title buffer is R/B-swapped vs the splash: likely the guest registers/flips a different color-buffer FORMAT (B8G8R8A8 vs R8G8B8A8, from sceVideoOutRegisterBuffers pixelFormat or the CB_COLOR format) and our present/swapchain path picks R/B by flip-index instead of by the actual buffer format. Fix: choose the R/B swap (or the Vulkan image format) from the buffer's real pixel format, so BOTH splash and title render correct colors. Confirm-before-implement + PNG/live oracle (title must go blue like the console capture, splash must stay correct). Relates task-163/171 (present path).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 Root-caused: why the title buffer renders R/B-swapped while the splash is correct (buffer pixel-format vs flip-index R/B selection), with evidence
- [x] #2 Present/swapchain path selects R/B (or image format) from the buffer's actual pixel format, not the flip index
- [x] #3 Celeste title renders the correct COOL palette (blue mountain/navy bg) AND the studio splash stays correct — live/PNG oracle; build+test+clippy clean
<!-- AC:END -->



## Notes

<!-- SECTION:NOTES:BEGIN -->
### PREMISE REFUTED — real root cause found (2026-07-18, opus headless, PNG oracle self-inspected)
The task premise (per-buffer pixel-format difference → select R/B from format) is WRONG and a no-op:
- **Both scanout buffers register the SAME format:** `sceVideoOutRegisterBuffers` (bridge.rs:303-345) → both index 0/1 = `pixel_format=0x80000000` (A8R8G8B8_SRGB/BGRA), so `scanout_swap_rb` (backend.rs:42-44) returns TRUE for every flip. The present path is ALREADY format-driven (not flip-index; that was rewritten by task-154). No per-scene format signal exists to key on.
- **Present path identical for both scenes** (per-frame log: every presented frame splash+title has `pixfmt=0x80000000`, `swap_rb=1`, `current_target` just alternates 0/1 double-buffering).

**Decisive swap experiment (PNG oracle, frames inspected):** `swap_rb=1` (current) → splash correct (warm), title WRONG (pink). `swap_rb=0` (forced) → title correct (navy/cyan cool palette we want!), splash WRONG (teal). So **splash needs swap=TRUE, title needs swap=FALSE, through the SAME uniform present** — a single global present flag cannot satisfy both.

**REAL root cause (content/shader level):** our recompiler exports color as logical RGBA and textures sample as logical RGBA (T# `dst_sel` dropped — all 12,560 binds identity `[4,5,6,7]`; offscreen RTs decode R8G8B8A8_UNORM/RGBA). So texture-sourced pixels are genuine RGBA → correct with NO swap → the title (textures + offscreen-RT composite, doc-6 Entry 18) wants swap=FALSE. The **splash gradient is direct-scanout and its color arrives R↔B-swapped (BGRA)** in `texture_image` → wants swap=TRUE. The present `swap_rb` (task-154/doc-6 Entry 19) was **calibrated on the splash** — it masks the splash's BGRA but wrongly swaps every texture-sourced (RGBA) scene = the entire title.

**FIX direction (NOT a present-path change):** make the splash's color SOURCE produce logical RGBA like textures already do, then DROP the splash-era global `swap_rb` (a swapchain fed logical RGBA presents correctly with no swap — proven by the experiment's title branch). **Open item:** locate the splash's BGRA color input — vertex-color + V# paths already honor `dst_sel` (RGBA), so the culprit is most likely a **packed color delivered via a CONSTANT BUFFER** (or a non-`8_8_8_8` packed texture format) that the splash shader unpacks in BGRA order. Needs a targeted trace of the splash gradient draw's specific shader + its constant/vertex color source. Do NOT ship a per-frame "did this composite from an RT" gate — that's the same fragile flip-index-style heuristic this task rejects.

**Separate latent bug found (left alone):** offscreen RTs are created `R8G8B8A8_UNORM` (backend.rs:1481) but `create_rt_target`'s render pass uses `R8G8B8A8_SRGB` (backend.rs:2980) — a gamma/VUID mismatch, NOT the R↔B cause. Worth its own task.

**AC #1 satisfied** (root-caused, evidence). AC #2 is moot (present path is already format-driven; the fix is at the color source, not the present). Reproduce: force `swap_rb=0` at backend.rs:1667 → title cool/correct, splash cool/wrong. Title appears ~90-100s in.

### RESOLVED (2026-07-20, opus) — TWO independent bugs; the "splash is correct" premise was FALSE
The premise that the splash renders correctly is **wrong**, and it is what defeated the previous session.
Captured the splash (`UNEMUPS4_DUMP_PNG`, flips ~120-400) and the title (flips ~1500-1601): **both render the same
warm gold/purple cast**. The real Celeste splash is a deep navy/indigo gradient — its warm version merely *looks*
plausible as a "gold gradient", so it was mis-scored as correct. That mis-scoring made a global fix look impossible.

**Offline oracle proof (on the flip-1601 title capture):** R↔B swap alone → correct HUES but a milky, washed-out
background (this is the previous session's "teal" splash). R↔B swap **plus removing exactly one sRGB encode** →
the console-capture reference exactly (deep navy bg, blue/purple mountain, cyan logo). So **both** a channel swap and a
gamma error were in play; each alone looks like a partial fix, which is why single-variable experiments failed.

**Maintainer's UNORM-vs-sRGB hypothesis: CONFIRMED, by register value.** Probed `CB_COLOR0_INFO` on a live run:
`0x00008828` → FORMAT `0xA` (COLOR_8_8_8_8), **NUMBER_TYPE = 0 (NUMBER_UNORM)**, COMP_SWAP = 1 (ALT/BGRA). One
single value for the whole run — splash and title share the target, which is why they share the bugs.

- **Bug A (gamma).** The guest's videoout CB is **UNORM**, but we rendered it into an `R8G8B8A8_SRGB`
  `texture_image`/`EMBEDDED_TARGET_FORMAT`. Celeste's atlases are `nfmt = 0` UNORM and it composites in gamma
  space (exactly as a UNORM CB does on real HW), so its fragment values are *already* gamma-space; the _SRGB
  attachment encoded them a second time on store → washout. Fixed by making the videoout image + render pass
  `R8G8B8A8_UNORM` (mirrors the guest's NUMBER_TYPE) and having the present shader `srgb→linear` **decode** so the
  _SRGB swapchain's encode-on-store cancels it. Net: the guest's bytes reach the display untouched.
- **Bug B (channel order).** `swap_rb` describes the **byte order of GUEST MEMORY**, so it is only meaningful when
  the present sources pixels from the guest framebuffer. An embedded GNM draw does not: our render pass writes
  shader-space `(r,g,b,a)` straight into the RGBA `texture_image`. On real HW the guest's COMP_SWAP and its scanout
  pixelFormat describe the *same* buffer and compose to the identity, so no swap is ever owed to embedded content.
  Fixed by gating: `swap_rb = !embedded_drawn && scanout_swap_rb(...)`. Probe confirmed Celeste is
  `embedded_drawn = true` from ~flip 300 on, so it was taking an unmatched R↔B flip on every frame.

**Files:** `crates/gpu/src/vulkan.rs` (videoout image + view → `R8G8B8A8_UNORM`), `crates/gpu/src/backend.rs`
(`EMBEDDED_TARGET_FORMAT` → UNORM; `swap_rb` gated on `!embedded_drawn`; `srgb` push-constant renamed/inverted to
`decode_srgb`), `crates/gpu/shaders/shader.frag` + regenerated `frag.spv` (`linear_to_srgb` → `srgb_to_linear`).
Format-derived, not index-derived: the swap now follows *where the pixels came from* and the gamma follows the
guest's CB NUMBER_TYPE.

**Verified live (PNG oracle, this build):** splash → deep navy gradient; title (flip ~1500+) → navy bg, blue/purple
mountain, cyan CELESTE logo, purple version string = the console-capture reference. Build + clippy clean; `cargo test -p
ps4-gpu` 10/10. NOTE: 7 `ps4-gnm` `cache::tests::*` failures in the tree are **pre-existing and unrelated** —
reproduced with these changes stashed; they come from the uncommitted `crates/gnm/src/exec.rs` work, and `ps4-gnm`
does not depend on `ps4-gpu`.

**Still open (NOT fixed here, deliberately):** `create_rt_target` (backend.rs:3022) hardcodes
`EMBEDDED_TARGET_FORMAT` for **offscreen** RT render passes, but the RT images are created via `vk_color_format`
— Celeste's COMP_SWAP=ALT RTs are `B8G8R8A8_UNORM`, so render-pass format ≠ image format. This change narrows the
mismatch (it was _SRGB vs _UNORM, now it is only a channel-order mismatch) but does not close it. It is a spec
violation the current driver tolerates; **a MoltenVK/Metal portability risk** worth its own task.
<!-- SECTION:NOTES:END -->
