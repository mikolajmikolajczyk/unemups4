//! Differential harness — TIER (a), headless (doc-2 §1, decision-3, decision-6).
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

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use ps4_core::memory::{MemoryProtection, VirtualMemoryManager};
use ps4_gcn::{
    DescriptorSource, ExportRecord, ExportTarget, IoLayout, IoRole, LaunchAbi, PixelLaunch,
    PsInputs, ShaderStage, WAVE_SIZE, decode_all, has_fetch_call, recompile, resolve_fetch_call,
    run,
};

// The CPU SPIR-V value evaluator (task-122). Placed in a `tests/spirv_eval/` subdir
// so Rust does NOT compile it as its own standalone integration-test binary; it is
// pulled in here as a module and reused by `recompiled_spirv_matches_oracle`.
mod spirv_eval;

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

/// The fetch-shader callee a fetch-CALLER corpus VS jumps to via `s_swappc_b64`.
/// A caller (e.g. `inline_fetch_vs`) has no runnable stream on its own — its call
/// must be resolved against the callee's fetch body before the oracle/recompiler
/// see it. Adding a new fetch-caller means adding one row here.
fn fetch_callee_for(name: &str) -> &'static str {
    match name {
        "inline_fetch_vs" => "fetch_pos_vs",
        "inline_multi_fetch_vs" => "fetch_vs",
        other => panic!(
            "corpus shader {other:?} has an s_swappc_b64 fetch call but no registered fetch \
             callee — add it to fetch_callee_for"
        ),
    }
}

