use crate::context::NativeContext;
use ps4_core::guest_ptr::{GuestPtr, read_cstr};
use ps4_core::kernel::get_kernel;
use ps4_macros::ps4_syscall;
use ps4_syscalls::SyscallId;
use tracing::{error, info};
// `std::ffi::CStr` is no longer imported: all name reads now go through
// `ps4_core::guest_ptr::read_cstr` (task-115), replacing the raw `CStr::from_ptr` derefs.

/// Upper bound on a scanned guest debug/thread name (task-115). The old reads did an unbounded
/// strlen / `CStr::from_ptr` on an untrusted pointer — the Case-6 crash. `read_cstr` scans at
/// most this many bytes through the VMA-bounded read seam and stops at the first NUL, so a
/// missing terminator can't walk off the end of a mapping.
const NAME_MAX: usize = 256;

#[ps4_syscall(id = SyscallId::SCE_PTHREAD_CREATE, lib = crate::libs::LIB_KERNEL, name = "scePthreadCreate")]
pub fn sce_pthread_create(
    thread_ptr: *mut u64,
    _attr: *const u8,
    entry: usize,
    arg: usize,
    name: *const u8,
) -> i64 {
    // task-115: replace the unbounded strlen on an untrusted pointer with a bounded, seam-read
    // scan; a junk/short name yields "unnamed" instead of walking off the mapping.
    let t_name = read_cstr(name as u64, NAME_MAX).unwrap_or_else(|| "unnamed".into());

    info!(
        "[SYSCALL] scePthreadCreate: entry={:#x}, arg={:#x}, name='{}'",
        entry, arg, t_name
    );

    if let Some(kernel) = get_kernel() {
        match kernel.create_thread(entry as u64, arg as u64) {
            Ok(tid) => {
                if !thread_ptr.is_null() {
                    unsafe {
                        *thread_ptr = tid as u64;
                    }
                }
                0 // SCE_OK
            }
            Err(code) => code,
        }
    } else {
        error!("FATAL: Kernel Interface not registered in Core!");
        0x80020001
    }
}

#[ps4_syscall(id = SyscallId::SYS_PTHREAD_CREATE, lib = crate::libs::LIB_KERNEL, names = ["pthread_create", "_pthread_create"])]
pub fn sys_pthread_create(thread_ptr: *mut u64, _attr: *const u8, entry: usize, arg: usize) -> i64 {
    info!("[SYSCALL] pthread_create(entry={:#x})", entry);

    if let Some(kernel) = get_kernel() {
        // Delegate to kernel, but with NULL name
        match kernel.create_thread(entry as u64, arg as u64) {
            Ok(tid) => {
                if !thread_ptr.is_null() {
                    unsafe {
                        *thread_ptr = tid as u64;
                    }
                }
                0
            }
            Err(code) => code,
        }
    } else {
        -1
    }
}

/// `pthread_create_name_np(thread, attr, start, arg, name)` — FreeBSD's non-portable
/// create-with-a-name, which is what a native title uses so its threads are identifiable.
///
/// Same shape as `scePthreadCreate`, but kept separate because the RETURN conventions differ:
/// the Sony call reports an `0x8002_00xx` code, this one a POSIX errno. Folding them would
/// make one of the two lie about failure, which is the mistake task-216 spent a day undoing on
/// the timed waits.
///
/// The name is recorded on the thread rather than dropped: it is the only label a stalled
/// thread carries in the diagnostics, and an emulator whose report says "thread 4" instead of
/// the title's own name for it makes every stall harder to read.
#[ps4_syscall(id = SyscallId::SYS_PTHREAD_CREATE_NAME_NP, lib = crate::libs::LIB_KERNEL, names = ["pthread_create_name_np"])]
pub fn sys_pthread_create_name_np(
    thread_ptr: *mut u64,
    _attr: *const u8,
    entry: usize,
    arg: usize,
    name: *const u8,
) -> i32 {
    let t_name = read_cstr(name as u64, NAME_MAX).unwrap_or_else(|| "unnamed".into());
    info!("[SYSCALL] pthread_create_name_np(entry={entry:#x}, name='{t_name}')");

    let Some(kernel) = get_kernel() else {
        return 11; // EAGAIN: no kernel to create on
    };
    match kernel.create_thread(entry as u64, arg as u64) {
        Ok(tid) => {
            let _ = kernel.thread_set_name(tid, &t_name);
            if let Some(gp) = GuestPtr::<u64>::new(thread_ptr as u64) {
                let _ = gp.write(tid as u64);
            }
            0
        }
        // POSIX reports failure as an errno, not as a Sony status word.
        Err(_) => 11, // EAGAIN
    }
}

#[ps4_syscall(id = SyscallId::SCE_PTHREAD_EXIT, lib = crate::libs::LIB_KERNEL, names = ["scePthreadExit", "pthread_exit"])]
pub fn sce_pthread_exit(value_ptr: *mut libc::c_void) -> i64 {
    // Request the current guest call unwind with this exit value. The run
    // loop returns GuestExit::ThreadExit after this syscall returns; the thread stores it
    // as its exit value.
    ps4_cpu::request_thread_exit(value_ptr as u64);
    0
}

/// libc registers a per-thread C++/`__cxa_thread_atexit` destructor runner here
/// during its `module_start`. We drive our own TLS-destructor path on thread exit
/// ([`crate::libkernel`] pthread key destructors), so accept and ignore the callback
/// and return 0 (success) — this unblocks libc module init.
#[ps4_syscall(id = SyscallId::SCE_KERNEL_SET_THREAD_DTORS, lib = crate::libs::LIB_KERNEL, name = "_sceKernelSetThreadDtors")]
pub fn sce_kernel_set_thread_dtors(_dtor: u64) -> i64 {
    0
}

// The rtld/libc thread-atexit registration family, all called from libc's
// `module_start`. libc hands the runtime a set of hooks (dtor runner, atexit
// counters, a report callback, and rtld refcount inc/dec) to drive C++
// `__cxa_thread_atexit` teardown. We manage thread lifecycle + TLS destructors
// ourselves, so each is a no-op returning 0 — enough to unblock libc init.
#[ps4_syscall(id = SyscallId::SCE_KERNEL_SET_THREAD_ATEXIT_COUNT, lib = crate::libs::LIB_KERNEL, name = "_sceKernelSetThreadAtexitCount")]
pub fn sce_kernel_set_thread_atexit_count(_count: u64) -> i64 {
    0
}

#[ps4_syscall(id = SyscallId::SCE_KERNEL_SET_THREAD_ATEXIT_REPORT, lib = crate::libs::LIB_KERNEL, name = "_sceKernelSetThreadAtexitReport")]
pub fn sce_kernel_set_thread_atexit_report(_report: u64) -> i64 {
    0
}

#[ps4_syscall(id = SyscallId::SCE_KERNEL_RTLD_THREAD_ATEXIT_INCREMENT, lib = crate::libs::LIB_KERNEL, name = "_sceKernelRtldThreadAtexitIncrement")]
pub fn sce_kernel_rtld_thread_atexit_increment(_arg: u64) -> i64 {
    0
}

#[ps4_syscall(id = SyscallId::SCE_KERNEL_RTLD_THREAD_ATEXIT_DECREMENT, lib = crate::libs::LIB_KERNEL, name = "_sceKernelRtldThreadAtexitDecrement")]
pub fn sce_kernel_rtld_thread_atexit_decrement(_arg: u64) -> i64 {
    0
}

