//! `GpuState` (doc-2 §5, §C7): a sparse shadow register file (CONTEXT/SH/UCONFIG
//! banks, plus CONFIG) plus derived, typed views that grow per phase. New state =
//! decoding more register indices, never restructuring.
//!
//! The state lives in the [`GnmDriver`](crate::driver::GnmDriver) singleton, not
//! in the per-submit [`Executor`](crate::exec::Executor): PS4 context/SH registers
//! persist across submits (set in submit N, drawn in submit N+1), so the state must
//! outlive a single executor. The executor borrows `&mut GpuState` for the duration
//! of one submission (doc-2 §5 / §C7).
//!
//! ## Lock invariant (load-bearing — do not break)
//!
//! `GpuState` reaches the guest through [`driver()`](crate::driver::driver), whose
//! lock `record_submit` holds across the whole `exec.run(...)`, and `exec.run`
//! blocks on the display channel. Therefore **the display thread must NEVER acquire
//! `driver()`** — doing so deadlocks instantly. See the doc comment on
//! [`driver()`](crate::driver::driver) for the full statement.
//!
//! Phase 3.5 keeps the first derived view the draw path needs:
//! [`BoundShaders`] — the currently-bound VS/PS as [`ShaderRef`]s. The bind flows
//! through the `libSceGnmDriver` HLE (`sceGnmSetEmbeddedVs/PsShader`) into this
//! shadow state (via the driver), and the `IT_DRAW_INDEX_AUTO` executor arm reads it
//! back and resolves the pair through the `ShaderProvider` (doc-2 §4). Modeling the
//! bind as a `ShaderRef` (not a hardcoded "two embedded shaders") keeps phase 4's
//! real `.sb` binds a matter of storing a different `ShaderRef`, not restructuring.
//!
//! TODO phase-4: the derived textures/samplers/RT views are added the same additive
//! way — by interpreting more register indices at draw time.

use std::collections::HashMap;

use ps4_core::gpu::TargetDesc;

use crate::shader::source::{GcnResources, ShaderRef, Stage};

/// One offscreen render target a draw has rendered into (doc-2 §8.5, task-56). The
/// registry records its guest `[base, base+size)` range and the [`TargetDesc`] the draw
/// derived, so a later draw whose sampled T# base matches this range is recognized as
/// RT-as-texture: the sampled bind resolves to the RT's cache entry (host-side) instead of
/// detiling guest bytes the GPU wrote. Plain data — the ResourceId is not stored here (the
/// [`ResourceCache`](crate::cache::ResourceCache) mints and owns it, keyed on the
/// `RenderTarget` layout over this same range).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RegisteredRt {
    /// Guest base address of the render target (`CB_COLOR0_BASE << 8`).
    pub base: u64,
    /// Byte size of the render target's guest range.
    pub size: u64,
    /// The target description the draw-into-RT derived (extent + format).
    pub desc: TargetDesc,
}

/// The offscreen render targets drawn into this or a prior submit (doc-2 §8.5, task-56).
/// Lives on [`GpuState`] so it persists across submits (a producer draw in submit N, a
/// consumer sample in submit N+1). A draw-into-RT records its range here; a sampling draw
/// consults it to recognize RT-as-texture. Only exact-base / full-containment counts —
/// partial/sub-rect overlap is out of scope (the consumer defers).
#[derive(Debug, Clone, Default)]
pub struct RenderTargetRegistry {
    rts: Vec<RegisteredRt>,
}

impl RenderTargetRegistry {
    /// Record (or refresh) a render target over `[base, base+size)`. Idempotent per base: a
    /// re-render into the same base updates its descriptor rather than adding a duplicate,
    /// so the registry holds one entry per distinct RT base.
    pub fn register(&mut self, rt: RegisteredRt) {
        if let Some(existing) = self.rts.iter_mut().find(|r| r.base == rt.base) {
            *existing = rt;
        } else {
            self.rts.push(rt);
        }
    }

    /// The registered RT a sampled texture at `[tex_base, tex_base+tex_size)` names, if any
    /// (doc-2 §8.5). A match is **exact base + full containment**: the sampled range must
    /// start at the RT base and fit entirely within the RT's range. A partial/sub-rect
    /// overlap returns `None` (out of scope — the consumer defers rather than half-binding).
    pub fn lookup(&self, tex_base: u64, tex_size: u64) -> Option<RegisteredRt> {
        self.rts
            .iter()
            .copied()
            .find(|rt| rt.base == tex_base && tex_size <= rt.size)
    }
}

