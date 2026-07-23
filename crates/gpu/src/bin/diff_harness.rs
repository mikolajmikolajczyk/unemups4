//! Differential harness — TIER (b), live GPU (doc-2 §1, decision-3, decision-6).
//!
//! The maintainer-run counterpart to the headless tier in
//! `crates/gcn/tests/differential.rs`. Per corpus shader it:
//!
//!   1. runs the CPU interpreter (the ORACLE) over synthetic launch inputs;
//!   2. executes the RECOMPILED SPIR-V on a real Vulkan device in a minimal
//!      OFFSCREEN pass, feeding the SAME inputs;
//!   3. reads the GPU outputs back and diffs them against the oracle within an
//!      epsilon, printing a per-lane divergence report;
//!   4. exits NONZERO on any divergence.
//!
//! It is NOT wired into CI or `cargo test` — it needs a Vulkan driver. Run it:
//!
//! ```text
//! LD_LIBRARY_PATH=/usr/lib cargo run -p ps4-gpu --bin diff_harness --release
//! ```
//!
//! Add `--shader <name>` to run a single corpus shader (default: all).
//!
//! ## The four handshake contracts (a false report otherwise)
//!
//! The GPU path MUST feed the recompiled shader the SAME inputs the oracle sees, or
//! the diff reports false divergence (or false agreement):
//!
//! (a) **PS interpolants**: each `Location` input is driven with the oracle's OWN
//!     screen-space-linear plane value `P0 + I·(P1−P0) + J·(P2−P0)` (the VINTRP
//!     handshake) — the recompiled PS emits no interpolation math and trusts the
//!     input already carries the final interpolant. Every `Location` var is a vec4.
//! (b) **VS draw**: a SEQUENTIAL non-indexed `vkCmdDraw` (whose `firstVertex` seeds
//!     `gl_VertexIndex`) matches the oracle's `first_vertex + lane` fetch index —
//!     NOT `vkCmdDrawIndexed`.
//! (c) **num_records**: pushed as the push constant per `IoLayout::push_constants`;
//!     a missing/zero push degenerates every fetch to element 0.
//! (d) **deferred**: a recompile that returns `RecompileError::Unsupported` is
//!     DEFERRED (skipped with a note), not a divergence — it is outside the subset.
//!
//! ## Offscreen execution model
//!
//! - **VS** (`passthrough_vs`): the recompiled vertex shader is paired with a
//!   trivial pass-through fragment shader that forwards the VS's `Location=0` param
//!   (which carries `exp pos0`) into an RGBA32F offscreen color target. The 3 corpus
//!   vertices are drawn as a non-indexed `POINT_LIST` (`firstVertex = 0`, so
//!   `gl_VertexIndex` = 0..3 matches the oracle's `first_vertex + lane`); each vertex
//!   is a single 1×1 fragment whose param equals that vertex's `exp pos0` verbatim —
//!   no cross-vertex interpolation, so the value read back is the shader's exact
//!   computed value, not the rasterized pixel position. One pixel is read per vertex.
//!   This is the VS value-readback path (3-vertex; N>3 would need instanced draws).
//! - **PS** (`flat_color_ps`, `interp_color_ps`): the recompiled fragment shader is
//!   paired with a trivial fullscreen-triangle vertex shader that forwards a
//!   per-attribute vec4 interpolant (fed the oracle's plane value per contract (a)).
//!   One fragment is rendered into a 1×1 RGBA32F target; its texel is the PS's
//!   `exp mrt0`.
//!
//! Both readbacks compare the RGBA32F texel(s) to the oracle's `ExportRecord`s.

use std::collections::BTreeMap;
use std::ffi::CString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use ash::vk;
use ps4_core::memory::{MemoryProtection, VirtualMemoryManager};
use ps4_gcn::{
    ExportRecord, ExportTarget, IoLayout, LaunchAbi, PixelLaunch, PsInputs, PushConstantRole,
    RecompileError, ShaderStage, WAVE_SIZE, decode_all, recompile, run,
};

/// Per-channel absolute tolerance for the GPU-vs-oracle compare. The corpus math is
/// exact-in-f32 (adds, muls, power-of-two omods), so agreement is bit-exact in
/// principle; a small epsilon absorbs any driver-side FMA contraction of `a*b+c`.
const EPSILON: f32 = 1e-5;

/// Upper bound on a single offscreen submit before it is treated as a GPU hang (5 s
/// in nanoseconds). Keeps a bad shader from wedging the whole corpus run.
const GPU_FENCE_TIMEOUT_NS: u64 = 5_000_000_000;

fn corpus_dir() -> PathBuf {
    // The corpus lives with the gcn crate; reference it directly (this binary is a
    // maintainer tool, so a workspace-relative path is acceptable).
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../gcn/tests/corpus")
        .canonicalize()
        .expect("locate corpus dir")
}

fn read_code_dwords(name: &str) -> Vec<u32> {
    let p = corpus_dir().join(format!("{name}.code.bin"));
    let bytes = std::fs::read(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()));
    bytes
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

fn stage_from_name(name: &str) -> ShaderStage {
    if name.ends_with("_vs") {
        ShaderStage::Vertex
    } else if name.ends_with("_ps") {
        ShaderStage::Fragment
    } else {
        panic!("corpus shader {name:?} has no _vs/_ps stage suffix")
    }
}

/// Enumerate the corpus (data-driven): every `.s` with an assembled `.code.bin`.
fn enumerate_corpus() -> Vec<(String, ShaderStage)> {
    let dir = corpus_dir();
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&dir).expect("read corpus dir") {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("s") {
            continue;
        }
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .expect("utf8 name")
            .to_string();
        if !dir.join(format!("{name}.code.bin")).exists() {
            continue;
        }
        out.push((name.clone(), stage_from_name(&name)));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

// ---- mock memory: the ONLY bytes the oracle may see ------------------------

struct MockMem {
    base: u64,
    data: Vec<u8>,
}
impl MockMem {
    fn new(base: u64, data: Vec<u8>) -> Self {
        MockMem { base, data }
    }
}
impl VirtualMemoryManager for MockMem {
    fn map(
        &mut self,
        _a: u64,
        _s: usize,
        _p: MemoryProtection,
        _n: Option<&str>,
    ) -> Result<u64, &'static str> {
        Err("unsupported")
    }
    fn unmap(&mut self, _a: u64, _s: usize) -> Result<(), &'static str> {
        Err("unsupported")
    }
    fn protect(&mut self, _a: u64, _s: usize, _p: MemoryProtection) -> Result<(), &'static str> {
        Err("unsupported")
    }
    unsafe fn get_host_ptr(&self, _a: u64) -> Option<*mut u8> {
        None
    }
    fn find_free_region(&mut self, _s: usize) -> u64 {
        0
    }
    fn is_memory_free(&self, _a: u64, _s: usize) -> bool {
        false
    }
    fn read_bytes(&self, addr: u64, size: usize) -> Result<Vec<u8>, &'static str> {
        let start = addr.checked_sub(self.base).ok_or("read below base")? as usize;
        let end = start.checked_add(size).ok_or("size overflow")?;
        if end > self.data.len() {
            return Err("read past end");
        }
        Ok(self.data[start..end].to_vec())
    }
}

// ---- synthetic launch inputs (shared oracle + GPU) -------------------------

/// The inputs a corpus shader is driven with. Both the oracle and the GPU path read
/// from THIS single description so the two sides cannot diverge on their inputs.
struct HarnessInput {
    /// VS: the vertex positions fetched from the V# (contract (b): the GPU draw uses
    /// firstVertex = 0, sequential, matching the oracle's `first_vertex + lane`).
    vs_positions: Vec<[f32; 4]>,
    /// PS: per-attribute plane triples `attr_planes[attr][chan] = [P0,P1,P2]`.
    ps_planes: Vec<[[f32; 3]; 4]>,
    /// PS: the single fragment's barycentrics (I, J) — one lane fragment.
    ps_bary: (f32, f32),
    /// PS: an optional sampled texture (a shader with `image_sample`). Both the oracle
    /// (CPU sample) and the GPU (combined image-sampler) read THIS single description,
    /// so the sampling differential cannot diverge on the texture it samples.
    ps_texture: Option<HarnessTexture>,
}

/// A linear R8G8B8A8 texture + the UV the single fragment samples it at (decision-3
/// sampling differential). `point` selects nearest vs bilinear on both sides.
#[derive(Clone)]
struct HarnessTexture {
    width: u32,
    height: u32,
    /// Row-major RGBA bytes (`width * height * 4`).
    texels: Vec<u8>,
    /// `true` = point/nearest filter, `false` = bilinear.
    point: bool,
}

