---
id: TASK-172
title: >-
  tools: extend GNM scraper to capture real-HW referenced anim-buffer CONTENT
  (task-170 sawtooth-faithfulness oracle)
status: Done
assignee: []
created_date: '2026-07-18 13:06'
updated_date: '2026-07-18 14:41'
labels:
  - tools
  - celeste
  - gnm
  - scraper
  - retail
dependencies: []
priority: high
ordinal: 176000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
task-170 narrowed the Celeste splash 'rewind' to the referenced dynamic-buffer sawtooth (ramp->hard-reset ~27 frames); clock, structural-scene-replay, and boot-clamp are all ruled out. DECISIVE UNKNOWN: is our per-frame animation-buffer content faithful to real HW, or does it diverge (jarring snap vs smooth loop)? The scraper corpus (task-168) captures DCBs ONLY, not the referenced vertex/uniform buffer CONTENT. Extend it: (Phase 1, headless) decode the existing real corpus DCBs to extract the animation-buffer V# base addresses+sizes for the intro-overlay + steady-splash draws, and determine if they are STABLE or rotating across flips (decides plugin design). (Phase 2) add a memory-region dump to the GoldHEN plugin for those addresses (new wire KIND, reuse framing+RLE). (Phase 3, needs PS4) deploy + capture round 2 with buffer content. (Phase 4, headless) frame-by-frame compare real-HW vs our buffer content -> verdict on whether the sawtooth is faithful. Relates task-170 (the bug), task-168 (scraper).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 Phase 1: real-HW animation-buffer addresses + stability verdict extracted from existing corpus (designs the plugin)
- [x] #2 Plugin dumps the referenced buffer content per flip; receiver saves it
- [x] #3 Frame-by-frame real-vs-ours buffer-content diff yields a faithful/divergent verdict for the splash sawtooth
<!-- AC:END -->





## Notes

<!-- SECTION:NOTES:BEGIN -->
### Phase 1 DONE (2026-07-18, opus headless — worktree agent-a72ec28c3e17364e9, uncommitted)
Built `vref` (`tools/ps4-gnm-scrape/host/src/bin/vref.rs`, reuses `ps4_gnm::pm4` + `ps4_gnm::vbuf::decode_v_sharp`): per real DCB shadows VS/PS user-data, extracts per-draw inline V# (base/stride/num_records) + user-data pointers + whole-DCB V# scan. Ran over all 300 real flip DCBs → `scratch/vref-all.csv`.

**STABLE vs ROTATING verdict: ROTATING, double-buffered, period = 2 flips.** Referenced dynamic buffers alternate strictly A,B,A,B (150 pool A / 149 pool B, never co-occurring); the two pools are a fixed **0xC00008 (~12 MB) apart**; within a pool, addresses stable across the whole session. Real-HW dynamic-buffer heap = **0x2xx band (~11 GB)**, distinct from the static atlas T# at 0x9afc28000.

**Per-draw buffer map (real flip 30, pool A; pool B = base − 0xC00008):**
- draw0 DRAW_AUTO idx3 shader 28e8000 — fullscreen bg/clear tri (no vbuf).
- draw1 DRAW_OFF2 idx6 shader 28ea6f8 — UI quad (bg image): inline V# `0x20f963818` (4v×16=64B), slot2 CB `0x210163174`, slot0 `0x28e800068`.
- draw2 DRAW_OFF2 **idx300** shader 28ea6f8 — **SNOW particles**: slot2 ptr `0x21016341c`→particle buffer, inline V# `0x20f963a0c`.
- draw3 DRAW_OFF2 idx6 shader 28ea6f8 — UI quad (logo): inline V# `0x20f963b04`, slot2 CB `0x2101634ac`.
- draw4 DRAW_AUTO idx6 **INTRO-ONLY** shader 2bcee59 — **"Matt Makes Games" zoom-text quad**: inline V# `0x20f963d84` (64B), slot2 CB `0x21016353c`.

**Correlation:** the zoom-text quad (`0x20f963d84`/poolB `0x20ed63d7c`) + its shader appear in EXACTLY frames 2–54 (27 flips) = the structural intro window. Stable non-rotating global `VS slot0 = 0x28e800068` on every main draw.

