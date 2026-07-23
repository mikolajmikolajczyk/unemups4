//! wave64 CPU interpreter over the decoded GCN `Inst` stream — the differential
//! oracle (doc-4 §1, doc-5, decision-6).
//!
//! This executes a shader on the CPU, one wavefront of 64 lanes, with per-lane
//! `EXEC` masking from the first instruction. It is deliberately the *reference*
//! implementation: never fast, never GPU-bound, never discarded. The SPIR-V
//! recompiler (a later layer) is validated by diffing its output against the
//! [`ExportRecord`]s this produces, so the two must agree on the observable
//! contract — the captured exports — but not on how they get there.
//!
//! Scope is the triangle subset the committed corpus exercises: the VOP1/VOP2/VOP3
//! ALU it uses, `s_mov`, SMRD `s_load_dwordx*`, `s_waitcnt` (a no-op — there is no
//! async model here), MUBUF `buffer_load_format_*`, VINTRP `v_interp_p1/p2_f32`,
//! and EXP. It does **not** rasterize, feed the draw path, or model DS/MIMG/FLAT,
//! f64, or transcendentals beyond what the corpus needs. Anything outside the
//! subset — an unmapped op, an invalid operand, an [`Inst::Unknown`] — becomes a
//! structured [`InterpError`], never a panic.
//!
//! All memory the interpreter touches — every SMRD and MUBUF load — flows through
//! [`VirtualMemoryManager`]. There is no ambient/host access: in production the VMM
//! is the identity-mapped guest space; in tests it is a `Vec<u8>` mock, so the only
//! bytes the interpreter can ever see are the ones the mock holds.
//!
//! ## VINTRP / barycentric model (the recompiler must mirror this)
//!
//! GCN interpolates an attribute in two phases: `v_interp_p1_f32` reads the
//! barycentric *I* and computes `P0 + I·(P1−P0)`; `v_interp_p2_f32` reads *J* and
//! adds `J·(P2−P0)`, leaving `P0 + I·(P1−P0) + J·(P2−P0)` — the plane equation over
//! the triangle's three per-vertex attribute values `P0,P1,P2`.
//!
//! This is **screen-space-linear** interpolation: the *I,J* barycentrics the shader
//! reads in `v0,v1` are the launch inputs, and this stage does **no** perspective
//! divide of its own. Any perspective correction lives in how those barycentrics are
//! produced upstream (they can already be perspective-weighted), not here — a
//! recompiler must **not** add a perspective divide inside the interpolation, or its
//! exports will diverge from this oracle.
//!
//! Attribute selection here comes from the VINTRP instruction's own `attr`/`chan`
//! fields, not from `m0`. On real GCN `m0` holds the LDS param-cache base that
//! selects the attribute set; this interpreter deliberately does not model that
//! indirection (the corpus does `s_mov m0, s0` with a single attr0 set), so `m0` is
//! written but never consulted by [`Interp::exec_vintrp`]. A recompiler must mirror
//! the same simplification: attribute comes from the VINTRP field, not `m0`.
//!
//! The per-attr, per-channel `(P0,P1,P2)` triples live in [`PsInputs::attr_planes`]
//! (the SPI/PS input), indexed by the VINTRP `attr`/`chan` fields. `p1` writes
//! `P0 + I·(P1−P0)` to `vdst`; `p2` reads back that partial from `vdst` and adds
//! `J·(P2−P0)`. A recompiler diffing against this oracle must use the same plane
//! equation and the same P0/P1/P2 ordering for the exports to match.

use ps4_core::memory::VirtualMemoryManager;

use crate::inst::{Decoded, ExportTarget, Inst};
use crate::opcodes;
use crate::operand::{Operand, SpecialReg};

/// A wavefront is 64 lanes on GCN (wave64). EXEC / VCC are one bit per lane.
pub const WAVE_SIZE: usize = 64;

/// Number of SGPRs the interpreter models (GCN exposes s0..s103 + specials).
/// Exported so a recompiler / diff harness can range-validate register fields
/// against the same bound the oracle enforces.
pub const NUM_SGPRS: usize = 104;

/// Number of VGPRs the interpreter models (GCN exposes v0..v255).
/// Exported so a recompiler / diff harness can range-validate register fields
/// against the same bound the oracle enforces.
pub const NUM_VGPRS: usize = 256;

/// The per-attribute, per-channel triple of per-vertex attribute values a pixel
/// shader interpolates. `planes[attr][chan] = [P0, P1, P2]` — the values at the
/// triangle's three vertices, matching the VINTRP plane equation documented above.
#[derive(Clone, Debug, Default)]
pub struct PsInputs {
    /// `attr_planes[attr][chan] = [P0, P1, P2]`. Sparse is fine — a missing attr or
    /// channel reads as `[0.0; 3]`.
    pub attr_planes: Vec<[[f32; 3]; 4]>,
}

impl PsInputs {
    fn plane(&self, attr: u8, chan: u8) -> [f32; 3] {
        self.attr_planes
            .get(attr as usize)
            .map(|a| a[(chan & 0x3) as usize])
            .unwrap_or([0.0; 3])
    }
}

/// How to initialize the wave before the first instruction (the launch ABI).
pub enum LaunchAbi {
    /// Vertex-shader launch: `[user_sgprs]` are written to `s0..` and per-lane
    /// `v0 = first_vertex + lane`. `num_lanes` lanes are made live in `EXEC`.
    ///
    /// Instanced draws are not modeled: real GCN delivers `instance_id` in `v1`,
    /// but this ABI leaves `v1 = 0`. A shader that reads `v1` as an instance index
    /// would silently see instance 0 (not an error) — a limitation to lift before
    /// instanced geometry works.
    Vertex {
        /// User-data SGPRs (e.g. the V#-descriptor-set pointer in `s[2:3]`).
        user_sgprs: Vec<u32>,
        /// Vertex index of lane 0; lane `i` gets `first_vertex + i`.
        first_vertex: u32,
        /// How many lanes carry a real vertex (the rest are masked off in EXEC).
        num_lanes: usize,
    },
    /// Pixel-shader launch. The payload is boxed because it carries two 64-lane
    /// barycentric arrays — far larger than the `Vertex` variant.
    Pixel(Box<PixelLaunch>),
}

