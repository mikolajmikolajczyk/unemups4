// glue between guest kernel calls and the emulator. exposed as a single
// global instance; syscalls dispatch through the methods below.

use std::cell::Cell;
use std::sync::Arc;

use crate::pad::PadState;
use crate::registered::Registered;

/// services the kernel provides to libraries (thread creation, sync, files, mmap, ...).
pub trait KernelInterface: Send + Sync {
    // tls
    fn tls_key_create(&self, destructor: u64) -> Result<u32, u64>;
    fn tls_set_specific(&self, key: u32, value: u64) -> Result<(), u64>;
    fn tls_get_specific(&self, key: u32) -> Result<u64, u64>;

    // mutexes
    fn mutex_init(&self, mutex_ptr: u64, recursive: bool) -> Result<i32, u64>;
    fn mutex_destroy(&self, mutex_ptr: u64) -> Result<i32, u64>;
    fn mutex_lock(&self, mutex_ptr: u64) -> Result<i32, u64>;
    fn mutex_unlock(&self, mutex_ptr: u64) -> Result<i32, u64>;
    fn mutex_timedlock(&self, mutex_ptr: u64, abstime_ptr: u64) -> Result<i32, u64>;
    fn mutex_trylock(&self, mutex_ptr: u64) -> Result<i32, u64>;

    fn cond_init(&self, cond_ptr: u64) -> Result<i32, u64>;
    fn cond_destroy(&self, cond_ptr: u64) -> Result<i32, u64>;
    fn cond_wait(&self, cond_ptr: u64, mutex_ptr: u64) -> Result<i32, u64>;
    fn cond_signal(&self, cond_ptr: u64) -> Result<i32, u64>;
    fn cond_broadcast(&self, cond_ptr: u64) -> Result<i32, u64>;
    fn cond_timedwait(&self, cond_ptr: u64, mutex_ptr: u64, micros: u32) -> Result<i32, u64>;

    // threads
    fn create_thread(&self, entry: u64, arg: u64) -> Result<u32, i64>;
    fn join_thread(&self, tid: u32) -> Result<u64, u64>;
    fn thread_detach(&self, tid: u32) -> Result<i32, u64>;
    fn thread_yield(&self);
    fn thread_self(&self) -> u32;
    fn thread_equal(&self, t1: u32, t2: u32) -> i32;
    fn thread_set_name(&self, tid: u32, name: &str) -> Result<i32, u64>;
    fn thread_get_name(&self, tid: u32, out_buf: u64, len: usize) -> Result<i32, u64>;
    fn thread_cancel(&self, tid: u32) -> Result<i32, u64>;

    // rwlocks
    fn rwlock_init(&self, ptr: u64) -> Result<i32, u64>;
    fn rwlock_destroy(&self, ptr: u64) -> Result<i32, u64>;
    fn rwlock_rdlock(&self, ptr: u64) -> Result<i32, u64>;
    fn rwlock_tryrdlock(&self, ptr: u64) -> Result<i32, u64>;
    fn rwlock_wrlock(&self, ptr: u64) -> Result<i32, u64>;
    fn rwlock_trywrlock(&self, ptr: u64) -> Result<i32, u64>;
    fn rwlock_unlock(&self, ptr: u64) -> Result<i32, u64>;

    // files
    fn file_open(&self, path: &str, flags: i32, mode: i32) -> Result<i32, i32>;
    /// Stat a guest path, returning `(is_dir, size_bytes)`; `Err` is a PS4 errno.
    fn file_stat(&self, path: &str) -> Result<(bool, u64), i32>;
    /// Stat an open fd, returning `(is_dir, size_bytes)`; `Err` is a PS4 errno.
    fn file_fstat(&self, fd: i32) -> Result<(bool, u64), i32>;
    fn file_close(&self, fd: i32) -> Result<i32, i32>;
    fn file_read(&self, fd: i32, ptr: u64, len: usize) -> Result<usize, i32>;
    fn file_write(&self, fd: i32, ptr: u64, len: usize) -> Result<usize, i32>;
    fn file_lseek(&self, fd: i32, offset: i64, whence: i32) -> Result<u64, i32>;
    fn file_mkdir(&self, path: &str, mode: i32) -> Result<i32, i32>;
    fn file_rmdir(&self, path: &str) -> Result<i32, i32>;
    fn file_unlink(&self, path: &str) -> Result<i32, i32>;
    fn file_rename(&self, old_path: &str, new_path: &str) -> Result<i32, i32>;