**Animation buffers to capture on real HW (session-relative — offsets/roles transfer, absolute bases don't):**
- Zoom-text quad `…d84 / …d7c` 64 B, INTRO only (opening-zoom geometry — dump to see if zoom is baked into vertices).
- Zoom-text CB slot2 `…53c / …534` ~256 B, INTRO only (zoom transform candidate if geometry static).
- Snow slot2 `…41c` → ~300-vertex particle buffer, ALL flips.
- UI quads `…818/a0c/b04` 64 B each, ALL flips. Global `0x28e800068` (stable).

**Plugin design (Phase 2): LIVE-DCB-PARSE mandatory, NOT a fixed list** — bases are session-specific mallocs; a hardcoded list won't survive a fresh capture. Plugin must per-flip parse the live DCB like `vref`: shadow VS/PS user-data → read inline V#s (base/stride/num_records → span) + follow user-data pointers → read V# → dump `[base, span]`. Rotation is only 2 pools, so dumping the current flip's referenced set is complete. On-device pointer-follow MUST be guarded (validate base in plausible heap range + span cap; a bogus pointer would crash the game — use sceKernelVirtualQuery or a range check).

**Our-side diff (Phase 4) keys on ROLE, not address** (our load base differs): `(scene shader-lo12, draw index, num_records/stride/span)`. Add `UNEMUPS4_DUMP_VBUF` at draw time in `vbuf::derive_buffer_ranges` (where V#→(addr,span) already resolved) dumping content+hash tagged with the role key. (Replaces the raw-address `UNEMUPS4_VBHASH`.)

**AC #1 satisfied.** Next: Phase 2 plugin (needs OpenOrbis build) → Phase 3 user's PS4 capture → Phase 4 real-vs-ours role-keyed content diff.

### Phase 2 DONE (2026-07-18, opus — worktree agent-a72ec28c3e17364e9, uncommitted)
Plugin buffer-content capture built + verified. New wire `KIND_VBUF=4`: same 20-byte header + RLE, for VBUF the de-RLE'd payload starts with an 8-byte LE u64 guest base then `span` content bytes (raw_size == 8+span); DCB/CCB framing byte-for-byte unchanged (5/5 host tests pass). On-device per-flip live-DCB-parse (ports `vref`): shadow VS/PS user-data → inline V# + followed-pointer V# → dump [base,span]. Crash-safe: `region_readable` gates every read (sceKernelVirtualQuery isCommitted + heap band [4GB,64GB) + span≤1MiB → skip, never fault). `.prx` = 72096 B, OpenOrbis -Wall clean. Our-side `UNEMUPS4_DUMP_VBUF=<dir>` in exec.rs draw path, role-keyed `(vs-lo12,ps-lo12,stride,num_records,span)` (supersedes raw-address VBHASH). AC #2 satisfied.

### Phase 3 DONE (2026-07-18) — real-HW capture
Deployed the new .prx via lftp (replaced task-168's), ran receiver on PC, user launched Celeste. Captured **600 flips**: ~15046 VBUF content files + 600 DCB at `<session>/scratchpad/real-vbuf/` (game-derived, NOT committed). Game stuttered during capture then went smooth at the 600-flip cap (plugin dormant, not a crash). Key buffers present: zoom/UI quads (64B), snow/sprite (8–48KB), transform CBs (256B).

### Phase 4 DONE (2026-07-18) — **VERDICT: FAITHFUL** (AC #3 satisfied)
Bridged real base→role via `vref` on paired real DCBs; decoded per-flip trajectories, real vs ours, actual values:
- **Steady transform CB (draw1/6f8): real HW is ITSELF a ~28-flip ramp-and-reset SAWTOOTH** (float[0] -0.0364→-0.0371 over 14 steps, then hard-reset -0.0365, repeat) — matches task-170's measured ~27-flip loop IN SHAPE AND PERIOD, **on real hardware**.
- **Intro-overlay transform CB (draw4/e59): real HW is a smooth monotonic ramp, NO reset** (-0.0365→-0.0369 across the intro window) — a one-shot ease, then the overlay is dropped at the 5→4-draw transition (~flip 54).
- **UI quad geometry byte-identical:** ours (flip5) `(210,448)(1710,448)(210,647)(1710,647)` == real (frame60). Static V#s; only the CBs + sprite streams are dynamic.
- **Snow/sprite:** dynamic on both (particle drift, hash-unique/flip); our content matches real's in kind.

**Conclusion:** our splash animation-buffer content is FAITHFUL to real hardware. The periodic ramp-and-reset "cofa się" (task-170) is present on real HW too = by-design ~27–28-flip looping animation, NOT a buffer-content bug and NOT something we compute wrong. **task-170's symptom is not the referenced-buffer content.** The remaining, separately-pinned our-side divergence is the INTRO-OVERLAY DURATION: real eases the opening zoom over ~27 flips then drops it; ours drops after ~4 flips (boot-clock phantom time compresses the one-shot ease → the dramatic zoom plays near-instantly). That is NOT fixable by boot-clamping (Exp 2 = deadlock). Only the live PNG/eye oracle can now distinguish "faithful loop that looks fine" from "compressed intro that looks wrong" — the numbers match real HW either way. **This task's oracle goal is COMPLETE; the residual is a task-170 visual call, not a scraper task.**
<!-- SECTION:NOTES:END -->
