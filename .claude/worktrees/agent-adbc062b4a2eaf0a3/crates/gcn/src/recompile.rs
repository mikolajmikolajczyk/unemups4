//! GCN → SPIR-V recompiler for the straight-line triangle subset (doc-4 §1,
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
    pub const FMIN: u32 = 37;
    pub const FMAX: u32 = 40;
    pub const FMA: u32 = 50;
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
}

/// What a [`PushConstantField`] carries.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PushConstantRole {
    /// The vertex-buffer `num_records` (from the V#) used by the VS fetch clamp.
    /// The provider MUST push this — a zero/missing value clamps every fetch to
    /// element 0.
    NumRecords,
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
    /// Element stride in bytes baked into the module's `OpTypeRuntimeArray`
    /// `ArrayStride` (`16` = one `vec4` per vertex). See [`VB_ELEMENT_STRIDE`] for
    /// the LIMITATION: the recompiler cannot see the guest V# stride at recompile
    /// time (it resolves the descriptor symbolically), so this is fixed at 16 and
    /// the provider MUST reject a bound V# whose stride differs.
    pub stride_bytes: u32,
    /// Number of `f32` components fetched per element (`4` for
    /// `buffer_load_format_xyzw`).
    pub components: u32,
}

/// The I/O + resource metadata a recompiled module needs at bind time. This lives
/// in `ps4-gcn` (which cannot depend on `ps4-gnm` — that would be a cycle); the
/// host-pipeline provider (`HostShader` construction in `ps4-gnm`) maps it into the
/// host pipeline.
///
/// DRAW-MODE ASSUMPTION (VS): a recompiled VS fetches by `gl_VertexIndex`, which must
/// equal the oracle's sequential `first_vertex + lane` fetch index. That holds for a
/// non-indexed `vkCmdDraw` (whose `firstVertex` seeds `gl_VertexIndex` sequentially),
/// NOT for `vkCmdDrawIndexed` (where `gl_VertexIndex` is index-buffer driven). The
/// provider MUST drive this shader with a sequential (non-indexed) draw.
#[derive(Clone, PartialEq, Debug)]
pub struct IoLayout {
    /// The pipeline stage.
    pub stage: ShaderStage,
    /// `Location` interface inputs (PS interpolants), one entry per Location.
    pub inputs: Vec<IoVar>,
    /// `Location` interface outputs (VS params, PS render targets).
    pub outputs: Vec<IoVar>,
    /// Descriptor-backed buffer bindings the module reads (empty for a PS with no
    /// resource fetch).
    pub buffers: Vec<BufferBinding>,
    /// Combined image-sampler bindings the module samples through (empty for a shader
    /// that samples no texture). A PS with `image_sample` declares exactly one; the
    /// provider points it at the bound texture + sampler at draw time.
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
    /// An operand the recompiler cannot lower in this position.
    #[error("invalid operand at dword offset {offset}: {operand:?} ({reason})")]
    InvalidOperand {
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

// ---- descriptor / interface layout constants -------------------------------

/// Descriptor set for the vertex-buffer V# (the only resource the corpus VS uses).
const VS_BUFFER_SET: u32 = 0;
/// Binding within [`VS_BUFFER_SET`].
const VS_BUFFER_BINDING: u32 = 0;

/// The vertex-buffer element stride, in bytes, baked into the SSBO's
/// `OpTypeRuntimeArray` `ArrayStride` and reported in [`BufferBinding::stride_bytes`].
///
/// LIMITATION: this is fixed at 16 (one `vec4` per vertex). The interpreter reads
/// the true stride from the V# (`word1[29:16]`, via `decode_v_sharp`), but the
/// recompiler resolves the descriptor symbolically and never sees the descriptor
/// bytes at recompile time, so it cannot bake a per-shader stride into the SPIR-V.
/// The corpus stride is 16, so this is exact for the corpus. For a shader whose
/// bound V# stride differs, the host-pipeline provider MUST reject the pairing (the
/// SSBO layout would mis-address every fetch); to lift this the provider would
/// re-emit the module with the runtime-array typed as a byte/`uint` array and index
/// by `stride/4` instead of a `vec4` array.
const VB_ELEMENT_STRIDE: u32 = 16;

/// Descriptor set for a PS combined image-sampler (the only sampled resource the
/// corpus PS uses). Binding 1 keeps it clear of the VS SSBO at binding 0 in set 0.
const PS_TEXTURE_SET: u32 = 0;
/// Binding within [`PS_TEXTURE_SET`] for the combined image-sampler.
const PS_TEXTURE_BINDING: u32 = 1;

/// Recompile a decoded straight-line GCN shader to a portable SPIR-V module for
/// `stage`. Mirrors [`crate::interp`]'s semantics op-for-op so the differential
/// harness can diff the two.
pub fn recompile(
    insts: &[Decoded],
    stage: ShaderStage,
) -> Result<RecompiledShader, RecompileError> {
    let mut rc = Recompiler::new(stage);
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
    /// The vertex-buffer storage buffer + its runtime-array member pointer type.
    vs_buffer: Option<VsBuffer>,
    /// The PS combined image-sampler resource (image + sampled-image types), lazily
    /// materialized on the first `image_sample`.
    ps_texture: Option<PsTexture>,
    /// `num_records` push constant (VS fetch clamp).
    num_records_pc: Option<spirv::Word>,
    /// `Location` param outputs (VS) / MRT outputs (PS), keyed by location.
    loc_outputs: HashMap<u32, LocVar>,
    /// `Location` interpolant inputs (PS): one `vec4` Input variable per attribute
    /// (Location), keyed by attr. A VINTRP `chan` read extracts the channel from the
    /// vec4 (`OpCompositeExtract`) — the MoltenVK-reliable pattern (scalar Input +
    /// `Component` decoration is mistranslated on Metal, reading channel 0 for all).
    ps_inputs: HashMap<u8, PsInput>,
    /// VGPRs currently known to carry the launch vertex index (`gl_VertexIndex`).
    /// Seeded with `v0` (the launch ABI's vertex-index register) and propagated
    /// through `v_mov_b32 vN, vM` so an idxen MUBUF that relocates the index into
    /// another VGPR still resolves to `gl_VertexIndex` instead of reading an
    /// uninitialized slot. An idxen fetch on a VGPR not in this set is rejected.
    vertex_index_regs: std::collections::HashSet<u8>,
    /// SGPRs holding the fetched V# resource (SMRD dst → decoded at MUBUF time). We
    /// do not model the descriptor bytes; the fetch resolves to the bound buffer.
    /// This set records which SGPRs the SMRD wrote so a MUBUF `srsrc` referencing
    /// them resolves to the descriptor buffer rather than a wave register.
    vsharp_sgprs: std::collections::HashSet<u8>,

    // running metadata
    io_inputs: Vec<IoVar>,
    io_outputs: Vec<IoVar>,
    io_buffers: Vec<BufferBinding>,
    io_samplers: Vec<SamplerBinding>,
    io_push_constants: Vec<PushConstantField>,
    exports_position: bool,
    interface: Vec<spirv::Word>,
}

/// A `Location`-decorated interface variable id (outputs are deduped by location).
struct LocVar {
    var: spirv::Word,
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
}

/// The vertex-buffer storage buffer resource.
struct VsBuffer {
    /// The `OpVariable` (StorageBuffer storage class).
    var: spirv::Word,
    /// Pointer type to a `vec4` member of the runtime array (StorageBuffer class).
    ptr_member: spirv::Word,
    /// Recorded stride in bytes (from the V# — 16 for the corpus).
    stride_bytes: u32,
}

impl Recompiler {
    fn new(stage: ShaderStage) -> Self {
        let mut b = Builder::new();
        // Vulkan 1.1 targets SPIR-V 1.3.
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
            vs_buffer: None,
            ps_texture: None,
            num_records_pc: None,
            loc_outputs: HashMap::new(),
            ps_inputs: HashMap::new(),
            // v0 = the launch ABI's vertex index — but only for a VS. A PS's v0 is a
            // barycentric, not a vertex index, so seed the tracker only for the Vertex
            // stage (a PS never does an idxen vertex fetch anyway).
            vertex_index_regs: match stage {
                ShaderStage::Vertex => std::collections::HashSet::from([0u8]),
                ShaderStage::Fragment => std::collections::HashSet::new(),
            },
            vsharp_sgprs: std::collections::HashSet::new(),
            io_inputs: Vec::new(),
            io_outputs: Vec::new(),
            io_buffers: Vec::new(),
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
        self.b.begin_block(None).expect("entry block");

        for d in insts {
            let off = d.offset_dwords;
            match &d.inst {
                // Scalar control that the oracle treats as no-ops.
                Inst::Sopp { op, .. }
                    if *op == opcodes::sopp::S_ENDPGM
                        || *op == opcodes::sopp::S_WAITCNT
                        || *op == opcodes::sopp::S_NOP =>
                {
                    if *op == opcodes::sopp::S_ENDPGM {
                        break;
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
                } => self.emit_vop3(*op, *vdst, *src0, *src1, *src2, *abs, *neg, *omod, off)?,
                Inst::Smrd {
                    op, sdst, sbase, ..
                } => self.emit_smrd(*op, *sdst, *sbase, off)?,
                Inst::Mubuf {
                    op,
                    vdata,
                    vaddr,
                    srsrc,
                    soffset,
                    idxen,
                    offen,
                    ..
                } => self.emit_mubuf(*op, *vdata, *vaddr, *srsrc, *soffset, *idxen, *offen, off)?,
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
                Inst::Exp { target, srcs, .. } => self.emit_exp(*target, srcs, off)?,
                other => {
                    return Err(RecompileError::UnsupportedInst {
                        inst: Box::new(other.clone()),
                        offset: off,
                    });
                }
            }
        }

        self.b.ret().expect("return");
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

    fn finish(mut self) -> Result<RecompiledShader, RecompileError> {
        if self.stage == ShaderStage::Vertex && !self.exports_position {
            return Err(RecompileError::Shape(
                "vertex shader exported no clip-space position (exp pos0)",
            ));
        }
        // SPIR-V requires every function-local OpVariable to be at the top of the
        // first block. The builder appends register-slot variables lazily as the
        // stream references them, so hoist them to the block front (preserving their
        // relative order) before assembling.
        {
            let m = self.b.module_mut();
            if let Some(block) = m.functions.first_mut().and_then(|f| f.blocks.first_mut()) {
                block
                    .instructions
                    .sort_by_key(|inst| u8::from(inst.class.opcode != spirv::Op::Variable));
            }
        }
        let module = self.b.module();
        let spirv = module.assemble();
        let io = IoLayout {
            stage: self.stage,
            inputs: self.io_inputs,
            outputs: self.io_outputs,
            buffers: self.io_buffers,
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

    fn reg_u32_ptr(&mut self, is_vgpr: bool, n: u8) -> spirv::Word {
        if let Some(&id) = self.reg_u32.get(&(is_vgpr, n)) {
            return id;
        }
        let id = self
            .b
            .variable(self.t.ptr_fn_u32, None, spirv::StorageClass::Function, None);
        self.reg_u32.insert((is_vgpr, n), id);
        id
    }

    fn reg_f32_ptr(&mut self, is_vgpr: bool, n: u8) -> spirv::Word {
        if let Some(&id) = self.reg_f32.get(&(is_vgpr, n)) {
            return id;
        }
        let id = self
            .b
            .variable(self.t.ptr_fn_f32, None, spirv::StorageClass::Function, None);
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

    fn load_reg_u32(&mut self, is_vgpr: bool, n: u8) -> spirv::Word {
        let p = self.reg_u32_ptr(is_vgpr, n);
        self.b
            .load(self.t.u32, None, p, None, [])
            .expect("load u32")
    }

    fn load_reg_f32(&mut self, is_vgpr: bool, n: u8) -> spirv::Word {
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
                operand: other,
                offset: off,
                reason: "not a bit source",
            }),
        }
    }

    fn special_bits(&mut self, sr: SpecialReg, off: u32) -> Result<spirv::Word, RecompileError> {
        // m0 is never consulted for interpolation (the attribute comes from the VINTRP
        // field, per the oracle) and `s_mov m0, s0` is lowered without a modeled write,
        // so the recompiler never stores an m0 value. The interp, by contrast, keeps a
        // real m0 slot and reads it back — so an m0 *source* read is the one place the
        // recompiler cannot reproduce the interp's value. Rather than silently return an
        // uninitialized 0 (which would diverge), reject: an m0 source is outside the
        // faithfully lowerable subset.
        match sr {
            SpecialReg::M0 => Err(RecompileError::Unsupported {
                offset: off,
                reason: "m0 source read is not faithfully modeled (m0 is never written here)",
            }),
            other => Err(RecompileError::InvalidOperand {
                operand: Operand::Special(other),
                offset: off,
                reason: "special register not modeled in the subset",
            }),
        }
    }

    /// Validate that `op` is a well-formed scalar source (an in-range SGPR, an inline,
    /// a literal, or a modeled special), emitting no SPIR-V. Mirrors the *validation*
    /// half of [`crate::interp::Interp::read_scalar`] for a value we discard (e.g.
    /// `s_mov m0, ssrc0`, where the interp reads the source before storing but the
    /// recompiler keeps no m0 slot). Kept separate from `eval_bits` so validating a
    /// discarded source does not emit a dead `OpLoad`.
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
            // m0 as a *source* is not faithfully modeled (see `special_bits`).
            Operand::Special(SpecialReg::M0) => Err(RecompileError::Unsupported {
                offset: off,
                reason: "m0 source read is not faithfully modeled (m0 is never written here)",
            }),
            other => Err(RecompileError::InvalidOperand {
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
        if op != opcodes::sop1::S_MOV_B32 {
            return Err(RecompileError::UnsupportedInst {
                inst: Box::new(Inst::Sop1 { op, sdst, ssrc0 }),
                offset: off,
            });
        }
        match sdst {
            Operand::Sgpr(n) => {
                let bits = self.eval_bits(ssrc0, off)?;
                self.store_reg_bits(false, n, bits);
            }
            // `s_mov m0, s0`: m0 holds the interpolation base on real GCN, but neither
            // the recompiler nor the oracle consults it (the attribute comes from the
            // VINTRP field), so no m0 slot is written. We still evaluate ssrc0 and
            // discard it: the interp reads the source first (`read_scalar`), so a
            // malformed/out-of-range SGPR source field must error here too rather than
            // being silently swallowed.
            Operand::Special(SpecialReg::M0) => {
                self.validate_scalar_src(ssrc0, off)?;
            }
            other => {
                return Err(RecompileError::InvalidOperand {
                    operand: other,
                    offset: off,
                    reason: "not a scalar destination",
                });
            }
        }
        Ok(())
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
                let bits = self.eval_bits(src0, off)?;
                self.store_reg_bits(true, n, bits);
                if src_is_tracked_index {
                    self.vertex_index_regs.insert(n);
                } else {
                    self.vertex_index_regs.remove(&n);
                }
            }
            V_CVT_F32_I32 => {
                // Arithmetic write: clobbers any launch-vertex-index tracking on the dst.
                self.vertex_index_regs.remove(&n);
                let bits = self.eval_bits(src0, off)?;
                let asi = self.b.bitcast(self.t.i32, None, bits).expect("bitcast i32");
                let f = self.b.convert_s_to_f(self.t.f32, None, asi).expect("s->f");
                self.store_reg_f32(true, n, f);
            }
            V_CVT_F32_U32 => {
                self.vertex_index_regs.remove(&n);
                let bits = self.eval_bits(src0, off)?;
                let f = self.b.convert_u_to_f(self.t.f32, None, bits).expect("u->f");
                self.store_reg_f32(true, n, f);
            }
            V_CVT_U32_F32 => {
                self.vertex_index_regs.remove(&n);
                let f = self.eval_f32(src0, off)?;
                let u = self.b.convert_f_to_u(self.t.u32, None, f).expect("f->u");
                self.store_reg_bits(true, n, u);
            }
            V_CVT_I32_F32 => {
                self.vertex_index_regs.remove(&n);
                let f = self.eval_f32(src0, off)?;
                let i = self.b.convert_f_to_s(self.t.i32, None, f).expect("f->s");
                let bits = self.b.bitcast(self.t.u32, None, i).expect("bitcast u32");
                self.store_reg_bits(true, n, bits);
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
        self.vertex_index_regs.remove(&n);
        let a = self.eval_f32(src0, off)?;
        let b = self.eval_f32(vsrc1, off)?;
        let out = match op {
            V_ADD_F32 => self.b.f_add(self.t.f32, None, a, b).expect("fadd"),
            V_SUB_F32 => self.b.f_sub(self.t.f32, None, a, b).expect("fsub"),
            V_MUL_F32 => self.b.f_mul(self.t.f32, None, a, b).expect("fmul"),
            V_MAC_F32 => {
                // vdst = src0*vsrc1 + vdst. UNFUSED (the oracle uses `a*b + acc`, two
                // roundings), so emit OpFMul then OpFAdd — never GLSL Fma.
                let acc = self.load_reg_f32(true, n);
                let m = self.b.f_mul(self.t.f32, None, a, b).expect("fmul");
                self.b.f_add(self.t.f32, None, m, acc).expect("fadd")
            }
            V_MADMK_F32 => {
                // vdst = src0*K + vsrc1. UNFUSED (`a*kf + b`).
                let kf = self.const_f32(k.unwrap_or(0));
                let m = self.b.f_mul(self.t.f32, None, a, kf).expect("fmul");
                self.b.f_add(self.t.f32, None, m, b).expect("fadd")
            }
            V_MADAK_F32 => {
                // vdst = src0*vsrc1 + K. UNFUSED (`a*b + kf`).
                let kf = self.const_f32(k.unwrap_or(0));
                let m = self.b.f_mul(self.t.f32, None, a, b).expect("fmul");
                self.b.f_add(self.t.f32, None, m, kf).expect("fadd")
            }
            _ => {
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
        };
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
        off: u32,
    ) -> Result<(), RecompileError> {
        use opcodes::vop3::*;
        let n = self.vgpr_dst(vdst, off)?;
        // Any VOP3 write in the subset is arithmetic (no VOP3-encoded v_mov is lowered
        // here), so the dst no longer carries the launch vertex index: untrack it — see
        // the VOP2 note above for why a stale-tracked dst would silently diverge.
        self.vertex_index_regs.remove(&n);
        let a0 = self.eval_f32(src0, off)?;
        let b0 = self.eval_f32(src1, off)?;
        let c0 = self.eval_f32(src2, off)?;
        let a = self.apply_mods(a0, abs, neg, 0);
        let b = self.apply_mods(b0, abs, neg, 1);
        let c = self.apply_mods(c0, abs, neg, 2);
        let raw = match op {
            // v_mad_f32 is UNFUSED on GCN: a*b rounds, then +c rounds again — so emit
            // OpFMul + OpFAdd, NOT GLSL Fma.
            V_MAD_F32 => {
                let m = self.b.f_mul(self.t.f32, None, a, b).expect("fmul");
                self.b.f_add(self.t.f32, None, m, c).expect("fadd")
            }
            // v_fma_f32 is FUSED (single rounding): GLSL.std.450 Fma is the fused
            // multiply-add, matching the oracle's `f32::mul_add`.
            V_FMA_F32 => self
                .b
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
                .expect("fma"),
            // v_med3_f32 = median(a,b,c) = max(min(a,b), min(max(a,b), c)). The oracle
            // computes `a.max(b).min(a.min(b).max(c))` — algebraically the median.
            // DIVERGENCE(nan): GLSL FMin/FMax and Rust f32::min/max have different NaN
            // rules; the corpus feeds only finite values, so exports match. The diff
            // harness treats NaN-input med3 as out of the agreed contract.
            V_MED3_F32 => {
                let max_ab = self.glsl2(glsl::FMAX, a, b);
                let min_ab = self.glsl2(glsl::FMIN, a, b);
                let max_minab_c = self.glsl2(glsl::FMAX, min_ab, c);
                self.glsl2(glsl::FMIN, max_ab, max_minab_c)
            }
            _ => {
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
                    }),
                    offset: off,
                });
            }
        };
        let out = self.apply_omod(raw, omod);
        self.store_reg_f32(true, n, out);
        Ok(())
    }

    /// Apply the VOP3 src abs/neg modifiers (abs first, then neg), mirroring
    /// [`crate::interp::Interp::apply_mods`].
    fn apply_mods(&mut self, mut v: spirv::Word, abs: u8, neg: u8, idx: u8) -> spirv::Word {
        if abs & (1 << idx) != 0 {
            v = self.glsl1(glsl::FABS, v);
        }
        if neg & (1 << idx) != 0 {
            v = self.b.f_negate(self.t.f32, None, v).expect("fnegate");
        }
        v
    }

    /// Apply the VOP3 output modifier (1=×2, 2=×4, 3=÷2), mirroring
    /// [`crate::interp::apply_omod`]. Multiplication by an exact power of two is
    /// bit-exact, matching the oracle.
    fn apply_omod(&mut self, v: spirv::Word, omod: u8) -> spirv::Word {
        let factor = match omod {
            1 => 2.0f32,
            2 => 4.0f32,
            3 => 0.5f32,
            _ => return v,
        };
        let f = self.const_f32(factor.to_bits());
        self.b.f_mul(self.t.f32, None, v, f).expect("omod mul")
    }

    fn glsl1(&mut self, inst: u32, a: spirv::Word) -> spirv::Word {
        self.b
            .ext_inst(self.t.f32, None, self.glsl_ext, inst, [DrOperand::IdRef(a)])
            .expect("glsl1")
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

    // ---- SMRD --------------------------------------------------------------

    fn emit_smrd(
        &mut self,
        op: u8,
        sdst: Operand,
        sbase: u8,
        off: u32,
    ) -> Result<(), RecompileError> {
        // The corpus SMRD loads the vertex-buffer V# descriptor into an SGPR block.
        // We don't model the descriptor bytes; the fetch (MUBUF) resolves to the
        // bound storage buffer directly. Record which SGPRs the load wrote so the
        // MUBUF `srsrc` referencing them resolves to the buffer, not wave registers.
        let count = opcodes::smrd::dst_count(op).ok_or(RecompileError::UnsupportedInst {
            inst: Box::new(Inst::Smrd {
                op,
                sdst,
                sbase,
                imm: true,
                offset: 0,
            }),
            offset: off,
        })?;
        let dst0 = match sdst {
            Operand::Sgpr(n) => n,
            other => {
                return Err(RecompileError::InvalidOperand {
                    operand: other,
                    offset: off,
                    reason: "SMRD destination must be an SGPR",
                });
            }
        };
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
            self.vsharp_sgprs.insert(reg);
        }
        Ok(())
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
                offset: 0,
                idxen,
                offen,
            }),
            offset: off,
        })?;
        if let Operand::Raw(255) = soffset {
            return Err(RecompileError::InvalidOperand {
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
                    operand: other,
                    offset: off,
                    reason: "MUBUF vdata must be a VGPR",
                });
            }
        };
        // srsrc must name the SGPR block the SMRD wrote (the V#). We resolve the
        // fetch to the bound vertex buffer regardless of the descriptor bytes.
        let _ = srsrc;

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

        // num_records clamp: an index >= num_records clamps to num_records-1 (and 0
        // records ⇒ index 0). num_records is a bind-time value (from the V#), supplied
        // as a push constant. Mirror the oracle:
        //   idx = if nr != 0 && index >= nr { nr - 1 } else { index }
        let nr = self.load_num_records();
        let clamped = self.clamp_index(index_u32, nr);

        // Fetch `count` f32 components of the vec4 element at `clamped`.
        let buf = self.ensure_vs_buffer(count);
        for i in 0..count {
            let comp = self.fetch_buffer_component(&buf, clamped, i);
            let reg = vdata0
                .checked_add(i)
                .filter(|r| (*r as usize) < crate::interp::NUM_VGPRS)
                .ok_or(RecompileError::InvalidRegister {
                    kind: "vgpr",
                    reg: vdata0 as usize + i as usize,
                    max: crate::interp::NUM_VGPRS,
                    offset: off,
                })?;
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

    /// Fetch f32 component `comp` of the `vec4` at element `index` in the vertex
    /// buffer. Modeled as `data[index][comp]` of a runtime array of `vec4`.
    fn fetch_buffer_component(
        &mut self,
        buf: &VsBuffer,
        index: spirv::Word,
        comp: u8,
    ) -> spirv::Word {
        let zero = self.const_u32(0);
        let comp_c = self.const_u32(comp as u32);
        // access chain: buffer -> member 0 (runtime array) -> [index] -> [comp]
        let ptr = self
            .b
            .access_chain(buf.ptr_member, None, buf.var, [zero, index, comp_c])
            .expect("vb access chain");
        self.b
            .load(self.t.f32, None, ptr, None, [])
            .expect("vb load")
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
        let _ = (srsrc, ssamp);
        let vdata0 = match vdata {
            Operand::Vgpr(n) => n,
            other => {
                return Err(RecompileError::InvalidOperand {
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
                    operand: other,
                    offset: off,
                    reason: "MIMG vaddr must be a VGPR",
                });
            }
        };
        // Coordinate = vec2(v[vaddr], v[vaddr+1]).
        let u = self.load_reg_f32(true, vu);
        let v = self.load_reg_f32(true, vv);
        let tex = self.ensure_ps_texture();
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
        // enabled channels contiguously — dst[0] = first enabled).
        let mut dreg = vdata0;
        for ch in 0..4u32 {
            if dmask & (1 << ch) == 0 {
                continue;
            }
            let comp = self
                .b
                .composite_extract(self.t.f32, None, rgba, [ch])
                .expect("rgba extract");
            if (dreg as usize) >= crate::interp::NUM_VGPRS {
                return Err(RecompileError::InvalidRegister {
                    kind: "vgpr",
                    reg: dreg as usize,
                    max: crate::interp::NUM_VGPRS,
                    offset: off,
                });
            }
            self.store_reg_f32(true, dreg, comp);
            dreg += 1;
        }
        Ok(())
    }

    /// Materialize the PS combined image-sampler resource on first use: a 2D float
    /// `OpTypeImage` (sampled=1, no depth/array/MS), its `OpTypeSampledImage`, and a
    /// `UniformConstant` variable decorated `DescriptorSet`/`Binding`. This is the
    /// portable combined image-sampler form MoltenVK/Metal accepts (decision-3) — no
    /// separate-sampler or non-portable image capability.
    fn ensure_ps_texture(&mut self) -> PsTexture {
        if let Some(t) = &self.ps_texture {
            return PsTexture {
                var: t.var,
                sampled_image_ty: t.sampled_image_ty,
                v2f32: t.v2f32,
            };
        }
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
            [DrOperand::LiteralBit32(PS_TEXTURE_BINDING)],
        );
        // Not an Input/Output — a UniformConstant resource is excluded from the SPIR-V
        // ≤1.3 entry-point interface (same as the VS SSBO / push constant). A later 1.4
        // target bump would add it; for the committed 1.3 floor it stays out.
        self.io_samplers.push(SamplerBinding {
            set: PS_TEXTURE_SET,
            binding: PS_TEXTURE_BINDING,
        });
        self.ps_texture = Some(PsTexture {
            var,
            sampled_image_ty,
            v2f32,
        });
        PsTexture {
            var,
            sampled_image_ty,
            v2f32,
        }
    }

    // ---- EXP ---------------------------------------------------------------

    fn emit_exp(
        &mut self,
        target: ExportTarget,
        srcs: &[Option<Operand>; 4],
        off: u32,
    ) -> Result<(), RecompileError> {
        // Gather the four channel values (a disabled channel is 0.0, as the oracle
        // records).
        let mut comps = [None; 4];
        for (ch, slot) in srcs.iter().enumerate() {
            if let Some(src) = slot {
                comps[ch] = Some(self.eval_f32(*src, off)?);
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
        self.b.select_block(None).expect("deselect block");
        let var = self.b.variable(ptr_type, None, sc, None);
        self.b.select_block(Some(0)).expect("reselect entry block");
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
        // The Input variable is ALWAYS a `vec4` here; `IoVar.components` below records
        // only the channels actually read (channels-used metadata), never the SPIR-V
        // width. The provider MUST emit a `vec4` output at the matching Location — see
        // the `IoVar.components` contract for why a narrower output fails spirv-val.
        let var = if let Some(inp) = self.ps_inputs.get(&attr) {
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
                [DrOperand::LiteralBit32(attr as u32)],
            );
            self.interface.push(var);
            let io_index = self.io_inputs.len();
            self.io_inputs.push(IoVar {
                location: attr as u32,
                components: ch + 1,
                role: IoRole::Attribute(attr),
            });
            self.ps_inputs.insert(attr, PsInput { var, io_index });
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

    fn load_num_records(&mut self) -> spirv::Word {
        let pc = self.ensure_num_records_pc();
        let zero = self.const_u32(0);
        let ptr_pc_u32 = self
            .b
            .type_pointer(None, spirv::StorageClass::PushConstant, self.t.u32);
        let member = self
            .b
            .access_chain(ptr_pc_u32, None, pc, [zero])
            .expect("pc access");
        self.b
            .load(self.t.u32, None, member, None, [])
            .expect("load num_records")
    }

    fn ensure_num_records_pc(&mut self) -> spirv::Word {
        if let Some(v) = self.num_records_pc {
            return v;
        }
        // A single-uint push-constant block { uint num_records; } at offset 0.
        // Block-decorated; PushConstant is a portable, MoltenVK-safe storage class.
        //
        // CONTRACT: the host-pipeline provider MUST push this uint (the V#'s
        // num_records) into the pipeline's push-constant range at offset 0. A missing
        // or zero-initialized push constant degenerates the fetch clamp to index 0 —
        // every vertex reads element 0 — which neither spirv-val nor the CPU oracle
        // (it reads num_records from the V#, not a push constant) can catch. The
        // explicit layout is exported via IoLayout::push_constants so the provider
        // wires the exact byte range instead of relying on convention; when a second
        // field (e.g. base vertex) is added it appends at the next offset.
        const NR_OFFSET: u32 = 0;
        const NR_SIZE: u32 = 4;
        let block = self.b.type_struct([self.t.u32]);
        self.b.decorate(block, spirv::Decoration::Block, []);
        self.b.member_decorate(
            block,
            0,
            spirv::Decoration::Offset,
            [DrOperand::LiteralBit32(NR_OFFSET)],
        );
        let ptr_pc = self
            .b
            .type_pointer(None, spirv::StorageClass::PushConstant, block);
        let var = self.global_variable(ptr_pc, spirv::StorageClass::PushConstant);
        // Not an Input/Output — excluded from the SPIR-V ≤1.3 entry-point interface.
        self.io_push_constants.push(PushConstantField {
            offset_bytes: NR_OFFSET,
            size_bytes: NR_SIZE,
            role: PushConstantRole::NumRecords,
        });
        self.num_records_pc = Some(var);
        var
    }

    fn ensure_vs_buffer(&mut self, components: u8) -> VsBuffer {
        if let Some(b) = &self.vs_buffer {
            let out = VsBuffer {
                var: b.var,
                ptr_member: b.ptr_member,
                stride_bytes: b.stride_bytes,
            };
            // Record the MAX fetch width across all MUBUF fetches: a later fetch reading
            // more components than the first must not leave the binding under-reported
            // (the provider sizes the descriptor's element from this count).
            if let Some(binding) = self.io_buffers.first_mut()
                && (components as u32) > binding.components
            {
                binding.components = components as u32;
            }
            return out;
        }
        // struct VertexBuffer { vec4 data[]; } as a StorageBuffer. The runtime array
        // of vec4 gives element = vertex, component = xyzw — the fetch layout the
        // provider maps the guest V# onto. StorageBuffer + std430 is portable.
        let rt_array = self.b.type_runtime_array(self.t.v4f32);
        // ArrayStride = VB_ELEMENT_STRIDE (one vec4). LIMITATION: fixed at 16 — the
        // recompiler resolves the V# symbolically and cannot see its real stride
        // here; a bound V# with a different stride must be rejected by the provider
        // (see VB_ELEMENT_STRIDE). The interp reads the true stride from the V#.
        self.b.decorate(
            rt_array,
            spirv::Decoration::ArrayStride,
            [DrOperand::LiteralBit32(VB_ELEMENT_STRIDE)],
        );
        let block = self.b.type_struct([rt_array]);
        self.b.decorate(block, spirv::Decoration::Block, []);
        self.b.member_decorate(
            block,
            0,
            spirv::Decoration::Offset,
            [DrOperand::LiteralBit32(0)],
        );
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
            [DrOperand::LiteralBit32(VS_BUFFER_BINDING)],
        );
        // Not an Input/Output — excluded from the SPIR-V ≤1.3 entry-point interface.
        // Pointer to a vec4 member (StorageBuffer class) for the access chain load.
        let ptr_member = self
            .b
            .type_pointer(None, spirv::StorageClass::StorageBuffer, self.t.f32);
        let stride_bytes = VB_ELEMENT_STRIDE;
        self.vs_buffer = Some(VsBuffer {
            var,
            ptr_member,
            stride_bytes,
        });
        self.io_buffers.push(BufferBinding {
            set: VS_BUFFER_SET,
            binding: VS_BUFFER_BINDING,
            stride_bytes,
            components: components as u32,
        });
        VsBuffer {
            var,
            ptr_member,
            stride_bytes,
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
                operand: other,
                offset: off,
                reason: "not a vector destination",
            }),
        }
    }
}