/// A sparse register bank: absolute register index → last-written u32 (doc-2 §C7).
/// One bank per GFX6 register window (CONTEXT / SH / UCONFIG / CONFIG). Sparse
/// because the guest touches a small fraction of the window; the derived pipeline
/// views (phase 4) read specific indices back at draw time.
#[derive(Debug, Clone, Default)]
pub struct RegFile {
    regs: HashMap<u32, u32>,
}

impl RegFile {
    /// Write `value` at absolute register `index`, overwriting any prior write.
    pub fn set(&mut self, index: u32, value: u32) {
        self.regs.insert(index, value);
    }

    /// The last value written at absolute register `index`, or `None` if never set.
    pub fn get(&self, index: u32) -> Option<u32> {
        self.regs.get(&index).copied()
    }

    /// Every `(absolute index, value)` this bank holds, in unspecified order.
    ///
    /// The whole population, not a curated subset: the snapshot dumper (task-185) emits
    /// every register the guest has written, including the ones no derivation reads. That
    /// is deliberate — the registers that cost task-179 hours were ones the guest wrote and
    /// nothing on our side looked at, so a dump limited to "registers we consume" would
    /// reproduce the same blind spot.
    pub fn iter(&self) -> impl Iterator<Item = (u32, u32)> + '_ {
        self.regs.iter().map(|(&i, &v)| (i, v))
    }

    /// Number of distinct registers written (sparse population). Test/introspection.
    pub fn len(&self) -> usize {
        self.regs.len()
    }

    /// Whether no register has been written yet.
    pub fn is_empty(&self) -> bool {
        self.regs.is_empty()
    }

    /// Clear every write in this bank (part of `IT_CLEAR_STATE`).
    pub fn clear(&mut self) {
        self.regs.clear();
    }
}

/// The GPU shadow state (doc-2 §5, §C7): the sparse register banks plus the derived,
/// typed views the draw path reads. Lives in the [`GnmDriver`](crate::driver::GnmDriver)
/// singleton so context/SH registers persist across submits; the per-submit
/// [`Executor`](crate::exec::Executor) borrows it `&mut`. This phase stores register
/// writes verbatim and does not yet interpret any specific index — that is
/// the draw path derives pipeline state from these banks at draw time.
///
/// See the module docs and [`driver()`](crate::driver::driver) for the lock
/// invariant that guards this state.
#[derive(Debug, Clone, Default)]
pub struct GpuState {
    /// CONTEXT_REG writes (render-target / rasterizer / blend state, phase 4).
    pub ctx_regs: RegFile,
    /// SH_REG writes (shader addresses, user data).
    pub sh_regs: RegFile,
    /// UCONFIG_REG writes (index type, primitive topology, …).
    pub uconfig_regs: RegFile,
    /// CONFIG_REG writes (global config).
    pub config_regs: RegFile,
    /// Derived view: the currently-bound VS/PS pair (doc-2 §5). Read by the
    /// `IT_DRAW_INDEX_AUTO` arm; written by the embedded-shader HLE binds. Kept a
    /// field (not a global) so multi-ring/multi-context submits don't stomp one
    /// shared store.
    pub shaders: BoundShaders,
    /// Offscreen render targets drawn into this or a prior submit (doc-2 §8.5, task-56).
    /// A draw-into-RT records its range here; a later sampling draw consults it to
    /// recognize RT-as-texture. Lives on the submit-spanning state so a producer/consumer
    /// pair can straddle submits.
    pub render_targets: RenderTargetRegistry,
    /// The on-demand GPU state snapshot recorder (task-185). Lives here for the same reason
    /// the register banks do: a frame spans several submits, so the per-draw records must
    /// outlive the per-submit [`Executor`](crate::exec::Executor) that appends them. Idle by
    /// default — see [`Recorder::armed`](crate::snapshot::Recorder::armed) for the
    /// zero-cost-when-not-capturing contract the draw path relies on.
    pub snapshot: crate::snapshot::Recorder,
}

