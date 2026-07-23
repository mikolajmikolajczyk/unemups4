//! GCN → SPIR-V recompiler for the straight-line triangle subset (doc-2 §1,
//! phase 4).
//!
//! This is the recompiler half of the shader-translation chain. Its counterpart
//! is the wave64 CPU interpreter in [`crate::interp`] — the differential ORACLE
//! (decision-3, decision-6). The two must agree on the observable contract (the
//! captured exports), so this module reproduces the interpreter's semantics
//! op-for-op wherever a choice is observable. The differential test harness diffs
//! the two; where a SPIR-V idiom forces a deviation it is called out in a
//! `DIVERGENCE:` comment so the harness can account for it.
//!
//! ## Scope
//!
//! The same subset the corpus exercises: `s_mov`, SMRD `s_load_dwordx*`,
//! `s_waitcnt`/`s_nop`/`s_endpgm` (no-ops), MUBUF `buffer_load_format_*`, VINTRP
//! `v_interp_p1/p2/mov_f32`, the VOP1/VOP2/VOP3 ALU the corpus uses, and EXP.
//! Straight-line only: the corpus has no branches, so there is no structured CFG
//! here — a single basic block from entry to `OpReturn`. Loops/tess/GS are out of
//! scope (deferred). An unsupported instruction becomes a structured
//! [`RecompileError`], never a panic — mirroring the oracle.
//!
//! ## Register model
//!
//! GCN SGPRs and VGPRs become function-local `OpVariable`s (`Function` storage),
//! one per register the shader touches, materialized lazily. A read is an
//! `OpLoad`, a write an `OpStore`. Straight-line execution over a single block
//! makes this a faithful, order-preserving model of the wave state without
//! needing SSA phi nodes; `v_mac`/`v_interp_p2` (which read their own `vdst`) fall
//! out naturally. This is one *invocation* of the shader (one lane): the wave64 /
//! EXEC masking the interpreter models is the GPU's rasterizer/launch job here,
//! not the shader body's.
//!
//! ## I/O and resources
//!
//! - **VS**: `gl_VertexIndex` (the launch ABI's `v0` = vertex index) drives an
//!   indexed fetch from a descriptor-backed storage buffer (the vertex-buffer
//!   V#). `exp pos0` → the `Position` builtin; `exp param<n>` → a `Location`
//!   output. The buffer binding + element layout is carried in [`IoLayout`] for
//!   the host-pipeline provider (`HostShader` construction in `ps4-gnm`) to map
//!   into the descriptor set.
//! - **PS**: VINTRP interpolants → `Location` inputs, evaluated screen-space-
//!   linear exactly as the oracle does. `exp mrt0` → the fragment `Location=0`
//!   output.
//!
//! ## Portability (decision-3, HARD)
//!
//! Only the portable `Shader` capability is declared — nothing MoltenVK/Metal
//! rejects. Every emitted module validates against `spirv-val` in the test suite.
//!
//! ### Min-target decision: Vulkan 1.1 (SPIR-V 1.3), StorageBuffer
//!
//! The VS vertex-buffer fetch uses a `StorageBuffer`-class SSBO, which requires
//! the SPIR-V 1.3 `StorageBuffer` storage class (Vulkan 1.1). We therefore commit
//! to a **Vulkan-1.1-minimum** MoltenVK target: MoltenVK exposes Vulkan 1.1 +
//! `VK_KHR_portability_subset`, and the plain `Shader` capability + SPIR-V 1.3 is
//! within the portability subset. The alternative — a `Uniform` + `BufferBlock`
//! SSBO valid down to SPIR-V 1.0 / Vulkan 1.0 — is deprecated in SPIR-V 1.3 and
//! would force a second emission path, so we do not pursue it. `spirv-val` runs
//! `--target-env vulkan1.1` to keep validation consistent with this decision.

use std::collections::HashMap;

use rspirv::binary::Assemble;
use rspirv::dr::{Builder, Operand as DrOperand};
use rspirv::spirv;

use crate::inst::{Decoded, ExportTarget, Inst};
use crate::opcodes;
use crate::operand::{Operand, SpecialReg};

/// The GLSL.std.450 extended-instruction opcodes this recompiler emits. Named
/// locally so the mapping to the oracle's math is explicit at the call site.
mod glsl {
    pub const FABS: u32 = 4;
    pub const SIN: u32 = 13;
    pub const FLOOR: u32 = 8;
    pub const CEIL: u32 = 9;
    pub const FRACT: u32 = 10;
    pub const SQRT: u32 = 31;
    pub const FMIN: u32 = 37;
    pub const FMAX: u32 = 40;
    pub const FMA: u32 = 50;
    /// Pack a vec2 of f32 into a u32 as two f16 (round-to-nearest-even).
    pub const PACK_HALF_2X16: u32 = 58;
    /// Unpack a u32 into a vec2 of f32 from two f16.
    pub const UNPACK_HALF_2X16: u32 = 62;
}

/// The pipeline stage a recompiled module targets. Chosen from the GCN shader's
/// stage (the corpus `.sb` header carries it) so the recompiler picks the VS vs
/// PS I/O ABI.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ShaderStage {
    /// Vertex shader: `gl_VertexIndex`-driven fetch, `Position`/param outputs.
    Vertex,
    /// Fragment (pixel) shader: interpolated `Location` inputs, MRT output.
    Fragment,
}

/// Number of `SPI_PS_INPUT_CNTL_n` slots the hardware provides (`n` in `0..32`).
pub const PS_INPUT_SLOTS: usize = 32;

/// Which VS export **parameter** feeds each PS interpolant attribute slot.
///
/// On GCN this routing is programmed per draw in the context register
/// `SPI_PS_INPUT_CNTL_n` (`R_028644`+`n`): its `OFFSET` field (bits `[4:0]`) names the
/// VS export parameter that PS attribute slot `n` reads. It is NOT the identity in
/// general — a PS whose `v_interp_p1_f32` reads `attr0` may well be programmed to take
/// VS parameter 1, and feeding it parameter 0 silently hands the shader the wrong
/// interpolant (a constant vertex colour where a UV was expected).
///
/// Only the PS **input** side is remapped. VS exports stay identity (`exp param<n>` →
/// `Location = n`), which is what makes the routing expressible as a pure input-side
/// permutation.
///
/// [`Default`] is the IDENTITY map, so a caller with no register context (the
/// differential harness against [`crate::interp`], which models no routing, and the
/// in-crate tests) gets the historical behaviour.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct PsInputMap {
    /// `offsets[n]` = the VS export parameter feeding PS attribute slot `n`, i.e. the
    /// `Location` that slot's Input variable is decorated with. Always masked to the
    /// hardware field width (5 bits) by the producer.
    offsets: [u8; PS_INPUT_SLOTS],
}

impl PsInputMap {
    /// Build a map from the raw per-slot `OFFSET` values, masking each to the hardware's
    /// 5-bit field so neighbouring `SPI_PS_INPUT_CNTL` bits (`DEFAULT_VAL`, `FLAT_SHADE`,
    /// `PT_SPRITE_TEX`, …) can never leak into a `Location`.
    pub fn from_offsets(offsets: [u8; PS_INPUT_SLOTS]) -> Self {
        let mut m = PsInputMap { offsets };
        for o in &mut m.offsets {
            *o &= 0x1F;
        }
        m
    }

    /// The `Location` PS attribute slot `attr` reads from. Slots beyond the hardware's
    /// 32 are not addressable by a `SPI_PS_INPUT_CNTL` register and stay identity.
    pub fn location_for(&self, attr: u8) -> u32 {
        match self.offsets.get(attr as usize) {
            Some(&off) => off as u32,
            None => attr as u32,
        }
    }
}

impl Default for PsInputMap {
    fn default() -> Self {
        let mut offsets = [0u8; PS_INPUT_SLOTS];
        for (n, o) in offsets.iter_mut().enumerate() {
            *o = n as u8;
        }
        PsInputMap { offsets }
    }
}

/// One `Location`-decorated interface variable (input or output) the recompiled
/// module exposes, carried so the provider wiring can match VS param outputs to
/// PS interpolant inputs and set up the render-target output.
#[derive(Clone, PartialEq, Debug)]
pub struct IoVar {
    /// The `Location` decoration value.
    pub location: u32,
    /// Number of `f32` components USED at this `Location` (1..=4) — NOT the width of
    /// the emitted SPIR-V interface variable.
    ///
    /// CONTRACT for the provider wiring: every `Location` interface variable this
    /// recompiler emits — VS `param`/RT outputs AND PS interpolant inputs — is ALWAYS
    /// a `vec4` in the SPIR-V. `components` is the count of channels the shader
    /// actually touches (for a PS interpolant, coalesced across all VINTRP `chan`
    /// reads; one entry per Location, not per channel), exposed so the provider can
    /// skip wiring unused channels and match one output var to one input var. The
    /// provider MUST still emit a `vec4` output at the matching Location: a narrower
    /// (e.g. `vec3`) output against this `vec4` input is a SPIR-V interface mismatch
    /// that `spirv-val` rejects. In short: SPIR-V width is fixed at 4; `components`
    /// is channels-used metadata, never the declared width.
    pub components: u32,
    /// Which export/attr this corresponds to (for provider matching / diagnostics).
    pub role: IoRole,
}

/// What an [`IoVar`] carries, so the provider chain can wire VS↔PS and RT outputs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IoRole {
    /// VS `exp param<n>` output / PS interpolant input for attribute `n`.
    Attribute(u8),
    /// PS `exp mrt<n>` render-target output.
    RenderTarget(u8),
}

/// One push-constant field the module reads, described explicitly so the
/// host-pipeline provider wires the byte range without relying on convention.
///
/// The provider MUST push each field's value into the pipeline's push-constant
/// range at exactly `offset_bytes..offset_bytes + size_bytes`. In particular the
/// [`PushConstantRole::NumRecords`] field is load-bearing: a missing or
/// zero-initialized push constant degenerates the VS fetch clamp to index 0 (every
/// vertex reads element 0), which is invisible to `spirv-val` and to the CPU oracle
/// (the oracle takes `num_records` from the V#, not a push constant).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PushConstantField {
    /// Byte offset within the push-constant block.
    pub offset_bytes: u32,
    /// Field size in bytes (`4` for a `uint`).
    pub size_bytes: u32,
    /// What the field carries, so the provider knows which value to push.
    pub role: PushConstantRole,
    /// Which vertex stream this field belongs to (task-153). A VS may fetch several
    /// distinct vertex-buffer V# streams (attr0/attr1/attr2 — interleaved or separate
    /// buffers); each has its OWN `num_records`/`stride`/`dst_sel` group. The provider
    /// pushes this field with the matching stream's V# value. Matches the descriptor
    /// binding order: stream `i` is the `i`-th [`BufferBinding`] in [`IoLayout::buffers`].
    pub stream: u32,
}

/// What a [`PushConstantField`] carries.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PushConstantRole {
    /// The vertex-buffer `num_records` (from the V#) used by the VS fetch clamp.
    /// The provider MUST push this — a zero/missing value clamps every fetch to
    /// element 0.
    NumRecords,
    /// The vertex-buffer element STRIDE in BYTES (from the V#, `word1[29:16]`), read
    /// by the VS vertex fetch: the addressed dword is `(vertex_index * stride) / 4 +
    /// component`. A PUSH CONSTANT (not a spec constant) so ONE recompiled module
    /// serves EVERY stride dynamically — stride never enters the pipeline key and no
    /// pipeline re-specialization/explosion is needed (task-140). The provider MUST
    /// push this — a zero/missing value degenerates every fetch to element 0's dword.
    Stride,
    /// The vertex-buffer destination swizzle (`dst_sel_{x,y,z,w}`, from the V#
    /// `word3[11:0]`), packed 4×3 bits (channel `ch` at bits `[ch*3 .. ch*3+2]`). The VS
    /// vertex fetch applies it per channel exactly as GCN's format/swizzle stage does:
    /// selector `0` → constant `0.0`, `1` → constant `1.0`, `4..7` → source component
    /// `selector-4`. A PUSH CONSTANT (task-155) so ONE module honors any swizzle
    /// dynamically — the swizzle never enters the pipeline key. The provider pushes the
    /// guest V#'s low-12 word3 bits; the IDENTITY swizzle `[4,5,6,7]` (packed
    /// `0b111_110_101_100` = `0xFAC`) is a pure raw passthrough (channel `ch` → source
    /// `ch`). A zero/missing value substitutes `0.0` for EVERY channel (selector 0).
    DstSel,
    /// The vertex-buffer packed FORMAT (`dfmt` in `[7:0]`, `nfmt` in `[15:8]`, from the
    /// V# `word3`), read by the VS vertex fetch to unpack each fetched component per the
    /// data/number format (task-164). A 32-bit float format (`dfmt` 4/11/13/14) reads the
    /// raw dword and bitcasts (the position/UV/atlas path — unchanged, bit-identical); a
    /// packed `_8_8_8_8`/`_8_8`/`_8` or `_16*` format extracts the component's byte/half and
    /// converts per `nfmt` (unorm → byte/255, snorm, uint, sint, half-float). This is how
    /// Celeste's `_8_8_8_8` UNORM sprite color (one packed dword) unpacks to four normalized
    /// floats instead of reading four raw dwords as garbage. A PUSH CONSTANT so one module
    /// serves every format dynamically — format never enters the pipeline key. A
    /// zero/missing value (`dfmt` 0 = Invalid) degenerates to the raw-dword path. Any format
    /// this module does not model also falls back to the raw read.
    Format,
}

/// Where a descriptor's resource (V#/T#/S#) came from in the shader's SGPR file — the
/// provenance the recompiler resolves symbolically at recompile time and, before this
/// slice, discarded. Carried per-binding on the three binding structs so the executor
/// can bind from the signature instead of re-deriving it from Celeste-shaped constants
/// (task-130 slice 1: additive only — the executor still ignores these fields).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum DescriptorSource {
    /// The resource descriptor (V#/T#/S#) lives INLINE in the SGPR block the operand
    /// names — no memory dereference. `sgpr` is the first SGPR of the descriptor quad
    /// (e.g. an `s_buffer_load`'s SBASE for a constant buffer, or an `image_sample`
    /// srsrc/ssamp whose T#/S# the launch ABI loaded into user SGPRs directly).
    InlineVSharp {
        /// First SGPR of the inline descriptor.
        sgpr: u8,
    },
    /// The SGPR block the operand names holds a descriptor-set POINTER pair that an
    /// SMRD (`s_load`) fetched; the actual descriptor is at `desc_offset` bytes into
    /// that set. `sgpr` is the SMRD's SBASE (the pointer pair), `desc_offset` is the
    /// SMRD's immediate offset in bytes (dword offset × 4).
    SetPointer {
        /// First SGPR of the descriptor-set pointer pair (the SMRD SBASE).
        sgpr: u8,
        /// Byte offset of the descriptor within the pointed-at set.
        desc_offset: u32,
    },
}

/// A combined image-sampler binding the module samples through (the T#/S# for a PS
/// `image_sample`). The host-pipeline provider maps this into the host descriptor set
/// and points it at the bound texture image + sampler at draw time. The recompiler
/// emits exactly one `OpTypeSampledImage` combined image-sampler at `(set, binding)`,
/// the portable MoltenVK/Metal-safe descriptor form (decision-3).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SamplerBinding {
    /// Descriptor-set index.
    pub set: u32,
    /// Binding index within the set.
    pub binding: u32,
    /// Where the T# (image resource) came from in the SGPR file (the MIMG `srsrc`).
    pub source: DescriptorSource,
    /// First SGPR of the S# (sampler descriptor) — the MIMG `ssamp`. Recorded so the
    /// executor can locate the sampler independently of the T# (they need not be
    /// contiguous).
    pub s_offset: u32,
}

/// A descriptor-backed buffer binding the module reads (the vertex-buffer V# for a
/// VS). The host-pipeline provider maps this into the host descriptor set and
/// supplies the actual guest buffer + `num_records` at bind time.
#[derive(Clone, PartialEq, Debug)]
pub struct BufferBinding {
    /// Descriptor-set index.
    pub set: u32,
    /// Binding index within the set.
    pub binding: u32,
    /// The DEFAULT vertex element stride in BYTES (16 = one `vec4`) — reported so a
    /// provider has a fallback when the draw's V# does not resolve a stride. The stride
    /// is NOT baked into the SPIR-V: the module reads its true per-draw stride from the
    /// [`PushConstantRole::Stride`] push constant, so one recompiled module serves every
    /// stride (12/24/32…) dynamically with no re-emit, no re-specialization, and stride
    /// stays out of the pipeline key (task-140).
    pub stride_bytes: u32,
    /// Number of `f32` components fetched per element (`4` for
    /// `buffer_load_format_xyzw`).
    pub components: u32,
    /// Where the V# (buffer resource) came from in the SGPR file. For the vertex-fetch
    /// MUBUF this is always a [`DescriptorSource::SetPointer`]: the SMRD `s_load` fetched
    /// the descriptor-set pointer pair into the SGPRs the MUBUF's `srsrc` names.
    pub source: DescriptorSource,
}

/// A scalar constant-buffer binding the module reads via `s_buffer_load` (a uniform
/// buffer the guest addresses through a V# descriptor). Exposed as a `StorageBuffer`
/// SSBO of raw `uint` dwords; the provider binds the guest constant buffer's bytes
/// (starting at the V#'s base address) at this `(set, binding)`.
#[derive(Clone, PartialEq, Debug)]
pub struct ConstBufferBinding {
    /// Descriptor-set index.
    pub set: u32,
    /// Binding index within the set.
    pub binding: u32,
    /// Highest dword index the module loads plus one — the minimum size, in dwords,
    /// the provider must make readable at this binding. (The recompiler resolves the
    /// V# symbolically, so it reports the extent it addresses, not the V#'s true size.)
    pub size_dwords: u32,
    /// Where the constant-buffer V# came from in the SGPR file — always a
    /// [`DescriptorSource::InlineVSharp`] whose `sgpr` is the `s_buffer_load` SBASE (the
    /// SGPR quad holding the V# inline, no memory dereference).
    pub source: DescriptorSource,
}

/// The I/O + resource metadata a recompiled module needs at bind time. This lives
/// in `ps4-gcn` (which cannot depend on `ps4-gnm` — that would be a cycle); the
/// host-pipeline provider (`HostShader` construction in `ps4-gnm`) maps it into the
/// host pipeline.
///
/// DRAW-MODE ASSUMPTION (VS): a recompiled VS resolves the launch vertex index to
/// `gl_VertexIndex` — both for an `idxen` vertex fetch and (task-184) for a direct
/// `v0` read — which must equal the oracle's sequential `first_vertex + lane` index.
/// That holds for a non-indexed `vkCmdDraw` (whose `firstVertex` seeds
/// `gl_VertexIndex` sequentially), NOT for `vkCmdDrawIndexed` (where `gl_VertexIndex`
/// is index-buffer driven). The provider MUST drive this shader with a sequential
/// (non-indexed) draw. Note this constrains agreement with the *interp oracle*, which
/// has no index buffer; against real hardware `gl_VertexIndex` is the right value in
/// both draw modes, since GCN's VGT delivers the fetched index in `v0` for an indexed
/// draw exactly as Vulkan delivers it in `gl_VertexIndex`.
#[derive(Clone, PartialEq, Debug)]
pub struct IoLayout {
    /// The pipeline stage.
    pub stage: ShaderStage,
    /// `Location` interface inputs (PS interpolants), one entry per Location.
    pub inputs: Vec<IoVar>,
    /// `Location` interface outputs (VS params, PS render targets).
    pub outputs: Vec<IoVar>,
    /// Descriptor-backed buffer bindings the module reads (empty for a PS with no
    /// resource fetch). This is the vertex-buffer V# fetched via MUBUF — NOT scalar
    /// constant buffers (those are `const_buffers`).
    pub buffers: Vec<BufferBinding>,
    /// Scalar constant-buffer bindings the module reads via `s_buffer_load` (empty
    /// when the module loads no uniform constants).
    pub const_buffers: Vec<ConstBufferBinding>,
    /// Combined image-sampler bindings the module samples through (empty for a shader
    /// that samples no texture). A PS declares ONE PER DISTINCT `image_sample` descriptor
    /// pair — the T# provenance (MIMG `srsrc`) plus the S# SGPR block (`ssamp`) — in
    /// first-sample order; repeat samples through the same pair share a binding. The
    /// provider points each at its own bound texture + sampler at draw time (task-199).
    pub samplers: Vec<SamplerBinding>,
    /// The push-constant block layout: an explicit per-field offset/size the
    /// provider must honor when populating the pipeline's push-constant range.
    /// Empty when the module declares no push constant. The provider MUST push
    /// every listed field; see [`PushConstantField`] for the load-bearing
    /// `num_records` contract (a missing value silently clamps the fetch to
    /// element 0).
    pub push_constants: Vec<PushConstantField>,
    /// Whether the VS exports a clip-space `Position` (`exp pos0`).
    pub exports_position: bool,
}

impl IoLayout {
    /// Whether a push constant carrying `num_records` is declared (the VS fetch clamp).
    /// Derived from `push_constants` — the single source of truth — so it cannot drift
    /// from the actual field list.
    pub fn uses_num_records(&self) -> bool {
        self.push_constants
            .iter()
            .any(|f| f.role == PushConstantRole::NumRecords)
    }
}

/// A recompiled shader: the SPIR-V words plus the I/O layout the provider wiring
/// consumes. The recompiler produces this; it does not touch `ps4-gnm`'s
/// `HostShader` (that mapping is a later layer's job).
#[derive(Clone, PartialEq, Debug)]
pub struct RecompiledShader {
    /// The assembled SPIR-V module, ready for `spirv-val` / pipeline creation.
    pub spirv: Vec<u32>,
    /// The I/O + resource metadata for binding.
    pub io: IoLayout,
}

