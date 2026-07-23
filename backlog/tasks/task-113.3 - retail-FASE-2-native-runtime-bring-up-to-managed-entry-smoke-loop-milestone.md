---
id: TASK-113.3
title: 'retail FASE 2: native runtime bring-up to managed entry (smoke-loop milestone)'
status: Done
assignee: []
created_date: '2026-07-14 08:27'
updated_date: '2026-07-23 18:41'
labels:
  - retail
  - hle
dependencies:
  - TASK-29
  - TASK-113.2
parent_task_id: TASK-113
priority: high
ordinal: 115000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
FASE 2 (parent epic). Drive the native host runtime far enough to load the managed assemblies and begin executing AOT-compiled managed code on x86jit. Surface class (title-agnostic): GC arenas (mmap/mprotect/madvise patterns), threading (many threads, TLS, sync primitives), exception-unwind table registration (eh_frame), file I/O to read assemblies, env/config. METHOD: smoke loop — run -> first wall (missing sceXxx / UnknownInstruction / unhandled syscall) -> triage via FASE-0 diagnostics -> stub/implement -> rerun. This task is the MILESTONE umbrella; granular gaps are filed as pull-driven sub-tasks as the loop surfaces them (do NOT pre-file speculative sub-tasks). x86jit gaps -> x86jit backlog (user lands, bump pin), never edit x86jit directly. Boundary: game files local + gitignored, never committed.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 the native runtime initializes without a fatal wall (GC + threads + unwind tables up)
- [ ] #2 managed assemblies are loaded and AOT managed code begins executing on x86jit (managed entry reached)
- [ ] #3 each distinct blocker hit along the way is filed as its own follow-up task
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
SMOKE LOOP started 2026-07-14 (Celeste eboot CUSA11302). Method: run -> wall -> fix -> rerun. Progress this session (uncommitted at write, then committed):

FIXES LANDED:
1. loader: map PT_SCE_RELRO (0x61000010) as a loadable segment (image.rs extract_segments). It holds relocated data (SceKernelProcParam sub-structs, sceLibcParam) that a PT_LOAD does NOT cover; without it those pointers dangled into an unmapped gap and libc raised sceKernelDebugRaiseException. RELRO is mapped RW, relocated, then protected RO by the existing linker flow.
2. sceKernelGetProcParam: image.rs extracts PT_SCE_PROCPARAM vaddr; process.rs base-shifts it (eboot_base + vaddr = 0x4588000) and publishes via new ps4_core::kernel::set_proc_param_addr; handler in libkernel/mod.rs returns it. libc reads sceLibcParam from it.
3. libc-init HLE no-op stubs (libkernel): _sceKernelSetThreadDtors, _sceKernelSetThreadAtexit{Count,Report}, _sceKernelRtldThreadAtexit{Increment,Decrement}, _sceKernelRtldSetApplicationHeapAPI, sceKernelGetSanitizer{Malloc,New}Replace{,External}. All rtld/libc init hooks we don't need.
4. __stack_chk_guard (NID f7uOxY9mM1U): a DATA import (R_X86_64_64 into libc RELRO slot 0x203e18). Unresolved -> slot 0 -> null deref of the canary. Added an HLE DATA-export mechanism (hle.rs): a guest-resident qword in a new HLE_Data page, address exported under the NID in libkernel. Made ps4_libs::libs pub for LIB_KERNEL.

RESULT: libSceFios2 + libfmod module_start now return 0. libc module_start runs deep into main-thread init.

