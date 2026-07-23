//! GCN instruction-stream decoder (doc-2 §1, phase 4).
//!
//! Walks a `&[u32]` of GCN machine code and yields one [`Decoded`] per
//! instruction. It is a *total* decoder in the PM4 tradition (`pm4::decode`): an
//! arbitrary dword stream never panics — an unrecognized or truncated encoding
//! becomes [`Inst::Unknown`] and the walk continues by a single dword (AC #2).
//! Every yielded [`Decoded`] carries `size_dwords`, so a caller advancing by that
//! amount lands exactly on the next instruction, including past 32-bit literals
//! and VOP3's second dword (AC #3).
//!
//! Encoding-class dispatch reads the top bits of the first dword; the SI/CI
//! instruction encodings are disjoint on those high-bit prefixes. This is decode
//! only — nothing here executes.
//!
//! Every per-format bit layout below is the AMD Sea Islands (GCN2 = PS4
//! Liverpool/GFX7) microcode format published in the "CI Series Instruction Set
//! Architecture" (AMD, 2013; `amd/ci-isa.pdf`), Chapter 13 "Microcode Formats":
//! §13.1 SALU (SOP2/SOPK/SOP1/SOPC/SOPP), §13.2 SMRD, §13.3 Vector ALU
//! (VOP1/VOP2/VOPC/VOP3a/VOP3b), §13.4 VINTRP, §13.6 MUBUF, §13.7 MIMG, §13.8 EXP.
//! Each ENCODING-prefix and field `[hi:lo]` is stated as a "Must be …" / bit-range
//! row in those tables. Representative real encodings for every format are pinned
//! to `llvm-mc --mcpu=gfx700` output by `decoder_fields_match_amd_oracle` below.

use std::sync::OnceLock;

use crate::inst::{Decoded, ExportTarget, Inst};
use crate::operand::{self, Operand};

/// Env var that turns GCN decode tracing on (mirrors `UNEMUPS4_PM4_TRACE`).
pub const TRACE_ENV: &str = "UNEMUPS4_GCN_TRACE";

/// Whether GCN decode tracing is enabled for this process. Read once and cached so
/// the `decode_all` hot loop does not touch `std::env` per instruction.
pub fn trace_enabled() -> bool {
    static TRACE: OnceLock<bool> = OnceLock::new();
    *TRACE.get_or_init(|| match std::env::var(TRACE_ENV) {
        Ok(v) => !v.is_empty() && v != "0",
        Err(_) => false,
    })
}

/// Decode every instruction in `code`, returning them in stream order. A trailing
/// partial instruction (a multi-dword form whose extra dword is missing) is decoded
/// as far as possible and does not panic.
pub fn decode_all(code: &[u32]) -> Vec<Decoded> {
    let trace = trace_enabled();
    let mut out = Vec::new();
    let mut pc = 0usize;
    while pc < code.len() {
        let mut d = decode_one(&code[pc..]);
        d.offset_dwords = pc as u32;
        // A multi-dword form at the tail of a truncated stream must not advance the
        // PC past the buffer — clamp the reported length to what remains so the
        // consumed total always equals the input length.
        let remaining = (code.len() - pc) as u32;
        if d.size_dwords > remaining {
            d.size_dwords = remaining;
        }
        if trace {
            tracing::info!("[GCN] {:#06x}: {:?}", pc * 4, d.inst);
        }
        pc += d.size_dwords as usize;
        out.push(d);
    }
    out
}

