//! Differential harness — TIER (a), headless (doc-4 §1, decision-3, decision-6).
//!
//! decision-3's "the recompiler MUST agree with the interpreter" discipline made
//! EXECUTABLE, and the structural guard against interp/recompiler drift. This is the
//! Vulkan-free half; the live-GPU half is a maintainer-run binary in `ps4-gpu` (it
//! executes the recompiled SPIR-V on a real device and diffs it against the same
//! oracle — see `crates/gpu/src/bin/diff_harness.rs`).
//!
//! Two things are asserted here, per corpus shader, DATA-DRIVEN over the corpus
//! directory (adding a `.s` + `.code.bin` entry needs no code change):
//!
//! 1. **Oracle vs analytic expectation.** For each shader with a registered
//!    [`ShaderSpec`], run the CPU interpreter (the oracle) over synthetic launch
//!    inputs and assert the captured exports equal values computed HERE, by hand,
//!    from the shader's math — never captured from the interpreter itself.
//!    Independence is the point: a systematic oracle bug must not be able to make
//!    the expectation agree with it.
//!
//! 2. **Structural drift guard.** Recompile the same shader to SPIR-V and assert its
//!    [`IoLayout`] consumes the SAME inputs (buffers / interpolant Locations) and
//!    exports the SAME outputs (targets / Locations) the oracle reads / writes —
//!    same semantic locations, same channel widths, same push constants. This is the
//!    headless drift guard: the two sides must agree on the interface even where a
//!    CPU cannot re-execute the SPIR-V (that is the GPU tier's job). Every recompiled
//!    module is also `spirv-val`'d (Vulkan 1.1 portability floor).
//!
//! This deliberately does NOT build a CPU SPIR-V executor — value-level agreement on
//! a real device is the GPU tier. Here we prove the oracle is correct against
//! independent math, and that the recompiler's declared interface cannot drift from
//! what the oracle reads and writes.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use ps4_core::memory::{MemoryProtection, VirtualMemoryManager};
use ps4_gcn::{
    ExportRecord, ExportTarget, IoLayout, IoRole, LaunchAbi, PixelLaunch, PsInputs, ShaderStage,
    WAVE_SIZE, decode_all, recompile, run,
};

// ---- corpus enumeration (data-driven) --------------------------------------

fn corpus_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus")
}

/// Every corpus shader on disk: `(base_name, stage)`, derived from the committed
/// `.s` files. The stage comes from the filename suffix the corpus already uses
/// (`_vs` → vertex, `_ps` → fragment) so adding a shader needs no code change here.
/// A `.s` without a matching `.code.bin` is skipped (source-only entries are not
/// runnable), and an ambiguous suffix fails loudly rather than guessing.
fn enumerate_corpus() -> Vec<(String, ShaderStage)> {
    let dir = corpus_dir();
    let mut out = Vec::new();
    let entries = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("read corpus dir {}: {e}", dir.display()));
    for entry in entries {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("s") {
            continue;
        }
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .expect("utf8 corpus name")
            .to_string();
        // Skip a source-only shader with no assembled code (nothing to run/recompile).
        if !dir.join(format!("{name}.code.bin")).exists() {
            continue;
        }
        // A `fetch_*` corpus entry is a fetch-SHADER subroutine (a callee the main VS
        // jumps to, ending in `s_setpc` not `s_endpgm`, with no position export). It is
        // exercised by the fetch-shader parser (`fetch_shader::tests`), not the
        // recompile/interp differential harness — recompiling it as a standalone VS is
        // meaningless. Skip it here.
        if name.starts_with("fetch_") {
            continue;
        }
        let stage = stage_from_name(&name);
        out.push((name, stage));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    assert!(
        !out.is_empty(),
        "corpus enumeration found no runnable shaders"
    );
    out
}

/// Derive the pipeline stage from the corpus naming convention. Explicit and total:
/// an unrecognized suffix is a corpus-authoring error, not a silent default.
fn stage_from_name(name: &str) -> ShaderStage {
    if name.ends_with("_vs") {
        ShaderStage::Vertex
    } else if name.ends_with("_ps") {
        ShaderStage::Fragment
    } else {
        panic!("corpus shader {name:?} has no _vs/_ps stage suffix; cannot classify")
    }
}

fn read_code_dwords(name: &str) -> Vec<u32> {
    let p = corpus_dir().join(format!("{name}.code.bin"));
    let bytes = std::fs::read(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()));
    bytes
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

