//! Env-gated dump of the loaded, post-relocation module images to disk, so any loaded
//! guest module can be pulled out for offline disassembly/decompilation in objdump,
//! Ghidra, or radare2 (task-113.2). This replaces ad-hoc one-time disassembly for
//! guest-side crash investigation: a fault reported as `eboot.bin +0x16de90` is
//! disassembled by opening the dump at file offset `0x16de90`.
//!
//! Gated on [`DUMP_ENV`] (`UNEMUPS4_DUMP_MODULES=<dir>`). When set, after the modules are
//! loaded and relocated, [`maybe_dump_modules`] writes, for each loaded non-HLE module:
//!
//! - `<dir>/<name>.bin` — the loaded segment image, i.e. the guest bytes at
//!   `[base_addr, base_addr + memory_size)` exactly as they will execute (post-relocation).
//!   File offset `N` maps to guest VA `base_addr + N`, so a `<module> +0xN` backtrace frame
//!   lands at file offset `0xN` and `objdump --adjust-vma=<base_addr>` matches byte-for-byte.
//! - `<dir>/<name>.map` — a text sidecar with the load layout, the sections, the full
//!   export table (sorted by address), and a header carrying the exact objdump/Ghidra
//!   recipe so a reader does not reconstruct it.
//!
//! Zero cost when the env var is unset: a single lookup and return, no per-module work.
//!
//! Scope: flat image + `.map` only. A synthetic-ELF wrapper (which would let objdump/Ghidra
//! auto-detect the arch and base) is a deliberate follow-up if the flat form proves awkward,
//! not built here. This is a dump tool, not a disassembler — it pulls no disassembly crate
//! and decompiles nothing in-process; the whole point is to hand the bytes to external tools.

use crate::manager::{Module, ModuleManager};
use ps4_core::memory::VirtualMemoryManager;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use tracing::{info, warn};

/// Env var: set to a directory path to dump every loaded non-HLE module image there.
pub const DUMP_ENV: &str = "UNEMUPS4_DUMP_MODULES";

/// Dump every loaded non-HLE module to `$UNEMUPS4_DUMP_MODULES` if that env var is set.
///
/// The no-op fast path is a single [`std::env::var_os`] lookup — this is safe to call
/// unconditionally on the boot path once loading (and relocation) has completed. Failures
/// (unwritable dir, an unreadable module range) are logged and skipped; a debug dump never
/// aborts the boot.
pub fn maybe_dump_modules(mgr: &ModuleManager, mem: &dyn VirtualMemoryManager) {
    let Some(dir) = std::env::var_os(DUMP_ENV) else {
        return;
    };
    let dir = PathBuf::from(dir);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        warn!(
            "{DUMP_ENV}={}: could not create dump dir: {e}",
            dir.display()
        );
        return;
    }

    // Iterate in a stable (handle) order so a re-run overwrites the same files predictably.
    let mut handles: Vec<_> = mgr.modules.keys().copied().collect();
    handles.sort_unstable();
    let mut dumped = 0usize;
    for h in handles {
        let module = &mgr.modules[&h];
        // HLE stub modules keep base 0 / size 0: they span no address range, so there is
        // nothing to pull out. Skip them (there is no on-disk image behind a SYSCALL stub).
        if module.is_hle || module.memory_size == 0 {
            continue;
        }
        match dump_one(&dir, module, mem) {
            Ok(gaps) => {
                dumped += 1;
                if gaps == 0 {
                    info!(
                        "dumped {} ({:#x} bytes @ {:#x}) to {}",
                        module.name,
                        module.memory_size,
                        module.base_addr,
                        dir.display()
                    );
                } else {
                    warn!(
                        "dumped {} to {} with {gaps} unreadable gap(s) zero-filled (see .map)",
                        module.name,
                        dir.display()
                    );
                }
            }
            Err(e) => warn!("failed to dump {}: {e}", module.name),
        }
    }
    dump_stub_map(&dir);
    info!("{DUMP_ENV}: dumped {dumped} module(s) to {}", dir.display());
}

