//! Backend-neutral control-flow graph over a decoded GCN shader (task-129).
//!
//! The interpreter and the SPIR-V recompiler both need the *same* view of a
//! shader's control flow, so the CFG is built once here from `&[Decoded]` and
//! consumed by each backend. This is deliberately backend-neutral: it names basic
//! blocks and their terminators in terms of GCN semantics (a whole-wave branch
//! condition + a taken/fall target), and leaves the lowering — a per-invocation
//! `OpBranchConditional` in the recompiler, an EXEC-narrow/merge walk in the interp
//! — to each side.
//!
//! # Control-flow slices (task-129)
//!
//! This models forward-only conditional branches (`s_cbranch_vccz` /
//! `s_cbranch_execz` and their non-zero twins) and `s_branch`, enough for a single
//! forward `if` and an if-else diamond (the selection-merge slices), PLUS a single
//! reducible natural **loop** (the loops slice): a lone conditional back-edge whose
//! target is a lower dword than the branch (the loop header), with a single exit
//! (the branch's fall-through past the loop = the merge). See [`Cfg::loop_of`].
//!
//! It does NOT structurize arbitrary reducible control flow: anything outside the
//! recognized single-back-edge/single-exit loop subset — multiple back-edges, a
//! back-edge target that is not a clean block leader, a nested/irreducible loop, or a
//! second `Cond` inside the loop body — is reported as [`CfgError::IrreducibleLoop`]
//! (or [`CfgError::TargetOutOfRange`] for an off-stream target) so the caller defers
//! cleanly rather than emitting unstructured / wrong SPIR-V. Later slices grow this.
//!
//! # Block model
//!
//! Leaders (block-start offsets) are: the entry (offset 0), every branch *target*,
//! and every instruction that immediately follows a branch (the fall-through). The
//! stream is partitioned at those leaders into [`BasicBlock`]s, each carrying the
//! contiguous slice of instructions it owns plus a [`Terminator`] describing how it
//! leaves. Offsets throughout are *dword* offsets into the decoded stream (matching
//! `Decoded::offset_dwords`), so a target computed from a branch's `simm16` maps
//! directly onto a leader.

use crate::inst::{Decoded, Inst};
use crate::opcodes;

/// How a basic block transfers control when it finishes.
///
/// `Fallthrough`/`Branch` are unconditional; `Cond` is a whole-wave conditional
/// split into a `taken` and a `fall` successor; `Return` ends the wave
/// (`s_endpgm` or the end of the stream).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Terminator {
    /// Control falls into the block at `target` (dword offset). Produced when a
    /// block ends only because the next instruction is a branch target (a leader),
    /// with no branch of its own.
    Fallthrough { target: usize },
    /// An unconditional `s_branch` to `target` (dword offset).
    Branch { target: usize },
    /// A whole-wave conditional branch. `cond` names the GCN predicate; when it
    /// holds the wave goes to `taken`, otherwise to `fall` (both dword offsets).
    Cond {
        cond: BranchCond,
        taken: usize,
        fall: usize,
    },
    /// The block ends the shader (`s_endpgm` or end of stream).
    Return,
}

/// The whole-wave predicate a conditional branch tests. GCN's cbranch family tests
/// a scalar condition register against zero; this slice models the VCC and EXEC
/// variants (SCC is deferred until an s_cmp/carry producer exists — see task-129).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BranchCond {
    /// `s_cbranch_vccz`: branch taken when `(vcc & exec) == 0`.
    Vccz,
    /// `s_cbranch_vccnz`: branch taken when `(vcc & exec) != 0`.
    Vccnz,
    /// `s_cbranch_execz`: branch taken when `exec == 0`.
    Execz,
    /// `s_cbranch_execnz`: branch taken when `exec != 0`.
    Execnz,
}

/// One basic block: a contiguous run of decoded instructions plus its terminator.
#[derive(Clone, PartialEq, Debug)]
pub struct BasicBlock {
    /// This block's leader — its first instruction's dword offset. Also its key in
    /// the [`Cfg::block_at`] map.
    pub start: usize,
    /// Indices into the original `&[Decoded]` slice that this block owns, in stream
    /// order. Kept as indices (not a copied slice) so the backend can borrow the
    /// original `Decoded`s without cloning.
    pub insts: Vec<usize>,
    /// How the block leaves.
    pub terminator: Terminator,
}

/// The control-flow graph of a decoded shader: its blocks in stream order.
#[derive(Clone, PartialEq, Debug)]
pub struct Cfg {
    /// Basic blocks, sorted by `start` (stream order).
    pub blocks: Vec<BasicBlock>,
}

