//! GCN → SPIR-V recompiler integration tests (doc-2 §1, phase 4).
//!
//! Recompiles the committed GCN corpus to SPIR-V and enforces the acceptance
//! fences:
//!
//! - **AC #1** — every corpus shader recompiles to a module that passes
//!   `spirv-val --target-env vulkan1.1`. This target env is the module's committed
//!   portability floor (see the recompiler module doc): the VS SSBO uses the
//!   SPIR-V 1.3 `StorageBuffer` class, so Vulkan 1.1 is the minimum, and MoltenVK
//!   exposes Vulkan 1.1 + `VK_KHR_portability_subset` with the plain `Shader`
//!   capability inside the subset. Skips cleanly if `spirv-val` is absent (it is
//!   present in CI here).
//! - **AC #2** — golden `spirv-dis` disassembly snapshots, committed under
//!   `tests/recompile_golden/`, act as a regression fence.
//! - **AC #3** — modules declare only the portable `Shader` capability (asserted
//!   by parsing the assembled module and reading its `OpCapability` instructions
//!   via a real SPIR-V parser, not a hand-rolled word scan).
//!
//! AC #4 (a live GPU triangle render) is a maintainer step, unticked here.
//!
//! Each external-tool invocation writes its `.spv` to a process/shader-unique temp
//! path (see [`unique_spv_path`]) so parallel test runners never race on a shared
//! file.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use ps4_gcn::{ShaderStage, decode_all, recompile};

/// A per-process, per-call unique temp `.spv` path. Combines the process id, a
/// caller-supplied tag (the shader name), and a monotonic counter so concurrent
/// runners (cargo nextest / sharding) never write to the same file — the previous
/// fixed `temp_dir()/unemups4_*.spv` paths raced and corrupted each other's bytes.
fn unique_spv_path(tag: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    std::env::temp_dir().join(format!("unemups4_recompile_{tag}_{pid}_{n}.spv"))
}

fn corpus_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus")
}

fn golden_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/recompile_golden")
}

fn read_code_dwords(name: &str) -> Vec<u32> {
    let p = corpus_dir().join(format!("{name}.code.bin"));
    let bytes = std::fs::read(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()));
    bytes
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

/// (corpus base name, stage).
const CORPUS: &[(&str, ShaderStage)] = &[
    ("passthrough_vs", ShaderStage::Vertex),
    ("flat_color_ps", ShaderStage::Fragment),
    ("interp_color_ps", ShaderStage::Fragment),
];

fn recompile_corpus(name: &str, stage: ShaderStage) -> Vec<u32> {
    let code = read_code_dwords(name);
    let insts = decode_all(&code);
    let out = recompile(&insts, stage).unwrap_or_else(|e| panic!("recompile {name}: {e}"));
    out.spirv
}

// ---- AC #3: only the portable `Shader` capability --------------------------

/// Parse the assembled module and collect its declared capabilities. Uses rspirv's
/// SPIR-V parser (not a hand-rolled word scan) so the check can't be bypassed by a
/// malformed stream or an unexpected instruction layout — proving the module stays
/// inside the Vulkan portability subset MoltenVK/Metal accept (decision-3).
fn declared_capabilities(name: &str, words: &[u32]) -> Vec<spirv::Capability> {
    let module = rspirv::dr::load_words(words)
        .unwrap_or_else(|e| panic!("{name}: parse assembled SPIR-V failed: {e:?}"));
    module
        .capabilities
        .iter()
        .map(|inst| match inst.operands.first() {
            Some(rspirv::dr::Operand::Capability(c)) => *c,
            other => panic!("OpCapability without a capability operand: {other:?}"),
        })
        .collect()
}

#[test]
fn modules_declare_only_portable_shader_capability() {
    for (name, stage) in CORPUS {
        let spirv = recompile_corpus(name, *stage);
        let caps = declared_capabilities(name, &spirv);
        assert_eq!(
            caps,
            vec![spirv::Capability::Shader],
            "{name}: must declare exactly the portable `Shader` capability, got {caps:?}"
        );
    }
}

// ---- AC #2: every Function-storage register variable is zero-initialized ----

/// A recompiled register slot (VGPR/SGPR u32+f32 view, m0, …) is a Function-storage
/// `OpVariable`. A shader that reads a register never written in it must get a defined
/// 0, not an undefined value: an undefined Function read passes spirv-val but crashes
/// RADV's ACO compiler inside vkCreateGraphicsPipelines (task-134, doc-6 Entry 11). So
/// EVERY Function-storage variable the recompiler emits must carry an initializer.
#[test]
fn function_storage_variables_are_initialized() {
    for (name, stage) in CORPUS {
        let spirv = recompile_corpus(name, *stage);
        let module = rspirv::dr::load_words(&spirv)
            .unwrap_or_else(|e| panic!("{name}: parse assembled SPIR-V failed: {e:?}"));
        for func in &module.functions {
            for block in &func.blocks {
                for inst in &block.instructions {
                    if inst.class.opcode != spirv::Op::Variable {
                        continue;
                    }
                    // OpVariable operands: [StorageClass, Initializer?]. A Function var
                    // with an initializer has 2 operands; one without has only 1.
                    let is_function = matches!(
                        inst.operands.first(),
                        Some(rspirv::dr::Operand::StorageClass(
                            spirv::StorageClass::Function
                        ))
                    );
                    if is_function {
                        assert!(
                            inst.operands.len() >= 2,
                            "{name}: Function-storage OpVariable (result %{:?}) has no \
                             initializer — an unwritten register would read undefined and \
                             crash RADV-ACO (task-134)",
                            inst.result_id
                        );
                    }
                }
            }
        }
    }
}

// ---- AC #1: spirv-val (Vulkan 1.1 / portability) ---------------------------

/// Locate `spirv-val`, or `None` if unavailable (test skips cleanly).
fn spirv_val() -> Option<PathBuf> {
    for cand in ["/usr/bin/spirv-val", "spirv-val"] {
        if Command::new(cand).arg("--version").output().is_ok() {
            return Some(PathBuf::from(cand));
        }
    }
    None
}

fn words_to_bytes(words: &[u32]) -> Vec<u8> {
    let mut b = Vec::with_capacity(words.len() * 4);
    for w in words {
        b.extend_from_slice(&w.to_le_bytes());
    }
    b
}

