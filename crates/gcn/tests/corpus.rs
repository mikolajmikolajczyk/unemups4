//! Synthetic GCN shader corpus + OrbShdr wrapper.
//!
//! A small set of hand-written GFX7 / Sea Islands (bonaire) GCN shaders — a
//! pass-through vertex shader and two pixel shaders (flat + interpolating color) —
//! each committed as BOTH `.s` assembly source and the assembled OrbShdr `.sb`
//! blob under `tests/corpus/`. These feed the `.sb` parser (`ps4-gnm`),
//! the GCN decoder/interpreter, and the recompiler tests (phase 4).
//!
//! # No copyrighted assets
//!
//! Every shader here is **self-authored**. None is derived from a game eboot, a
//! dumped `.sb`, or any Sony / OpenOrbis SDK artifact. Do not add corpus entries
//! sourced from copyrighted material.
//!
//! # Container layout (doc-1 §3.3)
//!
//! An OrbShdr `.sb` blob is `[ raw GCN machine code ][ 28-byte ShaderBinaryInfo ]`
//! — the header sits immediately *after* the code. `ShaderBinaryInfo` packs, little
//! endian: `"OrbShdr"` magic (7) + version (1) + a 32-bit bitfield word
//! (`m_pssl_or_cg:1, m_cached:1, m_type:4, m_source_type:2, m_length:24`) +
//! `m_chunkUsageBaseOffsetInDW` (1) + `m_numInputUsageSlots` (1) + a flags byte
//! (`m_isSrt:1, m_isSrtUsedInfoValid:1, m_isExtendedUsageInfo:1`) + reserved (1) +
//! `m_shaderHash0` (4) + `m_shaderHash1` (4) + `m_crc32` (4). `m_length` is the GCN
//! code size in bytes. This mirrors `ps4_gnm::shader::sb`, kept independent here so
//! `ps4-gcn` (which `ps4-gnm` depends on) needs no reverse dependency.
//!
//! # Regenerating the blobs
//!
//! 1. Edit the `.s` source(s).
//! 2. Assemble to raw GCN code bytes (`.code.bin`) with llvm-mc (amdgcn target):
//!
//!    ```text
//!    crates/gcn/tests/corpus/regen.sh
//!    # per shader, that runs:
//!    #   llvm-mc -triple amdgcn -mcpu=bonaire -filetype=asm -show-encoding <name>.s
//!    ```
//!
//!    Where llvm-mc is unavailable the `.code.bin` bytes may be hand-encoded — they
//!    must still be real GCN instruction encodings, not marker bytes.
//! 3. Wrap into the committed `.sb` blobs:
//!
//!    ```text
//!    cargo test -p ps4-gcn --test corpus -- --ignored regen_sb_blobs
//!    ```
//!
//! 4. Commit the updated `.s`, `.code.bin`, and `.sb` together.
//!
//! [`corpus_blobs_match_committed`] then asserts every committed `.sb` is exactly
//! what wrapping its `.code.bin` produces, and [`corpus_headers_are_valid`] asserts
//! the on-disk header integrity (magic / stage / length) — so a stale checked-in
//! blob fails CI.

use std::path::{Path, PathBuf};

/// `ShaderBinaryInfo::m_signature` magic (doc-1 §3.3).
const ORBSHDR_MAGIC: &[u8; 7] = b"OrbShdr";
/// Packed `ShaderBinaryInfo` header size in bytes (doc-1 §3.3).
const HEADER_SIZE: usize = 28;

/// `m_type` stage codes (doc-1 §3.3, matching `ps4_gnm::shader::sb::SbStage`).
const M_TYPE_PIXEL: u32 = 0;
const M_TYPE_VERTEX: u32 = 1;

/// One corpus entry: the base filename, the expected GCN stage, and a hash pair the
/// wrapper stamps into the header. The hashes are arbitrary fixed test constants
/// (not real shader hashes) — they only need to round-trip through the parser.
struct CorpusEntry {
    name: &'static str,
    m_type: u32,
    hash0: u32,
    hash1: u32,
}

