//! L3 — dynamic-linking source (the pluggable symbol seam).
//!
//! The dynamic-linking view of an image — imports, exports, relocations, needed
//! libraries — is produced by a [`DynamicSource`]. Two impls, auto-selected by
//! L2 at parse time:
//!
//! - [`StdDynamic`] reads goblin's standard `DT_*` tables (`dynsyms` /
//!   `dynrelas` / `pltrelocs` / `dynstrtab`) verbatim — the homebrew path.
//! - [`SceDynamic`] reads the PS4 `DT_SCE_*` tables out of `PT_SCE_DYNLIBDATA`
//!   **without goblin** and decodes NID-hashed symbol names — the retail path.
//!   Goblin cannot decode `DT_SCE_*`, so on a retail `ET_SCE_DYNEXEC` binary its
//!   `dynsyms`/`dynrelas` come back empty; `SceDynamic` fills them.
//!
//! Both produce the **same** [`Import`] / `HashMap` / [`Relocation`] records —
//! the linker never learns whether a symbol arrived as a string or a NID.

use goblin::elf::Elf;
use ps4_core::img::{Import, Relocation, RelocationKind};
use std::collections::HashMap;

use crate::linker::LoaderError;

/// Produces the dynamic-linking view of an image. One impl today ([`StdDynamic`]);
/// a second (`SceDynamic`), auto-selected by L2 at parse time, lands later.
pub trait DynamicSource {
    fn imports(&self) -> Result<Vec<Import>, LoaderError>;
    fn exports(&self) -> Result<HashMap<String, u64>, LoaderError>;
    fn relocations(&self) -> Result<Vec<Relocation>, LoaderError>;
    fn libraries(&self) -> Result<Vec<String>, LoaderError>;
}

/// Homebrew / standard-ELF dynamic source: goblin's `DT_*` path, unchanged.
/// `lib_name` stays empty (only the HLE registry populates library names).
pub struct StdDynamic<'a> {
    elf: &'a Elf<'a>,
}

impl<'a> StdDynamic<'a> {
    pub fn new(elf: &'a Elf<'a>) -> Self {
        StdDynamic { elf }
    }
}

impl DynamicSource for StdDynamic<'_> {
    fn imports(&self) -> Result<Vec<Import>, LoaderError> {
        let elf = self.elf;
        let mut result = Vec::new();

        for (i, sym) in elf.dynsyms.iter().enumerate() {
            if sym.st_shndx == goblin::elf64::section_header::SHN_UNDEF as usize
                && let Some(name) = elf.dynstrtab.get_at(sym.st_name)
                && !name.is_empty()
            {
                result.push(Import {
                    lib_name: String::new(),
                    symbol_name: name.to_string(),
                    symbol_id: i,
                });
            }
        }
        Ok(result)
    }

    fn exports(&self) -> Result<HashMap<String, u64>, LoaderError> {
        let elf = self.elf;
        let mut exports = HashMap::new();
        for sym in elf.dynsyms.iter() {
            if sym.st_shndx != goblin::elf64::section_header::SHN_UNDEF as usize
                && let Some(name) = elf.dynstrtab.get_at(sym.st_name)
                && !name.is_empty()
            {
                exports.insert(name.to_string(), sym.st_value);
            }
        }
        Ok(exports)
    }

    fn relocations(&self) -> Result<Vec<Relocation>, LoaderError> {
        let elf = self.elf;
        let map_reloc = |r: goblin::elf::Reloc| {
            let sym = if r.r_sym > 0 {
                elf.dynsyms.get(r.r_sym)
            } else {
                None
            };
            let symbol_name = sym
                .as_ref()
                .and_then(|s| elf.dynstrtab.get_at(s.st_name))
                .map(|s| s.to_string());
            // only defined symbols (st_shndx != SHN_UNDEF) carry a usable value;
            // imports get resolved later via module exports
            let symbol_value = sym.as_ref().and_then(|s| {
                if s.st_shndx != goblin::elf64::section_header::SHN_UNDEF as usize {
                    Some(s.st_value)
                } else {
                    None
                }
            });

            Relocation {
                offset: r.r_offset,
                kind: map_goblin_to_kind(r.r_type),
                symbol_index: if r.r_sym > 0 { Some(r.r_sym) } else { None },
                addend: r.r_addend.unwrap_or(0),
                symbol_name,
                symbol_value,
            }
        };

        // .rela.dyn
        let mut all_relocs: Vec<Relocation> = elf.dynrelas.iter().map(map_reloc).collect();

        // .rela.plt
        let plt_relocs: Vec<Relocation> = elf.pltrelocs.iter().map(map_reloc).collect();

        all_relocs.extend(plt_relocs);

        Ok(all_relocs)
    }

    fn libraries(&self) -> Result<Vec<String>, LoaderError> {
        Ok(self.elf.libraries.iter().map(|s| s.to_string()).collect())
    }
}

