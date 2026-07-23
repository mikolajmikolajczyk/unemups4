//! PM4 packet *emitters* for the HLE Gnm shader-set builders (doc-1 §2).
//!
//! `sceGnmSetVsShader` / `sceGnmSetPsShader` are, on real hardware, guest-side gnmx
//! builders that write a fixed number of PM4 dwords (29 for VS, 40 for PS — doc-1 §2)
//! into the caller's command buffer from a shader register-setup block. When a game
//! links them from `libSceGnmDriver` (rather than statically), the emulator provides
//! the body; these emitters produce the PM4 so the HLE-linked path and a
//! statically-linked builder both converge on the same shadow register file (doc-2
//! §5): the executor's `derive_bound_shaders` reads `SPI_SHADER_PGM_LO/HI/RSRC1/2`
//! back and produces a `ShaderRef::GcnBinary`.
//!
//! # Register-block layout and packet stream
//!
//! The register block (`vsregs`/`psregs`) is the Sony Gnm `VsStageRegisters` /
//! `PsStageRegisters` struct. The field→register mapping, packet grouping, register
//! offsets, and trailing-NOP size below match the command stream the PS4 console's own
//! gnmx emits, read out of a real Celeste GPU command-buffer capture: `SetVsShader` is
//! two `SET_SH_REG` runs — `{PGM_LO, PGM_HI}` at SH reg `0x48`, `{RSRC1, RSRC2}` at
//! `0x4a` (each a run-of-2, header `0xC0027600`) — then per-register `SET_CONTEXT_REG`
//! runs and an 11-dword trailing `IT_NOP` (header `0xC00A1000`). Each register's absolute
//! index is the standard AMD SI/GFX6 offset from Mesa's `src/amd/registers/gfx6.json`
//! (see [`crate::pm4::opcodes::context_reg`] / [`crate::pm4::opcodes::sh_reg`]); the
//! `emit_matches_console_pm4_capture` test pins these packet headers to the captured
//! console values.
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
//! Matching the console, `PGM_HI` is written as **0** (the captured stream writes the
//! high program-address dword as 0; hardware needs only the low dword), the
//! shader-program registers are two separate `SET_SH_REG` runs (`PGM_LO/HI`, then
//! `RSRC1/RSRC2`), the pipeline state is per-register `SET_CONTEXT_REG` runs, and the
//! stream ends with an 11-dword `IT_NOP` data block — the console emits this same
//! trailing NOP (the capture's NOP run sizes include 11, header `0xC00A1000`), so the
//! caller's `cmd` advances by exactly 29/40 (doc-1 §2). This is not filler standing in
//! for un-emitted state: every meaningful register the struct carries is a real write
//! here. Vulkan-free.

use crate::pm4::opcodes::{context_reg, op, reg_base, sh_reg, t3_header};

/// Documented total PM4 dword count `sceGnmSetVsShader` writes (doc-1 §2).
pub const SET_VS_SHADER_DWORDS: usize = 29;
/// Documented total PM4 dword count `sceGnmSetPsShader` writes (doc-1 §2).
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

/// Size of the trailing `IT_NOP` data block the console emits after a shader set: 11
/// dwords (a real Celeste capture shows an 11-dword NOP, header `0xC00A1000` =
/// `t3_header(IT_NOP, 11)`). The emitted NOP occupies `NOP_DATA_BLOCK + 1` dwords
/// (header + data).
const NOP_DATA_BLOCK: usize = 11;

/// A Type-2 filler-NOP header (type field `0b10` in bits [31:30], no body). Occupies
/// exactly one dword — the decoder skips it header-only (see `pm4::decode`). Used to fill
/// a single-dword reserved-slot gap, which a Type-3 `IT_NOP` can't (its body is ≥ 1 dword).
const TYPE2_NOP: u32 = 0b10 << 30;

