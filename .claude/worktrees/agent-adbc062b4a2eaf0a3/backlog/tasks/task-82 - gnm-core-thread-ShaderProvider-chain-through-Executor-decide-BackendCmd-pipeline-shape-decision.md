---
id: TASK-82
title: >-
  gnm/core: thread ShaderProvider chain through Executor + decide BackendCmd
  pipeline shape (decision)
status: Done
assignee: []
created_date: '2026-07-12 07:55'
updated_date: '2026-07-12 08:45'
labels:
  - gpu
  - gnm
  - core
dependencies: []
priority: low
ordinal: 81000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Fable phase-4 quality review finding #4 (QUALITY, coordinate with task-42/52/53). crates/gnm/src/exec.rs:190-224 (resolve_embedded_pair constructs EmbeddedShaderProvider::new() internally, peeks at ShaderRef variants, discards the resolved HostShader — sends only embedded ids over the channel) + crates/core/src/gpu.rs:107-125 (BackendCmd, currently Copy, id-only). doc-4 §4 declares the provider chain 'the SINGLE route for all binds' but the executor bakes the embedded provider into a free function. task-53 needs GcnBinary->recompiled SPIR-V, and SPIR-V must cross the channel — if the shape isn't decided first, task-53 bolts a 2nd special case into dispatch_draw_auto. TWO coordinated moves: (a) THREAD THE CHAIN: Executor::new(mode, sink, state, providers: &dyn ShaderProvider) (a composite: embedded, then GCN) so task-53 ADDS a provider, not an executor rewrite. (b) DECIDE THE CHANNEL SHAPE now, reusing the guest-minted-id pattern already adopted for ResourceId: a guest-minted ShaderId/PipelineId + a CreatePipeline{ id, vs_spirv: Arc<[u32]>, ps_spirv: Arc<[u32]>, ... } BackendCmd variant (accept BackendCmd loses Copy), channel stays fire-and-forget, pipeline cache display-side keyed by id. RECORD AS A backlog decision at kickoff (cheap now; discovering it mid-task-53 under the keystone is not). Ties task-52 (per-submit list) + task-42 (GcnShaderProvider).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Executor takes a ShaderProvider (composite chain), not a hardcoded EmbeddedShaderProvider free fn; embedded path unchanged behavior
- [ ] #2 a backlog decision records the BackendCmd pipeline shape (guest-minted PipelineId + CreatePipeline carrying Arc<[u32]> SPIR-V; BackendCmd loses Copy); task-52/42 reference it
- [ ] #3 no functional change to phase-3.5 embedded draw (ps4-pm4-test Tier B unchanged); build/test/clippy/fmt green
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
DONE 2026-07-12 (feat/task-82 @ 0995b9f, merged). ChainProvider<'p> (gnm/shader/source.rs) wraps providers: &[&dyn ShaderProvider]; resolve walks in order — first Ok(Some) wins, Err(ShaderUnsupported) short-circuits (clean defer e.g. GCN-before-translator), Ok(None) falls through, all-None→Ok(None) unbound. GCN provider APPENDS to the slice, not special-cased in executor. Executor::new(mode, sink, state, providers: &'a dyn ShaderProvider) + new field; dispatch_draw_auto → resolve_embedded_pair(self.providers, &bound); resolve_embedded_pair + embedded_id take &dyn ShaderProvider (no more EmbeddedShaderProvider::new() hardcode). Embedded path UNCHANGED (same [BindEmbeddedPipeline{vs,ps}, DrawAuto] list) — proven by embedded_bound_draw_dispatches_host_pipeline + defer/unbound tests. submit.rs builds [&embedded] slice → ChainProvider → passes &chain. DECISION-7 (proposed): guest-minted PipelineId (mirrors ResourceId) + future BackendCmd::CreatePipeline{id, vs_spirv:Arc<[u32]>, ps_spirv:Arc<[u32]>,..}, fire-and-forget, display-side pipeline cache keyed by PipelineId; variant NOT implemented (gpu.rs untouched); refs decision-6 + doc-4 §4. task-42 appends a provider + feeds HostShader.spirv; task-52/53 add PipelineId+CreatePipeline/BindPipeline to BackendCmd + display cache. Verify: gnm+libs 104 pass, clippy 0, fmt clean, gnm Vulkan-free. Combined gate: 29 suites, oracle 6/6.
<!-- SECTION:NOTES:END -->