use goblin::elf::reloc::*;

fn map_goblin_to_kind(r_type: u32) -> RelocationKind {
    macro_rules! map_relocs {
        ( $( $goblin_const:ident => $my_variant:ident ),* $(,)? ) => {
            match r_type {
                $(
                    $goblin_const => RelocationKind::$my_variant,
                )*
                _ => RelocationKind::Unknown(r_type),
            }
        };
    }

    // goblin r_type -> internal kind
    map_relocs! {
        R_X86_64_NONE            => None,
        R_X86_64_64              => Absolute64,      // S + A
        R_X86_64_PC32            => Pc32,            // S + A - P
        R_X86_64_GOT32           => Got32,           // G + A
        R_X86_64_PLT32           => Plt32,           // L + A - P
        R_X86_64_COPY            => Copy,
        R_X86_64_GLOB_DAT        => GlobDat,         // S
        R_X86_64_JUMP_SLOT       => JumpSlot,        // S
        R_X86_64_RELATIVE        => Relative,        // B + A
        R_X86_64_GOTPCREL        => GotPcRel,        // G + GOT + A - P
        R_X86_64_32              => Absolute32,      // S + A
        R_X86_64_32S             => Absolute32S,     // S + A
        R_X86_64_16              => Absolute16,      // S + A
        R_X86_64_PC16            => Pc16,            // S + A - P
        R_X86_64_8               => Absolute8,       // S + A
        R_X86_64_PC8             => Pc8,             // S + A - P

        // TLS (Thread Local Storage)
        R_X86_64_DTPMOD64        => DtpMod64,
        R_X86_64_DTPOFF64        => DtpOff64,
        R_X86_64_TPOFF64         => TpOff64,
        R_X86_64_TLSGD           => TlsGd,
        R_X86_64_TLSLD           => TlsLd,
        R_X86_64_DTPOFF32        => DtpOff32,
        R_X86_64_GOTTPOFF        => GotTpOff,
        R_X86_64_TPOFF32         => TpOff32,

        // More 64-bit specific ones
        R_X86_64_PC64            => Pc64,            // S + A - P
        R_X86_64_GOTOFF64        => GotOff64,        // S + A - GOT
        R_X86_64_GOTPC32         => GotPc32,         // GOT + A - P

        // Sizes and other newer ones
        R_X86_64_SIZE32          => Size32,          // Z + A
        R_X86_64_SIZE64          => Size64,          // Z + A
        R_X86_64_GOTPC32_TLSDESC => GotPc32TlsDesc,
        R_X86_64_TLSDESC_CALL    => TlsDescCall,
        R_X86_64_TLSDESC         => TlsDesc,
        R_X86_64_IRELATIVE       => IRelative,       // indirect (B + A)
    }
}

// ---------------------------------------------------------------------------
// SceDynamic — the retail DT_SCE_* + NID reader.
// ---------------------------------------------------------------------------

use goblin::elf::program_header::PT_DYNAMIC;

use crate::nid::encode_id;

/// `PT_SCE_DYNLIBDATA`: the program-header segment holding the SCE dynlib blob
/// (symbol / string / rela tables). All `DT_SCE_*` table offsets are relative to
/// this segment's file offset.
const PT_SCE_DYNLIBDATA: u32 = 0x6100_0000;

// SCE dynamic tag constants (shadPS4 `src/core/loader/elf.h`, OpenOrbis
// "PS4 ELF Specification — Dynlib Data"). These live in the DT_SCE_* range and
// goblin does not decode them.
const DT_SCE_HASH: u64 = 0x6100_0025;
const DT_SCE_PLTGOT: u64 = 0x6100_0027;
const DT_SCE_JMPREL: u64 = 0x6100_0029;
const DT_SCE_PLTREL: u64 = 0x6100_002b;
const DT_SCE_PLTRELSZ: u64 = 0x6100_002d;
const DT_SCE_RELA: u64 = 0x6100_002f;
const DT_SCE_RELASZ: u64 = 0x6100_0031;
const DT_SCE_RELAENT: u64 = 0x6100_0033;
const DT_SCE_STRTAB: u64 = 0x6100_0035;
const DT_SCE_STRSZ: u64 = 0x6100_0037;
const DT_SCE_SYMTAB: u64 = 0x6100_0039;
const DT_SCE_SYMENT: u64 = 0x6100_003b;
const DT_SCE_HASHSZ: u64 = 0x6100_003d;
const DT_SCE_SYMTABSZ: u64 = 0x6100_003f;
const DT_SCE_MODULE_INFO: u64 = 0x6100_000d;
const DT_SCE_NEEDED_MODULE: u64 = 0x6100_000f;
const DT_SCE_IMPORT_LIB: u64 = 0x6100_0015;
const DT_SCE_EXPORT_LIB: u64 = 0x6100_0013;

