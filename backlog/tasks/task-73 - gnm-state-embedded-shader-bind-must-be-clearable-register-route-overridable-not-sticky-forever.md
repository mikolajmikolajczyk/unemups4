---
id: TASK-73
title: >-
  gnm/state: embedded shader bind must be clearable / register route overridable
  (not sticky forever)
status: Done
assignee: []
created_date: '2026-07-12 06:01'
updated_date: '2026-07-13 20:00'
labels:
  - gpu
  - gnm
dependencies: []
priority: low
ordinal: 72000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Code-review finding #8 (latent design). derive_bound_shaders (state.rs:158) starts from self.shaders and for each stage where an Embedded bind exists, skips register derivation (state.rs:178 'continue'). self.shaders is set ONLY by bind_embedded_shader and is NEVER cleared — clear_regs (task-43, deliberate) leaves the shader view intact. So once a stage is bound embedded, it is embedded FOREVER: a later sceGnmSetVsShader (register route) writes PGM regs but derive_bound_shaders keeps returning the stale embedded shader. Harmless for the target path (real games use only the register route → self.shaders stays None) and intended for the current phase-3.5 corpus (embedded wins, Tier B unchanged), but it is a latent trap: any future corpus/app that switches a stage from embedded to register silently keeps the wrong shader. This touches the DELIBERATE task-43 clear-keeps-binds decision — decide the right model (e.g. an explicit unbind, or register-write clears the embedded shadow for that stage, or a per-frame bind reset) rather than have an agent guess. FILE-scoped to state.rs (+ maybe the HLE bind handlers).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 a stage can transition embedded → register: after an embedded bind then a register PGM_LO/HI write, derive_bound_shaders yields the register GcnBinary (not the stale embedded), unit-tested
- [x] #2 the phase-3.5 embedded corpus (ps4-pm4-test Tier B) is unchanged (embedded still wins when NO register bind is programmed)
- [x] #3 the chosen model is documented against the task-43 clear-keeps-binds decision
<!-- AC:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Model (verified): sceGnmSetVsShader/SetPsShader ARE HLE-hooked and emit PM4; the phase-3.5 corpus uses raw emit_set_reg (never calls them). So the HLE register-bind handlers clear the embedded shadow for that stage (unbind_embedded_shader), while a raw PM4 SET_SH_REG write does not. Add BoundShaders::clear + GnmState::unbind_embedded_shader; call from the two HLE handlers; AC#1/#2 unit tests. Respects task-43 (clear_regs keeps binds).
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Done 2026-07-13. HLE register handlers (sceGnmSetVsShader/SetPsShader) clear the embedded shadow (unbind_embedded_shader); raw PM4 SET_SH_REG (corpus) does not → Tier B unchanged. AC#1 unbind_embedded_lets_register_route_win + AC#2 derive_prefers_embedded_over_pgm_regs pass; full gnm/libs suites + embedded-draw tests green. Model documented vs task-43.
<!-- SECTION:NOTES:END -->
