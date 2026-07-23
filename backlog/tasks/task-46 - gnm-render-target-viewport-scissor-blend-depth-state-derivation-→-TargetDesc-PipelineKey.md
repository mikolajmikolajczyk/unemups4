---
id: TASK-46
title: >-
  gnm: render-target/viewport/scissor/blend/depth state derivation → TargetDesc
  + PipelineKey
status: Done
assignee: []
created_date: '2026-07-11 12:54'
updated_date: '2026-07-12 16:13'
labels:
  - gpu
  - gnm
dependencies:
  - TASK-43
priority: medium
ordinal: 45000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Derive at draw time: color target from CB_COLOR0_BASE/PITCH/SLICE/VIEW/INFO/ATTRIB (format, tiling field carried per §C3/§C9 even while forced linear/uncompressed), viewport/scissor from PA_CL_VPORT_*/PA_SC_*, blend from CB_BLEND*/CB_COLOR_CONTROL, depth presence from DB_Z_INFO/DB_DEPTH_CONTROL. Snapshot pipeline-relevant bits into a real PipelineKey (shader hashes + vertex layout + RT format + blend/depth bits — §4 "must not hardcode"), in gnm, mirrored by core::gpu types as needed. First impl maps RT to videoout framebuffer when base matches a registered display buffer; arbitrary RTs = P4-21 scope. Does NOT implement HTILE/DCC (forced off §C9); does NOT MRT>1.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 headless: DrawInitDefaultHardwareState+RT-setup register stream → expected TargetDesc/viewport (golden vs decoded default-state)
- [x] #2 headless: PipelineKey changes iff a key-relevant register changed (cache-identity)
- [x] #3 headless: unknown/unsupported RT format defers draw cleanly with log
<!-- AC:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Add ctx-reg consts (CB_COLOR0/PA/CB_BLEND/DB) + gnm target/pipeline derivation module (TargetDesc/PipelineKey) reading GpuState at draw time; extend core::gpu TargetDesc/add PipelineKey (plain data, Vulkan-free); map RT base to registered videoout fb via bounded seam; 3 headless ACs in gnm.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
DONE (worktree unemups4-task46, branch feat/task-46 — UNCOMMITTED).

Shape landed:
- crates/gnm/src/derive.rs (NEW): register→pipeline-state translation read at draw time. derive_target (CB_COLOR0_BASE/INFO/PITCH/ATTRIB → TargetDesc, format+tiling carried per §C3/§C9, tile-mode-index retained even while forced-linear), derive_pipeline (PipelineKey: FNV-1a shader-identity hash per stage, vertex_layout, RT format, blend/depth bits — no hardcoded handle §4), derive_viewport (PA_CL_VPORT_* scale/offset→pixel rect), derive_scissor (PA_SC_SCREEN_SCISSOR_TL/BR), derive_blend (CB_BLEND0_CONTROL bit30 + word), derive_depth (DB_DEPTH_CONTROL bit1 && DB_Z_INFO fmt!=0). RT base mapped to videoout fb via new DisplayBufferSource seam; unregistered base / unsupported format → clean TargetError defer.
- crates/gnm/src/pm4/opcodes.rs: added CB_COLOR0_{BASE,PITCH,SLICE,VIEW,INFO,ATTRIB}, PA_CL_VPORT_{X,Y}{SCALE,OFFSET}, PA_SC_SCREEN_SCISSOR_{TL,BR}, CB_BLEND0_CONTROL, CB_COLOR_CONTROL, DB_DEPTH_CONTROL, DB_Z_INFO CONTEXT-bank consts (GFX6 (byte-0x28000)/4; SPI_SHADER_COL_FORMAT 0x1C5 cross-checks).
- crates/core/src/gpu.rs (Vulkan-free plain data): TargetDesc grew {pitch,format:ColorFormat,tiling:Tiling}; added PipelineKey{vs_hash,ps_hash,vertex_layout,color_format,blend:BlendKey,depth:DepthKey}, VertexLayout, ColorFormat, Tiling. New DisplayBufferSource registered seam (lookup(base)->Option<DisplayBuffer{base,width,height}>) mirroring PresentSink/bounded_read, + register_display_buffers/display_buffers/registered_display_buffers(test-hooks).
- crates/gnm/src/exec.rs: dispatch_draw_auto now calls color_target_ok() — derives draw state when CB_COLOR0_BASE programmed; NoColorBase (embedded fullscreen-quad corpus, Tier B unchanged)→proceed; UnsupportedFormat/UnregisteredTarget→debug-log defer, no BackendCmd. + 2 exec tests (registered target dispatches; unsupported format defers).

ACs: #1 default_state_derives_expected_target_and_viewport (+ pitch/tiled variants); #2 pipeline_key_stable/_changes_when_shader_bind_changes/_on_blend_depth_and_format + depth_absent_without_surface; #3 unsupported_format_defers_with_error + unregistered_base + headless_unwired + exec-seam embedded_draw_with_unsupported_target_format_defers.

Gate: cargo build --release 0 err; cargo test -p ps4-gnm 115 lib+1 integ+1 doctest all pass (11 new derive + 2 new exec); cargo test workspace 0 failed; clippy --all-targets --all-features -D warnings 0 (only ps4-syscalls SDK-not-found build-script notice); cargo fmt --check clean. gnm cargo tree has no ash/winit/vulkan; ps4-core stays Vulkan-free (vk mentioned only in doc comments); RT base is NEVER dereferenced (only compared to registered buffers), so no bounded-read seam needed here.

DEFERRED: boot-wiring DisplayBufferSource into GpuManager/AshBackend (kernel currently hardcodes 1920x1080 in bridge.rs; the private buffers HashMap in ps4-gpu backend.rs would back a real impl). The seam + gnm consumption + headless ACs are the task-46 deliverable; live boot-wiring is display-thread plumbing beyond scope — file as follow-up if a running-frame integration is wanted.
<!-- SECTION:NOTES:END -->
