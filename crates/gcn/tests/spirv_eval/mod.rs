//! A minimal, GPU-free CPU evaluator for the recompiler's SPIR-V — the missing
//! value-level half of the differential harness (task-122).
//!
//! # Why this exists
//!
//! `differential.rs` proves the interp oracle is correct against independent math,
//! and that the recompiler's declared *interface* (IoLayout) cannot drift from what
//! the oracle reads/writes. But it deliberately did NOT execute the recompiled
//! SPIR-V, so every "interp matches recompile bit-for-bit" claim shipped verified
//! only by the maintainer-run GPU `diff_harness`. A wrong operand order, a flipped
//! compare, or a rounding bug in an *emitted op* would pass CI green.
//!
//! This module closes that gap: inside `cargo test`, it re-executes each corpus
//! shader's recompiled SPIR-V on the CPU, once per live lane, and the caller
//! compares its exports against the interp oracle per-lane / per-channel.
//!
//! # Why rspirv (no new dependency)
//!
//! The recompiler emits a SINGLE straight-line basic block (no CFG / loops / phi —
//! see `recompile.rs`). That makes a walking interpreter tractable: we parse the
//! assembled words back with [`rspirv::dr::load_words`] — `rspirv` (0.13) is ALREADY
//! a dependency of this crate (the recompiler builds SPIR-V with it) — and walk the
//! one function's instructions in order, keeping an `id -> Value` map. Reusing the
//! present `rspirv` (and the present `half`) crate avoids pulling in a full CPU
//! SPIR-V executor crate, dodging license/dependency churn for what is a small,
//! closed op subset.
//!
//! # The register-file shape we're re-executing
//!
//! The recompiler models each GCN register slot as a pair of *function-local*
//! `OpVariable`s (a u32 view and an f32 view, kept in sync by a bitcast on every
//! store — see `recompile.rs::store_reg_bits`). So the SPIR-V is full of
//! `OpStore`/`OpLoad` against those Function-storage variables. We honor that with a
//! `var -> Value` memory map alongside the SSA `id -> Value` map: `OpVariable`
//! creates a slot, `OpStore` writes it, `OpLoad` reads it. Interface variables
//! (Input / PushConstant / SSBO / sampler / Output) are recognized by their
//! decorations and are the only external I/O: an `OpLoad` of an interface variable
//! is answered from the supplied [`Bindings`], and an `OpStore` to an Output
//! variable is captured as an export.
//!
//! # Numeric fidelity
//!
//! Every arithmetic op mirrors the interp oracle's exact f32 semantics (verified by
//! reading `interp.rs`): `fract = x - floor(x)` clamped to `<1`, `rcp = 1.0/x`,
//! unfused `mad` vs fused `fma`, `half::f16::from_f32` (round-to-nearest-even) for
//! the f16 pack, byte/255.0 texel normalization, `(x*TAU).sin()` for GCN sin. All
//! are bit-exact EXCEPT `sin`, which is compared by the caller within a documented
//! ULP budget (host `sinf` is not correctly rounded — see the compare in
//! `differential.rs`).

use std::collections::HashMap;

use rspirv::dr::Operand as DrOperand;
use rspirv::spirv::{BuiltIn, Decoration, Op, StorageClass};

use ps4_gcn::ExportTarget;

/// Push-constant block member indices (mirror `recompile.rs::PC_NUM_RECORDS_MEMBER` /
/// `PC_STRIDE_MEMBER` / `PC_DST_SEL_MEMBER`): member 0 = `num_records` (fetch clamp),
/// member 1 = the vertex element stride in bytes, member 2 = packed dst_sel. The
/// evaluator answers a load of member 1 with the draw's `vertex_stride_bytes` (task-140)
/// and member 2 with `dst_sel_packed` (task-155).
const PC_NUM_RECORDS_MEMBER: u32 = 0;
const PC_STRIDE_MEMBER: u32 = 1;
const PC_DST_SEL_MEMBER: u32 = 2;
/// Member 3 = packed vertex format (`dfmt` in `[7:0]`, `nfmt` in `[15:8]`; task-164). The
/// evaluator answers a load of it with `format_packed`.
const PC_FORMAT_MEMBER: u32 = 3;
/// Push-constant members per vertex stream (mirror `recompile.rs::PC_MEMBERS_PER_STREAM`);
/// a stream's group starts at member `4 * stream` (task-153, task-164).
const PC_MEMBERS_PER_STREAM: u32 = 4;

/// GLSL.std.450 ext-inst opcode numbers the recompiler emits (mirrors
/// `recompile.rs::glsl`). Only these appear in the corpus.
mod glsl {
    pub const FABS: u32 = 4;
    pub const FLOOR: u32 = 8;
    pub const FRACT: u32 = 10;
    pub const SIN: u32 = 13;
    pub const SQRT: u32 = 31;
    pub const FMIN: u32 = 37;
    pub const FMAX: u32 = 40;
    pub const FMA: u32 = 50;
    pub const PACK_HALF_2X16: u32 = 58;
    pub const UNPACK_HALF_2X16: u32 = 62;
}

/// A runtime value in the evaluator. SPIR-V is strongly typed, but every scalar the
/// recompiler produces is 32-bit; a `Value` stores the raw 32 bits plus a light type
/// tag so `OpBitcast` is a no-op tag change and compares/converts pick the right
/// interpretation. Vectors hold their component scalars.
#[derive(Clone, Debug)]
enum Value {
    /// A 32-bit scalar carrying raw bits. Every scalar in the emitted subset is
    /// 32-bit; the interpretation (f32 / u32 / i32 / bool) is always known from the
    /// consuming opcode, so no type tag is stored. `OpBitcast` is therefore a no-op.
    Scalar(u32),
    /// A vector of scalar values (vec2 / vec3 / vec4 of f32 or u32).
    Vector(Vec<Value>),
    /// A pointer to a variable slot (result of `OpVariable` / `OpAccessChain`). The
    /// `PtrTarget` payload names which supplied resource + indices an access chain
    /// resolved to, so the following `OpLoad` can be answered from `Bindings`.
    Pointer(PtrTarget),
    /// A combined image-sampler handle (result of `OpLoad` on the sampler variable).
    SampledImage,
}