// ---- mock memory: the ONLY bytes the oracle may see ------------------------

/// A `Vec<u8>`-backed VMM serving one contiguous region at `base`; every host
/// (ambient) access is refused, so a passing run also proves the oracle read only
/// the bytes it was handed. Mirrors the mock in `tests/interp.rs`.
pub struct MockMem {
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
        _addr: u64,
        _size: usize,
        _prot: MemoryProtection,
        _name: Option<&str>,
    ) -> Result<u64, &'static str> {
        Err("unsupported")
    }
    fn unmap(&mut self, _addr: u64, _size: usize) -> Result<(), &'static str> {
        Err("unsupported")
    }
    fn protect(
        &mut self,
        _addr: u64,
        _size: usize,
        _prot: MemoryProtection,
    ) -> Result<(), &'static str> {
        Err("unsupported")
    }
    unsafe fn get_host_ptr(&self, _addr: u64) -> Option<*mut u8> {
        None
    }
    fn find_free_region(&mut self, _size: usize) -> u64 {
        0
    }
    fn is_memory_free(&self, _addr: u64, _size: usize) -> bool {
        false
    }
    fn read_bytes(&self, addr: u64, size: usize) -> Result<Vec<u8>, &'static str> {
        let start = addr.checked_sub(self.base).ok_or("read below mock base")? as usize;
        let end = start.checked_add(size).ok_or("read size overflow")?;
        if end > self.data.len() {
            return Err("read past end of mock buffer");
        }
        Ok(self.data[start..end].to_vec())
    }
}

fn push_u32(buf: &mut Vec<u8>, v: u32) {
    buf.extend_from_slice(&v.to_le_bytes());
}
fn push_f32(buf: &mut Vec<u8>, v: f32) {
    buf.extend_from_slice(&v.to_bits().to_le_bytes());
}

/// Build a mock holding a 128-bit vertex-buffer V# (base, stride 16, num_records)
/// followed by the vec4 vertex data. Returns `(mem, desc_addr)` where `desc_addr`
/// is what `s[2:3]` points at. Shared by the analytic VS spec and any GPU-tier
/// reader that wants the identical bytes.
pub fn build_vs_memory(positions: &[[f32; 4]]) -> (MockMem, u64) {
    const BASE: u64 = 0x1_0000;
    const DESC_OFF: u64 = 0;
    const VB_OFF: u64 = 64;
    let vb_addr = BASE + VB_OFF;
    let stride: u32 = 16;

    let mut data = Vec::new();
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
            push_f32(&mut data, *c);
        }
    }
    (MockMem::new(BASE, data), BASE + DESC_OFF)
}

/// Build a mock holding a `w`×`h` linear R8G8B8A8 texture at a 256-byte-aligned base,
/// plus the T# (256-bit) and S# (128-bit) SGPR words that address it. `texels` is
/// row-major RGBA bytes. Returns `(mem, t_sharp, s_sharp)` where the SGPR-word arrays
/// go straight into `user_sgprs` at s0.. (T#) and s8.. (S#). `bilinear` sets the S#
/// filter bit. The T#/S# bit layout here MUST match the interpreter's `decode_t_sharp`
/// / `decode_s_sharp` — it is a HAND-LAID hardware descriptor, not read back from the
/// decoder under test.
fn build_texture_memory(
    w: u32,
    h: u32,
    texels: &[u8],
    bilinear: bool,
) -> (MockMem, [u32; 8], [u32; 4]) {
    const BASE: u64 = 0x2_0000; // 256-byte aligned (low 8 bits zero)
    assert_eq!(texels.len(), (w * h * 4) as usize, "texel byte count");
    assert_eq!(
        BASE & 0xFF,
        0,
        "T# base must be 256-byte aligned (word0 = base>>8)"
    );

    let data = texels.to_vec();

    // T#: word0 = base>>8; word1[25:20]=dfmt, word1[29:26]=nfmt (unused by the sampler
    // subset but stamped for realism); word2[13:0]=width-1, word2[27:14]=height-1;
    // word3[22:20]=tiling index (0 = linear). Remaining words zero.
    let dfmt: u32 = 10; // FORMAT_8_8_8_8
    let nfmt: u32 = 0; // UNORM
    let mut t = [0u32; 8];
    t[0] = (BASE >> 8) as u32;
    t[1] = (dfmt << 20) | (nfmt << 26);
    t[2] = (w - 1) | ((h - 1) << 14);
    t[3] = 0; // tiling index 0 = linear

    // S#: word2[20] = filter select (0 = point, 1 = bilinear). Everything else zero.
    let mut s = [0u32; 4];
    if bilinear {
        s[2] = 1 << 20;
    }

    (MockMem::new(BASE, data), t, s)
}

