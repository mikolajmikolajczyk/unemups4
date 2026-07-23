use crate::context::NativeContext;
use ps4_core::kernel::get_kernel;
use ps4_macros::ps4_syscall;
use ps4_syscalls::SyscallId;
use std::ffi::CStr;
use tracing::{error, info};

#[ps4_syscall(id = SyscallId::SCE_PTHREAD_CREATE, lib = crate::libs::LIB_KERNEL, name = "scePthreadCreate")]
pub fn sce_pthread_create(
    thread_ptr: *mut u64,
    _attr: *const u8,
    entry: usize,
    arg: usize,
    name: *const u8,
) -> i64 {
    let t_name = if crate::is_guest_ptr(name) {
        unsafe {
            let mut l = 0;
            while *name.add(l) != 0 {
                l += 1;
            }
            String::from_utf8_lossy(std::slice::from_raw_parts(name, l))
        }
    } else {
        "unnamed".into()
    };

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

#[ps4_syscall(id = SyscallId::SCE_PTHREAD_EXIT, lib = crate::libs::LIB_KERNEL, names = ["scePthreadExit", "pthread_exit"])]
pub fn sce_pthread_exit(value_ptr: *mut libc::c_void) -> i64 {
    // Request the current guest call unwind with this exit value (doc-1 dec 3). The run
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
    let atomic_ptr = unsafe { &*(once_ptr as *const AtomicU32) };
    if atomic_ptr.load(Ordering::Acquire) == 2 {
        return 0;
    }

    match atomic_ptr.compare_exchange(0, 1, Ordering::Acquire, Ordering::Acquire) {
        Ok(_) => {
            // Nested guest call from inside this syscall handler: run the init routine on a
            // fresh vcpu carved below the current guest stack (doc-1 dec 3). Full validation
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
    unsafe {
        *out_type = *attr_ptr;
    }
    0
}

#[ps4_syscall(id = SyscallId::SCE_PTHREAD_MUTEX_INIT, lib = crate::libs::LIB_KERNEL, names = ["scePthreadMutexInit", "pthread_mutex_init"])]
pub fn sce_pthread_mutex_init(mutex_ptr: u64, attr_ptr: *const i32, name: *const u8) -> i32 {
    if mutex_ptr == 0 {
        return 22;
    }

    // Optional debug name. Guard against a non-pointer: the guest sometimes passes a
    // small integer / junk here (the Mono runtime passed 0x44), and dereferencing an
    // address below the guest arena base (0x10000) segfaults the host. Only read it when
    // it points inside the guest arena.
    if crate::is_guest_ptr(name) {
        let name_str = unsafe { std::ffi::CStr::from_ptr(name as *const i8) };
        info!("[SYSCALL] scePthreadMutexInit name={:?}", name_str);
    }

    // Check attributes for recursion
    let mut recursive = false;
    if !attr_ptr.is_null() {
        let type_val = unsafe { *attr_ptr };
        if type_val == 2 {
            // PTHREAD_MUTEX_RECURSIVE
            recursive = true;
        }
    }

    // A PS4 ScePthreadMutex is an opaque POINTER: the handle slot holds a pointer to a
    // mutex object. A guest libc stores that pointer here and then pokes the object's
    // fields directly, so the slot must be a non-null guest address. We host-side lock
    // keyed by the slot address (mutex_ptr, unchanged below); the object exists only so
    // libc's direct pokes land on valid guest memory. Allocate once (slot == 0).
    // KNOWN LIMITATION (task-115): mutex_ptr is deref'd here without an is_guest_ptr
    // range check; a junk handle (POSIX alias) would segfault the host. See also
    // hle_alloc exhaustion (task-115) leaving the slot null.
    unsafe {
        let slot = mutex_ptr as *mut u64;
        if *slot == 0 {
            let obj = ps4_core::kernel::hle_alloc(0x40);
            if obj != 0 {
                *slot = obj;
            }
        }
    }

    if let Some(k) = get_kernel() {
        match k.mutex_init(mutex_ptr, recursive) {
            Ok(res) => res,
            Err(_) => 0x80020001u32 as i32,
        }
    } else {
        0x80020001u32 as i32
    }
}

#[ps4_syscall(id = SyscallId::SCE_PTHREAD_MUTEX_DESTROY, lib = crate::libs::LIB_KERNEL, names = ["scePthreadMutexDestroy", "pthread_mutex_destroy"])]
pub fn sce_pthread_mutex_destroy(mutex_ptr: u64) -> i32 {
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

    // POSIX pthread_cond_init has no name arg, so the 3rd register is junk under that
    // alias; only read it when it points into the guest arena (see scePthreadMutexInit).
    if crate::is_guest_ptr(name) {
        let name_str = unsafe { std::ffi::CStr::from_ptr(name as *const i8) };
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

#[ps4_syscall(id = SyscallId::SCE_PTHREAD_MUTEX_TIMEDLOCK, lib = crate::libs::LIB_KERNEL, name = "scePthreadMutexTimedlock")]
pub fn sce_pthread_mutex_timedlock(mutex_ptr: u64, abstime_ptr: u64) -> i32 {
    if let Some(k) = get_kernel() {
        k.mutex_timedlock(mutex_ptr, abstime_ptr).unwrap_or(22)
    } else {
        22
    }
}

#[ps4_syscall(id = SyscallId::SCE_PTHREAD_COND_TIMEDWAIT, lib = crate::libs::LIB_KERNEL, names = ["scePthreadCondTimedwait", "pthread_cond_timedwait"])]
pub fn sce_pthread_cond_timedwait(cond_ptr: u64, mutex_ptr: u64, micros: u32) -> i32 {
    if let Some(k) = get_kernel() {
        k.cond_timedwait(cond_ptr, mutex_ptr, micros).unwrap_or(22)
    } else {
        22
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
    let name_str = unsafe { CStr::from_ptr(name_ptr).to_string_lossy() };

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
        match k.mutex_init(ptr, false) {
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

#[ps4_syscall(id = SyscallId::SCE_PTHREAD_ATTR_DESTROY, lib = crate::libs::LIB_KERNEL, name = "scePthreadAttrDestroy")]
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
    if !state.is_null() {
        unsafe { *state = 0 }; // PTHREAD_CREATE_JOINABLE
    }
    0
}

#[ps4_syscall(id = SyscallId::SCE_PTHREAD_ATTR_GETAFFINITY, lib = crate::libs::LIB_KERNEL, name = "scePthreadAttrGetaffinity")]
pub fn sce_pthread_attr_getaffinity(_attr: *const u64, mask: *mut u64) -> i32 {
    if !mask.is_null() {
        unsafe { *mask = 0x7f }; // 7 PS4 game cores available
    }
    0
}

// Mono's GC needs real stack bounds to scan the stack; it queries them via the current
// thread's attr and asserts the stack address is non-zero. We don't model per-attr stack
// storage, so report the CURRENT guest thread's real bounds (base = lowest address).
#[ps4_syscall(id = SyscallId::SCE_PTHREAD_ATTR_GETSTACK, lib = crate::libs::LIB_KERNEL, names = ["scePthreadAttrGetstack", "pthread_attr_getstack"])]
pub fn sce_pthread_attr_getstack(_attr: *const u64, addr: *mut u64, size: *mut usize) -> i32 {
    let (base, sz) = ps4_core::kernel::current_stack();
    unsafe {
        if crate::is_guest_ptr(addr) {
            *addr = base;
        }
        if crate::is_guest_ptr(size) {
            *size = sz as usize;
        }
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
    if !mask.is_null() {
        unsafe { *mask = 0x7f }; // 7 PS4 game cores
    }
    0
}

#[ps4_syscall(id = SyscallId::SCE_PTHREAD_SETPRIO, lib = crate::libs::LIB_KERNEL, name = "scePthreadSetprio")]
pub fn sce_pthread_setprio(_thread: u64, _prio: i32) -> i32 {
    0
}

#[ps4_syscall(id = SyscallId::SCE_PTHREAD_GETPRIO, lib = crate::libs::LIB_KERNEL, name = "scePthreadGetprio")]
pub fn sce_pthread_getprio(_thread: u64, prio: *mut i32) -> i32 {
    if !prio.is_null() {
        unsafe { *prio = 700 }; // SCE_KERNEL_PRIO_FIFO_DEFAULT
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
    unsafe {
        if !policy.is_null() {
            *policy = 1; // SCHED_FIFO
        }
        if !param.is_null() {
            *param = 700; // default priority
        }
    }
    0
}

#[ps4_syscall(id = SyscallId::SCE_PTHREAD_ATTR_GETSCHEDPOLICY, lib = crate::libs::LIB_KERNEL, names = ["scePthreadAttrGetschedpolicy", "pthread_attr_getschedpolicy"])]
pub fn sce_pthread_attr_getschedpolicy(_attr: *const u64, policy: *mut i32) -> i32 {
    if !policy.is_null() {
        unsafe { *policy = 1 }; // SCHED_FIFO
    }
    0
}

#[ps4_syscall(id = SyscallId::SCE_PTHREAD_ATTR_GETSTACKSIZE, lib = crate::libs::LIB_KERNEL, names = ["scePthreadAttrGetstacksize", "pthread_attr_getstacksize"])]
pub fn sce_pthread_attr_getstacksize(_attr: *const u64, size: *mut usize) -> i32 {
    if !size.is_null() {
        unsafe { *size = 0x200000 }; // 2 MiB default stack
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
