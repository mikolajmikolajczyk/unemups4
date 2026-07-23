use crate::fs::FileSystem;
use crate::sync::SyncManager;
use ps4_core::img::ExecutableImage;
use ps4_core::pad::InputManager;
use std::sync::{Arc, RwLock};

use crate::thread::{Thread, ThreadManager};
use crate::tls::TlsKeys;
use ps4_core::img::TlsInfo;
use ps4_core::kernel::VqRegion;
use ps4_core::memory::{MemoryProtection, VirtualMemoryManager};
use ps4_loader::image::{ParsedImage, PlainElf};
use ps4_loader::linker::DynamicLinker;
use ps4_loader::manager::ModuleManager;
use tracing::info;

pub struct Process {
    pub id: u32,
    /// The shared identity-mapped guest VM (x86jit backend). Every guest thread's
    /// [`crate::thread::Thread::execute`] drives `run_guest_call` over this `Arc`.
    pub guest_vm: Arc<ps4_cpu::GuestVm>,
    pub memory: Arc<RwLock<Box<dyn VirtualMemoryManager>>>,
    pub modules: Arc<RwLock<ModuleManager>>,
    pub fs: Arc<FileSystem>,
    linker: DynamicLinker,
    pub tls_keys: TlsKeys,
    pub sync_manager: Arc<SyncManager>,
    pub thread_manager: Arc<ThreadManager>,
    pub tls_template: RwLock<Option<TlsInfo>>,
    pub input_manager: InputManager,
    /// Absolute `module_start` addresses of the dependency `.prx` modules, in the
    /// order they must be called (leaves-first, i.e. dependencies before dependents).
    /// A PS4 PRX's entry (`e_entry`, == its `DT_INIT`) is its `module_start`; the main
    /// thread invokes these before jumping to the eboot entry so each module's globals
    /// (libc malloc arena, etc.) are initialized before the eboot CRT calls into them.
    /// Empty for single-module homebrew (no sibling `.prx`).
    pub module_inits: RwLock<Vec<u64>>,
    /// Guest address of the main-thread pthread/TCB structure passed as `module_start`'s
    /// arg0 (rdi). PS4 hands each module_start a pointer to the main thread; libc caches
    /// it and derives its pthread state from it. 0 = none (single-module homebrew).
    pub main_thread_pthread: std::sync::atomic::AtomicU64,
    /// Guest path (e.g. `/app0/scePlayStation4.prx`) -> module handle for modules loaded
    /// at runtime via `sceKernelLoadStartModule`. Makes a repeat load of the same path
    /// idempotent (return the existing handle, run `module_start` only once).
    loaded_by_path: std::sync::Mutex<std::collections::HashMap<String, i32>>,
    /// Guest mount point (e.g. `/savedata0`) -> the save directory name mounted there, for
    /// slots mounted via `sceSaveDataMount2`. Assigns a fresh mount point per distinct
    /// directory and lets `sceSaveDataUmount` unregister the right one.
    savedata_mounts: std::sync::Mutex<std::collections::HashMap<String, String>>,
    /// ELF general-dynamic TLS (task-121): thread id -> base guest address of that
    /// thread's lazily-allocated, zero-initialised per-thread TLS arena. Keyed by tid
    /// alone (single flat block per thread; `tls_index.ti_module` is collapsed). Fed by
    /// [`Self::tls_arena_base`], consumed by the `__tls_get_addr` HLE.
    tls_arenas: std::sync::Mutex<std::collections::HashMap<u32, u64>>,
    /// Direct-memory model (task-148). PS4's direct-memory API is *physical-offset* based:
    /// `sceKernelAllocateDirectMemory` reserves a physical range and returns its offset,
    /// `sceKernelMapDirectMemory` maps that offset to a VA, and
    /// `sceKernelReleaseDirectMemory(physOff, len)` frees by **offset**, not VA. Our old HLE
    /// conflated the two (returned the VA as the "offset" and, on release, blindly `munmap`ped
    /// the offset value as if it were a VA), so a release whose `start` was not a live VA —
    /// Mono passes `start=0x0` for hundreds of its releases — corrupted our tracking and
    /// Mono's own phys↔VA bookkeeping, tripping its internal `g_assert(res==0)`
    /// (mono-mmap-orbis.c:219) and killing the asset-streaming thread. This tracker records
    /// each allocation and its VA mapping so release resolves a *tracked* offset to its VA
    /// span (unmapping exactly that), and is a clean success no-op for an untracked /
    /// already-freed / zero offset.
    direct_memory: std::sync::Mutex<DirectMemory>,
    /// Canonical host paths of every `.prx` already mapped, across BOTH the boot dependency
    /// walk and later `sceKernelLoadStartModule` loads.
    ///
    /// The per-walk `loaded` set cannot do this job: a runtime load starts a fresh walk and
    /// would re-map a dependency the boot walk already mapped, at a second base, with a
    /// second `module_start` over a second copy of its globals. Keyed by PATH rather than
    /// module name because one file legitimately answers to several names (`libc.prx` is both
    /// `libc` and `libSceLibcInternal`) — task-29.
    loaded_module_paths: std::sync::Mutex<std::collections::HashMap<String, i32>>,
}

/// Page granularity for direct-memory reservations (4 KiB). Offsets and lengths are rounded
/// up to this so a sub-span release aligns to whole pages, matching the VMA unmap's aligned
/// `madvise` interior.
const DIRECT_MEMORY_PAGE: u64 = 0x1000;

/// Dense physical-offset pool for the direct-memory API (task-148).
///
/// PS4 direct memory is a *physical-offset* coordinate space, dense from 0. Mono (Celeste)
/// allocates offsets here, builds its own physical→virtual table (`va = pool_base + off`),
/// then both releases by offset *and* `munmap`s the VAs it derives. To keep Mono's phys↔VA
/// math in lockstep with ours the pool is one contiguous VA window and the mapping is the
/// pure function `va = DIRECT_MEMORY_POOL_BASE + off` ([`ps4_core::kernel`] owns the
/// constants). This tracker is only the *offset-space* allocator: a bump cursor plus the set
/// of live reservations. Offsets are never reused (the generous 5 GiB pool covers bring-up),
/// so a fresh allocation always maps never-before-used pages and release can be a pure no-op
/// that never aliases a stale offset onto a live region. The VA is never stored — it is
/// recomputed from the offset — so there is no phys↔VA map to desync.
#[derive(Default)]
struct DirectMemory {
    /// Next never-yet-handed-out physical offset (bump cursor into the pool, dense from 0).
    next_off: u64,
    /// Live reservations: physical offset -> length, one per outstanding `AllocateDirectMemory`
    /// (trimmed/split on a partial release). Bounds-checks releases and records granularity.
    reservations: std::collections::BTreeMap<u64, u64>,
}

impl DirectMemory {
    /// Map a physical offset to its virtual address: `va = POOL_BASE + off`.
    fn phys_to_va(off: u64) -> u64 {
        ps4_core::kernel::DIRECT_MEMORY_POOL_BASE + off
    }

