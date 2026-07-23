//! L2 — parse-once image.
//!
//! goblin's `Elf<'a>` borrows its byte buffer (the self-referential-struct
//! problem), so the clean move is a **single** parse pass into an owned
//! [`ParsedImage`]; every `ExecutableImage` accessor then reads from that cache
//! instead of re-parsing (form A — the trait and every caller are unchanged, only
//! the method bodies). No `goblin::elf::Elf::parse` runs anywhere except once in
//! [`ParsedImage::parse`]; the previous 9-parses-per-load is gone.

use goblin::elf::Elf;
use ps4_core::img::{ExecutableImage, Import, LoadableSegment, Relocation, Section, TlsInfo};
use ps4_core::memory::MemoryProtection;
use std::collections::HashMap;

use crate::container::{Container, ContainerMeta};
use crate::dynamic::{DynamicSource, SceDynamic, StdDynamic};
use crate::linker::LoaderError;

/// `ET_SCE_DYNEXEC` — an ASLR-enabled PS4 main executable. `e_type` value `0xFE10`,
/// listed as `ET_SCE_EXEC_ASLR (default)` in the OpenOrbis OELF spec's `e_type` table
/// (`docs/MD/OELF Specification/PS4 ELF Specification.md`, "Executable Types");
/// `ET_SCE_DYNEXEC` is the common alternate name for the same value. Its dynamic tables
/// use `DT_SCE_*` tags goblin cannot decode — routed to [`SceDynamic`].
const ET_SCE_DYNEXEC: u16 = 0xfe10;
/// `ET_SCE_DYNAMIC` — a retail PS4 shared object (.prx / .sprx). `e_type` value `0xFE18`
/// (OpenOrbis OELF spec `PS4 ELF Specification.md` `e_type` table). Same routing.
const ET_SCE_DYNAMIC: u16 = 0xfe18;
/// `PT_SCE_DYNLIBDATA` — the Sony program-header type carrying the dynamic-link tables
/// (`p_type` value `0x61000000`, OpenOrbis OELF spec `PS4 ELF Specification.md`
/// "Program Header Types"). Its presence marks an SCE image even when `e_type` is a
/// standard value.
const PT_SCE_DYNLIBDATA: u32 = 0x6100_0000;

/// True when the image must be read through [`SceDynamic`] rather than
/// [`StdDynamic`]: a retail SCE `e_type` or a `PT_SCE_DYNLIBDATA` program header.
fn is_sce_image(elf: &Elf) -> bool {
    matches!(elf.header.e_type, ET_SCE_DYNEXEC | ET_SCE_DYNAMIC)
        || elf
            .program_headers
            .iter()
            .any(|ph| ph.p_type == PT_SCE_DYNLIBDATA)
}

/// Everything a load needs, extracted **once** into owned structs. No
/// `goblin::Elf<'a>` is stored (that would re-introduce the self-referential
/// borrow); fields are copied out. The dynamic-linking half (imports / exports /
/// relocations / libraries) is produced by an L3 [`DynamicSource`] at parse time.
#[derive(Debug, Clone)]
pub struct ParsedImage {
    pub entry: u64,
    pub memory_size: usize,
    pub segments: Vec<LoadableSegment>,
    pub sections: Vec<Section>,
    pub tls: Option<TlsInfo>,
    pub libraries: Vec<String>,
    /// Needed MODULE names (`.prx` files) — see [`ps4_core::img::ExecutableImage::needed_modules`].
    pub needed_modules: Vec<String>,
    pub imports: Vec<Import>,
    pub exports: HashMap<String, u64>,
    pub relocations: Vec<Relocation>,
    /// Module-relative virtual address of the `PT_SCE_PROCPARAM` segment
    /// (`SceKernelProcParam`), if present. The eboot carries it; PRX modules and
    /// homebrew do not. `sceKernelGetProcParam` returns `base + this`.
    pub proc_param_vaddr: Option<u64>,
    /// Container metadata carried down from L1.
    pub meta: ContainerMeta,
}