/// Decode a corpus shader and, if it calls a fetch shader (`s_swappc_b64`), resolve
/// that call by inlining its registered fetch callee — yielding the straight-line
/// stream the oracle ([`run`]) and recompiler ([`recompile`]) consume. A
/// self-contained shader is returned decoded, unchanged. This is the single seam the
/// whole harness routes decoding through so every test (oracle, drift, spirv-val)
/// sees the resolved fetch call identically.
fn runnable_insts(name: &str) -> Vec<ps4_gcn::Decoded> {
    let main = decode_all(&read_code_dwords(name));
    if !has_fetch_call(&main) {
        return main;
    }
    let fetch = decode_all(&read_code_dwords(fetch_callee_for(name)));
    resolve_fetch_call(&main, &fetch).unwrap_or_else(|e| {
        panic!(
            "{name}: resolve fetch call against {}: {e}",
            fetch_callee_for(name)
        )
    })
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
    /// The region base — the guest address `data[0]` lives at. Used by the CPU
    /// SPIR-V evaluator harness to reconstruct SSBO/texture contents from the SAME
    /// bytes the oracle reads.
    fn region_base(&self) -> u64 {
        self.base
    }
    /// The raw backing bytes.
    fn region_data(&self) -> &[u8] {
        &self.data
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
    // word3: IDENTITY dst_sel [4,5,6,7] (0xFAC) in bits [11:0] — a pure raw passthrough
    // (channel ch → source ch), so the SPIR-V's per-channel swizzle apply matches the
    // interp's raw read exactly (task-155). num_/data-format bits are unused by the fetch.
    push_u32(&mut data, ps4_gcn::DST_SEL_IDENTITY);
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

/// Like [`build_vs_memory`] but the vertex data is a packed `_8_8_8_8` UNORM color — ONE
/// packed dword per vertex (r|g<<8|b<<16|a<<24), padded to a 16-byte element — and word3
/// carries `dfmt` 10 (`_8_8_8_8`) / `nfmt` 0 (UNORM) so the format-aware fetch unpacks each
/// byte to a normalized float (task-164). This is the shape of Celeste's SpriteBatch vertex
/// color. Identity dst_sel (raw passthrough) so the swizzle is a no-op and only the format
/// unpack is under test. The 16-byte element keeps every 8-/16-bit candidate dword the fetch
/// reads in-range (the recompiler evaluates all format candidates, discarding the unselected).
pub fn build_vs_memory_rgba8_unorm(colors: &[[u8; 4]]) -> (MockMem, u64) {
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
    push_u32(&mut data, colors.len() as u32);
    // word3: identity dst_sel [11:0], nfmt 0 (unorm) [14:12], dfmt 10 (_8_8_8_8) [18:15].
    let w3 = ps4_gcn::DST_SEL_IDENTITY | (10u32 << 15);
    push_u32(&mut data, w3);
    while data.len() < VB_OFF as usize {
        data.push(0);
    }
    for c in colors {
        let packed =
            c[0] as u32 | ((c[1] as u32) << 8) | ((c[2] as u32) << 16) | ((c[3] as u32) << 24);
        push_u32(&mut data, packed);
        data.resize(data.len() + 12, 0); // pad to a 16-byte element
    }
    (MockMem::new(BASE, data), BASE + DESC_OFF)
}

/// Like [`build_vs_memory`] but with an ARBITRARY per-vertex byte stride and a
/// per-vertex component count (`comps` f32s laid out, then `stride_bytes - comps*4`
/// padding bytes to the next vertex). Used by the `nonstd_stride_vs` spec (task-140)
/// to prove a non-16 stride (12/24/32…) renders correctly with the stride flowing in as
/// the SPIR-V PUSH CONSTANT — NOT baked. `stride_bytes` MUST be 4-byte aligned and
/// `>= comps*4`. The V# records the real stride so both oracle sides read it.
pub fn build_vs_memory_strided(
    verts: &[Vec<f32>],
    stride_bytes: u32,
    comps: usize,
) -> (MockMem, u64) {
    const BASE: u64 = 0x1_0000;
    const DESC_OFF: u64 = 0;
    const VB_OFF: u64 = 64;
    assert!(
        stride_bytes.is_multiple_of(4),
        "stride must be 4-byte aligned"
    );
    assert!(stride_bytes as usize >= comps * 4, "stride must hold comps");
    let vb_addr = BASE + VB_OFF;

    let mut data = Vec::new();
    push_u32(&mut data, (vb_addr & 0xFFFF_FFFF) as u32);
    push_u32(
        &mut data,
        ((vb_addr >> 32) as u32 & 0xFFFF) | (stride_bytes << 16),
    );
    push_u32(&mut data, verts.len() as u32);
    // word3: IDENTITY dst_sel [4,5,6,7] (0xFAC) — a raw passthrough (task-155).
    push_u32(&mut data, ps4_gcn::DST_SEL_IDENTITY);
    while data.len() < VB_OFF as usize {
        data.push(0);
    }
    for v in verts {
        assert_eq!(v.len(), comps, "each vertex supplies `comps` components");
        let start = data.len();
        for c in v {
            push_f32(&mut data, *c);
        }
        // Pad to the next vertex at `stride_bytes` (a gap the tight vec4 fetch skips).
        while data.len() < start + stride_bytes as usize {
            data.push(0);
        }
    }
    (MockMem::new(BASE, data), BASE + DESC_OFF)
}

/// Build a mock for a fetch with a NON-ZERO MUBUF immediate offset (task-153 Bug 1). The
/// V# base points 8 bytes BEFORE the first vertex, and each vec4 element sits at
/// `base + index*16 + 8` — exactly where a fetch with `offset:8` reads. The V#'s
/// `num_records` is sized so the last valid element is in range. A recompiler that dropped
/// the immediate offset would read from `base + index*16` (8 bytes early → the prior
/// element's tail), producing wrong values, so an exact match witnesses the offset threads.
/// Returns `(mem, desc_addr)` for `s[2:3]`.
pub fn build_vs_memory_offset(positions: &[[f32; 4]], imm_offset: u32) -> (MockMem, u64) {
    const BASE: u64 = 0x1_0000;
    const VB_OFF: u64 = 64;
    let vb_addr = BASE + VB_OFF;
    let stride: u32 = 16;

    let mut data = Vec::new();
    push_u32(&mut data, (vb_addr & 0xFFFF_FFFF) as u32);
    push_u32(
        &mut data,
        ((vb_addr >> 32) as u32 & 0xFFFF) | (stride << 16),
    );
    // num_records must cover the last element the offset fetch reads: element `n-1` sits at
    // byte `imm_offset + (n-1)*stride`, i.e. within record index `(imm_offset)/stride + n`.
    // Report a generous count so no fetch clamps (the mock buffer is sized to match).
    let extra = imm_offset.div_ceil(stride);
    push_u32(&mut data, positions.len() as u32 + extra);
    push_u32(&mut data, ps4_gcn::DST_SEL_IDENTITY);
    while data.len() < VB_OFF as usize {
        data.push(0);
    }
    // `imm_offset` bytes of prefix, then the vec4 vertex data.
    data.resize(data.len() + imm_offset as usize, 0);
    for p in positions {
        for c in p {
            push_f32(&mut data, *c);
        }
    }
    (MockMem::new(BASE, data), BASE)
}

/// Build a mock for a TWO-STREAM fetch (task-153): a descriptor set holding two distinct
/// 128-bit V# — V#0 at byte offset 0 (attr0, a vec4) and V#1 at byte offset 16 (attr1, a
/// vec2) — each pointing at its OWN buffer with its OWN base, num_records, and contents.
/// This is the Celeste-shaped interleaved/multi-V# fetch: a recompiler that collapsed both
/// attributes onto one binding (the pre-task-153 bug) would fetch attr1 from V#0's buffer
/// and produce the WRONG param0, so an exact interp==recompile match witnesses that each
/// stream reaches its own V#. Returns `(mem, desc_addr)` for `s[2:3]`.
pub fn build_two_stream_memory(attr0: &[[f32; 4]], attr1: &[[f32; 2]]) -> (MockMem, u64) {
    const BASE: u64 = 0x1_0000;
    // Two 16-byte V# descriptors at offsets 0 and 16, then each stream's data.
    const VB0_OFF: u64 = 64;
    const VB1_OFF: u64 = 512;
    let vb0_addr = BASE + VB0_OFF;
    let vb1_addr = BASE + VB1_OFF;
    let stride0: u32 = 16; // vec4
    let stride1: u32 = 8; // vec2

    let mut data = Vec::new();
    // V#0 (attr0) at offset 0.
    push_u32(&mut data, (vb0_addr & 0xFFFF_FFFF) as u32);
    push_u32(
        &mut data,
        ((vb0_addr >> 32) as u32 & 0xFFFF) | (stride0 << 16),
    );
    push_u32(&mut data, attr0.len() as u32);
    push_u32(&mut data, ps4_gcn::DST_SEL_IDENTITY);
    // V#1 (attr1) at offset 16 — a DIFFERENT base + num_records.
    push_u32(&mut data, (vb1_addr & 0xFFFF_FFFF) as u32);
    push_u32(
        &mut data,
        ((vb1_addr >> 32) as u32 & 0xFFFF) | (stride1 << 16),
    );
    push_u32(&mut data, attr1.len() as u32);
    push_u32(&mut data, ps4_gcn::DST_SEL_IDENTITY);
    // Pad to attr0's buffer.
    while data.len() < VB0_OFF as usize {
        data.push(0);
    }
    for p in attr0 {
        for c in p {
            push_f32(&mut data, *c);
        }
    }
    // Pad to attr1's buffer.
    while data.len() < VB1_OFF as usize {
        data.push(0);
    }
    for p in attr1 {
        for c in p {
            push_f32(&mut data, *c);
        }
    }
    (MockMem::new(BASE, data), BASE)
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

/// Build a mock holding an RGBA constant buffer (`values`, as four f32 dwords) at a
/// fixed base, plus the 128-bit V# descriptor (4 SGPR words) that addresses it. The
/// V# words go straight into `user_sgprs` at s0..s3 (the SBASE an s_buffer_load
/// reads). The bit layout MUST match the interpreter's `decode_v_sharp`. Returns
/// `(mem, v_sharp)`.
fn build_cbuffer_memory(values: [f32; 4]) -> (MockMem, [u32; 4]) {
    const BASE: u64 = 0x2_0000;
    let mut data = Vec::new();
    for v in values {
        push_f32(&mut data, v);
    }
    // word0 = base[31:0]; word1 = base[47:32] | (stride<<16); word2 = num_records;
    // word3 = format/swizzle (unused). Stride is irrelevant to a scalar buffer load.
    let v_sharp = [
        (BASE & 0xFFFF_FFFF) as u32,
        (BASE >> 32) as u32 & 0xFFFF,
        4, // num_records (dwords) — not clamped by s_buffer_load
        0,
    ];
    (MockMem::new(BASE, data), v_sharp)
}

/// Build a mock holding a 16-dword constant block (`values`, 16 f32 dwords) at a
/// fixed base, plus the 128-bit V# descriptor (4 SGPR words) that addresses it —
/// for the `s_buffer_load_dwordx16` (4×4 matrix) load path. The V# words go into
/// `user_sgprs` at s4..s7 (the SBASE the corpus shader reads). The bit layout MUST
/// match the interpreter's `decode_v_sharp`. Returns `(mem, v_sharp)`.
fn build_cbuffer16_memory(values: [f32; 16]) -> (MockMem, [u32; 4]) {
    const BASE: u64 = 0x2_0000;
    let mut data = Vec::new();
    for v in values {
        push_f32(&mut data, v);
    }
    let v_sharp = [
        (BASE & 0xFFFF_FFFF) as u32,
        (BASE >> 32) as u32 & 0xFFFF,
        16, // num_records (dwords) — not clamped by s_buffer_load
        0,
    ];
    (MockMem::new(BASE, data), v_sharp)
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

    // index_tri_vs: derives clip-space position from the launch vertex index alone —
    // no vertex buffer, no fetch shader, `v0` read as a plain ALU source (task-184).
    // The oracle seeds `v0 = first_vertex + lane`; the recompiled module must read the
    // same index from `gl_VertexIndex`, or every lane collapses onto one point.
    // Analytic, from the shader math: x = f32(idx & 1)*2 - 1, y = f32(idx & ~1) - 1.
    m.insert(
        "index_tri_vs",
        ShaderSpec {
            build_launch: || {
                // The shader touches no memory; the mock exists only to satisfy the
                // harness (and to prove no ambient access happens).
                let mem = MockMem::new(0x1_0000, Vec::new());
                let abi = LaunchAbi::Vertex {
                    user_sgprs: Vec::new(),
                    // A non-zero first_vertex so a shader that ignored the index (or read
                    // only the lane number) could not pass by coincidence.
                    first_vertex: 1,
                    num_lanes: 3,
                };
                (abi, mem)
            },
            expected: {
                let mut e = Vec::new();
                for lane in 0..3usize {
                    let idx = 1 + lane as u32;
                    let p = [
                        (idx & 1) as f32 * 2.0 - 1.0,
                        (idx & !1) as f32 - 1.0,
                        0.0,
                        1.0,
                    ];
                    e.push(ExpectedExport {
                        lane,
                        target: ExportTarget::Pos(0),
                        values: p,
                    });
                    e.push(ExpectedExport {
                        lane,
                        target: ExportTarget::Param(0),
                        values: p,
                    });
                }
                e
            },
        },
    );

    // index_tri_inplace_vs: the same index-derived shape as index_tri_vs, but the second
    // read of `v0` is the source of an instruction whose destination is ALSO `v0`
    // (`v_and_b32 v0, -2, v0`). Every ALU emitter untracks the destination as a
    // launch-index carrier before evaluating its sources, so an unspilled untrack makes
    // that read return the zero initializer and pins Y to -1 for every lane — a zero-area
    // triangle. index_tri_vs cannot catch it (its second read writes a different VGPR),
    // which is why the first fix passed the suite and still rendered nothing (task-184).
    // Analytic, from the shader math, INTEGER: x = f32(i32((idx & 1) * 2 - 1)),
    // y = f32(i32((idx & ~1) - 1)); z = 0, w = 1; param0 exports the same vector.
    m.insert(
        "index_tri_inplace_vs",
        ShaderSpec {
            build_launch: || {
                // Touches no memory; the mock proves no ambient access happens.
                let mem = MockMem::new(0x1_0000, Vec::new());
                let abi = LaunchAbi::Vertex {
                    user_sgprs: Vec::new(),
                    // Non-zero so a shader that ignored the index cannot pass by chance.
                    first_vertex: 1,
                    num_lanes: 3,
                };
                (abi, mem)
            },
            expected: {
                let mut e = Vec::new();
                for lane in 0..3usize {
                    let idx = 1 + lane as u32;
                    let p = [
                        ((idx & 1) as i32 * 2 - 1) as f32,
                        ((idx & !1) as i32 - 1) as f32,
                        0.0,
                        1.0,
                    ];
                    e.push(ExpectedExport {
                        lane,
                        target: ExportTarget::Pos(0),
                        values: p,
                    });
                    e.push(ExpectedExport {
                        lane,
                        target: ExportTarget::Param(0),
                        values: p,
                    });
                }
                e
            },
        },
    );

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

    // inline_fetch_vs: the fetch-CALLER counterpart of passthrough_vs (task-113.4.2
    // AC #7). It calls a separate fetch shader (fetch_pos_vs) via s_swappc_b64; after
    // resolve_fetch_call inlines the fetch body, the fetch's idxen MUBUF loads the
    // per-vertex vec4 position into v[4:7], which this VS exports verbatim as BOTH
    // pos0 and param0 — a pass-through. So the analytic expectation is the input
    // position, EXACTLY as passthrough_vs, proving the resolved fetch call fetches the
    // right attribute into the VGPRs the main body reads. The fetch shader reads the
    // V# descriptor set from s[2:3] (loading the V# into s[8:11]); s[0:1] carries the
    // (now-inlined) fetch pointer and is unread after resolution.
    m.insert(
        "inline_fetch_vs",
        ShaderSpec {
            build_launch: || {
                let positions = [
                    [0.0f32, 1.0, 0.0, 1.0],
                    [-1.0, -1.0, 0.0, 1.0],
                    [1.0, -1.0, 0.0, 1.0],
                ];
                let (mem, desc_addr) = build_vs_memory(&positions);
                let abi = LaunchAbi::Vertex {
                    // s0/s1 = fetch pointer (inlined away); s2/s3 = V# descriptor set.
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

    // offset_fetch_vs (task-153 Bug 1): a pass-through VS whose buffer_load_format carries a
    // NON-ZERO MUBUF immediate offset (offset:8). The oracle reads base+index*stride+8; the
    // recompiler must thread the offset into the fetch address or it reads element 0's dword.
    // The vec4 data sits 8 bytes past the V# base, so an exact match on all lanes witnesses
    // the immediate offset threads into the recompiled fetch (a dropped offset reads garbage
    // from the prefix). Exports (x,y,z,w) verbatim as pos0 and param0.
    m.insert(
        "offset_fetch_vs",
        ShaderSpec {
            build_launch: || {
                let positions = [
                    [0.5f32, 1.0, 0.0, 1.0],
                    [-1.0, -0.5, 0.25, 1.0],
                    [1.0, -1.0, 0.75, 1.0],
                ];
                let (mem, desc_addr) = build_vs_memory_offset(&positions, 8);
                let abi = LaunchAbi::Vertex {
                    user_sgprs: vec![0, 0, desc_addr as u32, (desc_addr >> 32) as u32],
                    first_vertex: 0,
                    num_lanes: positions.len(),
                };
                (abi, mem)
            },
            expected: {
                let positions = [
                    [0.5f32, 1.0, 0.0, 1.0],
                    [-1.0, -0.5, 0.25, 1.0],
                    [1.0, -1.0, 0.75, 1.0],
                ];
                let mut e = Vec::new();
                for (lane, p) in positions.iter().enumerate() {
                    e.push(ExpectedExport {
                        lane,
                        target: ExportTarget::Pos(0),
                        values: *p,
                    });
                    e.push(ExpectedExport {
                        lane,
                        target: ExportTarget::Param(0),
                        values: *p,
                    });
                }
                e
            },
        },
    );

    // inline_multi_fetch_vs (task-153): the TWO-STREAM fetch caller. It s_swappc's into
    // fetch_vs, which recovers TWO distinct V# from the descriptor set (attr0 vec4 @ desc
    // offset 0 → v[4:7]; attr1 vec2 @ desc offset 16 → v[8:9]) that point at DIFFERENT
    // buffers (different bases + num_records). The caller exports attr0 as pos0 and attr1
    // (padded z=0, w=1) as param0. A recompiler that bound one shared SSBO for both streams
    // (the pre-task-153 bug) would read attr1 from attr0's buffer and mis-export param0, so
    // the exact interp==recompile match on BOTH exports witnesses per-stream binding.
    m.insert(
        "inline_multi_fetch_vs",
        ShaderSpec {
            build_launch: || {
                let attr0 = [
                    [0.0f32, 1.0, 0.0, 1.0],
                    [-1.0, -1.0, 0.0, 1.0],
                    [1.0, -1.0, 0.0, 1.0],
                ];
                let attr1 = [[0.25f32, 0.75], [0.5, 0.5], [0.125, 0.875]];
                let (mem, desc_addr) = build_two_stream_memory(&attr0, &attr1);
                let abi = LaunchAbi::Vertex {
                    // s0/s1 = fetch pointer (inlined away); s2/s3 = V# descriptor set.
                    user_sgprs: vec![0, 0, desc_addr as u32, (desc_addr >> 32) as u32],
                    first_vertex: 0,
                    num_lanes: attr0.len(),
                };
                (abi, mem)
            },
            expected: {
                let attr0 = [
                    [0.0f32, 1.0, 0.0, 1.0],
                    [-1.0, -1.0, 0.0, 1.0],
                    [1.0, -1.0, 0.0, 1.0],
                ];
                let attr1 = [[0.25f32, 0.75], [0.5, 0.5], [0.125, 0.875]];
                let mut e = Vec::new();
                for lane in 0..attr0.len() {
                    e.push(ExpectedExport {
                        lane,
                        target: ExportTarget::Pos(0),
                        values: attr0[lane],
                    });
                    e.push(ExpectedExport {
                        lane,
                        target: ExportTarget::Param(0),
                        values: [attr1[lane][0], attr1[lane][1], 0.0, 1.0],
                    });
                }
                e
            },
        },
    );

    // nonstd_stride_vs (task-128 / task-140): fetches an xyz position from a vertex
    // buffer with a NON-16 stride (24 bytes: 12 bytes xyz + 12 padding) and exports
    // (x, y, z, 1.0). Proves the stride flows in as the SPIR-V PUSH CONSTANT — the
    // recompiler bakes NO stride, so the module must address the padded elements
    // correctly for a stride it never saw at recompile time (the oracle resolves it via
    // Bindings.vertex_stride_bytes = 24, mirroring the pushed value). A 24-byte stride
    // mis-read as 16 would fetch garbage from the padding gap, so an exact match is the
    // witness.
    m.insert(
        "nonstd_stride_vs",
        ShaderSpec {
            build_launch: || {
                let verts = vec![
                    vec![0.0f32, 1.0, 0.0],
                    vec![-1.0, -1.0, 0.5],
                    vec![1.0, -1.0, 0.25],
                ];
                let (mem, desc_addr) = build_vs_memory_strided(&verts, 24, 3);
                let abi = LaunchAbi::Vertex {
                    user_sgprs: vec![0, 0, desc_addr as u32, (desc_addr >> 32) as u32],
                    first_vertex: 0,
                    num_lanes: verts.len(),
                };
                (abi, mem)
            },
            expected: {
                let verts = [[0.0f32, 1.0, 0.0], [-1.0, -1.0, 0.5], [1.0, -1.0, 0.25]];
                let mut e = Vec::new();
                for (v, p) in verts.iter().enumerate() {
                    let vals = [p[0], p[1], p[2], 1.0];
                    e.push(ExpectedExport {
                        lane: v,
                        target: ExportTarget::Pos(0),
                        values: vals,
                    });
                    e.push(ExpectedExport {
                        lane: v,
                        target: ExportTarget::Param(0),
                        values: vals,
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

    // pkrtz_ps: moves the constant RGBA (1.0, 0.25, 0.5, 1.0) into v0..v3, packs it to
    // two f16 pairs with v_cvt_pkrtz_f16_f32 (v0 = {1.0, 0.25}, v1 = {0.5, 1.0}), then
    // compr-exports it — vsrc0 (v0) carries channels 0,1 and vsrc1 (v1) channels 2,3.
    // Every constant is exactly representable in f16, so the pack→unpack round-trip is
    // lossless and the analytic expectation is the literals verbatim (read off the .s,
    // NOT captured from the pack under test). This isolates the compr pack/unpack
    // plumbing from f16 rounding.
    m.insert(
        "pkrtz_ps",
        ShaderSpec {
            build_launch: || {
                let abi = LaunchAbi::Pixel(Box::new(PixelLaunch {
                    user_sgprs: vec![],
                    inputs: PsInputs::default(),
                    bary_i: [0.0; WAVE_SIZE],
                    bary_j: [0.0; WAVE_SIZE],
                    exec: 0b1, // lane 0 live
                }));
                (abi, MockMem::new(0, Vec::new()))
            },
            expected: vec![ExpectedExport {
                lane: 0,
                target: ExportTarget::Mrt(0),
                values: [1.0, 0.25, 0.5, 1.0],
            }],
        },
    );

    // wqm_bracket_ps: exports the constant RGBA (1.0, 0.25, 0.5, 1.0) but wraps a
    // save-EXEC / s_wqm / restore-EXEC bracket around it. The bracket must be
    // transparent to the export — the analytic value is the constants verbatim,
    // proving the exec-mask save/restore neither corrupts a channel nor changes which
    // lane exports.
    m.insert(
        "wqm_bracket_ps",
        ShaderSpec {
            build_launch: || {
                let abi = LaunchAbi::Pixel(Box::new(PixelLaunch {
                    user_sgprs: vec![],
                    inputs: PsInputs::default(),
                    bary_i: [0.0; WAVE_SIZE],
                    bary_j: [0.0; WAVE_SIZE],
                    exec: 0b1, // lane 0 live
                }));
                (abi, MockMem::new(0, Vec::new()))
            },
            expected: vec![ExpectedExport {
                lane: 0,
                target: ExportTarget::Mrt(0),
                values: [1.0, 0.25, 0.5, 1.0],
            }],
        },
    );

    // cbuffer_ps: s_buffer_loads an RGBA constant (1.0, 0.25, 0.5, 1.0) from a uniform
    // buffer via its V# descriptor and exports it. The analytic value is the constants
    // placed in the mock buffer — proving the oracle resolves the V# base and reads the
    // real bytes through the VMM (not a stubbed zero).
    m.insert(
        "cbuffer_ps",
        ShaderSpec {
            build_launch: || {
                let (mem, v_sharp) = build_cbuffer_memory([1.0, 0.25, 0.5, 1.0]);
                let abi = LaunchAbi::Pixel(Box::new(PixelLaunch {
                    user_sgprs: v_sharp.to_vec(), // s0..s3 = constant-buffer V#
                    inputs: PsInputs::default(),
                    bary_i: [0.0; WAVE_SIZE],
                    bary_j: [0.0; WAVE_SIZE],
                    exec: 0b1, // lane 0 live
                }));
                (abi, mem)
            },
            expected: vec![ExpectedExport {
                lane: 0,
                target: ExportTarget::Mrt(0),
                values: [1.0, 0.25, 0.5, 1.0],
            }],
        },
    );

    // cbuffer16_vs: s_buffer_load_dwordx16 loads a 16-dword constant block (a 4×4
    // matrix's worth) from a V# in s[4:7], then exports its DIAGONAL (dwords 0, 5,
    // 10, 15) as pos0 and param0. The analytic values are exactly those four dwords
    // as placed in the mock buffer — proving both sides address the full 16-dword
    // extent at the right offsets (the wider sibling of cbuffer_ps's dwordx4). The
    // block is a recognizable matrix so the diagonal 1,6,11,16 is easy to verify.
    m.insert(
        "cbuffer16_vs",
        ShaderSpec {
            build_launch: || {
                let matrix: [f32; 16] = [
                    1.0, 2.0, 3.0, 4.0, //
                    5.0, 6.0, 7.0, 8.0, //
                    9.0, 10.0, 11.0, 12.0, //
                    13.0, 14.0, 15.0, 16.0,
                ];
                let (mem, v_sharp) = build_cbuffer16_memory(matrix);
                // s0..s3 unused; s4..s7 = the constant-buffer V#.
                let user_sgprs = vec![0, 0, 0, 0, v_sharp[0], v_sharp[1], v_sharp[2], v_sharp[3]];
                let abi = LaunchAbi::Vertex {
                    user_sgprs,
                    first_vertex: 0,
                    num_lanes: 1,
                };
                (abi, mem)
            },
            // Diagonal = dwords 0, 5, 10, 15 = 1.0, 6.0, 11.0, 16.0.
            expected: {
                let diag = [1.0f32, 6.0, 11.0, 16.0];
                vec![
                    ExpectedExport {
                        lane: 0,
                        target: ExportTarget::Pos(0),
                        values: diag,
                    },
                    ExpectedExport {
                        lane: 0,
                        target: ExportTarget::Param(0),
                        values: diag,
                    },
                ]
            },
        },
    );

    // transcendental_ps: floor(2.5)=2.0, fract(2.5)=0.5, sqrt(4.0)=2.0 → exports
    // (2.0, 0.5, 2.0, 2.0). Every result is exact in f32, so the compare is bit-exact
    // and the expectation is reasoned from the math, not the op under test.
    m.insert(
        "transcendental_ps",
        ShaderSpec {
            build_launch: || {
                let abi = LaunchAbi::Pixel(Box::new(PixelLaunch {
                    user_sgprs: vec![],
                    inputs: PsInputs::default(),
                    bary_i: [0.0; WAVE_SIZE],
                    bary_j: [0.0; WAVE_SIZE],
                    exec: 0b1, // lane 0 live
                }));
                (abi, MockMem::new(0, Vec::new()))
            },
            expected: vec![ExpectedExport {
                lane: 0,
                target: ExportTarget::Mrt(0),
                values: [2.0, 0.5, 2.0, 2.0],
            }],
        },
    );

    // minmax_shift_ps: min(0.5,0.25)=0.25, max(0.5,0.25)=0.5, (8>>1) cvt = 4.0 →
    // exports (0.25, 0.5, 4.0, 4.0). Exact in f32; expectation reasoned from the math.
    m.insert(
        "minmax_shift_ps",
        ShaderSpec {
            build_launch: || {
                let abi = LaunchAbi::Pixel(Box::new(PixelLaunch {
                    user_sgprs: vec![],
                    inputs: PsInputs::default(),
                    bary_i: [0.0; WAVE_SIZE],
                    bary_j: [0.0; WAVE_SIZE],
                    exec: 0b1, // lane 0 live
                }));
                (abi, MockMem::new(0, Vec::new()))
            },
            expected: vec![ExpectedExport {
                lane: 0,
                target: ExportTarget::Mrt(0),
                values: [0.25, 0.5, 4.0, 4.0],
            }],
        },
    );

    // vop3_mad_sin_fract_ps: sin(2*PI*0.25)=1.0; fract(|-2.125|)=0.125 then omod ×4 =
    // 0.5; v_mad_u32_u24(3,2,1)=7 then cvt_f32_u32=7.0; v3=1.0 → exports
    // (1.0, 0.5, 7.0, 1.0). Every value exact in f32; expectation reasoned from the math.
    m.insert(
        "vop3_mad_sin_fract_ps",
        ShaderSpec {
            build_launch: || {
                let abi = LaunchAbi::Pixel(Box::new(PixelLaunch {
                    user_sgprs: vec![],
                    inputs: PsInputs::default(),
                    bary_i: [0.0; WAVE_SIZE],
                    bary_j: [0.0; WAVE_SIZE],
                    exec: 0b1, // lane 0 live
                }));
                (abi, MockMem::new(0, Vec::new()))
            },
            expected: vec![ExpectedExport {
                lane: 0,
                target: ExportTarget::Mrt(0),
                values: [1.0, 0.5, 7.0, 1.0],
            }],
        },
    );

    // vop3_clamp_ps: the VOP3 `clamp` output modifier, alone and combined with `omod`
    // (task-188). The hardware output chain is raw -> omod -> clamp, so:
    //   r: 2.0*2.0 + 1.0  =  5.0  -> clamp        -> 1.0
    //   g: 2.0*-2.0 + 1.0 = -3.0  -> clamp        -> 0.0
    //   b: 0.5*0.5 + 0.5  =  0.75 -> mul:2, clamp -> 1.0   (reversed order: 1.5)
    //   a: 1.0*1.0 + 0.5  =  1.5  -> div:2, clamp -> 0.75  (reversed order: 0.5)
    // b and a are the ORDER probes — clamping BEFORE omod yields a different value for
    // each, so the wrong order fails here instead of passing silently. Every value is
    // exact in f32; expectation reasoned from the math, not captured from the oracle.
    m.insert(
        "vop3_clamp_ps",
        ShaderSpec {
            build_launch: || {
                let abi = LaunchAbi::Pixel(Box::new(PixelLaunch {
                    user_sgprs: vec![],
                    inputs: PsInputs::default(),
                    bary_i: [0.0; WAVE_SIZE],
                    bary_j: [0.0; WAVE_SIZE],
                    exec: 0b1, // lane 0 live
                }));
                (abi, MockMem::new(0, Vec::new()))
            },
            expected: vec![ExpectedExport {
                lane: 0,
                target: ExportTarget::Mrt(0),
                values: [1.0, 0.0, 1.0, 0.75],
            }],
        },
    );

    // vop3_clamp_nan_ps: clamp on the non-finite inputs (task-188). GCN's clamp is a
    // min/max saturate, so NaN -> 0.0 (`max(NaN, 0)` returns the non-NaN operand),
    // +inf -> 1.0, -inf -> 0.0; alpha is a plain 1.0. Both backends go through the same
    // f_max/f_min pair, so this pins that they agree here too.
    m.insert(
        "vop3_clamp_nan_ps",
        ShaderSpec {
            build_launch: || {
                let abi = LaunchAbi::Pixel(Box::new(PixelLaunch {
                    user_sgprs: vec![],
                    inputs: PsInputs::default(),
                    bary_i: [0.0; WAVE_SIZE],
                    bary_j: [0.0; WAVE_SIZE],
                    exec: 0b1, // lane 0 live
                }));
                (abi, MockMem::new(0, Vec::new()))
            },
            expected: vec![ExpectedExport {
                lane: 0,
                target: ExportTarget::Mrt(0),
                values: [0.0, 1.0, 0.0, 1.0],
            }],
        },
    );

    // vop3_mul_ps: v_mul_f32_e64 v2 = 4.0 * |-0.5| = 2.0 (abs on src1); v3=1.0 →
    // exports (2.0, 2.0, 2.0, 1.0). Exact in f32; expectation reasoned from the math.
    m.insert(
        "vop3_mul_ps",
        ShaderSpec {
            build_launch: || {
                let abi = LaunchAbi::Pixel(Box::new(PixelLaunch {
                    user_sgprs: vec![],
                    inputs: PsInputs::default(),
                    bary_i: [0.0; WAVE_SIZE],
                    bary_j: [0.0; WAVE_SIZE],
                    exec: 0b1, // lane 0 live
                }));
                (abi, MockMem::new(0, Vec::new()))
            },
            expected: vec![ExpectedExport {
                lane: 0,
                target: ExportTarget::Mrt(0),
                values: [2.0, 2.0, 2.0, 1.0],
            }],
        },
    );

    // rcp_ps: 1/4.0 = 0.25, 1/2.0 = 0.5; v3=1.0 → exports (0.25, 0.5, 0.25, 1.0).
    // Exact in f32; expectation reasoned from the math.
    m.insert(
        "rcp_ps",
        ShaderSpec {
            build_launch: || {
                let abi = LaunchAbi::Pixel(Box::new(PixelLaunch {
                    user_sgprs: vec![],
                    inputs: PsInputs::default(),
                    bary_i: [0.0; WAVE_SIZE],
                    bary_j: [0.0; WAVE_SIZE],
                    exec: 0b1, // lane 0 live
                }));
                (abi, MockMem::new(0, Vec::new()))
            },
            expected: vec![ExpectedExport {
                lane: 0,
                target: ExportTarget::Mrt(0),
                values: [0.25, 0.5, 0.25, 1.0],
            }],
        },
    );

    // vop3_mac_ps: v2 pre-loaded 0.5, then v_mac v2 = 2.0*1.5 + 0.5 = 3.5; v3=1.0 →
    // exports (3.5, 3.5, 3.5, 1.0). Exact in f32; expectation reasoned from the math.
    m.insert(
        "vop3_mac_ps",
        ShaderSpec {
            build_launch: || {
                let abi = LaunchAbi::Pixel(Box::new(PixelLaunch {
                    user_sgprs: vec![],
                    inputs: PsInputs::default(),
                    bary_i: [0.0; WAVE_SIZE],
                    bary_j: [0.0; WAVE_SIZE],
                    exec: 0b1, // lane 0 live
                }));
                (abi, MockMem::new(0, Vec::new()))
            },
            expected: vec![ExpectedExport {
                lane: 0,
                target: ExportTarget::Mrt(0),
                values: [3.5, 3.5, 3.5, 1.0],
            }],
        },
    );

    // vop3_pkrtz_ps: VOP3-form f16 pack of (1.0,0.25) and (0.5,1.0), compressed export
    // unpacks losslessly (all exact in f16) → exports (1.0, 0.25, 0.5, 1.0).
    m.insert(
        "vop3_pkrtz_ps",
        ShaderSpec {
            build_launch: || {
                let abi = LaunchAbi::Pixel(Box::new(PixelLaunch {
                    user_sgprs: vec![],
                    inputs: PsInputs::default(),
                    bary_i: [0.0; WAVE_SIZE],
                    bary_j: [0.0; WAVE_SIZE],
                    exec: 0b1, // lane 0 live
                }));
                (abi, MockMem::new(0, Vec::new()))
            },
            expected: vec![ExpectedExport {
                lane: 0,
                target: ExportTarget::Mrt(0),
                values: [1.0, 0.25, 0.5, 1.0],
            }],
        },
    );

    // m0_save_ps: m0 read before any write is the launch default 0 (→ v0 = 0.0); m0
    // written to bits-of-50.0 then read back is a faithful copy (→ v1 = 50.0); v2=1.0 →
    // exports (0.0, 50.0, 1.0, 1.0). Exact in f32; expectation reasoned from the math.
    m.insert(
        "m0_save_ps",
        ShaderSpec {
            build_launch: || {
                let abi = LaunchAbi::Pixel(Box::new(PixelLaunch {
                    user_sgprs: vec![],
                    inputs: PsInputs::default(),
                    bary_i: [0.0; WAVE_SIZE],
                    bary_j: [0.0; WAVE_SIZE],
                    exec: 0b1, // lane 0 live
                }));
                (abi, MockMem::new(0, Vec::new()))
            },
            expected: vec![ExpectedExport {
                lane: 0,
                target: ExportTarget::Mrt(0),
                values: [0.0, 50.0, 1.0, 1.0],
            }],
        },
    );

    // cmp_cndmask_ps: standalone VOPC → VCC then v_cndmask_b32.
    //   ch0: (1.0 < 2.0)=true  → cndmask(0.25,0.75) picks true  → 0.75
    //   ch1: (1.0 > 2.0)=false → cndmask(0.25,0.75) picks false → 0.25
    //   ch2: 0.5 ; ch3: 1.0
    // Export (0.75, 0.25, 0.5, 1.0). Exact in f32; expectation reasoned from the
    // compare truth values, not captured from the select under test.
    m.insert(
        "cmp_cndmask_ps",
        ShaderSpec {
            build_launch: || {
                let abi = LaunchAbi::Pixel(Box::new(PixelLaunch {
                    user_sgprs: vec![],
                    inputs: PsInputs::default(),
                    bary_i: [0.0; WAVE_SIZE],
                    bary_j: [0.0; WAVE_SIZE],
                    exec: 0b1, // lane 0 live
                }));
                (abi, MockMem::new(0, Vec::new()))
            },
            expected: vec![ExpectedExport {
                lane: 0,
                target: ExportTarget::Mrt(0),
                values: [0.75, 0.25, 0.5, 1.0],
            }],
        },
    );

    // cbranch_alpha_ps: the first branching corpus shader (task-129) — v_cmp_lt_f32 →
    // VCC, then s_cbranch_vccz over a "bright color" block, a single forward `if`.
    //   v_cmp_lt_f32 vcc, 1.0, 2.0 → TRUE, so VCC != 0 → vccz NOT taken → the fall
    //   block runs and overwrites the pre-seeded 0.25 background with (0.75, 0.5,
    //   0.25, 1.0). Export = (0.75, 0.5, 0.25, 1.0). Every value exact in f32.
    // The interp models the branch by narrowing EXEC to the fall lane and OR-restoring
    // at the merge; the recompiler emits OpSelectionMerge + OpBranchConditional on the
    // per-invocation VCC bool. Both must agree on this export (the differential + the
    // CPU SPIR-V value oracle are the guard).
    m.insert(
        "cbranch_alpha_ps",
        ShaderSpec {
            build_launch: || {
                let abi = LaunchAbi::Pixel(Box::new(PixelLaunch {
                    user_sgprs: vec![],
                    inputs: PsInputs::default(),
                    bary_i: [0.0; WAVE_SIZE],
                    bary_j: [0.0; WAVE_SIZE],
                    exec: 0b1, // lane 0 live
                }));
                (abi, MockMem::new(0, Vec::new()))
            },
            expected: vec![ExpectedExport {
                lane: 0,
                target: ExportTarget::Mrt(0),
                values: [0.75, 0.5, 0.25, 1.0],
            }],
        },
    );

    // cbranch_select_ps: the second branching corpus shader (task-129, slice 4) — a
    // real if-ELSE DIAMOND. v_cmp_lt_f32 → VCC, then s_cbranch_vccz to a "dark" arm; a
    // "bright" arm (the fall side) writes v2..v5 then s_branch's over the dark arm; both
    // arms reconverge at a merge that exports v2..v5.
    //   v_cmp_lt_f32 vcc, 1.0, 2.0 → TRUE, so VCC != 0 → vccz NOT taken → the BRIGHT arm
    //   runs and writes (0.75, 0.5, 0.25, 1.0). The dark arm (0.125,…) is skipped, so
    //   its writes to the SAME v2..v5 never apply for the live lane. Export =
    //   (0.75, 0.5, 0.25, 1.0). Every value exact in f32.
    // Unlike cbranch_alpha_ps (single `if`, one no-op side), BOTH arms here write the
    // same VGPRs — this pins the load/store register model's last-writer-wins across a
    // REAL merge with NO OpPhi (the no-phi decision). The interp runs each arm under its
    // EXEC lane mask and reconverges at the merge; the recompiler emits
    // OpSelectionMerge + OpBranchConditional to the two arms, each OpBranch'ing to the
    // merge. The differential + the CPU SPIR-V value oracle both guard the agreement.
    m.insert(
        "cbranch_select_ps",
        ShaderSpec {
            build_launch: || {
                let abi = LaunchAbi::Pixel(Box::new(PixelLaunch {
                    user_sgprs: vec![],
                    inputs: PsInputs::default(),
                    bary_i: [0.0; WAVE_SIZE],
                    bary_j: [0.0; WAVE_SIZE],
                    exec: 0b1, // lane 0 live
                }));
                (abi, MockMem::new(0, Vec::new()))
            },
            expected: vec![ExpectedExport {
                lane: 0,
                target: ExportTarget::Mrt(0),
                values: [0.75, 0.5, 0.25, 1.0],
            }],
        },
    );

    // loop_accum_ps: the counted, UNIFORM natural-loop shape (the loops slice — the
    // last control-flow slice after the single `if` and the if-else diamond). Derived
    // analytically from the shader math, NOT captured from the code under test:
    //
    //   v0 = 0.0 (accumulator), v1 = 0.0 (counter), v2 = 1.0 (alpha), v3 = 4.0 (limit)
    //   header (each iteration):
    //     v0 += 0.25            ; accumulate a constant
    //     v1 += 1.0             ; ++counter
    //     v_cmp_lt_f32 vcc, v1, v3   ; continue while counter < 4
    //     s_cbranch_vccnz header     ; back-edge: loop while VCC != 0
    //
    // Trip count: the counter takes values 1, 2, 3, 4 at the compare; `1<4, 2<4, 3<4`
    // are true (loop continues), `4<4` is false (VCC clears → exit) after the 4th body
    // execution. So the body runs exactly 4 times and the accumulator ends at
    // 4 * 0.25 = 1.0 — every partial (0.25, 0.5, 0.75, 1.0) is exact in f32, so there
    // is no rounding drift. The loop is UNIFORM: v1/v3 are lane-independent constants,
    // so every live lane runs the identical 4 iterations; the single live lane (0)
    // exports (v0, v0, v0, v2) = (1.0, 1.0, 1.0, 1.0).
    //
    // This pins the whole loop lowering end to end: the interp oracle walks the body
    // under EXEC re-testing the back-edge each iteration (with a safety iteration cap),
    // the recompiler emits OpLoopMerge %merge %continue with the back-edge as an
    // OpBranchConditional carrying v0/v1 across iterations via Function OpVariable
    // load/store (NO OpPhi), and the CPU SPIR-V value oracle re-executes the
    // OpLoopMerge/back-edge (with a block-visit cap). All three must agree bit-for-bit.
    m.insert(
        "loop_accum_ps",
        ShaderSpec {
            build_launch: || {
                let abi = LaunchAbi::Pixel(Box::new(PixelLaunch {
                    user_sgprs: vec![],
                    inputs: PsInputs::default(),
                    bary_i: [0.0; WAVE_SIZE],
                    bary_j: [0.0; WAVE_SIZE],
                    exec: 0b1, // lane 0 live
                }));
                (abi, MockMem::new(0, Vec::new()))
            },
            expected: vec![ExpectedExport {
                lane: 0,
                target: ExportTarget::Mrt(0),
                values: [1.0, 1.0, 1.0, 1.0],
            }],
        },
    );

    // vop3_cmp_cndmask_ps: VOP3-form VOPC → an SGPR pair, then VOP3-form v_cndmask
    // reading that pair. Same truth values as cmp_cndmask_ps but the predicate is an
    // arbitrary sgpr pair (s[16:17] / s[12:13]) rather than the implicit VCC.
    //   ch0: (1.0 < 2.0)=true  via s[16:17] → 0.75
    //   ch1: (1.0 > 2.0)=false via s[12:13] → 0.25
    // Export (0.75, 0.25, 0.5, 1.0). Exact in f32.
    m.insert(
        "vop3_cmp_cndmask_ps",
        ShaderSpec {
            build_launch: || {
                let abi = LaunchAbi::Pixel(Box::new(PixelLaunch {
                    user_sgprs: vec![],
                    inputs: PsInputs::default(),
                    bary_i: [0.0; WAVE_SIZE],
                    bary_j: [0.0; WAVE_SIZE],
                    exec: 0b1, // lane 0 live
                }));
                (abi, MockMem::new(0, Vec::new()))
            },
            expected: vec![ExpectedExport {
                lane: 0,
                target: ExportTarget::Mrt(0),
                values: [0.75, 0.25, 0.5, 1.0],
            }],
        },
    );

    // vop3_cmp3_ps: the remaining three VOP3-form f32 compares (le/ge/eq), each into
    // its own SGPR pair, read back by a VOP3-form v_cndmask. Companion to
    // vop3_cmp_cndmask_ps (which covers lt/gt).
    //   ch0: (1.0 <= 2.0)=true  via s[16:17] → 0.75
    //   ch1: (1.0 >= 2.0)=false via s[12:13] → 0.25
    //   ch2: (1.0 == 1.0)=true  via s[8:9]   → 0.75
    // Export (0.75, 0.25, 0.75, 1.0). Exact in f32; expectation reasoned from the
    // compare truth values, not captured from the select under test.
    m.insert(
        "vop3_cmp3_ps",
        ShaderSpec {
            build_launch: || {
                let abi = LaunchAbi::Pixel(Box::new(PixelLaunch {
                    user_sgprs: vec![],
                    inputs: PsInputs::default(),
                    bary_i: [0.0; WAVE_SIZE],
                    bary_j: [0.0; WAVE_SIZE],
                    exec: 0b1, // lane 0 live
                }));
                (abi, MockMem::new(0, Vec::new()))
            },
            expected: vec![ExpectedExport {
                lane: 0,
                target: ExportTarget::Mrt(0),
                values: [0.75, 0.25, 0.75, 1.0],
            }],
        },
    );

    // vadd_i32_ps: v_add_i32 with a VCC carry-out, consumed by v_cndmask.
    //   1 + 2 = 3, carry=false → cvt_f32_i32 → 3.0 (ch0); cndmask picks false → 0.25 (ch1)
    //   -1 + 1 = 0 (wraps), carry=true → cndmask picks true → 0.75 (ch2) ; ch3: 1.0
    // Export (3.0, 0.25, 0.75, 1.0). Exact in f32; expectation reasoned from the
    // wrapping add + carry, not captured from the op under test.
    m.insert(
        "vadd_i32_ps",
        ShaderSpec {
            build_launch: || {
                let abi = LaunchAbi::Pixel(Box::new(PixelLaunch {
                    user_sgprs: vec![],
                    inputs: PsInputs::default(),
                    bary_i: [0.0; WAVE_SIZE],
                    bary_j: [0.0; WAVE_SIZE],
                    exec: 0b1, // lane 0 live
                }));
                (abi, MockMem::new(0, Vec::new()))
            },
            expected: vec![ExpectedExport {
                lane: 0,
                target: ExportTarget::Mrt(0),
                values: [3.0, 0.25, 0.75, 1.0],
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

        let insts = runnable_insts(name);
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
        let insts = runnable_insts(&name);
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
    /// The DISTINCT `(srsrc, ssamp)` descriptor pairs the oracle samples through — the
    /// recompiler must declare exactly one combined image-sampler per pair (task-199).
    /// A pair sampled repeatedly is one binding; two different pairs are two textures,
    /// which is the whole point: the MIMG operands are per-instruction, so one PS can mix
    /// a register-resident T# with a memory-resident one. Scanned off the instruction
    /// stream, independent of the recompiler's own bookkeeping.
    texture_descriptors: BTreeSet<(u8, u8)>,
}

impl OracleInterface {
    /// Whether the oracle samples any texture at all.
    fn has_texture_sample(&self) -> bool {
        !self.texture_descriptors.is_empty()
    }
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
            texture_descriptors: BTreeSet::new(),
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
                Inst::Mimg { srsrc, ssamp, .. } => {
                    // image_sample: the oracle samples a texture through a T#/S# pair; the
                    // recompiler must declare one combined image-sampler per DISTINCT pair.
                    o.texture_descriptors.insert((*srsrc, *ssamp));
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
        oracle.has_texture_sample(),
        "{name}: texture sampler — recompiler {} bindings vs oracle has_sample {}",
        io.samplers.len(),
        oracle.has_texture_sample(),
    );
    // One combined image-sampler per DISTINCT descriptor pair (task-199): repeat samples
    // through the same T#/S# share a binding, two different pairs are two textures. A
    // collapse here is what made Celeste's distortion pass export the wrong texture.
    assert_eq!(
        io.samplers.len(),
        oracle.texture_descriptors.len(),
        "{name}: recompiler declared {} sampler bindings for {} distinct oracle T#/S# pairs",
        io.samplers.len(),
        oracle.texture_descriptors.len(),
    );
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
        let insts = runnable_insts(&name);
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

// ---- AC #1 + #2 + #3: CPU-SPIR-V value oracle vs interp oracle --------------

/// The dual-oracle value check (task-122). For every corpus shader WITH an analytic
/// spec, recompile it, then execute the recompiled SPIR-V on the CPU — once per live
/// lane, driven by inputs reconstructed from the SAME launch that drives the interp
/// oracle — and assert its exports agree with the interp oracle per lane, per
/// channel. This is the value-level agreement the maintainer-run GPU `diff_harness`
/// used to be the ONLY witness of; now a wrong operand order / flipped compare /
/// rounding bug in an emitted op turns this test RED under plain `cargo test` (AC #2),
/// not just on a real device.
///
/// Comparison rule (AC #1 + #3): every channel must be bit-for-bit equal EXCEPT for
/// the `sin`-containing shader, where the host `sinf` is not correctly rounded and is
/// compared within a small absolute epsilon (documented at the compare site).
#[test]
fn recompiled_spirv_matches_oracle() {
    let specs = analytic_specs();
    let corpus = enumerate_corpus();

    let mut checked_any = false;
    for (name, stage) in &corpus {
        let Some(spec) = specs.get(name.as_str()) else {
            eprintln!("{name}: no analytic spec — skipped by the CPU-SPIR-V value oracle");
            continue;
        };
        checked_any = true;

        let insts = runnable_insts(name);
        let recompiled =
            recompile(&insts, *stage).unwrap_or_else(|e| panic!("{name}: recompile: {e}"));

        // Drive the interp oracle (the reference).
        let (abi, mem) = (spec.build_launch)();
        let live = live_lanes(&abi);
        let oracle = run(&insts, oracle_abi(&spec.build_launch), &mem)
            .unwrap_or_else(|e| panic!("{name}: oracle run: {e}"));

        // sin is the only transcendental in the corpus; its shader gets an epsilon.
        let sin_shader = name == "vop3_mad_sin_fract_ps";

        for lane in live {
            let bindings = reconstruct_bindings(&spec.build_launch, &recompiled.io, *stage, lane);
            let evald = spirv_eval::eval_lane(&recompiled.spirv, &bindings)
                .unwrap_or_else(|e| panic!("{name}: eval lane {lane}: {e}"));

            // Map each evaluator export target to the oracle's target for this stage,
            // then compare per channel against the oracle's export for (lane, target).
            for ev in &evald {
                let target = map_eval_target(*stage, ev.target);
                let want = find_export(&oracle, lane, target).unwrap_or_else(|| {
                    panic!(
                        "{name}: lane {lane} — CPU-SPIR-V produced export {target:?} \
                         the oracle did not (values {:?})",
                        ev.values
                    )
                });
                for ch in 0..4 {
                    let got = ev.values[ch];
                    let exp = want.values[ch];
                    if sin_shader {
                        // AC #3: host sinf is ULP-class, not correctly rounded; the
                        // recompiled GLSL sin and the oracle's `(x*TAU).sin()` agree
                        // only to a small budget. 1e-6 abs is ~8 ULP near 1.0 — far
                        // tighter than any real op error, loose enough for sinf.
                        assert!(
                            (got - exp).abs() <= 1e-6,
                            "{name}: lane {lane} {target:?} ch{ch} — CPU-SPIR-V {got} \
                             vs oracle {exp} exceeds sin ULP budget 1e-6"
                        );
                    } else {
                        // Every other corpus export is exact in f32 (pack/fract/etc are
                        // all exactly representable for the corpus inputs).
                        assert_eq!(
                            got.to_bits(),
                            exp.to_bits(),
                            "{name}: lane {lane} {target:?} ch{ch} — CPU-SPIR-V {got} \
                             != oracle {exp} (bit-for-bit)"
                        );
                    }
                }
            }

            // Coverage: the evaluator must produce an export for every (lane, target)
            // the oracle produced for this lane — no missing exports either.
            for rec in oracle.iter().filter(|e| e.lane == lane) {
                let found = evald
                    .iter()
                    .any(|ev| map_eval_target(*stage, ev.target) == rec.target);
                assert!(
                    found,
                    "{name}: lane {lane} — oracle exported {:?} but CPU-SPIR-V did not",
                    rec.target
                );
            }
        }
    }
    assert!(
        checked_any,
        "no corpus shader had an analytic spec — the CPU-SPIR-V oracle ran nothing"
    );
}

/// The synthetic launch driving the packed `_8_8_8_8` UNORM fetch test — hardcoded (no
/// capture) so it coerces to the `fn()` pointer `reconstruct_bindings` takes (task-164).
fn build_packed_rgba8_launch() -> (LaunchAbi, MockMem) {
    // A mix: full/half/zero channels so unorm rounding (byte/255) is actually exercised.
    let colors = [[255u8, 128, 0, 255], [10, 20, 30, 40], [0, 200, 255, 1]];
    let (mem, desc_addr) = build_vs_memory_rgba8_unorm(&colors);
    let abi = LaunchAbi::Vertex {
        user_sgprs: vec![0, 0, desc_addr as u32, (desc_addr >> 32) as u32],
        first_vertex: 0,
        num_lanes: colors.len(),
    };
    (abi, mem)
}

/// task-164: a passthrough VS fetching a packed `_8_8_8_8` UNORM color (dfmt 10 / nfmt 0)
/// must unpack the ONE packed dword into four normalized floats (byte/255) IDENTICALLY on the
/// interp oracle and the recompiled SPIR-V — the Celeste sprite-color path. This exercises the
/// new format-aware fetch (the existing corpus specs are all 32-bit float, which takes the raw
/// path). Also pins the numeric answer so both sides unpacking wrong the same way is caught.
#[test]
fn packed_rgba8_unorm_vertex_fetch_interp_matches_recompile() {
    let build: fn() -> (LaunchAbi, MockMem) = build_packed_rgba8_launch;
    let insts = runnable_insts("passthrough_vs");
    let recompiled = recompile(&insts, ShaderStage::Vertex).expect("recompile passthrough_vs");

    // Drive the interp oracle (the reference).
    let (abi, mem) = build();
    let live = live_lanes(&abi);
    let oracle = run(&insts, build().0, &mem).expect("oracle run");

    // Independent expectation: byte/255 per channel for each vertex's packed color.
    let colors = [[255u8, 128, 0, 255], [10, 20, 30, 40], [0, 200, 255, 1]];
    let expected = |lane: usize| {
        let c = colors[lane];
        [
            c[0] as f32 / 255.0,
            c[1] as f32 / 255.0,
            c[2] as f32 / 255.0,
            c[3] as f32 / 255.0,
        ]
    };

    for lane in live {
        let bindings = reconstruct_bindings(&build, &recompiled.io, ShaderStage::Vertex, lane);
        let evald = spirv_eval::eval_lane(&recompiled.spirv, &bindings)
            .unwrap_or_else(|e| panic!("eval lane {lane}: {e}"));
        assert!(!evald.is_empty(), "lane {lane} produced no exports");
        for ev in &evald {
            let target = map_eval_target(ShaderStage::Vertex, ev.target);
            let want = find_export(&oracle, lane, target).unwrap_or_else(|| {
                panic!("lane {lane}: CPU-SPIR-V export {target:?} the oracle did not produce")
            });
            let exp_unorm = expected(lane);
            for (ch, &exp) in exp_unorm.iter().enumerate() {
                // interp == recompile, bit-for-bit.
                assert_eq!(
                    ev.values[ch].to_bits(),
                    want.values[ch].to_bits(),
                    "lane {lane} {target:?} ch{ch}: CPU-SPIR-V {} != oracle {} (bit-for-bit)",
                    ev.values[ch],
                    want.values[ch]
                );
                // Both actually unpacked byte/255 (not raw-dword garbage).
                assert_eq!(
                    want.values[ch].to_bits(),
                    exp.to_bits(),
                    "lane {lane} {target:?} ch{ch}: unpacked {} != expected byte/255 {}",
                    want.values[ch],
                    exp
                );
            }
        }
    }
}

/// Rebuild the launch once for the oracle (the spec's closure consumes it).
fn oracle_abi(build_launch: &fn() -> (LaunchAbi, MockMem)) -> LaunchAbi {
    build_launch().0
}

/// The set of live lanes for a launch (the lanes that export).
fn live_lanes(abi: &LaunchAbi) -> Vec<usize> {
    match abi {
        LaunchAbi::Vertex { num_lanes, .. } => (0..(*num_lanes).min(WAVE_SIZE)).collect(),
        LaunchAbi::Pixel(p) => (0..WAVE_SIZE).filter(|l| (p.exec >> l) & 1 == 1).collect(),
    }
}

/// Map an evaluator export target (which always stamps `Param(loc)` for a
/// Location-decorated Output, and `Pos(0)` for the position builtin) to the interp
/// oracle's target for this stage: VS Location → `Param(n)`, PS Location → `Mrt(n)`,
/// position stays `Pos(0)`.
fn map_eval_target(stage: ShaderStage, ev: ExportTarget) -> ExportTarget {
    match (stage, ev) {
        (ShaderStage::Fragment, ExportTarget::Param(n)) => ExportTarget::Mrt(n),
        (_, other) => other,
    }
}

/// Reconstruct the SPIR-V evaluator's per-lane [`spirv_eval::Bindings`] from the SAME
/// `(LaunchAbi, MockMem)` the interp oracle runs on. Both sides therefore see byte-
/// identical resource contents; only the plumbing differs.
fn reconstruct_bindings(
    build_launch: &fn() -> (LaunchAbi, MockMem),
    io: &IoLayout,
    stage: ShaderStage,
    lane: usize,
) -> spirv_eval::Bindings {
    let (abi, mem) = build_launch();
    match abi {
        LaunchAbi::Vertex {
            user_sgprs,
            first_vertex,
            ..
        } => {
            // One stream per SSBO the recompiled VS declares (task-153): read each stream's
            // V# from the descriptor-set pointer its `source` names, then read that V#'s
            // buffer as flat dwords from ITS base — mirroring the per-stream SSBO the backend
            // binds at each V# base.
            let vertex_streams = io
                .buffers
                .iter()
                .map(|b| reconstruct_vertex_stream(&user_sgprs, &mem, b.source))
                .collect();
            let cbuffer = reconstruct_cbuffer(&user_sgprs, &mem);
            spirv_eval::Bindings {
                vertex_index: Some(first_vertex + lane as u32),
                vertex_streams,
                interpolants: std::collections::HashMap::new(),
                texture: None,
                cbuffer,
            }
        }
        LaunchAbi::Pixel(p) => {
            let _ = stage;
            // Pre-interpolate every attribute channel to a per-Location vec4, matching
            // the oracle's plane equation P0 + I·(P1-P0) + J·(P2-P0).
            let i = p.bary_i[lane];
            let j = p.bary_j[lane];
            let mut interpolants = std::collections::HashMap::new();
            for (attr, planes) in p.inputs.attr_planes.iter().enumerate() {
                let mut vec = [0.0f32; 4];
                for (ch, plane) in planes.iter().enumerate() {
                    let (p0, p1, p2) = (plane[0], plane[1], plane[2]);
                    vec[ch] = p0 + i * (p1 - p0) + j * (p2 - p0);
                }
                interpolants.insert(attr as u32, vec);
            }
            let texture = reconstruct_texture(&p.user_sgprs, &mem);
            let cbuffer = reconstruct_cbuffer(&p.user_sgprs, &mem);
            spirv_eval::Bindings {
                vertex_index: None,
                vertex_streams: Vec::new(),
                interpolants,
                texture,
                cbuffer,
            }
        }
    }
}

/// Reconstruct ONE vertex-fetch stream's [`spirv_eval::VertexStreamBinding`] (task-153)
/// from the descriptor provenance the recompiler recorded in the stream's
/// [`BufferBinding::source`]. The `SetPointer{sgpr, desc_offset}` names the user-SGPR pair
/// holding the descriptor-set pointer and the V#'s byte offset within that set — exactly
/// how `derive_buffer_ranges` resolves it in `ps4-gnm`. Reads that V#, then reads the
/// buffer as a FLAT dword array from ITS base (the evaluator addresses stream `s`
/// dword-wise from its own base, mirroring the SSBO bound at that V# base). `stride` comes
/// from `word1[29:16]` (what `interp.rs::decode_v_sharp` reads) so a non-16 stride is
/// honored; `dst_sel` from `word3[11:0]` (the GPU push constant, task-155).
fn reconstruct_vertex_stream(
    user_sgprs: &[u32],
    mem: &MockMem,
    source: DescriptorSource,
) -> spirv_eval::VertexStreamBinding {
    let empty = || spirv_eval::VertexStreamBinding {
        dwords: Vec::new(),
        stride_bytes: 0,
        num_records: 0,
        dst_sel_packed: 0,
        format_packed: 0,
    };
    let DescriptorSource::SetPointer { sgpr, desc_offset } = source else {
        return empty();
    };
    let lo = sgpr as usize;
    if lo + 1 >= user_sgprs.len() {
        return empty();
    }
    let set_ptr = u64::from(user_sgprs[lo]) | (u64::from(user_sgprs[lo + 1]) << 32);
    let desc_addr = set_ptr.wrapping_add(u64::from(desc_offset));
    let Some(v_words) = read_dwords_at(mem, desc_addr, 4) else {
        return empty();
    };
    let base = u64::from(v_words[0]) | (u64::from(v_words[1] & 0xFFFF) << 32);
    let stride_bytes = (v_words[1] >> 16) & 0x3FFF;
    let num_records = v_words[2];
    // word3[11:0] = packed dst_sel — the value the GPU pushes as this stream's member 2.
    let dst_sel_packed = v_words[3] & 0xFFF;
    // word3[14:12] = nfmt, word3[18:15] = dfmt — packed dfmt[7:0] | nfmt[15:8] as this
    // stream's member 3 (task-164), the value the GPU pushes from `BufferDesc::packed_format`.
    let dfmt = (v_words[3] >> 15) & 0xF;
    let nfmt = (v_words[3] >> 12) & 0x7;
    let format_packed = dfmt | (nfmt << 8);
    // Read the WHOLE buffer region from `base` to the end of the mock's backing bytes as a
    // flat dword array — the evaluator addresses it dword-wise from this stream's base,
    // exactly as the interp reads flat guest memory (and the backend binds the SSBO at the
    // V# base). Reading to the region end keeps a tightly-packed buffer with no tail padding
    // in bounds.
    let avail_bytes = mem
        .region_data()
        .len()
        .saturating_sub((base - mem.region_base()) as usize);
    let dword_count = avail_bytes / 4;
    let dwords = read_dwords_at(mem, base, dword_count).unwrap_or_default();
    spirv_eval::VertexStreamBinding {
        dwords,
        stride_bytes,
        num_records,
        dst_sel_packed,
        format_packed,
    }
}

/// Reconstruct the const-buffer SSBO dwords: find the V# among the user SGPRs whose
/// base matches the mock's region, then read its `num_records` dwords. Returns an
/// empty vec when no const buffer is present. Both `build_cbuffer_memory` and
/// `build_cbuffer16_memory` place the block at the mock's base with the V# in the
/// user SGPRs (s0..s3 or s4..s7).
fn reconstruct_cbuffer(user_sgprs: &[u32], mem: &MockMem) -> Vec<u32> {
    // Try each 4-dword aligned window as a candidate V#; accept the one whose base
    // equals the mock's region base (that is the const buffer).
    for start in (0..user_sgprs.len().saturating_sub(3)).step_by(4) {
        let w = &user_sgprs[start..start + 4];
        let base = u64::from(w[0]) | (u64::from(w[1] & 0xFFFF) << 32);
        let num_records = w[2];
        if base == mem.region_base()
            && num_records > 0
            && num_records <= 64
            && let Some(words) = read_dwords_at(mem, base, num_records as usize)
        {
            return words;
        }
    }
    Vec::new()
}

/// Reconstruct the sampler texture from the T# (`s0..s7`) and S# (`s8..s11`) the
/// pixel launch carries, reading texels from the mock — mirroring
/// `interp.rs::decode_t_sharp` / `decode_s_sharp` / `texel` on the same bytes
/// `build_texture_memory` wrote. Returns `None` when there is no texture descriptor.
fn reconstruct_texture(user_sgprs: &[u32], mem: &MockMem) -> Option<spirv_eval::Texture> {
    if user_sgprs.len() < 12 {
        return None;
    }
    let w0 = user_sgprs[0];
    let w1 = user_sgprs[1];
    let w2 = user_sgprs[2];
    let w3 = user_sgprs[3];
    let base = (u64::from(w0) << 8) | (u64::from(w1 & 0xFF) << 40);
    if base != mem.region_base() {
        return None;
    }
    let width = (w2 & 0x3FFF) + 1;
    let height = ((w2 >> 14) & 0x3FFF) + 1;
    // linear tiling only (tiling index 0); the corpus uses linear.
    let _tiling = (w3 >> 20) & 0x1F;
    let s2 = user_sgprs[10]; // S# word 2 (S# starts at s8, so word2 = s10).
    let bilinear = (s2 >> 20) & 1 == 1;
    let rgba = mem
        .region_data()
        .get(..(width * height * 4) as usize)?
        .to_vec();
    Some(spirv_eval::Texture {
        width,
        height,
        rgba,
        bilinear,
    })
}

/// Read `count` little-endian dwords starting at guest address `addr` from the mock,
/// or `None` if the range is outside the mock's region.
fn read_dwords_at(mem: &MockMem, addr: u64, count: usize) -> Option<Vec<u32>> {
    let base = mem.region_base();
    let start = addr.checked_sub(base)? as usize;
    let data = mem.region_data();
    let end = start.checked_add(count * 4)?;
    if end > data.len() {
        return None;
    }
    Some(
        data[start..end]
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect(),
    )
}