    /// `AllocateDirectMemory`: reserve `len` bytes at `align`, returning a **fresh** dense
    /// physical offset (bump the cursor, aligned). `None` == pool exhausted (caller returns
    /// ENOMEM). No VA is mapped here — that is `MapDirectMemory`'s job (allocate is a pure
    /// physical reservation).
    ///
    /// task-148: offsets are never reused. The identity model this replaced always minted a
    /// fresh VA per allocation and never reused freed direct memory; matching that (with a
    /// generous 5 GiB pool) means a genuinely-new allocation always maps to never-before-used
    /// pages that `MapDirectMemory` cleanly zero-fills, and release stays a pure no-op that
    /// cannot alias a stale offset onto a live region. Bring-up churn fits comfortably.
    fn allocate(&mut self, len: u64, align: u64) -> Option<u64> {
        // `len`/`align` are guest-controlled: round and sum with checked arithmetic so a huge
        // length can't wrap the page-round or the `off + len` bound check past `pool_size` and
        // corrupt the bump cursor (release: wrap; debug: panic). Overflow == pool exhausted.
        let len = len
            .max(DIRECT_MEMORY_PAGE)
            .checked_next_multiple_of(DIRECT_MEMORY_PAGE)?;
        let align = align.max(DIRECT_MEMORY_PAGE);
        let pool_size = ps4_core::kernel::DIRECT_MEMORY_POOL_SIZE;

        let off = self.next_off.next_multiple_of(align);
        let end = off.checked_add(len).filter(|&e| e <= pool_size)?;
        self.next_off = end;
        self.reservations.insert(off, len);
        Some(off)
    }

    /// `ReleaseDirectMemory(off, len)`: bookkeeping-only free. Trims/splits every reservation
    /// the release overlaps so the offset stops being tracked as live, and returns the pool VA
    /// span `(va, len)` the offset *would* own (the caller currently discards it — release does
    /// not touch the backing pages; see [`Process::release_direct_memory`]). Returns `None` —
    /// a clean no-op — for an out-of-pool offset (Mono's spurious `start=0x0`/foreign releases
    /// land here since nothing is reserved out of range).
    fn take_for_release(&mut self, off: u64, len: usize) -> Option<(u64, u64)> {
        let pool_size = ps4_core::kernel::DIRECT_MEMORY_POOL_SIZE;
        if off >= pool_size {
            return None; // out of pool — spurious/foreign offset, clean no-op
        }
        // Length: caller's `len` rounded up, or the covering reservation's length if 0.
        let mut free_len = (len as u64).next_multiple_of(DIRECT_MEMORY_PAGE);
        if free_len == 0 {
            free_len = self
                .reservations
                .range(..=off)
                .next_back()
                .filter(|&(&ro, &rl)| ro <= off && off < ro + rl)
                .map(|(&ro, &rl)| ro + rl - off)
                .unwrap_or(DIRECT_MEMORY_PAGE);
        }
        let rel_start = off;
        let rel_end = (off + free_len).min(pool_size);
        self.trim_reservations(rel_start, rel_end);
        Some((Self::phys_to_va(off), rel_end - rel_start))
    }

    /// Trim/split every reservation overlapping `[start, end)`: the released sub-range stops
    /// being reserved while any still-live head/tail of a straddled reservation remains.
    fn trim_reservations(&mut self, start: u64, end: u64) {
        let overlapping: Vec<(u64, u64)> = self
            .reservations
            .range(..end)
            .filter(|&(&ro, &rl)| ro < end && ro + rl > start)
            .map(|(&ro, &rl)| (ro, rl))
            .collect();
        for (ro, rl) in overlapping {
            self.reservations.remove(&ro);
            let re = ro + rl;
            if ro < start {
                self.reservations.insert(ro, start - ro); // surviving head
            }
            if re > end {
                self.reservations.insert(end, re - end); // surviving tail
            }
        }
    }
}

impl Process {
    pub fn new(
        guest_vm: Arc<ps4_cpu::GuestVm>,
        memory: Box<dyn VirtualMemoryManager>,
        modules: ModuleManager,
        linker: DynamicLinker,
        input_manager: InputManager,
    ) -> Arc<Self> {
        Arc::new(Process {
            id: 1,
            guest_vm,
            memory: Arc::new(RwLock::new(memory)),
            modules: Arc::new(RwLock::new(modules)),
            fs: Arc::new(FileSystem::new()),
            sync_manager: Arc::new(SyncManager::new()),
            thread_manager: Arc::new(ThreadManager::new()),
            linker,
            tls_keys: TlsKeys::new(),
            tls_template: RwLock::new(None),
            input_manager,
            module_inits: RwLock::new(Vec::new()),
            main_thread_pthread: std::sync::atomic::AtomicU64::new(0),
            loaded_by_path: std::sync::Mutex::new(std::collections::HashMap::new()),
            savedata_mounts: std::sync::Mutex::new(std::collections::HashMap::new()),
            tls_arenas: std::sync::Mutex::new(std::collections::HashMap::new()),
            direct_memory: std::sync::Mutex::new(DirectMemory::default()),
            loaded_module_paths: std::sync::Mutex::new(std::collections::HashMap::new()),
        })
    }

