// Companion SPIR-V builders for the differential harness (included into
// `diff_harness.rs`). These assemble the tiny partner stages the recompiled shader
// is paired with, using rspirv (the same builder the recompiler uses). Only the
// portable `Shader` capability is declared — nothing MoltenVK/Metal rejects.

use rspirv::binary::Assemble;
use rspirv::dr::{Builder, Operand as DrOperand};
use rspirv::spirv;

/// A fragment shader: `Location=0` vec4 input → `Location=0` vec4 output. Pairs with
/// the recompiled VS so its `Location=0` param (which carries the `exp pos0` value)
/// is written straight into the color target. The input is decorated `Flat`, so every
/// fragment of a triangle carries the PROVOKING vertex's `exp pos0` verbatim — the
/// harness renders one triangle per vertex (that vertex provoking) and reads any
/// covered texel, getting the shader's exported value with no interpolation/pixel
/// quantization (task-91).
fn build_forward_fs() -> Vec<u32> {
    let mut b = Builder::new();
    b.set_version(1, 3);
    b.capability(spirv::Capability::Shader);
    b.memory_model(spirv::AddressingModel::Logical, spirv::MemoryModel::GLSL450);

    let void = b.type_void();
    let f32 = b.type_float(32, None);
    let v4f32 = b.type_vector(f32, 4);
    let fn_void = b.type_function(void, []);
    let ptr_in = b.type_pointer(None, spirv::StorageClass::Input, v4f32);
    let ptr_out = b.type_pointer(None, spirv::StorageClass::Output, v4f32);

    let in_var = b.variable(ptr_in, None, spirv::StorageClass::Input, None);
    let out_var = b.variable(ptr_out, None, spirv::StorageClass::Output, None);
    b.decorate(
        in_var,
        spirv::Decoration::Location,
        [DrOperand::LiteralBit32(0)],
    );
    // Flat: the fragment reads the provoking vertex's value verbatim, so a whole
    // triangle carries one vertex's exp pos0 with no interpolation (task-91).
    b.decorate(in_var, spirv::Decoration::Flat, []);
    b.decorate(
        out_var,
        spirv::Decoration::Location,
        [DrOperand::LiteralBit32(0)],
    );

    let main = b.id();
    b.begin_function(void, Some(main), spirv::FunctionControl::NONE, fn_void)
        .unwrap();
    b.begin_block(None).unwrap();
    let val = b.load(v4f32, None, in_var, None, []).unwrap();
    b.store(out_var, val, None, []).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();

    b.entry_point(
        spirv::ExecutionModel::Fragment,
        main,
        "main",
        [in_var, out_var],
    );
    b.execution_mode(main, spirv::ExecutionMode::OriginUpperLeft, []);
    b.module().assemble()
}