/// The synthetic launch for each corpus shader. A shader absent here is skipped with
/// a note (the harness only runs shaders it knows how to feed).
fn harness_inputs(name: &str) -> Option<HarnessInput> {
    match name {
        "passthrough_vs" => Some(HarnessInput {
            vs_positions: vec![
                [0.0, 1.0, 0.0, 1.0],
                [-1.0, -1.0, 0.0, 1.0],
                [1.0, -1.0, 0.0, 1.0],
            ],
            ps_planes: Vec::new(),
            ps_bary: (0.0, 0.0),
            ps_texture: None,
        }),
        "flat_color_ps" => Some(HarnessInput {
            vs_positions: Vec::new(),
            // flat PS reads no interpolants; planes unused.
            ps_planes: Vec::new(),
            ps_bary: (0.0, 0.0),
            ps_texture: None,
        }),
        "interp_color_ps" => Some(HarnessInput {
            vs_positions: Vec::new(),
            ps_planes: vec![[
                [0.25, 0.75, 1.25], // R
                [1.0, 0.5, 0.0],    // G
                [0.0, 0.0, 2.0],    // B
                [0.0, 0.0, 0.0],    // unused
            ]],
            ps_bary: (0.5, 0.5),
            ps_texture: None,
        }),
        "texture_sample_ps" => Some(HarnessInput {
            vs_positions: Vec::new(),
            // The PS interpolates attr0.xy = the UV; drive it with a constant plane so the
            // fragment's UV is (0.75, 0.25) regardless of barycentrics (like the headless
            // spec). attr0.z/.w unused (the PS samples xy only).
            ps_planes: vec![[
                [0.75, 0.75, 0.75], // attr0.x = u
                [0.25, 0.25, 0.25], // attr0.y = v
                [0.0, 0.0, 0.0],
                [0.0, 0.0, 0.0],
            ]],
            ps_bary: (0.5, 0.5),
            ps_texture: Some(HarnessTexture {
                width: 2,
                height: 2,
                // 2×2 RGBA, row-major; (1,0) is the point-sampled target color.
                texels: vec![
                    0, 0, 0, 255, // (0,0)
                    102, 204, 51, 255, // (1,0) ← UV (0.75,0.25) point-samples here
                    10, 20, 30, 255, // (0,1)
                    200, 100, 150, 255, // (1,1)
                ],
                point: true,
            }),
        }),
        _ => None,
    }
}

fn build_vs_memory(positions: &[[f32; 4]]) -> (MockMem, u64) {
    const BASE: u64 = 0x1_0000;
    const VB_OFF: u64 = 64;
    let vb_addr = BASE + VB_OFF;
    let stride: u32 = 16;
    let mut data = Vec::new();
    let push_u32 = |d: &mut Vec<u8>, v: u32| d.extend_from_slice(&v.to_le_bytes());
    push_u32(&mut data, (vb_addr & 0xFFFF_FFFF) as u32);
    push_u32(
        &mut data,
        ((vb_addr >> 32) as u32 & 0xFFFF) | (stride << 16),
    );
    push_u32(&mut data, positions.len() as u32);
    push_u32(&mut data, 0);
    while data.len() < VB_OFF as usize {
        data.push(0);
    }
    for p in positions {
        for c in p {
            data.extend_from_slice(&c.to_bits().to_le_bytes());
        }
    }
    (MockMem::new(BASE, data), BASE)
}

/// Build the oracle's texture memory + T#/S# SGPRs for a sampling PS. The texels live
/// at a 256-aligned base; the T# (s0..s7) points at them and the S# (s8..s11) carries
/// the filter bit — the same descriptor layout `ps4_gcn`'s sampling oracle decodes.
fn build_texture_memory(t: &HarnessTexture) -> (Vec<u32>, MockMem) {
    const BASE: u64 = 0x2_0000; // 256-aligned (word0 = base>>8 round-trips)
    let mut tw = [0u32; 8];
    tw[0] = (BASE >> 8) as u32; // base[39:8]
    tw[1] = 10 << 20; // dfmt=8_8_8_8 (bits [23:20]); nfmt=UNORM=0
    tw[2] = (t.width - 1) | ((t.height - 1) << 14);
    tw[3] = 0; // linear
    let mut sw = [0u32; 4];
    if !t.point {
        sw[2] = 1 << 20; // bilinear filter select
    }
    let mut user = Vec::new();
    user.extend_from_slice(&tw);
    user.extend_from_slice(&sw);
    (user, MockMem::new(BASE, t.texels.clone()))
}

/// The oracle's plane value at (I,J): `P0 + I·(P1−P0) + J·(P2−P0)`. This is the
/// value the GPU PS's `Location=attr` input MUST be driven with (contract (a)).
fn plane_eval(p: [f32; 3], i: f32, j: f32) -> f32 {
    p[0] + i * (p[1] - p[0]) + j * (p[2] - p[0])
}

// ---- oracle side -----------------------------------------------------------

/// Run the CPU interpreter over the synthetic inputs. Returns the exports keyed for
/// the GPU compare: for a VS, one `pos0` per vertex; for a PS, the single-fragment
/// `mrt0`.
fn run_oracle(
    name: &str,
    stage: ShaderStage,
    input: &HarnessInput,
) -> Result<Vec<ExportRecord>, String> {
    let code = read_code_dwords(name);
    let insts = decode_all(&code);
    match stage {
        ShaderStage::Vertex => {
            let (mem, desc_addr) = build_vs_memory(&input.vs_positions);
            let abi = LaunchAbi::Vertex {
                user_sgprs: vec![0, 0, desc_addr as u32, (desc_addr >> 32) as u32],
                first_vertex: 0,
                num_lanes: input.vs_positions.len(),
            };
            run(&insts, abi, &mem).map_err(|e| format!("VS oracle run: {e}"))
        }
        ShaderStage::Fragment => {
            let mut bary_i = [0.0f32; WAVE_SIZE];
            let mut bary_j = [0.0f32; WAVE_SIZE];
            bary_i[0] = input.ps_bary.0;
            bary_j[0] = input.ps_bary.1;
            let inputs = PsInputs {
                attr_planes: input.ps_planes.clone(),
            };
            // A sampling PS: build the T#/S# SGPRs (s0..s7 = T#, s8..s11 = S#) and a mock
            // memory holding the texels at the T# base, so the oracle samples the SAME
            // texture the GPU side binds.
            let (user_sgprs, mem) = match &input.ps_texture {
                Some(t) => build_texture_memory(t),
                None => (vec![], MockMem::new(0, Vec::new())),
            };
            let abi = LaunchAbi::Pixel(Box::new(PixelLaunch {
                user_sgprs,
                inputs,
                bary_i,
                bary_j,
                exec: 0b1,
            }));
            run(&insts, abi, &mem).map_err(|e| format!("PS oracle run: {e}"))
        }
    }
}

// ---- entry point -----------------------------------------------------------

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let only_shader = args
        .iter()
        .position(|a| a == "--shader")
        .and_then(|i| args.get(i + 1))
        .cloned();

    // Debug aid (no Vulkan device needed): dump the companion-stage SPIR-V so a
    // maintainer can spirv-val it independently of the recompiled shaders.
    if args.iter().any(|a| a == "--dump-companions") {
        let fs = passthrough_fs_forward_location0();
        let vs = fullscreen_vs_forward_locations(&[0]);
        std::fs::write("companion_fs.spv", words_to_bytes(&fs)).expect("write fs");
        std::fs::write("companion_vs.spv", words_to_bytes(&vs)).expect("write vs");
        println!("wrote companion_fs.spv and companion_vs.spv");
        return ExitCode::SUCCESS;
    }

    let gpu = match GpuHarness::new() {
        Ok(g) => g,
        Err(e) => {
            eprintln!("diff_harness: Vulkan init failed: {e}");
            eprintln!("  (a working Vulkan driver is required; run with LD_LIBRARY_PATH=/usr/lib)");
            return ExitCode::FAILURE;
        }
    };

    let mut any_divergence = false;
    let mut ran_any = false;

    for (name, stage) in enumerate_corpus() {
        if let Some(only) = &only_shader
            && *only != name
        {
            continue;
        }
        let Some(input) = harness_inputs(&name) else {
            println!("[skip] {name}: no synthetic input registered");
            continue;
        };

        // Contract (d): a deferred recompile is not a divergence.
        let code = read_code_dwords(&name);
        let insts = decode_all(&code);
        let recompiled = match recompile(&insts, stage) {
            Ok(r) => r,
            Err(RecompileError::Unsupported { offset, reason }) => {
                println!("[defer] {name}: recompile deferred at dword {offset}: {reason}");
                continue;
            }
            Err(e) => {
                eprintln!("[FAIL] {name}: recompile error: {e}");
                any_divergence = true;
                continue;
            }
        };

        ran_any = true;
        // A shader the interpreter rejects (or a mis-set-up mock input) surfaces as a
        // per-shader failure — not a panic that aborts the whole corpus run.
        let oracle = match run_oracle(&name, stage, &input) {
            Ok(o) => o,
            Err(e) => {
                eprintln!("[FAIL] {name}: {e}");
                any_divergence = true;
                continue;
            }
        };

        let diverged = match stage {
            ShaderStage::Vertex => run_vs_case(&gpu, &name, &recompiled, &input, &oracle),
            ShaderStage::Fragment => run_ps_case(&gpu, &name, &recompiled, &input, &oracle),
        };
        any_divergence |= diverged;
    }

    if !ran_any {
        eprintln!("diff_harness: no shader executed");
        return ExitCode::FAILURE;
    }
    if any_divergence {
        eprintln!("diff_harness: DIVERGENCE detected — see the per-lane report above");
        ExitCode::FAILURE
    } else {
        println!("diff_harness: all shaders agree (GPU == oracle within {EPSILON})");
        ExitCode::SUCCESS
    }
}