/// Decode the single instruction at the start of `words`. `words` must be
/// non-empty. Never panics: an unknown/short encoding returns [`Inst::Unknown`]
/// with `size_dwords == 1`.
pub fn decode_one(words: &[u32]) -> Decoded {
    let w0 = words[0];

    // Encoding dispatch by high-bit prefix (SI/CI). Order matters: the scalar
    // formats share the 0b10 top prefix and are distinguished by more bits. Each
    // prefix is the "ENCODING … Must be …" row of the matching CI-ISA §13 format
    // table (bits [31:26] for the 6-bit-prefix formats, [31:25] for VOP1/VOPC).
    if w0 >> 26 == 0b111110 {
        // EXP: CI-ISA §13.8, ENCODING [31:26] = 1 1 1 1 1 0.
        return decode_exp(words);
    }
    if w0 >> 26 == 0b111000 {
        // MUBUF: CI-ISA §13.6, ENCODING [31:26] = 1 1 1 0 0 0.
        return decode_mubuf(words);
    }
    if w0 >> 26 == 0b111100 {
        // MIMG: CI-ISA §13.7, ENCODING [31:26] = 1 1 1 1 0 0.
        return decode_mimg(words);
    }
    if w0 >> 26 == 0b110010 {
        // VINTRP: CI-ISA §13.4, ENCODING [31:26] = 1 1 0 0 1 0.
        return decode_vintrp(w0);
    }
    if w0 >> 25 == 0b0111111 {
        // VOP1: CI-ISA §13.3, ENCODE [31:25] = 0 1 1 1 1 1 1.
        return decode_vop1(words);
    }
    if w0 >> 25 == 0b0111110 {
        // VOPC: CI-ISA §13.3, ENCODE [31:25] = 0 1 1 1 1 1 0.
        return decode_vopc(words);
    }
    if w0 >> 26 == 0b110100 {
        // VOP3 (VOP3a/VOP3b): CI-ISA §13.3, ENCODING [31:26] = 1 1 0 1 0 0.
        return decode_vop3(words);
    }
    if w0 >> 27 == 0b11000 {
        // SMRD: CI-ISA §13.2, ENCODING [31:27] = 1 1 0 0 0.
        return decode_smrd(w0);
    }
    // Scalar formats: top two bits 0b10. SOPP/SOPC/SOPK/SOP1/SOP2 by inner prefix.
    if w0 >> 30 == 0b10 {
        return decode_scalar(words);
    }
    // VOP2 has the widest prefix (top bit 0) and must be tested last so it does not
    // swallow the more-specific VOP1/VOPC/scalar encodings above.
    if w0 >> 31 == 0 {
        return decode_vop2(words);
    }
    unknown(w0)
}

/// Build a `Decoded` at a placeholder offset (0); `decode_all` fixes `offset_dwords`
/// to the real stream position. `decode_one` on its own has no offset context.
fn decoded(inst: Inst, size_dwords: u32) -> Decoded {
    Decoded {
        inst,
        size_dwords,
        offset_dwords: 0,
    }
}

fn unknown(raw: u32) -> Decoded {
    decoded(
        Inst::Unknown {
            raw,
            raw_words: Vec::new(),
        },
        1,
    )
}

/// Resolve a 9-bit source field to an [`Operand`], splicing in a trailing literal
/// from `words[lit_idx]` when the field is 255. Returns the operand and whether a
/// literal dword was consumed (so the caller can bump the length exactly once even
/// if two operands both name 255 — GCN allows only one literal per instruction).
fn read_src(field: u32, words: &[u32], lit_idx: usize, literal_taken: &mut bool) -> Operand {
    let op = operand::decode_src(field);
    if op.is_literal() {
        let val = words.get(lit_idx).copied().unwrap_or(0);
        *literal_taken = true;
        Operand::Literal(val)
    } else {
        op
    }
}