#[ps4_syscall(id = SyscallId::SCE_PTHREAD_JOIN, lib = crate::libs::LIB_KERNEL, name = "scePthreadJoin")]
pub fn sce_pthread_join(thread_id: u64, value_ptr: *mut u64) -> u64 {
    info!("[SYSCALL] scePthreadJoin(tid={})", thread_id);

    if let Some(kernel) = ps4_core::kernel::get_kernel() {
        match kernel.join_thread(thread_id as u32) {
            Ok(exit_value) => {
                if !value_ptr.is_null() {
                    unsafe {
                        *value_ptr = exit_value;
                    }
                }
                0
            }
            Err(e) => e,
        }
    } else {
        0x80020001
    }
}

#[ps4_syscall(id = SyscallId::SYS_PTHREAD_JOIN, lib = crate::libs::LIB_KERNEL, names = ["pthread_join", "_pthread_join"])]
pub fn sys_pthread_join(thread_id: u64, value_ptr: *mut u64) -> i32 {
    info!("[SYSCALL] pthread_join(tid={})", thread_id);

    if let Some(kernel) = get_kernel() {
        match kernel.join_thread(thread_id as u32) {
            Ok(exit_value) => {
                if !value_ptr.is_null() {
                    unsafe {
                        *value_ptr = exit_value;
                    }
                }
                0
            }
            Err(e) => e as i32,
        }
    } else {
        -1
    }
}

#[ps4_syscall(id = SyscallId::SCE_PTHREAD_SETSPECIFIC, lib = crate::libs::LIB_KERNEL, names = ["scePthreadSetSpecific", "pthread_setspecific"])]
pub fn sce_pthread_setspecific(key: u32, value: u64) -> u64 {
    if let Some(k) = ps4_core::kernel::get_kernel() {
        match k.tls_set_specific(key, value) {
            Ok(()) => 0,
            Err(e) => e,
        }
    } else {
        0x80020001
    }
}

#[ps4_syscall(id = SyscallId::SCE_PTHREAD_GETSPECIFIC, lib = crate::libs::LIB_KERNEL, names = ["scePthreadGetSpecific", "pthread_getspecific"])]
pub fn sce_pthread_getspecific(key: u32) -> u64 {
    if let Some(k) = ps4_core::kernel::get_kernel() {
        // pthread_getspecific returns NULL (0) on a missing/never-set key
        k.tls_get_specific(key).unwrap_or_default()
    } else {
        0
    }
}

#[ps4_syscall(
    id = SyscallId::SCE_PTHREAD_KEY_CREATE,
    lib = crate::libs::LIB_KERNEL,
    names = ["scePthreadKeyCreate", "pthread_key_create"]
)]
pub fn sce_pthread_key_create(key_ptr: *mut u32, destructor: u64) -> u64 {
    info!(
        "[SYSCALL] scePthreadKeyCreate(key_ptr={:?}, dtor={:#x})",
        key_ptr, destructor
    );

    if key_ptr.is_null() {
        // Orbis typically uses EINVAL-style errors;
        return 0x80020016;
    }

    let Some(k) = ps4_core::kernel::get_kernel() else {
        return 0x80020001;
    };

    match k.tls_key_create(destructor) {
        Ok(key) => {
            unsafe {
                *key_ptr = key;
            }
            0
        }
        Err(e) => e,
    }
}

use std::sync::atomic::{AtomicU32, Ordering};

#[ps4_syscall(
    id = SyscallId::SCE_PTHREAD_ONCE,
    lib = crate::libs::LIB_KERNEL,
    names = ["scePthreadOnce", "pthread_once"]
)]
pub fn sce_pthread_once(once_ptr: *mut u32, init_routine: u64) -> u64 {
    if once_ptr.is_null() {
        return 0x80020016; // EINVAL
    }
    // task-115: range-check the once control before forming a reference to it. The other
    // handlers validate guest addresses via GuestPtr/is_guest_range; without it a non-null but
    // junk/unmapped once_ptr would segfault the host on the `load` below.
    if !crate::is_guest_range(once_ptr as u64, 4) {
        return 0x80020016; // EINVAL — the once control is not a valid guest address.
    }
    let atomic_ptr = unsafe { &*(once_ptr as *const AtomicU32) };
    if atomic_ptr.load(Ordering::Acquire) == 2 {
        return 0;
    }

    match atomic_ptr.compare_exchange(0, 1, Ordering::Acquire, Ordering::Acquire) {
        Ok(_) => {
            // Nested guest call from inside this syscall handler: run the init routine on a
            // fresh vcpu carved below the current guest stack. Full validation
            // is deferred; this keeps the path exercised without regressing compilation.
            ps4_cpu::call_guest(init_routine, 0);

            atomic_ptr.store(2, Ordering::Release);
        }
        Err(state) => {
            if state == 2 {
                return 0;
            }

            while atomic_ptr.load(Ordering::Acquire) != 2 {
                std::hint::spin_loop();
            }
        }
    }

    0
}

const PTHREAD_MUTEX_NORMAL: i32 = 0;

#[ps4_syscall(id = SyscallId::SCE_PTHREAD_MUTEXATTR_INIT, lib = crate::libs::LIB_KERNEL, names = ["scePthreadMutexattrInit", "pthread_mutexattr_init"])]
pub fn sce_pthread_mutexattr_init(attr_ptr: *mut i32) -> i32 {
    if attr_ptr.is_null() {
        return 22;
    } // EINVAL
    unsafe {
        *attr_ptr = PTHREAD_MUTEX_NORMAL;
    } // Default
    0
}

#[ps4_syscall(id = SyscallId::SCE_PTHREAD_MUTEXATTR_DESTROY, lib = crate::libs::LIB_KERNEL, names = ["scePthreadMutexattrDestroy", "pthread_mutexattr_destroy"])]
pub fn sce_pthread_mutexattr_destroy(_attr_ptr: *mut i32) -> i32 {
    0 // Nothing to clean up for a simple integer
}

/// `pthread_mutexattr_setprotocol(attr, protocol)` — PRIO_NONE / PRIO_INHERIT / PRIO_PROTECT.
///
/// Accepted and ignored. We schedule guest threads as ordinary host threads, so there is no
/// guest priority for a mutex to inherit or ceiling to raise; recording the protocol would
/// change nothing about how the lock behaves. Returning success is the honest answer for a
/// model without guest priorities — an error would make a title conclude the platform cannot
/// do priority inheritance, which is a different (and false) claim.
///
/// Both spellings share this handler because both really are the same call, unlike the
/// timed-wait pairs whose ABIs differ (task-216).
#[ps4_syscall(id = SyscallId::SCE_PTHREAD_MUTEXATTR_SETPROTOCOL, lib = crate::libs::LIB_KERNEL, names = ["scePthreadMutexattrSetprotocol", "pthread_mutexattr_setprotocol"])]
pub fn sce_pthread_mutexattr_setprotocol(_attr_ptr: *mut i32, _protocol: i32) -> i32 {
    0
}

#[ps4_syscall(id = SyscallId::SCE_PTHREAD_MUTEXATTR_SETTYPE, lib = crate::libs::LIB_KERNEL, names = ["scePthreadMutexattrSettype", "pthread_mutexattr_settype"])]
pub fn sce_pthread_mutexattr_settype(attr_ptr: *mut i32, type_val: i32) -> i32 {
    if attr_ptr.is_null() {
        return 22;
    }
    unsafe {
        *attr_ptr = type_val;
    }
    0
}