impl GpuState {
    /// Apply one `SET_*_REG` packet body into the bank named by `base` (a
    /// `reg_base::*` window). Body layout (doc-2 §C7): the first dword is the
    /// register offset relative to `base`; each following dword is written at
    /// consecutive absolute indices `base + offset + i`. An empty body (no offset
    /// dword) is a no-op.
    pub fn apply_set_reg(&mut self, base: u32, body: &[u32]) {
        let Some((&offset, values)) = body.split_first() else {
            return;
        };
        let bank = self.bank_mut(base);
        let start = base.wrapping_add(offset);
        for (i, &value) in values.iter().enumerate() {
            bank.set(start.wrapping_add(i as u32), value);
        }
    }

    /// Reset all register banks (`IT_CLEAR_STATE`). The derived shader view is left
    /// intact: `IT_CLEAR_STATE` clears the register state, not the guest's separate
    /// embedded-shader bind (which is re-emitted per draw setup on real HW).
    pub fn clear_regs(&mut self) {
        self.ctx_regs.clear();
        self.sh_regs.clear();
        self.uconfig_regs.clear();
        self.config_regs.clear();
    }

    /// The bank matching a `reg_base::*` window. Falls back to CONFIG for an
    /// unrecognized base (only the four SET_*_REG windows reach here).
    fn bank_mut(&mut self, base: u32) -> &mut RegFile {
        use crate::pm4::opcodes::reg_base;
        match base {
            reg_base::CONTEXT => &mut self.ctx_regs,
            reg_base::SH => &mut self.sh_regs,
            reg_base::UCONFIG => &mut self.uconfig_regs,
            _ => &mut self.config_regs,
        }
    }

    /// Record an embedded-shader bind (`sceGnmSetEmbeddedVs/PsShader`): store
    /// `Embedded{stage,id}` as the bound `ShaderRef` for `stage`. Called from the HLE
    /// handler under the driver lock; the executor reads it back at draw time.
    pub fn bind_embedded_shader(&mut self, stage: Stage, id: u32) {
        self.shaders.set(stage, ShaderRef::Embedded { stage, id });
    }

    /// Clear the embedded-shader shadow for `stage` so the register route
    /// ([`derive_bound_shaders`]) takes over again. Called by the HLE register-bind
    /// handlers (`sceGnmSetVsShader`/`sceGnmSetPsShader`): once a game programs a real
    /// shader through the register route, the earlier embedded bind must not keep
    /// shadowing it (task-73). This is deliberately NOT triggered by a raw PM4
    /// `SET_SH_REG` PGM write — the phase-3.5 embedded corpus stamps PGM markers that
    /// way and must keep its embedded pipeline (Tier B). It also does not fight the
    /// task-43 `clear_regs` decision: `clear_regs` keeps binds; only an explicit
    /// register-route bind unbinds the embedded shadow.
    ///
    /// [`derive_bound_shaders`]: Self::derive_bound_shaders
    pub fn unbind_embedded_shader(&mut self, stage: Stage) {
        self.shaders.clear(stage);
    }

    /// The VS/PS pair effective for the next draw (doc-2 §5 derived view).
    ///
    /// Registers are the truth: for each stage this reads the SH-bank
    /// `SPI_SHADER_PGM_LO/HI` back and, when programmed, derives a
    /// [`ShaderRef::GcnBinary`] whose `addr` is `(hi:lo) << 8` (the `.sb` code start,
    /// [`crate::shader::sb::pgm_addr`]) and whose resource footprint comes from
    /// `PGM_RSRC1/2`. This is the route freegnm / retail games (Bloodborne) use —
    /// they program the shader address into registers, not via an embedded id.
    ///
    /// The embedded-shader global route ([`bind_embedded_shader`], stored in
    /// [`Self::shaders`]) **takes precedence** per stage: an embedded bind wins even
    /// when the guest also stamped a `PGM_LO/HI` marker (the phase-3.5 embedded corpus
    /// does both — the embedded pipeline stays selected, so Tier B is unchanged). A
    /// stage with neither an embedded bind nor programmed `PGM_LO/HI` stays `None`.
    ///
    /// [`bind_embedded_shader`]: Self::bind_embedded_shader
    pub fn derive_bound_shaders(&self) -> BoundShaders {
        use crate::pm4::opcodes::sh_reg;
        let mut out = self.shaders;
        for (stage, lo, hi, rsrc1, rsrc2) in [
            (
                Stage::Vertex,
                sh_reg::SPI_SHADER_PGM_LO_VS,
                sh_reg::SPI_SHADER_PGM_HI_VS,
                sh_reg::SPI_SHADER_PGM_RSRC1_VS,
                sh_reg::SPI_SHADER_PGM_RSRC2_VS,
            ),
            (
                Stage::Pixel,
                sh_reg::SPI_SHADER_PGM_LO_PS,
                sh_reg::SPI_SHADER_PGM_HI_PS,
                sh_reg::SPI_SHADER_PGM_RSRC1_PS,
                sh_reg::SPI_SHADER_PGM_RSRC2_PS,
            ),
        ] {
            // Embedded global route wins for this stage (see method docs).
            if matches!(out.get(stage), Some(ShaderRef::Embedded { .. })) {
                continue;
            }
            if let Some(r) = self.gcn_ref_from_regs(stage, lo, hi, rsrc1, rsrc2) {
                out.set(stage, r);
            }
        }
        out
    }

