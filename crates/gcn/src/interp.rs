//! wave64 CPU interpreter over the decoded GCN `Inst` stream — the differential
//! oracle (doc-2 §1, doc-3, decision-6).
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

/// Is this decoded instruction a CFG *branch* terminator (`s_branch` /
/// `s_cbranch_*`)? Such an instruction drives the block terminator in the CFG walk,
/// not a per-lane dataflow op, so the block body skips it. `s_endpgm` is handled
/// separately (it ends the wave). Mirrors `recompile::is_cfg_branch`.
fn is_cfg_branch(d: &Decoded) -> bool {
    use opcodes::sopp::*;
    matches!(
        &d.inst,
        Inst::Sopp { op, .. }
            if matches!(
                *op,
                S_BRANCH
                    | S_CBRANCH_SCC0
                    | S_CBRANCH_SCC1
                    | S_CBRANCH_VCCZ
                    | S_CBRANCH_VCCNZ
                    | S_CBRANCH_EXECZ
                    | S_CBRANCH_EXECNZ
            )
    )
}

/// Map a CFG rejection to an [`InterpError`] so the oracle defers on the same
/// control-flow shapes the recompiler defers on (an irreducible/unsupported loop, an
/// SCC branch, an out-of-range target). Carries the offending branch's offset. A
/// *recognized* natural loop is NOT a rejection — it builds a CFG the walk executes.
fn cfg_error_to_interp(e: crate::cfg::CfgError) -> InterpError {
    use crate::cfg::CfgError;
    let (op, offset) = match e {
        CfgError::IrreducibleLoop { branch_off, .. } => (opcodes::sopp::S_BRANCH, branch_off),
        CfgError::TargetOutOfRange { branch_off, .. } => (opcodes::sopp::S_BRANCH, branch_off),
        CfgError::UnsupportedCondBranch { branch_off, op } => (op, branch_off),
    };
    InterpError::UnsupportedInst {
        inst: Box::new(Inst::Sopp { op, simm16: 0 }),
        offset: offset as u32,
    }
}

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
        // Build the shared CFG (same view the recompiler lowers). A shader whose
        // control flow is outside the first-slice subset (a loop back-edge, an SCC
        // branch, an out-of-range target) surfaces as an UnsupportedInst so the oracle
        // defers in lockstep with the recompiler instead of misexecuting.
        let cfg = crate::cfg::build_cfg(insts).map_err(cfg_error_to_interp)?;
        self.execute_cfg(insts, &cfg)
    }

    /// Execute a CFG keyed on EXEC. Straight-line blocks run their body; a `Cond`
    /// terminator narrows EXEC per arm, runs the `taken` arm then the `fall` arm under
    /// their respective masks, and OR-restores EXEC at the structured merge (the
    /// branch's post-dominator, [`crate::cfg::Cfg::merge_target`]).
    ///
    /// Two shapes are handled, mirroring the recompiler's two lowering shapes:
    ///
    /// * **Single forward `if`** — the branch's `taken` (skip) target IS the merge, so
    ///   the taken arm is empty and only the `fall` body runs under the not-taken lanes.
    /// * **If-else diamond** — `taken`/`fall` are two arm bodies that each flow to a
    ///   distinct merge; each runs under its lane mask, so every EXEC-gated write/export
    ///   in an arm affects only that arm's lanes. At the merge the wave reconverges with
    ///   EXEC restored; a VGPR written by both arms holds each lane's arm value, exactly
    ///   the recompiler's last-writer-wins-per-lane over the same divergent EXEC.
    fn execute_cfg(&mut self, insts: &[Decoded], cfg: &crate::cfg::Cfg) -> Result<(), InterpError> {
        use crate::cfg::{BranchCond, Terminator};

        // Visit blocks by leader offset, following terminators. A structured, acyclic
        // CFG visits each block once (an arm region is run under `run_region`, not the
        // top-level walk); the cap turns a lowering bug into a clean error, not a hang.
        let visit_cap = cfg.blocks.len().saturating_mul(4).max(16);
        let mut visits = 0usize;

        let mut cur = cfg.blocks.first().map(|b| b.start);
        while let Some(start) = cur {
            visits += 1;
            if visits > visit_cap {
                return Err(InterpError::UnsupportedInst {
                    inst: Box::new(Inst::Sopp {
                        op: opcodes::sopp::S_BRANCH,
                        simm16: 0,
                    }),
                    offset: start as u32,
                });
            }

            let bi = cfg
                .block_index_at(start)
                .expect("current leader is a real block");
            let block = &cfg.blocks[bi];

            // Run the entry/merge block's own body under the current (whole) EXEC.
            if self.run_block_body(insts, block)? {
                break; // s_endpgm
            }

            cur = match &block.terminator {
                Terminator::Return => None,
                Terminator::Fallthrough { target } | Terminator::Branch { target } => Some(*target),
                Terminator::Cond { cond, taken, fall } => {
                    // A back-edge `Cond` (taken arm targets a block at or before this
                    // one) closes a natural loop: run the loop body under EXEC, dropping
                    // lanes as they fail the continue test, then continue to the merge.
                    // Recognized loops are the only back-edges in the CFG (build_cfg
                    // validated the shape), so this is always a well-formed loop.
                    if let Some(li) = cfg.loop_of(bi) {
                        if self.run_loop(insts, cfg, li, visit_cap)? {
                            break; // s_endpgm inside the loop body ended the wave
                        }
                        cur = Some(li.merge);
                        continue;
                    }
                    // Whole-wave branch predicate over the CURRENT exec mask.
                    let taken_whole = match cond {
                        BranchCond::Vccz => (self.st.vcc & self.st.exec) == 0,
                        BranchCond::Vccnz => (self.st.vcc & self.st.exec) != 0,
                        BranchCond::Execz => self.st.exec == 0,
                        BranchCond::Execnz => self.st.exec != 0,
                    };
                    // Per-lane split of the current EXEC. A lane goes to the `taken` arm
                    // when it satisfies the branch condition (vccz: VCC bit 0; vccnz: VCC
                    // bit 1); to the `fall` arm otherwise. EXEC-based branches are
                    // whole-wave (no per-lane divergence per invocation).
                    let saved = self.st.exec;
                    let taken_mask = match cond {
                        BranchCond::Vccz => saved & !self.st.vcc, // take when vcc bit clear
                        BranchCond::Vccnz => saved & self.st.vcc, // take when vcc bit set
                        BranchCond::Execz | BranchCond::Execnz => {
                            if taken_whole {
                                saved
                            } else {
                                0
                            }
                        }
                    };
                    let fall_mask = saved & !taken_mask;

                    if *fall == usize::MAX {
                        // Degenerate: conditional branch with no fall-through. Treat as
                        // an unconditional jump to the target.
                        Some(*taken)
                    } else {
                        // The reconvergence point both arms flow into. Computed from the
                        // shared CFG so the interp and the recompiler agree on the merge.
                        let merge =
                            cfg.merge_target(bi)
                                .ok_or_else(|| InterpError::UnsupportedInst {
                                    inst: Box::new(Inst::Sopp {
                                        op: opcodes::sopp::S_BRANCH,
                                        simm16: 0,
                                    }),
                                    offset: block.start as u32,
                                })?;

                        // Run the taken arm, then the fall arm, each under its lane mask,
                        // each stopping when it reaches the merge. An empty arm (single
                        // `if`: taken == merge) runs no blocks. Every write in an arm is
                        // EXEC-gated, so narrowing disables the other arm's lanes.
                        if *taken != merge && taken_mask != 0 {
                            self.st.exec = taken_mask;
                            if self.run_region(insts, cfg, *taken, merge, visit_cap)? {
                                break; // s_endpgm inside the arm ends the wave
                            }
                        }
                        if *fall != merge && fall_mask != 0 {
                            self.st.exec = fall_mask;
                            if self.run_region(insts, cfg, *fall, merge, visit_cap)? {
                                break;
                            }
                        }

                        // Reconverge: restore the full wave EXEC at the merge.
                        self.st.exec = saved;
                        Some(merge)
                    }
                }
            };
        }
        Ok(())
    }

    /// Run one block's straight-line body (dataflow ops; the CFG branch terminator is
    /// lowered from `block.terminator`, not executed). Returns `true` if an `s_endpgm`
    /// in the body ended the wave.
    fn run_block_body(
        &mut self,
        insts: &[Decoded],
        block: &crate::cfg::BasicBlock,
    ) -> Result<bool, InterpError> {
        for &idx in &block.insts {
            let d = &insts[idx];
            self.st.pc = d.offset_dwords;
            match &d.inst {
                Inst::Sopp { op, .. } if *op == opcodes::sopp::S_ENDPGM => return Ok(true),
                Inst::Sopp { op, .. } if *op == opcodes::sopp::S_WAITCNT => {}
                Inst::Sopp { op, .. } if *op == opcodes::sopp::S_NOP => {}
                // The block's control-flow branch terminator is lowered by the walk, not
                // executed as a dataflow op.
                _ if is_cfg_branch(d) => {}
                _ => self.exec_one(d)?,
            }
        }
        Ok(false)
    }

    /// Run a straight-line arm region under the CURRENT (already-narrowed) EXEC: execute
    /// blocks from `start`, following `Fallthrough`/`Branch` terminators, stopping when
    /// the next block would be `stop` (the merge) — the merge is run by the caller after
    /// reconvergence. Returns `true` if an `s_endpgm` ended the wave.
    ///
    /// This slice's arms are straight-line (no nested `Cond`): a `Cond` or `Return`
    /// terminator inside an arm is outside the structured subset and surfaces as an
    /// `UnsupportedInst` so the oracle defers in lockstep with the recompiler.
    fn run_region(
        &mut self,
        insts: &[Decoded],
        cfg: &crate::cfg::Cfg,
        start: usize,
        stop: usize,
        visit_cap: usize,
    ) -> Result<bool, InterpError> {
        use crate::cfg::Terminator;
        let mut cur = start;
        let mut steps = 0usize;
        loop {
            if cur == stop {
                return Ok(false);
            }
            steps += 1;
            if steps > visit_cap {
                return Err(InterpError::UnsupportedInst {
                    inst: Box::new(Inst::Sopp {
                        op: opcodes::sopp::S_BRANCH,
                        simm16: 0,
                    }),
                    offset: cur as u32,
                });
            }
            let bi = cfg.block_index_at(cur).expect("arm leader is a real block");
            let block = &cfg.blocks[bi];
            if self.run_block_body(insts, block)? {
                return Ok(true);
            }
            match &block.terminator {
                Terminator::Fallthrough { target } | Terminator::Branch { target } => {
                    cur = *target;
                }
                // A nested branch or a bare return inside an arm is not in the structured
                // forward-`if`/diamond subset — defer cleanly (lockstep with recompile).
                Terminator::Cond { .. } | Terminator::Return => {
                    return Err(InterpError::UnsupportedInst {
                        inst: Box::new(Inst::Sopp {
                            op: opcodes::sopp::S_BRANCH,
                            simm16: 0,
                        }),
                        offset: block.start as u32,
                    });
                }
            }
        }
    }

    /// Absolute safety cap on loop iterations, per loop. A real corpus loop has a tiny
    /// trip count (the loops-slice corpus runs 4); this bound comfortably exceeds any
    /// bounded loop while making a mis-structured / never-terminating loop fail cleanly
    /// with an [`InterpError::UnsupportedInst`] instead of hanging the oracle (and the
    /// test suite). It is separate from the acyclic `visit_cap`: a loop legitimately
    /// re-visits its header, so the acyclic cap cannot bound it.
    const LOOP_ITER_CAP: usize = 1 << 16;

    /// Execute a recognized natural loop. On entry the loop header's body has ALREADY
    /// run once (the top-level walk ran `run_block_body` on the header before reaching
    /// its back-edge `Cond` terminator), so the first thing to evaluate is the first
    /// iteration's continue condition.
    ///
    /// Semantics: each iteration runs the loop body (`[header, back_edge_block]`) under
    /// the current EXEC; a lane drops out of EXEC when it fails the continue test (for
    /// `vccnz`: a lane continues while its VCC bit is set). The loop exits when no lane
    /// still continues (EXEC ∩ continue == 0). The outer EXEC is saved on entry and
    /// restored at the merge so the wave reconverges. For a UNIFORM loop every live
    /// lane drops together, so this reduces to "run the body N times under the full
    /// mask". Returns `true` if an `s_endpgm` in the body ended the wave.
    fn run_loop(
        &mut self,
        insts: &[Decoded],
        cfg: &crate::cfg::Cfg,
        li: crate::cfg::LoopInfo,
        visit_cap: usize,
    ) -> Result<bool, InterpError> {
        use crate::cfg::BranchCond;

        let saved_exec = self.st.exec;
        let mut iters = 0usize;
        loop {
            // The continue mask: which currently-live lanes take the back-edge. For an
            // EXEC-based test the whole (narrowed) wave continues-or-not together.
            let continue_mask = match li.cond {
                BranchCond::Vccnz => self.st.exec & self.st.vcc,
                BranchCond::Vccz => self.st.exec & !self.st.vcc,
                BranchCond::Execnz => self.st.exec, // exec != 0 ⇒ all live lanes continue
                BranchCond::Execz => 0,             // exec != 0 here ⇒ never continues
            };
            if continue_mask == 0 {
                // No lane loops back: reconverge and exit to the merge.
                self.st.exec = saved_exec;
                return Ok(false);
            }

            iters += 1;
            if iters > Self::LOOP_ITER_CAP {
                self.st.exec = saved_exec;
                return Err(InterpError::UnsupportedInst {
                    inst: Box::new(Inst::Sopp {
                        op: opcodes::sopp::S_BRANCH,
                        simm16: 0,
                    }),
                    offset: li.back_edge_block as u32,
                });
            }

            // Narrow EXEC to the continuing lanes and re-run the loop body one iteration.
            self.st.exec = continue_mask;
            if self.run_loop_body(insts, cfg, li, visit_cap)? {
                self.st.exec = saved_exec;
                return Ok(true); // s_endpgm inside the body
            }
        }
    }

    /// Run one iteration of a loop body: the blocks `[header, back_edge_block]` under the
    /// current (already-narrowed) EXEC. The back-edge block's own `Cond` terminator is
    /// the loop test — it is NOT followed here (the caller re-evaluates it); all other
    /// terminators in the body must be straight-line (`Fallthrough`/`Branch`) or the loop
    /// body carries an unsupported nested branch and defers cleanly. Returns `true` on
    /// `s_endpgm`.
    fn run_loop_body(
        &mut self,
        insts: &[Decoded],
        cfg: &crate::cfg::Cfg,
        li: crate::cfg::LoopInfo,
        visit_cap: usize,
    ) -> Result<bool, InterpError> {
        use crate::cfg::Terminator;
        let mut cur = li.header;
        let mut steps = 0usize;
        loop {
            steps += 1;
            if steps > visit_cap {
                return Err(InterpError::UnsupportedInst {
                    inst: Box::new(Inst::Sopp {
                        op: opcodes::sopp::S_BRANCH,
                        simm16: 0,
                    }),
                    offset: cur as u32,
                });
            }
            let bi = cfg
                .block_index_at(cur)
                .expect("loop-body leader is a real block");
            let block = &cfg.blocks[bi];
            if self.run_block_body(insts, block)? {
                return Ok(true);
            }
            if cur == li.back_edge_block {
                // Reached the back-edge test; the caller re-evaluates the continue mask.
                return Ok(false);
            }
            match &block.terminator {
                Terminator::Fallthrough { target } | Terminator::Branch { target } => {
                    cur = *target;
                }
                // A nested branch or a bare return inside the loop body is outside the
                // structured single-back-edge/single-exit subset — defer (build_cfg
                // should already have rejected it, but stay total).
                Terminator::Cond { .. } | Terminator::Return => {
                    return Err(InterpError::UnsupportedInst {
                        inst: Box::new(Inst::Sopp {
                            op: opcodes::sopp::S_BRANCH,
                            simm16: 0,
                        }),
                        offset: block.start as u32,
                    });
                }
            }
        }
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
                clamp,
            } => self.exec_vop3(
                *op, *vdst, *src0, *src1, *src2, *abs, *neg, *omod, *clamp, off,
            ),
            Inst::Vopc { op, src0, vsrc1 } => self.exec_vopc(*op, *src0, *vsrc1, off),
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
            Inst::Exp {
                target,
                srcs,
                compr,
                ..
            } => self.exec_exp(*target, srcs, *compr, off),
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
            // s_mov_b64 / s_wqm_b64: 64-bit moves used to save EXEC before a
            // whole-quad-mode region and restore it after. We model a wave that is
            // fully covered, so s_wqm (which expands EXEC to complete quads for helper
            // lanes) is the identity here — both are a plain 64-bit copy. The oracle
            // still moves the real bits so a later restore reproduces the saved EXEC.
            opcodes::sop1::S_MOV_B64 | opcodes::sop1::S_WQM_B64 => {
                let v = self.read_scalar_pair(ssrc0, off)?;
                self.write_scalar_pair(sdst, v, off)
            }
            _ => Err(self.unsupported_sop1(op, sdst, ssrc0, off)),
        }
    }

    /// Read a 64-bit scalar operand (a register PAIR or a 64-bit special register).
    fn read_scalar_pair(&self, op: Operand, off: u32) -> Result<u64, InterpError> {
        match op {
            Operand::Sgpr(n) => self.read_sgpr_u64(n as usize, off),
            Operand::Special(SpecialReg::ExecLo) => Ok(self.st.exec),
            Operand::Special(SpecialReg::VccLo) => Ok(self.st.vcc),
            other => Err(InterpError::InvalidOperand {
                operand: other,
                offset: off,
                reason: "not a 64-bit scalar source",
            }),
        }
    }

    /// Write a 64-bit scalar destination (a register PAIR or a 64-bit special register).
    fn write_scalar_pair(&mut self, sdst: Operand, v: u64, off: u32) -> Result<(), InterpError> {
        match sdst {
            Operand::Sgpr(n) => {
                self.set_sgpr(n as usize, v as u32, off)?;
                self.set_sgpr(n as usize + 1, (v >> 32) as u32, off)
            }
            Operand::Special(SpecialReg::ExecLo) => {
                self.st.exec = v;
                Ok(())
            }
            Operand::Special(SpecialReg::VccLo) => {
                self.st.vcc = v;
                Ok(())
            }
            other => Err(InterpError::InvalidOperand {
                operand: other,
                offset: off,
                reason: "not a 64-bit scalar destination",
            }),
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
            // The universal Orbis shader prologue is `s_mov_b32 vcc_hi, <imm>` — every
            // retail `.sb` opens with it (RE'd from Celeste; see recompile's emit_sop1
            // note). It stashes a constant into VCC that these shaders never read back,
            // so a faithful 32-bit write into the VCC half keeps the oracle exact and
            // mirrors `read_scalar` (which already reads vcc_lo/vcc_hi). The recompiler
            // validates-and-discards the same write; both agree because VCC never feeds
            // an export in this subset.
            Operand::Special(SpecialReg::VccLo) => {
                self.st.vcc = (self.st.vcc & 0xFFFF_FFFF_0000_0000) | u64::from(v);
                Ok(())
            }
            Operand::Special(SpecialReg::VccHi) => {
                self.st.vcc = (self.st.vcc & 0x0000_0000_FFFF_FFFF) | (u64::from(v) << 32);
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
            V_MOV_B32
                | V_CVT_F32_I32
                | V_CVT_F32_U32
                | V_CVT_U32_F32
                | V_CVT_I32_F32
                | V_CVT_OFF_F32_I4
                | V_FRACT_F32
                | V_CEIL_F32
                | V_FLOOR_F32
                | V_SQRT_F32
                | V_SIN_F32
                | V_RCP_F32
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
                V_CVT_I32_F32 => (self.read_f32_lane(src0, lane, off)? as i32) as u32,
                // v_cvt_off_f32_i4: CI-ISA V_CVT_OFF_F32_I4 (VOP1 opcode 0xE) —
                // "4-bit signed int to 32-bit float. For interpolation in shader." Its
                // result table (S0 1000 → -0.5f, 0000 → 0.0f, 0001 → 0.0625f, 0111 →
                // 0.4375f, 1111 → -0.0625f) is exactly `sext4(S0[3:0]) / 16.0`. Sign-extend
                // via a shift pair so 0x8→−8. Divisor 16 is an exact power of two, so this
                // is bit-exact and matches the recompiler's OpBitFieldSExtract → convert →
                // ×0.0625.
                V_CVT_OFF_F32_I4 => {
                    let i4 = (((self.read_src_lane(src0, lane, off)? & 0xF) as i32) << 28) >> 28;
                    (i4 as f32 / 16.0).to_bits()
                }
                // Transcendentals. CI-ISA V_SQRT_F32 is documented "< 1 ulp error" (an
                // approximate macro op on real hardware); we model the correctly-rounded
                // IEEE result so the oracle matches the recompiler's portable GLSL Sqrt
                // (both correctly rounded). CI-ISA V_FRACT_F32 (VOP1 opcode 0x20):
                // "D.f = S0.f - floor(S0.f)", clamped to [0,1) here (see fract_f32).
                V_FRACT_F32 => {
                    let x = self.read_f32_lane(src0, lane, off)?;
                    fract_f32(x).to_bits()
                }
                V_FLOOR_F32 => self.read_f32_lane(src0, lane, off)?.floor().to_bits(),
                V_CEIL_F32 => self.read_f32_lane(src0, lane, off)?.ceil().to_bits(),
                // CI-ISA V_SIN_F32 (VOP1 opcode 0x35): "Input must be normalized from
                // radians by dividing by 2*PI", i.e. the argument is in revolutions, so
                // D = sin(2*PI*S0). The recompiler emits GLSL Sin(TAU*x) with the same f32
                // TAU. NOTE: GLSL
                // Sin is only ULP-bounded (implementation-defined), not correctly
                // rounded, so the oracle (host libm sinf) and the GPU agree only to the
                // driver's Sin ULP budget, NOT bit-for-bit — same transcendental-class
                // gap as any GPU sine. The corpus tests only the exact quarter-turn.
                V_SIN_F32 => {
                    let x = self.read_f32_lane(src0, lane, off)?;
                    (x * std::f32::consts::TAU).sin().to_bits()
                }
                // CI-ISA V_RCP_F32: "Reciprocal, < 1 ulp error" (an approximate macro on
                // hardware); we model the exact 1.0/x (correctly rounded) so the oracle
                // matches the recompiler's portable OpFDiv 1.0/x (both correctly rounded)
                // bit-for-bit.
                V_RCP_F32 => (1.0f32 / self.read_f32_lane(src0, lane, off)?).to_bits(),
                // The only remaining supported op after the guard above.
                _ => self.read_f32_lane(src0, lane, off)?.sqrt().to_bits(),
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
            V_ADD_F32
                | V_SUB_F32
                | V_SUBREV_F32
                | V_MUL_F32
                | V_MAC_F32
                | V_MADMK_F32
                | V_MADAK_F32
                | V_MIN_F32
                | V_MAX_F32
                | V_LSHRREV_B32
                | V_LSHLREV_B32
                | V_AND_B32
                | V_ADD_I32
                | V_CNDMASK_B32
                | V_CVT_PKRTZ_F16_F32
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
                // v_add_i32 writes a per-lane carry to VCC; an EXEC-disabled lane stores
                // 0 (not a stale bit), matching the hardware's EXEC-gated write. Other
                // vop2 ops here never touch VCC, so leave it alone for them.
                if op == V_ADD_I32 {
                    self.write_predicate_bit(
                        Operand::Special(SpecialReg::VccLo),
                        lane,
                        false,
                        off,
                    )?;
                }
                continue;
            }
            // Integer/bitwise ops read and write raw bits (no float reinterpretation).
            let out_bits = match op {
                // CI-ISA V_LSHLREV_B32: "D.u = S1.u << S0.u[4:0]" (reversed operands; shift
                // is src0, value is vsrc1). The `[4:0]` masks the shift to the low 5 bits.
                V_LSHLREV_B32 => {
                    let shift = self.read_src_lane(src0, lane, off)? & 0x1f;
                    let val = self.read_src_lane(vsrc1, lane, off)?;
                    val << shift
                }
                // CI-ISA V_LSHRREV_B32: "D.u = S1.u >> S0.u[4:0]" (unsigned/logical shift;
                // reversed operands).
                V_LSHRREV_B32 => {
                    let shift = self.read_src_lane(src0, lane, off)? & 0x1f;
                    let val = self.read_src_lane(vsrc1, lane, off)?;
                    val >> shift
                }
                // CI-ISA V_AND_B32: "Logical bit-wise AND", D = S0 & S1.
                V_AND_B32 => {
                    let a = self.read_src_lane(src0, lane, off)?;
                    let b = self.read_src_lane(vsrc1, lane, off)?;
                    a & b
                }
                // CI-ISA V_ADD_I32 (VOP2 opcode 0x25): "D.u = S0.u + S1.u; VCC=carry-out".
                // The result is a plain 32-bit wrapping add; the unsigned overflow bit is
                // written to VCC per-lane (only matters if a later op reads it).
                V_ADD_I32 => {
                    let a = self.read_src_lane(src0, lane, off)?;
                    let b = self.read_src_lane(vsrc1, lane, off)?;
                    let (sum, carry) = a.overflowing_add(b);
                    self.write_predicate_bit(
                        Operand::Special(SpecialReg::VccLo),
                        lane,
                        carry,
                        off,
                    )?;
                    sum
                }
                // CI-ISA V_CNDMASK_B32 (VOP2 opcode 0x0): "D.u = VCC[i] ? S1.u : S0.u
                // (i = threadID in wave)" — a per-lane select on the predicate. Reads back
                // the bool a prior VOPC / v_add_i32 wrote to VCC.
                V_CNDMASK_B32 => {
                    let s0 = self.read_src_lane(src0, lane, off)?;
                    let s1 = self.read_src_lane(vsrc1, lane, off)?;
                    let pred =
                        self.read_predicate_bit(Operand::Special(SpecialReg::VccLo), lane, off)?;
                    if pred { s1 } else { s0 }
                }
                // CI-ISA V_CVT_PKRTZ_F16_F32 (VOP2 opcode 0x2F): "D = {flt32_to_flt16(S1.f),
                // flt32_to_flt16(S0.f)}, with round-toward-zero" — so D[15:0] = f16(S0),
                // D[31:16] = f16(S1). The ISA rounds toward zero (PKRTZ); we model it as
                // round-to-nearest-even to match the recompiler's portable GLSL PackHalf2x16
                // (portable SPIR-V has no cheap RTZ f16 without the float16 capability).
                // The two sides therefore agree bit-for-bit; the ≤1-f16-ULP deviation
                // from true hardware RTZ is invisible through an 8-bit render target.
                V_CVT_PKRTZ_F16_F32 => {
                    let lo = half::f16::from_f32(self.read_f32_lane(src0, lane, off)?).to_bits();
                    let hi = half::f16::from_f32(self.read_f32_lane(vsrc1, lane, off)?).to_bits();
                    (u32::from(hi) << 16) | u32::from(lo)
                }
                // The uniformly-f32 VOP2 ops (add/sub/mul/min/max/mac/madmk/madak):
                // shared with the recompiler via the uop layer so the ALU semantics
                // are written once. `Val` here is raw f32 bits (reinterpreted per op).
                _ => {
                    let a = self.read_f32_lane(src0, lane, off)?.to_bits();
                    let b = self.read_f32_lane(vsrc1, lane, off)?.to_bits();
                    // dst_old is only read by v_mac (the accumulator); harmless otherwise.
                    let dst_old = self.read_f32_lane(vdst, lane, off)?.to_bits();
                    crate::uop::eval_vop2(&mut InterpAlu, op, a, b, dst_old, k)
                }
            };
            self.write_vgpr(vdst, lane, out_bits, off)?;
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
        clamp: bool,
        off: u32,
    ) -> Result<(), InterpError> {
        // Reject an unsupported op before touching any lane (no partial wave state).
        use opcodes::vop3::*;
        if !matches!(
            op,
            V_MUL_F32
                | V_MAC_F32
                | V_MAD_F32
                | V_FMA_F32
                | V_MED3_F32
                | V_FRACT_F32
                | V_MAD_U32_U24
                | V_CVT_PKRTZ_F16_F32
                | V_CMP_LT_F32
                | V_CMP_EQ_F32
                | V_CMP_LE_F32
                | V_CMP_GT_F32
                | V_CMP_GE_F32
                | V_CNDMASK_B32
        ) {
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
                    clamp,
                }),
                offset: off,
            });
        }
        // VOP3-form VOPC: an f32 compare whose per-lane bool lands in the ARBITRARY
        // SGPR-pair destination the decoder placed in `vdst` (the `sdst` field), rather
        // than the implicit VCC of the standalone VOPC. abs/neg fold into the inputs.
        if matches!(
            op,
            V_CMP_LT_F32 | V_CMP_EQ_F32 | V_CMP_LE_F32 | V_CMP_GT_F32 | V_CMP_GE_F32
        ) {
            let cmp = match op {
                V_CMP_LT_F32 => opcodes::vopc::V_CMP_LT_F32,
                V_CMP_EQ_F32 => opcodes::vopc::V_CMP_EQ_F32,
                V_CMP_LE_F32 => opcodes::vopc::V_CMP_LE_F32,
                V_CMP_GE_F32 => opcodes::vopc::V_CMP_GE_F32,
                _ => opcodes::vopc::V_CMP_GT_F32,
            };
            for lane in 0..WAVE_SIZE {
                // An EXEC-disabled lane stores 0 into the compare's predicate
                // destination (not a stale bit), matching the EXEC-gated write.
                if !self.st.lane_live(lane) {
                    self.write_predicate_bit(vdst, lane, false, off)?;
                    continue;
                }
                let a = self.apply_mods(self.read_f32_lane(src0, lane, off)?, abs, neg, 0);
                let b = self.apply_mods(self.read_f32_lane(src1, lane, off)?, abs, neg, 1);
                let r = Self::eval_f32_compare(cmp, a, b, off)?;
                self.write_predicate_bit(vdst, lane, r, off)?;
            }
            return Ok(());
        }
        // v_cndmask_b32 (VOP3 form): CI-ISA V_CNDMASK_B32 notes "VOP3: specify VCC as a
        // scalar GPR in S2", so D = src2[lane] ? S1 : S0 with the predicate the arbitrary
        // SGPR pair (or VCC) named by src2, read per-lane; the sources are
        // raw bits (no float reinterpretation), so run on its own lane loop.
        if op == V_CNDMASK_B32 {
            for lane in 0..WAVE_SIZE {
                if !self.st.lane_live(lane) {
                    continue;
                }
                let s0 = self.read_src_lane(src0, lane, off)?;
                let s1 = self.read_src_lane(src1, lane, off)?;
                let pred = self.read_predicate_bit(src2, lane, off)?;
                self.write_vgpr(vdst, lane, if pred { s1 } else { s0 }, off)?;
            }
            return Ok(());
        }
        // CI-ISA V_MAD_U32_U24: "24 bit unsigned integer muladd. Src a and b treated as 24
        // bit unsigned integers. Src c treated as 32 bit signed or unsigned integer. Bits
        // [31:24] ignored. The result represents the low-order 32 bits of the multiply add
        // result." So it reads raw bits (not f32) — a & 0xFFFFFF, b & 0xFFFFFF, then +c
        // (32-bit wrapping); the float abs/neg/omod modifiers do not apply. Nor does clamp:
        // GFX7 has no integer
        // clamping at all (llvm-mc -mcpu=bonaire rejects `v_mad_u32_u24 ... clamp` with
        // "integer clamping is not supported on this GPU"), so the bit cannot legally be
        // set here. Handle it on its own lane loop.
        if op == V_MAD_U32_U24 {
            for lane in 0..WAVE_SIZE {
                if !self.st.lane_live(lane) {
                    continue;
                }
                let a = self.read_src_lane(src0, lane, off)? & 0x00FF_FFFF;
                let b = self.read_src_lane(src1, lane, off)? & 0x00FF_FFFF;
                let c = self.read_src_lane(src2, lane, off)?;
                let out = a.wrapping_mul(b).wrapping_add(c);
                self.write_vgpr(vdst, lane, out, off)?;
            }
            return Ok(());
        }
        // v_cvt_pkrtz_f16_f32 (VOP3 form) packs two f32 → f16 pair into a u32. Same RNE
        // f16 modeling as the VOP2 form (see exec_vop2); abs/neg fold into the inputs,
        // but the corpus uses none. Produces bits, so it runs on its own lane loop.
        if op == V_CVT_PKRTZ_F16_F32 {
            for lane in 0..WAVE_SIZE {
                if !self.st.lane_live(lane) {
                    continue;
                }
                let a = self.apply_mods(self.read_f32_lane(src0, lane, off)?, abs, neg, 0);
                let b = self.apply_mods(self.read_f32_lane(src1, lane, off)?, abs, neg, 1);
                let lo = half::f16::from_f32(a).to_bits();
                let hi = half::f16::from_f32(b).to_bits();
                self.write_vgpr(vdst, lane, (u32::from(hi) << 16) | u32::from(lo), off)?;
            }
            return Ok(());
        }
        for lane in 0..WAVE_SIZE {
            if !self.st.lane_live(lane) {
                continue;
            }
            // The uniformly-f32 VOP3 ops (mul/mac/mad/fma/med3/fract): shared with the
            // recompiler via the uop layer (abs/neg/omod/clamp modifiers included). `Val` is
            // raw f32 bits; the shared `apply_mods`/`apply_omod` reinterpret per op.
            let a = crate::uop::apply_mods(
                &mut InterpAlu,
                self.read_f32_lane(src0, lane, off)?.to_bits(),
                abs,
                neg,
                0,
            );
            let b = crate::uop::apply_mods(
                &mut InterpAlu,
                self.read_f32_lane(src1, lane, off)?.to_bits(),
                abs,
                neg,
                1,
            );
            let c = crate::uop::apply_mods(
                &mut InterpAlu,
                self.read_f32_lane(src2, lane, off)?.to_bits(),
                abs,
                neg,
                2,
            );
            // dst_old is only read by v_mac (the accumulator); harmless otherwise.
            let dst_old = self.read_f32_lane(vdst, lane, off)?.to_bits();
            let raw = crate::uop::eval_vop3(&mut InterpAlu, op, a, b, c, dst_old);
            // Output-modifier chain in CI-ISA order: the VOP3a OMOD field is "applied
            // before clamping", so omod scales first, THEN clamp saturates to [0.0, 1.0].
            let scaled = crate::uop::apply_omod(&mut InterpAlu, raw, omod);
            let out_bits = crate::uop::apply_clamp(&mut InterpAlu, scaled, clamp);
            self.write_vgpr(vdst, lane, out_bits, off)?;
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

    // ---- predication / VCC family -------------------------------------------
    //
    // A VOPC compare and a v_add_i32 carry both produce a per-lane bool that lands in
    // a 64-bit predicate mask: VCC (the standalone destination) or an arbitrary SGPR
    // pair (the VOP3-form `sdst`). We model the mask faithfully — one bit per lane,
    // exactly as the hardware — over `st.vcc` (for VCC) or the SGPR pair `[n:n+1]`.
    // A later `v_cndmask` reads that same bit back to select its per-lane source.

    /// Set/clear lane `lane`'s bit in the predicate destination `dst` (VCC or an SGPR
    /// pair). Only the named bit is touched, so accumulating a whole-wave mask across
    /// the lane loop is correct.
    fn write_predicate_bit(
        &mut self,
        dst: Operand,
        lane: usize,
        set: bool,
        off: u32,
    ) -> Result<(), InterpError> {
        match dst {
            Operand::Special(SpecialReg::VccLo | SpecialReg::VccHi) => {
                let bit = 1u64 << lane;
                if set {
                    self.st.vcc |= bit;
                } else {
                    self.st.vcc &= !bit;
                }
                Ok(())
            }
            // An SGPR pair `[n:n+1]` holds the 64-bit mask: lane 0..31 in `s[n]`,
            // 32..63 in `s[n+1]`. Bounds-check both halves (write the touched half).
            Operand::Sgpr(n) => {
                let (reg, shift) = if lane < 32 {
                    (n as usize, lane)
                } else {
                    (n as usize + 1, lane - 32)
                };
                let cur = self.sgpr(reg, off)?;
                let bit = 1u32 << shift;
                let new = if set { cur | bit } else { cur & !bit };
                self.set_sgpr(reg, new, off)
            }
            other => Err(InterpError::InvalidOperand {
                operand: other,
                offset: off,
                reason: "not a predicate (VCC / SGPR-pair) destination",
            }),
        }
    }

    /// Read lane `lane`'s bit from the predicate source `src` (VCC or an SGPR pair) —
    /// the per-lane selector `v_cndmask` consumes. Mirrors [`write_predicate_bit`].
    fn read_predicate_bit(&self, src: Operand, lane: usize, off: u32) -> Result<bool, InterpError> {
        match src {
            Operand::Special(SpecialReg::VccLo | SpecialReg::VccHi) => {
                Ok(self.st.vcc & (1u64 << lane) != 0)
            }
            Operand::Sgpr(n) => {
                let (reg, shift) = if lane < 32 {
                    (n as usize, lane)
                } else {
                    (n as usize + 1, lane - 32)
                };
                Ok(self.sgpr(reg, off)? & (1u32 << shift) != 0)
            }
            other => Err(InterpError::InvalidOperand {
                operand: other,
                offset: off,
                reason: "not a predicate (VCC / SGPR-pair) source",
            }),
        }
    }

    /// Evaluate an f32 compare `op` for a lane's two source values → bool. Only the
    /// ordered f32 compares the retail set reaches are modeled; an unmodeled op faults.
    fn eval_f32_compare(op: u8, a: f32, b: f32, off: u32) -> Result<bool, InterpError> {
        use opcodes::vopc::*;
        Ok(match op {
            V_CMP_LT_F32 => a < b,
            V_CMP_EQ_F32 => a == b,
            V_CMP_LE_F32 => a <= b,
            V_CMP_GT_F32 => a > b,
            V_CMP_GE_F32 => a >= b,
            _ => {
                return Err(InterpError::UnsupportedInst {
                    inst: Box::new(Inst::Vopc {
                        op,
                        src0: Operand::Raw(0),
                        vsrc1: Operand::Raw(0),
                    }),
                    offset: off,
                });
            }
        })
    }

    /// VOPC (standalone): an f32 compare whose per-lane bool lands in VCC. The whole
    /// wave's mask is (re)built one lane at a time — a masked-off lane clears its bit
    /// so VCC reflects only live lanes (matching the hardware's EXEC-gated write).
    fn exec_vopc(
        &mut self,
        op: u8,
        src0: Operand,
        vsrc1: Operand,
        off: u32,
    ) -> Result<(), InterpError> {
        // Reject an unmodeled compare before touching VCC (no partial mask).
        Self::eval_f32_compare(op, 0.0, 0.0, off)?;
        for lane in 0..WAVE_SIZE {
            // A masked-off lane's VCC bit is cleared (the hardware's EXEC-gated write
            // stores 0), not left stale — otherwise a later re-enable + v_cndmask would
            // read a bit from a prior compare or the Orbis vcc_hi prologue constant.
            if !self.st.lane_live(lane) {
                self.write_predicate_bit(Operand::Special(SpecialReg::VccLo), lane, false, off)?;
                continue;
            }
            let a = self.read_f32_lane(src0, lane, off)?;
            let b = self.read_f32_lane(vsrc1, lane, off)?;
            let r = Self::eval_f32_compare(op, a, b, off)?;
            self.write_predicate_bit(Operand::Special(SpecialReg::VccLo), lane, r, off)?;
        }
        Ok(())
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
        // The base address resolves differently by load kind: an s_buffer_load's SBASE
        // names a 128-bit V# descriptor (base + stride + num_records), while an s_load's
        // SBASE is a plain 64-bit guest pointer pair.
        let base = if opcodes::smrd::is_buffer_load(op) {
            self.decode_v_sharp(sbase as usize, off)?.base
        } else {
            self.read_sgpr_u64(sbase as usize, off)?
        };
        // CI-ISA §13 SMRD encoding, OFFSET field: "IMM = 1: Specifies an 8-bit unsigned
        // Dword offset" — so the immediate offset is scaled by 4 to bytes; IMM = 0 is an
        // SGPR supplying a byte offset. The corpus uses the immediate form only.
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
        let dfmt = vsharp.dfmt;
        let nfmt = vsharp.nfmt;

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

        // `vaddr` is the index VGPR. CI-ISA §8.1.8: "off_vgpr = offset value from a VGPR
        // (located at VADDR or VADDR+1)" — so when BOTH idxen and offen are set the byte
        // offset comes from the NEXT VGPR (`vaddr + 1`), not a re-read of `vaddr` (which
        // would silently double-count). The corpus never sets `offen`, so this path is
        // unexercised but must be correct for a recompiler to mirror.
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
            // An index at or past `num_records` is out of bounds. CI-ISA §8.1.5.3 "Range
            // Checking" specifies that for `stride != 0`, an `index >= num_records` read
            // returns 0. This oracle instead CLAMPS the index to the last valid record
            // (num_records - 1) — a deliberate divergence from the CI-ISA return-0 rule; a
            // recompiler / diff harness must clamp the same way to agree with this oracle.
            // `num_records == 0` degenerates to index 0.
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
            // CI-ISA §8.1.5 linear buffer addressing: `base = const_base + sgpr_offset`,
            // `offset = (offen ? vgpr_offset : 0) + inst_offset`,
            // `buffer_offset = index * const_stride + offset`, `address = base +
            // buffer_offset` — i.e. base + index*stride + soffset + inst_offset + voffset.
            let addr = base
                .wrapping_add(index.wrapping_mul(stride))
                .wrapping_add(u64::from(soff))
                .wrapping_add(u64::from(offset))
                .wrapping_add(voffset);
            let bytes = self.load(addr, count as usize * 4, off)?;
            for i in 0..count as usize {
                // Unpack component `i` per the V#'s packed format (task-164), mirroring the
                // recompiler's `fetch_buffer_component` bit-for-bit. A 32-bit float / unmodeled
                // format stores the raw dword (the position/UV/atlas path — unchanged); a
                // packed `_8*`/`_16*` format extracts the component's byte/half and converts
                // per `nfmt`. The stored value is the RAW BITS of the produced f32 (the vgpr
                // holds bits; the exporter reads them back as f32) — for the raw path that is
                // the dword verbatim.
                // `dfmt` values are the CI-ISA §8 data-format enum (p8-25 table): 1 = 8,
                // 2 = 16, 3 = 8_8, 5 = 16_16, 10 = 8_8_8_8, 12 = 16_16_16_16.
                let stored = match dfmt {
                    // 8-bit family (8 / 8_8 / 8_8_8_8 = dfmt 1/3/10): all components in the
                    // element's first dword; component `i` is byte `i` of it.
                    1 | 3 | 10 => {
                        let dword0 = read_le_u32(&bytes, 0);
                        let byte = (dword0 >> (i * 8)) & 0xFF;
                        convert_packed_int(byte, 8, nfmt, 0.0).to_bits()
                    }
                    // 16-bit family (16 / 16_16 / 16_16_16_16 = dfmt 2/5/12): component `i`
                    // is the (i>>1)-th dword, half (i & 1).
                    2 | 5 | 12 => {
                        let dword = read_le_u32(&bytes, (i >> 1) * 4);
                        let sh = (i & 1) * 16;
                        let half_u = (dword >> sh) & 0xFFFF;
                        let float_val = half::f16::from_bits(half_u as u16).to_f32();
                        convert_packed_int(half_u, 16, nfmt, float_val).to_bits()
                    }
                    // 32-bit float / invalid / unmodeled: raw dword, bitcast to f32 downstream.
                    _ => read_le_u32(&bytes, i * 4),
                };
                self.set_vgpr(vdata0 as usize + i, lane, stored, off)?;
            }
        }
        Ok(())
    }

    /// Decode the 128-bit V# (buffer resource) from the four SGPRs beginning at `srsrc`.
    /// Shared by SMRD-fetched and MUBUF-referenced descriptors so the two agree on the
    /// layout. All four words are read and bounds-checked (`srsrc + 0..=3`): word3 carries
    /// the `dfmt`/`nfmt` the format-aware vertex fetch unpacks each element with (task-164;
    /// before that slice word3 was unused and not read). Field positions are the AMD Sea
    /// Islands buffer descriptor, CI-ISA §8.1.7 Table 8.5 "Buffer Resource Descriptor":
    ///
    /// - base address = bits[47:0]: word0 = base[31:0]; word1[15:0] = base[47:32].
    /// - stride = bits[61:48] = word1[29:16] ("Bytes 0 to 16383", 14 bits).
    /// - num_records = bits[95:64] = word2 ("in units of stride").
    /// - num format = bits[110:108] = word3[14:12]; data format = bits[114:111] = word3[18:15].
    fn decode_v_sharp(&self, srsrc: usize, off: u32) -> Result<VSharp, InterpError> {
        let w0 = u64::from(self.sgpr(srsrc, off)?);
        let w1 = self.sgpr(srsrc + 1, off)?;
        let w2 = self.sgpr(srsrc + 2, off)?;
        let w3 = self.sgpr(srsrc + 3, off)?;
        Ok(VSharp {
            base: w0 | (u64::from(w1 & 0xFFFF) << 32),
            stride: u64::from((w1 >> 16) & 0x3FFF),
            num_records: u64::from(w2),
            nfmt: ((w3 >> 12) & 0x7) as u8,
            dfmt: ((w3 >> 15) & 0xF) as u8,
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
        // CI-ISA V_INTERP_P1_F32 (VINTRP opcode 0x0): "The ATTR field indicates which
        // attribute (0-32) to interpolate. The ATTRCHAN field indicates which channel:
        // 0=x, 1=y, 2=z and 3=w." This interpreter takes attribute selection from those
        // VINTRP `attr`/`chan` fields, NOT `m0` (CI-ISA's `lds_param_offset`) — a
        // deliberate simplification; see the module-level VINTRP contract.
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
        // CI-ISA §8.2 DMASK: the texture unit sends the enabled components "starting with
        // R, then G, B, and A", packed contiguously — so the destination register index
        // advances only for enabled channels (dst[0] = first enabled, …).
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
    /// fields the linear-RGBA8 sampling subset needs are read + bounds-checked. Field
    /// positions are the AMD Sea Islands (= Liverpool/GCN2) image descriptor, CI-ISA
    /// §8.2.5 Table 8.11 "Image Resource Definition", which the driver writes as 256 bits
    /// (eight SGPRs):
    /// - base address = bits[39:0] (word0 = base[31:0]; word1[7:0] = base[39:32]),
    ///   "256-byte aligned", so the byte address is `field << 8` (occupying bits[47:8]).
    /// - data format = bits[57:52] = word1[25:20]; num format = bits[61:58] = word1[29:26].
    /// - width - 1 = bits[77:64] = word2[13:0]; height - 1 = bits[91:78] = word2[27:14].
    /// - tiling index = bits[120:116] = word3[24:20] (0 = linear; the subset only linear).
    /// - pitch - 1 = bits[154:141] = word4[26:13], "in texel units" (256-bit section).
    ///
    /// The recompiler mirrors nothing here — it resolves the sampled image symbolically —
    /// so only the interpreter reads these bytes.
    fn decode_t_sharp(&self, base: usize, off: u32) -> Result<TSharp, InterpError> {
        let w0 = self.sgpr(base, off)?;
        let w1 = self.sgpr(base + 1, off)?;
        let w2 = self.sgpr(base + 2, off)?;
        let w3 = self.sgpr(base + 3, off)?;
        // pitch - 1 = CI-ISA Table 8.11 bits[154:141] = word4[26:13], in texel units
        // (task-155). Only the linear-aligned sampling arm uses it — it must resolve the
        // SAME pitch the gnm upload path detiles with (`linear_aligned_pitch_or`), so
        // oracle == upload stays byte-for-byte.
        let w4 = self.sgpr(base + 4, off)?;
        // Bounds-check the rest of the 256-bit descriptor even though the subset reads only
        // the low five words now, so a T# whose implicit tail runs past the file faults
        // cleanly rather than silently truncating.
        for i in 5..8 {
            self.sgpr(base + i, off)?;
        }
        Ok(TSharp {
            base: (u64::from(w0) << 8) | (u64::from(w1 & 0xFF) << 40),
            width: ((w2 & 0x3FFF) + 1) as usize,
            height: (((w2 >> 14) & 0x3FFF) + 1) as usize,
            dfmt: ((w1 >> 20) & 0x3F) as u8,
            nfmt: ((w1 >> 26) & 0xF) as u8,
            tiling_index: ((w3 >> 20) & 0x1F) as u8,
            pitch: ((w4 >> 13) & 0x3FFF) + 1,
        })
    }

    /// Decode the 128-bit S# (sampler) from the four SGPRs at `base`. Only the mag-filter
    /// selector the subset honors is read; the rest of the sampler word is not consulted
    /// (no anisotropy/LOD/border in the subset). Field positions are the AMD Sea Islands
    /// sampler descriptor, CI-ISA §8.2.6 Table 8.12 "Sampler Resource Definition":
    /// `xy mag filter` = bits[85:84] = word2[21:20], `xy min filter` = bits[87:86] =
    /// word2[23:22] ("Magnification/Minification filter"); filter enum 0 = point.
    fn decode_s_sharp(&self, base: usize, off: u32) -> Result<SSharp, InterpError> {
        let w0 = self.sgpr(base, off)?;
        let w1 = self.sgpr(base + 1, off)?;
        let w2 = self.sgpr(base + 2, off)?;
        let w3 = self.sgpr(base + 3, off)?;
        let _ = (w0, w1, w3);
        // The subset keys point-vs-bilinear off word2[20], the low bit of the CI-ISA
        // 2-bit `xy mag filter` field (bits[85:84] = word2[21:20]): 0 = point, non-zero =
        // bilinear. Any non-point select is treated as bilinear.
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
            // The f32→i64 cast saturates, so an adversarial non-finite/huge UV (inf
            // from an upstream divide-by-zero, or garbage VGPR reinterpreted as a
            // large float) lands `x0`/`y0` at i64::MAX; `+ 1` would then overflow and
            // panic under overflow-checks. Saturate the neighbour index instead —
            // `texel` wraps it into [0, extent) via `rem_euclid` regardless.
            let (x1, y1) = (x0.saturating_add(1), y0.saturating_add(1));
            let c00 = self.texel(t, x0, y0, off)?;
            let c10 = self.texel(t, x1, y0, off)?;
            let c01 = self.texel(t, x0, y1, off)?;
            let c11 = self.texel(t, x1, y1, off)?;
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
        use ps4_core::tiling::{
            TileKind, linear_aligned_pitch_or, linear_aligned_texel_offset, thin1d_texel_offset,
            tile_kind,
        };
        // Repeat addressing: euclidean-mod into [0, extent). Both extents are non-zero
        // (checked in `sample_texture`).
        let w = t.width as i64;
        let h = t.height as i64;
        let xx = x.rem_euclid(w) as u32;
        let yy = y.rem_euclid(h) as u32;
        // Byte offset of the logical texel within the guest surface, per its tile mode.
        // Linear is row-major; linear-aligned is row-major over a padded pitch (task-153);
        // 1D-thin swizzles into 8×8 micro-tiles; 2D macro-tiling has no detiler (the GPU
        // path defers it too) so it faults rather than mis-reading. The upload path and
        // this oracle MUST use the same offset for each mode (task-98), which is why both
        // call the shared `ps4_core::tiling` helpers.
        let texel_off = match tile_kind(t.tiling_index) {
            TileKind::Linear => (yy as u64 * t.width as u64 + xx as u64) * 4,
            TileKind::LinearAligned => {
                let pitch = linear_aligned_pitch_or(t.width as u32, t.pitch);
                linear_aligned_texel_offset(xx, yy, pitch, 4) as u64
            }
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
        compr: bool,
        off: u32,
    ) -> Result<(), InterpError> {
        for lane in 0..WAVE_SIZE {
            if !self.st.lane_live(lane) {
                continue;
            }
            let mut values = [0.0f32; 4];
            if compr {
                // Compressed export: each of the first two sources holds TWO f16
                // channels packed into a 32-bit register (produced by
                // v_cvt_pkrtz_f16_f32). src0 → channels 0,1; src1 → channels 2,3.
                // We unpack back to f32 because the HLE render target is f32-typed
                // (the Vulkan pipeline converts to the real RT format), matching the
                // recompiler's GLSL UnpackHalf2x16 lowering.
                for (pair, slot) in srcs[..2].iter().enumerate() {
                    if let Some(src) = slot {
                        let packed = self.read_src_lane(*src, lane, off)?;
                        let lo = half::f16::from_bits(packed as u16).to_f32();
                        let hi = half::f16::from_bits((packed >> 16) as u16).to_f32();
                        values[pair * 2] = lo;
                        values[pair * 2 + 1] = hi;
                    }
                }
            } else {
                for (ch, slot) in srcs.iter().enumerate() {
                    if let Some(src) = slot {
                        values[ch] = self.read_f32_lane(*src, lane, off)?;
                    }
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
/// subset needs. Field positions are CI-ISA Table 8.11 "Image Resource Definition"
/// (see [`Interp::decode_t_sharp`]). `dfmt`/`nfmt` are carried but only the RGBA8
/// case is sampled.
struct TSharp {
    /// Guest byte address of the texel data (CI-ISA base field bits[39:0], `<< 8`).
    base: u64,
    /// Texel width (CI-ISA bits[77:64] = word2[13:0], stored as width - 1).
    width: usize,
    /// Texel height (CI-ISA bits[91:78] = word2[27:14], stored as height - 1).
    height: usize,
    /// Data format (`dfmt`, CI-ISA bits[57:52]) — the subset samples R8G8B8A8 only.
    #[allow(dead_code)]
    dfmt: u8,
    /// Number format (`nfmt`, CI-ISA bits[61:58]) — the subset samples UNORM only.
    #[allow(dead_code)]
    nfmt: u8,
    /// Tiling index (CI-ISA bits[120:116] = word3[24:20]); 0 = linear (the only mode
    /// the subset samples).
    #[allow(dead_code)]
    tiling_index: u8,
    /// Row pitch in texels (`word4[26:13] + 1`, task-155). Only the linear-aligned arm reads
    /// it; `0`/too-narrow falls back to the `align(width, 64)` heuristic, matching upload.
    pitch: u32,
}

/// A decoded 128-bit S# (sampler) — only the filter selector the subset honors.
struct SSharp {
    /// `true` = bilinear filtering, `false` = point/nearest.
    bilinear: bool,
}

/// A decoded 128-bit V# (buffer resource) — the fields the fetch path needs. Field
/// positions are CI-ISA Table 8.5 "Buffer Resource Descriptor" (see
/// [`Interp::decode_v_sharp`]).
struct VSharp {
    /// 48-bit base guest address (CI-ISA bits[47:0]).
    base: u64,
    /// Per-index stride in bytes (CI-ISA bits[61:48] = word1[29:16], 14 bits).
    stride: u64,
    /// Element count (CI-ISA bits[95:64] = word2); a fetch index at or past this is out of
    /// bounds (clamped — see the MUBUF fetch loop).
    num_records: u64,
    /// Data format (CI-ISA bits[114:111] = word3[18:15], 4 bits) — the packed component
    /// width/count the fetch unpacks each element with (task-164). `0` = Invalid → the
    /// raw-dword path.
    dfmt: u8,
    /// Num format (CI-ISA bits[110:108] = word3[14:12], 3 bits) — how each packed
    /// component's bits are interpreted (unorm/snorm/uint/sint/float).
    nfmt: u8,
}

/// Read a little-endian `u32` from `bytes` at byte offset `at`. The caller sizes
/// the slice, so the four indices are always in range.
fn read_le_u32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

/// Convert a packed unsigned integer component of `width` bits (8 or 16) into the f32 the
/// GCN format stage produces, per `nfmt`. The `nfmt` values are the CI-ISA §8 num-format
/// enum (p8-25 table): `0` unorm, `1` snorm, `4` uint, `5` sint, `7` float.
/// `raw_u` is the width-bit field already extracted (masked, right-aligned);
/// `float_val` is the pre-decoded half→f32 candidate used only when `nfmt == 7` (16-bit;
/// pass any value for 8-bit — a well-formed 8-bit V# is never a float format). An unmodeled
/// `nfmt` degrades to unorm.
///
/// This mirrors the recompiler's SPIR-V `convert_packed_component` BIT-FOR-BIT (task-164)
/// so the task-122 differential (interp == recompile) holds: `raw_u as f32` matches
/// `OpConvertUToF`, the two's-complement sign-extension + `as i32 as f32` matches
/// `OpISub`/`OpBitcast`/`OpConvertSToF`, and `.max(-1.0)` matches GLSL `FMax`.
fn convert_packed_int(raw_u: u32, width: u32, nfmt: u8, float_val: f32) -> f32 {
    let max_u = ((1u32 << width) - 1) as f32;
    let half_max = ((1u32 << (width - 1)) - 1) as f32;
    let sign_bit = 1u32 << (width - 1);
    let full = 1u32 << width;
    // Sign-extend to two's complement, exactly as the SPIR-V does in u32 then bitcasts to i32.
    let sub = if raw_u & sign_bit != 0 { full } else { 0 };
    let s = raw_u.wrapping_sub(sub) as i32;
    let uf = raw_u as f32;
    let sf = s as f32;
    match nfmt {
        1 => (sf / half_max).max(-1.0), // snorm
        4 => uf,                        // uint
        5 => sf,                        // sint
        7 => float_val,                 // float (half — 16-bit only)
        _ => uf / max_u,                // unorm (0) + fallback
    }
}

/// CI-ISA V_FRACT_F32 (VOP1 opcode 0x20): "D.f = S0.f - floor(S0.f)". Clamped here to the
/// `[0, 1)` range the hardware (and a conformant GLSL `Fract`) guarantees. For a small negative `x` the naive difference
/// rounds up to exactly `1.0` in f32 (e.g. `-1e-8 - floor(-1e-8)` = `1.0`); hardware
/// and most drivers return the largest float below 1.0 instead, so clamp to keep the
/// oracle from diverging from the recompiler's GLSL `Fract` by a full ULP-of-1. The
/// `>= 1.0` test preserves NaN (unlike `f32::min`, which would drop it).
fn fract_f32(x: f32) -> f32 {
    let f = x - x.floor();
    if f >= 1.0 {
        f32::from_bits(0x3f7f_ffff) // 0.999_999_94, the largest f32 < 1.0
    } else {
        f
    }
}

/// The interp's implementation of the shared [`crate::uop::AluBuilder`] value algebra
/// (task-131). `Val` is raw f32 bits (a lane's VGPR word), reinterpreted as f32 per
/// op with the EXACT `f32::from_bits` / `.to_bits` arithmetic the hand-written arms
/// used — so the shared per-opcode body computes the identical value the oracle did.
/// Zero-sized: the ALU is pure (no interp state), so a fresh `&mut InterpAlu` per lane
/// is free.
struct InterpAlu;

impl crate::uop::AluBuilder for InterpAlu {
    type Val = u32;

    fn const_f32_bits(&mut self, bits: u32) -> u32 {
        bits
    }
    fn f_add(&mut self, a: u32, b: u32) -> u32 {
        (f32::from_bits(a) + f32::from_bits(b)).to_bits()
    }
    fn f_sub(&mut self, a: u32, b: u32) -> u32 {
        (f32::from_bits(a) - f32::from_bits(b)).to_bits()
    }
    fn f_mul(&mut self, a: u32, b: u32) -> u32 {
        (f32::from_bits(a) * f32::from_bits(b)).to_bits()
    }
    fn f_min(&mut self, a: u32, b: u32) -> u32 {
        // f32::min returns the non-NaN operand — matches GLSL FMin the recompiler emits.
        f32::from_bits(a).min(f32::from_bits(b)).to_bits()
    }
    fn f_max(&mut self, a: u32, b: u32) -> u32 {
        f32::from_bits(a).max(f32::from_bits(b)).to_bits()
    }
    fn f_fma(&mut self, a: u32, b: u32, c: u32) -> u32 {
        // FUSED: a single rounding via `mul_add`, matching the recompiler's GLSL Fma.
        f32::from_bits(a)
            .mul_add(f32::from_bits(b), f32::from_bits(c))
            .to_bits()
    }
    fn f_abs(&mut self, a: u32) -> u32 {
        f32::from_bits(a).abs().to_bits()
    }
    fn f_neg(&mut self, a: u32) -> u32 {
        (-f32::from_bits(a)).to_bits()
    }
    fn f_fract(&mut self, a: u32) -> u32 {
        fract_f32(f32::from_bits(a)).to_bits()
    }
}

#[cfg(test)]
mod tests {
    //! Witness tests: pin what each `exec_*` op COMPUTES to the value CI-ISA (AMD Sea
    //! Islands ISA = PS4 Liverpool/GCN2) specifies for that instruction. Each expected
    //! literal below is the CI-ISA instruction description or table entry named in the
    //! test's comment; a change to the interpreter's arithmetic that diverges from the
    //! ISA fails these.

    use super::*;
    use ps4_core::memory::{MemoryProtection, VirtualMemoryManager};

    /// A memory that backs nothing: every access errors. The ALU/interp witness programs
    /// below issue no loads, so a passing run also proves they never touched memory.
    struct NoMem;
    impl VirtualMemoryManager for NoMem {
        fn map(
            &mut self,
            _addr: u64,
            _size: usize,
            _prot: MemoryProtection,
            _name: Option<&str>,
        ) -> Result<u64, &'static str> {
            Err("no memory")
        }
        fn unmap(&mut self, _addr: u64, _size: usize) -> Result<(), &'static str> {
            Err("no memory")
        }
        fn protect(
            &mut self,
            _addr: u64,
            _size: usize,
            _prot: MemoryProtection,
        ) -> Result<(), &'static str> {
            Err("no memory")
        }
        unsafe fn get_host_ptr(&self, _addr: u64) -> Option<*mut u8> {
            None
        }
        fn find_free_region(&mut self, _size: usize) -> u64 {
            0
        }
        fn is_memory_free(&self, _addr: u64, _size: usize) -> bool {
            false
        }
    }

    /// Wrap a bare instruction list into a `Decoded` stream (one dword each, sequential
    /// offsets) — the shape `run` consumes.
    fn decode_seq(insts: Vec<Inst>) -> Vec<Decoded> {
        insts
            .into_iter()
            .enumerate()
            .map(|(i, inst)| Decoded {
                inst,
                size_dwords: 1,
                offset_dwords: i as u32,
            })
            .collect()
    }

    fn exp_param0(src: Operand) -> Inst {
        Inst::Exp {
            target: ExportTarget::Param(0),
            srcs: [Some(src), None, None, None],
            done: true,
            compr: false,
            vm: false,
        }
    }

    fn endpgm() -> Inst {
        Inst::Sopp {
            op: opcodes::sopp::S_ENDPGM,
            simm16: 0,
        }
    }

    /// Run a single-lane vertex-launch program and return lane-0's exported channel 0.
    fn run_lane0_param0(insts: Vec<Inst>) -> f32 {
        let stream = decode_seq(insts);
        let exports = run(
            &stream,
            LaunchAbi::Vertex {
                user_sgprs: vec![],
                first_vertex: 0,
                num_lanes: 1,
            },
            &NoMem,
        )
        .expect("witness program runs");
        let rec = exports
            .iter()
            .find(|e| e.lane == 0)
            .expect("lane 0 exported");
        rec.values[0]
    }

    /// CI-ISA V_CVT_OFF_F32_I4 (VOP1 opcode 0xE), "4-bit signed int to 32-bit float":
    /// its full result table on CI-ISA p8-38 is `sext4(S0[3:0]) / 16`. Pin every one of
    /// the 16 entries to the published table literal.
    #[test]
    fn v_cvt_off_f32_i4_matches_ci_isa_table() {
        // (S0[3:0], CI-ISA result). The exact table printed in the ISA doc.
        let oracle: [(u32, f32); 16] = [
            (0b1000, -0.5),
            (0b1001, -0.4375),
            (0b1010, -0.375),
            (0b1011, -0.3125),
            (0b1100, -0.25),
            (0b1101, -0.1875),
            (0b1110, -0.125),
            (0b1111, -0.0625),
            (0b0000, 0.0),
            (0b0001, 0.0625),
            (0b0010, 0.125),
            (0b0011, 0.1875),
            (0b0100, 0.25),
            (0b0101, 0.3125),
            (0b0110, 0.375),
            (0b0111, 0.4375),
        ];
        for (i4, want) in oracle {
            let got = run_lane0_param0(vec![
                Inst::Vop1 {
                    op: opcodes::vop1::V_CVT_OFF_F32_I4,
                    vdst: Operand::Vgpr(0),
                    src0: Operand::Literal(i4),
                },
                exp_param0(Operand::Vgpr(0)),
                endpgm(),
            ]);
            assert_eq!(got, want, "v_cvt_off_f32_i4({i4:#06b})");
        }
    }

    /// CI-ISA V_ADD_I32 (VOP2 opcode 0x25): "D.u = S0.u + S1.u; VCC=carry-out". Witness
    /// both the wrapping sum and the carry-out bit by feeding the carry through a
    /// V_CNDMASK_B32 (CI-ISA VOP2 opcode 0x0: "D.u = VCC[i] ? S1.u : S0.u").
    #[test]
    fn v_add_i32_carry_out_matches_ci_isa() {
        let add = |a: u32, b: i64| {
            vec![
                Inst::Vop2 {
                    op: opcodes::vop2::V_ADD_I32,
                    vdst: Operand::Vgpr(0),
                    src0: Operand::Literal(a),
                    vsrc1: Operand::InlineInt(b),
                    k: None,
                },
                // v1 = VCC[0] ? 1.0 : 0.0 — surfaces the carry bit as a float.
                Inst::Vop2 {
                    op: opcodes::vop2::V_CNDMASK_B32,
                    vdst: Operand::Vgpr(1),
                    src0: Operand::InlineFloat(0.0),
                    vsrc1: Operand::InlineFloat(1.0),
                    k: None,
                },
                exp_param0(Operand::Vgpr(1)),
                endpgm(),
            ]
        };
        // 0xFFFF_FFFF + 1 wraps to 0 with carry-out set → cndmask picks 1.0.
        assert_eq!(run_lane0_param0(add(0xFFFF_FFFF, 1)), 1.0);
        // 5 + 7 = 12, no carry → cndmask picks 0.0.
        assert_eq!(run_lane0_param0(add(5, 7)), 0.0);
    }

    /// CI-ISA V_MAD_U32_U24: "Src a and b treated as 24 bit unsigned integers ... Bits
    /// [31:24] ignored. The result represents the low-order 32 bits of the multiply add
    /// result." Feed a with a nonzero high byte (must be masked off) and choose c so the
    /// integer result equals the bit pattern of 1.0f (0x3F80_0000).
    #[test]
    fn v_mad_u32_u24_masks_high_byte_matches_ci_isa() {
        // a = 0xFF00_0002 → masked to 2; b = 3; c = 0x3F80_0000 - 6.
        // 2*3 + (0x3F80_0000 - 6) = 0x3F80_0000 = f32 bits of 1.0.
        let got = run_lane0_param0(vec![
            Inst::Vop3 {
                op: opcodes::vop3::V_MAD_U32_U24,
                vdst: Operand::Vgpr(0),
                src0: Operand::Literal(0xFF00_0002),
                src1: Operand::InlineInt(3),
                src2: Operand::Literal(0x3F80_0000 - 6),
                abs: 0,
                neg: 0,
                omod: 0,
                clamp: false,
            },
            exp_param0(Operand::Vgpr(0)),
            endpgm(),
        ]);
        assert_eq!(got, 1.0);
    }

    /// CI-ISA V_INTERP_P1/P2_F32 barycentric parameter interpolation (see the module-level
    /// VINTRP contract): p1 computes `P0 + I*(P1-P0)`, p2 adds `J*(P2-P0)`, and the
    /// attribute/channel come from the ATTR/ATTRCHAN fields. With plane (P0,P1,P2) =
    /// (1,2,4), I = 0.5, J = 0.25 the result is 1 + 0.5*(2-1) + 0.25*(4-1) = 2.25.
    #[test]
    fn v_interp_plane_equation_matches_ci_isa() {
        // attr0: chan0 plane = [P0, P1, P2] = [1.0, 2.0, 4.0]; other chans zero.
        let inputs = PsInputs {
            attr_planes: vec![[[1.0, 2.0, 4.0], [0.0; 3], [0.0; 3], [0.0; 3]]],
        };
        let mut bary_i = [0.0f32; WAVE_SIZE];
        let mut bary_j = [0.0f32; WAVE_SIZE];
        bary_i[0] = 0.5; // I → v0
        bary_j[0] = 0.25; // J → v1
        let stream = decode_seq(vec![
            // v2 = P0 + I*(P1-P0), I read from v0.
            Inst::Vintrp {
                op: opcodes::vintrp::V_INTERP_P1_F32,
                vdst: Operand::Vgpr(2),
                vsrc: Operand::Vgpr(0),
                attr: 0,
                chan: 0,
            },
            // v2 += J*(P2-P0), J read from v1.
            Inst::Vintrp {
                op: opcodes::vintrp::V_INTERP_P2_F32,
                vdst: Operand::Vgpr(2),
                vsrc: Operand::Vgpr(1),
                attr: 0,
                chan: 0,
            },
            exp_param0(Operand::Vgpr(2)),
            endpgm(),
        ]);
        let exports = run(
            &stream,
            LaunchAbi::Pixel(Box::new(PixelLaunch {
                user_sgprs: vec![],
                inputs,
                bary_i,
                bary_j,
                exec: 1, // lane 0 only
            })),
            &NoMem,
        )
        .expect("interp program runs");
        let rec = exports.iter().find(|e| e.lane == 0).expect("lane 0 export");
        assert_eq!(rec.values[0], 2.25);
    }

    /// CI-ISA V_FRACT_F32 (VOP1 opcode 0x20): "D.f = S0.f - floor(S0.f)", clamped into
    /// `[0, 1)` (the range hardware/GLSL Fract guarantee).
    #[test]
    fn fract_f32_matches_ci_isa() {
        assert_eq!(fract_f32(2.75), 0.75); // 2.75 - floor = 0.75
        assert_eq!(fract_f32(-0.25), 0.75); // -0.25 - (-1) = 0.75
        // A tiny negative would round the naive difference up to 1.0; the clamp keeps it
        // strictly below 1.0.
        assert!(fract_f32(-1e-8) < 1.0);
    }

    /// `convert_packed_int` decodes a packed integer component per the CI-ISA §8
    /// num-format enum (0 unorm, 1 snorm, 4 uint, 5 sint): unorm scales by `2^w - 1`,
    /// snorm scales the sign-extended value by `2^(w-1) - 1` clamped to `>= -1`, uint/sint
    /// pass the (un)signed integer through as a float.
    #[test]
    fn convert_packed_int_matches_ci_isa_num_formats() {
        // 8-bit unorm: 255 → 1.0, 0 → 0.0, 128 → 128/255.
        assert_eq!(convert_packed_int(255, 8, 0, 0.0), 1.0);
        assert_eq!(convert_packed_int(0, 8, 0, 0.0), 0.0);
        assert_eq!(convert_packed_int(128, 8, 0, 0.0), 128.0 / 255.0);
        // 8-bit uint: raw unsigned value as float.
        assert_eq!(convert_packed_int(200, 8, 4, 0.0), 200.0);
        // 8-bit sint: 0xFF sign-extends to -1.
        assert_eq!(convert_packed_int(0xFF, 8, 5, 0.0), -1.0);
        // 8-bit snorm: 0x7F (=127) / 127 = 1.0; 0x80 (=-128)/127 clamps to -1.0.
        assert_eq!(convert_packed_int(0x7F, 8, 1, 0.0), 1.0);
        assert_eq!(convert_packed_int(0x80, 8, 1, 0.0), -1.0);
    }

    /// A memory that backs a single contiguous mapping `[base, base + bytes.len())`, so
    /// texel loads succeed and it is the sampler index math — not a memory fault — that a
    /// test exercises. `get_host_ptr` reads through a const pointer cast to `*mut`.
    struct BufMem {
        base: u64,
        bytes: Vec<u8>,
    }
    impl VirtualMemoryManager for BufMem {
        fn map(
            &mut self,
            _addr: u64,
            _size: usize,
            _prot: MemoryProtection,
            _name: Option<&str>,
        ) -> Result<u64, &'static str> {
            Err("no memory")
        }
        fn unmap(&mut self, _addr: u64, _size: usize) -> Result<(), &'static str> {
            Err("no memory")
        }
        fn protect(
            &mut self,
            _addr: u64,
            _size: usize,
            _prot: MemoryProtection,
        ) -> Result<(), &'static str> {
            Err("no memory")
        }
        unsafe fn get_host_ptr(&self, addr: u64) -> Option<*mut u8> {
            let end = self.base + self.bytes.len() as u64;
            if addr >= self.base && addr < end {
                let off = (addr - self.base) as usize;
                Some(unsafe { self.bytes.as_ptr().add(off) as *mut u8 })
            } else {
                None
            }
        }
        fn find_free_region(&mut self, _size: usize) -> u64 {
            0
        }
        fn is_memory_free(&self, _addr: u64, _size: usize) -> bool {
            false
        }
    }

    /// A guest pixel shader can feed a non-finite UV to `image_sample` — an `inf` from an
    /// upstream divide-by-zero, or a garbage VGPR reinterpreted as a large `f32`. The
    /// f32→i64 texel-index cast saturates such a coordinate to `i64::MAX`; the bilinear
    /// neighbour index must then not overflow `+ 1` (which panics under overflow-checks).
    /// This pins that an adversarial coordinate faults/wraps cleanly instead of aborting.
    #[test]
    fn bilinear_sample_saturating_uv_does_not_overflow() {
        const BASE: u64 = 0x1000;
        let mem = BufMem {
            base: BASE,
            bytes: vec![0x11u8; 64],
        };
        let interp = Interp {
            st: WaveState::new(),
            inputs: PsInputs::default(),
            mem: &mem,
            exports: Vec::new(),
        };
        let t = TSharp {
            base: BASE,
            width: 2,
            height: 2,
            dfmt: 0,
            nfmt: 0,
            tiling_index: 0,
            pitch: 0,
        };
        let s = SSharp { bilinear: true };
        // `INFINITY as i64` saturates to `i64::MAX`; the pre-fix `x0 + 1` overflowed here.
        // Neither call may panic — both must return an `Ok` sample.
        assert!(
            interp
                .sample_texture(&t, &s, f32::INFINITY, 0.0, false, 0)
                .is_ok()
        );
        assert!(
            interp
                .sample_texture(&t, &s, 0.0, f32::INFINITY, false, 0)
                .is_ok()
        );
    }
}
