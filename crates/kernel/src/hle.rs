use ps4_core::memory::{MemoryAccessExt, MemoryProtection, VirtualMemoryManager};
use ps4_libs::registry::HleSyscallDef;
use ps4_loader::manager::{Module, ModuleManager};
use std::collections::HashMap;
use tracing::{info, warn};

pub struct HleBootstrap;

impl HleBootstrap {
    pub fn install(
        memory: &mut dyn VirtualMemoryManager,
        modules: &mut ModuleManager,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let hle_base = 0x2000_0000;

        let definitions: Vec<&HleSyscallDef> =
            inventory::iter::<HleSyscallDef>.into_iter().collect();

        if definitions.is_empty() {
            warn!("HLE: Warning - No syscalls found.");
        }

        let alloc_size = (definitions.len() * 32 + 0xFFF) & !0xFFF;
        memory.map(
            hle_base,
            alloc_size,
            MemoryProtection::READ | MemoryProtection::WRITE | MemoryProtection::EXEC,
            Some("HLE_Stubs"),
        )?;

        let mut library_exports: HashMap<String, HashMap<String, u64>> = HashMap::new();
        let mut cursor = 0;

        for def in definitions {
            let stub_addr = hle_base + cursor;
            let sys_id = def.id.0 as u32;

            Self::write_stub(memory, stub_addr as usize, sys_id)?;

            // Name this address for the fault reporter (task-113.2). An implemented import
            // that answers wrongly leaves no trace but its stub address in a GOT slot.
            ps4_core::debug::register_stub_symbol(
                stub_addr,
                def.lib_name,
                def.names.first().copied().unwrap_or_else(|| def.id.name()),
            );

            ps4_libs::register_handler(def.id.0, def.handler);

            let lib_entry = library_exports.entry(def.lib_name.to_string()).or_default();

            for &name in def.names {
                lib_entry.insert(name.to_string(), stub_addr);
                // Retail imports resolve by NID. A handler's alias names each hash to
                // their OWN NID (distinct from the SyscallId's canonical NID), so export
                // every alias's NID too — otherwise e.g. a POSIX `pthread_mutex_lock`
                // (NID 7H0iTOciTLo) import can't find the scePthreadMutexLock handler.
                if let Some(alias) = ps4_syscalls::SyscallId::from_symbol_name(name) {
                    let alias_nid = alias.nid();
                    if !alias_nid.is_empty() {
                        lib_entry.entry(alias_nid.to_string()).or_insert(stub_addr);
                    }
                }
            }
            let nid = def.id.nid();
            if !nid.is_empty() {
                lib_entry.insert(nid.to_string(), stub_addr);
            }
            // Raw NIDs declared by the handler itself, for symbols whose NAME we do not have
            // and therefore cannot hash to reach. `insert`, not `entry`: an explicitly
            // declared NID is a deliberate statement about which import this serves and
            // should win over anything inferred from a name.
            for &raw_nid in def.nids {
                if !raw_nid.is_empty() {
                    lib_entry.insert(raw_nid.to_string(), stub_addr);
                }
            }
            let auto_name = def.id.name();
            if !auto_name.is_empty() && auto_name != "Unknown" {
                lib_entry.entry(auto_name.to_string()).or_insert(stub_addr);
            }
            cursor += 32;
        }

        // HLE DATA exports: symbols libc imports as DATA (not syscall stubs), so an
        // R_X86_64_64 reloc writes the symbol's ADDRESS into a (RELRO) slot the guest
        // then dereferences. __stack_chk_guard is the stack-smashing canary — libc
        // reads it at every protected function's prologue/epilogue. We don't enforce
        // stack protection, so any consistent readable value works; only its address
        // must be non-null (an unresolved import would leave the slot 0 → null deref).
        let data_base = hle_base + alloc_size as u64;
        memory.map(
            data_base,
            0x1000,
            MemoryProtection::READ | MemoryProtection::WRITE,
            Some("HLE_Data"),
        )?;
        memory.write::<u64>(data_base, 0x0011_2233_4455_6600)?;
        if let Some(id) = ps4_syscalls::SyscallId::from_symbol_name("__stack_chk_guard") {
            let entry = library_exports
                .entry(ps4_libs::libs::LIB_KERNEL.to_string())
                .or_default();
            entry.insert("__stack_chk_guard".to_string(), data_base);
            let nid = id.nid();
            if !nid.is_empty() {
                entry.insert(nid.to_string(), data_base);
            }
        }

        // HLE object arena: guest-resident scratch for opaque HLE-owned objects that a
        // guest libc allocates via an sce* call, stores in its handle slot, then pokes
        // directly (e.g. pthread mutex/cond objects). Identity-mapped, so handlers write
        // it through raw pointers.
        let arena_size: u64 = 0x10_0000;
        let arena_base = data_base + 0x1000;
        memory.map(
            arena_base,
            arena_size as usize,
            MemoryProtection::READ | MemoryProtection::WRITE,
            Some("HLE_Objects"),
        )?;
        ps4_core::kernel::set_hle_arena(arena_base, arena_size);

        for (lib_name, exports) in library_exports {
            info!("HLE: Loaded {}", lib_name);
            let handle = modules.get_next_handle();
            modules.register_module(Module::new_hle(handle, &lib_name, exports));
        }

        Ok(())
    }