/// Widest slot-fill a single trailing `IT_NOP` can express, in dwords. A Type-3 body is
/// bounded by the decoder's 14-bit count field to `0x3FFF + 1 = 16384` dwords (the AMD PM4
/// `count = body_len - 1` layout — see [`t3_header`] / `pm4::decode`), so header + body span
/// at most `1 + 16384` dwords. `reserved` (the guest's `numdwords`) is untrusted and
/// unbounded; a value past `packet.len() + this` can't be represented by one well-formed
/// NOP and would force a multi-gibibyte `Vec::with_capacity` (`handle_alloc_error` → process
/// abort). [`pad_to_reserved`] caps at this and leaves an over-large slot unpadded.
const MAX_NOP_FILL_DWORDS: usize = 1 + (0x3FFF + 1);

/// Pad an emitted PM4 packet to exactly `reserved` dwords with a trailing `IT_NOP`, so a
/// decoder walking a guest command-buffer slot the guest reserved `reserved` dwords for
/// lands cleanly on the next real packet instead of mis-reading whatever stale bytes sit in
/// the unwritten `reserved - packet.len()` tail (task-166).
///
/// Retail gnmx hands the builder the slot size (`numdwords`) and advances the guest cursor by
/// it; we write only `packet.len()` dwords and used to leave the rest untouched. On a *reused*
/// command arena that hole exposes stale prior-frame bytes that our PM4 decode walk mis-reads
/// as real packets (a phantom `SET_SH_REG`, a `Truncated header=0xffffffff` that halts the
/// walk before the frame tail). Filling the slot to exactly `reserved` — `[packet][NOP]` — is
/// the same discipline [`emit_shader_set`] already applies to its own documented length.
///
/// - `reserved <= packet.len()` (includes `reserved == 0`, the "no slot size given" case, and
///   `reserved == len`, no room to pad): return the packet unchanged — the caller writes
///   exactly what it built. A `reserved` *smaller* than the packet is the caller's
///   undersized-reservation concern, gated off before this is reached.
/// - a `gap` (`reserved - packet.len()`) wider than one `IT_NOP` can span
///   ([`MAX_NOP_FILL_DWORDS`]): return the packet unchanged. `reserved` is a guest-controlled
///   `numdwords`; the caller only rejects an *under*-sized reservation, so an absurd value
///   (e.g. `0xFFFFFFFF`) would otherwise drive `Vec::with_capacity(reserved)` to request
///   ~gibibytes and abort the process via `handle_alloc_error`. Such a slot can't be padded
///   with a single well-formed NOP anyway, so it is left as the caller built it.
/// - otherwise: append an `IT_NOP` whose body claims `gap - 1` dwords (`gap = reserved -
///   packet.len()`), zero-filled, so `packet + NOP == reserved` dwords. A one-dword gap uses a
///   header-only Type-2 filler NOP instead (a Type-3 `IT_NOP` body is ≥ 1 dword).
pub fn pad_to_reserved(packet: &[u32], reserved: u32) -> Vec<u32> {
    let reserved = reserved as usize;
    if reserved <= packet.len() {
        return packet.to_vec();
    }
    let gap = reserved - packet.len();
    if gap > MAX_NOP_FILL_DWORDS {
        // Untrusted `reserved` (numdwords): a slot wider than a single `IT_NOP` can span
        // can't be filled by one well-formed NOP, and allocating for a multi-gibibyte
        // `reserved` would abort the process (`handle_alloc_error`). Leave it unpadded — the
        // caller writes exactly what it built, as in the undersized-reservation case.
        return packet.to_vec();
    }
    let mut out = Vec::with_capacity(reserved);
    out.extend_from_slice(packet);
    if gap == 1 {
        // One-dword hole: a Type-3 NOP needs at least a 1-dword body (header + ≥ 1), so it
        // can't fit. A header-only Type-2 NOP fills exactly one dword and the decoder skips it.
        out.push(TYPE2_NOP);
    } else {
        // A single `IT_NOP` claiming `gap - 1` body dwords occupies exactly `gap` dwords; the
        // decoder's 14-bit count field caps the body at 16383 dwords, far past any real draw /
        // shader-set slot, and even an under-claim would only leave benign zero-dword fill
        // (Type-0 runs of 1) that the walk steps through without executing anything.
        out.push(t3_header(op::IT_NOP, gap - 1));
        out.resize(reserved, 0);
    }
    out
}

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
/// Matches the console's `SetVsShader` command stream (real Celeste capture): two SH
/// runs (`PGM_LO/HI` at SH reg `0x48`, `RSRC1/RSRC2` at `0x4a`) then per-register CONTEXT
/// runs, `PGM_HI` written 0. `shader_modifier` handling (the `RSRC1` mix) is left to the
/// caller — the register block is written verbatim.
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
/// Matches the console's `SetPsShader` command stream (real Celeste capture): two SH
/// runs (`PGM_LO/HI` at SH reg `0x08`, `RSRC1/RSRC2` at `0x0a`) then per-/paired CONTEXT
/// runs for the export / interpolation state, `PGM_HI` written 0.
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