#[ps4_syscall(id = SyscallId::SCE_PTHREAD_MUTEXATTR_GETTYPE, lib = crate::libs::LIB_KERNEL, name = "scePthreadMutexattrGettype")]
pub fn sce_pthread_mutexattr_gettype(attr_ptr: *const i32, out_type: *mut i32) -> i32 {
    if attr_ptr.is_null() || out_type.is_null() {
        return 22;
    }
    // task-115: read the attr and write the out param through the range-validated GuestPtr
    // seam, not raw `*attr_ptr` / `*out_type` derefs — a junk/unmapped non-null pointer fails
    // clean (EINVAL) instead of segfaulting the host.
    let Some(mtype) = GuestPtr::<i32>::new(attr_ptr as u64).and_then(|p| p.read()) else {
        return 22;
    };
    let Some(out) = GuestPtr::<i32>::new(out_type as u64) else {
        return 22;
    };
    if out.write(mtype).is_err() {
        return 22;
    }
    0
}

#[ps4_syscall(id = SyscallId::SCE_PTHREAD_MUTEX_INIT, lib = crate::libs::LIB_KERNEL, names = ["scePthreadMutexInit", "pthread_mutex_init"])]
pub fn sce_pthread_mutex_init(mutex_ptr: u64, attr_ptr: *const i32, name: *const u8) -> i32 {
    if mutex_ptr == 0 {
        return 22;
    }

    // Optional debug name. The guest sometimes passes a small integer / junk here (the Mono
    // runtime passed 0x44), and a raw `CStr::from_ptr` on it segfaults the host (Case-6).
    // task-115: read it through the bounded seam scan — junk yields None and is skipped.
    if let Some(name_str) = read_cstr(name as u64, NAME_MAX) {
        info!("[SYSCALL] scePthreadMutexInit name={:?}", name_str);
    }

    // Read the mutex type from the attr (an int written by scePthreadMutexattrSettype).
    // The three POSIX/Orbis types differ ONLY in how a self-relock by the owning thread
    // is handled (see MutexType / mutex_lock). Collapsing them (as the old bool did) hands
    // a NORMAL mutex a spurious EDEADLK on self-relock, which is fatal to Mono's
    // mono_os_mutex (a NORMAL mutex — Case: "Resource deadlock avoided", doc-5).
    // Orbis matches FreeBSD's libthr: ERRORCHECK=1, RECURSIVE=2, NORMAL=3. A null attr
    // uses the Orbis default, which is ERRORCHECK-free NORMAL semantics for our purposes
    // (Mono relies on the default not returning EDEADLK).
    // task-115: read the attr's type through the range-validated GuestPtr seam, not a raw
    // `*attr_ptr`. A null attr, or a junk/unmapped non-null attr (POSIX alias, stale pointer),
    // both yield None and fall back to the Orbis default (NORMAL) instead of segfaulting the
    // host — matching the null-attr default the old code already used.
    let mtype = GuestPtr::<i32>::new(attr_ptr as u64)
        .and_then(|p| p.read())
        .map(|v| match v {
            1 => ps4_core::kernel::MutexType::ErrorCheck,
            2 => ps4_core::kernel::MutexType::Recursive,
            _ => ps4_core::kernel::MutexType::Normal,
        })
        .unwrap_or(ps4_core::kernel::MutexType::Normal);

    // A PS4 ScePthreadMutex is an opaque POINTER: the handle slot holds a pointer to a
    // mutex object. A guest libc stores that pointer here and then pokes the object's
    // fields directly, so the slot must be a non-null guest address. We host-side lock
    // keyed by the slot address (mutex_ptr, unchanged below); the object exists only so
    // libc's direct pokes land on valid guest memory. Allocate once (slot == 0).
    // task-115: validate the slot through GuestPtr before touching it. A junk handle (POSIX
    // alias) fails the constructor (out of arena) and returns EINVAL rather than segfaulting
    // the host. The HLE arena has a free-list (mutex_destroy frees the object) so heavy churn
    // recycles; if an alloc still fails (region truly full), FAIL with ENOMEM instead of
    // leaving a null slot for the guest to deref.
    let Some(slot) = GuestPtr::<u64>::new(mutex_ptr) else {
        return 22; // EINVAL — the mutex handle is not a valid guest address.
    };
    match slot.read() {
        Some(0) => {
            let obj = ps4_core::kernel::hle_alloc(0x40);
            if obj == 0 {
                // ENOMEM — HLE arena exhausted. Reporting the failure is safer than leaving a
                // null slot that libc's direct pokes would deref (null-deref under churn).
                return 12; // ENOMEM
            }
            if slot.write(obj).is_err() {
                return 22; // EINVAL — the slot became unwritable (unmapped page inside arena).
            }
        }
        Some(_) => {}      // slot already holds an object pointer — reuse it.
        None => return 22, // EINVAL — the slot read failed (unmapped page inside arena).
    }

    if let Some(k) = get_kernel() {
        match k.mutex_init(mutex_ptr, mtype) {
            Ok(res) => res,
            Err(_) => 0x80020001u32 as i32,
        }
    } else {
        0x80020001u32 as i32
    }
}

#[ps4_syscall(id = SyscallId::SCE_PTHREAD_MUTEX_DESTROY, lib = crate::libs::LIB_KERNEL, names = ["scePthreadMutexDestroy", "pthread_mutex_destroy"])]
pub fn sce_pthread_mutex_destroy(mutex_ptr: u64) -> i32 {
    // task-115: return the HLE-arena object `mutex_init` stashed in the slot to the free-list
    // so mutex churn recycles instead of leaking → exhausting the region. `hle_free` bounds-
    // checks the slot value against the arena, so a guest-owned or junk slot value is dropped
    // rather than recycled. Zero the slot so a later re-init re-allocates cleanly.
    // task-138: read the stashed object and zero the slot through the validated GuestPtr
    // seam (GuestPtr::new rejects a null / junk slot → clean no-op) instead of a raw deref.
    if let Some(slot) = GuestPtr::<u64>::new(mutex_ptr)
        && let Some(obj) = slot.read()
        && obj != 0
    {
        ps4_core::kernel::hle_free(obj, 0x40);
        let _ = slot.write(0);
    }
    if let Some(k) = get_kernel() {
        k.mutex_destroy(mutex_ptr).unwrap_or(22)
    } else {
        22
    }
}

#[ps4_syscall(id = SyscallId::SCE_PTHREAD_MUTEX_LOCK, lib = crate::libs::LIB_KERNEL, names = ["scePthreadMutexLock", "pthread_mutex_lock"])]
pub fn sce_pthread_mutex_lock(mutex_ptr: u64) -> i32 {
    if let Some(k) = get_kernel() {
        k.mutex_lock(mutex_ptr).unwrap_or(22)
    } else {
        22
    }
}

#[ps4_syscall(id = SyscallId::SCE_PTHREAD_MUTEX_UNLOCK, lib = crate::libs::LIB_KERNEL, names = ["scePthreadMutexUnlock", "pthread_mutex_unlock"])]
pub fn sce_pthread_mutex_unlock(mutex_ptr: u64) -> i32 {
    if let Some(k) = get_kernel() {
        k.mutex_unlock(mutex_ptr).unwrap_or(22)
    } else {
        22
    }
}

#[ps4_syscall(id = SyscallId::SCE_PTHREAD_CONDATTR_INIT, lib = crate::libs::LIB_KERNEL, name = "scePthreadCondattrInit")]
pub fn sce_pthread_condattr_init(_attr: *mut u32) -> i32 {
    0 // Stub is fine for now
}

#[ps4_syscall(id = SyscallId::SCE_PTHREAD_CONDATTR_DESTROY, lib = crate::libs::LIB_KERNEL, name = "scePthreadCondattrDestroy")]
pub fn sce_pthread_condattr_destroy(_attr: *mut u32) -> i32 {
    0
}

