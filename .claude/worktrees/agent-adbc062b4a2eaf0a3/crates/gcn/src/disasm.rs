//! GCN instruction → human-readable text (doc-4 §1, phase 4).
//!
//! Renders a [`Decoded`] to one llvm-mc-style line for golden tests and traces,
//! mirroring `pm4::trace`: a total function, unknown ops print their raw hex, no
//! panics. The output format tracks AMD / llvm-mc disassembly closely enough to be
//! a readable golden — it is not a byte-exact re-assembler.

use crate::inst::{Decoded, ExportTarget, Inst};
use crate::opcodes;
use crate::operand::Operand;

/// Render one decoded instruction to a trace/golden line (no trailing newline).
pub fn disasm(d: &Decoded) -> String {
    match &d.inst {
        Inst::Sop1 { op, sdst, ssrc0 } => {
            format!(
                "{} {}, {}",
                mnemonic(opcodes::sop1::name(*op), "sop1", u16::from(*op)),
                reg(*sdst),
                reg(*ssrc0)
            )
        }
        Inst::Sop2 {
            op,
            sdst,
            ssrc0,
            ssrc1,
        } => format!(
            "{} {}, {}, {}",
            mnemonic(opcodes::sop2::name(*op), "sop2", u16::from(*op)),
            reg(*sdst),
            reg(*ssrc0),
            reg(*ssrc1)
        ),
        Inst::Sopk { op, sdst, simm16 } => format!(
            "{} {}, {:#x}",
            mnemonic(opcodes::sopk::name(*op), "sopk", u16::from(*op)),
            reg(*sdst),
            simm16
        ),
        Inst::Sopc { op, ssrc0, ssrc1 } => format!(
            "{} {}, {}",
            mnemonic(opcodes::sopc::name(*op), "sopc", u16::from(*op)),
            reg(*ssrc0),
            reg(*ssrc1)
        ),
        Inst::Sopp { op, simm16 } => disasm_sopp(*op, *simm16),
        Inst::Smrd {
            op,
            sdst,
            sbase,
            imm,
            offset,
        } => disasm_smrd(*op, *sdst, *sbase, *imm, *offset),
        Inst::Vop1 { op, vdst, src0 } => format!(
            "{} {}, {}",
            mnemonic(opcodes::vop1::name(*op), "vop1", u16::from(*op)),
            reg(*vdst),
            reg(*src0)
        ),
        Inst::Vop2 {
            op,
            vdst,
            src0,
            vsrc1,
            k,
        } => {
            let name = mnemonic(opcodes::vop2::name(*op), "vop2", u16::from(*op));
            match k {
                // v_madmk: `vdst, src0, K, vsrc1`; v_madak: `vdst, src0, vsrc1, K`.
                // AMD/llvm-mc places K between the two VGPR sources for madmk and
                // after them for madak; render each in its canonical operand order.
                Some(k) if *op == opcodes::vop2::V_MADMK_F32 => format!(
                    "{name} {}, {}, {k:#x}, {}",
                    reg(*vdst),
                    reg(*src0),
                    reg(*vsrc1)
                ),
                Some(k) => format!(
                    "{name} {}, {}, {}, {k:#x}",
                    reg(*vdst),
                    reg(*src0),
                    reg(*vsrc1)
                ),
                None => format!("{name} {}, {}, {}", reg(*vdst), reg(*src0), reg(*vsrc1)),
            }
        }
        Inst::Vop3 {
            op,
            vdst,
            src0,
            src1,
            src2,
            omod,
            ..
        } => {
            let mut line = format!(
                "{} {}, {}, {}, {}",
                mnemonic(opcodes::vop3::name(*op), "vop3", *op),
                reg(*vdst),
                reg(*src0),
                reg(*src1),
                reg(*src2)
            );
            // Output modifier (llvm-mc renders `mul:2`/`mul:4`/`div:2`).
            match omod {
                1 => line.push_str(" mul:2"),
                2 => line.push_str(" mul:4"),
                3 => line.push_str(" div:2"),
                _ => {}
            }
            line
        }
        Inst::Vopc { op, src0, vsrc1 } => format!(
            "{} vcc, {}, {}",
            mnemonic(None, "vopc", u16::from(*op)),
            reg(*src0),
            reg(*vsrc1)
        ),
        Inst::Vintrp {
            op,
            vdst,
            vsrc,
            attr,
            chan,
        } => format!(
            "{} {}, {}, attr{}.{}",
            mnemonic(opcodes::vintrp::name(*op), "vintrp", u16::from(*op)),
            reg(*vdst),
            reg(*vsrc),
            attr,
            "xyzw".as_bytes()[(*chan & 3) as usize] as char
        ),
        Inst::Mubuf {
            op,
            vdata,
            vaddr,
            srsrc,
            soffset,
            idxen,
            offen,
            offset,
        } => disasm_mubuf(
            *op, *vdata, *vaddr, *srsrc, *soffset, *idxen, *offen, *offset,
        ),
        Inst::Mimg {
            op,
            vdata,
            vaddr,
            srsrc,
            ssamp,
            dmask,
            unrm,
        } => disasm_mimg(*op, *vdata, *vaddr, *srsrc, *ssamp, *dmask, *unrm),
        Inst::Exp {
            target,
            srcs,
            done,
            compr,
            vm,
        } => disasm_exp(*target, srcs, *done, *compr, *vm),
        Inst::Unknown { raw, raw_words } => {
            if raw_words.is_empty() {
                format!("<unknown {raw:#010x}>")
            } else {
                let extra = raw_words
                    .iter()
                    .map(|w| format!("{w:#010x}"))
                    .collect::<Vec<_>>()
                    .join(",");
                format!("<unknown {raw:#010x} +[{extra}]>")
            }
        }
    }
}

