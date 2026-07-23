//! L1 — container detection & unwrap (**extraction only, never decryption**).
//!
//! unemups4 operates exclusively on already-decrypted dumps (project ethos and
//! legal constraint; shadPS4 likewise dropped decryption). A SELF is the PS4
//! executable container: an SCE header + a segment table wrapping a plaintext
//! inner ELF. This layer detects the container by magic, reconstructs the inner
//! ELF bytes from the segment table, and **retains** the SCE/SELF container
//! metadata (rather than discarding it) for later layers. It contains no crypto,
//! no keys, and no SAMU logic; encrypted or compressed payloads are rejected with
//! an explicit error rather than any decode attempt.

use byteorder::{LittleEndian, ReadBytesExt};
use std::io::Cursor;

const ELF_MAGIC: u32 = 0x464c_457f; // 0x7F 'E' 'L' 'F' read little-endian
const SELF_MAGIC: u32 = 0x1d3d_154f; // 0x4F 0x15 0x3D 0x1D read little-endian

const SCE_HEADER_SIZE: usize = 0x20;
const SELF_SEGMENT_SIZE: usize = 0x20;
const ELF_HEADER_SIZE: usize = 0x40;

// self_segment_header::flags bitfield layout, per shadPS4
// (src/core/loader/elf.h self_segment_header accessors):
//   IsBlocked() == (flags >> 11) & 0x1   -> segment backs an ELF program header
//   GetId()     == (flags >> 20) & 0xFFF -> program-header index this segment fills
// Compression is NOT reliably derivable from a flag bit across dumps (the low
// flag nibble also encodes ordering/self-segment kind), so it is detected the
// unambiguous way: compressed_size != uncompressed_size.
const SELF_SEG_BLOCKED_BIT: u64 = 1 << 11;
const SELF_SEG_ID_SHIFT: u64 = 20;
const SELF_SEG_ID_MASK: u64 = 0xFFF;

#[derive(Debug, thiserror::Error)]
pub enum ContainerError {
    #[error("input too short: {0}")]
    TooShort(&'static str),
    #[error("unrecognized executable: not an ELF or SELF container (magic {0:#010x})")]
    UnknownMagic(u32),
    #[error("malformed SELF: {0}")]
    Malformed(String),
    #[error(
        "SELF segment {segment} is compressed (zlib); compressed SELF segments are unsupported"
    )]
    Compressed { segment: usize },
    #[error(
        "SELF payload appears encrypted — this build performs no decryption; the file must be decrypted first"
    )]
    Encrypted,
}

/// The container format an input file was recognized as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerKind {
    /// Plain, unwrapped ELF (`0x7F E L F`). No container metadata.
    RawElf,
    /// SCE SELF container (`0x4F 0x15 0x3D 0x1D`), inner ELF reconstructed.
    Self_,
}

/// SCE/SELF container metadata **retained** (rather than discarded) at unwrap.
///
/// Kept deliberately minimal (doc-5 open-Q5: grow per-consumer, don't model the
/// whole SCE header speculatively). Empty for [`ContainerKind::RawElf`]. Every
/// field is `Option` so a raw ELF — and any SELF field we do not yet parse — is
/// simply `None`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContainerMeta {
    /// Number of SELF segments in the container segment table (the one SCE-header
    /// field the unwrap already reads).
    pub num_segments: Option<u16>,
    /// SCE program/content-type discriminator (eboot vs prx vs lib), read from the
    /// SELF header `key_type`/category `u16` at offset `0x08`. This is a coarse
    /// container-level discriminator; the fine-grained module program type lives
    /// in the SCE program-identification header, which this layer does not yet
    /// parse.
    pub sce_program_type: Option<u16>,
    /// SCE module attributes. NOT populated: the real module-attribute word lives
    /// in the SCE program-info / `PT_SCE_PROCPARAM` blob, which the current unwrap
    /// does not parse. Left `None` until a consumer needs it (doc-5 open-Q5); a
    /// future step fills it when it parses that header.
    pub module_attributes: Option<u64>,
}

/// What L1 hands to L2 (the parse-once image layer): the plaintext inner ELF plus
/// the container metadata we chose to keep instead of discarding.
#[derive(Debug, Clone)]
pub struct Container {
    pub kind: ContainerKind,
    pub elf_bytes: Vec<u8>,
    pub meta: ContainerMeta,
}