    /// Emit an HLE syscall stub: `MOV R10, RCX; MOV EAX, id; SYSCALL; RET`, NOP-padded
    /// to 32 bytes. Under the x86jit backend the `SYSCALL` traps into the
    /// run loop's dispatcher (`rust_syscall_handler`) but, like real hardware, clobbers
    /// RCX (<-RIP) and R11 (<-RFLAGS). The 4th call-ABI arg arrives in RCX, so the stub
    /// copies it into R10 (the syscall-ABI 4th-arg register, untouched by SYSCALL) before
    /// the trap; `NativeContext::arg3()` then reads R10. The write goes through the memory
    /// manager (`VmMemoryManager::write_bytes`) so x86jit SMC tracking sees it.
    fn write_stub(
        memory: &mut dyn VirtualMemoryManager,
        addr: usize,
        id: u32,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut code = Vec::with_capacity(32);
        // MOV R10, RCX
        code.extend_from_slice(&[0x49, 0x89, 0xCA]);
        // MOV EAX, id
        code.push(0xB8);
        code.extend_from_slice(&id.to_le_bytes());
        // SYSCALL
        code.push(0x0F);
        code.push(0x05);
        // RET
        code.push(0xC3);

        while code.len() < 32 {
            code.push(0x90);
        }
        memory.write_bytes(addr as u64, &code)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ps4_core::img::Import;
    use ps4_syscalls::SyscallId;

    /// In-memory VMM: each `map` owns a boxed buffer so `write_bytes` (used by
    /// `write_stub`) has real backing. Enough to drive `HleBootstrap::install`.
    #[derive(Default)]
    struct MockMemory {
        regions: Vec<(u64, Box<[u8]>)>,
    }

    impl VirtualMemoryManager for MockMemory {
        fn map(
            &mut self,
            addr: u64,
            size: usize,
            _prot: MemoryProtection,
            _name: Option<&str>,
        ) -> Result<u64, &'static str> {
            self.regions
                .push((addr, vec![0u8; size].into_boxed_slice()));
            Ok(addr)
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

    fn install_hle() -> ModuleManager {
        let mut mem = MockMemory::default();
        let mut modules = ModuleManager::new();
        HleBootstrap::install(&mut mem, &mut modules).expect("HLE install");
        modules
    }

    /// AC#1 (synthetic): a retail import carries the raw NID string. The same NID
    /// keys the HLE export (hle.rs registers under `def.id.nid()`), so
    /// `resolve_symbol(nid)` returns the stub — no re-hashing at resolve time.
    #[test]
    fn retail_nid_import_resolves_to_hle_stub() {
        let modules = install_hle();

        // A retail import as SceDynamic would produce it: symbol_name = canonical NID.
        let nid = SyscallId::from_symbol_name("sceKernelAllocateDirectMemory")
            .unwrap()
            .nid();
        let import = Import {
            lib_name: "libkernel".into(),
            symbol_name: nid.to_string(),
            symbol_id: 0,
        };

        let addr = modules
            .resolve_symbol(&import.symbol_name)
            .expect("retail NID resolves to an HLE stub");
        assert!(addr >= 0x2000_0000, "resolves into the HLE stub heap");

        // And it points at the SAME stub the plaintext name resolves to.
        let by_name = modules
            .resolve_symbol("sceKernelAllocateDirectMemory")
            .unwrap();
        assert_eq!(addr, by_name);
    }

    /// AC#1 (real eboot, guarded + ignored, never committed). Loads the retail
    /// Bloodborne eboot, then asserts at least one `sceKernel*` import (raw NID)
    /// resolves to a stub via the same HLE path. Skips cleanly if absent or if the
    /// container is encrypted (no decryption in this project).
    #[test]
    #[ignore = "requires local retail eboot; run with --ignored"]
    fn bloodborne_ebot_sce_kernel_import_resolves() {
        use ps4_loader::container;
        use ps4_loader::image::ParsedImage;

        let path = "/home/mikolaj/PS4/CUSA03173/eboot.bin";
        let Ok(raw) = std::fs::read(path) else {
            eprintln!("skip: {path} not present");
            return;
        };
        let Ok(container) = container::open(raw) else {
            eprintln!("skip: eboot container not openable (likely encrypted; no decryption here)");
            return;
        };
        let Ok(parsed) = ParsedImage::parse(container) else {
            eprintln!("skip: retail image not parseable without decryption");
            return;
        };

        let modules = install_hle();

        // Every import's symbol_name is the raw NID (SceDynamic). Any sceKernel*
        // NID whose SyscallId we implement must resolve to a stub.
        let mut resolved = None;
        for imp in &parsed.imports {
            let Some(id) = SyscallId::from_nid(&imp.symbol_name) else {
                continue;
            };
            if id.name().starts_with("sceKernel")
                && let Some(addr) = modules.resolve_symbol(&imp.symbol_name)
            {
                resolved = Some((imp.symbol_name.clone(), id.name(), addr));
                break;
            }
        }

        let (nid, name, addr) = resolved.expect("at least one sceKernel* import resolves");
        eprintln!("resolved retail import NID {nid} ({name}) -> stub {addr:#x}");
    }
}