    pub fn load_executable(&self, filename: &str) -> Result<u64, Box<dyn std::error::Error>> {
        let raw = std::fs::read(filename)?;
        let container = ps4_loader::container::open(raw)?;
        let parsed = ParsedImage::parse(container)?;
        let elf_loader = PlainElf::new(parsed);

        if let Ok(Some(info)) = elf_loader.tls_info() {
            let mut lock = self.tls_template.write().unwrap();
            *lock = Some(info);
            tracing::info!(target: "Kernel.Process", "Loaded TLS Template: size={}", lock.as_ref().unwrap().mem_size);
        }

        let mut memory_guard = self.memory.write().unwrap();
        let mut modules_guard = self.modules.write().unwrap();

        // Load the executable's sibling .prx modules (from the dump directory) before
        // the executable itself, leaves-first, so their exports are registered when the
        // executable's imports are linked (the cross-module NID resolution FASE-1). Libs
        // with no local .prx (libkernel, libScePosix, …) are HLE-provided or absent, and
        // are skipped here — the linker stubs any that stay unresolved.
        let game_dir = std::path::Path::new(filename)
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .to_path_buf();
        let mut loaded: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut loading: std::collections::HashSet<String> = std::collections::HashSet::new();
        // MODULE names, not library names: the file to map is named by DT_SCE_NEEDED_MODULE.
        // Using the library name here looked for `libSceLibcInternal.prx`, which does not
        // exist — the file is `libc.prx` — so libc was never loaded as a dependency and every
        // module relocated before it lost its libc imports to missing-symbol stubs (task-29).
        // Collect the WHOLE graph first, then map it all, then relocate it all — the module
        // graph has cycles and no walk order can resolve them (task-29).
        let mut set: Vec<(String, std::path::PathBuf, PlainElf)> = Vec::new();
        if let Ok(libs) = elf_loader.needed_modules() {
            for lib in libs {
                self.collect_module_graph(&game_dir, &lib, &mut set, &mut loaded, &mut loading);
            }
        }
        self.load_module_set(&set, &mut modules_guard, &mut **memory_guard);

        let entry_point = self.linker.load_executable(
            &mut modules_guard,
            &mut **memory_guard,
            &elf_loader,
            "eboot.bin",
        )?;
        info!("Kernel: Executable loaded at {:#x}", entry_point);

        // Publish the absolute SceKernelProcParam address for sceKernelGetProcParam.
        // The blob lives inside a PT_LOAD (already mapped); we only base-shift its vaddr.
        if let Some(pp_vaddr) = elf_loader.parsed().proc_param_vaddr
            && let Some(eboot) = modules_guard.get_by_name("eboot.bin")
        {
            let abs = eboot.base_addr + pp_vaddr;
            ps4_core::kernel::set_proc_param_addr(abs);
            info!("Kernel: SceKernelProcParam at {:#x}", abs);
        }

        // Retail (multi-module) only: allocate the main-thread pthread/TCB and pass it as
        // module_start's arg0. PS4 hands each module its main-thread pointer here; libc
        // caches it and builds its pthread state onto it. Zeroed canvas — libc initializes
        // the fields it needs. Single-module homebrew never enters this path (no .prx).
        if !self.module_inits.read().unwrap().is_empty() {
            const PTHREAD_SIZE: usize = 0x4000;
            let pthread = memory_guard
                .map(
                    0,
                    PTHREAD_SIZE,
                    MemoryProtection::READ | MemoryProtection::WRITE,
                    Some("main_pthread"),
                )
                .map_err(|e| format!("main pthread map failed: {e}"))?;
            memory_guard
                .zero_memory(pthread, PTHREAD_SIZE)
                .map_err(|e| format!("main pthread zero failed: {e}"))?;
            self.main_thread_pthread
                .store(pthread, std::sync::atomic::Ordering::Relaxed);
            info!("Kernel: main-thread pthread at {:#x}", pthread);
        }

        Ok(entry_point)
    }

