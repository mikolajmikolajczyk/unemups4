// glue between guest kernel calls and the emulator. exposed as a single
// global instance; syscalls dispatch through the methods below.

use std::cell::Cell;
use std::sync::{Arc, Mutex};

use crate::pad::PadState;
use crate::registered::Registered;

/// POSIX/Orbis mutex type. The three variants differ only in how a *self-relock* (the
/// owning thread locking again while it already holds the mutex) is handled:
///
/// - `Normal`: default. Self-relock is undefined in POSIX (a real deadlock). Crucially it
///   does NOT return `EDEADLK` — an app that self-relocks a NORMAL mutex expects to hang,
///   never a checked error. Mono's `mono_os_mutex` is NORMAL and treats an `EDEADLK` from
///   lock as fatal ("Resource deadlock avoided"). We therefore treat a NORMAL self-relock
///   as a benign recursive acquire (count up) rather than blocking forever or erroring —
///   the app's own logic never intends to deadlock here (doc-5).
/// - `ErrorCheck`: self-relock returns `EDEADLK`. The only type for which `EDEADLK` is a
///   defined, expected result.
/// - `Recursive`: self-relock succeeds and bumps a recursion count; matched by unlocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MutexType {
    Normal,
    ErrorCheck,
    Recursive,
}

/// services the kernel provides to libraries (thread creation, sync, files, mmap, ...).
pub trait KernelInterface: Send + Sync {
    // tls
    fn tls_key_create(&self, destructor: u64) -> Result<u32, u64>;
    fn tls_set_specific(&self, key: u32, value: u64) -> Result<(), u64>;
    fn tls_get_specific(&self, key: u32) -> Result<u64, u64>;

    // mutexes
    fn mutex_init(&self, mutex_ptr: u64, mtype: MutexType) -> Result<i32, u64>;
    fn mutex_destroy(&self, mutex_ptr: u64) -> Result<i32, u64>;
    fn mutex_lock(&self, mutex_ptr: u64) -> Result<i32, u64>;
    fn mutex_unlock(&self, mutex_ptr: u64) -> Result<i32, u64>;
    /// Lock `mutex_ptr`, giving up after `micros` RELATIVE microseconds (task-216). Both
    /// spellings converge here: Sony's `scePthreadMutexTimedlock` is declared with
    /// `OrbisKernelUseconds`, and POSIX's absolute `abstime` is converted against the
    /// virtual clock before it crosses this seam — as [`Self::cond_timedwait`] already did.
    fn mutex_timedlock(&self, mutex_ptr: u64, micros: u32) -> Result<i32, u64>;
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
    /// The debug name a thread carries, read HOST-side. Unlike [`Self::thread_get_name`],
    /// which answers the guest by writing into guest memory, this is for our own reports:
    /// a stalled title's profiler dump lists thirty threads parked in `scePthreadCondWait`,
    /// and "RHIThread" versus "TaskGraphThread 4" is the whole diagnosis. Defaults to `None`
    /// so a kernel that tracks no names still compiles.
    fn thread_name_of(&self, _tid: u32) -> Option<String> {
        None
    }
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
    /// Positional read into the guest buffer at `ptr`, leaving the fd's cursor alone.
    /// Default: unsupported, so a kernel without positional I/O says so instead of silently
    /// reading from the wrong place.
    fn file_pread(&self, _fd: i32, _ptr: u64, _len: usize, _offset: u64) -> Result<usize, i32> {
        Err(78) // ENOSYS
    }
    /// Positional write from the guest buffer at `ptr`; mirror of [`Self::file_pread`].
    fn file_pwrite(&self, _fd: i32, _ptr: u64, _len: usize, _offset: u64) -> Result<usize, i32> {
        Err(78) // ENOSYS
    }
    /// Pack directory entries from `fd` into the guest buffer at `ptr` (`len` bytes),
    /// FreeBSD `getdents(2)` semantics. Returns bytes written (0 == end of dir).
    fn file_getdents(&self, fd: i32, ptr: u64, len: usize) -> Result<usize, i32>;
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