/// A structured, non-panicking recompile failure — the recompiler's analogue of
/// [`crate::interp::InterpError`]. Every unsupported instruction or ill-formed
/// operand surfaces here instead of aborting.
#[derive(Clone, PartialEq, Debug, thiserror::Error)]
pub enum RecompileError {
    /// An instruction outside the recompiler's subset (or an `Inst::Unknown`).
    #[error("unsupported instruction at dword offset {offset}: {inst:?}")]
    UnsupportedInst { inst: Box<Inst>, offset: u32 },
    /// An operand the recompiler cannot lower in this position. Carries the containing
    /// decoded instruction (mirroring [`UnsupportedInst`](Self::UnsupportedInst)) so the
    /// failure names the opcode, not just the bare operand — `emit_inst` fills `inst` at
    /// the dispatch boundary (see [`RecompileError::with_inst`]).
    #[error("invalid operand at dword offset {offset} in {inst:?}: {operand:?} ({reason})")]
    InvalidOperand {
        inst: Box<Inst>,
        operand: Operand,
        offset: u32,
        reason: &'static str,
    },
    /// A register field addressed a register outside the modeled file.
    #[error("register {kind} {reg} out of range (max {max}) at dword offset {offset}")]
    InvalidRegister {
        kind: &'static str,
        reg: usize,
        max: usize,
        offset: u32,
    },
    /// The instruction stream is malformed for the chosen stage (e.g. a VS with no
    /// position export, or an EXP target the stage cannot produce).
    #[error("shape error: {0}")]
    Shape(&'static str),
    /// A well-formed instruction the recompiler deliberately does not yet lower
    /// (outside the exercised subset), distinct from a malformed shader. The
    /// differential harness categorizes these as "not yet supported" rather than
    /// "broken", so it can defer them without treating the shader as invalid.
    #[error("unsupported (deferred) at dword offset {offset}: {reason}")]
    Unsupported { offset: u32, reason: &'static str },
}

impl RecompileError {
    /// A placeholder `inst` for an [`InvalidOperand`](Self::InvalidOperand) raised deep
    /// inside an `emit_*` helper that does not have the decoded `Inst` in scope.
    /// [`emit_inst`](Recompiler::emit_inst) overwrites it with the real instruction at
    /// the dispatch boundary via [`Self::with_inst`], so this value never surfaces on an
    /// error that reaches `recompile`'s caller.
    fn pending_inst() -> Box<Inst> {
        Box::new(Inst::Unknown {
            raw: 0,
            raw_words: Vec::new(),
        })
    }

    /// Fill the containing instruction on an [`InvalidOperand`](Self::InvalidOperand)
    /// whose `inst` is still the pending placeholder. Called at the `emit_inst` dispatch
    /// boundary — the point where the decoded `Inst` is in scope — so every
    /// `InvalidOperand` that propagates out of instruction emission names its opcode.
    /// Other variants pass through unchanged.
    fn with_inst(self, inst: &Inst) -> Self {
        match self {
            RecompileError::InvalidOperand {
                operand,
                offset,
                reason,
                ..
            } => RecompileError::InvalidOperand {
                inst: Box::new(inst.clone()),
                operand,
                offset,
                reason,
            },
            other => other,
        }
    }
}

/// Whether an emitted instruction ended the wave (`s_endpgm`) so the caller stops
/// filling the current block.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum FlowEnd {
    Continue,
    Endpgm,
}

/// Is this decoded instruction a CFG *branch* terminator (`s_branch` /
/// `s_cbranch_*`)? Such an instruction is lowered from the block's structured
/// terminator, not as a dataflow op, so the block-body walk skips it. `s_endpgm` is
/// NOT a branch (it is emitted as a body op that returns `Endpgm`).
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

/// The whole-wave predicate a loop back-edge block's `Cond` terminator tests. Panics
/// if the block is not a `Cond` — callers only pass a validated back-edge block.
fn header_block_cond(block: &crate::cfg::BasicBlock) -> crate::cfg::BranchCond {
    match &block.terminator {
        crate::cfg::Terminator::Cond { cond, .. } => *cond,
        _ => unreachable!("loop back-edge block must be a Cond"),
    }
}

/// Map a CFG rejection to a clean recompile deferral. These are shaders whose control
/// flow is outside the structured subset (an irreducible / multi-exit / nested loop,
/// an SCC branch, an out-of-range target): the recompiler defers rather than emitting
/// unstructured / wrong SPIR-V. A *recognized* natural loop is NOT a rejection — it
/// lowers to a structured `OpLoopMerge` in [`Recompiler::emit_cfg`].
fn cfg_error_to_recompile(e: crate::cfg::CfgError) -> RecompileError {
    use crate::cfg::CfgError;
    match e {
        CfgError::IrreducibleLoop { branch_off, .. } => RecompileError::Unsupported {
            offset: branch_off as u32,
            reason: "irreducible / multi-exit loop — outside the structured loop subset",
        },
        CfgError::TargetOutOfRange { branch_off, .. } => RecompileError::Unsupported {
            offset: branch_off as u32,
            reason: "branch target outside the shader stream",
        },
        CfgError::UnsupportedCondBranch { branch_off, .. } => RecompileError::Unsupported {
            offset: branch_off as u32,
            reason: "SCC-conditional branch — no SCC producer modeled yet",
        },
    }
}

// ---- descriptor / interface layout constants -------------------------------

/// Descriptor set for the vertex-buffer V# (the only resource the corpus VS uses).
const VS_BUFFER_SET: u32 = 0;
/// Binding within [`VS_BUFFER_SET`] for the FIRST vertex stream (attr0). Later streams
/// take bindings 3, 4, … (see [`vs_stream_binding`]) — 1 is the PS combined
/// image-sampler and 2 is the constant buffer, so multi-stream vertex bindings start
/// past them.
const VS_BUFFER_BINDING: u32 = 0;

/// The maximum number of DISTINCT vertex-buffer V# streams one recompiled VS may fetch
/// (task-153). Celeste's atlas VS uses three interleaved streams (attr0/attr1/attr2 —
/// each a distinct V# at a different descriptor-set byte offset, all into one buffer at
/// a different base). Four covers that with headroom; the fixed push-constant block
/// sizes to `4 * MAX_VS_STREAMS` uints (= 64 bytes, well under the 128-byte guaranteed
/// range), and only the streams actually fetched get a descriptor binding + a pushed
/// group.
const MAX_VS_STREAMS: usize = 4;

/// Descriptor binding for vertex stream `stream` within [`VS_BUFFER_SET`]. Stream 0 keeps
/// binding 0 (the single-stream path, unchanged); streams 1.. take bindings 3, 4, … so
/// they never collide with the PS combined image-sampler (binding 1) or the constant
/// buffer (binding 2). A texturing/constant-loading PS lives in the same set 0.
fn vs_stream_binding(stream: usize) -> u32 {
    if stream == 0 {
        VS_BUFFER_BINDING
    } else {
        // 1→3, 2→4, 3→5: skip the sampler (1) and const (2) bindings.
        stream as u32 + 2
    }
}

/// The DEFAULT vertex-buffer element stride, in bytes — the value reported in
/// [`BufferBinding::stride_bytes`] as a fallback when a draw's V# does not resolve one.
///
/// The vertex-fetch SSBO is a dword-addressed `uint[]` (`ArrayStride` 4); the per-vertex
/// dword base is `vertex_index * (stride / 4)`, where `stride` is the guest V#'s true
/// stride (`word1[29:16]`, what the interpreter reads via `decode_v_sharp`). The
/// recompiler resolves the descriptor symbolically and never sees the descriptor bytes,
/// so it does NOT bake the stride: the module reads it from a PUSH CONSTANT
/// ([`PushConstantRole::Stride`]) the provider fills with the guest V#'s stride at draw
/// time (task-140). One module therefore serves every stride — a non-16 stride (12/24/32…)
/// renders WITHOUT a re-emit, a re-specialization, or a deferral, and stride never enters
/// the pipeline key.
const VB_ELEMENT_STRIDE: u32 = 16;

/// Push-constant block member index of `num_records` WITHIN a stream's 3-uint group
/// (the VS fetch clamp). A stream's group starts at member `3 * stream`.
const PC_NUM_RECORDS_MEMBER: u32 = 0;
/// Per-stream member index of the vertex element stride in bytes (task-140).
const PC_STRIDE_MEMBER: u32 = 1;
/// Per-stream member index of the packed vertex `dst_sel` (4×3 bits; task-155).
const PC_DST_SEL_MEMBER: u32 = 2;
/// Per-stream member index of the packed vertex FORMAT (`dfmt` in `[7:0]`, `nfmt` in
/// `[15:8]`; task-164) the fetch unpacks each component with.
const PC_FORMAT_MEMBER: u32 = 3;
/// Number of uint members per vertex stream in the push-constant block (num_records,
/// stride, dst_sel, format).
const PC_MEMBERS_PER_STREAM: u32 = 4;

/// The push-constant block member index of `role`-member for vertex `stream`: each
/// stream owns a contiguous 4-uint group `[4*stream .. 4*stream+4)`.
fn pc_member(stream: usize, member: u32) -> u32 {
    stream as u32 * PC_MEMBERS_PER_STREAM + member
}

/// The IDENTITY destination swizzle `[4,5,6,7]` packed into `word3[11:0]`
/// (`4 | 5<<3 | 6<<6 | 7<<9` = `0xFAC`). Applying it is a pure raw passthrough — channel
/// `ch` reads source component `ch` — so a corpus/oracle that pushes this value matches
/// the raw-read semantics the interp uses (the interp does not read `dst_sel`).
pub const DST_SEL_IDENTITY: u32 = 4 | (5 << 3) | (6 << 6) | (7 << 9);

/// Descriptor set for a PS combined image-sampler. Binding 1 keeps the FIRST one clear
/// of the VS SSBO at binding 0 in set 0.
const PS_TEXTURE_SET: u32 = 0;
/// Binding within [`PS_TEXTURE_SET`] for the FIRST combined image-sampler.
const PS_TEXTURE_BINDING: u32 = 1;

/// Descriptor set + binding for a scalar constant buffer (`s_buffer_load`). Set 0
/// keeps it with the other resources; binding 2 clears the VS SSBO (binding 0) and
/// the PS combined image-sampler (binding 1) so a texturing PS that also loads
/// constants has no collision. This is the VERTEX-stage constant buffer's binding.
const CONST_BUFFER_SET: u32 = 0;
const CONST_BUFFER_BINDING: u32 = 2;

/// Binding within [`CONST_BUFFER_SET`] for the FRAGMENT-stage constant buffer
/// (task-174). A draw whose VS and PS BOTH declare a constant buffer needs two distinct
/// set-0 slots — a shared binding would collide in the combined pipeline layout. The VS
/// keeps binding 2; the PS takes binding 6, clear of the VS SSBO streams (0, 3, 4, 5 —
/// see [`vs_stream_binding`], max [`MAX_VS_STREAMS`]=4 → 5), the PS texture (1), and the
/// VS const (2). Emitting the PS const at its OWN binding unconditionally (not just in
/// the dual case — the recompiler compiles each stage in isolation and cannot see the
/// other) keeps every PS-const draw self-consistent.
const PS_CONST_BUFFER_BINDING: u32 = 6;

/// First set-0 binding for a PS combined image-sampler BEYOND the first (task-199). A PS
/// samples as many distinct textures as it names distinct T#/S# descriptor pairs — the
/// GCN MIMG `srsrc`/`ssamp` operands are per-instruction, so one shader routinely mixes a
/// register-resident T# (loaded into user SGPRs by the launch ABI) with a memory-resident
/// one (`s_load_dwordx8` through a user-data pointer). Texture 0 keeps
/// [`PS_TEXTURE_BINDING`] so a single-texture module is emitted byte-identically to
/// before; extras start at 7, clear of the VS SSBO streams (0, 3, 4, 5), the PS texture
/// (1), the VS const (2) and the PS const (6).
const PS_TEXTURE_EXTRA_BINDING_BASE: u32 = 7;

/// Upper bound on distinct combined image-samplers one PS may declare. Bounded so a
/// malformed or unexpected shader cannot grow the set-0 layout without limit; a shader
/// that needs more defers cleanly rather than emitting a layout the backend cannot build.
/// Shared with the pipeline key's slot array — one source of truth.
use ps4_core::gpu::MAX_PS_TEXTURES;

/// Set-0 binding for PS texture `index`, in first-sample order.
///
/// Index 0 is [`PS_TEXTURE_BINDING`]; every later texture takes a slot from
/// [`PS_TEXTURE_EXTRA_BINDING_BASE`] upward. Deterministic in shader order, so the
/// binding numbers are stable across recompiles and safe to use in a pipeline cache key.
fn ps_texture_binding(index: usize) -> u32 {
    if index == 0 {
        PS_TEXTURE_BINDING
    } else {
        PS_TEXTURE_EXTRA_BINDING_BASE + index as u32 - 1
    }
}

/// Recompile a decoded straight-line GCN shader to a portable SPIR-V module for
/// `stage`. Mirrors [`crate::interp`]'s semantics op-for-op so the differential
/// harness can diff the two.
pub fn recompile(
    insts: &[Decoded],
    stage: ShaderStage,
) -> Result<RecompiledShader, RecompileError> {
    recompile_with(insts, stage, &PsInputMap::default())
}

/// Recompile as [`recompile`], but with an explicit PS attribute→VS-parameter routing
/// ([`PsInputMap`], from the draw's `SPI_PS_INPUT_CNTL_n` registers).
///
/// The map is meaningful only for [`ShaderStage::Fragment`]; a VS ignores it (its exports
/// are identity-located). Callers that cannot see context registers — the differential
/// harness, which diffs against [`crate::interp`] and so must not permute anything — use
/// [`recompile`] and get the identity map.
pub fn recompile_with(
    insts: &[Decoded],
    stage: ShaderStage,
    ps_input_map: &PsInputMap,
) -> Result<RecompiledShader, RecompileError> {
    let mut rc = Recompiler::new(stage, *ps_input_map);
    rc.emit(insts)?;
    rc.finish()
}

/// Cached SPIR-V type / constant ids so the module stays deduplicated and small.
struct Types {
    void: spirv::Word,
    f32: spirv::Word,
    u32: spirv::Word,
    i32: spirv::Word,
    bool: spirv::Word,
    v4f32: spirv::Word,
    fn_void: spirv::Word,
    ptr_fn_f32: spirv::Word,
    ptr_fn_u32: spirv::Word,
}

struct Recompiler {
    b: Builder,
    stage: ShaderStage,
    t: Types,
    glsl_ext: spirv::Word,
    /// f32-typed function-local register slots, keyed by (`is_vgpr`, reg).
    reg_f32: HashMap<(bool, u8), spirv::Word>,
    /// u32-typed views of the same slots (a register holds raw bits; we keep a
    /// parallel u32 var so integer ops and float ops each see the right type
    /// without bitcasts scattered through the body). Keyed identically.
    reg_u32: HashMap<(bool, u8), spirv::Word>,
    /// f32 constant cache (bit pattern → id).
    const_f32: HashMap<u32, spirv::Word>,
    /// u32 constant cache.
    const_u32: HashMap<u32, spirv::Word>,

    // interface variables (materialized lazily as the stream references them)
    /// `gl_VertexIndex` builtin input (VS).
    vertex_index: Option<spirv::Word>,
    /// `Position` builtin output (VS).
    position_out: Option<spirv::Word>,
    /// The vertex-buffer storage buffer streams (task-153): one per distinct V# the VS
    /// fetches, in first-fetch order. A repeat fetch from the same `DescriptorSource`
    /// reuses its stream. The corpus/single-stream path has exactly one entry.
    vs_streams: Vec<VsStream>,
    /// The PS combined image-sampler resources, one per distinct `image_sample`
    /// descriptor pair, in first-sample order (task-199). A repeat sample through the
    /// same (T# provenance, S# SGPR) pair reuses its entry instead of allocating a second
    /// binding — the same dedup discipline [`VsStream`] applies to vertex fetches.
    ps_textures: Vec<PsTexture>,
    /// The VS fetch push-constant block variable
    /// `{ uint num_records; uint stride; uint dst_sel; }` (member 0 = fetch clamp, member
    /// 1 = vertex element stride in bytes, member 2 = packed dst_sel). All members are
    /// load-bearing and pushed by the provider at draw time (task-140, task-155).
    pc_block: Option<spirv::Word>,
    /// The shared `OpTypeStruct { OpTypeRuntimeArray uint (ArrayStride 4) } (Block,
    /// member Offset 0)` used by BOTH the const-buffer and vertex-buffer SSBOs. rspirv
    /// dedups identical `OpType*` to one id, so decorating this block type in each SSBO
    /// emitter would emit `ArrayStride`/`Block`/`Offset` twice on the same id — illegal
    /// SPIR-V (spirv-val: "decorated with ArrayStride multiple times"). Emit the type +
    /// its decorations exactly once and hand the memoized id to every SSBO.
    dword_ssbo_block: Option<spirv::Word>,
    /// `Location` param outputs (VS) / MRT outputs (PS), keyed by location.
    loc_outputs: HashMap<u32, LocVar>,
    /// `Location` interpolant inputs (PS): one `vec4` Input variable per **resolved
    /// Location**, keyed by that Location — NOT by the GCN attribute slot. Under a
    /// non-identity [`PsInputMap`] two distinct attr slots can resolve to the same
    /// location (an unwritten `SPI_PS_INPUT_CNTL` slot reads `OFFSET = 0`), and two Input
    /// variables sharing a `Location` is invalid SPIR-V; keying by location makes them
    /// share one variable and keeps the component coalescing correct. A VINTRP `chan`
    /// read extracts the channel from the vec4 (`OpCompositeExtract`) — the
    /// MoltenVK-reliable pattern (scalar Input + `Component` decoration is mistranslated
    /// on Metal, reading channel 0 for all).
    ps_inputs: HashMap<u32, PsInput>,
    /// Which VS export parameter feeds each PS attribute slot, from the draw's
    /// `SPI_PS_INPUT_CNTL_n` registers. Identity for a VS (and for callers with no
    /// register context).
    ps_input_map: PsInputMap,
    /// VGPRs currently known to carry the launch vertex index (`gl_VertexIndex`).
    /// Seeded with `v0` (the launch ABI's vertex-index register) and propagated
    /// through `v_mov_b32 vN, vM` so an idxen MUBUF that relocates the index into
    /// another VGPR still resolves to `gl_VertexIndex` instead of reading an
    /// uninitialized slot. An idxen fetch on a VGPR not in this set is rejected.
    vertex_index_regs: std::collections::HashSet<u8>,
    /// SGPRs holding the fetched V# resource (SMRD dst → decoded at MUBUF time). We
    /// do not model the descriptor bytes; the fetch resolves to the bound buffer.
    /// This map records, for each SGPR the SMRD `s_load` wrote, the SMRD's provenance:
    /// `(sbase, desc_offset_bytes)` — the descriptor-set POINTER pair the SMRD read
    /// from and the byte offset of the descriptor within it. A MUBUF `srsrc` referencing
    /// a written SGPR resolves to that descriptor (a [`DescriptorSource::SetPointer`]);
    /// an `srsrc` not in the map is not a fetched descriptor and defers.
    vsharp_sgprs: std::collections::HashMap<u8, (u8, u32)>,
    /// The scalar constant buffer's SSBO (declared lazily on the first s_buffer_load).
    /// A single binding: the recompiler resolves the V# symbolically and cannot tell
    /// two distinct constant buffers apart, so a second distinct SBASE defers.
    const_buffer: Option<ConstBuffer>,
    /// The `m0` register variable (Function storage, initialized to 0), declared lazily
    /// on the first m0 read or write. m0 is NOT consulted for interpolation (the
    /// attribute comes from the VINTRP field — see `special_bits`); this slot only makes
    /// the plain `s_mov m0, s` / `s_mov s, m0` save-restore idiom a faithful copy,
    /// matching the oracle's `st.m0` (default 0). See `m0_ptr`.
    m0_var: Option<spirv::Word>,
    /// Per-invocation predicate registers: the single bool a VOPC compare (or a
    /// v_add_i32 carry) writes, keyed by its destination (VCC or an SGPR pair). Because
    /// recompiled SPIR-V is one invocation = ONE lane, a wave-level VCC/SGPR-pair mask
    /// collapses to a single `bool` here; a later v_cndmask reads it back with OpSelect.
    /// The value id is the most-recent bool stored to that key, valid only WITHIN the
    /// block that produced it (an SSA id cannot cross a block boundary). All reads — a
    /// v_cndmask select as well as a conditional branch — instead reload the bool from
    /// the cross-block-safe backing [`Self::pred_vars`] variable, so a predicate produced
    /// in another block is never referenced outside its dominance scope.
    predicates: HashMap<PredKey, spirv::Word>,
    /// Function-storage `bool` OpVariables backing each predicate key, so a predicate
    /// produced in one block survives into a later block's conditional branch. Written
    /// alongside `predicates` on every compare/carry; read back with an `OpLoad` at a
    /// branch. Declared lazily on first predicate write. (task-129: cached SSA value
    /// ids aren't valid cross-block — the register model already relies on this for
    /// VGPRs; predicates need the same treatment.)
    pred_vars: HashMap<PredKey, spirv::Word>,
    /// Pointer-to-`bool` (Function storage) type, declared lazily on the first
    /// predicate var so a branchless shader's SPIR-V (and its committed golden) never
    /// carries an unused bool pointer type.
    ptr_fn_bool: Option<spirv::Word>,

    // running metadata
    io_inputs: Vec<IoVar>,
    io_outputs: Vec<IoVar>,
    io_buffers: Vec<BufferBinding>,
    io_const_buffers: Vec<ConstBufferBinding>,
    io_samplers: Vec<SamplerBinding>,
    io_push_constants: Vec<PushConstantField>,
    exports_position: bool,
    interface: Vec<spirv::Word>,
}

/// A `Location`-decorated interface variable id (outputs are deduped by location).
struct LocVar {
    var: spirv::Word,
}

/// Which predicate register a per-invocation compare bool lives in. VCC is the
/// standalone VOPC / v_add_i32 carry destination; an SGPR pair is the VOP3-form VOPC
/// `sdst`. Keyed so a later v_cndmask resolves the bool its own predicate operand
/// names. VCC low/high both map to the single `Vcc` key (one wave mask, one bool here).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum PredKey {
    /// VCC (the implicit compare / carry destination).
    Vcc,
    /// The SGPR pair starting at register `n` (`s[n:n+1]`).
    SgprPair(u8),
}

/// A PS interpolant input: one `vec4` Input variable at a `Location`, plus the index
/// of its [`IoVar`] in `io_inputs` so the coalesced component count can be widened
/// as further channels of the same attribute are read.
struct PsInput {
    /// The `vec4` Input `OpVariable`.
    var: spirv::Word,
    /// Index into `io_inputs` (to widen `components` on later-channel reads).
    io_index: usize,
}

/// The PS combined image-sampler resource (a `sampled image` variable + the cached
/// SPIR-V type ids the sample needs).
struct PsTexture {
    /// The `OpVariable` of pointer-to-`OpTypeSampledImage` (UniformConstant class).
    var: spirv::Word,
    /// The `OpTypeSampledImage` id (loaded from `var` before the sample).
    sampled_image_ty: spirv::Word,
    /// The `OpTypeVector` `v2f32` id for the (u, v) coordinate.
    v2f32: spirv::Word,
    /// The T# provenance (MIMG `srsrc`) this binding was declared for. A later sample
    /// naming the SAME source and `s_offset` maps back to this texture.
    source: DescriptorSource,
    /// First SGPR of the S# (MIMG `ssamp`) this binding was declared for.
    s_offset: u32,
}

/// One vertex-buffer storage buffer stream: a dword-addressed runtime array of raw
/// `uint`, whose per-vertex byte stride is read from a push constant (task-140). A VS
/// may fetch SEVERAL distinct streams (attr0/attr1/attr2 — each a distinct V#), so this
/// carries the stream index that selects its descriptor binding and push-constant group
/// (task-153).
struct VsBuffer {
    /// The `OpVariable` (StorageBuffer storage class).
    var: spirv::Word,
    /// Pointer type to a `uint` member of the runtime array (StorageBuffer class).
    ptr_member: spirv::Word,
    /// The stream index (0-based, in first-fetch order). Selects the push-constant group
    /// (`3 * stream`) the fetch loads `num_records`/`stride`/`dst_sel` from.
    stream: usize,
}

/// A declared vertex stream: its SSBO variable + the descriptor source it fetches, so a
/// later fetch from the SAME V# (same `DescriptorSource`) reuses it instead of allocating
/// a second binding (task-153).
struct VsStream {
    var: spirv::Word,
    ptr_member: spirv::Word,
    /// The V# provenance this stream binds — a repeat fetch with the same source maps
    /// back to this stream.
    source: DescriptorSource,
}

/// The scalar constant-buffer storage buffer resource: a runtime array of raw `uint`
/// dwords the `s_buffer_load` path indexes directly.
struct ConstBuffer {
    /// The `OpVariable` (StorageBuffer storage class).
    var: spirv::Word,
    /// Pointer type to a `uint` member of the runtime array (StorageBuffer class).
    ptr_member: spirv::Word,
    /// The SBASE (first V# SGPR) this binding was declared for. A later s_buffer_load
    /// with a different SBASE names a different constant buffer the single binding
    /// cannot represent, and defers.
    sbase: u8,
    /// Highest dword index addressed so far, plus one (the reported binding size).
    size_dwords: u32,
}

/// The ids an `s_buffer_load` needs, copied out of [`ConstBuffer`] so the load loop
/// doesn't hold a borrow of `self.const_buffer` across `self.b` mutations.
struct ConstBufferRef {
    var: spirv::Word,
    ptr_member: spirv::Word,
}

