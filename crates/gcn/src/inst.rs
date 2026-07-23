//! Typed GCN instruction (doc-2 §1, phase 4).
//!
//! One [`Inst`] variant per encoding class the corpus subset needs. Each carries
//! the decoded opcode plus its operands as [`Operand`]s. The interpreter (later)
//! matches on these; the disassembler renders them. Unhandled encodings decode to
//! [`Inst::Unknown`] rather than panicking (AC #2), and every instruction records
//! its length in dwords so the walk advances correctly past multi-dword forms
//! (AC #3).
//!
//! Most of this file is our own decoded representation — the Rust struct/enum shape
//! is ours, not a hardware fact. Where a field carries an ISA-defined bit position,
//! descriptor size, or enumerated value, the doc-comment names the AMD source: the
//! GCN encoding facts are the AMD *Sea Islands (CIK/GFX7) Instruction Set Architecture*
//! (`oracles/amd/ci-isa.pdf`, the PS4 Liverpool ISA), and the empirical
//! GFX7 bit positions of the VOP3 modifier fields are pinned to `llvm-mc --mcpu=gfx700`
//! encodings (the byte literals live in `decoder.rs` / `disasm.rs`). The
//! `export_target_matches_ci_isa_enum` test below pins [`ExportTarget::decode`] to the
//! CI-ISA EXP target enumeration.

use crate::operand::Operand;

/// A decoded GCN instruction plus the number of dwords it occupied (1, 2, or —
/// for a VOP3 carrying a literal — more). The decoder returns this so the PC
/// advances by exactly the consumed length.
#[derive(Clone, PartialEq, Debug)]
pub struct Decoded {
    pub inst: Inst,
    /// Dwords consumed by this instruction (opcode word + any literal / second
    /// dword). Always ≥ 1.
    pub size_dwords: u32,
    /// This instruction's start position in the stream, in dwords from the start of
    /// the decoded buffer. Lets a consumer (interpreter / recompiler) correlate a
    /// `Decoded` back to its stream offset for patching or diagnostics.
    pub offset_dwords: u32,
}