    /// `mmap` that honours a guest-requested `align` for an "allocate anywhere" request
    /// (`addr == 0`). Direct/flexible-memory maps that carry an alignment (e.g. Mono SGen's
    /// 1 MB-aligned LOS sections) route here; the default ignores `align` and delegates to
    /// [`mmap`](Self::mmap).
    #[allow(clippy::too_many_arguments)]
    fn mmap_aligned(
        &self,
        addr: u64,
        len: usize,
        _align: usize,
        prot: i32,
        flags: i32,
        fd: i32,
        offset: i64,
    ) -> Result<u64, i64> {
        self.mmap(addr, len, prot, flags, fd, offset)
    }

    fn munmap(&self, addr: u64, len: usize) -> Result<i32, i64>;

    // direct memory (task-148) — PS4's physical-offset-based direct-memory API. The three
    // calls are modelled as: reserve a physical range (return its offset), map an offset to a
    // VA, release by *offset* (not VA). Defaults fall back to plain `mmap`/`munmap` so a
    // kernel without direct-memory tracking still functions; the real `Process` overrides them
    // to track the phys-offset ⇄ VA mapping so release frees the right span and an untracked
    // offset is a clean no-op (see [`KernelInterface::release_direct_memory`]).

    /// `sceKernelAllocateDirectMemory`: reserve `len` bytes of physical direct memory aligned
    /// to `align`, returning its **physical offset**. Default: a plain aligned anonymous
    /// mapping whose base doubles as the offset (identity map). `Err` is a PS4 errno.
    fn allocate_direct_memory(&self, len: usize, align: usize) -> Result<u64, i64> {
        // PROT_READ|WRITE=3, MAP_ANON|MAP_PRIVATE=0x1002.
        self.mmap_aligned(0, len, align, 3, 0x1002, -1, 0)
    }

    /// `sceKernelMapDirectMemory`: map the physical `phys_off` (from
    /// [`allocate_direct_memory`](Self::allocate_direct_memory)) to a virtual address,
    /// returning that VA. Default: the identity map already made `phys_off` a usable VA, so
    /// echo it back.
    fn map_direct_memory(&self, phys_off: u64, _len: usize) -> u64 {
        phys_off
    }

    /// `sceKernelAvailableDirectMemorySize`: the largest contiguous free physical run inside
    /// `[search_start, search_end)` that satisfies `align`, as `(offset, size)`.
    ///
    /// A native title asks this BEFORE reserving, to size its pools against what the console
    /// actually has left, so answering it wrongly is worse than not answering: report too
    /// much and the guest's own allocator commits to a budget we cannot honour. `None` means
    /// nothing in the window fits.
    ///
    /// Default: `None` — a kernel without direct-memory tracking cannot know, and saying so
    /// is the honest answer.
    fn available_direct_memory(
        &self,
        _search_start: u64,
        _search_end: u64,
        _align: u64,
    ) -> Option<(u64, u64)> {
        None
    }

    /// `sceKernelReleaseDirectMemory(phys_off, len)`: free a direct-memory region **by its
    /// physical offset**. An untracked / zero / already-freed offset is a clean no-op that
    /// returns success — it must *not* `munmap` the offset value as if it were a VA. Default:
    /// clean no-op success (a non-tracking kernel has nothing to free).
    fn release_direct_memory(&self, _phys_off: u64, _len: usize) -> i32 {
        0
    }

    /// ELF general-dynamic TLS: return the base guest address of the *calling thread's*
    /// per-thread TLS arena, lazily allocating a fresh zero-initialised block on first
    /// access. `__tls_get_addr` adds the requested `ti_offset` to this base. The arena is
    /// keyed by thread id alone (a single flat per-thread block; `ti_module` is collapsed),
    /// so two threads get distinct bases and repeat calls on one thread return the same
    /// base. `Err` is a PS4 errno-style code. Default: not supported (no TLS infra).
    fn tls_arena_base(&self) -> Result<u64, u64> {
        Err(0x16) // EINVAL
    }

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

    // dynamic module loading (sceKernelLoadStartModule / sceKernelDlsym)
    /// Load + link the `.prx` at a runtime guest path, registering its exports.
    /// Returns `(handle, module_starts)`: the positive module handle and the `module_start`
    /// addresses to run in order (newly-loaded deps leaves-first, then the target); the list
    /// is empty when the module was already loaded. `Err` is a PS4 errno.
    fn load_start_module(&self, guest_path: &str) -> Result<(i32, Vec<u64>), i32>;
    /// Resolve an exported symbol by name in a loaded module to its absolute guest
    /// address, or `None` if the module has no such export.
    fn module_dlsym(&self, handle: i32, name: &str) -> Option<u64>;