fn decode_scalar(words: &[u32]) -> Decoded {
    // All five SALU formats: CI-ISA §13.1 "Scalar ALU and Control Formats". The
    // longer ENCODING prefixes are tested before the shorter ones so a specific
    // format is not swallowed by the SOP2 fall-through ([31:30] = 1 0).
    let w0 = words[0];
    // SOPP: CI-ISA §13.1 ENCODING [31:23] = 1 0 1 1 1 1 1 1 1; OP [22:16], SIMM16 [15:0].
    if w0 >> 23 == 0b1011_11111 {
        let op = ((w0 >> 16) & 0x7F) as u8;
        let simm16 = (w0 & 0xFFFF) as u16;
        return decoded(Inst::Sopp { op, simm16 }, 1);
    }
    // SOPC: CI-ISA §13.1 ENCODING [31:23] = 1 0 1 1 1 1 1 1 0; OP [22:16],
    // SSRC0 [7:0], SSRC1 [15:8].
    if w0 >> 23 == 0b1011_11110 {
        let op = ((w0 >> 16) & 0x7F) as u8;
        let mut lit = false;
        let ssrc0 = read_src(w0 & 0xFF, words, 1, &mut lit);
        let ssrc1 = read_src((w0 >> 8) & 0xFF, words, 1, &mut lit);
        return decoded(Inst::Sopc { op, ssrc0, ssrc1 }, 1 + lit as u32);
    }
    // SOP1: CI-ISA §13.1 ENCODING [31:23] = 1 0 1 1 1 1 1 0 1; SSRC0 [7:0],
    // OP [15:8], SDST [22:16].
    if w0 >> 23 == 0b1011_11101 {
        let op = ((w0 >> 8) & 0xFF) as u8;
        let sdst = operand::decode_src((w0 >> 16) & 0x7F);
        let mut lit = false;
        let ssrc0 = read_src(w0 & 0xFF, words, 1, &mut lit);
        return decoded(Inst::Sop1 { op, sdst, ssrc0 }, 1 + lit as u32);
    }
    // SOPK: CI-ISA §13.1 ENCODING [31:28] = 1 0 1 1; OP [27:23], SDST [22:16],
    // SIMM16 [15:0] (signed).
    if w0 >> 28 == 0b1011 {
        let op = ((w0 >> 23) & 0x1F) as u8;
        let sdst = operand::decode_src((w0 >> 16) & 0x7F);
        let simm16 = (w0 & 0xFFFF) as i16;
        return decoded(Inst::Sopk { op, sdst, simm16 }, 1);
    }
    // SOP2: CI-ISA §13.1 ENCODING [31:30] = 1 0 (the scalar-space fall-through);
    // OP [29:23], SSRC0 [7:0], SSRC1 [15:8], SDST [22:16].
    let op = ((w0 >> 23) & 0x7F) as u8;
    let sdst = operand::decode_src((w0 >> 16) & 0x7F);
    let mut lit = false;
    let ssrc0 = read_src(w0 & 0xFF, words, 1, &mut lit);
    let ssrc1 = read_src((w0 >> 8) & 0xFF, words, 1, &mut lit);
    decoded(
        Inst::Sop2 {
            op,
            sdst,
            ssrc0,
            ssrc1,
        },
        1 + lit as u32,
    )
}

fn decode_smrd(w0: u32) -> Decoded {
    // SMRD: CI-ISA §13.2 "Scalar Memory Read". OP [26:22], SDST [21:15],
    // SBASE [14:9], IMM [8], OFFSET [7:0]. SBASE names an *aligned pair* of SGPRs,
    // so the encoded value is the SGPR index >> 1 — the raw field is doubled to
    // recover the SGPR number (llvm-mc: `s_load_dword s0, s[2:3], 0x10` encodes
    // SBASE = 1, i.e. s[2:3]).
    let op = ((w0 >> 22) & 0x1F) as u8;
    let sdst = operand::decode_src((w0 >> 15) & 0x7F);
    let sbase = (((w0 >> 9) & 0x3F) * 2) as u8;
    let imm = (w0 >> 8) & 1 == 1;
    let offset_field = w0 & 0xFF;
    decoded(
        Inst::Smrd {
            op,
            sdst,
            sbase,
            imm,
            offset: offset_field,
        },
        1,
    )
}

