//! PM4 packet *emitters* for the HLE Gnm shader-set builders (doc-3 §2).
//!
//! `sceGnmSetVsShader` / `sceGnmSetPsShader` are, on real hardware, guest-side gnmx
//! builders that write a fixed number of PM4 dwords (29 for VS, 40 for PS — doc-3 §2)
//! into the caller's command buffer from a shader register-setup block. When a game
//! links them from `libSceGnmDriver` (rather than statically), the emulator provides
//! the body; these emitters produce the PM4 so the HLE-linked path and a
//! statically-linked builder both converge on the same shadow register file (doc-4
//! §5): the executor's `derive_bound_shaders` reads `SPI_SHADER_PGM_LO/HI/RSRC1/2`
//! back and produces a `ShaderRef::GcnBinary`.
//!
//! # Register-block layout and packet stream
//!
//! The register block (`vsregs`/`psregs`) is the Sony Gnm `VsStageRegisters` /
//! `PsStageRegisters` struct. The **exact** field→register mapping, packet grouping,
//! register offsets, and trailing-NOP size below mirror the retail builder as
//! reconstructed in shadPS4 `src/core/libraries/gnmdriver/gnmdriver.cpp`
//! (`sceGnmSetVsShader` / `sceGnmSetPsShader`). Each register's absolute index is the
//! standard AMD SI/GFX6 offset from Mesa's `src/amd/common/sid.h` (see
//! [`crate::pm4::opcodes::context_reg`] / [`crate::pm4::opcodes::sh_reg`]); the in-repo
//! corpus corroborates one anchor — `CONTEXT_SPI_SHADER_COL_FORMAT 0x01C5` in
//! `examples/ps4-pm4-test/ps4-pm4-test/main.c` equals `SPI_SHADER_COL_FORMAT` here.
//!
//! `VsStageRegisters` fields consumed (indices into the block):
//! ```text
//! [0] PGM_LO_VS   [2] RSRC1_VS   [4] SPI_VS_OUT_CONFIG    [6] PA_CL_VS_OUT_CNTL
//! [1] (PGM_HI_VS) [3] RSRC2_VS   [5] SPI_SHADER_POS_FORMAT
//! ```
//! `PsStageRegisters` fields consumed:
//! ```text
//! [0] PGM_LO_PS   [4] Z_FORMAT     [6] PS_INPUT_ENA   [8]  PS_IN_CONTROL   [10] DB_SHADER_CONTROL
//! [1] (PGM_HI_PS) [5] COL_FORMAT   [7] PS_INPUT_ADDR  [9]  BARYC_CNTL      [11] CB_SHADER_MASK
//! [2] RSRC1_PS    [3] RSRC2_PS
//! ```
//!
//! Faithful to retail, `PGM_HI` is written as **0** (the driver validates `regs[1] == 0`
//! and hardware needs only the low program-address dword), the shader-program registers
//! are two separate `SET_SH_REG` runs (`PGM_LO/HI`, then `RSRC1/RSRC2`), the pipeline
//! state is per-register `SET_CONTEXT_REG` runs, and the stream ends with an 11-dword
//! `IT_NOP` data block — the retail builder emits this same trailing NOP (`WriteTrailingNop<11>`),
//! so the caller's `cmd` advances by exactly 29/40 (doc-3 §2). This is not filler
//! standing in for un-emitted state: every meaningful register the struct carries is a
//! real write here. Vulkan-free.

use crate::pm4::opcodes::{context_reg, op, reg_base, sh_reg, t3_header};

/// Documented total PM4 dword count `sceGnmSetVsShader` writes (doc-3 §2).
pub const SET_VS_SHADER_DWORDS: usize = 29;
/// Documented total PM4 dword count `sceGnmSetPsShader` writes (doc-3 §2).
pub const SET_PS_SHADER_DWORDS: usize = 40;

/// Number of `VsStageRegisters` dwords the emitter maps to real register writes.
pub const VS_STAGE_REG_FIELDS: usize = 7;
/// Number of `PsStageRegisters` dwords the emitter maps to real register writes.
pub const PS_STAGE_REG_FIELDS: usize = 12;

/// Number of leading `vsregs`/`psregs` dwords that are the SH shader-program run
/// `[PGM_LO, PGM_HI, PGM_RSRC1, PGM_RSRC2]` (the values the derived view consumes).
pub const SHADER_PGM_REG_RUN: usize = 4;

