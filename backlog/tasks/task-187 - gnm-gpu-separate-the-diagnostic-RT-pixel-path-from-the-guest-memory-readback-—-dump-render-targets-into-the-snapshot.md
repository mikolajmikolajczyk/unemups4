---
id: TASK-187
title: >-
  gnm/gpu: separate the diagnostic RT-pixel path from the guest-memory readback
  — dump render targets into the snapshot
status: Done
assignee: []
created_date: '2026-07-20 17:52'
updated_date: '2026-07-23 18:39'
labels:
  - gpu
  - gnm
  - tooling
  - diagnostics
dependencies: []
priority: high
ordinal: 191000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
The RT readback (task-181) exists to write a render target's contents back into GUEST memory in the GUEST's tiled layout — that is sceGnm semantics. We had been using it as a diagnostic, and that conflation is why our diagnostic inherited the hardest part of the problem, 2D macro-tiling, for no benefit: to LOOK at pixels the guest's tiling is entirely irrelevant. It is also why the readback misled two investigations before task-181 made it refuse.\n\nAfter task-181 the readback correctly declines for every Celeste target, since they are all macro-tiled and this repo deliberately implements no macro-tiler. So the diagnostic capability is currently zero for our only real title, and every question about render-target contents costs a round trip to the maintainer with RenderDoc. That is the present bottleneck on task-184.\n\nThe fix is cheap because the pieces already exist. backend.rs::copy_rt_to_host already returns the RT's linear RGBA8 host bytes — exactly what a PNG wants — and the macro-tiled refusal currently short-circuits BEFORE it, so the diagnostic dies together with a semantic requirement it never had. Separating them yields render-target dumps for Celeste immediately, with no macro-tiler.\n\nThe two paths must stay visibly distinct in the code and in the output, so nobody later reuses one for the other's job: the guest-memory path needs the guest's exact layout and must keep refusing what it cannot express; the diagnostic path needs only linear host pixels and should never touch guest memory at all.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 The snapshot tool can dump render-target pixels as PNG for a captured frame, including macro-tiled targets, without writing anything to guest memory
- [x] #2 The guest-memory readback path is unchanged in behaviour and still refuses layouts it cannot express — the two paths are separate in the code, not one path with a flag
- [x] #3 RT dumping is opt-in and its cost is documented: copying an RT stalls the GPU, so it perturbs frame TIMING (never frame CONTENT), and the distinction is stated where a reader will see it
- [x] #4 draws.json references each dumped render target, and a target that could not be dumped carries an explicit reason rather than being silently absent
- [x] #5 summary.txt no longer says render-target pixels are out of scope, and instead states which path produced them
- [x] #6 build + cargo test + clippy clean
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Implemented 2026-07-20. Two paths, deliberately separate.

DIAGNOSTIC path (new): BackendCmd::DumpRenderTargetPng { id, path } -> AshBackend::dump_render_target_png(). Copies the HOST RT image (already linear RGBA8) via the existing copy_rt_to_host and hands the PNG encode + file write to ps4-gnm's background snapshot writer (new pub fn snapshot::enqueue_png). No guest address, no pitch, no tiling, no guest write - so macro-tiled Celeste targets dump fine. Fire-and-forget across the thread boundary; display thread takes no driver() lock (task-66 untouched).

GUEST-MEMORY path (unchanged): BackendCmd::ReadbackRenderTarget -> AshBackend::readback(). Not touched; still refuses what pack_guest_surface cannot express (task-181). Its tests pass unmodified.

Opt-in: UNEMUPS4_SNAPSHOT_RENDER_TARGETS=1, alongside an armed F10/F9 capture. Files land at <root>/frame-NNNNN/render-targets/rt-<base:016x>-<w>x<h>.png (per-frame, deduped per target per frame - RT contents are exactly what differs between burst frames).

draws.json: every draw carries target_dump - {dumped:true,key,png,source} or {dumped:false,reason} for Disabled / Videoout. Never silently absent. summary.txt no longer defers to RenderDoc; it states the source (host image copy, NOT the guest-memory readback) and the timing-vs-content cost distinction.

Tests: gnm/snapshot.rs (opt-in + per-frame dedupe + path shape + outcome JSON always carries a reason + summary text), gnm/exec.rs (armed capture with readback OFF emits exactly one DumpRenderTargetPng, zero ReadbackRenderTarget, after the producer draw, and draws.json references the PNG). NOT covered: the copy itself - dump_render_target_png needs a live Vulkan device; that link is exercised only by running the emulator.

Verify: UNEMUPS4_SNAPSHOT_RENDER_TARGETS=1 cargo run --release -p unemups4 -- <celeste>, get to the menu, press F10.
<!-- SECTION:NOTES:END -->