// ---- per-shader analytic spec ----------------------------------------------

/// One expected export the oracle must produce for a lane, computed by hand from
/// the shader math (never captured from the interpreter).
struct ExpectedExport {
    lane: usize,
    target: ExportTarget,
    values: [f32; 4],
}

/// The synthetic launch a shader is driven with, plus the analytic exports the
/// oracle must then produce. This is the OPT-IN analytic layer: a corpus shader
/// without a spec still gets the structural + spirv-val checks (so adding a `.s`
/// needs no harness change), but a spec pins the exact numeric behavior of the
/// oracle against independent math.
struct ShaderSpec {
    /// Builds the launch ABI and the mock memory it reads (memory outlives the run).
    build_launch: fn() -> (LaunchAbi, MockMem),
    /// The exports the oracle must produce, hand-computed.
    expected: Vec<ExpectedExport>,
}

/// Registry of analytic specs, keyed by corpus base name. Extend by adding an entry;
/// a shader absent here is exercised only structurally + validated.
fn analytic_specs() -> BTreeMap<&'static str, ShaderSpec> {
    let mut m = BTreeMap::new();

    // passthrough_vs: fetches a vec4 position by gl_VertexIndex and exports it as
    // BOTH pos0 and param0. So for each vertex, both exports must equal that vertex's
    // input position verbatim — the identity of a pass-through. Analytic = the input.
    m.insert(
        "passthrough_vs",
        ShaderSpec {
            build_launch: || {
                let positions = [
                    [0.0f32, 1.0, 0.0, 1.0],
                    [-1.0, -1.0, 0.0, 1.0],
                    [1.0, -1.0, 0.0, 1.0],
                ];
                let (mem, desc_addr) = build_vs_memory(&positions);
                let abi = LaunchAbi::Vertex {
                    user_sgprs: vec![0, 0, desc_addr as u32, (desc_addr >> 32) as u32],
                    first_vertex: 0,
                    num_lanes: positions.len(),
                };
                (abi, mem)
            },
            expected: {
                let positions = [
                    [0.0f32, 1.0, 0.0, 1.0],
                    [-1.0, -1.0, 0.0, 1.0],
                    [1.0, -1.0, 0.0, 1.0],
                ];
                let mut e = Vec::new();
                for (v, p) in positions.iter().enumerate() {
                    e.push(ExpectedExport {
                        lane: v,
                        target: ExportTarget::Pos(0),
                        values: *p,
                    });
                    e.push(ExpectedExport {
                        lane: v,
                        target: ExportTarget::Param(0),
                        values: *p,
                    });
                }
                e
            },
        },
    );

    // flat_color_ps: unconditionally exports the constant RGBA the shader moves into
    // v0..v3 — (1.0, 0.25, 0.5, 1.0). Analytic = read straight off the .s literals.
    m.insert(
        "flat_color_ps",
        ShaderSpec {
            build_launch: || {
                let abi = LaunchAbi::Pixel(Box::new(PixelLaunch {
                    user_sgprs: vec![],
                    inputs: PsInputs::default(),
                    bary_i: [0.0; WAVE_SIZE],
                    bary_j: [0.0; WAVE_SIZE],
                    exec: 0b101, // lanes 0 and 2 live, lane 1 masked out
                }));
                (abi, MockMem::new(0, Vec::new()))
            },
            expected: vec![
                ExpectedExport {
                    lane: 0,
                    target: ExportTarget::Mrt(0),
                    values: [1.0, 0.25, 0.5, 1.0],
                },
                ExpectedExport {
                    lane: 2,
                    target: ExportTarget::Mrt(0),
                    values: [1.0, 0.25, 0.5, 1.0],
                },
            ],
        },
    );

    // interp_color_ps: interpolates attr0.xyz with the plane equation and exports
    // (r,g,b,1.0). The analytic values below are computed BY HAND — deliberately NOT
    // via the plane formula the oracle uses — with exact-in-f32 inputs and I=J=0.5:
    //   R: P=[0.25,0.75,1.25] → 0.25 + 0.5·0.5 + 0.5·1.0 = 1.0
    //   G: P=[1.0, 0.5, 0.0 ] → 1.0  + 0.5·(-0.5) + 0.5·(-1.0) = 0.25
    //   B: P=[0.0, 0.0, 2.0 ] → 0.0  + 0.5·0.0 + 0.5·2.0 = 1.0
    //   A = 1.0 (v_mov 1.0).
    // Every step is exact in f32, so the compare below is bit-exact.
    m.insert(
        "interp_color_ps",
        ShaderSpec {
            build_launch: || {
                let planes: [[f32; 3]; 4] = [
                    [0.25, 0.75, 1.25],
                    [1.0, 0.5, 0.0],
                    [0.0, 0.0, 2.0],
                    [0.0, 0.0, 0.0],
                ];
                let mut bary_i = [0.0f32; WAVE_SIZE];
                let mut bary_j = [0.0f32; WAVE_SIZE];
                bary_i[0] = 0.5;
                bary_j[0] = 0.5;
                let abi = LaunchAbi::Pixel(Box::new(PixelLaunch {
                    user_sgprs: vec![],
                    inputs: PsInputs {
                        attr_planes: vec![planes],
                    },
                    bary_i,
                    bary_j,
                    exec: 0b1,
                }));
                (abi, MockMem::new(0, Vec::new()))
            },
            expected: vec![ExpectedExport {
                lane: 0,
                target: ExportTarget::Mrt(0),
                values: [1.0, 0.25, 1.0, 1.0],
            }],
        },
    );

    // texture_sample_ps: interpolates a UV (attr0.xy), samples a 2×2 linear RGBA8
    // texture with POINT filtering, exports the sampled RGBA to mrt0. The UV plane is
    // constant (P0=P1=P2), so interpolation yields (0.75, 0.25) regardless of the
    // barycentrics — a stable sample point. Point-sample maps UV to texel space:
    //   fx = 0.75 * 2 = 1.5 → floor = texel x 1
    //   fy = 0.25 * 2 = 0.5 → floor = texel y 0
    // so it reads texel (1, 0). The texture below places (102,204,51,255) there; the
    // expected export is those bytes / 255 — reasoned FROM the texture, not captured
    // from the sampler under test.
    m.insert(
        "texture_sample_ps",
        ShaderSpec {
            build_launch: || {
                // 2×2 texels, row-major RGBA: (0,0)=black, (1,0)=the target color,
                // (0,1)/(1,1)=other distinct colors so a wrong texel choice is visible.
                #[rustfmt::skip]
                let texels: [u8; 16] = [
                    0,   0,   0,   255,   // (0,0)
                    102, 204, 51,  255,   // (1,0)  ← point-sampled target
                    10,  20,  30,  255,   // (0,1)
                    200, 100, 150, 255,   // (1,1)
                ];
                let (mem, t, s) = build_texture_memory(2, 2, &texels, false);
                // user_sgprs: s0..s7 = T#, s8..s11 = S#. s0 is also the interp base the
                // shader moves into m0 (unused by the oracle's attr-field interpolation).
                let mut user = Vec::new();
                user.extend_from_slice(&t);
                user.extend_from_slice(&s);
                // Constant UV planes: interpolation returns P0 regardless of I/J.
                let planes: [[f32; 3]; 4] = [
                    [0.75, 0.75, 0.75], // attr0.x = u
                    [0.25, 0.25, 0.25], // attr0.y = v
                    [0.0, 0.0, 0.0],
                    [0.0, 0.0, 0.0],
                ];
                let abi = LaunchAbi::Pixel(Box::new(PixelLaunch {
                    user_sgprs: user,
                    inputs: PsInputs {
                        attr_planes: vec![planes],
                    },
                    bary_i: [0.5; WAVE_SIZE],
                    bary_j: [0.5; WAVE_SIZE],
                    exec: 0b1, // lane 0 live
                }));
                (abi, mem)
            },
            expected: vec![ExpectedExport {
                lane: 0,
                target: ExportTarget::Mrt(0),
                values: [102.0 / 255.0, 204.0 / 255.0, 51.0 / 255.0, 1.0],
            }],
        },
    );

    m
}