fn decode_vop1(words: &[u32]) -> Decoded {
    // VOP1: CI-ISA §13.3 "Vector Instruction One Input, One Output".
    // SRC0 [8:0], OP [16:9], VDST [24:17].
    let w0 = words[0];
    let op = ((w0 >> 9) & 0xFF) as u8;
    let vdst = Operand::Vgpr(((w0 >> 17) & 0xFF) as u8);
    let mut lit = false;
    let src0 = read_src(w0 & 0x1FF, words, 1, &mut lit);
    decoded(Inst::Vop1 { op, vdst, src0 }, 1 + lit as u32)
}

fn decode_vop2(words: &[u32]) -> Decoded {
    // VOP2: CI-ISA §13.3 "Vector Instruction Two Inputs, One Output".
    // ENCODE [31] = 0; SRC0 [8:0], VSRC1 [16:9], VDST [24:17], OP [30:25].
    let w0 = words[0];
    let op = ((w0 >> 25) & 0x3F) as u8;
    let vdst = Operand::Vgpr(((w0 >> 17) & 0xFF) as u8);
    let vsrc1 = Operand::Vgpr(((w0 >> 9) & 0xFF) as u8);
    let mut lit = false;
    let src0 = read_src(w0 & 0x1FF, words, 1, &mut lit);
    // v_madmk/v_madak carry a 32-bit K constant as their second dword unconditionally
    // (independent of the src0 literal path). Capture it so the interpreter has it.
    let (k, size_dwords) = if crate::opcodes::vop2::has_inline_literal(op) {
        (Some(words.get(1).copied().unwrap_or(0)), 2)
    } else {
        (None, 1 + lit as u32)
    };
    decoded(
        Inst::Vop2 {
            op,
            vdst,
            src0,
            vsrc1,
            k,
        },
        size_dwords,
    )
}

fn decode_vopc(words: &[u32]) -> Decoded {
    // VOPC: CI-ISA §13.3 "Vector Instruction Two Inputs, One Comparison Result".
    // SRC0 [8:0], VSRC1 [16:9], OP [24:17]. The compare result lands in VCC in this
    // encoding (an arbitrary SGPR only in the VOP3 form).
    let w0 = words[0];
    let op = ((w0 >> 17) & 0xFF) as u8;
    let vsrc1 = Operand::Vgpr(((w0 >> 9) & 0xFF) as u8);
    let mut lit = false;
    let src0 = read_src(w0 & 0x1FF, words, 1, &mut lit);
    decoded(Inst::Vopc { op, src0, vsrc1 }, 1 + lit as u32)
}

fn decode_vop3(words: &[u32]) -> Decoded {
    // VOP3a/VOP3b: CI-ISA §13.3. Low dword: VDST [7:0], ABS [10:8], CLAMP [11],
    // OP [25:17]. High dword: SRC0 [8:0], SRC1 [17:9], SRC2 [26:18], OMOD [28:27],
    // NEG [31:29] — i.e. full-instruction bits [40:32]/[49:41]/[58:50]/[60:59]/[63:61]
    // relative to the 64-bit format table. Always two dwords; a missing second dword
    // decodes what it can.
    let w0 = words[0];
    let w1 = words.get(1).copied().unwrap_or(0);
    let op = ((w0 >> 17) & 0x1FF) as u16;
    // OP < 0x100 is the VOPC compare range promoted into VOP3 (VOP3b form). Its
    // comparison result is a scalar mask, so the destination is decoded as a general
    // scalar operand (`operand::decode_src`, which maps 106/107 → vcc, otherwise an
    // SGPR index) rather than a VGPR. Non-compare VOP3 uses VDST [7:0] as the VGPR
    // result. (CI-ISA §13.3 places the VOP3b scalar destination SDST at bits [14:8];
    // this decoder reads [7:0] — see the file's provenance-audit flag.)
    let vdst = if op < 0x100 {
        operand::decode_src(w0 & 0xFF)
    } else {
        Operand::Vgpr((w0 & 0xFF) as u8)
    };
    let abs = ((w0 >> 8) & 0x7) as u8;
    // CLAMP is bit [11] of the low dword, just above ABS [10:8] (CI-ISA §13.3 VOP3a).
    // llvm-mc --mcpu=gfx700 `v_mad_f32 v0, v1, v2, v3 clamp` emits low bytes
    // [0x00,0x08,0x82,0xd2] = 0xd2820800, which differs from the no-clamp form
    // (0xd2820000) only in bit 11 (0x800). Pinned by the witness test below.
    let clamp = (w0 >> 11) & 1 != 0;
    // OMOD [28:27] and NEG [31:29] of the high dword (CI-ISA §13.3 VOP3a fields
    // OMOD [59:58] / NEG [63:60] of the 64-bit instruction).
    let omod = ((w1 >> 27) & 0x3) as u8;
    let neg = ((w1 >> 29) & 0x7) as u8;
    let src0 = operand::decode_src(w1 & 0x1FF);
    let src1 = operand::decode_src((w1 >> 9) & 0x1FF);
    let src2 = operand::decode_src((w1 >> 18) & 0x1FF);
    decoded(
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
        },
        2,
    )
}

