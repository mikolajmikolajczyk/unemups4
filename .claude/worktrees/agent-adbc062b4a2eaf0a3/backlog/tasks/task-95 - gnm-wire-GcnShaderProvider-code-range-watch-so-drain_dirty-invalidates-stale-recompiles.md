---
id: TASK-95
title: >-
  gnm: wire GcnShaderProvider code-range watch so drain_dirty invalidates stale
  recompiles
status: Done
assignee: []
created_date: '2026-07-12 18:22'
updated_date: '2026-07-12 18:50'
labels:
  - gpu
  - gnm
  - cpu
dependencies:
  - TASK-53
  - TASK-48
priority: medium
ordinal: 94000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
task-53 review finding (MINOR, latent). GcnShaderProvider::resolve (shader/gcn.rs) calls resolve_gcn(..., dirty: None), so shader .sb code ranges are NEVER passed to DirtySource::watch. task-53 wired driver-owned gcn.drain_dirty() per submit (submit.rs), but take_dirty only reports previously-watched ranges -> drain_dirty is a no-op for shaders. Trigger: guest recompiles then overwrites .sb code bytes at the same address in a later submit -> range never watched -> cache never invalidated -> executor binds STALE recompiled SPIR-V forever. Masked today under the AlwaysDirty fallback (re-recompiles every submit); becomes a silent stale-shader bug once the x86jit SMC-backed DirtySource lands. FIX: thread a DirtySource into ShaderProvider::resolve (signature change) or have the driver watch the resolved code_range, so drain_dirty actually invalidates a mutated shader.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 a shader whose .sb code bytes change between submits is re-recompiled (cache entry invalidated), unit-tested with a MockDirty
- [x] #2 the ShaderProvider::resolve dirty:None path is removed or documented as intentional with the watch happening elsewhere
<!-- AC:END -->



## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Approach (a): thread Option<&dyn DirtySource> into ShaderProvider::resolve (trait, embedded no-op, gcn watches code range, chain, executor passes self.dirty). Add independent MockDirty unit test driving watch->mutate->take_dirty->drain_dirty->re-resolve. Gate: build/test/clippy/fmt/run_examples 6/6.
<!-- SECTION:PLAN:END -->