/// What a pointer refers to. A plain function-register variable points at a slot in
/// the `vars` memory map; an interface access chain resolves to a specific supplied
/// resource element so the load can be answered externally.
#[derive(Clone, Debug)]
enum PtrTarget {
    /// A function-local register variable, addressed by its result id.
    Var(u32),
    /// `Bindings.vertex_streams[stream].dwords[dword]` (VS SSBO fetch, dword-addressed,
    /// task-153: one SSBO per distinct V# stream).
    VertexBufferDword(usize, u32),
    /// `Bindings.vertex_streams[stream].num_records` (this stream's PC group member 0).
    NumRecords(usize),
    /// `Bindings.vertex_streams[stream].stride_bytes` (member 1, task-140).
    Stride(usize),
    /// `Bindings.vertex_streams[stream].dst_sel_packed` (member 2, task-155).
    DstSel(usize),
    /// `Bindings.vertex_streams[stream].format_packed` (member 3, task-164).
    Format(usize),
    /// `Bindings.cbuffer[dword]` (const-buffer SSBO).
    CbufferDword(u32),
}

impl Value {
    fn scalar_bits(&self) -> u32 {
        match self {
            Value::Scalar(bits) => *bits,
            other => panic!("expected scalar, got {other:?}"),
        }
    }
    fn as_f32(&self) -> f32 {
        f32::from_bits(self.scalar_bits())
    }
    fn as_u32(&self) -> u32 {
        self.scalar_bits()
    }
    fn as_i32(&self) -> i32 {
        self.scalar_bits() as i32
    }
    fn as_bool(&self) -> bool {
        // OpConstantTrue/False and compare results store 1/0 in the low bit.
        self.scalar_bits() != 0
    }
    fn f32v(x: f32) -> Value {
        Value::Scalar(x.to_bits())
    }
    fn u32v(x: u32) -> Value {
        Value::Scalar(x)
    }
    fn boolv(b: bool) -> Value {
        Value::Scalar(b as u32)
    }
    fn components(&self) -> &[Value] {
        match self {
            Value::Vector(v) => v,
            other => panic!("expected vector, got {other:?}"),
        }
    }
}

/// The external inputs one invocation (= one lane) reads. The caller reconstructs
/// these from the SAME `(LaunchAbi, MockMem)` that drives the interp oracle, so both
/// sides see identical bytes.
pub struct Bindings {
    /// `gl_VertexIndex` for this lane (`first_vertex + lane`); `None` for a PS.
    pub vertex_index: Option<u32>,
    /// The vertex-fetch streams, one per distinct V# the VS fetches (task-153). Stream
    /// `s` holds the buffer at ITS V# base as flat dwords + that V#'s
    /// stride/num_records/dst_sel. The recompiled VS addresses stream `s` dword-wise —
    /// `(vertex_index * stride + byte_offset) / 4 + component` — so the evaluator answers
    /// each fetch from the flat dword the SPIR-V computed, exactly as the interp reads flat
    /// guest memory. Empty for a PS; a single entry for the single-stream corpus.
    pub vertex_streams: Vec<VertexStreamBinding>,
    /// PS interpolants, PRE-INTERPOLATED for this lane: `interpolants[location]` is a
    /// vec4 whose channel `c` = the oracle plane value `P0 + I·(P1-P0) + J·(P2-P0)`.
    /// The recompiled PS just `OpLoad`s the Location input and extracts a channel, so
    /// the host must supply the already-interpolated value (the VINTRP handshake).
    pub interpolants: HashMap<u32, [f32; 4]>,
    /// The point/bilinear sampler's texture (PS), if the shader samples one.
    pub texture: Option<Texture>,
    /// The const-buffer SSBO dwords (raw bits), if the shader loads one.
    pub cbuffer: Vec<u32>,
}

/// One vertex-fetch stream's external inputs (task-153): its buffer (flat dwords from the
/// V# base) plus the four push-constant values the draw pushes for this stream. A
/// single-stream VS supplies exactly one of these.
pub struct VertexStreamBinding {
    /// The buffer as flat raw dwords starting at THIS stream's V# base.
    pub dwords: Vec<u32>,
    /// The V#'s per-element stride in BYTES (push-constant member `4*stream+1`).
    pub stride_bytes: u32,
    /// The V#'s `num_records` fetch clamp (push-constant member `4*stream`).
    pub num_records: u32,
    /// The V#'s packed `dst_sel` (push-constant member `4*stream+2`).
    pub dst_sel_packed: u32,
    /// The V#'s packed format (`dfmt` in `[7:0]`, `nfmt` in `[15:8]`; push-constant member
    /// `4*stream+3`, task-164).
    pub format_packed: u32,
}

/// A linear R8G8B8A8 texture, reconstructed to match the interp oracle's `texel`.
pub struct Texture {
    pub width: u32,
    pub height: u32,
    /// Row-major RGBA bytes.
    pub rgba: Vec<u8>,
    pub bilinear: bool,
}