/// `Elf64_Sym` / `Elf64_Rela` entry size (both 24 bytes).
const SYM_ENT: usize = 24;
const RELA_ENT: usize = 24;

/// A resolved `DT_SCE_*` tag pointing into the dynlib-data blob.
#[derive(Default)]
struct SceTables {
    symtab: usize,
    symtab_sz: usize,
    strtab: usize,
    strtab_sz: usize,
    rela: usize,
    rela_sz: usize,
    jmprel: usize,
    jmprel_sz: usize,
}

/// A module or library name entry decoded from a `DT_SCE_*_LIB` /
/// `DT_SCE_*MODULE*` d_val: the name string-table offset (low 32 bits) and the
/// id (high 16 bits), which the encoded `symbol#lib#module` name references.
struct NamedId {
    name_offset: u32,
    id: u16,
}

impl NamedId {
    /// d_val layout (shadPS4 module.h): name_offset = bits 0..31, id = bits
    /// 48..63.
    fn from_val(val: u64) -> Self {
        NamedId {
            name_offset: (val & 0xffff_ffff) as u32,
            id: ((val >> 48) & 0xffff) as u16,
        }
    }
}

/// Retail dynamic source: reads the PS4 `DT_SCE_*` tables out of
/// `PT_SCE_DYNLIBDATA` **without goblin** and decodes NID-hashed symbol names
/// into the same [`Import`] / [`Export`] / [`Relocation`] records `StdDynamic`
/// produces.
pub struct SceDynamic<'a> {
    /// The whole inner ELF; SCE table offsets are relative to the dynlib-data
    /// segment's file offset.
    dynlib: &'a [u8],
    tables: SceTables,
    /// Library id (encoded) -> library name, from `DT_SCE_IMPORT_LIB` /
    /// `DT_SCE_EXPORT_LIB`.
    libraries_by_enc: HashMap<String, String>,
    /// Every library name (for `libraries()`).
    library_names: Vec<String>,
}

