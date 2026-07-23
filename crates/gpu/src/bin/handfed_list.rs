//! Hand-fed `BackendCmd` list generator for the generic pipeline path (doc-2 §4,
//! decision-7). Assembles the exact one-per-submit command list an
//! `AshBackend::run_command_list` consumes to build + bind a host pipeline from
//! recompiled/embedded SPIR-V and issue a draw — the list AC#3's live triangle run
//! feeds. Prints the list shape and validates each stage's SPIR-V module, doing NO
//! device work, so it compiles and runs anywhere (a real GPU render is the maintainer's
//! live Tier-B step, not this binary).
//!
//! Run:
//!   cargo run -p ps4-gpu --bin handfed_list --release
//!
//! The list here pairs the firmware-embedded fullscreen VS + R/G-export PS (both valid,
//! portable SPIR-V) so the generated list is self-contained. Swapping in a corpus
//! VS/PS (crates/gcn/tests/corpus, recompiled) is the maintainer's live triangle case;
//! the list SHAPE — `CreatePipeline{id, vs, ps, key, target}` → `BindPipeline{id}` →
//! `DrawAuto{n}` — is identical.

use std::process::ExitCode;

use ps4_core::gpu::{BackendCmd, ColorFormat, PipelineId, PipelineKey, TargetDesc, VertexLayout};
use ps4_gnm::shader::embedded::{
    EMBEDDED_PS_RG_EXPORT, EMBEDDED_VS_FULLSCREEN_QUAD, embedded_spirv,
};
use ps4_gnm::shader::source::Stage;

/// Reinterpret a committed `.spv` byte blob as SPIR-V words (little-endian, whole words
/// by construction — same reinterpret the embedded provider does).
fn words(bytes: &[u8]) -> Vec<u32> {
    bytes
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

fn main() -> ExitCode {
    let Some(vs_bytes) = embedded_spirv(Stage::Vertex, EMBEDDED_VS_FULLSCREEN_QUAD) else {
        eprintln!("handfed_list: embedded VS SPIR-V unavailable");
        return ExitCode::FAILURE;
    };
    let Some(ps_bytes) = embedded_spirv(Stage::Pixel, EMBEDDED_PS_RG_EXPORT) else {
        eprintln!("handfed_list: embedded PS SPIR-V unavailable");
        return ExitCode::FAILURE;
    };
    let vs_spirv: std::sync::Arc<[u32]> = words(vs_bytes).into();
    let ps_spirv: std::sync::Arc<[u32]> = words(ps_bytes).into();

    // A representative pipeline key/target for the videoout target. A corpus triangle
    // would carry a register-derived VertexLayout here; the embedded fullscreen draw
    // reads gl_VertexIndex (no vertex buffer), so the layout is None.
    let key = PipelineKey {
        vs_hash: 0x1111_1111,
        ps_hash: 0x2222_2222,
        vertex_layout: None::<VertexLayout>,
        color_format: ColorFormat::B8G8R8A8Unorm,
        ..Default::default()
    };
    let target = TargetDesc::default();
    let id = PipelineId(1);

    // The one-per-submit list: create (SPIR-V crosses once) → bind by id → draw.
    let list = [
        BackendCmd::CreatePipeline {
            id,
            vs_spirv: vs_spirv.clone(),
            ps_spirv: ps_spirv.clone(),
            key: Box::new(key),
            target,
            vertex_storage: Vec::new(),
            push_constants: None,
            textures: Vec::new(),
            const_storage: None,
            const_storage_fragment: None,
        },
        BackendCmd::BindPipeline { id },
        BackendCmd::DrawAuto { vertex_count: 3 },
    ];

    // Validate each SPIR-V module (magic word) — a hand-fed list must not ship garbage.
    const SPIRV_MAGIC: u32 = 0x0723_0203;
    assert_eq!(vs_spirv[0], SPIRV_MAGIC, "VS is not a SPIR-V module");
    assert_eq!(ps_spirv[0], SPIRV_MAGIC, "PS is not a SPIR-V module");

    println!(
        "hand-fed BackendCmd list ({} commands, one submit):",
        list.len()
    );
    for (i, cmd) in list.iter().enumerate() {
        match cmd {
            BackendCmd::CreatePipeline {
                id,
                vs_spirv,
                ps_spirv,
                ..
            } => println!(
                "  [{i}] CreatePipeline {{ id: {}, vs: {} words, ps: {} words }}",
                id.0,
                vs_spirv.len(),
                ps_spirv.len()
            ),
            BackendCmd::BindPipeline { id } => println!("  [{i}] BindPipeline {{ id: {} }}", id.0),
            BackendCmd::DrawAuto { vertex_count } => {
                println!("  [{i}] DrawAuto {{ vertex_count: {vertex_count} }}")
            }
            other => println!("  [{i}] {other:?}"),
        }
    }
    println!(
        "feed this list to AshBackend::run_command_list on the display thread to render \
         (maintainer live step)."
    );
    ExitCode::SUCCESS
}