    // memory
    fn mmap(
        &self,
        addr: u64,
        len: usize,
        prot: i32,
        flags: i32,
        fd: i32,
        offset: i64,
    ) -> Result<u64, i64>;

    fn munmap(&self, addr: u64, len: usize) -> Result<i32, i64>;

    // video out
    fn video_out_open(
        &self,
        user_id: i32,
        bus_type: i32,
        index: i32,
        param: u64,
    ) -> Result<i32, i32>;
    fn video_out_register_buffers(
        &self,
        handle: i32,
        start_index: i32,
        ptr: u64,
        count: i32,
        attr_ptr: u64,
    ) -> Result<i32, i32>;
    fn video_out_submit_flip(
        &self,
        handle: i32,
        index: i32,
        flip_mode: i32,
        arg: i64,
    ) -> Result<i32, i32>;

    // input
    fn pad_get_state(&self, handle: i32) -> PadState;
}

static KERNEL_INTERFACE: Registered<dyn KernelInterface> = Registered::new();

/// Register the process-global kernel interface. Called once at boot, before
/// guest threads start, so the write lock is uncontended and can't be poisoned;
/// a failed lock is silently ignored rather than logged.
pub fn register_kernel(kernel: Arc<dyn KernelInterface>) {
    KERNEL_INTERFACE.register(kernel);
}

pub fn get_kernel() -> Option<Arc<dyn KernelInterface>> {
    KERNEL_INTERFACE.get()
}

/// Absolute guest address of the `SceKernelProcParam` (0 = not set / no proc param,
/// e.g. single-module homebrew). Set once at load, read by `sceKernelGetProcParam`.
static PROC_PARAM_ADDR: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub fn set_proc_param_addr(addr: u64) {
    PROC_PARAM_ADDR.store(addr, std::sync::atomic::Ordering::Relaxed);
}

pub fn proc_param_addr() -> u64 {
    PROC_PARAM_ADDR.load(std::sync::atomic::Ordering::Relaxed)
}

/// Guest-resident bump arena for small HLE-owned objects (e.g. the opaque pthread
/// mutex/cond objects that a guest libc stores in its handle slot and then pokes).
/// Base+end set once at HLE install; `hle_alloc` bump-allocates 16-byte-aligned.
static HLE_ARENA_CURSOR: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static HLE_ARENA_END: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Register the HLE object arena `[base, base+size)`.
pub fn set_hle_arena(base: u64, size: u64) {
    HLE_ARENA_CURSOR.store(base, std::sync::atomic::Ordering::Relaxed);
    HLE_ARENA_END.store(base + size, std::sync::atomic::Ordering::Relaxed);
}

/// Bump-allocate `size` bytes (16-byte-aligned) of guest HLE-object memory. Returns
/// the guest address, or 0 if unset/exhausted. The region is identity-mapped, so the
/// caller may write it through a raw pointer.
///
/// KNOWN LIMITATION (task-115): bump-only, no free list — objects leak for the process
/// lifetime, and once the 1 MiB region is exhausted this returns 0, which a caller like
/// `scePthreadMutexInit` turns into a null slot -> guest null-deref under heavy churn.
pub fn hle_alloc(size: u64) -> u64 {
    use std::sync::atomic::Ordering;
    let size = (size + 15) & !15;
    let end = HLE_ARENA_END.load(Ordering::Relaxed);
    loop {
        let cur = HLE_ARENA_CURSOR.load(Ordering::Relaxed);
        if cur == 0 || cur + size > end {
            return 0;
        }
        if HLE_ARENA_CURSOR
            .compare_exchange(cur, cur + size, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            return cur;
        }
    }
}

thread_local! {
    static CURRENT_TID: Cell<u32> = const { Cell::new(0) };
    /// This guest thread's stack bounds `(base, size)` — base is the LOWEST address, the
    /// stack grows down from `base + size`. Set when the thread starts so pthread stack
    /// queries (which the Mono GC needs to scan the stack) return real values.
    static CURRENT_STACK: Cell<(u64, u64)> = const { Cell::new((0, 0)) };
}

pub fn set_current_tid(tid: u32) {
    CURRENT_TID.with(|c| c.set(tid));
}

pub fn current_tid() -> u32 {
    CURRENT_TID.with(|c| c.get())
}

/// Record the current guest thread's stack bounds `(base, size)`.
pub fn set_current_stack(base: u64, size: u64) {
    CURRENT_STACK.with(|c| c.set((base, size)));
}

/// This guest thread's stack `(base, size)`; `(0, 0)` if unset.
pub fn current_stack() -> (u64, u64) {
    CURRENT_STACK.with(|c| c.get())
}