// ---- AC #1 + #3: oracle vs analytic expectation, data-driven ---------------

/// For every corpus shader that has a registered analytic spec, run the oracle over
/// the synthetic launch and assert each captured export equals the hand-computed
/// value. Enumerated over the corpus directory + keyed by name, so a new `.s` (with
/// a spec) is picked up automatically; a new `.s` without a spec is exercised by the
/// structural test below without touching this harness.
#[test]
fn oracle_matches_analytic_expectation() {
    let specs = analytic_specs();
    let corpus = enumerate_corpus();

    let mut checked_any = false;
    for (name, _stage) in &corpus {
        let Some(spec) = specs.get(name.as_str()) else {
            eprintln!("{name}: no analytic spec — structural + spirv-val only");
            continue;
        };
        checked_any = true;

        let code = read_code_dwords(name);
        let insts = decode_all(&code);
        let (abi, mem) = (spec.build_launch)();
        let exports = run(&insts, abi, &mem).unwrap_or_else(|e| panic!("{name}: oracle run: {e}"));

        for want in &spec.expected {
            let got = find_export(&exports, want.lane, want.target).unwrap_or_else(|| {
                panic!(
                    "{name}: oracle produced no export for lane {} target {:?}",
                    want.lane, want.target
                )
            });
            assert_eq!(
                got.values, want.values,
                "{name}: lane {} {:?} — oracle {:?} != analytic {:?}",
                want.lane, want.target, got.values, want.values
            );
        }

        // A masked-off / absent lane must not export where the spec has none — guards
        // the oracle against fabricating exports from dead lanes.
        assert_export_coverage(name, &exports, &spec.expected);
    }
    assert!(
        checked_any,
        "no corpus shader had an analytic spec — the oracle side ran nothing"
    );
}

