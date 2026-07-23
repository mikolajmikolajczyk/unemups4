//! Dynamic linker: maps images at an allocated base and applies their relocations.
//!
//! The relocation kinds this file interprets — `RelocationKind::{Relative, Absolute64,
//! JumpSlot, GlobDat, DtpMod64, DtpOff64, TpOff64}` — are the System V x86-64 psABI
//! relocation types. Their numbers and one-line semantics are witnessed by FreeBSD 9.0
//! `sys/sys/elf_common.h` (Orbis OS's ELF base): `R_X86_64_64` (1, "Add 64 bit symbol
//! value" → S + A), `R_X86_64_GLOB_DAT` (6) / `R_X86_64_JMP_SLOT` (7, "Set GOT entry to
//! data/code address" → S), `R_X86_64_RELATIVE` (8, "Add load address of shared object"
//! → B + A), `R_X86_64_DTPMOD64` (16, "ID of module containing symbol"),
//! `R_X86_64_DTPOFF64` (17, "Offset in TLS block"), `R_X86_64_TPOFF64` (18, "Offset in
//! static TLS block"). Pinned by `reloc_kinds_match_x86_64_psabi_oracle` below. The
//! two-phase map/relocate split, the lazy-stub layout, and the missing-symbol marker are
//! this emulator's own loading design, not hardware facts.

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

/// Low-bit mask of the loader's 4 KB mapping granularity — the same page size the
/// segment map used inline (`align_mask = 0xFFF`) before segment spans were paged.
const SEGMENT_PAGE_MASK: u64 = 0xFFF;

/// Bytes `init_stubs` maps for the Lazy_Stubs region (64 KiB). Every unresolved import
/// consumes one `LAZY_STUB_SIZE`-byte slot from it, so the region holds
/// `LAZY_STUBS_SIZE / LAZY_STUB_SIZE` = 2048 stubs. `relocate_image` bounds the cursor
/// against this before emitting a stub: the identity arena backs every in-range address
/// regardless of VMA, so an unchecked advance would write a stub past the region into
/// adjacent arena bytes (see the guard in `relocate_image`).
const LAZY_STUBS_SIZE: u64 = 0x10000;

/// Bytes each lazy stub reserves — the `stubs_cursor` advance. The stub body is 11 bytes
/// (`MOV R10,RCX; MOV EAX,imm32; SYSCALL; RET`); the rest is slack.
const LAZY_STUB_SIZE: u64 = 32;

/// The bare NID hash of an import symbol name, for the NID→name table lookup.
///
/// An SCE import's `symbol_name` is `NID#library#module` (e.g. `bzQExy189ZI#W#W`), where
/// the leading segment is the base64 NID hash and the `#…` suffix encodes which import
/// library/module it comes from. The generated NID table (`ps4_syscalls::SyscallId::
/// from_nid`) is keyed by the bare hash, so the suffix must be stripped or every suffixed
/// import misses the table and logs a raw hash even when the name is known (`_init_env`
/// was hashing to `bzQExy189ZI` but the import carried `bzQExy189ZI#W#W`). Names with no
/// `#` (plain-ELF homebrew symbols) pass through unchanged.
fn nid_key(symbol_name: &str) -> &str {
    symbol_name.split('#').next().unwrap_or(symbol_name)
}

/// Human-readable label for an unresolved import, so the log — and the eventual
/// missing-symbol FATAL, which echoes the registered string — read
/// `_init_env [NID bzQExy189ZI]` instead of a bare hash. Resolve the NID to a name
/// where our map knows it; otherwise annotate the raw NID with the library it was
/// imported from (the only remaining handle on an unknown NID), else the bare NID.
/// Only ~17% of a real title's imports are known (libc/pthread/sce*); the rest stay
/// raw NIDs until the name data grows (task-113.2 diagnostics).
fn missing_symbol_display_name(sym_name: &str, import_libs: &HashMap<String, String>) -> String {
    ps4_syscalls::SyscallId::from_nid(nid_key(sym_name))
        .map(|s| s.as_str())
        .filter(|n| !n.is_empty() && *n != "Unknown")
        .map(|n| format!("{n} [NID {sym_name}]"))
        .unwrap_or_else(|| match import_libs.get(sym_name) {
            // Unknown name: say where it came from. That is the only handle left on it.
            Some(lib) => format!("{sym_name} (unnamed NID, from {lib})"),
            None => sym_name.to_string(),
        })
}