/// Disassemble a whole stream, one instruction per line.
pub fn disasm_all(decoded: &[Decoded]) -> String {
    decoded.iter().map(disasm).collect::<Vec<_>>().join("\n")
}

fn mnemonic(named: Option<&'static str>, class: &str, op: u16) -> String {
    match named {
        Some(n) => n.to_string(),
        None => format!("{class}_{op:#x}"),
    }
}

fn reg(op: Operand) -> String {
    match op {
        Operand::Sgpr(n) => format!("s{n}"),
        Operand::Vgpr(n) => format!("v{n}"),
        Operand::Special(s) => s.name().to_string(),
        Operand::InlineInt(v) => format!("{v}"),
        Operand::InlineFloat(f) => fmt_float(f),
        Operand::Literal(v) => format!("{v:#x}"),
        Operand::Raw(v) => format!("src({v})"),
    }
}

/// Print an inline float the way llvm-mc does (`1.0`, `0.5`, `-2.0`, …).
fn fmt_float(f: f32) -> String {
    if f == f.trunc() {
        format!("{f:.1}")
    } else {
        format!("{f}")
    }
}

/// `<c>[lo:hi]` for a multi-register span, or `<c><n>` for one. `class` is the
/// register-class prefix (`'s'` scalar, `'v'` vector).
fn reg_span(class: char, base: u8, count: u8) -> String {
    if count <= 1 {
        format!("{class}{base}")
    } else {
        format!("{class}[{}:{}]", base, base + count - 1)
    }
}

fn disasm_sopp(op: u8, simm16: u16) -> String {
    let name = mnemonic(opcodes::sopp::name(op), "sopp", u16::from(op));
    match op {
        opcodes::sopp::S_ENDPGM | opcodes::sopp::S_NOP if simm16 == 0 => name,
        opcodes::sopp::S_WAITCNT => format!("{name} {}", waitcnt_operand(simm16)),
        _ => format!("{name} {simm16:#x}"),
    }
}

/// Render an s_waitcnt SIMM16 as its `vmcnt/lgkmcnt/expcnt` breakdown. On SI/CI the
/// fields are vmcnt[3:0] (max 15), expcnt[6:4] (max 7), lgkmcnt[11:8] (4-bit, max
/// 15). Only the non-max counters are printed, matching llvm-mc.
fn waitcnt_operand(simm16: u16) -> String {
    let vmcnt = simm16 & 0xF;
    let expcnt = (simm16 >> 4) & 0x7;
    let lgkmcnt = (simm16 >> 8) & 0xF;
    let mut parts = Vec::new();
    if vmcnt != 0xF {
        parts.push(format!("vmcnt({vmcnt})"));
    }
    if expcnt != 0x7 {
        parts.push(format!("expcnt({expcnt})"));
    }
    if lgkmcnt != 0xF {
        parts.push(format!("lgkmcnt({lgkmcnt})"));
    }
    if parts.is_empty() {
        "0".to_string()
    } else {
        parts.join(" ")
    }
}

fn disasm_smrd(op: u8, sdst: Operand, sbase: u8, imm: bool, offset: u32) -> String {
    let name = mnemonic(opcodes::smrd::name(op), "smrd", u16::from(op));
    let count = opcodes::smrd::dst_count(op).unwrap_or(1);
    let dst = match sdst {
        Operand::Sgpr(n) => reg_span('s', n, count),
        other => reg(other),
    };
    let off = if imm {
        format!("{offset:#x}")
    } else {
        format!("s{offset}")
    };
    // Base is an SGPR pair (V#/T# base is 2 SGPRs).
    format!("{name} {dst}, {}, {off}", reg_span('s', sbase, 2))
}

#[allow(clippy::too_many_arguments)]
fn disasm_mubuf(
    op: u8,
    vdata: Operand,
    vaddr: Operand,
    srsrc: u8,
    soffset: Operand,
    idxen: bool,
    offen: bool,
    offset: u16,
) -> String {
    let name = mnemonic(opcodes::mubuf::name(op), "mubuf", u16::from(op));
    let count = opcodes::mubuf::vdata_count(op).unwrap_or(1);
    let data = match vdata {
        Operand::Vgpr(n) => reg_span('v', n, count),
        other => reg(other),
    };
    // srsrc names a 4-SGPR V# resource.
    let mut line = format!(
        "{name} {data}, {}, {}, {}",
        reg(vaddr),
        reg_span('s', srsrc, 4),
        reg(soffset)
    );
    if offset != 0 {
        line.push_str(&format!(" offset:{offset:#x}"));
    }
    if idxen {
        line.push_str(" idxen");
    }
    if offen {
        line.push_str(" offen");
    }
    line
}