impl<'a> SceDynamic<'a> {
    /// Build from the raw inner-ELF bytes. Locates `PT_SCE_DYNLIBDATA` and
    /// `PT_DYNAMIC`, resolves the `DT_SCE_*` table offsets, and decodes the
    /// module/library name tables. Returns `None` if the image has no
    /// `PT_SCE_DYNLIBDATA` segment (i.e. it is not a retail SCE image).
    pub fn new(elf: &Elf, elf_bytes: &'a [u8]) -> Result<Option<Self>, LoaderError> {
        let Some(dynlib_ph) = elf
            .program_headers
            .iter()
            .find(|ph| ph.p_type == PT_SCE_DYNLIBDATA)
        else {
            return Ok(None);
        };

        let dl_start = dynlib_ph.p_offset as usize;
        let dl_end = dl_start
            .checked_add(dynlib_ph.p_filesz as usize)
            .ok_or_else(|| LoaderError::Format("PT_SCE_DYNLIBDATA extent overflow".into()))?;
        if elf_bytes.len() < dl_end {
            return Err(LoaderError::Format(
                "PT_SCE_DYNLIBDATA out of file bounds".into(),
            ));
        }
        let dynlib = &elf_bytes[dl_start..dl_end];

        // The dynamic tag array lives in PT_DYNAMIC. Read it raw (goblin exposes
        // it too, but keep the whole SCE path goblin-free and explicit).
        let dyn_ph = elf
            .program_headers
            .iter()
            .find(|ph| ph.p_type == PT_DYNAMIC)
            .ok_or_else(|| LoaderError::Format("SCE image has no PT_DYNAMIC".into()))?;
        let dyn_start = dyn_ph.p_offset as usize;
        let dyn_end = dyn_start
            .checked_add(dyn_ph.p_filesz as usize)
            .ok_or_else(|| LoaderError::Format("PT_DYNAMIC extent overflow".into()))?;
        if elf_bytes.len() < dyn_end {
            return Err(LoaderError::Format("PT_DYNAMIC out of file bounds".into()));
        }
        let dyn_bytes = &elf_bytes[dyn_start..dyn_end];

        let mut tables = SceTables::default();
        let mut lib_entries: Vec<NamedId> = Vec::new();
        let mut module_entries: Vec<NamedId> = Vec::new();

        // each Elf64_Dyn is 16 bytes: d_tag (u64) + d_val (u64)
        for chunk in dyn_bytes.chunks_exact(16) {
            let d_tag = u64::from_le_bytes(chunk[0..8].try_into().unwrap());
            let d_val = u64::from_le_bytes(chunk[8..16].try_into().unwrap());
            match d_tag {
                DT_SCE_SYMTAB => tables.symtab = d_val as usize,
                DT_SCE_SYMTABSZ => tables.symtab_sz = d_val as usize,
                DT_SCE_STRTAB => tables.strtab = d_val as usize,
                DT_SCE_STRSZ => tables.strtab_sz = d_val as usize,
                DT_SCE_RELA => tables.rela = d_val as usize,
                DT_SCE_RELASZ => tables.rela_sz = d_val as usize,
                DT_SCE_JMPREL => tables.jmprel = d_val as usize,
                DT_SCE_PLTRELSZ => tables.jmprel_sz = d_val as usize,
                DT_SCE_IMPORT_LIB | DT_SCE_EXPORT_LIB => lib_entries.push(NamedId::from_val(d_val)),
                DT_SCE_MODULE_INFO | DT_SCE_NEEDED_MODULE => {
                    module_entries.push(NamedId::from_val(d_val))
                }
                // read but unused here; named for provenance.
                DT_SCE_HASH | DT_SCE_HASHSZ | DT_SCE_PLTGOT | DT_SCE_PLTREL | DT_SCE_RELAENT
                | DT_SCE_SYMENT => {}
                _ => {}
            }
        }
        let _ = &module_entries;

        // Decode library names now (they live in the SCE string table); build the
        // encoded-id -> name map the `symbol#lib#module` split resolves against.
        let mut libraries_by_enc = HashMap::new();
        let mut library_names = Vec::new();
        for lib in &lib_entries {
            let name = read_cstr(dynlib, tables.strtab, lib.name_offset as usize);
            if !name.is_empty() {
                libraries_by_enc.insert(encode_id(lib.id as u64), name.clone());
                if !library_names.contains(&name) {
                    library_names.push(name);
                }
            }
        }

        Ok(Some(SceDynamic {
            dynlib,
            tables,
            libraries_by_enc,
            library_names,
        }))
    }

    /// Iterate the SCE symbol table, yielding `(index, sym, decoded)`.
    fn symbols(&self) -> impl Iterator<Item = (usize, SceSym, DecodedName)> + '_ {
        let base = self.tables.symtab;
        let count = self.tables.symtab_sz / SYM_ENT;
        (0..count).filter_map(move |i| {
            let off = base + i * SYM_ENT;
            let raw = self.dynlib.get(off..off + SYM_ENT)?;
            let sym = SceSym::parse(raw);
            let encoded = read_cstr(self.dynlib, self.tables.strtab, sym.st_name as usize);
            let decoded = self.decode_name(&encoded);
            Some((i, sym, decoded))
        })
    }

    /// Decode an encoded `symbol#library#module` name into a NID plus the
    /// resolved library name (doc-5 open-Q6: resolve scoped to `(library, NID)`).
    /// A plain (already-decoded) name passes through with no library.
    fn decode_name(&self, encoded: &str) -> DecodedName {
        let mut parts = encoded.split('#');
        let sym = parts.next().unwrap_or("").to_string();
        match (parts.next(), parts.next()) {
            (Some(lib_enc), Some(_mod_enc)) => DecodedName {
                nid: sym,
                lib_name: self
                    .libraries_by_enc
                    .get(lib_enc)
                    .cloned()
                    .unwrap_or_default(),
            },
            _ => DecodedName {
                nid: sym,
                lib_name: String::new(),
            },
        }
    }

    fn relocs_in(&self, base: usize, size: usize) -> Vec<Relocation> {
        let count = size / RELA_ENT;
        let mut out = Vec::with_capacity(count);
        for i in 0..count {
            let off = base + i * RELA_ENT;
            let Some(raw) = self.dynlib.get(off..off + RELA_ENT) else {
                break;
            };
            let r_offset = u64::from_le_bytes(raw[0..8].try_into().unwrap());
            let r_info = u64::from_le_bytes(raw[8..16].try_into().unwrap());
            let r_addend = i64::from_le_bytes(raw[16..24].try_into().unwrap());
            let r_sym = (r_info >> 32) as usize;
            let r_type = (r_info & 0xffff_ffff) as u32;

            let (symbol_name, symbol_value) = if r_sym > 0 {
                let sym_off = self.tables.symtab + r_sym * SYM_ENT;
                if let Some(sraw) = self.dynlib.get(sym_off..sym_off + SYM_ENT) {
                    let sym = SceSym::parse(sraw);
                    let encoded = read_cstr(self.dynlib, self.tables.strtab, sym.st_name as usize);
                    let decoded = self.decode_name(&encoded);
                    let value = if sym.st_shndx != 0 {
                        Some(sym.st_value)
                    } else {
                        None
                    };
                    (Some(decoded.nid), value)
                } else {
                    (None, None)
                }
            } else {
                (None, None)
            };

            out.push(Relocation {
                offset: r_offset,
                kind: map_goblin_to_kind(r_type),
                symbol_index: if r_sym > 0 { Some(r_sym) } else { None },
                addend: r_addend,
                symbol_name,
                symbol_value,
            });
        }
        out
    }
}