/// Named `VsStageRegisters` field indices (dword offset into the `vsregs` block). Each
/// names the register the emitter feeds from that slot, so a field addition/reorder that
/// shifts the mapping is a visible edit here rather than a silent literal drift.
mod vs_field {
    /// `[0] SPI_SHADER_PGM_LO_VS` — low half of the shader program address.
    pub const PGM_LO: usize = 0;
    /// `[1] SPI_SHADER_PGM_HI_VS` — high half; retail invariant is 0 (see [`PGM_HI`]).
    pub const PGM_HI: usize = 1;
    /// `[2] SPI_SHADER_PGM_RSRC1_VS`.
    pub const PGM_RSRC1: usize = 2;
    /// `[3] SPI_SHADER_PGM_RSRC2_VS`.
    pub const PGM_RSRC2: usize = 3;
    /// `[4] SPI_VS_OUT_CONFIG`.
    pub const SPI_VS_OUT_CONFIG: usize = 4;
    /// `[5] SPI_SHADER_POS_FORMAT`.
    pub const SPI_SHADER_POS_FORMAT: usize = 5;
    /// `[6] PA_CL_VS_OUT_CNTL`.
    pub const PA_CL_VS_OUT_CNTL: usize = 6;
}

/// Named `PsStageRegisters` field indices (dword offset into the `psregs` block).
mod ps_field {
    /// `[0] SPI_SHADER_PGM_LO_PS` — low half of the shader program address.
    pub const PGM_LO: usize = 0;
    /// `[1] SPI_SHADER_PGM_HI_PS` — high half; retail invariant is 0 (see [`PGM_HI`]).
    pub const PGM_HI: usize = 1;
    /// `[2] SPI_SHADER_PGM_RSRC1_PS`.
    pub const PGM_RSRC1: usize = 2;
    /// `[3] SPI_SHADER_PGM_RSRC2_PS`.
    pub const PGM_RSRC2: usize = 3;
    /// `[4] SPI_SHADER_Z_FORMAT`.
    pub const SPI_SHADER_Z_FORMAT: usize = 4;
    /// `[5] SPI_SHADER_COL_FORMAT`.
    pub const SPI_SHADER_COL_FORMAT: usize = 5;
    /// `[6] SPI_PS_INPUT_ENA`.
    pub const SPI_PS_INPUT_ENA: usize = 6;
    /// `[7] SPI_PS_INPUT_ADDR`.
    pub const SPI_PS_INPUT_ADDR: usize = 7;
    /// `[8] SPI_PS_IN_CONTROL`.
    pub const SPI_PS_IN_CONTROL: usize = 8;
    /// `[9] SPI_BARYC_CNTL`.
    pub const SPI_BARYC_CNTL: usize = 9;
    /// `[10] DB_SHADER_CONTROL`.
    pub const DB_SHADER_CONTROL: usize = 10;
    /// `[11] CB_SHADER_MASK`.
    pub const CB_SHADER_MASK: usize = 11;
}

/// Size of the retail trailing `IT_NOP` data block (`WriteTrailingNop<11>` in shadPS4).
/// The emitted NOP occupies `NOP_DATA_BLOCK + 1` dwords (header + data).
const NOP_DATA_BLOCK: usize = 11;

/// One `SET_*_REG` run: a `bank` window base (a `reg_base::*`), the register offset
/// *relative to that base*, and the contiguous values written at `offset, offset+1, …`.
/// Emitted as `SET_SH_REG` / `SET_CONTEXT_REG` by bank.
///
/// The inline `data` array holds up to 2 dwords (all shader-set runs are 1 or 2 values),
/// so no heap allocation occurs per run.
struct RegRun {
    bank: u32,
    offset: u32,
    /// Inline storage; `len` ≤ 2 for all shader-set runs.
    data: [u32; 2],
    len: usize,
}

impl RegRun {
    /// The values slice for this run.
    fn values(&self) -> &[u32] {
        &self.data[..self.len]
    }
}

/// A run of 1 or 2 `values` written at the absolute register index `abs_index` (and up).
fn run(bank: u32, abs_index: u32, values: [u32; 2], len: usize) -> RegRun {
    debug_assert!(
        (1..=2).contains(&len),
        "shader-set run length must be 1 or 2"
    );
    RegRun {
        bank,
        offset: abs_index - bank,
        data: values,
        len,
    }
}