/// A recognized reducible natural loop: the three leader dword offsets both backends
/// need to lower it. Built by [`Cfg::loop_of`] from the single-back-edge/single-exit
/// shape [`build_cfg`] validates ([`validate_loops`]).
///
/// The loop is: control enters `header` (from an outside predecessor), the header's
/// body runs, and the `back_edge_block` (the block whose `Cond` terminator's `taken`
/// arm is the back-edge) either loops back to `header` (continue) or falls out to
/// `merge` (exit). In the corpus shape `header == back_edge_block` — the GCN header
/// block fuses the loop body and the back-edge test into one block — but the fields
/// are kept distinct so the model also names the general (split) shape.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct LoopInfo {
    /// The loop header's leader dword offset — the back-edge target, the block every
    /// iteration re-enters. Dominates the whole loop body.
    pub header: usize,
    /// Leader of the block whose `Cond` terminator carries the back-edge (its `taken`
    /// arm == `header`, its `fall` arm == `merge`).
    pub back_edge_block: usize,
    /// The single loop exit / merge leader — the back-edge branch's fall-through, run
    /// once after the loop terminates.
    pub merge: usize,
    /// Which whole-wave predicate the back-edge tests (the *continue* condition: the
    /// wave loops while it holds).
    pub cond: BranchCond,
}

impl Cfg {
    /// Return the index into [`Cfg::blocks`] of the block whose leader is `start`,
    /// or `None` if no block starts there.
    pub fn block_index_at(&self, start: usize) -> Option<usize> {
        self.blocks.iter().position(|b| b.start == start)
    }

    /// The unconditional successor a block flows into, if any (`Fallthrough`/`Branch`).
    /// A `Cond` block has two successors (not modeled here); `Return` has none.
    fn uncond_succ(&self, bi: usize) -> Option<usize> {
        match &self.blocks[bi].terminator {
            Terminator::Fallthrough { target } | Terminator::Branch { target } => {
                self.block_index_at(*target)
            }
            _ => None,
        }
    }

    /// The structured merge point of the conditional block at index `bi` — the block
    /// where its two arms reconverge (its immediate post-dominator, as a *leader dword
    /// offset*). Returns `None` if `bi` is not a `Cond` block, or if the arms don't
    /// reconverge inside the (forward-only, structured) subset these slices support.
    ///
    /// Two shapes are recognized, covering the first two control-flow slices:
    ///
    /// * **Single forward `if`** — one successor (`taken`, the skip target) already IS
    ///   the merge; the other (`fall`) is the guarded body that flows into it.
    /// * **If-else diamond** — both successors are arm blocks that each end in an
    ///   unconditional edge to a common block; that common block is the merge.
    ///
    /// It walks the unconditional-successor chain from each arm (each arm is a
    /// straight-line block ending in `Branch`/`Fallthrough`, or the merge itself in the
    /// single-`if` case) and takes the earliest block reachable from *both* arms — an
    /// arm that is already the meet reaches it in zero steps. This finds the diamond's
    /// merge and, degenerately, the single-`if` merge (where one arm is the meet).
    pub fn merge_target(&self, bi: usize) -> Option<usize> {
        let Terminator::Cond { taken, fall, .. } = &self.blocks[bi].terminator else {
            return None;
        };
        let taken_i = self.block_index_at(*taken)?;
        let fall_i = self.block_index_at(*fall)?;

        // Blocks reachable from an arm by following unconditional edges only (the arm
        // itself is reachable in zero steps). Bounded by the block count.
        let reach = |start: usize| -> Vec<usize> {
            let mut seen = vec![start];
            let mut b = start;
            let cap = self.blocks.len();
            while let Some(next) = self.uncond_succ(b) {
                if seen.contains(&next) || seen.len() > cap {
                    break;
                }
                seen.push(next);
                b = next;
            }
            seen
        };
        let taken_reach = reach(taken_i);
        let fall_reach = reach(fall_i);

        // The merge is the earliest (lowest leader offset) block reachable from both.
        taken_reach
            .iter()
            .filter(|t| fall_reach.contains(t))
            .map(|&i| self.blocks[i].start)
            .min()
    }

    /// If the block at index `bi` carries a loop back-edge (a `Cond` whose `taken` arm
    /// targets a block at or before it), describe the natural loop it closes.
    ///
    /// `taken` is the back-edge (continue → `header`), `fall` is the exit (`merge`).
    /// This is the counterpart of [`merge_target`] for the loops slice: the interp
    /// (EXEC-narrow re-entry) and the recompiler (`OpLoopMerge`) both read the loop's
    /// header/back-edge/merge from here so they agree. Returns `None` for a block that
    /// is not a back-edge `Cond`.
    ///
    /// [`build_cfg`] has already validated (via [`validate_loops`]) that any back-edge
    /// in the CFG has exactly this reducible single-back-edge/single-exit shape, so a
    /// `Some` result is always a well-formed loop the backends can lower.
    ///
    /// [`merge_target`]: Cfg::merge_target
    pub fn loop_of(&self, bi: usize) -> Option<LoopInfo> {
        let Terminator::Cond {
            cond, taken, fall, ..
        } = &self.blocks[bi].terminator
        else {
            return None;
        };
        // A back-edge is a conditional `taken` arm landing at or before this block's
        // own leader (a lower-or-equal dword offset). The `fall` arm is the exit/merge.
        let back = *taken;
        if back > self.blocks[bi].start {
            return None; // forward branch — a selection, handled by `merge_target`.
        }
        Some(LoopInfo {
            header: back,
            back_edge_block: self.blocks[bi].start,
            merge: *fall,
            cond: *cond,
        })
    }
}