// ---- VS case: one texel per vertex -----------------------------------------

/// Execute the recompiled VS over the synthetic vertex buffer and diff each vertex's
/// GPU-read `pos0` against the oracle's `pos0`. Returns true on any divergence.
fn run_vs_case(
    gpu: &GpuHarness,
    name: &str,
    recompiled: &ps4_gcn::RecompiledShader,
    input: &HarnessInput,
    oracle: &[ExportRecord],
) -> bool {
    let n = input.vs_positions.len();
    // The VS-under-test drives gl_Position for each vertex and forwards its exp-pos0
    // value as the Location=0 param. execute_vs renders one flat-shaded triangle per
    // vertex (that vertex provoking) and returns the exact exported value per vertex —
    // no rasterized pixel position, no interpolation quantization (task-91).
    if n != 3 {
        eprintln!(
            "[skip] {name}: VS GPU path handles a 3-vertex primitive only (got {n}); \
             a rotation forms one triangle per vertex. Extend for N>3 if a corpus needs it."
        );
        return false;
    }
    let got_vals = match gpu.execute_vs(recompiled, &input.vs_positions) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[FAIL] {name}: GPU VS execution: {e}");
            return true;
        }
    };
    let mut diverged = false;
    for (v, &got) in got_vals.iter().enumerate().take(n) {
        let want = oracle
            .iter()
            .find(|e| e.lane == v && e.target == ExportTarget::Pos(0))
            .map(|e| e.values)
            .unwrap_or_else(|| panic!("{name}: oracle missing pos0 for vertex {v}"));
        if !within_eps(got, want) {
            diverged = true;
            eprintln!(
                "[DIVERGE] {name} vertex {v}: GPU pos0 {got:?} != oracle {want:?} \
                 (eps {EPSILON})"
            );
        }
    }
    if !diverged {
        println!("[ok] {name}: {n} vertices, GPU pos0 == oracle");
    }
    diverged
}

/// The exported value from a NaN-cleared flat-triangle render: the first covered
/// (all-finite) texel, identical across every covered fragment since the FS reads the
/// provoking vertex's value flat. Returns all-NaN if nothing was covered (a
/// degenerate/off-screen triangle from a bad recompile), which fails the eps compare
/// as a divergence rather than silently passing.
fn first_covered_texel(texels: &[[f32; 4]]) -> [f32; 4] {
    texels
        .iter()
        .copied()
        .find(|t| t.iter().all(|c| c.is_finite()))
        .unwrap_or([f32::NAN; 4])
}

// ---- PS case: single fragment ----------------------------------------------

/// Execute the recompiled PS for one fragment, driving each `Location` interpolant
/// input with the oracle's plane value (contract (a)), and diff the GPU-read `mrt0`
/// against the oracle's `mrt0`. Returns true on divergence.
fn run_ps_case(
    gpu: &GpuHarness,
    name: &str,
    recompiled: &ps4_gcn::RecompiledShader,
    input: &HarnessInput,
    oracle: &[ExportRecord],
) -> bool {
    // Contract (a): build the vec4 interpolant value per input Location from the
    // oracle's own plane equation at the fragment's (I,J).
    let (i, j) = input.ps_bary;
    let mut interpolants: BTreeMap<u32, [f32; 4]> = BTreeMap::new();
    for var in &recompiled.io.inputs {
        let attr = var.location as usize;
        let planes = input.ps_planes.get(attr).copied().unwrap_or([[0.0; 3]; 4]);
        let mut vals = [0.0f32; 4];
        for (c, slot) in vals.iter_mut().enumerate() {
            *slot = plane_eval(planes[c], i, j);
        }
        interpolants.insert(var.location, vals);
    }

    let gpu_out = match gpu.execute_ps(recompiled, &interpolants, input.ps_texture.as_ref()) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[FAIL] {name}: GPU PS execution: {e}");
            return true;
        }
    };
    let want = oracle
        .iter()
        .find(|e| e.lane == 0 && e.target == ExportTarget::Mrt(0))
        .map(|e| e.values)
        .unwrap_or_else(|| panic!("{name}: oracle missing mrt0"));
    if !within_eps(gpu_out, want) {
        eprintln!(
            "[DIVERGE] {name} fragment 0: GPU mrt0 {gpu_out:?} != oracle {want:?} (eps {EPSILON})"
        );
        true
    } else {
        println!("[ok] {name}: fragment 0, GPU mrt0 == oracle");
        false
    }
}

fn within_eps(a: [f32; 4], b: [f32; 4]) -> bool {
    a.iter().zip(b).all(|(x, y)| (x - y).abs() <= EPSILON)
}

fn words_to_bytes(words: &[u32]) -> Vec<u8> {
    let mut b = Vec::with_capacity(words.len() * 4);
    for w in words {
        b.extend_from_slice(&w.to_le_bytes());
    }
    b
}

// ============================================================================
// Headless Vulkan (self-contained; does NOT reuse the surface-bound
// VulkanContext). One instance/device, an RGBA32F offscreen target, a graphics
// pipeline built from the recompiled SPIR-V plus a trivial companion stage, a
// draw, and a readback.
// ============================================================================

const RT_FORMAT: vk::Format = vk::Format::R32G32B32A32_SFLOAT;

struct GpuHarness {
    _entry: ash::Entry,
    instance: ash::Instance,
    device: ash::Device,
    queue: vk::Queue,
    command_pool: vk::CommandPool,
    mem_props: vk::PhysicalDeviceMemoryProperties,
}

impl GpuHarness {
    fn new() -> Result<Self, String> {
        unsafe {
            let entry = ash::Entry::load().map_err(|e| format!("load Vulkan: {e}"))?;
            let app_name = CString::new("unemups4-diff-harness").unwrap();
            let app_info = vk::ApplicationInfo::default()
                .application_name(&app_name)
                .api_version(vk::API_VERSION_1_1);
            // Headless: no surface, no swapchain extensions.
            let ci = vk::InstanceCreateInfo::default().application_info(&app_info);
            let instance = entry
                .create_instance(&ci, None)
                .map_err(|e| format!("create instance: {e}"))?;

            let pdevices = instance
                .enumerate_physical_devices()
                .map_err(|e| format!("enumerate devices: {e}"))?;
            let pdevice = *pdevices.first().ok_or("no Vulkan physical device")?;
            let mem_props = instance.get_physical_device_memory_properties(pdevice);

            // Pick any graphics-capable queue family.
            let qfams = instance.get_physical_device_queue_family_properties(pdevice);
            let queue_family = qfams
                .iter()
                .position(|q| q.queue_flags.contains(vk::QueueFlags::GRAPHICS))
                .ok_or("no graphics queue family")? as u32;

            let prio = [1.0f32];
            let qci = [vk::DeviceQueueCreateInfo::default()
                .queue_family_index(queue_family)
                .queue_priorities(&prio)];
            let dci = vk::DeviceCreateInfo::default().queue_create_infos(&qci);
            let device = instance
                .create_device(pdevice, &dci, None)
                .map_err(|e| format!("create device: {e}"))?;
            let queue = device.get_device_queue(queue_family, 0);

            let pool_ci = vk::CommandPoolCreateInfo::default()
                .queue_family_index(queue_family)
                .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
            let command_pool = device
                .create_command_pool(&pool_ci, None)
                .map_err(|e| format!("create command pool: {e}"))?;

            Ok(GpuHarness {
                _entry: entry,
                instance,
                device,
                queue,
                command_pool,
                mem_props,
            })
        }
    }