impl Texture {
    /// Fetch texel `(x, y)` with euclidean wrap, normalized `byte/255.0` — matches
    /// `interp.rs::texel` for linear tiling.
    fn texel(&self, x: i64, y: i64) -> [f32; 4] {
        let w = self.width as i64;
        let h = self.height as i64;
        let xx = x.rem_euclid(w) as usize;
        let yy = y.rem_euclid(h) as usize;
        let off = (yy * self.width as usize + xx) * 4;
        [
            self.rgba[off] as f32 / 255.0,
            self.rgba[off + 1] as f32 / 255.0,
            self.rgba[off + 2] as f32 / 255.0,
            self.rgba[off + 3] as f32 / 255.0,
        ]
    }

    /// Sample at normalized UV, mirroring `interp.rs`'s point / bilinear paths.
    fn sample(&self, u: f32, v: f32) -> [f32; 4] {
        let fx = u * self.width as f32;
        let fy = v * self.height as f32;
        if self.bilinear {
            // Texel centers at integer + 0.5 (GPU convention); 4-tap bilinear lerp.
            let x = fx - 0.5;
            let y = fy - 0.5;
            let x0 = x.floor();
            let y0 = y.floor();
            let tx = x - x0;
            let ty = y - y0;
            let (x0, y0) = (x0 as i64, y0 as i64);
            let c00 = self.texel(x0, y0);
            let c10 = self.texel(x0 + 1, y0);
            let c01 = self.texel(x0, y0 + 1);
            let c11 = self.texel(x0 + 1, y0 + 1);
            let mut out = [0.0f32; 4];
            for (c, o) in out.iter_mut().enumerate() {
                let top = c00[c] + (c10[c] - c00[c]) * tx;
                let bot = c01[c] + (c11[c] - c01[c]) * tx;
                *o = top + (bot - top) * ty;
            }
            out
        } else {
            // Point / nearest: floor of the texel coordinate.
            self.texel(fx.floor() as i64, fy.floor() as i64)
        }
    }
}

/// An export captured from an `OpStore` to an Output variable.
#[derive(Clone, Debug, PartialEq)]
pub struct EvalExport {
    pub target: ExportTarget,
    pub values: [f32; 4],
}

/// Decoded, role-tagged interface facts recovered from the module's decorations +
/// type table — computed once and reused across every lane's invocation.
struct ModuleInfo {
    /// `variable id -> role` for every Input/Output/PushConstant/UniformConstant/SSBO
    /// interface variable.
    var_roles: HashMap<u32, VarRole>,
    /// `id -> constant value` for OpConstant / OpConstantComposite / bool constants.
    constants: HashMap<u32, Value>,
    /// The imported GLSL.std.450 ext-inst-set id.
    glsl_ext: Option<u32>,
}

#[derive(Clone, Debug)]
enum VarRole {
    /// A plain function-local register (no interface decoration).
    Register,
    /// `gl_VertexIndex` builtin input.
    VertexIndex,
    /// A PS interpolant input at this Location.
    InterpInput(u32),
    /// A VS param / PS mrt output at this Location.
    OutputLocation(u32),
    /// The position builtin output.
    OutputPosition,
    /// The push-constant block (per-stream fetch groups).
    PushConstant,
    /// A vertex-buffer SSBO stream (task-153); the payload is the 0-based stream index
    /// recovered from the binding number (0→0, 3→1, 4→2, 5→3).
    VertexBuffer(usize),
    /// The const-buffer SSBO (Set 0, Binding 2).
    Cbuffer,
    /// The combined image-sampler (Set 0, Binding 1).
    Sampler,
}

/// Execute one shader's recompiled SPIR-V once per supplied `Bindings` (= per lane)
/// and return the exports captured for that lane. `spirv` is the assembled word
/// stream (`RecompiledShader::spirv`).
pub fn eval_lane(spirv: &[u32], bindings: &Bindings) -> Result<Vec<EvalExport>, String> {
    let module = rspirv::dr::load_words(spirv).map_err(|e| format!("load_words: {e}"))?;
    let info = ModuleInfo::scan(&module);
    let func = module.functions.first().ok_or("module has no function")?;

    // Multi-block: index blocks by their OpLabel result id so a branch terminator can
    // jump to a successor. The recompiler emits structured control flow
    // (OpSelectionMerge + OpBranchConditional for a forward `if`); we walk it with a
    // fetch-execute loop over blocks. The register load/store model means the `vars`
    // map survives jumps — no OpPhi needed. (task-129 first slice: forward-only `if`.)
    let mut block_at: HashMap<u32, &rspirv::dr::Block> = HashMap::new();
    for block in &func.blocks {
        let label = block
            .label
            .as_ref()
            .and_then(|l| l.result_id)
            .ok_or("block without a label")?;
        block_at.insert(label, block);
    }
    let entry_label = func
        .blocks
        .first()
        .and_then(|b| b.label.as_ref())
        .and_then(|l| l.result_id)
        .ok_or("function has no entry block")?;

    let mut ev = Evaluator {
        info: &info,
        bindings,
        ids: HashMap::new(),
        vars: HashMap::new(),
        exports: Vec::new(),
    };

    // Block-visit cap: an acyclic selection module visits each block once, but a
    // structured LOOP legitimately re-visits its header/body/continue once per
    // iteration (the loops slice). A bounded corpus loop has a tiny trip count (the
    // loop_accum_ps loop runs 4 iterations over ~5 blocks), so a generous ABSOLUTE cap
    // (`1024`) comfortably completes any real bounded loop while still turning a true
    // infinite cycle (a lowering bug) into a clean error instead of a hang. It is a
    // floor via `.max`, so a large straight-line module still gets `blocks*4`.
    let visit_cap = func.blocks.len().saturating_mul(4).max(1024);
    let mut visits = 0usize;
    let mut cur = entry_label;
    loop {
        visits += 1;
        if visits > visit_cap {
            return Err(format!(
                "block-visit cap ({visit_cap}) exceeded — possible cycle in the SPIR-V CFG"
            ));
        }
        let block = *block_at
            .get(&cur)
            .ok_or_else(|| format!("branch to undefined block %{cur}"))?;
        match ev.run_block(block)? {
            Flow::Return => break,
            Flow::Goto(next) => cur = next,
        }
    }
    Ok(ev.exports)
}