/// A vertex shader emitting a fullscreen triangle (`gl_VertexIndex`-driven, 3 verts)
/// and forwarding one constant vec4 per requested output `Location`, sourced from a
/// push-constant block `{ vec4 interp[N]; }`. Pairs with the recompiled PS: each PS
/// `Location=attr` input reads the oracle's interpolated plane value (contract (a)).
/// The values are flat (identical at all three triangle vertices), so every fragment
/// sees exactly the oracle's interpolant regardless of barycentric position.
fn build_fullscreen_vs(locations: &[u32]) -> Vec<u32> {
    let mut b = Builder::new();
    b.set_version(1, 3);
    b.capability(spirv::Capability::Shader);
    b.memory_model(spirv::AddressingModel::Logical, spirv::MemoryModel::GLSL450);

    let void = b.type_void();
    let f32 = b.type_float(32, None);
    let u32 = b.type_int(32, 0);
    let i32 = b.type_int(32, 1);
    let v4f32 = b.type_vector(f32, 4);
    let fn_void = b.type_function(void, []);

    // gl_VertexIndex (Input, i32) — its own builtin variable.
    let ptr_in_i32 = b.type_pointer(None, spirv::StorageClass::Input, i32);
    let vertex_index = b.variable(ptr_in_i32, None, spirv::StorageClass::Input, None);
    b.decorate(
        vertex_index,
        spirv::Decoration::BuiltIn,
        [DrOperand::BuiltIn(spirv::BuiltIn::VertexIndex)],
    );

    // gl_Position (Output, vec4) inside a gl_PerVertex block.
    let per_vertex = b.type_struct([v4f32]);
    b.decorate(per_vertex, spirv::Decoration::Block, []);
    b.member_decorate(
        per_vertex,
        0,
        spirv::Decoration::BuiltIn,
        [DrOperand::BuiltIn(spirv::BuiltIn::Position)],
    );
    let ptr_out_pv = b.type_pointer(None, spirv::StorageClass::Output, per_vertex);
    let pv_var = b.variable(ptr_out_pv, None, spirv::StorageClass::Output, None);

    // Push constant block { vec4 interp[N]; } (N = locations.len(), min 1).
    let n = locations.len().max(1) as u32;
    let n_const = b.constant_bit32(u32, n);
    let arr_ty = b.type_array(v4f32, n_const);
    b.decorate(
        arr_ty,
        spirv::Decoration::ArrayStride,
        [DrOperand::LiteralBit32(16)],
    );
    let pc_block = b.type_struct([arr_ty]);
    b.decorate(pc_block, spirv::Decoration::Block, []);
    b.member_decorate(
        pc_block,
        0,
        spirv::Decoration::Offset,
        [DrOperand::LiteralBit32(0)],
    );
    let ptr_pc_block = b.type_pointer(None, spirv::StorageClass::PushConstant, pc_block);
    let pc_var = b.variable(ptr_pc_block, None, spirv::StorageClass::PushConstant, None);
    let ptr_pc_v4 = b.type_pointer(None, spirv::StorageClass::PushConstant, v4f32);

    // Output vars for each forwarded Location.
    let ptr_out_v4 = b.type_pointer(None, spirv::StorageClass::Output, v4f32);
    let mut loc_vars = Vec::new();
    for &loc in locations {
        let v = b.variable(ptr_out_v4, None, spirv::StorageClass::Output, None);
        b.decorate(
            v,
            spirv::Decoration::Location,
            [DrOperand::LiteralBit32(loc)],
        );
        loc_vars.push(v);
    }

    // Constants for the fullscreen-triangle position lookup.
    let c0 = b.constant_bit32(f32, 0.0f32.to_bits());
    let c1 = b.constant_bit32(f32, 1.0f32.to_bits());
    let cn1 = b.constant_bit32(f32, (-1.0f32).to_bits());
    let c3 = b.constant_bit32(f32, 3.0f32.to_bits());
    let u0 = b.constant_bit32(u32, 0);

    // Big-triangle trick: pos.xy = (index==0)?(-1,-1) etc. We compute:
    //   x = (VertexIndex == 2) ? 3 : -1
    //   y = (VertexIndex == 1) ? 3 : -1
    // giving verts (-1,-1),(-1,3),(3,-1) — a triangle covering the whole [-1,1] clip.
    let cidx1 = b.constant_bit32(i32, 1);
    let cidx2 = b.constant_bit32(i32, 2);
    let bool_ty = b.type_bool();

    let main = b.id();
    b.begin_function(void, Some(main), spirv::FunctionControl::NONE, fn_void)
        .unwrap();
    b.begin_block(None).unwrap();

    let idx = b.load(i32, None, vertex_index, None, []).unwrap();
    let is2 = b.i_equal(bool_ty, None, idx, cidx2).unwrap();
    let is1 = b.i_equal(bool_ty, None, idx, cidx1).unwrap();
    let x = b.select(f32, None, is2, c3, cn1).unwrap();
    let y = b.select(f32, None, is1, c3, cn1).unwrap();
    let pos = b.composite_construct(v4f32, None, [x, y, c0, c1]).unwrap();
    let ptr_out_v4_pos = b.type_pointer(None, spirv::StorageClass::Output, v4f32);
    let ptr_pos = b.access_chain(ptr_out_v4_pos, None, pv_var, [u0]).unwrap();
    b.store(ptr_pos, pos, None, []).unwrap();

    for (i, &out_var) in loc_vars.iter().enumerate() {
        let member_idx = b.constant_bit32(u32, i as u32);
        let ptr = b
            .access_chain(ptr_pc_v4, None, pc_var, [u0, member_idx])
            .unwrap();
        let val = b.load(v4f32, None, ptr, None, []).unwrap();
        b.store(out_var, val, None, []).unwrap();
    }

    b.ret().unwrap();
    b.end_function().unwrap();

    let mut interface = vec![vertex_index, pv_var];
    interface.extend_from_slice(&loc_vars);
    b.entry_point(spirv::ExecutionModel::Vertex, main, "main", interface);
    b.module().assemble()
}