impl ParsedImage {
    /// The single parse: one `goblin` pass over the inner ELF, own-extracted into
    /// [`ParsedImage`]. The dynamic half is read through the L3 [`StdDynamic`]
    /// source (`SceDynamic` will be auto-selected here for retail images).
    pub fn parse(container: Container) -> Result<ParsedImage, LoaderError> {
        let mut raw = container.elf_bytes;
        // Retail PS5 executables carry an `e_shoff` that points past the end of the file:
        // the section-header table (and its string data) is stripped from the console dump,
        // leaving only the `e_shoff`/`e_shnum` fields describing a table that is no longer
        // there. goblin's `Elf::parse` reads the section headers eagerly and fails ("bad
        // offset") on the dangling pointer. Neutralize an out-of-bounds table before parsing
        // so goblin reads program headers only — our loader maps strictly from program
        // headers and uses sections for debugger symbol names alone. Covers the
        // reconstructed-SELF path too, since `container::open` hands reconstructed inner-ELF
        // bytes to this same parse.
        sanitize_out_of_bounds_section_table(&mut raw);
        let elf = Elf::parse(&raw)
            .map_err(|e| LoaderError::Format(format!("Goblin parse error: {e}")))?;

        let entry = elf.entry;

        let segments = extract_segments(&elf, &raw)?;

        // Address SPAN the module occupies, not the sum of segment sizes: segments sit at
        // their p_vaddr (with alignment gaps between them), so the allocator must reserve
        // up to the highest segment end or a later module's base lands inside this one's
        // tail (a "Memory collision" once more than one module is loaded).
        let memory_size = segments
            .iter()
            // `s.offset` is the segment's guest-controlled p_vaddr; a hostile/corrupt value
            // (e.g. p_vaddr = 0xFFFF_FFFF_FFFF_F000 with a few KiB of data+bss) would overflow
            // the sum (debug panic / release wrap to an undersized span). Saturating adds cap it
            // at usize::MAX so the reduction is well-defined; a genuinely out-of-range span is
            // then rejected downstream where the mapper faults on the address.
            .map(|s| {
                (s.offset as usize)
                    .saturating_add(s.data.len())
                    .saturating_add(s.bss_size)
            })
            .max()
            .unwrap_or(0);
        let sections = extract_sections(&elf);
        let tls = extract_tls(&elf, &raw)?;

        // PT_SCE_PROC_PARAM: the SceKernelProcParam blob (heap/libc config the runtime
        // reads via sceKernelGetProcParam). `p_type` value 0x61000001 (OpenOrbis OELF
        // spec `docs/MD/OELF Specification/PS4 ELF Specification.md`, "Program Header
        // Types"; the `.sce_process_param` segment). It sits inside a PT_LOAD, so its
        // bytes are already mapped; we only need its module-relative vaddr to hand back
        // an absolute pointer once the module base is known.
        let proc_param_vaddr = elf
            .program_headers
            .iter()
            .find(|ph| ph.p_type == 0x6100_0001)
            .map(|ph| ph.p_vaddr);

        // Auto-select the L3 dynamic source: retail SCE images (ET_SCE_DYNEXEC /
        // ET_SCE_DYNAMIC, or any PT_SCE_DYNLIBDATA phdr) use DT_SCE_* + NID and
        // must go through SceDynamic; goblin's DT_* path yields nothing for them.
        // Homebrew (standard ELF, no SCE segment) keeps taking StdDynamic
        // byte-for-byte.
        let sce = if is_sce_image(&elf) {
            SceDynamic::new(&elf, &raw)?
        } else {
            None
        };

        let (libraries, needed_modules, imports, exports, relocations) = if let Some(sce) = sce {
            (
                sce.libraries()?,
                sce.needed_modules()?,
                sce.imports()?,
                sce.exports()?,
                sce.relocations()?,
            )
        } else {
            let dynamic = StdDynamic::new(&elf);
            (
                dynamic.libraries()?,
                dynamic.needed_modules()?,
                dynamic.imports()?,
                dynamic.exports()?,
                dynamic.relocations()?,
            )
        };

        Ok(ParsedImage {
            entry,
            memory_size,
            segments,
            sections,
            tls,
            libraries,
            needed_modules,
            imports,
            exports,
            relocations,
            proc_param_vaddr,
            meta: container.meta,
        })
    }
}

/// `PT_SCE_RELRO` — a loadable, relocated, then read-only segment (`.data.rel.ro`).
/// `p_type` value `0x61000010` (OpenOrbis OELF spec `docs/MD/OELF Specification/PS4 ELF
/// Specification.md`, "Program Header Types" and the `PT_SCE_RELRO (.data.rel.ro)` row).
/// Retail SCE images keep relocated data (e.g. the `SceKernelProcParam` sub-structs
/// like `sceLibcParam`) here rather than in a `PT_LOAD`; it must be mapped and
/// relocated exactly like a `PT_LOAD` or those pointers dangle into an unmapped gap.
const PT_SCE_RELRO: u32 = 0x6100_0010;

