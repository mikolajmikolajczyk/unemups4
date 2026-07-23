//! THROWAWAY (task-172 Phase 1): extract, from a real-HW DCB dump, the referenced
//! dynamic-buffer addresses the draws use — the guest addresses the splash animation
//! lives at. Reuses `ps4_gnm::pm4` (the same PM4 decoder) + `ps4_gnm::vbuf::decode_v_sharp`
//! (the same V# decode the draw path uses).
//!
//! What a DCB *contains* (the V# itself is loaded from guest memory the plugin will read,
//! not present in the DCB), so this reports two things per flip:
//!   1. INLINE V# candidates — any 4-consecutive-dword window that decodes to a V# whose
//!      base is in the guest-heap band, plausible stride, sane num_records. Gnm often
//!      pushes small dynamic vertex-stream V#s straight into the user-data block.
//!   2. GUEST-HEAP POINTERS in the VS/PS user-data (SPI_SHADER_USER_DATA_{VS,PS}_*) — these
//!      point at descriptor sets / vertex-buffer tables / constant (uniform) buffers.
//!
//! Usage: vref <file.bin>   (emits CSV-ish lines; drive it across the corpus with a loop)

use ps4_gnm::pm4::decode::{OwnedPacket, decode_bytes};
use ps4_gnm::pm4::opcodes::{op, reg_base, sh_reg};
use ps4_gnm::vbuf::decode_v_sharp;

const HEAP_LO: u64 = 0x1_0000_0000; // >4GB: real-HW guest heap spans 0x2xx (bufs) + 0x9xx (atlas)
const HEAP_HI: u64 = 0x10_0000_0000;

fn is_draw(o: u8) -> bool {
    o == op::IT_DRAW_INDEX_AUTO || o == op::IT_DRAW_INDEX_2 || o == op::IT_DRAW_INDEX_OFFSET_2
}

fn in_heap(v: u64) -> bool {
    (HEAP_LO..HEAP_HI).contains(&v)
}

fn main() {
    let path = std::env::args().nth(1).expect("usage: vref <file.bin>");
    let name = std::path::Path::new(&path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    let frame = name
        .find("frame")
        .map(|i| i + 5)
        .and_then(|i| name.get(i..i + 6))
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(u64::MAX);
    let bytes = std::fs::read(&path).expect("read");
    let packets = decode_bytes(&bytes);

    // Track the running shadow of the VS and PS user-data blocks (16 slots each) as
    // SET_SH_REG writes flow, plus draws, so pointers are attributed to their draw.
    let vs0 = sh_reg::SPI_SHADER_USER_DATA_VS_0;
    let ps0 = sh_reg::SPI_SHADER_USER_DATA_PS_0;
    let mut vs_ud = [0u32; 16];
    let mut ps_ud = [0u32; 16];
    let mut vs_pgm = (0u32, 0u32);
    let mut ps_pgm = (0u32, 0u32);
    let mut draw_idx = 0usize;

    for p in &packets {
        if let OwnedPacket::Type3 { opcode, body, .. } = p {
            if *opcode == op::IT_SET_SH_REG {
                if let Some((&off, vals)) = body.split_first() {
                    let base = reg_base::SH + off;
                    for (i, v) in vals.iter().enumerate() {
                        let r = base + i as u32;
                        if (vs0..vs0 + 16).contains(&r) {
                            vs_ud[(r - vs0) as usize] = *v;
                        } else if (ps0..ps0 + 16).contains(&r) {
                            ps_ud[(r - ps0) as usize] = *v;
                        }
                        if r == sh_reg::SPI_SHADER_PGM_LO_VS {
                            vs_pgm.0 = *v;
                        } else if r == sh_reg::SPI_SHADER_PGM_HI_VS {
                            vs_pgm.1 = *v;
                        } else if r == sh_reg::SPI_SHADER_PGM_LO_PS {
                            ps_pgm.0 = *v;
                        } else if r == sh_reg::SPI_SHADER_PGM_HI_PS {
                            ps_pgm.1 = *v;
                        }
                    }
                }
            } else if is_draw(*opcode) {
                let vspgm = ((vs_pgm.1 as u64) << 32) | vs_pgm.0 as u64;
                let pspgm = ((ps_pgm.1 as u64) << 32) | ps_pgm.0 as u64;
                // Pointers in the user-data blocks (SGPR pairs → 48-bit ptr).
                emit_ptrs(frame, draw_idx, "VS", vspgm, pspgm, &vs_ud);
                emit_ptrs(frame, draw_idx, "PS", vspgm, pspgm, &ps_ud);
                // Inline V# candidates inside the VS user-data block.
                emit_inline_vsharps(frame, draw_idx, "VSUD", &vs_ud);
                draw_idx += 1;
            }
        }
    }

    // Whole-DCB inline V# scan (catches V#s the driver embedded as data, not via user-data).
    let words: Vec<u32> = bytes
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();
    let mut seen = std::collections::BTreeSet::new();
    for w in words.windows(4) {
        let d = decode_v_sharp([w[0], w[1], w[2], w[3]]);
        if in_heap(d.base)
            && d.stride > 0
            && d.stride <= 256
            && d.num_records > 0
            && d.num_records < 1_000_000
            && seen.insert((d.base, d.stride, d.num_records))
        {
            println!(
                "{frame},DCBVSHARP,base={:#x},stride={},num_records={},span={}",
                d.base,
                d.stride,
                d.num_records,
                d.byte_span()
            );
        }
    }
}

fn emit_ptrs(frame: u64, draw: usize, stage: &str, vspgm: u64, pspgm: u64, ud: &[u32; 16]) {
    // SGPR pairs: slot i (low) + slot i+1 (high) form a 48-bit pointer.
    for i in 0..15 {
        let lo = ud[i] as u64;
        let hi = ud[i + 1] as u64;
        let ptr = lo | (hi << 32);
        if in_heap(ptr) {
            println!(
                "{frame},PTR,draw={draw},stage={stage},slot={i},ptr={ptr:#x},vspgm={vspgm:#x},pspgm={pspgm:#x}"
            );
        }
    }
}

fn emit_inline_vsharps(frame: u64, draw: usize, tag: &str, ud: &[u32; 16]) {
    for i in 0..13 {
        let d = decode_v_sharp([ud[i], ud[i + 1], ud[i + 2], ud[i + 3]]);
        if in_heap(d.base)
            && d.stride > 0
            && d.stride <= 256
            && d.num_records > 0
            && d.num_records < 1_000_000
        {
            println!(
                "{frame},INLINEV,draw={draw},{tag},slot={i},base={:#x},stride={},num_records={},span={}",
                d.base,
                d.stride,
                d.num_records,
                d.byte_span()
            );
        }
    }
}