/// Auto-detect the executable container and return the plaintext inner ELF plus
/// retained metadata.
///
/// The dispatch is a `match` on magic (two real formats, a handful of future
/// ones), **not** a plugin registry (doc-5 §5 trap #1):
/// - Plain ELF (`0x7F E L F`) passes through byte-identical, `meta` empty.
/// - SELF (`0x4F 0x15 0x3D 0x1D`) is parsed, its inner ELF reconstructed, and its
///   container metadata retained.
/// - Anything else is an error. Encrypted/compressed payloads are rejected.
pub fn open(raw: Vec<u8>) -> Result<Container, ContainerError> {
    if raw.len() < 4 {
        return Err(ContainerError::TooShort("less than 4 bytes"));
    }

    let magic = u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]);
    match magic {
        ELF_MAGIC => Ok(Container {
            kind: ContainerKind::RawElf,
            elf_bytes: raw,
            meta: ContainerMeta::default(),
        }),
        SELF_MAGIC => {
            let (elf_bytes, meta) = extract_from_self(&raw)?;
            Ok(Container {
                kind: ContainerKind::Self_,
                elf_bytes,
                meta,
            })
        }
        other => Err(ContainerError::UnknownMagic(other)),
    }
}

struct SelfSegment {
    flags: u64,
    offset: u64,
    compressed_size: u64,
    uncompressed_size: u64,
}

impl SelfSegment {
    fn is_blocked(&self) -> bool {
        self.flags & SELF_SEG_BLOCKED_BIT != 0
    }

    fn program_header_id(&self) -> usize {
        ((self.flags >> SELF_SEG_ID_SHIFT) & SELF_SEG_ID_MASK) as usize
    }

    fn is_compressed(&self) -> bool {
        self.compressed_size != self.uncompressed_size
    }
}

fn read_segments(raw: &[u8], num_segments: usize) -> Result<Vec<SelfSegment>, ContainerError> {
    let table_end = SCE_HEADER_SIZE + num_segments * SELF_SEGMENT_SIZE;
    if raw.len() < table_end {
        return Err(ContainerError::TooShort("segment table truncated"));
    }

    let mut cur = Cursor::new(&raw[SCE_HEADER_SIZE..table_end]);
    let mut segments = Vec::with_capacity(num_segments);
    for _ in 0..num_segments {
        // reads cannot fail: the slice length was validated above.
        let flags = cur.read_u64::<LittleEndian>().unwrap();
        let offset = cur.read_u64::<LittleEndian>().unwrap();
        let compressed_size = cur.read_u64::<LittleEndian>().unwrap();
        let uncompressed_size = cur.read_u64::<LittleEndian>().unwrap();
        segments.push(SelfSegment {
            flags,
            offset,
            compressed_size,
            uncompressed_size,
        });
    }
    Ok(segments)
}

struct ProgramHeader {
    file_offset: u64,
    file_size: u64,
}

/// Unpack the ELF64 program-header-table locator fields from an ELF header slice
/// (`ehdr` must be at least [`ELF_HEADER_SIZE`] bytes, starting at the ELF magic).
/// Returns `(e_phoff, e_phentsize, e_phnum)`, all little-endian per the ELF64 spec:
///   e_phoff     @ 0x20 (u64)
///   e_phentsize @ 0x36 (u16)
///   e_phnum     @ 0x38 (u16)
fn phdr_table_info(ehdr: &[u8]) -> (u64, usize, usize) {
    let e_phoff = u64::from_le_bytes(ehdr[0x20..0x28].try_into().unwrap());
    let e_phentsize = u16::from_le_bytes(ehdr[0x36..0x38].try_into().unwrap()) as usize;
    let e_phnum = u16::from_le_bytes(ehdr[0x38..0x3A].try_into().unwrap()) as usize;
    (e_phoff, e_phentsize, e_phnum)
}

/// Unpack the file-image extent fields from an ELF64 program-header slice (`phdr`
/// must be at least 0x28 bytes, starting at the phdr entry). Returns
/// `(p_offset, p_filesz)`, both little-endian per the ELF64 spec:
///   p_offset @ 0x08 (u64)
///   p_filesz @ 0x20 (u64)
fn phdr_extent(phdr: &[u8]) -> (u64, u64) {
    let p_offset = u64::from_le_bytes(phdr[0x08..0x10].try_into().unwrap());
    let p_filesz = u64::from_le_bytes(phdr[0x20..0x28].try_into().unwrap());
    (p_offset, p_filesz)
}

