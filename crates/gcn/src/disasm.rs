//! GCN instruction → human-readable text (doc-2 §1, phase 4).
//!
//! Renders a [`Decoded`] to one llvm-mc-style line for golden tests and traces,
//! mirroring `pm4::trace`: a total function, unknown ops print their raw hex, no
//! panics. The output format tracks AMD / llvm-mc disassembly closely enough to be
//! a readable golden — it is not a byte-exact re-assembler.
//!
//! Mnemonics are pure glue over [`crate::opcodes`] (the GCN opcode tables) — this
//! renderer composes those names with operand text and does not itself assert any
//! opcode value. What it *does* assert about the hardware — the s_waitcnt SIMM16
//! field layout, VINTRP channel select, the register-span widths of a V#/T#/S#
//! descriptor, and the export-target enumeration — is the AMD "Sea Islands Series
//! ISA" (`oracles/amd/ci-isa.pdf`; GCN2 = PS4 Liverpool). Those facts
//! are pinned to CI-ISA literals by `disasm_facts_match_amd_oracle` below, with the
//! encoding bytes cross-checked against `llvm-mc --arch=amdgcn --mcpu=gfx700`.

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
                // The operand order mirrors each op's arithmetic: madmk is
                // `D = S0 * K + S1` and madak is `D = S0 * S1 + K` (CI-ISA VOP2
                // V_MADMK_F32 / V_MADAK_F32), so K prints between the two VGPR
                // sources for madmk and after them for madak — matching llvm-mc.
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
            abs,
            neg,
            omod,
            clamp,
        } if *op < 0x100 => {
            // VOPC-encoded VOP3: `vdst` is the SGPR-PAIR (or vcc) compare destination,
            // and the compare takes only two sources (no src2). Render `<cmp> <sdst>,
            // s0, s1` — the sdst as a 2-register span when it names an SGPR pair.
            let dst = match vdst {
                Operand::Sgpr(base) => format!("s[{}:{}]", base, base + 1),
                other => reg(*other),
            };
            let mut line = format!(
                "{} {}, {}, {}",
                mnemonic(opcodes::vop3::name(*op), "vop3", *op),
                dst,
                vop3_src(*src0, *abs, *neg, 0),
                vop3_src(*src1, *abs, *neg, 1)
            );
            if *clamp {
                line.push_str(" clamp");
            }
            match omod {
                1 => line.push_str(" mul:2"),
                2 => line.push_str(" mul:4"),
                3 => line.push_str(" div:2"),
                _ => {}
            }
            line
        }
        Inst::Vop3 {
            op,
            vdst,
            src0,
            src1,
            src2,
            abs,
            neg,
            omod,
            clamp,
        } => {
            let mut line = format!(
                "{} {}, {}, {}, {}",
                mnemonic(opcodes::vop3::name(*op), "vop3", *op),
                reg(*vdst),
                vop3_src(*src0, *abs, *neg, 0),
                vop3_src(*src1, *abs, *neg, 1),
                vop3_src(*src2, *abs, *neg, 2)
            );
            // Output modifiers. llvm-mc's VOP3 asm string is `…$clamp$omod`, so `clamp`
            // is printed BEFORE `mul:2`/`div:2` even though the hardware applies it
            // after — matching llvm-mc's text is what keeps the golden dumps comparable.
            if *clamp {
                line.push_str(" clamp");
            }
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
            mnemonic(opcodes::vopc::name(*op), "vopc", u16::from(*op)),
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
            // ATTRCHAN selects the interpolated component: 0=x, 1=y, 2=z, 3=w
            // (CI-ISA §12.12 "VINTRP Instructions", ATTRCHAN field).
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

/// Glue: the opcode's name from [`crate::opcodes`], or a `<class>_<op>` fallback for
/// an unmapped opcode (never fatal). Composes the opcode table; asserts nothing itself.
fn mnemonic(named: Option<&'static str>, class: &str, op: u16) -> String {
    match named {
        Some(n) => n.to_string(),
        None => format!("{class}_{op:#x}"),
    }
}

/// Glue: render one decoded [`Operand`] as its llvm-mc token (`s0`, `v1`, `vcc`,
/// inline constant, or literal). Composes an already-decoded operand; no hardware fact.
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

/// Render one VOP3 source with its per-source `abs`/`neg` modifiers in AMD/llvm-mc
/// syntax: `|v2|` for abs, `-v1` for neg, `-|v2|` for both. Bit `idx` of each mask
/// selects source `idx`, and abs applies before neg — matching `uop::apply_mods`,
/// which is what the recompiler and interpreter actually execute. The VOP3a form
/// carries a per-operand ABS and NEG field applied to floating-point inputs
/// (CI-ISA §6.2.1 "Instruction Inputs", ABS/NEG fields).
fn vop3_src(op: Operand, abs: u8, neg: u8, idx: u8) -> String {
    let s = if abs & (1 << idx) != 0 {
        format!("|{}|", reg(op))
    } else {
        reg(op)
    };
    if neg & (1 << idx) != 0 {
        format!("-{s}")
    } else {
        s
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
        // Widen to u16 for the top index: a guest-controlled VGPR span near 255
        // (e.g. MIMG VADDR 254 with dcount 2) makes `base + count - 1` exceed
        // u8::MAX, which panics in debug and wraps in release. Keep the module a
        // total function on untrusted input.
        format!(
            "{class}[{}:{}]",
            base,
            u16::from(base) + u16::from(count) - 1
        )
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

/// Render an s_waitcnt SIMM16 as its `vmcnt/lgkmcnt/expcnt` breakdown. Per CI-ISA
/// (SOPP opcode 0xC, S_WAITCNT): `simm16[3:0] = vmcnt` (4-bit, max 15),
/// `simm16[6:4] = export/mem-write count` (3-bit, max 7), `simm16[12:8] = LGKM_cnt`
/// (scalar-mem/GDS/LDS count). Only the non-max counters are printed, matching
/// llvm-mc; a maxed field means "do not wait on this class".
///
/// NOTE: this renderer masks LGKM_cnt as `[11:8]` (4 bits) rather than CI's `[12:8]`
/// (5 bits) — a narrowing that drops bit 12 for lgkmcnt > 15. Golden-text only; see
/// the audit flag. Do not change the mask here without re-checking the golden dumps.
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
    // SMRD SBASE (encoding bits [15:9]) is, for S_LOAD_DWORD*, the SGPR-pair holding
    // the 64-bit base byte-address (CI-ISA Table 7.1, SBASE); render it as a 2-SGPR
    // span. (S_BUFFER_LOAD* instead points SBASE at a 4-SGPR V#; the renderer prints
    // a 2-span uniformly, which is exact for the S_LOAD case.)
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
    // SRSRC names a buffer resource (V#) held in four consecutive SGPRs
    // (CI-ISA §8.2 "Buffer Resource": a 128-bit descriptor in 4 aligned SGPRs).
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
    // VDATA is one VGPR per enabled DMASK channel (CI-ISA Table 8.7, DMASK: "one to
    // four consecutive VGPRs", enabled components come from consecutive VGPRs). SRSRC
    // names the image resource (T#) — CI-ISA §8.2.1 "Image Resource": four or eight
    // consecutive SGPRs (128- or 256-bit); we render the 8-SGPR (256-bit) form. SSAMP
    // names the sampler resource (S#) in four consecutive SGPRs (CI-ISA "Sampler
    // Resource", 128-bit). VADDR gives the first address VGPR; the renderer assumes a
    // 2D (u, v) coordinate pair.
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
    // Export TGT names (CI-ISA §12.17 EXP, TGT field [9:4]): 0–7 EXP_MRT (color
    // MRT0+n), 8 EXP_MRTZ (Z), 9 EXP_NULL, 12–15 EXP_POS (position0+n), 32–63
    // EXP_PARAM (parameter0+n). Glue: it maps the already-decoded [`ExportTarget`]
    // enum to those names.
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
    // EXP modifier bits (CI-ISA §12.17): COMPR bit 10 (float16 vs 32-bit data),
    // DONE bit 11 (last export of a type), VM bit 12 (mask carries the valid-mask).
    // Print order matches llvm-mc / AMD: `done` precedes `compr`.
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

    /// Encode a VOP3 `op vdst, src0, src1, src2` with the given modifier masks.
    fn vop3(op: u16, vdst: u8, srcs: [u32; 3], abs: u8, neg: u8, omod: u8) -> [u32; 2] {
        vop3_clamp(op, vdst, srcs, abs, neg, omod, false)
    }

    /// As [`vop3`], plus the `clamp` output modifier (low-dword bit 11).
    fn vop3_clamp(
        op: u16,
        vdst: u8,
        srcs: [u32; 3],
        abs: u8,
        neg: u8,
        omod: u8,
        clamp: bool,
    ) -> [u32; 2] {
        let w0 = (0b110100 << 26)
            | (u32::from(op) << 17)
            | (u32::from(clamp) << 11)
            | (u32::from(abs) << 8)
            | u32::from(vdst);
        let w1 = (u32::from(neg) << 29)
            | (u32::from(omod) << 27)
            | (srcs[2] << 18)
            | (srcs[1] << 9)
            | srcs[0];
        [w0, w1]
    }

    #[test]
    fn renders_vop3_neg_and_abs_modifiers() {
        // v_mad_f32 v0, v1, v2, v3 with abs on src1 and neg on src1+src2: AMD syntax
        // puts neg outside abs (`-|v2|`), matching `uop::apply_mods` (abs then neg).
        let w = vop3(
            opcodes::vop3::V_MAD_F32,
            0,
            [257, 258, 259],
            0b010,
            0b110,
            0,
        );
        assert_eq!(line(&w), "v_mad_f32 v0, v1, -|v2|, -v3");
        // abs alone, neg alone, and neither — on the same instruction.
        let w = vop3(
            opcodes::vop3::V_MAD_F32,
            0,
            [257, 258, 259],
            0b001,
            0b100,
            0,
        );
        assert_eq!(line(&w), "v_mad_f32 v0, |v1|, v2, -v3");
        // Modifiers coexist with the output modifier.
        let w = vop3(
            opcodes::vop3::V_MAD_F32,
            0,
            [257, 258, 259],
            0b100,
            0b001,
            3,
        );
        assert_eq!(line(&w), "v_mad_f32 v0, -v1, v2, |v3| div:2");
    }

    /// The `clamp` output modifier renders, and renders BEFORE `mul:2`/`div:2` — the
    /// order llvm-mc uses (`…$clamp$omod`), which is the reverse of the order the
    /// hardware applies them in. Encodings cross-checked against
    /// `llvm-mc -arch=amdgcn -mcpu=bonaire`.
    #[test]
    fn renders_vop3_clamp_and_clamp_with_omod() {
        // v_mad_f32 v0, v1, v2, v3 clamp -> [0xd2820800, 0x040e0501].
        let w = vop3_clamp(opcodes::vop3::V_MAD_F32, 0, [257, 258, 259], 0, 0, 0, true);
        assert_eq!(w[0], 0xd282_0800, "clamp is low-dword bit 11");
        assert_eq!(line(&w), "v_mad_f32 v0, v1, v2, v3 clamp");
        // v_mad_f32 v0, v1, v2, v3 clamp mul:2 -> [0xd2820800, 0x0c0e0501].
        let w = vop3_clamp(opcodes::vop3::V_MAD_F32, 0, [257, 258, 259], 0, 0, 1, true);
        assert_eq!(w, [0xd282_0800, 0x0c0e_0501]);
        assert_eq!(line(&w), "v_mad_f32 v0, v1, v2, v3 clamp mul:2");
        // clamp coexists with the src abs/neg modifiers.
        let w = vop3_clamp(
            opcodes::vop3::V_MAD_F32,
            0,
            [257, 258, 259],
            0b001,
            0b100,
            3,
            true,
        );
        assert_eq!(line(&w), "v_mad_f32 v0, |v1|, v2, -v3 clamp div:2");
        // Not set -> nothing printed (guards against an unconditional suffix).
        let w = vop3_clamp(opcodes::vop3::V_MAD_F32, 0, [257, 258, 259], 0, 0, 1, false);
        assert_eq!(line(&w), "v_mad_f32 v0, v1, v2, v3 mul:2");
    }

    #[test]
    fn renders_vop3_modifiers_on_vopc_encoded_compare() {
        // A VOPC-encoded VOP3 (op < 0x100) renders only two sources; both still carry
        // their modifiers.
        let w = vop3(0x04, 0, [257, 258, 0], 0b010, 0b001, 0);
        let s = line(&w);
        assert!(s.ends_with("s[0:1], -v1, |v2|"), "got {s}");
    }

    #[test]
    fn unmapped_opcode_renders_numeric_not_panics() {
        // SOP2 with an unmapped op → numeric mnemonic, still decodes.
        let w = (0b10u32 << 30) | (0x40 << 23) | (5 << 16) | 6;
        let s = line(&[w]);
        assert!(s.starts_with("sop2_"), "got {s}");
    }

    /// Pins the hardware facts this renderer asserts to their AMD "Sea Islands Series
    /// ISA" (CI-ISA; GCN2 = PS4 Liverpool) literals. Each right-hand value is the AMD
    /// definition; the assembled instruction words are cross-checked with
    /// `llvm-mc --arch=amdgcn --mcpu=gfx700 --show-encoding`. Fails if our rendering
    /// drifts from those definitions.
    #[test]
    fn disasm_facts_match_amd_oracle() {
        // --- S_WAITCNT SIMM16 field layout (CI-ISA SOPP opcode 0xC, S_WAITCNT):
        //   simm16[3:0] = vmcnt, simm16[6:4] = expcnt, simm16[12:8] = lgkmcnt.
        // A maxed field is suppressed; only the waited-on class prints. Encodings from
        // llvm-mc: lgkmcnt(0)=0xbf8c007f, vmcnt(0)=0xbf8c0f70, expcnt(0)=0xbf8c0f0f.
        assert_eq!(line(&[0xbf8c_007f]), "s_waitcnt lgkmcnt(0)"); // vmcnt/expcnt maxed
        assert_eq!(line(&[0xbf8c_0f70]), "s_waitcnt vmcnt(0)"); // expcnt/lgkmcnt maxed
        assert_eq!(line(&[0xbf8c_0f0f]), "s_waitcnt expcnt(0)"); // vmcnt/lgkmcnt maxed
        // Direct field decode against the CI-ISA bit positions.
        assert_eq!(waitcnt_operand(0x007f), "lgkmcnt(0)");
        assert_eq!(waitcnt_operand(0x0f70), "vmcnt(0)");
        assert_eq!(waitcnt_operand(0x0f0f), "expcnt(0)");

        // --- VINTRP ATTRCHAN (CI-ISA §12.12): 0=x, 1=y, 2=z, 3=w. v_interp_p1_f32
        // v2, v0, attr0.x assembles (llvm-mc) to 0xc8080000.
        assert_eq!(line(&[0xc808_0000]), "v_interp_p1_f32 v2, v0, attr0.x");

        // --- Descriptor register-span widths (CI-ISA §8.2): a buffer resource (V#) is
        // 4 SGPRs, an image resource (T#) is 4 or 8 SGPRs (we render 8), a sampler (S#)
        // is 4 SGPRs. Exercised through the MUBUF/MIMG renderers via reg_span.
        assert_eq!(reg_span('s', 0, 4), "s[0:3]"); // V# / S# = 128-bit = 4 SGPRs
        assert_eq!(reg_span('s', 0, 8), "s[0:7]"); // T# = 256-bit = 8 SGPRs
        assert_eq!(reg_span('s', 0, 2), "s[0:1]"); // SMRD S_LOAD SBASE = 64-bit ptr

        // --- EXP TGT enum (CI-ISA §12.17, TGT field [9:4]) and modifier bits: 0–7
        // EXP_MRT, 8 EXP_MRTZ, 9 EXP_NULL, 12–15 EXP_POS, 32–63 EXP_PARAM; DONE = bit
        // 11. `exp mrt0, v0, v1, v2, v3 done` assembles (llvm-mc) to [0xf800080f,
        // 0x03020100]. (We render the target comma-separated from the operands.)
        assert_eq!(
            line(&[0xf800_080f, 0x0302_0100]),
            "exp mrt0, v0, v1, v2, v3 done"
        );
    }

    #[test]
    fn reg_span_high_vgpr_does_not_overflow() {
        // Guest-controlled VGPR index near the top of the 256-entry file: a span
        // whose last index exceeds u8::MAX must not panic (debug) or wrap
        // (release) — the renderer is a total function on untrusted input.
        assert_eq!(reg_span('v', 254, 2), "v[254:255]");
        assert_eq!(reg_span('v', 252, 4), "v[252:255]");
        // A MIMG image op with VADDR=254 and DMASK=1 (dcount 2) reaches reg_span
        // via disasm_mimg; it decodes without panicking.
        let _ = line(&[0xf000_0100, 0x0000_00fe]);
    }
}