/// A parsed `Elf64_Sym` from the SCE symbol table.
struct SceSym {
    st_name: u32,
    st_shndx: u16,
    st_value: u64,
}

impl SceSym {
    fn parse(raw: &[u8]) -> Self {
        // st_name u32 @0, st_info u8 @4, st_other u8 @5, st_shndx u16 @6,
        // st_value u64 @8, st_size u64 @16
        SceSym {
            st_name: u32::from_le_bytes(raw[0..4].try_into().unwrap()),
            st_shndx: u16::from_le_bytes(raw[6..8].try_into().unwrap()),
            st_value: u64::from_le_bytes(raw[8..16].try_into().unwrap()),
        }
    }
}

/// An import name decoded from the SCE strtab: the bare NID plus the library it
/// was scoped to (empty when the name was not `symbol#lib#module`).
struct DecodedName {
    nid: String,
    lib_name: String,
}

/// Read a NUL-terminated string at `strtab_base + offset` within the dynlib blob.
fn read_cstr(dynlib: &[u8], strtab_base: usize, offset: usize) -> String {
    let start = strtab_base + offset;
    if start >= dynlib.len() {
        return String::new();
    }
    let end = dynlib[start..]
        .iter()
        .position(|&b| b == 0)
        .map(|p| start + p)
        .unwrap_or(dynlib.len());
    String::from_utf8_lossy(&dynlib[start..end]).into_owned()
}

impl DynamicSource for SceDynamic<'_> {
    fn imports(&self) -> Result<Vec<Import>, LoaderError> {
        let mut result = Vec::new();
        for (i, sym, decoded) in self.symbols() {
            // undefined (SHN_UNDEF == 0) symbols with a name are imports.
            if sym.st_shndx == 0 && !decoded.nid.is_empty() {
                result.push(Import {
                    lib_name: decoded.lib_name,
                    symbol_name: decoded.nid,
                    symbol_id: i,
                });
            }
        }
        Ok(result)
    }

    fn exports(&self) -> Result<HashMap<String, u64>, LoaderError> {
        let mut exports = HashMap::new();
        for (_i, sym, decoded) in self.symbols() {
            if sym.st_shndx != 0 && !decoded.nid.is_empty() {
                exports.insert(decoded.nid, sym.st_value);
            }
        }
        Ok(exports)
    }

    fn relocations(&self) -> Result<Vec<Relocation>, LoaderError> {
        let mut relocs = self.relocs_in(self.tables.rela, self.tables.rela_sz);
        relocs.extend(self.relocs_in(self.tables.jmprel, self.tables.jmprel_sz));
        Ok(relocs)
    }

    fn libraries(&self) -> Result<Vec<String>, LoaderError> {
        Ok(self.library_names.clone())
    }
}

#[cfg(test)]
mod sce_tests {
    use super::*;
    use crate::nid::encode_id;
    use ps4_syscalls::SyscallId;

    /// Canonical NID for a real HLE export, via the generated `MAP_BY_NID`.
    fn nid_of(name: &str) -> String {
        SyscallId::from_symbol_name(name)
            .expect("known HLE export")
            .nid()
            .to_string()
    }

    /// A synthetic 11-char NID for a symbol that is not a real syscall (the
    /// image's own export); it only has to round-trip through decode, not match
    /// any table.
    const LOCAL_EXPORT_NID: &str = "myLocalExpo";

    const ELF_HDR: usize = 0x40;
    const PH_ENT: usize = 0x38;
    const PT_LOAD: u32 = 1;
    const PT_DYNAMIC_T: u32 = 2;

    fn push_u64(v: &mut Vec<u8>, x: u64) {
        v.extend_from_slice(&x.to_le_bytes());
    }