    /// Locate the `.prx` file backing a DT_NEEDED library name in the dump layout:
    /// `<dir>/<lib>.prx` (root modules like scePlayStation4/libfmod) or
    /// `<dir>/sce_module/<lib>.prx` (libc/libSceFios2). Returns `None` when no local
    /// file exists — the lib is then either HLE-provided or genuinely missing.
    /// Alternative FILE stems for a needed module name, tried in order after the name itself.
    ///
    /// On a real console `libSceLibcInternal` is a system module and the `libc.prx` a title
    /// ships is the SDK's C runtime over it — two different modules. We have neither system
    /// module nor an HLE for the several hundred C functions involved, but the shipped
    /// `libc.prx` exports the same symbols under the same NIDs, which is all resolution needs.
    ///
    /// Without this, `libSceFios2` (UE4's file I/O, so unavoidable) asks for
    /// `libSceLibcInternal`, finds no file, is treated as HLE-provided, and has every
    /// `malloc`/`memcpy` import written as a missing-symbol stub — permanently, since its GOT
    /// is relocated long before `libc` loads under its own name (task-29).
    fn module_file_aliases(lib: &str) -> &'static [&'static str] {
        match lib {
            "libSceLibcInternal" => &["libc"],
            _ => &[],
        }
    }

    fn find_module_file(game_dir: &std::path::Path, lib: &str) -> Option<std::path::PathBuf> {
        std::iter::once(lib)
            .chain(Self::module_file_aliases(lib).iter().copied())
            .flat_map(|stem| {
                [
                    game_dir.join(format!("{stem}.prx")),
                    game_dir.join("sce_module").join(format!("{stem}.prx")),
                ]
            })
            .find(|p| p.exists())
    }

    /// Walk the dependency graph rooted at `lib` and COLLECT the modules to load, leaves
    /// first — without mapping or relocating anything.
    ///
    /// Collection is separated from loading because a real title's module graph contains
    /// CYCLES: `libSceLibcInternal` and `libSceFios2` import each other. Post-order alone
    /// cannot order a cycle, so the old load-as-you-walk shape gave whichever module went
    /// first a set of missing-symbol stubs for the other's exports — permanently, since a
    /// relocated GOT is not revisited. [`Self::load_module_set`] fixes that by mapping every
    /// collected module before relocating any (task-29).
    ///
    /// `loading` breaks cycles within this walk and `loaded_module_paths` covers modules an
    /// earlier walk already mapped; both are consulted by PATH, because the cycle that matters
    /// crosses names — `libc.prx` is reached as `libSceLibcInternal` and lists `libc` among
    /// its own needed modules, so by name alone the recursion does not recognise itself.
    fn collect_module_graph(
        &self,
        game_dir: &std::path::Path,
        lib: &str,
        out: &mut Vec<(String, std::path::PathBuf, PlainElf)>,
        loaded: &mut std::collections::HashSet<String>,
        loading: &mut std::collections::HashSet<String>,
    ) {
        if loaded.contains(lib) || loading.contains(lib) {
            return;
        }
        let path_key = Self::find_module_file(game_dir, lib)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        if !path_key.is_empty()
            && (loading.contains(&path_key)
                || loaded.contains(&path_key)
                || self
                    .loaded_module_paths
                    .lock()
                    .is_ok_and(|p| p.contains_key(&path_key)))
        {
            loaded.insert(lib.to_string());
            return;
        }

        let Some(path) = Self::find_module_file(game_dir, lib) else {
            // No local .prx: HLE-provided (libkernel/libSceNet/…) or absent. Mark seen so
            // a repeated DT_NEEDED does not re-probe the filesystem.
            loaded.insert(lib.to_string());
            return;
        };

        loading.insert(lib.to_string());
        if !path_key.is_empty() {
            loading.insert(path_key.clone());
        }
        match std::fs::read(&path)
            .map_err(|e| e.to_string())
            .and_then(|raw| ps4_loader::container::open(raw).map_err(|e| e.to_string()))
            .and_then(|c| ParsedImage::parse(c).map_err(|e| e.to_string()))
        {
            Ok(parsed) => {
                let module = PlainElf::new(parsed);
                if let Ok(deps) = module.needed_modules() {
                    for dep in deps {
                        self.collect_module_graph(game_dir, &dep, out, loaded, loading);
                    }
                }
                out.push((lib.to_string(), path, module));
            }
            Err(e) => tracing::warn!(
                "Loader: module '{}' parse failed ({}): {}",
                lib,
                path.display(),
                e
            ),
        }
        loading.remove(lib);
        if !path_key.is_empty() {
            loading.remove(&path_key);
            loaded.insert(path_key);
        }
        loaded.insert(lib.to_string());
    }

    /// Map every collected module, THEN relocate every one of them, and record their
    /// `module_start`s leaves-first.
    ///
    /// The two passes are the point. After pass one every module's exports are registered, so
    /// pass two resolves imports across a cycle exactly as it does across a tree — no ordering
    /// of the graph is required, because there is nothing left to order.
    fn load_module_set(
        &self,
        set: &[(String, std::path::PathBuf, PlainElf)],
        modules: &mut ModuleManager,
        memory: &mut dyn VirtualMemoryManager,
    ) {
        let mut mapped: Vec<(usize, i32)> = Vec::new();
        for (i, (lib, path, image)) in set.iter().enumerate() {
            match self.linker.map_image(modules, memory, image, lib) {
                Ok(id) => {
                    if let Ok(mut paths) = self.loaded_module_paths.lock() {
                        paths.insert(path.to_string_lossy().into_owned(), id);
                    }
                    mapped.push((i, id));
                    info!("Loader: module '{}' mapped from {}", lib, path.display());
                }
                Err(e) => tracing::warn!("Loader: module '{}' failed to map: {}", lib, e),
            }
        }

        for &(i, id) in &mapped {
            let (lib, _, image) = &set[i];
            if let Err(e) = self.linker.relocate_image(modules, memory, image, id) {
                tracing::warn!("Loader: module '{}' failed to relocate: {}", lib, e);
            }
            if let Some(m) = modules.modules.get(&id) {
                self.module_inits.write().unwrap().push(m.entry_point);
            }
        }
    }

    /// Load + link a `.prx` named by a runtime guest path (`sceKernelLoadStartModule`).
    /// Translates the guest path to its host file, loads any sibling `.prx` it needs
    /// (leaves-first, into the shared identity space), maps + relocates the target, and
    /// registers its exports. Returns `(handle, module_starts)`: the positive module handle
    /// to hand back to the guest, and the `module_start` addresses the caller must run on
    /// the guest thread **in order** — every newly-loaded dependency leaves-first, then the
    /// target itself. A repeat load of the same path is idempotent: it returns the cached
    /// handle with an empty list (nothing to re-run). `Err` is a PS4 errno.
    ///
    /// The module's own DT_NEEDED deps are probed as bare libs in the game dir (the same
    /// `<dir>/<lib>.prx` / `<dir>/sce_module/<lib>.prx` layout the boot-time auto-load
    /// uses); libs with no local `.prx` are HLE-provided and skipped.
    ///
    /// The whole load runs under the `loaded_by_path` lock: it both dedups a concurrent
    /// same-path load (without it two threads could map the same `.prx` twice, at two bases)
    /// and serializes the `module_inits` tail capture below.
    pub fn load_start_module(&self, guest_path: &str) -> Result<(i32, Vec<u64>), i32> {
        let mut cache = self.loaded_by_path.lock().unwrap();
        if let Some(&handle) = cache.get(guest_path) {
            return Ok((handle, Vec::new()));
        }

        // Already loaded under its bare name — a boot-time DT_NEEDED of the eboot, or a prior
        // runtime load via a different guest path. Reuse the existing handle; its module_start
        // already ran (at boot, or on that prior load), so nothing to re-run. Without this the
        // module would be mapped a second time at another base, with duplicate exports.
        let lib_name = std::path::Path::new(guest_path)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("module");
        if let Some(existing) = self.modules.read().unwrap().get_by_name(lib_name) {
            let handle = existing.id;
            cache.insert(guest_path.to_string(), handle);
            info!("Loader: runtime module '{guest_path}' already loaded (handle {handle})");
            return Ok((handle, Vec::new()));
        }

        let host_path = self.fs.host_path(guest_path).ok_or(2i32)?; // ENOENT

        // Already mapped under a DIFFERENT module name (task-29). Celeste loads
        // `/app0/sce_module/libc.prx` at runtime, while the boot walk already mapped that same
        // file as `libSceLibcInternal` — the name check above cannot see it, and mapping it
        // again would put a second libc at a second base with its own uninitialised globals.
        let host_key = host_path.to_string_lossy().into_owned();
        if let Some(&handle) = self
            .loaded_module_paths
            .lock()
            .ok()
            .and_then(|p| p.get(&host_key).copied())
            .as_ref()
        {
            cache.insert(guest_path.to_string(), handle);
            info!(
                "Loader: runtime module '{guest_path}' already mapped from {host_key} \
                 (handle {handle}); reusing"
            );
            return Ok((handle, Vec::new()));
        }
        let game_dir = host_path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .to_path_buf();
        // A runtime module path names a bare lib whose deps live alongside the boot
        // modules; treat the target's directory's parent as the game root when it sits in
        // sce_module/, else the directory itself.
        let game_dir = if game_dir.file_name().and_then(|n| n.to_str()) == Some("sce_module") {
            game_dir.parent().unwrap_or(&game_dir).to_path_buf()
        } else {
            game_dir
        };

        let raw = std::fs::read(&host_path).map_err(|_| 2i32)?; // ENOENT
        let container = ps4_loader::container::open(raw).map_err(|_| 22i32)?; // EINVAL
        let parsed = ParsedImage::parse(container).map_err(|_| 22i32)?;
        let image = PlainElf::new(parsed);

        let mut memory_guard = self.memory.write().unwrap();
        let mut modules_guard = self.modules.write().unwrap();

        // Load the target's dependency graph (mapped in full, then relocated) so its imports
        // resolve against already-registered exports. `load_module_set` appends each newly
        // loaded dep's `module_start` to `self.module_inits`; that vec is drained once at boot
        // and never again, so we snapshot its length here and reclaim the tail below to run
        // these runtime deps ourselves — otherwise their globals never initialize.
        let inits_before = self.module_inits.read().unwrap().len();
        let mut loaded: std::collections::HashSet<String> = std::collections::HashSet::new();
        // Seed with the target's own name: some `.prx` list themselves in DT_NEEDED, which
        // would otherwise make the walk map + init a second copy at another base before we
        // load the real target below.
        loaded.insert(lib_name.to_string());
        let mut loading: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut set: Vec<(String, std::path::PathBuf, PlainElf)> = Vec::new();
        if let Ok(deps) = image.needed_modules() {
            for dep in deps {
                self.collect_module_graph(&game_dir, &dep, &mut set, &mut loaded, &mut loading);
            }
        }
        self.load_module_set(&set, &mut modules_guard, &mut **memory_guard);
        // Reclaim the dep `module_start`s appended above (leaves-first order) so the caller
        // runs them; truncating keeps `module_inits` boot-only and prevents unbounded growth.
        let mut module_starts = self.module_inits.write().unwrap().split_off(inits_before);

        let handle = self
            .linker
            .load_image(&mut modules_guard, &mut **memory_guard, &image, lib_name)
            .map_err(|e| {
                tracing::warn!("Loader: runtime load of '{guest_path}' failed: {e}");
                22i32 // EINVAL
            })?;

        if let Some(target_start) = modules_guard
            .modules
            .get(&handle)
            .map(|m| m.entry_point)
            .filter(|&e| e != 0)
        {
            module_starts.push(target_start);
        }

        drop(modules_guard);
        drop(memory_guard);

        cache.insert(guest_path.to_string(), handle);
        info!(
            "Loader: runtime module '{guest_path}' loaded (handle {handle}, {} module_start(s) to run)",
            module_starts.len()
        );
        Ok((handle, module_starts))
    }

    /// Resolve an exported symbol (by plain C name) in a runtime-loaded module to its
    /// absolute guest address (`sceKernelDlsym`). Retail `.prx` exports are keyed by NID,
    /// so the name is hashed to its NID before lookup; the module's `exports` map also
    /// carries any plain-named (homebrew) exports, so both forms are tried.
    pub fn module_dlsym(&self, handle: i32, name: &str) -> Option<u64> {
        let modules = self.modules.read().unwrap();
        let module = modules.modules.get(&handle)?;

        if let Some(&addr) = module.exports.get(name) {
            return Some(addr);
        }
        // KNOWN LIMITATION (task-120): NID is derived only from the SDK name->NID table, so
        // an export name absent from that table (app-specific / C++-mangled) misses even when
        // the module exports it under NID(name). Real dlsym forward-hashes any name
        // (SHA-1(name||salt) -> Sony base64). Masked today: the observed misses were genuinely
        // optional probes the .prx does not export.
        let nid = ps4_syscalls::SyscallId::from_symbol_name(name)?.nid();
        module.exports.get(nid).copied()
    }

    /// Host directory that backs the guest's save-data area: `<title_dir>/savedata/`.
    /// Anchored on the title's own `eboot.bin` (which exists only in the dump dir, so it
    /// resolves through the union `/app0` mounts to the title dir rather than the dev
    /// `game_data` layer), so saves live alongside the dump — outside the repo — and
    /// persist across runs.
    fn savedata_root(&self) -> Option<std::path::PathBuf> {
        self.fs
            .host_path("/app0/eboot.bin")
            .and_then(|p| p.parent().map(|d| d.join("savedata")))
    }

    /// `sceAppContentTemporaryDataMount2`: mount the title's TEMPORARY data area and return
    /// its guest mount point.
    ///
    /// Temporary data is the console's scratch area — a title keeps caches, shader archives
    /// and streaming spill there — and it is separate from savedata: the system may clear it
    /// between runs, so nothing that matters to a player lives in it.
    ///
    /// Backed by a real host directory beside the dump (`<game>/tempdata/`) rather than a host
    /// temp dir, so it survives a run and can be inspected after a crash. We never clear it;
    /// a title that assumes an empty mount will find its own last run's files, which is
    /// permitted (the system MAY clear, not MUST) and is the more useful behaviour while
    /// bringing a title up.
    pub fn tempdata_mount(&self) -> Result<String, i32> {
        let root = self
            .fs
            .host_path("/app0/eboot.bin")
            .and_then(|p| p.parent().map(|d| d.join("tempdata")))
            .ok_or(0x16i32)?; // EINVAL: no /app0 mount
        if !root.is_dir() {
            std::fs::create_dir_all(&root).map_err(|_| 0x5i32)?; // EIO
        }
        let mount_point = "/temp0".to_string();
        self.fs.mount(&mount_point, root);
        Ok(mount_point)
    }

    pub fn savedata_mount(
        &self,
        _user_id: u32,
        dir_name: &str,
        requested_blocks: u64,
        _mount_mode: u32,
    ) -> Result<(String, u32, u64), i32> {
        // Default budget when the request carries none: a modest slot size.
        const DEFAULT_BLOCKS: u64 = 4096;

        let root = self.savedata_root().ok_or(0x16i32)?; // EINVAL: no /app0 mount
        let host_dir = root.join(dir_name);
        let existed = host_dir.is_dir();

        // Trusted-homebrew local mount: always back the slot with a real host directory so
        // the guest's subsequent file I/O succeeds, regardless of the request's mount-mode
        // flags (the observed titles mount without the CREATE bit yet expect the slot).
        if !existed {
            std::fs::create_dir_all(&host_dir).map_err(|_| 0x5i32)?; // EIO
        }

        let mut mounts = self.savedata_mounts.lock().unwrap();
        let mount_point = alloc_savedata_mount_point(&mounts, dir_name);

        self.fs.mount(&mount_point, host_dir);
        mounts.insert(mount_point.clone(), dir_name.to_string());

        let required_blocks = if requested_blocks == 0 {
            DEFAULT_BLOCKS
        } else {
            requested_blocks
        };
        let mount_status = if existed { 0 } else { 1 };
        Ok((mount_point, mount_status, required_blocks))
    }

    pub fn savedata_umount(&self, mount_point: &str) -> Result<(), i32> {
        let mut mounts = self.savedata_mounts.lock().unwrap();
        if mounts.remove(mount_point).is_none() {
            return Err(0x13i32); // ENODEV: nothing mounted there
        }
        self.fs.unmount(mount_point);
        Ok(())
    }

    pub fn savedata_dir_count(&self) -> u32 {
        let Some(root) = self.savedata_root() else {
            return 0;
        };
        let Ok(entries) = std::fs::read_dir(&root) else {
            return 0;
        };
        entries
            .filter_map(Result::ok)
            .filter(|e| e.path().is_dir())
            .count() as u32
    }

    pub fn virtual_query(&self, addr: u64, find_next: bool) -> Option<VqRegion> {
        let memory = self.memory.read().unwrap();
        let vma = memory.query_region(addr, find_next)?;
        // A flexible/anonymous mapping is one the guest mapped at runtime (mmap /
        // MapFlexibleMemory) rather than a file/direct one; the loader names its own
        // segments, so treat the runtime "dynamic_alloc"/flexible names as flexible.
        let is_flexible =
            matches!(vma.name.as_str(), "dynamic_alloc") || vma.name.starts_with("flexible");
        Some(VqRegion {
            start: vma.start,
            end: vma.end,
            protection: vma.protection.bits() as i32,
            memory_type: 0,
            is_flexible,
            name: vma.name,
        })
    }

    pub fn create_thread(
        &self,
        entry_point: u64,
        arg: u64,
        process_weak: std::sync::Weak<Process>,
    ) -> Result<Arc<Thread>, Box<dyn std::error::Error>> {
        let mut memory = self.memory.write().unwrap();
        self.thread_manager
            .create_thread(entry_point, arg, process_weak, &mut **memory)
    }

    pub fn tls_set_specific(&self, key: u32, value: u64) -> Result<(), u64> {
        let t = self.thread_manager.current_thread()?;
        t.tls_set_specific(key, value)
    }

    pub fn tls_get_specific(&self, key: u32) -> Result<u64, u64> {
        let t = self.thread_manager.current_thread()?;
        Ok(t.tls_get_specific(key))
    }
    pub fn tls_keys_max(&self) -> usize {
        self.tls_keys.max_key()
    }

    pub fn tls_key_destructor(&self, key: u32) -> Option<u64> {
        self.tls_keys.get_dtor(key)
    }

    pub fn thread_get_name(&self, tid: u32, out_ptr: u64, len: usize) -> Result<i32, u64> {
        let memory = self.memory.read().unwrap();
        self.thread_manager
            .thread_get_name(tid, out_ptr, len, &**memory)
    }

    pub fn cond_timedwait(&self, cond_addr: u64, mutex_addr: u64, micros: u32) -> Result<i32, u64> {
        let tid = ps4_core::kernel::current_tid();
        self.sync_manager
            .cond_timedwait(cond_addr, mutex_addr, tid, micros)
    }

    pub fn mutex_timedlock(&self, addr: u64, micros: u32) -> Result<i32, u64> {
        let tid = ps4_core::kernel::current_tid();
        self.sync_manager.mutex_timedlock(addr, tid, micros)
    }
    pub fn mmap(
        &self,
        addr: u64,
        len: usize,
        prot: i32,
        flags: i32,
        fd: i32,
        offset: i64,
    ) -> Result<u64, i64> {
        self.mmap_aligned(addr, len, 0, prot, flags, fd, offset)
    }

    /// `mmap` with an explicit `align` for an "allocate anywhere" request (rounds the
    /// chosen base up to `align`). Direct/flexible-memory maps that carry a guest
    /// alignment (e.g. Mono SGen's 1 MB-aligned LOS sections) route here so the base
    /// honours it; the plain [`mmap`](Self::mmap) passes `align = 0` (backend default).
    #[allow(clippy::too_many_arguments)]
    pub fn mmap_aligned(
        &self,
        addr: u64,
        len: usize,
        align: usize,
        prot: i32,
        flags: i32,
        fd: i32,
        offset: i64,
    ) -> Result<u64, i64> {
        let mut memory = self.memory.write().unwrap();

        // translate protections guest -> host
        // PROT_READ=1, PROT_WRITE=2, PROT_EXEC=4
        let mut protection = MemoryProtection::empty();
        if prot & 1 != 0 {
            protection |= MemoryProtection::READ;
        }
        if prot & 2 != 0 {
            protection |= MemoryProtection::WRITE;
        }
        if prot & 4 != 0 {
            protection |= MemoryProtection::EXEC;
        }

        // MAP_FIXED (0x10). without it addr is just a hint; the mgr treats a
        // nonzero addr as fixed, so pass 0 to let the allocator choose.
        let target_addr = if (flags & 0x10) == 0x10 { addr } else { 0 };

        let ptr = match memory.map_aligned(target_addr, len, align, protection, Some("guest_mmap"))
        {
            Ok(ptr) => ptr,
            Err(_) => return Err(12), // ENOMEM
        };

        // File-backed mapping (fd >= 0, not MAP_ANONYMOUS 0x1000): populate the region
        // with the file's bytes at `offset`. The Mono runtime mmaps its managed
        // assemblies (mscorlib.dll, …) this way; an anonymous (zeroed) mapping made Mono
        // reject them as "invalid CIL image". The arena is identity-mapped, so the mapped
        // guest address IS a host pointer — pread straight into it (no `len`-sized host
        // bounce buffer, so a large/garbage `len` can't force a huge host allocation).
        if fd >= 0 && (flags & 0x1000) == 0 {
            let region = unsafe { std::slice::from_raw_parts_mut(ptr as *mut u8, len) };
            match self.fs.pread(fd, offset.max(0) as u64, region) {
                Ok(n) => {
                    // A short read / bytes past EOF read as zero, per mmap-of-file semantics.
                    region[n..].fill(0);
                }
                Err(_) => {
                    let _ = memory.unmap(ptr, len); // don't leak the just-created mapping
                    return Err(9); // EBADF
                }
            }
        }
        Ok(ptr)
    }
    pub fn munmap(&self, addr: u64, len: usize) -> Result<i32, i64> {
        let mut memory = self.memory.write().unwrap();
        let result = match memory.unmap(addr, len) {
            Ok(_) => Ok(0),
            Err(_) => Err(22), // EINVAL
        };
        drop(memory);
        // Signal the GPU resource cache that this guest range is gone (doc-2 §8): it evicts
        // any cache entry keyed on the range so a later realloc of the same address mints a
        // fresh id (not a stale-id clean hit) and revokes any zero-copy import so the
        // backend's external-memory buffer does not dangle into the freed pages. Only on a
        // successful unmap; a no-op (nothing to free) when no GPU cache is wired (headless).
        // The `memory` write lock is dropped first so the sink's cross-thread teardown does
        // not run under it.
        if result.is_ok()
            && let Some(sink) = ps4_core::gpu::memory_free_sink()
        {
            sink.notify_free(addr, len as u64);
        }
        result
    }

    /// `sceKernelAllocateDirectMemory` (task-148): reserve `len` bytes of direct memory at
    /// `align`, returning a **physical offset** into the dense pool (the guest maps it via
    /// `MapDirectMemory`'s job), matching the real API. The pool is a coordinate space
    /// distinct from VAs so a guest (Mono) that carves its own physical pool and releases
    /// sub-chunks by pool-relative offset stays consistent with us. `align` honours the
    /// guest's alignment (e.g. Mono SGen's 1 MB LOS sections). `Err(12)` (ENOMEM) on
    /// exhaustion.
    pub fn allocate_direct_memory(&self, len: usize, align: usize) -> Result<u64, i64> {
        // Tolerate a poisoned lock at the guest-syscall boundary: the direct-memory tracker has
        // no cross-invariant a poison could corrupt, so degrade to the inner guard rather than
        // panicking the whole emulator on a poisoned mutex.
        self.direct_memory
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .allocate(len as u64, align as u64)
            .ok_or(12) // ENOMEM: pool exhausted
    }

    /// `sceKernelAvailableDirectMemorySize`: the largest free run in `[search_start,
    /// search_end)` at `align`, as `(physical offset, size)`.
    ///
    /// The pool is a bump allocator that never reuses offsets (see [`DirectMemory::allocate`]),
    /// so the only free run is everything above the cursor. That makes this answer exact for
    /// the allocator we actually have, rather than optimistic: a guest told about space we
    /// would then refuse to reserve is worse off than one told the truth.
    pub fn available_direct_memory(
        &self,
        search_start: u64,
        search_end: u64,
        align: u64,
    ) -> Option<(u64, u64)> {
        let dm = self.direct_memory.lock().unwrap_or_else(|e| e.into_inner());
        let align = align.max(DIRECT_MEMORY_PAGE);
        let pool_size = ps4_core::kernel::DIRECT_MEMORY_POOL_SIZE;

        // `search_start` is guest-controlled: round it up with checked arithmetic so a value
        // near u64::MAX can't wrap the page-round to 0 and report offset 0 (already reserved)
        // as free (release: wrap; debug: panic). Overflow or start past the pool == no space.
        let start = search_start
            .max(dm.next_off)
            .checked_next_multiple_of(align)
            .filter(|&s| s < pool_size)?;
        // A zero `search_end` means "no upper bound" — the guest passes the pool size it read
        // from GetDirectMemorySize, but not every caller bothers.
        let end = if search_end == 0 {
            pool_size
        } else {
            search_end.min(pool_size)
        };
        (start < end).then(|| (start, end - start))
    }

    /// `sceKernelMapDirectMemory` (task-148): map the physical `phys_off` (from
    /// [`Self::allocate_direct_memory`]) to its virtual address `va = POOL_BASE + phys_off` and
    /// materialise the mapping so the pages are backed and tracked. Returns that VA.
    ///
    /// The VA is a pure function of the offset (no stored phys↔VA map), so
    /// [`Self::release_direct_memory`] recomputes it — this is exactly what keeps Mono's own
    /// phys→VA table in lockstep with ours. The map is `MAP_FIXED` at the computed VA; a
    /// collision (Mono re-mapping an offset it already mapped, or sub-mapping inside a larger
    /// mapped span) is benign — the pages are already backed, so we still return the VA.
    pub fn map_direct_memory(&self, phys_off: u64, len: usize) -> u64 {
        let va = DirectMemory::phys_to_va(phys_off);
        // PROT_READ|WRITE=3, MAP_ANON|MAP_PRIVATE|MAP_FIXED=0x1012, no fd, addr=va (fixed).
        // On collision the arena pages already exist; ignore the error and return the VA.
        let _ = self.mmap_aligned(va, len.max(1), 0, 3, 0x1012, -1, 0);
        va
    }

    /// `sceKernelReleaseDirectMemory(physOff, len)` (task-148): free a direct-memory region by
    /// its **physical offset**, not by treating the offset as a VA. This is
    /// **bookkeeping-only**: it frees the offset range in our reservation/free-list so it can
    /// be re-handed by a later `AllocateDirectMemory`, but it deliberately does NOT `munmap`,
    /// `madvise(DONTNEED)`, or otherwise zero the backing pages.
    ///
    /// Why not touch the pages (task-148): Mono manages sub-chunks of its own physical pool and
    /// releases them by offset with lengths/coverage that need not match a region *we* still
    /// consider live, and our pool VAs are all host-backed by the one arena mapping. If release
    /// zeroed the VA span it would wipe pages Mono still references (or that a concurrent thread
    /// re-derived), corrupting the Mono/GC heaps and faulting nondeterministically. The
    /// identity model never zeroed freed direct memory either; matching that keeps contents
    /// stable. Pages are legitimately re-zeroed only when a *fresh* (collision-free)
    /// `MapDirectMemory` maps a reused offset. Always returns 0 (success); an out-of-pool /
    /// spurious offset (e.g. Mono's `start=0x0` releases) is likewise a clean no-op.
    pub fn release_direct_memory(&self, phys_off: u64, len: usize) -> i32 {
        // Update offset bookkeeping only (trim/split reservations, add to the reuse list). The
        // returned VA span is intentionally discarded — we do not unmap or zero the pages.
        // Tolerate a poisoned lock at the guest-syscall boundary (see `allocate_direct_memory`):
        // degrade to the inner guard rather than panicking the emulator.
        let _ = self
            .direct_memory
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take_for_release(phys_off, len);
        0
    }

    /// ELF general-dynamic TLS (task-121): base guest address of the calling thread's
    /// per-thread TLS arena, lazily allocated on first access. The arena is a fresh
    /// [`Self::mmap_aligned`] anonymous mapping (identity-mapped, so the returned base is a
    /// dereferenceable guest pointer) which the memory backend zero-fills — correct for
    /// bss-only/empty TLS templates such as the eboot's.
    ///
    /// KNOWN LIMITATION (task-121): the block is zero-initialised only. Modules whose TLS
    /// segment carries initialised `tdata` are NOT template-copied here, so their `__thread`
    /// variables read as zero. That template copy is a deliberate follow-up (needs the
    /// loader to keep per-module tdata and a DTV), not part of this minimal HLE.
    pub fn tls_arena_base(&self) -> Result<u64, u64> {
        let tid = ps4_core::kernel::current_tid();
        let mut arenas = self.tls_arenas.lock().unwrap();
        tls_arena_lookup_or_alloc(&mut arenas, tid, || {
            // 64 KiB per thread: generously covers a flat aggregate of every module's TLS
            // block (the eboot's own PT_TLS is empty; retail managed-runtime __thread usage
            // is small) while staying one arena page-run. `ti_offset` is well within it.
            const TLS_ARENA_SIZE: usize = 64 * 1024;
            // PROT_READ|PROT_WRITE=3, MAP_ANONYMOUS|MAP_PRIVATE=0x1002, no fd, addr=0.
            let base = self
                .mmap_aligned(0, TLS_ARENA_SIZE, 0, 3, 0x1002, -1, 0)
                .map_err(|e| e as u64)?;
            tracing::info!(
                target: "Kernel.Process",
                "tls_arena_base: allocated TLS arena for tid={} at {:#x} ({} bytes)",
                tid, base, TLS_ARENA_SIZE
            );
            Ok(base)
        })
    }
}

