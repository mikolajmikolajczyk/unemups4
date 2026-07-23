//! Decode a dumped real-hardware DCB and run the task-157 atlas analysis.
//!
//! Feeds a captured DCB `.bin` (written by `receiver`, already de-RLE'd) through
//! the real [`ps4_gnm::pm4`] decoder — the exact same path our own-capture recon
//! used — and reports:
//!   * every `SET_SH_REG` write to the target absolute reg (default 0x2c0c: the
//!     PS user-data slot the atlas T# lands in), with its value dwords,
//!   * every PS program bind (`SPI_SHADER_PGM_LO_PS` / `HI_PS`),
//!   * the draw packets,
//!   * a raw-byte scan for the atlas base address (default 0x9afc28000).
//!
//! DECISIVE QUESTION: does a real steady-state Celeste flip DCB contain the
//! 8-dword atlas 0x2c0c bind every frame? The final summary answers it.
//!
//! Usage:
//!   decode <file.bin> [ATLAS_BASE_HEX] [TARGET_SH_REG_HEX]
//!     ATLAS_BASE_HEX     default 0x9afc28000
//!     TARGET_SH_REG_HEX  default 0x2c0c  (absolute SH-window dword index)

use std::process::ExitCode;

use ps4_gnm::pm4::decode::{OwnedPacket, decode_bytes};
use ps4_gnm::pm4::opcodes::{op, reg_base, sh_reg};