fn read_program_headers(raw: &[u8], elf_base: usize) -> Result<Vec<ProgramHeader>, ContainerError> {
    if raw.len() < elf_base + ELF_HEADER_SIZE {
        return Err(ContainerError::TooShort("inner ELF header truncated"));
    }
    let ehdr = &raw[elf_base..elf_base + ELF_HEADER_SIZE];
    let (e_phoff, e_phentsize, e_phnum) = phdr_table_info(ehdr);

    if e_phentsize < 0x38 {
        return Err(ContainerError::Malformed(format!(
            "e_phentsize {e_phentsize:#x} too small for ELF64 program header"
        )));
    }

    let table_start = elf_base
        .checked_add(e_phoff as usize)
        .ok_or_else(|| ContainerError::Malformed("e_phoff overflow".into()))?;
    let table_len = e_phnum
        .checked_mul(e_phentsize)
        .ok_or_else(|| ContainerError::Malformed("phdr table size overflow".into()))?;
    let table_end = table_start
        .checked_add(table_len)
        .ok_or_else(|| ContainerError::Malformed("phdr table end overflow".into()))?;
    if raw.len() < table_end {
        return Err(ContainerError::TooShort(
            "inner ELF program header table truncated",
        ));
    }

    let mut headers = Vec::with_capacity(e_phnum);
    for i in 0..e_phnum {
        let base = table_start + i * e_phentsize;
        let phdr = &raw[base..base + e_phentsize];
        let (file_offset, file_size) = phdr_extent(phdr);
        headers.push(ProgramHeader {
            file_offset,
            file_size,
        });
    }
    Ok(headers)
}