/// Cache-then-allocate core of [`Process::tls_arena_base`] (task-121), split out so the
/// per-thread-arena invariants (stable base per tid, distinct base per tid, allocator runs
/// at most once per tid) are unit-testable without a live guest VM. Returns the cached base
/// for `tid` if present, else runs `alloc`, memoises its result, and returns it.
fn tls_arena_lookup_or_alloc<F>(
    arenas: &mut std::collections::HashMap<u32, u64>,
    tid: u32,
    alloc: F,
) -> Result<u64, u64>
where
    F: FnOnce() -> Result<u64, u64>,
{
    if let Some(&base) = arenas.get(&tid) {
        return Ok(base);
    }
    let base = alloc()?;
    arenas.insert(tid, base);
    Ok(base)
}

/// Pick the guest mount point for a savedata slot (`sceSaveDataMount2`). A dir already
/// mounted reuses its existing point (a re-mount of the same directory is idempotent);
/// otherwise the lowest `/savedataN` not currently live is allocated. Split out so the
/// no-collision invariant is unit-testable without a live guest VM.
///
/// The name must NOT be sized from the live map: `sceSaveDataUmount` shrinks the map, so a
/// length-derived name (`/savedata{len}`) can equal a still-live mount point and silently
/// re-point another slot's host directory. Scanning excludes every live key, so a freed
/// index is only ever reused once no live mount holds it.
fn alloc_savedata_mount_point(
    mounts: &std::collections::HashMap<String, String>,
    dir_name: &str,
) -> String {
    if let Some((mp, _)) = mounts.iter().find(|(_, d)| d.as_str() == dir_name) {
        return mp.clone();
    }
    let mut idx = 0u32;
    while mounts.contains_key(&format!("/savedata{idx}")) {
        idx += 1;
    }
    format!("/savedata{idx}")
}

