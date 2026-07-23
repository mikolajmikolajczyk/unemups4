---
id: TASK-24
title: 'gpu: execute PM4 draw with embedded shaders (no GCN) — phase 3.5'
status: Done
assignee: []
created_date: '2026-07-10 18:45'
updated_date: '2026-07-11 13:42'
labels:
  - gpu
dependencies:
  - TASK-34
priority: medium
ordinal: 24000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Phase 3.5 (doc-3): the first milestone that draws real guest geometry, deliberately sequenced BEFORE any GCN work. Retail/homebrew Gnm can bind firmware-EMBEDDED shaders via sceGnmSetEmbeddedVsShader(id) / sceGnmSetEmbeddedPsShader(id) — a small fixed set (VS 0 = fullscreen-quad, PS 1 = R/G-export, per doc-3) that carry NO .sb shader binary. Because the IDs map to known, fixed shader behavior, the emulator can substitute a HARDCODED host shader (SPIR-V, via the existing ash/Vulkan pipeline) instead of decoding GCN. This unblocks a visible GPU-drawn frame with zero GCN interpreter/recompiler. Scope: when the PM4 executor (phase 3, post task-21) sees a draw (DrawIndexAuto) whose bound VS/PS are embedded IDs, route to a hardcoded host pipeline; wire PM4 draw state (render target = the videoout framebuffer we already present, vertex fetch, primitive) into a real Vulkan draw into that target; DrawIndexAuto with the embedded quad renders the R/G gradient. Arbitrary .sb shaders (freegnm triangle, real games) require the GCN decoder/interpreter and are explicitly OUT of scope — this is the shader-free rung. Consumes task-22 Tier B as its corpus. Portability: keep to the Vulkan-portable subset (decision-3 MoltenVK/Metal policy). Depends on the PM4 present/execute subset (phase-3 tasks, TBD) and task-22 Tier B.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 The PM4 executor detects a DrawIndexAuto bound to embedded VS/PS IDs and dispatches a hardcoded host (SPIR-V) pipeline instead of attempting GCN decode
- [x] #2 task-22 Tier B renders a real GPU-drawn frame (embedded fullscreen-quad VS + R/G-export PS) into the videoout framebuffer, presented via the existing Vulkan path
- [x] #3 NO GCN decoding/interpretation involved; a draw bound to a NON-embedded (real .sb) shader is cleanly detected and deferred with a clear 'needs GCN (phase 4)' log, not a crash
- [x] #4 Vulkan-portable-subset only (no non-portable extension without a gated fallback), per decision-3
<!-- AC:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. ps4-core::gpu — add BackendCmd (Vulkan-free enum: BindEmbeddedPipeline{vs_id,ps_id}, DrawAuto{vertex_count}) + grow PresentSink with run_command_list(&[BackendCmd]); DrawSink seam. Executor stays Vulkan-free (names only ps4-core traits).
2. EmbeddedShaderProvider (gnm/shader/embedded.rs) — impl ShaderProvider: Embedded{Vertex,0}->fullscreen-quad VS SPIR-V; Embedded{Pixel,1}->R/G-export PS SPIR-V; GcnBinary->Err(ShaderUnsupported). SPIR-V hand-authored via glslc (portable subset), baked as include_bytes .spv.
3. GpuState (gnm/state.rs) — BoundShaders{vs,ps: Option<ShaderRef>}; process-global bound-shader state set by HLE SetEmbeddedVs/PsShader stubs.
4. HLE stubs (libs/libscegnmdriver) — record bound embedded (stage,id) into shared GpuState.
5. Executor ExecMode::Draw + IT_DRAW_INDEX_AUTO arm — resolve bound VS/PS through EmbeddedShaderProvider; embedded->emit BackendCmd list to draw sink; GcnBinary/unsupported->log 'needs GCN (phase 4)', skip (AC#3).
6. Minimal pipeline cache in AshBackend keyed by (vs_id,ps_id,fmt); RunCommandList channel variant; AshBackend records draw into videoout target + presents (reuse present chain).
7. Unit tests: provider resolve (embedded ok, gcn err), executor draw arm emits BindPipeline+DrawAuto via MockSink, non-embedded defers. Verify: build/test/clippy/fmt/oracle 6/6/cargo-tree empty.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Implemented phase-3.5 embedded-shader draw (no GCN). Worktree /home/mikolaj/src/unemups4-task24, branch feat/gnm-embedded-draw. NOT committed.

Design:
- Bound-shader tracking: ps4-gnm::state — process-global BoundShaders{vs,ps: Option<ShaderRef>} (Mutex/OnceLock like driver()). HLE sceGnmSetEmbeddedVs/PsShader stubs (libs/libscegnmdriver) record Embedded{stage,id}. Executor IT_DRAW_INDEX_AUTO arm snapshots it, resolves BOTH stages through EmbeddedShaderProvider (doc-4 §4 single route).
- Thread-boundary seam: added ps4-core::gpu::BackendCmd (Vulkan-free: BindEmbeddedPipeline{vs_id,ps_id}, DrawAuto{vertex_count}) + PresentSink::run_command_list default-noop. Executor emits a BackendCmd list over the sink (guest thread), never touches Vulkan. GpuManager impl sends GpuCommand::RunCommandList over the crossbeam channel (blocking handshake); display loop calls AshBackend::run_command_list. ps4-gnm stays Vulkan-free (cargo tree empty).
- SPIR-V: hand-authored GLSL in crates/gnm/shaders/{embedded_fullscreen.vert (gl_VertexIndex fullscreen triangle), embedded_rg_export.frag (R/G export)}, compiled with glslc, spirv-val clean, baked via include_bytes. Portable subset (no cap beyond gl_VertexIndex + 1 RGBA export) — MoltenVK/Metal safe (decision-3, AC#4).
- Pipeline cache: AshBackend.embedded_pipelines keyed by (vs_id,ps_id); shared render pass+framebuffer over videoout texture_image (added COLOR_ATTACHMENT usage). Draw renders into texture_image, leaves SHADER_READ_ONLY; present() gated to skip the guest-fb copy when embedded_drawn so the drawn frame scans out.
- AC#3 defer: EmbeddedShaderProvider returns Err(ShaderUnsupported) for GcnBinary; executor logs 'needs GCN (phase 4)' (debug!) and skips — unit-tested (non_embedded_bound_draw_defers_not_fatal).

ACs: #1 (detect/dispatch), #3 (defer), #4 (portable) ticked — proven headless (12 new unit tests). #2 (live rendered frame) UNCHECKED — needs a GPU; structurally wired + logged; maintainer live-verifies with ps4-pm4-test.

Verify (all pass): cargo build --release OK; cargo test 112 passed/3 ignored; cargo clippy -D warnings 0 code warnings (9 are pre-existing ps4-syscalls SDK-not-found build.rs); cargo fmt --check clean; run_examples.sh check 6/6 OK; cargo tree -p ps4-gnm |grep ash/winit/vulkan empty.

CORPUS GAP (not applied): ps4-pm4-test Tier B hand-emits SH-register writes but does NOT call sceGnmSetEmbeddedVs/PsShader, so it does not exercise the new bound-shader HLE path end-to-end. To drive a live embedded draw, Tier B should call sceGnmSetEmbeddedVsShader(...,0)+sceGnmSetEmbeddedPsShader(...,1) before DrawIndexAuto (doc-3 §3.4: needs no blob). Proposed, NOT changed — do not add to the 6-example oracle (needs a GPU).
<!-- SECTION:NOTES:END -->

## Comments

<!-- COMMENTS:BEGIN -->
created: 2026-07-11 13:42
---
Live-verified 2026-07-11 (maintainer, GPU): ps4-pm4-test Tier B (SetEmbeddedVs id=0 + SetEmbeddedPs id=1 + DrawIndexAuto) renders the R/G embedded-shader fill (green-yellow flash) via the hardcoded host SPIR-V pipeline; PM4 trace shows the full bind+draw packet sequence; guest completes cleanly. AC#2 confirmed — first GPU-drawn frame from shaders.
---
<!-- COMMENTS:END -->