/// Convenience wrapper for a single-value run.
fn run1(bank: u32, abs_index: u32, v0: u32) -> RegRun {
    run(bank, abs_index, [v0, 0], 1)
}

/// Convenience wrapper for a two-value run.
fn run2(bank: u32, abs_index: u32, v0: u32, v1: u32) -> RegRun {
    run(bank, abs_index, [v0, v1], 2)
}

/// Read `regs[i]`, treating a short block as 0 (a truncated guest struct never OOBs).
fn r(regs: &[u32], i: usize) -> u32 {
    regs.get(i).copied().unwrap_or(0)
}

/// The two SH shader-program-address dwords `[PGM_LO, PGM_HI]` for the shader run.
///
/// Retail's gnmdriver validates `regs[HI] == 0` and *rejects* a non-zero high half; the
/// emitted packet always writes HI as 0 (the hardware invariant). We keep that forcing
/// but surface a non-zero incoming HI as a warning instead of silently discarding it —
/// otherwise the dropped high bits would yield a wrong program address downstream
/// (`MagicNotFound`) with no signal at the point the value was lost.
fn shader_pgm_lohi(regs: &[u32], lo_field: usize, hi_field: usize, stage: &str) -> [u32; 2] {
    let hi = r(regs, hi_field);
    if hi != 0 {
        tracing::warn!(
            stage,
            pgm_hi = format_args!("{hi:#010x}"),
            "non-zero {stage} SPI_SHADER_PGM_HI forced to 0 (retail rejects non-zero); \
             program address high bits dropped",
        );
    }
    [r(regs, lo_field), 0]
}

/// Emit the VS shader-set PM4 (29 dwords) from a `vsregs` `VsStageRegisters` block.
/// Mirrors shadPS4 `sceGnmSetVsShader`: two SH runs (`PGM_LO/HI`, `RSRC1/RSRC2`) then
/// per-register CONTEXT runs, `PGM_HI` forced to 0. `shader_modifier` handling (the
/// `RSRC1` mix) is left to the caller — the register block is written verbatim.
pub fn set_vs_shader(vs_regs: &[u32]) -> Vec<u32> {
    let lohi = shader_pgm_lohi(vs_regs, vs_field::PGM_LO, vs_field::PGM_HI, "VS");
    let runs = [
        // SH `SPI_SHADER_PGM_LO_VS`/`_HI_VS` — HI forced to 0 (retail invariant).
        run2(reg_base::SH, sh_reg::SPI_SHADER_PGM_LO_VS, lohi[0], lohi[1]),
        // SH `SPI_SHADER_PGM_RSRC1_VS`/`_RSRC2_VS` — a separate run.
        run2(
            reg_base::SH,
            sh_reg::SPI_SHADER_PGM_RSRC1_VS,
            r(vs_regs, vs_field::PGM_RSRC1),
            r(vs_regs, vs_field::PGM_RSRC2),
        ),
        // CONTEXT pipeline state (retail order: PA_CL, then VS_OUT_CONFIG, POS_FORMAT).
        run1(
            reg_base::CONTEXT,
            context_reg::PA_CL_VS_OUT_CNTL,
            r(vs_regs, vs_field::PA_CL_VS_OUT_CNTL),
        ),
        run1(
            reg_base::CONTEXT,
            context_reg::SPI_VS_OUT_CONFIG,
            r(vs_regs, vs_field::SPI_VS_OUT_CONFIG),
        ),
        run1(
            reg_base::CONTEXT,
            context_reg::SPI_SHADER_POS_FORMAT,
            r(vs_regs, vs_field::SPI_SHADER_POS_FORMAT),
        ),
    ];
    emit_shader_set(&runs, SET_VS_SHADER_DWORDS)
}