#[cfg(test)]
mod tls_arena_tests {
    use super::tls_arena_lookup_or_alloc;
    use std::cell::Cell;
    use std::collections::HashMap;

    #[test]
    fn same_tid_is_stable_and_allocates_once() {
        let mut arenas: HashMap<u32, u64> = HashMap::new();
        let calls = Cell::new(0u32);
        let alloc = || {
            calls.set(calls.get() + 1);
            Ok(0x1000 + u64::from(calls.get()) * 0x10000)
        };

        let a = tls_arena_lookup_or_alloc(&mut arenas, 7, alloc).unwrap();
        let b = tls_arena_lookup_or_alloc(&mut arenas, 7, alloc).unwrap();
        assert_eq!(a, b, "same tid must return a stable base");
        assert_eq!(calls.get(), 1, "allocator must run at most once per tid");
    }

    #[test]
    fn distinct_tids_get_distinct_bases() {
        let mut arenas: HashMap<u32, u64> = HashMap::new();
        let next = Cell::new(0x2000_0000u64);
        let alloc = || {
            let base = next.get();
            next.set(base + 0x10000);
            Ok(base)
        };

        let t1 = tls_arena_lookup_or_alloc(&mut arenas, 1, alloc).unwrap();
        let t2 = tls_arena_lookup_or_alloc(&mut arenas, 2, alloc).unwrap();
        assert_ne!(t1, t2, "distinct threads must see distinct TLS bases");
        // and each stays stable afterwards
        assert_eq!(
            t1,
            tls_arena_lookup_or_alloc(&mut arenas, 1, alloc).unwrap()
        );
        assert_eq!(
            t2,
            tls_arena_lookup_or_alloc(&mut arenas, 2, alloc).unwrap()
        );
    }

