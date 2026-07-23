//! THROWAWAY (task-170): per-flip signature extractor over a directory of DCB
//! dumps. Emits one CSV-ish line per `*_dcb.bin`, ordered by frame index, so the
//! real-HW timeline and our-emulator timeline can be diffed for the intro-loop.
//!
//! Usage: sig <dir-with-*_dcb.bin>
//!
//! Signature fields per flip:
//!   frame, kind(flip|flipwl|other), bytes, ndraw, idx_total, n_sh, n_ctx,
//!   vs_pgm, ps_pgm, ctx_hash, sh_hash, full_hash
//! plus a decoded viewport (xscale,yscale,xoff,yoff) when present — the zoom
//! animation would show up here if it is programmed via the viewport transform.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::Path;

use ps4_gnm::pm4::decode::{OwnedPacket, decode_bytes};
use ps4_gnm::pm4::opcodes::{context_reg, op, reg_base, sh_reg};

fn h(data: &[u32]) -> u64 {
    let mut s = DefaultHasher::new();
    data.hash(&mut s);
    s.finish()
}

fn is_draw(o: u8) -> bool {
    o == op::IT_DRAW_INDEX_AUTO || o == op::IT_DRAW_INDEX_2 || o == op::IT_DRAW_INDEX_OFFSET_2
}

struct Sig {
    frame: u64,
    kind: String,
    bytes: usize,
    ndraw: usize,
    idx_total: u64,
    n_sh: usize,
    n_ctx: usize,
    vs_pgm: u64,
    ps_pgm: u64,
    ctx_hash: u64,
    sh_hash: u64,
    full_hash: u64,
    vp: Option<(f32, f32, f32, f32)>,
}

fn analyze(frame: u64, kind: String, bytes: &[u8]) -> Sig {
    let packets = decode_bytes(bytes);
    let mut ndraw = 0usize;
    let mut idx_total = 0u64;
    let mut n_sh = 0usize;
    let mut n_ctx = 0usize;
    let mut vs_lo = 0u32;
    let mut vs_hi = 0u32;
    let mut ps_lo = 0u32;
    let mut ps_hi = 0u32;
    let mut ctx_words: Vec<u32> = Vec::new();
    let mut sh_words: Vec<u32> = Vec::new();
    let mut vp_xs = None;
    let mut vp_ys = None;
    let mut vp_xo = None;
    let mut vp_yo = None;

    for p in &packets {
        if let OwnedPacket::Type3 { opcode, body, .. } = p {
            if *opcode == op::IT_SET_SH_REG {
                n_sh += 1;
                if let Some((&off, vals)) = body.split_first() {
                    let base = reg_base::SH + off;
                    sh_words.push(base);
                    sh_words.extend_from_slice(vals);
                    for (i, v) in vals.iter().enumerate() {
                        let r = base + i as u32;
                        if r == sh_reg::SPI_SHADER_PGM_LO_VS {
                            vs_lo = *v;
                        } else if r == sh_reg::SPI_SHADER_PGM_HI_VS {
                            vs_hi = *v;
                        } else if r == sh_reg::SPI_SHADER_PGM_LO_PS {
                            ps_lo = *v;
                        } else if r == sh_reg::SPI_SHADER_PGM_HI_PS {
                            ps_hi = *v;
                        }
                    }
                }
            } else if *opcode == op::IT_SET_CONTEXT_REG {
                n_ctx += 1;
                if let Some((&off, vals)) = body.split_first() {
                    let base = reg_base::CONTEXT + off;
                    ctx_words.push(base);
                    ctx_words.extend_from_slice(vals);
                    for (i, v) in vals.iter().enumerate() {
                        let r = base + i as u32;
                        if r == context_reg::PA_CL_VPORT_XSCALE {
                            vp_xs = Some(f32::from_bits(*v));
                        } else if r == context_reg::PA_CL_VPORT_YSCALE {
                            vp_ys = Some(f32::from_bits(*v));
                        } else if r == context_reg::PA_CL_VPORT_XOFFSET {
                            vp_xo = Some(f32::from_bits(*v));
                        } else if r == context_reg::PA_CL_VPORT_YOFFSET {
                            vp_yo = Some(f32::from_bits(*v));
                        }
                    }
                }
            } else if is_draw(*opcode) {
                ndraw += 1;
                // DRAW_INDEX_AUTO body: [index_count, init]; DRAW_INDEX_2 body:
                // [max_size, addr_lo, addr_hi, index_count, init]; OFFSET_2:
                // [max, offset, count, init]. Grab the plausible count word.
                let cnt = match *opcode {
                    x if x == op::IT_DRAW_INDEX_AUTO => body.first().copied().unwrap_or(0),
                    x if x == op::IT_DRAW_INDEX_2 => body.get(3).copied().unwrap_or(0),
                    x if x == op::IT_DRAW_INDEX_OFFSET_2 => body.get(2).copied().unwrap_or(0),
                    _ => 0,
                };
                idx_total += cnt as u64;
            }
        }
    }

    let vp = match (vp_xs, vp_ys, vp_xo, vp_yo) {
        (Some(a), Some(b), Some(c), Some(d)) => Some((a, b, c, d)),
        _ => None,
    };

    Sig {
        frame,
        kind,
        bytes: bytes.len(),
        ndraw,
        idx_total,
        n_sh,
        n_ctx,
        vs_pgm: ((vs_hi as u64) << 32) | vs_lo as u64,
        ps_pgm: ((ps_hi as u64) << 32) | ps_lo as u64,
        ctx_hash: h(&ctx_words),
        sh_hash: h(&sh_words),
        full_hash: {
            let words: Vec<u32> = bytes
                .chunks_exact(4)
                .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                .collect();
            h(&words)
        },
        vp,
    }
}

fn main() {
    let dir = std::env::args().nth(1).expect("usage: sig <dir>");
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .expect("read_dir")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.ends_with("_dcb.bin"))
                .unwrap_or(false)
        })
        .collect();
    files.sort();

    println!(
        "frame,kind,bytes,ndraw,idx_total,n_sh,n_ctx,vs_pgm,ps_pgm,ctx_hash,sh_hash,full_hash,vp_xs,vp_ys,vp_xo,vp_yo"
    );
    for f in &files {
        let name = f.file_name().and_then(|n| n.to_str()).unwrap_or("");
        // "frameNNNNNN" or "ourframeNNNNNN"; take the 6 digits after "frame".
        let frame = name
            .find("frame")
            .map(|i| i + 5)
            .and_then(|i| name.get(i..i + 6))
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(u64::MAX);
        // real corpus: flip/flipwl; our dump: flip/nonflip.
        let kind = if name.contains("flipwl") {
            "flipwl"
        } else if name.contains("nonflip") {
            "nonflip"
        } else if name.contains("flip") {
            "flip"
        } else {
            "other"
        }
        .to_string();
        let bytes = std::fs::read(f).unwrap_or_default();
        let s = analyze(frame, kind, &bytes);
        let (xs, ys, xo, yo) = s.vp.unwrap_or((0.0, 0.0, 0.0, 0.0));
        println!(
            "{},{},{},{},{},{},{},{:#x},{:#x},{:016x},{:016x},{:016x},{},{},{},{}",
            s.frame,
            s.kind,
            s.bytes,
            s.ndraw,
            s.idx_total,
            s.n_sh,
            s.n_ctx,
            s.vs_pgm,
            s.ps_pgm,
            s.ctx_hash,
            s.sh_hash,
            s.full_hash,
            xs,
            ys,
            xo,
            yo,
        );
    }
    eprintln!("{} flips from {}", files.len(), Path::new(&dir).display());
}
