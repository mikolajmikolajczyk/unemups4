//! Verify the synthetic GCN corpus loads through the real `.sb` parser.
//!
//! The corpus (`.s` sources + assembled OrbShdr `.sb` blobs) lives in the `ps4-gcn`
//! crate (`crates/gcn/tests/corpus/`), where it is built + header-checked. Because
//! `ps4-gnm` depends on `ps4-gcn` (never the reverse), the check that each blob is
//! loadable by [`ps4_gnm::shader::sb::parse_sb`] lives here — this is the only place
//! the parser and the corpus meet without a dependency cycle.

use ps4_core::bounded_read::BoundedRead;
use ps4_gnm::shader::sb::{SbStage, parse_sb};

/// Guest base the corpus blobs are loaded at for the parse (arbitrary, 256-aligned
/// like a real `SPI_SHADER_PGM_*` code start).
const BASE: u64 = 0x0020_0000;

/// The corpus blobs, resolved relative to this crate's manifest.
fn corpus_sb(name: &str) -> Vec<u8> {
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../gcn/tests/corpus")
        .join(format!("{name}.sb"));
    std::fs::read(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
}

/// A flat backing-buffer bounded reader: guest addr == `base + index`. `read_ranged` is
/// bounds-checked so an over-read is a clean fault — the minimal [`BoundedRead`] seam the
/// parser takes (the parser only reads; no `VirtualMemoryManager` boilerplate needed).
struct BufMem {
    base: u64,
    buf: Vec<u8>,
}

impl BoundedRead for BufMem {
    fn read_ranged(&self, addr: u64, size: usize) -> Result<Vec<u8>, &'static str> {
        let start = addr
            .checked_sub(self.base)
            .ok_or("Invalid memory address (segfault)")? as usize;
        let end = start
            .checked_add(size)
            .ok_or("Invalid memory address (segfault)")?;
        if end > self.buf.len() {
            return Err("Invalid memory address (segfault)");
        }
        Ok(self.buf[start..end].to_vec())
    }
}

/// AC #1: each committed corpus blob parses to the expected stage, with a code
/// range whose length equals the blob's `m_length` (header immediately follows).
#[test]
fn corpus_blobs_parse_via_parse_sb() {
    for (name, want_stage) in [
        ("passthrough_vs", SbStage::Vertex),
        ("flat_color_ps", SbStage::Pixel),
        ("interp_color_ps", SbStage::Pixel),
    ] {
        let blob = corpus_sb(name);
        let code_len = blob.len() - 28; // blob is [code][28-byte header]
        let mem = BufMem {
            base: BASE,
            buf: blob,
        };

        let sb = parse_sb(BASE, &mem).unwrap_or_else(|e| panic!("{name}: parse_sb failed: {e}"));
        assert_eq!(sb.stage, want_stage, "{name}: wrong stage");
        assert_eq!(
            sb.info.code_len as usize, code_len,
            "{name}: m_length != code region"
        );
        assert_eq!(
            sb.code_range,
            BASE..(BASE + code_len as u64),
            "{name}: wrong code range"
        );
        assert!(sb.semantics.vertex_inputs.is_empty());
    }
}