    #[test]
    fn alloc_failure_is_not_cached() {
        let mut arenas: HashMap<u32, u64> = HashMap::new();
        let r = tls_arena_lookup_or_alloc(&mut arenas, 3, || Err(12));
        assert_eq!(r, Err(12));
        // a later successful call for the same tid must still allocate (nothing memoised)
        let ok = tls_arena_lookup_or_alloc(&mut arenas, 3, || Ok(0xDEAD_0000)).unwrap();
        assert_eq!(ok, 0xDEAD_0000);
    }
}

#[cfg(test)]
mod savedata_mount_tests {
    use super::alloc_savedata_mount_point;
    use std::collections::HashMap;

    #[test]
    fn fresh_mounts_get_sequential_points() {
        let mut mounts: HashMap<String, String> = HashMap::new();
        let a = alloc_savedata_mount_point(&mounts, "A");
        assert_eq!(a, "/savedata0");
        mounts.insert(a, "A".to_string());
        let b = alloc_savedata_mount_point(&mounts, "B");
        assert_eq!(b, "/savedata1");
        mounts.insert(b, "B".to_string());
    }

    #[test]
    fn remount_of_same_dir_reuses_its_point() {
        let mut mounts: HashMap<String, String> = HashMap::new();
        mounts.insert("/savedata0".to_string(), "A".to_string());
        mounts.insert("/savedata1".to_string(), "B".to_string());
        assert_eq!(alloc_savedata_mount_point(&mounts, "A"), "/savedata0");
        assert_eq!(alloc_savedata_mount_point(&mounts, "B"), "/savedata1");
    }