    fn find_mem_type(&self, type_bits: u32, props: vk::MemoryPropertyFlags) -> Option<u32> {
        (0..self.mem_props.memory_type_count).find(|&i| {
            (type_bits & (1 << i)) != 0
                && self.mem_props.memory_types[i as usize]
                    .property_flags
                    .contains(props)
        })
    }

    unsafe fn shader_module(&self, spirv: &[u32]) -> Result<vk::ShaderModule, String> {
        let ci = vk::ShaderModuleCreateInfo::default().code(spirv);
        unsafe { self.device.create_shader_module(&ci, None) }
            .map_err(|e| format!("create shader module: {e}"))
    }

    /// Execute the recompiled VS, returning one exported `exp pos0` value per input
    /// vertex. Each vertex is rendered as its own flat-shaded triangle (that vertex
    /// provoking) into an RGBA32F target; the flat companion FS carries the vertex's
    /// `Location=0` param across the whole triangle, so any covered texel is that
    /// vertex's exported value verbatim — no interpolation or pixel quantization.
    fn execute_vs(
        &self,
        recompiled: &ps4_gcn::RecompiledShader,
        positions: &[[f32; 4]],
    ) -> Result<Vec<[f32; 4]>, String> {
        const DIM: u32 = 64;
        unsafe { self.render_vs(recompiled, positions, DIM) }
    }

    /// Execute the recompiled PS. A fullscreen triangle whose single interpolant
    /// (fed the oracle's plane value, contract (a)) covers a 1×1 target; the PS's
    /// `exp mrt0` is the one texel read back.
    fn execute_ps(
        &self,
        recompiled: &ps4_gcn::RecompiledShader,
        interpolants: &BTreeMap<u32, [f32; 4]>,
        texture: Option<&HarnessTexture>,
    ) -> Result<[f32; 4], String> {
        let texel = unsafe { self.render_ps(recompiled, interpolants, texture)? };
        Ok(texel)
    }

    // ---- offscreen render target + readback --------------------------------

    unsafe fn create_color_target(
        &self,
        width: u32,
        height: u32,
    ) -> Result<OffscreenTarget, String> {
        unsafe {
            let image_ci = vk::ImageCreateInfo::default()
                .image_type(vk::ImageType::TYPE_2D)
                .format(RT_FORMAT)
                .extent(vk::Extent3D {
                    width,
                    height,
                    depth: 1,
                })
                .mip_levels(1)
                .array_layers(1)
                .samples(vk::SampleCountFlags::TYPE_1)
                .tiling(vk::ImageTiling::OPTIMAL)
                .usage(vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::TRANSFER_SRC)
                .initial_layout(vk::ImageLayout::UNDEFINED);
            let image = self
                .device
                .create_image(&image_ci, None)
                .map_err(|e| format!("create image: {e}"))?;
            let req = self.device.get_image_memory_requirements(image);
            let mt = self
                .find_mem_type(req.memory_type_bits, vk::MemoryPropertyFlags::DEVICE_LOCAL)
                .ok_or("no device-local memory type")?;
            let alloc = vk::MemoryAllocateInfo::default()
                .allocation_size(req.size)
                .memory_type_index(mt);
            let mem = self
                .device
                .allocate_memory(&alloc, None)
                .map_err(|e| format!("allocate image memory: {e}"))?;
            self.device
                .bind_image_memory(image, mem, 0)
                .map_err(|e| format!("bind image: {e}"))?;

            let view_ci = vk::ImageViewCreateInfo::default()
                .image(image)
                .view_type(vk::ImageViewType::TYPE_2D)
                .format(RT_FORMAT)
                .subresource_range(vk::ImageSubresourceRange {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    base_mip_level: 0,
                    level_count: 1,
                    base_array_layer: 0,
                    layer_count: 1,
                });
            let view = self
                .device
                .create_image_view(&view_ci, None)
                .map_err(|e| format!("create image view: {e}"))?;
            Ok(OffscreenTarget {
                image,
                mem,
                view,
                width,
                height,
            })
        }
    }

    unsafe fn create_readback_buffer(&self, size: u64) -> Result<HostBuffer, String> {
        unsafe {
            let bci = vk::BufferCreateInfo::default()
                .size(size)
                .usage(vk::BufferUsageFlags::TRANSFER_DST)
                .sharing_mode(vk::SharingMode::EXCLUSIVE);
            let buffer = self
                .device
                .create_buffer(&bci, None)
                .map_err(|e| format!("create readback buffer: {e}"))?;
            let req = self.device.get_buffer_memory_requirements(buffer);
            let mt = self
                .find_mem_type(
                    req.memory_type_bits,
                    vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
                )
                .ok_or("no host-visible memory type")?;
            let alloc = vk::MemoryAllocateInfo::default()
                .allocation_size(req.size)
                .memory_type_index(mt);
            let mem = self
                .device
                .allocate_memory(&alloc, None)
                .map_err(|e| format!("allocate readback memory: {e}"))?;
            self.device
                .bind_buffer_memory(buffer, mem, 0)
                .map_err(|e| format!("bind readback buffer: {e}"))?;
            Ok(HostBuffer { buffer, mem, size })
        }
    }

    unsafe fn create_storage_buffer(&self, bytes: &[u8]) -> Result<HostBuffer, String> {
        unsafe {
            let size = bytes.len().max(16) as u64;
            let bci = vk::BufferCreateInfo::default()
                .size(size)
                .usage(vk::BufferUsageFlags::STORAGE_BUFFER)
                .sharing_mode(vk::SharingMode::EXCLUSIVE);
            let buffer = self
                .device
                .create_buffer(&bci, None)
                .map_err(|e| format!("create storage buffer: {e}"))?;
            let req = self.device.get_buffer_memory_requirements(buffer);
            let mt = self
                .find_mem_type(
                    req.memory_type_bits,
                    vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
                )
                .ok_or("no host-visible memory type")?;
            let alloc = vk::MemoryAllocateInfo::default()
                .allocation_size(req.size)
                .memory_type_index(mt);
            let mem = self
                .device
                .allocate_memory(&alloc, None)
                .map_err(|e| format!("allocate storage memory: {e}"))?;
            self.device
                .bind_buffer_memory(buffer, mem, 0)
                .map_err(|e| format!("bind storage buffer: {e}"))?;
            if !bytes.is_empty() {
                let ptr = self
                    .device
                    .map_memory(mem, 0, size, vk::MemoryMapFlags::empty())
                    .map_err(|e| format!("map storage: {e}"))? as *mut u8;
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
                self.device.unmap_memory(mem);
            }
            Ok(HostBuffer { buffer, mem, size })
        }
    }

    /// Host-visible upload buffer usable as the source of a buffer→image copy.
    /// Carries `TRANSFER_SRC` so `vkCmdCopyBufferToImage` is legal
    /// (VUID-vkCmdCopyBufferToImage-srcBuffer-00174, Vulkan spec).
    unsafe fn create_staging_buffer(&self, bytes: &[u8]) -> Result<HostBuffer, String> {
        unsafe {
            let size = bytes.len().max(16) as u64;
            let bci = vk::BufferCreateInfo::default()
                .size(size)
                .usage(vk::BufferUsageFlags::TRANSFER_SRC)
                .sharing_mode(vk::SharingMode::EXCLUSIVE);
            let buffer = self
                .device
                .create_buffer(&bci, None)
                .map_err(|e| format!("create staging buffer: {e}"))?;
            let req = self.device.get_buffer_memory_requirements(buffer);
            let mt = self
                .find_mem_type(
                    req.memory_type_bits,
                    vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
                )
                .ok_or("no host-visible memory type")?;
            let alloc = vk::MemoryAllocateInfo::default()
                .allocation_size(req.size)
                .memory_type_index(mt);
            let mem = self
                .device
                .allocate_memory(&alloc, None)
                .map_err(|e| format!("allocate staging memory: {e}"))?;
            self.device
                .bind_buffer_memory(buffer, mem, 0)
                .map_err(|e| format!("bind staging buffer: {e}"))?;
            if !bytes.is_empty() {
                let ptr = self
                    .device
                    .map_memory(mem, 0, size, vk::MemoryMapFlags::empty())
                    .map_err(|e| format!("map staging: {e}"))? as *mut u8;
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
                self.device.unmap_memory(mem);
            }
            Ok(HostBuffer { buffer, mem, size })
        }
    }

