---
id: TASK-84
title: >-
  gnm/exec: resolve each ShaderRef once (not twice), parse_sb_bounded unwired
  diagnostic, RAII test hooks
status: Done
assignee: []
created_date: '2026-07-12 09:05'
updated_date: '2026-07-12 09:19'
labels:
  - gpu
  - gnm
dependencies: []
priority: medium
ordinal: 83000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Code-review round-4 findings #2/#10/#6(exec-side). #2 [correctness, bites task-42]: resolve_embedded_pair (crates/gnm/src/exec.rs:208-231) resolves EACH ShaderRef TWICE — once in the Err-check loop (exec.rs:214 provider.resolve(r,&mem).is_err()) and again in embedded_id (exec.rs:237 provider.resolve(r,mem)). Stateless-harmless for the embedded provider, but task-42's GcnShaderProvider (parse_sb + SPIR-V recompile, side-effecting/caching) will run the FULL resolve TWICE per draw. FIX: resolve each ref ONCE, inspect the single Result (Err→NeedsGcn, Ok(Some)→embedded id, Ok(None)→unbound) and reuse it — no second provider.resolve call. #10 [quality]: parse_sb_bounded (exec.rs ~243+) returns Err(MemoryFault) when no bounded source is wired (headless None path), indistinguishable from a genuinely-unmapped address — task-53's harness forgetting register_bounded_read gets MemoryFault on every shader parse with no diagnostic. FIX: log once (or a distinct error/variant) when the seam is ABSENT vs when a wired read genuinely faults. #6(exec-side) [test-safety]: the parse_sb_bounded test uses a module-local SEAM_LOCK + raw register_bounded_read/clear_bounded_read — a panic between register and clear leaks the wired source to later tests in the ps4-gnm binary. FIX: convert this test to the RAII ps4_core registered override guard (override_scoped / override_none_scoped) so the global is restored on drop even on panic. Do NOT delete the clear_bounded_read fn itself (a follow-up removes it once no caller remains) — just stop USING it in exec.rs tests.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 each ShaderRef is resolved exactly once per dispatch_draw_auto (no double provider.resolve); embedded draw + NeedsGcn defer + unbound behavior unchanged — unit tests + a spy provider asserting call-count==1 per ref
- [ ] #2 parse_sb_bounded logs/signals distinctly when the bounded seam is unwired vs a genuine read fault
- [ ] #3 the exec.rs parse_sb_bounded test uses the RAII override guard (panic-safe), not raw register/clear
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
DONE 2026-07-12 (fix/task-84 @ 71648c2, merged). #2: resolve_embedded_pair resolves each stage ONCE — new embedded_id_of folds the single provider.resolve + ref-kind check → Result<Option<u32>, ShaderUnsupported>; map each bound stage once, any Some(Err)→NeedsGcn, both Some(Ok(Some id))→Embedded{vs,ps}, else Unbound. Old embedded_id (2nd resolve) gone. Spy test each_shader_ref_resolved_exactly_once_per_draw (CountingProvider, per-stage count==1). Saves task-42's GcnShaderProvider from double parse_sb+recompile per draw. #10: parse_sb_bounded None branch emits one-time tracing::warn! ('bounded read seam not wired...') + code_start before Err(MemoryFault) — distinguishes unwired-seam from genuine fault; still refuses parse (no unbounded fallback). #6: parse_sb_bounded test → RAII override_scoped (wired, own scope so drops before headless assert) + override_none_scoped (headless); SEAM_LOCK static dropped (guard serialization covers it); clear_bounded_read fn untouched (task-85 domain). Verify: gnm+libs 105 pass, clippy 0, fmt clean, gnm Vulkan-free. Combined gate: 29 suites, oracle 6/6.
<!-- SECTION:NOTES:END -->
