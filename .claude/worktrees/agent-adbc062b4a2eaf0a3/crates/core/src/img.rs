// unified interface over the two executable formats the PS4 uses:
// SELF (signed) and plain ELF.

use std::collections::HashMap;

use crate::memory::MemoryProtection;

#[derive(Debug, Clone)]
pub struct TlsInfo {
    pub data: Vec<u8>,
    pub mem_size: usize,
    pub align: u64,
}

#[derive(Debug, Clone)]
pub struct LoadableSegment {
    pub offset: u64,
    pub data: Vec<u8>,
    pub protection: MemoryProtection,
    pub bss_size: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Import {
    pub lib_name: String,
    pub symbol_name: String,
    pub symbol_id: usize,
}

#[derive(Debug, Clone)]
pub struct Relocation {
    pub offset: u64,
    pub kind: RelocationKind,
    pub symbol_index: Option<usize>,
    pub addend: i64,
    pub symbol_name: Option<String>,
    /// `st_value` of the referenced symbol when it is defined in this module
    /// (`st_shndx != SHN_UNDEF`); `None` for undefined/imported symbols. Needed
    /// to compute `S + A` style relocations (e.g. R_X86_64_64).
    pub symbol_value: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelocationKind {
    /// R_X86_64_NONE
    None,

    /// R_X86_64_64: S + A
    Absolute64,
    /// R_X86_64_PC32: S + A - P
    Pc32,
    /// R_X86_64_GOT32: G + A
    Got32,
    /// R_X86_64_PLT32: L + A - P
    Plt32,
    /// R_X86_64_COPY
    Copy,
    /// R_X86_64_GLOB_DAT: S
    GlobDat,
    /// R_X86_64_JUMP_SLOT: S
    JumpSlot,
    /// R_X86_64_RELATIVE: B + A
    Relative,
    /// R_X86_64_GOTPCREL: G + GOT + A - P
    GotPcRel,
    /// R_X86_64_32: S + A
    Absolute32,
    /// R_X86_64_32S: S + A
    Absolute32S,
    /// R_X86_64_16: S + A
    Absolute16,
    /// R_X86_64_PC16: S + A - P
    Pc16,
    /// R_X86_64_8: S + A
    Absolute8,
    /// R_X86_64_PC8: S + A - P
    Pc8,

    // tls
    DtpMod64,
    DtpOff64,
    TpOff64,
    TlsGd,
    TlsLd,
    DtpOff32,
    GotTpOff,
    TpOff32,

    // 64-bit specific
    /// R_X86_64_PC64: S + A - P
    Pc64,
    /// R_X86_64_GOTOFF64: S + A - GOT
    GotOff64,
    /// R_X86_64_GOTPC32: GOT + A - P
    GotPc32,

    // sizes and newer relocs
    Size32,
    Size64,
    GotPc32TlsDesc,
    TlsDescCall,
    TlsDesc,
    /// R_X86_64_IRELATIVE: indirect (B + A)
    IRelative,

    /// Stores the raw type if we don't know the mapping
    Unknown(u32),
}

#[derive(Debug, Clone)]
pub struct Section {
    pub name: String,
    pub vaddr: u64,
    pub size: u64,
    pub raw_offset: u64,
}

pub trait ExecutableImage: Send + Sync {
    fn segments(&self) -> Result<Vec<LoadableSegment>, std::io::Error>;
    fn sections(&self) -> Result<Vec<Section>, std::io::Error>;
    fn entry_point(&self) -> Result<u64, std::io::Error>;
    fn memory_size(&self) -> Result<usize, std::io::Error>;
    fn imports(&self) -> Result<Vec<Import>, std::io::Error>;
    fn exports(&self) -> Result<HashMap<String, u64>, std::io::Error>;
    fn libraries(&self) -> Result<Vec<String>, std::io::Error>;
    fn relocations(&self) -> Result<Vec<Relocation>, std::io::Error>;
    fn tls_info(&self) -> Result<Option<TlsInfo>, std::io::Error>;
}