/// Why a shader's control flow could not be turned into a (first-slice) CFG.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum CfgError {
    /// A backward branch (loop back-edge) whose surrounding control flow is NOT the
    /// recognized single-back-edge/single-exit reducible natural loop — an irreducible
    /// loop, a multi-back-edge loop, a nested loop, a back-edge target that is not a
    /// clean block leader, or a second `Cond` inside the loop body. Deferred cleanly so
    /// the recompiler never emits unstructured SPIR-V. (The loops slice DOES lower the
    /// recognized natural-loop shape; this is only its rejection path.)
    IrreducibleLoop { branch_off: usize, target: usize },
    /// A branch whose computed target falls outside the decoded stream.
    TargetOutOfRange { branch_off: usize, target: usize },
    /// An SCC-conditional branch (`s_cbranch_scc0/1`). Deferred until an SCC producer
    /// exists (task-129 sequences SCC after VCC/EXEC).
    UnsupportedCondBranch { branch_off: usize, op: u8 },
}

/// Compute a branch's target as a *dword* offset into the stream.
///
/// GCN branch semantics: the target is the dword address of the instruction after
/// the branch, plus the sign-extended 16-bit immediate (also in dwords). Since a
/// SOPP branch is one dword, "after the branch" is `offset_dwords + size_dwords`.
pub fn branch_target(offset_dwords: u32, size_dwords: u32, simm16: u16) -> i64 {
    // simm16 is a signed dword displacement.
    let disp = (simm16 as i16) as i64;
    (offset_dwords as i64) + (size_dwords as i64) + disp
}

/// Is this decoded instruction a control-flow terminator, and if so which kind?
/// Returns `None` for a straight-line instruction.
fn classify_terminator(d: &Decoded) -> Option<TermKind> {
    match &d.inst {
        Inst::Sopp { op, simm16 } => {
            use opcodes::sopp::*;
            match *op {
                S_ENDPGM => Some(TermKind::End),
                S_BRANCH => Some(TermKind::Uncond {
                    target: branch_target(d.offset_dwords, d.size_dwords, *simm16),
                }),
                S_CBRANCH_VCCZ => Some(TermKind::Cond {
                    op: *op,
                    cond: Some(BranchCond::Vccz),
                    target: branch_target(d.offset_dwords, d.size_dwords, *simm16),
                }),
                S_CBRANCH_VCCNZ => Some(TermKind::Cond {
                    op: *op,
                    cond: Some(BranchCond::Vccnz),
                    target: branch_target(d.offset_dwords, d.size_dwords, *simm16),
                }),
                S_CBRANCH_EXECZ => Some(TermKind::Cond {
                    op: *op,
                    cond: Some(BranchCond::Execz),
                    target: branch_target(d.offset_dwords, d.size_dwords, *simm16),
                }),
                S_CBRANCH_EXECNZ => Some(TermKind::Cond {
                    op: *op,
                    cond: Some(BranchCond::Execnz),
                    target: branch_target(d.offset_dwords, d.size_dwords, *simm16),
                }),
                // SCC-conditional branches: no SCC producer in this slice.
                S_CBRANCH_SCC0 | S_CBRANCH_SCC1 => Some(TermKind::Cond {
                    op: *op,
                    cond: None,
                    target: branch_target(d.offset_dwords, d.size_dwords, *simm16),
                }),
                _ => None,
            }
        }
        _ => None,
    }
}

/// Internal, pre-validation classification of a terminating instruction.
enum TermKind {
    End,
    Uncond {
        target: i64,
    },
    Cond {
        op: u8,
        /// `None` for an SCC branch we don't yet model.
        cond: Option<BranchCond>,
        target: i64,
    },
}

