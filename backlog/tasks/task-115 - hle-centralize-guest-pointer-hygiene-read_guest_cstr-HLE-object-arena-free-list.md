---
id: TASK-115
title: >-
  hle: centralize guest-pointer hygiene (read_guest_cstr) + HLE object-arena
  free-list
status: To Do
assignee: []
created_date: '2026-07-14 20:18'
updated_date: '2026-07-16 10:03'
labels:
  - hle
  - bug
  - tech-debt
dependencies:
  - TASK-113.3
priority: medium
ordinal: 119000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Code-review altitude finding (task-113.3). is_guest_ptr is applied ad-hoc in a few handlers while ~11 others (sceKernelOpen path, condattr, thread names, mutex_ptr slot deref, etc.) still deref guest pointers unguarded, and sceKernelStat uses a raw '< 0x10000' literal (inconsistent). Under JIT identity-map a junk guest ptr = host SIGSEGV, not a guest fault, so this is whack-a-mole. Fix: a single read_guest_cstr(ptr)->Option<String> (range-check via GUEST_BASE/DEFAULT_SPAN + bounded NUL scan) used by ALL handlers; route the sceKernelStat check through is_guest_ptr; guard scePthreadMutexInit's *(mutex_ptr) slot deref. ALSO: the HLE object bump-arena (ps4_core::kernel hle_alloc, 1 MiB, ~16k objs, NO free) leaks and, when exhausted, returns 0 -> scePthreadMutexInit leaves the slot null -> guest null-deref under heavy mutex/cond churn. Add a free path (mutex_destroy -> hle_free) or a slab/free-list keyed to object lifecycle.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Design pass 2026-07-16 (Plan agent). Both seams ALREADY exist+registered: READ = BoundedRead::read_ranged (bounded_read.rs:31, validates [addr,addr+size) vs live VMA set, registered from process.memory @main.rs:209, reachable via bounded_read():120). WRITE must go through VirtualMemoryManager::write_bytes (vm_backend.rs:413→GuestVm::write_bytes guest_vm.rs:216→vm.write_bytes = x86jit SMC-observed). Ambient-unsafe default = IdentityMem::get_host_ptr Some for every non-null addr (idmem.rs:49) + MemoryAccessExt raw copy (memory.rs:205). is_guest_ptr (lib.rs:55) = BASE-ONLY, no base+size → fixed-size writes near arena top overrun (systemservice.rs:34, fs.rs:81, mman.rs:235, savedata:73). HLE arena bump-only no free (kernel.rs:257), exhaustion→0→null-slot (pthread mutex_init:335). Fixed handle=1: pad(13,98), mouse(27), equeue(events:29), AJM/NGS2 sentinels — none validate on use.

(1) NEW crates/core/src/guest_ptr.rs (ps4-core = where both seams live, dep of libs/gnm/cpu). GuestPtr<T:Copy>{addr}, GuestSlice<T>{addr,len}. Constructors ONLY way to get one, validate base+size_of*count vs arena bounds (checked-mul): GuestPtr::new(addr)->Option, GuestSlice::new(addr,count)->Option, null→None. READ via bounded seam never get_host_ptr: read()->Option<T> (read_ranged+unaligned reinterpret, Err/headless→None), read_vec, read_cstr(addr,max) chunked bounded-scan for NUL (replaces read_guest_cstr mod.rs:193 + the CStr::from_ptr Case-6 crash pthread.rs:397). WRITE via write seam never raw store: write(val), write_slice, zero (for the many write_bytes(p,0,N) sites). NEW crates/core/src/write_guest.rs: WriteGuest trait + register_write_guest/write_guest() global, blanket-impl over Arc<RwLock<Box<dyn VirtualMemoryManager>>> calling write_bytes, wired @main.rs:209 from SAME process.memory handle → every migrated write stays SMC-tracked for free. Defense-in-depth: constructor coarse base+size (no lock), read/write precise per-VMA.

(2) Close is_guest_ptr gap: promote GUEST_BASE/DEFAULT_SPAN from ps4-cpu (guest_vm.rs:67) into a ps4-core process-global arena-bounds AtomicU64 pair set at boot (mirror set_hle_arena kernel.rs:245) — removes libs→cpu reach. Reframe is_guest_ptr→is_guest_range(ptr,len); thin is_guest_ptr<T>=is_guest_range(p,size_of::<T>()). Route sceKernelStat raw >=0x10000 literal (mod.rs:421) through it.