/// The committed corpus. Extend by adding a `.s` + `.code.bin` + `.sb` triple and an
/// entry here.
const CORPUS: &[CorpusEntry] = &[
    CorpusEntry {
        name: "passthrough_vs",
        m_type: M_TYPE_VERTEX,
        hash0: 0x5653_5f30, // "VS_0"
        hash1: 0x0000_0001,
    },
    CorpusEntry {
        name: "flat_color_ps",
        m_type: M_TYPE_PIXEL,
        hash0: 0x5053_5f30, // "PS_0"
        hash1: 0x0000_0001,
    },
    CorpusEntry {
        name: "interp_color_ps",
        m_type: M_TYPE_PIXEL,
        hash0: 0x5053_5f31, // "PS_1"
        hash1: 0x0000_0001,
    },
    CorpusEntry {
        name: "texture_sample_ps",
        m_type: M_TYPE_PIXEL,
        hash0: 0x5053_5f32, // "PS_2"
        hash1: 0x0000_0001,
    },
    CorpusEntry {
        name: "pkrtz_ps",
        m_type: M_TYPE_PIXEL,
        hash0: 0x5053_5f33, // "PS_3"
        hash1: 0x0000_0001,
    },
    CorpusEntry {
        name: "wqm_bracket_ps",
        m_type: M_TYPE_PIXEL,
        hash0: 0x5053_5f34, // "PS_4"
        hash1: 0x0000_0001,
    },
    CorpusEntry {
        name: "cbuffer_ps",
        m_type: M_TYPE_PIXEL,
        hash0: 0x5053_5f35, // "PS_5"
        hash1: 0x0000_0001,
    },
    CorpusEntry {
        name: "transcendental_ps",
        m_type: M_TYPE_PIXEL,
        hash0: 0x5053_5f36, // "PS_6"
        hash1: 0x0000_0001,
    },
    CorpusEntry {
        name: "minmax_shift_ps",
        m_type: M_TYPE_PIXEL,
        hash0: 0x5053_5f37, // "PS_7"
        hash1: 0x0000_0001,
    },
    CorpusEntry {
        name: "vop3_mad_sin_fract_ps",
        m_type: M_TYPE_PIXEL,
        hash0: 0x5053_5f38, // "PS_8"
        hash1: 0x0000_0001,
    },
    CorpusEntry {
        name: "vop3_mul_ps",
        m_type: M_TYPE_PIXEL,
        hash0: 0x5053_5f39, // "PS_9"
        hash1: 0x0000_0001,
    },
    CorpusEntry {
        name: "rcp_ps",
        m_type: M_TYPE_PIXEL,
        hash0: 0x5053_5f41, // "PS_A"
        hash1: 0x0000_0001,
    },
    CorpusEntry {
        name: "vop3_mac_ps",
        m_type: M_TYPE_PIXEL,
        hash0: 0x5053_5f42, // "PS_B"
        hash1: 0x0000_0001,
    },
    CorpusEntry {
        name: "vop3_pkrtz_ps",
        m_type: M_TYPE_PIXEL,
        hash0: 0x5053_5f43, // "PS_C"
        hash1: 0x0000_0001,
    },
    CorpusEntry {
        name: "m0_save_ps",
        m_type: M_TYPE_PIXEL,
        hash0: 0x5053_5f44, // "PS_D"
        hash1: 0x0000_0001,
    },
    CorpusEntry {
        name: "cmp_cndmask_ps",
        m_type: M_TYPE_PIXEL,
        hash0: 0x5053_5f45, // "PS_E"
        hash1: 0x0000_0001,
    },
    CorpusEntry {
        name: "vop3_cmp_cndmask_ps",
        m_type: M_TYPE_PIXEL,
        hash0: 0x5053_5f46, // "PS_F"
        hash1: 0x0000_0001,
    },
    CorpusEntry {
        name: "vadd_i32_ps",
        m_type: M_TYPE_PIXEL,
        hash0: 0x5053_5f47, // "PS_G"
        hash1: 0x0000_0001,
    },
    CorpusEntry {
        name: "vop3_cmp3_ps",
        m_type: M_TYPE_PIXEL,
        hash0: 0x5053_5f48, // "PS_H"
        hash1: 0x0000_0001,
    },
    CorpusEntry {
        name: "cbranch_alpha_ps",
        m_type: M_TYPE_PIXEL,
        hash0: 0x5053_5f49, // "PS_I"
        hash1: 0x0000_0001,
    },
    CorpusEntry {
        name: "cbranch_select_ps",
        m_type: M_TYPE_PIXEL,
        hash0: 0x5053_5f4a, // "PS_J"
        hash1: 0x0000_0001,
    },
    CorpusEntry {
        name: "loop_accum_ps",
        m_type: M_TYPE_PIXEL,
        hash0: 0x5053_5f4b, // "PS_K"
        hash1: 0x0000_0001,
    },
    CorpusEntry {
        name: "vop3_clamp_ps",
        m_type: M_TYPE_PIXEL,
        hash0: 0x5053_5f4c, // "PS_L"
        hash1: 0x0000_0001,
    },
    CorpusEntry {
        name: "vop3_clamp_nan_ps",
        m_type: M_TYPE_PIXEL,
        hash0: 0x5053_5f4d, // "PS_M"
        hash1: 0x0000_0001,
    },
    CorpusEntry {
        name: "inline_fetch_vs",
        m_type: M_TYPE_VERTEX,
        hash0: 0x5653_5f31, // "VS_1"
        hash1: 0x0000_0001,
    },
    CorpusEntry {
        name: "cbuffer16_vs",
        m_type: M_TYPE_VERTEX,
        hash0: 0x5653_5f32, // "VS_2"
        hash1: 0x0000_0001,
    },
];

fn corpus_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus")
}