/// One decoded GCN instruction, grouped by SI/CI encoding class.
#[derive(Clone, PartialEq, Debug)]
pub enum Inst {
    /// Scalar ALU, one input (SOP1): `op sdst, ssrc0`.
    Sop1 {
        op: u8,
        sdst: Operand,
        ssrc0: Operand,
    },
    /// Scalar ALU, two inputs (SOP2): `op sdst, ssrc0, ssrc1`.
    Sop2 {
        op: u8,
        sdst: Operand,
        ssrc0: Operand,
        ssrc1: Operand,
    },
    /// Scalar, sdst + 16-bit immediate (SOPK): `op sdst, simm16`.
    Sopk { op: u8, sdst: Operand, simm16: i16 },
    /// Scalar compare, two inputs (SOPC): `op ssrc0, ssrc1`.
    Sopc {
        op: u8,
        ssrc0: Operand,
        ssrc1: Operand,
    },
    /// Scalar program control (SOPP): `op simm16` (s_waitcnt, s_endpgm, s_branch…).
    Sopp { op: u8, simm16: u16 },
    /// Scalar memory read (SMRD): `op sdst, sbase, offset`.
    Smrd {
        op: u8,
        sdst: Operand,
        /// The SGPR that begins the resource/base pair (already a register index).
        sbase: u8,
        /// `true` when the offset is an inline immediate; `false` when it is an
        /// SGPR index carried in `offset`.
        imm: bool,
        offset: u32,
    },
    /// Vector ALU, one input (VOP1): `op vdst, src0`.
    Vop1 {
        op: u8,
        vdst: Operand,
        src0: Operand,
    },
    /// Vector ALU, two inputs (VOP2): `op vdst, src0, vsrc1`.
    Vop2 {
        op: u8,
        vdst: Operand,
        src0: Operand,
        vsrc1: Operand,
        /// The 32-bit K constant `v_madmk_f32`/`v_madak_f32` carry as their second
        /// dword. `None` for every other VOP2 op (which has no K).
        k: Option<u32>,
    },
    /// Vector ALU, three inputs / VOP3 encoding of a VOP1/2/C op: `op vdst, s0, s1, s2`.
    Vop3 {
        op: u16,
        vdst: Operand,
        src0: Operand,
        src1: Operand,
        src2: Operand,
        /// Per-operand absolute-value flags. CI-ISA VOP3a field `ABS` [10:8] (low dword):
        /// "if ABS[N] is set, take the floating-point absolute value of the N'th input
        /// operand"; applied before negation.
        abs: u8,
        /// Per-operand negate flags. CI-ISA VOP3a field `NEG`: "if NEG[N] is set, take the
        /// floating-point negation of the N'th input operand … applied after absolute
        /// value." The empirical GFX7 position is the high dword's top 3 bits [31:29]
        /// (absolute [63:61]) — llvm-mc `--mcpu=gfx700` places `NEG` there (the CI-ISA
        /// doc table's 8-bit SRC ranges are one bit narrow than the 9-bit encoding); the
        /// decode `(w1 >> 29) & 0x7` lives in `decoder.rs`.
        neg: u8,
        /// Output modifier. CI-ISA VOP3a field `OMOD` (enum(2)): 0 = no modification,
        /// 1 = multiply output by 2.0, 2 = multiply by 4.0, 3 = divide by 2.0; "applied
        /// before clamping." The interpreter applies it to the result. Empirical GFX7
        /// position is the high dword's bits [28:27] — llvm-mc `--mcpu=gfx700` encodes
        /// `v_mad_f32 … mul:2` as high dword `0x0c0e0501` (base `0x040e0501`), setting bit
        /// 27; decode `(w1 >> 27) & 0x3` lives in `decoder.rs`.
        omod: u8,
        /// Clamp / `saturate`. CI-ISA VOP3a field `CLAMP` (low-dword bit 11): saturate the
        /// f32 result to `[0.0, 1.0]`. Applied AFTER [`omod`](Self::Vop3::omod) — CI-ISA
        /// says `OMOD` is "applied before clamping", so the chain is result → omod → clamp.
        /// llvm-mc `--mcpu=gfx700` encodes `… clamp` as low dword `0xd2820800` (bit 11 set).
        clamp: bool,
    },
    /// Vector compare (VOPC): `op src0, vsrc1` → writes vcc.
    Vopc {
        op: u8,
        src0: Operand,
        vsrc1: Operand,
    },
    /// Parameter interpolation (VINTRP): `op vdst, vsrc, attr, chan`.
    Vintrp {
        op: u8,
        vdst: Operand,
        /// Barycentric VGPR (p1/p2) or, for p1, the I/J coordinate register.
        vsrc: Operand,
        attr: u8,
        chan: u8,
    },
    /// Untyped buffer load/store (MUBUF): `op vdata, vaddr, srsrc, soffset`.
    Mubuf {
        op: u8,
        vdata: Operand,
        vaddr: Operand,
        /// SGPR index that begins the V# buffer resource. CI-ISA §8 "Buffer Resource":
        /// it "is specified in four consecutive SGPRs (four aligned SGPRs)".
        srsrc: u8,
        soffset: Operand,
        offset: u16,
        idxen: bool,
        offen: bool,
    },
    /// Image memory op (MIMG): `op vdata, vaddr, srsrc, ssamp dmask:m`. The corpus
    /// uses `image_sample` — sample a texture (T# at `srsrc`, S# at `ssamp`) at the
    /// coordinates in the `vaddr` VGPR block, writing the enabled `dmask` channels to
    /// the `vdata` VGPR block.
    Mimg {
        op: u8,
        /// First VGPR of the destination block (one per enabled `dmask` channel).
        vdata: Operand,
        /// First VGPR of the address/coordinate block (u, v, … as f32).
        vaddr: Operand,
        /// SGPR index that begins the T# image resource. CI-ISA §8 "Image Resource": a
        /// T# is "stored in four or eight consecutive SGPRs"; the driver writes all image
        /// view descriptors as 256 bits, i.e. 8 SGPRs.
        srsrc: u8,
        /// SGPR index that begins the S# sampler resource. CI-ISA §8 "Sampler Resource":
        /// an S# is "a 128-bit constant in SGPRs", "defined in four consecutive SGPRs".
        ssamp: u8,
        /// Channel write mask. CI-ISA §8 DMASK: the texture unit sends the enabled
        /// components "starting with R, then G, B, and A" (bits 0..3 = R,G,B,A), so the
        /// popcount is the number of destination VGPRs (Dwords) received.
        dmask: u8,
        /// `UNRM` — CI-ISA §8: "force address to be un-normalized regardless of T#" (texel
        /// indices rather than [0,1] coordinates).
        unrm: bool,
    },
    /// Export to a pixel/position/param target (EXP): `exp tgt, v0..v3`.
    Exp {
        target: ExportTarget,
        /// The four export source VGPRs (or `None` for a disabled channel).
        srcs: [Option<Operand>; 4],
        done: bool,
        compr: bool,
        vm: bool,
    },
    /// A valid-length-unknown or unhandled encoding. Carries the raw dword(s) it
    /// consumed (the first, plus any trailing dword read as part of a recognized
    /// multi-dword shape whose op was still unmapped) so a trace can show them and a
    /// later pass can correlate/patch. The walk continues past it by a single dword.
    Unknown {
        raw: u32,
        /// Any additional raw dwords consumed beyond `raw` (empty for the common
        /// single-dword unknown).
        raw_words: Vec<u32>,
    },
}