fn decode_vintrp(w0: u32) -> Decoded {
    // VINTRP: CI-ISA §13.4 "Vector Parameter Interpolation". VSRC [7:0],
    // ATTRCHAN [9:8], ATTR [15:10], OP [17:16], VDST [25:18]. Single dword.
    let vsrc = Operand::Vgpr((w0 & 0xFF) as u8);
    let attr = ((w0 >> 10) & 0x3F) as u8;
    let chan = ((w0 >> 8) & 0x3) as u8;
    let op = ((w0 >> 16) & 0x3) as u8;
    let vdst = Operand::Vgpr(((w0 >> 18) & 0xFF) as u8);
    decoded(
        Inst::Vintrp {
            op,
            vdst,
            vsrc,
            attr,
            chan,
        },
        1,
    )
}

fn decode_mubuf(words: &[u32]) -> Decoded {
    // MUBUF: CI-ISA §13.6 "Vector Memory Buffer". OFFSET [11:0], OFFEN [12],
    // IDXEN [13]; VADDR [39:32]=w1[7:0], VDATA [47:40]=w1[15:8], SRSRC [52:48]=w1[20:16]
    // (a resource constant "in units of four SGPRs" — hence ×4), SOFFSET [63:56]=w1[31:24].
    // OP is the 7-bit opcode at low-dword bits [24:18], witnessed by llvm-mc
    // (`buffer_load_dword … idxen` → OP = 12 = BUFFER_LOAD_DWORD); the §13.6 field
    // table prints OP as "[15:8]", which collides with OFFSET/OFFEN and is a known
    // doc typo, so the bit position is pinned to llvm-mc below.
    let w0 = words[0];
    let w1 = words.get(1).copied().unwrap_or(0);
    let op = ((w0 >> 18) & 0x7F) as u8;
    let offset = (w0 & 0xFFF) as u16;
    let offen = (w0 >> 12) & 1 == 1;
    let idxen = (w0 >> 13) & 1 == 1;

    let vaddr = Operand::Vgpr((w1 & 0xFF) as u8);
    let vdata = Operand::Vgpr(((w1 >> 8) & 0xFF) as u8);
    let srsrc = (((w1 >> 16) & 0x1F) * 4) as u8;
    // SOFFSET is an 8-bit field: an SGPR, an inline constant, or m0 — GFX7 does NOT
    // permit a 32-bit literal here (llvm-mc rejects one). Field 255 is therefore
    // invalid; keep it as a raw marker rather than fabricating a `Literal(0)`, which
    // would (wrongly) imply a trailing dword and desync the PC. MUBUF is
    // unconditionally two dwords regardless.
    let soffset_field = (w1 >> 24) & 0xFF;
    let soffset = if soffset_field == operand::LITERAL_SRC {
        Operand::Raw(soffset_field as u16)
    } else {
        operand::decode_src(soffset_field)
    };
    decoded(
        Inst::Mubuf {
            op,
            vdata,
            vaddr,
            srsrc,
            soffset,
            offset,
            idxen,
            offen,
        },
        2,
    )
}