(3) Migration order (crash-risk first), mechanical 1:1 (if is_guest_ptr{*p=v} → if let Some(gp)=GuestPtr::new(p){gp.write(v)}): T1 pthread.rs Case-6 (CStr::from_ptr:397, mutex slot:331, thread names). T2 fixed-size struct writes near arena top (systemservice:34, fs:81, mman:235, savedata:73, videoout:74, pad:35). T3 read_guest_cstr/is_guest_ptr consolidation. T4 GNM-driver writes on IdentityMem (events:20, workload:42, draw:46, shader_bind:53, nptrophy, ngs2, ajm) — these BYPASS SMC today so migrating = correctness upgrade. PREVENT NEW raw derefs: demote IdentityMem→pub(crate) in ps4-gnm (only exec/pm4-decode legitimately need it) = compiler blocks old pattern from libs; + clippy disallowed_methods (ptr::write_bytes, CStr::from_ptr, bare stores) in clippy.toml.

(4) ObjectArena/HandleTable in ps4-core: HandleKind enum (Pad/Mouse/Equeue/Ajm/Ngs2*/Mutex/Cond), handle=(kind_tag<<shift)|monotonic_id so wrong-kind/stale is DETECTABLE (generalizes NGS2 tagging). Per-kind slab+free-list: alloc(kind)->Handle, free(handle) validates kind+liveness, resolve(handle)->Option (rejects never-alloc/freed). Guest-resident mutex/cond: hle_alloc gains hle_free(addr,size) per-size free-list (kernel.rs:257); mutex_destroy/cond_destroy call it (fixes leak→exhaust→null-slot); if alloc still 0, mutex_init FAILS errno not null-slot. Adopters: pad/mouse/equeue/AJM/NGS2 return alloc(kind); *Close/*Destroy free; *Read/*Submit resolve→-EFAULT on bad handle.

(5) SMC preserved BY CONSTRUCTION (write seam→write_bytes→GuestVm). One legit IdentityMem stays: GNM Executor+PM4 decoder (guest thread, no mem-mgr handle, reads PM4 stream, exec.rs:1066/pm4:235) — keep pub(crate) in ps4-gnm; EOP/EOS label store (exec:1182, now VMA-guarded by task-134-sibling) stays identity fast-path (hot submit path), documented exception.

(6) SEQUENCE (independently mergeable): PR-A arena+HandleTable+hle_free, migrate 6 handle=1 stubs (no dep on GuestPtr, lands FIRST). PR-B guest_ptr.rs+write_guest.rs+arena-bounds reg, additive migrates nothing, unit tests via RegionReader override. PR-C is_guest_range + kill stat magic literal. PR-D..N handler migration tiers (1 PR each, revertable). PR-final LOCKDOWN: IdentityMem pub(crate)+clippy disallowed_methods (compile-break proves migration complete, lands LAST).

(7) RISKS: read_ranged/write_ranged take process.memory RwLock + VMA lookup + Vec alloc — OK on low-freq syscall path (lib.rs:63), do NOT migrate executor/PM4/label-store (hot, stay identity); add stack path for size<=16 to skip Vec alloc on scalar out-params. Demote-to-pub(crate) must not break exec/decode/gcn (all ps4-gnm). ~40 sites/15 files — final lockdown is the didnt-miss-any net. Poisoned lock→Err→-EFAULT never panic. Headless (no seam)→fail clean never raw-deref fallback. Full design in agent transcript.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
PR-A+B + PR-C + tier-1 merged @main (816d8bf). PR-C: is_guest_range(ptr,len) base+size check vs arena_bounds global; is_guest_ptr<T> thin wrapper (callers transparent, drops ps4-cpu reach); routed the >=0x10000 magic literal in sys_clock_getres (NOT sceKernelStat — matched by exact line 421/literal) through it. Tier-1: 5 pthread Case-6 sites migrated to GuestPtr/read_cstr (scePthreadCreate/MutexInit/CondInit/SetName name reads + MutexInit slot deref -> validate-then-rw, bad ptr = EINVAL/ENOMEM not host segfault); removed unused CStr import. 27 core + 23 libs pass, NO hang. NEXT tiers: tier-2 fixed-size struct writes near arena top (systemservice/fs/mman/savedata/videoout/pad), tier-3 read_guest_cstr consolidation, tier-4 GNM-driver IdentityMem writes (gnm — conflicts with exec.rs work, sequence carefully), PR-final LOCKDOWN (IdentityMem pub(crate) + clippy disallowed_methods, lands LAST).
<!-- SECTION:NOTES:END -->