/// How a block's execution left it.
enum Flow {
    /// OpReturn — end the function.
    Return,
    /// OpBranch / OpBranchConditional resolved to this successor block label.
    Goto(u32),
}

struct Evaluator<'a> {
    info: &'a ModuleInfo,
    bindings: &'a Bindings,
    /// SSA results: `result_id -> Value`.
    ids: HashMap<u32, Value>,
    /// Function-register memory: `variable id -> stored Value`.
    vars: HashMap<u32, Value>,
    exports: Vec<EvalExport>,
}

impl ModuleInfo {
    /// The recompiled module emits NO spec constants: the vertex element stride is read
    /// from the push-constant block (member 1), which the evaluator answers with
    /// `Bindings.vertex_stride_bytes` at load time — see `exec_access_chain` /
    /// `load_ptr` (task-140).
    fn scan(module: &rspirv::dr::Module) -> ModuleInfo {
        let mut info = ModuleInfo {
            var_roles: HashMap::new(),
            constants: HashMap::new(),
            glsl_ext: None,
        };

        // GLSL.std.450 import id.
        for imp in &module.ext_inst_imports {
            if let Some(DrOperand::LiteralString(s)) = imp.operands.first()
                && s == "GLSL.std.450"
            {
                info.glsl_ext = imp.result_id;
            }
        }

        // Pass 1: decorations. Collect per-id Location/Binding/BuiltIn so a variable
        // can be classified once its OpVariable is seen.
        let mut locations: HashMap<u32, u32> = HashMap::new();
        let mut bindings: HashMap<u32, u32> = HashMap::new();
        let mut builtins: HashMap<u32, BuiltIn> = HashMap::new();
        for ann in &module.annotations {
            if ann.class.opcode != Op::Decorate {
                continue;
            }
            let target = ann.operands[0].unwrap_id_ref();
            match ann.operands[1] {
                DrOperand::Decoration(Decoration::Location) => {
                    locations.insert(target, ann.operands[2].unwrap_literal_bit32());
                }
                DrOperand::Decoration(Decoration::Binding) => {
                    bindings.insert(target, ann.operands[2].unwrap_literal_bit32());
                }
                DrOperand::Decoration(Decoration::BuiltIn) => {
                    builtins.insert(target, ann.operands[2].unwrap_built_in());
                }
                _ => {}
            }
        }

        // Pass 2: types + constants (types_global_values, in dependency order). We
        // only need pointer types (to read a variable's storage class for
        // classification) and constant values; scalar/vector types carry no runtime
        // info because a `Value` is self-describing.
        let mut ptr_storage: HashMap<u32, StorageClass> = HashMap::new();
        for inst in &module.types_global_values {
            let rid = inst.result_id;
            match inst.class.opcode {
                Op::TypePointer => {
                    let storage = inst.operands[0].unwrap_storage_class();
                    ptr_storage.insert(rid.unwrap(), storage);
                }
                Op::Constant => {
                    // Raw bits are stored verbatim; the interpretation follows from the
                    // consuming opcode, so the constant's declared type is irrelevant here.
                    let bits = inst.operands[0].unwrap_literal_bit32();
                    info.constants.insert(rid.unwrap(), Value::Scalar(bits));
                }
                Op::SpecConstant => {
                    // The recompiler emits no spec constants any more (the vertex stride
                    // moved to a push constant, task-140); keep the DEFAULT-literal fallback
                    // so an unexpected spec constant at least evaluates rather than panics.
                    let bits = inst.operands[0].unwrap_literal_bit32();
                    info.constants.insert(rid.unwrap(), Value::Scalar(bits));
                }
                Op::ConstantTrue => {
                    info.constants.insert(rid.unwrap(), Value::boolv(true));
                }
                Op::ConstantFalse => {
                    info.constants.insert(rid.unwrap(), Value::boolv(false));
                }
                Op::ConstantComposite => {
                    let comps = inst
                        .operands
                        .iter()
                        .map(|o| {
                            let id = o.unwrap_id_ref();
                            info.constants.get(&id).cloned().unwrap_or(Value::u32v(0))
                        })
                        .collect();
                    info.constants.insert(rid.unwrap(), Value::Vector(comps));
                }
                Op::Variable => {
                    let rid = rid.unwrap();
                    let ptr_ty = inst.result_type.unwrap();
                    if let Some(storage) = ptr_storage.get(&ptr_ty).copied() {
                        let role = classify_var(rid, storage, &locations, &bindings, &builtins);
                        info.var_roles.insert(rid, role);
                    }
                }
                _ => {}
            }
        }

        // Function-local OpVariable declarations are always plain registers.
        for func in &module.functions {
            for block in &func.blocks {
                for inst in &block.instructions {
                    if inst.class.opcode == Op::Variable {
                        info.var_roles
                            .insert(inst.result_id.unwrap(), VarRole::Register);
                    }
                }
            }
        }

        info
    }
}

/// Inverse of `recompile::vs_stream_binding`: map an SSBO binding number back to its
/// 0-based vertex-stream index (task-153). Stream 0 → binding 0; streams 1.. → bindings
/// 3, 4, 5 (skipping the sampler at 1 and const buffer at 2). Returns `None` for a
/// binding that names no vertex stream.
fn vs_binding_to_stream(binding: u32) -> Option<usize> {
    match binding {
        0 => Some(0),
        3..=5 => Some(binding as usize - 2),
        _ => None,
    }
}