    unsafe fn create_render_pass(&self) -> Result<vk::RenderPass, String> {
        unsafe {
            let attach = [vk::AttachmentDescription::default()
                .format(RT_FORMAT)
                .samples(vk::SampleCountFlags::TYPE_1)
                .load_op(vk::AttachmentLoadOp::CLEAR)
                .store_op(vk::AttachmentStoreOp::STORE)
                .stencil_load_op(vk::AttachmentLoadOp::DONT_CARE)
                .stencil_store_op(vk::AttachmentStoreOp::DONT_CARE)
                .initial_layout(vk::ImageLayout::UNDEFINED)
                .final_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)];
            let color_ref = [vk::AttachmentReference::default()
                .attachment(0)
                .layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)];
            let subpass = [vk::SubpassDescription::default()
                .pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS)
                .color_attachments(&color_ref)];
            // Make the color-attachment writes available AND visible to the transfer read in
            // `readback` (a separate submit copying this image to a buffer). Without an
            // explicit dependency the implicit subpass→EXTERNAL one ends at BOTTOM_OF_PIPE /
            // dstAccess=0, so on a tiled/deferred driver the copy can read stale texels and
            // the harness prints a false verdict. (Vulkan spec 8.1, synchronization chapter.)
            let deps = [vk::SubpassDependency::default()
                .src_subpass(0)
                .dst_subpass(vk::SUBPASS_EXTERNAL)
                .src_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
                .src_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)
                .dst_stage_mask(vk::PipelineStageFlags::TRANSFER)
                .dst_access_mask(vk::AccessFlags::TRANSFER_READ)];
            let ci = vk::RenderPassCreateInfo::default()
                .attachments(&attach)
                .subpasses(&subpass)
                .dependencies(&deps);
            self.device
                .create_render_pass(&ci, None)
                .map_err(|e| format!("create render pass: {e}"))
        }
    }

    /// Submit `record` on a one-shot command buffer and wait for completion.
    unsafe fn submit_sync(&self, record: impl FnOnce(vk::CommandBuffer)) -> Result<(), String> {
        unsafe {
            let alloc = vk::CommandBufferAllocateInfo::default()
                .command_pool(self.command_pool)
                .level(vk::CommandBufferLevel::PRIMARY)
                .command_buffer_count(1);
            let cb = self
                .device
                .allocate_command_buffers(&alloc)
                .map_err(|e| format!("alloc cmd buffer: {e}"))?[0];
            let begin = vk::CommandBufferBeginInfo::default()
                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
            self.device
                .begin_command_buffer(cb, &begin)
                .map_err(|e| format!("begin cmd: {e}"))?;
            record(cb);
            self.device
                .end_command_buffer(cb)
                .map_err(|e| format!("end cmd: {e}"))?;

            let fence = self
                .device
                .create_fence(&vk::FenceCreateInfo::default(), None)
                .map_err(|e| format!("create fence: {e}"))?;
            let cbs = [cb];
            let submit = [vk::SubmitInfo::default().command_buffers(&cbs)];
            self.device
                .queue_submit(self.queue, &submit, fence)
                .map_err(|e| format!("submit: {e}"))?;
            // Finite wait: a pathological recompiled shader (runaway loop, unsupported
            // instruction the driver mis-executes) must surface as a per-shader failure,
            // not hang the whole corpus run. On VK_TIMEOUT ash returns Err here.
            self.device
                .wait_for_fences(&[fence], true, GPU_FENCE_TIMEOUT_NS)
                .map_err(|e| format!("wait fence: {e}"))?;
            self.device.destroy_fence(fence, None);
            self.device.free_command_buffers(self.command_pool, &[cb]);
            Ok(())
        }
    }

    /// Copy the color target into the readback buffer and map its f32 texels.
    unsafe fn readback(
        &self,
        target: &OffscreenTarget,
        readback: &HostBuffer,
    ) -> Result<Vec<[f32; 4]>, String> {
        unsafe {
            self.submit_sync(|cb| {
                let region = vk::BufferImageCopy::default()
                    .image_subresource(vk::ImageSubresourceLayers {
                        aspect_mask: vk::ImageAspectFlags::COLOR,
                        mip_level: 0,
                        base_array_layer: 0,
                        layer_count: 1,
                    })
                    .image_extent(vk::Extent3D {
                        width: target.width,
                        height: target.height,
                        depth: 1,
                    });
                self.device.cmd_copy_image_to_buffer(
                    cb,
                    target.image,
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    readback.buffer,
                    &[region],
                );
            })?;
            let ptr = self
                .device
                .map_memory(readback.mem, 0, readback.size, vk::MemoryMapFlags::empty())
                .map_err(|e| format!("map readback: {e}"))? as *const f32;
            let count = (target.width * target.height) as usize;
            let mut out = Vec::with_capacity(count);
            for i in 0..count {
                let base = i * 4;
                out.push([
                    *ptr.add(base),
                    *ptr.add(base + 1),
                    *ptr.add(base + 2),
                    *ptr.add(base + 3),
                ]);
            }
            self.device.unmap_memory(readback.mem);
            Ok(out)
        }
    }

    // ---- VS pipeline -------------------------------------------------------

    unsafe fn render_vs(
        &self,
        recompiled: &ps4_gcn::RecompiledShader,
        positions: &[[f32; 4]],
        dim: u32,
    ) -> Result<Vec<[f32; 4]>, String> {
        unsafe {
            let n = positions.len();
            let target = self.create_color_target(dim, dim)?;
            let render_pass = self.create_render_pass()?;
            let fb = self.framebuffer(render_pass, &target)?;

            // Repeated vertex SSBO: for each vertex i we render the SAME triangle with i
            // as its FIRST (provoking) vertex, so the flat companion FS carries vertex
            // i's exp pos0 across the whole triangle. Block i = [v_i, v_{i+1}, v_{i+2}]
            // laid out consecutively; draw i is a non-indexed draw with firstVertex=3*i,
            // so gl_VertexIndex 3*i (provoking), 3*i+1, 3*i+2 fetch that rotation.
            let mut vb_bytes = Vec::new();
            for i in 0..n {
                for k in 0..3 {
                    for c in &positions[(i + k) % n] {
                        vb_bytes.extend_from_slice(&c.to_bits().to_le_bytes());
                    }
                }
            }
            let vb = self.create_storage_buffer(&vb_bytes)?;

            let vs_mod = self.shader_module(&recompiled.spirv)?;
            let fs_spirv = passthrough_fs_forward_location0();
            let fs_mod = self.shader_module(&fs_spirv)?;

            // Descriptor set 0, binding 0: the vertex-buffer SSBO.
            let dsl = self.storage_descriptor_layout()?;
            let (dpool, dset) = self.alloc_descriptor(dsl, &vb)?;

            // Push constant: num_records (contract (c)) — the SSBO holds 3*n vec4s.
            let pc_ranges = pc_ranges_for(&recompiled.io);
            let layout = self.pipeline_layout(&[dsl], &pc_ranges)?;

            // TRIANGLE_LIST: the triangle always rasterizes — unlike a POINT_LIST, which
            // needs the VS to write gl_PointSize (the recompiled VS does not, so points
            // rendered nothing). The flat FS then gives every fragment the provoking
            // vertex's exact exported value, with no interpolation/pixel quantization.
            let pipeline = self.graphics_pipeline(
                render_pass,
                layout,
                vs_mod,
                fs_mod,
                vk::PrimitiveTopology::TRIANGLE_LIST,
                &target,
            )?;

            let readback = self.create_readback_buffer((dim as u64) * (dim as u64) * 16)?;
            let num_records = (3 * n) as u32;

            // The SSBO packs one vec4 (4 f32) per vertex consecutively, so the vertex
            // element stride is 16 bytes (task-140: pushed as push-constant member 1).
            let stride: u32 = 16;
            let pc_bytes: Vec<u8> = if pc_ranges.is_empty() {
                Vec::new()
            } else {
                let mut buf = vec![0u8; pc_ranges[0].size as usize];
                for f in &recompiled.io.push_constants {
                    let o = f.offset_bytes as usize;
                    match f.role {
                        PushConstantRole::NumRecords => {
                            buf[o..o + 4].copy_from_slice(&num_records.to_le_bytes());
                        }
                        PushConstantRole::Stride => {
                            buf[o..o + 4].copy_from_slice(&stride.to_le_bytes());
                        }
                        PushConstantRole::DstSel => {
                            // This harness lays out a tight vec4 per vertex with no
                            // swizzle, so push the IDENTITY dst_sel [4,5,6,7] (raw
                            // passthrough): channel ch reads source component ch (task-155).
                            buf[o..o + 4].copy_from_slice(&ps4_gcn::DST_SEL_IDENTITY.to_le_bytes());
                        }
                        PushConstantRole::Format => {
                            // Tight vec4-f32 vertices: dfmt 14 (_32_32_32_32) / nfmt 7 (float),
                            // packed dfmt[7:0] | nfmt[15:8] → the raw-dword fetch path (task-164).
                            let format: u32 = 14 | (7 << 8);
                            buf[o..o + 4].copy_from_slice(&format.to_le_bytes());
                        }
                    }
                }
                buf
            };

            // One flat triangle per vertex; the exported value is any covered texel.
            let mut values = Vec::with_capacity(n);
            for i in 0..n {
                self.submit_sync(|cb| {
                    // Clear to NaN so a covered (finite) texel is distinguishable from
                    // the background whatever the exported value is (incl. all-zero).
                    self.begin_pass(cb, render_pass, fb, &target, [f32::NAN; 4]);
                    self.device
                        .cmd_bind_pipeline(cb, vk::PipelineBindPoint::GRAPHICS, pipeline);
                    self.device.cmd_bind_descriptor_sets(
                        cb,
                        vk::PipelineBindPoint::GRAPHICS,
                        layout,
                        0,
                        &[dset],
                        &[],
                    );
                    if !pc_bytes.is_empty() {
                        self.device.cmd_push_constants(
                            cb,
                            layout,
                            vk::ShaderStageFlags::VERTEX,
                            0,
                            &pc_bytes,
                        );
                    }
                    // Triangle i: firstVertex = 3*i seeds gl_VertexIndex for the fetch.
                    self.device.cmd_draw(cb, 3, 1, (3 * i) as u32, 0);
                    self.device.cmd_end_render_pass(cb);
                })?;
                let texels = self.readback(&target, &readback)?;
                values.push(first_covered_texel(&texels));
            }

            // Cleanup (leak-free for a long-running loop).
            self.device.destroy_pipeline(pipeline, None);
            self.device.destroy_pipeline_layout(layout, None);
            self.device.destroy_descriptor_pool(dpool, None);
            self.device.destroy_descriptor_set_layout(dsl, None);
            self.device.destroy_shader_module(vs_mod, None);
            self.device.destroy_shader_module(fs_mod, None);
            self.destroy_target(target, render_pass, fb);
            self.destroy_host_buffer(vb);
            self.destroy_host_buffer(readback);
            Ok(values)
        }
    }

    // ---- PS pipeline -------------------------------------------------------

    unsafe fn render_ps(
        &self,
        recompiled: &ps4_gcn::RecompiledShader,
        interpolants: &BTreeMap<u32, [f32; 4]>,
        texture: Option<&HarnessTexture>,
    ) -> Result<[f32; 4], String> {
        unsafe {
            let target = self.create_color_target(1, 1)?;
            let render_pass = self.create_render_pass()?;
            let fb = self.framebuffer(render_pass, &target)?;

            // The companion VS forwards each PS Location interpolant as a constant
            // vec4 (fed the oracle plane value, contract (a)) — a flat, uniform value
            // so every fragment sees exactly the oracle's interpolant.
            let locations: Vec<u32> = interpolants.keys().copied().collect();
            let vs_spirv = fullscreen_vs_forward_locations(&locations);
            let vs_mod = self.shader_module(&vs_spirv)?;
            let ps_mod = self.shader_module(&recompiled.spirv)?;

            // The interpolant values travel to the VS as a push-constant block of
            // vec4[locations.len()].
            let mut pc_bytes = Vec::new();
            for loc in &locations {
                for c in interpolants[loc] {
                    pc_bytes.extend_from_slice(&c.to_bits().to_le_bytes());
                }
            }
            let pc_ranges = if pc_bytes.is_empty() {
                Vec::new()
            } else {
                vec![
                    vk::PushConstantRange::default()
                        .stage_flags(vk::ShaderStageFlags::VERTEX)
                        .offset(0)
                        .size(pc_bytes.len() as u32),
                ]
            };

            // If the recompiled PS declares a combined image-sampler (image_sample),
            // create + bind the sampled texture the oracle also samples (decision-3). The
            // set layout must match the recompiler's PS_TEXTURE_SET/BINDING.
            // This harness supplies exactly ONE `HarnessTexture`, so it can only honour a
            // single-texture PS. A module declaring several (task-199) would otherwise have
            // every sample silently read texture 0 — the very bug the multi-binding work
            // removed — so refuse instead of reporting a green diff against a wrong bind.
            if recompiled.io.samplers.len() > 1 {
                return Err(format!(
                    "recompiled PS declares {} combined image-samplers; this harness binds \
                     one texture and cannot drive a multi-texture module",
                    recompiled.io.samplers.len()
                ));
            }
            let sampler_binding = recompiled.io.samplers.first().copied();
            let sampled = match (sampler_binding, texture) {
                (Some(sb), Some(tex)) => {
                    Some((self.make_sampled_texture(sb.binding, tex)?, sb.binding))
                }
                (Some(_), None) => {
                    return Err(
                        "recompiled PS declares a sampler but no texture was supplied".into(),
                    );
                }
                _ => None,
            };
            let set_layouts: Vec<vk::DescriptorSetLayout> =
                sampled.iter().map(|(t, _)| t.dsl).collect();
            let layout = self.pipeline_layout(&set_layouts, &pc_ranges)?;
            let pipeline = self.graphics_pipeline(
                render_pass,
                layout,
                vs_mod,
                ps_mod,
                vk::PrimitiveTopology::TRIANGLE_LIST,
                &target,
            )?;

            let readback = self.create_readback_buffer(16)?;
            self.submit_sync(|cb| {
                self.begin_pass(cb, render_pass, fb, &target, [0.0, 0.0, 0.0, 0.0]);
                self.device
                    .cmd_bind_pipeline(cb, vk::PipelineBindPoint::GRAPHICS, pipeline);
                if let Some((tex, _)) = &sampled {
                    self.device.cmd_bind_descriptor_sets(
                        cb,
                        vk::PipelineBindPoint::GRAPHICS,
                        layout,
                        0,
                        &[tex.set],
                        &[],
                    );
                }
                if !pc_bytes.is_empty() {
                    self.device.cmd_push_constants(
                        cb,
                        layout,
                        vk::ShaderStageFlags::VERTEX,
                        0,
                        &pc_bytes,
                    );
                }
                // A single triangle covering the 1×1 target; 3 vertices, no buffer.
                self.device.cmd_draw(cb, 3, 1, 0, 0);
                self.device.cmd_end_render_pass(cb);
            })?;
            let texels = self.readback(&target, &readback)?;

            self.device.destroy_pipeline(pipeline, None);
            self.device.destroy_pipeline_layout(layout, None);
            self.device.destroy_shader_module(vs_mod, None);
            self.device.destroy_shader_module(ps_mod, None);
            if let Some((tex, _)) = sampled {
                self.destroy_sampled_texture(tex);
            }
            self.destroy_target(target, render_pass, fb);
            self.destroy_host_buffer(readback);
            Ok(texels[0])
        }
    }

    // ---- shared pipeline plumbing ------------------------------------------

    unsafe fn framebuffer(
        &self,
        render_pass: vk::RenderPass,
        target: &OffscreenTarget,
    ) -> Result<vk::Framebuffer, String> {
        unsafe {
            let attachments = [target.view];
            let ci = vk::FramebufferCreateInfo::default()
                .render_pass(render_pass)
                .attachments(&attachments)
                .width(target.width)
                .height(target.height)
                .layers(1);
            self.device
                .create_framebuffer(&ci, None)
                .map_err(|e| format!("create framebuffer: {e}"))
        }
    }

    unsafe fn begin_pass(
        &self,
        cb: vk::CommandBuffer,
        render_pass: vk::RenderPass,
        fb: vk::Framebuffer,
        target: &OffscreenTarget,
        clear_color: [f32; 4],
    ) {
        unsafe {
            let clear = [vk::ClearValue {
                color: vk::ClearColorValue {
                    float32: clear_color,
                },
            }];
            let begin = vk::RenderPassBeginInfo::default()
                .render_pass(render_pass)
                .framebuffer(fb)
                .render_area(vk::Rect2D {
                    offset: vk::Offset2D { x: 0, y: 0 },
                    extent: vk::Extent2D {
                        width: target.width,
                        height: target.height,
                    },
                })
                .clear_values(&clear);
            self.device
                .cmd_begin_render_pass(cb, &begin, vk::SubpassContents::INLINE);
            let viewport = [vk::Viewport {
                x: 0.0,
                y: 0.0,
                width: target.width as f32,
                height: target.height as f32,
                min_depth: 0.0,
                max_depth: 1.0,
            }];
            let scissor = [vk::Rect2D {
                offset: vk::Offset2D { x: 0, y: 0 },
                extent: vk::Extent2D {
                    width: target.width,
                    height: target.height,
                },
            }];
            self.device.cmd_set_viewport(cb, 0, &viewport);
            self.device.cmd_set_scissor(cb, 0, &scissor);
        }
    }

    unsafe fn storage_descriptor_layout(&self) -> Result<vk::DescriptorSetLayout, String> {
        unsafe {
            let binding = [vk::DescriptorSetLayoutBinding::default()
                .binding(0)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::VERTEX)];
            let ci = vk::DescriptorSetLayoutCreateInfo::default().bindings(&binding);
            self.device
                .create_descriptor_set_layout(&ci, None)
                .map_err(|e| format!("create dsl: {e}"))
        }
    }

    unsafe fn alloc_descriptor(
        &self,
        dsl: vk::DescriptorSetLayout,
        buf: &HostBuffer,
    ) -> Result<(vk::DescriptorPool, vk::DescriptorSet), String> {
        unsafe {
            let sizes = [vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::STORAGE_BUFFER)
                .descriptor_count(1)];
            let pci = vk::DescriptorPoolCreateInfo::default()
                .max_sets(1)
                .pool_sizes(&sizes);
            let pool = self
                .device
                .create_descriptor_pool(&pci, None)
                .map_err(|e| format!("create descriptor pool: {e}"))?;
            let layouts = [dsl];
            let aci = vk::DescriptorSetAllocateInfo::default()
                .descriptor_pool(pool)
                .set_layouts(&layouts);
            let set = self
                .device
                .allocate_descriptor_sets(&aci)
                .map_err(|e| format!("alloc descriptor set: {e}"))?[0];
            let info = [vk::DescriptorBufferInfo::default()
                .buffer(buf.buffer)
                .offset(0)
                .range(vk::WHOLE_SIZE)];
            let write = [vk::WriteDescriptorSet::default()
                .dst_set(set)
                .dst_binding(0)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&info)];
            self.device.update_descriptor_sets(&write, &[]);
            Ok((pool, set))
        }
    }

    /// A descriptor-set layout with one COMBINED_IMAGE_SAMPLER at `binding` (FRAGMENT),
    /// matching the recompiler's PS texture binding. The VS SSBO (binding 0) is absent —
    /// a sampling PS is paired with the harness's constant-forwarding VS (no fetch).
    unsafe fn sampler_descriptor_layout(
        &self,
        binding: u32,
    ) -> Result<vk::DescriptorSetLayout, String> {
        unsafe {
            let b = [vk::DescriptorSetLayoutBinding::default()
                .binding(binding)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT)];
            let ci = vk::DescriptorSetLayoutCreateInfo::default().bindings(&b);
            self.device
                .create_descriptor_set_layout(&ci, None)
                .map_err(|e| format!("create sampler dsl: {e}"))
        }
    }

    /// Create a linear R8G8B8A8 sampled image from `tex`, upload its texels, create a
    /// sampler (point/bilinear per `tex`), and write both into a combined image-sampler
    /// descriptor at `binding`. Returns the pool/set + the vk objects to destroy.
    unsafe fn make_sampled_texture(
        &self,
        binding: u32,
        tex: &HarnessTexture,
    ) -> Result<SampledTexture, String> {
        unsafe {
            // Sampled image (SAMPLED | TRANSFER_DST), device-local.
            let ci = vk::ImageCreateInfo::default()
                .image_type(vk::ImageType::TYPE_2D)
                .format(vk::Format::R8G8B8A8_UNORM)
                .extent(vk::Extent3D {
                    width: tex.width,
                    height: tex.height,
                    depth: 1,
                })
                .mip_levels(1)
                .array_layers(1)
                .samples(vk::SampleCountFlags::TYPE_1)
                .tiling(vk::ImageTiling::OPTIMAL)
                .usage(vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_DST)
                .initial_layout(vk::ImageLayout::UNDEFINED);
            let image = self
                .device
                .create_image(&ci, None)
                .map_err(|e| format!("create sampled image: {e}"))?;
            let req = self.device.get_image_memory_requirements(image);
            let mt = self
                .find_mem_type(req.memory_type_bits, vk::MemoryPropertyFlags::DEVICE_LOCAL)
                .ok_or("no device-local mem for image")?;
            let mem = self
                .device
                .allocate_memory(
                    &vk::MemoryAllocateInfo::default()
                        .allocation_size(req.size)
                        .memory_type_index(mt),
                    None,
                )
                .map_err(|e| format!("alloc image mem: {e}"))?;
            self.device
                .bind_image_memory(image, mem, 0)
                .map_err(|e| format!("bind image mem: {e}"))?;

            // Staging buffer with the texel bytes. Needs TRANSFER_SRC so the
            // buffer→image copy below is legal
            // (VUID-vkCmdCopyBufferToImage-srcBuffer-00174, Vulkan spec).
            let staging = self.create_staging_buffer(&tex.texels)?;

            // Upload: UNDEFINED → TRANSFER_DST, copy, → SHADER_READ_ONLY.
            self.submit_sync(|cb| {
                let to_dst = vk::ImageMemoryBarrier::default()
                    .old_layout(vk::ImageLayout::UNDEFINED)
                    .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                    .src_access_mask(vk::AccessFlags::empty())
                    .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                    .image(image)
                    .subresource_range(color_subresource());
                self.device.cmd_pipeline_barrier(
                    cb,
                    vk::PipelineStageFlags::TOP_OF_PIPE,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &[to_dst],
                );
                let region = vk::BufferImageCopy::default()
                    .image_subresource(
                        vk::ImageSubresourceLayers::default()
                            .aspect_mask(vk::ImageAspectFlags::COLOR)
                            .mip_level(0)
                            .base_array_layer(0)
                            .layer_count(1),
                    )
                    .image_extent(vk::Extent3D {
                        width: tex.width,
                        height: tex.height,
                        depth: 1,
                    });
                self.device.cmd_copy_buffer_to_image(
                    cb,
                    staging.buffer,
                    image,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    &[region],
                );
                let to_read = vk::ImageMemoryBarrier::default()
                    .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                    .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                    .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                    .dst_access_mask(vk::AccessFlags::SHADER_READ)
                    .image(image)
                    .subresource_range(color_subresource());
                self.device.cmd_pipeline_barrier(
                    cb,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::PipelineStageFlags::FRAGMENT_SHADER,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &[to_read],
                );
            })?;
            self.destroy_host_buffer(staging);

            let view = self
                .device
                .create_image_view(
                    &vk::ImageViewCreateInfo::default()
                        .image(image)
                        .view_type(vk::ImageViewType::TYPE_2D)
                        .format(vk::Format::R8G8B8A8_UNORM)
                        .subresource_range(color_subresource()),
                    None,
                )
                .map_err(|e| format!("create image view: {e}"))?;

            let filter = if tex.point {
                vk::Filter::NEAREST
            } else {
                vk::Filter::LINEAR
            };
            let sampler = self
                .device
                .create_sampler(
                    &vk::SamplerCreateInfo::default()
                        .mag_filter(filter)
                        .min_filter(filter)
                        .mipmap_mode(vk::SamplerMipmapMode::NEAREST)
                        .address_mode_u(vk::SamplerAddressMode::REPEAT)
                        .address_mode_v(vk::SamplerAddressMode::REPEAT)
                        .address_mode_w(vk::SamplerAddressMode::REPEAT)
                        .max_lod(0.0),
                    None,
                )
                .map_err(|e| format!("create sampler: {e}"))?;

            // Descriptor pool + set for the combined image-sampler.
            let dsl = self.sampler_descriptor_layout(binding)?;
            let sizes = [vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .descriptor_count(1)];
            let pool = self
                .device
                .create_descriptor_pool(
                    &vk::DescriptorPoolCreateInfo::default()
                        .max_sets(1)
                        .pool_sizes(&sizes),
                    None,
                )
                .map_err(|e| format!("create sampler pool: {e}"))?;
            let layouts = [dsl];
            let set = self
                .device
                .allocate_descriptor_sets(
                    &vk::DescriptorSetAllocateInfo::default()
                        .descriptor_pool(pool)
                        .set_layouts(&layouts),
                )
                .map_err(|e| format!("alloc sampler set: {e}"))?[0];
            let ii = [vk::DescriptorImageInfo::default()
                .sampler(sampler)
                .image_view(view)
                .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
            let write = [vk::WriteDescriptorSet::default()
                .dst_set(set)
                .dst_binding(binding)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .image_info(&ii)];
            self.device.update_descriptor_sets(&write, &[]);

            Ok(SampledTexture {
                image,
                view,
                mem,
                sampler,
                dsl,
                pool,
                set,
            })
        }
    }

    unsafe fn pipeline_layout(
        &self,
        set_layouts: &[vk::DescriptorSetLayout],
        pc_ranges: &[vk::PushConstantRange],
    ) -> Result<vk::PipelineLayout, String> {
        unsafe {
            let ci = vk::PipelineLayoutCreateInfo::default()
                .set_layouts(set_layouts)
                .push_constant_ranges(pc_ranges);
            self.device
                .create_pipeline_layout(&ci, None)
                .map_err(|e| format!("create pipeline layout: {e}"))
        }
    }

    #[allow(clippy::too_many_arguments)]
    unsafe fn graphics_pipeline(
        &self,
        render_pass: vk::RenderPass,
        layout: vk::PipelineLayout,
        vs: vk::ShaderModule,
        fs: vk::ShaderModule,
        topology: vk::PrimitiveTopology,
        target: &OffscreenTarget,
    ) -> Result<vk::Pipeline, String> {
        unsafe {
            let entry = CString::new("main").unwrap();
            let stages = [
                vk::PipelineShaderStageCreateInfo::default()
                    .stage(vk::ShaderStageFlags::VERTEX)
                    .module(vs)
                    .name(&entry),
                vk::PipelineShaderStageCreateInfo::default()
                    .stage(vk::ShaderStageFlags::FRAGMENT)
                    .module(fs)
                    .name(&entry),
            ];
            let vertex_input = vk::PipelineVertexInputStateCreateInfo::default();
            let input_assembly =
                vk::PipelineInputAssemblyStateCreateInfo::default().topology(topology);
            let viewport_state = vk::PipelineViewportStateCreateInfo::default()
                .viewport_count(1)
                .scissor_count(1);
            let raster = vk::PipelineRasterizationStateCreateInfo::default()
                .polygon_mode(vk::PolygonMode::FILL)
                .cull_mode(vk::CullModeFlags::NONE)
                .front_face(vk::FrontFace::COUNTER_CLOCKWISE)
                .line_width(1.0);
            let multisample = vk::PipelineMultisampleStateCreateInfo::default()
                .rasterization_samples(vk::SampleCountFlags::TYPE_1);
            let color_blend_attach = [vk::PipelineColorBlendAttachmentState::default()
                .color_write_mask(vk::ColorComponentFlags::RGBA)
                .blend_enable(false)];
            let color_blend =
                vk::PipelineColorBlendStateCreateInfo::default().attachments(&color_blend_attach);
            let dyn_states = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
            let dynamic = vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dyn_states);
            let _ = target;

            let ci = [vk::GraphicsPipelineCreateInfo::default()
                .stages(&stages)
                .vertex_input_state(&vertex_input)
                .input_assembly_state(&input_assembly)
                .viewport_state(&viewport_state)
                .rasterization_state(&raster)
                .multisample_state(&multisample)
                .color_blend_state(&color_blend)
                .dynamic_state(&dynamic)
                .layout(layout)
                .render_pass(render_pass)
                .subpass(0)];
            let pipelines = self
                .device
                .create_graphics_pipelines(vk::PipelineCache::null(), &ci, None)
                .map_err(|(_, e)| format!("create graphics pipeline: {e}"))?;
            Ok(pipelines[0])
        }
    }

    unsafe fn destroy_target(
        &self,
        t: OffscreenTarget,
        render_pass: vk::RenderPass,
        fb: vk::Framebuffer,
    ) {
        unsafe {
            self.device.destroy_framebuffer(fb, None);
            self.device.destroy_render_pass(render_pass, None);
            self.device.destroy_image_view(t.view, None);
            self.device.destroy_image(t.image, None);
            self.device.free_memory(t.mem, None);
        }
    }

    unsafe fn destroy_host_buffer(&self, b: HostBuffer) {
        unsafe {
            self.device.destroy_buffer(b.buffer, None);
            self.device.free_memory(b.mem, None);
        }
    }

    unsafe fn destroy_sampled_texture(&self, t: SampledTexture) {
        unsafe {
            self.device.destroy_descriptor_pool(t.pool, None);
            self.device.destroy_descriptor_set_layout(t.dsl, None);
            self.device.destroy_sampler(t.sampler, None);
            self.device.destroy_image_view(t.view, None);
            self.device.destroy_image(t.image, None);
            self.device.free_memory(t.mem, None);
        }
    }
}