#[ps4_syscall(id = SyscallId::SCE_PTHREAD_COND_INIT, lib = crate::libs::LIB_KERNEL, names = ["scePthreadCondInit", "pthread_cond_init"])]
pub fn sce_pthread_cond_init(cond_ptr: u64, _attr: *const u32, name: *const u8) -> i32 {
    if cond_ptr == 0 {
        return 22;
    }

    // POSIX pthread_cond_init has no name arg, so the 3rd register is junk under that alias.
    // task-115: read it through the bounded seam scan (see scePthreadMutexInit) — junk yields
    // None and is skipped rather than crashing on a raw `CStr::from_ptr`.
    if let Some(name_str) = read_cstr(name as u64, NAME_MAX) {
        info!("[SYSCALL] scePthreadCondInit name={:?}", name_str);
    }

    if let Some(k) = get_kernel() {
        match k.cond_init(cond_ptr) {
            Ok(res) => res,
            Err(_) => 0x80020001u32 as i32,
        }
    } else {
        0x80020001u32 as i32
    }
}

#[ps4_syscall(id = SyscallId::SCE_PTHREAD_COND_DESTROY, lib = crate::libs::LIB_KERNEL, names = ["scePthreadCondDestroy", "pthread_cond_destroy"])]
pub fn sce_pthread_cond_destroy(cond_ptr: u64) -> i32 {
    // task-115 symmetry with mutex_destroy: return any HLE-arena object the slot points at to
    // the free-list. `scePthreadCondInit` does not currently stash an HLE object in the slot
    // (unlike mutex_init), so the slot value is guest-owned — `hle_free`'s arena bounds-check
    // drops it, making this a safe no-op today while keeping destroy symmetric if cond_init
    // ever allocates one.
    // task-138: read the slot through the validated GuestPtr seam (rejects null / junk →
    // clean no-op) instead of a raw deref. No write-back here (see doc comment above).
    if let Some(slot) = GuestPtr::<u64>::new(cond_ptr)
        && let Some(obj) = slot.read()
        && obj != 0
    {
        ps4_core::kernel::hle_free(obj, 0x40);
    }
    if let Some(k) = get_kernel() {
        k.cond_destroy(cond_ptr).unwrap_or(22)
    } else {
        22
    }
}

#[ps4_syscall(id = SyscallId::SCE_PTHREAD_COND_WAIT, lib = crate::libs::LIB_KERNEL, names = ["scePthreadCondWait", "pthread_cond_wait"])]
pub fn sce_pthread_cond_wait(cond_ptr: u64, mutex_ptr: u64) -> i32 {
    if let Some(k) = get_kernel() {
        k.cond_wait(cond_ptr, mutex_ptr).unwrap_or(22)
    } else {
        22
    }
}

#[ps4_syscall(id = SyscallId::SCE_PTHREAD_COND_SIGNAL, lib = crate::libs::LIB_KERNEL, names = ["scePthreadCondSignal", "pthread_cond_signal"])]
pub fn sce_pthread_cond_signal(cond_ptr: u64) -> i32 {
    if let Some(k) = get_kernel() {
        k.cond_signal(cond_ptr).unwrap_or(22)
    } else {
        22
    }
}

#[ps4_syscall(id = SyscallId::SCE_PTHREAD_COND_BROADCAST, lib = crate::libs::LIB_KERNEL, names = ["scePthreadCondBroadcast", "pthread_cond_broadcast"])]
pub fn sce_pthread_cond_broadcast(cond_ptr: u64) -> i32 {
    if let Some(k) = get_kernel() {
        k.cond_broadcast(cond_ptr).unwrap_or(22)
    } else {
        22
    }
}

/// Convert a guest POSIX `abstime` pointer into the RELATIVE microseconds the kernel seam
/// carries, or return the errno to hand back (task-216).
///
/// `abstime` is absolute against the clock the GUEST reads, and `clock_gettime` hands the
/// guest [`virtual_epoch_ns`] — the virtual clock, whose rate is set by `UNEMUPS4_CLOCK`
/// (decision-8). Subtracting host wall time here instead would make every POSIX timeout
/// wrong by the current speed ratio, which is the subtler half of the task-214 bug.
///
/// A deadline already in the past saturates to zero, which is the POSIX behaviour: an
/// immediate `ETIMEDOUT`. The result is clamped to `u32::MAX` micros (~71 minutes) because
/// that is the width the seam carries; no real timeout comes near it.
///
/// [`virtual_epoch_ns`]: super::virtual_epoch_ns
pub(crate) fn abstime_to_relative_micros(abstime_ptr: u64) -> Result<u32, i32> {
    // A null `abstime` is EINVAL, not "wait forever"; a pointer outside the guest arena is
    // EFAULT. Both beat dereferencing whatever the guest handed us.
    if abstime_ptr == 0 {
        return Err(22); // EINVAL
    }

    // Read the two 64-bit fields of `Timespec` through the bounded per-VMA seam, NOT a raw
    // `&*(_ as *const Timespec)` deref. `is_guest_range` only checks the arena span, but the
    // arena can contain unmapped holes — exactly what `GuestPtr::read`'s per-VMA bounded read
    // guards against — and a range-valid pointer need not be 8-byte aligned, so forming a
    // `&Timespec` over it would be misaligned-reference UB. `GuestPtr::read` routes through
    // `read_ranged` (per-VMA bounded) + `read_unaligned`, so an in-arena hole, a misaligned
    // base, or an unwired seam yields EFAULT instead of faulting the host (doc-2 §1 identity
    // mapping; task-216). `tv_nsec` sits at offset 8 in the `{ i64; i64 }` layout.
    let Some(sec) = GuestPtr::<i64>::new(abstime_ptr).and_then(GuestPtr::read) else {
        return Err(14); // EFAULT
    };
    let Some(nsec) = GuestPtr::<i64>::new(abstime_ptr + 8).and_then(GuestPtr::read) else {
        return Err(14); // EFAULT
    };
    if !(0..1_000_000_000).contains(&nsec) || sec < 0 {
        return Err(22); // EINVAL: POSIX requires a normalized timespec
    }
    let deadline_ns = (sec as u64)
        .saturating_mul(1_000_000_000)
        .saturating_add(nsec as u64);

    let rel_us = deadline_ns.saturating_sub(super::virtual_epoch_ns()) / 1000;
    Ok(rel_us.min(u32::MAX as u64) as u32)
}

/// Sony's `scePthreadMutexTimedlock(mutex, usec)`: a RELATIVE [`SceKernelUseconds`], per
/// `data/oo_sdk/include/orbis/libkernel.h:629`.
///
/// This used to take an `abstime` POINTER and hand it to a kernel routine that dereferenced
/// it as a timespec — the exact mirror of the task-214 defect, and wrong in the more
/// dangerous direction: a guest passing a small integer count of microseconds had it read as
/// an address, which lands unmapped (EFAULT) or, worse, resolves to garbage and produces a
/// deadline out of unrelated memory. The SDK header is the oracle here, and it is the same
/// header that settled `scePthreadCondTimedwait`.
#[ps4_syscall(id = SyscallId::SCE_PTHREAD_MUTEX_TIMEDLOCK, lib = crate::libs::LIB_KERNEL, name = "scePthreadMutexTimedlock")]
pub fn sce_pthread_mutex_timedlock(mutex_ptr: u64, micros: u32) -> i32 {
    if let Some(k) = get_kernel() {
        k.mutex_timedlock(mutex_ptr, micros).unwrap_or(22)
    } else {
        22
    }
}