fn parse_hex(s: &str) -> Option<u64> {
    let s = s.trim().trim_start_matches("0x").trim_start_matches("0X");
    u64::from_str_radix(s, 16).ok()
}

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let Some(path) = args.next() else {
        eprintln!("usage: decode <file.bin> [ATLAS_BASE_HEX] [TARGET_SH_REG_HEX]");
        return ExitCode::from(2);
    };
    let atlas_base = args
        .next()
        .and_then(|s| parse_hex(&s))
        .unwrap_or(0x9_afc2_8000);
    let target_reg = args.next().and_then(|s| parse_hex(&s)).unwrap_or(0x2c0c) as u32;

    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("cannot read {path}: {e}");
            return ExitCode::from(1);
        }
    };

    println!(
        "== decode {path} ({} bytes, {} dwords) ==",
        bytes.len(),
        bytes.len() / 4
    );
    println!(
        "   target SH reg = {target_reg:#06x} (SH base {:#06x} + off {:#x}); atlas base = {atlas_base:#x}",
        reg_base::SH,
        target_reg.wrapping_sub(reg_base::SH)
    );
    println!();

    let packets = decode_bytes(&bytes);

    let mut set_sh_reg_total = 0usize;
    let mut target_writes = 0usize;
    let mut target_8dword_writes = 0usize;
    let mut ps_pgm_binds = 0usize;
    let mut draws = 0usize;
    let mut truncated = false;

    let verbose = std::env::var("DECODE_VERBOSE")
        .map(|v| v != "0" && !v.is_empty())
        .unwrap_or(false);
    for (i, p) in packets.iter().enumerate() {
        if verbose {
            match p {
                OwnedPacket::Type3 {
                    opcode,
                    count,
                    body,
                } => {
                    let reg = if *opcode == op::IT_SET_SH_REG || *opcode == op::IT_SET_CONTEXT_REG {
                        body.first()
                            .map(|off| {
                                let base = if *opcode == op::IT_SET_SH_REG {
                                    reg_base::SH
                                } else {
                                    reg_base::CONTEXT
                                };
                                format!(
                                    " reg={:#06x} nval={}",
                                    base + off,
                                    body.len().saturating_sub(1)
                                )
                            })
                            .unwrap_or_default()
                    } else {
                        String::new()
                    };
                    println!("[v {i:4}] T3 {} count={count}{reg}", op_name(*opcode));
                }
                OwnedPacket::Type0 {
                    base_index, count, ..
                } => println!("[v {i:4}] T0 base={base_index:#x} count={count}"),
                OwnedPacket::Type2 => println!("[v {i:4}] T2 NOP"),
                OwnedPacket::Truncated { header } => println!("[v {i:4}] TRUNCATED {header:#010x}"),
            }
        }
        match p {
            OwnedPacket::Type3 { opcode, body, .. } => {
                let opname = op_name(*opcode);
                if *opcode == op::IT_SET_SH_REG {
                    set_sh_reg_total += 1;
                    // body[0] = reg offset relative to SH base; body[1..] = values,
                    // one per consecutive register starting at that offset.
                    if let Some((&reg_off, values)) = body.split_first() {
                        let base_abs = reg_base::SH + reg_off;
                        let nvals = values.len();
                        // Does this run cover the target register?
                        let covers_target =
                            target_reg >= base_abs && (target_reg - base_abs) < nvals.max(1) as u32;
                        let is_ps_pgm = (base_abs..base_abs + nvals as u32).any(|r| {
                            r == sh_reg::SPI_SHADER_PGM_LO_PS || r == sh_reg::SPI_SHADER_PGM_HI_PS
                        });
                        if base_abs == target_reg {
                            target_writes += 1;
                            if nvals == 8 {
                                target_8dword_writes += 1;
                            }
                            println!(
                                "[pkt {i:4}] SET_SH_REG @ {base_abs:#06x}  <== TARGET  ({nvals} value dwords){}",
                                if nvals == 8 { "  [8-dword T#]" } else { "" }
                            );
                            print_dwords("            ", values);
                            decode_tsharp_if(values, atlas_base);
                        } else if covers_target {
                            target_writes += 1;
                            println!(
                                "[pkt {i:4}] SET_SH_REG @ {base_abs:#06x}..{:#06x}  covers TARGET {target_reg:#06x} ({nvals} dwords)",
                                base_abs + nvals as u32 - 1
                            );
                            print_dwords("            ", values);
                        }
                        if is_ps_pgm {
                            ps_pgm_binds += 1;
                            println!(
                                "[pkt {i:4}] SET_SH_REG @ {base_abs:#06x}  PS PGM bind (LO/HI_PS) ({nvals} dwords)"
                            );
                            print_dwords("            ", values);
                        }
                    }
                } else if is_draw(*opcode) {
                    draws += 1;
                    println!("[pkt {i:4}] {opname}  (draw)");
                }
            }
            OwnedPacket::Type0 {
                base_index, count, ..
            } => {
                // Type-0 register runs are rare on GFX6 Gnm buffers, but flag any
                // that touch the target register window.
                let start = *base_index as u32;
                let end = start + *count as u32;
                if (start..end).contains(&target_reg) {
                    println!("[pkt {i:4}] Type0 reg run {start:#06x}..{end:#06x} covers TARGET");
                }
            }
            OwnedPacket::Truncated { header } => {
                truncated = true;
                println!("[pkt {i:4}] TRUNCATED header {header:#010x} — decode stopped");
            }
            OwnedPacket::Type2 => {}
        }
    }

    // Raw-byte scan for the atlas base address, independent of PM4 structure.
    println!();
    println!("== raw-byte atlas-base scan ==");
    let hits = scan_atlas(&bytes, atlas_base);
    if hits == 0 {
        println!("   no atlas-base byte pattern found anywhere in the buffer");
    }

    println!();
    println!("== summary ==");
    println!("   packets decoded         : {}", packets.len());
    println!("   SET_SH_REG total        : {set_sh_reg_total}");
    println!("   writes to {target_reg:#06x}        : {target_writes}");
    println!("   8-dword T# @ {target_reg:#06x}     : {target_8dword_writes}");
    println!("   PS PGM binds            : {ps_pgm_binds}");
    println!("   draw packets            : {draws}");
    if truncated {
        println!("   NOTE: buffer decode hit a truncated/garbage header (see above).");
    }
    println!();
    let verdict = if target_8dword_writes > 0 {
        "PRESENT: this DCB re-emits the 8-dword atlas bind."
    } else if target_writes > 0 {
        "PARTIAL: this DCB writes the target reg, but not as an 8-dword T# (check dword counts above)."
    } else {
        "ABSENT: this DCB contains NO write to the target SH reg."
    };
    println!("   VERDICT: {verdict}");
    ExitCode::SUCCESS
}