/// Every export the oracle produced must correspond to an expected one (no phantom
/// exports from lanes the spec did not list). This catches an oracle that leaks
/// exports from masked-off lanes.
fn assert_export_coverage(name: &str, exports: &[ExportRecord], expected: &[ExpectedExport]) {
    for e in exports {
        let listed = expected
            .iter()
            .any(|w| w.lane == e.lane && w.target == e.target);
        assert!(
            listed,
            "{name}: unexpected export lane {} target {:?} (values {:?}) not in the analytic spec",
            e.lane, e.target, e.values
        );
    }
}

fn find_export(
    exports: &[ExportRecord],
    lane: usize,
    target: ExportTarget,
) -> Option<&ExportRecord> {
    exports
        .iter()
        .find(|e| e.lane == lane && e.target == target)
}

// ---- AC #1 + #3: structural drift guard, data-driven -----------------------

/// The recompiled module's declared interface must consume the same inputs and
/// export the same outputs the oracle reads / writes — same semantic locations,
/// widths, and push constants. Data-driven over the whole corpus: EVERY shader is
/// checked (no spec needed), so a newly-added `.s` is guarded automatically.
///
/// This is the headless drift guard. It does not execute the SPIR-V (the GPU tier
/// does that on a device); it proves the two sides cannot disagree on the interface.
#[test]
fn recompiled_layout_matches_oracle_semantics() {
    for (name, stage) in enumerate_corpus() {
        let code = read_code_dwords(&name);
        let insts = decode_all(&code);
        let recompiled =
            recompile(&insts, stage).unwrap_or_else(|e| panic!("{name}: recompile: {e}"));

        // Derive what the ORACLE reads/writes straight from the decoded stream — the
        // same instructions the interpreter walks — and compare to the recompiler's
        // declared IoLayout. Both sides consume the identical `Decoded` list, so a
        // divergence is a genuine interface drift, not a difference of inputs.
        let oracle = OracleInterface::scan(&insts, stage);
        assert_layout_agrees(&name, stage, &oracle, &recompiled.io);
    }
}

/// The semantic interface the ORACLE touches, recovered from the decoded stream
/// exactly as the interpreter would act on it: which EXP targets it writes, which
/// interpolant attributes (and max channel per attr) it reads, and whether it
/// performs a buffer fetch. Independent of the recompiler's own bookkeeping.
struct OracleInterface {
    /// Position export present (VS `exp pos0`).
    exports_position: bool,
    /// `location → max channel index written` for VS `param<n>` / PS `mrt<n>` exports.
    output_locations: BTreeMap<u32, u32>,
    /// `attr → max channel index read` across all VINTRP reads (PS interpolants).
    input_attrs: BTreeMap<u8, u32>,
    /// A MUBUF buffer fetch occurs (VS vertex fetch).
    has_buffer_fetch: bool,
    /// A robust-buffer clamp against `num_records` occurs — the oracle reads it from
    /// the V#; the recompiler must declare a `num_records` push constant for it.
    needs_num_records: bool,
    /// An `image_sample` occurs (PS texture sample) — the recompiler must declare a
    /// combined image-sampler binding for it.
    has_texture_sample: bool,
}