/// Emit an `IT_DRAW_INDEX_AUTO` (opcode 0x2D) draw packet: a non-indexed draw whose
/// vertex indices the GPU auto-generates `0..index_count` (doc-2 §5). Body is
/// `[index_count, draw_initiator]` (2 dwords, header `0xC0012D00`) — the layout the
/// console capture emits and the executor's `dispatch_draw_auto` decodes. The console
/// writes `draw_initiator = 2` (VGT source-select = auto-index); we write 0 — our
/// software executor auto-generates the indices and ignores the initiator's source-select,
/// a deliberate simplification (the `ps4-gcn-triangle`/`ps4-pm4-test` corpus hand-emit the
/// same 0 form).
pub fn draw_index_auto(index_count: u32) -> Vec<u32> {
    vec![t3_header(op::IT_DRAW_INDEX_AUTO, 2), index_count, 0]
}

/// Emit an `IT_DRAW_INDEX_OFFSET_2` (opcode 0x35) draw packet: an indexed draw over the
/// currently-bound index buffer starting `index_offset` elements in, for `index_count`
/// indices (doc-2 §5). GFX6 body is `[max_size, index_offset, index_count, draw_initiator]`
/// (4 dwords) — the AMD `DRAW_INDEX_OFFSET_2` layout; the console capture emits this
/// packet as `[max_size, index_offset, index_count, draw_initiator]` (header `0xC0033500`).
/// `max_size` (the VGT index
/// buffer element bound) is written as `index_offset + index_count` — the highest element
/// the draw touches — and `draw_initiator` as 0. The index base/type come from the bound
/// index-buffer state (`IT_INDEX_BASE`/`IT_INDEX_TYPE`); a draw whose base is unset defers
/// cleanly in the executor.
pub fn draw_index_offset(index_offset: u32, index_count: u32) -> Vec<u32> {
    let max_size = index_offset.saturating_add(index_count);
    vec![
        t3_header(op::IT_DRAW_INDEX_OFFSET_2, 4),
        max_size,
        index_offset,
        index_count,
        0,
    ]
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
/// or resync mid-stream). `documented_dwords` is the ABI total from doc-1 §2 carried
/// alongside purely as a cross-check: the derived length must equal it, so the two can't
/// silently diverge, but the derivation — not the constant — drives the actual output.
fn emit_shader_set(runs: &[RegRun], documented_dwords: usize) -> Vec<u32> {
    // Self-derived: the sum of every run plus the trailing NOP (header + data block).
    let total_dwords: usize = runs.iter().map(run_dwords).sum::<usize>() + 1 + NOP_DATA_BLOCK;
    // The documented ABI constant is a cross-check, not the source of truth; if a run is
    // added/removed the derived total moves and this guards the doc-1 constant from drift.
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

    /// Ground-truth witness: these PM4 packet headers and shapes are what the PS4
    /// console's own gnmx emits, read out of a real Celeste GPU command-buffer capture.
    /// Our emitters reproduce the same structure (the literals below are the captured
    /// dwords: SET_SH header `0xC0027600`, draw headers `0xC0012D00` / `0xC0033500`,
    /// trailing NOP `0xC00A1000`).
    #[test]
    fn emit_matches_console_pm4_capture() {
        // SetVsShader — two SET_SH_REG runs (header 0xC0027600, a run-of-2):
        // {PGM_LO, PGM_HI=0} at SH offset 0x48, {RSRC1, RSRC2} at 0x4a.
        let vs = set_vs_shader(&[0x028e_8001, 0xDEAD, 0x002c_0000, 0x0, 0, 0, 0]);
        assert_eq!(vs[0], 0xC002_7600); // t3_header(SET_SH_REG, 3) — console header
        assert_eq!(vs[1], sh_reg::SPI_SHADER_PGM_LO_VS - reg_base::SH); // 0x48
        assert_eq!(vs[2], 0x028e_8001); // PGM_LO
        assert_eq!(vs[3], 0); // PGM_HI written 0 (console writes 0x0); non-zero incoming dropped
        assert_eq!(vs[4], 0xC002_7600);
        assert_eq!(vs[5], sh_reg::SPI_SHADER_PGM_RSRC1_VS - reg_base::SH); // 0x4a
        assert_eq!(vs[6], 0x002c_0000); // RSRC1
        // ...and the stream ends with the 11-dword trailing IT_NOP.
        assert_eq!(vs[vs.len() - 1 - NOP_DATA_BLOCK], 0xC00A_1000);

        // SetPsShader — same grouping at the PS SH offsets 0x08 / 0x0a.
        let ps = set_ps_shader(&sample_ps());
        assert_eq!(ps[0], 0xC002_7600);
        assert_eq!(ps[1], sh_reg::SPI_SHADER_PGM_LO_PS - reg_base::SH); // 0x08
        assert_eq!(ps[5], sh_reg::SPI_SHADER_PGM_RSRC1_PS - reg_base::SH); // 0x0a

        // DRAW_INDEX_AUTO — header 0xC0012D00. The console body is [count, initiator=2];
        // we write initiator 0 (deliberate: the SW executor auto-generates indices and
        // ignores VGT source-select), so the header + count match, the initiator diverges.
        let da = draw_index_auto(3);
        assert_eq!(da[0], 0xC001_2D00);
        assert_eq!(da[1], 3);

        // DRAW_INDEX_OFFSET_2 — the console capture emits [0x12c, 0, 0x12c, 0]
        // (header 0xC0033500); ours for offset=0, count=0x12c is byte-identical.
        assert_eq!(
            draw_index_offset(0, 0x12c),
            vec![0xC003_3500, 0x12c, 0, 0x12c, 0]
        );
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
        // the documented doc-1 §2 ABI totals. This is the release-safe check the old
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
    fn draw_index_auto_emits_the_corpus_packet() {
        // The HLE builder must emit exactly the 3-dword DRAW_INDEX_AUTO the corpus
        // hand-emits (`pm4_type3(IT_DRAW_INDEX_AUTO, 2); count; 0`) so the executor's
        // dispatch_draw_auto arm decodes it identically.
        let pm4 = draw_index_auto(3);
        assert_eq!(pm4, vec![t3_header(op::IT_DRAW_INDEX_AUTO, 2), 3, 0]);
        // Round-trips through the decoder as a single DRAW_INDEX_AUTO whose body carries
        // the index count.
        let mut bytes = Vec::new();
        for w in &pm4 {
            bytes.extend_from_slice(&w.to_le_bytes());
        }
        let pkts = decode::decode_bytes(&bytes);
        assert_eq!(pkts.len(), 1);
        match &pkts[0] {
            OwnedPacket::Type3 { opcode, body, .. } => {
                assert_eq!(*opcode, op::IT_DRAW_INDEX_AUTO);
                assert_eq!(body[0], 3, "index count");
            }
            other => panic!("expected DRAW_INDEX_AUTO, got {other:?}"),
        }
    }

    #[test]
    fn draw_index_offset_emits_offset2_packet() {
        // DRAW_INDEX_OFFSET_2 (0x35) body is [max_size, index_offset, index_count,
        // draw_initiator]; max_size is the highest element the draw touches.
        let pm4 = draw_index_offset(6, 12);
        assert_eq!(
            pm4,
            vec![t3_header(op::IT_DRAW_INDEX_OFFSET_2, 4), 18, 6, 12, 0]
        );
        let mut bytes = Vec::new();
        for w in &pm4 {
            bytes.extend_from_slice(&w.to_le_bytes());
        }
        let pkts = decode::decode_bytes(&bytes);
        assert_eq!(pkts.len(), 1);
        match &pkts[0] {
            OwnedPacket::Type3 { opcode, body, .. } => {
                assert_eq!(*opcode, op::IT_DRAW_INDEX_OFFSET_2);
                assert_eq!(body[1], 6, "index offset");
                assert_eq!(body[2], 12, "index count");
            }
            other => panic!("expected DRAW_INDEX_OFFSET_2, got {other:?}"),
        }
    }

    #[test]
    fn pad_to_reserved_fills_oversized_slot_over_stale_bytes() {
        // task-166: a draw builder written into an over-sized reserved slot pre-filled with
        // stale prior-frame bytes must decode as exactly [draw][NOP] — no stale packet
        // survives, and the walk lands cleanly on the next real packet after the slot.
        let draw = draw_index_auto(3); // 3 dwords
        let reserved = 10u32;
        let padded = pad_to_reserved(&draw, reserved);
        assert_eq!(padded.len(), reserved as usize, "slot filled to reserved");

        // Simulate a reused arena: lay the padded slot over 0xdeadbeef stale bytes, then a
        // real DRAW_INDEX_AUTO that follows the slot. Without the pad the stale tail bytes
        // would be mis-decoded before the walk reached this trailing draw.
        let mut arena = vec![0xDEAD_BEEFu32; reserved as usize];
        arena[..padded.len()].copy_from_slice(&padded);
        arena.push(t3_header(op::IT_DRAW_INDEX_AUTO, 2));
        arena.extend_from_slice(&[7, 0]);

        let mut bytes = Vec::with_capacity(arena.len() * 4);
        for w in &arena {
            bytes.extend_from_slice(&w.to_le_bytes());
        }
        let pkts = decode::decode_bytes(&bytes);
        // Exactly: the slot's DRAW_INDEX_AUTO(3), the slot's filler NOP, then the trailing
        // DRAW_INDEX_AUTO(7). No Truncated, no stale SET_*_REG conjured from 0xdeadbeef.
        assert_eq!(pkts.len(), 3, "expected [draw][nop][draw], got {pkts:?}");
        match &pkts[0] {
            OwnedPacket::Type3 { opcode, body, .. } => {
                assert_eq!(*opcode, op::IT_DRAW_INDEX_AUTO);
                assert_eq!(body[0], 3, "slot draw index count");
            }
            other => panic!("expected slot DRAW_INDEX_AUTO, got {other:?}"),
        }
        assert!(
            matches!(&pkts[1], OwnedPacket::Type3 { opcode, .. } if *opcode == op::IT_NOP),
            "expected filler NOP, got {:?}",
            pkts[1]
        );
        match &pkts[2] {
            OwnedPacket::Type3 { opcode, body, .. } => {
                assert_eq!(*opcode, op::IT_DRAW_INDEX_AUTO);
                assert_eq!(body[0], 7, "trailing draw index count");
            }
            other => panic!("expected trailing DRAW_INDEX_AUTO, got {other:?}"),
        }
        assert!(
            !pkts
                .iter()
                .any(|p| matches!(p, OwnedPacket::Truncated { .. })),
            "no Truncated packet may appear inside the reserved slot: {pkts:?}"
        );
    }

    #[test]
    fn pad_to_reserved_one_dword_gap_uses_type2_filler() {
        // A single-dword gap can't hold a Type-3 NOP (body ≥ 1); a header-only Type-2 filler
        // fills it and the decoder skips it, still landing on the next real packet.
        let draw = draw_index_auto(3); // 3 dwords
        let padded = pad_to_reserved(&draw, 4); // gap == 1
        assert_eq!(padded.len(), 4);
        assert_eq!(
            padded[3], TYPE2_NOP,
            "one-dword gap must be a Type-2 filler NOP"
        );

        let mut buf = padded.clone();
        buf.push(t3_header(op::IT_DRAW_INDEX_AUTO, 2));
        buf.extend_from_slice(&[9, 0]);
        let mut bytes = Vec::with_capacity(buf.len() * 4);
        for w in &buf {
            bytes.extend_from_slice(&w.to_le_bytes());
        }
        let pkts = decode::decode_bytes(&bytes);
        // [draw][Type2][draw] — the Type-2 decodes as OwnedPacket::Type2.
        assert_eq!(pkts.len(), 3, "expected [draw][type2][draw], got {pkts:?}");
        assert!(matches!(pkts[1], OwnedPacket::Type2));
        assert!(
            matches!(&pkts[2], OwnedPacket::Type3 { opcode, body, .. }
                if *opcode == op::IT_DRAW_INDEX_AUTO && body[0] == 9),
            "trailing draw dropped: {:?}",
            pkts[2]
        );
    }

    #[test]
    fn pad_to_reserved_caps_absurd_reserved_without_huge_alloc() {
        // A guest-controlled `reserved` (numdwords) beyond what a single trailing IT_NOP can
        // span must NOT drive `Vec::with_capacity(reserved)` — a value like 0xFFFFFFFF would
        // request ~17 GiB and abort the whole process via handle_alloc_error. The slot is left
        // unpadded (the caller writes exactly what it built), never allocated for.
        let draw = draw_index_auto(3); // 3 dwords
        assert_eq!(
            pad_to_reserved(&draw, u32::MAX),
            draw,
            "absurd reserved must return the packet unchanged, not allocate for it"
        );

        // Boundary: the widest gap one NOP can span (packet.len() + MAX_NOP_FILL_DWORDS) still
        // pads to exactly `reserved` with a single well-formed NOP whose claimed body matches
        // the fill; one dword past it is rejected (unpadded) rather than emitting a truncated
        // header / over-allocating.
        let widest = (draw.len() + MAX_NOP_FILL_DWORDS) as u32;
        let padded = pad_to_reserved(&draw, widest);
        assert_eq!(
            padded.len(),
            widest as usize,
            "widest fillable slot is padded"
        );
        // The trailing NOP's claimed body (count + 1) must equal the actual fill, so a decoder
        // lands on the next packet — i.e. no 14-bit count truncation at the boundary.
        let nop_header = padded[draw.len()];
        let claimed_body = ((nop_header >> 16) & 0x3FFF) as usize + 1;
        assert_eq!(
            claimed_body,
            padded.len() - draw.len() - 1,
            "boundary NOP body must match the fill (no count truncation)"
        );
        assert_eq!(
            pad_to_reserved(&draw, widest + 1),
            draw,
            "one dword past the widest fillable slot is left unpadded"
        );
    }

    #[test]
    fn pad_to_reserved_noop_when_no_room() {
        // reserved == 0 (no slot size given) and reserved == len (exact fit) both return the
        // packet unchanged — the caller writes exactly what it built.
        let draw = draw_index_offset(6, 12); // 5 dwords
        assert_eq!(pad_to_reserved(&draw, 0), draw, "reserved==0 → unchanged");
        assert_eq!(
            pad_to_reserved(&draw, draw.len() as u32),
            draw,
            "reserved==len → unchanged"
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