fn decode_mimg(words: &[u32]) -> Decoded {
    // MIMG: CI-ISA §13.7 "Vector Memory Image". Unconditionally two dwords.
    //   w0: DMASK [11:8], UNORM [12], GLC [13], DA [14], R128 [15], TFE [16],
    //       LWE [17], OP [24:18].
    //   w1: VADDR [39:32]=w1[7:0], VDATA [47:40]=w1[15:8],
    //       SRSRC [52:48]=w1[20:16], SSAMP [57:53]=w1[25:21].
    // SRSRC (T#) and SSAMP (S#) are each "in units of four SGPRs" per §13.7 — hence ×4.
    let w0 = words[0];
    let w1 = words.get(1).copied().unwrap_or(0);
    let op = ((w0 >> 18) & 0x7F) as u8;
    let dmask = ((w0 >> 8) & 0xF) as u8;
    let unrm = (w0 >> 12) & 1 == 1;
    let vaddr = Operand::Vgpr((w1 & 0xFF) as u8);
    let vdata = Operand::Vgpr(((w1 >> 8) & 0xFF) as u8);
    let srsrc = (((w1 >> 16) & 0x1F) * 4) as u8;
    let ssamp = (((w1 >> 21) & 0x1F) * 4) as u8;
    decoded(
        Inst::Mimg {
            op,
            vdata,
            vaddr,
            srsrc,
            ssamp,
            dmask,
            unrm,
        },
        2,
    )
}