    // save data (sceSaveDataMount2 / Umount2 / DirNameSearch2)
    /// Mount the save slot named `dir_name` for `user_id`, registering a host directory
    /// under a guest mount point so later `sceKernelOpen("/savedataN/...")` resolves to
    /// it. `requested_blocks` is the request's block budget (used to report
    /// `requiredBlocks` back). Returns `(mount_point, mount_status, required_blocks)`:
    /// the guest mount-point path (e.g. `/savedata0`), a mount-status code (1 = created a
    /// fresh dir, 0 = re-mounted an existing one), and the block count to report.
    /// `Err` is a PS4 errno.
    fn savedata_mount(
        &self,
        user_id: u32,
        dir_name: &str,
        requested_blocks: u64,
        mount_mode: u32,
    ) -> Result<(String, u32, u64), i32>;
    /// Mount the title's temporary-data area, returning its guest mount point. `Err` is a
    /// PS4 errno. Default: unsupported — a kernel without a filesystem has nowhere to put it.
    fn tempdata_mount(&self) -> Result<String, i32> {
        Err(0x16) // EINVAL
    }
    /// Unmount the save slot currently mounted at `mount_point`, unregistering it. `Err`
    /// is a PS4 errno (ENODEV when nothing is mounted there).
    fn savedata_umount(&self, mount_point: &str) -> Result<(), i32>;
    /// Number of existing save directories under the host save root (what a
    /// `sceSaveDataDirNameSearch` reports as the found-directory count).
    fn savedata_dir_count(&self) -> u32;

    // memory-region query (sceKernelVirtualQuery)
    /// Look up the tracked VMA that answers a `sceKernelVirtualQuery(addr, flags, ...)`.
    /// With `find_next` false, return the region containing `addr`; with it true (the
    /// `SCE_KERNEL_VQ_FIND_NEXT` bit), return the nearest region starting at/above `addr`
    /// (so the Mono GC can walk the map). `None` when nothing matches.
    fn virtual_query(&self, addr: u64, find_next: bool) -> Option<VqRegion>;
}