/// An EXP instruction's destination class, decoded from the 6-bit `TGT` target field.
/// The value ranges are the CI-ISA §13.8 "Export Instruction" `TGT` [9:4] enumeration
/// (`EXP_MRT` 0–7, `EXP_MRTZ` 8, `EXP_NULL` 9, `EXP_POS` 12–15, `EXP_PARAM` 32–63; "all
/// other values are reserved").
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ExportTarget {
    /// `mrt<n>` render target. CI-ISA `EXP_MRT` = TGT 0..=7 ("output to color MRT 0;
    /// increment from here for additional MRTs").
    Mrt(u8),
    /// `mrtz` — depth. CI-ISA `EXP_MRTZ` = TGT 8 ("output to Z").
    MrtZ,
    /// `null` — no target. CI-ISA `EXP_NULL` = TGT 9.
    Null,
    /// `pos<n>` — position export. CI-ISA `EXP_POS` = TGT 12..=15 ("output to position 0;
    /// increment from here for additional positions").
    Pos(u8),
    /// `param<n>` — vertex parameter export. CI-ISA `EXP_PARAM` = TGT 32..=63 ("output to
    /// parameter 0; increment from here for additional parameters").
    Param(u8),
    /// A `TGT` value in the CI-ISA "reserved" ranges (10–11, 16–31), kept verbatim.
    Raw(u8),
}

impl ExportTarget {
    /// Decode the 6-bit EXP `TGT` field per the CI-ISA §13.8 enumeration. Glue over the
    /// enumerated ranges named on the [`ExportTarget`] variants above.
    pub fn decode(tgt: u8) -> ExportTarget {
        match tgt {
            0..=7 => ExportTarget::Mrt(tgt),
            8 => ExportTarget::MrtZ,
            9 => ExportTarget::Null,
            12..=15 => ExportTarget::Pos(tgt - 12),
            32..=63 => ExportTarget::Param(tgt - 32),
            other => ExportTarget::Raw(other),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pins [`ExportTarget::decode`] to the AMD CI-ISA §13.8 "Export Instruction" `TGT`
    /// [9:4] enumeration: `EXP_MRT` 0–7, `EXP_MRTZ` 8, `EXP_NULL` 9, `EXP_POS` 12–15,
    /// `EXP_PARAM` 32–63, all other values reserved. The right-hand expectations are those
    /// literal ranges; this fails if our decode drifts from the AMD enumeration.
    #[test]
    fn export_target_matches_ci_isa_enum() {
        // EXP_MRT = 0..=7, indexed from MRT 0.
        assert_eq!(ExportTarget::decode(0), ExportTarget::Mrt(0));
        assert_eq!(ExportTarget::decode(7), ExportTarget::Mrt(7));
        // EXP_MRTZ = 8, EXP_NULL = 9.
        assert_eq!(ExportTarget::decode(8), ExportTarget::MrtZ);
        assert_eq!(ExportTarget::decode(9), ExportTarget::Null);
        // EXP_POS = 12..=15, indexed from position 0.
        assert_eq!(ExportTarget::decode(12), ExportTarget::Pos(0));
        assert_eq!(ExportTarget::decode(15), ExportTarget::Pos(3));
        // EXP_PARAM = 32..=63, indexed from parameter 0.
        assert_eq!(ExportTarget::decode(32), ExportTarget::Param(0));
        assert_eq!(ExportTarget::decode(63), ExportTarget::Param(31));
        // CI-ISA "reserved" values (10, 11, 16..=31) are not one of the named targets.
        assert_eq!(ExportTarget::decode(10), ExportTarget::Raw(10));
        assert_eq!(ExportTarget::decode(11), ExportTarget::Raw(11));
        assert_eq!(ExportTarget::decode(16), ExportTarget::Raw(16));
        assert_eq!(ExportTarget::decode(31), ExportTarget::Raw(31));
    }
}