/// POSIX `pthread_mutex_timedlock(mutex, const struct timespec *abstime)`: the second
/// argument is a POINTER to an ABSOLUTE deadline (task-216).
///
/// Kept separate from the Sony spelling above rather than sharing one handler under two
/// names — that sharing is what produced the task-214 bug and, in mirror image, the one
/// fixed here. The generated table carries distinct ids for both, so there is no reason to
/// merge them.
#[ps4_syscall(id = SyscallId::SYS_PTHREAD_MUTEX_TIMEDLOCK, lib = crate::libs::LIB_KERNEL, name = "pthread_mutex_timedlock")]
pub fn sys_pthread_mutex_timedlock(mutex_ptr: u64, abstime_ptr: u64) -> i32 {
    let Some(k) = get_kernel() else { return 22 };
    match abstime_to_relative_micros(abstime_ptr) {
        Ok(micros) => k.mutex_timedlock(mutex_ptr, micros).unwrap_or(22),
        Err(errno) => errno,
    }
}

/// Sony's `scePthreadCondTimedwait(cond, mutex, usec)`: the timeout is a RELATIVE
/// [`SceKernelUseconds`], so it goes straight to the kernel.
///
/// Deliberately NOT sharing a handler with `pthread_cond_timedwait` below: the two APIs
/// disagree about the third argument, and serving both from one signature is what made
/// Celeste's frame thread sleep 2.18 s at a time (task-214).
#[ps4_syscall(id = SyscallId::SCE_PTHREAD_COND_TIMEDWAIT, lib = crate::libs::LIB_KERNEL, name = "scePthreadCondTimedwait")]
pub fn sce_pthread_cond_timedwait(cond_ptr: u64, mutex_ptr: u64, micros: u32) -> i32 {
    if let Some(k) = get_kernel() {
        k.cond_timedwait(cond_ptr, mutex_ptr, micros).unwrap_or(22)
    } else {
        22
    }
}

/// POSIX `pthread_cond_timedwait(cond, mutex, const struct timespec *abstime)`: the third
/// argument is a POINTER to an ABSOLUTE deadline, not a duration (task-214).
///
/// This used to share the Sony handler above, which treated the argument as relative
/// microseconds — so the pointer was truncated to `u32` and slept as a duration. Celeste
/// imports only this POSIX name (never the Sony one), so every one of its timed condition
/// waits blocked for a fixed, meaningless 2176848 us (`0x00213750`, the low half of a
/// guest stack address) instead of its real timeout.
///
/// Deadline handling lives in [`abstime_to_relative_micros`], shared with
/// `pthread_mutex_timedlock`.
#[ps4_syscall(id = SyscallId::SYS_PTHREAD_COND_TIMEDWAIT, lib = crate::libs::LIB_KERNEL, name = "pthread_cond_timedwait")]
pub fn sys_pthread_cond_timedwait(cond_ptr: u64, mutex_ptr: u64, abstime_ptr: u64) -> i32 {
    let Some(k) = get_kernel() else { return 22 };
    match abstime_to_relative_micros(abstime_ptr) {
        Ok(micros) => k.cond_timedwait(cond_ptr, mutex_ptr, micros).unwrap_or(22),
        Err(errno) => errno,
    }
}

#[ps4_syscall(id = SyscallId::SCE_PTHREAD_DETACH, lib = crate::libs::LIB_KERNEL, names = ["scePthreadDetach", "pthread_detach"])]
pub fn sce_pthread_detach(tid: u64) -> i32 {
    if let Some(k) = get_kernel() {
        match k.thread_detach(tid as u32) {
            Ok(res) => res,
            Err(e) => e as i32,
        }
    } else {
        0x80020001u32 as i32
    }
}

#[ps4_syscall(id = SyscallId::SCE_PTHREAD_YIELD, lib = crate::libs::LIB_KERNEL, names = ["scePthreadYield", "pthread_yield", "sched_yield"])]
pub fn sce_pthread_yield() -> i32 {
    if let Some(k) = get_kernel() {
        k.thread_yield();
        0
    } else {
        0
    }
}

#[ps4_syscall(id = SyscallId::SCE_PTHREAD_SELF, lib = crate::libs::LIB_KERNEL, names = ["scePthreadSelf", "pthread_self"])]
pub fn sce_pthread_self() -> u64 {
    if let Some(k) = get_kernel() {
        k.thread_self() as u64
    } else {
        0
    }
}

/// `scePthreadGetthreadid()` — the calling thread's OS thread id, as an `int`.
///
/// Distinct from [`sce_pthread_self`] on real hardware: `scePthreadSelf` returns the opaque
/// `ScePthread` handle, this returns the kernel's numeric tid, and a title uses the latter for
/// logging, thread naming and its own affinity bookkeeping. Our thread ids ARE small integers
/// handed out by the kernel bridge, so the two coincide here — but they are kept as separate
/// handlers, because a title that starts treating one as the other would do so on the strength
/// of an accident of this implementation.
///
/// `pthread_getthreadid_np` is the POSIX-flavoured spelling of the same call and shares it.
#[ps4_syscall(id = SyscallId::SCE_PTHREAD_GETTHREADID, lib = crate::libs::LIB_KERNEL, names = ["scePthreadGetthreadid", "pthread_getthreadid_np"])]
pub fn sce_pthread_getthreadid() -> i32 {
    ps4_core::kernel::current_tid() as i32
}

#[ps4_syscall(id = SyscallId::SCE_PTHREAD_EQUAL, lib = crate::libs::LIB_KERNEL, names = ["scePthreadEqual", "pthread_equal"])]
pub fn sce_pthread_equal(t1: u64, t2: u64) -> i32 {
    if let Some(k) = get_kernel() {
        k.thread_equal(t1 as u32, t2 as u32)
    } else {
        0
    }
}

#[ps4_syscall(id = SyscallId::SCE_PTHREAD_SET_NAME, lib = crate::libs::LIB_KERNEL, name = "scePthreadSetName")]
pub fn sce_pthread_setname_np(tid: u64, name_ptr: *const i8) -> i32 {
    if name_ptr.is_null() {
        return 22;
    } // EINVAL
    // task-115: read the name through the bounded seam scan instead of a raw `CStr::from_ptr`
    // on an untrusted pointer (Case-6). A non-arena / unterminated name fails clean with EINVAL.
    let Some(name_str) = read_cstr(name_ptr as u64, NAME_MAX) else {
        return 22; // EINVAL — the name pointer is not a valid guest C-string.
    };

    if let Some(k) = get_kernel() {
        match k.thread_set_name(tid as u32, &name_str) {
            Ok(res) => res,
            Err(e) => e as i32,
        }
    } else {
        0
    }
}

#[ps4_syscall(id = SyscallId::SCE_PTHREAD_GETNAME, lib = crate::libs::LIB_KERNEL, name = "scePthreadGetname")]
pub fn sce_pthread_getname_np(tid: u64, buf_ptr: u64, len: usize) -> i32 {
    if buf_ptr == 0 {
        return 22;
    }
    if let Some(k) = get_kernel() {
        match k.thread_get_name(tid as u32, buf_ptr, len) {
            Ok(res) => res,
            Err(e) => e as i32,
        }
    } else {
        0
    }
}

#[ps4_syscall(id = SyscallId::SCE_PTHREAD_CANCEL, lib = crate::libs::LIB_KERNEL, name = "scePthreadCancel")]
pub fn sce_pthread_cancel(tid: u64) -> i32 {
    // Stub or attempt cancel
    if let Some(k) = get_kernel() {
        match k.thread_cancel(tid as u32) {
            Ok(res) => res,
            Err(e) => e as i32,
        }
    } else {
        0
    }
}