/// The pixel-shader launch payload (see [`LaunchAbi::Pixel`]).
pub struct PixelLaunch {
    /// User-data SGPRs written to `s0..`.
    pub user_sgprs: Vec<u32>,
    /// The interpolation planes the shader reads via `v_interp_*`.
    pub inputs: PsInputs,
    /// Per-lane barycentric I (goes to `v0`).
    pub bary_i: [f32; WAVE_SIZE],
    /// Per-lane barycentric J (goes to `v1`).
    pub bary_j: [f32; WAVE_SIZE],
    /// Initial EXEC coverage mask (a 0 bit = masked-off lane, no export).
    pub exec: u64,
}

/// One captured export — the interpreter's only observable output, and the shape
/// the recompiler's differential harness compares against. One record per EXP
/// instruction per live lane.
#[derive(Clone, PartialEq, Debug)]
pub struct ExportRecord {
    /// Which lane produced it (0..WAVE_SIZE). Masked-off lanes never appear.
    pub lane: usize,
    /// The export destination (pos/param/mrt/…), decoded from the EXP target.
    pub target: ExportTarget,
    /// The four channel values (RGBA / XYZW). A disabled channel is `0.0`.
    pub values: [f32; 4],
}

/// A structured, non-panicking failure. Every unsupported instruction, invalid
/// operand, or out-of-model condition surfaces here (AC #4) instead of aborting.
#[derive(Clone, PartialEq, Debug, thiserror::Error)]
pub enum InterpError {
    /// An instruction the interpreter does not model (op outside the subset, or an
    /// [`Inst::Unknown`]). Carries the offending inst and its stream offset. The
    /// [`Inst`] is boxed so this large variant does not bloat every `InterpError`.
    #[error("unsupported instruction at dword offset {offset}: {inst:?}")]
    UnsupportedInst { inst: Box<Inst>, offset: u32 },
    /// An operand the interpreter cannot evaluate in this position (e.g. a MUBUF
    /// `soffset` of raw 255, which the decoder marks invalid).
    #[error("invalid operand at dword offset {offset}: {operand:?} ({reason})")]
    InvalidOperand {
        operand: Operand,
        offset: u32,
        reason: &'static str,
    },
    /// A memory load through the VMM failed (unbacked address / short mapping).
    #[error("memory load at guest addr {addr:#x} ({size} bytes) failed: {reason}")]
    MemoryFault {
        addr: u64,
        size: usize,
        reason: &'static str,
    },
    /// An `image_sample` on a texture whose tile-mode index the interpreter has no
    /// detiler for (2D macro-tiling). Faulting keeps the oracle from silently reading
    /// swizzled bytes in the wrong order — the GPU path defers such a draw (task-98).
    #[error("unsupported texture tiling index {tiling_index} at dword offset {offset}")]
    UnsupportedTiling { tiling_index: u8, offset: u32 },
    /// A register-number field addressed an SGPR/VGPR outside the modeled file
    /// (`reg >= NUM_SGPRS`/`NUM_VGPRS`). Raised for any out-of-range register the
    /// instruction encodes — including implicit neighbours (an SGPR pair's high
    /// half, a multi-dword load's tail) — so a malformed or adversarial instruction
    /// faults cleanly instead of panicking on an out-of-bounds index (AC #4).
    #[error("register {kind} {reg} out of range (max {max}) at dword offset {offset}")]
    InvalidRegister {
        /// `"sgpr"` or `"vgpr"`.
        kind: &'static str,
        /// The register number that was out of range.
        reg: usize,
        /// The exclusive upper bound (`NUM_SGPRS` / `NUM_VGPRS`).
        max: usize,
        offset: u32,
    },
}

/// The architectural state of one in-flight wavefront.
///
/// SGPRs are wave-uniform (shared across lanes); VGPRs are per-lane
/// (`vgprs[reg][lane]`). `exec`/`vcc` are one bit per lane. This shape is what the
/// recompiler and the differential harness reuse.
pub struct WaveState {
    /// Wave-uniform scalar GPRs.
    pub sgprs: [u32; NUM_SGPRS],
    /// Per-lane vector GPRs: `vgprs[reg][lane]`.
    pub vgprs: Vec<[u32; WAVE_SIZE]>,
    /// Per-lane execute mask (bit `i` = lane `i` live).
    pub exec: u64,
    /// Per-lane vector condition code.
    pub vcc: u64,
    /// Scalar condition code.
    pub scc: bool,
    /// `m0` — interpolation base / addressing.
    pub m0: u32,
    /// Program counter, in dwords from the start of the decoded stream.
    pub pc: u32,
}

impl WaveState {
    fn new() -> Self {
        WaveState {
            sgprs: [0; NUM_SGPRS],
            vgprs: vec![[0; WAVE_SIZE]; NUM_VGPRS],
            exec: 0,
            vcc: 0,
            scc: false,
            m0: 0,
            pc: 0,
        }
    }

    #[inline]
    fn lane_live(&self, lane: usize) -> bool {
        self.exec & (1u64 << lane) != 0
    }
}

/// Run a decoded shader to completion (`s_endpgm` or end of stream) and return the
/// captured exports. `insts` is the decoded stream; `abi` is the launch ABI; `mem`
/// is the *only* memory the interpreter may touch.
pub fn run(
    insts: &[Decoded],
    abi: LaunchAbi,
    mem: &dyn VirtualMemoryManager,
) -> Result<Vec<ExportRecord>, InterpError> {
    let mut st = WaveState::new();
    let inputs = init_launch(&mut st, abi);
    let mut interp = Interp {
        st,
        inputs,
        mem,
        exports: Vec::new(),
    };
    interp.execute(insts)?;
    Ok(interp.exports)
}