impl Recompiler {
    fn new(stage: ShaderStage, ps_input_map: PsInputMap) -> Self {
        let mut b = Builder::new();
        // Vulkan 1.1 targets SPIR-V 1.3.
        //
        // task-136: GpuCaps plugs in here for SPIR-V feature clamping. This is the
        // recompiler's future caps consumer — a caps-tiered path would consult the
        // queried device capabilities (ps4_core::gpu::GpuCaps, populated at device
        // selection) to raise the SPIR-V version / declare extra capabilities on a
        // full-power Vulkan target while keeping the portable baseline (the single
        // `Shader` capability, SPIR-V 1.3, task-133 clamp) on MoltenVK. NOT threaded
        // yet: doing so would change `recompile()`'s signature + every caller + the
        // goldens for an UNUSED param (premature churn). Landing the seam as data on
        // the backend now (see AshBackend::caps / GpuCaps) keeps the future fork a
        // flag through one code path, never a second recompiler or a second golden
        // set. Emit stays IDENTICAL for now — behavior unchanged.
        b.set_version(1, 3);
        b.capability(spirv::Capability::Shader);
        let glsl_ext = b.ext_inst_import("GLSL.std.450");
        b.memory_model(spirv::AddressingModel::Logical, spirv::MemoryModel::GLSL450);

        let void = b.type_void();
        let f32 = b.type_float(32, None);
        let u32 = b.type_int(32, 0);
        let i32 = b.type_int(32, 1);
        let bool = b.type_bool();
        let v4f32 = b.type_vector(f32, 4);
        let fn_void = b.type_function(void, []);
        let ptr_fn_f32 = b.type_pointer(None, spirv::StorageClass::Function, f32);
        let ptr_fn_u32 = b.type_pointer(None, spirv::StorageClass::Function, u32);

        Recompiler {
            b,
            stage,
            t: Types {
                void,
                f32,
                u32,
                i32,
                bool,
                v4f32,
                fn_void,
                ptr_fn_f32,
                ptr_fn_u32,
            },
            glsl_ext,
            reg_f32: HashMap::new(),
            reg_u32: HashMap::new(),
            const_f32: HashMap::new(),
            const_u32: HashMap::new(),
            vertex_index: None,
            position_out: None,
            vs_streams: Vec::new(),
            ps_textures: Vec::new(),
            pc_block: None,
            dword_ssbo_block: None,
            loc_outputs: HashMap::new(),
            ps_inputs: HashMap::new(),
            ps_input_map,
            // v0 = the launch ABI's vertex index — but only for a VS. A PS's v0 is a
            // barycentric, not a vertex index, so seed the tracker only for the Vertex
            // stage (a PS never does an idxen vertex fetch anyway).
            vertex_index_regs: match stage {
                ShaderStage::Vertex => std::collections::HashSet::from([0u8]),
                ShaderStage::Fragment => std::collections::HashSet::new(),
            },
            vsharp_sgprs: std::collections::HashMap::new(),
            const_buffer: None,
            m0_var: None,
            predicates: HashMap::new(),
            pred_vars: HashMap::new(),
            ptr_fn_bool: None,
            io_inputs: Vec::new(),
            io_outputs: Vec::new(),
            io_buffers: Vec::new(),
            io_const_buffers: Vec::new(),
            io_samplers: Vec::new(),
            io_push_constants: Vec::new(),
            exports_position: false,
            interface: Vec::new(),
        }
    }

    fn emit(&mut self, insts: &[Decoded]) -> Result<(), RecompileError> {
        let main = self.b.id();
        self.b
            .begin_function(
                self.t.void,
                Some(main),
                spirv::FunctionControl::NONE,
                self.t.fn_void,
            )
            .expect("begin main");

        // Build the shared CFG. A shader outside the first-slice control-flow subset
        // (a loop back-edge, an SCC branch, an out-of-range target) defers cleanly to
        // `Unsupported` rather than emitting unstructured / wrong SPIR-V.
        let cfg = crate::cfg::build_cfg(insts).map_err(cfg_error_to_recompile)?;

        if cfg.blocks.len() == 1 {
            // Straight-line: one block, identical to the pre-CFG path (keeps existing
            // goldens byte-stable).
            self.b.begin_block(None).expect("entry block");
            for d in insts {
                if self.emit_inst(d)? == FlowEnd::Endpgm {
                    break;
                }
            }
            self.b.ret().expect("return");
        } else {
            self.emit_cfg(insts, &cfg)?;
        }

        self.b.end_function().expect("end main");

        // Entry point + execution mode.
        let (model, name) = match self.stage {
            ShaderStage::Vertex => (spirv::ExecutionModel::Vertex, "main"),
            ShaderStage::Fragment => (spirv::ExecutionModel::Fragment, "main"),
        };
        self.b
            .entry_point(model, main, name, self.interface.clone());
        if self.stage == ShaderStage::Fragment {
            self.b
                .execution_mode(main, spirv::ExecutionMode::OriginUpperLeft, []);
        }
        Ok(())
    }

    /// Emit one straight-line (dataflow) instruction into the current block. Returns
    /// whether it was an `s_endpgm` (so the caller stops the block). Control-flow
    /// terminators (`s_branch`, `s_cbranch_*`) are NOT handled here — they are the
    /// block terminator, lowered by [`Self::emit_cfg`] / [`Self::emit_block_body`].
    fn emit_inst(&mut self, d: &Decoded) -> Result<FlowEnd, RecompileError> {
        // The dispatch boundary: fill the containing instruction on any `InvalidOperand`
        // an `emit_*` helper raised without the decoded `Inst` in scope, so the failure
        // names the opcode rather than a bare operand.
        self.emit_inst_inner(d).map_err(|e| e.with_inst(&d.inst))
    }

