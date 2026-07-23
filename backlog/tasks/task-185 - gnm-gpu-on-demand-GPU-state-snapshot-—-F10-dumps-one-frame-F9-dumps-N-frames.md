---
id: TASK-185
title: 'gnm/gpu: on-demand GPU state snapshot — F10 dumps one frame, F9 dumps N frames'
status: Done
assignee: []
created_date: '2026-07-20 13:35'
updated_date: '2026-07-23 18:39'
labels:
  - gpu
  - gnm
  - tooling
  - diagnostics
dependencies: []
priority: high
ordinal: 189000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Debugging retail GPU walls has so far relied on ad-hoc env-gated probes bolted on per investigation. That method failed badly during task-179: two separate probe-derived measurements were wrong, and both happened to be wrong in the direction that flattered the hypothesis under test, which cost hours. What is missing is a single, always-available, complete dump of GPU state at a moment the maintainer chooses. Build it: while the game runs, F10 captures the CURRENT frame and F9 captures the next N frames, N from an environment variable. Each captured frame writes a directory containing the full CONTEXT/SH register file, the per-draw derived state, the decoded guest descriptors each draw actually received, and a short human-readable summary. This replaces per-bug instrumentation with one tool, and makes frame-to-frame and ours-vs-oracle diffing possible — which is exactly what was unavailable during task-179.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 F10 captures the current frame; F9 captures the next N frames, N read from an environment variable with a sane default
- [x] #2 Each frame writes registers.json (full CONTEXT + SH file: index, decoded name, value), draws.json (per draw: derived target/pipeline/blend/viewport/scissor, bound VS+PS addresses and hashes, decoded T#/V#/S# descriptors, bounded constant-buffer contents) and summary.txt (one screen, human-readable)
- [x] #3 The keypress path does NOT violate the display-thread-never-locks-driver() invariant (task-66) — the request crosses threads as an atomic, and the dump itself runs on the submit thread at a frame boundary
- [x] #4 Zero cost when no capture is requested — no per-draw allocation or formatting on the hot path
- [x] #5 Capturing does not perturb what is captured: a dumped frame renders identically to an undumped one
- [x] #6 build + cargo test + clippy clean
- [x] #7 Per draw, dump the bound VS and PS: the SPIR-V module actually handed to Vulkan (.spv, spirv-dis/spirv-val ready), the raw .sb bytes, and the disassembly — the disasm labelled KNOWN-LOSSY per task-182
- [x] #8 Shader dumps are deduped by the routing-aware shader hash into a capture-level shaders/ directory; draws.json references the key, so one .sb under two ps_input_maps lands as two distinct modules
- [x] #9 Per draw, dump the sampled guest textures raw (tiled) AND detiled, reusing the existing detiler and the exact SurfaceLayout the upload path used
- [x] #10 Texture dumping is bounded: opt-in env var (off by default), a per-texture size cap, and content-hash dedupe across frames within a session
- [x] #11 Every not-dumped texture carries an explicit reason in draws.json (disabled / RT-source / over-cap / read-failed) — never a silent omission and never a zero-filled substitute
- [x] #12 Per draw, record the register DELTA versus the previous draw, so mid-frame reprogramming is attributable to the draw that ran with it (registers.json is end-of-frame only)
- [x] #13 Draws that bail out of setup_draw are recorded with their reason, in a separate deferred_draws array so they can never be counted as submitted
- [x] #14 draws.json carries the DECODED blend factors/equations/write-mask alongside the raw control word, sharing its field split with the Vulkan pipeline so the two cannot drift
- [x] #15 A samplerless fill draw's clear/fill colour is recorded directly, labelled a heuristic, with the raw constant-buffer dwords kept alongside
- [x] #16 Render-target pixel dumping stays OUT of scope (task-181), and summary.txt states that boundary so an absent RT dump is never read as an empty target
- [x] #17 Bulk writes (SPIR-V, texture texels) are handed to a background writer thread, not written under the driver() lock; hot path stays zero-cost when idle
- [x] #18 build + cargo test --workspace + clippy clean
<!-- AC:END -->