/// Map every 4 KB page in `[start, end)` that is not already mapped, skipping the pages an
/// earlier segment already claimed (an abutting PT_LOAD may share a boundary page — see
/// [`DynamicLinker::map_image`]). Contiguous free pages are coalesced into one `map` call,
/// so a fully-free span (the PS4/homebrew case) is a single map identical to before.
fn map_free_pages(
    memory: &mut dyn VirtualMemoryManager,
    start: u64,
    end: u64,
    prot: MemoryProtection,
    name: &str,
) -> Result<(), LoaderError> {
    let page = SEGMENT_PAGE_MASK + 1;
    let mut addr = start;
    while addr < end {
        if memory.is_memory_free(addr, page as usize) {
            let run_start = addr;
            while addr < end && memory.is_memory_free(addr, page as usize) {
                addr += page;
            }
            memory
                .map(run_start, (addr - run_start) as usize, prot, Some(name))
                .map_err(|e| LoaderError::Memory(e.to_string()))?;
        } else {
            addr += page;
        }
    }
    Ok(())
}

/// The page-aligned `[start, end)` span a segment at `vaddr` occupies — exactly the range
/// [`map_free_pages`] maps for it. Returns `None` when the span math overflows `u64`, which
/// a corrupt/hostile PT_LOAD can trigger via a huge `p_vaddr`, `p_filesz`, or `p_memsz`
/// (guest-controlled): checked arithmetic here turns that into a clean rejection instead of a
/// debug panic or a release wrap that would make `end < start`. Both `map_image` (to reserve
/// the span) and `relocate_image` (to restore protection over the same span) go through this
/// so the mapped and protected ranges are provably identical.
fn segment_span(vaddr: u64, data_len: usize, bss_size: usize) -> Option<(u64, u64)> {
    let end = vaddr
        .checked_add(data_len as u64)?
        .checked_add(bss_size as u64)?
        .checked_add(SEGMENT_PAGE_MASK)?;
    Some((vaddr & !SEGMENT_PAGE_MASK, end & !SEGMENT_PAGE_MASK))
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
                LAZY_STUBS_SIZE as usize, // 64KB — the ceiling relocate_image bounds against
                MemoryProtection::READ | MemoryProtection::WRITE | MemoryProtection::EXEC,
                Some("Lazy_Stubs"),
            )
            .map_err(|e| e.to_string())?;

        let mut state = self.state.lock().unwrap();
        state.stubs_base = addr;
        tracing::info!("Linker: Allocated Lazy Stubs heap at {:#x}", addr);
        Ok(())
    }

    /// PHASE 1 of loading: map ONE image at a freshly-allocated base and REGISTER it —
    /// base, entry point and base-shifted exports — without applying a single relocation.
    ///
    /// Split from relocation ([`Self::relocate_image`]) because eager binding cannot be
    /// ordered around a dependency CYCLE, and real ones exist: a title's `libSceLibcInternal`
    /// and `libSceFios2` import each other, so under load-and-relocate-in-one-step whichever
    /// went first had the other's imports written as missing-symbol stubs — permanently, with
    /// the exporting module sitting loaded moments later (task-29). Mapping every module in
    /// the graph before relocating any of them makes the cycle a non-event: by the time a
    /// relocation looks up a symbol, every module's exports are registered.
    ///
    /// Base comes from `ModuleManager::allocator` (the allocator starts at
    /// `0x400_000`, so the FIRST image still lands there — no regression) rather than
    /// a hardcoded constant; the module id comes from `get_next_handle`, so
    /// DTPMOD/TLS relocations name the real module instead of the literal `1`.
    pub fn map_image(
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
        for lib in image.libraries().map_err(LoaderError::Io)? {
            info!("Linker: Required Library: {}", lib);
        }
        // Logged separately because they are a different namespace and the difference is
        // exactly what broke dependency ordering (task-29): a needed MODULE names a file.
        for m in image.needed_modules().map_err(LoaderError::Io)? {
            info!("Linker: Needed Module: {}", m);
        }
        let segments = image.segments().map_err(LoaderError::Io)?;

        for seg in &segments {
            // `base_addr` is allocator-assigned, but `seg.offset` is the guest-controlled
            // p_vaddr — guard the add so a hostile offset can't panic (debug) or wrap (release)
            // before the map below.
            let vaddr = base_addr.checked_add(seg.offset).ok_or_else(|| {
                LoaderError::Format(format!(
                    "segment offset {:#x} overflows module base {:#x}",
                    seg.offset, base_addr
                ))
            })?;

            let map_prot = seg.protection | MemoryProtection::WRITE;

            // Map the page-aligned span this segment occupies, but only the pages an
            // earlier segment has not already claimed. Real ELF PT_LOADs may legitimately
            // abut inside a boundary page — PS5 executables pack segments tight, so one
            // segment can begin mid-page where the previous ended (e.g. Dead Cells:
            // ph[7] ends at 0x226cdc0 and ph[8] starts at 0x226cdc0, sharing page
            // 0x226c000). The VMA backend tracks whole-page mappings, so mapping ph[8]'s
            // full page range would collide on that shared page. Mapping only the free
            // sub-ranges mirrors how a real loader lets adjacent PT_LOADs share a boundary
            // page. PS4/homebrew segments sit page-aligned with gaps, so each span is fully
            // free and this maps it in a single call, identical to before. Segment bytes
            // and bss are written below regardless — the identity arena is already backed,
            // so `map` is VMA bookkeeping, not the write seam.
            let (span_start, span_end) = segment_span(vaddr, seg.data.len(), seg.bss_size)
                .ok_or_else(|| {
                    LoaderError::Format(format!(
                        "segment at vaddr {vaddr:#x} has an out-of-range span \
                         (data {:#x}, bss {:#x})",
                        seg.data.len(),
                        seg.bss_size
                    ))
                })?;
            map_free_pages(memory, span_start, span_end, map_prot, name)?;

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

        let entry_point = base_addr + image.entry_point().map_err(LoaderError::Io)?;
        let sections = image.sections().map_err(LoaderError::Io)?;
        let imports = image.imports().map_err(LoaderError::Io)?;

        // exports, shifted by base_addr
        let raw_exports = image.exports().map_err(LoaderError::Io)?;
        let mut abs_exports = HashMap::new();
        for (sym_name, offset) in raw_exports {
            abs_exports.insert(sym_name, base_addr + offset);
        }

        manager.register_module(Module {
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
        });

        Ok(module_id)
    }

    /// PHASE 2 of loading: apply `image`'s relocations against the modules registered so far.
    ///
    /// Call only after every module in the dependency graph has been through
    /// [`Self::map_image`] — that is the whole point of the split. `handle` names the module
    /// mapped from this same image, and supplies the base every relocation is applied at.
    pub fn relocate_image(
        &self,
        manager: &mut ModuleManager,
        memory: &mut dyn VirtualMemoryManager,
        image: &impl ExecutableImage,
        handle: ModuleHandle,
    ) -> Result<(), LoaderError> {
        let (base_addr, name) = match manager.modules.get(&handle) {
            Some(m) => (m.base_addr, m.name.clone()),
            None => {
                return Err(LoaderError::Relocation(format!(
                    "relocate_image: module handle {handle} is not registered"
                )));
            }
        };

        let relocs = image.relocations().map_err(LoaderError::Io)?;
        let segments = image.segments().map_err(LoaderError::Io)?;
        let module_id = handle;
        let mut applied_relocs = 0;

        // NID -> the library it is imported FROM. A NID is a one-way hash, so an import whose
        // name is not in our table can never be recovered from the hash — but the SCE import
        // record also names the library, and "an unknown symbol from libScePlayGo" narrows the
        // search to one API where a bare hash narrows it to nothing (task-113.2).
        let import_libs: HashMap<String, String> = image
            .imports()
            .map_err(LoaderError::Io)?
            .into_iter()
            .filter(|i| !i.lib_name.is_empty())
            .map(|i| (i.symbol_name, i.lib_name))
            .collect();

        for reloc in relocs {
            let target_vaddr = base_addr + reloc.offset;

            match reloc.kind {
                // R_X86_64_RELATIVE (FBSD elf_common.h type 8, "Add load address of
                // shared object"): B + A, no symbol.
                RelocationKind::Relative => {
                    let val = (base_addr as i64 + reloc.addend) as usize;

                    memory
                        .write::<usize>(target_vaddr, val)
                        .map_err(|e| LoaderError::Relocation(e.to_string()))?;

                    applied_relocs += 1;
                }

                // R_X86_64_64 (FBSD elf_common.h type 1, "Add 64 bit symbol value"):
                // S + A. Resolve the actual symbol value, don't assume it sits at the
                // load base (previously this was folded into the RELATIVE arm, dropping S).
                RelocationKind::Absolute64 => {
                    let s = if let Some(v) = reloc.symbol_value {
                        base_addr + v // defined in this module
                    } else if let Some(addr) = reloc
                        .symbol_name
                        .as_deref()
                        .and_then(|n| manager.resolve_symbol(n))
                    {
                        addr // imported / cross-module symbol, incl. HLE stubs
                    } else if let Some(sym_name) = &reloc.symbol_name {
                        // Named import with no exporter. Folding this to s = 0 used to write a
                        // ~null function pointer: when the guest later CALLs through it the
                        // process takes an unnamed SIGSEGV — while the SAME missing import
                        // reached via a GOT slot (JumpSlot/GlobDat) gets a named missing-symbol
                        // stub. Route it through that stub so a stored-then-called pointer traps
                        // as a named FATAL, turning a nameable HLE gap into a diagnosable wall
                        // instead of a mystery crash (task-113.2).
                        let display_name = missing_symbol_display_name(sym_name, &import_libs);
                        self.alloc_missing_stub(memory, &display_name)?
                    } else {
                        // No symbol name (pure addend / unnamed): unchanged from before.
                        0
                    };

                    let val = (s as i64 + reloc.addend) as usize;

                    memory
                        .write::<usize>(target_vaddr, val)
                        .map_err(|e| LoaderError::Relocation(e.to_string()))?;

                    applied_relocs += 1;
                }

                // R_X86_64_JMP_SLOT (FBSD elf_common.h type 7, "Set GOT entry to code
                // address") / R_X86_64_GLOB_DAT (type 6, "Set GOT entry to data address"):
                // value = S — point the GOT slot at the resolved symbol.
                RelocationKind::JumpSlot | RelocationKind::GlobDat => {
                    if let Some(sym_name) = &reloc.symbol_name {
                        if let Some(target_addr) = manager.resolve_symbol(sym_name) {
                            // DIAGNOSTIC (task-113.2): a symbol can resolve to a REAL export
                            // whose body is an Orbis "unimplemented" trap. Observed in the
                            // dumped retail libc (DUMP oracle, e.g. CUSA11302/eboot.bin):
                            // functions the module does not provide are compiled as
                            // `push rbp; mov rbp,rsp; int 0x44` — the trailing bytes are the
                            // x86 `INT imm8` encoding `0xCD 0x44` (opcode 0xCD, vector 0x44).
                            // Such an import links "successfully" (so it never hits the
                            // missing-symbol lazy-stub path below) but faults as a bare
                            // "Exception vector 68" the moment the guest CALLS it, with no name.
                            // Peek the target and, if it is that stub, name the culprit here so
                            // the wall is a named HLE gap instead of a mystery SIGILL.
                            if let Ok(head) = memory.read_bytes(target_addr, 6)
                                && head.len() == 6
                                && head[4] == 0xCD
                                && head[5] == 0x44
                            {
                                let display_name =
                                    ps4_syscalls::SyscallId::from_nid(nid_key(sym_name))
                                        .map(|s| s.as_str())
                                        .filter(|n| !n.is_empty() && *n != "Unknown")
                                        .map(|n| format!("{n} [NID {sym_name}]"))
                                        .unwrap_or_else(|| sym_name.clone());
                                tracing::warn!(
                                    "[LINKER] Import resolved to an Orbis unimplemented-stub \
                                     (int 0x44) at {:#x}: {} — calling it will trap. Needs an HLE.",
                                    target_addr,
                                    display_name
                                );
                            }
                            memory
                                .write::<usize>(target_vaddr, target_addr as usize)
                                .map_err(|e| LoaderError::Memory(String::from(e)))?;
                            applied_relocs += 1;
                        } else {
                            // No exporter for this import. Name it (task-113.2 diagnostics),
                            // emit a lazy missing-symbol stub, and point the GOT slot at it so
                            // a CALL through the slot traps as a named FATAL, not a bare fault.
                            let display_name = missing_symbol_display_name(sym_name, &import_libs);
                            let stub_addr = self.alloc_missing_stub(memory, &display_name)?;

                            // point GOT at the stub
                            memory
                                .write::<usize>(target_vaddr, stub_addr as usize)
                                .map_err(|e| LoaderError::Memory(String::from(e)))?;
                        }
                    }
                }
                RelocationKind::DtpMod64 => {
                    // R_X86_64_DTPMOD64 (FBSD elf_common.h type 16, "ID of module
                    // containing symbol"): write the module id that owns this TLS block.
                    // Use the real id assigned by ModuleManager (was a hardcoded `1`); for
                    // the main executable this is still id 1, so no behavior change for the
                    // single-module corpus.
                    let val = module_id as usize;
                    memory
                        .write::<usize>(target_vaddr, val)
                        .map_err(|e| LoaderError::Relocation(e.to_string()))?;
                    applied_relocs += 1;
                }

                RelocationKind::DtpOff64 => {
                    // R_X86_64_DTPOFF64 (FBSD elf_common.h type 17, "Offset in TLS
                    // block"). Full per-symbol TLS offset resolution is deferred; today,
                    // as before, we write the module id as a placeholder (was the same
                    // hardcoded `1`, now sourced from the real handle).
                    let val = module_id as usize;
                    memory
                        .write::<usize>(target_vaddr, val)
                        .map_err(|e| LoaderError::Relocation(e.to_string()))?;
                    applied_relocs += 1;
                }

                RelocationKind::TpOff64 => {
                    // R_X86_64_TPOFF64 (FBSD elf_common.h type 18, "Offset in static TLS
                    // block").
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

        // strip the temporary WRITE flag back off code/rodata segments. Protect the SAME
        // page-aligned span map_free_pages mapped (via segment_span), not the raw seg vaddr:
        // the VMA backend keys its allocation table by the page-aligned mapping start, so a
        // non-page-aligned seg.offset (PS5-packed images abut segments mid-page) protected at
        // the raw vaddr misses the lookup and leaves the temporary WRITE flag un-stripped. The
        // span overflow was already rejected at map time, so a `None` here means nothing was
        // mapped to restore — skip it.
        for seg in &segments {
            let Some(vaddr) = base_addr.checked_add(seg.offset) else {
                continue;
            };
            let Some((span_start, span_end)) = segment_span(vaddr, seg.data.len(), seg.bss_size)
            else {
                continue;
            };
            let map_size = (span_end - span_start) as usize;

            // restore original protection unless it was RW to begin with
            if seg.protection != (MemoryProtection::READ | MemoryProtection::WRITE) {
                memory
                    .protect(span_start, map_size, seg.protection)
                    .map_err(|e| LoaderError::Memory(e.to_string()))?;
            }
        }

        tracing::debug!("Linker: '{}' applied {} relocations", name, applied_relocs);
        Ok(())
    }

    /// Allocate a lazy stub for an unresolved import and return its guest address.
    /// The stub is `MOV R10, RCX; MOV EAX, magic_id; SYSCALL; RET`. The magic id carries
    /// the 0xC000_0000 missing-symbol marker, so when the guest reaches the stub the SYSCALL
    /// traps into `rust_syscall_handler`, which sees the marker and reports a named
    /// "[FATAL ERROR] ... missing symbol" instead of the guest faulting on a bare address.
    /// The `MOV R10, RCX` preserves the 4th call-ABI arg (RCX) before SYSCALL clobbers it.
    /// `display_name` is registered so the stub is nameable from its address alone. Bounds
    /// the cursor against the fixed Lazy_Stubs region first: the identity arena backs every
    /// in-range address regardless of VMA, so an out-of-range `write_bytes` would SUCCEED,
    /// silently overwriting whatever adjacent arena bytes back it — a clean error beats
    /// corruption. A retail title has thousands of unresolved imports (only ~17% of NIDs are
    /// known), so this ceiling is reachable.
    fn alloc_missing_stub(
        &self,
        memory: &mut dyn VirtualMemoryManager,
        display_name: &str,
    ) -> Result<u64, LoaderError> {
        let mut state = self.state.lock().unwrap();

        if state.stubs_base == 0 {
            return Err(LoaderError::Relocation(
                "Linker not initialized (stubs heap missing)".into(),
            ));
        }

        if state.stubs_cursor + LAZY_STUB_SIZE > LAZY_STUBS_SIZE {
            return Err(LoaderError::Relocation(format!(
                "Lazy_Stubs region exhausted after {} stubs ({:#x} bytes): \
                 cannot allocate a stub for {display_name}",
                LAZY_STUBS_SIZE / LAZY_STUB_SIZE,
                LAZY_STUBS_SIZE
            )));
        }

        let magic_id = ps4_core::debug::register_missing_symbol(display_name);

        let stub_addr = state.stubs_base + state.stubs_cursor;
        state.stubs_cursor += LAZY_STUB_SIZE;

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

        // Name the lazy stub too, so a pointer at an unresolved import is nameable from an
        // address alone — the same way an implemented one is (task-113.2).
        ps4_core::debug::register_stub_symbol(stub_addr, "missing", display_name);

        info!(
            "[LINKER] Stubbed missing: {} (ID: {:#x})",
            display_name, magic_id
        );

        Ok(stub_addr)
    }

    /// Map and relocate ONE image in a single step, for callers with no dependency graph to
    /// order — the main executable and the loader's own tests. Anything loading a set of
    /// interdependent modules must use [`Self::map_image`] over the whole set first, then
    /// [`Self::relocate_image`]; see the cycle note on `map_image`.
    pub fn load_image(
        &self,
        manager: &mut ModuleManager,
        memory: &mut dyn VirtualMemoryManager,
        image: &impl ExecutableImage,
        name: &str,
    ) -> Result<ModuleHandle, LoaderError> {
        let handle = self.map_image(manager, memory, image, name)?;
        self.relocate_image(manager, memory, image, handle)?;
        Ok(handle)
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

    /// An SCE import symbol name is `NID#library#module`; the NID table is keyed by the
    /// bare hash. `nid_key` must return that leading segment so a suffixed import resolves
    /// to its name (this was `_init_env` logging as a raw `bzQExy189ZI#W#W`). A plain
    /// homebrew symbol name (no `#`) is returned unchanged.
    #[test]
    fn nid_key_strips_library_module_suffix() {
        assert_eq!(nid_key("bzQExy189ZI#W#W"), "bzQExy189ZI");
        assert_eq!(nid_key("1jfXLRVzisc#A#B"), "1jfXLRVzisc");
        assert_eq!(nid_key("plain_symbol"), "plain_symbol");
        assert_eq!(nid_key(""), "");
    }

    /// `segment_span` must page-align both ends (so `map_image`'s map and `relocate_image`'s
    /// protect target the identical range the VMA backend tracks) and reject a span whose math
    /// overflows `u64` — a corrupt/hostile PT_LOAD size — instead of panicking (debug) or
    /// wrapping to `end < start` (release).
    #[test]
    fn segment_span_page_aligns_and_rejects_overflow() {
        // A non-page-aligned vaddr rounds start DOWN and end UP to page boundaries.
        // 0x401dc0 & !0xfff = 0x401000; 0x401dc0 + 0x40 + 0 + 0xfff, aligned down = 0x402000.
        assert_eq!(segment_span(0x401dc0, 0x40, 0), Some((0x401000, 0x402000)));
        // A page-aligned vaddr with a page of data stays a single page span.
        assert_eq!(
            segment_span(0x400000, 0x1000, 0),
            Some((0x400000, 0x401000))
        );
        // A hostile p_vaddr near the top of the address space overflows the span: rejected.
        assert_eq!(segment_span(0xFFFF_FFFF_FFFF_F000, 0x2000, 0), None);
        assert_eq!(segment_span(0x1000, usize::MAX, usize::MAX), None);
    }

    /// Every relocation kind this linker's `match` arms interpret is a System V x86-64 psABI
    /// relocation type, whose number + one-line semantic is defined in
    /// FreeBSD 9.0 `sys/sys/elf_common.h` (Orbis OS's ELF base). The right-hand literals are
    /// the FBSD `#define R_X86_64_*` values; this test fails if the mapping this file relies
    /// on drifts from those definitions.
    #[test]
    fn reloc_kinds_match_x86_64_psabi_oracle() {
        // FBSD elf_common.h `#define R_X86_64_*` number for each kind we handle.
        fn elf_type(kind: RelocationKind) -> u32 {
            match kind {
                RelocationKind::Absolute64 => 1, // R_X86_64_64        "Add 64 bit symbol value"
                RelocationKind::GlobDat => 6, // R_X86_64_GLOB_DAT  "Set GOT entry to data address"
                RelocationKind::JumpSlot => 7, // R_X86_64_JMP_SLOT  "Set GOT entry to code address"
                RelocationKind::Relative => 8, // R_X86_64_RELATIVE  "Add load address of shared object"
                RelocationKind::DtpMod64 => 16, // R_X86_64_DTPMOD64  "ID of module containing symbol"
                RelocationKind::DtpOff64 => 17, // R_X86_64_DTPOFF64  "Offset in TLS block"
                RelocationKind::TpOff64 => 18,  // R_X86_64_TPOFF64   "Offset in static TLS block"
                other => panic!("{other:?} is not a relocation kind this linker interprets"),
            }
        }

        // (kind we branch on, FBSD elf_common.h R_X86_64_* number). Literals mirror the
        // `#define`s so an edit that diverges from the FBSD oracle fails here.
        let oracle: [(RelocationKind, u32); 7] = [
            (RelocationKind::Absolute64, 1),
            (RelocationKind::GlobDat, 6),
            (RelocationKind::JumpSlot, 7),
            (RelocationKind::Relative, 8),
            (RelocationKind::DtpMod64, 16),
            (RelocationKind::DtpOff64, 17),
            (RelocationKind::TpOff64, 18),
        ];
        for (kind, num) in oracle {
            assert_eq!(
                elf_type(kind),
                num,
                "{kind:?} must map to FBSD elf_common.h R_X86_64 type {num}"
            );
        }
    }

    /// In-memory VMM for loader unit tests: identity-mapped, each `map` owns a boxed
    /// buffer; `get_host_ptr` finds the region containing an address. No real mmap,
    /// no Vulkan — pure `cargo test`-able (doc-3 §1: everything but the actual
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

    /// Wall 2 (task-233): abutting PT_LOADs that share a boundary page must map. PS5
    /// executables pack segments tight, so one begins mid-page where the previous ended
    /// (Dead Cells: ph[7] ends at 0x226cdc0, ph[8] starts there, sharing page 0x226c000).
    /// `map_free_pages` must skip the already-mapped shared page and map only the free
    /// delta — a naive full-range map would hit the backend's per-page collision check.
    #[test]
    fn map_free_pages_skips_pages_an_earlier_segment_claimed() {
        /// Collision-aware page tracker mirroring the real VMA backend: `map` rejects any
        /// range touching an already-mapped page; `is_memory_free` reports the same.
        #[derive(Default)]
        struct PagedMemory {
            pages: std::collections::BTreeSet<u64>,
        }
        const PG: u64 = 0x1000;
        impl VirtualMemoryManager for PagedMemory {
            fn map(
                &mut self,
                addr: u64,
                size: usize,
                _prot: MemoryProtection,
                _name: Option<&str>,
            ) -> Result<u64, &'static str> {
                let end = addr + size as u64;
                let mut p = addr & !(PG - 1);
                while p < end {
                    if !self.pages.insert(p) {
                        return Err("Memory collision");
                    }
                    p += PG;
                }
                Ok(addr)
            }
            fn unmap(&mut self, _a: u64, _s: usize) -> Result<(), &'static str> {
                Ok(())
            }
            fn protect(
                &mut self,
                _a: u64,
                _s: usize,
                _p: MemoryProtection,
            ) -> Result<(), &'static str> {
                Ok(())
            }
            unsafe fn get_host_ptr(&self, _a: u64) -> Option<*mut u8> {
                None
            }
            fn find_free_region(&mut self, _s: usize) -> u64 {
                0
            }
            fn is_memory_free(&self, addr: u64, size: usize) -> bool {
                let end = addr + size as u64;
                let mut p = addr & !(PG - 1);
                while p < end {
                    if self.pages.contains(&p) {
                        return false;
                    }
                    p += PG;
                }
                true
            }
        }

        let mut mem = PagedMemory::default();
        let prot = MemoryProtection::READ | MemoryProtection::WRITE;
        // Segment A: span [0x400000, 0x402000) — occupies pages 0x400000, 0x401000.
        map_free_pages(&mut mem, 0x400000, 0x402000, prot, "segA").expect("segA maps");
        // Segment B abuts inside page 0x401000 and extends into 0x402000. Its page-aligned
        // span [0x401000, 0x403000) overlaps A's last page; map_free_pages must map only
        // the free page 0x402000, not collide on the shared 0x401000.
        map_free_pages(&mut mem, 0x401000, 0x403000, prot, "segB")
            .expect("abutting segment must map the free page delta without collision");
        assert!(
            mem.pages.contains(&0x402000),
            "the free page must be mapped"
        );
        assert_eq!(
            mem.pages.len(),
            3,
            "exactly pages 0x400000/0x401000/0x402000 mapped — the shared page once"
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

    /// An Absolute64 (R_X86_64_64) reloc against an UNRESOLVED named import must emit a
    /// missing-symbol stub and point the pointer at it — not fold to s = 0 and write a
    /// ~null pointer that CALLs into a mystery SIGSEGV. This mirrors what the GOT path
    /// (JumpSlot/GlobDat) already does, so a stored-then-called function pointer for an
    /// unimplemented sce* API traps as a named FATAL (task-113.2).
    #[test]
    fn absolute64_unresolved_import_emits_lazy_stub() {
        let linker = DynamicLinker::new();
        let mut mgr = ModuleManager::new();
        let mut mem = MockMemory::default();

        linker.init_stubs(&mut mem).expect("stubs heap must init");

        let mut img = FakeImage::new(0x1000);
        img.relocations = vec![Relocation {
            offset: 0x18,
            kind: RelocationKind::Absolute64,
            symbol_index: None,
            addend: 0,
            symbol_name: Some("sceNeverDefined".to_string()),
            symbol_value: None, // imported, not defined here
        }];
        let id = linker
            .load_image(&mut mgr, &mut mem, &img, "eboot.bin")
            .unwrap();
        let base = mgr.modules.get(&id).unwrap().base_addr;

        // The pointer now targets a stub address (non-zero, in the stubs heap) — the bug
        // wrote 0 (the addend) here.
        let stub_addr: usize = mem.read(base + 0x18).unwrap();
        assert_ne!(
            stub_addr, 0,
            "Absolute64 against an unresolved import must point at an emitted stub, not null"
        );

        // The stub is the missing-symbol trampoline: MOV R10,RCX (49 89 CA), MOV EAX,imm32
        // (B8 ..), SYSCALL (0F 05), RET (C3).
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

    /// The Lazy_Stubs region is a fixed `LAZY_STUBS_SIZE` (64 KiB); each stub reserves
    /// `LAZY_STUB_SIZE` bytes, so it holds exactly `LAZY_STUBS_SIZE / LAZY_STUB_SIZE`
    /// (2048) stubs. Emitting one more must return `LoaderError::Relocation`, NOT advance
    /// the cursor past the region — the identity arena writes to any in-range address
    /// regardless of VMA, so an unchecked overrun would silently clobber adjacent arena
    /// bytes. A retail title has thousands of unresolved imports, so this is reachable.
    #[test]
    fn lazy_stubs_region_exhaustion_errors_not_overruns() {
        let linker = DynamicLinker::new();
        let mut mgr = ModuleManager::new();
        let mut mem = MockMemory::default();
        linker.init_stubs(&mut mem).expect("stubs heap must init");

        // One more unresolved import than the region can hold. Each JumpSlot writes a
        // usize GOT slot at `offset`; keep them 8 bytes apart and size the segment to fit.
        let capacity = (LAZY_STUBS_SIZE / LAZY_STUB_SIZE) as usize; // 2048
        let n = capacity + 1;
        let mut img = FakeImage::new(n * 8 + 0x1000);
        img.relocations = (0..n)
            .map(|i| Relocation {
                offset: (i * 8) as u64,
                kind: RelocationKind::JumpSlot,
                symbol_index: None,
                addend: 0,
                symbol_name: Some(format!("sceUnresolved{i}")),
                symbol_value: None,
            })
            .collect();

        let err = linker
            .load_image(&mut mgr, &mut mem, &img, "eboot.bin")
            .expect_err("emitting more stubs than the region holds must error");
        assert!(
            matches!(&err, LoaderError::Relocation(m) if m.contains("Lazy_Stubs region exhausted")),
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