#[test]
fn corpus_recompiles_and_passes_spirv_val() {
    let Some(val) = spirv_val() else {
        eprintln!("spirv-val not found; skipping AC #1 validation");
        return;
    };
    for (name, stage) in CORPUS {
        let spirv = recompile_corpus(name, *stage);
        let bytes = words_to_bytes(&spirv);
        let path = unique_spv_path(name);
        std::fs::write(&path, &bytes).expect("write spv");

        // --target-env vulkan1.1: the committed portability floor (StorageBuffer =
        // SPIR-V 1.3 / Vulkan 1.1); MoltenVK exposes Vulkan 1.1 + portability subset.
        let out = Command::new(&val)
            .arg("--target-env")
            .arg("vulkan1.1")
            .arg(&path)
            .output()
            .expect("run spirv-val");
        let ok = out.status.success();
        let _ = std::fs::remove_file(&path);
        assert!(
            ok,
            "{name}: spirv-val (vulkan1.1) failed:\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
    }
}

// ---- AC #2: golden spirv-dis snapshots -------------------------------------

fn spirv_dis() -> Option<PathBuf> {
    for cand in ["/usr/bin/spirv-dis", "spirv-dis"] {
        if Command::new(cand).arg("--version").output().is_ok() {
            return Some(PathBuf::from(cand));
        }
    }
    None
}

fn disassemble(dis: &Path, tag: &str, spirv: &[u32]) -> String {
    let bytes = words_to_bytes(spirv);
    let path = unique_spv_path(tag);
    std::fs::write(&path, &bytes).expect("write spv");
    let out = Command::new(dis)
        .arg("--no-color")
        .arg("--no-header")
        .arg(&path)
        .output()
        .expect("run spirv-dis");
    let _ = std::fs::remove_file(&path);
    assert!(out.status.success(), "spirv-dis failed");
    String::from_utf8(out.stdout).expect("utf8 disasm")
}

/// Regenerate the committed golden disassembly. Ignored by default (it writes into
/// the source tree); run explicitly after a deliberate recompiler change:
///
/// ```text
/// cargo test -p ps4-gcn --test recompile -- --ignored regen_golden_disasm
/// ```
#[test]
#[ignore = "writes committed golden disasm; run after a deliberate recompiler change"]
fn regen_golden_disasm() {
    let Some(dis) = spirv_dis() else {
        panic!("spirv-dis required to regenerate goldens");
    };
    std::fs::create_dir_all(golden_dir()).expect("mkdir golden");
    for (name, stage) in CORPUS {
        let spirv = recompile_corpus(name, *stage);
        let text = disassemble(&dis, name, &spirv);
        let out = golden_dir().join(format!("{name}.spvasm"));
        std::fs::write(&out, text).unwrap_or_else(|e| panic!("write {}: {e}", out.display()));
        eprintln!("wrote {}", out.display());
    }
}

#[test]
fn recompiled_disasm_matches_golden() {
    let Some(dis) = spirv_dis() else {
        eprintln!("spirv-dis not found; skipping AC #2 golden check");
        return;
    };
    for (name, stage) in CORPUS {
        let spirv = recompile_corpus(name, *stage);
        let text = disassemble(&dis, name, &spirv);
        let golden_path = golden_dir().join(format!("{name}.spvasm"));
        let golden = std::fs::read_to_string(&golden_path).unwrap_or_else(|e| {
            panic!(
                "read golden {} ({e}); run `--ignored regen_golden_disasm`",
                golden_path.display()
            )
        });
        assert_eq!(
            text.trim_end(),
            golden.trim_end(),
            "{name}: recompiled SPIR-V disasm drifted from the committed golden — \
             re-run `--ignored regen_golden_disasm` if intended"
        );
    }
}

// ---- I/O layout shape ------------------------------------------------------

#[test]
fn vs_io_layout_has_position_and_param_and_buffer() {
    let code = read_code_dwords("passthrough_vs");
    let insts = decode_all(&code);
    let out = recompile(&insts, ShaderStage::Vertex).expect("recompile vs");
    assert_eq!(out.io.stage, ShaderStage::Vertex);
    assert!(out.io.exports_position, "VS must export pos0");
    assert!(
        out.io.uses_num_records(),
        "VS fetch clamp uses num_records push constant"
    );
    // The push-constant layout is exported explicitly (offset/size/role) so the
    // provider wires the exact byte range, not by convention: member 0 = num_records
    // (fetch clamp, offset 0), member 1 = stride (vertex element stride, offset 4;
    // task-140 — was a spec constant), member 2 = dst_sel (destination swizzle, offset 8;
    // task-155), member 3 = format (packed dfmt/nfmt, offset 12; task-164).
    assert_eq!(out.io.push_constants.len(), 4, "four push-constant fields");
    let nr = out.io.push_constants[0];
    assert_eq!(nr.offset_bytes, 0);
    assert_eq!(nr.size_bytes, 4);
    assert_eq!(nr.role, ps4_gcn::PushConstantRole::NumRecords);
    let stride = out.io.push_constants[1];
    assert_eq!(stride.offset_bytes, 4);
    assert_eq!(stride.size_bytes, 4);
    assert_eq!(stride.role, ps4_gcn::PushConstantRole::Stride);
    let dst_sel = out.io.push_constants[2];
    assert_eq!(dst_sel.offset_bytes, 8);
    assert_eq!(dst_sel.size_bytes, 4);
    assert_eq!(dst_sel.role, ps4_gcn::PushConstantRole::DstSel);
    let format = out.io.push_constants[3];
    assert_eq!(format.offset_bytes, 12);
    assert_eq!(format.size_bytes, 4);
    assert_eq!(format.role, ps4_gcn::PushConstantRole::Format);
    assert_eq!(out.io.buffers.len(), 1, "one vertex-buffer binding");
    assert_eq!(out.io.buffers[0].components, 4, "xyzw fetch");
    // exactly one param0 output (Location 0).
    assert_eq!(out.io.outputs.len(), 1);
    assert_eq!(out.io.outputs[0].location, 0);
    assert!(out.io.inputs.is_empty(), "VS has no Location inputs");
}

#[test]
fn flat_ps_io_layout_has_single_mrt_output() {
    let code = read_code_dwords("flat_color_ps");
    let insts = decode_all(&code);
    let out = recompile(&insts, ShaderStage::Fragment).expect("recompile flat ps");
    assert_eq!(out.io.stage, ShaderStage::Fragment);
    assert!(!out.io.exports_position);
    assert!(out.io.buffers.is_empty(), "flat PS reads no buffer");
    assert_eq!(out.io.outputs.len(), 1, "single mrt0 output");
    assert_eq!(out.io.outputs[0].location, 0);
    assert!(out.io.inputs.is_empty(), "flat PS has no interpolants");
}

#[test]
fn cbuffer_ps_io_layout_declares_one_const_buffer() {
    let code = read_code_dwords("cbuffer_ps");
    let insts = decode_all(&code);
    let out = recompile(&insts, ShaderStage::Fragment).expect("recompile cbuffer ps");
    // The s_buffer_load surfaces as a constant-buffer binding, NOT a vertex-buffer
    // (io.buffers) binding — the drift guard keys io.buffers to the MUBUF fetch.
    assert!(out.io.buffers.is_empty(), "cbuffer PS does no MUBUF fetch");
    assert_eq!(out.io.const_buffers.len(), 1, "one constant-buffer binding");
    let cb = &out.io.const_buffers[0];
    assert_eq!(
        (cb.set, cb.binding),
        (0, 6),
        "set 0, binding 6 — the FRAGMENT-stage const slot, distinct from the VS const at \
         binding 2 (task-174 two-slot dual-CB)"
    );
    assert_eq!(cb.size_dwords, 4, "loads 4 dwords (s_buffer_load_dwordx4)");
    assert_eq!(out.io.outputs.len(), 1, "single mrt0 output");
}

// ---- PS input routing (SPI_PS_INPUT_CNTL_n.OFFSET) -------------------------

/// The `Location`s of the module's Input-class `OpVariable`s, read back with a real
/// SPIR-V parser rather than trusting the returned `IoLayout` — the decoration is what
/// the driver actually links against.
fn input_locations(words: &[u32]) -> Vec<u32> {
    use rspirv::dr::Operand as DrOperand;
    let module = rspirv::dr::load_words(words).expect("parse assembled module");
    let inputs: std::collections::HashSet<spirv::Word> = module
        .types_global_values
        .iter()
        .filter(|i| i.class.opcode == spirv::Op::Variable)
        .filter(|i| {
            matches!(
                i.operands.first(),
                Some(DrOperand::StorageClass(spirv::StorageClass::Input))
            )
        })
        .filter_map(|i| i.result_id)
        .collect();
    let mut locs: Vec<u32> = module
        .annotations
        .iter()
        .filter(|a| a.class.opcode == spirv::Op::Decorate)
        .filter_map(
            |a| match (a.operands.first(), a.operands.get(1), a.operands.get(2)) {
                (
                    Some(DrOperand::IdRef(id)),
                    Some(DrOperand::Decoration(spirv::Decoration::Location)),
                    Some(DrOperand::LiteralBit32(loc)),
                ) if inputs.contains(id) => Some(*loc),
                _ => None,
            },
        )
        .collect();
    locs.sort_unstable();
    locs
}

/// A PS attribute slot is NOT its own `Location`: `SPI_PS_INPUT_CNTL_n.OFFSET` names the
/// VS export parameter that feeds slot `n`. Celeste's bloom blur programs `OFFSET = 1` for
/// slot 0, so its `attr0` UV read must resolve to `Location = 1` — feeding it parameter 0
/// (the vertex colour) hands the sampler a constant UV and blanks the bloom target.
#[test]
fn ps_input_map_routes_attr0_to_the_programmed_offset() {
    use ps4_gcn::{PS_INPUT_SLOTS, PsInputMap, recompile_with};

    let code = read_code_dwords("interp_color_ps");
    let insts = decode_all(&code);

    let mut offsets = [0u8; PS_INPUT_SLOTS];
    for (n, o) in offsets.iter_mut().enumerate() {
        *o = n as u8;
    }
    offsets[0] = 1;
    let map = PsInputMap::from_offsets(offsets);
    assert_eq!(map.location_for(0), 1);

    let out =
        recompile_with(&insts, ShaderStage::Fragment, &map).expect("recompile interp ps routed");
    assert_eq!(out.io.inputs.len(), 1, "still one coalesced input");
    assert_eq!(
        out.io.inputs[0].location, 1,
        "attr0 under OFFSET=1 must read VS parameter 1"
    );
    assert_eq!(input_locations(&out.spirv), vec![1]);

    // The identity map is the historical behaviour, unchanged.
    let identity = recompile_with(&insts, ShaderStage::Fragment, &PsInputMap::default())
        .expect("recompile interp ps identity");
    assert_eq!(identity.io.inputs[0].location, 0);
    assert_eq!(input_locations(&identity.spirv), vec![0]);
}

/// Two attribute slots routed to the SAME parameter must share ONE Input variable: two
/// variables decorated with the same `Location` is invalid SPIR-V and `spirv-val` rejects
/// the module. Slots the guest never programs read `OFFSET = 0`, so this collision is the
/// common case, not a corner one.
#[test]
fn ps_input_map_aliased_slots_share_one_input_variable() {
    use ps4_gcn::{
        Decoded, ExportTarget, Inst, Operand, PS_INPUT_SLOTS, PsInputMap, recompile_with,
    };

    // Opcode literals (the `opcodes` module is crate-private): V_INTERP_P1_F32 = 0x00.
    const V_INTERP_P1_F32: u8 = 0x00;

    let interp = |vdst: u8, attr: u8, offset_dwords: u32| Decoded {
        inst: Inst::Vintrp {
            op: V_INTERP_P1_F32,
            vdst: Operand::Vgpr(vdst),
            vsrc: Operand::Vgpr(0),
            attr,
            chan: 0,
        },
        size_dwords: 1,
        offset_dwords,
    };
    let export = Decoded {
        inst: Inst::Exp {
            target: ExportTarget::Mrt(0),
            srcs: [
                Some(Operand::Vgpr(1)),
                Some(Operand::Vgpr(2)),
                Some(Operand::Vgpr(1)),
                Some(Operand::Vgpr(2)),
            ],
            done: true,
            compr: false,
            vm: true,
        },
        size_dwords: 2,
        offset_dwords: 2,
    };

    // Both slots programmed to parameter 0 — the shape an unwritten SPI_PS_INPUT_CNTL
    // register produces.
    let map = PsInputMap::from_offsets([0u8; PS_INPUT_SLOTS]);
    let out = recompile_with(
        &[interp(1, 0, 0), interp(2, 1, 1), export],
        ShaderStage::Fragment,
        &map,
    )
    .expect("recompile aliased ps");
    assert_eq!(
        out.io.inputs.len(),
        1,
        "attr0 and attr1 both routed to parameter 0 share one Location"
    );
    assert_eq!(out.io.inputs[0].location, 0);
    assert_eq!(input_locations(&out.spirv), vec![0]);

    let Some(val) = spirv_val() else {
        eprintln!("spirv-val not found; skipping validation");
        return;
    };
    let path = unique_spv_path("aliased_ps_input");
    std::fs::write(&path, words_to_bytes(&out.spirv)).expect("write spv");
    let res = Command::new(&val)
        .arg("--target-env")
        .arg("vulkan1.1")
        .arg(&path)
        .output()
        .expect("run spirv-val");
    let ok = res.status.success();
    let _ = std::fs::remove_file(&path);
    assert!(
        ok,
        "aliased PS inputs must not emit duplicate Location decorations:\n{}",
        String::from_utf8_lossy(&res.stderr),
    );
}

// ---- VOPC integer compares → predicate → cndmask (task-197) ----------------

/// A VOP3B-encoded integer compare (`v_cmp_eq_i32 s[0:1], 1, v6`) writes its lane mask
/// to an ARBITRARY SGPR pair; a later `v_cndmask_b32 v8, v10, v8, s0` reads that
/// predicate and must lower to OpSelect — the switch/case colour selector Celeste's
/// background PS uses. Before task-197 the VOP3-VOPC integer compare fell through to the
/// generic VOP3 path and was rejected as "not a vector destination", deferring the
/// whole shader. Assert the pattern now recompiles with NO RecompileError.
#[test]
fn vop3_int_compare_flows_into_cndmask() {
    use ps4_gcn::{Decoded, ExportTarget, Inst, Operand};

    // v_cmp_eq_i32 (VOP3B) = VOPC op 0x82 in the VOP3 range; v_cndmask_b32 (VOP3) = 0x100.
    const V_CMP_EQ_I32: u16 = 0x082;
    const V_CNDMASK_B32: u16 = 0x100;

    let cmp = |sdst: u8, imm: i64, off: u32| Decoded {
        inst: Inst::Vop3 {
            op: V_CMP_EQ_I32,
            vdst: Operand::Sgpr(sdst), // SGPR-PAIR predicate destination (VOP3B)
            src0: Operand::InlineInt(imm),
            src1: Operand::Vgpr(6),
            src2: Operand::Sgpr(sdst),
            abs: 0,
            neg: 0,
            omod: 0,
            clamp: false,
        },
        size_dwords: 2,
        offset_dwords: off,
    };
    // v_cndmask_b32 v8, v10, v8, s0 — select on the s[0:1] compare mask.
    let cndmask = Decoded {
        inst: Inst::Vop3 {
            op: V_CNDMASK_B32,
            vdst: Operand::Vgpr(8),
            src0: Operand::Vgpr(10),
            src1: Operand::Vgpr(8),
            src2: Operand::Sgpr(0),
            abs: 0,
            neg: 0,
            omod: 0,
            clamp: false,
        },
        size_dwords: 2,
        offset_dwords: 4,
    };
    let export = Decoded {
        inst: Inst::Exp {
            target: ExportTarget::Mrt(0),
            srcs: [
                Some(Operand::Vgpr(8)),
                Some(Operand::Vgpr(8)),
                Some(Operand::Vgpr(8)),
                Some(Operand::Vgpr(8)),
            ],
            done: true,
            compr: false,
            vm: true,
        },
        size_dwords: 2,
        offset_dwords: 6,
    };

    let insts = [cmp(0, 1, 0), cndmask, export];
    let out = recompile(&insts, ShaderStage::Fragment)
        .expect("VOP3B integer compare + cndmask must recompile (no RecompileError)");
    assert_eq!(out.io.stage, ShaderStage::Fragment);
    assert_eq!(out.io.outputs.len(), 1, "single mrt0 output");
}

/// The exact deferred switch/case colour PS dumped from Celeste
/// (`gpu-snapshots/shaders/ps-7220397693965fd8.sb`, addr 0x9afae5a00) must now recompile
/// clean — it carries both VOP3B (`vop3_0x82 s[0:1], 1, v6`) and standalone
/// (`vopc_0x82 vcc, 1, v6`) integer compares feeding v_cndmask selects. Skips if the
/// gitignored snapshot asset is not present in the working tree.
#[test]
fn deferred_celeste_switch_case_ps_recompiles() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../gpu-snapshots/shaders/ps-7220397693965fd8.sb");
    let Ok(bytes) = std::fs::read(&path) else {
        eprintln!("{} not present; skipping", path.display());
        return;
    };
    let code: Vec<u32> = bytes
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();
    let insts = decode_all(&code);
    let res = recompile(&insts, ShaderStage::Fragment);
    assert!(
        res.is_ok(),
        "deferred Celeste switch-case PS must recompile after task-197: {:?}",
        res.err()
    );
}

