//! ps4-gcn — GCN ISA decode + disassembler + (later) CPU interpreter and
//! GCN→SPIR-V recompiler (doc-2 §1, phase 4).
//!
//! This layer is the serial heart of the shader-translation chain: a total,
//! never-panicking decoder from a `&[u32]` GCN machine-code stream into a typed
//! [`Inst`] with [`Operand`]s, plus a text disassembler for golden tests and
//! traces. It executes nothing and touches no GPU — the interpreter (oracle) and
//! the SPIR-V recompiler build on the [`Inst`]/[`Operand`] types here.
//!
//! Kept out of `ps4-gnm` so the large, late shader-translation body never
//! entangles the command processor; depends on `ps4-core` only — never
//! `ash`/`winit`/vulkan. `ps4-gnm` calls in behind the `ShaderProvider` trait.
//!
//! Decoder discipline mirrors `ps4_gnm::pm4`: an unrecognized encoding becomes
//! [`Inst::Unknown`] (env-gated trace via `UNEMUPS4_GCN_TRACE`), the walk
//! continues, and every instruction reports its length in dwords so multi-dword
//! forms (literals, VOP3's second dword) advance the PC correctly.

mod cfg;
mod decoder;
mod disasm;
mod fetch_call;
mod fetch_shader;
mod inst;
mod interp;
mod opcodes;
mod operand;
mod recompile;
mod uop;

pub use decoder::{TRACE_ENV, decode_all, decode_one, trace_enabled};
pub use disasm::{disasm, disasm_all};
pub use fetch_call::{
    FetchResolveError, has_fetch_call, resolve_fetch_call, resolve_fetch_call_from_code,
};
pub use fetch_shader::{FetchAttribute, FetchLayout as FetchShaderLayout, parse_fetch_shader};
pub use inst::{Decoded, ExportTarget, Inst};
pub use interp::{
    ExportRecord, InterpError, LaunchAbi, NUM_SGPRS, NUM_VGPRS, PixelLaunch, PsInputs, WAVE_SIZE,
    WaveState, run,
};
pub use operand::{Operand, SpecialReg};
pub use recompile::{
    BufferBinding, ConstBufferBinding, DST_SEL_IDENTITY, DescriptorSource, IoLayout, IoRole, IoVar,
    PS_INPUT_SLOTS, PsInputMap, PushConstantField, PushConstantRole, RecompileError,
    RecompiledShader, SamplerBinding, ShaderStage, recompile, recompile_with,
};
