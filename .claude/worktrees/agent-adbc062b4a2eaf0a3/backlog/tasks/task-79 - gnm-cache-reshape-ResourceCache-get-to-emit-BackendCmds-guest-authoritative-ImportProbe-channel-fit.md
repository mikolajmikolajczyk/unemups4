---
id: TASK-79
title: >-
  gnm/cache: reshape ResourceCache::get to emit BackendCmds +
  guest-authoritative ImportProbe (channel-fit)
status: Done
assignee: []
created_date: '2026-07-12 07:54'
updated_date: '2026-07-12 08:19'
labels:
  - gpu
  - gnm
  - core
dependencies: []
priority: medium
ordinal: 78000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Fable phase-4 quality review finding #1 (ARCHITECTURE — strongest shape mismatch). crates/gnm/src/cache/mod.rs:228-315 (get/try_import/upload) + crates/core/src/gpu.rs:92-98 (try_import_host_range). The module doc correctly mints ResourceIds guest-side, but get() still takes &mut dyn GpuBackend and calls create_resource/upload/try_import_host_range SYNCHRONOUSLY. The cache's only production home (executor, task-53/55) runs on the guest thread holding only &dyn PresentSink — NO GpuBackend reachable there. create_resource/upload can go fire-and-forget over the channel, but try_import_host_range->bool CANNOT, and that bool sets Entry::imported which changes dirty semantics (imported = never re-uploaded). The ImportProbe mirror was invented for exactly this, yet the code treats the backend's sync answer as authoritative — contradicting the mirror's 'hint, never correctness path' doc. FIX (before task-53 wires it): reshape get to append BackendCmds, e.g. get(&mut self, key, mem, dirty, out: &mut Vec<BackendCmd>) -> ResourceId, and make the guest-side ImportProbe AUTHORITATIVE — if the probe says import, the display thread MUST import (its failure = logged hard error), so  is decided guest-side with no round-trip; try_import_host_range's sync-bool becomes backend-internal. Coordinate with task-52 (per-submit BackendCmd list) and task-49's id model. WHY NOW: first wiring otherwise needs an awkward channel-adapter faking GpuBackend + the import fork is silently dead/wrong; reshaping after task-38..53 pile on means rewriting cache tests too.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 get() emits BackendCmd(s) (create/upload) instead of driving GpuBackend synchronously; no &mut dyn GpuBackend on the guest-thread path
- [ ] #2 Entry::imported is decided by the guest-side ImportProbe (authoritative); a probe-yes that the display thread can't honor is a logged hard error, not a silent fallback
- [ ] #3 MockBackend cache tests updated to the command-emitting shape; first-use/clean/dirty/import ACs from task-49 still hold
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
DONE 2026-07-12 (feat/task-79 @ 1e2611e, merged). get() reshaped: get(&mut self, key, mem, dirty, out: &mut Vec<BackendCmd>) -> ResourceId (dropped &mut dyn GpuBackend). Helper emit_upload(id,key,mem,out)->bool replaces upload/reupload (false on read fail → entry stays dirty, preserves task-71). try_import removed from cache. NEW BackendCmd variants (core/gpu.rs): CreateBuffer{id,size}, UploadBuffer{id,offset,data:Arc<[u8]>}, ImportBuffer{id,addr,size}. Arc<[u8]> payload → BackendCmd LOST Copy, now derive(Clone,Debug,PartialEq,Eq). ImportProbe AUTHORITATIVE: Entry::imported = coherence==ZeroCopyCandidate && policy.probe.can_import(...), NO backend round-trip; probe-yes pushes ImportBuffer + no upload; AshBackend::run_command_list replay_import logs tracing::error! on device decline (hard error, NOT silent copy fallback). try_import_host_range bool now backend-internal. Tests → command-assertion (first-use=Create+Upload, clean=empty, dirty=1 Upload, import=1 ImportBuffer no upload, failed-read=no upload+stays dirty); all task-49 ACs hold. TASK-51/52 MUST AGREE: BackendCmd is Clone not Copy (match cmd not match *cmd); UploadBuffer carries Arc<[u8]>; per-submit Vec<BackendCmd> interleaves cache create/upload/import with bind/draw; display owns import hard-error contract. Verify: gnm+core+gpu 100 pass, clippy 0, fmt clean, gnm Vulkan-free. Combined gate: 29 suites, oracle 6/6.
<!-- SECTION:NOTES:END -->
