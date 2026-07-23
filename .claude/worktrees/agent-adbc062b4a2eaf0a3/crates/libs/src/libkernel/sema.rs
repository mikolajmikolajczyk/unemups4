//! Semaphores: POSIX `sem_*` (keyed by the guest `sem_t` address) and the SCE
//! `sceKernel*Sema` handle API (keyed by an assigned u32 id). The two namespaces use
//! SEPARATE registries: a POSIX `sem_t` is a guest address that can exceed 32 bits and
//! would otherwise collide with a small SCE handle id. Each semaphore is a host counting
//! semaphore (`Mutex<i64>` + `Condvar`). Blocking waits park the host thread running that
//! guest thread; another guest thread's post/signal wakes it (each guest thread is its
//! own host thread).
//!
//! KNOWN LIMITATION (task-118): this is a parallel process-global registry rather than
//! part of kernel/sync.rs `SyncManager` (which owns mutex/cond/rwlock). Fold semaphores
//! into SyncManager so all guest sync objects share one lifecycle/reset path.

use crate::context::NativeContext;
use ps4_macros::ps4_syscall;
use ps4_syscalls::SyscallId;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};

struct HostSem {
    count: Mutex<i64>,
    cond: Condvar,
}

impl HostSem {
    fn new(initial: i64) -> Arc<Self> {
        Arc::new(HostSem {
            count: Mutex::new(initial),
            cond: Condvar::new(),
        })
    }

    /// Wait until at least `need` permits are available, then take them.
    fn wait(&self, need: i64) {
        let mut count = self.count.lock().unwrap();
        while *count < need {
            count = self.cond.wait(count).unwrap();
        }
        *count -= need;
    }

    /// Try to take `need` permits without blocking; `true` on success.
    fn try_wait(&self, need: i64) -> bool {
        let mut count = self.count.lock().unwrap();
        if *count >= need {
            *count -= need;
            true
        } else {
            false
        }
    }

    fn signal(&self, n: i64) {
        let mut count = self.count.lock().unwrap();
        *count += n;
        self.cond.notify_all();
    }
}

/// POSIX semaphores, keyed by the guest `sem_t` address.
static POSIX_SEMS: OnceLock<Mutex<HashMap<u64, Arc<HostSem>>>> = OnceLock::new();
/// SCE handle semaphores, keyed by the assigned u32 id.
static SCE_SEMS: OnceLock<Mutex<HashMap<u32, Arc<HostSem>>>> = OnceLock::new();
/// SCE sema handle ids. Start at 1 so 0 is never a valid handle; a u32 so it round-trips
/// through the guest's `int` handle without truncation.
static NEXT_SEMA_ID: AtomicU32 = AtomicU32::new(1);

fn posix_map() -> &'static Mutex<HashMap<u64, Arc<HostSem>>> {
    POSIX_SEMS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn sce_map() -> &'static Mutex<HashMap<u32, Arc<HostSem>>> {
    SCE_SEMS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Atomically fetch (or lazily create at `initial`) the POSIX semaphore at `addr`. The
/// lookup and the insert happen under one lock, so two threads racing on a never-init'd
/// `sem_t` can't each install a different `HostSem` and lose each other's permits.
fn posix_get_or_create(addr: u64, initial: i64) -> Arc<HostSem> {
    posix_map()
        .lock()
        .unwrap()
        .entry(addr)
        .or_insert_with(|| HostSem::new(initial))
        .clone()
}

// ---------------------------------------------------------------------------
// POSIX sem_* — the `sem_t` guest address is the key.
// ---------------------------------------------------------------------------

#[ps4_syscall(id = SyscallId::SYS_SEM_INIT, lib = crate::libs::LIB_KERNEL, names = ["sem_init", "_sem_init"])]
pub fn sys_sem_init(sem: u64, _pshared: i32, value: u32) -> i32 {
    // sem_init defines the semaphore's value; overwrite any prior instance.
    posix_map()
        .lock()
        .unwrap()
        .insert(sem, HostSem::new(value as i64));
    0
}