    /// Build a [`ShaderRef::GcnBinary`] from the SH-bank shader-program registers, or
    /// `None` if `PGM_LO`/`PGM_HI` were never written (that stage isn't
    /// register-bound). `PGM_RSRC1/2` default to 0 (an empty footprint) if absent.
    ///
    /// For the vertex stage this also snapshots the **fetch-shader pointer** the driver
    /// preloads into VS user-SGPR pair `s[0:1]` (doc-6 Entry 9) onto
    /// [`GcnResources::fetch_addr`], so the provider — which cannot read registers — can
    /// inline the fetch body before recompiling. The pixel stage takes no fetch call, so
    /// its `fetch_addr` stays `None`.
    ///
    /// For the pixel stage it snapshots the **PS input routing** from the CONTEXT-bank
    /// `SPI_PS_INPUT_CNTL_0..31` registers: slot `n`'s `OFFSET` field names the VS export
    /// parameter that PS attribute `n` interpolates. A slot the guest never programmed
    /// falls back to identity for that slot, preserving the old behaviour where nothing
    /// is set. The vertex stage carries the identity map — the register does not apply.
    ///
    /// This method is glue: every register index it reads
    /// (`SPI_SHADER_PGM_LO/HI/RSRC1/2_{VS,PS}`, `SPI_SHADER_USER_DATA_VS_0`,
    /// `SPI_PS_INPUT_CNTL_0`) and every bitfield decode (`pgm_rsrc::num_{v,s,user}gprs`)
    /// is a constant/helper from [`crate::pm4::opcodes`], where each is pinned to its AMD
    /// hardware value (Mesa `src/amd/registers/gfx6.json`, AMD CI-ISA). It composes those
    /// already-cited facts; it asserts none of its own beyond the s[0:1] pointer assembly.
    fn gcn_ref_from_regs(
        &self,
        stage: Stage,
        lo: u32,
        hi: u32,
        rsrc1: u32,
        rsrc2: u32,
    ) -> Option<ShaderRef> {
        use crate::pm4::opcodes::{context_reg, pgm_rsrc, sh_reg};
        // Require both address halves so a partial write isn't read as a garbage addr.
        let (pgm_lo, pgm_hi) = (self.sh_regs.get(lo)?, self.sh_regs.get(hi)?);
        let addr = crate::shader::sb::pgm_addr(pgm_lo, pgm_hi);
        let r1 = self.sh_regs.get(rsrc1).unwrap_or(0);
        let r2 = self.sh_regs.get(rsrc2).unwrap_or(0);
        // The fetch-shader pointer lives in VS user-SGPR pair s[0:1] (which SGPRs the
        // driver preloads it into: doc-6 Entry 9). A 64-bit value in an even-aligned GCN
        // SGPR pair keeps its low-order word in the first (lesser) SGPR — AMD CI-ISA
        // (Sea Islands ISA, ci-isa.pdf) §5.2 "If an instruction uses 64-bit data in SGPRs,
        // the SGPR pair must be aligned to an even boundary", and the descriptor-load note
        // "The low-order bits are in the first SGPR" — so s0 is the low dword, s1 the high.
        // Only meaningful for the VS; a zero / unprogrammed pair yields None so a
        // self-fetching VS (no s_swappc) is unaffected.
        let fetch_addr = if matches!(stage, Stage::Vertex) {
            let f_lo = self.sh_regs.get(sh_reg::SPI_SHADER_USER_DATA_VS_0);
            let f_hi = self.sh_regs.get(sh_reg::SPI_SHADER_USER_DATA_VS_0 + 1);
            match (f_lo, f_hi) {
                (Some(l), Some(h)) => {
                    let ptr = u64::from(l) | (u64::from(h) << 32);
                    (ptr != 0).then_some(ptr)
                }
                _ => None,
            }
        } else {
            None
        };
        let ps_input_map = if matches!(stage, Stage::Pixel) {
            let mut offsets = [0u8; ps4_gcn::PS_INPUT_SLOTS];
            for (n, o) in offsets.iter_mut().enumerate() {
                *o = match self
                    .ctx_regs
                    .get(context_reg::SPI_PS_INPUT_CNTL_0 + n as u32)
                {
                    Some(v) => v as u8,
                    None => n as u8,
                };
            }
            ps4_gcn::PsInputMap::from_offsets(offsets)
        } else {
            ps4_gcn::PsInputMap::default()
        };
        Some(ShaderRef::GcnBinary {
            addr,
            ps_input_map,
            res: GcnResources {
                num_vgprs: pgm_rsrc::num_vgprs(r1),
                num_sgprs: pgm_rsrc::num_sgprs(r1),
                num_user_sgprs: pgm_rsrc::num_user_sgprs(r2),
                fetch_addr,
            },
        })
    }
}