fn decode_exp(words: &[u32]) -> Decoded {
    // EXP: CI-ISA §13.8 "Export". EN [3:0], TGT [9:4], COMPR [10], DONE [11],
    // VM [12]; second dword carries VSRC0..3 as the four bytes VSRC0 [39:32]=w1[7:0],
    // VSRC1 [47:40]=w1[15:8], VSRC2 [55:48]=w1[23:16], VSRC3 [63:56]=w1[31:24].
    // A source is only meaningful where the matching EN bit is set. Two dwords.
    let w0 = words[0];
    let w1 = words.get(1).copied().unwrap_or(0);
    let en = (w0 & 0xF) as u8;
    let target = ExportTarget::decode(((w0 >> 4) & 0x3F) as u8);
    let compr = (w0 >> 10) & 1 == 1;
    let done = (w0 >> 11) & 1 == 1;
    let vm = (w0 >> 12) & 1 == 1;

    let vsrc = [
        (w1 & 0xFF) as u8,
        ((w1 >> 8) & 0xFF) as u8,
        ((w1 >> 16) & 0xFF) as u8,
        ((w1 >> 24) & 0xFF) as u8,
    ];
    let mut srcs = [None; 4];
    for (i, slot) in srcs.iter_mut().enumerate() {
        if en & (1 << i) != 0 {
            *slot = Some(Operand::Vgpr(vsrc[i]));
        }
    }
    decoded(
        Inst::Exp {
            target,
            srcs,
            done,
            compr,
            vm,
        },
        2,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::operand::Operand;

    /// Pins the per-format field extraction to real AMD-hardware encodings. Each
    /// right-hand dword pair is `llvm-mc --arch=amdgcn --mcpu=gfx700 --show-encoding`
    /// output for a representative instruction of every format this decoder handles
    /// (gfx700 = Bonaire = GCN2/Sea Islands = PS4 Liverpool); the field layouts those
    /// bits realize are the CI-ISA §13 microcode format tables named on each decoder.
    /// If any `[hi:lo]` extraction drifts, the decoded operands stop matching the
    /// assembler and this test fails.
    #[test]
    fn decoder_fields_match_amd_oracle() {
        // SOPP: `s_endpgm` -> [0x00,0x00,0x81,0xbf]. OP [22:16] = 1.
        assert_eq!(
            decode_one(&[0xbf81_0000]).inst,
            Inst::Sopp { op: 1, simm16: 0 }
        );
        // SOP1: `s_mov_b32 s0, s1` -> [0x01,0x03,0x80,0xbe]. OP [15:8]=3, SDST [22:16]=s0,
        // SSRC0 [7:0]=s1.
        assert_eq!(
            decode_one(&[0xbe80_0301]).inst,
            Inst::Sop1 {
                op: 3,
                sdst: Operand::Sgpr(0),
                ssrc0: Operand::Sgpr(1),
            }
        );
        // SOP2: `s_add_u32 s0, s1, s2` -> [0x01,0x02,0x00,0x80]. OP [29:23]=0,
        // SDST [22:16]=s0, SSRC0 [7:0]=s1, SSRC1 [15:8]=s2.
        assert_eq!(
            decode_one(&[0x8000_0201]).inst,
            Inst::Sop2 {
                op: 0,
                sdst: Operand::Sgpr(0),
                ssrc0: Operand::Sgpr(1),
                ssrc1: Operand::Sgpr(2),
            }
        );
        // SOPC: `s_cmp_eq_u32 s0, s1` -> [0x00,0x01,0x06,0xbf]. OP [22:16]=6,
        // SSRC0 [7:0]=s0, SSRC1 [15:8]=s1.
        assert_eq!(
            decode_one(&[0xbf06_0100]).inst,
            Inst::Sopc {
                op: 6,
                ssrc0: Operand::Sgpr(0),
                ssrc1: Operand::Sgpr(1),
            }
        );
        // SOPK: `s_movk_i32 s0, 0x1234` -> [0x34,0x12,0x00,0xb0]. OP [27:23]=0,
        // SDST [22:16]=s0, SIMM16 [15:0]=0x1234.
        assert_eq!(
            decode_one(&[0xb000_1234]).inst,
            Inst::Sopk {
                op: 0,
                sdst: Operand::Sgpr(0),
                simm16: 0x1234,
            }
        );
        // SMRD: `s_load_dword s0, s[2:3], 0x10` -> [0x10,0x03,0x00,0xc0]. OP [26:22]=0,
        // SDST [21:15]=s0, SBASE [14:9]=1 -> s[2:3] (×2), IMM [8]=1, OFFSET [7:0]=0x10.
        assert_eq!(
            decode_one(&[0xc000_0310]).inst,
            Inst::Smrd {
                op: 0,
                sdst: Operand::Sgpr(0),
                sbase: 2,
                imm: true,
                offset: 0x10,
            }
        );
        // VOP1: `v_mov_b32 v0, v1` -> [0x01,0x03,0x00,0x7e]. OP [16:9]=1, VDST [24:17]=v0,
        // SRC0 [8:0]=v1.
        assert_eq!(
            decode_one(&[0x7e00_0301]).inst,
            Inst::Vop1 {
                op: 1,
                vdst: Operand::Vgpr(0),
                src0: Operand::Vgpr(1),
            }
        );
        // VOP2: `v_add_f32 v0, v1, v2` -> [0x01,0x05,0x00,0x06]. OP [30:25]=3,
        // VDST [24:17]=v0, VSRC1 [16:9]=v2, SRC0 [8:0]=v1.
        assert_eq!(
            decode_one(&[0x0600_0501]).inst,
            Inst::Vop2 {
                op: 3,
                vdst: Operand::Vgpr(0),
                src0: Operand::Vgpr(1),
                vsrc1: Operand::Vgpr(2),
                k: None,
            }
        );
        // VOPC: `v_cmp_eq_f32 vcc, v0, v1` -> [0x00,0x03,0x04,0x7c]. OP [24:17]=2,
        // VSRC1 [16:9]=v1, SRC0 [8:0]=v0.
        assert_eq!(
            decode_one(&[0x7c04_0300]).inst,
            Inst::Vopc {
                op: 2,
                src0: Operand::Vgpr(0),
                vsrc1: Operand::Vgpr(1),
            }
        );
        // VINTRP: `v_interp_p1_f32 v0, v1, attr0.x` -> [0x01,0x00,0x00,0xc8].
        // VSRC [7:0]=v1, ATTRCHAN [9:8]=0(.x), ATTR [15:10]=0, OP [17:16]=0, VDST [25:18]=v0.
        assert_eq!(
            decode_one(&[0xc800_0001]).inst,
            Inst::Vintrp {
                op: 0,
                vdst: Operand::Vgpr(0),
                vsrc: Operand::Vgpr(1),
                attr: 0,
                chan: 0,
            }
        );
        // MUBUF: `buffer_load_dword v0, v1, s[4:7], 0 idxen`
        //   -> [0x00,0x20,0x30,0xe0, 0x01,0x00,0x01,0x80]. OP [24:18]=12, OFFSET [11:0]=0,
        // IDXEN [13]=1, VADDR=v1, VDATA=v0, SRSRC [20:16]=1 -> s[4:7] (×4),
        // SOFFSET [31:24]=128 -> inline constant 0.
        assert_eq!(
            decode_one(&[0xe030_2000, 0x8001_0001]).inst,
            Inst::Mubuf {
                op: 12,
                vdata: Operand::Vgpr(0),
                vaddr: Operand::Vgpr(1),
                srsrc: 4,
                soffset: Operand::InlineInt(0),
                offset: 0,
                idxen: true,
                offen: false,
            }
        );
        // MIMG: `image_sample v0, v1, s[8:15], s[16:19] dmask:0x1`
        //   -> [0x00,0x01,0x80,0xf0, 0x01,0x00,0x82,0x00]. OP [24:18]=0x20, DMASK [11:8]=1,
        // VADDR=v1, VDATA=v0, SRSRC [20:16]=2 -> s[8:15] (×4), SSAMP [25:21]=4 -> s[16:19] (×4).
        assert_eq!(
            decode_one(&[0xf080_0100, 0x0082_0001]).inst,
            Inst::Mimg {
                op: 0x20,
                vdata: Operand::Vgpr(0),
                vaddr: Operand::Vgpr(1),
                srsrc: 8,
                ssamp: 16,
                dmask: 1,
                unrm: false,
            }
        );
        // EXP: `exp mrt0 v0, v1, v2, v3` -> [0x0f,0x00,0x00,0xf8, 0x00,0x01,0x02,0x03].
        // EN [3:0]=0xf, TGT [9:4]=0(mrt0); VSRC0..3 = v0..v3 (the four bytes of w1).
        assert_eq!(
            decode_one(&[0xf800_000f, 0x0302_0100]).inst,
            Inst::Exp {
                target: ExportTarget::Mrt(0),
                srcs: [
                    Some(Operand::Vgpr(0)),
                    Some(Operand::Vgpr(1)),
                    Some(Operand::Vgpr(2)),
                    Some(Operand::Vgpr(3)),
                ],
                done: false,
                compr: false,
                vm: false,
            }
        );
        // VOP3a: `v_mad_f32 v0, v1, v2, v3 clamp` -> [0x00,0x08,0x82,0xd2, 0x01,0x05,0x0e,0x04].
        // OP [25:17]=0x141, VDST [7:0]=v0, CLAMP [11]=1; w1: SRC0 [8:0]=v1, SRC1 [17:9]=v2,
        // SRC2 [26:18]=v3, OMOD [28:27]=0, NEG [31:29]=0. The no-clamp form is 0xd2820000,
        // differing only in bit 11.
        assert_eq!(
            decode_one(&[0xd282_0800, 0x040e_0501]).inst,
            Inst::Vop3 {
                op: 0x141,
                vdst: Operand::Vgpr(0),
                src0: Operand::Vgpr(1),
                src1: Operand::Vgpr(2),
                src2: Operand::Vgpr(3),
                abs: 0,
                neg: 0,
                omod: 0,
                clamp: true,
            }
        );
    }
}