/// Zero the section-header table pointer of an ELF64 image whose table lies **past the
/// end of the file** so goblin will parse it (program headers only).
///
/// Retail PS5 executables reference a section-header table whose data is stripped from the
/// dump — `e_shoff` names an offset beyond EOF. When that is the case, zero the three
/// `Elf64_Ehdr` fields that describe the table (`e_shoff`, `e_shnum`, `e_shstrndx`) in
/// place; goblin then sees "no sections" and reads program headers only, which is all the
/// loader maps from. A normal image (`e_shoff == 0`, i.e. no table, or a table that lies
/// inside the file) takes the same untouched path as before — PS4/homebrew bytes are never
/// modified.
///
/// `Elf64_Ehdr` field byte offsets are from FreeBSD 9 (the Orbis OS ELF base) `sys/elf64.h`:
/// e_shoff @ 0x28 (Off, u64), e_shentsize @ 0x3A (Half, u16), e_shnum @ 0x3C (Half, u16),
/// e_shstrndx @ 0x3E (Half, u16); the `Elf64_Ehdr` is 0x40 bytes.
fn sanitize_out_of_bounds_section_table(raw: &mut [u8]) {
    // Shorter than a full Elf64_Ehdr: goblin would reject it regardless, so leave it.
    if raw.len() < 0x40 {
        return;
    }
    let e_shoff = u64::from_le_bytes(raw[0x28..0x30].try_into().unwrap());
    // No section-header table at all (the PS4 inner-ELF / homebrew case) — nothing to do.
    if e_shoff == 0 {
        return;
    }
    let e_shentsize = u16::from_le_bytes(raw[0x3a..0x3c].try_into().unwrap()) as u64;
    let e_shnum = u16::from_le_bytes(raw[0x3c..0x3e].try_into().unwrap()) as u64;
    let table_end = e_shoff.saturating_add(e_shnum.saturating_mul(e_shentsize));
    // Table lies inside the file — a normal, complete image; leave it byte-identical.
    if table_end <= raw.len() as u64 {
        return;
    }
    // Out of bounds: zero e_shoff / e_shnum / e_shstrndx so goblin ignores the missing table.
    raw[0x28..0x30].copy_from_slice(&0u64.to_le_bytes()); // e_shoff
    raw[0x3c..0x3e].copy_from_slice(&0u16.to_le_bytes()); // e_shnum
    raw[0x3e..0x40].copy_from_slice(&0u16.to_le_bytes()); // e_shstrndx
}

fn extract_segments(elf: &Elf, raw: &[u8]) -> Result<Vec<LoadableSegment>, LoaderError> {
    let mut segments = Vec::new();

    for ph in &elf.program_headers {
        // Base ELF phdr facts via goblin's constants: PT_LOAD == 1, and the p_flags bits
        // PF_R == 0x4 / PF_W == 0x2 / PF_X == 0x1 (FreeBSD 9 `sys/sys/elf_common.h`, the
        // Orbis OS ELF base). PT_SCE_RELRO is the Sony extension cited above.
        if ph.p_type == goblin::elf::program_header::PT_LOAD || ph.p_type == PT_SCE_RELRO {
            let mut prot = MemoryProtection::empty();
            if ph.p_flags & goblin::elf64::program_header::PF_R != 0 {
                prot |= MemoryProtection::READ;
            }
            if ph.p_flags & goblin::elf64::program_header::PF_W != 0 {
                prot |= MemoryProtection::WRITE;
            }
            if ph.p_flags & goblin::elf64::program_header::PF_X != 0 {
                prot |= MemoryProtection::EXEC;
            }
            let start = ph.p_offset as usize;
            let size = ph.p_filesz as usize;

            // Compute the end without wrapping: `start + size` can overflow u64/usize for a
            // crafted phdr (p_offset near u64::MAX), and in release builds (overflow-checks
            // off) the wrapped sum would pass the guard and then panic the slice below.
            if start > raw.len() || size > raw.len() - start {
                return Err(LoaderError::Format("Segment outside of file bounds".into()));
            }

            let data = raw[start..start + size].to_vec();

            let bss_size = if ph.p_memsz > ph.p_filesz {
                (ph.p_memsz - ph.p_filesz) as usize
            } else {
                0
            };

            segments.push(LoadableSegment {
                offset: ph.p_vaddr,
                data,
                protection: prot,
                bss_size,
            });
        }
    }
    Ok(segments)
}

fn extract_sections(elf: &Elf) -> Vec<Section> {
    let mut sections = Vec::new();

    for sh in &elf.section_headers {
        // names live in shdr_strtab, indexed by sh_name
        let name = elf
            .shdr_strtab
            .get_at(sh.sh_name)
            .unwrap_or("<unknown>") // corrupted/empty name
            .to_string();

        sections.push(Section {
            name,
            vaddr: sh.sh_addr,
            size: sh.sh_size,
            raw_offset: sh.sh_offset,
        });
    }

    sections
}