/// Write `hle_stubs.map`: every import stub address and the symbol behind it.
///
/// The module images alone cannot answer the question that actually comes up when an
/// implemented import misbehaves. A GOT slot in a dumped module holds a stub ADDRESS, and
/// without this file the reader is left with a bare number — which is exactly where the
/// investigation stalled the first time. A missing import announces itself loudly; a wrong
/// one is only ever a pointer into the stub arena.
fn dump_stub_map(dir: &Path) {
    let stubs = ps4_core::debug::stub_symbols();
    if stubs.is_empty() {
        return;
    }
    let mut out = String::from(
        "# unemups4 import-stub map\n\
         #\n\
         # Every emitted import stub, ascending by address. A GOT/PLT slot in a dumped\n\
         # module image that holds one of these addresses is a call into that symbol's HLE\n\
         # handler (or, under `missing!`, into the missing-symbol trap).\n\
         #\n\
         # Each stub occupies 32 bytes, so an address a few bytes past a base is still\n\
         # inside that stub.\n\
         #\n",
    );
    for (addr, name) in &stubs {
        let _ = writeln!(out, "{addr:#018x}  {name}");
    }
    let path = dir.join("hle_stubs.map");
    match std::fs::write(&path, out) {
        Ok(()) => info!("dumped {} import stubs to {}", stubs.len(), path.display()),
        Err(e) => warn!("failed to write {}: {e}", path.display()),
    }
}

/// Write `<name>.bin` and `<name>.map` for one module. Returns the number of unreadable
/// gaps that were zero-filled (0 = clean dump).
fn dump_one(dir: &Path, module: &Module, mem: &dyn VirtualMemoryManager) -> std::io::Result<usize> {
    let (image, gaps) = read_module_image(mem, module.base_addr, module.memory_size);
    let stem = safe_stem(&module.name);
    let bin_name = format!("{stem}.bin");
    let map_name = format!("{stem}.map");
    std::fs::write(dir.join(&bin_name), &image)?;
    std::fs::write(dir.join(&map_name), format_map(module, &bin_name, &gaps))?;
    Ok(gaps.len())
}

/// A filesystem-safe stem for a dump filename derived from a module name.
///
/// Module names for dependencies come from the guest's DT_SCE_NEEDED_MODULE string table
/// (`dynamic.rs` `read_cstr` over guest bytes) and are therefore untrusted: a crafted name
/// like `../../home/user/.bashrc` would make `dir.join(name)` escape the dump directory and
/// clobber an arbitrary writable file. Keep only the final path component (`Path::file_name`
/// drops every `..`/separator prefix and yields `None` for `.`/`..`/trailing-slash names) and
/// fall back to a fixed slug for anything that does not reduce to a plain component.
fn safe_stem(name: &str) -> String {
    match Path::new(name).file_name().and_then(|s| s.to_str()) {
        Some(s) if !s.is_empty() && s != ".." => s.to_string(),
        _ => "module".to_string(),
    }
}

/// Read the loaded segment image `[base, base + size)` through the SMC-safe, range-validated
/// memory seam ([`VirtualMemoryManager::read_bytes_ranged`]) — never a raw host-pointer deref.
///
/// A module whose range is fully mapped reads in one shot. A range that is partly unmapped
/// (a relocated retail module can leave `.bss`-only tails or guard pages backing no VMA)
/// must NOT fault the emulator and must NOT be silently zero-filled as if it were real code:
/// on the one-shot failure we fall back to a page-by-page read, copy every readable page, and
/// zero-fill each unreadable page while recording its `[start, end)` in the returned gap list.
/// The gaps are reported in the `.map` so a reader never mistakes a zero fill for guest bytes.
/// File offsets stay exact (the image is always `size` bytes), so backtrace offsets still map
/// straight to file offsets.
fn read_module_image(
    mem: &dyn VirtualMemoryManager,
    base: u64,
    size: usize,
) -> (Vec<u8>, Vec<(u64, u64)>) {
    // Fast path: the whole range is contiguously mapped (the common case for a loaded
    // module). One validated read, no gaps.
    if let Ok(buf) = mem.read_bytes_ranged(base, size) {
        return (buf, Vec::new());
    }

    // Slow path: some page in the range is unmapped. Probe page-by-page so the dump captures
    // every readable byte and notes the holes rather than failing the whole module.
    const PAGE: usize = 0x1000;
    let mut out = vec![0u8; size];
    let mut gaps: Vec<(u64, u64)> = Vec::new();
    let mut off = 0usize;
    while off < size {
        let chunk = PAGE.min(size - off);
        let addr = base + off as u64;
        match mem.read_bytes_ranged(addr, chunk) {
            Ok(bytes) => out[off..off + chunk].copy_from_slice(&bytes),
            Err(_) => {
                let (g_start, g_end) = (addr, addr + chunk as u64);
                // Coalesce with the previous gap when adjacent, so a large unmapped tail is
                // one line in the `.map` instead of thousands.
                match gaps.last_mut() {
                    Some(prev) if prev.1 == g_start => prev.1 = g_end,
                    _ => gaps.push((g_start, g_end)),
                }
            }
        }
        off += chunk;
    }
    (out, gaps)
}

