use crate::fs::FileSystem;
use crate::sync::SyncManager;
use ps4_core::img::ExecutableImage;
use ps4_core::pad::InputManager;
use std::sync::{Arc, RwLock};

use crate::thread::{Thread, ThreadManager};
use crate::tls::TlsKeys;
use ps4_core::img::TlsInfo;
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
    pub gpu_manager: Option<Arc<ps4_gpu::GpuManager>>,
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
}

impl Process {
    pub fn new(
        guest_vm: Arc<ps4_cpu::GuestVm>,
        memory: Box<dyn VirtualMemoryManager>,
        modules: ModuleManager,
        linker: DynamicLinker,
        gpu_manager: Option<Arc<ps4_gpu::GpuManager>>,
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
            gpu_manager,
            input_manager,
            module_inits: RwLock::new(Vec::new()),
            main_thread_pthread: std::sync::atomic::AtomicU64::new(0),
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
        if let Ok(libs) = elf_loader.libraries() {
            for lib in libs {
                self.load_module_tree(
                    &game_dir,
                    &lib,
                    &mut modules_guard,
                    &mut **memory_guard,
                    &mut loaded,
                    &mut loading,
                );
            }
        }

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
    fn find_module_file(game_dir: &std::path::Path, lib: &str) -> Option<std::path::PathBuf> {
        let candidates = [
            game_dir.join(format!("{lib}.prx")),
            game_dir.join("sce_module").join(format!("{lib}.prx")),
        ];
        candidates.into_iter().find(|p| p.exists())
    }

    /// Post-order dependency load: parse the `.prx` for `lib`, recurse into ITS
    /// DT_NEEDED first (so leaves are registered before the modules that import them),
    /// then map + link this module. `loading` breaks import cycles; `loaded` dedups.
    fn load_module_tree(
        &self,
        game_dir: &std::path::Path,
        lib: &str,
        modules: &mut ModuleManager,
        memory: &mut dyn VirtualMemoryManager,
        loaded: &mut std::collections::HashSet<String>,
        loading: &mut std::collections::HashSet<String>,
    ) {
        if loaded.contains(lib) || loading.contains(lib) {
            return;
        }
        let Some(path) = Self::find_module_file(game_dir, lib) else {
            // No local .prx: HLE-provided (libkernel/libSceNet/…) or absent. Mark seen so
            // a repeated DT_NEEDED does not re-probe the filesystem.
            loaded.insert(lib.to_string());
            return;
        };

        loading.insert(lib.to_string());
        match std::fs::read(&path)
            .map_err(|e| e.to_string())
            .and_then(|raw| ps4_loader::container::open(raw).map_err(|e| e.to_string()))
            .and_then(|c| ParsedImage::parse(c).map_err(|e| e.to_string()))
        {
            Ok(parsed) => {
                let module = PlainElf::new(parsed);
                if let Ok(deps) = module.libraries() {
                    for dep in deps {
                        self.load_module_tree(game_dir, &dep, modules, memory, loaded, loading);
                    }
                }
                match self.linker.load_image(modules, memory, &module, lib) {
                    Ok(id) => {
                        // Record this module's `module_start` (its entry_point = base + e_entry)
                        // in load order. `load_module_tree` is post-order, so pushing here — after
                        // recursing into deps and mapping this module — yields leaves-first order.
                        if let Some(m) = modules.modules.get(&id) {
                            self.module_inits.write().unwrap().push(m.entry_point);
                        }
                        info!("Loader: module '{}' loaded from {}", lib, path.display());
                    }
                    Err(e) => tracing::warn!("Loader: module '{}' failed to load: {}", lib, e),
                }
            }
            Err(e) => tracing::warn!(
                "Loader: module '{}' parse failed ({}): {}",
                lib,
                path.display(),
                e
            ),
        }
        loading.remove(lib);
        loaded.insert(lib.to_string());
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

    pub fn mutex_timedlock(&self, addr: u64, abstime_ptr: u64) -> Result<i32, u64> {
        let tid = ps4_core::kernel::current_tid();
        let memory_guard = self.memory.read().unwrap();
        self.sync_manager
            .mutex_timedlock(addr, tid, abstime_ptr, &**memory_guard)
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

        let ptr = match memory.map(target_addr, len, protection, Some("guest_mmap")) {
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
        // Signal the GPU resource cache that this guest range is gone (doc-4 §8): it evicts
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
}