fn extract_tls(elf: &Elf, raw: &[u8]) -> Result<Option<TlsInfo>, LoaderError> {
    for ph in &elf.program_headers {
        // PT_TLS == 7, the base ELF thread-local-storage segment (FreeBSD 9
        // `sys/sys/elf_common.h`); goblin supplies the constant.
        if ph.p_type == goblin::elf::program_header::PT_TLS {
            let start = ph.p_offset as usize;
            let filesz = ph.p_filesz as usize;
            let memsz = ph.p_memsz as usize;

            // Non-wrapping bounds check: a crafted PT_TLS phdr (p_offset near u64::MAX) makes
            // `start + filesz` overflow, and in release builds the wrapped sum would slip past
            // the guard and panic the slice below (start > end).
            if start > raw.len() || filesz > raw.len() - start {
                return Err(LoaderError::Format("TLS segment out of bounds".into()));
            }

            let data = raw[start..start + filesz].to_vec();

            return Ok(Some(TlsInfo {
                data,
                mem_size: memsz,
                align: ph.p_align,
            }));
        }
    }
    Ok(None)
}

/// The sole [`ExecutableImage`] impl (form A): a thin cache-backed view over a
/// [`ParsedImage`]. Every method is a cheap clone/borrow of a field extracted at
/// parse time — no `Elf::parse` per method.
pub struct PlainElf {
    parsed: ParsedImage,
}

impl PlainElf {
    pub fn new(parsed: ParsedImage) -> Self {
        PlainElf { parsed }
    }

    pub fn parsed(&self) -> &ParsedImage {
        &self.parsed
    }
}

impl ExecutableImage for PlainElf {
    fn segments(&self) -> Result<Vec<LoadableSegment>, std::io::Error> {
        Ok(self.parsed.segments.clone())
    }

    fn sections(&self) -> Result<Vec<Section>, std::io::Error> {
        Ok(self.parsed.sections.clone())
    }

    fn entry_point(&self) -> Result<u64, std::io::Error> {
        Ok(self.parsed.entry)
    }

    fn memory_size(&self) -> Result<usize, std::io::Error> {
        Ok(self.parsed.memory_size)
    }

    fn imports(&self) -> Result<Vec<Import>, std::io::Error> {
        Ok(self.parsed.imports.clone())
    }

    fn exports(&self) -> Result<HashMap<String, u64>, std::io::Error> {
        Ok(self.parsed.exports.clone())
    }

    fn libraries(&self) -> Result<Vec<String>, std::io::Error> {
        Ok(self.parsed.libraries.clone())
    }

    fn needed_modules(&self) -> Result<Vec<String>, std::io::Error> {
        Ok(self.parsed.needed_modules.clone())
    }

    fn relocations(&self) -> Result<Vec<Relocation>, std::io::Error> {
        Ok(self.parsed.relocations.clone())
    }