#[test]
fn interp_ps_io_layout_coalesces_attr0_into_one_input() {
    let code = read_code_dwords("interp_color_ps");
    let insts = decode_all(&code);
    let out = recompile(&insts, ShaderStage::Fragment).expect("recompile interp ps");
    assert_eq!(out.io.stage, ShaderStage::Fragment);
    // attr0.x/.y/.z → ONE coalesced Location input (Location 0, components = 3),
    // not one entry per channel — so the VS↔PS provider wiring matches one output
    // var to one input var.
    assert_eq!(out.io.inputs.len(), 1, "one coalesced attr0 input");
    assert_eq!(out.io.inputs[0].location, 0, "attr0");
    assert_eq!(out.io.inputs[0].components, 3, "attr0.xyz → 3 components");
    assert_eq!(out.io.outputs.len(), 1, "single mrt0 output");
}

// ---- descriptor provenance (task-130 slice 1) ------------------------------

/// The recompiler resolves each descriptor's SGPR provenance and records it on the
/// binding's `source` field (previously discarded). Slice 1 is additive — the
/// executor still ignores these fields — but the resolved value must be exact so
/// later slices can bind from the signature instead of Celeste-shaped constants.
#[test]
fn cbuffer16_vs_const_buffer_source_is_inline_vsharp_at_sbase() {
    use ps4_gcn::DescriptorSource;
    let code = read_code_dwords("cbuffer16_vs");
    let insts = decode_all(&code);
    let out = recompile(&insts, ShaderStage::Vertex).expect("recompile cbuffer16_vs");
    assert_eq!(out.io.const_buffers.len(), 1, "one constant-buffer binding");
    // `s_buffer_load_dwordx16 s[0:15], s[4:5], 0x0` — the V# is inline in the SBASE
    // quad s[4:...], so the source is InlineVSharp with sgpr = 4.
    assert_eq!(
        out.io.const_buffers[0].source,
        DescriptorSource::InlineVSharp { sgpr: 4 },
        "constant-buffer V# is inline at the s_buffer_load SBASE (s[4])"
    );
}

