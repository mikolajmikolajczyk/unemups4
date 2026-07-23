//! Typed GCN instruction (doc-4 §1, phase 4).
//!
//! One [`Inst`] variant per encoding class the corpus subset needs. Each carries
//! the decoded opcode plus its operands as [`Operand`]s. The interpreter (later)
//! matches on these; the disassembler renders them. Unhandled encodings decode to
//! [`Inst::Unknown`] rather than panicking (AC #2), and every instruction records
//! its length in dwords so the walk advances correctly past multi-dword forms
//! (AC #3).

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
        /// Absolute-value flags (bits [10:8] of the low dword).
        abs: u8,
        /// Negate flags (bits [63:61] of the high dword).
        neg: u8,
        /// Output modifier (bits [28:27] of the high dword): 0 = none, 1 = ×2,
        /// 2 = ×4, 3 = ÷2. The interpreter applies it to the result.
        omod: u8,
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
        /// SGPR index that begins the V# resource (4 SGPRs).
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
        /// SGPR index that begins the T# image resource (8 SGPRs = 256-bit).
        srsrc: u8,
        /// SGPR index that begins the S# sampler resource (4 SGPRs = 128-bit).
        ssamp: u8,
        /// Channel write mask (bits 0..3 = R,G,B,A); popcount = destination VGPR count.
        dmask: u8,
        /// `UNRM` — coordinates are unnormalized texel indices rather than [0,1].
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

/// An EXP instruction's destination class, decoded from the 6-bit target field.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ExportTarget {
    /// `mrt<n>` render target (target 0..=7).
    Mrt(u8),
    /// `mrtz` — depth (target 8).
    MrtZ,
    /// `null` — no target (target 9).
    Null,
    /// `pos<n>` — position export (target 12..=15).
    Pos(u8),
    /// `param<n>` — vertex parameter export (target 32..=63).
    Param(u8),
    /// A target value outside the named ranges.
    Raw(u8),
}

impl ExportTarget {
    /// Decode the 6-bit EXP `tgt` field.
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