    /// Hand-craft a minimal `ET_SCE_DYNEXEC` ELF: header + three program headers
    /// (PT_LOAD, PT_DYNAMIC, PT_SCE_DYNLIBDATA), a dynlib blob (strtab + symtab +
    /// rela + jmprel) and the dynamic tag array. One imported symbol scoped to a
    /// library, one exported symbol, one .rela.dyn reloc and one .rela.plt reloc.
    fn build_minimal_sce() -> Vec<u8> {
        // ---- SCE dynlib blob (strtab, symtab, rela, jmprel) ----
        // strtab: [0]=NUL, then library name, then two encoded symbol names.
        let lib_id: u16 = 3;
        let mod_id: u16 = 1;
        let lib_enc = encode_id(lib_id as u64);
        let mod_enc = encode_id(mod_id as u64);

        let import_nid = nid_of("sceKernelUsleep"); // an import we resolve later
        let export_nid = LOCAL_EXPORT_NID.to_string();

        let mut strtab: Vec<u8> = Vec::new();
        strtab.push(0); // index 0 = empty
        let lib_name_off = strtab.len() as u32;
        strtab.extend_from_slice(b"libkernel\0");
        let import_name_off = strtab.len() as u32;
        strtab.extend_from_slice(format!("{import_nid}#{lib_enc}#{mod_enc}\0").as_bytes());
        let export_name_off = strtab.len() as u32;
        strtab.extend_from_slice(format!("{export_nid}#{lib_enc}#{mod_enc}\0").as_bytes());

        // symtab: index 0 reserved (undef, empty), 1 = import (SHN_UNDEF),
        // 2 = export (defined, st_shndx != 0, st_value = 0x1234).
        let mut symtab: Vec<u8> = Vec::new();
        let push_sym = |v: &mut Vec<u8>, name: u32, shndx: u16, value: u64| {
            v.extend_from_slice(&name.to_le_bytes()); // st_name
            v.push(0); // st_info
            v.push(0); // st_other
            v.extend_from_slice(&shndx.to_le_bytes()); // st_shndx
            push_u64(v, value); // st_value
            push_u64(v, 0); // st_size
        };
        push_sym(&mut symtab, 0, 0, 0); // [0] reserved
        push_sym(&mut symtab, import_name_off, 0, 0); // [1] import (undef)
        push_sym(&mut symtab, export_name_off, 1, 0x1234); // [2] export (defined)

        // rela.dyn: one R_X86_64_64 against the import symbol (index 1).
        let r_x86_64_64: u32 = 1;
        let r_x86_64_jump_slot: u32 = 7;
        let mut rela: Vec<u8> = Vec::new();
        push_u64(&mut rela, 0x2000); // r_offset
        push_u64(&mut rela, ((1u64) << 32) | r_x86_64_64 as u64); // r_info: sym=1
        push_u64(&mut rela, 0); // r_addend

        // rela.plt (jmprel): one JUMP_SLOT against the import symbol (index 1).
        let mut jmprel: Vec<u8> = Vec::new();
        push_u64(&mut jmprel, 0x3000);
        push_u64(&mut jmprel, ((1u64) << 32) | r_x86_64_jump_slot as u64);
        push_u64(&mut jmprel, 0);

        // Assemble the dynlib blob; record each table's offset within it.
        let mut dynlib: Vec<u8> = Vec::new();
        let strtab_off = dynlib.len();
        dynlib.extend_from_slice(&strtab);
        let symtab_off = dynlib.len();
        dynlib.extend_from_slice(&symtab);
        let rela_off = dynlib.len();
        dynlib.extend_from_slice(&rela);
        let jmprel_off = dynlib.len();
        dynlib.extend_from_slice(&jmprel);

        // ---- dynamic tag array (Elf64_Dyn: d_tag u64, d_val u64) ----
        let mut dynamic: Vec<u8> = Vec::new();
        let push_dyn = |v: &mut Vec<u8>, tag: u64, val: u64| {
            push_u64(v, tag);
            push_u64(v, val);
        };
        push_dyn(&mut dynamic, DT_SCE_STRTAB, strtab_off as u64);
        push_dyn(&mut dynamic, DT_SCE_STRSZ, strtab.len() as u64);
        push_dyn(&mut dynamic, DT_SCE_SYMTAB, symtab_off as u64);
        push_dyn(&mut dynamic, DT_SCE_SYMTABSZ, symtab.len() as u64);
        push_dyn(&mut dynamic, DT_SCE_RELA, rela_off as u64);
        push_dyn(&mut dynamic, DT_SCE_RELASZ, rela.len() as u64);
        push_dyn(&mut dynamic, DT_SCE_JMPREL, jmprel_off as u64);
        push_dyn(&mut dynamic, DT_SCE_PLTRELSZ, jmprel.len() as u64);
        // library entry: id in bits 48..63, name_offset in bits 0..31.
        let lib_val = ((lib_id as u64) << 48) | (lib_name_off as u64);
        push_dyn(&mut dynamic, DT_SCE_IMPORT_LIB, lib_val);
        push_dyn(&mut dynamic, 0, 0); // DT_NULL

        // ---- lay out the file: header, phdrs, then dynamic, then dynlib ----
        let ph_count = 3usize;
        let phoff = ELF_HDR;
        let body_start = phoff + ph_count * PH_ENT;
        let dyn_file_off = body_start;
        let dynlib_file_off = dyn_file_off + dynamic.len();
        let total = dynlib_file_off + dynlib.len();

        let mut out = vec![0u8; total];
        // ELF header
        out[0..4].copy_from_slice(b"\x7FELF");
        out[4] = 2; // ELFCLASS64
        out[5] = 1; // ELFDATA2LSB
        out[6] = 1; // EV_CURRENT
        out[16..18].copy_from_slice(&0xfe10u16.to_le_bytes()); // e_type ET_SCE_DYNEXEC
        out[18..20].copy_from_slice(&62u16.to_le_bytes()); // e_machine EM_X86_64
        out[20..24].copy_from_slice(&1u32.to_le_bytes()); // e_version
        out[24..32].copy_from_slice(&0x1000u64.to_le_bytes()); // e_entry
        out[32..40].copy_from_slice(&(phoff as u64).to_le_bytes()); // e_phoff
        out[52..54].copy_from_slice(&(ELF_HDR as u16).to_le_bytes()); // e_ehsize
        out[54..56].copy_from_slice(&(PH_ENT as u16).to_le_bytes()); // e_phentsize
        out[56..58].copy_from_slice(&(ph_count as u16).to_le_bytes()); // e_phnum

        // program headers
        let write_ph =
            |out: &mut [u8], i: usize, p_type: u32, off: u64, vaddr: u64, filesz: u64| {
                let b = phoff + i * PH_ENT;
                out[b..b + 4].copy_from_slice(&p_type.to_le_bytes()); // p_type
                out[b + 4..b + 8].copy_from_slice(&5u32.to_le_bytes()); // p_flags R|X
                out[b + 8..b + 16].copy_from_slice(&off.to_le_bytes()); // p_offset
                out[b + 16..b + 24].copy_from_slice(&vaddr.to_le_bytes()); // p_vaddr
                out[b + 24..b + 32].copy_from_slice(&vaddr.to_le_bytes()); // p_paddr
                out[b + 32..b + 40].copy_from_slice(&filesz.to_le_bytes()); // p_filesz
                out[b + 40..b + 48].copy_from_slice(&filesz.to_le_bytes()); // p_memsz
                out[b + 48..b + 56].copy_from_slice(&0x1000u64.to_le_bytes()); // p_align
            };
        write_ph(&mut out, 0, PT_LOAD, 0, 0, ELF_HDR as u64);
        write_ph(
            &mut out,
            1,
            PT_DYNAMIC_T,
            dyn_file_off as u64,
            0x10_0000,
            dynamic.len() as u64,
        );
        write_ph(
            &mut out,
            2,
            PT_SCE_DYNLIBDATA,
            dynlib_file_off as u64,
            0,
            dynlib.len() as u64,
        );

        out[dyn_file_off..dyn_file_off + dynamic.len()].copy_from_slice(&dynamic);
        out[dynlib_file_off..dynlib_file_off + dynlib.len()].copy_from_slice(&dynlib);
        out
    }