/// The vertex fetch's V# is NOT inline: an SMRD `s_load` fetches a descriptor-set
/// pointer pair into the SGPRs the MUBUF `srsrc` names, so the source is a
/// `SetPointer` carrying that SMRD's SBASE + byte offset.
#[test]
fn passthrough_vs_buffer_source_is_set_pointer_at_smrd_sbase() {
    use ps4_gcn::DescriptorSource;
    let code = read_code_dwords("passthrough_vs");
    let insts = decode_all(&code);
    let out = recompile(&insts, ShaderStage::Vertex).expect("recompile passthrough_vs");
    assert_eq!(out.io.buffers.len(), 1, "one vertex-buffer binding");
    // `s_load_dwordx4 s[0:3], s[2:3], 0x0` then `buffer_load ... s[0:3]` — the MUBUF
    // srsrc = s[0] was written by the SMRD whose SBASE is s[2] at dword offset 0.
    assert_eq!(
        out.io.buffers[0].source,
        DescriptorSource::SetPointer {
            sgpr: 2,
            desc_offset: 0,
        },
        "vertex V# is a SetPointer at the s_load SBASE (s[2], offset 0)"
    );
}

/// A MUBUF whose `srsrc` no SMRD wrote is not a fetched descriptor — the recompiler
/// defers cleanly (a structured `Unsupported`) rather than fabricating provenance.
#[test]
fn mubuf_srsrc_without_smrd_defers() {
    use ps4_gcn::RecompileError;
    // buffer_load_format_xyzw v[4:7], v0, s[0:3], 0 idxen — WITHOUT a preceding
    // s_load writing s[0:3]. Reuse passthrough's MUBUF encoding but drop the SMRD.
    let code = read_code_dwords("passthrough_vs");
    let insts = decode_all(&code);
    // Skip the leading `s_load_dwordx4` (the first decoded instruction) so srsrc s[0]
    // is unmapped when the MUBUF is reached.
    let without_smrd = &insts[1..];
    match recompile(without_smrd, ShaderStage::Vertex) {
        Err(RecompileError::Unsupported { reason, .. }) => {
            assert!(
                reason.contains("srsrc"),
                "expected an srsrc-provenance defer, got: {reason}"
            );
        }
        other => panic!("expected Unsupported defer for unmapped srsrc, got {other:?}"),
    }
}