#[ps4_syscall(id = SyscallId::SYS_SEM_WAIT, lib = crate::libs::LIB_KERNEL, names = ["sem_wait", "_sem_wait"])]
pub fn sys_sem_wait(sem: u64) -> i32 {
    posix_get_or_create(sem, 0).wait(1);
    0
}

#[ps4_syscall(id = SyscallId::SYS_SEM_TRYWAIT, lib = crate::libs::LIB_KERNEL, names = ["sem_trywait", "_sem_trywait"])]
pub fn sys_sem_trywait(sem: u64) -> i32 {
    if posix_get_or_create(sem, 0).try_wait(1) {
        0
    } else {
        -1 // EAGAIN
    }
}

#[ps4_syscall(id = SyscallId::SYS_SEM_TIMEDWAIT, lib = crate::libs::LIB_KERNEL, names = ["sem_timedwait", "_sem_timedwait"])]
pub fn sys_sem_timedwait(sem: u64, _abstime: u64) -> i32 {
    // Timeout not modelled yet — block like sem_wait (correct for a permit that
    // eventually arrives; a never-arriving permit hangs, same as the guest intends).
    posix_get_or_create(sem, 0).wait(1);
    0
}

#[ps4_syscall(id = SyscallId::SYS_SEM_POST, lib = crate::libs::LIB_KERNEL, names = ["sem_post", "_sem_post"])]
pub fn sys_sem_post(sem: u64) -> i32 {
    posix_get_or_create(sem, 0).signal(1);
    0
}

#[ps4_syscall(id = SyscallId::SYS_SEM_DESTROY, lib = crate::libs::LIB_KERNEL, names = ["sem_destroy", "_sem_destroy"])]
pub fn sys_sem_destroy(sem: u64) -> i32 {
    posix_map().lock().unwrap().remove(&sem);
    0
}

// ---------------------------------------------------------------------------
// SCE sceKernel*Sema — u32 handle (id) API.
// ---------------------------------------------------------------------------

#[ps4_syscall(id = SyscallId::SCE_KERNEL_CREATE_SEMA, lib = crate::libs::LIB_KERNEL, name = "sceKernelCreateSema")]
pub fn sce_kernel_create_sema(
    out_id: *mut u32,
    _name: *const u8,
    _attr: u32,
    init: i32,
    _max: i32,
    _opt: u64,
) -> i32 {
    if !crate::is_guest_ptr(out_id) {
        return 0x80020016u32 as i32; // EINVAL
    }
    let id = NEXT_SEMA_ID.fetch_add(1, Ordering::Relaxed);
    sce_map()
        .lock()
        .unwrap()
        .insert(id, HostSem::new(init.max(0) as i64));
    unsafe { *out_id = id };
    0
}

#[ps4_syscall(id = SyscallId::SCE_KERNEL_WAIT_SEMA, lib = crate::libs::LIB_KERNEL, name = "sceKernelWaitSema")]
pub fn sce_kernel_wait_sema(id: u32, need: i32, _timeout: u64) -> i32 {
    let sem = sce_map().lock().unwrap().get(&id).cloned();
    match sem {
        Some(s) => {
            s.wait(need.max(1) as i64);
            0
        }
        // Unknown handle: do NOT silently succeed (that would report a permit the guest
        // never acquired). SCE_KERNEL_ERROR_ESRCH.
        None => 0x80020003u32 as i32,
    }
}

#[ps4_syscall(id = SyscallId::SCE_KERNEL_SIGNAL_SEMA, lib = crate::libs::LIB_KERNEL, name = "sceKernelSignalSema")]
pub fn sce_kernel_signal_sema(id: u32, count: i32) -> i32 {
    let sem = sce_map().lock().unwrap().get(&id).cloned();
    match sem {
        Some(s) => {
            s.signal(count.max(1) as i64);
            0
        }
        None => 0x80020003u32 as i32, // ESRCH
    }
}

#[ps4_syscall(id = SyscallId::SCE_KERNEL_DELETE_SEMA, lib = crate::libs::LIB_KERNEL, name = "sceKernelDeleteSema")]
pub fn sce_kernel_delete_sema(id: u32) -> i32 {
    sce_map().lock().unwrap().remove(&id);
    0
}