impl OracleInterface {
    fn scan(insts: &[ps4_gcn::Decoded], _stage: ShaderStage) -> Self {
        use ps4_gcn::Inst;
        let mut o = OracleInterface {
            exports_position: false,
            output_locations: BTreeMap::new(),
            input_attrs: BTreeMap::new(),
            has_buffer_fetch: false,
            needs_num_records: false,
            has_texture_sample: false,
        };
        for d in insts {
            match &d.inst {
                Inst::Exp { target, srcs, .. } => {
                    let hi_chan = highest_enabled_channel(srcs);
                    match target {
                        ExportTarget::Pos(0) => o.exports_position = true,
                        ExportTarget::Param(n) | ExportTarget::Mrt(n) => {
                            let e = o.output_locations.entry(*n as u32).or_insert(0);
                            *e = (*e).max(hi_chan);
                        }
                        _ => {}
                    }
                }
                Inst::Vintrp { attr, chan, .. } => {
                    let e = o.input_attrs.entry(*attr).or_insert(0);
                    *e = (*e).max((*chan & 0x3) as u32);
                }
                Inst::Mubuf { .. } => {
                    o.has_buffer_fetch = true;
                    // The oracle clamps the fetch index against the V#'s num_records
                    // (robust-buffer behavior), so the recompiler must supply it.
                    o.needs_num_records = true;
                }
                Inst::Mimg { .. } => {
                    // image_sample: the oracle samples a texture through a T#/S#; the
                    // recompiler must declare a combined image-sampler binding for it.
                    o.has_texture_sample = true;
                }
                _ => {}
            }
        }
        o
    }
}

/// Highest enabled channel index (0-based) in an EXP's four source slots, or 0 if
/// only channel 0 is enabled. Mirrors how the oracle records a disabled channel as
/// 0.0 but still writes the vec; the recompiler declares a vec4 with `components`
/// = channels-used.
fn highest_enabled_channel(srcs: &[Option<ps4_gcn::Operand>; 4]) -> u32 {
    let mut hi = 0u32;
    for (ch, slot) in srcs.iter().enumerate() {
        if slot.is_some() {
            hi = ch as u32;
        }
    }
    hi
}