/// Build the [`Cfg`] for a decoded shader, or a [`CfgError`] if its control flow is
/// outside the first slice (backward branch, out-of-range target, SCC branch).
///
/// A shader with no branches yields a single block ending in `Return` — identical in
/// behavior to the old straight-line walk.
pub fn build_cfg(insts: &[Decoded]) -> Result<Cfg, CfgError> {
    // The stream may carry instructions after an s_endpgm (padding); the walk stops
    // at the first s_endpgm, so cap the effective length there.
    let end_index = insts
        .iter()
        .position(|d| matches!(&d.inst, Inst::Sopp { op, .. } if *op == opcodes::sopp::S_ENDPGM))
        .map(|i| i + 1)
        .unwrap_or(insts.len());
    let live = &insts[..end_index];

    // Map a dword offset -> index into `live`, and collect the ordered offsets so a
    // computed branch target can be validated to land on an instruction boundary. The
    // table is built over `live` (NOT the full `insts`) on purpose: blocks are cut over
    // `live`, so a target that lands in the post-`s_endpgm` padding must be rejected as
    // out-of-range here — otherwise it would validate against the padding, become a
    // leader beyond `live`, and pass 2 would build an empty / out-of-bounds block.
    let offset_of_index: Vec<usize> = live.iter().map(|d| d.offset_dwords as usize).collect();
    let index_of_offset = |off: usize| offset_of_index.iter().position(|&o| o == off);

    // Pass 1: collect leaders. Entry is a leader; every branch target is a leader;
    // the instruction after any branch is a leader. Validate targets here.
    let mut leaders: Vec<usize> = Vec::new();
    if let Some(first) = live.first() {
        leaders.push(first.offset_dwords as usize);
    }
    for (i, d) in live.iter().enumerate() {
        match classify_terminator(d) {
            Some(TermKind::End) | None => {}
            Some(TermKind::Uncond { target }) => {
                let target = validate_target(d.offset_dwords as usize, target, &index_of_offset)?;
                leaders.push(target);
                // Fall-through leader (unreachable for an unconditional branch, but
                // still a block boundary if some other branch lands there).
                if let Some(next) = live.get(i + 1) {
                    leaders.push(next.offset_dwords as usize);
                }
            }
            Some(TermKind::Cond { op, cond, target }) => {
                if cond.is_none() {
                    return Err(CfgError::UnsupportedCondBranch {
                        branch_off: d.offset_dwords as usize,
                        op,
                    });
                }
                let target = validate_target(d.offset_dwords as usize, target, &index_of_offset)?;
                leaders.push(target);
                if let Some(next) = live.get(i + 1) {
                    leaders.push(next.offset_dwords as usize);
                }
            }
        }
    }
    leaders.sort_unstable();
    leaders.dedup();

    // Pass 2: cut the stream at each leader into blocks and assign terminators.
    let mut blocks: Vec<BasicBlock> = Vec::new();
    for (li, &leader) in leaders.iter().enumerate() {
        let start_idx = index_of_offset(leader).expect("leader is an instruction boundary");
        // This block runs from `leader` up to (but not including) the next leader, or
        // the end of the live stream.
        let next_leader_idx = leaders
            .get(li + 1)
            .map(|&nl| index_of_offset(nl).expect("leader is a boundary"))
            .unwrap_or(live.len());
        let inst_indices: Vec<usize> = (start_idx..next_leader_idx).collect();

        // The block's terminator is decided by its last instruction. If that last
        // instruction is not itself a control-flow op, the block falls through into
        // the following leader (or returns if it is the last block).
        let last_idx = *inst_indices.last().expect("non-empty block");
        let last = &live[last_idx];
        let terminator = match classify_terminator(last) {
            Some(TermKind::End) => Terminator::Return,
            Some(TermKind::Uncond { target }) => Terminator::Branch {
                target: target as usize,
            },
            Some(TermKind::Cond {
                cond: Some(cond),
                target,
                ..
            }) => {
                // `fall` is the instruction after the branch (the next leader).
                let fall = live
                    .get(last_idx + 1)
                    .map(|d| d.offset_dwords as usize)
                    // A conditional branch at the very end with no fall-through is
                    // degenerate; treat the fall as one-past-end (unreachable).
                    .unwrap_or(usize::MAX);
                Terminator::Cond {
                    cond,
                    taken: target as usize,
                    fall,
                }
            }
            Some(TermKind::Cond { cond: None, .. }) => {
                unreachable!("SCC cond branch rejected in pass 1")
            }
            None => {
                // Straight-line block boundary: fall into the next leader, or return
                // if this is the last block.
                match leaders.get(li + 1) {
                    Some(&next) => Terminator::Fallthrough { target: next },
                    None => Terminator::Return,
                }
            }
        };

        blocks.push(BasicBlock {
            start: leader,
            insts: inst_indices,
            terminator,
        });
    }

    let cfg = Cfg { blocks };
    // Post-pass: any back-edge must form the recognized single-back-edge/single-exit
    // reducible natural loop, else defer cleanly (never lower unstructured SPIR-V).
    validate_loops(&cfg)?;
    Ok(cfg)
}

