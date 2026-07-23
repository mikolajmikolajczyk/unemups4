//! `GpuState` (doc-4 §5, §C7): a sparse shadow register file (CONTEXT/SH/UCONFIG
//! banks, plus CONFIG) plus derived, typed views that grow per phase. New state =
//! decoding more register indices, never restructuring.
//!
//! The state lives in the [`GnmDriver`](crate::driver::GnmDriver) singleton, not
//! in the per-submit [`Executor`](crate::exec::Executor): PS4 context/SH registers
//! persist across submits (set in submit N, drawn in submit N+1), so the state must
//! outlive a single executor. The executor borrows `&mut GpuState` for the duration
//! of one submission (doc-4 §5 / §C7).
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
//! back and resolves the pair through the `ShaderProvider` (doc-4 §4). Modeling the
//! bind as a `ShaderRef` (not a hardcoded "two embedded shaders") keeps phase 4's
//! real `.sb` binds a matter of storing a different `ShaderRef`, not restructuring.
//!
//! TODO phase-4: the derived textures/samplers/RT views are added the same additive
//! way — by interpreting more register indices at draw time.

use std::collections::HashMap;

use crate::shader::source::{GcnResources, ShaderRef, Stage};

/// A sparse register bank: absolute register index → last-written u32 (doc-4 §C7).
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

/// The GPU shadow state (doc-4 §5, §C7): the sparse register banks plus the derived,
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
    /// Derived view: the currently-bound VS/PS pair (doc-4 §5). Read by the
    /// `IT_DRAW_INDEX_AUTO` arm; written by the embedded-shader HLE binds. Kept a
    /// field (not a global) so multi-ring/multi-context submits don't stomp one
    /// shared store.
    pub shaders: BoundShaders,
}

impl GpuState {
    /// Apply one `SET_*_REG` packet body into the bank named by `base` (a
    /// `reg_base::*` window). Body layout (doc-4 §C7): the first dword is the
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

    /// The VS/PS pair effective for the next draw (doc-4 §5 derived view).
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
            if let Some(r) = self.gcn_ref_from_regs(lo, hi, rsrc1, rsrc2) {
                out.set(stage, r);
            }
        }
        out
    }

    /// Build a [`ShaderRef::GcnBinary`] from the SH-bank shader-program registers, or
    /// `None` if `PGM_LO`/`PGM_HI` were never written (that stage isn't
    /// register-bound). `PGM_RSRC1/2` default to 0 (an empty footprint) if absent.
    fn gcn_ref_from_regs(&self, lo: u32, hi: u32, rsrc1: u32, rsrc2: u32) -> Option<ShaderRef> {
        use crate::pm4::opcodes::pgm_rsrc;
        // Require both address halves so a partial write isn't read as a garbage addr.
        let (pgm_lo, pgm_hi) = (self.sh_regs.get(lo)?, self.sh_regs.get(hi)?);
        let addr = crate::shader::sb::pgm_addr(pgm_lo, pgm_hi);
        let r1 = self.sh_regs.get(rsrc1).unwrap_or(0);
        let r2 = self.sh_regs.get(rsrc2).unwrap_or(0);
        Some(ShaderRef::GcnBinary {
            addr,
            res: GcnResources {
                num_vgprs: pgm_rsrc::num_vgprs(r1),
                num_sgprs: pgm_rsrc::num_sgprs(r1),
                num_user_sgprs: pgm_rsrc::num_user_sgprs(r2),
            },
        })
    }
}

/// The shaders currently bound for the next draw (doc-4 §5 derived view). Each is a
/// backend-agnostic [`ShaderRef`] the executor resolves through a `ShaderProvider`
/// at draw time — `None` until the guest binds one. Only VS/PS are tracked this
/// phase (the embedded pair); other HW stages (LS/HS/ES/GS) are added when a corpus
/// needs them (doc-4 §C8).
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
        // CONTEXT + offset + i (doc-4 §C7).
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
            Some(ShaderRef::GcnBinary { addr, res }) => {
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