/// The shaders currently bound for the next draw (doc-2 §5 derived view). Each is a
/// backend-agnostic [`ShaderRef`] the executor resolves through a `ShaderProvider`
/// at draw time — `None` until the guest binds one. Only VS/PS are tracked this
/// phase (the embedded pair); other HW stages (LS/HS/ES/GS) are added when a corpus
/// needs them (doc-2 §C8).
#[derive(Debug, Clone, Copy, Default)]
pub struct BoundShaders {
    /// The bound vertex shader, or `None` if unbound.
    pub vs: Option<ShaderRef>,
    /// The bound pixel shader, or `None` if unbound.
    pub ps: Option<ShaderRef>,
}

impl BoundShaders {
    /// Record a bind for `stage` (`Vertex`/`Pixel`), overwriting any prior bind.
    pub fn set(&mut self, stage: Stage, r: ShaderRef) {
        match stage {
            Stage::Vertex => self.vs = Some(r),
            Stage::Pixel => self.ps = Some(r),
        }
    }

    /// Drop the bind for `stage`, so `derive_bound_shaders` re-derives it from registers.
    pub fn clear(&mut self, stage: Stage) {
        match stage {
            Stage::Vertex => self.vs = None,
            Stage::Pixel => self.ps = None,
        }
    }

    /// The bound `ShaderRef` for `stage`, or `None` if unbound.
    pub fn get(&self, stage: Stage) -> Option<ShaderRef> {
        match stage {
            Stage::Vertex => self.vs,
            Stage::Pixel => self.ps,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pm4::opcodes::reg_base;

    #[test]
    fn set_records_per_stage() {
        let mut b = BoundShaders::default();
        assert!(b.vs.is_none() && b.ps.is_none());
        b.set(
            Stage::Vertex,
            ShaderRef::Embedded {
                stage: Stage::Vertex,
                id: 0,
            },
        );
        b.set(
            Stage::Pixel,
            ShaderRef::Embedded {
                stage: Stage::Pixel,
                id: 1,
            },
        );
        assert!(matches!(
            b.vs,
            Some(ShaderRef::Embedded {
                stage: Stage::Vertex,
                id: 0
            })
        ));
        assert!(matches!(
            b.ps,
            Some(ShaderRef::Embedded {
                stage: Stage::Pixel,
                id: 1
            })
        ));
    }

    #[test]
    fn set_overwrites_prior_bind() {
        let mut b = BoundShaders::default();
        b.set(
            Stage::Vertex,
            ShaderRef::Embedded {
                stage: Stage::Vertex,
                id: 0,
            },
        );
        b.set(
            Stage::Vertex,
            ShaderRef::GcnBinary {
                addr: 0x1000,
                res: crate::shader::source::GcnResources::default(),
                ps_input_map: ps4_gcn::PsInputMap::default(),
            },
        );
        assert!(matches!(
            b.vs,
            Some(ShaderRef::GcnBinary { addr: 0x1000, .. })
        ));
    }

    #[test]
    fn apply_set_reg_lands_values_at_absolute_indices() {
        // A SET_CONTEXT_REG body [offset, v0, v1, v2] writes v_i at
        // CONTEXT + offset + i (doc-2 §C7).
        let mut s = GpuState::default();
        s.apply_set_reg(reg_base::CONTEXT, &[0x10, 0xAA, 0xBB, 0xCC]);
        assert_eq!(s.ctx_regs.get(reg_base::CONTEXT + 0x10), Some(0xAA));
        assert_eq!(s.ctx_regs.get(reg_base::CONTEXT + 0x11), Some(0xBB));
        assert_eq!(s.ctx_regs.get(reg_base::CONTEXT + 0x12), Some(0xCC));
        assert_eq!(s.ctx_regs.len(), 3);
    }

    #[test]
    fn apply_set_reg_routes_to_the_named_bank() {
        let mut s = GpuState::default();
        s.apply_set_reg(reg_base::CONTEXT, &[0, 1]);
        s.apply_set_reg(reg_base::SH, &[0, 2]);
        s.apply_set_reg(reg_base::UCONFIG, &[0, 3]);
        s.apply_set_reg(reg_base::CONFIG, &[0, 4]);
        assert_eq!(s.ctx_regs.get(reg_base::CONTEXT), Some(1));
        assert_eq!(s.sh_regs.get(reg_base::SH), Some(2));
        assert_eq!(s.uconfig_regs.get(reg_base::UCONFIG), Some(3));
        assert_eq!(s.config_regs.get(reg_base::CONFIG), Some(4));
    }

    #[test]
    fn apply_set_reg_empty_body_is_noop() {
        let mut s = GpuState::default();
        s.apply_set_reg(reg_base::CONTEXT, &[]);
        assert!(s.ctx_regs.is_empty());
    }

    #[test]
    fn apply_set_reg_offset_only_writes_nothing() {
        // Just the offset dword, no values: nothing to write.
        let mut s = GpuState::default();
        s.apply_set_reg(reg_base::SH, &[0x40]);
        assert!(s.sh_regs.is_empty());
    }

    #[test]
    fn clear_regs_resets_all_banks() {
        let mut s = GpuState::default();
        s.apply_set_reg(reg_base::CONTEXT, &[0, 1]);
        s.apply_set_reg(reg_base::SH, &[0, 2]);
        s.clear_regs();
        assert!(s.ctx_regs.is_empty());
        assert!(s.sh_regs.is_empty());
    }

    #[test]
    fn later_write_overwrites_same_index() {
        let mut s = GpuState::default();
        s.apply_set_reg(reg_base::CONTEXT, &[0x5, 0x1111]);
        s.apply_set_reg(reg_base::CONTEXT, &[0x5, 0x2222]);
        assert_eq!(s.ctx_regs.get(reg_base::CONTEXT + 0x5), Some(0x2222));
        assert_eq!(s.ctx_regs.len(), 1);
    }

    #[test]
    fn derive_bound_shaders_from_pgm_regs() {
        // PGM_LO/HI + RSRC1/2 in the SH bank → a GcnBinary ref with the
        // derived .sb addr ((hi:lo)<<8) and RSRC-decoded GPR/user-SGPR counts.
        use crate::pm4::opcodes::sh_reg;
        let mut s = GpuState::default();
        s.sh_regs.set(sh_reg::SPI_SHADER_PGM_LO_VS, 0x0000_2000);
        s.sh_regs.set(sh_reg::SPI_SHADER_PGM_HI_VS, 0x0000_0000);
        // RSRC1: VGPRS field 3 → (3+1)*4 = 16; SGPRS field 1 (bits[9:6]) → (1+1)*8=16.
        s.sh_regs
            .set(sh_reg::SPI_SHADER_PGM_RSRC1_VS, 0b0000_0000_0100_0011);
        // RSRC2: USER_SGPR (bits[5:1]) = 5 → 0b0000_1010.
        s.sh_regs.set(sh_reg::SPI_SHADER_PGM_RSRC2_VS, 0b0000_1010);

        let bound = s.derive_bound_shaders();
        match bound.vs {
            Some(ShaderRef::GcnBinary { addr, res, .. }) => {
                assert_eq!(addr, 0x0020_0000);
                assert_eq!(res.num_vgprs, 16);
                assert_eq!(res.num_sgprs, 16);
                assert_eq!(res.num_user_sgprs, 5);
            }
            other => panic!("expected GcnBinary VS, got {other:?}"),
        }
        // PS was never programmed → still unbound.
        assert!(bound.ps.is_none());
    }

    /// The PS ref carries the `SPI_PS_INPUT_CNTL_n.OFFSET` routing: slot `n` reads the VS
    /// export parameter named there, not `n`. Unwritten slots stay identity, and the
    /// neighbouring `DEFAULT_VAL`/`FLAT_SHADE` bits above the 5-bit `OFFSET` field must not
    /// leak into a location.
    #[test]
    fn derive_ps_carries_spi_ps_input_cntl_routing() {
        use crate::pm4::opcodes::{context_reg, sh_reg};
        let mut s = GpuState::default();
        s.sh_regs.set(sh_reg::SPI_SHADER_PGM_LO_PS, 0x0000_3000);
        s.sh_regs.set(sh_reg::SPI_SHADER_PGM_HI_PS, 0x0000_0000);
        // Slot 0 → parameter 1, with FLAT_SHADE-and-above bits set to prove the mask.
        s.ctx_regs
            .set(context_reg::SPI_PS_INPUT_CNTL_0, 0xFFFF_FFE1);
        // Slot 1 → parameter 0 (the shape an "unused" slot takes).
        s.ctx_regs.set(context_reg::SPI_PS_INPUT_CNTL_0 + 1, 0);

        let bound = s.derive_bound_shaders();
        match bound.ps {
            Some(ShaderRef::GcnBinary { ps_input_map, .. }) => {
                assert_eq!(ps_input_map.location_for(0), 1);
                assert_eq!(ps_input_map.location_for(1), 0);
                // Slot 2 was never written → identity.
                assert_eq!(ps_input_map.location_for(2), 2);
            }
            other => panic!("expected GcnBinary PS, got {other:?}"),
        }
        // The VS side is never routed by this register.
        s.sh_regs.set(sh_reg::SPI_SHADER_PGM_LO_VS, 0x0000_2000);
        s.sh_regs.set(sh_reg::SPI_SHADER_PGM_HI_VS, 0x0000_0000);
        match s.derive_bound_shaders().vs {
            Some(ShaderRef::GcnBinary { ps_input_map, .. }) => {
                assert_eq!(ps_input_map, ps4_gcn::PsInputMap::default());
            }
            other => panic!("expected GcnBinary VS, got {other:?}"),
        }
    }

    /// Witness: the fetch-shader pointer is assembled from VS user-SGPR pair s[0:1] with
    /// the low-order dword in the first (lesser) SGPR. That word order is the AMD CI-ISA
    /// (Sea Islands ISA) SGPR-pair convention — a 64-bit value occupies an even-aligned
    /// pair and "The low-order bits are in the first SGPR" (ci-isa.pdf). s0 therefore holds
    /// the low 32 bits, s1 the high, so a pointer p reads back as
    /// `(s1 << 32) | s0`. Pin that against the oracle literals below.
    #[test]
    fn fetch_addr_assembles_sgpr_pair_low_word_first() {
        use crate::pm4::opcodes::sh_reg;
        // s0 (SPI_SHADER_USER_DATA_VS_0) = low dword; s1 = high dword. CI-ISA: low-order
        // word in the first SGPR.
        const S0_LOW: u32 = 0xDEAD_BEEF;
        const S1_HIGH: u32 = 0x0000_1234;
        const EXPECT: u64 = ((S1_HIGH as u64) << 32) | S0_LOW as u64; // 0x0000_1234_DEAD_BEEF

        let mut s = GpuState::default();
        s.sh_regs.set(sh_reg::SPI_SHADER_PGM_LO_VS, 0x0000_2000);
        s.sh_regs.set(sh_reg::SPI_SHADER_PGM_HI_VS, 0x0000_0000);
        s.sh_regs.set(sh_reg::SPI_SHADER_USER_DATA_VS_0, S0_LOW);
        s.sh_regs
            .set(sh_reg::SPI_SHADER_USER_DATA_VS_0 + 1, S1_HIGH);

        match s.derive_bound_shaders().vs {
            Some(ShaderRef::GcnBinary { res, .. }) => {
                assert_eq!(res.fetch_addr, Some(EXPECT));
            }
            other => panic!("expected GcnBinary VS with fetch_addr, got {other:?}"),
        }

        // The pixel stage takes no fetch call: its fetch_addr stays None even if the PS
        // user-data slot happens to hold bytes.
        let mut p = GpuState::default();
        p.sh_regs.set(sh_reg::SPI_SHADER_PGM_LO_PS, 0x0000_3000);
        p.sh_regs.set(sh_reg::SPI_SHADER_PGM_HI_PS, 0x0000_0000);
        p.sh_regs
            .set(sh_reg::SPI_SHADER_USER_DATA_PS_0, 0xFFFF_FFFF);
        match p.derive_bound_shaders().ps {
            Some(ShaderRef::GcnBinary { res, .. }) => assert_eq!(res.fetch_addr, None),
            other => panic!("expected GcnBinary PS, got {other:?}"),
        }
    }

    #[test]
    fn derive_prefers_embedded_over_pgm_regs() {
        // The embedded global route wins per stage even if PGM_LO/HI is also stamped
        // (the phase-3.5 embedded corpus does both) — Tier B stays embedded.
        use crate::pm4::opcodes::sh_reg;
        let mut s = GpuState::default();
        s.bind_embedded_shader(Stage::Vertex, 0);
        s.sh_regs.set(sh_reg::SPI_SHADER_PGM_LO_VS, 0x0000_E000);
        s.sh_regs.set(sh_reg::SPI_SHADER_PGM_HI_VS, 0x0000_0000);

        let bound = s.derive_bound_shaders();
        assert!(matches!(
            bound.vs,
            Some(ShaderRef::Embedded {
                stage: Stage::Vertex,
                id: 0
            })
        ));
    }

    #[test]
    fn derive_requires_both_address_halves() {
        // Only PGM_LO written (no HI) → not treated as a register bind.
        use crate::pm4::opcodes::sh_reg;
        let mut s = GpuState::default();
        s.sh_regs.set(sh_reg::SPI_SHADER_PGM_LO_PS, 0x0000_3000);
        assert!(s.derive_bound_shaders().ps.is_none());
    }

    #[test]
    fn bind_embedded_shader_records_view() {
        let mut s = GpuState::default();
        s.bind_embedded_shader(Stage::Vertex, 7);
        assert!(matches!(
            s.shaders.vs,
            Some(ShaderRef::Embedded {
                stage: Stage::Vertex,
                id: 7
            })
        ));
    }

    #[test]
    fn unbind_embedded_lets_register_route_win() {
        // task-73 AC#1: an embedded bind followed by an HLE register-route bind
        // (sceGnmSetVsShader -> unbind_embedded_shader) must yield the register
        // GcnBinary, not the stale embedded shadow — the embedded->register transition.
        // The raw-PM4 embedded corpus never calls unbind, so it keeps embedded (see
        // derive_prefers_embedded_over_pgm_regs), preserving Tier B (AC#2).
        use crate::pm4::opcodes::sh_reg;
        let mut s = GpuState::default();
        s.bind_embedded_shader(Stage::Vertex, 0);
        s.unbind_embedded_shader(Stage::Vertex);
        s.sh_regs.set(sh_reg::SPI_SHADER_PGM_LO_VS, 0x0000_2000);
        s.sh_regs.set(sh_reg::SPI_SHADER_PGM_HI_VS, 0x0000_0000);

        match s.derive_bound_shaders().vs {
            Some(ShaderRef::GcnBinary { addr, .. }) => assert_eq!(addr, 0x0020_0000),
            other => panic!("expected register GcnBinary after unbind, got {other:?}"),
        }
    }
}