/// Render one module's `.map` sidecar: the objdump/Ghidra/radare2 recipe header, the load
/// layout, sections, any zero-filled gaps, and the full export table sorted by absolute
/// address. Pure (no I/O), so the format is unit-tested against a synthetic [`Module`].
fn format_map(module: &Module, bin_name: &str, gaps: &[(u64, u64)]) -> String {
    let base = module.base_addr;
    let mut s = String::new();

    // --- Header: the exact recipe, so a reader never reconstructs the --adjust-vma value. ---
    let _ = writeln!(s, "# unemups4 module dump — {}", module.name);
    let _ = writeln!(s, "#");
    let _ = writeln!(
        s,
        "# {bin_name} is a FLAT, post-relocation image of the loaded segment."
    );
    let _ = writeln!(
        s,
        "# File offset N == guest virtual address {base:#x} + N. A backtrace frame reported"
    );
    let _ = writeln!(
        s,
        "# as `{} +0xNNNNN` is at file offset 0xNNNNN in {bin_name}.",
        module.name
    );
    let _ = writeln!(s, "#");
    let _ = writeln!(s, "# objdump (disassemble the whole image):");
    let _ = writeln!(
        s,
        "#   objdump -D -b binary -m i386:x86-64 --adjust-vma={base:#x} {bin_name} | less"
    );
    let _ = writeln!(s, "#");
    let _ = writeln!(s, "# Ghidra: import {bin_name} as a Raw Binary, language");
    let _ = writeln!(
        s,
        "#   x86:LE:64:default, then set the image base to {base:#x}."
    );
    let _ = writeln!(s, "#");
    let _ = writeln!(s, "# radare2:");
    let _ = writeln!(s, "#   r2 -a x86 -b 64 -m {base:#x} {bin_name}");
    let _ = writeln!(s, "#");

    // --- Load layout. ---
    let _ = writeln!(s, "module   {}", module.name);
    let _ = writeln!(s, "path     {}", module.path);
    let _ = writeln!(s, "arch     i386:x86-64");
    let _ = writeln!(s, "base     {base:#x}");
    let _ = writeln!(
        s,
        "size     {:#x} ({} bytes)",
        module.memory_size, module.memory_size
    );
    // entry_point is absolute; show the module-relative offset too (guard the subtraction —
    // a malformed entry below base prints "n/a" rather than wrapping).
    match module.entry_point.checked_sub(base) {
        Some(rel) => {
            let _ = writeln!(
                s,
                "entry    {:#x} (relative +{:#x})",
                module.entry_point, rel
            );
        }
        None => {
            let _ = writeln!(
                s,
                "entry    {:#x} (relative n/a — below base)",
                module.entry_point
            );
        }
    }

    // --- Sections (as recorded from the ELF section headers; vaddr is module-relative for a
    // PIE). Absent for a stripped module — say so instead of an empty block. ---
    let _ = writeln!(s, "\n[sections]");
    if module.sections.is_empty() {
        let _ = writeln!(s, "(none recorded)");
    } else {
        for sec in &module.sections {
            let _ = writeln!(
                s,
                "{:<20} vaddr={:#x} size={:#x} raw={:#x}",
                sec.name, sec.vaddr, sec.size, sec.raw_offset
            );
        }
    }

    // --- Gaps: unreadable ranges dumped as zero fill (empty for a clean dump). ---
    if !gaps.is_empty() {
        let _ = writeln!(
            s,
            "\n[gaps]  (unreadable — zero-filled in the image, NOT guest bytes)"
        );
        for (start, end) in gaps {
            let _ = writeln!(
                s,
                "{start:#x}-{end:#x}  (offset +{:#x}, {:#x} bytes)",
                start - base,
                end - start
            );
        }
    }

    // --- Export table: `<name-or-NID>  <absolute-addr>  +<relative-offset>`, sorted by
    // address (task's required format). Retail modules key exports by NID, recovered to a
    // plain name where the generated table knows it, else printed as the raw NID. ---
    let _ = writeln!(s, "\n[exports]  (name-or-NID  absolute  +relative)");
    let mut exports: Vec<(&String, u64)> = module.exports.iter().map(|(k, &v)| (k, v)).collect();
    exports.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(b.0)));
    if exports.is_empty() {
        let _ = writeln!(s, "(none)");
    } else {
        for (key, addr) in exports {
            let rel = addr.wrapping_sub(base);
            let _ = writeln!(s, "{:<28} {:#018x}  +{:#x}", export_token(key), addr, rel);
        }
    }

    s
}

