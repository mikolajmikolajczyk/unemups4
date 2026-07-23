---
id: TASK-85
title: >-
  core+libs: Registered<T> poison-safe restore + cfg-gate test_lock,
  bounded_read footgun guard, RAII test hooks
status: Done
assignee: []
created_date: '2026-07-12 09:05'
updated_date: '2026-07-12 09:19'
labels:
  - core
  - gpu
  - gnm
dependencies: []
priority: medium
ordinal: 84000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Code-review round-4 findings #4/#9/#8/#6(libs-side). #4 [correctness, test-bleed]: Registered<T> (crates/core/src/registered.rs) loses the prior value on a poisoned slot RwLock. override_inner (registered.rs:94-97) records prior=None on slot.write()==Err; ScopeGuard::drop (registered.rs:126-130) skips the restore entirely on slot.write()==Err. If any test panics while holding slot.write() the RwLock poisons → a live guard's drop leaves the global at the OVERRIDE value → cross-test bleed. FIX: recover the poisoned lock with unwrap_or_else(|e| e.into_inner()) at BOTH slot.write() sites in override_inner + Drop (the same recovery test_lock already uses at registered.rs:93), so the prior value is always captured + restored. #9 [quality]: test_lock: Mutex<()> (registered.rs:32) is NOT cfg-gated — it's compiled into every prod singleton, contradicting the 'test-only, does not leak into the library build' claim. FIX: gate the field + its uses behind #[cfg(any(test, feature="test-hooks"))] (keep const fn new() working — a cfg'd field with a cfg'd initializer, or split the struct), or if gating the field is impractical, correct the doc/Cargo claim to say the 1-byte lock is always present. #8 [robustness]: the blanket impl<T: VirtualMemoryManager + ?Sized> BoundedRead for T (crates/core/src/bounded_read.rs) routes read_ranged -> read_bytes_ranged, whose default (task-75) is now Err. So registering ANY non-overriding VirtualMemoryManager (IdentityMem, a boot stub) as the bounded_read source makes EVERY shader-regs read return Err silently → shaders unbound (a reversal from the old delegating default). Prod is safe (real VmMemoryManager overrides), but it's a silent footgun. FIX: document the contract loudly on register_bounded_read (the registered source MUST override read_bytes_ranged) and/or log once if a bounded read returns the 'not implemented' Err so a misregistration is diagnosable. #6(libs-side) [test-safety]: shader_bind.rs test out_of_bounds_regs_emits_nothing_and_never_over_reads calls raw register_bounded_read(mem2) INSIDE an override_scoped guard (bypasses test_lock) — convert to the RAII guard consistently so no raw register happens under a guard. Do NOT delete clear_bounded_read (follow-up).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 override_inner + ScopeGuard::drop recover a poisoned slot lock (into_inner) and always capture/restore the true prior value — test: poison the slot, assert a guard still restores prior
- [ ] #2 test_lock is cfg-gated out of prod builds OR the 'does not leak' claim is corrected to match reality; build green with and without test-hooks
- [ ] #3 register_bounded_read documents the must-override-read_bytes_ranged contract; a non-overriding source is diagnosable (logged), not a silent no-op
- [ ] #4 shader_bind out_of_bounds test uses the RAII guard consistently (no raw register under a guard)
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
DONE 2026-07-12 (fix/task-85 @ 58c1215, merged). #4: .write().unwrap_or_else(|e| e.into_inner()) at ALL FOUR slot.write() sites (register/reset/override_inner/ScopeGuard::drop) — register+reset were silent-no-op on poison; override_inner now captures TRUE prior (not None); drop always restores. Test poisoned_slot_still_captures_and_restores_prior (panicking scoped thread poisons slot, scoped override still restores prior 7). #9: cfg-gated test_lock field + init behind #[cfg(any(test, feature=test-hooks))] via two const fn new() bodies + cfg'd Mutex import; verified cargo build --release -p ps4-core green (field out of prod). Doc updated (now true). #8: register_bounded_read gets loud # Contract doc (source MUST override read_bytes_ranged); shared Err str as pub const RANGED_READ_UNIMPLEMENTED in memory.rs; blanket impl<T:VMM> BoundedRead matches it + warn_once (AtomicBool compare_exchange → single tracing::warn!) so misregistration diagnosable. #6: shader_bind out_of_bounds test → each phase own override_scoped (OOB guard scoped in {} to drop before in-bounds guard — non-reentrant mutex would deadlock nested); clear_bounded_read intact. Verify: workspace pass, core prod build green (test_lock gated out), clippy 0, fmt clean. Combined gate: 29 suites, oracle 6/6.
<!-- SECTION:NOTES:END -->