/// A memory region as reported by `sceKernelVirtualQuery` — the fields the guest's
/// `SceKernelVirtualQueryInfo` carries, sourced from the memory manager's VMA set.
#[derive(Clone, Debug)]
pub struct VqRegion {
    pub start: u64,
    pub end: u64,
    /// PS4 protection bits (R=1, W=2, X=4) for the region.
    pub protection: i32,
    /// PS4 memory-type code (0 = wb-onion default; we don't distinguish types).
    pub memory_type: i32,
    /// True for a flexible-memory / anonymous mapping (vs. a file/direct one).
    pub is_flexible: bool,
    /// Region name (truncated into the guest's fixed name buffer).
    pub name: String,
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

/// Fixed virtual-address base of the direct-memory pool (task-148). PS4 direct memory is a
/// *physical-offset* coordinate space: `sceKernelAllocateDirectMemory` hands out dense
/// offsets from 0, and Mono (Celeste) builds its own physical→virtual table on top and both
/// releases by offset and `munmap`s the VAs it derives. To keep Mono's phys↔VA math in
/// lockstep with ours, our pool is one contiguous VA window and the mapping is a pure
/// function: `va = DIRECT_MEMORY_POOL_BASE + phys_off`. The base sits at 36 GiB — above the
/// 17 GiB "allocate anywhere" heap cursor and below the 64 GiB arena top — so it never
/// collides with the loaded image, HLE arena, or anonymous/flexible maps (the anywhere
/// allocator is additionally guarded to skip this window). 4 GiB-aligned, so an
/// alignment-honouring offset yields an equally aligned VA (Mono SGen's 1 MiB LOS mask math).
pub const DIRECT_MEMORY_POOL_BASE: u64 = 0x9_0000_0000;
/// Size of the direct-memory pool. Kept equal to what `sceKernelGetDirectMemorySize`
/// reports so a guest that sizes its pool from that call never overflows ours. Sized at
/// **5 GiB** to match the real PS4's application-visible direct-memory budget: a retail
/// title's Onion/Garlic GPU allocator reserves multi-GiB direct-memory pools up front
/// (Celeste asks for a 2 GiB + a 1 GiB reservation during GPU init), so a 512 MiB pool
/// would spuriously ENOMEM and the guest's allocator would report itself exhausted. The
/// window `[36 GiB, 41 GiB)` still sits inside the 64 GiB arena and above the 17 GiB
/// anywhere-cursor.
pub const DIRECT_MEMORY_POOL_SIZE: u64 = 5 * 1024 * 1024 * 1024; // 0x1_4000_0000

/// Guest-resident bump arena for small HLE-owned objects (e.g. the opaque pthread
/// mutex/cond objects that a guest libc stores in its handle slot and then pokes).
/// Base+end set once at HLE install; `hle_alloc` bump-allocates 16-byte-aligned.
static HLE_ARENA_CURSOR: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static HLE_ARENA_END: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// The HLE arena's low bound (its base). `HLE_ARENA_CURSOR` bumps forward from here, so it
/// alone can't tell whether a freed address belongs to the region; the base does. Used by
/// [`hle_free`] to reject a non-arena pointer (e.g. a guest-owned slot value we never handed
/// out) so recycling can never re-hand a foreign address.
static HLE_ARENA_BASE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Per-size free-list of returned HLE-arena blocks, keyed by the 16-byte-rounded size an
/// alloc requested. `hle_free` pushes a block back under its rounded size; `hle_alloc`
/// pops a same-size block before bumping the cursor. Keeping the key = rounded size means a
/// recycled block is always exactly large enough for the next same-size request (mutex and
/// cond objects are both 0x40), so heavy sync churn recycles instead of exhausting the
/// 1 MiB region (task-115). Contended only on the low-frequency mutex/cond init/destroy
/// path, so a plain `Mutex<HashMap>` is fine.
static HLE_FREE_LIST: Mutex<Option<std::collections::HashMap<u64, Vec<u64>>>> = Mutex::new(None);

/// Register the HLE object arena `[base, base+size)`.
pub fn set_hle_arena(base: u64, size: u64) {
    HLE_ARENA_BASE.store(base, std::sync::atomic::Ordering::Relaxed);
    HLE_ARENA_CURSOR.store(base, std::sync::atomic::Ordering::Relaxed);
    HLE_ARENA_END.store(base + size, std::sync::atomic::Ordering::Relaxed);
}

/// Bump-allocate `size` bytes (16-byte-aligned) of guest HLE-object memory. Returns
/// the guest address, or 0 if unset/exhausted. The region is identity-mapped, so the
/// caller may write it through a raw pointer.
///
/// A same-size block previously returned via [`hle_free`] is recycled before the cursor is
/// bumped, so a create/destroy churn (heavy mutex/cond traffic) reuses arena space rather
/// than leaking it. A `0` return (unset/exhausted) is still possible on first-time growth
/// past the region; callers must fail (e.g. `scePthreadMutexInit` returns an errno) rather
/// than leave a null slot.
pub fn hle_alloc(size: u64) -> u64 {
    use std::sync::atomic::Ordering;
    let size = (size + 15) & !15;
    // Recycle a same-size freed block first (mutex/cond churn) before growing the region.
    if let Ok(mut guard) = HLE_FREE_LIST.lock()
        && let Some(map) = guard.as_mut()
        && let Some(list) = map.get_mut(&size)
        && let Some(addr) = list.pop()
    {
        return addr;
    }
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

/// Return an HLE-arena block previously handed out by [`hle_alloc`] so a same-size later
/// alloc can recycle it. `size` must be the same value passed to the matching `hle_alloc`
/// (it is 16-byte-rounded here identically, so the free-list key lines up). A `0` address
/// or an out-of-region address is ignored (defensive: a never-alloc'd or double-freed slot
/// must not corrupt the list). Wired from `mutex_destroy`/`cond_destroy` so the sync-object
/// churn that used to leak (task-115) now recycles.
pub fn hle_free(addr: u64, size: u64) {
    if addr == 0 {
        return;
    }
    use std::sync::atomic::Ordering;
    let base = HLE_ARENA_BASE.load(Ordering::Relaxed);
    let end = HLE_ARENA_END.load(Ordering::Relaxed);
    // Only recycle an address inside the registered HLE region `[base, end)`. A pointer we
    // never handed out (e.g. a guest-owned cond slot value) is dropped rather than added to
    // the free-list, so a later alloc can never re-hand a foreign address.
    if end == 0 || addr < base || addr >= end {
        return;
    }
    let size = (size + 15) & !15;
    if let Ok(mut guard) = HLE_FREE_LIST.lock() {
        let map = guard.get_or_insert_with(std::collections::HashMap::new);
        let list = map.entry(size).or_default();
        // Guard against a double-free adding the same block twice.
        if !list.contains(&addr) {
            list.push(addr);
        }
    }
}

/// The kind of HLE object a [`Handle`] refers to. Encoded into the handle's high bits so a
/// handle carries its own type: a `resolve`/`free` against the wrong kind (or a stale/never-
/// allocated value) is *detectable* rather than silently accepted. Generalises the ad-hoc
/// per-subsystem sentinel tags (AJM `0x0A4A4D..`, NGS2) into one scheme (task-115).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandleKind {
    Pad = 1,
    Mouse = 2,
    Equeue = 3,
    Ajm = 4,
    Ngs2System = 5,
    Ngs2Rack = 6,
    Ngs2Voice = 7,
}

impl HandleKind {
    #[inline]
    fn tag(self) -> u64 {
        self as u64
    }

