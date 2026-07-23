---
id: TASK-83
title: >-
  gnm/cache+gpu: cache correctness — bounded upload read, single-copy Arc,
  import-veto lying-state, decline coverage
status: Done
assignee: []
created_date: '2026-07-12 09:05'
updated_date: '2026-07-12 09:19'
labels:
  - gpu
  - gnm
  - core
dependencies: []
priority: high
ordinal: 82000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Code-review round-4 findings #1/#7/#3/#5 (all cache correctness; cohesive). #1 [HIGH]: emit_upload (crates/gnm/src/cache/mod.rs:346) reads the guest buffer via mem.read_bytes(key.addr, key.size) — UNBOUNDED. On the real VmMemoryManager read_bytes over-reads past a VMA boundary/gap into raw host memory (SIGSEGV or garbage snapshot shipped in the UploadBuffer Arc). This is the EXACT over-read class task-75/80 killed for the regs/parse paths, reintroduced on the cache upload path by the task-79 reshape. FIX: call mem.read_bytes_ranged(key.addr, key.size) (same &dyn VirtualMemoryManager trait object already has it); on Err emit no UploadBuffer + keep the entry dirty (task-71 semantics). NOTE: the BufMem test stub (cache/tests.rs) does not override read_bytes_ranged → after this change it must impl the ranged read (or impl BoundedRead) so the AC-#1 'first use = create+upload' tests still pass. #7 [EFFICIENCY]: emit_upload then does bytes.into() (cache/mod.rs:351) Vec<u8> -> Arc<[u8]> which allocates + memcpys AGAIN (read_bytes already allocated+copied once). FIX: Arc::from(bytes.into_boxed_slice()) reuses the Vec allocation. #3 [ALTITUDE/correctness]: ImportProbe is authoritative (task-79) but the display-side replay_import (crates/gpu/src/backend.rs) LOGS an error and returns on device decline, while the cache already recorded Entry::imported=true+clean — so the entry is permanently imported with NO vk::Buffer ever created → every subsequent draw consumes an absent/zero resource forever, no retry, no veto. FIX: make the probe provably-honorable (the guest-side ImportProbe mirror must reflect boot-resolved device caps so a probe-yes can ALWAYS be honored), and treat a replay_import decline as a hard programming error (panic/abort or an explicit downgrade path), NOT a silent log+continue that strands the cache. Decide the cleanest of {conservative-mirror, fail-fast, downgrade-command} and document it. Only reachable with a non-default (non-copy-side) import policy today, but fix before task-53/55 enable import. #5 [COVERAGE]: task-79 deleted the import-decline branch test with no replacement — add a cache test asserting the copy-path command list (CreateBuffer+UploadBuffer) is emitted when the probe returns false (Coherence::CopySide / always-no probe), and that ImportBuffer is NOT emitted then.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 emit_upload uses read_bytes_ranged; a buffer whose range crosses a VMA gap does NOT over-read (emits no upload, stays dirty) — unit test on a boundary-crossing key
- [ ] #2 the Vec->Arc<[u8]> path is single-allocation (Arc::from(into_boxed_slice) or equivalent); emitted UploadBuffer bytes unchanged
- [ ] #3 a probe-yes the display cannot honor no longer strands the cache (conservative mirror / fail-fast / downgrade — documented); replay_import decline is not a silent-log-and-continue
- [ ] #4 a test asserts copy-path (Create+Upload, no ImportBuffer) when the probe declines; existing task-49 ACs still hold
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
DONE 2026-07-12 (fix/task-83 @ 887f50b, merged). #1: emit_upload now mem.read_bytes_ranged (was unbounded read_bytes) — no over-read on VMA-gap buffer; Err→no UploadBuffer+stays dirty (task-71). BufMem stub overrides read_bytes_ranged (length-validated); FailMem's override renamed to read_bytes_ranged so fail-toggle fires on upload path. #7: bytes.into() → Arc::from(bytes.into_boxed_slice()) (single alloc, reuses Vec). #3 IMPORT-VETO DECISION = (b) FAIL-FAST: replay_import (gpu/backend.rs) now assert!s on device decline instead of tracing::error!+return. Rationale: (a) provably-honorable mirror unachievable — per-pointer import success depends on runtime alignment vs minImportedHostPointerAlignment + driver acceptance the boot mirror can't know; (c) downgrade reintroduces the channel round-trip the authoritative-probe design kills. Cache already recorded imported+clean, so any silent recovery strands it serving absent/zero buffer forever. Documented in cache mod doc + ImportProbe trait doc + replay_import doc. #5: probe_declines_emits_copy_path_no_import (copy-side + garlic-candidate-NoImport → exact Create+Upload, no ImportBuffer) + boundary_crossing_key_does_not_over_read. FLAG task-53/55: default copy-side → panic unreachable today; when they wire a real ImportProbe it MUST only return true for boot-caps-certainly-importable ranges (over-eager yes = hard display-thread crash by design). Verify: gnm+core+gpu 103 pass, clippy 0, fmt clean, gnm Vulkan-free. Combined gate: 29 suites, oracle 6/6.
<!-- SECTION:NOTES:END -->
