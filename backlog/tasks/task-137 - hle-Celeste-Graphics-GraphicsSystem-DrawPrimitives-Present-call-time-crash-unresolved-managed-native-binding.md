---
id: TASK-137
title: >-
  hle: Celeste Graphics::GraphicsSystem::DrawPrimitives/Present call-time crash
  (unresolved managed native binding)
status: Done
assignee: []
created_date: '2026-07-16 11:46'
updated_date: '2026-07-16 12:11'
labels:
  - hle
  - celeste
  - retail
  - bug
dependencies: []
priority: high
ordinal: 143000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
The REAL current Celeste (CUSA11302) wall after task-135. Celeste boots Mono, submits GNM PM4, but the process SIGSEGVs after submit and the frame stays white. Root cause characterized (task-135 live run, main 7d690da): a host SIGSEGV (SEGV_MAPERR, rc=139) inside JIT'd guest code on a guest worker thread — faulting instr 'cmpl $0x3b9ce510, 0xb0(%rax)' with %rax null/garbage, a guest deref at struct offset 0xb0 immediately downstream of FAILED 'Graphics::GraphicsSystem::DrawPrimitives' / 'Graphics::GraphicsSystem::Present' dlsym lookups.

Refines the earlier belief that 'Graphics:: dlsym is WARN, non-fatal': it is non-fatal at RESOLVE time (the game tolerates the not-found and keeps booting) but FATAL at CALL time — when Celeste actually invokes DrawPrimitives/Present it dispatches through the unresolved (null/garbage) target and derefs into nothing. These 'Graphics::GraphicsSystem::*' (C++-mangled _ZN8Graphics14GraphicsSystem*) are managed P/Invoke declarations living in Sce.PlayStation4.dll, targeting scePlayStation4.prx; the game calls them once its graphics path goes live. Full list seen NOT-FOUND in the live log: SetShaderConstants, SetTexture, CreateSamplerState, SetSamplerState, DrawIndexedPrimitives, DrawPrimitives, Present (+ more Graphics::Texture/RenderTarget names).

Investigate + fix: provide these Graphics::GraphicsSystem::* entry points so the guest call does not dispatch into garbage. Options to weigh: (a) HLE-stub the whole Graphics::GraphicsSystem native surface (each returns cleanly / wires to our GNM path), (b) ensure the managed->native binding resolves to a real trap-stub (like our sceKernel* SYSCALL stubs) instead of a null, so an unimplemented call is a clean guest-visible error not a host segfault, (c) find whether scePlayStation4.prx itself SHOULD export them and we mis-link. Start read-only: confirm HOW the guest resolves/calls these (dlsym handle 19 path), what the call ABI is (thiscall? struct at 0xb0?), and which of DrawPrimitives/Present is first-called. Then the minimal fix to get past the crash. RELATED: mem->register IT_DMA_DATA deferral (task-135 finding) may be how Celeste programs draw registers — a separate downstream lead, investigate only after the crash clears. NEVER copy other-emulator source; RE from the dumped guest binary. Assets at /home/mikolaj/PS4/CUSA11302 are gitignored, NEVER commit.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 The guest call to Graphics::GraphicsSystem::DrawPrimitives/Present no longer host-SIGSEGVs — either the entry points are provided/stubbed or the binding resolves to a clean guest-visible trap-stub (no dispatch into null/garbage)
- [x] #2 Documented: how the managed runtime resolves+calls these names (dlsym handle-19 path + call ABI, incl. the struct-at-0xb0 the crash derefs), and which Graphics::GraphicsSystem::* method is invoked first
- [ ] #3 Live: Celeste re-run past the crash point (dump PNG via UNEMUPS4_DUMP_PNG for the orchestrator to Read); report the next wall reached
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Done 2026-07-16 (merge c6ce36d). Fix (b): sceKernelDlsym MISS now writes a lazily-mmapped process-global trap-stub (MOV EAX,0xA000_0000/DLSYM_TRAP_MARKER; SYSCALL; RET) into *func_out instead of leaving it null; call traps to rust_syscall_handler -> log once -> return 0. Marker disjoint from MAGIC_MASK so FATAL branch skips it. Hardens the null-dispatch hazard. AC#3 (live past crash) NOT met by this: coredump analysis PROVED the Celeste crash is NOT this path — RIP maps into /usr/lib/libvulkan_radeon.so (RADV) on the present thread ~65ms after sceGnmSubmitAndFlipCommandBuffers; same instr cmpl $0x3b9ce510,0xb0(%rax) but 0x3b9ce510=RADV sentinel, %rax=0x38 garbage handle. The dlsym trap-stub never fires for Celeste (managed code checks ENOENT). Real wall = malformed Vulkan submission -> RADV segfault -> filed as task-139 (gcn/gpu/gnm). NOTE straggler: the *func_out write here is a raw guest store (task-138 should migrate to GuestPtr).
<!-- SECTION:NOTES:END -->
