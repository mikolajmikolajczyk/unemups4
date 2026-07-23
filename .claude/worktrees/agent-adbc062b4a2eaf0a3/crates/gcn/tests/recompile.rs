//! GCN → SPIR-V recompiler integration tests (doc-4 §1, phase 4).
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
    // provider wires the exact byte range, not by convention.
    assert_eq!(out.io.push_constants.len(), 1, "one push-constant field");
    let pc = out.io.push_constants[0];
    assert_eq!(pc.offset_bytes, 0);
    assert_eq!(pc.size_bytes, 4);
    assert_eq!(pc.role, ps4_gcn::PushConstantRole::NumRecords);
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