/// Assert the recompiler's [`IoLayout`] agrees with the oracle's semantic interface:
/// same position export, same output Locations (with matching channel-width), same
/// interpolant input Locations (with matching channel-width), same buffer-fetch
/// presence, same num_records push constant. A mismatch is exactly the interp /
/// recompiler drift this guard exists to catch.
fn assert_layout_agrees(name: &str, stage: ShaderStage, oracle: &OracleInterface, io: &IoLayout) {
    assert_eq!(io.stage, stage, "{name}: recompiler stage mismatch");

    // Position export.
    assert_eq!(
        io.exports_position, oracle.exports_position,
        "{name}: exports_position — recompiler {} vs oracle {}",
        io.exports_position, oracle.exports_position
    );

    // Output Locations: the recompiler must emit exactly the Locations the oracle
    // exports to (param<n>/mrt<n>), and its `components` (channels-used) must cover
    // the highest channel the oracle writes.
    let rc_outputs: BTreeMap<u32, (u32, IoRole)> = io
        .outputs
        .iter()
        .map(|v| (v.location, (v.components, v.role)))
        .collect();
    assert_eq!(
        rc_outputs.keys().copied().collect::<Vec<_>>(),
        oracle.output_locations.keys().copied().collect::<Vec<_>>(),
        "{name}: output Locations — recompiler {:?} vs oracle {:?}",
        rc_outputs.keys().collect::<Vec<_>>(),
        oracle.output_locations.keys().collect::<Vec<_>>(),
    );
    for (loc, hi_chan) in &oracle.output_locations {
        let (components, _role) = rc_outputs[loc];
        // The recompiler declares vec4 outputs; `components` = channels-used, which
        // must be at least the highest channel the oracle writes (+1).
        assert!(
            components > *hi_chan,
            "{name}: output Location {loc} — recompiler components {components} \
             does not cover oracle's channel {hi_chan}"
        );
    }

    // Interpolant input Locations (PS): the recompiler coalesces attr<n>.chan reads
    // into one vec4 Input per Location; the oracle reads the same attr/chan planes.
    let rc_inputs: BTreeMap<u32, u32> = io
        .inputs
        .iter()
        .map(|v| (v.location, v.components))
        .collect();
    let oracle_inputs: BTreeMap<u32, u32> = oracle
        .input_attrs
        .iter()
        .map(|(a, hi)| (*a as u32, *hi))
        .collect();
    assert_eq!(
        rc_inputs.keys().copied().collect::<Vec<_>>(),
        oracle_inputs.keys().copied().collect::<Vec<_>>(),
        "{name}: interpolant input Locations — recompiler {:?} vs oracle {:?}",
        rc_inputs.keys().collect::<Vec<_>>(),
        oracle_inputs.keys().collect::<Vec<_>>(),
    );
    for (loc, hi_chan) in &oracle_inputs {
        let components = rc_inputs[loc];
        assert!(
            components > *hi_chan,
            "{name}: input Location {loc} — recompiler components {components} \
             does not cover oracle's channel {hi_chan}"
        );
    }

    // Buffer fetch: presence must match (a VS fetch → exactly one binding).
    assert_eq!(
        !io.buffers.is_empty(),
        oracle.has_buffer_fetch,
        "{name}: buffer fetch — recompiler {} bindings vs oracle has_fetch {}",
        io.buffers.len(),
        oracle.has_buffer_fetch,
    );

    // num_records push constant: the oracle's robust-buffer clamp reads it from the
    // V#; the recompiler must declare it as a push constant, or every GPU fetch
    // silently clamps to element 0 (invisible to spirv-val and the CPU oracle).
    assert_eq!(
        io.uses_num_records(),
        oracle.needs_num_records,
        "{name}: num_records push constant — recompiler {} vs oracle {}",
        io.uses_num_records(),
        oracle.needs_num_records,
    );

    // Texture sample: an oracle `image_sample` must correspond to exactly one declared
    // combined image-sampler binding on the recompiler side (and none otherwise). This
    // is the sampling half of the interp/recompiler interface guard.
    assert_eq!(
        !io.samplers.is_empty(),
        oracle.has_texture_sample,
        "{name}: texture sampler — recompiler {} bindings vs oracle has_sample {}",
        io.samplers.len(),
        oracle.has_texture_sample,
    );
    if oracle.has_texture_sample {
        assert_eq!(
            io.samplers.len(),
            1,
            "{name}: a single image_sample must declare exactly one sampler binding"
        );
    }
}

// ---- AC #1: every recompiled module passes spirv-val -----------------------

/// Locate `spirv-val`, or `None` (test skips cleanly if absent).
fn spirv_val() -> Option<PathBuf> {
    for cand in ["/usr/bin/spirv-val", "spirv-val"] {
        if Command::new(cand).arg("--version").output().is_ok() {
            return Some(PathBuf::from(cand));
        }
    }
    None
}

fn unique_spv_path(tag: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    std::env::temp_dir().join(format!("unemups4_diff_{tag}_{pid}_{n}.spv"))
}

fn words_to_bytes(words: &[u32]) -> Vec<u8> {
    let mut b = Vec::with_capacity(words.len() * 4);
    for w in words {
        b.extend_from_slice(&w.to_le_bytes());
    }
    b
}

/// Every enumerated corpus shader recompiles to a module that passes `spirv-val`
/// (Vulkan 1.1 portability floor) — part of the data-driven enumeration, so a new
/// `.s` is validated automatically.
#[test]
fn corpus_recompiles_and_validates() {
    let Some(val) = spirv_val() else {
        eprintln!("spirv-val not found; skipping recompiled-module validation");
        return;
    };
    for (name, stage) in enumerate_corpus() {
        let code = read_code_dwords(&name);
        let insts = decode_all(&code);
        let recompiled =
            recompile(&insts, stage).unwrap_or_else(|e| panic!("{name}: recompile: {e}"));
        let bytes = words_to_bytes(&recompiled.spirv);
        let path = unique_spv_path(&name);
        std::fs::write(&path, &bytes).expect("write spv");
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