    fn tls_info(&self) -> Result<Option<TlsInfo>, std::io::Error> {
        Ok(self.parsed.tls.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::container;
    use goblin::elf::header::*;
    use goblin::elf64::header::SIZEOF_EHDR;
    use ps4_core::img::ExecutableImage;

    fn create_minimal_elf_header(entry: u64) -> Vec<u8> {
        let mut data = vec![0u8; SIZEOF_EHDR];

        // magic + class/data/version
        data[0..4].copy_from_slice(ELFMAG); // 0x7F 'E' 'L' 'F'
        data[EI_CLASS] = ELFCLASS64;
        data[EI_DATA] = ELFDATA2LSB;
        data[EI_VERSION] = EV_CURRENT;

        // e_type @16, ET_DYN
        let e_type = ET_DYN.to_le_bytes();
        data[16..18].copy_from_slice(&e_type);

        // e_machine @18, EM_X86_64
        let e_machine = EM_X86_64.to_le_bytes();
        data[18..20].copy_from_slice(&e_machine);

        // e_version @20
        let e_version = (EV_CURRENT as u32).to_le_bytes();
        data[20..24].copy_from_slice(&e_version);

        // e_entry @24
        let entry_bytes = entry.to_le_bytes();
        data[24..32].copy_from_slice(&entry_bytes);

        // e_ehsize @52
        let ehsize = (SIZEOF_EHDR as u16).to_le_bytes();
        data[52..54].copy_from_slice(&ehsize);

        let phentsize = (56u16).to_le_bytes();
        data[54..56].copy_from_slice(&phentsize);

        data
    }

    fn parse_bytes(raw: Vec<u8>) -> Result<PlainElf, LoaderError> {
        let container = container::open(raw)?;
        Ok(PlainElf::new(ParsedImage::parse(container)?))
    }

    /// Pins the Sony OELF `e_type` / `p_type` constants this module routes on to the
    /// literals published in the OpenOrbis OELF spec (`docs/MD/OELF Specification/PS4 ELF
    /// Specification.md`, "Executable Types" and "Program Header Types"), and the base
    /// ELF program-header types to FreeBSD 9 `sys/sys/elf_common.h`. Fails if ours drift.
    #[test]
    fn sce_elf_constants_match_openorbis_oracle() {
        // Sony OELF e_type values (OpenOrbis OELF spec e_type table). 0xFE10 is listed
        // there as ET_SCE_EXEC_ASLR; ET_SCE_DYNEXEC is the alternate name for that value.
        assert_eq!(ET_SCE_DYNEXEC, 0xFE10);
        assert_eq!(ET_SCE_DYNAMIC, 0xFE18);
        // Sony OELF p_type values (OpenOrbis OELF spec program-header table).
        assert_eq!(PT_SCE_DYNLIBDATA, 0x6100_0000);
        assert_eq!(PT_SCE_RELRO, 0x6100_0010);
        // PT_SCE_PROC_PARAM 0x61000001 is matched inline in ParsedImage::parse.
        const PT_SCE_PROC_PARAM: u32 = 0x6100_0001;
        assert_eq!(PT_SCE_PROC_PARAM, 0x6100_0001);

        // Base ELF program-header types goblin supplies (FreeBSD 9 sys/sys/elf_common.h:
        // PT_LOAD 1, PT_TLS 7) and the p_flags bits (PF_X 0x1, PF_W 0x2, PF_R 0x4).
        assert_eq!(goblin::elf::program_header::PT_LOAD, 1);
        assert_eq!(goblin::elf::program_header::PT_TLS, 7);
        assert_eq!(goblin::elf64::program_header::PF_X, 0x1);
        assert_eq!(goblin::elf64::program_header::PF_W, 0x2);
        assert_eq!(goblin::elf64::program_header::PF_R, 0x4);

        // Elf64_Ehdr / Elf64_Phdr sizes (FreeBSD 9 sys/sys/elf64.h).
        assert_eq!(goblin::elf64::header::SIZEOF_EHDR, 64);
        assert_eq!(PH_ENT, 56);
    }

    #[test]
    fn test_invalid_magic_should_fail() {
        // A non-ELF/non-SELF magic is rejected at the container layer now.
        let data = vec![0xDE, 0xAD, 0xBE, 0xEF];
        let result = parse_bytes(data);
        assert!(result.is_err());
    }

    #[test]
    fn test_valid_entry_point() {
        let expected_entry = 0x123456;
        let data = create_minimal_elf_header(expected_entry);
        let image = parse_bytes(data).expect("minimal ELF must parse");

        let entry = image.entry_point();
        assert!(entry.is_ok(), "Parsing failed: {:?}", entry.err());
        assert_eq!(entry.unwrap(), expected_entry);
    }

    // Elf64_Phdr is 56 bytes (FreeBSD 9 `sys/sys/elf64.h` `Elf64_Phdr`: two Word + two
    // Off/Addr + Addr + three Xword = 4+4+8+8+8+8+8+8); PT_LOAD == 1 / PT_TLS == 7 are
    // the base ELF program-header types (FreeBSD 9 `sys/sys/elf_common.h`).
    const PH_ENT: usize = 56;
    const PT_LOAD: u32 = 1;
    const PT_TLS: u32 = 7;

    /// Build a minimal ELF64 header with `phnum` program headers placed straight
    /// after the header, then write each phdr with the caller-supplied closure.
    fn elf_with_phdrs(phnum: usize, write_ph: impl Fn(&mut [u8], usize)) -> Vec<u8> {
        let phoff = SIZEOF_EHDR;
        let mut data = vec![0u8; phoff + phnum * PH_ENT];
        data[0..4].copy_from_slice(ELFMAG);
        data[EI_CLASS] = ELFCLASS64;
        data[EI_DATA] = ELFDATA2LSB;
        data[EI_VERSION] = EV_CURRENT;
        data[16..18].copy_from_slice(&ET_DYN.to_le_bytes()); // e_type
        data[18..20].copy_from_slice(&EM_X86_64.to_le_bytes()); // e_machine
        data[20..24].copy_from_slice(&(EV_CURRENT as u32).to_le_bytes());
        data[32..40].copy_from_slice(&(phoff as u64).to_le_bytes()); // e_phoff
        data[52..54].copy_from_slice(&(SIZEOF_EHDR as u16).to_le_bytes()); // e_ehsize
        data[54..56].copy_from_slice(&(PH_ENT as u16).to_le_bytes()); // e_phentsize
        data[56..58].copy_from_slice(&(phnum as u16).to_le_bytes()); // e_phnum
        for i in 0..phnum {
            let b = phoff + i * PH_ENT;
            write_ph(&mut data[b..b + PH_ENT], i);
        }
        data
    }

    /// Fill a single ELF64 program header entry. Field byte offsets follow the
    /// `Elf64_Phdr` layout (FreeBSD 9 `sys/sys/elf64.h`): p_type @0, p_flags @4,
    /// p_offset @8, p_vaddr @16, p_paddr @24, p_filesz @32, p_memsz @40, p_align @48.
    fn put_phdr(
        ph: &mut [u8],
        p_type: u32,
        flags: u32,
        offset: u64,
        vaddr: u64,
        filesz: u64,
        memsz: u64,
    ) {
        ph[0..4].copy_from_slice(&p_type.to_le_bytes());
        ph[4..8].copy_from_slice(&flags.to_le_bytes());
        ph[8..16].copy_from_slice(&offset.to_le_bytes()); // p_offset
        ph[16..24].copy_from_slice(&vaddr.to_le_bytes()); // p_vaddr
        ph[24..32].copy_from_slice(&vaddr.to_le_bytes()); // p_paddr
        ph[32..40].copy_from_slice(&filesz.to_le_bytes()); // p_filesz
        ph[40..48].copy_from_slice(&memsz.to_le_bytes()); // p_memsz
        ph[48..56].copy_from_slice(&0x1000u64.to_le_bytes()); // p_align
    }

    #[test]
    fn segment_file_range_beyond_buffer_is_error() {
        // A PT_LOAD whose p_offset + p_filesz runs past the end of the file must be
        // rejected, not sliced out of bounds.
        let data = elf_with_phdrs(1, |ph, _| {
            // offset just past the header table, but filesz claims 0x9000 bytes the
            // file does not contain.
            put_phdr(ph, PT_LOAD, 5, 0x80, 0x0, 0x9000, 0x9000);
        });
        // PlainElf isn't Debug, so match the Result directly rather than expect_err.
        match parse_bytes(data) {
            Err(LoaderError::Format(m)) if m.contains("Segment outside") => {}
            Err(other) => panic!("wrong error: {other:?}"),
            Ok(_) => panic!("out-of-bounds segment must fail"),
        }
    }

    #[test]
    fn section_table_past_eof_is_tolerated() {
        // A PS5-style image: valid program headers, but e_shoff points past EOF (the
        // section-header table was stripped from the dump). goblin's Elf::parse rejects
        // the dangling pointer; the sanitizer must zero it so the phdrs still parse.
        // Elf64_Ehdr fields (FreeBSD 9 sys/elf64.h): e_shoff @0x28, e_shnum @0x3C.
        let mut data = elf_with_phdrs(1, |ph, _| {
            put_phdr(ph, PT_LOAD, 5, 0x80, 0x0, 0x10, 0x10);
        });
        data.resize(0x90, 0); // hold the tiny PT_LOAD payload the phdr claims
        let len = data.len() as u64;
        // e_shoff far past EOF, with a non-zero e_shnum/e_shentsize so the table is "real".
        data[0x28..0x30].copy_from_slice(&(len + 0x1000).to_le_bytes()); // e_shoff
        data[0x3a..0x3c].copy_from_slice(&64u16.to_le_bytes()); // e_shentsize
        data[0x3c..0x3e].copy_from_slice(&48u16.to_le_bytes()); // e_shnum

        // Without the sanitizer goblin would fail with "bad offset"; with it, phdrs parse.
        let image = parse_bytes(data).expect("out-of-bounds section table must be tolerated");
        assert_eq!(
            image.segments().expect("segments").len(),
            1,
            "the single PT_LOAD must survive section-table sanitization"
        );
    }

    #[test]
    fn in_bounds_section_table_is_left_untouched() {
        // A normal image whose (empty) section table is in bounds must be byte-identical:
        // the sanitizer only fires when the table runs past EOF.
        let mut data = create_minimal_elf_header(0x1000);
        data[0x28..0x30].copy_from_slice(&0u64.to_le_bytes()); // e_shoff = 0 (no table)
        let before = data.clone();
        super::sanitize_out_of_bounds_section_table(&mut data);
        assert_eq!(data, before, "no-table image must not be modified");
    }

    #[test]
    fn tls_info_extracted_from_pt_tls() {
        // A PT_TLS phdr with 4 bytes of file-backed init data and a larger memsz.
        let filesz = 4u64;
        let memsz = 16u64;
        // TLS init bytes live right after the phdr table.
        let tls_offset = (SIZEOF_EHDR + PH_ENT) as u64;
        let mut data = elf_with_phdrs(1, |ph, _| {
            put_phdr(ph, PT_TLS, 4, tls_offset, 0x1000, filesz, memsz);
        });
        // append the 4 init bytes at tls_offset
        data.resize(tls_offset as usize + filesz as usize, 0);
        data[tls_offset as usize..tls_offset as usize + 4]
            .copy_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);

        let image = parse_bytes(data).expect("ELF with PT_TLS must parse");
        let tls = image
            .tls_info()
            .expect("tls_info Ok")
            .expect("PT_TLS yields Some(TlsInfo)");
        assert_eq!(tls.data, vec![0xDE, 0xAD, 0xBE, 0xEF], "TLS init image");
        assert_eq!(tls.mem_size, memsz as usize, "TLS mem size = p_memsz");
        assert_eq!(tls.align, 0x1000, "TLS align = p_align");
    }

    #[test]
    fn no_tls_phdr_yields_none() {
        let data = elf_with_phdrs(1, |ph, _| {
            put_phdr(
                ph,
                PT_LOAD,
                5,
                0x0,
                0x0,
                SIZEOF_EHDR as u64 + PH_ENT as u64,
                0x2000,
            );
        });
        let image = parse_bytes(data).expect("ELF without PT_TLS must parse");
        assert!(
            image.tls_info().expect("tls_info Ok").is_none(),
            "no PT_TLS -> None"
        );
    }

    #[test]
    fn test_parse_real_header() {
        let data: Vec<u8> = vec![
            0x7F, 0x45, 0x4C, 0x46, 0x02, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x03, 0x00, 0x3E, 0x00, 0x01, 0x00, 0x00, 0x00, 0x18, 0x31, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x40, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x78, 0x96,
            0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x40, 0x00, 0x38, 0x00,
            0x08, 0x00, 0x40, 0x00, 0x18, 0x00, 0x16, 0x00, 0x01, 0x00, 0x00, 0x00, 0x05, 0x00,
            0x00, 0x00, 0x00, 0x40, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x45,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x45, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x40, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x06, 0x00,
            0x00, 0x00, 0x00, 0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x80, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x18, 0x40,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x18, 0x40, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x40, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x06, 0x00,
            0x00, 0x00, 0x00, 0x40, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0xE8, 0x40,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x50, 0x41, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x40, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x07, 0x00, 0x00, 0x00, 0x04, 0x00,
            0x00, 0x00, 0x00, 0x85, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x45, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x45, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x06, 0x00,
            0x00, 0x00, 0xE8, 0xC1, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xE8, 0x81, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0xE8, 0x81, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x50, 0x01,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x50, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x52, 0xE5, 0x74, 0x64, 0x04, 0x00,
            0x00, 0x00, 0x00, 0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x80, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x18, 0x40,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x50, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x50, 0xE5, 0x74, 0x64, 0x04, 0x00,
            0x00, 0x00, 0xE4, 0x84, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xE4, 0x44, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0xE4, 0x44, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x1C, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x1C, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x04, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x51, 0xE5, 0x74, 0x64, 0x06, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];
        // Pad with zeros: the fixture is truncated, segments() would otherwise slice out of bounds.
        let mut full_data = data.clone();
        full_data.resize(0x20000, 0);

        let image = parse_bytes(full_data).expect("real header must parse");

        assert_eq!(image.entry_point().unwrap(), 0x3118);

        // base 0x0, addresses are relative
        let segments = image.segments().unwrap();

        // expect at least TEXT (RX) and DATA (RW)
        let text_seg = segments
            .iter()
            .find(|s| s.protection == (MemoryProtection::READ | MemoryProtection::EXEC));
        assert!(text_seg.is_some(), "TEXT (RX) segment not found");

        if let Some(seg) = text_seg {
            assert_eq!(seg.offset, 0x0);
            assert_eq!(seg.data.len(), 0x4500);
        }
        let data_seg = segments
            .iter()
            .find(|s| s.protection == (MemoryProtection::READ | MemoryProtection::WRITE));

        assert!(data_seg.is_some(), "DATA (RW) segment not found");

        if let Some(seg) = data_seg {
            // offset comes from p_vaddr: 0x8000 (mem address), not 0xC000 (file pos)
            assert_eq!(seg.offset, 0x8000);

            // data still comes from file offset 0xC000; p_filesz is 0x4018
            assert_eq!(seg.data.len(), 0x4018);
        }
        // PIE (ET_DYN): segments() should return vaddr starting from 0
    }

    // The parse-once path must yield the same segments / entry / imports as a
    // direct goblin parse of the same example ELF (the L2 zero-behavior-change
    // proof at the unit level).
    #[test]
    fn parse_matches_direct_goblin_for_example() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../examples/ps4-helloworld/hello_world.elf"
        );
        let raw = std::fs::read(path).expect("example hello_world.elf must be present");