/// Emit the PS shader-set PM4 (40 dwords) from a `psregs` `PsStageRegisters` block.
/// Mirrors shadPS4 `sceGnmSetPsShader`: two SH runs then per-/paired CONTEXT runs for
/// the export / interpolation state, `PGM_HI` forced to 0.
pub fn set_ps_shader(ps_regs: &[u32]) -> Vec<u32> {
    let lohi = shader_pgm_lohi(ps_regs, ps_field::PGM_LO, ps_field::PGM_HI, "PS");
    let runs = [
        // SH `SPI_SHADER_PGM_LO_PS`/`_HI_PS` — HI forced to 0 (retail invariant).
        run2(reg_base::SH, sh_reg::SPI_SHADER_PGM_LO_PS, lohi[0], lohi[1]),
        // SH `SPI_SHADER_PGM_RSRC1_PS`/`_RSRC2_PS` — a separate run.
        run2(
            reg_base::SH,
            sh_reg::SPI_SHADER_PGM_RSRC1_PS,
            r(ps_regs, ps_field::PGM_RSRC1),
            r(ps_regs, ps_field::PGM_RSRC2),
        ),
        // CONTEXT `SPI_SHADER_Z_FORMAT`/`_COL_FORMAT` (contiguous pair).
        run2(
            reg_base::CONTEXT,
            context_reg::SPI_SHADER_Z_FORMAT,
            r(ps_regs, ps_field::SPI_SHADER_Z_FORMAT),
            r(ps_regs, ps_field::SPI_SHADER_COL_FORMAT),
        ),
        // CONTEXT `SPI_PS_INPUT_ENA`/`_ADDR` (contiguous pair).
        run2(
            reg_base::CONTEXT,
            context_reg::SPI_PS_INPUT_ENA,
            r(ps_regs, ps_field::SPI_PS_INPUT_ENA),
            r(ps_regs, ps_field::SPI_PS_INPUT_ADDR),
        ),
        run1(
            reg_base::CONTEXT,
            context_reg::SPI_PS_IN_CONTROL,
            r(ps_regs, ps_field::SPI_PS_IN_CONTROL),
        ),
        run1(
            reg_base::CONTEXT,
            context_reg::SPI_BARYC_CNTL,
            r(ps_regs, ps_field::SPI_BARYC_CNTL),
        ),
        run1(
            reg_base::CONTEXT,
            context_reg::DB_SHADER_CONTROL,
            r(ps_regs, ps_field::DB_SHADER_CONTROL),
        ),
        run1(
            reg_base::CONTEXT,
            context_reg::CB_SHADER_MASK,
            r(ps_regs, ps_field::CB_SHADER_MASK),
        ),
    ];
    emit_shader_set(&runs, SET_PS_SHADER_DWORDS)
}

/// Dwords a single [`RegRun`] emits: a type-3 header, the offset dword, then one dword
/// per value.
fn run_dwords(run: &RegRun) -> usize {
    // header + offset + values
    2 + run.values().len()
}