#[allow(clippy::too_many_arguments)]
fn disasm_mimg(
    op: u8,
    vdata: Operand,
    vaddr: Operand,
    srsrc: u8,
    ssamp: u8,
    dmask: u8,
    unrm: bool,
) -> String {
    let name = mnemonic(opcodes::mimg::name(op), "mimg", u16::from(op));
    // vdata is one VGPR per enabled dmask channel; vaddr is the 2D coordinate pair
    // (u, v). The T# is 8 SGPRs (256-bit), the S# 4 SGPRs (128-bit).
    let dcount = dmask.count_ones().max(1) as u8;
    let data = match vdata {
        Operand::Vgpr(n) => reg_span('v', n, dcount),
        other => reg(other),
    };
    let addr = match vaddr {
        Operand::Vgpr(n) => reg_span('v', n, 2),
        other => reg(other),
    };
    let mut line = format!(
        "{name} {data}, {addr}, {}, {} dmask:{dmask:#x}",
        reg_span('s', srsrc, 8),
        reg_span('s', ssamp, 4),
    );
    if unrm {
        line.push_str(" unorm");
    }
    line
}

fn disasm_exp(
    target: ExportTarget,
    srcs: &[Option<Operand>; 4],
    done: bool,
    compr: bool,
    vm: bool,
) -> String {
    let tgt = match target {
        ExportTarget::Mrt(n) => format!("mrt{n}"),
        ExportTarget::MrtZ => "mrtz".to_string(),
        ExportTarget::Null => "null".to_string(),
        ExportTarget::Pos(n) => format!("pos{n}"),
        ExportTarget::Param(n) => format!("param{n}"),
        ExportTarget::Raw(n) => format!("exptgt{n}"),
    };
    let mut line = format!("exp {tgt}");
    for s in srcs {
        match s {
            Some(op) => line.push_str(&format!(", {}", reg(*op))),
            None => line.push_str(", off"),
        }
    }
    // Modifier order matches llvm-mc / AMD: `done` precedes `compr`.
    if done {
        line.push_str(" done");
    }
    if compr {
        line.push_str(" compr");
    }
    if vm {
        line.push_str(" vm");
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decoder::decode_one;

    fn line(w: &[u32]) -> String {
        disasm(&decode_one(w))
    }

    #[test]
    fn unknown_renders_raw_hex() {
        let d = Decoded {
            inst: Inst::Unknown {
                raw: 0xDEAD_BEEF,
                raw_words: Vec::new(),
            },
            size_dwords: 1,
            offset_dwords: 0,
        };
        assert_eq!(disasm(&d), "<unknown 0xdeadbeef>");
    }

    #[test]
    fn unknown_with_extra_words_renders() {
        let d = Decoded {
            inst: Inst::Unknown {
                raw: 0xDEAD_BEEF,
                raw_words: vec![0x1122_3344],
            },
            size_dwords: 2,
            offset_dwords: 0,
        };
        assert_eq!(disasm(&d), "<unknown 0xdeadbeef +[0x11223344]>");
    }

    #[test]
    fn renders_scalar_and_endpgm() {
        assert_eq!(line(&[0xbefc_0300]), "s_mov_b32 m0, s0");
        assert_eq!(line(&[0xbf81_0000]), "s_endpgm");
        assert_eq!(line(&[0xbf8c_007f]), "s_waitcnt lgkmcnt(0)");
        assert_eq!(line(&[0xbf8c_0f70]), "s_waitcnt vmcnt(0)");
    }

    #[test]
    fn renders_vop1_inline_and_literal() {
        assert_eq!(line(&[0x7e00_02f2]), "v_mov_b32 v0, 1.0");
        assert_eq!(
            line(&[0x7e02_02ff, 0x3e80_0000]),
            "v_mov_b32 v1, 0x3e800000"
        );
    }

    #[test]
    fn renders_smrd_span() {
        assert_eq!(line(&[0xc080_0300]), "s_load_dwordx4 s[0:3], s[2:3], 0x0");
    }

    #[test]
    fn renders_vintrp() {
        assert_eq!(line(&[0xc808_0000]), "v_interp_p1_f32 v2, v0, attr0.x");
    }

    #[test]
    fn unmapped_opcode_renders_numeric_not_panics() {
        // SOP2 with an unmapped op → numeric mnemonic, still decodes.
        let w = (0b10u32 << 30) | (0x40 << 23) | (5 << 16) | 6;
        let s = line(&[w]);
        assert!(s.starts_with("sop2_"), "got {s}");
    }
}