        let container = container::open(raw.clone()).expect("plain ELF opens");
        let parsed = ParsedImage::parse(container).expect("parse");

        let elf = Elf::parse(&raw).expect("goblin parses example");

        assert_eq!(parsed.entry, elf.entry, "entry matches direct goblin parse");

        let direct_loads = elf
            .program_headers
            .iter()
            .filter(|ph| ph.p_type == goblin::elf::program_header::PT_LOAD)
            .count();
        assert_eq!(
            parsed.segments.len(),
            direct_loads,
            "segment count matches PT_LOAD count"
        );

        let direct_imports = {
            let mut n = 0;
            for sym in elf.dynsyms.iter() {
                if sym.st_shndx == goblin::elf64::section_header::SHN_UNDEF as usize
                    && let Some(name) = elf.dynstrtab.get_at(sym.st_name)
                    && !name.is_empty()
                {
                    n += 1;
                }
            }
            n
        };
        assert_eq!(
            parsed.imports.len(),
            direct_imports,
            "import count matches direct goblin dynsyms scan"
        );
    }

    // AC #4: source auto-selection. A standard homebrew ELF is NOT an SCE image
    // (routed to StdDynamic); a retail ET_SCE_DYNEXEC image IS (routed to
    // SceDynamic).
    #[test]
    fn homebrew_example_selects_std_dynamic() {
        let raw = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../examples/ps4-helloworld/hello_world.elf"
        ))
        .expect("example present");
        let elf = Elf::parse(&raw).expect("goblin parses example");
        assert!(!is_sce_image(&elf), "homebrew must take StdDynamic");
    }

    #[test]
    fn sce_dynexec_selects_sce_dynamic() {
        // Minimal ELF header with e_type = ET_SCE_DYNEXEC and no program headers.
        let mut data = vec![0u8; SIZEOF_EHDR];
        data[0..4].copy_from_slice(ELFMAG);
        data[EI_CLASS] = ELFCLASS64;
        data[EI_DATA] = ELFDATA2LSB;
        data[EI_VERSION] = EV_CURRENT;
        data[16..18].copy_from_slice(&ET_SCE_DYNEXEC.to_le_bytes());
        data[18..20].copy_from_slice(&EM_X86_64.to_le_bytes());
        data[20..24].copy_from_slice(&(EV_CURRENT as u32).to_le_bytes());
        data[52..54].copy_from_slice(&(SIZEOF_EHDR as u16).to_le_bytes());
        data[54..56].copy_from_slice(&56u16.to_le_bytes());
        let elf = Elf::parse(&data).expect("goblin parses SCE header");
        assert!(is_sce_image(&elf), "ET_SCE_DYNEXEC must take SceDynamic");
    }

    // AC #1: confirm the failure mode on the real Bloodborne inner ELF — goblin's
    // standard dynamic parse yields empty imports/relocs on an ET_SCE_DYNEXEC
    // binary, while SceDynamic decodes them. Copyrighted/huge local dump; never
    // committed, never in CI. Runs only when the file exists.
    #[test]
    #[ignore = "requires local copyrighted dump; manual smoke check only"]
    fn real_dump_goblin_empty_but_sce_decodes() {
        let path = "/home/mikolaj/PS4/CUSA03173/eboot.bin";
        if !std::path::Path::new(path).exists() {
            eprintln!("skipping: {path} not present");
            return;
        }
        let raw = std::fs::read(path).expect("read eboot.bin");
        let container = container::open(raw).expect("eboot extracts");
        let elf = Elf::parse(&container.elf_bytes).expect("goblin parses inner ELF");

        // goblin's standard DT_* path: empty on a retail SCE binary.
        eprintln!(
            "goblin: e_type={:#x} dynsyms={} dynrelas={} pltrelocs={}",
            elf.header.e_type,
            elf.dynsyms.len(),
            elf.dynrelas.len(),
            elf.pltrelocs.len(),
        );
        assert_eq!(elf.dynsyms.len(), 0, "goblin decodes no SCE symbols");
        assert_eq!(elf.dynrelas.len(), 0, "goblin decodes no SCE relas");

        // SceDynamic decodes the DT_SCE_* tables instead.
        let sce = crate::dynamic::SceDynamic::new(&elf, &container.elf_bytes)
            .expect("SceDynamic::new")
            .expect("retail image is SCE");
        let imports = sce.imports().unwrap();
        let relocs = sce.relocations().unwrap();
        eprintln!(
            "SceDynamic: imports={} relocs={} libraries={}",
            imports.len(),
            relocs.len(),
            sce.libraries().unwrap().len(),
        );
        assert!(
            !imports.is_empty(),
            "SceDynamic decodes imports goblin missed"
        );
        assert!(!relocs.is_empty(), "SceDynamic decodes relocations");
    }
}
