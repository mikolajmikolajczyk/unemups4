use crate::manager::{Module, ModuleHandle, ModuleManager};
use ps4_core::img::{ExecutableImage, RelocationKind};
use ps4_core::memory::{MemoryAccessExt, MemoryProtection, VirtualMemoryManager};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tracing::info;

#[derive(Debug, thiserror::Error)]
pub enum LoaderError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Container error: {0}")]
    Container(#[from] crate::container::ContainerError),
    #[error("Format error: {0}")]
    Format(String),
    #[error("Memory error: {0}")]
    Memory(String),
    #[error("Relocation error: {0}")]
    Relocation(String),
}

struct LinkerState {
    stubs_base: u64,
    stubs_cursor: u64,
}

#[derive(Clone)]
pub struct DynamicLinker {
    state: Arc<Mutex<LinkerState>>,
}

impl Default for DynamicLinker {
    fn default() -> Self {
        Self::new()
    }
}

impl DynamicLinker {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(LinkerState {
                stubs_base: 0,
                stubs_cursor: 0,
            })),
        }
    }

    pub fn init_stubs(&self, memory: &mut dyn VirtualMemoryManager) -> Result<(), String> {
        let addr = memory
            .map(
                0,
                0x10000, // 64KB
                MemoryProtection::READ | MemoryProtection::WRITE | MemoryProtection::EXEC,
                Some("Lazy_Stubs"),
            )
            .map_err(|e| e.to_string())?;

        let mut state = self.state.lock().unwrap();
        state.stubs_base = addr;
        tracing::info!("Linker: Allocated Lazy Stubs heap at {:#x}", addr);
        Ok(())
    }

    /// Load ONE image at a freshly-allocated base with its own real module id, and
    /// register it (base + base-shifted exports) so a later module can cross-resolve
    /// against it. Returns the module handle; the entry point lives on the registered
    /// `Module`. `load_executable` is a thin wrapper over this that returns the entry
    /// point for the kernel's main-executable path.
    ///
    /// Base comes from `ModuleManager::allocator` (the allocator starts at
    /// `0x400_000`, so the FIRST image still lands there — no regression) rather than
    /// a hardcoded constant; the module id comes from `get_next_handle`, so
    /// DTPMOD/TLS relocations name the real module instead of the literal `1`.
    pub fn load_image(
        &self,
        manager: &mut ModuleManager,
        memory: &mut dyn VirtualMemoryManager,
        image: &impl ExecutableImage,
        name: &str,
    ) -> Result<ModuleHandle, LoaderError> {
        let total_size = image.memory_size().map_err(LoaderError::Io)?;
        let module_id = manager.get_next_handle();
        let base_addr = if total_size > 0 {
            manager.allocator.allocate(total_size)
        } else {
            0
        };

        info!(
            "Linker: Loading '{}' at {:#x} (size: {:#x})",
            name, base_addr, total_size
        );
        tracing::debug!("Linker: '{}' assigned module id {}", name, module_id);
        let libraries = image.libraries().map_err(LoaderError::Io)?;
        for lib in libraries {
            info!("Linker: Required Library: {}", lib);
        }
        let segments = image.segments().map_err(LoaderError::Io)?;

        for seg in &segments {
            let vaddr = base_addr + seg.offset;

            let align_mask = 0xFFF;
            let map_size = (seg.data.len() + seg.bss_size + align_mask) & !align_mask;

            let map_prot = seg.protection | MemoryProtection::WRITE;

            memory
                .map(vaddr, map_size, map_prot, Some(name))
                .map_err(|e| LoaderError::Memory(e.to_string()))?;

            if !seg.data.is_empty() {
                memory
                    .write_bytes(vaddr, &seg.data)
                    .map_err(|e| LoaderError::Memory(e.to_string()))?;
            }

            if seg.bss_size > 0 {
                let bss_start = vaddr + seg.data.len() as u64;
                memory
                    .zero_memory(bss_start, seg.bss_size)
                    .map_err(|e| LoaderError::Memory(e.to_string()))?;
            }
        }

        let relocs = image.relocations().map_err(LoaderError::Io)?;
        let mut applied_relocs = 0;

        for reloc in relocs {
            let target_vaddr = base_addr + reloc.offset;

            match reloc.kind {
                // R_X86_64_RELATIVE: B + A (no symbol).
                RelocationKind::Relative => {
                    let val = (base_addr as i64 + reloc.addend) as usize;

                    memory
                        .write::<usize>(target_vaddr, val)
                        .map_err(|e| LoaderError::Relocation(e.to_string()))?;

                    applied_relocs += 1;
                }

                // R_X86_64_64: S + A. resolve the actual symbol value, don't
                // assume it sits at the load base (previously this was folded
                // into the RELATIVE arm, dropping S).
                RelocationKind::Absolute64 => {
                    let s = reloc
                        .symbol_value
                        .map(|v| base_addr + v) // defined in this module
                        .or_else(|| {
                            // imported / cross-module symbol, incl. HLE stubs
                            reloc
                                .symbol_name
                                .as_deref()
                                .and_then(|n| manager.resolve_symbol(n))
                        })
                        .unwrap_or(0);

                    let val = (s as i64 + reloc.addend) as usize;

                    memory
                        .write::<usize>(target_vaddr, val)
                        .map_err(|e| LoaderError::Relocation(e.to_string()))?;

                    applied_relocs += 1;
                }

                RelocationKind::JumpSlot | RelocationKind::GlobDat => {
                    if let Some(sym_name) = &reloc.symbol_name {
                        if let Some(target_addr) = manager.resolve_symbol(sym_name) {
                            memory
                                .write::<usize>(target_vaddr, target_addr as usize)
                                .map_err(|e| LoaderError::Memory(String::from(e)))?;
                            applied_relocs += 1;
                        } else {
                            let mut state = self.state.lock().unwrap();

                            if state.stubs_base == 0 {
                                return Err(LoaderError::Relocation(
                                    "Linker not initialized (stubs heap missing)".into(),
                                ));
                            }

                            // Resolve the NID to a human name when our map knows it, so the
                            // log — and the eventual missing-symbol FATAL, which echoes the
                            // registered string — read `_init_env [NID bzQExy189ZI]` instead
                            // of a bare hash. Only ~17% of a real title's imports are known
                            // (libc/pthread/sce*); the rest stay raw NIDs until the name data
                            // grows (task-113.2 diagnostics).
                            let display_name = ps4_syscalls::SyscallId::from_nid(sym_name)
                                .map(|s| s.as_str())
                                .filter(|n| !n.is_empty() && *n != "Unknown")
                                .map(|n| format!("{n} [NID {sym_name}]"))
                                .unwrap_or_else(|| sym_name.clone());

                            let magic_id = ps4_core::debug::register_missing_symbol(&display_name);

                            let stub_addr = state.stubs_base + state.stubs_cursor;
                            state.stubs_cursor += 32;

                            // Lazy stub for an unresolved import: `MOV R10, RCX;
                            // MOV EAX, magic_id; SYSCALL; RET` (doc-1 dec 2). The magic id
                            // carries the 0xC000_0000 missing-symbol marker; when the guest
                            // calls the stub, the SYSCALL traps into `rust_syscall_handler`,
                            // which sees the marker and reports "[FATAL ERROR] ... missing
                            // symbol". The `MOV R10, RCX` preserves the 4th call-ABI arg
                            // (RCX) into R10 before SYSCALL clobbers RCX.
                            let mut code = Vec::new();
                            // MOV R10, RCX
                            code.extend_from_slice(&[0x49, 0x89, 0xCA]);
                            // MOV EAX, magic_id
                            code.push(0xB8);
                            code.extend_from_slice(&(magic_id as u32).to_le_bytes());
                            // SYSCALL
                            code.push(0x0F);
                            code.push(0x05);
                            // RET
                            code.push(0xC3);

                            memory
                                .write_bytes(stub_addr, &code)
                                .map_err(|e| LoaderError::Memory(e.to_string()))?;

                            // point GOT at the stub
                            memory
                                .write::<usize>(target_vaddr, stub_addr as usize)
                                .map_err(|e| LoaderError::Memory(String::from(e)))?;

                            info!(
                                "[LINKER] Stubbed missing: {} (ID: {:#x})",
                                display_name, magic_id
                            );
                        }
                    }
                }
                RelocationKind::DtpMod64 => {
                    // DTPMOD names the module that owns this TLS block. Use the real
                    // id assigned by ModuleManager (was a hardcoded `1`); for the main
                    // executable this is still id 1, so no behavior change for the
                    // single-module corpus.
                    let val = module_id as usize;
                    memory
                        .write::<usize>(target_vaddr, val)
                        .map_err(|e| LoaderError::Relocation(e.to_string()))?;
                    applied_relocs += 1;
                }

                RelocationKind::DtpOff64 => {
                    // DTPOFF is an offset within the owning module's TLS block. Full
                    // per-symbol TLS offset resolution is deferred; today,
                    // as before, we write the module id as a placeholder (was the same
                    // hardcoded `1`, now sourced from the real handle).
                    let val = module_id as usize;
                    memory
                        .write::<usize>(target_vaddr, val)
                        .map_err(|e| LoaderError::Relocation(e.to_string()))?;
                    applied_relocs += 1;
                }

                RelocationKind::TpOff64 => {
                    // TODO: only handles the addend; external TLS symbol offsets aren't resolved yet
                    let val = reloc.addend as usize;

                    tracing::trace!("TLS TpOff64 at {:#x} = {:#x}", target_vaddr, val);

                    memory
                        .write::<usize>(target_vaddr, val)
                        .map_err(|e| LoaderError::Relocation(e.to_string()))?;
                    applied_relocs += 1;
                }
                _ => {
                    // types we don't handle yet
                }
            }
        }
        info!("Linker: Applied {} relocations.", applied_relocs);

        // strip the temporary WRITE flag back off code/rodata segments
        for seg in &segments {
            let vaddr = base_addr + seg.offset;
            let align_mask = 0xFFF;
            let map_size = (seg.data.len() + seg.bss_size + align_mask) & !align_mask;

            // restore original protection unless it was RW to begin with
            if seg.protection != (MemoryProtection::READ | MemoryProtection::WRITE) {
                memory
                    .protect(vaddr, map_size, seg.protection)
                    .map_err(|e| LoaderError::Memory(e.to_string()))?;
            }
        }

        let entry_point = base_addr + image.entry_point().map_err(LoaderError::Io)?;
        let sections = image.sections().map_err(LoaderError::Io)?;
        let imports = image.imports().map_err(LoaderError::Io)?;

        // exports, shifted by base_addr
        let raw_exports = image.exports().map_err(LoaderError::Io)?;
        let mut abs_exports = HashMap::new();
        for (sym_name, offset) in raw_exports {
            abs_exports.insert(sym_name, base_addr + offset);
        }

        let module = Module {
            id: module_id,
            name: name.to_string(),
            path: format!("/app0/{}", name), // default PS4 path
            base_addr,
            memory_size: total_size,
            entry_point,
            exports: abs_exports,
            imports,
            is_hle: false,
            sections, // used by the debugger
        };

        manager.register_module(module);

        Ok(module_id)
    }

    /// Load the main executable and return its entry point. Thin wrapper over
    /// [`Self::load_image`]; the entry point is read back from the registered module.
    pub fn load_executable(
        &self,
        manager: &mut ModuleManager,
        memory: &mut dyn VirtualMemoryManager,
        image: &impl ExecutableImage,
        name: &str,
    ) -> Result<u64, LoaderError> {
        let id = self.load_image(manager, memory, image, name)?;
        let entry_point = manager
            .modules
            .get(&id)
            .map(|m| m.entry_point)
            .expect("just-registered module must be present");
        Ok(entry_point)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ps4_core::img::{Import, LoadableSegment, Relocation, RelocationKind, Section, TlsInfo};
    use std::collections::HashMap;

    /// In-memory VMM for loader unit tests: identity-mapped, each `map` owns a boxed
    /// buffer; `get_host_ptr` finds the region containing an address. No real mmap,
    /// no Vulkan — pure `cargo test`-able (doc-5 §1: everything but the actual
    /// `map`/`write` calls is driver-free).
    #[derive(Default)]
    struct MockMemory {
        regions: Vec<(u64, Box<[u8]>)>,
        /// Bump cursor for `map(0, ..)` ("allocate anywhere") requests, mirroring a
        /// real VMM: `addr == 0` means place it, not map literally at 0. Kept high so
        /// it never overlaps module bases (allocator starts at 0x40_0000).
        anywhere_cursor: u64,
    }

    impl VirtualMemoryManager for MockMemory {
        fn map(
            &mut self,
            addr: u64,
            size: usize,
            _prot: MemoryProtection,
            _name: Option<&str>,
        ) -> Result<u64, &'static str> {
            // `addr == 0` is the "map anywhere" convention (the Lazy_Stubs heap uses
            // it): place it at a fresh high base instead of literally at 0.
            let placed = if addr == 0 {
                if self.anywhere_cursor == 0 {
                    self.anywhere_cursor = 0x7000_0000;
                }
                let base = self.anywhere_cursor;
                self.anywhere_cursor += (size as u64 + 0xFFF) & !0xFFF;
                base
            } else {
                addr
            };
            self.regions
                .push((placed, vec![0u8; size].into_boxed_slice()));
            Ok(placed)
        }

        fn unmap(&mut self, _addr: u64, _size: usize) -> Result<(), &'static str> {
            Ok(())
        }

        fn protect(
            &mut self,
            _addr: u64,
            _size: usize,
            _prot: MemoryProtection,
        ) -> Result<(), &'static str> {
            Ok(())
        }

        unsafe fn get_host_ptr(&self, addr: u64) -> Option<*mut u8> {
            for (start, buf) in &self.regions {
                let end = start + buf.len() as u64;
                if addr >= *start && addr < end {
                    let off = (addr - start) as usize;
                    return Some(unsafe { buf.as_ptr().add(off) as *mut u8 });
                }
            }
            None
        }

        fn find_free_region(&mut self, _size: usize) -> u64 {
            0
        }

        fn is_memory_free(&self, _addr: u64, _size: usize) -> bool {
            true
        }
    }

    /// Minimal in-memory image: a single RW segment big enough to hold `memory_size`
    /// bytes at offset 0, plus caller-supplied exports/relocations.
    struct FakeImage {
        entry: u64,
        memory_size: usize,
        exports: HashMap<String, u64>,
        relocations: Vec<Relocation>,
    }

    impl FakeImage {
        fn new(memory_size: usize) -> Self {
            FakeImage {
                entry: 0,
                memory_size,
                exports: HashMap::new(),
                relocations: Vec::new(),
            }
        }
    }

    impl ExecutableImage for FakeImage {
        fn segments(&self) -> Result<Vec<LoadableSegment>, std::io::Error> {
            Ok(vec![LoadableSegment {
                offset: 0,
                data: vec![0u8; self.memory_size],
                protection: MemoryProtection::READ | MemoryProtection::WRITE,
                bss_size: 0,
            }])
        }
        fn sections(&self) -> Result<Vec<Section>, std::io::Error> {
            Ok(Vec::new())
        }
        fn entry_point(&self) -> Result<u64, std::io::Error> {
            Ok(self.entry)
        }
        fn memory_size(&self) -> Result<usize, std::io::Error> {
            Ok(self.memory_size)
        }
        fn imports(&self) -> Result<Vec<Import>, std::io::Error> {
            Ok(Vec::new())
        }
        fn exports(&self) -> Result<HashMap<String, u64>, std::io::Error> {
            Ok(self.exports.clone())
        }
        fn libraries(&self) -> Result<Vec<String>, std::io::Error> {
            Ok(Vec::new())
        }
        fn relocations(&self) -> Result<Vec<Relocation>, std::io::Error> {
            Ok(self.relocations.clone())
        }
        fn tls_info(&self) -> Result<Option<TlsInfo>, std::io::Error> {
            Ok(None)
        }
    }

    /// AC#1: the (previously dead) `ModuleAllocator` is now the source of the base.
    /// The allocator starts at `0x400_000`, so the FIRST image still lands there —
    /// homebrew is unmoved — but the value comes from `allocate`, not a constant.
    #[test]
    fn first_image_base_comes_from_allocator_at_0x400000() {
        let linker = DynamicLinker::new();
        let mut mgr = ModuleManager::new();
        let mut mem = MockMemory::default();
        let img = FakeImage::new(0x1000);

        let id = linker
            .load_image(&mut mgr, &mut mem, &img, "eboot.bin")
            .unwrap();

        let module = mgr.modules.get(&id).unwrap();
        assert_eq!(
            module.base_addr, 0x400_000,
            "first image must still load at the allocator start"
        );

        // A second image must get the NEXT slot from the allocator, proving the base
        // is allocated, not a hardcoded constant.
        let img2 = FakeImage::new(0x1000);
        let id2 = linker
            .load_image(&mut mgr, &mut mem, &img2, "second.prx")
            .unwrap();
        let base2 = mgr.modules.get(&id2).unwrap().base_addr;
        assert!(
            base2 > 0x400_000,
            "second image must be allocated past the first (got {base2:#x})"
        );
    }

    /// AC#2: DTPMOD/DTPOFF write the module's real id (from `get_next_handle`), not a
    /// hardcoded literal `1`. The first module's id happens to be 1 — so we load a
    /// SECOND module carrying the TLS relocs and assert it writes id 2.
    #[test]
    fn dtpmod_writes_real_module_id_not_literal_one() {
        let linker = DynamicLinker::new();
        let mut mgr = ModuleManager::new();
        let mut mem = MockMemory::default();

        // module 1: no TLS relocs, just consumes id 1.
        let first = FakeImage::new(0x1000);
        linker
            .load_image(&mut mgr, &mut mem, &first, "eboot.bin")
            .unwrap();

        // module 2 (id 2): a DTPMOD64 at offset 0 and a DTPOFF64 at offset 8.
        let mut second = FakeImage::new(0x1000);
        second.relocations = vec![
            Relocation {
                offset: 0,
                kind: RelocationKind::DtpMod64,
                symbol_index: None,
                addend: 0,
                symbol_name: None,
                symbol_value: None,
            },
            Relocation {
                offset: 8,
                kind: RelocationKind::DtpOff64,
                symbol_index: None,
                addend: 0,
                symbol_name: None,
                symbol_value: None,
            },
        ];
        let id2 = linker
            .load_image(&mut mgr, &mut mem, &second, "second.prx")
            .unwrap();
        assert_eq!(id2, 2, "second module must get id 2");

        let base2 = mgr.modules.get(&id2).unwrap().base_addr;
        let dtpmod: usize = mem.read(base2).unwrap();
        let dtpoff: usize = mem.read(base2 + 8).unwrap();
        assert_eq!(dtpmod, 2, "DTPMOD must write the real module id (2), not 1");
        assert_eq!(dtpoff, 2, "DTPOFF must write the real module id (2), not 1");
    }

    /// AC#3: each image is registered with its allocated base + base-shifted exports
    /// before a dependent relocates, so cross-module `resolve_symbol` works. Load two
    /// images; the second has an Absolute64 reloc against a symbol EXPORTED by the
    /// first, and must resolve to the first module's `base + export_offset`.
    #[test]
    fn second_module_resolves_symbol_exported_by_first() {
        let linker = DynamicLinker::new();
        let mut mgr = ModuleManager::new();
        let mut mem = MockMemory::default();

        // module 1 exports `shared_sym` at offset 0x40.
        let mut first = FakeImage::new(0x1000);
        first.exports.insert("shared_sym".to_string(), 0x40);
        let id1 = linker
            .load_image(&mut mgr, &mut mem, &first, "eboot.bin")
            .unwrap();
        let base1 = mgr.modules.get(&id1).unwrap().base_addr;

        // module 2 has an Absolute64 reloc referencing `shared_sym` (no local value).
        let mut second = FakeImage::new(0x1000);
        second.relocations = vec![Relocation {
            offset: 0x10,
            kind: RelocationKind::Absolute64,
            symbol_index: None,
            addend: 0,
            symbol_name: Some("shared_sym".to_string()),
            symbol_value: None,
        }];
        let id2 = linker
            .load_image(&mut mgr, &mut mem, &second, "second.prx")
            .unwrap();
        let base2 = mgr.modules.get(&id2).unwrap().base_addr;

        let resolved: usize = mem.read(base2 + 0x10).unwrap();
        assert_eq!(
            resolved as u64,
            base1 + 0x40,
            "cross-module reloc must resolve to the first module's base-shifted export"
        );
    }

    /// R_X86_64_RELATIVE: writes `base + addend` (no symbol).
    #[test]
    fn relative_reloc_writes_base_plus_addend() {
        let linker = DynamicLinker::new();
        let mut mgr = ModuleManager::new();
        let mut mem = MockMemory::default();

        let mut img = FakeImage::new(0x1000);
        img.relocations = vec![Relocation {
            offset: 0x20,
            kind: RelocationKind::Relative,
            symbol_index: None,
            addend: 0x8,
            symbol_name: None,
            symbol_value: None,
        }];
        let id = linker
            .load_image(&mut mgr, &mut mem, &img, "eboot.bin")
            .unwrap();
        let base = mgr.modules.get(&id).unwrap().base_addr;
        let written: usize = mem.read(base + 0x20).unwrap();
        assert_eq!(written as u64, base + 0x8, "RELATIVE writes base + addend");
    }

    /// TpOff64: writes the addend verbatim (external TLS offsets not resolved yet).
    #[test]
    fn tpoff64_reloc_writes_addend() {
        let linker = DynamicLinker::new();
        let mut mgr = ModuleManager::new();
        let mut mem = MockMemory::default();

        let mut img = FakeImage::new(0x1000);
        img.relocations = vec![Relocation {
            offset: 0x30,
            kind: RelocationKind::TpOff64,
            symbol_index: None,
            addend: -16,
            symbol_name: None,
            symbol_value: None,
        }];
        let id = linker
            .load_image(&mut mgr, &mut mem, &img, "eboot.bin")
            .unwrap();
        let base = mgr.modules.get(&id).unwrap().base_addr;
        let written: usize = mem.read(base + 0x30).unwrap();
        assert_eq!(written, (-16i64) as usize, "TpOff64 writes the raw addend");
    }

    /// Absolute64 against a symbol defined IN THIS module (symbol_value present)
    /// resolves to `base + symbol_value`, distinct from the cross-module path.
    #[test]
    fn absolute64_uses_local_symbol_value_over_base() {
        let linker = DynamicLinker::new();
        let mut mgr = ModuleManager::new();
        let mut mem = MockMemory::default();

        let mut img = FakeImage::new(0x1000);
        img.relocations = vec![Relocation {
            offset: 0x40,
            kind: RelocationKind::Absolute64,
            symbol_index: None,
            addend: 0x4,
            symbol_name: None,
            symbol_value: Some(0x100), // defined locally
        }];
        let id = linker
            .load_image(&mut mgr, &mut mem, &img, "eboot.bin")
            .unwrap();
        let base = mgr.modules.get(&id).unwrap().base_addr;
        let written: usize = mem.read(base + 0x40).unwrap();
        assert_eq!(
            written as u64,
            base + 0x100 + 0x4,
            "Absolute64 with local symbol_value = base + value + addend"
        );
    }

    /// An unresolved import (JumpSlot, no exporter) must emit a lazy stub and point
    /// the GOT slot at it. The stub is `MOV EAX, magic; SYSCALL; RET`.
    #[test]
    fn unresolved_import_emits_lazy_stub() {
        let linker = DynamicLinker::new();
        let mut mgr = ModuleManager::new();
        let mut mem = MockMemory::default();

        linker.init_stubs(&mut mem).expect("stubs heap must init");

        let mut img = FakeImage::new(0x1000);
        img.relocations = vec![Relocation {
            offset: 0x10,
            kind: RelocationKind::JumpSlot,
            symbol_index: None,
            addend: 0,
            symbol_name: Some("sceNeverDefined".to_string()),
            symbol_value: None,
        }];
        let id = linker
            .load_image(&mut mgr, &mut mem, &img, "eboot.bin")
            .unwrap();
        let base = mgr.modules.get(&id).unwrap().base_addr;

        // The GOT slot now points at a stub address (non-zero, in the stubs heap).
        let stub_addr: usize = mem.read(base + 0x10).unwrap();
        assert_ne!(stub_addr, 0, "GOT slot must point at an emitted stub");

        // The stub begins with MOV R10, RCX (49 89 CA), then MOV EAX, imm32 (0xB8), then
        // SYSCALL (0F 05) + RET (C3).
        let prefix0: u8 = mem.read(stub_addr as u64).unwrap();
        let prefix1: u8 = mem.read(stub_addr as u64 + 1).unwrap();
        let prefix2: u8 = mem.read(stub_addr as u64 + 2).unwrap();
        assert_eq!(
            (prefix0, prefix1, prefix2),
            (0x49, 0x89, 0xCA),
            "stub starts with MOV R10, RCX"
        );
        let opcode: u8 = mem.read(stub_addr as u64 + 3).unwrap();
        assert_eq!(opcode, 0xB8, "MOV EAX, imm32 follows the R10 prefix");
        let syscall0: u8 = mem.read(stub_addr as u64 + 8).unwrap();
        let syscall1: u8 = mem.read(stub_addr as u64 + 9).unwrap();
        let ret: u8 = mem.read(stub_addr as u64 + 10).unwrap();
        assert_eq!((syscall0, syscall1), (0x0F, 0x05), "SYSCALL bytes");
        assert_eq!(ret, 0xC3, "RET byte");
    }

    /// Without init_stubs, an unresolved import must error rather than emit a stub
    /// into a zero heap.
    #[test]
    fn unresolved_import_without_init_errors() {
        let linker = DynamicLinker::new();
        let mut mgr = ModuleManager::new();
        let mut mem = MockMemory::default();

        let mut img = FakeImage::new(0x1000);
        img.relocations = vec![Relocation {
            offset: 0x10,
            kind: RelocationKind::JumpSlot,
            symbol_index: None,
            addend: 0,
            symbol_name: Some("sceNeverDefined".to_string()),
            symbol_value: None,
        }];
        let err = linker
            .load_image(&mut mgr, &mut mem, &img, "eboot.bin")
            .expect_err("unresolved import without stubs heap must error");
        assert!(
            matches!(&err, LoaderError::Relocation(m) if m.contains("not initialized")),
            "got {err:?}"
        );
    }

    /// GlobDat that DOES resolve (a prior module exports the symbol) writes the
    /// resolved absolute address, no stub.
    #[test]
    fn globdat_resolves_to_exported_symbol() {
        let linker = DynamicLinker::new();
        let mut mgr = ModuleManager::new();
        let mut mem = MockMemory::default();

        let mut first = FakeImage::new(0x1000);
        first.exports.insert("g".to_string(), 0x80);
        let id1 = linker
            .load_image(&mut mgr, &mut mem, &first, "a.prx")
            .unwrap();
        let base1 = mgr.modules.get(&id1).unwrap().base_addr;

        let mut second = FakeImage::new(0x1000);
        second.relocations = vec![Relocation {
            offset: 0x8,
            kind: RelocationKind::GlobDat,
            symbol_index: None,
            addend: 0,
            symbol_name: Some("g".to_string()),
            symbol_value: None,
        }];
        let id2 = linker
            .load_image(&mut mgr, &mut mem, &second, "b.prx")
            .unwrap();
        let base2 = mgr.modules.get(&id2).unwrap().base_addr;
        let written: usize = mem.read(base2 + 0x8).unwrap();
        assert_eq!(written as u64, base1 + 0x80, "GlobDat resolves to export");
    }

    /// `load_executable` returns the same entry point the registered module carries
    /// (base-shifted image entry).
    #[test]
    fn load_executable_returns_base_shifted_entry() {
        let linker = DynamicLinker::new();
        let mut mgr = ModuleManager::new();
        let mut mem = MockMemory::default();

        let mut img = FakeImage::new(0x1000);
        img.entry = 0x120;
        let entry = linker
            .load_executable(&mut mgr, &mut mem, &img, "eboot.bin")
            .unwrap();
        // first module lands at allocator start 0x40_0000.
        assert_eq!(entry, 0x40_0000 + 0x120, "entry = base + image entry");
    }
}