/// Validate that every back-edge in `cfg` closes a reducible natural loop of the
/// single-back-edge / single-exit shape the backends can lower (the loops slice),
/// rejecting anything else as [`CfgError::IrreducibleLoop`].
///
/// A back-edge is a `Cond` terminator whose `taken` arm targets a block at or before
/// it (the loop header). The shape we accept, and the reasons we reject the rest:
///
/// * The header must be a real block leader (guaranteed — targets are leaders).
/// * There must be exactly ONE back-edge in the whole CFG. A second back-edge (to the
///   same or a different header) is a multi-loop / irreducible shape — reject.
/// * The loop body — blocks in `[header, back_edge_block]` by leader offset — must
///   contain NO other `Cond` terminator: the only conditional inside the loop is the
///   back-edge itself. A nested `if`/loop inside the body is outside the subset.
/// * The header must be entered from OUTSIDE the loop by a single fall-in edge (it is
///   not the function entry block: the entry block falls into the header). That holds
///   structurally here because the header is a branch target with a straight-line
///   predecessor; we assert the header is not block 0 so a self-looping entry (no
///   distinct pre-header) is rejected — the recompiler needs a pre-header to place the
///   `OpBranch` into the loop.
/// * The back-edge's `fall` (merge) must be a real leader after the loop (a forward
///   exit), i.e. the single exit. A degenerate branch with no fall-through is rejected.
fn validate_loops(cfg: &Cfg) -> Result<(), CfgError> {
    // An UNCONDITIONAL backward branch (`s_branch` to a lower/equal offset) is a cycle
    // with no exit test — not a structured loop at all (no continue condition to lower
    // to an OpBranchConditional). Reject before looking at conditional back-edges.
    for block in &cfg.blocks {
        if let Terminator::Branch { target } = &block.terminator
            && *target <= block.start
        {
            return Err(CfgError::IrreducibleLoop {
                branch_off: block.start,
                target: *target,
            });
        }
    }

    let mut back_edges = 0usize;
    for bi in 0..cfg.blocks.len() {
        let Some(li) = cfg.loop_of(bi) else {
            continue;
        };
        back_edges += 1;
        let branch_off = cfg.blocks[bi].start;

        // Only one back-edge across the whole shader (single natural loop, no nesting).
        if back_edges > 1 {
            return Err(CfgError::IrreducibleLoop {
                branch_off,
                target: li.header,
            });
        }

        // The header must have a distinct pre-header: it cannot be the function entry
        // (block 0), or there is no outside block to branch into the loop from.
        if cfg.block_index_at(li.header) == Some(0) {
            return Err(CfgError::IrreducibleLoop {
                branch_off,
                target: li.header,
            });
        }

        // The merge (single exit) must be a real leader forward of the loop; a
        // degenerate branch with no fall-through (`usize::MAX`) has no structured exit.
        if cfg.block_index_at(li.merge).is_none() {
            return Err(CfgError::IrreducibleLoop {
                branch_off,
                target: li.header,
            });
        }

        // No other `Cond` inside the loop body [header, back_edge_block]. A second
        // conditional in the body is an unsupported nested selection/loop.
        for (obi, ob) in cfg.blocks.iter().enumerate() {
            if ob.start < li.header || ob.start > li.back_edge_block {
                continue; // outside the loop body span
            }
            if obi == bi {
                continue; // the back-edge itself is allowed
            }
            if matches!(ob.terminator, Terminator::Cond { .. }) {
                return Err(CfgError::IrreducibleLoop {
                    branch_off,
                    target: li.header,
                });
            }
        }
    }
    Ok(())
}

