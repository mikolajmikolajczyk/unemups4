//! Human-readable PM4 disassembly of a captured DCB/CCB `.bin`.
//!
//! Walks a dumped command buffer through the real [`ps4_gnm::pm4`] decoder — the
//! same clean decoder the emulator uses — and prints every packet in order:
//! Type-3 opcode names, `SET_*_REG` runs resolved to register names (via
//! [`ps4_gnm::pm4::opcodes::reg_name`]) with their value dwords, draws with their
//! arguments, and collapsed `IT_NOP` / Type-2 filler runs. This is the readable
//! view of the console ground truth — use it to see, by eye, what gnmx actually
//! emits (shader-set SH runs, the trailing NOP pad, the draw packets).
//!
//! Usage:
//!   dcbdump <file.bin> [--limit N]
//!     --limit N   stop after N packets (default: all)

use std::process::ExitCode;

use ps4_gnm::pm4::decode::{OwnedPacket, decode_bytes};
use ps4_gnm::pm4::opcodes::{name as op_name, reg_name, set_reg_base};

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let Some(path) = args.next() else {
        eprintln!("usage: dcbdump <file.bin> [--limit N]");
        return ExitCode::from(2);
    };
    let mut limit = usize::MAX;
    while let Some(a) = args.next() {
        if a == "--limit" {
            limit = args
                .next()
                .and_then(|s| s.parse().ok())
                .unwrap_or(usize::MAX);
        }
    }

    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("cannot read {path}: {e}");
            return ExitCode::from(1);
        }
    };
    println!(
        "== dcbdump {path} ({} bytes, {} dwords) ==",
        bytes.len(),
        bytes.len() / 4
    );

    let mut off = 0usize; // running dword offset
    let mut n = 0usize;
    for pkt in decode_bytes(&bytes) {
        if n >= limit {
            println!("… (--limit {limit} reached)");
            break;
        }
        n += 1;
        match pkt {
            OwnedPacket::Type2 => {
                println!("[{off:04}] (type-2 filler nop)");
                off += 1;
            }
            OwnedPacket::Truncated { header } => {
                println!("[{off:04}] TRUNCATED header={header:#010x} — stop");
                break;
            }
            OwnedPacket::Type0 {
                base_index,
                count,
                body,
            } => {
                println!(
                    "[{off:04}] TYPE0 base={base_index:#06x} x{} [{}]",
                    count as usize + 1,
                    hexlist(&body)
                );
                off += 1 + body.len();
            }
            OwnedPacket::Type3 { opcode, body, .. } => {
                let nm = op_name(opcode).unwrap_or("<unknown>");
                let words = 1 + body.len();
                match set_reg_base(opcode) {
                    // SET_*_REG: body[0] is the offset within the window; the rest are
                    // consecutive register values. Name each register we can.
                    Some(base) if !body.is_empty() => {
                        let start = base + body[0];
                        println!("[{off:04}] {nm}");
                        for (i, val) in body[1..].iter().enumerate() {
                            let abs = start + i as u32;
                            match reg_name(abs) {
                                Some(rn) => println!("           {rn:<28} = {val:#010x}"),
                                None => {
                                    println!("           (idx {abs:#06x})            = {val:#010x}")
                                }
                            }
                        }
                    }
                    // Everything else (draws, NOP, sync, dma, …): opcode + raw body.
                    _ => {
                        if opcode == ps4_gnm::pm4::opcodes::op::IT_NOP {
                            println!("[{off:04}] NOP x{words}");
                        } else {
                            println!("[{off:04}] {nm:<24} [{}]", hexlist(&body));
                        }
                    }
                }
                off += words;
            }
        }
    }
    println!("== {n} packets, {off} dwords ==");
    ExitCode::SUCCESS
}

fn hexlist(w: &[u32]) -> String {
    w.iter()
        .map(|v| format!("{v:08x}"))
        .collect::<Vec<_>>()
        .join(" ")
}