/// A COLOR, mip-0, single-layer subresource range — the sampled texture's whole extent.
fn color_subresource() -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .base_mip_level(0)
        .level_count(1)
        .base_array_layer(0)
        .layer_count(1)
}

/// A sampled image + sampler + its combined image-sampler descriptor set (the GPU side
/// of the sampling differential). All vk objects are destroyed via
/// `destroy_sampled_texture` after the readback.
struct SampledTexture {
    image: vk::Image,
    view: vk::ImageView,
    mem: vk::DeviceMemory,
    sampler: vk::Sampler,
    dsl: vk::DescriptorSetLayout,
    pool: vk::DescriptorPool,
    set: vk::DescriptorSet,
}

impl Drop for GpuHarness {
    fn drop(&mut self) {
        unsafe {
            let _ = self.device.device_wait_idle();
            self.device.destroy_command_pool(self.command_pool, None);
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
        }
    }
}

struct OffscreenTarget {
    image: vk::Image,
    mem: vk::DeviceMemory,
    view: vk::ImageView,
    width: u32,
    height: u32,
}

struct HostBuffer {
    buffer: vk::Buffer,
    mem: vk::DeviceMemory,
    size: u64,
}

/// The push-constant ranges the recompiled VS declares (contract (c): num_records).
fn pc_ranges_for(io: &IoLayout) -> Vec<vk::PushConstantRange> {
    if io.push_constants.is_empty() {
        return Vec::new();
    }
    let total: u32 = io
        .push_constants
        .iter()
        .map(|f| f.offset_bytes + f.size_bytes)
        .max()
        .unwrap_or(0);
    vec![
        vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::VERTEX)
            .offset(0)
            .size(total),
    ]
}