#[ps4_syscall(id = SyscallId::SCE_PTHREAD_MUTEX_TRYLOCK, lib = crate::libs::LIB_KERNEL, names = ["scePthreadMutexTrylock", "pthread_mutex_trylock"])]
pub fn sce_pthread_mutex_trylock(mutex_ptr: u64) -> i32 {
    if let Some(k) = get_kernel() {
        k.mutex_trylock(mutex_ptr).unwrap_or(22)
    } else {
        22
    }
}

#[ps4_syscall(id = SyscallId::SCE_PTHREAD_RWLOCK_INIT, lib = crate::libs::LIB_KERNEL, name = "scePthreadRwlockInit")]
pub fn sce_pthread_rwlock_init(ptr: u64, _attr: *const u32) -> i32 {
    // Treat as Normal Mutex (not recursive)
    if let Some(k) = get_kernel() {
        match k.mutex_init(ptr, ps4_core::kernel::MutexType::Normal) {
            Ok(r) => r,
            Err(_) => 0x80020001u32 as i32,
        }
    } else {
        0x80020001u32 as i32
    }
}

#[ps4_syscall(id = SyscallId::SCE_PTHREAD_RWLOCK_DESTROY, lib = crate::libs::LIB_KERNEL, name = "scePthreadRwlockDestroy")]
pub fn sce_pthread_rwlock_destroy(ptr: u64) -> i32 {
    if let Some(k) = get_kernel() {
        k.mutex_destroy(ptr).unwrap_or(22)
    } else {
        22
    }
}

#[ps4_syscall(id = SyscallId::SCE_PTHREAD_RWLOCK_RDLOCK, lib = crate::libs::LIB_KERNEL, names = ["scePthreadRwlockRdlock", "pthread_rwlock_rdlock"])]
pub fn sce_pthread_rwlock_rdlock(ptr: u64) -> i32 {
    if let Some(k) = get_kernel() {
        k.mutex_lock(ptr).unwrap_or(22)
    } else {
        22
    }
}

#[ps4_syscall(id = SyscallId::SCE_PTHREAD_RWLOCK_WRLOCK, lib = crate::libs::LIB_KERNEL, names = ["scePthreadRwlockWrlock", "pthread_rwlock_wrlock"])]
pub fn sce_pthread_rwlock_wrlock(ptr: u64) -> i32 {
    if let Some(k) = get_kernel() {
        k.mutex_lock(ptr).unwrap_or(22)
    } else {
        22
    }
}

#[ps4_syscall(id = SyscallId::SCE_PTHREAD_RWLOCK_UNLOCK, lib = crate::libs::LIB_KERNEL, names = ["scePthreadRwlockUnlock", "pthread_rwlock_unlock"])]
pub fn sce_pthread_rwlock_unlock(ptr: u64) -> i32 {
    if let Some(k) = get_kernel() {
        k.mutex_unlock(ptr).unwrap_or(22)
    } else {
        22
    }
}

// attribute stubs

#[ps4_syscall(id = SyscallId::SCE_PTHREAD_ATTR_INIT, lib = crate::libs::LIB_KERNEL, names = ["scePthreadAttrInit", "pthread_attr_init"])]
pub fn sce_pthread_attr_init(_ptr: *mut u64) -> i32 {
    0
}

// The POSIX spelling shares this: destroying an attr object is the same no-op under both
// ABIs, and both report success as 0 — unlike the create/timed-wait pairs, where the two
// conventions genuinely differ and had to stay apart.
#[ps4_syscall(id = SyscallId::SCE_PTHREAD_ATTR_DESTROY, lib = crate::libs::LIB_KERNEL, names = ["scePthreadAttrDestroy", "pthread_attr_destroy"])]
pub fn sce_pthread_attr_destroy(_ptr: *mut u64) -> i32 {
    0
}

// libc reads its own (main) thread's attributes during init. We don't model a full
// attr object; return success and leave the caller's attr as-is (its scePthreadAttrInit
// already zero/default-initialized it). Extend to fill real fields if libc validates them.
#[ps4_syscall(id = SyscallId::SCE_PTHREAD_ATTR_GET, lib = crate::libs::LIB_KERNEL, name = "scePthreadAttrGet")]
pub fn sce_pthread_attr_get(_thread: u64, _attr: *mut u64) -> i32 {
    0
}

// pthread attribute setters libc/threads call during init: we don't model a full attr
// object (thread creation ignores most attrs today), so accept and return success.
#[ps4_syscall(id = SyscallId::SCE_PTHREAD_ATTR_SETAFFINITY, lib = crate::libs::LIB_KERNEL, name = "scePthreadAttrSetaffinity")]
pub fn sce_pthread_attr_setaffinity(_attr: *mut u64, _mask: u64) -> i32 {
    0
}

#[ps4_syscall(id = SyscallId::SCE_PTHREAD_ATTR_SETDETACHSTATE, lib = crate::libs::LIB_KERNEL, name = "scePthreadAttrSetdetachstate")]
pub fn sce_pthread_attr_setdetachstate(_attr: *mut u64, _state: i32) -> i32 {
    0
}

#[ps4_syscall(id = SyscallId::SCE_PTHREAD_ATTR_SETINHERITSCHED, lib = crate::libs::LIB_KERNEL, name = "scePthreadAttrSetinheritsched")]
pub fn sce_pthread_attr_setinheritsched(_attr: *mut u64, _inherit: i32) -> i32 {
    0
}

#[ps4_syscall(id = SyscallId::SCE_PTHREAD_ATTR_SETSCHEDPARAM, lib = crate::libs::LIB_KERNEL, names = ["scePthreadAttrSetschedparam", "pthread_attr_setschedparam"])]
pub fn sce_pthread_attr_setschedparam(_attr: *mut u64, _param: *const u64) -> i32 {
    0
}

#[ps4_syscall(id = SyscallId::SCE_PTHREAD_ATTR_SETSCHEDPOLICY, lib = crate::libs::LIB_KERNEL, name = "scePthreadAttrSetschedpolicy")]
pub fn sce_pthread_attr_setschedpolicy(_attr: *mut u64, _policy: i32) -> i32 {
    0
}

#[ps4_syscall(id = SyscallId::SCE_PTHREAD_ATTR_SETSTACKSIZE, lib = crate::libs::LIB_KERNEL, names = ["scePthreadAttrSetstacksize", "pthread_attr_setstacksize"])]
pub fn sce_pthread_attr_setstacksize(_attr: *mut u64, _size: usize) -> i32 {
    0
}

// pthread attribute getters. libc reads the outputs, so write sane defaults: joinable
// detach state, and an all-ones affinity mask (run on any core). Stack getter leaves the
// caller's buffers (extend if libc validates the main-thread stack bounds).
#[ps4_syscall(id = SyscallId::SCE_PTHREAD_ATTR_GETDETACHSTATE, lib = crate::libs::LIB_KERNEL, name = "scePthreadAttrGetdetachstate")]
pub fn sce_pthread_attr_getdetachstate(_attr: *const u64, state: *mut i32) -> i32 {
    // task-115: write the out-param through the validated GuestPtr seam; a junk pointer
    // fails the constructor (out of arena) and is a clean no-op instead of a host segfault.
    if let Some(gp) = GuestPtr::<i32>::new(state as u64) {
        let _ = gp.write(0); // PTHREAD_CREATE_JOINABLE
    }
    0
}

#[ps4_syscall(id = SyscallId::SCE_PTHREAD_ATTR_GETAFFINITY, lib = crate::libs::LIB_KERNEL, name = "scePthreadAttrGetaffinity")]
pub fn sce_pthread_attr_getaffinity(_attr: *const u64, mask: *mut u64) -> i32 {
    // task-115: validated GuestPtr write; junk pointer = clean no-op.
    if let Some(gp) = GuestPtr::<u64>::new(mask as u64) {
        let _ = gp.write(0x7f); // 7 PS4 game cores available
    }
    0
}