    fn emit_inst_inner(&mut self, d: &Decoded) -> Result<FlowEnd, RecompileError> {
        let off = d.offset_dwords;
        match &d.inst {
            // Scalar control the oracle treats as no-ops (waitcnt/nop) or the wave end.
            Inst::Sopp { op, .. }
                if *op == opcodes::sopp::S_ENDPGM
                    || *op == opcodes::sopp::S_WAITCNT
                    || *op == opcodes::sopp::S_NOP =>
            {
                if *op == opcodes::sopp::S_ENDPGM {
                    return Ok(FlowEnd::Endpgm);
                }
            }
            Inst::Sop1 { op, sdst, ssrc0 } => self.emit_sop1(*op, *sdst, *ssrc0, off)?,
            Inst::Vop1 { op, vdst, src0 } => self.emit_vop1(*op, *vdst, *src0, off)?,
            Inst::Vop2 {
                op,
                vdst,
                src0,
                vsrc1,
                k,
            } => self.emit_vop2(*op, *vdst, *src0, *vsrc1, *k, off)?,
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
            } => self.emit_vop3(
                *op, *vdst, *src0, *src1, *src2, *abs, *neg, *omod, *clamp, off,
            )?,
            Inst::Vopc { op, src0, vsrc1 } => self.emit_vopc(*op, *src0, *vsrc1, off)?,
            Inst::Smrd {
                op,
                sdst,
                sbase,
                imm,
                offset,
            } => self.emit_smrd(*op, *sdst, *sbase, *imm, *offset, off)?,
            Inst::Mubuf {
                op,
                vdata,
                vaddr,
                srsrc,
                soffset,
                offset,
                idxen,
                offen,
            } => self.emit_mubuf(
                *op, *vdata, *vaddr, *srsrc, *soffset, *offset, *idxen, *offen, off,
            )?,
            Inst::Vintrp {
                op,
                vdst,
                vsrc,
                attr,
                chan,
            } => self.emit_vintrp(*op, *vdst, *vsrc, *attr, *chan, off)?,
            Inst::Mimg {
                op,
                vdata,
                vaddr,
                srsrc,
                ssamp,
                dmask,
                unrm,
            } => self.emit_mimg(*op, *vdata, *vaddr, *srsrc, *ssamp, *dmask, *unrm, off)?,
            Inst::Exp {
                target,
                srcs,
                compr,
                ..
            } => self.emit_exp(*target, srcs, *compr, off)?,
            other => {
                return Err(RecompileError::UnsupportedInst {
                    inst: Box::new(other.clone()),
                    offset: off,
                });
            }
        }
        Ok(FlowEnd::Continue)
    }

    /// Lower a multi-block CFG to structured SPIR-V.
    ///
    /// Forward-only conditional branches (`s_cbranch_vccz` / `s_cbranch_execz` + their
    /// non-zero twins) building either a single `if` or an if-else diamond (selection
    /// merge). Each GCN basic block gets one SPIR-V label; a `Cond` terminator emits an
    /// `OpSelectionMerge` naming the post-dominator merge block — the block where the
    /// two arms reconverge ([`crate::cfg::Cfg::merge_target`]) — followed by
    /// `OpBranchConditional` to the `taken`/`fall` arms. In a single `if`, the `taken`
    /// (skip) target IS the merge and one arm is empty; in a diamond, `taken`/`fall` are
    /// two arm blocks that each `OpBranch` to the distinct merge. Because recompiled
    /// SPIR-V is per-invocation (one lane), the whole-wave predicate degenerates to a
    /// single per-lane bool — the driver reconverges. The register load/store model
    /// carries values across blocks with NO hand-rolled phi, so each arm writing a
    /// different value to the same VGPR resolves as last-writer-wins across the merge.
    fn emit_cfg(&mut self, insts: &[Decoded], cfg: &crate::cfg::Cfg) -> Result<(), RecompileError> {
        use crate::cfg::{BranchCond, Terminator};

        // A CFG carrying a recognized natural loop is lowered by the dedicated,
        // structured `OpLoopMerge` path (the loops slice). `build_cfg` guarantees such a
        // CFG has exactly one back-edge and no other `Cond`, so the selection-merge path
        // below never has to cope with a loop.
        if let Some(loop_bi) = (0..cfg.blocks.len()).find(|&bi| cfg.loop_of(bi).is_some()) {
            let li = cfg.loop_of(loop_bi).expect("just found a loop block");
            return self.emit_loop_cfg(insts, cfg, li);
        }

        // Pre-allocate a SPIR-V label id per block so branches can name their targets.
        let labels: Vec<spirv::Word> = cfg.blocks.iter().map(|_| self.b.id()).collect();
        let label_at = |start: usize| -> spirv::Word {
            let idx = cfg
                .block_index_at(start)
                .expect("terminator names a real block leader");
            labels[idx]
        };

        for (bi, block) in cfg.blocks.iter().enumerate() {
            self.b
                .begin_block(Some(labels[bi]))
                .expect("cfg block label");

            // Emit the block body (all but a trailing control-flow terminator; the
            // terminator instruction itself is lowered from `block.terminator`).
            self.emit_block_body(insts, block)?;

            match &block.terminator {
                Terminator::Return => {
                    self.b.ret().expect("return");
                }
                Terminator::Fallthrough { target } | Terminator::Branch { target } => {
                    self.b.branch(label_at(*target)).expect("branch");
                }
                Terminator::Cond { cond, taken, fall } => {
                    // The per-invocation branch predicate. GCN tests the whole-wave
                    // condition against zero; per-lane it degenerates:
                    //   vccz   -> take when this lane's VCC bit is 0  (= !vcc_bool)
                    //   vccnz  -> take when this lane's VCC bit is 1  (=  vcc_bool)
                    //   execz  -> take when EXEC == 0. A running invocation is a live
                    //             lane, so EXEC is never 0 here: execz is never taken.
                    //   execnz -> the always-live twin: always taken.
                    let (pred, static_take): (Option<spirv::Word>, Option<bool>) = match cond {
                        BranchCond::Vccz => {
                            let b = self.cond_bool_for_branch(PredKey::Vcc, block)?;
                            let neg = self.b.logical_not(self.t.bool, None, b).expect("not");
                            (Some(neg), None)
                        }
                        BranchCond::Vccnz => {
                            let b = self.cond_bool_for_branch(PredKey::Vcc, block)?;
                            (Some(b), None)
                        }
                        // Per-invocation EXEC degeneracy: a running lane is always live.
                        BranchCond::Execz => (None, Some(false)),
                        BranchCond::Execnz => (None, Some(true)),
                    };

                    // The structured merge block: where the two arms reconverge. For a
                    // single forward `if` this IS the `taken` (skip) target; for an
                    // if-else diamond it is a distinct block both arms branch to. The
                    // shared CFG computes it identically for the interp oracle.
                    let merge_off = cfg.merge_target(bi).ok_or(RecompileError::Unsupported {
                        offset: block.start as u32,
                        reason: "conditional branch whose arms do not reconverge \
                                 (irreducible / non-structured control flow)",
                    })?;
                    let merge = label_at(merge_off);
                    let take_l = label_at(*taken);
                    let fall_l = label_at(*fall);

                    match (pred, static_take) {
                        (Some(pred), _) => {
                            // Structured selection: name the reconvergence (merge) block,
                            // then branch to the two arms (which flow into it).
                            self.b
                                .selection_merge(merge, spirv::SelectionControl::NONE)
                                .expect("selection merge");
                            self.b
                                .branch_conditional(pred, take_l, fall_l, [])
                                .expect("branch conditional");
                        }
                        (None, Some(true)) => {
                            self.b.branch(take_l).expect("branch (execnz always)");
                        }
                        (None, Some(false)) => {
                            self.b.branch(fall_l).expect("branch (execz never)");
                        }
                        (None, None) => unreachable!(),
                    }
                }
            }
        }
        Ok(())
    }

    /// Lower a CFG containing a single recognized natural loop to a structured SPIR-V
    /// `OpLoopMerge`. Called from [`Self::emit_cfg`] when [`crate::cfg::Cfg::loop_of`]
    /// finds a back-edge; `build_cfg` guarantees the shape (single back-edge, single
    /// exit, no other `Cond`), so the CFG is: some straight-line entry blocks → the loop
    /// header block (whose `Cond` terminator is the back-edge) → straight-line
    /// merge/exit blocks → `OpReturn`.
    ///
    /// SPIR-V structured-loop rules (spirv-val vulkan1.1, strict) require: a loop header
    /// that dominates the whole construct; an `OpLoopMerge %merge %continue None` as the
    /// header's second-to-last instruction followed by an unconditional `OpBranch`; the
    /// back-edge coming from the *continue* construct and branching to the header; the
    /// merge as the single exit. A SPIR-V loop header also cannot be the function's
    /// entry block. GCN fuses the loop body and the back-edge test into ONE block, so we
    /// SYNTHESIZE the required split into four SPIR-V labels around it:
    ///
    /// ```text
    ///   %entry:    <all entry-block bodies>; OpBranch %header
    ///   %header:   OpLoopMerge %merge %continue None; OpBranch %body
    ///   %body:     <the GCN header block's body, incl the compare that writes VCC>;
    ///              OpBranch %continue
    ///   %continue: OpBranchConditional %pred %header %merge
    ///   %merge:    <exit-block bodies>; OpReturn
    /// ```
    ///
    /// The predicate is the per-invocation back-edge continue condition: for `vccnz` the
    /// VCC bool (loop while set), for `vccz` its negation. `execnz`/`execz` degenerate
    /// per-invocation (a running lane is always live): `execnz` is an unconditional
    /// back-edge — an infinite per-lane loop we CANNOT structure to a terminating merge,
    /// so we defer it; `execz` never loops (branch straight to the merge). Loop
    /// variables (v0/v1) stay Function `OpVariable` load/store updated in `%body` and
    /// read across the back-edge — the glslang pre-mem2reg form spirv-val accepts, with
    /// NO OpPhi. `finish()` hoists those Function variables to the entry block front, so
    /// a var first touched in `%body` is still dominance-valid.
    fn emit_loop_cfg(
        &mut self,
        insts: &[Decoded],
        cfg: &crate::cfg::Cfg,
        li: crate::cfg::LoopInfo,
    ) -> Result<(), RecompileError> {
        use crate::cfg::{BranchCond, Terminator};

        let header_bi = cfg
            .block_index_at(li.back_edge_block)
            .expect("back-edge block is a real leader");
        let header_block = &cfg.blocks[header_bi];
        let (BranchCond::Vccnz | BranchCond::Vccz) = header_block_cond(header_block) else {
            // execnz/execz back-edges: per-invocation these are static (a running lane is
            // always live). execnz is an unconditional back-edge (infinite per-lane loop)
            // that has no terminating structured form; execz never loops. Neither is in
            // the corpus. Defer both cleanly rather than emit a non-terminating or
            // degenerate loop.
            return Err(RecompileError::Unsupported {
                offset: li.back_edge_block as u32,
                reason: "EXEC-conditional loop back-edge — per-invocation degenerate \
                         (infinite or never-taken); not lowered",
            });
        };
        let cond = header_block_cond(header_block);

        // Synthetic SPIR-V labels for the loop scaffold + a label per straight-line GCN
        // block (entry/merge/exit) so branches can name them.
        let block_labels: Vec<spirv::Word> = cfg.blocks.iter().map(|_| self.b.id()).collect();
        let header_l = self.b.id();
        let body_l = self.b.id();
        let continue_l = self.b.id();
        let label_at = |start: usize| -> spirv::Word {
            let idx = cfg
                .block_index_at(start)
                .expect("a terminator names a real block leader");
            block_labels[idx]
        };
        let merge_l = label_at(li.merge);

        // Emit each GCN block. The entry blocks and merge/exit blocks are straight-line;
        // the loop header block is expanded into %header/%body/%continue.
        for (bi, block) in cfg.blocks.iter().enumerate() {
            if bi == header_bi {
                // ---- %header: declare the loop, branch into the body ----------
                self.b
                    .begin_block(Some(header_l))
                    .expect("loop header label");
                self.b
                    .loop_merge(merge_l, continue_l, spirv::LoopControl::NONE, [])
                    .expect("loop merge");
                self.b.branch(body_l).expect("branch header->body");

                // ---- %body: the GCN header block's dataflow (incl the VCC compare) ----
                self.b.begin_block(Some(body_l)).expect("loop body label");
                self.emit_block_body(insts, block)?;
                self.b.branch(continue_l).expect("branch body->continue");

                // ---- %continue: the back-edge test (continue vs exit) ----------
                self.b
                    .begin_block(Some(continue_l))
                    .expect("loop continue label");
                let vcc = self.cond_bool_for_branch(PredKey::Vcc, block)?;
                let pred = match cond {
                    BranchCond::Vccnz => vcc, // loop while VCC set
                    BranchCond::Vccz => self.b.logical_not(self.t.bool, None, vcc).expect("not"),
                    _ => unreachable!("execz/execnz rejected above"),
                };
                // Continue (back to %header) when the predicate holds, else exit (%merge).
                self.b
                    .branch_conditional(pred, header_l, merge_l, [])
                    .expect("loop back-edge branch");
                continue;
            }

            // A straight-line entry/merge/exit block.
            self.b
                .begin_block(Some(block_labels[bi]))
                .expect("cfg block label");
            self.emit_block_body(insts, block)?;
            match &block.terminator {
                Terminator::Return => {
                    self.b.ret().expect("return");
                }
                Terminator::Fallthrough { target } | Terminator::Branch { target } => {
                    // A block that falls into the loop header must branch to %header (the
                    // SPIR-V loop header), not the raw GCN header leader.
                    let dst = if *target == li.header {
                        header_l
                    } else {
                        label_at(*target)
                    };
                    self.b.branch(dst).expect("branch");
                }
                // No other `Cond` exists in a validated loop CFG.
                Terminator::Cond { .. } => {
                    return Err(RecompileError::Unsupported {
                        offset: block.start as u32,
                        reason: "unexpected conditional in a loop CFG (should be validated out)",
                    });
                }
            }
        }
        Ok(())
    }

    /// Emit a block's straight-line body — every instruction it owns except a trailing
    /// control-flow op (which becomes the structured terminator). An `s_endpgm` inside
    /// the body stops the body (the block terminator is then `Return`).
    fn emit_block_body(
        &mut self,
        insts: &[Decoded],
        block: &crate::cfg::BasicBlock,
    ) -> Result<(), RecompileError> {
        for &idx in &block.insts {
            let d = &insts[idx];
            // Skip the trailing control-flow terminator instruction — it is lowered
            // from `block.terminator`, not as a dataflow op. `s_endpgm` is handled by
            // `emit_inst` returning `Endpgm` (the terminator is `Return`).
            if is_cfg_branch(d) {
                continue;
            }
            if self.emit_inst(d)? == FlowEnd::Endpgm {
                break;
            }
        }
        Ok(())
    }

    /// Resolve the predicate bool a conditional branch tests. If the predicate was
    /// produced in THIS block, its SSA id is still valid; otherwise reload it from the
    /// backing `bool` variable (cross-block). A branch on a never-written predicate
    /// defers cleanly.
    fn cond_bool_for_branch(
        &mut self,
        key: PredKey,
        _block: &crate::cfg::BasicBlock,
    ) -> Result<spirv::Word, RecompileError> {
        // Always reload from the backing variable: it is written on every predicate
        // production and is valid in any block, so this is correct whether the compare
        // was in this block or an earlier one. (A same-block SSA id would also work,
        // but the var load is uniformly valid and keeps the lowering simple.)
        self.load_predicate_var(key)
            .ok_or(RecompileError::Unsupported {
                offset: 0,
                reason: "conditional branch on a predicate never written in this shader",
            })
    }

    fn finish(mut self) -> Result<RecompiledShader, RecompileError> {
        if self.stage == ShaderStage::Vertex && !self.exports_position {
            return Err(RecompileError::Shape(
                "vertex shader exported no clip-space position (exp pos0)",
            ));
        }
        // Export one push-constant group per fetched vertex stream, now that the final
        // stream count is known (task-153).
        self.finalize_push_constants();
        // SPIR-V requires every function-local OpVariable to be at the top of the
        // FIRST block of the function (SPIR-V §2.4: "All OpVariable ... must be the
        // first instructions in the first block"). The builder appends register/
        // predicate-slot variables lazily as the stream references them, so a slot
        // first touched inside a non-entry CFG block lands in that block. Hoist every
        // Function OpVariable out of every block into the entry block's front,
        // preserving their relative (creation) order across blocks. Their initializers
        // reference only module-global constants, so relocation is always dominance-safe.
        {
            let m = self.b.module_mut();
            if let Some(func) = m.functions.first_mut() {
                // Pull all OpVariables out of every block (in block, then in-block order).
                let mut hoisted: Vec<rspirv::dr::Instruction> = Vec::new();
                for block in func.blocks.iter_mut() {
                    let mut kept = Vec::with_capacity(block.instructions.len());
                    for inst in block.instructions.drain(..) {
                        if inst.class.opcode == spirv::Op::Variable {
                            hoisted.push(inst);
                        } else {
                            kept.push(inst);
                        }
                    }
                    block.instructions = kept;
                }
                if let Some(entry) = func.blocks.first_mut() {
                    // Prepend hoisted variables (before the entry block's first real op).
                    hoisted.append(&mut entry.instructions);
                    entry.instructions = hoisted;
                }
            }
        }
        let module = self.b.module();
        let spirv = module.assemble();
        let io = IoLayout {
            stage: self.stage,
            inputs: self.io_inputs,
            outputs: self.io_outputs,
            buffers: self.io_buffers,
            const_buffers: self.io_const_buffers,
            samplers: self.io_samplers,
            push_constants: self.io_push_constants,
            exports_position: self.exports_position,
        };
        Ok(RecompiledShader { spirv, io })
    }

    // ---- constants ---------------------------------------------------------

    fn const_f32(&mut self, bits: u32) -> spirv::Word {
        if let Some(&id) = self.const_f32.get(&bits) {
            return id;
        }
        let id = self.b.constant_bit32(self.t.f32, bits);
        self.const_f32.insert(bits, id);
        id
    }

    fn const_u32(&mut self, v: u32) -> spirv::Word {
        if let Some(&id) = self.const_u32.get(&v) {
            return id;
        }
        let id = self.b.constant_bit32(self.t.u32, v);
        self.const_u32.insert(v, id);
        id
    }

    // ---- register slots ----------------------------------------------------
    //
    // Each register is a Function-storage variable. We keep a parallel f32 and u32
    // slot for the same register number so a value written as bits (v_mov, a fetch)
    // and read as float (an ALU op) round-trips through OpBitcast at the boundary.
    // To keep it simple and total, the *canonical* store is the u32 (raw bits) slot;
    // the f32 slot is a bitcast view refreshed on write. We store to both on every
    // write and read from whichever type the consumer needs.

    /// The `m0` register slot: a Function-storage u32 variable initialized to 0, so an
    /// m0 *read* before any in-shader write yields 0 — exactly the oracle's `st.m0`
    /// default. Declared lazily on first use. m0 is otherwise not consulted (VINTRP
    /// takes its attribute from the instruction field, not m0).
    fn m0_ptr(&mut self) -> spirv::Word {
        if let Some(id) = self.m0_var {
            return id;
        }
        let zero = self.const_u32(0);
        let id = self.b.variable(
            self.t.ptr_fn_u32,
            None,
            spirv::StorageClass::Function,
            Some(zero),
        );
        self.m0_var = Some(id);
        id
    }

    fn reg_u32_ptr(&mut self, is_vgpr: bool, n: u8) -> spirv::Word {
        if let Some(&id) = self.reg_u32.get(&(is_vgpr, n)) {
            return id;
        }
        // Zero-initialize the slot (`OpVariable ... %const_0`). A shader that *reads* a
        // register never written in that shader must get a defined 0 (the interp oracle
        // zero-defaults unwritten regs), not an undefined value: an undefined Function
        // read crashes RADV's ACO compiler inside vkCreateGraphicsPipelines even though
        // it passes spirv-val (task-134, doc-6 Entry 11). Mirrors `m0_ptr`.
        let zero = self.const_u32(0);
        let id = self.b.variable(
            self.t.ptr_fn_u32,
            None,
            spirv::StorageClass::Function,
            Some(zero),
        );
        self.reg_u32.insert((is_vgpr, n), id);
        id
    }

    fn reg_f32_ptr(&mut self, is_vgpr: bool, n: u8) -> spirv::Word {
        if let Some(&id) = self.reg_f32.get(&(is_vgpr, n)) {
            return id;
        }
        // Zero-initialize the f32 view (bit-pattern 0 == 0.0f) for the same reason as
        // `reg_u32_ptr`: an unwritten register must read a defined 0, not undefined
        // (task-134). Mirrors `m0_ptr`.
        let zero = self.const_f32(0.0f32.to_bits());
        let id = self.b.variable(
            self.t.ptr_fn_f32,
            None,
            spirv::StorageClass::Function,
            Some(zero),
        );
        self.reg_f32.insert((is_vgpr, n), id);
        id
    }

    /// Store raw bits into a register (updates both the u32 and f32 views).
    fn store_reg_bits(&mut self, is_vgpr: bool, n: u8, bits: spirv::Word) {
        let up = self.reg_u32_ptr(is_vgpr, n);
        self.b.store(up, bits, None, []).expect("store u32 reg");
        let fp = self.reg_f32_ptr(is_vgpr, n);
        let asf = self
            .b
            .bitcast(self.t.f32, None, bits)
            .expect("bitcast to f32");
        self.b.store(fp, asf, None, []).expect("store f32 reg");
    }

    /// Store an f32 value into a register (updates both views).
    fn store_reg_f32(&mut self, is_vgpr: bool, n: u8, val: spirv::Word) {
        let fp = self.reg_f32_ptr(is_vgpr, n);
        self.b.store(fp, val, None, []).expect("store f32 reg");
        let up = self.reg_u32_ptr(is_vgpr, n);
        let asu = self
            .b
            .bitcast(self.t.u32, None, val)
            .expect("bitcast to u32");
        self.b.store(up, asu, None, []).expect("store u32 reg");
    }

    /// Resolve a register read that still carries the launch vertex index to the
    /// `gl_VertexIndex` builtin, or `None` when it does not.
    ///
    /// The launch ABI hands a VS its vertex index in `v0` (the interp models this as
    /// `vgprs[0][lane] = first_vertex + lane`). The recompiler has no wave state to
    /// pre-load, so it resolves the index at every *read* of a register
    /// [`Self::vertex_index_regs`] still tracks — not just at the `idxen` MUBUF fetch
    /// that first motivated the tracker. A VS that derives its position from the index
    /// arithmetically (the index-driven full-screen-triangle idiom: `v_and_b32 v1, 1,
    /// v0` …) reads `v0` as a plain ALU source, and without this would read the
    /// zero-initialized `v0` slot instead — collapsing every vertex onto one point
    /// (task-184).
    ///
    /// `vertex_index_regs` is empty for a Fragment shader (whose `v0` is a barycentric,
    /// not a vertex index), so this is inert outside a VS.
    fn launch_vertex_index(&mut self, is_vgpr: bool, n: u8) -> Option<spirv::Word> {
        if !is_vgpr || !self.vertex_index_regs.contains(&n) {
            return None;
        }
        Some(self.load_vertex_index())
    }

    /// Stop tracking VGPR `n` as a carrier of the launch vertex index, SPILLING the
    /// index into the register slot first.
    ///
    /// A tracked register lives only in [`Self::vertex_index_regs`] — its Function slot
    /// still holds the zero initializer, because the index is materialized on demand at
    /// each read. Untracking without spilling therefore turns every later read of that
    /// register into a read of zero.
    ///
    /// That is not hypothetical: an in-place update reads its own destination
    /// (`v_and_b32 v0, -2, v0`), and every ALU emitter untracks the destination BEFORE
    /// evaluating its source operands. Celeste's full-screen-fill VS is exactly this
    /// shape — `v_and_b32 v1, 1, v0` (resolved) followed by `v_and_b32 v0, -2, v0` (read
    /// zero), which collapsed the Y coordinate of all three vertices onto -1 and left the
    /// triangle zero-area (task-184).
    ///
    /// The spill is a dead store when the caller overwrites the slot without reading it,
    /// and is emitted at most once per register per shader (the tracker only ever loses a
    /// register, never regains one except through a `v_mov` that writes the slot anyway).
    fn untrack_vertex_index(&mut self, n: u8) {
        if !self.vertex_index_regs.remove(&n) {
            return;
        }
        let vi = self.load_vertex_index();
        self.store_reg_bits(true, n, vi);
    }

    fn load_reg_u32(&mut self, is_vgpr: bool, n: u8) -> spirv::Word {
        if let Some(v) = self.launch_vertex_index(is_vgpr, n) {
            return v;
        }
        let p = self.reg_u32_ptr(is_vgpr, n);
        self.b
            .load(self.t.u32, None, p, None, [])
            .expect("load u32")
    }

    fn load_reg_f32(&mut self, is_vgpr: bool, n: u8) -> spirv::Word {
        // A register holding the launch index holds its raw bits; an f32 read
        // bit-reinterprets them, exactly as the oracle's `read_f32_lane` does.
        if let Some(v) = self.launch_vertex_index(is_vgpr, n) {
            return self.bitcast_f32(v);
        }
        let p = self.reg_f32_ptr(is_vgpr, n);
        self.b
            .load(self.t.f32, None, p, None, [])
            .expect("load f32")
    }

    // ---- operand evaluation -------------------------------------------------

    /// Evaluate a source operand to an f32 value id (bit-reinterpreting where the
    /// operand carries raw bits) — mirrors [`crate::interp::Interp::read_f32_lane`].
    fn eval_f32(&mut self, op: Operand, off: u32) -> Result<spirv::Word, RecompileError> {
        match op {
            Operand::Vgpr(n) => Ok(self.load_reg_f32(true, n)),
            Operand::Sgpr(n) => Ok(self.load_reg_f32(false, n)),
            Operand::InlineFloat(f) => Ok(self.const_f32(f.to_bits())),
            // An inline integer used as a float source is bit-reinterpreted, exactly
            // as the oracle does (`read_scalar` returns the bits, `read_f32_lane`
            // reinterprets them). GCN inline ints as float sources are unusual but
            // the corpus's literal-float path takes `Literal`.
            Operand::InlineInt(v) => {
                let bits = self.const_u32(v as u32);
                Ok(self.bitcast_f32(bits))
            }
            Operand::Literal(v) => {
                let bits = self.const_u32(v);
                Ok(self.bitcast_f32(bits))
            }
            Operand::Special(SpecialReg::M0) => {
                // m0 is never a float source in the subset; reinterpret its bits.
                let sp = self.special_bits(SpecialReg::M0, off)?;
                Ok(self.bitcast_f32(sp))
            }
            other => Err(RecompileError::InvalidOperand {
                inst: RecompileError::pending_inst(),
                operand: other,
                offset: off,
                reason: "not a float source",
            }),
        }
    }

    /// Evaluate a source operand to raw u32 bits — mirrors
    /// [`crate::interp::Interp::read_src_lane`].
    fn eval_bits(&mut self, op: Operand, off: u32) -> Result<spirv::Word, RecompileError> {
        match op {
            Operand::Vgpr(n) => Ok(self.load_reg_u32(true, n)),
            Operand::Sgpr(n) => Ok(self.load_reg_u32(false, n)),
            Operand::InlineInt(v) => Ok(self.const_u32(v as u32)),
            Operand::InlineFloat(f) => Ok(self.const_u32(f.to_bits())),
            Operand::Literal(v) => Ok(self.const_u32(v)),
            Operand::Special(sr) => self.special_bits(sr, off),
            other => Err(RecompileError::InvalidOperand {
                inst: RecompileError::pending_inst(),
                operand: other,
                offset: off,
                reason: "not a bit source",
            }),
        }
    }

    fn special_bits(&mut self, sr: SpecialReg, off: u32) -> Result<spirv::Word, RecompileError> {
        // m0 is never consulted for interpolation (the attribute comes from the VINTRP
        // field, per the oracle), but it IS a real scalar register for the plain
        // `s_mov m0, s` / `s_mov s, m0` save-restore idiom retail vertex shaders use.
        // The `m0_ptr` slot initializes to 0, matching the oracle's `st.m0` default, so
        // an m0 read — whether before or after an in-shader write — reproduces the
        // interp's value.
        match sr {
            SpecialReg::M0 => {
                let p = self.m0_ptr();
                Ok(self.b.load(self.t.u32, None, p, None, []).expect("load m0"))
            }
            other => Err(RecompileError::InvalidOperand {
                inst: RecompileError::pending_inst(),
                operand: Operand::Special(other),
                offset: off,
                reason: "special register not modeled in the subset",
            }),
        }
    }

    /// Validate that `op` is a well-formed scalar source (an in-range SGPR, an inline,
    /// a literal, or a modeled special), emitting no SPIR-V. Mirrors the *validation*
    /// half of [`crate::interp::Interp::read_scalar`] for a value we discard (e.g. the
    /// source of a `s_mov vcc_hi, ssrc0` prologue write, where the interp reads the
    /// source before storing but the recompiler discards the vcc write). Kept separate
    /// from `eval_bits` so validating a discarded source does not emit a dead `OpLoad`.
    fn validate_scalar_src(&self, op: Operand, off: u32) -> Result<(), RecompileError> {
        match op {
            Operand::Sgpr(n) if (n as usize) < crate::interp::NUM_SGPRS => Ok(()),
            Operand::Sgpr(n) => Err(RecompileError::InvalidRegister {
                kind: "sgpr",
                reg: n as usize,
                max: crate::interp::NUM_SGPRS,
                offset: off,
            }),
            Operand::InlineInt(_) | Operand::InlineFloat(_) | Operand::Literal(_) => Ok(()),
            // m0 is now a modeled scalar register (see `special_bits` / `m0_ptr`), so it
            // is a valid source even where the consuming write is discarded.
            Operand::Special(SpecialReg::M0) => Ok(()),
            other => Err(RecompileError::InvalidOperand {
                inst: RecompileError::pending_inst(),
                operand: other,
                offset: off,
                reason: "not a scalar source",
            }),
        }
    }

    fn bitcast_f32(&mut self, bits: spirv::Word) -> spirv::Word {
        self.b
            .bitcast(self.t.f32, None, bits)
            .expect("bitcast to f32")
    }

    // ---- SOP1 --------------------------------------------------------------

    fn emit_sop1(
        &mut self,
        op: u8,
        sdst: Operand,
        ssrc0: Operand,
        off: u32,
    ) -> Result<(), RecompileError> {
        use opcodes::sop1;
        match op {
            sop1::S_MOV_B32 => {}
            sop1::S_MOV_B64 | sop1::S_WQM_B64 => return self.emit_smov_b64(sdst, ssrc0, off),
            _ => {
                return Err(RecompileError::UnsupportedInst {
                    inst: Box::new(Inst::Sop1 { op, sdst, ssrc0 }),
                    offset: off,
                });
            }
        }
        match sdst {
            Operand::Sgpr(n) => {
                let bits = self.eval_bits(ssrc0, off)?;
                self.store_reg_bits(false, n, bits);
            }
            // `s_mov m0, s0`: m0 holds the interpolation base on real GCN, which neither
            // the recompiler nor the oracle consults (the attribute comes from the VINTRP
            // field). But m0 IS a real scalar register: retail vertex shaders save it
            // (`s_mov s, m0`) and restore it, so we store the write into the `m0_ptr`
            // slot to keep that idiom a faithful copy (the oracle keeps `st.m0` too).
            Operand::Special(SpecialReg::M0) => {
                let bits = self.eval_bits(ssrc0, off)?;
                let p = self.m0_ptr();
                self.b.store(p, bits, None, []).expect("store m0");
            }
            // `s_mov_b32 vcc_hi, <imm>` is the Orbis shader prologue every retail `.sb`
            // opens with (RE'd from a managed-runtime title — 22/22 shaders start with it).
            // It stashes a constant into VCC that these shaders never read back. VCC is not
            // modeled as a slot, so we validate the source and discard the write. If a later
            // instruction ever *reads* vcc, `special_bits` rejects it (a clean defer), so
            // discarding here can only ever be lossless or safely deferred.
            Operand::Special(SpecialReg::VccLo | SpecialReg::VccHi) => {
                self.validate_scalar_src(ssrc0, off)?;
            }
            other => {
                return Err(RecompileError::InvalidOperand {
                    inst: RecompileError::pending_inst(),
                    operand: other,
                    offset: off,
                    reason: "not a scalar destination",
                });
            }
        }
        Ok(())
    }

    /// `s_mov_b64` / `s_wqm_b64` — 64-bit scalar moves the retail pixel shaders use to
    /// save EXEC before a whole-quad-mode region and restore it after. Per-invocation
    /// SPIR-V has no wave EXEC mask, and we treat every invocation's quad as fully
    /// covered, so the save/WQM/restore bracket is transparent to the exported result:
    /// the saved value flows only back to EXEC on restore. When EXEC is an operand we
    /// validate both sides and discard (the oracle's EXEC round-trips through the same
    /// bracket, so the two agree on exports). A non-EXEC pair move is a real copy.
    fn emit_smov_b64(
        &mut self,
        sdst: Operand,
        ssrc0: Operand,
        off: u32,
    ) -> Result<(), RecompileError> {
        let is_exec =
            |o: Operand| matches!(o, Operand::Special(SpecialReg::ExecLo | SpecialReg::ExecHi));
        if is_exec(sdst) || is_exec(ssrc0) {
            self.validate_pair_operand(sdst, off)?;
            self.validate_pair_operand(ssrc0, off)?;
            return Ok(());
        }
        match (sdst, ssrc0) {
            (Operand::Sgpr(d), Operand::Sgpr(s)) => {
                self.validate_pair_operand(sdst, off)?;
                self.validate_pair_operand(ssrc0, off)?;
                // Read BOTH source dwords before writing either: a 64-bit scalar move is
                // atomic on hardware, so an overlapping pair (e.g. `s_mov_b64 s[3:4],
                // s[2:3]`, dst.lo == src.hi) must see the OLD high dword. Storing lo first
                // would clobber src.hi before it is read, diverging from the interp oracle
                // (read_scalar_pair reads the pair up front). Matches the whole-pair
                // read-then-write the oracle performs.
                let lo = self.load_reg_u32(false, s);
                let hi = self.load_reg_u32(false, s + 1);
                self.store_reg_bits(false, d, lo);
                self.store_reg_bits(false, d + 1, hi);
                Ok(())
            }
            _ => Err(RecompileError::Unsupported {
                offset: off,
                reason: "64-bit scalar move outside the EXEC-save/restore subset",
            }),
        }
    }

    /// Validate a 64-bit scalar operand: an SGPR pair `[n:n+1]` in range, or a modeled
    /// 64-bit special register (EXEC/VCC low half names the pair).
    fn validate_pair_operand(&self, op: Operand, off: u32) -> Result<(), RecompileError> {
        match op {
            Operand::Special(SpecialReg::ExecLo | SpecialReg::VccLo) => Ok(()),
            Operand::Sgpr(n) => {
                let hi = n as usize + 1;
                if hi >= crate::interp::NUM_SGPRS {
                    return Err(RecompileError::InvalidRegister {
                        kind: "sgpr",
                        reg: hi,
                        max: crate::interp::NUM_SGPRS,
                        offset: off,
                    });
                }
                Ok(())
            }
            other => Err(RecompileError::InvalidOperand {
                inst: RecompileError::pending_inst(),
                operand: other,
                offset: off,
                reason: "not a 64-bit scalar operand",
            }),
        }
    }

    // ---- VOP1 --------------------------------------------------------------

    fn emit_vop1(
        &mut self,
        op: u8,
        vdst: Operand,
        src0: Operand,
        off: u32,
    ) -> Result<(), RecompileError> {
        use opcodes::vop1::*;
        let n = self.vgpr_dst(vdst, off)?;
        // A `v_mov` from a currently-tracked reg (captured before the write) propagates
        // the launch-vertex-index tracking to the dst; every other VOP1 write (cvt, or a
        // move from an untracked source) clobbers it. A stale-tracked dst would make a
        // later idxen fetch read `gl_VertexIndex` (the unmodified launch index) instead
        // of the computed value, diverging silently from the interp (which reads the
        // actual VGPR).
        let src_is_tracked_index =
            matches!(src0, Operand::Vgpr(m) if self.vertex_index_regs.contains(&m));
        match op {
            V_MOV_B32 => {
                // Source first (a self-move `v_mov_b32 v0, v0` must read the index, not
                // the slot), then re-track only if the source carried the index.
                let bits = self.eval_bits(src0, off)?;
                self.untrack_vertex_index(n);
                if src_is_tracked_index {
                    self.vertex_index_regs.insert(n);
                }
                self.store_reg_bits(true, n, bits);
            }
            V_CVT_F32_I32 => {
                // Arithmetic write: clobbers any launch-vertex-index tracking on the dst.
                self.untrack_vertex_index(n);
                let bits = self.eval_bits(src0, off)?;
                let asi = self.b.bitcast(self.t.i32, None, bits).expect("bitcast i32");
                let f = self.b.convert_s_to_f(self.t.f32, None, asi).expect("s->f");
                self.store_reg_f32(true, n, f);
            }
            V_CVT_F32_U32 => {
                self.untrack_vertex_index(n);
                let bits = self.eval_bits(src0, off)?;
                let f = self.b.convert_u_to_f(self.t.f32, None, bits).expect("u->f");
                self.store_reg_f32(true, n, f);
            }
            V_CVT_U32_F32 => {
                // Saturating float→u32 (NaN → 0, out-of-range → clamp), matching the
                // oracle's `f32 as u32`; see `emit_cvt_f_to_int` for why a bare
                // OpConvertFToU would diverge.
                self.untrack_vertex_index(n);
                let f = self.eval_f32(src0, off)?;
                let bits = self.emit_cvt_f_to_int(f, false);
                self.store_reg_bits(true, n, bits);
            }
            V_CVT_I32_F32 => {
                // Saturating float→i32 (NaN → 0, out-of-range → i32::MIN/MAX), matching
                // the oracle's `f32 as i32`; see `emit_cvt_f_to_int`.
                self.untrack_vertex_index(n);
                let f = self.eval_f32(src0, off)?;
                let bits = self.emit_cvt_f_to_int(f, true);
                self.store_reg_bits(true, n, bits);
            }
            // v_fract = x - floor(x), clamped to [0,1) to match the oracle (see
            // emit_fract). GLSL/Metal Fract can return exactly 1.0 for small-negative x.
            V_FRACT_F32 => {
                self.untrack_vertex_index(n);
                let f = self.eval_f32(src0, off)?;
                let r = self.emit_fract(f);
                self.store_reg_f32(true, n, r);
            }
            // Transcendentals via GLSL.std.450 (correctly-rounded / exact, matching the
            // oracle). GCN's v_sqrt is an approximate macro on hardware, but both sides
            // use the IEEE-rounded Sqrt so they agree.
            V_FLOOR_F32 | V_SQRT_F32 => {
                self.untrack_vertex_index(n);
                let f = self.eval_f32(src0, off)?;
                let ext = if op == V_FLOOR_F32 {
                    glsl::FLOOR
                } else {
                    glsl::SQRT
                };
                let r = self
                    .b
                    .ext_inst(self.t.f32, None, self.glsl_ext, ext, [DrOperand::IdRef(f)])
                    .expect("vop1 transcendental");
                self.store_reg_f32(true, n, r);
            }
            // v_ceil_f32 = ceil(src0), via GLSL.std.450 Ceil (mirrors the V_FLOOR arm).
            V_CEIL_F32 => {
                self.untrack_vertex_index(n);
                let f = self.eval_f32(src0, off)?;
                let r = self.glsl1(glsl::CEIL, f);
                self.store_reg_f32(true, n, r);
            }
            // v_cvt_off_f32_i4: D.f = float(sext4(src0[3:0])) / 16.0. The low 4 bits are a
            // signed [-8,7] integer mapped to the pixel-offset table (−0.5 … 0.4375 in 1/16
            // steps) — sign-extract 4 bits, convert to float, scale by the exact 1/16. The
            // interp models the identical i4/16 (exact power-of-two divisor → bit-for-bit).
            V_CVT_OFF_F32_I4 => {
                self.untrack_vertex_index(n);
                let bits = self.eval_bits(src0, off)?;
                let as_i = self.b.bitcast(self.t.i32, None, bits).expect("bitcast i32");
                let zero = self.const_u32(0);
                let four = self.const_u32(4);
                let i4 = self
                    .b
                    .bit_field_s_extract(self.t.i32, None, as_i, zero, four)
                    .expect("cvt_off sext4");
                let f = self.b.convert_s_to_f(self.t.f32, None, i4).expect("s->f");
                let inv16 = self.const_f32((1.0f32 / 16.0).to_bits());
                let r = self
                    .b
                    .f_mul(self.t.f32, None, f, inv16)
                    .expect("cvt_off scale");
                self.store_reg_f32(true, n, r);
            }
            // GCN v_rcp is an approximate macro on hardware; emit the exact OpFDiv
            // 1.0/x (correctly rounded), matching the oracle's 1.0/x. GLSL.std.450 has
            // no reciprocal, so a division is the portable equivalent.
            V_RCP_F32 => {
                self.untrack_vertex_index(n);
                let x = self.eval_f32(src0, off)?;
                let one = self.const_f32(1.0f32.to_bits());
                let r = self
                    .b
                    .f_div(self.t.f32, None, one, x)
                    .expect("vop1 rcp fdiv");
                self.store_reg_f32(true, n, r);
            }
            // GCN sine takes revolutions: D = sin(2*PI*S0). Scale by the f32 TAU (same
            // constant the oracle uses) then GLSL Sin. NOTE: GLSL Sin is only ULP-bounded
            // (implementation-defined), not correctly rounded, so this agrees with the
            // oracle (host libm sinf) only to the driver's Sin ULP budget, not bit-for-bit.
            V_SIN_F32 => {
                self.untrack_vertex_index(n);
                let x = self.eval_f32(src0, off)?;
                let tau = self.const_f32(std::f32::consts::TAU.to_bits());
                let scaled = self.b.f_mul(self.t.f32, None, x, tau).expect("sin tau mul");
                let r = self
                    .b
                    .ext_inst(
                        self.t.f32,
                        None,
                        self.glsl_ext,
                        glsl::SIN,
                        [DrOperand::IdRef(scaled)],
                    )
                    .expect("vop1 sin");
                self.store_reg_f32(true, n, r);
            }
            _ => {
                return Err(RecompileError::UnsupportedInst {
                    inst: Box::new(Inst::Vop1 { op, vdst, src0 }),
                    offset: off,
                });
            }
        }
        Ok(())
    }

    // ---- VOP2 --------------------------------------------------------------

    fn emit_vop2(
        &mut self,
        op: u8,
        vdst: Operand,
        src0: Operand,
        vsrc1: Operand,
        k: Option<u32>,
        off: u32,
    ) -> Result<(), RecompileError> {
        use opcodes::vop2::*;
        let n = self.vgpr_dst(vdst, off)?;
        // Any VOP2 write is arithmetic (never a vertex-index-preserving move), so the
        // dst no longer carries the launch vertex index: untrack it. Leaving it tracked
        // would make a later idxen fetch read `gl_VertexIndex` (the unmodified launch
        // index) instead of this computed value — silent divergence from the interp,
        // which reads the actual VGPR.
        self.untrack_vertex_index(n);
        // Integer/bitwise ops read and write raw bits (mirrors interp's read_src_lane
        // path). GCN masks the v_lshlrev shift to the low 5 bits; SPIR-V leaves an
        // out-of-range shift undefined, so the mask is load-bearing for agreement.
        match op {
            V_LSHLREV_B32 => {
                // D = S1 << S0[4:0] (reversed: shift is src0, value is vsrc1).
                let shift = self.eval_bits(src0, off)?;
                let val = self.eval_bits(vsrc1, off)?;
                let mask = self.const_u32(0x1f);
                let sh = self
                    .b
                    .bitwise_and(self.t.u32, None, shift, mask)
                    .expect("and");
                let out = self
                    .b
                    .shift_left_logical(self.t.u32, None, val, sh)
                    .expect("shl");
                self.store_reg_bits(true, n, out);
                return Ok(());
            }
            V_LSHRREV_B32 => {
                // D = S1 >> S0[4:0] (unsigned/logical; reversed operands). Mask the
                // shift as SPIR-V leaves an out-of-range shift undefined.
                let shift = self.eval_bits(src0, off)?;
                let val = self.eval_bits(vsrc1, off)?;
                let mask = self.const_u32(0x1f);
                let sh = self
                    .b
                    .bitwise_and(self.t.u32, None, shift, mask)
                    .expect("and");
                let out = self
                    .b
                    .shift_right_logical(self.t.u32, None, val, sh)
                    .expect("shr");
                self.store_reg_bits(true, n, out);
                return Ok(());
            }
            V_AND_B32 => {
                let a = self.eval_bits(src0, off)?;
                let b = self.eval_bits(vsrc1, off)?;
                let out = self.b.bitwise_and(self.t.u32, None, a, b).expect("and");
                self.store_reg_bits(true, n, out);
                return Ok(());
            }
            V_ADD_I32 => {
                // D = S0 + S1 (32-bit wrapping); carry-OUT to VCC. SPIR-V OpIAdd wraps
                // (two's-complement modulo 2^32), matching the oracle's wrapping add.
                // The unsigned carry is `(a + b) < a` (OpULessThan), stored as the VCC
                // predicate bool — only consumed if a later op reads VCC.
                let a = self.eval_bits(src0, off)?;
                let b = self.eval_bits(vsrc1, off)?;
                let sum = self.b.i_add(self.t.u32, None, a, b).expect("iadd");
                let carry = self
                    .b
                    .u_less_than(self.t.bool, None, sum, a)
                    .expect("carry");
                self.set_predicate(PredKey::Vcc, carry);
                self.store_reg_bits(true, n, sum);
                return Ok(());
            }
            V_CNDMASK_B32 => {
                // D = VCC ? S1 : S0 — per-invocation OpSelect on the VCC predicate bool.
                let s0 = self.eval_bits(src0, off)?;
                let s1 = self.eval_bits(vsrc1, off)?;
                let pred = self.load_predicate_bool(Operand::Special(SpecialReg::VccLo), off)?;
                let out = self
                    .b
                    .select(self.t.u32, None, pred, s1, s0)
                    .expect("cndmask select");
                self.store_reg_bits(true, n, out);
                return Ok(());
            }
            V_CVT_PKRTZ_F16_F32 => {
                // D[15:0] = f16(src0), D[31:16] = f16(vsrc1). GLSL PackHalf2x16 packs
                // vec2(x,y) as {f16(x) low, f16(y) high} with round-to-nearest-even —
                // exactly the interp oracle's `half::f16::from_f32` (see interp's
                // exec_vop2 note on the RTZ→RNE modeling choice).
                let x = self.eval_f32(src0, off)?;
                let y = self.eval_f32(vsrc1, off)?;
                // v2f32 declared lazily (rspirv dedups): modules that never pack f16
                // don't carry an unused OpTypeVector.
                let v2f32 = self.b.type_vector(self.t.f32, 2);
                let vec = self
                    .b
                    .composite_construct(v2f32, None, [x, y])
                    .expect("vec2");
                let packed = self
                    .b
                    .ext_inst(
                        self.t.u32,
                        None,
                        self.glsl_ext,
                        glsl::PACK_HALF_2X16,
                        [DrOperand::IdRef(vec)],
                    )
                    .expect("packHalf2x16");
                self.store_reg_bits(true, n, packed);
                return Ok(());
            }
            _ => {}
        }
        // The uniformly-f32 VOP2 ops (add/sub/mul/min/max/mac/madmk/madak) are shared
        // with the interp oracle via the uop layer, so the ALU semantics are written
        // once. `Val` here is an f32-typed SPIR-V id; each `AluBuilder` method emits
        // the IDENTICAL builder/GLSL call the hand arm did. Anything else is unsupported.
        if !crate::uop::is_uop_vop2(op) {
            return Err(RecompileError::UnsupportedInst {
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
        let a = self.eval_f32(src0, off)?;
        let b = self.eval_f32(vsrc1, off)?;
        // dst_old is only consumed by v_mac (the accumulator). Emit the OpLoad ONLY for
        // mac so a non-mac op never carries a dead OpLoad that would perturb the golden
        // disasm; the dummy id is never referenced by the shared body for other ops.
        let dst_old = if op == opcodes::vop2::V_MAC_F32 {
            self.load_reg_f32(true, n)
        } else {
            0
        };
        let out = crate::uop::eval_vop2(self, op, a, b, dst_old, k);
        self.store_reg_f32(true, n, out);
        Ok(())
    }

    // ---- VOP3 --------------------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    fn emit_vop3(
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
    ) -> Result<(), RecompileError> {
        use opcodes::vop3::*;
        // VOP3-form VOPC: an f32 compare whose bool lands in the ARBITRARY predicate
        // destination (VCC or an SGPR pair) the decoder placed in `vdst` — NOT a VGPR,
        // so this must run BEFORE the `vgpr_dst` check below. abs/neg fold in.
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
            let a0 = self.eval_f32(src0, off)?;
            let b0 = self.eval_f32(src1, off)?;
            let a = self.apply_mods(a0, abs, neg, 0);
            let b = self.apply_mods(b0, abs, neg, 1);
            let bool_id = self.emit_f32_compare(cmp, a, b, off)?;
            let key = Self::predicate_dst_key(vdst, off)?;
            self.set_predicate(key, bool_id);
            return Ok(());
        }
        // VOP3-form VOPC integer compare: `S0 <cmp> S1` (i32/u32), bool into the
        // ARBITRARY predicate destination (VCC or an SGPR pair) in `vdst`. The float
        // abs/neg/omod/clamp modifiers do not apply to an integer compare. Runs BEFORE
        // the `vgpr_dst` check below (the dst is an SGPR pair, not a VGPR). A later
        // v_cndmask reads the stored predicate and lowers to OpSelect — the switch/case
        // colour selector Celeste's background PS uses.
        if Self::is_int_compare_vop3(op) {
            let a = self.eval_bits(src0, off)?;
            let b = self.eval_bits(src1, off)?;
            let bool_id = self.emit_int_compare(op, a, b, off)?;
            let key = Self::predicate_dst_key(vdst, off)?;
            self.set_predicate(key, bool_id);
            return Ok(());
        }
        let n = self.vgpr_dst(vdst, off)?;
        // v_cndmask_b32 (VOP3 form): D = src2[pred] ? S1 : S0. The predicate is the
        // arbitrary SGPR pair (or VCC) named by src2. Bits sources, so OpSelect on u32.
        if op == V_CNDMASK_B32 {
            self.untrack_vertex_index(n);
            let s0 = self.eval_bits(src0, off)?;
            let s1 = self.eval_bits(src1, off)?;
            let pred = self.load_predicate_bool(src2, off)?;
            let out = self
                .b
                .select(self.t.u32, None, pred, s1, s0)
                .expect("vop3 cndmask select");
            self.store_reg_bits(true, n, out);
            return Ok(());
        }
        // Any VOP3 write in the subset is arithmetic (no VOP3-encoded v_mov is lowered
        // here), so the dst no longer carries the launch vertex index: untrack it — see
        // the VOP2 note above for why a stale-tracked dst would silently diverge.
        self.untrack_vertex_index(n);
        // v_mad_u32_u24 is INTEGER: read raw bits, mask src0/src1 to 24 bits, multiply,
        // add src2 (32-bit wrapping). The float abs/neg/omod modifiers do not apply, and
        // neither does clamp — GFX7 has no integer clamping (llvm-mc -mcpu=bonaire:
        // "integer clamping is not supported on this GPU"), so the bit cannot be set here.
        if op == V_MAD_U32_U24 {
            let a = self.eval_bits(src0, off)?;
            let b = self.eval_bits(src1, off)?;
            let c = self.eval_bits(src2, off)?;
            let mask = self.const_u32(0x00FF_FFFF);
            let am = self
                .b
                .bitwise_and(self.t.u32, None, a, mask)
                .expect("mad and0");
            let bm = self
                .b
                .bitwise_and(self.t.u32, None, b, mask)
                .expect("mad and1");
            let prod = self.b.i_mul(self.t.u32, None, am, bm).expect("mad imul");
            let sum = self.b.i_add(self.t.u32, None, prod, c).expect("mad iadd");
            self.store_reg_bits(true, n, sum);
            return Ok(());
        }
        // v_cvt_pkrtz_f16_f32 (VOP3 form) packs src0/src1 as two f16 into a u32 — same
        // GLSL PackHalf2x16 (RNE) as the VOP2 form; abs/neg fold into the inputs.
        if op == V_CVT_PKRTZ_F16_F32 {
            let x0 = self.eval_f32(src0, off)?;
            let y0 = self.eval_f32(src1, off)?;
            let x = self.apply_mods(x0, abs, neg, 0);
            let y = self.apply_mods(y0, abs, neg, 1);
            let v2f32 = self.b.type_vector(self.t.f32, 2);
            let vec = self
                .b
                .composite_construct(v2f32, None, [x, y])
                .expect("vec2");
            let packed = self
                .b
                .ext_inst(
                    self.t.u32,
                    None,
                    self.glsl_ext,
                    glsl::PACK_HALF_2X16,
                    [DrOperand::IdRef(vec)],
                )
                .expect("packHalf2x16");
            self.store_reg_bits(true, n, packed);
            return Ok(());
        }
        // The uniformly-f32 VOP3 ops (mul/mac/mad/fma/med3/fract) — incl. the abs/neg
        // src modifiers and the omod/clamp output chain — are shared with the interp oracle
        // via the uop layer, so the ALU semantics are written once. `Val` is an
        // f32-typed SPIR-V id; each `AluBuilder` method emits the IDENTICAL builder/GLSL
        // call the hand arm did. Anything else in VOP3 is unsupported here.
        if !crate::uop::is_uop_vop3(op) {
            return Err(RecompileError::UnsupportedInst {
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
        let a0 = self.eval_f32(src0, off)?;
        let b0 = self.eval_f32(src1, off)?;
        let c0 = self.eval_f32(src2, off)?;
        let a = crate::uop::apply_mods(self, a0, abs, neg, 0);
        let b = crate::uop::apply_mods(self, b0, abs, neg, 1);
        let c = crate::uop::apply_mods(self, c0, abs, neg, 2);
        // dst_old is only read by v_mac (the accumulator). Emit the OpLoad ONLY for mac
        // (no dead load perturbing other ops); the dummy id is never referenced
        // otherwise. med3's FMAX/FMIN emission order is pinned by the shared `median3`.
        let dst_old = if op == opcodes::vop3::V_MAC_F32 {
            self.load_reg_f32(true, n)
        } else {
            0
        };
        let raw = crate::uop::eval_vop3(self, op, a, b, c, dst_old);
        // Output-modifier chain, in hardware order: omod scales, THEN clamp saturates.
        let scaled = crate::uop::apply_omod(self, raw, omod);
        let out = crate::uop::apply_clamp(self, scaled, clamp);
        self.store_reg_f32(true, n, out);
        Ok(())
    }

    /// Apply the VOP3 src abs/neg modifiers (abs first, then neg) via the shared uop
    /// layer, so the non-uop callers (the VOP3-form compares and `v_cvt_pkrtz`) use the
    /// same single implementation as the uniformly-f32 VOP3 body. Emits the identical
    /// `FABS` / `OpFNegate` the hand path did (see `AluBuilder for Recompiler`).
    fn apply_mods(&mut self, v: spirv::Word, abs: u8, neg: u8, idx: u8) -> spirv::Word {
        crate::uop::apply_mods(self, v, abs, neg, idx)
    }

    fn glsl1(&mut self, inst: u32, a: spirv::Word) -> spirv::Word {
        self.b
            .ext_inst(self.t.f32, None, self.glsl_ext, inst, [DrOperand::IdRef(a)])
            .expect("glsl1")
    }

    /// Emit `fract(x)` clamped to `[0, 1)`, matching the interp oracle's `fract_f32`.
    /// GLSL/Metal `Fract` is defined as `x - floor(x)`, which rounds up to exactly `1.0`
    /// for a small-negative `x` (e.g. `-1e-8`); clamp to the largest f32 below 1.0 so the
    /// SPIR-V agrees with the oracle bit-for-bit (esp. on MoltenVK, where the driver does
    /// NOT clamp). Ordered `>= 1.0` preserves NaN (mirrors the oracle's `>= 1.0` test).
    fn emit_fract(&mut self, x: spirv::Word) -> spirv::Word {
        let f = self.glsl1(glsl::FRACT, x);
        let one = self.const_f32(0x3f80_0000); // 1.0
        let almost_one = self.const_f32(0x3f7f_ffff); // 0.999_999_94, largest f32 < 1.0
        let ge = self
            .b
            .f_ord_greater_than_equal(self.t.bool, None, f, one)
            .expect("fract >= 1.0");
        self.b
            .select(self.t.f32, None, ge, almost_one, f)
            .expect("fract clamp select")
    }

    /// Lower `v_cvt_u32_f32` (`signed == false`) / `v_cvt_i32_f32` (`signed == true`)
    /// with the interp's *saturating* float→int semantics, and return the u32 result
    /// bits. The oracle is Rust's `f32 as u32` / `f32 as i32`, which since Rust 1.45 is
    /// saturating: NaN → 0, and out-of-range → the integer's min/max. A bare
    /// `OpConvertFToU` / `OpConvertFToS` is *undefined* for NaN or an out-of-range value
    /// (SPIR-V 1.0, OpConvertFToU/OpConvertFToS: the value must be representable in the
    /// result type, else the result is undefined), so on-device it would be a
    /// driver-defined value diverging from the CPU oracle. To match: map NaN to 0, clamp
    /// into the band that converts in range, convert (truncates toward zero), then pin
    /// the top of the range to the exact integer max — the clamp bound is the largest
    /// f32 strictly below 2^N, which converts to less than the integer max.
    fn emit_cvt_f_to_int(&mut self, f: spirv::Word, signed: bool) -> spirv::Word {
        // NaN → 0.0 first: the SPIR-V core min/max do not guarantee a NaN operand is
        // dropped, so removing NaN here keeps the convert's input always in range.
        let is_num = self
            .b
            .f_ord_equal(self.t.bool, None, f, f)
            .expect("cvt nan test");
        let zero_f = self.const_f32(0);
        let f_safe = self
            .b
            .select(self.t.f32, None, is_num, f, zero_f)
            .expect("cvt nan->0");
        // (low clamp, high clamp = largest f32 < 2^N, saturation threshold = 2^N, int max)
        let (lo_bits, hi_bits, thr_bits, int_max) = if signed {
            // i32: [-2^31 .. 2^31-128] converts in range; x >= 2^31 → i32::MAX.
            (
                (-2147483648.0f32).to_bits(), // -2^31 (== i32::MIN, exact)
                0x4EFF_FFFF,                  // 2147483520.0 = 2^31 - 128, largest f32 < 2^31
                0x4F00_0000,                  // 2147483648.0 = 2^31
                i32::MAX as u32,              // 0x7FFF_FFFF
            )
        } else {
            // u32: [0 .. 2^32-256] converts in range; x >= 2^32 → u32::MAX.
            (
                0,           // 0.0
                0x4F7F_FFFF, // 4294967040.0 = 2^32 - 256, largest f32 < 2^32
                0x4F80_0000, // 4294967296.0 = 2^32
                u32::MAX,    // 0xFFFF_FFFF
            )
        };
        let lo = self.const_f32(lo_bits);
        let hi = self.const_f32(hi_bits);
        let clamped_lo = self.glsl2(glsl::FMAX, f_safe, lo);
        let clamped = self.glsl2(glsl::FMIN, clamped_lo, hi);
        let conv = if signed {
            let i = self
                .b
                .convert_f_to_s(self.t.i32, None, clamped)
                .expect("f->s");
            self.b.bitcast(self.t.u32, None, i).expect("bitcast u32")
        } else {
            self.b
                .convert_f_to_u(self.t.u32, None, clamped)
                .expect("f->u")
        };
        // Above the high clamp the convert undershoots the integer max, so pin it to the
        // exact max when the ORIGINAL value reaches 2^N (ordered compare: NaN is false,
        // and NaN was already mapped to 0, so it keeps its 0 convert).
        let thr = self.const_f32(thr_bits);
        let ge_max = self
            .b
            .f_ord_greater_than_equal(self.t.bool, None, f, thr)
            .expect("cvt saturate test");
        let int_max = self.const_u32(int_max);
        self.b
            .select(self.t.u32, None, ge_max, int_max, conv)
            .expect("cvt saturate")
    }

    fn glsl2(&mut self, inst: u32, a: spirv::Word, b: spirv::Word) -> spirv::Word {
        self.b
            .ext_inst(
                self.t.f32,
                None,
                self.glsl_ext,
                inst,
                [DrOperand::IdRef(a), DrOperand::IdRef(b)],
            )
            .expect("glsl2")
    }

    // ---- predication / VCC family ------------------------------------------
    //
    // Per-invocation SPIR-V is ONE lane, so a VOPC / v_add_i32 predicate is a single
    // `bool` (not a 64-bit wave mask). We record the bool value id under its
    // destination key; a later v_cndmask reads it back and lowers to OpSelect. This is
    // straight-line (no CFG), so "the most-recent store" is the faithful value.

    /// Resolve a predicate DESTINATION operand (VCC or an SGPR pair) to its key.
    fn predicate_dst_key(dst: Operand, off: u32) -> Result<PredKey, RecompileError> {
        match dst {
            Operand::Special(SpecialReg::VccLo | SpecialReg::VccHi) => Ok(PredKey::Vcc),
            Operand::Sgpr(n) => Ok(PredKey::SgprPair(n)),
            other => Err(RecompileError::InvalidOperand {
                inst: RecompileError::pending_inst(),
                operand: other,
                offset: off,
                reason: "not a predicate (VCC / SGPR-pair) destination",
            }),
        }
    }

    /// Resolve a predicate SOURCE operand (VCC or an SGPR pair) to its current bool.
    /// Reloads from the backing `bool` variable (via [`Self::load_predicate_var`]), NOT
    /// the same-block SSA cache in `self.predicates`: a predicate produced in an earlier
    /// or sibling block has an SSA id that does not dominate this v_cndmask, so
    /// referencing it directly emits a module spirv-val rejects ("ID used in a block not
    /// dominated by its definition"), failing pipeline creation. The variable is stored
    /// on every predicate production and loads validly in any block — the same
    /// cross-block-safe read `cond_bool_for_branch` uses. A read of an unwritten
    /// predicate is a clean defer, not a fabricated 0 (which would silently diverge from
    /// the oracle's real mask bit).
    fn load_predicate_bool(
        &mut self,
        src: Operand,
        off: u32,
    ) -> Result<spirv::Word, RecompileError> {
        let key = Self::predicate_dst_key(src, off)?;
        self.load_predicate_var(key)
            .ok_or(RecompileError::Unsupported {
                offset: off,
                reason: "v_cndmask reads a predicate never written in this shader",
            })
    }

    /// The `bool` Function OpVariable backing predicate `key`, declared lazily and
    /// zero-initialized (an unread predicate reads `false`). See [`Self::pred_vars`].
    fn pred_var(&mut self, key: PredKey) -> spirv::Word {
        if let Some(&id) = self.pred_vars.get(&key) {
            return id;
        }
        let ptr_ty = self.ptr_fn_bool();
        let f = self.b.constant_false(self.t.bool);
        let id = self
            .b
            .variable(ptr_ty, None, spirv::StorageClass::Function, Some(f));
        self.pred_vars.insert(key, id);
        id
    }

    /// The lazily-declared pointer-to-`bool` (Function storage) type.
    fn ptr_fn_bool(&mut self) -> spirv::Word {
        if let Some(id) = self.ptr_fn_bool {
            return id;
        }
        let id = self
            .b
            .type_pointer(None, spirv::StorageClass::Function, self.t.bool);
        self.ptr_fn_bool = Some(id);
        id
    }

    /// Record a freshly produced predicate bool: cache the SSA id (for a same-block
    /// v_cndmask) AND store it into the key's backing `bool` variable (so a later
    /// block's conditional branch can reload it).
    fn set_predicate(&mut self, key: PredKey, bool_id: spirv::Word) {
        self.predicates.insert(key, bool_id);
        let var = self.pred_var(key);
        self.b
            .store(var, bool_id, None, [])
            .expect("store pred var");
    }

    /// Load a predicate bool from its backing variable — the cross-block-safe read a
    /// conditional branch uses (its predicate may have been produced in an earlier
    /// block). Returns `None` if the key was never written.
    fn load_predicate_var(&mut self, key: PredKey) -> Option<spirv::Word> {
        let var = *self.pred_vars.get(&key)?;
        Some(
            self.b
                .load(self.t.bool, None, var, None, [])
                .expect("load pred var"),
        )
    }

    /// VOPC (standalone): an f32 compare whose bool lands in VCC. abs/neg do not apply
    /// (the standalone VOPC has no modifiers). Records the bool under the VCC key.
    fn emit_vopc(
        &mut self,
        op: u8,
        src0: Operand,
        vsrc1: Operand,
        off: u32,
    ) -> Result<(), RecompileError> {
        // A standalone VOPC integer compare (`vopc_0x82 vcc, 1, v6`) writes its lane
        // mask to VCC. The VOPC op field equals the VOP3-VOPC op number, so reuse the
        // shared integer-compare emitter (raw u32 bits, sign carried by the SPIR-V op).
        if Self::is_int_compare_vop3(u16::from(op)) {
            let a = self.eval_bits(src0, off)?;
            let b = self.eval_bits(vsrc1, off)?;
            let bool_id = self.emit_int_compare(u16::from(op), a, b, off)?;
            self.set_predicate(PredKey::Vcc, bool_id);
            return Ok(());
        }
        let a = self.eval_f32(src0, off)?;
        let b = self.eval_f32(vsrc1, off)?;
        let bool_id = self.emit_f32_compare(op, a, b, off)?;
        self.set_predicate(PredKey::Vcc, bool_id);
        Ok(())
    }

    /// Emit the SPIR-V bool for an f32 compare `op` on `a`/`b`. Mirrors the oracle's
    /// `eval_f32_compare`; an unmodeled compare defers cleanly.
    fn emit_f32_compare(
        &mut self,
        op: u8,
        a: spirv::Word,
        b: spirv::Word,
        off: u32,
    ) -> Result<spirv::Word, RecompileError> {
        use opcodes::vopc::*;
        let bool_ty = self.t.bool;
        // Ordered compares (GCN's `_f32` compares are ordered: a NaN operand yields
        // false), matching Rust's `<`/`==`/… on non-NaN inputs the corpus feeds.
        Ok(match op {
            V_CMP_LT_F32 => self.b.f_ord_less_than(bool_ty, None, a, b).expect("flt"),
            V_CMP_EQ_F32 => self.b.f_ord_equal(bool_ty, None, a, b).expect("feq"),
            V_CMP_LE_F32 => self
                .b
                .f_ord_less_than_equal(bool_ty, None, a, b)
                .expect("fle"),
            V_CMP_GT_F32 => self.b.f_ord_greater_than(bool_ty, None, a, b).expect("fgt"),
            V_CMP_GE_F32 => self
                .b
                .f_ord_greater_than_equal(bool_ty, None, a, b)
                .expect("fge"),
            _ => {
                return Err(RecompileError::Unsupported {
                    offset: off,
                    reason: "unmodeled VOPC f32 compare",
                });
            }
        })
    }

    /// Emit the SPIR-V bool for an INTEGER VOPC compare `op` (VOP3B-encoded) on the raw
    /// u32-bit operands `a`/`b`. Signedness is carried by the SPIR-V opcode, not the
    /// operand type: signed compares use `OpSLessThan`/… (interpreting the bits as i32),
    /// unsigned use `OpULessThan`/… (as u32); equality/inequality are sign-agnostic
    /// (`OpIEqual`/`OpINotEqual`). `op` is the VOP3 op number, which for the VOPC range
    /// equals the raw VOPC op field. An unmodeled compare defers cleanly.
    fn emit_int_compare(
        &mut self,
        op: u16,
        a: spirv::Word,
        b: spirv::Word,
        off: u32,
    ) -> Result<spirv::Word, RecompileError> {
        use opcodes::vop3::*;
        let bool_ty = self.t.bool;
        Ok(match op {
            // Signed i32.
            V_CMP_LT_I32 => self.b.s_less_than(bool_ty, None, a, b).expect("slt"),
            V_CMP_LE_I32 => self.b.s_less_than_equal(bool_ty, None, a, b).expect("sle"),
            V_CMP_GT_I32 => self.b.s_greater_than(bool_ty, None, a, b).expect("sgt"),
            V_CMP_GE_I32 => self
                .b
                .s_greater_than_equal(bool_ty, None, a, b)
                .expect("sge"),
            // Unsigned u32.
            V_CMP_LT_U32 => self.b.u_less_than(bool_ty, None, a, b).expect("ult"),
            V_CMP_LE_U32 => self.b.u_less_than_equal(bool_ty, None, a, b).expect("ule"),
            V_CMP_GT_U32 => self.b.u_greater_than(bool_ty, None, a, b).expect("ugt"),
            V_CMP_GE_U32 => self
                .b
                .u_greater_than_equal(bool_ty, None, a, b)
                .expect("uge"),
            // Equality / inequality are sign-agnostic (same bits either way).
            V_CMP_EQ_I32 | V_CMP_EQ_U32 => self.b.i_equal(bool_ty, None, a, b).expect("ieq"),
            V_CMP_NE_I32 | V_CMP_NE_U32 => self.b.i_not_equal(bool_ty, None, a, b).expect("ine"),
            _ => {
                return Err(RecompileError::Unsupported {
                    offset: off,
                    reason: "unmodeled VOPC integer compare",
                });
            }
        })
    }

    /// True if `op` is a VOP3-encoded VOPC integer compare this recompiler models.
    fn is_int_compare_vop3(op: u16) -> bool {
        use opcodes::vop3::*;
        matches!(
            op,
            V_CMP_LT_I32
                | V_CMP_EQ_I32
                | V_CMP_LE_I32
                | V_CMP_GT_I32
                | V_CMP_NE_I32
                | V_CMP_GE_I32
                | V_CMP_LT_U32
                | V_CMP_EQ_U32
                | V_CMP_LE_U32
                | V_CMP_GT_U32
                | V_CMP_NE_U32
                | V_CMP_GE_U32
        )
    }

    // ---- SMRD --------------------------------------------------------------

    fn emit_smrd(
        &mut self,
        op: u8,
        sdst: Operand,
        sbase: u8,
        imm: bool,
        offset: u32,
        off: u32,
    ) -> Result<(), RecompileError> {
        let count = opcodes::smrd::dst_count(op).ok_or(RecompileError::UnsupportedInst {
            inst: Box::new(Inst::Smrd {
                op,
                sdst,
                sbase,
                imm,
                offset,
            }),
            offset: off,
        })?;
        let dst0 = match sdst {
            Operand::Sgpr(n) => n,
            other => {
                return Err(RecompileError::InvalidOperand {
                    inst: RecompileError::pending_inst(),
                    operand: other,
                    offset: off,
                    reason: "SMRD destination must be an SGPR",
                });
            }
        };
        if opcodes::smrd::is_buffer_load(op) {
            return self.emit_s_buffer_load(dst0, sbase, imm, offset, count, off);
        }
        // s_load: the SMRD loads the vertex-buffer V# descriptor into an SGPR block.
        // We don't model the descriptor bytes; the fetch (MUBUF) resolves to the bound
        // storage buffer directly. Record which SGPRs the load wrote, along with the
        // load's provenance — the SBASE (descriptor-set pointer pair) and the byte
        // offset of the descriptor within that set (the SMRD immediate `offset` is a
        // dword index, so ×4 to bytes) — so a MUBUF `srsrc` referencing them resolves to
        // that descriptor (a `SetPointer`), not wave registers.
        let desc_offset = offset.wrapping_mul(4);
        for i in 0..count {
            let reg = dst0
                .checked_add(i)
                .filter(|r| (*r as usize) < crate::interp::NUM_SGPRS)
                .ok_or(RecompileError::InvalidRegister {
                    kind: "sgpr",
                    reg: dst0 as usize + i as usize,
                    max: crate::interp::NUM_SGPRS,
                    offset: off,
                })?;
            self.vsharp_sgprs.insert(reg, (sbase, desc_offset));
        }
        Ok(())
    }

    /// `s_buffer_load` — load `count` scalar dwords from a constant buffer (a uniform
    /// buffer addressed through a V# descriptor) into `s[dst0 .. dst0+count]`. The
    /// constant buffer is a `StorageBuffer` SSBO of raw `uint`; the immediate offset
    /// is a dword index into it (matching the oracle, which reads `V#.base + off*4`).
    fn emit_s_buffer_load(
        &mut self,
        dst0: u8,
        sbase: u8,
        imm: bool,
        offset: u32,
        count: u8,
        off: u32,
    ) -> Result<(), RecompileError> {
        if !imm {
            // The SGPR-offset form would need the offset SGPR's value; the constant
            // loads the retail shaders emit are all immediate.
            return Err(RecompileError::Unsupported {
                offset: off,
                reason: "s_buffer_load with an SGPR (non-immediate) offset",
            });
        }
        // The last dword this load touches must fit in an SGPR and stay in range.
        let dst_hi = dst0 as usize + count as usize - 1;
        if dst_hi >= crate::interp::NUM_SGPRS {
            return Err(RecompileError::InvalidRegister {
                kind: "sgpr",
                reg: dst_hi,
                max: crate::interp::NUM_SGPRS,
                offset: off,
            });
        }
        let cb = self.ensure_const_buffer(sbase, offset + count as u32, off)?;
        let (var, ptr_member) = (cb.var, cb.ptr_member);
        let zero = self.const_u32(0);
        for i in 0..count as u32 {
            let idx = self.const_u32(offset + i);
            let ptr = self
                .b
                .access_chain(ptr_member, None, var, [zero, idx])
                .expect("cbuffer access chain");
            let val = self
                .b
                .load(self.t.u32, None, ptr, None, [])
                .expect("cbuffer load");
            self.store_reg_bits(false, dst0 + i as u8, val);
        }
        Ok(())
    }

    /// Declare (once) the scalar constant buffer's `StorageBuffer` SSBO — a runtime
    /// array of raw `uint` dwords at `(CONST_BUFFER_SET, CONST_BUFFER_BINDING)` — and
    /// return its variable + member-pointer type. A second s_buffer_load with a
    /// different SBASE names a distinct constant buffer the single binding cannot
    /// represent (the V# is resolved symbolically), so it defers.
    fn ensure_const_buffer(
        &mut self,
        sbase: u8,
        size_dwords: u32,
        off: u32,
    ) -> Result<ConstBufferRef, RecompileError> {
        if let Some(cb) = &mut self.const_buffer {
            if cb.sbase != sbase {
                return Err(RecompileError::Unsupported {
                    offset: off,
                    reason: "a second distinct constant buffer (SBASE) is not modeled",
                });
            }
            if size_dwords > cb.size_dwords {
                cb.size_dwords = size_dwords;
                if let Some(binding) = self.io_const_buffers.first_mut() {
                    binding.size_dwords = size_dwords;
                }
            }
            return Ok(ConstBufferRef {
                var: cb.var,
                ptr_member: cb.ptr_member,
            });
        }
        // struct ConstBuffer { uint data[]; } as a StorageBuffer, ArrayStride 4 — the
        // shared dword-SSBO block (decorated once; see `dword_ssbo_block`).
        let block = self.dword_ssbo_block();
        let ptr_ssbo = self
            .b
            .type_pointer(None, spirv::StorageClass::StorageBuffer, block);
        let var = self.global_variable(ptr_ssbo, spirv::StorageClass::StorageBuffer);
        // Stage-distinct binding (task-174): the FRAGMENT stage's const buffer takes its
        // own binding so a VS+PS dual-CB draw has two non-colliding set-0 slots.
        let binding = match self.stage {
            ShaderStage::Fragment => PS_CONST_BUFFER_BINDING,
            _ => CONST_BUFFER_BINDING,
        };
        self.b.decorate(
            var,
            spirv::Decoration::DescriptorSet,
            [DrOperand::LiteralBit32(CONST_BUFFER_SET)],
        );
        self.b.decorate(
            var,
            spirv::Decoration::Binding,
            [DrOperand::LiteralBit32(binding)],
        );
        let ptr_member = self
            .b
            .type_pointer(None, spirv::StorageClass::StorageBuffer, self.t.u32);
        self.const_buffer = Some(ConstBuffer {
            var,
            ptr_member,
            sbase,
            size_dwords,
        });
        self.io_const_buffers.push(ConstBufferBinding {
            set: CONST_BUFFER_SET,
            binding,
            size_dwords,
            // The V# is inline in the SGPR quad the `s_buffer_load` SBASE names.
            source: DescriptorSource::InlineVSharp { sgpr: sbase },
        });
        Ok(ConstBufferRef { var, ptr_member })
    }

    // ---- MUBUF: indexed vertex fetch → descriptor buffer -------------------

    #[allow(clippy::too_many_arguments)]
    fn emit_mubuf(
        &mut self,
        op: u8,
        vdata: Operand,
        vaddr: Operand,
        srsrc: u8,
        soffset: Operand,
        imm_offset: u16,
        idxen: bool,
        offen: bool,
        off: u32,
    ) -> Result<(), RecompileError> {
        let count = opcodes::mubuf::vdata_count(op).ok_or(RecompileError::UnsupportedInst {
            inst: Box::new(Inst::Mubuf {
                op,
                vdata,
                vaddr,
                srsrc,
                soffset,
                offset: imm_offset,
                idxen,
                offen,
            }),
            offset: off,
        })?;
        if let Operand::Raw(255) = soffset {
            return Err(RecompileError::InvalidOperand {
                inst: RecompileError::pending_inst(),
                operand: soffset,
                offset: off,
                reason: "MUBUF soffset field 255 is invalid",
            });
        }
        if offen {
            // The corpus never sets offen; the per-lane byte offset would come from
            // vaddr+1. Not modeled here (deferred) — reject with the deferred/
            // unsupported variant (a valid-but-unlowered instruction, not a malformed
            // shader) so the differential harness can categorize it.
            return Err(RecompileError::Unsupported {
                offset: off,
                reason: "MUBUF offen not modeled in the recompiler subset",
            });
        }
        let vdata0 = match vdata {
            Operand::Vgpr(n) => n,
            other => {
                return Err(RecompileError::InvalidOperand {
                    inst: RecompileError::pending_inst(),
                    operand: other,
                    offset: off,
                    reason: "MUBUF vdata must be a VGPR",
                });
            }
        };
        // srsrc must name the SGPR block the SMRD wrote (the V#). We resolve the
        // fetch to the bound vertex buffer regardless of the descriptor bytes, but
        // record its provenance: the SMRD that wrote `srsrc` names the descriptor-set
        // pointer pair (SBASE) and the descriptor's byte offset within that set. An
        // `srsrc` no SMRD wrote is not a fetched descriptor — defer cleanly.
        let Some(&(sbase, desc_offset)) = self.vsharp_sgprs.get(&srsrc) else {
            return Err(RecompileError::Unsupported {
                offset: off,
                reason: "MUBUF srsrc does not name an SMRD-loaded vertex-buffer descriptor",
            });
        };
        let source = DescriptorSource::SetPointer {
            sgpr: sbase,
            desc_offset,
        };

        // The fetch index is the vertex index (idxen: vaddr's VGPR, per-lane). In the
        // recompiled shader the launch ABI's v0 = gl_VertexIndex; the index may also
        // have been relocated into another VGPR via v_mov (tracked in
        // vertex_index_regs). An idxen fetch on any index-carrying reg reads
        // gl_VertexIndex; on any other VGPR we would read an uninitialized slot, so
        // reject cleanly (deferred/unsupported) instead of fetching garbage. A
        // non-idxen fetch reads index 0.
        let index_u32 = if idxen {
            let vaddr_n = match vaddr {
                Operand::Vgpr(n) => n,
                other => {
                    return Err(RecompileError::InvalidOperand {
                        inst: RecompileError::pending_inst(),
                        operand: other,
                        offset: off,
                        reason: "MUBUF idxen requires a VGPR vaddr",
                    });
                }
            };
            if self.vertex_index_regs.contains(&vaddr_n) {
                self.load_vertex_index()
            } else {
                return Err(RecompileError::Unsupported {
                    offset: off,
                    reason: "MUBUF idxen vaddr does not carry the launch vertex index",
                });
            }
        } else {
            self.const_u32(0)
        };

        // Resolve (or allocate) the SSBO stream for THIS V# (task-153): a distinct V#
        // (attr0/attr1/attr2) gets its own binding + push-constant group, so its
        // num_records/stride/dst_sel are read from the group the provider fills with THIS
        // stream's V#. The stream index selects the push-constant group below.
        let buf = self.ensure_vs_buffer(count, source);
        let stream = buf.stream;

        // The MUBUF byte offset the oracle adds on top of index*stride: the immediate
        // `offset` field plus a constant (inline) `soffset`. In the fetch-shader
        // convention both are 0, but thread them so a non-zero offset (Bug 1, task-153)
        // addresses the right dword instead of silently reading element 0. A
        // register-valued `soffset` is unknown at recompile time — the corpus never uses
        // one (it is always inline-0), so a non-inline soffset defers cleanly.
        let soff_const = match soffset {
            // Mirror the oracle's `read_scalar` for the compile-time-constant subset.
            Operand::InlineInt(v) => v as u32,
            Operand::InlineFloat(f) => f.to_bits(),
            Operand::Literal(v) => v,
            _ => {
                return Err(RecompileError::Unsupported {
                    offset: off,
                    reason: "MUBUF soffset is a runtime SGPR — not modeled in the fetch subset",
                });
            }
        };
        let byte_offset = soff_const.wrapping_add(u32::from(imm_offset));

        // num_records clamp: an index >= num_records clamps to num_records-1 (and 0
        // records ⇒ index 0). num_records is a bind-time value (from THIS stream's V#),
        // supplied as this stream's push-constant group. Mirror the oracle:
        //   idx = if nr != 0 && index >= nr { nr - 1 } else { index }
        let nr = self.load_num_records(stream);
        let clamped = self.clamp_index(index_u32, nr);

        // Fetch `count` f32 components of the vec4 element at `clamped`. The vertex
        // element stride (this stream's push-constant group) is loaded ONCE and reused
        // across every component's dword-address math (task-140 — was a module-level spec
        // constant, now a dynamic push-constant load).
        let stride = self.load_stride(stream);
        // The packed dst_sel push constant (task-155): the GCN format/swizzle stage
        // substitutes 0.0/1.0 or reroutes the source component per channel, exactly as
        // real hardware does. Loaded once and applied per fetched channel below.
        let dst_sel = self.load_dst_sel(stream);
        // The packed format push constant (task-164): the GCN format stage unpacks each
        // fetched component per dfmt/nfmt (raw dword for 32-bit float, byte/half decode for
        // packed 8/16-bit). Loaded once and applied per fetched channel below.
        let format = self.load_format(stream);
        for i in 0..count {
            let comp =
                self.fetch_buffer_component(&buf, clamped, stride, byte_offset, dst_sel, format, i);
            let reg = vdata0
                .checked_add(i)
                .filter(|r| (*r as usize) < crate::interp::NUM_VGPRS)
                .ok_or(RecompileError::InvalidRegister {
                    kind: "vgpr",
                    reg: vdata0 as usize + i as usize,
                    max: crate::interp::NUM_VGPRS,
                    offset: off,
                })?;
            // A fetch destination no longer carries the launch vertex index — including
            // the degenerate `buffer_load_format_x v0, v0, …` that fetches over its own
            // index register. Without this the overwritten reg would keep resolving to
            // `gl_VertexIndex` on every later read (task-184).
            self.untrack_vertex_index(reg);
            self.store_reg_f32(true, reg, comp);
        }
        Ok(())
    }

    /// `idx = (nr != 0 && index >= nr) ? nr - 1 : index`, matching the oracle's
    /// robust-buffer clamp. `nr == 0` degenerates to index 0 because `index >= 0` is
    /// always true and `nr - 1` wraps — instead we special-case it with a select.
    fn clamp_index(&mut self, index: spirv::Word, nr: spirv::Word) -> spirv::Word {
        let zero = self.const_u32(0);
        let one = self.const_u32(1);
        // last = nr - 1 (only used when nr != 0).
        let last = self.b.i_sub(self.t.u32, None, nr, one).expect("nr-1");
        // oob = index >= nr  (i.e. !(index < nr))
        let lt = self
            .b
            .u_less_than(self.t.bool, None, index, nr)
            .expect("index<nr");
        let clamped_when_nonzero = self
            .b
            .select(self.t.u32, None, lt, index, last)
            .expect("select clamp");
        // nr == 0 ⇒ index 0; else the clamped value above.
        let nr_is_zero = self.b.i_equal(self.t.bool, None, nr, zero).expect("nr==0");
        self.b
            .select(self.t.u32, None, nr_is_zero, zero, clamped_when_nonzero)
            .expect("select nr==0")
    }

    /// Fetch f32 channel `channel` of the vertex at `index`, applying the V#'s per-channel
    /// destination swizzle `dst_sel` (task-155) exactly as GCN's format/swizzle stage does.
    /// The 3-bit selector for this channel (`dst_sel >> channel*3 & 7`) picks:
    ///
    /// - `0` → constant `0.0`,
    /// - `1` → constant `1.0`,
    /// - `4..7` → SOURCE component `selector-4` (raw dword read + bitcast to f32).
    ///
    /// Any other selector value passes through as source component `channel` (identity),
    /// so an unmodeled swizzle degrades to a plain raw read rather than a bad substitution.
    /// The IDENTITY swizzle `[4,5,6,7]` makes channel `ch` read source `ch` — a pure raw
    /// passthrough matching the interp (which never reads `dst_sel`).
    ///
    /// The source dword read is `(index * stride)/4 + src_comp`, where `stride` is the
    /// vertex element stride in bytes from the push constant (`PC_STRIDE_MEMBER`) — exactly
    /// the interp's byte math (`base + index*stride + comp*4`) expressed in dwords. All
    /// four source components are addressed from the SAME `dword_base`; the runtime
    /// selector chooses which one (and whether a constant substitutes), so a non-16 stride
    /// or any swizzle is honored dynamically without a re-emit or re-specialization
    /// (task-140, task-155).
    ///
    /// The fetched component is then unpacked per the packed FORMAT push constant (`format`:
    /// `dfmt` in `[7:0]`, `nfmt` in `[15:8]`; task-164), branchlessly selected among:
    /// - **32-bit float / unmodeled** (`dfmt` 4/11/13/14/0/…): read the raw dword and bitcast
    ///   — the position/UV/atlas path, bit-identical to the pre-task-164 fetch.
    /// - **packed 8-bit** (`dfmt` 1/3/10 — `_8`/`_8_8`/`_8_8_8_8`): extract byte `src_comp`
    ///   of the element dword and convert per `nfmt` (unorm → byte/255, snorm, uint, sint).
    ///   This is Celeste's `_8_8_8_8` UNORM sprite color — one packed dword → four floats.
    /// - **packed 16-bit** (`dfmt` 2/5/12 — `_16`/`_16_16`/`_16_16_16_16`): extract the
    ///   `src_comp`-th 16-bit lane and convert per `nfmt` (unorm/snorm/uint/sint, or a
    ///   half-float via `UnpackHalf2x16`).
    ///
    /// The unpack is BRANCHLESS (nested `OpSelect` on the runtime format, exactly like the
    /// `dst_sel` swizzle) — MoltenVK/Metal-portable, no divergent control flow — and mirrors
    /// `crate::interp` `exec_mubuf` bit-for-bit so the task-122 differential holds. The
    /// `dst_sel` constant substitution is applied AFTER the format unpack.
    #[allow(clippy::too_many_arguments)]
    fn fetch_buffer_component(
        &mut self,
        buf: &VsBuffer,
        index: spirv::Word,
        stride: spirv::Word,
        byte_offset: u32,
        dst_sel: spirv::Word,
        format: spirv::Word,
        channel: u8,
    ) -> spirv::Word {
        let four = self.const_u32(4);
        // byte_off = index * stride + byte_offset  (stride is the push-constant member;
        // byte_offset is the MUBUF soffset+immediate, task-153 Bug 1 — mirrors the oracle's
        // `base + index*stride + soff + offset`, with `base` absorbed into the SSBO binding).
        let index_stride = self
            .b
            .i_mul(self.t.u32, None, index, stride)
            .expect("vb index*stride");
        let byte_off = if byte_offset != 0 {
            let bo = self.const_u32(byte_offset);
            self.b
                .i_add(self.t.u32, None, index_stride, bo)
                .expect("vb + byte_offset")
        } else {
            index_stride
        };
        // dword_base = byte_off / 4  (exact: every valid stride + offset is 4-byte aligned)
        let dword_base = self
            .b
            .u_div(self.t.u32, None, byte_off, four)
            .expect("vb byte_off/4");

        // Extract this channel's 3-bit selector: (dst_sel >> channel*3) & 7.
        let shift = self.const_u32(channel as u32 * 3);
        let seven = self.const_u32(7);
        let shifted = self
            .b
            .shift_right_logical(self.t.u32, None, dst_sel, shift)
            .expect("dst_sel >> shift");
        let sel = self
            .b
            .bitwise_and(self.t.u32, None, shifted, seven)
            .expect("dst_sel & 7");

        // src_comp = sel >= 4 ? sel - 4 : 0  (a constant-substitution selector 0/1 still
        // reads some in-range dword; the result is discarded by the OpSelect below). The
        // ternary keeps every access-chain index in-range for a well-formed swizzle.
        let sel_ge_4 = self
            .b
            .u_greater_than_equal(self.t.bool, None, sel, four)
            .expect("sel>=4");
        let sel_minus_4 = self.b.i_sub(self.t.u32, None, sel, four).expect("sel-4");
        let zero = self.const_u32(0);
        let src_comp = self
            .b
            .select(self.t.u32, None, sel_ge_4, sel_minus_4, zero)
            .expect("select src_comp");

        // dword = dword_base + src_comp — the FLOAT-path source dword (one raw dword per
        // component, bitcast to f32; the 32-bit `buffer_load_format` vertex attribute).
        let dword = self
            .b
            .i_add(self.t.u32, None, dword_base, src_comp)
            .expect("vb dword+src_comp");

        // ---- format-aware unpack (task-164) ----
        // Decode the packed format push constant: dfmt in [7:0], nfmt in [15:8].
        let ff = self.const_u32(0xFF);
        let eight = self.const_u32(8);
        let dfmt = self
            .b
            .bitwise_and(self.t.u32, None, format, ff)
            .expect("dfmt bits");
        let nfmt_sh = self
            .b
            .shift_right_logical(self.t.u32, None, format, eight)
            .expect("format>>8");
        let nfmt = self
            .b
            .bitwise_and(self.t.u32, None, nfmt_sh, ff)
            .expect("nfmt bits");

        // Format-class booleans. Modeled packed families: 8-bit {1,3,10}, 16-bit {2,5,12};
        // every other dfmt (32-bit float 4/11/13/14, invalid 0, unmodeled) uses the raw dword.
        let is8 = {
            let d1 = self.const_u32(1);
            let d3 = self.const_u32(3);
            let d10 = self.const_u32(10);
            let e1 = self
                .b
                .i_equal(self.t.bool, None, dfmt, d1)
                .expect("dfmt==1");
            let e3 = self
                .b
                .i_equal(self.t.bool, None, dfmt, d3)
                .expect("dfmt==3");
            let e10 = self
                .b
                .i_equal(self.t.bool, None, dfmt, d10)
                .expect("dfmt==10");
            let a = self
                .b
                .logical_or(self.t.bool, None, e1, e3)
                .expect("or 1/3");
            self.b.logical_or(self.t.bool, None, a, e10).expect("or 10")
        };
        let is16 = {
            let d2 = self.const_u32(2);
            let d5 = self.const_u32(5);
            let d12 = self.const_u32(12);
            let e2 = self
                .b
                .i_equal(self.t.bool, None, dfmt, d2)
                .expect("dfmt==2");
            let e5 = self
                .b
                .i_equal(self.t.bool, None, dfmt, d5)
                .expect("dfmt==5");
            let e12 = self
                .b
                .i_equal(self.t.bool, None, dfmt, d12)
                .expect("dfmt==12");
            let a = self
                .b
                .logical_or(self.t.bool, None, e2, e5)
                .expect("or 2/5");
            self.b.logical_or(self.t.bool, None, a, e12).expect("or 12")
        };
        let is_packed = self
            .b
            .logical_or(self.t.bool, None, is8, is16)
            .expect("is_packed");

        // Raw f32 candidate (float / fallback path). For a packed format the raw read
        // collapses to `dword_base` so the access stays in-range (its value is discarded by
        // the class select); for the float path it is `dword_base + src_comp` — same address,
        // same load, same bitcast as before, so a float vertex is bit-identical to pre-164.
        let raw_index = self
            .b
            .select(self.t.u32, None, is_packed, dword_base, dword)
            .expect("raw index");
        let raw_ptr = self
            .b
            .access_chain(buf.ptr_member, None, buf.var, [zero, raw_index])
            .expect("vb raw access chain");
        let raw_bits = self
            .b
            .load(self.t.u32, None, raw_ptr, None, [])
            .expect("vb raw load");
        let raw_f32 = self
            .b
            .bitcast(self.t.f32, None, raw_bits)
            .expect("vb bitcast u32->f32");

        let f_zero = self.const_f32(0.0f32.to_bits());

        // 8-bit family: `_8`/`_8_8`/`_8_8_8_8` all fit in the one dword at `dword_base`;
        // component `src_comp` (0..3) is byte `src_comp` of it.
        let three = self.const_u32(3);
        let sh8 = self
            .b
            .shift_left_logical(self.t.u32, None, src_comp, three)
            .expect("src_comp*8");
        let d8_ptr = self
            .b
            .access_chain(buf.ptr_member, None, buf.var, [zero, dword_base])
            .expect("vb 8-bit access chain");
        let d8_bits = self
            .b
            .load(self.t.u32, None, d8_ptr, None, [])
            .expect("vb 8-bit load");
        let byte = {
            let shifted = self
                .b
                .shift_right_logical(self.t.u32, None, d8_bits, sh8)
                .expect("byte>>");
            self.b
                .bitwise_and(self.t.u32, None, shifted, ff)
                .expect("byte&0xFF")
        };
        let val8 = self.convert_packed_component(byte, 8, nfmt, f_zero);

        // 16-bit family: component `src_comp` is the (src_comp>>1)-th dword, half (src_comp&1).
        let one = self.const_u32(1);
        let four = self.const_u32(4);
        let ffff = self.const_u32(0xFFFF);
        let d16_off = self
            .b
            .shift_right_logical(self.t.u32, None, src_comp, one)
            .expect("src_comp>>1");
        let half_idx = self
            .b
            .bitwise_and(self.t.u32, None, src_comp, one)
            .expect("src_comp&1");
        let d16_dword = self
            .b
            .i_add(self.t.u32, None, dword_base, d16_off)
            .expect("dword_base+d16_off");
        let d16_ptr = self
            .b
            .access_chain(buf.ptr_member, None, buf.var, [zero, d16_dword])
            .expect("vb 16-bit access chain");
        let d16_bits = self
            .b
            .load(self.t.u32, None, d16_ptr, None, [])
            .expect("vb 16-bit load");
        let sh16 = self
            .b
            .shift_left_logical(self.t.u32, None, half_idx, four)
            .expect("half_idx*16");
        let half_u = {
            let shifted = self
                .b
                .shift_right_logical(self.t.u32, None, d16_bits, sh16)
                .expect("half>>");
            self.b
                .bitwise_and(self.t.u32, None, shifted, ffff)
                .expect("half&0xFFFF")
        };
        // Half-float candidate: UnpackHalf2x16 the dword into a vec2, pick the src_comp lane.
        let v2f32 = self.b.type_vector(self.t.f32, 2);
        let hv = self
            .b
            .ext_inst(
                v2f32,
                None,
                self.glsl_ext,
                glsl::UNPACK_HALF_2X16,
                [DrOperand::IdRef(d16_bits)],
            )
            .expect("unpack half2x16");
        let h_lo = self
            .b
            .composite_extract(self.t.f32, None, hv, [0])
            .expect("half lo");
        let h_hi = self
            .b
            .composite_extract(self.t.f32, None, hv, [1])
            .expect("half hi");
        let is_hi = self
            .b
            .i_equal(self.t.bool, None, half_idx, one)
            .expect("half_idx==1");
        let half_f = self
            .b
            .select(self.t.f32, None, is_hi, h_hi, h_lo)
            .expect("select half");
        let val16 = self.convert_packed_component(half_u, 16, nfmt, half_f);

        // Class select: 8-bit → val8, 16-bit → val16, else the raw f32.
        let inner_fmt = self
            .b
            .select(self.t.f32, None, is16, val16, raw_f32)
            .expect("select 16");
        let unpacked = self
            .b
            .select(self.t.f32, None, is8, val8, inner_fmt)
            .expect("select 8");

        // Apply the dst_sel constant substitutions AFTER the format unpack: sel==0 → 0.0,
        // sel==1 → 1.0, else the unpacked component. Nested OpSelect on the runtime selector.
        let f_one = self.const_f32(1.0f32.to_bits());
        let sel_is_zero = self
            .b
            .i_equal(self.t.bool, None, sel, zero)
            .expect("sel==0");
        let sel_is_one = self.b.i_equal(self.t.bool, None, sel, one).expect("sel==1");
        // inner = sel==1 ? 1.0 : unpacked
        let inner = self
            .b
            .select(self.t.f32, None, sel_is_one, f_one, unpacked)
            .expect("select 1.0");
        // out = sel==0 ? 0.0 : inner
        self.b
            .select(self.t.f32, None, sel_is_zero, f_zero, inner)
            .expect("select 0.0")
    }

    /// Convert a packed unsigned integer component of `width` bits (8 or 16) into the f32 the
    /// GCN format stage produces, per the runtime `nfmt` (`0` unorm, `1` snorm, `4` uint,
    /// `5` sint, `7` float). `raw_u` is the width-bit field already extracted (masked, right-
    /// aligned); `float_val` is the pre-decoded half→f32 candidate used only when `nfmt`==7
    /// (16-bit; for 8-bit pass any value — a well-formed 8-bit V# is never a float format).
    /// Branchless nested `OpSelect` on `nfmt` (MoltenVK/portable); an unmodeled `nfmt`
    /// degrades to unorm. Mirrors `crate::interp` `convert_packed_int` bit-for-bit (task-164).
    fn convert_packed_component(
        &mut self,
        raw_u: spirv::Word,
        width: u32,
        nfmt: spirv::Word,
        float_val: spirv::Word,
    ) -> spirv::Word {
        let max_u = self.const_f32((((1u32 << width) - 1) as f32).to_bits());
        let half_max = self.const_f32((((1u32 << (width - 1)) - 1) as f32).to_bits());
        let sign_bit = self.const_u32(1u32 << (width - 1));
        let full = self.const_u32(1u32 << width);
        let zero = self.const_u32(0);
        // Sign-extend: s = raw_u - ((raw_u & sign_bit) != 0 ? full : 0) — two's complement in u32.
        let masked = self
            .b
            .bitwise_and(self.t.u32, None, raw_u, sign_bit)
            .expect("sign mask");
        let masked_is_zero = self
            .b
            .i_equal(self.t.bool, None, masked, zero)
            .expect("mask==0");
        let sub = self
            .b
            .select(self.t.u32, None, masked_is_zero, zero, full)
            .expect("select sub");
        let s_u = self.b.i_sub(self.t.u32, None, raw_u, sub).expect("raw-sub");
        let s_i = self.b.bitcast(self.t.i32, None, s_u).expect("bitcast i32");
        let uf = self
            .b
            .convert_u_to_f(self.t.f32, None, raw_u)
            .expect("u->f");
        let sf = self.b.convert_s_to_f(self.t.f32, None, s_i).expect("s->f");
        let unorm = self
            .b
            .f_div(self.t.f32, None, uf, max_u)
            .expect("unorm div");
        let snorm_div = self
            .b
            .f_div(self.t.f32, None, sf, half_max)
            .expect("snorm div");
        let neg_one = self.const_f32((-1.0f32).to_bits());
        let snorm = self
            .b
            .ext_inst(
                self.t.f32,
                None,
                self.glsl_ext,
                glsl::FMAX,
                [DrOperand::IdRef(neg_one), DrOperand::IdRef(snorm_div)],
            )
            .expect("snorm max(-1)");
        // Select by nfmt; base = unorm (nfmt 0 and any unmodeled value).
        let n1 = self.const_u32(1);
        let n4 = self.const_u32(4);
        let n5 = self.const_u32(5);
        let n7 = self.const_u32(7);
        let is_snorm = self
            .b
            .i_equal(self.t.bool, None, nfmt, n1)
            .expect("nfmt==1");
        let is_uint = self
            .b
            .i_equal(self.t.bool, None, nfmt, n4)
            .expect("nfmt==4");
        let is_sint = self
            .b
            .i_equal(self.t.bool, None, nfmt, n5)
            .expect("nfmt==5");
        let is_float = self
            .b
            .i_equal(self.t.bool, None, nfmt, n7)
            .expect("nfmt==7");
        let r = self
            .b
            .select(self.t.f32, None, is_snorm, snorm, unorm)
            .expect("sel snorm");
        let r = self
            .b
            .select(self.t.f32, None, is_uint, uf, r)
            .expect("sel uint");
        let r = self
            .b
            .select(self.t.f32, None, is_sint, sf, r)
            .expect("sel sint");
        self.b
            .select(self.t.f32, None, is_float, float_val, r)
            .expect("sel float")
    }

    // ---- VINTRP: screen-space-linear plane eval ----------------------------

    fn emit_vintrp(
        &mut self,
        op: u8,
        vdst: Operand,
        vsrc: Operand,
        attr: u8,
        chan: u8,
        off: u32,
    ) -> Result<(), RecompileError> {
        use opcodes::vintrp::*;
        let ndst = self.vgpr_dst(vdst, off)?;
        // Attribute comes from the VINTRP attr/chan fields, NOT m0 — the oracle's
        // deliberate simplification. The interpolant is a Location input carrying the
        // per-vertex plane value already interpolated by the rasterizer.
        //
        // DIVERGENCE(interp): the oracle evaluates the plane equation itself from
        // P0/P1/P2 and the launch barycentrics (v0=I, v1=J):
        //     p1: P0 + I·(P1-P0);  p2: partial + J·(P2-P0).
        // In a real GPU pipeline the fixed-function interpolator computes exactly this
        // plane value and delivers it as the Location input — screen-space-linear, no
        // perspective divide (matching the oracle). So the recompiled PS reads the
        // interpolated attribute directly. The two-phase p1/p2 split collapses to a
        // single read of the final interpolant; a p2 without a matching p1 (or the
        // `mov` form) still reads the same input.
        //
        // HANDSHAKE: for the two sides to agree, the differential test harness MUST
        // drive this recompiled PS's `Location=attr` vec4 input, channel `chan`, with
        // the oracle's own computed plane value P0 + I·(P1-P0) + J·(P2-P0) (I,J = the
        // launch barycentrics v0,v1). The recompiler emits no interpolation math; it
        // trusts the input already carries that value.
        match op {
            V_INTERP_P1_F32 | V_INTERP_P2_F32 | V_INTERP_MOV_F32 => {
                let _ = vsrc; // the barycentric VGPR: consumed by the interpolator, not here
                let val = self.read_ps_input(attr, chan);
                // p2 accumulates onto the p1 partial in the same vdst; because the
                // interpolator already delivers the final plane value, both phases
                // store the same interpolated attribute. Storing on each phase leaves
                // vdst holding the final value after p2 — identical to the oracle's
                // end state.
                self.store_reg_f32(true, ndst, val);
                Ok(())
            }
            _ => Err(RecompileError::UnsupportedInst {
                inst: Box::new(Inst::Vintrp {
                    op,
                    vdst,
                    vsrc,
                    attr,
                    chan,
                }),
                offset: off,
            }),
        }
    }

    // ---- MIMG: image_sample → combined image-sampler -----------------------

    /// Lower `image_sample` to a portable `OpImageSampleImplicitLod` through a combined
    /// image-sampler descriptor, mirroring [`crate::interp::Interp::exec_mimg`]. The
    /// oracle samples the SAME detiled texture bytes on the CPU; for the two to agree
    /// the differential harness drives this module's combined image-sampler with that
    /// texture (the sampling analogue of the VINTRP interpolant handshake). The
    /// recompiler emits no sampling math — the GPU's fixed-function sampler evaluates
    /// the filter the bound S# selected.
    #[allow(clippy::too_many_arguments)]
    fn emit_mimg(
        &mut self,
        op: u8,
        vdata: Operand,
        vaddr: Operand,
        srsrc: u8,
        ssamp: u8,
        dmask: u8,
        unrm: bool,
        off: u32,
    ) -> Result<(), RecompileError> {
        if op != opcodes::mimg::IMAGE_SAMPLE {
            return Err(RecompileError::UnsupportedInst {
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
        // Only a PS samples — a VS `image_sample` is outside the ABI (the vertex stage
        // has no fixed-function sampler input in the subset).
        if self.stage != ShaderStage::Fragment {
            return Err(RecompileError::Shape(
                "image_sample outside a fragment shader",
            ));
        }
        // UNRM coordinates (unnormalized texel indices) would need an OpImageFetch/
        // texel-space scale; the corpus uses normalized coords. Defer cleanly.
        if unrm {
            return Err(RecompileError::Unsupported {
                offset: off,
                reason: "image_sample with unnormalized coordinates not modeled",
            });
        }
        // The T#/S# name descriptor SGPR blocks; the fetch resolves to the bound
        // combined image-sampler regardless of the descriptor bytes (like the VS's V#).
        // Record the T#'s provenance (the MIMG `srsrc`): if an SMRD `s_load` wrote it,
        // it holds a descriptor-set pointer pair (a `SetPointer`); otherwise the T# is
        // inline in the SGPR block the launch ABI loaded (an `InlineVSharp`, the corpus
        // shape — a texturing PS whose T#/S# arrive directly in user SGPRs). The S# lives
        // at the `ssamp` SGPR block, recorded separately as `s_offset`.
        let tex_source = match self.vsharp_sgprs.get(&srsrc) {
            Some(&(sbase, desc_offset)) => DescriptorSource::SetPointer {
                sgpr: sbase,
                desc_offset,
            },
            None => DescriptorSource::InlineVSharp { sgpr: srsrc },
        };
        let s_offset = ssamp as u32;
        let vdata0 = match vdata {
            Operand::Vgpr(n) => n,
            other => {
                return Err(RecompileError::InvalidOperand {
                    inst: RecompileError::pending_inst(),
                    operand: other,
                    offset: off,
                    reason: "MIMG vdata must be a VGPR",
                });
            }
        };
        let (vu, vv) = match vaddr {
            Operand::Vgpr(n) => {
                let vv = n
                    .checked_add(1)
                    .filter(|r| (*r as usize) < crate::interp::NUM_VGPRS);
                let Some(vv) = vv else {
                    return Err(RecompileError::InvalidRegister {
                        kind: "vgpr",
                        reg: n as usize + 1,
                        max: crate::interp::NUM_VGPRS,
                        offset: off,
                    });
                };
                (n, vv)
            }
            other => {
                return Err(RecompileError::InvalidOperand {
                    inst: RecompileError::pending_inst(),
                    operand: other,
                    offset: off,
                    reason: "MIMG vaddr must be a VGPR",
                });
            }
        };
        // Coordinate = vec2(v[vaddr], v[vaddr+1]).
        let u = self.load_reg_f32(true, vu);
        let v = self.load_reg_f32(true, vv);
        let tex = self.ensure_ps_texture(tex_source, s_offset, off)?;
        let coord = self
            .b
            .composite_construct(tex.v2f32, None, [u, v])
            .expect("coord vec2");
        // Load the combined image-sampler and sample it. ImplicitLod picks the LOD from
        // screen-space derivatives — valid in a fragment shader, matching the GPU's
        // default sample behavior; the subset has no mips, so LOD 0 is always chosen.
        let si = self
            .b
            .load(tex.sampled_image_ty, None, tex.var, None, [])
            .expect("load sampled image");
        let rgba = self
            .b
            .image_sample_implicit_lod(
                self.t.v4f32,
                None,
                si,
                coord,
                Some(spirv::ImageOperands::NONE),
                [],
            )
            .expect("image sample");
        // Write the enabled dmask channels to consecutive vdata VGPRs (hardware packs
        // enabled channels contiguously — dst[0] = first enabled). `dreg` is a `usize`,
        // mirroring interp::exec_mimg: a `u8` counter can never satisfy the
        // `>= NUM_VGPRS` (256) guard, and incrementing it past 255 overflow-panics in
        // debug (wraps to v0 in release), so an over-the-top image_sample would fault the
        // recompiler or silently misdirect channels. With a `usize` the guard fires and
        // the destination faults with InvalidRegister exactly like the oracle.
        let mut dreg = vdata0 as usize;
        for ch in 0..4u32 {
            if dmask & (1 << ch) == 0 {
                continue;
            }
            let comp = self
                .b
                .composite_extract(self.t.f32, None, rgba, [ch])
                .expect("rgba extract");
            if dreg >= crate::interp::NUM_VGPRS {
                return Err(RecompileError::InvalidRegister {
                    kind: "vgpr",
                    reg: dreg,
                    max: crate::interp::NUM_VGPRS,
                    offset: off,
                });
            }
            self.store_reg_f32(true, dreg as u8, comp);
            dreg += 1;
        }
        Ok(())
    }

    /// Materialize the PS combined image-sampler resource on first use: a 2D float
    /// `OpTypeImage` (sampled=1, no depth/array/MS), its `OpTypeSampledImage`, and a
    /// `UniformConstant` variable decorated `DescriptorSet`/`Binding`. This is the
    /// portable combined image-sampler form MoltenVK/Metal accepts (decision-3) — no
    /// separate-sampler or non-portable image capability.
    fn ensure_ps_texture(
        &mut self,
        source: DescriptorSource,
        s_offset: u32,
        off: u32,
    ) -> Result<PsTexture, RecompileError> {
        // A sample through a descriptor pair we already declared reuses that binding —
        // a shader that reads one texture N times keeps ONE combined image-sampler (and
        // so stays byte-identical to the pre-task-199 single-binding emission). A pair we
        // have not seen names a genuinely different texture and gets its own binding: the
        // GCN `srsrc`/`ssamp` operands are per-instruction, and Celeste's distortion and
        // colour-grade passes each mix a register-resident T# with a memory-resident one.
        if let Some(t) = self
            .ps_textures
            .iter()
            .find(|t| t.source == source && t.s_offset == s_offset)
        {
            return Ok(PsTexture {
                var: t.var,
                sampled_image_ty: t.sampled_image_ty,
                v2f32: t.v2f32,
                source: t.source,
                s_offset: t.s_offset,
            });
        }
        let index = self.ps_textures.len();
        if index >= MAX_PS_TEXTURES {
            return Err(RecompileError::Unsupported {
                offset: off,
                reason: "PS declares more distinct image_sample descriptors than the \
                         set-0 layout reserves",
            });
        }
        let binding = ps_texture_binding(index);
        // OpTypeImage %f32 2D 0(no depth) 0(no array) 0(no MS) 1(sampled) Unknown.
        let image_ty = self.b.type_image(
            self.t.f32,
            spirv::Dim::Dim2D,
            0,
            0,
            0,
            1,
            spirv::ImageFormat::Unknown,
            None,
        );
        let sampled_image_ty = self.b.type_sampled_image(image_ty);
        let v2f32 = self.b.type_vector(self.t.f32, 2);
        let ptr_uc =
            self.b
                .type_pointer(None, spirv::StorageClass::UniformConstant, sampled_image_ty);
        let var = self.global_variable(ptr_uc, spirv::StorageClass::UniformConstant);
        self.b.decorate(
            var,
            spirv::Decoration::DescriptorSet,
            [DrOperand::LiteralBit32(PS_TEXTURE_SET)],
        );
        self.b.decorate(
            var,
            spirv::Decoration::Binding,
            [DrOperand::LiteralBit32(binding)],
        );
        // Not an Input/Output — a UniformConstant resource is excluded from the SPIR-V
        // ≤1.3 entry-point interface (same as the VS SSBO / push constant). A later 1.4
        // target bump would add it; for the committed 1.3 floor it stays out.
        self.io_samplers.push(SamplerBinding {
            set: PS_TEXTURE_SET,
            binding,
            source,
            s_offset,
        });
        self.ps_textures.push(PsTexture {
            var,
            sampled_image_ty,
            v2f32,
            source,
            s_offset,
        });
        Ok(PsTexture {
            var,
            sampled_image_ty,
            v2f32,
            source,
            s_offset,
        })
    }

    // ---- EXP ---------------------------------------------------------------

    fn emit_exp(
        &mut self,
        target: ExportTarget,
        srcs: &[Option<Operand>; 4],
        compr: bool,
        off: u32,
    ) -> Result<(), RecompileError> {
        // Gather the four channel values (a disabled channel is 0.0, as the oracle
        // records).
        let mut comps = [None; 4];
        if compr {
            // Compressed export: srcs[0]/srcs[1] each hold TWO f16 channels packed
            // into a u32 (from v_cvt_pkrtz_f16_f32). Unpack back to f32 (the HLE MRT
            // is f32-typed; the pipeline converts to the real RT format). Mirrors the
            // interp oracle's UnpackHalf2x16-equivalent in exec_exp.
            for (pair, slot) in srcs[..2].iter().enumerate() {
                if let Some(src) = slot {
                    let packed = self.eval_bits(*src, off)?;
                    let v2f32 = self.b.type_vector(self.t.f32, 2);
                    let unpacked = self
                        .b
                        .ext_inst(
                            v2f32,
                            None,
                            self.glsl_ext,
                            glsl::UNPACK_HALF_2X16,
                            [DrOperand::IdRef(packed)],
                        )
                        .expect("unpackHalf2x16");
                    let lo = self
                        .b
                        .composite_extract(self.t.f32, None, unpacked, [0])
                        .expect("extract lo");
                    let hi = self
                        .b
                        .composite_extract(self.t.f32, None, unpacked, [1])
                        .expect("extract hi");
                    comps[pair * 2] = Some(lo);
                    comps[pair * 2 + 1] = Some(hi);
                }
            }
        } else {
            for (ch, slot) in srcs.iter().enumerate() {
                if let Some(src) = slot {
                    comps[ch] = Some(self.eval_f32(*src, off)?);
                }
            }
        }
        match (self.stage, target) {
            (ShaderStage::Vertex, ExportTarget::Pos(0)) => {
                let v = self.build_vec4(&comps);
                let pos = self.ensure_position_out();
                self.b.store(pos, v, None, []).expect("store position");
                self.exports_position = true;
            }
            (ShaderStage::Vertex, ExportTarget::Param(n)) => {
                let v = self.build_vec4(&comps);
                let loc = self.ensure_loc_output(n as u32, IoRole::Attribute(n));
                self.b.store(loc, v, None, []).expect("store param");
            }
            (ShaderStage::Fragment, ExportTarget::Mrt(n)) => {
                let v = self.build_vec4(&comps);
                let loc = self.ensure_loc_output(n as u32, IoRole::RenderTarget(n));
                self.b.store(loc, v, None, []).expect("store mrt");
            }
            (_, ExportTarget::Null) => { /* no-op export */ }
            (stage, tgt) => {
                let _ = (stage, tgt);
                return Err(RecompileError::Shape(
                    "export target not valid for this stage",
                ));
            }
        }
        Ok(())
    }

    fn build_vec4(&mut self, comps: &[Option<spirv::Word>; 4]) -> spirv::Word {
        let zero = self.const_f32(0.0f32.to_bits());
        let ids: Vec<spirv::Word> = comps.iter().map(|c| c.unwrap_or(zero)).collect();
        self.b
            .composite_construct(self.t.v4f32, None, ids)
            .expect("vec4 construct")
    }

    // ---- interface variable materialization --------------------------------

    /// Create an `OpVariable` at *module* scope (module-level storage classes:
    /// Input/Output/StorageBuffer/PushConstant must not be function-local). The
    /// builder places a variable into the current block when one is selected, so we
    /// deselect, create the global, then reselect the entry block (index 0).
    fn global_variable(&mut self, ptr_type: spirv::Word, sc: spirv::StorageClass) -> spirv::Word {
        // Creating a global OpVariable requires no block selected (it lands in
        // types_global_values, not a function block). Save the CURRENTLY selected block
        // and restore it afterwards — NOT a hardcoded block 0. With a multi-block CFG a
        // global materialized while emitting a non-entry block (e.g. an export in the
        // merge block) must return to THAT block, or the following store/terminator
        // would land in the wrong block (invalid SPIR-V).
        let prev = self.b.selected_block();
        self.b.select_block(None).expect("deselect block");
        let var = self.b.variable(ptr_type, None, sc, None);
        self.b.select_block(prev).expect("reselect prior block");
        var
    }

    fn load_vertex_index(&mut self) -> spirv::Word {
        let var = self.ensure_vertex_index();
        self.b
            .load(self.t.u32, None, var, None, [])
            .expect("load vidx")
    }

    fn ensure_vertex_index(&mut self) -> spirv::Word {
        if let Some(v) = self.vertex_index {
            return v;
        }
        let ptr_in_u32 = self
            .b
            .type_pointer(None, spirv::StorageClass::Input, self.t.u32);
        let var = self.global_variable(ptr_in_u32, spirv::StorageClass::Input);
        // DRAW-MODE ASSUMPTION: `gl_VertexIndex` here must equal the interp's fetch
        // index. The interp indexes by `first_vertex + lane` (a sequential index), which
        // matches a NON-indexed `vkCmdDraw` whose `firstVertex` seeds `gl_VertexIndex`
        // sequentially. It does NOT match `vkCmdDrawIndexed`, where `gl_VertexIndex`
        // comes from the index buffer. The provider MUST issue this shader with a
        // sequential (non-indexed) draw, or the recompiled VS diverges from the oracle.
        // gl_VertexIndex builtin (VertexIndex = 42). Portable core builtin.
        self.b.decorate(
            var,
            spirv::Decoration::BuiltIn,
            [DrOperand::BuiltIn(spirv::BuiltIn::VertexIndex)],
        );
        self.interface.push(var);
        self.vertex_index = Some(var);
        var
    }

    fn ensure_position_out(&mut self) -> spirv::Word {
        if let Some(v) = self.position_out {
            return v;
        }
        let ptr_out_v4 = self
            .b
            .type_pointer(None, spirv::StorageClass::Output, self.t.v4f32);
        let var = self.global_variable(ptr_out_v4, spirv::StorageClass::Output);
        self.b.decorate(
            var,
            spirv::Decoration::BuiltIn,
            [DrOperand::BuiltIn(spirv::BuiltIn::Position)],
        );
        self.interface.push(var);
        self.position_out = Some(var);
        var
    }

    fn ensure_loc_output(&mut self, location: u32, role: IoRole) -> spirv::Word {
        if let Some(l) = self.loc_outputs.get(&location) {
            return l.var;
        }
        let ptr_out_v4 = self
            .b
            .type_pointer(None, spirv::StorageClass::Output, self.t.v4f32);
        let var = self.global_variable(ptr_out_v4, spirv::StorageClass::Output);
        self.b.decorate(
            var,
            spirv::Decoration::Location,
            [DrOperand::LiteralBit32(location)],
        );
        self.interface.push(var);
        self.loc_outputs.insert(location, LocVar { var });
        self.io_outputs.push(IoVar {
            location,
            components: 4,
            role,
        });
        var
    }

    /// Read channel `chan` of PS interpolant attribute `attr`, materializing the
    /// `vec4` Input variable for the attribute on first use and coalescing the
    /// per-Location `IoVar` component count.
    ///
    /// The channel is read via `OpLoad` of the whole `vec4` + `OpCompositeExtract`
    /// of `chan`, rather than a scalar Input decorated `Component=chan`. Both are
    /// valid SPIR-V, but MoltenVK/Metal mistranslates the scalar+`Component` form
    /// (it can read component 0 for every channel), so the vec4+extract pattern is
    /// the portable one (decision-3).
    fn read_ps_input(&mut self, attr: u8, chan: u8) -> spirv::Word {
        let ch = (chan & 0x3) as u32;
        // The GCN attribute SLOT is not the VS export parameter it reads: the draw's
        // `SPI_PS_INPUT_CNTL_<attr>.OFFSET` picks that. Resolve it here, and key the
        // variable cache on the RESULT — distinct slots routed to the same parameter must
        // share one Input variable (duplicate `Location` decorations are invalid SPIR-V).
        let location = self.ps_input_map.location_for(attr);
        // The Input variable is ALWAYS a `vec4` here; `IoVar.components` below records
        // only the channels actually read (channels-used metadata), never the SPIR-V
        // width. The provider MUST emit a `vec4` output at the matching Location — see
        // the `IoVar.components` contract for why a narrower output fails spirv-val.
        let var = if let Some(inp) = self.ps_inputs.get(&location) {
            let io_index = inp.io_index;
            // Widen the coalesced component count to cover this channel.
            let want = ch + 1;
            if self.io_inputs[io_index].components < want {
                self.io_inputs[io_index].components = want;
            }
            inp.var
        } else {
            let ptr_in_v4 = self
                .b
                .type_pointer(None, spirv::StorageClass::Input, self.t.v4f32);
            let var = self.global_variable(ptr_in_v4, spirv::StorageClass::Input);
            self.b.decorate(
                var,
                spirv::Decoration::Location,
                [DrOperand::LiteralBit32(location)],
            );
            self.interface.push(var);
            let io_index = self.io_inputs.len();
            // `role` names the attr slot that FIRST reached this location — diagnostics
            // only; `location` is what the provider wiring matches on.
            self.io_inputs.push(IoVar {
                location,
                components: ch + 1,
                role: IoRole::Attribute(attr),
            });
            self.ps_inputs.insert(location, PsInput { var, io_index });
            var
        };
        let vec = self
            .b
            .load(self.t.v4f32, None, var, None, [])
            .expect("interp vec4 load");
        self.b
            .composite_extract(self.t.f32, None, vec, [ch])
            .expect("interp channel extract")
    }

    /// Load vertex `stream`'s push-constant group member `member` (0 = `num_records`,
    /// 1 = `stride`, 2 = `dst_sel`, 3 = `format`) as a `uint`. Each stream owns a contiguous
    /// 4-uint group (`4 * stream`) in the shared push-constant block; the access chain
    /// selects the group member (task-153, task-164).
    fn load_pc_member(&mut self, stream: usize, member: u32) -> spirv::Word {
        let pc = self.ensure_pc_block();
        let idx = self.const_u32(pc_member(stream, member));
        let ptr_pc_u32 = self
            .b
            .type_pointer(None, spirv::StorageClass::PushConstant, self.t.u32);
        let ptr = self
            .b
            .access_chain(ptr_pc_u32, None, pc, [idx])
            .expect("pc access");
        self.b
            .load(self.t.u32, None, ptr, None, [])
            .expect("load pc member")
    }

    /// The `num_records` fetch clamp for vertex `stream` (its group's member 0).
    fn load_num_records(&mut self, stream: usize) -> spirv::Word {
        self.load_pc_member(stream, PC_NUM_RECORDS_MEMBER)
    }

    /// The vertex element stride in BYTES for `stream` (its group's member 1). Read
    /// dynamically per draw so one module serves every stride without re-specialization
    /// (task-140).
    fn load_stride(&mut self, stream: usize) -> spirv::Word {
        self.load_pc_member(stream, PC_STRIDE_MEMBER)
    }

    /// The packed vertex `dst_sel` for `stream` (its group's member 2, task-155). Read
    /// dynamically per draw so one module honors any swizzle without re-specialization.
    fn load_dst_sel(&mut self, stream: usize) -> spirv::Word {
        self.load_pc_member(stream, PC_DST_SEL_MEMBER)
    }

    /// The packed vertex FORMAT for `stream` (its group's member 3, task-164): `dfmt` in
    /// `[7:0]`, `nfmt` in `[15:8]`. Read dynamically per draw so one module unpacks any
    /// vertex format without re-specialization.
    fn load_format(&mut self, stream: usize) -> spirv::Word {
        self.load_pc_member(stream, PC_FORMAT_MEMBER)
    }

    /// The shared `OpTypeStruct { OpTypeRuntimeArray uint (ArrayStride 4) } (Block,
    /// member Offset 0)` block type used by both the const-buffer and vertex-buffer
    /// SSBOs. rspirv dedups identical `OpType*` to one id, so the type + its
    /// `ArrayStride`/`Block`/`Offset` decorations must be emitted EXACTLY ONCE — a
    /// per-emitter re-decoration produces a duplicate-`ArrayStride` module that
    /// spirv-val rejects (the Celeste VS wall: it has both SSBOs). Memoized here so the
    /// second SSBO reuses the already-decorated block id.
    fn dword_ssbo_block(&mut self) -> spirv::Word {
        if let Some(b) = self.dword_ssbo_block {
            return b;
        }
        let rt_array = self.b.type_runtime_array(self.t.u32);
        self.b.decorate(
            rt_array,
            spirv::Decoration::ArrayStride,
            [DrOperand::LiteralBit32(4)],
        );
        let block = self.b.type_struct([rt_array]);
        self.b.decorate(block, spirv::Decoration::Block, []);
        self.b.member_decorate(
            block,
            0,
            spirv::Decoration::Offset,
            [DrOperand::LiteralBit32(0)],
        );
        self.dword_ssbo_block = Some(block);
        block
    }

    fn ensure_pc_block(&mut self) -> spirv::Word {
        if let Some(v) = self.pc_block {
            return v;
        }
        // The VS fetch push-constant block: `MAX_VS_STREAMS` contiguous 4-uint groups, one
        // per vertex stream (task-153). Group `s` lives at members `[4s .. 4s+4)` / bytes
        // `[16s .. 16s+16)`: member 4s (+0 B) = num_records (fetch clamp), 4s+1 (+4 B) =
        // stride in bytes (task-140), 4s+2 (+8 B) = packed dst_sel (task-155), 4s+3 (+12 B) =
        // packed format (dfmt/nfmt; task-164). The TYPE is fixed-size (64 B — well under the
        // 128-B guaranteed push range) so its emission needn't wait for the final stream
        // count; only the groups the module actually fetches are exported in
        // `io_push_constants` (see `finish`), so the provider pushes exactly those.
        // Block-decorated; PushConstant is a portable, MoltenVK-safe storage class.
        //
        // CONTRACT: the host-pipeline provider MUST push each exported group's four uints
        // (that stream's V# num_records, stride, low-12 word3 dst_sel, and packed dfmt/nfmt)
        // at their byte offsets. A missing/zero-initialized value degenerates the fetch clamp
        // to index 0, the stride to 0, dst_sel to "constant 0.0 for every channel", and the
        // format to `dfmt` 0 (the raw-dword path) — which neither
        // spirv-val nor the CPU oracle (it reads these from the V#, not the block) catches.
        const MEMBER_COUNT: usize = PC_MEMBERS_PER_STREAM as usize * MAX_VS_STREAMS;
        let members = [self.t.u32; MEMBER_COUNT];
        let block = self.b.type_struct(members);
        self.b.decorate(block, spirv::Decoration::Block, []);
        for m in 0..MEMBER_COUNT as u32 {
            self.b.member_decorate(
                block,
                m,
                spirv::Decoration::Offset,
                [DrOperand::LiteralBit32(m * 4)],
            );
        }
        let ptr_pc = self
            .b
            .type_pointer(None, spirv::StorageClass::PushConstant, block);
        let var = self.global_variable(ptr_pc, spirv::StorageClass::PushConstant);
        // Not an Input/Output — excluded from the SPIR-V ≤1.3 entry-point interface.
        self.pc_block = Some(var);
        var
    }

    /// Populate [`Self::io_push_constants`] with one 3-field group per fetched vertex
    /// stream (task-153). Called at [`Self::finish`] once every stream is known, so the
    /// provider pushes exactly the groups the module reads — no more, no less.
    fn finalize_push_constants(&mut self) {
        const FIELD_SIZE: u32 = 4;
        for stream in 0..self.vs_streams.len() {
            let base = pc_member(stream, 0) * 4;
            self.io_push_constants.push(PushConstantField {
                offset_bytes: base + PC_NUM_RECORDS_MEMBER * 4,
                size_bytes: FIELD_SIZE,
                role: PushConstantRole::NumRecords,
                stream: stream as u32,
            });
            self.io_push_constants.push(PushConstantField {
                offset_bytes: base + PC_STRIDE_MEMBER * 4,
                size_bytes: FIELD_SIZE,
                role: PushConstantRole::Stride,
                stream: stream as u32,
            });
            self.io_push_constants.push(PushConstantField {
                offset_bytes: base + PC_DST_SEL_MEMBER * 4,
                size_bytes: FIELD_SIZE,
                role: PushConstantRole::DstSel,
                stream: stream as u32,
            });
            self.io_push_constants.push(PushConstantField {
                offset_bytes: base + PC_FORMAT_MEMBER * 4,
                size_bytes: FIELD_SIZE,
                role: PushConstantRole::Format,
                stream: stream as u32,
            });
        }
    }

    /// Find-or-create the vertex-buffer SSBO stream for this V# `source` (task-153). A VS
    /// that fetches several distinct V# (attr0/attr1/attr2 — interleaved or separate
    /// buffers) gets ONE binding + one push-constant group PER distinct source; a repeat
    /// fetch from the same source reuses its stream. Returns the stream's SSBO handle +
    /// index (the index selects its push-constant group and descriptor binding). Streams
    /// beyond [`MAX_VS_STREAMS`] fold onto the last one (a clean over-fetch cap rather than
    /// an unbounded binding blow-up — no real VS in the corpus/Celeste exceeds the cap).
    fn ensure_vs_buffer(&mut self, components: u8, source: DescriptorSource) -> VsBuffer {
        // Reuse an existing stream for the same source (the common per-component repeat, and
        // any later fetch from the same V#).
        if let Some(idx) = self.vs_streams.iter().position(|s| s.source == source) {
            let s = &self.vs_streams[idx];
            let out = VsBuffer {
                var: s.var,
                ptr_member: s.ptr_member,
                stream: idx,
            };
            // Record the MAX fetch width for this stream: a later fetch reading more
            // components than the first must not leave the binding under-reported (the
            // provider sizes the descriptor's element from this count).
            if let Some(binding) = self.io_buffers.get_mut(idx)
                && (components as u32) > binding.components
            {
                binding.components = components as u32;
            }
            return out;
        }
        // Cap the stream count: fold a beyond-cap fetch onto the last declared stream
        // rather than allocating an unbounded number of bindings.
        if self.vs_streams.len() >= MAX_VS_STREAMS {
            let idx = self.vs_streams.len() - 1;
            let s = &self.vs_streams[idx];
            return VsBuffer {
                var: s.var,
                ptr_member: s.ptr_member,
                stream: idx,
            };
        }
        let stream = self.vs_streams.len();
        let binding = vs_stream_binding(stream);
        // struct VertexBuffer { uint data[]; } as a StorageBuffer — a dword-addressed
        // runtime array (ArrayStride 4), the SAME shared dword-SSBO block as the
        // const-buffer (decorated once; see `dword_ssbo_block`). The per-vertex byte
        // stride is a PUSH CONSTANT (per-stream group — see `ensure_pc_block`), so one
        // module serves every stride dynamically: the provider pushes the guest V#'s
        // stride and the fetch addresses `(index * stride)/4 + comp` dwords. StorageBuffer
        // + std430 is portable (MoltenVK/portability-subset safe — no extra capability).
        let block = self.dword_ssbo_block();
        let ptr_ssbo = self
            .b
            .type_pointer(None, spirv::StorageClass::StorageBuffer, block);
        let var = self.global_variable(ptr_ssbo, spirv::StorageClass::StorageBuffer);
        self.b.decorate(
            var,
            spirv::Decoration::DescriptorSet,
            [DrOperand::LiteralBit32(VS_BUFFER_SET)],
        );
        self.b.decorate(
            var,
            spirv::Decoration::Binding,
            [DrOperand::LiteralBit32(binding)],
        );
        // Pointer to a uint member (StorageBuffer class) for the access chain load.
        let ptr_member = self
            .b
            .type_pointer(None, spirv::StorageClass::StorageBuffer, self.t.u32);
        self.vs_streams.push(VsStream {
            var,
            ptr_member,
            source,
        });
        self.io_buffers.push(BufferBinding {
            set: VS_BUFFER_SET,
            binding,
            stride_bytes: VB_ELEMENT_STRIDE,
            components: components as u32,
            source,
        });
        VsBuffer {
            var,
            ptr_member,
            stream,
        }
    }

    // ---- helpers -----------------------------------------------------------

    fn vgpr_dst(&self, vdst: Operand, off: u32) -> Result<u8, RecompileError> {
        match vdst {
            Operand::Vgpr(n) if (n as usize) < crate::interp::NUM_VGPRS => Ok(n),
            Operand::Vgpr(n) => Err(RecompileError::InvalidRegister {
                kind: "vgpr",
                reg: n as usize,
                max: crate::interp::NUM_VGPRS,
                offset: off,
            }),
            other => Err(RecompileError::InvalidOperand {
                inst: RecompileError::pending_inst(),
                operand: other,
                offset: off,
                reason: "not a vector destination",
            }),
        }
    }
}

/// The recompiler's implementation of the shared [`crate::uop::AluBuilder`] value
/// algebra (task-131). `Val` is an f32-typed SPIR-V result id; each method emits the
/// EXACT builder / GLSL.std.450 instruction the hand-written arm emitted, so the shared
/// per-opcode body produces the byte-identical instruction sequence the golden
/// `spirv-dis` snapshot fences. The per-invocation execution model (predication,
/// vertex-index tracking, register I/O) stays in `emit_vop2` / `emit_vop3` around this.
impl crate::uop::AluBuilder for Recompiler {
    type Val = spirv::Word;

    fn const_f32_bits(&mut self, bits: u32) -> spirv::Word {
        self.const_f32(bits)
    }
    fn f_add(&mut self, a: spirv::Word, b: spirv::Word) -> spirv::Word {
        self.b.f_add(self.t.f32, None, a, b).expect("fadd")
    }
    fn f_sub(&mut self, a: spirv::Word, b: spirv::Word) -> spirv::Word {
        self.b.f_sub(self.t.f32, None, a, b).expect("fsub")
    }
    fn f_mul(&mut self, a: spirv::Word, b: spirv::Word) -> spirv::Word {
        self.b.f_mul(self.t.f32, None, a, b).expect("fmul")
    }
    fn f_min(&mut self, a: spirv::Word, b: spirv::Word) -> spirv::Word {
        // GLSL FMin returns the non-NaN operand, matching the oracle's f32::min.
        self.glsl2(glsl::FMIN, a, b)
    }
    fn f_max(&mut self, a: spirv::Word, b: spirv::Word) -> spirv::Word {
        self.glsl2(glsl::FMAX, a, b)
    }
    fn f_fma(&mut self, a: spirv::Word, b: spirv::Word, c: spirv::Word) -> spirv::Word {
        // FUSED (single rounding): GLSL.std.450 Fma, matching the oracle's mul_add.
        self.b
            .ext_inst(
                self.t.f32,
                None,
                self.glsl_ext,
                glsl::FMA,
                [
                    DrOperand::IdRef(a),
                    DrOperand::IdRef(b),
                    DrOperand::IdRef(c),
                ],
            )
            .expect("fma")
    }
    fn f_abs(&mut self, a: spirv::Word) -> spirv::Word {
        self.glsl1(glsl::FABS, a)
    }
    fn f_neg(&mut self, a: spirv::Word) -> spirv::Word {
        self.b.f_negate(self.t.f32, None, a).expect("fnegate")
    }
    fn f_fract(&mut self, a: spirv::Word) -> spirv::Word {
        self.emit_fract(a)
    }
}

#[cfg(test)]
mod tests {
    /// Pure-Rust re-execution of the lowering `Recompiler::emit_cvt_f_to_int` emits:
    /// map NaN to 0, clamp into the safely-convertible band (`FMax`/`FMin` against the
    /// same bit constants the emitter uses), truncate toward zero (in-range
    /// `OpConvertFToU`/`OpConvertFToS`), then pin the top of the range to the exact
    /// integer max. In-range clamped values are truncated by Rust's `as` too, so `as`
    /// here faithfully stands in for the SPIR-V convert.
    fn model_cvt(f: f32, signed: bool) -> u32 {
        let f_safe = if f.is_nan() { 0.0 } else { f }; // NaN -> 0.0 (models the ordered-equal drop)
        let (lo, hi, thr, int_max) = if signed {
            (
                -2147483648.0f32,            // -2^31 (== i32::MIN, exact)
                f32::from_bits(0x4EFF_FFFF), // 2147483520.0 = 2^31 - 128, largest f32 < 2^31
                f32::from_bits(0x4F00_0000), // 2147483648.0 = 2^31
                i32::MAX as u32,
            )
        } else {
            (
                0.0f32,
                f32::from_bits(0x4F7F_FFFF), // 4294967040.0 = 2^32 - 256, largest f32 < 2^32
                f32::from_bits(0x4F80_0000), // 4294967296.0 = 2^32
                u32::MAX,
            )
        };
        // No NaN operand remains, so FMax/FMin are the plain numeric clamp.
        let clamped = f_safe.max(lo).min(hi);
        let conv = if signed {
            (clamped as i32) as u32
        } else {
            clamped as u32
        };
        if f >= thr { int_max } else { conv }
    }

    /// Witnesses that the emitted `v_cvt_u32_f32` / `v_cvt_i32_f32` lowering reproduces
    /// the interp oracle's saturating cast (Rust `f32 as u32` / `f32 as i32`, saturating
    /// since Rust 1.45) for out-of-range and NaN inputs — where a bare
    /// `OpConvertFToU`/`OpConvertFToS` is undefined (SPIR-V 1.0) and would give a
    /// driver-defined result diverging from the differential CPU oracle.
    #[test]
    fn cvt_f_to_int_matches_saturating_oracle() {
        // Boundary check: the emitter's clamp/threshold literals are the intended f32s.
        assert_eq!(f32::from_bits(0x4EFF_FFFF), 2_147_483_520.0);
        assert_eq!(f32::from_bits(0x4F00_0000), 2_147_483_648.0);
        assert_eq!(f32::from_bits(0x4F7F_FFFF), 4_294_967_040.0);
        assert_eq!(f32::from_bits(0x4F80_0000), 4_294_967_296.0);

        let probes = [
            0.0f32,
            -0.0,
            1.0,
            -1.0,
            1.5,
            -1.5,
            42.9,
            1e9,
            -1e9,
            1e20,
            -1e20,
            f32::INFINITY,
            f32::NEG_INFINITY,
            f32::NAN,
            2_147_483_520.0, // 2^31 - 128, largest f32 below the i32 saturation point
            2_147_483_648.0, // 2^31, exact i32 saturation threshold
            2_147_483_904.0, // just above 2^31: in range for u32, saturates for i32
            -2_147_483_648.0, // i32::MIN, exact
            4_294_967_040.0, // 2^32 - 256, largest f32 below the u32 saturation point
            4_294_967_296.0, // 2^32, exact u32 saturation threshold
        ];
        for &f in &probes {
            assert_eq!(
                model_cvt(f, true),
                (f as i32) as u32,
                "i32 cvt mismatch for {f:?}"
            );
            assert_eq!(model_cvt(f, false), f as u32, "u32 cvt mismatch for {f:?}");
        }
    }
}