// ============================================================================
// Companion shaders, hand-assembled SPIR-V (portable Shader capability only).
// These are the trivial partner stages the recompiled shader is paired with; they
// carry no GCN semantics, only the plumbing to route inputs/outputs.
// ============================================================================

/// A fragment shader that forwards its `Location=0` vec4 input to `Location=0`
/// output — pairs with the recompiled VS so `exp pos0`→`gl_Position`, delivered as
/// the `Location=0` param interpolant, lands in the color target. Built with rspirv
/// (already in the tree via ps4-gcn's dep — but rspirv is not a direct dep here, so
/// this assembles the words by hand-driven builder through ps4-gcn's re-export).
fn passthrough_fs_forward_location0() -> Vec<u32> {
    build_forward_fs()
}

/// A vertex shader emitting a fullscreen triangle and forwarding constant vec4
/// interpolants (from a push-constant block) to the given output `Location`s. Pairs
/// with the recompiled PS: each PS `Location=attr` input reads the oracle's plane
/// value (contract (a)).
fn fullscreen_vs_forward_locations(locations: &[u32]) -> Vec<u32> {
    build_fullscreen_vs(locations)
}

// The companion shaders are assembled with rspirv in the included module below.
// They are tiny and fixed; see `build_forward_fs` / `build_fullscreen_vs`. The file
// lives in a subdirectory so cargo's `src/bin/` auto-discovery does not treat it as
// a separate binary (only top-level `.rs` files there are binaries).
include!("diff_harness_support/companion_spirv.rs");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_covered_texel_picks_finite_over_nan_clear() {
        // The flat-triangle render clears to NaN and writes the exported value only on
        // covered fragments; the readback is NaN background with the value where drawn.
        let nan = [f32::NAN; 4];
        let val = [1.0, 2.0, 3.0, 4.0];
        assert_eq!(first_covered_texel(&[nan, nan, val, nan]), val);
    }

    #[test]
    fn first_covered_texel_all_nan_when_nothing_covered() {
        // A degenerate/off-screen triangle (bad recompile) covers nothing → all-NaN,
        // which fails the eps compare as a divergence rather than silently passing.
        let all_nan = first_covered_texel(&[[f32::NAN; 4]; 4]);
        assert!(all_nan.iter().all(|c| c.is_nan()));
    }
}