/// A single whitespace-free token naming an export: the plain name recovered from the NID
/// table where possible, otherwise the raw key (a NID for a retail module, a plain name for
/// homebrew). Kept a single token so the `.map` export column stays parseable. A wrong name
/// is worse than the NID, so an unrecognised key is passed through unchanged, never guessed.
fn export_token(key: &str) -> String {
    match ps4_syscalls::SyscallId::from_nid(key).map(|s| s.as_str()) {
        Some(name) if !name.is_empty() && name != "Unknown" => name.to_string(),
        _ => key.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ps4_core::img::Section;
    use std::collections::HashMap;

    fn module(base: u64, size: usize, exports: &[(&str, u64)]) -> Module {
        let map = exports
            .iter()
            .map(|(k, v)| (k.to_string(), *v))
            .collect::<HashMap<_, _>>();
        let mut m = Module::new(1, "eboot.bin", map);
        m.path = "/app0/eboot.bin".to_string();
        m.base_addr = base;
        m.memory_size = size;
        m.entry_point = base + 0x1000;
        m
    }

    #[test]
    fn map_header_carries_the_exact_objdump_and_ghidra_recipe() {
        let m = module(0x1988000, 0x2807000, &[]);
        let out = format_map(&m, "eboot.bin.bin", &[]);

        // The --adjust-vma value must be the load base, verbatim, so a reader copies it.
        assert!(
            out.contains("--adjust-vma=0x1988000 eboot.bin.bin"),
            "objdump recipe:\n{out}"
        );
        assert!(out.contains("-m i386:x86-64"), "arch in recipe:\n{out}");
        assert!(
            out.contains("set the image base to 0x1988000"),
            "Ghidra recipe:\n{out}"
        );
        assert!(
            out.contains("r2 -a x86 -b 64 -m 0x1988000 eboot.bin.bin"),
            "radare2 recipe:\n{out}"
        );
        // The file-offset == VA - base invariant is stated, and named for THIS module.
        assert!(
            out.contains("`eboot.bin +0xNNNNN` is at file offset 0xNNNNN"),
            "offset invariant:\n{out}"
        );
    }

    #[test]
    fn map_reports_layout_and_relative_entry() {
        let m = module(0x1988000, 0x2807000, &[]);
        let out = format_map(&m, "eboot.bin.bin", &[]);
        assert!(out.contains("base     0x1988000"), "{out}");
        assert!(out.contains("size     0x2807000 (41971712 bytes)"), "{out}");
        // entry_point = base + 0x1000 → relative +0x1000, absolute 0x1989000.
        assert!(
            out.contains("entry    0x1989000 (relative +0x1000)"),
            "{out}"
        );
    }

    #[test]
    fn map_exports_are_sorted_by_address_with_absolute_and_relative() {
        // Deliberately unsorted input; the table must come out address-ordered.
        let m = module(
            0x92c000,
            0x112000,
            &[
                ("memcpy", 0x97b400),
                ("malloc", 0x930000),
                ("strlen", 0x97a9e0),
            ],
        );
        let out = format_map(&m, "eboot.bin.bin", &[]);

        // Find each export line and confirm strictly increasing order.
        let lines: Vec<&str> = out.lines().collect();
        let idx = |needle: &str| lines.iter().position(|l| l.contains(needle)).unwrap();
        let (i_malloc, i_strlen, i_memcpy) = (idx("malloc"), idx("strlen"), idx("memcpy"));
        assert!(
            i_malloc < i_strlen && i_strlen < i_memcpy,
            "exports not address-sorted:\n{out}"
        );

        // Each line carries the absolute address and the base-relative offset.
        assert!(
            out.contains("malloc") && out.contains("0x0000000000930000") && out.contains("+0x4000"),
            "malloc line (abs 0x930000, +0x4000):\n{out}"
        );
        // strlen at 0x97a9e0 → relative +0x4e9e0.
        assert!(out.contains("+0x4e9e0"), "strlen relative offset:\n{out}");
    }

    #[test]
    fn map_recovers_a_known_nid_export_name() {
        let nid = ps4_syscalls::SyscallId::from_symbol_name("sceKernelUsleep")
            .map(|s| s.nid().to_string())
            .filter(|n| !n.is_empty());
        let Some(nid) = nid else {
            return; // no NID table entry in this build; nothing to assert
        };
        let m = module(0x40_0000, 0x1000, &[(nid.as_str(), 0x40_0100)]);
        let out = format_map(&m, "eboot.bin.bin", &[]);
        // The plain name is recovered as a single token; the raw NID is not the column value.
        assert!(
            out.contains("sceKernelUsleep"),
            "recovered NID name:\n{out}"
        );
    }

    #[test]
    fn map_passes_through_an_unrecognised_key_unchanged() {
        let m = module(0x40_0000, 0x1000, &[("ZZZZZZZZZZZ", 0x40_0100)]);
        let out = format_map(&m, "eboot.bin.bin", &[]);
        assert!(
            out.contains("ZZZZZZZZZZZ"),
            "unknown key printed verbatim, never guessed:\n{out}"
        );
    }

    #[test]
    fn map_notes_zero_filled_gaps_and_marks_them_not_guest_bytes() {
        let m = module(0x1000_0000, 0x4000, &[]);
        // A hole at [0x10002000, 0x10003000): offset +0x2000, 0x1000 bytes.
        let out = format_map(&m, "eboot.bin.bin", &[(0x1000_2000, 0x1000_3000)]);
        assert!(out.contains("[gaps]"), "gaps block present:\n{out}");
        assert!(
            out.contains("NOT guest bytes"),
            "gap fill flagged as not real:\n{out}"
        );
        assert!(
            out.contains("0x10002000-0x10003000  (offset +0x2000, 0x1000 bytes)"),
            "gap range/offset/len:\n{out}"
        );
    }

    #[test]
    fn map_with_no_gaps_omits_the_gaps_block() {
        let m = module(0x1000_0000, 0x4000, &[]);
        let out = format_map(&m, "eboot.bin.bin", &[]);
        assert!(
            !out.contains("[gaps]"),
            "clean dump has no gaps block:\n{out}"
        );
    }

    #[test]
    fn safe_stem_strips_path_traversal_from_untrusted_module_names() {
        // A plain name passes through unchanged.
        assert_eq!(safe_stem("libc.prx"), "libc.prx");
        // A traversal name keeps only its final component, so `dir.join` cannot escape.
        assert_eq!(safe_stem("../../../home/mikolaj/.bashrc"), ".bashrc");
        assert_eq!(safe_stem("/etc/passwd"), "passwd");
        assert_eq!(safe_stem("a/b/c.prx"), "c.prx");
        // Names that don't reduce to a plain component fall back to a fixed slug.
        assert_eq!(safe_stem(".."), "module");
        assert_eq!(safe_stem("../.."), "module");
        assert_eq!(safe_stem(""), "module");
        assert_eq!(safe_stem("/"), "module");
    }

    #[test]
    fn map_lists_sections_when_present() {
        let mut m = module(0x40_0000, 0x2000, &[]);
        m.sections = vec![Section {
            name: ".text".to_string(),
            vaddr: 0x1000,
            size: 0x800,
            raw_offset: 0x1000,
        }];
        let out = format_map(&m, "eboot.bin.bin", &[]);
        assert!(out.contains("[sections]"), "{out}");
        assert!(
            out.contains(".text") && out.contains("vaddr=0x1000") && out.contains("size=0x800"),
            "section line:\n{out}"
        );
    }
}