// Mono's GC needs real stack bounds to scan the stack; it queries them via the current
// thread's attr and asserts the stack address is non-zero. We don't model per-attr stack
// storage, so report the CURRENT guest thread's real bounds (base = lowest address).
#[ps4_syscall(id = SyscallId::SCE_PTHREAD_ATTR_GETSTACK, lib = crate::libs::LIB_KERNEL, names = ["scePthreadAttrGetstack", "pthread_attr_getstack"])]
pub fn sce_pthread_attr_getstack(_attr: *const u64, addr: *mut u64, size: *mut usize) -> i32 {
    let (base, sz) = ps4_core::kernel::current_stack();
    // task-115: write both out-params through the validated GuestPtr seam; a junk pointer
    // fails the constructor (out of arena) and is a clean no-op instead of a host segfault.
    if let Some(gp) = GuestPtr::<u64>::new(addr as u64) {
        let _ = gp.write(base);
    }
    if let Some(gp) = GuestPtr::<usize>::new(size as u64) {
        let _ = gp.write(sz as usize);
    }
    0
}

// Live-thread scheduling/affinity/naming. The Mono runtime sets these on its worker
// threads. We don't model per-thread affinity or priority scheduling, so accept the
// sets and report benign defaults on the gets. Naming is cosmetic — accepted, ignored.
#[ps4_syscall(id = SyscallId::SCE_PTHREAD_SETAFFINITY, lib = crate::libs::LIB_KERNEL, name = "scePthreadSetaffinity")]
pub fn sce_pthread_setaffinity(_thread: u64, _mask: u64) -> i32 {
    0
}

#[ps4_syscall(id = SyscallId::SCE_PTHREAD_GETAFFINITY, lib = crate::libs::LIB_KERNEL, name = "scePthreadGetaffinity")]
pub fn sce_pthread_getaffinity(_thread: u64, mask: *mut u64) -> i32 {
    // task-115: validated GuestPtr write; junk pointer = clean no-op.
    if let Some(gp) = GuestPtr::<u64>::new(mask as u64) {
        let _ = gp.write(0x7f); // 7 PS4 game cores
    }
    0
}

#[ps4_syscall(id = SyscallId::SCE_PTHREAD_SETPRIO, lib = crate::libs::LIB_KERNEL, name = "scePthreadSetprio")]
pub fn sce_pthread_setprio(_thread: u64, _prio: i32) -> i32 {
    0
}

#[ps4_syscall(id = SyscallId::SCE_PTHREAD_GETPRIO, lib = crate::libs::LIB_KERNEL, name = "scePthreadGetprio")]
pub fn sce_pthread_getprio(_thread: u64, prio: *mut i32) -> i32 {
    // task-115: validated GuestPtr write; junk pointer = clean no-op.
    if let Some(gp) = GuestPtr::<i32>::new(prio as u64) {
        let _ = gp.write(700); // SCE_KERNEL_PRIO_FIFO_DEFAULT
    }
    0
}

#[ps4_syscall(id = SyscallId::SCE_PTHREAD_RENAME, lib = crate::libs::LIB_KERNEL, name = "scePthreadRename")]
pub fn sce_pthread_rename(_thread: u64, _name: *const u8) -> i32 {
    0
}

// POSIX scheduler priority range — the Mono thread pool queries it. Report a plausible
// range (matches the SCE FIFO/RR default band); we don't enforce priority scheduling.
#[ps4_syscall(id = SyscallId::SYS_SCHED_GET_PRIORITY_MIN, lib = crate::libs::LIB_KERNEL, names = ["sched_get_priority_min", "_sched_get_priority_min"])]
pub fn sys_sched_get_priority_min(_policy: i32) -> i32 {
    256
}

#[ps4_syscall(id = SyscallId::SYS_SCHED_GET_PRIORITY_MAX, lib = crate::libs::LIB_KERNEL, names = ["sched_get_priority_max", "_sched_get_priority_max"])]
pub fn sys_sched_get_priority_max(_policy: i32) -> i32 {
    767
}

// More thread/attr scheduling APIs the Mono runtime uses (POSIX + SCE names). We don't
// model priority scheduling; accept sets, report FIFO/default on gets. TLS key delete
// is a no-op (our key table never reclaims — acceptable for bring-up).
#[ps4_syscall(id = SyscallId::SCE_PTHREAD_KEY_DELETE, lib = crate::libs::LIB_KERNEL, names = ["scePthreadKeyDelete", "pthread_key_delete"])]
pub fn sce_pthread_key_delete(_key: u32) -> i32 {
    0
}

#[ps4_syscall(id = SyscallId::SCE_PTHREAD_SETSCHEDPARAM, lib = crate::libs::LIB_KERNEL, names = ["scePthreadSetschedparam", "pthread_setschedparam"])]
pub fn sce_pthread_setschedparam(_thread: u64, _policy: i32, _param: *const u64) -> i32 {
    0
}

#[ps4_syscall(id = SyscallId::SCE_PTHREAD_GETSCHEDPARAM, lib = crate::libs::LIB_KERNEL, names = ["scePthreadGetschedparam", "pthread_getschedparam"])]
pub fn sce_pthread_getschedparam(_thread: u64, policy: *mut i32, param: *mut i32) -> i32 {
    // task-115: validated GuestPtr writes; junk pointers = clean no-ops.
    if let Some(gp) = GuestPtr::<i32>::new(policy as u64) {
        let _ = gp.write(1); // SCHED_FIFO
    }
    if let Some(gp) = GuestPtr::<i32>::new(param as u64) {
        let _ = gp.write(700); // default priority
    }
    0
}

#[ps4_syscall(id = SyscallId::SCE_PTHREAD_ATTR_GETSCHEDPOLICY, lib = crate::libs::LIB_KERNEL, names = ["scePthreadAttrGetschedpolicy", "pthread_attr_getschedpolicy"])]
pub fn sce_pthread_attr_getschedpolicy(_attr: *const u64, policy: *mut i32) -> i32 {
    // task-115: validated GuestPtr write; junk pointer = clean no-op.
    if let Some(gp) = GuestPtr::<i32>::new(policy as u64) {
        let _ = gp.write(1); // SCHED_FIFO
    }
    0
}

#[ps4_syscall(id = SyscallId::SCE_PTHREAD_ATTR_GETSTACKSIZE, lib = crate::libs::LIB_KERNEL, names = ["scePthreadAttrGetstacksize", "pthread_attr_getstacksize"])]
pub fn sce_pthread_attr_getstacksize(_attr: *const u64, size: *mut usize) -> i32 {
    // task-115: validated GuestPtr write; junk pointer = clean no-op.
    if let Some(gp) = GuestPtr::<usize>::new(size as u64) {
        let _ = gp.write(0x200000); // 2 MiB default stack
    }
    0
}

#[ps4_syscall(id = SyscallId::SCE_PTHREAD_RWLOCKATTR_INIT, lib = crate::libs::LIB_KERNEL, name = "scePthreadRwlockattrInit")]
pub fn sce_pthread_rwlockattr_init(_ptr: *mut u64) -> i32 {
    0
}