/// A texturing PS whose T#/S# arrive inline in user SGPRs (no SMRD, the corpus shape)
/// records an `InlineVSharp` T# source and the `ssamp` SGPR as `s_offset`.
#[test]
fn texture_sample_ps_sampler_source_is_inline_with_s_offset() {
    use ps4_gcn::DescriptorSource;
    let code = read_code_dwords("texture_sample_ps");
    let insts = decode_all(&code);
    let out = recompile(&insts, ShaderStage::Fragment).expect("recompile texture_sample_ps");
    assert_eq!(
        out.io.samplers.len(),
        1,
        "one combined image-sampler binding"
    );
    // `image_sample v[4:7], v[2:3], s[0:7], s[8:11]` — T# inline at srsrc s[0], S# at
    // ssamp s[8]. No SMRD, so the T# source is InlineVSharp{sgpr:0}, s_offset = 8.
    assert_eq!(
        out.io.samplers[0].source,
        DescriptorSource::InlineVSharp { sgpr: 0 },
        "inline T# at srsrc s[0]"
    );
    assert_eq!(out.io.samplers[0].s_offset, 8, "S# at ssamp s[8]");
    // Regression guard for task-199: the FIRST (and here only) texture keeps set 0 /
    // binding 1, so a single-texture module is emitted exactly as it was before a PS
    // could declare several.
    assert_eq!(out.io.samplers[0].set, 0, "set 0");
    assert_eq!(out.io.samplers[0].binding, 1, "first texture at binding 1");
    assert_eq!(
        count_sampled_image_vars(&out.spirv),
        1,
        "one combined image-sampler variable in the module"
    );
}

// ---- multi-texture PS (task-199) -------------------------------------------

/// Count `OpVariable` instructions in the `UniformConstant` storage class — one per
/// combined image-sampler the module declares. Walks the SPIR-V word stream directly so
/// the assertion is about the MODULE that ships to the driver, not the recompiler's own
/// bookkeeping.
fn count_sampled_image_vars(spirv: &[u32]) -> usize {
    const OP_VARIABLE: u32 = 59;
    const STORAGE_CLASS_UNIFORM_CONSTANT: u32 = 0;
    let mut i = 5; // skip the 5-word module header
    let mut n = 0;
    while i < spirv.len() {
        let word_count = (spirv[i] >> 16) as usize;
        let opcode = spirv[i] & 0xFFFF;
        if word_count == 0 {
            break;
        }
        // OpVariable: result-type, result-id, storage-class[, initializer]
        if opcode == OP_VARIABLE
            && word_count >= 4
            && spirv[i + 3] == STORAGE_CLASS_UNIFORM_CONSTANT
        {
            n += 1;
        }
        i += word_count;
    }
    n
}