/// Validate a computed branch target: it must land on an instruction boundary within
/// the live stream (the caller passes an `index_of_offset` scoped to `live`, so a
/// target in the post-`s_endpgm` padding resolves to `None` and is rejected here). A
/// backward target (a loop back-edge, `target <= branch_off`) is allowed — the back-edge
/// is a leader like any other — and its *loop shape* is validated later by
/// [`validate_loops`]; an off-stream target is rejected immediately.
fn validate_target(
    branch_off: usize,
    target: i64,
    index_of_offset: &impl Fn(usize) -> Option<usize>,
) -> Result<usize, CfgError> {
    if target < 0 {
        return Err(CfgError::TargetOutOfRange {
            branch_off,
            target: 0,
        });
    }
    let target = target as usize;
    if index_of_offset(target).is_none() {
        return Err(CfgError::TargetOutOfRange { branch_off, target });
    }
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inst::Inst;

    /// Build a `Decoded` SOPP at a given dword offset (all SOPP are 1 dword).
    fn sopp(offset: u32, op: u8, simm16: u16) -> Decoded {
        Decoded {
            inst: Inst::Sopp { op, simm16 },
            size_dwords: 1,
            offset_dwords: offset,
        }
    }

    /// A 1-dword filler instruction (VOP1 v_nop-ish) at `offset`.
    fn filler(offset: u32) -> Decoded {
        Decoded {
            inst: Inst::Sopp {
                op: opcodes::sopp::S_NOP,
                simm16: 0,
            },
            size_dwords: 1,
            offset_dwords: offset,
        }
    }

    #[test]
    fn branch_target_forward() {
        // Branch at dword 4, size 1, simm16 = +3 → target = 4 + 1 + 3 = 8.
        assert_eq!(branch_target(4, 1, 3), 8);
    }

    #[test]
    fn branch_target_zero_displacement() {
        // simm16 = 0 → the instruction right after the branch.
        assert_eq!(branch_target(4, 1, 0), 5);
    }

    #[test]
    fn branch_target_negative_displacement_signextends() {
        // simm16 = 0xFFFF = -1 → target = 4 + 1 - 1 = 4 (points back at the branch).
        assert_eq!(branch_target(4, 1, 0xFFFF), 4);
        // simm16 = 0xFFFC = -4 → target = 10 + 1 - 4 = 7.
        assert_eq!(branch_target(10, 1, 0xFFFC), 7);
    }

    #[test]
    fn straight_line_is_one_block() {
        // Three fillers then s_endpgm → a single block ending Return.
        let insts = vec![
            filler(0),
            filler(1),
            filler(2),
            sopp(3, opcodes::sopp::S_ENDPGM, 0),
        ];
        let cfg = build_cfg(&insts).unwrap();
        assert_eq!(cfg.blocks.len(), 1);
        assert_eq!(cfg.blocks[0].start, 0);
        assert_eq!(cfg.blocks[0].insts, vec![0, 1, 2, 3]);
        assert_eq!(cfg.blocks[0].terminator, Terminator::Return);
    }

    #[test]
    fn forward_cbranch_vccz_splits_three_blocks() {
        // dword 0: filler (the compare would live here)
        // dword 1: s_cbranch_vccz +1  → target = 1 + 1 + 1 = 3
        // dword 2: filler (the "taken-when-vcc-nonzero" body — the fall block)
        // dword 3: filler (the merge / continuation)
        // dword 4: s_endpgm
        let insts = vec![
            filler(0),
            sopp(1, opcodes::sopp::S_CBRANCH_VCCZ, 1),
            filler(2),
            filler(3),
            sopp(4, opcodes::sopp::S_ENDPGM, 0),
        ];
        let cfg = build_cfg(&insts).unwrap();
        // Leaders: 0 (entry), 3 (target), 2 (post-branch fall). → blocks at 0, 2, 3.
        assert_eq!(cfg.blocks.len(), 3);

        let b0 = &cfg.blocks[0];
        assert_eq!(b0.start, 0);
        assert_eq!(b0.insts, vec![0, 1]);
        assert_eq!(
            b0.terminator,
            Terminator::Cond {
                cond: BranchCond::Vccz,
                taken: 3,
                fall: 2,
            }
        );

        let b1 = &cfg.blocks[1];
        assert_eq!(b1.start, 2);
        assert_eq!(b1.insts, vec![2]);
        assert_eq!(b1.terminator, Terminator::Fallthrough { target: 3 });

        let b2 = &cfg.blocks[2];
        assert_eq!(b2.start, 3);
        assert_eq!(b2.insts, vec![3, 4]);
        assert_eq!(b2.terminator, Terminator::Return);

        // The single-`if` merge is the branch's taken (skip) target.
        assert_eq!(cfg.merge_target(0), Some(3));
    }

    #[test]
    fn if_else_diamond_splits_four_blocks_and_finds_merge() {
        // A diamond: a cond-branch whose two arms each unconditionally reach a common
        // merge.
        //   dword 0: filler (the compare)
        //   dword 1: s_cbranch_vccz +2   → target = 1 + 1 + 2 = 4 (arm B leader)
        //   dword 2: filler (arm A body — the fall arm)
        //   dword 3: s_branch +2         → target = 3 + 1 + 2 = 6 (the merge; skips arm B)
        //   dword 4: filler (arm B body — the taken arm)
        //   dword 5: filler (arm B body)
        //   dword 6: filler (merge)
        //   dword 7: s_endpgm
        let insts = vec![
            filler(0),
            sopp(1, opcodes::sopp::S_CBRANCH_VCCZ, 2),
            filler(2),
            sopp(3, opcodes::sopp::S_BRANCH, 2),
            filler(4),
            filler(5),
            filler(6),
            sopp(7, opcodes::sopp::S_ENDPGM, 0),
        ];
        let cfg = build_cfg(&insts).unwrap();
        // Leaders: 0 (entry), 4 (cbranch target = arm B), 2 (fall = arm A), 6
        // (s_branch target = merge). → blocks at 0, 2, 4, 6.
        assert_eq!(cfg.blocks.len(), 4);

        let entry = &cfg.blocks[0];
        assert_eq!(entry.start, 0);
        assert_eq!(
            entry.terminator,
            Terminator::Cond {
                cond: BranchCond::Vccz,
                taken: 4, // arm B
                fall: 2,  // arm A
            }
        );

        // Arm A (fall) at dword 2 ends in an unconditional branch to the merge.
        let arm_a = &cfg.blocks[1];
        assert_eq!(arm_a.start, 2);
        assert_eq!(arm_a.terminator, Terminator::Branch { target: 6 });

        // Arm B (taken) at dword 4 falls through into the merge.
        let arm_b = &cfg.blocks[2];
        assert_eq!(arm_b.start, 4);
        assert_eq!(arm_b.terminator, Terminator::Fallthrough { target: 6 });

        // Merge at dword 6 returns.
        let merge = &cfg.blocks[3];
        assert_eq!(merge.start, 6);
        assert_eq!(merge.terminator, Terminator::Return);

        // Both arms reconverge at the merge (dword 6).
        assert_eq!(cfg.merge_target(0), Some(6));
    }

    #[test]
    fn unconditional_backward_branch_is_rejected() {
        // An unconditional `s_branch` back-edge is a cycle with no exit test — not a
        // structured loop (no continue condition). It defers as IrreducibleLoop.
        // s_branch with simm16 = -2 at dword 2 → target = 2 + 1 - 2 = 1 (backward).
        let insts = vec![
            filler(0),
            filler(1),
            sopp(2, opcodes::sopp::S_BRANCH, 0xFFFE),
            sopp(3, opcodes::sopp::S_ENDPGM, 0),
        ];
        let err = build_cfg(&insts).unwrap_err();
        assert!(matches!(err, CfgError::IrreducibleLoop { .. }));
    }

    #[test]
    fn natural_loop_splits_and_loop_query_finds_header_and_merge() {
        // The corpus loop shape in miniature:
        //   dword 0: filler (entry — falls into the header)
        //   dword 1: filler (loop header body / the compare would live here)
        //   dword 2: s_cbranch_vccnz -2  → target = 2 + 1 - 2 = 1 (BACK-EDGE to header)
        //   dword 3: filler (the exit / merge)
        //   dword 4: s_endpgm
        let insts = vec![
            filler(0),
            filler(1),
            sopp(2, opcodes::sopp::S_CBRANCH_VCCNZ, 0xFFFE),
            filler(3),
            sopp(4, opcodes::sopp::S_ENDPGM, 0),
        ];
        let cfg = build_cfg(&insts).unwrap();
        // Leaders: 0 (entry), 1 (back-edge target = header), 3 (post-branch = merge).
        assert_eq!(cfg.blocks.len(), 3);

        // Entry falls into the header.
        assert_eq!(cfg.blocks[0].start, 0);
        assert_eq!(
            cfg.blocks[0].terminator,
            Terminator::Fallthrough { target: 1 }
        );

        // The header IS the back-edge block: `taken` loops back to itself's leader (1),
        // `fall` is the exit (3).
        let hdr = &cfg.blocks[1];
        assert_eq!(hdr.start, 1);
        assert_eq!(
            hdr.terminator,
            Terminator::Cond {
                cond: BranchCond::Vccnz,
                taken: 1, // back-edge to header
                fall: 3,  // exit / merge
            }
        );

        // The exit block returns.
        assert_eq!(cfg.blocks[2].start, 3);
        assert_eq!(cfg.blocks[2].terminator, Terminator::Return);

        // The loop query names header/back-edge/merge for the back-edge block.
        let li = cfg.loop_of(1).expect("block 1 is a back-edge block");
        assert_eq!(li.header, 1);
        assert_eq!(li.back_edge_block, 1);
        assert_eq!(li.merge, 3);
        assert_eq!(li.cond, BranchCond::Vccnz);
        // The entry block is not a loop back-edge.
        assert_eq!(cfg.loop_of(0), None);
    }

    #[test]
    fn self_looping_entry_is_rejected() {
        // A back-edge whose header is the function entry (block 0) has no pre-header to
        // branch into the loop from — outside the structured subset.
        //   dword 0: filler (header == entry, no distinct pre-header)
        //   dword 1: s_cbranch_vccnz -1 → target = 1 + 1 - 1 = 1... make it target 0:
        //   simm16 = -2 → 1 + 1 - 2 = 0 (back-edge to entry)
        let insts = vec![
            filler(0),
            sopp(1, opcodes::sopp::S_CBRANCH_VCCNZ, 0xFFFE),
            filler(2),
            sopp(3, opcodes::sopp::S_ENDPGM, 0),
        ];
        let err = build_cfg(&insts).unwrap_err();
        assert!(matches!(err, CfgError::IrreducibleLoop { .. }));
    }

    #[test]
    fn nested_cond_in_loop_body_is_rejected() {
        // Two conditional branches, the second forming a back-edge over a body that
        // still contains the first `Cond` — a nested selection inside the loop, outside
        // the subset.
        //   dword 0: filler (entry)
        //   dword 1: header body
        //   dword 2: s_cbranch_vccz +1 → target = 2 + 1 + 1 = 4 (a forward Cond in body)
        //   dword 3: filler
        //   dword 4: s_cbranch_vccnz -3 → target = 4 + 1 - 3 = 2 (back-edge into body)
        //   dword 5: filler (exit)
        //   dword 6: s_endpgm
        let insts = vec![
            filler(0),
            filler(1),
            sopp(2, opcodes::sopp::S_CBRANCH_VCCZ, 1),
            filler(3),
            sopp(4, opcodes::sopp::S_CBRANCH_VCCNZ, 0xFFFD),
            filler(5),
            sopp(6, opcodes::sopp::S_ENDPGM, 0),
        ];
        let err = build_cfg(&insts).unwrap_err();
        assert!(matches!(err, CfgError::IrreducibleLoop { .. }));
    }

    #[test]
    fn scc_branch_is_rejected() {
        let insts = vec![
            filler(0),
            sopp(1, opcodes::sopp::S_CBRANCH_SCC0, 1),
            filler(2),
            sopp(3, opcodes::sopp::S_ENDPGM, 0),
        ];
        let err = build_cfg(&insts).unwrap_err();
        assert!(matches!(err, CfgError::UnsupportedCondBranch { .. }));
    }

    #[test]
    fn out_of_range_target_is_rejected() {
        // s_cbranch_vccz with a huge forward displacement → past the stream.
        let insts = vec![
            filler(0),
            sopp(1, opcodes::sopp::S_CBRANCH_VCCZ, 100),
            filler(2),
            sopp(3, opcodes::sopp::S_ENDPGM, 0),
        ];
        let err = build_cfg(&insts).unwrap_err();
        assert!(matches!(err, CfgError::TargetOutOfRange { .. }));
    }

    #[test]
    fn branch_target_past_first_endpgm_is_rejected_not_panicked() {
        // The GCN early-terminate idiom: a whole-wave conditional skips over an
        // s_endpgm to a continuation that also ends in s_endpgm. The target lands on a
        // real instruction, but one PAST the first s_endpgm — the walk stops at the
        // first s_endpgm, so that instruction is not in the live stream.
        //   dword 0: filler
        //   dword 1: s_cbranch_execnz +1 → target = 1 + 1 + 1 = 3 (CONT, past endpgm@2)
        //   dword 2: s_endpgm            (first s_endpgm → live = insts[..3])
        //   dword 3: filler (CONT)       (in the post-endpgm padding)
        //   dword 4: s_endpgm
        // Before the fix, target 3 validated against the full stream, became a leader
        // beyond `live`, and pass 2 built an empty block → `.expect("non-empty block")`
        // panic. It must instead defer as a clean CfgError.
        let insts = vec![
            filler(0),
            sopp(1, opcodes::sopp::S_CBRANCH_EXECNZ, 1),
            sopp(2, opcodes::sopp::S_ENDPGM, 0),
            filler(3),
            sopp(4, opcodes::sopp::S_ENDPGM, 0),
        ];
        let err = build_cfg(&insts).unwrap_err();
        assert!(matches!(err, CfgError::TargetOutOfRange { .. }));
    }

    #[test]
    fn branch_target_into_mid_padding_is_rejected_not_panicked() {
        // Variant: the target lands mid-padding with a further padding instruction
        // trailing it. Before the fix this produced a NON-empty block whose last index
        // was past the live slice → `&live[last_idx]` out-of-bounds panic (a different
        // panic site from the empty-block case above).
        //   dword 0: filler
        //   dword 1: s_cbranch_execnz +2 → target = 1 + 1 + 2 = 4 (mid-padding)
        //   dword 2: s_endpgm            (first s_endpgm → live = insts[..3])
        //   dword 3: filler (padding)
        //   dword 4: filler (padding, the target)
        //   dword 5: s_endpgm
        let insts = vec![
            filler(0),
            sopp(1, opcodes::sopp::S_CBRANCH_EXECNZ, 2),
            sopp(2, opcodes::sopp::S_ENDPGM, 0),
            filler(3),
            filler(4),
            sopp(5, opcodes::sopp::S_ENDPGM, 0),
        ];
        let err = build_cfg(&insts).unwrap_err();
        assert!(matches!(err, CfgError::TargetOutOfRange { .. }));
    }
}