NEXT WALL (main-thread TCB / pthread self): libc init function @ libc vaddr 0x5df60 reads global @ vaddr 0x405a58 (the main-thread pthread/TCB pointer), computes rbx = 0x378 + *global, then derefs *rbx and faults 'orl 0x100,0x20(%rax)' with rax=0 (rip 0x989fdf). The TCB global is uninitialized -> everything derived is null. This is the main-thread pthread/TCB bring-up: PS4 kernel sets up the main thread's TCB (reachable via fs_base) with libc-expected fields before module_start; our fs_base points at the TLS alloc but the pthread/TCB struct there is not populated. Needs: recover the pthread/TCB field layout **by reverse-engineering our own dumped libc** (disassemble the offsets libc reads/writes — do NOT copy another emulator's source; unemups4 is original-work + GPL-3) and initialize the main-thread TCB accordingly. Distinct deeper sub-problem -> next increment. Also queued: pthread_mutex_lock POSIX alias (7H0iTOciTLo) that libfmodstudio imports (we have scePthreadMutexLock; need the pthread_* alias).

PRECISE ROOT-CAUSE (disasm): libc module_start (vaddr 0) is an init_array runner: it loops calling the entries in [0x404020, *0x203e10) and a second table at 0x404000, then tail-calls its rdx arg. It does NOT set rdi/rsi for the init calls, so each init fn sees module_start's OWN arg0 (rdi) unchanged. The init fn @ 0x5df60 does `if (*g == 0) *g = rdi;` for the global g @ vaddr 0x405a58, then rbx = *g + 0x378 and dereferences into pthread internals. So **libc expects module_start's arg0 (rdi) to be the main-thread pthread pointer** and caches it as the process-wide 'main thread'. We call every module_start with rdi=0 (thread.rs), so g=0 and all pthread-derived state is null. FIX DIRECTION: build a minimal main-thread pthread/TCB in guest memory and pass its address as rdi to the module_start chain (or install it so libc's self-lookup finds it). Recover the required fields (+0x20, +0x378 already implicated, more will surface) purely by disassembling our dumped libc. This is the keystone of FASE-2 thread bring-up.

PTHREAD/MUTEX WALL PASSED (2026-07-14, batch 2). Two fixes together advanced libc through its entire main-thread mutex init:
1. main-thread pthread arg: process.rs allocates a zeroed guest pthread region (map(0,0x4000)) for retail (multi-module) loads and stores it in Process.main_thread_pthread; thread.rs passes it as rdi to every dependency module_start (was 0). libc caches it as the process main-thread pointer.
2. HLE mutex object model: a PS4 ScePthreadMutex is an opaque POINTER (the handle slot holds a pointer to a mutex object; libc stores it and pokes the object's fields directly, e.g. `orl 0x100,0x20(*mutex)`). Our old HLE keyed a host mutex by the slot ADDRESS and left the slot 0 -> libc deref of 0. Fix: ps4_core::kernel gained a guest-resident HLE object bump-arena (set up in hle.rs, 1 MiB); scePthreadMutexInit now hle_alloc's a 0x40 object and writes its address into the slot (when 0). Host locking still keyed by the slot address (unchanged). Confirmed the pthread NIDs (scePthreadMutex{attrInit,attrSettype,Init,Lock}) resolve to our HLE, and libc does NOT export its own pthread.
Plus stub: sceLibcHeapGetTraceInfo -> 0 (heap trace query).

NEXT WALL: libc null write `movq $0x0,(%rsi)` at libc vaddr 0x5e22c (rip 0x98a22c), rsi = *(libc global @ 0x405ab0) = 0. That global has NO relocation (verified) -> it is runtime-initialized by an earlier libc init step we haven't satisfied (not a data import). Investigate what writes 0x405ab0 (likely a libc-internal structure alloc during heap/env/reent setup). Also still queued: pthread_mutex_lock POSIX alias (NID 7H0iTOciTLo) that libfmodstudio imports — add the pthread_* alias to our scePthreadMutexLock.

BATCH 3 (2026-07-14):
- sceLibcHeapGetTraceInfo: the 0x405ab0 wall WAS this stub returning 0 without filling the output descriptor. libc reads out+0x10 (a pointer), stores it to a global and dereferences it (`movq $0,(*)`), so it must be non-null. Handler now points out+0x10 at a small HLE-arena buffer (best-effort; libc uses it as an untraced heap descriptor). Advanced past the null write. libc does NOT export it (genuine import → our HLE resolves it).
- POSIX pthread alias NIDs: general fix in hle.rs — for every alias name in a handler's `names`, also export that name's OWN NID (via from_symbol_name(name).nid()), not just the SyscallId's canonical NID. So `pthread_mutex_lock` (NID 7H0iTOciTLo) now resolves to the scePthreadMutexLock handler. Fixes the whole POSIX pthread_* alias family at once. libc's pthread_mutex_lock now works.

NEW WALL (int 0x44 abort): libc calls a dedicated panic function at libc vaddr 0x56e20 (`push rbp; mov rsp,rbp; int $0x44; mov $0xcccccccc,edx; mov $0xa002000b,ecx`) — x86jit surfaces the `int $0x44` as Exception vector 68 / SIGILL at 0x982e26 (rip after the int). This is libc DELIBERATELY aborting: some upstream call returned an error and libc trapped (error code 0xa002000b = SCE_KERNEL/libc error). Need a GUEST BACKTRACE (walk rbp chain) to find the caller — this is exactly task-113.2 FASE-0 diagnostics (why-did-it-stop reporter). Possible causes to check: our best-effort sceLibcHeapGetTraceInfo fill being semantically wrong, or another stub returning an unexpected value. NEXT: build the guest-backtrace diagnostic (task-113.2), then identify the aborting call.

BATCH 4 (2026-07-14): guest backtrace (task-113.2) diagnosed the int-0x44 abort -> libc `__cxa_guard_release` (C++ static-local init guard). Its panic strings ("__cxa_guard_release failed to acquire/release/broadcast mutex") show it calls pthread mutex lock/unlock + cond broadcast and aborts if any returns non-zero. Root cause: cond_signal/cond_broadcast (kernel/sync.rs) returned EINVAL(22) for an unknown cond, but the guard's cond is STATICALLY initialized (SCE_PTHREAD_COND_INITIALIZER) and never explicitly cond-init'd -> not in our map -> 22 -> panic. Fix: signal/broadcast on an unknown cond is a no-op success (no registered waiters), Ok(0). (mutex_lock already lazy-inits; mutex_unlock still returns 22 for unknown but lock creates it first.) Advanced past __cxa_guard.

NEW WALL: missing HLE scePthreadAttrGet (NID x1X76arYMxU) — libc queries its own thread attributes. Next: implement/stub the scePthreadAttr* family libc needs during thread init.

BATCH 5 — MILESTONE: ALL FIVE dependency module_start returned 0; main thread JUMPED to the eboot entry (0x1988280). The entire native module init chain is up. Stubs added to clear the tail of module_start init:
- scePthreadAttr* family (Get + Getaffinity/Getdetachstate/Getstack + Set{affinity,detachstate,inheritsched,schedparam,schedpolicy,stacksize}). Getters write sane defaults (joinable, 7-core affinity mask).
- libSceSysmodule (LoadModule/UnloadModule/IsLoaded/LoadModuleInternalWithArg -> 0; GetModuleInfoForUnwind -> -1 so the C++ unwinder skips it).
- libSceAudiodec (InitLibrary/TermLibrary/Create/Delete/Decode) — scePlayStation4's module_start inits audio; real decode deferred to FASE-3.

NOW IN THE EBOOT CRT (managed-runtime bootstrap = task-113.3 AC#2 territory). First eboot wall: sceKernelMapNamedFlexibleMemory (NID mL8NDH86iQI) — the Mono GC / runtime allocating its heap via flexible physical memory. Implement it as a real mapping through the VmMemoryManager (like mmap), not a stub — the runtime reads/writes this memory heavily. Also seen queued earlier: sceKernelMmap, sceKernelMapFlexibleMemory (non-named), sceKernelVirtualQuery, sceSystemServiceParamGetInt, sceKernelGetdents/Stat.

BATCH 6 — EBOOT CRT / MONO RUNTIME RUNNING: implemented sceKernelMap{Named,}FlexibleMemory as REAL mappings (VmMemoryManager mmap, honour requested addr, write back result) — the Mono GC heap. Added live-thread scePthreadSetaffinity/Getaffinity/Setprio/Getprio/Rename stubs. After these the eboot ran ~120ms of guest code: Mono runtime bootstrapping, GPU/Vulkan context init (real!), creating threads + mutexes.

CURRENT WALL — HOST SIGSEGV (exit 139, core dump), NOT a clean guest fault. Last log before crash: scePthreadMutexInit(name="U\xfd\xee\x03") -> mutex @0x458eba0. The x86jit JIT backend passes an out-of-arena/bad access straight to the host (identity map) → host segfault instead of surfacing Exit::UnmappedMemory. DIAGNOSIS PLAN: (a) rerun under UNEMUPS4_BACKEND=interp (interpreter surfaces guest faults cleanly + the new rbp backtrace) — slower but actionable; (b) or run the jit build under gdb. Suspects: an HLE handler dereferencing a bad guest pointer (a *out arg the runtime passed as junk), or the Mono JIT/AOT hitting an instruction x86jit mishandles → corrupt state → wild access. New (host-level) debugging class vs the guest-fault walls so far.

BATCH 7 — runtime file I/O + POSIX pthread + pointer hygiene:
- FS: union-mount (FileSystem::translate now tries every same-prefix mount, resolving an EXISTING file from any and creating under the first); main.rs also mounts /app0 -> the loaded title's own dir. So the retail eboot's assemblies (Celeste.exe etc., in the dump dir) resolve via /app0 while examples keep using game_data/app0.
- sceKernelStat: FS.stat(path) -> (is_dir, size); handler fills SceKernelStat st_mode(@0x08 u16)+st_nlink(@0x0a)+st_size(@0x48 i64)+st_blocks(@0x50)+st_blksize(@0x58). Added KernelInterface::file_stat + bridge.
- POSIX pthread family: added the pthread_* alias names to scePthreadExit/CondInit/AttrInit/AttrSetschedparam/AttrSetstacksize; new stubs scePthreadKeyDelete/Set+Getschedparam/AttrGetschedpolicy/AttrGetstacksize (+ POSIX aliases). clock_getres too.
- POINTER HYGIENE (important, general): added ps4_libs::is_guest_ptr(ptr) = ptr in [GUEST_BASE, GUEST_BASE+DEFAULT_SPAN). Under a POSIX alias the guest leaves JUNK in the dropped arg register (name=0x44, 0x11, 0xffffffff...), and the JIT identity-maps guest ptrs straight through -> reading one segfaults the HOST (not a guest fault). Guard the optional name args in scePthreadMutexInit/CondInit/Create with it. General lesson: validate every optional/out guest pointer before deref. Found all via gdb (host SIGSEGV, no guest-fault log).

RESULT: runtime advances further; current wall = sem_init (NID pDuPEf3m4fI) — POSIX semaphores the runtime uses. Next: implement the sem_* / sceKernelSema family. NOTE stat still not observed being called (runtime hit pthread/sem walls first); assembly-open path not yet reached.

BATCH 8 — semaphores + SCE memory syscalls:
- New libkernel/sema.rs: a host counting-semaphore registry (Mutex<i64>+Condvar per sem). POSIX sem_init/wait/trywait/timedwait/post/destroy keyed by the guest sem_t address (lazy-create at 0 for static/never-init'd); SCE sceKernelCreateSema/WaitSema/SignalSema/DeleteSema keyed by an assigned id (high range, no collision with POSIX addresses). Blocking waits park the host thread; another guest thread's post wakes it.
- sceKernelMunmap (routes to kernel), sceKernelMprotect + Mtypeprotect (tracking no-op: the identity arena is pre-mapped RWX). sceKernelMmap deferred — its 7th out-pointer arg exceeds our 6-register syscall ABI; the runtime uses flexible memory + POSIX mmap so far.

RESULT: current wall = sceSystemServiceParamGetInt (NID fZo48un7LK4) — the runtime queries system params (language/region/etc.). Next: stub the sceSystemServiceParam* + sceAppContent* config getters (fill out-param with a sane default, return 0).

## Known follow-ups from the code review (finding -> task -> code site)
Deferred bugs/tech-debt surfaced by /code-review; each has a `KNOWN LIMITATION (task-NNN)` anchor at its code site so editing/greping that code surfaces it:
- task-115 — guest-pointer hygiene (systemic read_guest_cstr; ~11 unguarded handlers) + HLE object-arena free-list. Anchors: crates/core/src/kernel.rs hle_alloc; crates/libs/src/libkernel/pthread.rs scePthreadMutexInit slot deref; crates/libs/src/lib.rs is_guest_ptr.
- task-116 — FS union-mount flag-blind resolution (O_TRUNC can wipe a title asset). Anchor: crates/kernel/src/fs.rs translate().
- task-117 — module_start as an explicit process bootstrap phase (worker-thread ordering race; Fatal-continues-into-CRT). Anchor: crates/kernel/src/thread.rs module_start loop.
- task-118 — typed SceKernelStat struct + SyncManager owns guest sync objects. Anchors: crates/libs/src/libkernel/fs.rs sceKernelStat; crates/libs/src/libkernel/sema.rs module doc.
Also open: task-114 (flaky ps4-thread-testing teardown SIGSEGV, low).

BATCH 9 — MONO RUNTIME EXECUTING; blocked on an x86jit opcode:
- libSceSystemService config getters (ParamGetInt=English/cross, GetStatus idle, ReceiveEvent NO_EVENT, DisplaySafeAreaInfo ratio 1.0, HideSplashScreen, sceAppContentInitialize) — out-params guarded by is_guest_ptr.
- REAL thread stack bounds: ps4_core::kernel CURRENT_STACK thread-local (set in Thread::execute), scePthreadAttrGetstack returns the current thread's (base,size). This cleared Mono's `staddr` (mono-threads.c:386) and `mono_thread_info_is_live` (:428) assertions — the Mono GC asserts a non-null stack address. Mono printed its OWN assertion strings (mono-ps4-alt/.../mono-threads.c), i.e. the runtime is now executing.

HARD BLOCKER: `UnknownInstruction: vroundsd $0x9,%xmm1,%xmm0,%xmm1` (bytes c4 e3 79 0b c9 09) in Mono code. x86jit CPU-lift gap, filed as x86jit TASK-242 (VROUNDSD + ROUNDSS/PS/PD). Per policy, USER lands it in x86jit, bumps the rev pin here, rebuilds, then the smoke loop resumes. Expect more AVX opcodes + HLE walls before managed AOT entry (AC#2).

BATCH 10-14 — MANAGED RUNTIME EXECUTING (AC#2 reached):
Smoke loop drove the Mono runtime to load mscorlib.dll (valid CIL) AND Celeste.exe, entering mono_main / app-domain setup. Landings: file-backed mmap + real fstat (zero-stub fstat made Mono mmap 0 bytes -> "invalid CIL"); sceKernelMmap 7th stack-arg (NativeContext.rsp + syscall_stack_arg); FS union-mount + /app0->dump + /app0/mono/4.5 aliases + relative-path union; getpid/getcwd; libSceSystemService getters; REAL thread stack bounds (cleared Mono staddr/thread-info asserts); libSceNet init stubs (offline) + sched priority. x86jit AVX lift grind delegated to an opus agent (VROUND/unpack-pack/hadd/nt-moves/phadd on x86jit main, LOCAL pending push); rev pin bumped each time, examples+softgpu clean.

NEXT WALL: Mono app-domain null. eglib asserts `filename != NULL` then FATAL UnmappedMemory write 0x0 at eboot rip 0x1a49f2a; rax = *(BSS global @ eboot vaddr 0x2c075a8) = 0, in an argv-processing fn. Likely a missing initial process stack (no argc/argv/envp/auxv frame passed to the eboot entry) OR an unset Mono runtime global. See retail-bringup-epic memory for the full diagnosis + next steps.
