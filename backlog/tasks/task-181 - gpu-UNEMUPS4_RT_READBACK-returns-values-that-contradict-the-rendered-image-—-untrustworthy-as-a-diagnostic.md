---
id: TASK-181
title: >-
  gpu: UNEMUPS4_RT_READBACK returns values that contradict the rendered image —
  untrustworthy as a diagnostic
status: Done
assignee: []
created_date: '2026-07-20 12:30'
updated_date: '2026-07-23 18:39'
labels:
  - gpu
  - diagnostics
  - correctness
dependencies: []
priority: medium
ordinal: 185000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
The RT readback path (BackendCmd::ReadbackRenderTarget -> copy_rt_to_host -> re-tile into the guest range) reports texel values that disagree with what the GPU demonstrably holds. During task-179 it reported the Celeste bloom RT as near-black (grid mean [2,4,8]) while forcing an additive composite of that same RT blew the screen to WHITE, i.e. the RT is bright. It also reported alpha 255 where the generated SPIR-V provably scales alpha with the other three channels. Reading the code found no conversion (a plain buffer copy), so the defect is elsewhere: possibly the copy happens at the wrong point in the frame, targets the wrong range, or races the draw it is meant to capture. This matters because it is a DIAGNOSTIC: it misled a long investigation more than once, and any future use will mislead again.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 Root-caused: why readback values disagree with the rendered image
- [x] #2 Readback of a known synthetic pattern round-trips exactly
- [x] #3 If it cannot be made trustworthy, it is removed or loudly marked unreliable rather than left as a plausible-looking probe
<!-- AC:END -->

## Notes

**Root cause: the readback packed the guest bytes in the WRONG LAYOUT.** Not a race, not a
barrier, not a format swap. The copy is correctly ordered — `record_passes` waits its fence
before any readback runs, `copy_rt_to_host` waits its own, and `run_command_list` blocks the
guest thread on the display thread (`rx.recv()`), so the guest cannot read the range before
the write lands. Timing was ruled out by reading those three waits, not assumed.

What was actually wrong is that the backend packed `w*h*4` CONTIGUOUS bytes at the RT's
CONTENT width into a guest surface that is neither contiguous nor row-major:

- **Padded pitch.** Since task-180 the host RT image is the content extent while the guest
  row stride is `CB_COLOR0_PITCH`. Celeste's bloom targets are **960 content in a 1024
  pitch** (`gpu-snapshots/frame-01660/draws.json`). Writing rows at stride 960 into a surface
  every reader indexes at stride 1024 skews from row 1 onward, and the written region ends
  ~2.07 MB into a 2.36 MB allocation — so the executor's own RT probe, which samples a 16x16
  grid across the central 80% at stride `pitch`, took most of its samples from guest bytes
  the readback never touched. That is the `[2,4,8]` near-black reading of a bright target,
  and it is also where a stale `alpha 255` comes from: unwritten guest memory, not a texel.
- **Tiling.** The re-tile was hardcoded `Tiling::LinearGeneral` (an identity copy). Every
  Celeste render target is **`Tiled { tile_mode_index: 14 }`** — 2D macro-tiled per
  `ps4_core::tiling::tile_kind` — and this repo has **no macro-tiler by design**. A linear
  write into a macro-tiled surface is decodable as noise only.

So both failures the task describes are the same defect wearing two hats, and the evidence is
in a capture already in the tree rather than in a new run.

### The fix

The guest surface geometry now travels WITH the command, because only the executor knows it:
`BackendCmd::ReadbackRenderTarget` gained `pitch` + `tiling`, sourced from `TargetDesc` at
`register_render_target`. The backend resolves them through `guest_surface_layout` and packs
via `pack_guest_surface` (pad rows out to the stride, then re-tile through the shared
`ps4_gnm::cache::tile::tile`, the exact inverse of the upload detile). Added a layout guard:
an RT that is not in `SHADER_READ_ONLY_OPTIMAL` — its producer draw was deferred — is skipped
rather than copied, since that path returns undefined texels without faulting.

**A surface that cannot be expressed is REFUSED, loudly, not approximated.** Macro-tiled,
`pitch < width`, and degenerate extents all log a warning naming the geometry and write
nothing.

### Bounded confidence claim (also written into the `readback` doc comment)

**Trustworthy** for 32-bpp RGBA8 targets that are **linear (any `pitch >= width`, padded
included)** or **1D-thin micro-tiled**: pack→detile round-trips texel-for-texel.

**NOT trustworthy, and refuses instead of writing:** 2D macro-tiled (`tile_mode_index >= 9`).

**NOT handled and NOT detected** (stated, not silently absorbed): the host RT image is always
created `R8G8B8A8_UNORM` because the executor hardcodes it, so a guest target declared
`B8G8R8A8` reads back channel-swapped; and only 32 bpp is expressible (`TexelSize::Bpp32`).

**Not covered by an automated test:** the Vulkan `vkCmdCopyImageToBuffer` link itself. The
gpu crate's unit tests are pure-function only (no device, matching the existing convention —
`diff_harness` is explicitly outside `cargo test` for this reason), so the tests start from
the tight `w*h*4` buffer that copy provably produces (`buffer_row_length` left zero) and
verify the half that was broken.

### The consequence for Celeste, stated plainly

Every Celeste RT is macro-tiled, so **for this title the readback now correctly declines**.
That is the honest outcome, not a regression: it previously answered, and its answers were
wrong. Making Celeste's RT contents observable needs the RT-dump follow-up on the snapshot
tool (task-185's deferred half) or RenderDoc — the `UNEMUPS4_DRAWTEX_TRACE` probe's own
comment in `exec.rs` now says so, since that probe is what surfaced the bad numbers.

### Tests (5 new, in `crates/gpu/src/backend.rs`)

- `readback_strides_rows_by_the_guest_pitch_not_the_content_width` — the padded case
  (content 10 in a 16 pitch), every texel checked at the guest's own stride arithmetic, plus
  the padding proven zero rather than borrowed content.
- `readback_of_a_tiled_surface_detiles_back_to_the_rendered_content` — 1D-thin round trip
  through the UPLOAD path's `detile`, and an `assert_ne!` against the linear pack so a tiling
  that was silently ignored cannot pass.
- `readback_refuses_surfaces_it_cannot_express_rather_than_writing_garbage` — macro indices
  9/13/**14**/31, an out-of-byte index, `pitch < width`, degenerate extents.
- `readback_treats_the_linear_aligned_mode_as_a_pitch_strided_surface`.
- `readback_of_an_unpadded_surface_is_the_content_verbatim` — the `width == pitch` shape that
  already worked must stay a straight copy.

Plus the existing `rt_readback_policy_off_emits_none_all_emits_one_and_leaves_entry_clean`
now asserts the forwarded `pitch`/`tiling`, and the `ps4-core` command test covers the
widened variant. Workspace: **505 passed / 0 failed**, clippy clean, `cargo fmt` applied.