#[ps4_syscall(id = SyscallId::SCE_PTHREAD_RWLOCKATTR_DESTROY, lib = crate::libs::LIB_KERNEL, name = "scePthreadRwlockattrDestroy")]
pub fn sce_pthread_rwlockattr_destroy(_ptr: *mut u64) -> i32 {
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialize on the crate-wide arena guard: `set_arena_bounds` is process-global with no
    /// scoped restore, so without this a parallel test in another module (notably
    /// `is_guest_range_fails_closed_with_no_arena`) would see our window and fail.
    fn lock() -> std::sync::MutexGuard<'static, ()> {
        crate::arena_test_lock()
    }

    /// task-216: a host-backed bounded-read source (guest ptr == host addr) that copies
    /// straight from host memory, matching the identity mapping (doc-2 §1). The abstime reader
    /// now routes through the bounded seam rather than a raw `&*Timespec` deref, so these tests
    /// wire this to feed the boxed guest `Timespec`. Read-only.
    struct HostMem;
    impl ps4_core::bounded_read::BoundedRead for HostMem {
        fn read_ranged(&self, addr: u64, size: usize) -> Result<Vec<u8>, &'static str> {
            if size == 0 {
                return Ok(Vec::new());
            }
            if addr == 0 {
                return Err("null");
            }
            let mut buf = vec![0u8; size];
            // SAFETY: identity-mapped test buffer (a boxed `Timespec`); the arena guard is held
            // and `GuestPtr::new` only lets in-arena addresses reach this seam.
            unsafe {
                std::ptr::copy_nonoverlapping(addr as *const u8, buf.as_mut_ptr(), size);
            }
            Ok(buf)
        }
    }

    /// Wire the host-backed read seam for a test body, returning the RAII restore guard the
    /// caller holds. The production reader pulls the timespec through this bounded seam, so a
    /// test that omitted it would read `None` (unwired) and see EFAULT for a valid deadline.
    fn wire_reads() -> impl Drop {
        use ps4_core::bounded_read::{BoundedRead, registered_source};
        registered_source()
            .override_scoped(std::sync::Arc::new(HostMem) as std::sync::Arc<dyn BoundedRead>)
    }

    /// Build a guest `Timespec` at a host address and register the arena around it, so
    /// [`abstime_to_relative_micros`] resolves the pointer the way it does under the
    /// identity mapping (doc-2 §1). Returns the boxed value so the caller keeps it alive —
    /// dropping it would leave the "arena" pointing at freed memory.
    fn guest_timespec(sec: i64, nsec: i64) -> Box<super::super::Timespec> {
        let ts = Box::new(super::super::Timespec {
            tv_sec: sec,
            tv_nsec: nsec,
        });
        let addr = &*ts as *const _ as u64;
        ps4_core::kernel::set_arena_bounds(addr, size_of::<super::super::Timespec>() as u64);
        ts
    }

    /// Drop the arena window again, so nothing outside this module inherits it.
    fn clear_arena() {
        ps4_core::kernel::set_arena_bounds(0, 0);
    }

    /// task-216 AC#5: an absolute POSIX deadline converts to a relative timeout measured
    /// against the VIRTUAL clock, which is the clock `clock_gettime` hands the guest to
    /// build that deadline with in the first place.
    ///
    /// The reference is pinned by construction: the deadline is built from
    /// [`virtual_epoch_ns`] here exactly as a guest builds it from `clock_gettime`, so a
    /// conversion that subtracted host wall time would only agree while the two clocks
    /// happen to run at the same rate. Under `UNEMUPS4_CLOCK=fixed-step` they do not, and
    /// that is the case this guards. Forcing the mode from here is not possible — it is
    /// process-global state owned by `ps4-core` and resolved once — so this test pins the
    /// arithmetic and the reference, not the two modes side by side.
    #[test]
    fn abstime_converts_against_the_virtual_clock() {
        let _t = lock();
        let _rd = wire_reads();
        let now_ns = super::super::virtual_epoch_ns();
        let deadline_ns = now_ns + 5 * 1_000_000_000;
        let ts = guest_timespec(
            (deadline_ns / 1_000_000_000) as i64,
            (deadline_ns % 1_000_000_000) as i64,
        );

        let micros = abstime_to_relative_micros(&*ts as *const _ as u64).expect("valid deadline");

        // Five seconds out, minus however long this test took to get here. A host-time
        // conversion in a fixed-step run would land nowhere near this.
        assert!(
            (4_900_000..=5_000_000).contains(&micros),
            "expected ~5 s of relative timeout, got {micros} us"
        );
        clear_arena();
    }

    /// A deadline already in the past is a ZERO timeout, not a huge one: POSIX says the
    /// call returns ETIMEDOUT immediately. Getting this wrong by letting the subtraction
    /// wrap is how a 5 ms timeout becomes a 584-year one.
    #[test]
    fn a_past_deadline_is_an_immediate_timeout() {
        let _t = lock();
        let _rd = wire_reads();
        let ts = guest_timespec(1, 0); // one second after the epoch — long gone
        let micros = abstime_to_relative_micros(&*ts as *const _ as u64).expect("valid deadline");
        assert_eq!(micros, 0);
        clear_arena();
    }

    /// The three rejections, each with the errno the guest expects. A null `abstime` is
    /// EINVAL rather than "wait forever", and a pointer outside the arena is EFAULT rather
    /// than a host segfault on whatever the guest handed us.
    #[test]
    fn malformed_deadlines_are_rejected_before_any_deref() {
        let _t = lock();
        let _rd = wire_reads();
        assert_eq!(
            abstime_to_relative_micros(0),
            Err(22),
            "null abstime → EINVAL"
        );

        // Register an arena that excludes the address we then pass.
        let _ts = guest_timespec(0, 0);
        assert_eq!(
            abstime_to_relative_micros(0x1000),
            Err(14),
            "pointer outside the guest arena → EFAULT"
        );

        // POSIX requires a normalized timespec: tv_nsec in [0, 1e9), tv_sec non-negative.
        let ts = guest_timespec(1, 2_000_000_000);
        assert_eq!(
            abstime_to_relative_micros(&*ts as *const _ as u64),
            Err(22),
            "denormalized tv_nsec → EINVAL"
        );

        let ts = guest_timespec(-1, 0);
        assert_eq!(
            abstime_to_relative_micros(&*ts as *const _ as u64),
            Err(22),
            "negative tv_sec → EINVAL"
        );
        clear_arena();
    }

    /// task-216 regression: a pointer that passes the arena SPAN check but lands in an in-arena
    /// UNMAPPED hole — the per-VMA bounded read rejects it — must return EFAULT, never a raw
    /// host deref. The old `&*(_ as *const Timespec)` path would have faulted the emulator
    /// process here; routing through `GuestPtr::read` maps the rejection to EFAULT.
    #[test]
    fn an_in_arena_unmapped_hole_is_efault_not_a_host_fault() {
        let _t = lock();

        // A read seam that always fails the bounded read, the way a per-VMA read rejects an
        // unmapped page inside the arena. The address it is handed is never dereferenced.
        struct HoleMem;
        impl ps4_core::bounded_read::BoundedRead for HoleMem {
            fn read_ranged(&self, _addr: u64, _size: usize) -> Result<Vec<u8>, &'static str> {
                Err("unmapped page inside arena")
            }
        }
        use ps4_core::bounded_read::{BoundedRead, registered_source};
        let _rd = registered_source()
            .override_scoped(std::sync::Arc::new(HoleMem) as std::sync::Arc<dyn BoundedRead>);

        // Register a wide arena so the pointer passes the span check; the bounded read still
        // fails, standing in for an in-arena hole. `0x1_0000` is never touched as host memory.
        let addr = 0x1_0000u64;
        ps4_core::kernel::set_arena_bounds(addr, 0x1000);
        assert_eq!(
            abstime_to_relative_micros(addr),
            Err(14),
            "in-arena unmapped hole → EFAULT, never a host deref"
        );
        clear_arena();
    }
}