fn extract_from_self(raw: &[u8]) -> Result<(Vec<u8>, ContainerMeta), ContainerError> {
    if raw.len() < SCE_HEADER_SIZE {
        return Err(ContainerError::TooShort("SCE header truncated"));
    }

    // num_segments is a u16 at 0x18 in the SCE/SELF header.
    let num_segments = u16::from_le_bytes([raw[0x18], raw[0x19]]) as usize;
    if num_segments == 0 {
        return Err(ContainerError::Malformed("SELF has zero segments".into()));
    }
    let segments = read_segments(raw, num_segments)?;

    // Container metadata retained at unwrap (doc-5 L1). Kept minimal: the segment
    // count (already read above) and the SELF header's key_type/category u16 at
    // 0x08 as a coarse program/content-type discriminator. The finer module
    // attributes live in the not-yet-parsed SCE program-info header -> None.
    let meta = ContainerMeta {
        num_segments: Some(num_segments as u16),
        sce_program_type: Some(u16::from_le_bytes([raw[0x08], raw[0x09]])),
        module_attributes: None,
    };

    // The inner ELF header + program headers are embedded immediately after the
    // segment table (offset 0x120 for the reference dump: 0x20 + 8*0x20).
    let elf_base = SCE_HEADER_SIZE + num_segments * SELF_SEGMENT_SIZE;
    if raw.len() < elf_base + 4 {
        return Err(ContainerError::TooShort("no room for inner ELF header"));
    }
    let inner_magic = u32::from_le_bytes([
        raw[elf_base],
        raw[elf_base + 1],
        raw[elf_base + 2],
        raw[elf_base + 3],
    ]);
    if inner_magic != ELF_MAGIC {
        // A decrypted SELF exposes a plaintext ELF header here; ciphertext does not.
        // With no crypto in this build, the only honest report is: decrypt first.
        return Err(ContainerError::Encrypted);
    }

    let phdrs = read_program_headers(raw, elf_base)?;

    // Output buffer holds the ELF header, program-header table, and every
    // segment's file image at its p_offset. Size it to the maximum extent.
    let phdr_region_end = {
        let ehdr = &raw[elf_base..elf_base + ELF_HEADER_SIZE];
        let (e_phoff, e_phentsize, e_phnum) = phdr_table_info(ehdr);
        e_phoff as usize + e_phnum * e_phentsize
    };

    let mut out_size = phdr_region_end.max(ELF_HEADER_SIZE);
    for ph in &phdrs {
        let end = (ph.file_offset)
            .checked_add(ph.file_size)
            .ok_or_else(|| ContainerError::Malformed("phdr extent overflow".into()))?;
        out_size = out_size.max(end as usize);
    }

    let mut out = vec![0u8; out_size];

    // Copy the ELF header + program-header table verbatim from the SELF.
    if raw.len() < elf_base + phdr_region_end {
        return Err(ContainerError::TooShort(
            "inner ELF header region truncated",
        ));
    }
    out[..phdr_region_end].copy_from_slice(&raw[elf_base..elf_base + phdr_region_end]);

    // Fill each program header's file image from its backing blocked segment.
    for (seg_idx, seg) in segments.iter().enumerate() {
        if !seg.is_blocked() {
            continue;
        }
        if seg.is_compressed() {
            return Err(ContainerError::Compressed { segment: seg_idx });
        }

        let id = seg.program_header_id();
        let ph = phdrs.get(id).ok_or_else(|| {
            ContainerError::Malformed(format!("segment {seg_idx} targets phdr {id}, out of range"))
        })?;

        let copy_len = seg.uncompressed_size.min(ph.file_size) as usize;
        if copy_len == 0 {
            continue;
        }

        let src_start = seg.offset as usize;
        let src_end = src_start
            .checked_add(copy_len)
            .ok_or_else(|| ContainerError::Malformed("segment source overflow".into()))?;
        if raw.len() < src_end {
            return Err(ContainerError::TooShort(
                "SELF segment data out of file bounds",
            ));
        }

        let dst_start = ph.file_offset as usize;
        let dst_end = dst_start
            .checked_add(copy_len)
            .ok_or_else(|| ContainerError::Malformed("phdr destination overflow".into()))?;
        if out.len() < dst_end {
            return Err(ContainerError::Malformed(
                "phdr destination out of buffer".into(),
            ));
        }

        out[dst_start..dst_end].copy_from_slice(&raw[src_start..src_end]);
    }

    Ok((out, meta))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Wrap arbitrary ELF bytes in a fake SELF mirroring the real container: SCE
    // header, one blocked segment per program header (each pointing at that
    // header's file image), then the inner ELF header + phdr table, then each
    // segment's payload. This reproduces exactly what open() must undo.
    fn build_fake_self(inner_elf: &[u8], encrypted: bool, compressed: bool) -> Vec<u8> {
        let (e_phoff_u64, e_phentsize, e_phnum) = phdr_table_info(inner_elf);
        let e_phoff = e_phoff_u64 as usize;

        struct Ph {
            offset: usize,
            filesz: usize,
        }
        let phdrs: Vec<Ph> = (0..e_phnum)
            .map(|i| {
                let ph = &inner_elf[e_phoff + i * e_phentsize..e_phoff + (i + 1) * e_phentsize];
                let (offset, filesz) = phdr_extent(ph);
                Ph {
                    offset: offset as usize,
                    filesz: filesz as usize,
                }
            })
            .collect();

        let num_segments = e_phnum as u16;
        let elf_base = SCE_HEADER_SIZE + num_segments as usize * SELF_SEGMENT_SIZE;
        let header_region = e_phoff + e_phnum * e_phentsize;

        // Layout: [sce hdr][seg table][elf hdr + phdrs][seg0 payload][seg1 payload]...
        let mut payload_offsets = Vec::with_capacity(e_phnum);
        let mut cursor = elf_base + header_region;
        for ph in &phdrs {
            payload_offsets.push(cursor);
            cursor += ph.filesz;
        }
        let mut out = vec![0u8; cursor];

        // SCE header
        out[0..4].copy_from_slice(&SELF_MAGIC.to_le_bytes());
        // key_type/category at 0x08 — a deterministic non-zero marker so tests can
        // assert the retained sce_program_type round-trips.
        out[0x08..0x0A].copy_from_slice(&0x0101u16.to_le_bytes());
        out[0x18..0x1A].copy_from_slice(&num_segments.to_le_bytes());

        // one blocked segment per program header (id == phdr index)
        for (i, ph) in phdrs.iter().enumerate() {
            let flags: u64 = SELF_SEG_BLOCKED_BIT | ((i as u64) << SELF_SEG_ID_SHIFT);
            let seg_off = SCE_HEADER_SIZE + i * SELF_SEGMENT_SIZE;
            let usize_val = ph.filesz as u64;
            // compressed => make compressed_size differ (only on the first non-empty seg)
            let csize = if compressed && ph.filesz > 0 {
                usize_val.saturating_sub(1).max(1)
            } else {
                usize_val
            };
            out[seg_off..seg_off + 8].copy_from_slice(&flags.to_le_bytes());
            out[seg_off + 8..seg_off + 16]
                .copy_from_slice(&(payload_offsets[i] as u64).to_le_bytes());
            out[seg_off + 16..seg_off + 24].copy_from_slice(&csize.to_le_bytes());
            out[seg_off + 24..seg_off + 32].copy_from_slice(&usize_val.to_le_bytes());
        }

        // inner ELF header + phdr table verbatim
        out[elf_base..elf_base + header_region].copy_from_slice(&inner_elf[..header_region]);
        // SELF-stripped ELFs carry no section-header table; neutralize e_shoff/
        // e_shnum so goblin parses the reconstructed image from program headers.
        out[elf_base + 0x28..elf_base + 0x30].copy_from_slice(&0u64.to_le_bytes()); // e_shoff
        out[elf_base + 0x3C..elf_base + 0x3E].copy_from_slice(&0u16.to_le_bytes()); // e_shnum

        // each segment payload = that phdr's file image
        for (i, ph) in phdrs.iter().enumerate() {
            if ph.filesz > 0 {
                let dst = payload_offsets[i];
                out[dst..dst + ph.filesz]
                    .copy_from_slice(&inner_elf[ph.offset..ph.offset + ph.filesz]);
            }
        }

        if encrypted {
            // scramble the embedded inner-ELF magic to simulate ciphertext
            out[elf_base] ^= 0xFF;
        }

        out
    }

    fn load_example_elf() -> Vec<u8> {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../examples/ps4-helloworld/hello_world.elf"
        );
        std::fs::read(path).expect("example hello_world.elf must be present")
    }

    #[test]
    fn plain_elf_passthrough_is_identical() {
        let elf = load_example_elf();
        let container = open(elf.clone()).expect("plain ELF must pass through");
        assert_eq!(container.kind, ContainerKind::RawElf);
        assert_eq!(container.elf_bytes, elf, "plain ELF must be byte-identical");
        assert_eq!(&container.elf_bytes[0..4], b"\x7FELF");
        assert_eq!(
            container.meta,
            ContainerMeta::default(),
            "plain ELF carries empty container metadata"
        );
    }

    #[test]
    fn self_extraction_yields_parseable_elf() {
        let elf = load_example_elf();
        let fake = build_fake_self(&elf, false, false);
        let container = open(fake).expect("fake SELF must extract");
        assert_eq!(container.kind, ContainerKind::Self_);
        assert_eq!(&container.elf_bytes[0..4], b"\x7FELF", "inner ELF magic");
        let parsed =
            goblin::elf::Elf::parse(&container.elf_bytes).expect("goblin must parse extracted ELF");
        assert!(
            !parsed.program_headers.is_empty(),
            "extracted ELF has program headers"
        );
    }

    #[test]
    fn self_extraction_retains_container_metadata() {
        let elf = load_example_elf();
        let fake = build_fake_self(&elf, false, false);
        let container = open(fake).expect("fake SELF must extract");
        // The SELF path retains metadata rather than discarding it.
        assert!(
            container.meta.num_segments.is_some(),
            "SELF retains segment count"
        );
        assert_eq!(
            container.meta.sce_program_type,
            Some(0x0101),
            "SELF retains the key_type/category discriminator from offset 0x08"
        );
        // module_attributes stays None until a consumer parses the SCE program-info.
        assert_eq!(container.meta.module_attributes, None);
    }

    #[test]
    fn encrypted_self_reports_decrypt_first() {
        let elf = load_example_elf();
        let fake = build_fake_self(&elf, true, false);
        let err = open(fake).expect_err("encrypted SELF must be rejected");
        assert!(matches!(err, ContainerError::Encrypted), "got {err:?}");
        let msg = err.to_string();
        assert!(msg.contains("decrypted first"), "message was: {msg}");
    }

    #[test]
    fn compressed_self_is_rejected() {
        let elf = load_example_elf();
        let fake = build_fake_self(&elf, false, true);
        let err = open(fake).expect_err("compressed SELF must be rejected");
        assert!(
            matches!(err, ContainerError::Compressed { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn unknown_magic_is_rejected() {
        let err = open(vec![0xDE, 0xAD, 0xBE, 0xEF]).expect_err("junk must be rejected");
        assert!(
            matches!(err, ContainerError::UnknownMagic(_)),
            "got {err:?}"
        );
    }

    #[test]
    fn too_short_is_rejected() {
        let err = open(vec![0x7F, 0x45]).expect_err("truncated must be rejected");
        assert!(matches!(err, ContainerError::TooShort(_)), "got {err:?}");
    }

    #[test]
    fn self_with_zero_segments_is_malformed() {
        // A SELF magic + valid-length SCE header but num_segments == 0.
        let mut raw = vec![0u8; SCE_HEADER_SIZE];
        raw[0..4].copy_from_slice(&SELF_MAGIC.to_le_bytes());
        // num_segments (u16 @ 0x18) is already 0.
        let err = open(raw).expect_err("zero-segment SELF must be rejected");
        assert!(matches!(err, ContainerError::Malformed(_)), "got {err:?}");
        assert!(err.to_string().contains("zero segments"), "{err}");
    }

    #[test]
    fn self_with_truncated_segment_table_is_too_short() {
        // SELF header claims 8 segments but the file ends right after the header,
        // so the segment table is truncated.
        let mut raw = vec![0u8; SCE_HEADER_SIZE + SELF_SEGMENT_SIZE]; // room for 1 seg only
        raw[0..4].copy_from_slice(&SELF_MAGIC.to_le_bytes());
        raw[0x18..0x1A].copy_from_slice(&8u16.to_le_bytes());
        let err = open(raw).expect_err("truncated segment table must be rejected");
        assert!(matches!(err, ContainerError::TooShort(_)), "got {err:?}");
        assert!(err.to_string().contains("segment table"), "{err}");
    }

    #[test]
    fn self_segment_targeting_out_of_range_phdr_is_malformed() {
        // Build a valid fake SELF, then rewrite segment 0's flags so its embedded
        // program-header id points past the real phdr count.
        let elf = load_example_elf();
        let mut fake = build_fake_self(&elf, false, false);
        let seg0 = SCE_HEADER_SIZE; // first segment entry, flags at its start
        let bad_id: u64 = 0xFFF; // far beyond any real phdr index
        let flags: u64 = SELF_SEG_BLOCKED_BIT | (bad_id << SELF_SEG_ID_SHIFT);
        fake[seg0..seg0 + 8].copy_from_slice(&flags.to_le_bytes());
        let err = open(fake).expect_err("out-of-range phdr id must be rejected");
        assert!(matches!(err, ContainerError::Malformed(_)), "got {err:?}");
        assert!(err.to_string().contains("out of range"), "{err}");
    }

    // Real-format oracle: local copyrighted dumps (an eboot + several .prx modules
    // across titles). Copyrighted and huge — never committed, never in CI. Each path
    // runs only when present on disk; the test asserts every SELF that IS present
    // unwraps to a goblin-parseable inner ELF (the AC#2 real-format check).
    #[test]
    #[ignore = "requires local copyrighted dumps; manual smoke check only"]
    fn real_dumps_extract_inner_elf() {
        let paths = [
            "/home/mikolaj/PS4/CUSA03173/eboot.bin",
            "/home/mikolaj/PS4/CUSA11302/eboot.bin",
            "/home/mikolaj/PS4/CUSA11302/scePlayStation4.prx",
            "/home/mikolaj/PS4/CUSA11302/libfmod.prx",
            "/home/mikolaj/PS4/CUSA11302/libfmodstudio.prx",
            "/home/mikolaj/PS4/CUSA11302/sce_module/libc.prx",
            "/home/mikolaj/PS4/CUSA11302/sce_module/libSceFios2.prx",
        ];
        let mut checked = 0;
        for path in paths {
            if !std::path::Path::new(path).exists() {
                eprintln!("skipping: {path} not present");
                continue;
            }
            let raw = std::fs::read(path).expect("read dump");
            let container = open(raw).unwrap_or_else(|e| panic!("{path} must extract: {e}"));
            assert_eq!(
                &container.elf_bytes[0..4],
                b"\x7FELF",
                "{path} inner ELF magic"
            );
            let parsed = goblin::elf::Elf::parse(&container.elf_bytes)
                .unwrap_or_else(|e| panic!("{path}: goblin must parse inner ELF: {e}"));
            eprintln!(
                "{path} -> {} bytes, e_type={:#x}, {} phdrs, meta={:?}",
                container.elf_bytes.len(),
                parsed.header.e_type,
                parsed.program_headers.len(),
                container.meta,
            );
            assert!(!parsed.program_headers.is_empty(), "{path} has phdrs");
            checked += 1;
        }
        assert!(checked > 0, "no local dump present to smoke-check");
    }
}
