---
id: TASK-201
title: >-
  gnm: RT-as-texture ignores the guest S# and hardcodes LINEAR/REPEAT —
  Celeste's 320x180 pixel-art upscale is blurred
status: Done
assignee: []
created_date: '2026-07-21 16:23'
updated_date: '2026-07-21 17:44'
labels:
  - gnm
  - gpu
  - celeste
  - retail
dependencies: []
priority: high
ordinal: 206000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
With the sky fixed (task-199) the maintainer reports the whole image is BLURRED. Celeste renders at 320x180 and upscales to 1920x1080, so pixel art must be sampled with NEAREST; we bilinear-filter it.\n\nRoot cause, and the code states it outright: bind_render_target_as_texture (crates/gnm/src/exec.rs ~1753) binds a hardcoded sampler --\n    // A render target is sampled with the portable-default sampler (linear/repeat); the\n    // S# filter/wrap refinement is out of scope for the RT path (RGBA8-only this phase).\n    mag_filter: SamplerFilter::Linear, min_filter: SamplerFilter::Linear,\n    address_mode_u/v: Repeat\n-- a deliberate task-56 shortcut that has now come due. The two other bind paths (exec.rs ~1579 and ~1666) DO use the filter decoded from the guest's S# via vbuf::decode_s_sharp. Celeste's whole composite chain plus the final 320x180 -> videoout upscale go through the RT path, so every one of them is force-bilinear.\n\nFix: thread the draw's decoded SamplerState (filter + address modes, as bind_texture already does) into bind_render_target_as_texture instead of the hardcoded default. The S# is already decoded at the descriptor-resolution site that chooses the RT path; pass it down rather than re-deriving. Keep the portable default only as the fallback for a draw that genuinely has no S#.\n\nGround-truth check available: tools/ps4-gnm-scrape/host/src/bin/framediff.rs can read the CONSOLE's S# for the same draws from dumps/scrape2 — confirm the guest really asks for NEAREST (and what wrap mode) on the composite/upscale draws rather than assuming it. Our snapshot currently records only a default sampler for RenderTarget binds (crates/gnm/src/snapshot.rs ~606/614), so ALSO record the real S# there, otherwise this class of bug stays invisible in our own captures.\n\nOracle: Celeste's pixel art renders crisp (no bilinear smear) at 1080p — maintainer live PNG oracle. Provenance: AMD GCN ISA / Mesa / llvm-mc only.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 bind_render_target_as_texture uses the draw's decoded S# (filter + address modes); the hardcoded Linear/Repeat remains only as a no-S# fallback
- [x] #2 the console's S# for the composite/upscale draws is checked with framediff and matches what we now bind
- [x] #3 the gpu-snapshot records the REAL sampler state for RenderTarget binds, not a default placeholder
- [x] #4 Celeste's pixel art is crisp at 1080p — maintainer live oracle; build + cargo test --workspace + clippy clean
<!-- AC:END -->



## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Landed in the working tree (NOT committed) 2026-07-21.

CONSOLE GROUND TRUTH FIRST (AC #2). Extended framediff to decode the S# on both sides — the
register-resident one from PS user-data slots 8..11, the memory-resident one from the descriptor
set at ptr+0x20 (the byte offset the shaders' own `s_load_dwordx4 ..., 0x8` implies, SMRD
immediates being dword indices). GFX6/7 sampler layout: word0[2:0]=CLAMP_X, word0[5:3]=CLAMP_Y,
word2[20]=XY_MAG_FILTER. Across the 16 sampling draws of dumps/scrape2 frame 4:
    11x NEAREST/ClampToEdge   4x LINEAR/ClampToEdge   1x NEAREST/Repeat
Draw 28 — the 320x180 -> 1920x1080 upscale — is NEAREST/ClampToEdge/ClampToEdge. So the hardcoded
LINEAR was wrong on FILTER *and* the hardcoded Repeat was wrong on WRAP (no RT-sampling draw in the
frame uses Repeat). Critically it is NOT uniformly nearest: draws 1/21/22/23 genuinely ask for
LINEAR, so "force NEAREST" would have broken the bloom chain. The fix had to be "honour the S#",
which is only knowable from the capture — worth recording that the ground truth changed the fix.

Changes:
- crates/gnm/src/exec.rs — new `sampler_desc_for(Option<&SamplerState>) -> SamplerDesc`, the one
  place that turns a guest S# into a host sampler (portable default ONLY for `None`). Both
  bind_texture paths now use it, and `bind_render_target_as_texture` gained a
  `sampler: Option<&SamplerState>` parameter and returns the `SamplerDesc` it bound. The call site
  passes `TextureSource::RenderTarget(rt, resolved)`'s `resolved.sampler`, which was already
  decoded and previously discarded as `_`.
- crates/gnm/src/snapshot.rs — `SampledRecord`/`SampledInput` gained `sampler_bound`, emitted as a
  `sampler_bound` JSON object next to `s_sharp`. `s_sharp` is what the guest REQUESTED,
  `sampler_bound` is what the backend was told to create; recording only the request is exactly how
  a hardcoded linear/repeat hid behind a faithfully-recorded NEAREST S# for this long. Populated
  from the SAME pure helper the bind calls, so the capture cannot drift from the GPU.
- tools/ps4-gnm-scrape/host/src/bin/framediff.rs — decodes + prints the console S#, and compares it
  against our `sampler_bound` (falling back to `s_sharp` and SAYING SO for pre-task-201 snapshots).

Tests added (3):
  exec::tests::rt_as_texture_binds_the_guest_s_sharp_not_a_fixed_default — a NEAREST/clamp S# binds
    NEAREST/clamp; a LINEAR S# still binds LINEAR (the fix is not "force nearest"); per-axis wrap is
    carried independently; and `None` falls back to the portable linear/repeat default. Asserts both
    the returned desc and the CreateSampler actually emitted, so the reported value is the bound one.
  exec::tests::sampler_desc_for_maps_filter_and_per_axis_wrap — the pure decision function.
  snapshot::tests::sampler_bound_json_reports_the_bound_filter_and_wrap — the JSON shape framediff
    now depends on.

cargo build --release -p ps4-gnm -p unemups4 OK; cargo test --workspace 566 passed / 0 failed;
cargo clippy -p ps4-gnm -p ps4-gpu --all-features -- -D warnings clean; the tool clippy clean.
(`--all-targets` still surfaces 7 pre-existing `redundant pattern matching` lints in the maintainer's
uncommitted crates/gnm/src/shader/gcn.rs — absent at HEAD, untouched here, deliberately left alone.)

AC #4 needs the maintainer's live oracle: Celeste's pixel art crisp at 1080p. Cannot run it here.
<!-- SECTION:NOTES:END -->