/// A PS that samples TWO DIFFERENT textures declares TWO combined image-samplers, each
/// carrying its OWN descriptor provenance — the bug behind Celeste's yellow sky
/// (task-199). `texture_two_sample_ps` mirrors the real shape: sample A reads a
/// MEMORY-resident T# fetched by `s_load_dwordx8` through a descriptor-set pointer, then
/// sample B reads a REGISTER-resident T# sitting inline in user SGPRs, and only sample
/// B's result is exported. Collapsing both onto one binding makes the export read the
/// wrong texture.
#[test]
fn texture_two_sample_ps_declares_two_distinct_sampler_bindings() {
    use ps4_gcn::DescriptorSource;
    let code = read_code_dwords("texture_two_sample_ps");
    let insts = decode_all(&code);
    let out = recompile(&insts, ShaderStage::Fragment).expect("recompile texture_two_sample_ps");

    assert_eq!(
        out.io.samplers.len(),
        2,
        "two distinct image_sample descriptor pairs -> two combined image-samplers"
    );

    // Sample A: `s_load_dwordx8 s[16:23], s[12:13], 0x0` then
    // `image_sample ..., s[16:23], s[24:27]` — the T# was FETCHED, so its provenance is a
    // SetPointer at the SMRD's SBASE (s[12], byte offset 0); the S# sits at ssamp s[24].
    assert_eq!(
        out.io.samplers[0].source,
        DescriptorSource::SetPointer {
            sgpr: 12,
            desc_offset: 0,
        },
        "texture 0 is the memory-resident T# fetched through the set pointer at s[12]"
    );
    assert_eq!(out.io.samplers[0].s_offset, 24, "S# A at ssamp s[24]");

    // Sample B: `image_sample ..., s[0:7], s[8:11]` with no SMRD writing s[0:7] — the T#
    // is inline in the user SGPRs the launch ABI preloaded.
    assert_eq!(
        out.io.samplers[1].source,
        DescriptorSource::InlineVSharp { sgpr: 0 },
        "texture 1 is the register-resident T# inline at srsrc s[0]"
    );
    assert_eq!(out.io.samplers[1].s_offset, 8, "S# B at ssamp s[8]");

    // Distinct set-0 bindings, allocated deterministically in first-sample order: the
    // first keeps binding 1 (so single-texture modules are unchanged), the second takes
    // the first extra slot.
    assert_eq!((out.io.samplers[0].set, out.io.samplers[0].binding), (0, 1));
    assert_eq!((out.io.samplers[1].set, out.io.samplers[1].binding), (0, 7));
    assert_ne!(
        out.io.samplers[0].binding, out.io.samplers[1].binding,
        "the two textures must not share a binding — that IS the bug"
    );

    // And the module really declares two separate combined image-samplers, not one
    // variable referenced twice.
    assert_eq!(
        count_sampled_image_vars(&out.spirv),
        2,
        "two combined image-sampler variables in the module"
    );
}

/// Sampling the SAME descriptor pair repeatedly declares ONE binding, not one per
/// instruction. This is what keeps a multi-tap blur/9-sample composite (all taps through
/// the same T#/S#) emitting exactly the module it emitted before task-199.
#[test]
fn repeat_sample_through_same_descriptor_reuses_one_binding() {
    let code = read_code_dwords("texture_sample_ps");
    // Duplicate the single `image_sample` (a 2-dword MIMG, dword0 = 0xf0800f00) so the
    // shader samples the same T#/S# twice.
    let mimg = code
        .iter()
        .position(|&w| w == 0xf080_0f00)
        .expect("corpus PS contains an image_sample");
    let mut doubled = code.clone();
    doubled.splice(mimg..mimg, [code[mimg], code[mimg + 1]].iter().copied());

    let insts = decode_all(&doubled);
    let out = recompile(&insts, ShaderStage::Fragment).expect("recompile doubled-sample PS");
    assert_eq!(
        out.io.samplers.len(),
        1,
        "two samples through the SAME T#/S# share one binding"
    );
    assert_eq!(out.io.samplers[0].binding, 1);
    assert_eq!(count_sampled_image_vars(&out.spirv), 1);
}

// ---- error path: unsupported instruction is structured, not a panic --------

#[test]
fn unknown_instruction_yields_structured_error() {
    use ps4_gcn::RecompileError;
    let insts = decode_all(&[0xFFFF_FFFF]);
    let err = recompile(&insts, ShaderStage::Fragment).expect_err("unknown must fail");
    assert!(
        matches!(err, RecompileError::UnsupportedInst { offset: 0, .. }),
        "got {err:?}"
    );
}

// ---- launch ABI: a VS that reads v0 directly gets gl_VertexIndex -----------

/// A VS that reads `v0` as a plain ALU source — no fetch shader, no `idxen` MUBUF —
/// must still see the launch vertex index. On GCN `v0` IS the vertex index at VS
/// entry; the recompiler models VGPRs as zero-initialized Function variables, so
/// without resolving the read to `gl_VertexIndex` the index reads 0 for every vertex
/// and an index-derived full-screen triangle collapses to a single point (task-184).
///
/// Driven by the `index_tri_vs` corpus shader, whose `v_and_b32 v1, 1, v0` is that
/// read.
///
/// The assertion is at the MODULE level, not the disassembly: the defect was
/// invisible in a casual read of the disassembly (a plausible-looking `OpBitwiseAnd`
/// on a local) and only visible in what the entry-point interface declared. So this
/// pins BOTH halves — `gl_VertexIndex` is decorated and listed in the interface, AND
/// the value that reaches the arithmetic is an `OpLoad` of that variable rather than
/// of a Function-storage register slot.
#[test]
fn vs_reading_v0_directly_binds_gl_vertex_index() {
    let spirv = recompile_corpus("index_tri_vs", ShaderStage::Vertex);
    let module = rspirv::dr::load_words(&spirv).expect("parse assembled SPIR-V");

    // The one variable decorated `BuiltIn VertexIndex`.
    let vidx = module
        .annotations
        .iter()
        .find(|inst| {
            inst.class.opcode == spirv::Op::Decorate
                && inst.operands.iter().any(|op| {
                    matches!(
                        op,
                        rspirv::dr::Operand::BuiltIn(spirv::BuiltIn::VertexIndex)
                    )
                })
        })
        .and_then(|inst| match inst.operands.first() {
            Some(rspirv::dr::Operand::IdRef(id)) => Some(*id),
            _ => None,
        })
        .expect("a VS reading v0 must declare a gl_VertexIndex builtin");

    // It must be in the entry point's interface, or the builtin is never delivered.
    let entry = module
        .entry_points
        .first()
        .expect("the module has an entry point");
    let in_interface = entry
        .operands
        .iter()
        .any(|op| matches!(op, rspirv::dr::Operand::IdRef(id) if *id == vidx));
    assert!(
        in_interface,
        "gl_VertexIndex (%{vidx}) is declared but missing from the OpEntryPoint \
         interface — the builtin would never be bound"
    );

    // And the arithmetic must consume a load of it: find the OpLoad of %vidx, then
    // assert an OpBitwiseAnd takes that load's result. Anything else means v0 still
    // resolves to a zero-initialized register slot.
    let body = &module.functions[0].blocks[0].instructions;
    let loaded: Vec<u32> = body
        .iter()
        .filter(|inst| {
            inst.class.opcode == spirv::Op::Load
                && matches!(inst.operands.first(), Some(rspirv::dr::Operand::IdRef(id)) if *id == vidx)
        })
        .filter_map(|inst| inst.result_id)
        .collect();
    assert!(
        !loaded.is_empty(),
        "gl_VertexIndex is declared but never loaded — v0 still reads its register slot"
    );
    let feeds_and = body.iter().any(|inst| {
        inst.class.opcode == spirv::Op::BitwiseAnd
            && inst
                .operands
                .iter()
                .any(|op| matches!(op, rspirv::dr::Operand::IdRef(id) if loaded.contains(id)))
    });
    assert!(
        feeds_and,
        "index_tri_vs's v_and_b32 on v0 does not consume gl_VertexIndex — the launch index is \
         not reaching the shader's arithmetic"
    );
}