/// Classify a global interface variable from its storage class + decorations.
fn classify_var(
    id: u32,
    storage: StorageClass,
    locations: &HashMap<u32, u32>,
    bindings: &HashMap<u32, u32>,
    builtins: &HashMap<u32, BuiltIn>,
) -> VarRole {
    match storage {
        StorageClass::Input => match builtins.get(&id) {
            Some(BuiltIn::VertexIndex) => VarRole::VertexIndex,
            _ => VarRole::InterpInput(*locations.get(&id).unwrap_or(&0)),
        },
        StorageClass::Output => match builtins.get(&id) {
            Some(BuiltIn::Position) => VarRole::OutputPosition,
            _ => VarRole::OutputLocation(*locations.get(&id).unwrap_or(&0)),
        },
        StorageClass::PushConstant => VarRole::PushConstant,
        StorageClass::UniformConstant => VarRole::Sampler,
        // Both SSBO kinds are StorageBuffer; the binding number disambiguates. Binding 2 =
        // the VS const buffer, binding 6 = the PS const buffer (task-174 two distinct slots);
        // bindings 0/3/4/5 = vertex streams 0/1/2/3 (task-153); binding 1 is the sampler
        // (UniformConstant, handled above). Both const slots evaluate the same (a flat uint[]
        // indexed by dword offset), so both map to `Cbuffer`.
        StorageClass::StorageBuffer => match bindings.get(&id).copied() {
            Some(2) | Some(6) => VarRole::Cbuffer,
            Some(b) => match vs_binding_to_stream(b) {
                Some(stream) => VarRole::VertexBuffer(stream),
                None => VarRole::Register,
            },
            None => VarRole::Register,
        },
        _ => VarRole::Register,
    }
}

