---
id: TASK-30
title: >-
  loader: resolve retail NID imports via existing ps4-syscalls MAP_BY_NID; drop
  duplicate loader NID hash
status: Done
assignee: []
created_date: '2026-07-10 21:30'
updated_date: '2026-07-11 05:25'
labels:
  - loader
  - bloodborne
dependencies:
  - TASK-27
priority: high
ordinal: 30000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
CORRECTS a false-alarm in task-27's notes. task-27's agent claimed the repo's NID scheme (ps4-syscalls build.rs calculate_nid) differs from the retail canonical form and deferred resolution as a 'mismatch'. THAT IS WRONG — verified: (a) mathematically, ps4-syscalls calculate_nid = SHA1(name||salt)[0..8] as LE u64 -> base64(big-endian bytes) = the SAME output as the canonical OpenOrbis StringToNid bit-slice (task-27's loader/nid.rs nid_for); (b) empirically, the generated MAP_BY_NID already contains the CANONICAL NID rTXw65xmLIA -> 94578 (sceKernelAllocateDirectMemory) and 1jfXLRVzisc (sceKernelUsleep). Same salt (518D64A635DED8C1E6B039B1C3E55230) in both. So there is NO second NID scheme. The full canonical NID list + the #[ps4_syscall]/inventory macro that assigns NIDs to syscall ids ALREADY EXIST, and the resolution path is ALREADY WIRED: SyscallId::from_nid(nid) binary-searches MAP_BY_NID; crates/kernel/src/hle.rs registers every HLE export under THREE keys incl. def.id.nid() (the canonical NID); ModuleManager::resolve_symbol(key) finds the stub. Therefore a retail import whose Relocation/Import symbol_name carries the raw NID string resolves to the correct HLE syscall stub with NO new hashing. SCOPE: (1) confirm task-27's SceDynamic populates Import/Relocation symbol_name with the raw NID (or a key resolve_symbol matches) and that a retail import resolves end-to-end to an HLE stub via the existing table (add a test loading the real Bloodborne eboot behind the ignore/guard, assert e.g. an sceKernel* import resolves); (2) DROP crates/loader/src/nid.rs's forward-hash reimplementation — use ps4_syscalls::SyscallId::from_nid / .nid() instead (keep only what SceDynamic genuinely needs that ps4-syscalls does not expose, e.g. encode_id for lib/module id decoding, if any); (3) fix task-27's notes + decision-5 to remove the 'mismatch' claim. NON-GOAL: any change to the NID algorithm or the generated table (they are correct); per-module/.prx work (task-28/29).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 Verified + tested: a retail import (raw NID in symbol_name) resolves to the correct HLE syscall stub via the existing SyscallId::from_nid / hle.rs NID-keyed exports / resolve_symbol path — no new hashing; test over the real Bloodborne eboot (ignored/guarded) asserts at least one sceKernel* import resolves
- [x] #2 crates/loader/src/nid.rs forward-hash duplication removed; the loader uses ps4_syscalls::SyscallId::{from_nid,nid} for NID<->id/name; only genuinely-unique helpers (if any, e.g. encode_id) retained with a comment
- [x] #3 task-27 notes + decision-5 corrected: no canonical-vs-internal NID mismatch; the generated MAP_BY_NID is canonical
- [x] #4 homebrew unchanged (six examples byte-identical); clippy -D warnings + fmt + cargo test clean; no decryption
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Done 2026-07-11. Retail NID resolution flows: SceDynamic puts the raw on-disk NID string into Import/Relocation.symbol_name; hle.rs registers every HLE export under def.id.nid() (canonical, from generated MAP_BY_NID); linker calls ModuleManager::resolve_symbol(symbol_name) which hits that NID key — NO hashing at resolve time. Removed nid.rs forward-hash dup (nid_for/NID_SALT/NidDatabase/HashNidDatabase); kept encode_id (lib/module id decode, ps4-syscalls doesn't expose it) + a canonical-NID guard test. Added ps4-syscalls dep to loader, dropped now-unused sha1. Tests: loader dynamic.rs tests now derive expected NID via SyscallId::from_symbol_name().nid(); new kernel hle.rs retail_nid_import_resolves_to_hle_stub (synthetic, always runs) + ignored guarded bloodborne_ebot_sce_kernel_import_resolves. Real eboot run: NID 1G3lF1Gg1k8 (sceKernelOpen) -> stub 0x20000c60. Corrected task-27 notes + decision-5 (no NID mismatch; MAP_BY_NID is canonical). Verify: build+test(62 pass/3 ign)+clippy clean+fmt clean; oracle only-known-artifact diffs (Vulkan headless + libSceGnmDriver task-20). No decryption. NOT committed.
<!-- SECTION:NOTES:END -->
