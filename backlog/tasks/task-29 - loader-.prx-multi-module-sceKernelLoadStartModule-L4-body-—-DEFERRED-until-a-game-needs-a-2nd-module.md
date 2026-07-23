---
id: TASK-29
title: >-
  loader: .prx multi-module load + dynamic link + sceKernelLoadStartModule
  (retail FASE 1)
status: Done
assignee: []
created_date: '2026-07-10 20:28'
updated_date: '2026-07-23 18:41'
labels:
  - bloodborne
  - loader
  - retail
dependencies:
  - TASK-113.1
priority: high
ordinal: 29000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
SPEC = doc-3 (loader architecture) layer L4 body + migration step 4 (behavior-adding, the LATER one). Read doc-3 first. DEFERRED by design — do NOT implement speculatively; this waits until a real target (Bloodborne or another dump) actually needs a second module. Filed now only to encode the roadmap + dependency chain in the backlog. Builds on task-28 (per-module base + real ids already wired) + task-27 (SCE-dynamic/NID so retail .prx imports resolve). Scope when it runs: .prx file discovery (sce_module/*.prx in a game dir like /home/mikolaj/PS4/CUSA03173/sce_module/); a dependency-ordered load loop over load_image (a module must be mapped + its exports registered BEFORE a dependent relocates against it — topological order, load leaves first); the runtime sceKernelLoadStartModule entrypoint (re-enters load_image for a named .prx at runtime, re-using the static load path); full DTV construction + __tls_get_dynamic across modules (task-28 fixes the module id; this builds the per-module TLS vector). NO decryption (inherited; .prx are SELF-wrapped, unwrapped by L1/task-23 the same way as eboot). Over-engineering guard: no lazy PLT binding — the eager-resolve + lazy-syscall-stub-for-missing model stays.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 sce_module/*.prx discovered and each unwrapped (L1/task-23) + parsed (L2/task-26) + linked (L4/task-28) via load_image
- [x] #2 modules loaded in dependency order; a dependent's imports resolve against an already-registered module's exports (cross-module, SCE-dynamic/NID via task-27)
- [x] #3 sceKernelLoadStartModule runtime entrypoint loads+starts a named .prx by re-entering load_image
- [ ] #4 per-module DTV / __tls_get_dynamic correct across >=2 modules with TLS
- [ ] #5 six examples + any single-module path unchanged; no decryption anywhere; clippy -D warnings + fmt + cargo test clean
<!-- AC:END -->



## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
FASE 1 of retail bring-up. Un-deferred: a managed-runtime title needs many native modules (host + interop + audio middleware + libc + fs). Load host + N .prx as separate modules in the identity space; resolve inter-module imports (NID -> export) across them; per-module TLS; relocations cross-module; init/fini in dependency order; sceKernelLoadStartModule body + dlsym-equivalent. Depends on FASE 0 (SELF unwrapper) to ingest each SELF-wrapped module.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
SMOKE RECON of CUSA11302 eboot (2026-07-14), FASE-1 scope map:

STATE: eboot SELF-unwraps clean (task-113.1), 3207 relocations applied, reaches entry 0x400280, main thread jumps — then FATAL immediately: guest calls unresolved import NID 'bzQExy189ZI' (a libc/CRT-startup symbol; the NID appears verbatim in data/oo_sdk .../Dynlib Data.md as 'bzQExy189ZI#B#C', i.e. symbol#lib#module encoding).

WALLS (all FASE-1 dynamic-link):
1. 1156 imports stubbed-missing (all raw NIDs) — none resolve because the sibling modules that export them are never loaded.
2. 8 required libs (from SCE dynamic): libc, libScePosix, libkernel, scePlayStation4, libfmod, libfmodstudio, libSceSystemService, libSceNet.
3. 7 'Invalid DT_NEEDED' (string-table offsets 1,21,35,49,73,85,103) — SCE dynlib name/id table partially parsed (8 names resolved, 7 not). SCE dynamic-format parse gap.

AVAILABLE .prx in dump to LOAD+LINK: scePlayStation4, libfmod, libfmodstudio, sce_module/libc, sce_module/libSceFios2. NO .prx for: libScePosix, libSceSystemService (need HLE); libkernel + libSceNet already HLE.

FASE-1 (task-29) work implied: (a) finish SCE dynamic parse so all DT_NEEDED + import/export tables + module/library ids read; (b) load the 5 sibling .prx as modules; (c) cross-module NID link: eboot imports resolve against .prx exports; (d) route libScePosix/libSceSystemService to HLE (posix likely via libc.prx). Once linked, the 1156 imports resolve from real module exports (libc/scePlayStation4/fmod) + HLE. Next wall after that = FASE 2 (runtime init).

Full log: scratchpad/celeste_smoke.log (1196 lines).

PROGRESS 2026-07-14 (committed 2498966): multi-module LOAD + LINK works.
- Auto-load sibling .prx leaves-first (post-order DFS over DT_NEEDED) from the dump dir before the executable; libs w/o local .prx skipped (HLE/absent).
- Fixed ParsedImage memory_size: was sum(p_memsz), now span max(p_vaddr+p_memsz) — the sum under-reserved and later module bases collided ('Memory collision'). Single-module (Doom) unaffected.
- RESULT on retail smoke: all 5 .prx load (libc@0x92c000, libSceFios2@0x400000, libfmod@0xd34000, libfmodstudio@0x1180000, scePlayStation4@0x1584000, eboot@0x1988280), 4252 relocations, ~466/1156 imports resolve, guest EXECUTES real module code past CRT _init_env. No more link-time missing-symbol FATAL.

NEXT WALL (FASE-2 boundary): guest dies on UnmappedMemory read 0x0 at rip 0x9d8554 (mov (%rbx),%rax, rbx=0) — INSIDE libc (0x92c000-0xd34000). CONFIRMED CAUSE: loader never calls module init (grep: zero init_array/module_start/DT_INIT). libc globals uninitialized -> null deref when the eboot CRT calls into libc.

REMAINING task-29: (a) call each module's init in dependency order (DT_INIT + DT_INIT_ARRAY, and PS4 module_start via the module's entry/PT_SCE_PROCPARAM) before jumping to the eboot entry; (b) explicit sceKernelLoadStartModule API (deferred until a game calls it — auto-load-by-DT_NEEDED covers the static case). 690 stubs remain (each .prx's own unresolved imports; will shrink as HLE/init lands).

MODULE-INIT ATTEMPT (2026-07-14, REVERTED — wrong mechanism identified):
Tried calling each dependency's module_start before the eboot entry via DT_INIT (goblin dynamic.info.init) + run_guest_call on the main thread (plumbed init through ExecutableImage::init_offset -> Module.init + ModuleManager.load_order, called leaves-first). ALL inits faulted (HLT / exec-of-0 / exec-unmapped).
ROOT CAUSE: goblin reports DT_INIT = 0x4000 for every .prx (= file offset of PT_LOAD[0], NOT a function). Verified libSceFios2: PT_LOAD[0] vaddr=0 filesz=0x5e748, PT_LOAD[1] vaddr=0x400000; the bytes at module vaddr 0x4000 are F4 44 89... — F4 = HLT. init_array present but init_arraysz=0 (empty). So neither standard DT_INIT nor DT_INIT_ARRAY gives module_start.
CONCLUSION: PS4 module_start/module_stop live in the SCE module_info / PT_SCE_PROCPARAM structure (referenced by DT_SCE_MODULE_INFO 0x6100000d, which SceDynamic already knows as a constant but does not yet decode). NEXT INCREMENT: parse the SCE module_info to get the real module_start offset per module, store in Module, call leaves-first (the run_guest_call sequencing worked; only the address source was wrong). Reference: OpenOrbis PS4 ELF spec (module_info / PT_SCE_PROCPARAM). Reverted to clean milestone 2498966 (multi-module load+link intact).

MODULE-INIT DONE (2026-07-14, milestone): module_start sequencing works. The earlier "need SCE module_info" conclusion was WRONG — the bug was reading DT_INIT via goblin's `dynamic.info.init` (which returned 0x4000 = a file offset, → HLT). Empirical recon of the real .prx (libc/libSceFios2/scePlayStation4): all are e_type=0xfe18, e_entry=0, DT_INIT=0 (raw), and the bytes at module-relative vaddr 0 are `55 48 89 e5 41 57...` (`push rbp; mov rbp,rsp; push r15..`) = a REAL function prologue. So module_start = base + e_entry (== base + DT_INIT = base+0), which the linker ALREADY stores as `Module.entry_point`. No SCE module_info decode needed.
IMPLEMENTATION (no loader/trait change): `load_module_tree` records each loaded .prx's `entry_point` into `Process.module_inits` in post-order (leaves-first); `Thread::execute` (main thread only) calls each via `run_guest_call(module_start, rsp, rdi=0, ...)` before jumping to the eboot entry. Empty for single-module homebrew → Doom + 5 examples unchanged (verified, no FATAL, clippy/fmt/67 tests clean).
RESULT on retail smoke: module_start[1/5] libSceFios2 @0x400000 -> returned 0; libc module_start @0x92c000 runs deep, cascading through the libc-init HLE hook family — stubbed no-op (return 0) this session: `_sceKernelSetThreadDtors`, `_sceKernelSetThreadAtexitCount`, `_sceKernelSetThreadAtexitReport`, `_sceKernelRtldThreadAtexitIncrement`, `_sceKernelRtldThreadAtexitDecrement` (all libkernel/pthread.rs). The prior null-deref @0x9d8554 is GONE.
NEW WALL = FASE-2 boundary (task-113.3): libc module_start calls `sceKernelGetProcParam` (NID 959qrazPIrg) — needs PT_SCE_PROCPARAM (type 0x61000001) parsed from the eboot + the proc-param guest address plumbed to the HLE handler, then libc reads sceLibcParam (heap config). That is native-runtime bring-up, not module load/link → hand to 113.3.
task-29 remaining: AC#3 sceKernelLoadStartModule runtime entrypoint (deferred until a game calls it), AC#4 per-module DTV/__tls_get_dynamic across ≥2 modules (not yet exercised).

AC#3 DONE (2026-07-15): runtime dynamic module load (sceKernelLoadStartModule + sceKernelDlsym). Celeste's Mono runtime dynamically loads /app0/scePlayStation4.prx (the native interop .prx — NOT in eboot DT_NEEDED, genuinely runtime) then resolves exports by name via dlsym.

DESIGN:
- Handle table = reuse the ModuleManager module handle (positive i32) as the LoadStartModule return value; dlsym takes it back. `Process.loaded_by_path: Mutex<HashMap<guest_path, handle>>` makes a repeat load idempotent (returns the cached handle with module_start=0 so init runs only once).
- `Process::load_start_module(guest_path) -> Result<(handle, module_start), errno>`: translate guest path -> host path via a new `FileSystem::host_path` (exposes the existing private `resolve`, same mount translation `open` uses); derive game_dir (parent, hop up out of sce_module/); parse+link the .prx and its DT_NEEDED deps leaves-first by reusing the existing `load_module_tree`, then `linker.load_image`. Returns the module's `entry_point` (== base+e_entry == module_start).
- `Process::module_dlsym(handle, name) -> Option<u64>`: retail .prx exports are keyed by BARE NID (SceDynamic::exports stores `decoded.nid`). So look up the plain name first (homebrew), else hash name->NID via `ps4_syscalls::SyscallId::from_symbol_name(name).nid()` and look that up. No hash reimpl (nid.rs contract).
- Two new KernelInterface trait methods (ps4-core) delegated by KernelBridge to Process.
- Handlers (crates/libs/src/libkernel/mod.rs) replace the temp stub: guard every guest ptr with is_guest_ptr (added a shared `read_guest_cstr(ptr, max)` bounded reader), call the kernel bridge to load, then run module_start as a NESTED guest call via `ps4_cpu::call_guest(module_start, argc)` (uses the current thread's exec context — VM/stack/tls/errno — installed by run_guest_call; the handler runs on the guest thread so the context is live). Write *pRes (module_start result) and *funcOut (export addr) through the guarded guest ptr (JIT identity-maps). Negative return = -errno.

FILES: crates/kernel/src/fs.rs (host_path accessor), crates/kernel/src/process.rs (load_start_module + module_dlsym + loaded_by_path), crates/core/src/kernel.rs (2 trait methods), crates/kernel/src/bridge.rs (delegation), crates/libs/src/libkernel/mod.rs (real handlers replacing sce_kernel_load_start_module_tmp + new sceKernelDlsym), crates/loader/src/manager.rs (unit test nid_keyed_export_resolves_from_plain_name). Temp stub REMOVED. clippy -D (touched crates) + fmt + 253 tests green.

RESULT on retail smoke (interp): sceKernelLoadStartModule('/app0/scePlayStation4.prx') -> handle 17, module_start 0x5444000 runs (deep pthread/mutex/TLS/key setup), *pRes written; sceKernelDlsym(handle 17, ...) then probes optional C++ symbols (`_ZN5Audio11SoundSystem...`) that this .prx genuinely does not export -> we return -ENOENT and the interop layer gracefully continues (correct — guest probes name variants incl. a trailing 'A'). Guest proceeds into audio-out init.

NEW WALL (FASE-3 boundary, hand to task-113.4): missing HLE symbol `sceAudioOutSetVolume [NID b+uAV89IlxE]` after sceAudioOutInit + sceAudioOutOpen(handle=1 grain=256 rate=48000 fmt=5). Log tail:
  [AUDIO] sceAudioOutInit
  [AUDIO] sceAudioOutOpen handle=1 grain=256 rate=48000 fmt=5
  [FATAL ERROR] The application crashed because it called a missing symbol: sceAudioOutSetVolume [NID b+uAV89IlxE]
This is a plain missing-HLE-export (add a stub in libscaudioout), NOT a load/link wall — module load+link+start+dlsym all cleared. AC#4 (per-module DTV/__tls_get_dynamic across ≥2 modules) still not exercised.
<!-- SECTION:NOTES:END -->
