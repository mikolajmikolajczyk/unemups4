---
id: TASK-53
title: >-
  gnm: real-shader draw end-to-end — GcnBinary resolve + cache-fed buffers +
  DRAW_INDEX_AUTO/DRAW_INDEX_2 (keystone)
status: Done
assignee: []
created_date: '2026-07-11 12:55'
updated_date: '2026-07-12 21:56'
labels:
  - gpu
  - gnm
dependencies:
  - TASK-65
  - TASK-70
  - TASK-79
  - TASK-80
priority: high
ordinal: 52000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Phase-4 integration keystone: a .sb-blob guest triangle. Wire all: draw arm reads binds from register file (P4-09), resolves through provider chain (P4-07), derives V# vertex state (P4-10) + RT/viewport (P4-11), pulls buffers through ResourceCache (P4-14), snapshots PipelineKey, emits P4-17 command list. Adds indexed-draw arms: IT_INDEX_TYPE, IT_INDEX_BASE/IT_DRAW_INDEX_2 (index buffer through cache), IT_NUM_INSTANCES (count carried, >1 deferred). Every failure mode (unsupported shader, bad descriptor, unknown format) defers per-draw with structured log, never fatal. Does NOT do textures (P4-20), fetch shaders (P4-12), compute.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 headless: MockBackend end-to-end — synthetic DCB (SET_SH_REG binds + V# user data + RT regs + DRAW_INDEX_AUTO over a corpus blob in mock memory) → exact expected create/upload/bind/draw sequence
- [x] #2 headless: same for DRAW_INDEX_2 with 16-bit index buffer (IndexBuf cache entry)
- [x] #3 live GPU (maintainer): P4-19 corpus ELF renders the colored triangle (LD_LIBRARY_PATH=/usr/lib)
- [x] #4 headless: all existing Tier A/B (softgpu present, embedded draw) regress-free
<!-- AC:END -->





## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Fix executor routing gap: route resolved-GCN HostShader into CreatePipeline(recompiled SPIR-V)+BindPipeline; add V#->BindVertexBuffer w/ vertex-input state, DrawIndexed (IT_INDEX_TYPE/INDEX_BASE/DRAW_INDEX_2), dynamic viewport/scissor, driver-owned provider chain; MockBackend end-to-end tests w/ independent literals; defer-not-fatal on every failure.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
EXECUTOR ROUTING GAP (found in task-42 review 2026-07-12, deferred here as draw-arm scope): with the chain fixed to reach GcnShaderProvider, a GcnBinary now resolves to Ok(Some(recompiled HostShader)). But exec.rs resolve_embedded_pair/embedded_id_of collapse a NON-embedded Ok(Some) to Ok(None) -> DrawResolution::Unbound, so a successfully-recompiled GCN shader is currently SKIPPED (draw dropped), and mislabeled Unbound rather than 'GCN ready'. Behaviorally same as before (no dispatch, no crash) but the recompiled SPIR-V is discarded. task-53 must add a DrawResolution path that carries the resolved GCN HostShader(s) into a real host-pipeline dispatch (PipelineKey + CreatePipeline BackendCmd, decision-7) instead of dropping them. Note: since providers are rebuilt per submit (task-52/53 ownership), the recompile also runs+discards per draw until the provider is driver-owned.
SESSION 2026-07-12 (uncommitted, orchestrator review). DONE headless (AC#1/#2/#4): real GCN .sb draw wired end-to-end. Routing gap already closed generically by task-52; task-53 added: driver-owned provider chain [embedded, gcn] + ResourceCache + PipelineCache (persist across submits, per-submit drain_dirty); V#->vertex buffers (fetch layout from HostShader.io, cache upload-on-use, BindVertexBuffer + vertex_layout folded into PipelineKey); indexed draws (IT_INDEX_TYPE/INDEX_BASE/NUM_INSTANCES state + IT_DRAW_INDEX_2 -> index buffer through cache + DrawIndexed); dynamic viewport/scissor (SetViewport/SetScissor cmds, VK_DYNAMIC_STATE in pipeline). New BackendCmd: BindVertexBuffer/DrawIndexed/SetViewport/SetScissor + IndexType/ViewportRect/ScissorRect. Display side (ps4-gpu) replays all. Found+fixed a REAL bug: cache upload read through IdentityMem (read_bytes_ranged=Err) so NO vertex/index bytes ever uploaded; added BoundedMem adapter routing uploads through the bounded seam. Two MockBackend end-to-end tests (passthrough_vs+flat_color_ps corpus, DRAW_INDEX_AUTO and DRAW_INDEX_2 16-bit) assert EXACT BackendCmd sequences as independent literals. Gate: build OK, ps4-gnm 141+1+1, ps4-core 9, ps4-gpu 4 pass; clippy 0; fmt OK; run_examples 6/6. AC#3 (LIVE, maintainer): NO corpus .sb triangle ELF exists under examples/ (only softgpu present-only + pm4-test). Needs a corpus ELF authored that binds a real GCN .sb VS/PS via SPI_SHADER_PGM_* and submits DRAW_INDEX_AUTO/2. Live cmd once authored: LD_LIBRARY_PATH=/usr/lib cargo run --release -p unemups4 -- <corpus.elf>. DO NOT commit — left for review.
<!-- SECTION:NOTES:END -->

## Absorbed from task-52 (deferred there, do here)

task-52 built the generic pipeline path but intentionally deferred the real-draw plumbing to this keystone. task-53 MUST add, on top of its GcnBinary-resolve + cache-fed buffers:
- BackendCmd::BindVertexBuffer (+ vertex-input pipeline state in create_host_pipeline — currently empty vertex input) fed from task-45 VertexInputDesc/BufferDesc.
- DrawIndexed / index-buffer command (DRAW_INDEX_2) alongside the existing DRAW_INDEX_AUTO/DrawAuto.
- Dynamic viewport/scissor: declare VK_DYNAMIC_STATE + cmd_set_viewport/scissor from the task-46 derived viewport (pipeline currently bakes static).
- Wire the ShaderProvider chain to be DRIVER-OWNED + per-submit drain_dirty (task-52 rebuilds it per submit and re-recompiles+discards on cache hit).
- Executor routing: resolve_embedded_pair/DrawResolution must route a resolved-GCN Ok(Some) into CreatePipeline{recompiled SPIR-V}+BindPipeline (today a non-embedded Ok(Some) collapses to Unbound and the SPIR-V is discarded).

AC#3 of task-52 (hardcoded-vertex-buffer triangle via hand-fed list) also relocated here — the handfed_list.rs harness currently renders a gl_VertexIndex triangle only.

Live-GPU: this keystone (and task-52 #2) needs a maintainer run — LD_LIBRARY_PATH=/usr/lib cargo run -p ps4-gpu --bin diff_harness --release / ps4-pm4-test Tier B / the emulator window.

## Notes

Headless-complete + merged. Wired the real GCN .sb draw path end-to-end: driver-owned provider chain resolves a .sb -> recompiled SPIR-V -> CreatePipeline/BindPipeline (decision-7); V# -> ResourceCache vertex buffer -> BindVertexBuffer + vertex-input; indexed draws IT_INDEX_TYPE/DRAW_INDEX_2; dynamic viewport/scissor (task-46/93); one-list-per-submit. Found+fixed a latent bug: ResourceCache uploaded through IdentityMem (read_bytes_ranged -> Err) so vertex/index bytes never uploaded in prod; added BoundedMem routing uploads through the bounded seam.

Two reviews (executor + gpu-backend) ZERO criticals. Independent MockBackend e2e tests (hand literals, incl index bytes [0,0,1,0,2,0]). Defer-not-fatal on every failure mode. Embedded R/G path regress-free (AC#4, oracle 6/6).

AC status: #1 (DRAW_INDEX_AUTO mock e2e) DONE. #2 (DRAW_INDEX_2 16-bit index mock e2e) DONE. #4 (Tier A/B regress-free) DONE. #3 (live triangle) BLOCKED on a corpus ELF -> task-96 (no register-route GCN .sb triangle ELF exists yet). Backend conditionally ready: renders IFF recompiled VS input is a Location=0 vec4.

Follow-ups filed: task-94 (general vertex-input from IoLayout — backend hardcodes single vec4, wrong for other formats/multi-buffer), task-95 (GcnShaderProvider code-range watch — drain_dirty is a no-op for shaders, stale recompile under SMC source), task-96 (author the corpus .sb triangle ELF for AC#3).

## AC#3 live-confirmed 2026-07-12 (maintainer + PNG oracle)

The real recompiled-GCN .sb triangle RENDERS in the emulator window (examples/ps4-gcn-triangle) — maintainer visually confirmed + captured via the task-97 PNG oracle. Required, beyond the headless mock, a chain of live-GPU fixes this took to surface: (1) homebrew CB_COLOR0_INFO must encode FORMAT=8_8_8_8 (0x0A<<2), not 0; (2) DisplayBufferSource wired at boot (GpuManager registry + register_display_buffers) so a CB_COLOR0_BASE aliasing the videoout fb resolves instead of deferring as an arbitrary RT; (3) the recompiler's vertex fetch is an SSBO vertex-pull (DescSet0/Bind0 StorageBuffer + num_records push-constant indexed by gl_VertexIndex), NOT fixed-function vertex-input — create_host_pipeline now builds the descriptor-set + push-constant layout and record_draw_list binds the SSBO descriptor; (4) the Vulkan negative-height Y-flip viewport is y=yoffset-yscale (flipped y=1080), correcting task-93's erroneous .abs() (which put the drawable region above the framebuffer). task-94's fixed-function vertex-input path is dormant (wrong model for this recompiler; left as-is).