    fn parse(raw: &[u8]) -> SceDynamic<'_> {
        let elf = Elf::parse(raw).expect("goblin parses minimal SCE ELF");
        SceDynamic::new(&elf, raw)
            .expect("SceDynamic::new")
            .expect("image is SCE")
    }

    #[test]
    fn decodes_imports_with_library_name() {
        let raw = build_minimal_sce();
        let sce = parse(&raw);
        let imports = sce.imports().unwrap();
        assert_eq!(imports.len(), 1, "one undef symbol is an import");
        let imp = &imports[0];
        assert_eq!(imp.symbol_name, nid_of("sceKernelUsleep"));
        assert_eq!(imp.lib_name, "libkernel", "lib_name is finally populated");
    }

    #[test]
    fn decodes_exports_by_nid() {
        let raw = build_minimal_sce();
        let sce = parse(&raw);
        let exports = sce.exports().unwrap();
        assert_eq!(exports.len(), 1);
        assert_eq!(exports.get(LOCAL_EXPORT_NID), Some(&0x1234));
    }

    #[test]
    fn decodes_relocations_from_both_tables() {
        let raw = build_minimal_sce();
        let sce = parse(&raw);
        let relocs = sce.relocations().unwrap();
        assert_eq!(relocs.len(), 2, ".rela.dyn + .rela.plt");

        let abs = relocs
            .iter()
            .find(|r| r.kind == RelocationKind::Absolute64)
            .expect("Absolute64 from rela.dyn");
        assert_eq!(abs.offset, 0x2000);
        assert_eq!(
            abs.symbol_name.as_deref(),
            Some(nid_of("sceKernelUsleep").as_str())
        );

        let js = relocs
            .iter()
            .find(|r| r.kind == RelocationKind::JumpSlot)
            .expect("JumpSlot from jmprel");
        assert_eq!(js.offset, 0x3000);
    }

    #[test]
    fn libraries_lists_decoded_name() {
        let raw = build_minimal_sce();
        let sce = parse(&raw);
        assert_eq!(sce.libraries().unwrap(), vec!["libkernel".to_string()]);
    }

    /// A malformed SCE image whose DT_SCE_SYMTABSZ / DT_SCE_RELASZ claim tables
    /// far larger than the dynlib blob must not panic: the bounds-checked readers
    /// (`symbols()` via `.get()`, `relocs_in()` via `.get()`) stop early.
    #[test]
    fn oversized_sce_tables_do_not_panic() {
        let mut raw = build_minimal_sce();

        // Locate the dynamic tag array and inflate the symtab/rela sizes to values
        // that would run past the dynlib blob if read unchecked.
        let elf = Elf::parse(&raw).expect("goblin parses");
        let dyn_ph = elf
            .program_headers
            .iter()
            .find(|ph| ph.p_type == PT_DYNAMIC)
            .expect("PT_DYNAMIC present");
        let dyn_start = dyn_ph.p_offset as usize;
        let dyn_end = dyn_start + dyn_ph.p_filesz as usize;

        for chunk_start in (dyn_start..dyn_end).step_by(16) {
            let tag = u64::from_le_bytes(raw[chunk_start..chunk_start + 8].try_into().unwrap());
            if tag == DT_SCE_SYMTABSZ || tag == DT_SCE_RELASZ || tag == DT_SCE_PLTRELSZ {
                raw[chunk_start + 8..chunk_start + 16]
                    .copy_from_slice(&0xFFFF_0000u64.to_le_bytes());
            }
        }

        let sce = parse(&raw);
        // These must return (possibly truncated) results without panicking.
        let _ = sce.imports().unwrap();
        let _ = sce.exports().unwrap();
        let _ = sce.relocations().unwrap();
    }
}