/// Wrap raw GCN `code` in a complete OrbShdr `.sb` blob: `[ code ][ 28-byte header ]`.
/// This is the single source of truth for the corpus header layout.
fn wrap_orbshdr(code: &[u8], entry: &CorpusEntry) -> Vec<u8> {
    let code_len = u32::try_from(code.len()).expect("corpus code fits in 24-bit m_length");
    assert!(code_len < (1 << 24), "m_length is 24-bit");

    let word: u32 = ((entry.m_type & 0xF) << 2) | ((code_len & 0x00FF_FFFF) << 8);

    let mut blob = Vec::with_capacity(code.len() + HEADER_SIZE);
    blob.extend_from_slice(code);
    blob.extend_from_slice(ORBSHDR_MAGIC);
    blob.push(1); // m_version
    blob.extend_from_slice(&word.to_le_bytes());
    blob.push(0x00); // m_chunkUsageBaseOffsetInDW (no input-usage table)
    blob.push(0x00); // m_numInputUsageSlots
    blob.push(0x00); // flags (not SRT)
    blob.push(0x00); // m_reserved3
    blob.extend_from_slice(&entry.hash0.to_le_bytes());
    blob.extend_from_slice(&entry.hash1.to_le_bytes());
    blob.extend_from_slice(&crc32(code).to_le_bytes());
    debug_assert_eq!(blob.len(), code.len() + HEADER_SIZE);
    blob
}

/// CRC-32 (IEEE, reflected) of the GCN code — stamped into `m_crc32` so the header
/// carries a genuine checksum rather than a placeholder.
fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

fn read_code_bin(name: &str) -> Vec<u8> {
    let p = corpus_dir().join(format!("{name}.code.bin"));
    std::fs::read(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
}

fn read_sb(name: &str) -> Vec<u8> {
    let p = corpus_dir().join(format!("{name}.sb"));
    std::fs::read(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
}

/// Regenerate the committed `.sb` blobs from the `.code.bin` files. Ignored by
/// default (it writes into the source tree); run explicitly after `regen.sh`:
///
/// ```text
/// cargo test -p ps4-gcn --test corpus -- --ignored regen_sb_blobs
/// ```
#[test]
#[ignore = "writes committed .sb blobs; run after editing shaders"]
fn regen_sb_blobs() {
    for entry in CORPUS {
        let code = read_code_bin(entry.name);
        let blob = wrap_orbshdr(&code, entry);
        let out = corpus_dir().join(format!("{}.sb", entry.name));
        std::fs::write(&out, &blob).unwrap_or_else(|e| panic!("write {}: {e}", out.display()));
        eprintln!("wrote {} ({} bytes)", out.display(), blob.len());
    }
}

/// AC #2: the committed `.sb` blobs are exactly what wrapping their `.code.bin`
/// produces — a stale or hand-edited blob fails here.
#[test]
fn corpus_blobs_match_committed() {
    for entry in CORPUS {
        let code = read_code_bin(entry.name);
        let expected = wrap_orbshdr(&code, entry);
        let committed = read_sb(entry.name);
        assert_eq!(
            committed, expected,
            "{}.sb is stale — run regen.sh then `--ignored regen_sb_blobs`",
            entry.name
        );
    }
}

/// AC #2: on-disk header integrity — magic, stage (`m_type`), and length
/// (`m_length` == committed GCN code size) match the assembled shader.
#[test]
fn corpus_headers_are_valid() {
    for entry in CORPUS {
        let code = read_code_bin(entry.name);
        let blob = read_sb(entry.name);

        assert!(
            !code.is_empty() && code.len().is_multiple_of(4),
            "{}: GCN code must be non-empty, 4-byte-aligned instructions",
            entry.name
        );
        assert_eq!(
            blob.len(),
            code.len() + HEADER_SIZE,
            "{}: blob is not [code][28-byte header]",
            entry.name
        );

        let header = &blob[code.len()..];
        assert_eq!(&header[0..7], ORBSHDR_MAGIC, "{}: bad magic", entry.name);
        assert_eq!(header[7], 1, "{}: unexpected m_version", entry.name);

        let word = u32::from_le_bytes([header[8], header[9], header[10], header[11]]);
        let m_type = (word >> 2) & 0xF;
        let m_length = (word >> 8) & 0x00FF_FFFF;
        assert_eq!(m_type, entry.m_type, "{}: wrong m_type", entry.name);
        assert_eq!(
            m_length as usize,
            code.len(),
            "{}: m_length != assembled code size",
            entry.name
        );

        let crc = u32::from_le_bytes([header[24], header[25], header[26], header[27]]);
        assert_eq!(crc, crc32(&code), "{}: m_crc32 mismatch", entry.name);
    }
}

/// AC #4 (self-check): the GCN code ends in `s_endpgm` (`[0x00,0x00,0x81,0xbf]`), so
/// the wrapped blob is a genuinely terminating program, not a marker payload. A full
/// on-hardware acceptance check is deferred; this guards the corpus stays real GCN.
#[test]
fn corpus_code_terminates_with_s_endpgm() {
    const S_ENDPGM: [u8; 4] = [0x00, 0x00, 0x81, 0xbf];
    for entry in CORPUS {
        let code = read_code_bin(entry.name);
        let tail = &code[code.len() - 4..];
        assert_eq!(
            tail, S_ENDPGM,
            "{}: GCN code must end in s_endpgm",
            entry.name
        );
    }
}