fn init_launch(st: &mut WaveState, abi: LaunchAbi) -> PsInputs {
    match abi {
        LaunchAbi::Vertex {
            user_sgprs,
            first_vertex,
            num_lanes,
        } => {
            for (i, v) in user_sgprs.iter().enumerate().take(NUM_SGPRS) {
                st.sgprs[i] = *v;
            }
            let n = num_lanes.min(WAVE_SIZE);
            for lane in 0..n {
                st.vgprs[0][lane] = first_vertex + lane as u32;
            }
            st.exec = if n == WAVE_SIZE {
                u64::MAX
            } else {
                (1u64 << n) - 1
            };
            PsInputs::default()
        }
        LaunchAbi::Pixel(px) => {
            let PixelLaunch {
                user_sgprs,
                inputs,
                bary_i,
                bary_j,
                exec,
            } = *px;
            for (i, v) in user_sgprs.iter().enumerate().take(NUM_SGPRS) {
                st.sgprs[i] = *v;
            }
            for lane in 0..WAVE_SIZE {
                st.vgprs[0][lane] = bary_i[lane].to_bits();
                st.vgprs[1][lane] = bary_j[lane].to_bits();
            }
            st.exec = exec;
            inputs
        }
    }
}

struct Interp<'m> {
    st: WaveState,
    inputs: PsInputs,
    mem: &'m dyn VirtualMemoryManager,
    exports: Vec<ExportRecord>,
}