    fn from_tag(tag: u64) -> Option<HandleKind> {
        match tag {
            1 => Some(HandleKind::Pad),
            2 => Some(HandleKind::Mouse),
            3 => Some(HandleKind::Equeue),
            4 => Some(HandleKind::Ajm),
            5 => Some(HandleKind::Ngs2System),
            6 => Some(HandleKind::Ngs2Rack),
            7 => Some(HandleKind::Ngs2Voice),
            _ => None,
        }
    }
}

/// An opaque HLE object handle: `(kind_tag << HANDLE_TAG_SHIFT) | monotonic_id`. The tag in
/// the high bits makes a wrong-kind or stale handle detectable; the monotonic per-kind id in
/// the low bits keeps values distinct so a freed handle never collides with a live one. The
/// whole value stays inside positive `i32` range so it survives the `i32`-returning open ABIs
/// (`scePadOpen`, `sceMouseOpen`) and the `u64`-out / `i32`-back equeue ABI unchanged.
///
/// Layout: id occupies bits `[0, HANDLE_TAG_SHIFT)`, tag occupies bits
/// `[HANDLE_TAG_SHIFT, 31)`. With a 24-bit shift the id space is 2^24 (~16M) per kind and the
/// tag space is 7, both comfortably inside 31 bits.
pub type Handle = i32;

const HANDLE_TAG_SHIFT: u32 = 24;
const HANDLE_ID_MASK: u64 = (1 << HANDLE_TAG_SHIFT) - 1;

/// A per-kind slab + free-list allocator for opaque HLE object handles. `alloc(kind)` hands
/// back a fresh tagged [`Handle`]; `free(handle)` validates the handle's kind + liveness and
/// retires it; `resolve(handle)` reports whether a handle is currently live *and* of the
/// expected kind, so a `*Read`/`*Submit` on a stale/foreign handle errors instead of acting
/// on garbage. Process-global, boot-independent (no wiring): the first `alloc` lazily seeds
/// the table.
struct HandleTableState {
    /// Next monotonic id per kind tag (never reused across the life of the table, so a freed
    /// id can't be handed out again and alias a stale guest-held handle).
    next_id: std::collections::HashMap<u64, u64>,
    /// Currently-live handles. A handle is present iff it is live; `free` removes it and
    /// `resolve` checks membership, so a double-free or a resolve-after-free is caught.
    live: std::collections::HashSet<Handle>,
}

static HANDLE_TABLE: Mutex<Option<HandleTableState>> = Mutex::new(None);

/// Allocate a fresh live [`Handle`] of `kind`. The returned value carries `kind` in its tag
/// bits so a later `free`/`resolve` can reject a wrong-kind or stale handle. Never returns 0
/// (the first id is 1), so a caller can keep treating 0 as "no handle". Returns `None` only
/// if the per-kind id space (2^24) is exhausted, which no real workload reaches.
pub fn handle_alloc(kind: HandleKind) -> Option<Handle> {
    let mut guard = HANDLE_TABLE.lock().ok()?;
    let state = guard.get_or_insert_with(|| HandleTableState {
        next_id: std::collections::HashMap::new(),
        live: std::collections::HashSet::new(),
    });
    let tag = kind.tag();
    let id = state.next_id.entry(tag).or_insert(1);
    if *id > HANDLE_ID_MASK {
        return None;
    }
    let handle = ((tag << HANDLE_TAG_SHIFT) | *id) as Handle;
    *id += 1;
    state.live.insert(handle);
    Some(handle)
}

/// Retire a [`Handle`] previously returned by [`handle_alloc`], validating that it is live
/// and (if `kind` is `Some`) of the expected kind. Returns `true` on a successful free,
/// `false` for a stale/never-allocated/double-freed or wrong-kind handle — so a `*Close`/
/// `*Destroy` on a bogus handle is a detectable no-op rather than corrupting state.
pub fn handle_free(handle: Handle, kind: Option<HandleKind>) -> bool {
    if let Some(k) = kind
        && handle_kind(handle) != Some(k)
    {
        return false;
    }
    let Ok(mut guard) = HANDLE_TABLE.lock() else {
        return false;
    };
    match guard.as_mut() {
        Some(state) => state.live.remove(&handle),
        None => false,
    }
}

/// Whether `handle` is currently live and (if `kind` is `Some`) of the expected kind. A
/// `*Read`/`*Submit` handler routes an incoming handle through this and errors on `false`, so
/// a stale/foreign/never-allocated handle can't drive a read against unrelated state.
pub fn handle_resolve(handle: Handle, kind: Option<HandleKind>) -> bool {
    if let Some(k) = kind
        && handle_kind(handle) != Some(k)
    {
        return false;
    }
    match HANDLE_TABLE.lock() {
        Ok(guard) => guard.as_ref().is_some_and(|s| s.live.contains(&handle)),
        Err(_) => false,
    }
}

/// The [`HandleKind`] encoded in a handle's tag bits, or `None` if the tag is not a known
/// kind (a junk / non-arena value).
pub fn handle_kind(handle: Handle) -> Option<HandleKind> {
    if handle <= 0 {
        return None;
    }
    HandleKind::from_tag((handle as u64) >> HANDLE_TAG_SHIFT)
}

/// Process-global guest-arena bounds `[base, base+span)`, promoted out of `ps4-cpu`
/// (`guest_vm::GUEST_BASE`/`DEFAULT_SPAN`) so range-checking guest pointers in `ps4-core`
/// (`guest_ptr::GuestPtr`) and `ps4-libs` (`is_guest_ptr`) does not have to reach into the
/// cpu crate. Set once at boot next to [`set_hle_arena`]; `0`/`0` (unset) means "no arena
/// wired" and every range check fails closed (headless / unit tests).
static ARENA_BASE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static ARENA_SPAN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Register the guest address-space bounds `[base, base+span)`. Called once at boot from the
/// composition root with the cpu's `GUEST_BASE`/`DEFAULT_SPAN`.
pub fn set_arena_bounds(base: u64, span: u64) {
    ARENA_BASE.store(base, std::sync::atomic::Ordering::Relaxed);
    ARENA_SPAN.store(span, std::sync::atomic::Ordering::Relaxed);
}

/// The registered guest arena bounds `(base, base + span)`, or `None` when unset (headless /
/// unit tests). A range check on `None` must fail closed (treat every address as out of the
/// arena) rather than fall back to an unbounded deref.
pub fn arena_bounds() -> Option<(u64, u64)> {
    use std::sync::atomic::Ordering;
    let base = ARENA_BASE.load(Ordering::Relaxed);
    let span = ARENA_SPAN.load(Ordering::Relaxed);
    if span == 0 {
        return None;
    }
    Some((base, base.saturating_add(span)))
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