/// EVERY read of the launch-index register must see the index — not just the first.
///
/// `vs_reading_v0_directly_binds_gl_vertex_index` above passes even when only the first
/// read resolves, because `index_tri_vs`'s second read of `v0` writes a different VGPR.
/// `index_tri_inplace_vs` is the real fill-shader shape: `v_and_b32 v0, -2, v0` reads its
/// own destination, and every ALU emitter untracks the destination BEFORE evaluating its
/// sources — so an unspilled untrack made that read return the zero initializer while the
/// builtin stayed correctly declared and bound. The module looked right and rendered
/// nothing (task-184).
///
/// The assertion targets the SECOND read specifically: the `OpBitwiseAnd` carrying the
/// `0xFFFFFFFE` mask (unique to `v_and_b32 v0, -2, v0`) must take a load of
/// `gl_VertexIndex` as its other operand. Pinning "some load of gl_VertexIndex feeds some
/// AND" — what the earlier test does — passes on the broken module.
#[test]
fn every_read_of_v0_resolves_not_only_the_first() {
    let spirv = recompile_corpus("index_tri_inplace_vs", ShaderStage::Vertex);
    let module = rspirv::dr::load_words(&spirv).expect("parse assembled SPIR-V");

    let vidx = module
        .annotations
        .iter()
        .find(|inst| {
            inst.class.opcode == spirv::Op::Decorate
                && inst.operands.iter().any(|op| {
                    matches!(
                        op,
                        rspirv::dr::Operand::BuiltIn(spirv::BuiltIn::VertexIndex)
                    )
                })
        })
        .and_then(|inst| match inst.operands.first() {
            Some(rspirv::dr::Operand::IdRef(id)) => Some(*id),
            _ => None,
        })
        .expect("a VS reading v0 must declare a gl_VertexIndex builtin");

    // The `-2` mask constant, which only the second v_and_b32 uses.
    let mask = module
        .types_global_values
        .iter()
        .find(|inst| {
            inst.class.opcode == spirv::Op::Constant
                && matches!(inst.operands.first(), Some(rspirv::dr::Operand::LiteralBit32(v)) if *v == 0xFFFF_FFFE)
        })
        .and_then(|inst| inst.result_id)
        .expect("the `v_and_b32 v0, -2, v0` mask must be materialized as a constant");

    // Walk the entry block in order, propagating "this value came from gl_VertexIndex"
    // through loads and stores. The index may reach the AND either directly (the read
    // resolved to the builtin) or via a register slot the recompiler SPILLED the builtin
    // into before untracking it — both are correct; reading a slot that was never stored
    // is the bug, because the slot's initializer is zero.
    let body = &module.functions[0].blocks[0].instructions;
    let mut from_index: std::collections::HashSet<u32> = std::collections::HashSet::new();
    // Function variable -> whether its current contents came from gl_VertexIndex.
    let mut slot_holds_index: std::collections::HashMap<u32, bool> =
        std::collections::HashMap::new();
    let id_of = |op: Option<&rspirv::dr::Operand>| match op {
        Some(rspirv::dr::Operand::IdRef(id)) => Some(*id),
        _ => None,
    };

    for inst in body {
        match inst.class.opcode {
            spirv::Op::Load => {
                let ptr = id_of(inst.operands.first());
                let tainted = ptr == Some(vidx)
                    || ptr.is_some_and(|p| *slot_holds_index.get(&p).unwrap_or(&false));
                if tainted && let Some(r) = inst.result_id {
                    from_index.insert(r);
                }
            }
            spirv::Op::Store => {
                if let (Some(ptr), Some(val)) =
                    (id_of(inst.operands.first()), id_of(inst.operands.get(1)))
                {
                    slot_holds_index.insert(ptr, from_index.contains(&val));
                }
            }
            spirv::Op::Bitcast => {
                // The u32/f32 views of a register slot round-trip through a bitcast.
                if let (Some(src), Some(r)) = (id_of(inst.operands.first()), inst.result_id)
                    && from_index.contains(&src)
                {
                    from_index.insert(r);
                }
            }
            spirv::Op::BitwiseAnd => {
                let uses_mask = inst
                    .operands
                    .iter()
                    .any(|op| matches!(op, rspirv::dr::Operand::IdRef(id) if *id == mask));
                if uses_mask {
                    let reads_index = inst.operands.iter().any(
                        |op| matches!(op, rspirv::dr::Operand::IdRef(id) if from_index.contains(id)),
                    );
                    assert!(
                        reads_index,
                        "`v_and_b32 v0, -2, v0` reads a value that did not come from \
                         gl_VertexIndex — the launch index resolves on the FIRST read only, \
                         so every vertex gets the same Y and the triangle is zero-area"
                    );
                    return;
                }
            }
            _ => {}
        }
    }
    panic!("the second v_and_b32 must be lowered to an OpBitwiseAnd with the -2 mask");
}

// ---- idxen tracker: an arithmetic write untracks the launch vertex index ----