    #[test]
    fn umount_then_new_mount_never_reuses_a_live_point() {
        // A -> /savedata0, B -> /savedata1, then umount /savedata0 leaves B live.
        let mut mounts: HashMap<String, String> = HashMap::new();
        mounts.insert("/savedata1".to_string(), "B".to_string());
        // A size-derived name would be `/savedata{len}` == "/savedata1" and hijack B's
        // slot; the scan must instead pick the freed "/savedata0".
        let c = alloc_savedata_mount_point(&mounts, "C");
        assert_eq!(c, "/savedata0");
        assert_ne!(
            c, "/savedata1",
            "must not reassign a still-live mount point"
        );
    }
}

#[cfg(test)]
mod direct_memory_tests {
    use super::DirectMemory;
    use ps4_core::kernel::{DIRECT_MEMORY_POOL_BASE, DIRECT_MEMORY_POOL_SIZE};

    #[test]
    fn allocate_bumps_dense_and_page_rounds() {
        let mut dm = DirectMemory::default();
        let a = dm.allocate(0x8000, 0).unwrap();
        let b = dm.allocate(0x8000, 0).unwrap();
        let c = dm.allocate(0x1234, 0).unwrap(); // rounded up to a page
        assert_eq!(a, 0);
        assert_eq!(b, 0x8000);
        assert_eq!(c, 0x10000);
        // 0x1234 rounds up to two pages (0x2000)
        assert_eq!(dm.next_off, 0x12000);
    }

    #[test]
    fn allocate_honours_alignment() {
        let mut dm = DirectMemory::default();
        let _ = dm.allocate(0x1000, 0).unwrap(); // consumes [0, 0x1000)
        // next request wants 1 MB alignment: base bumps up to the next 1 MB boundary
        let off = dm.allocate(0x1000, 0x10_0000).unwrap();
        assert_eq!(off, 0x10_0000);
    }

    #[test]
    fn phys_to_va_is_pool_base_plus_off() {
        assert_eq!(DirectMemory::phys_to_va(0), DIRECT_MEMORY_POOL_BASE);
        assert_eq!(
            DirectMemory::phys_to_va(0x28000),
            DIRECT_MEMORY_POOL_BASE + 0x28000
        );
    }

    #[test]
    fn take_for_release_partial_middle_trims_reservation() {
        let mut dm = DirectMemory::default();
        let base = dm.allocate(0x40000, 0).unwrap(); // one 256 KiB region at offset 0
        // release the middle 64 KiB
        let span = dm.take_for_release(base + 0x10000, 0x10000);
        assert_eq!(span, Some((DIRECT_MEMORY_POOL_BASE + 0x10000, 0x10000)),);
        // head [0, 0x10000) and tail [0x20000, 0x40000) survive
        assert!(dm.reservations.contains_key(&0));
        assert!(dm.reservations.contains_key(&0x20000));
    }

    #[test]
    fn take_for_release_out_of_pool_is_none() {
        let mut dm = DirectMemory::default();
        let _ = dm.allocate(0x8000, 0).unwrap();
        assert_eq!(dm.take_for_release(DIRECT_MEMORY_POOL_SIZE, 0x1000), None);
        assert_eq!(
            dm.take_for_release(DIRECT_MEMORY_POOL_SIZE + 0x9999, 0),
            None
        );
        // an in-pool offset returns its VA span even if untracked
        let span = dm.take_for_release(0x8000, 0x1000);
        assert_eq!(span, Some((DIRECT_MEMORY_POOL_BASE + 0x8000, 0x1000)));
    }

    #[test]
    fn released_offset_is_not_reused() {
        let mut dm = DirectMemory::default();
        let a = dm.allocate(0x10000, 0).unwrap(); // [0, 0x10000)
        let _b = dm.allocate(0x10000, 0).unwrap(); // [0x10000, 0x20000)
        assert_eq!(a, 0);
        let _ = dm.take_for_release(a, 0x10000);
        let fresh = dm.allocate(0x10000, 0).unwrap();
        // never reuses the freed `a`; hands out a fresh dense offset past the cursor
        assert_eq!(fresh, 0x20000);
    }

    #[test]
    fn allocate_returns_none_on_exhaustion() {
        let mut dm = DirectMemory::default();
        assert_eq!(dm.allocate(DIRECT_MEMORY_POOL_SIZE + 0x1000, 0), None);
    }
}