#[cfg(test)]
mod reloc_kind_tests {
    use super::map_goblin_to_kind;
    use goblin::elf::reloc::*;
    use ps4_core::img::RelocationKind;

    #[test]
    fn known_r_types_map_to_expected_kinds() {
        assert_eq!(map_goblin_to_kind(R_X86_64_NONE), RelocationKind::None);
        assert_eq!(map_goblin_to_kind(R_X86_64_64), RelocationKind::Absolute64);
        assert_eq!(map_goblin_to_kind(R_X86_64_PC32), RelocationKind::Pc32);
        assert_eq!(map_goblin_to_kind(R_X86_64_COPY), RelocationKind::Copy);
        assert_eq!(
            map_goblin_to_kind(R_X86_64_GLOB_DAT),
            RelocationKind::GlobDat
        );
        assert_eq!(
            map_goblin_to_kind(R_X86_64_JUMP_SLOT),
            RelocationKind::JumpSlot
        );
        assert_eq!(
            map_goblin_to_kind(R_X86_64_RELATIVE),
            RelocationKind::Relative
        );
        assert_eq!(
            map_goblin_to_kind(R_X86_64_DTPMOD64),
            RelocationKind::DtpMod64
        );
        assert_eq!(
            map_goblin_to_kind(R_X86_64_DTPOFF64),
            RelocationKind::DtpOff64
        );
        assert_eq!(
            map_goblin_to_kind(R_X86_64_TPOFF64),
            RelocationKind::TpOff64
        );
        assert_eq!(
            map_goblin_to_kind(R_X86_64_IRELATIVE),
            RelocationKind::IRelative
        );
    }

    #[test]
    fn unrecognized_r_type_maps_to_unknown_carrying_value() {
        // A vendor/unused r_type falls into the Unknown arm carrying its raw value.
        let bogus = 0x0BAD_F00D;
        assert_eq!(
            map_goblin_to_kind(bogus),
            RelocationKind::Unknown(bogus),
            "unrecognized r_type must preserve its raw value"
        );
    }
}