/// A VS that clobbers the launch-index register (`v0`) with arithmetic and then does
/// an `idxen` fetch on it must NOT read `gl_VertexIndex` (the unmodified launch
/// index): the tracked-index status is cleared on the arithmetic write, so the fetch
/// falls to the deferred `Unsupported` path (it cannot faithfully read the modified
/// VGPR as a fetch index). This matches the interp, which would fetch by the actual
/// (modified) VGPR value — a value the recompiler cannot map to a builtin. The one
/// outcome that must never happen is silently fetching by the stale `gl_VertexIndex`.
#[test]
fn arithmetic_write_untracks_vertex_index_so_idxen_is_not_stale() {
    use ps4_gcn::{Decoded, Inst, Operand, RecompileError};

    // Opcode literals (the `opcodes` module is crate-private): V_ADD_F32 = 0x03,
    // BUFFER_LOAD_FORMAT_X = 0x00.
    const V_ADD_F32: u8 = 0x03;
    const BUFFER_LOAD_FORMAT_X: u8 = 0x00;

    // v_add_f32 v0, v0, v0  — an arithmetic write to the tracked launch-index reg.
    let add = Decoded {
        inst: Inst::Vop2 {
            op: V_ADD_F32,
            vdst: Operand::Vgpr(0),
            src0: Operand::Vgpr(0),
            vsrc1: Operand::Vgpr(0),
            k: None,
        },
        size_dwords: 1,
        offset_dwords: 0,
    };
    // buffer_load_format_x v1, v0, s[0:3], idxen  — idxen fetch on the now-clobbered v0.
    let fetch = Decoded {
        inst: Inst::Mubuf {
            op: BUFFER_LOAD_FORMAT_X,
            vdata: Operand::Vgpr(1),
            vaddr: Operand::Vgpr(0),
            srsrc: 0,
            soffset: Operand::InlineInt(0),
            offset: 0,
            idxen: true,
            offen: false,
        },
        size_dwords: 1,
        offset_dwords: 1,
    };

    let err = recompile(&[add, fetch], ShaderStage::Vertex).expect_err(
        "idxen on a clobbered launch-index reg must not silently fetch by gl_VertexIndex",
    );
    assert!(
        matches!(err, RecompileError::Unsupported { offset: 1, .. }),
        "expected deferred Unsupported for the idxen fetch on a clobbered reg, got {err:?}"
    );
}

// ---- native VOP2 float ALU + the two VOP1 ops the Celeste composite shaders use ----

/// Celeste's per-frame layer-composite / blit passes (the full-screen quads) emit float
/// ALU in the NATIVE VOP2 encoding plus two VOP1 ops the recompiler used to bail on:
/// `v_subrev_f32` (VOP2 0x05), `v_ceil_f32` (VOP1 0x22) and `v_cvt_off_f32_i4`
/// (VOP1 0x0E). Each previously returned `RecompileError::UnsupportedInst`, which
/// deferred the whole draw (`unsupported-gcn-shader`) so the layer never composited and
/// the scene rendered near-black (task-194). All three must now recompile to a module
/// rather than defer.
#[test]
fn native_vop2_subrev_and_vop1_ceil_cvt_off_recompile() {
    use ps4_gcn::{Decoded, Inst, Operand, RecompileError};

    // Opcode literals (the `opcodes` module is crate-private): VOP2 V_SUBREV_F32 = 0x05,
    // VOP1 V_CEIL_F32 = 0x22, VOP1 V_CVT_OFF_F32_I4 = 0x0E (all confirmed via llvm-mc
    // `-mcpu=bonaire -show-encoding`).
    const V_SUBREV_F32: u8 = 0x05;
    const V_CEIL_F32: u8 = 0x22;
    const V_CVT_OFF_F32_I4: u8 = 0x0E;

    let prog = vec![
        // v_subrev_f32 v0, v1, v2  — reverse subtract, D = v2 - v1.
        Decoded {
            inst: Inst::Vop2 {
                op: V_SUBREV_F32,
                vdst: Operand::Vgpr(0),
                src0: Operand::Vgpr(1),
                vsrc1: Operand::Vgpr(2),
                k: None,
            },
            size_dwords: 1,
            offset_dwords: 0,
        },
        // v_ceil_f32 v1, v0
        Decoded {
            inst: Inst::Vop1 {
                op: V_CEIL_F32,
                vdst: Operand::Vgpr(1),
                src0: Operand::Vgpr(0),
            },
            size_dwords: 1,
            offset_dwords: 1,
        },
        // v_cvt_off_f32_i4 v10, 1  — the exact operand shape the guest emits (src0 is the
        // inline integer 1, decoded as `InlineInt(1)`).
        Decoded {
            inst: Inst::Vop1 {
                op: V_CVT_OFF_F32_I4,
                vdst: Operand::Vgpr(10),
                src0: Operand::InlineInt(1),
            },
            size_dwords: 1,
            offset_dwords: 2,
        },
    ];

    match recompile(&prog, ShaderStage::Fragment) {
        Ok(_) => {}
        Err(RecompileError::UnsupportedInst { inst, offset }) => panic!(
            "op still unsupported at offset {offset}: {inst:?} — the composite draw would defer"
        ),
        Err(e) => panic!("unexpected recompile error: {e:?}"),
    }
}

/// An `InvalidOperand` deferral must NAME the containing instruction, not just the bare
/// operand — otherwise a snapshot's deferred entry (which surfaces this Display verbatim)
/// says "invalid operand … Sgpr(0) (not a vector destination)" and never reveals which
/// opcode failed. `emit_inst` fills `inst` at the dispatch boundary (task-196 Part B).
#[test]
fn invalid_operand_error_names_the_instruction() {
    use ps4_gcn::{Decoded, Inst, Operand, RecompileError};

    const V_ADD_F32: u8 = 0x03;

    // v_add_f32 s0, v0, v0 — a VOP2 whose destination is an SGPR (not a vector dest),
    // which `vgpr_dst` rejects as `InvalidOperand` deep inside `emit_vop2`.
    let bad = Decoded {
        inst: Inst::Vop2 {
            op: V_ADD_F32,
            vdst: Operand::Sgpr(0),
            src0: Operand::Vgpr(0),
            vsrc1: Operand::Vgpr(0),
            k: None,
        },
        size_dwords: 1,
        offset_dwords: 0,
    };

    let err = recompile(&[bad], ShaderStage::Fragment)
        .expect_err("a VOP2 write to an SGPR must fail to recompile");
    let msg = err.to_string();

    match &err {
        RecompileError::InvalidOperand { inst, reason, .. } => {
            assert_eq!(*reason, "not a vector destination");
            // The boundary fills the real instruction, never leaving the pending placeholder.
            assert!(
                matches!(**inst, Inst::Vop2 { .. }),
                "InvalidOperand must carry the containing Vop2, got {inst:?}"
            );
        }
        other => panic!("expected InvalidOperand, got {other:?}"),
    }

    // The Display (surfaced verbatim into a snapshot's deferred `instruction` field) now
    // names the opcode, not just the operand.
    assert!(
        msg.contains("Vop2"),
        "Display must name the instruction opcode, got: {msg}"
    );
    assert!(
        msg.contains("not a vector destination"),
        "Display must keep the operand reason, got: {msg}"
    );
}
