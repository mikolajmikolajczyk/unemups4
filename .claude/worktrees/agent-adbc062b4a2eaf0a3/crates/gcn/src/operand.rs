//! GCN instruction operands (doc-4 §1, phase 4).
//!
//! An operand is one 9-bit SI/CI source field, or a scalar/vector destination
//! register. The 9-bit source-operand encoding is shared across the scalar and
//! vector encodings (SSRC/SRC fields): 0..=103 are SGPRs, a fixed block of
//! special registers (vcc/exec/m0/…), a run of inline integer/float constants,
//! 255 = a trailing 32-bit literal, and 256..=511 are VGPRs. This module is the
//! single decode of that field so every encoding class agrees on it.

/// A trailing 32-bit literal is signalled by source-field value 255; the literal
/// itself is the next dword in the stream.
pub const LITERAL_SRC: u32 = 255;

/// One decoded operand.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Operand {
    /// Scalar GPR `s<n>` (0..=103).
    Sgpr(u8),
    /// Vector GPR `v<n>` (0..=255).
    Vgpr(u8),
    /// A named special scalar register (vcc, exec, m0, scc, …).
    Special(SpecialReg),
    /// An inline constant that decodes to a fixed integer value (`0`, `1`, …,
    /// `-16`). The i64 is the constant's integer value.
    InlineInt(i64),
    /// An inline constant that decodes to a fixed float value (`0.5`, `1.0`, …).
    /// The f32 is the constant's value; disassembly prints it as GCN does.
    InlineFloat(f32),
    /// A trailing 32-bit literal constant (source field 255) — consumes one extra
    /// dword. The interpreter/recompiler reads the value directly.
    Literal(u32),
    /// A raw 9-bit source field the decoder did not map to a named operand. Kept
    /// so an unusual-but-valid field never forces the whole instruction to
    /// `Unknown`; disassembly prints it as `src(N)`.
    Raw(u16),
}

/// The named special scalar registers reachable through the 9-bit source field
/// (SI/CI). Only the members the corpus and the near-term interpreter need are
/// named; anything else in the special block decodes to [`Operand::Raw`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SpecialReg {
    /// `vcc_lo` (source field 106).
    VccLo,
    /// `vcc_hi` (source field 107).
    VccHi,
    /// `m0` (source field 124) — interpolation base / LDS/GDS addressing.
    M0,
    /// `exec_lo` (source field 126).
    ExecLo,
    /// `exec_hi` (source field 127).
    ExecHi,
    /// `scc` (source field 253).
    Scc,
}

impl SpecialReg {
    /// The disassembly mnemonic (matches llvm-mc / AMD output).
    pub fn name(self) -> &'static str {
        match self {
            SpecialReg::VccLo => "vcc_lo",
            SpecialReg::VccHi => "vcc_hi",
            SpecialReg::M0 => "m0",
            SpecialReg::ExecLo => "exec_lo",
            SpecialReg::ExecHi => "exec_hi",
            SpecialReg::Scc => "scc",
        }
    }
}

/// Decode the 9-bit SI/CI source-operand field (SSRC0/SSRC1/SRC0/…). `field` must
/// be a 9-bit value; higher bits are ignored. Returns the operand and, for the
/// literal case, [`Operand::Literal`] with a placeholder `0` — the caller splices
/// in the real dword after checking [`Operand::is_literal`].
///
/// This is a *total* function: every 9-bit value maps to some operand, never a
/// panic (AC #2).
pub fn decode_src(field: u32) -> Operand {
    match field & 0x1FF {
        s @ 0..=103 => Operand::Sgpr(s as u8),
        104 | 105 => Operand::Raw((field & 0x1FF) as u16), // flat_scr_lo/hi (unnamed)
        106 => Operand::Special(SpecialReg::VccLo),
        107 => Operand::Special(SpecialReg::VccHi),
        124 => Operand::Special(SpecialReg::M0),
        126 => Operand::Special(SpecialReg::ExecLo),
        127 => Operand::Special(SpecialReg::ExecHi),
        v @ 128..=192 => Operand::InlineInt(i64::from(v) - 128), // 0..=64
        v @ 193..=208 => Operand::InlineInt(-(i64::from(v) - 192)), // -1..=-16
        240 => Operand::InlineFloat(0.5),
        241 => Operand::InlineFloat(-0.5),
        242 => Operand::InlineFloat(1.0),
        243 => Operand::InlineFloat(-1.0),
        244 => Operand::InlineFloat(2.0),
        245 => Operand::InlineFloat(-2.0),
        246 => Operand::InlineFloat(4.0),
        247 => Operand::InlineFloat(-4.0),
        LITERAL_SRC => Operand::Literal(0),
        253 => Operand::Special(SpecialReg::Scc),
        v @ 256..=511 => Operand::Vgpr((v - 256) as u8),
        other => Operand::Raw(other as u16),
    }
}

impl Operand {
    /// Whether this operand is a trailing 32-bit literal (source field 255) and so
    /// consumes an extra dword. The decoder uses this to advance the PC (AC #3).
    pub fn is_literal(self) -> bool {
        matches!(self, Operand::Literal(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_gpr_ranges() {
        assert_eq!(decode_src(0), Operand::Sgpr(0));
        assert_eq!(decode_src(103), Operand::Sgpr(103));
        assert_eq!(decode_src(256), Operand::Vgpr(0));
        assert_eq!(decode_src(511), Operand::Vgpr(255));
    }

    #[test]
    fn maps_inline_constants() {
        assert_eq!(decode_src(128), Operand::InlineInt(0));
        assert_eq!(decode_src(129), Operand::InlineInt(1));
        assert_eq!(decode_src(192), Operand::InlineInt(64));
        assert_eq!(decode_src(193), Operand::InlineInt(-1));
        assert_eq!(decode_src(208), Operand::InlineInt(-16));
        assert_eq!(decode_src(242), Operand::InlineFloat(1.0));
        assert_eq!(decode_src(240), Operand::InlineFloat(0.5));
    }

    #[test]
    fn maps_special_and_literal() {
        assert_eq!(decode_src(106), Operand::Special(SpecialReg::VccLo));
        assert_eq!(decode_src(124), Operand::Special(SpecialReg::M0));
        assert_eq!(decode_src(126), Operand::Special(SpecialReg::ExecLo));
        assert!(decode_src(LITERAL_SRC).is_literal());
    }

    #[test]
    fn total_over_full_9bit_range() {
        // No 9-bit source value panics.
        for f in 0u32..512 {
            let _ = decode_src(f);
        }
    }
}