impl Interp<'_> {
    fn execute(&mut self, insts: &[Decoded]) -> Result<(), InterpError> {
        // Index by stream offset so a PC in dwords maps to the right instruction,
        // independent of variable instruction length.
        let mut i = 0usize;
        while i < insts.len() {
            let d = &insts[i];
            // Track the PC (in dwords) at the instruction boundary so wave state
            // reflects the position of the instruction being executed. There is no
            // control flow in the subset, so this only advances; a later branch-aware
            // interpreter would drive `i` from it.
            self.st.pc = d.offset_dwords;
            match &d.inst {
                Inst::Sopp { op, .. } if *op == opcodes::sopp::S_ENDPGM => break,
                Inst::Sopp { op, .. } if *op == opcodes::sopp::S_WAITCNT => {}
                Inst::Sopp { op, .. } if *op == opcodes::sopp::S_NOP => {}
                _ => self.exec_one(d)?,
            }
            i += 1;
        }
        Ok(())
    }

    fn exec_one(&mut self, d: &Decoded) -> Result<(), InterpError> {
        let off = d.offset_dwords;
        match &d.inst {
            Inst::Sop1 { op, sdst, ssrc0 } => self.exec_sop1(*op, *sdst, *ssrc0, off),
            Inst::Vop1 { op, vdst, src0 } => self.exec_vop1(*op, *vdst, *src0, off),
            Inst::Vop2 {
                op,
                vdst,
                src0,
                vsrc1,
                k,
            } => self.exec_vop2(*op, *vdst, *src0, *vsrc1, *k, off),
            Inst::Vop3 {
                op,
                vdst,
                src0,
                src1,
                src2,
                abs,
                neg,
                omod,
            } => self.exec_vop3(*op, *vdst, *src0, *src1, *src2, *abs, *neg, *omod, off),
            Inst::Smrd {
                op,
                sdst,
                sbase,
                imm,
                offset,
            } => self.exec_smrd(*op, *sdst, *sbase, *imm, *offset, off),
            Inst::Mubuf {
                op,
                vdata,
                vaddr,
                srsrc,
                soffset,
                offset,
                idxen,
                offen,
            } => self.exec_mubuf(
                *op, *vdata, *vaddr, *srsrc, *soffset, *offset, *idxen, *offen, off,
            ),
            Inst::Vintrp {
                op,
                vdst,
                vsrc,
                attr,
                chan,
            } => self.exec_vintrp(*op, *vdst, *vsrc, *attr, *chan, off),
            Inst::Mimg {
                op,
                vdata,
                vaddr,
                srsrc,
                ssamp,
                dmask,
                unrm,
            } => self.exec_mimg(*op, *vdata, *vaddr, *srsrc, *ssamp, *dmask, *unrm, off),
            Inst::Exp { target, srcs, .. } => self.exec_exp(*target, srcs, off),
            _ => Err(InterpError::UnsupportedInst {
                inst: Box::new(d.inst.clone()),
                offset: off,
            }),
        }
    }

    // ---- bounds-checked register access -------------------------------------
    //
    // Every path into the SGPR/VGPR arrays goes through these so an out-of-range
    // register number — from any field, including the implicit neighbours of a
    // pair or a multi-dword load — faults with a structured error instead of
    // panicking on an out-of-bounds index (AC #4).

    fn sgpr(&self, n: usize, off: u32) -> Result<u32, InterpError> {
        self.st
            .sgprs
            .get(n)
            .copied()
            .ok_or(InterpError::InvalidRegister {
                kind: "sgpr",
                reg: n,
                max: NUM_SGPRS,
                offset: off,
            })
    }

    fn set_sgpr(&mut self, n: usize, v: u32, off: u32) -> Result<(), InterpError> {
        let slot = self
            .st
            .sgprs
            .get_mut(n)
            .ok_or(InterpError::InvalidRegister {
                kind: "sgpr",
                reg: n,
                max: NUM_SGPRS,
                offset: off,
            })?;
        *slot = v;
        Ok(())
    }

    fn vgpr(&self, n: usize, lane: usize, off: u32) -> Result<u32, InterpError> {
        self.st
            .vgprs
            .get(n)
            .map(|r| r[lane])
            .ok_or(InterpError::InvalidRegister {
                kind: "vgpr",
                reg: n,
                max: NUM_VGPRS,
                offset: off,
            })
    }

    fn set_vgpr(&mut self, n: usize, lane: usize, v: u32, off: u32) -> Result<(), InterpError> {
        let reg = self
            .st
            .vgprs
            .get_mut(n)
            .ok_or(InterpError::InvalidRegister {
                kind: "vgpr",
                reg: n,
                max: NUM_VGPRS,
                offset: off,
            })?;
        reg[lane] = v;
        Ok(())
    }

    // ---- operand evaluation -------------------------------------------------

    /// Read a scalar (wave-uniform) operand as raw bits.
    fn read_scalar(&self, op: Operand, off: u32) -> Result<u32, InterpError> {
        match op {
            Operand::Sgpr(n) => self.sgpr(n as usize, off),
            Operand::Special(SpecialReg::M0) => Ok(self.st.m0),
            Operand::Special(SpecialReg::VccLo) => Ok(self.st.vcc as u32),
            Operand::Special(SpecialReg::VccHi) => Ok((self.st.vcc >> 32) as u32),
            Operand::Special(SpecialReg::ExecLo) => Ok(self.st.exec as u32),
            Operand::Special(SpecialReg::ExecHi) => Ok((self.st.exec >> 32) as u32),
            Operand::Special(SpecialReg::Scc) => Ok(self.st.scc as u32),
            Operand::InlineInt(v) => Ok(v as u32),
            Operand::InlineFloat(f) => Ok(f.to_bits()),
            Operand::Literal(v) => Ok(v),
            other => Err(InterpError::InvalidOperand {
                operand: other,
                offset: off,
                reason: "not a scalar source",
            }),
        }
    }

    /// Read a source operand for lane `lane` as raw bits (VGPR is per-lane; scalar
    /// and inline sources are lane-uniform).
    fn read_src_lane(&self, op: Operand, lane: usize, off: u32) -> Result<u32, InterpError> {
        match op {
            Operand::Vgpr(n) => self.vgpr(n as usize, lane, off),
            _ => self.read_scalar(op, off),
        }
    }

    fn read_f32_lane(&self, op: Operand, lane: usize, off: u32) -> Result<f32, InterpError> {
        Ok(f32::from_bits(self.read_src_lane(op, lane, off)?))
    }

    fn write_vgpr(
        &mut self,
        vdst: Operand,
        lane: usize,
        bits: u32,
        off: u32,
    ) -> Result<(), InterpError> {
        match vdst {
            Operand::Vgpr(n) => self.set_vgpr(n as usize, lane, bits, off),
            other => Err(InterpError::InvalidOperand {
                operand: other,
                offset: off,
                reason: "not a vector destination",
            }),
        }
    }

    // ---- scalar ALU ---------------------------------------------------------

    fn exec_sop1(
        &mut self,
        op: u8,
        sdst: Operand,
        ssrc0: Operand,
        off: u32,
    ) -> Result<(), InterpError> {
        match op {
            opcodes::sop1::S_MOV_B32 => {
                let v = self.read_scalar(ssrc0, off)?;
                self.write_sgpr(sdst, v, off)
            }
            _ => Err(self.unsupported_sop1(op, sdst, ssrc0, off)),
        }
    }

    fn unsupported_sop1(&self, op: u8, sdst: Operand, ssrc0: Operand, off: u32) -> InterpError {
        InterpError::UnsupportedInst {
            inst: Box::new(Inst::Sop1 { op, sdst, ssrc0 }),
            offset: off,
        }
    }

    fn write_sgpr(&mut self, sdst: Operand, v: u32, off: u32) -> Result<(), InterpError> {
        match sdst {
            Operand::Sgpr(n) => self.set_sgpr(n as usize, v, off),
            Operand::Special(SpecialReg::M0) => {
                self.st.m0 = v;
                Ok(())
            }
            other => Err(InterpError::InvalidOperand {
                operand: other,
                offset: off,
                reason: "not a scalar destination",
            }),
        }
    }

    // ---- vector ALU ---------------------------------------------------------

    fn exec_vop1(
        &mut self,
        op: u8,
        vdst: Operand,
        src0: Operand,
        off: u32,
    ) -> Result<(), InterpError> {
        // Reject an unsupported op BEFORE touching any lane, so a failure never
        // leaves a partially-written wave state.
        use opcodes::vop1::*;
        if !matches!(
            op,
            V_MOV_B32 | V_CVT_F32_I32 | V_CVT_F32_U32 | V_CVT_U32_F32 | V_CVT_I32_F32
        ) {
            return Err(InterpError::UnsupportedInst {
                inst: Box::new(Inst::Vop1 { op, vdst, src0 }),
                offset: off,
            });
        }
        for lane in 0..WAVE_SIZE {
            if !self.st.lane_live(lane) {
                continue;
            }
            let out = match op {
                V_MOV_B32 => self.read_src_lane(src0, lane, off)?,
                V_CVT_F32_I32 => (self.read_src_lane(src0, lane, off)? as i32 as f32).to_bits(),
                V_CVT_F32_U32 => (self.read_src_lane(src0, lane, off)? as f32).to_bits(),
                V_CVT_U32_F32 => self.read_f32_lane(src0, lane, off)? as u32,
                // The only remaining supported op after the guard above.
                _ => (self.read_f32_lane(src0, lane, off)? as i32) as u32,
            };
            self.write_vgpr(vdst, lane, out, off)?;
        }
        Ok(())
    }

    fn exec_vop2(
        &mut self,
        op: u8,
        vdst: Operand,
        src0: Operand,
        vsrc1: Operand,
        k: Option<u32>,
        off: u32,
    ) -> Result<(), InterpError> {
        // Reject an unsupported op before touching any lane (no partial wave state).
        use opcodes::vop2::*;
        if !matches!(
            op,
            V_ADD_F32 | V_SUB_F32 | V_MUL_F32 | V_MAC_F32 | V_MADMK_F32 | V_MADAK_F32
        ) {
            return Err(InterpError::UnsupportedInst {
                inst: Box::new(Inst::Vop2 {
                    op,
                    vdst,
                    src0,
                    vsrc1,
                    k,
                }),
                offset: off,
            });
        }
        for lane in 0..WAVE_SIZE {
            if !self.st.lane_live(lane) {
                continue;
            }
            let a = self.read_f32_lane(src0, lane, off)?;
            let b = self.read_f32_lane(vsrc1, lane, off)?;
            let out = match op {
                V_ADD_F32 => a + b,
                V_SUB_F32 => a - b,
                V_MUL_F32 => a * b,
                V_MAC_F32 => {
                    let acc = self.read_f32_lane(vdst, lane, off)?;
                    a * b + acc
                }
                // v_madmk: vdst = src0 * K + vsrc1. v_madak: vdst = src0 * vsrc1 + K.
                V_MADMK_F32 => {
                    let kf = f32::from_bits(k.unwrap_or(0));
                    a * kf + b
                }
                // v_madak (the only remaining op after the guard): src0*vsrc1 + K.
                _ => {
                    let kf = f32::from_bits(k.unwrap_or(0));
                    a * b + kf
                }
            };
            self.write_vgpr(vdst, lane, out.to_bits(), off)?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn exec_vop3(
        &mut self,
        op: u16,
        vdst: Operand,
        src0: Operand,
        src1: Operand,
        src2: Operand,
        abs: u8,
        neg: u8,
        omod: u8,
        off: u32,
    ) -> Result<(), InterpError> {
        // Reject an unsupported op before touching any lane (no partial wave state).
        use opcodes::vop3::*;
        if !matches!(op, V_MAD_F32 | V_FMA_F32 | V_MED3_F32) {
            return Err(InterpError::UnsupportedInst {
                inst: Box::new(Inst::Vop3 {
                    op,
                    vdst,
                    src0,
                    src1,
                    src2,
                    abs,
                    neg,
                    omod,
                }),
                offset: off,
            });
        }
        for lane in 0..WAVE_SIZE {
            if !self.st.lane_live(lane) {
                continue;
            }
            let a = self.apply_mods(self.read_f32_lane(src0, lane, off)?, abs, neg, 0);
            let b = self.apply_mods(self.read_f32_lane(src1, lane, off)?, abs, neg, 1);
            let c = self.apply_mods(self.read_f32_lane(src2, lane, off)?, abs, neg, 2);
            let raw = match op {
                // v_mad_f32 is UNFUSED on GCN: a*b rounds, then +c rounds again.
                V_MAD_F32 => a * b + c,
                // v_fma_f32 is FUSED: a single rounding of a*b+c. `mul_add` gives the
                // fused result so a recompiler's SPIR-V FMA matches bit-for-bit.
                V_FMA_F32 => a.mul_add(b, c),
                // v_med3_f32 (the only remaining op after the guard).
                _ => median3(a, b, c),
            };
            let out = apply_omod(raw, omod);
            self.write_vgpr(vdst, lane, out.to_bits(), off)?;
        }
        Ok(())
    }

    fn apply_mods(&self, mut v: f32, abs: u8, neg: u8, idx: u8) -> f32 {
        if abs & (1 << idx) != 0 {
            v = v.abs();
        }
        if neg & (1 << idx) != 0 {
            v = -v;
        }
        v
    }

    // ---- SMRD: scalar memory read via the VMM -------------------------------

    fn exec_smrd(
        &mut self,
        op: u8,
        sdst: Operand,
        sbase: u8,
        imm: bool,
        offset: u32,
        off: u32,
    ) -> Result<(), InterpError> {
        let count = opcodes::smrd::dst_count(op).ok_or(InterpError::UnsupportedInst {
            inst: Box::new(Inst::Smrd {
                op,
                sdst,
                sbase,
                imm,
                offset,
            }),
            offset: off,
        })?;
        // The base is an SGPR *pair* holding a 64-bit guest pointer.
        let base = self.read_sgpr_u64(sbase as usize, off)?;
        // SMRD offset is a dword (4-byte) offset when immediate, else an SGPR byte
        // offset. The corpus uses the immediate form only.
        let byte_off = if imm {
            u64::from(offset) * 4
        } else {
            u64::from(self.sgpr(offset as usize, off)?)
        };
        let addr = base.wrapping_add(byte_off);
        let bytes = self.load(addr, count as usize * 4, off)?;
        let dst0 = match sdst {
            Operand::Sgpr(n) => n as usize,
            other => {
                return Err(InterpError::InvalidOperand {
                    operand: other,
                    offset: off,
                    reason: "SMRD destination must be an SGPR",
                });
            }
        };
        for i in 0..count as usize {
            let w = read_le_u32(&bytes, i * 4);
            self.set_sgpr(dst0 + i, w, off)?;
        }
        Ok(())
    }

    /// Read the 64-bit value held in the SGPR *pair* at `base` (`base` low,
    /// `base + 1` high). Both halves are bounds-checked, so an out-of-range base —
    /// or a base whose implicit high half falls past the file — faults cleanly.
    fn read_sgpr_u64(&self, base: usize, off: u32) -> Result<u64, InterpError> {
        let lo = u64::from(self.sgpr(base, off)?);
        let hi = u64::from(self.sgpr(base + 1, off)?);
        Ok(lo | (hi << 32))
    }

    // ---- MUBUF: buffer_load_format_* via the VMM ----------------------------

    #[allow(clippy::too_many_arguments)]
    fn exec_mubuf(
        &mut self,
        op: u8,
        vdata: Operand,
        vaddr: Operand,
        srsrc: u8,
        soffset: Operand,
        offset: u16,
        idxen: bool,
        offen: bool,
        off: u32,
    ) -> Result<(), InterpError> {
        let count = opcodes::mubuf::vdata_count(op).ok_or(InterpError::UnsupportedInst {
            inst: Box::new(Inst::Mubuf {
                op,
                vdata,
                vaddr,
                srsrc,
                soffset,
                offset,
                idxen,
                offen,
            }),
            offset: off,
        })?;
        // Decoder marks an invalid MUBUF soffset (field 255) as Raw(255); the corpus
        // uses an inline-0 soffset. Evaluate it as a scalar; Raw is rejected.
        let soff = match soffset {
            Operand::Raw(255) => {
                return Err(InterpError::InvalidOperand {
                    operand: soffset,
                    offset: off,
                    reason: "MUBUF soffset field 255 is invalid",
                });
            }
            other => self.read_scalar(other, off)?,
        };

        // Decode the V# (128-bit buffer resource) from the four SGPRs at `srsrc`.
        let vsharp = self.decode_v_sharp(srsrc as usize, off)?;
        let base = vsharp.base;
        let stride = vsharp.stride;
        let num_records = vsharp.num_records;

        let vdata0 = match vdata {
            Operand::Vgpr(n) => n,
            other => {
                return Err(InterpError::InvalidOperand {
                    operand: other,
                    offset: off,
                    reason: "MUBUF vdata must be a VGPR",
                });
            }
        };

        // `vaddr` is the index VGPR; with `offen` the *byte offset* comes from the
        // next VGPR (`vaddr + 1`), not a re-read of `vaddr` (which would silently
        // double-count). The corpus never sets `offen`, so this path is unexercised
        // but must be correct for a recompiler to mirror.
        let offen_reg = if offen {
            match vaddr {
                Operand::Vgpr(n) => {
                    let reg = n as usize + 1;
                    // Bounds-check the offset register (vaddr+1) BEFORE the per-lane
                    // loop, so an out-of-range offset reg faults cleanly instead of
                    // writing some lanes' vdata and then aborting mid-fetch (partial
                    // state). Lane 0 stands in for the file bounds (register presence
                    // is lane-independent).
                    self.vgpr(reg, 0, off)?;
                    Some(reg)
                }
                other => {
                    return Err(InterpError::InvalidOperand {
                        operand: other,
                        offset: off,
                        reason: "MUBUF offen requires a VGPR vaddr (offset is vaddr+1)",
                    });
                }
            }
        } else {
            None
        };

        for lane in 0..WAVE_SIZE {
            if !self.st.lane_live(lane) {
                continue;
            }
            // idxen: the fetch index is vaddr's VGPR (per-lane), scaled by stride.
            let index = if idxen {
                u64::from(self.read_src_lane(vaddr, lane, off)?)
            } else {
                0
            };
            // An index at or past `num_records` is out of bounds. GCN's robust-buffer
            // behavior clamps such a fetch; we clamp the index to the last valid
            // record (num_records - 1). A recompiler / diff harness must clamp the
            // same way. `num_records == 0` degenerates to index 0.
            let index = if num_records != 0 && index >= num_records {
                num_records - 1
            } else {
                index
            };
            // offen: per-lane byte offset from vaddr+1 (0 when offen is clear).
            let voffset = match offen_reg {
                Some(reg) => u64::from(self.vgpr(reg, lane, off)?),
                None => 0,
            };
            let addr = base
                .wrapping_add(index.wrapping_mul(stride))
                .wrapping_add(u64::from(soff))
                .wrapping_add(u64::from(offset))
                .wrapping_add(voffset);
            let bytes = self.load(addr, count as usize * 4, off)?;
            for i in 0..count as usize {
                let w = read_le_u32(&bytes, i * 4);
                self.set_vgpr(vdata0 as usize + i, lane, w, off)?;
            }
        }
        Ok(())
    }

    /// Decode the 128-bit V# (buffer resource) from the SGPRs beginning at `srsrc`.
    /// Shared by SMRD-fetched and MUBUF-referenced descriptors so the two agree on
    /// the layout. Only the three words this simplified fetch needs are read and
    /// bounds-checked (`srsrc + 0..=2`); word3 (format/swizzle) is not consulted, so
    /// it is neither read nor bounds-checked.
    ///
    /// - word0 = base[31:0]
    /// - word1[15:0] = base[47:32]; word1[29:16] = stride
    /// - word2 = num_records (element count; index >= this is out of bounds)
    /// - word3 = format/swizzle (unused by this simplified fetch — not read)
    fn decode_v_sharp(&self, srsrc: usize, off: u32) -> Result<VSharp, InterpError> {
        let w0 = u64::from(self.sgpr(srsrc, off)?);
        let w1 = self.sgpr(srsrc + 1, off)?;
        let w2 = self.sgpr(srsrc + 2, off)?;
        Ok(VSharp {
            base: w0 | (u64::from(w1 & 0xFFFF) << 32),
            stride: u64::from((w1 >> 16) & 0x3FFF),
            num_records: u64::from(w2),
        })
    }

    // ---- VINTRP: barycentric interpolation ----------------------------------

    fn exec_vintrp(
        &mut self,
        op: u8,
        vdst: Operand,
        vsrc: Operand,
        attr: u8,
        chan: u8,
        off: u32,
    ) -> Result<(), InterpError> {
        // Reject an unsupported op before touching any lane (no partial wave state).
        // Attribute selection comes from the VINTRP `attr`/`chan` fields, NOT `m0`
        // (a deliberate simplification — see the module-level VINTRP contract).
        use opcodes::vintrp::*;
        if !matches!(op, V_INTERP_P1_F32 | V_INTERP_P2_F32 | V_INTERP_MOV_F32) {
            return Err(InterpError::UnsupportedInst {
                inst: Box::new(Inst::Vintrp {
                    op,
                    vdst,
                    vsrc,
                    attr,
                    chan,
                }),
                offset: off,
            });
        }
        let plane = self.inputs.plane(attr, chan);
        let (p0, p1, p2) = (plane[0], plane[1], plane[2]);
        for lane in 0..WAVE_SIZE {
            if !self.st.lane_live(lane) {
                continue;
            }
            let bary = self.read_f32_lane(vsrc, lane, off)?;
            let out = match op {
                // p1: P0 + I·(P1 − P0), with I = bary (v0).
                V_INTERP_P1_F32 => p0 + bary * (p1 - p0),
                // p2: (partial in vdst) + J·(P2 − P0), with J = bary (v1).
                V_INTERP_P2_F32 => {
                    let partial = self.read_f32_lane(vdst, lane, off)?;
                    partial + bary * (p2 - p0)
                }
                // mov: constant attribute (P0), no barycentric.
                _ => p0,
            };
            self.write_vgpr(vdst, lane, out.to_bits(), off)?;
        }
        Ok(())
    }

    // ---- MIMG: image_sample via the VMM (the sampling oracle) ---------------

    /// Reference `image_sample`: decode the T# (256-bit image resource) at `srsrc`
    /// and the S# (128-bit sampler) at `ssamp`, read the (u, v) coordinate from the
    /// `vaddr` VGPR pair, sample the guest texture on the CPU (point or bilinear per
    /// the S# filter), and write the enabled `dmask` channels to the `vdata` VGPRs.
    ///
    /// This is the differential ORACLE for sampling (decision-3): the recompiler emits
    /// an `OpImageSampleImplicitLod` that the GPU evaluates against the SAME detiled
    /// texture bytes, so the two must agree on the observable texel. The subset is
    /// linear (untiled) `R8G8B8A8_UNORM`, normalized coordinates, no mips/anisotropy —
    /// exactly what the corpus and the upload path produce.
    #[allow(clippy::too_many_arguments)]
    fn exec_mimg(
        &mut self,
        op: u8,
        vdata: Operand,
        vaddr: Operand,
        srsrc: u8,
        ssamp: u8,
        dmask: u8,
        unrm: bool,
        off: u32,
    ) -> Result<(), InterpError> {
        if op != opcodes::mimg::IMAGE_SAMPLE {
            return Err(InterpError::UnsupportedInst {
                inst: Box::new(Inst::Mimg {
                    op,
                    vdata,
                    vaddr,
                    srsrc,
                    ssamp,
                    dmask,
                    unrm,
                }),
                offset: off,
            });
        }
        let tsharp = self.decode_t_sharp(srsrc as usize, off)?;
        let ssharp = self.decode_s_sharp(ssamp as usize, off)?;
        let vdata0 = match vdata {
            Operand::Vgpr(n) => n as usize,
            other => {
                return Err(InterpError::InvalidOperand {
                    operand: other,
                    offset: off,
                    reason: "MIMG vdata must be a VGPR",
                });
            }
        };
        let (vu, vv) = match vaddr {
            Operand::Vgpr(n) => (n as usize, n as usize + 1),
            other => {
                return Err(InterpError::InvalidOperand {
                    operand: other,
                    offset: off,
                    reason: "MIMG vaddr must be a VGPR",
                });
            }
        };
        // The four RGBA channels always sample; `dmask` selects which reach `vdata`.
        // The destination register index advances only for enabled channels — the
        // hardware packs enabled channels contiguously (dst[0]=first enabled, …).
        for lane in 0..WAVE_SIZE {
            if !self.st.lane_live(lane) {
                continue;
            }
            let u = f32::from_bits(self.vgpr(vu, lane, off)?);
            let v = f32::from_bits(self.vgpr(vv, lane, off)?);
            let rgba = self.sample_texture(&tsharp, &ssharp, u, v, unrm, off)?;
            let mut dreg = vdata0;
            for (ch, &texel) in rgba.iter().enumerate() {
                if dmask & (1 << ch) == 0 {
                    continue;
                }
                self.set_vgpr(dreg, lane, texel.to_bits(), off)?;
                dreg += 1;
            }
        }
        Ok(())
    }

    /// Decode the 256-bit T# (image resource) from the eight SGPRs at `base`. Only the
    /// fields the linear-RGBA8 sampling subset needs are read + bounds-checked. Layout
    /// (GFX6/7 image descriptor, matching shadPS4/GPCS4):
    /// - word0 = base[39:8]; word1[7:0] = base[47:40] (guest base = (w0<<8)|(w1[7:0]<<40))
    /// - word1[25:20] = dfmt, word1[29:26] = nfmt
    /// - word2[13:0] = width - 1; word2[27:14] = height - 1
    /// - word3[22:20] = tiling index (0 = linear; the subset only samples linear)
    ///
    /// The base is treated as 48-bit (like the V#) so an identity-mapped host address
    /// round-trips; `word1[7:0]` (GFX6/7 min_lod fraction, unused without mips) carries
    /// base[47:40] in this HLE model. The recompiler mirrors nothing here — it resolves
    /// the sampled image symbolically — so only the interpreter reads these bytes.
    fn decode_t_sharp(&self, base: usize, off: u32) -> Result<TSharp, InterpError> {
        let w0 = self.sgpr(base, off)?;
        let w1 = self.sgpr(base + 1, off)?;
        let w2 = self.sgpr(base + 2, off)?;
        let w3 = self.sgpr(base + 3, off)?;
        // Bounds-check the upper half of the 256-bit descriptor even though the subset
        // reads only the low four words, so a T# whose implicit tail runs past the file
        // faults cleanly rather than silently truncating.
        for i in 4..8 {
            self.sgpr(base + i, off)?;
        }
        Ok(TSharp {
            base: (u64::from(w0) << 8) | (u64::from(w1 & 0xFF) << 40),
            width: ((w2 & 0x3FFF) + 1) as usize,
            height: (((w2 >> 14) & 0x3FFF) + 1) as usize,
            dfmt: ((w1 >> 20) & 0x3F) as u8,
            nfmt: ((w1 >> 26) & 0xF) as u8,
            tiling_index: ((w3 >> 20) & 0x1F) as u8,
        })
    }

    /// Decode the 128-bit S# (sampler) from the four SGPRs at `base`. Only the min/mag
    /// filter bit the subset honors is read; the rest of the sampler word is not
    /// consulted (no anisotropy/LOD/border in the subset). Layout: word2[21:20] carry
    /// the xy mag/min filter selects on GFX6/7 (0 = point, non-zero = bilinear).
    fn decode_s_sharp(&self, base: usize, off: u32) -> Result<SSharp, InterpError> {
        let w0 = self.sgpr(base, off)?;
        let w1 = self.sgpr(base + 1, off)?;
        let w2 = self.sgpr(base + 2, off)?;
        let w3 = self.sgpr(base + 3, off)?;
        let _ = (w0, w1, w3);
        // xy_mag_filter = word2[20], xy_min_filter = word2[22] on GFX6/7 (0 = point,
        // 1 = bilinear). The subset treats any non-point select as bilinear.
        let bilinear = (w2 >> 20) & 1 == 1;
        Ok(SSharp { bilinear })
    }

    /// Sample one texel of the linear `R8G8B8A8_UNORM` texture `t` at normalized
    /// coordinate `(u, v)` with `s`'s filter, reading the bytes through the VMM. Repeat
    /// addressing (the sampler default the S# subset models). Returns RGBA in [0, 1].
    fn sample_texture(
        &self,
        t: &TSharp,
        s: &SSharp,
        u: f32,
        v: f32,
        unrm: bool,
        off: u32,
    ) -> Result<[f32; 4], InterpError> {
        if t.width == 0 || t.height == 0 {
            return Err(InterpError::MemoryFault {
                addr: t.base,
                size: 0,
                reason: "T# has zero extent",
            });
        }
        // Convert to texel space. Normalized coords scale by the extent; UNRM coords
        // are already texel indices.
        let (fx, fy) = if unrm {
            (u, v)
        } else {
            (u * t.width as f32, v * t.height as f32)
        };
        if s.bilinear {
            // Bilinear: sample four texels around (fx-0.5, fy-0.5) and lerp. GPU
            // convention places texel centers at integer + 0.5.
            let x = fx - 0.5;
            let y = fy - 0.5;
            let x0 = x.floor();
            let y0 = y.floor();
            let tx = x - x0;
            let ty = y - y0;
            let (x0, y0) = (x0 as i64, y0 as i64);
            let c00 = self.texel(t, x0, y0, off)?;
            let c10 = self.texel(t, x0 + 1, y0, off)?;
            let c01 = self.texel(t, x0, y0 + 1, off)?;
            let c11 = self.texel(t, x0 + 1, y0 + 1, off)?;
            let mut out = [0.0f32; 4];
            for c in 0..4 {
                let top = c00[c] + (c10[c] - c00[c]) * tx;
                let bot = c01[c] + (c11[c] - c01[c]) * tx;
                out[c] = top + (bot - top) * ty;
            }
            Ok(out)
        } else {
            // Point: nearest texel (floor of the texel coordinate).
            self.texel(t, fx.floor() as i64, fy.floor() as i64, off)
        }
    }

    /// Read one `R8G8B8A8_UNORM` texel `(x, y)` of texture `t` (repeat wrap), through the
    /// VMM, as RGBA floats in [0, 1]. The guest bytes are stored per `t.tiling_index`, so
    /// the byte offset applies the SAME swizzle the gnm upload path detiles with
    /// (`ps4_core::tiling`) — otherwise the oracle and the GPU would sample different
    /// texels for a tiled texture (task-98). The four bytes are read as one bounded load,
    /// so an out-of-range base faults cleanly.
    fn texel(&self, t: &TSharp, x: i64, y: i64, off: u32) -> Result<[f32; 4], InterpError> {
        use ps4_core::tiling::{TileKind, thin1d_texel_offset, tile_kind};
        // Repeat addressing: euclidean-mod into [0, extent). Both extents are non-zero
        // (checked in `sample_texture`).
        let w = t.width as i64;
        let h = t.height as i64;
        let xx = x.rem_euclid(w) as u32;
        let yy = y.rem_euclid(h) as u32;
        // Byte offset of the logical texel within the guest surface, per its tile mode.
        // Linear is row-major; 1D-thin swizzles into 8×8 micro-tiles; 2D macro-tiling has
        // no detiler (the GPU path defers it too) so it faults rather than mis-reading.
        let texel_off = match tile_kind(t.tiling_index) {
            TileKind::Linear => (yy as u64 * t.width as u64 + xx as u64) * 4,
            TileKind::Thin1d => thin1d_texel_offset(xx, yy, t.width as u32, 4) as u64,
            TileKind::Macro2d => {
                return Err(InterpError::UnsupportedTiling {
                    tiling_index: t.tiling_index,
                    offset: off,
                });
            }
        };
        let addr = t.base.wrapping_add(texel_off);
        let bytes = self.load(addr, 4, off)?;
        Ok([
            bytes[0] as f32 / 255.0,
            bytes[1] as f32 / 255.0,
            bytes[2] as f32 / 255.0,
            bytes[3] as f32 / 255.0,
        ])
    }

    // ---- EXP: capture the export -------------------------------------------

    fn exec_exp(
        &mut self,
        target: ExportTarget,
        srcs: &[Option<Operand>; 4],
        off: u32,
    ) -> Result<(), InterpError> {
        for lane in 0..WAVE_SIZE {
            if !self.st.lane_live(lane) {
                continue;
            }
            let mut values = [0.0f32; 4];
            for (ch, slot) in srcs.iter().enumerate() {
                if let Some(src) = slot {
                    values[ch] = self.read_f32_lane(*src, lane, off)?;
                }
            }
            self.exports.push(ExportRecord {
                lane,
                target,
                values,
            });
        }
        Ok(())
    }

    // ---- memory: the only path to bytes -------------------------------------

    fn load(&self, addr: u64, size: usize, _off: u32) -> Result<Vec<u8>, InterpError> {
        self.mem
            .read_bytes(addr, size)
            .map_err(|reason| InterpError::MemoryFault { addr, size, reason })
    }
}

/// A decoded 256-bit T# (image resource) — the fields the linear-RGBA8 sampling
/// subset needs. `dfmt`/`nfmt` are carried but only the RGBA8 case is sampled.
struct TSharp {
    /// Guest base address of the texel data (word0 << 8).
    base: u64,
    /// Texel width (word2[13:0] + 1).
    width: usize,
    /// Texel height (word2[27:14] + 1).
    height: usize,
    /// Data format (`dfmt`) — the subset samples R8G8B8A8 only.
    #[allow(dead_code)]
    dfmt: u8,
    /// Number format (`nfmt`) — the subset samples UNORM only.
    #[allow(dead_code)]
    nfmt: u8,
    /// Tiling index (word3[22:20]); 0 = linear (the only mode the subset samples).
    #[allow(dead_code)]
    tiling_index: u8,
}

/// A decoded 128-bit S# (sampler) — only the filter selector the subset honors.
struct SSharp {
    /// `true` = bilinear filtering, `false` = point/nearest.
    bilinear: bool,
}

/// A decoded 128-bit V# (buffer resource) — the fields the fetch path needs.
struct VSharp {
    /// 48-bit base guest address.
    base: u64,
    /// Per-index stride in bytes.
    stride: u64,
    /// Element count; a fetch index at or past this is out of bounds (clamped).
    num_records: u64,
}

/// Read a little-endian `u32` from `bytes` at byte offset `at`. The caller sizes
/// the slice, so the four indices are always in range.
fn read_le_u32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

fn median3(a: f32, b: f32, c: f32) -> f32 {
    a.max(b).min(a.min(b).max(c))
}

/// Apply the VOP3 output modifier: 1 = ×2, 2 = ×4, 3 = ÷2 (0 = none).
fn apply_omod(v: f32, omod: u8) -> f32 {
    match omod {
        1 => v * 2.0,
        2 => v * 4.0,
        3 => v * 0.5,
        _ => v,
    }
}