impl Evaluator<'_> {
    fn get(&self, id: u32) -> Value {
        if let Some(v) = self.ids.get(&id) {
            return v.clone();
        }
        if let Some(v) = self.info.constants.get(&id) {
            return v.clone();
        }
        panic!("undefined id %{id}");
    }

    fn set(&mut self, id: u32, v: Value) {
        self.ids.insert(id, v);
    }

    /// The bound vertex stream `s`, or an error if the shader read a stream the caller did
    /// not supply (task-153).
    fn stream(&self, s: usize) -> Result<&VertexStreamBinding, String> {
        self.bindings
            .vertex_streams
            .get(s)
            .ok_or_else(|| format!("vertex stream {s} not bound"))
    }

    /// Execute a whole block: run each non-terminator instruction, then resolve the
    /// block's terminator to a [`Flow`]. `OpSelectionMerge` / `OpLoopMerge` are
    /// structural hints (no runtime effect) and are skipped.
    fn run_block(&mut self, block: &rspirv::dr::Block) -> Result<Flow, String> {
        for inst in &block.instructions {
            match inst.class.opcode {
                // ---- terminators -----------------------------------------
                Op::Return => return Ok(Flow::Return),
                Op::Branch => {
                    let target = inst.operands[0].unwrap_id_ref();
                    return Ok(Flow::Goto(target));
                }
                Op::BranchConditional => {
                    let cond = self.get(inst.operands[0].unwrap_id_ref()).as_bool();
                    let t = inst.operands[1].unwrap_id_ref();
                    let f = inst.operands[2].unwrap_id_ref();
                    return Ok(Flow::Goto(if cond { t } else { f }));
                }
                // ---- structural merge hints (no runtime effect) ----------
                Op::SelectionMerge | Op::LoopMerge => {}
                // ---- everything else is a dataflow op --------------------
                _ => self.step(inst)?,
            }
        }
        // A well-formed SPIR-V block always ends in a terminator; reaching here means
        // the block had none (a malformed module).
        Err("block fell through with no terminator".into())
    }

    fn step(&mut self, inst: &rspirv::dr::Instruction) -> Result<(), String> {
        let rid = inst.result_id;
        let ops = &inst.operands;
        match inst.class.opcode {
            // ---- structural no-ops ---------------------------------------
            Op::Label | Op::Return | Op::Nop => {}

            // ---- variables & memory --------------------------------------
            Op::Variable => {
                // Function-local register slot: create a pointer value to itself.
                let id = rid.unwrap();
                self.set(id, Value::Pointer(PtrTarget::Var(id)));
            }
            Op::Load => self.exec_load(inst)?,
            Op::Store => self.exec_store(inst)?,
            Op::AccessChain => self.exec_access_chain(inst)?,

            // ---- float ALU -----------------------------------------------
            Op::FMul => self.bin_f32(inst, |a, b| a * b),
            Op::FAdd => self.bin_f32(inst, |a, b| a + b),
            Op::FSub => self.bin_f32(inst, |a, b| a - b),
            Op::FDiv => self.bin_f32(inst, |a, b| a / b),
            Op::FNegate => {
                let a = self.get(ops[0].unwrap_id_ref()).as_f32();
                self.set(rid.unwrap(), Value::f32v(-a));
            }
            Op::FOrdLessThan => self.cmp_f32(inst, |a, b| a < b),
            Op::FOrdGreaterThan => self.cmp_f32(inst, |a, b| a > b),
            Op::FOrdEqual => self.cmp_f32(inst, |a, b| a == b),
            Op::FOrdLessThanEqual => self.cmp_f32(inst, |a, b| a <= b),
            Op::FOrdGreaterThanEqual => self.cmp_f32(inst, |a, b| a >= b),

            // ---- int ALU -------------------------------------------------
            Op::IAdd => self.bin_u32(inst, |a, b| a.wrapping_add(b)),
            Op::ISub => self.bin_u32(inst, |a, b| a.wrapping_sub(b)),
            Op::IMul => self.bin_u32(inst, |a, b| a.wrapping_mul(b)),
            // Unsigned divide: the vertex-fetch dword address does `byte_off / 4`.
            // The divisor is a nonzero constant (4) in every emitted use.
            Op::UDiv => self.bin_u32(inst, |a, b| a / b),
            Op::IEqual => {
                let a = self.get(ops[0].unwrap_id_ref()).as_u32();
                let b = self.get(ops[1].unwrap_id_ref()).as_u32();
                self.set(rid.unwrap(), Value::boolv(a == b));
            }
            Op::ULessThan => {
                let a = self.get(ops[0].unwrap_id_ref()).as_u32();
                let b = self.get(ops[1].unwrap_id_ref()).as_u32();
                self.set(rid.unwrap(), Value::boolv(a < b));
            }
            Op::UGreaterThanEqual => {
                // The dst_sel apply emits `sel >= 4` to pick the source component
                // (task-155).
                let a = self.get(ops[0].unwrap_id_ref()).as_u32();
                let b = self.get(ops[1].unwrap_id_ref()).as_u32();
                self.set(rid.unwrap(), Value::boolv(a >= b));
            }
            Op::LogicalNot => {
                let a = self.get(ops[0].unwrap_id_ref()).as_bool();
                self.set(rid.unwrap(), Value::boolv(!a));
            }
            Op::LogicalOr => {
                // The format-aware fetch (task-164) OR-combines the per-dfmt equality tests
                // into the 8-bit / 16-bit family class booleans.
                let a = self.get(ops[0].unwrap_id_ref()).as_bool();
                let b = self.get(ops[1].unwrap_id_ref()).as_bool();
                self.set(rid.unwrap(), Value::boolv(a || b));
            }
            Op::BitwiseAnd => self.bin_u32(inst, |a, b| a & b),
            // Shift: SPIR-V operand order is (Base, Shift).
            Op::ShiftLeftLogical => self.bin_u32(inst, |a, b| a.wrapping_shl(b)),
            Op::ShiftRightLogical => self.bin_u32(inst, |a, b| a.wrapping_shr(b)),

            // ---- conversions & bitcast -----------------------------------
            Op::ConvertUToF => {
                let a = self.get(ops[0].unwrap_id_ref()).as_u32();
                self.set(rid.unwrap(), Value::f32v(a as f32));
            }
            Op::ConvertSToF => {
                let a = self.get(ops[0].unwrap_id_ref()).as_i32();
                self.set(rid.unwrap(), Value::f32v(a as f32));
            }
            Op::ConvertFToU => {
                let a = self.get(ops[0].unwrap_id_ref()).as_f32();
                self.set(rid.unwrap(), Value::u32v(a as u32));
            }
            Op::ConvertFToS => {
                let a = self.get(ops[0].unwrap_id_ref()).as_f32();
                self.set(rid.unwrap(), Value::u32v((a as i32) as u32));
            }
            Op::Bitcast => {
                // Raw bits are preserved verbatim; a bitcast between f32/u32/i32 is a
                // no-op on the stored bits (the interpretation follows from the
                // consuming opcode).
                let bits = self.get(ops[0].unwrap_id_ref()).scalar_bits();
                self.set(rid.unwrap(), Value::Scalar(bits));
            }

            // ---- composites ----------------------------------------------
            Op::CompositeConstruct => {
                let comps = ops.iter().map(|o| self.get(o.unwrap_id_ref())).collect();
                self.set(rid.unwrap(), Value::Vector(comps));
            }
            Op::CompositeExtract => {
                let vec = self.get(ops[0].unwrap_id_ref());
                let idx = ops[1].unwrap_literal_bit32() as usize;
                self.set(rid.unwrap(), vec.components()[idx].clone());
            }
            Op::VectorShuffle => {
                let a = self.get(ops[0].unwrap_id_ref());
                let b = self.get(ops[1].unwrap_id_ref());
                let av = a.components();
                let bv = b.components();
                let mut out = Vec::new();
                for sel in &ops[2..] {
                    let s = sel.unwrap_literal_bit32() as usize;
                    if s < av.len() {
                        out.push(av[s].clone());
                    } else {
                        out.push(bv[s - av.len()].clone());
                    }
                }
                self.set(rid.unwrap(), Value::Vector(out));
            }

            // ---- select --------------------------------------------------
            Op::Select => {
                let cond = self.get(ops[0].unwrap_id_ref()).as_bool();
                let t = self.get(ops[1].unwrap_id_ref());
                let f = self.get(ops[2].unwrap_id_ref());
                self.set(rid.unwrap(), if cond { t } else { f });
            }

            // ---- ext inst (GLSL.std.450) ---------------------------------
            Op::ExtInst => self.exec_ext_inst(inst)?,

            // ---- image ---------------------------------------------------
            Op::SampledImage => {
                // The recompiler loads a combined image-sampler directly; if a plain
                // OpSampledImage appears, treat it as the sampled-image handle.
                self.set(rid.unwrap(), Value::SampledImage);
            }
            Op::ImageSampleImplicitLod | Op::ImageSampleExplicitLod => {
                self.exec_image_sample(inst)?;
            }

            other => return Err(format!("unhandled opcode {other:?}")),
        }
        Ok(())
    }

    fn bin_f32(&mut self, inst: &rspirv::dr::Instruction, f: impl Fn(f32, f32) -> f32) {
        let a = self.get(inst.operands[0].unwrap_id_ref()).as_f32();
        let b = self.get(inst.operands[1].unwrap_id_ref()).as_f32();
        self.set(inst.result_id.unwrap(), Value::f32v(f(a, b)));
    }

    fn bin_u32(&mut self, inst: &rspirv::dr::Instruction, f: impl Fn(u32, u32) -> u32) {
        let a = self.get(inst.operands[0].unwrap_id_ref()).as_u32();
        let b = self.get(inst.operands[1].unwrap_id_ref()).as_u32();
        self.set(inst.result_id.unwrap(), Value::u32v(f(a, b)));
    }

    fn cmp_f32(&mut self, inst: &rspirv::dr::Instruction, f: impl Fn(f32, f32) -> bool) {
        let a = self.get(inst.operands[0].unwrap_id_ref()).as_f32();
        let b = self.get(inst.operands[1].unwrap_id_ref()).as_f32();
        self.set(inst.result_id.unwrap(), Value::boolv(f(a, b)));
    }

    fn exec_load(&mut self, inst: &rspirv::dr::Instruction) -> Result<(), String> {
        let rid = inst.result_id.unwrap();
        let ptr_id = inst.operands[0].unwrap_id_ref();
        // The pointer operand may be an id produced by OpAccessChain (a materialized
        // `Value::Pointer` in the SSA map), or a bare variable id — a function-local
        // register or a GLOBAL interface variable. Global interface variables are
        // never inserted into `ids`, so resolve them by their id directly.
        let target = match self.ids.get(&ptr_id) {
            Some(Value::Pointer(t)) => t.clone(),
            _ => PtrTarget::Var(ptr_id),
        };
        let v = match target {
            PtrTarget::Var(vid) => self.load_var(vid)?,
            PtrTarget::VertexBufferDword(stream, dword) => {
                let s = self
                    .bindings
                    .vertex_streams
                    .get(stream)
                    .ok_or_else(|| format!("vertex stream {stream} not bound"))?;
                Value::u32v(
                    s.dwords
                        .get(dword as usize)
                        .copied()
                        .ok_or_else(|| format!("vertex stream {stream} dword {dword} OOB"))?,
                )
            }
            PtrTarget::NumRecords(stream) => Value::u32v(self.stream(stream)?.num_records),
            PtrTarget::Stride(stream) => Value::u32v(self.stream(stream)?.stride_bytes),
            PtrTarget::DstSel(stream) => Value::u32v(self.stream(stream)?.dst_sel_packed),
            PtrTarget::Format(stream) => Value::u32v(self.stream(stream)?.format_packed),
            PtrTarget::CbufferDword(d) => Value::u32v(
                self.bindings
                    .cbuffer
                    .get(d as usize)
                    .copied()
                    .ok_or_else(|| format!("cbuffer dword {d} OOB"))?,
            ),
        };
        self.set(rid, v);
        Ok(())
    }

    /// Load through a variable id, honoring its interface role.
    fn load_var(&mut self, vid: u32) -> Result<Value, String> {
        match self.info.var_roles.get(&vid) {
            Some(VarRole::VertexIndex) => {
                Ok(Value::u32v(self.bindings.vertex_index.ok_or(
                    "gl_VertexIndex read but no vertex_index supplied",
                )?))
            }
            Some(VarRole::InterpInput(loc)) => {
                let vec = self
                    .bindings
                    .interpolants
                    .get(loc)
                    .copied()
                    .ok_or_else(|| format!("no interpolant for Location {loc}"))?;
                Ok(Value::Vector(vec.iter().map(|c| Value::f32v(*c)).collect()))
            }
            Some(VarRole::Sampler) => Ok(Value::SampledImage),
            Some(VarRole::Register) | None => {
                // A function register slot: read raw bits from the memory map (a slot
                // never written reads as 0, matching a zero-initialized register).
                let bits = self.vars.get(&vid).map(|v| v.scalar_bits()).unwrap_or(0);
                Ok(Value::Scalar(bits))
            }
            other => Err(format!(
                "OpLoad of unsupported interface var role {other:?}"
            )),
        }
    }

    fn exec_store(&mut self, inst: &rspirv::dr::Instruction) -> Result<(), String> {
        let ptr_id = inst.operands[0].unwrap_id_ref();
        let val = self.get(inst.operands[1].unwrap_id_ref());
        match self.info.var_roles.get(&ptr_id) {
            Some(VarRole::OutputPosition) => {
                self.capture_export(ExportTarget::Pos(0), &val);
            }
            Some(VarRole::OutputLocation(loc)) => {
                // VS param<n> or PS mrt<n>. The differential harness's oracle treats
                // these symmetrically; the target class (Param vs Mrt) is chosen by
                // the caller-provided stage, but for value comparison the caller keys
                // on (lane, target) where target already distinguishes VS/PS via how
                // the oracle produced it. We stamp Param(loc); the caller maps this to
                // the oracle's target by location. See `differential.rs`.
                self.capture_export(ExportTarget::Param(*loc as u8), &val);
            }
            _ => {
                // A function register store: keep raw bits in the memory map. Both the
                // u32 and f32 views map to the same variable id in the recompiler, but
                // rspirv gives them distinct ids; we store under whichever id is used.
                self.vars.insert(ptr_id, val);
            }
        }
        Ok(())
    }

    fn capture_export(&mut self, target: ExportTarget, val: &Value) {
        let comps = val.components();
        let mut values = [0.0f32; 4];
        for (i, v) in values.iter_mut().enumerate() {
            *v = comps[i].as_f32();
        }
        self.exports.push(EvalExport { target, values });
    }

    fn exec_access_chain(&mut self, inst: &rspirv::dr::Instruction) -> Result<(), String> {
        let rid = inst.result_id.unwrap();
        let base = inst.operands[0].unwrap_id_ref();
        let idx_ops = &inst.operands[1..];
        // Resolve each index operand to a concrete u32 (they are OpConstant or a
        // computed id in the SSA map).
        let mut indices = Vec::new();
        for o in idx_ops {
            let id = o.unwrap_id_ref();
            indices.push(self.get(id).as_u32());
        }
        let target = match self.info.var_roles.get(&base) {
            Some(&VarRole::VertexBuffer(stream)) => {
                // Chain [0, dword]: member 0 (runtime array of uint) -> [dword]. The
                // dword index was computed as (vertex_index*stride+byte_offset)/4 + comp,
                // into THIS stream's buffer.
                PtrTarget::VertexBufferDword(stream, indices[1])
            }
            Some(VarRole::PushConstant) => {
                // Chain [member]: the push-constant block of per-stream 4-uint groups
                // (task-153, task-164). Member `m` selects stream `m/4` and role `m%4` (0 =
                // num_records fetch clamp, 1 = stride task-140, 2 = dst_sel task-155, 3 =
                // format task-164).
                let m = indices[0];
                let stream = (m / PC_MEMBERS_PER_STREAM) as usize;
                match m % PC_MEMBERS_PER_STREAM {
                    PC_NUM_RECORDS_MEMBER => PtrTarget::NumRecords(stream),
                    PC_STRIDE_MEMBER => PtrTarget::Stride(stream),
                    PC_DST_SEL_MEMBER => PtrTarget::DstSel(stream),
                    PC_FORMAT_MEMBER => PtrTarget::Format(stream),
                    _ => {
                        return Err(format!(
                            "OpAccessChain on push constant: unknown member {m}"
                        ));
                    }
                }
            }
            Some(VarRole::Cbuffer) => {
                // Chain [0, dword]: member 0 (runtime array) -> [dword].
                PtrTarget::CbufferDword(indices[1])
            }
            _ => {
                return Err(format!(
                    "OpAccessChain on base %{base} with unknown interface role"
                ));
            }
        };
        self.set(rid, Value::Pointer(target));
        Ok(())
    }

    fn exec_ext_inst(&mut self, inst: &rspirv::dr::Instruction) -> Result<(), String> {
        let rid = inst.result_id.unwrap();
        let set = inst.operands[0].unwrap_id_ref();
        if Some(set) != self.info.glsl_ext {
            return Err("ext inst from a non-GLSL.std.450 set".into());
        }
        let opcode = inst.operands[1].unwrap_literal_ext_inst_integer();
        // Argument ids follow.
        let arg = |n: usize| inst.operands[2 + n].unwrap_id_ref();
        let v = match opcode {
            glsl::FABS => Value::f32v(self.get(arg(0)).as_f32().abs()),
            glsl::FLOOR => Value::f32v(self.get(arg(0)).as_f32().floor()),
            glsl::FRACT => {
                // GLSL fract = x - floor(x); the oracle clamps a rounded-to-1.0 result
                // down to the largest f32 < 1.0 (see interp.rs::fract_f32).
                let x = self.get(arg(0)).as_f32();
                let f = x - x.floor();
                let f = if f >= 1.0 {
                    f32::from_bits(0x3f7f_ffff)
                } else {
                    f
                };
                Value::f32v(f)
            }
            glsl::SIN => {
                // GCN sin takes a normalized revolution; the recompiler pre-scales by
                // TAU before this GLSL sin (matching interp's `(x*TAU).sin()`). Here
                // the argument is already the pre-scaled radians, so plain f32::sin.
                // NOT bit-exact vs the oracle — the caller compares within a ULP
                // budget for sin-affected exports.
                Value::f32v(self.get(arg(0)).as_f32().sin())
            }
            glsl::SQRT => Value::f32v(self.get(arg(0)).as_f32().sqrt()),
            glsl::FMIN => {
                let a = self.get(arg(0)).as_f32();
                let b = self.get(arg(1)).as_f32();
                Value::f32v(a.min(b))
            }
            glsl::FMAX => {
                let a = self.get(arg(0)).as_f32();
                let b = self.get(arg(1)).as_f32();
                Value::f32v(a.max(b))
            }
            glsl::FMA => {
                // Fused (single rounding), matching interp's `mul_add` for V_FMA_F32.
                let a = self.get(arg(0)).as_f32();
                let b = self.get(arg(1)).as_f32();
                let c = self.get(arg(2)).as_f32();
                Value::f32v(a.mul_add(b, c))
            }
            glsl::PACK_HALF_2X16 => {
                // half::f16::from_f32 = round-to-nearest-even, matching the oracle.
                let vec = self.get(arg(0));
                let comps = vec.components();
                let lo = half::f16::from_f32(comps[0].as_f32()).to_bits();
                let hi = half::f16::from_f32(comps[1].as_f32()).to_bits();
                Value::u32v((u32::from(hi) << 16) | u32::from(lo))
            }
            glsl::UNPACK_HALF_2X16 => {
                let packed = self.get(arg(0)).as_u32();
                let lo = half::f16::from_bits(packed as u16).to_f32();
                let hi = half::f16::from_bits((packed >> 16) as u16).to_f32();
                Value::Vector(vec![Value::f32v(lo), Value::f32v(hi)])
            }
            other => return Err(format!("unhandled GLSL.std.450 ext inst {other}")),
        };
        self.set(rid, v);
        Ok(())
    }

    fn exec_image_sample(&mut self, inst: &rspirv::dr::Instruction) -> Result<(), String> {
        let rid = inst.result_id.unwrap();
        // operands: sampled_image, coord, [image operands...]
        let coord = self.get(inst.operands[1].unwrap_id_ref());
        let comps = coord.components();
        let u = comps[0].as_f32();
        let v = comps[1].as_f32();
        let tex = self
            .bindings
            .texture
            .as_ref()
            .ok_or("image sample but no texture supplied")?;
        let rgba = tex.sample(u, v);
        self.set(
            rid,
            Value::Vector(rgba.iter().map(|c| Value::f32v(*c)).collect()),
        );
        Ok(())
    }
}