fn op_name(opcode: u8) -> String {
    ps4_gnm::pm4::opcodes::name(opcode)
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("IT_UNKNOWN_{opcode:#04x}"))
}

fn is_draw(opcode: u8) -> bool {
    opcode == op::IT_DRAW_INDEX_AUTO
        || opcode == op::IT_DRAW_INDEX_2
        || opcode == op::IT_DRAW_INDEX_OFFSET_2
}

fn print_dwords(indent: &str, dwords: &[u32]) {
    for chunk in dwords.chunks(4) {
        let hex: Vec<String> = chunk.iter().map(|d| format!("{d:08x}")).collect();
        println!("{indent}{}", hex.join(" "));
    }
}

/// A GCN T# (image descriptor) is 8 dwords; word0 (+low 8 bits of word1) encode
/// `base_addr >> 8`. If `values` is an 8-dword run, check whether word0 matches
/// the expected atlas base and print the decoded base.
fn decode_tsharp_if(values: &[u32], atlas_base: u64) {
    if values.len() != 8 {
        return;
    }
    let word0 = values[0] as u64;
    let word1 = values[1] as u64;
    // GFX6 T#: base = ((word1 & 0xFF) << 32 | word0) << 8.
    let decoded_base = (((word1 & 0xFF) << 32) | word0) << 8;
    let expected_word0 = ((atlas_base >> 8) & 0xFFFF_FFFF) as u32;
    let matches = decoded_base == atlas_base;
    println!(
        "            -> T# base_addr = {decoded_base:#x} (word0={:#010x}, expected word0 for atlas = {expected_word0:#010x}){}",
        values[0],
        if matches {
            "  ***MATCHES ATLAS BASE***"
        } else {
            ""
        }
    );
}

/// Scan raw bytes for encodings of `atlas_base`. Reports every offset that
/// matches any candidate needle. Returns the number of hits.
fn scan_atlas(bytes: &[u8], atlas_base: u64) -> usize {
    // Candidate encodings of the atlas base as it may appear on the wire:
    //  A) T# word0 = (base >> 8) as u32 LE — the canonical GCN image-descriptor form.
    //  B) the raw base as a little-endian u64, trimmed of trailing zero bytes.
    //  C) the literal pattern named in task-168 (`00 28 fc 09`), kept verbatim so
    //     a mismatch between our model and the ticket's note is still surfaced.
    let word0 = ((atlas_base >> 8) & 0xFFFF_FFFF) as u32;
    let needle_a = word0.to_le_bytes().to_vec();

    let raw = atlas_base.to_le_bytes();
    let sig_len = raw
        .iter()
        .rposition(|&b| b != 0)
        .map(|i| i + 1)
        .unwrap_or(1);
    let needle_b = raw[..sig_len].to_vec();

    let needle_c = vec![0x00u8, 0x28, 0xfc, 0x09];

    let needles: [(&str, &[u8]); 3] = [
        ("A: T# word0 (base>>8) LE", &needle_a),
        ("B: raw base LE", &needle_b),
        ("C: task-168 literal 00 28 fc 09", &needle_c),
    ];

    let mut hits = 0;
    for (label, needle) in needles {
        let mut found = Vec::new();
        if !needle.is_empty() && needle.len() <= bytes.len() {
            for off in 0..=bytes.len() - needle.len() {
                if &bytes[off..off + needle.len()] == needle {
                    found.push(off);
                }
            }
        }
        let bytes_str: Vec<String> = needle.iter().map(|b| format!("{b:02x}")).collect();
        if found.is_empty() {
            println!("   [{label}] ({}) : not found", bytes_str.join(" "));
        } else {
            hits += found.len();
            let shown: Vec<String> = found.iter().take(8).map(|o| format!("{o:#x}")).collect();
            println!(
                "   [{label}] ({}) : {} hit(s) at {}{}",
                bytes_str.join(" "),
                found.len(),
                shown.join(", "),
                if found.len() > 8 { ", ..." } else { "" }
            );
        }
    }
    hits
}