/// Build the shader-set PM4 stream from `runs` (each a `SET_SH_REG` or
/// `SET_CONTEXT_REG` run, chosen by the run's `bank`), then the retail trailing
/// [`NOP_DATA_BLOCK`]-dword `IT_NOP`.
///
/// The emitted length is *self-derived* from the runs plus the trailing NOP, so adding or
/// removing a run automatically re-sizes the NOP-terminated stream — it can never emit a
/// wrong-length pad in release (a NOP that over-/under-claims would let a decoder swallow
/// or resync mid-stream). `documented_dwords` is the ABI total from doc-3 §2 carried
/// alongside purely as a cross-check: the derived length must equal it, so the two can't
/// silently diverge, but the derivation — not the constant — drives the actual output.
fn emit_shader_set(runs: &[RegRun], documented_dwords: usize) -> Vec<u32> {
    // Self-derived: the sum of every run plus the trailing NOP (header + data block).
    let total_dwords: usize = runs.iter().map(run_dwords).sum::<usize>() + 1 + NOP_DATA_BLOCK;
    // The documented ABI constant is a cross-check, not the source of truth; if a run is
    // added/removed the derived total moves and this guards the doc-3 constant from drift.
    debug_assert_eq!(
        total_dwords, documented_dwords,
        "self-derived shader-set length must match the documented ABI total",
    );

    let mut out = Vec::with_capacity(total_dwords);
    for run in runs {
        let opcode = match run.bank {
            reg_base::SH => op::IT_SET_SH_REG,
            reg_base::CONTEXT => op::IT_SET_CONTEXT_REG,
            // Only SH/CONTEXT runs are constructed above; guard the invariant.
            _ => unreachable!("shader-set run must target SH or CONTEXT bank"),
        };
        // Body = offset dword + one dword per value.
        out.push(t3_header(opcode, 1 + run.values().len()));
        out.push(run.offset);
        out.extend_from_slice(run.values());
    }
    // Trailing retail NOP claims exactly its own body length (`NOP_DATA_BLOCK` dwords), so
    // a decoder walking the stream lands on the next packet's header. `out` is now exactly
    // `total_dwords - (1 + NOP_DATA_BLOCK)` long, so header + resize fill the rest exactly.
    out.push(t3_header(op::IT_NOP, NOP_DATA_BLOCK));
    out.resize(total_dwords, 0);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pm4::decode::{self, OwnedPacket};
    use crate::pm4::opcodes::op;
    use crate::pm4::opcodes::{context_reg, sh_reg};
    use crate::state::GpuState;

    /// Apply an emitted DCB through the same shadow-register path the executor uses
    /// (SET_*_REG → `apply_set_reg`) and return the resulting state.
    fn apply(dcb: &[u32]) -> GpuState {
        let mut bytes = Vec::with_capacity(dcb.len() * 4);
        for w in dcb {
            bytes.extend_from_slice(&w.to_le_bytes());
        }
        let mut state = GpuState::default();
        for pkt in decode::decode_bytes(&bytes) {
            if let OwnedPacket::Type3 { opcode, body, .. } = pkt
                && let Some(base) = crate::pm4::opcodes::set_reg_base(opcode)
            {
                state.apply_set_reg(base, &body);
            }
        }
        state
    }

    /// A representative `VsStageRegisters` block: a distinct sentinel per field so a
    /// mis-routed dword shows up as the wrong value in the wrong bank. `[1]` (PGM_HI)
    /// is 0 — the retail invariant — and is forced to 0 by the emitter regardless.
    fn sample_vs() -> [u32; VS_STAGE_REG_FIELDS] {
        [
            0x0000_2000, // [0] PGM_LO_VS
            0x0000_0000, // [1] PGM_HI_VS (must be 0)
            0x00AB_CDEF, // [2] RSRC1_VS
            0x0000_00A0, // [3] RSRC2_VS
            0x0000_0005, // [4] SPI_VS_OUT_CONFIG
            0x0000_0004, // [5] SPI_SHADER_POS_FORMAT
            0x0000_00FF, // [6] PA_CL_VS_OUT_CNTL
        ]
    }

    /// A representative `PsStageRegisters` block.
    fn sample_ps() -> [u32; PS_STAGE_REG_FIELDS] {
        [
            0x0000_3000, // [0] PGM_LO_PS
            0x0000_0000, // [1] PGM_HI_PS (must be 0)
            0x0012_3456, // [2] RSRC1_PS
            0x0000_0040, // [3] RSRC2_PS
            0x0000_0000, // [4] SPI_SHADER_Z_FORMAT
            0x0000_0004, // [5] SPI_SHADER_COL_FORMAT
            0x0000_000F, // [6] SPI_PS_INPUT_ENA
            0x0000_000F, // [7] SPI_PS_INPUT_ADDR
            0x0000_0001, // [8] SPI_PS_IN_CONTROL
            0x0000_0002, // [9] SPI_BARYC_CNTL
            0x0000_0010, // [10] DB_SHADER_CONTROL
            0x0000_000F, // [11] CB_SHADER_MASK
        ]
    }

    #[test]
    fn vs_emit_is_29_dwords_and_round_trips_every_field() {
        // AC #1/#2: sceGnmSetVsShader emit is exactly 29 dwords and the decoder
        // round-trips every VsStageRegisters field into the correct bank (SH vs
        // CONTEXT) — PGM_LO/HI/RSRC1/2 into SH, the pipeline config into CONTEXT.
        let vs = sample_vs();
        let dcb = set_vs_shader(&vs);
        assert_eq!(dcb.len(), SET_VS_SHADER_DWORDS);

        let state = apply(&dcb);
        // SH bank: the four shader-program registers (HI written as 0).
        assert_eq!(state.sh_regs.get(sh_reg::SPI_SHADER_PGM_LO_VS), Some(vs[0]));
        assert_eq!(state.sh_regs.get(sh_reg::SPI_SHADER_PGM_HI_VS), Some(0));
        assert_eq!(
            state.sh_regs.get(sh_reg::SPI_SHADER_PGM_RSRC1_VS),
            Some(vs[2])
        );
        assert_eq!(
            state.sh_regs.get(sh_reg::SPI_SHADER_PGM_RSRC2_VS),
            Some(vs[3])
        );
        // CONTEXT bank: VS pipeline config.
        assert_eq!(
            state.ctx_regs.get(context_reg::SPI_VS_OUT_CONFIG),
            Some(vs[4])
        );
        assert_eq!(
            state.ctx_regs.get(context_reg::SPI_SHADER_POS_FORMAT),
            Some(vs[5])
        );
        assert_eq!(
            state.ctx_regs.get(context_reg::PA_CL_VS_OUT_CNTL),
            Some(vs[6])
        );
        // None of the VS context regs leaked into the SH bank (bank routing).
        assert_eq!(state.sh_regs.get(context_reg::SPI_VS_OUT_CONFIG), None);
    }

    #[test]
    fn ps_emit_is_40_dwords_and_round_trips_every_field() {
        let ps = sample_ps();
        let dcb = set_ps_shader(&ps);
        assert_eq!(dcb.len(), SET_PS_SHADER_DWORDS);

        let state = apply(&dcb);
        // SH bank (HI written as 0).
        assert_eq!(state.sh_regs.get(sh_reg::SPI_SHADER_PGM_LO_PS), Some(ps[0]));
        assert_eq!(state.sh_regs.get(sh_reg::SPI_SHADER_PGM_HI_PS), Some(0));
        assert_eq!(
            state.sh_regs.get(sh_reg::SPI_SHADER_PGM_RSRC1_PS),
            Some(ps[2])
        );
        assert_eq!(
            state.sh_regs.get(sh_reg::SPI_SHADER_PGM_RSRC2_PS),
            Some(ps[3])
        );
        // CONTEXT bank: every PS export / interpolation register.
        assert_eq!(
            state.ctx_regs.get(context_reg::SPI_SHADER_Z_FORMAT),
            Some(ps[4])
        );
        assert_eq!(
            state.ctx_regs.get(context_reg::SPI_SHADER_COL_FORMAT),
            Some(ps[5])
        );
        assert_eq!(
            state.ctx_regs.get(context_reg::SPI_PS_INPUT_ENA),
            Some(ps[6])
        );
        assert_eq!(
            state.ctx_regs.get(context_reg::SPI_PS_INPUT_ADDR),
            Some(ps[7])
        );
        assert_eq!(
            state.ctx_regs.get(context_reg::SPI_PS_IN_CONTROL),
            Some(ps[8])
        );
        assert_eq!(state.ctx_regs.get(context_reg::SPI_BARYC_CNTL), Some(ps[9]));
        assert_eq!(
            state.ctx_regs.get(context_reg::DB_SHADER_CONTROL),
            Some(ps[10])
        );
        assert_eq!(
            state.ctx_regs.get(context_reg::CB_SHADER_MASK),
            Some(ps[11])
        );
    }

    #[test]
    fn vs_emit_derives_gcn_binary_bind() {
        // The SH round-trip must still produce a GcnBinary bind (state.rs consumers).
        // pgm_addr uses (hi:lo); HI is 0, matching the retail invariant.
        let vs = sample_vs();
        let state = apply(&set_vs_shader(&vs));
        let bound = state.derive_bound_shaders();
        assert!(matches!(
            bound.vs,
            Some(crate::shader::source::ShaderRef::GcnBinary { addr, .. })
                if addr == crate::shader::sb::pgm_addr(vs[0], 0)
        ));
    }

    #[test]
    fn short_reg_block_is_zero_filled_not_oob() {
        // A truncated block reads missing dwords as 0, never panics; PGM_LO still
        // lands and the packet is still the documented length.
        let dcb = set_vs_shader(&[0x1234]);
        assert_eq!(dcb.len(), SET_VS_SHADER_DWORDS);
        let state = apply(&dcb);
        assert_eq!(
            state.sh_regs.get(sh_reg::SPI_SHADER_PGM_LO_VS),
            Some(0x1234)
        );
        assert_eq!(state.sh_regs.get(sh_reg::SPI_SHADER_PGM_HI_VS), Some(0));
        // Zero-filled context field still round-trips as 0 (written, not absent).
        assert_eq!(state.ctx_regs.get(context_reg::SPI_VS_OUT_CONFIG), Some(0));
    }

    #[test]
    fn derived_length_equals_documented_abi_totals() {
        // AC #1: the self-derived stream length (register runs + trailing NOP) must equal
        // the documented doc-3 §2 ABI totals. This is the release-safe check the old
        // debug_assert-only guard lacked: the emitted length is derived from the runs, so
        // a run added/removed without updating the constant surfaces here, not as a silent
        // malformed stream. Assert on the *actual emitted* dcb length (self-derived path).
        assert_eq!(set_vs_shader(&sample_vs()).len(), SET_VS_SHADER_DWORDS);
        assert_eq!(set_ps_shader(&sample_ps()).len(), SET_PS_SHADER_DWORDS);
        assert_eq!(SET_VS_SHADER_DWORDS, 29);
        assert_eq!(SET_PS_SHADER_DWORDS, 40);
    }

    #[test]
    fn non_zero_pgm_hi_is_forced_to_zero_and_length_unchanged() {
        // AC #2: a non-zero incoming PGM_HI is warned (tracing, not asserted here) but the
        // emitted packet still writes HI as 0 — the retail invariant — and the stream stays
        // the documented length. Behavior of the bytes is unchanged vs a zero HI input.
        let mut vs = sample_vs();
        vs[vs_field::PGM_HI] = 0xDEAD_BEEF;
        let dcb = set_vs_shader(&vs);
        assert_eq!(dcb.len(), SET_VS_SHADER_DWORDS);
        let state = apply(&dcb);
        assert_eq!(state.sh_regs.get(sh_reg::SPI_SHADER_PGM_HI_VS), Some(0));
        assert_eq!(state.sh_regs.get(sh_reg::SPI_SHADER_PGM_LO_VS), Some(vs[0]));

        let mut ps = sample_ps();
        ps[ps_field::PGM_HI] = 0x1234_5678;
        let pdcb = set_ps_shader(&ps);
        assert_eq!(pdcb.len(), SET_PS_SHADER_DWORDS);
        let pstate = apply(&pdcb);
        assert_eq!(pstate.sh_regs.get(sh_reg::SPI_SHADER_PGM_HI_PS), Some(0));
        assert_eq!(
            pstate.sh_regs.get(sh_reg::SPI_SHADER_PGM_LO_PS),
            Some(ps[0])
        );
    }

    #[test]
    fn trailing_draw_survives_after_shader_set() {
        // Regression: the shader-set's trailing IT_NOP must claim exactly its own
        // body length so a decoder walking the stream lands on the *next* packet's
        // header. A NOP that over-claims by one dword swallows the header of the draw
        // that follows, silently dropping it. Build [set_vs_shader(regs)...,
        // DRAW_INDEX_AUTO] and assert the decoder yields BOTH the SH regs AND the draw.
        let vs = sample_vs();
        let mut dcb = set_vs_shader(&vs);
        // DRAW_INDEX_AUTO: header + 2 body dwords (index count, draw initiator).
        dcb.push(t3_header(op::IT_DRAW_INDEX_AUTO, 2));
        dcb.extend_from_slice(&[3, 0]);

        let mut bytes = Vec::with_capacity(dcb.len() * 4);
        for w in &dcb {
            bytes.extend_from_slice(&w.to_le_bytes());
        }
        let mut saw_draw = false;
        let state = {
            let mut state = GpuState::default();
            for pkt in decode::decode_bytes(&bytes) {
                if let OwnedPacket::Type3 { opcode, body, .. } = pkt {
                    if opcode == op::IT_DRAW_INDEX_AUTO {
                        saw_draw = true;
                    }
                    if let Some(base) = crate::pm4::opcodes::set_reg_base(opcode) {
                        state.apply_set_reg(base, &body);
                    }
                }
            }
            state
        };

        assert_eq!(state.sh_regs.get(sh_reg::SPI_SHADER_PGM_LO_VS), Some(vs[0]));
        assert_eq!(
            state.ctx_regs.get(context_reg::PA_CL_VS_OUT_CNTL),
            Some(vs[6])
        );
        assert!(
            saw_draw,
            "trailing DRAW_INDEX_AUTO was dropped by the NOP pad"
        );
    }
}
