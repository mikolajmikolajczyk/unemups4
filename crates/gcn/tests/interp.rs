//! wave64 CPU interpreter (oracle) integration tests (doc-2 §1, doc-3).
//!
//! Drives the interpreter over the committed GCN corpus and asserts the captured
//! exports match hand-computed goldens. Every load flows through a `Vec<u8>`-backed
//! mock [`VirtualMemoryManager`] that records each access and refuses all host
//! (ambient) reads — so a passing run also proves the interpreter never reads
//! outside the memory it is handed (AC #3).

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use ps4_core::memory::{MemoryProtection, VirtualMemoryManager};
use ps4_gcn::{
    ExportRecord, ExportTarget, InterpError, LaunchAbi, PixelLaunch, PsInputs, WAVE_SIZE,
    decode_all, run,
};

// ---- corpus loading --------------------------------------------------------

fn corpus_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus")
}

fn read_code_dwords(name: &str) -> Vec<u32> {
    let p = corpus_dir().join(format!("{name}.code.bin"));
    let bytes = std::fs::read(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()));
    bytes
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

// ---- mock memory: the ONLY bytes the interpreter may see -------------------

/// A `Vec<u8>`-backed memory whose contents live at a single base address. It
/// serves [`read_bytes`] out of that buffer and records every access; any
/// `get_host_ptr` (the ambient/host path) returns `None`, so a stray host read
/// fails loudly instead of leaking real memory.
struct MockMem {
    base: u64,
    data: Vec<u8>,
    /// (addr, size) of every `read_bytes` call, in order — proves what was touched.
    reads: Mutex<Vec<(u64, usize)>>,
}

impl MockMem {
    fn new(base: u64, data: Vec<u8>) -> Self {
        MockMem {
            base,
            data,
            reads: Mutex::new(Vec::new()),
        }
    }

    fn reads(&self) -> Vec<(u64, usize)> {
        self.reads.lock().unwrap().clone()
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
    /// Host access is deliberately unavailable: the interpreter must go through
    /// `read_bytes`, and nothing may translate a guest addr to a host pointer.
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
        self.reads.lock().unwrap().push((addr, size));
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

// ---- AC #1: corpus VS over a synthetic vertex buffer -----------------------

/// Build the mock memory for the pass-through VS:
///
/// - a 128-bit buffer V# (base = VB, stride = 16 bytes) at `DESC_ADDR`,
/// - a vertex buffer at `VB_ADDR` with `positions.len()` vec4 positions.
///
/// Returns (mem, desc_addr) where desc_addr is what s[2:3] points at.
fn build_vs_memory(positions: &[[f32; 4]]) -> (MockMem, u64) {
    // Everything lives in one contiguous mock buffer starting at BASE.
    const BASE: u64 = 0x1_0000;
    const DESC_OFF: u64 = 0; // V# descriptor at start
    const VB_OFF: u64 = 64; // vertex data after the descriptor

    let vb_addr = BASE + VB_OFF;
    let stride: u32 = 16; // one vec4 = 16 bytes

    let mut data = Vec::new();
    // V# word0: base[31:0]; word1: base[47:32] | (stride << 16); word2: num_records;
    // word3: format/swizzle (unused by this simplified fetch).
    push_u32(&mut data, (vb_addr & 0xFFFF_FFFF) as u32);
    push_u32(
        &mut data,
        ((vb_addr >> 32) as u32 & 0xFFFF) | (stride << 16),
    );
    push_u32(&mut data, positions.len() as u32);
    push_u32(&mut data, 0);
    // pad to VB_OFF
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

fn find_export(
    exports: &[ExportRecord],
    lane: usize,
    target: ExportTarget,
) -> Option<&ExportRecord> {
    exports
        .iter()
        .find(|e| e.lane == lane && e.target == target)
}

#[test]
fn vs_exports_positions_from_synthetic_buffer() {
    let positions = [
        [0.0, 1.0, 0.0, 1.0],
        [-1.0, -1.0, 0.0, 1.0],
        [1.0, -1.0, 0.0, 1.0],
    ];
    let (mem, desc_addr) = build_vs_memory(&positions);

    let code = read_code_dwords("passthrough_vs");
    let insts = decode_all(&code);

    // ABI: s[2:3] = pointer to the V# descriptor set; v0 = vertex index per lane.
    let abi = LaunchAbi::Vertex {
        user_sgprs: vec![0, 0, desc_addr as u32, (desc_addr >> 32) as u32],
        first_vertex: 0,
        num_lanes: positions.len(),
    };

    let exports = run(&insts, abi, &mem).expect("VS interp");

    for (v, expect) in positions.iter().enumerate() {
        let pos = find_export(&exports, v, ExportTarget::Pos(0))
            .unwrap_or_else(|| panic!("no pos0 for vertex {v}"));
        assert_eq!(pos.values, *expect, "pos0 vertex {v}");
        let param = find_export(&exports, v, ExportTarget::Param(0))
            .unwrap_or_else(|| panic!("no param0 for vertex {v}"));
        assert_eq!(param.values, *expect, "param0 vertex {v}");
    }
    // No export from a lane beyond num_lanes.
    assert!(
        find_export(&exports, positions.len(), ExportTarget::Pos(0)).is_none(),
        "masked-off lane must not export"
    );
}

// ---- AC #2: corpus PS → mrt0 colors, incl. EXEC-masked lane ----------------

#[test]
fn flat_ps_exports_constant_color_and_respects_exec() {
    let code = read_code_dwords("flat_color_ps");
    let insts = decode_all(&code);

    // Cover lanes 0 and 2; mask lane 1 out of EXEC.
    let exec = 0b101u64;
    let abi = LaunchAbi::Pixel(Box::new(PixelLaunch {
        user_sgprs: vec![],
        inputs: PsInputs::default(),
        bary_i: [0.0; WAVE_SIZE],
        bary_j: [0.0; WAVE_SIZE],
        exec,
    }));

    // flat_color_ps writes (1.0, 0.25, 0.5, 1.0) — v1 is literal 0x3e800000 = 0.25.
    let expect = [1.0, 0.25, 0.5, 1.0];

    // No memory is touched by the flat PS; an all-failing mem still works.
    let mem = MockMem::new(0, Vec::new());
    let exports = run(&insts, abi, &mem).expect("flat PS interp");

    let m0 = find_export(&exports, 0, ExportTarget::Mrt(0)).expect("lane 0 mrt0");
    assert_eq!(m0.values, expect);
    let m2 = find_export(&exports, 2, ExportTarget::Mrt(0)).expect("lane 2 mrt0");
    assert_eq!(m2.values, expect);
    // The masked-off lane 1 must produce no export.
    assert!(
        find_export(&exports, 1, ExportTarget::Mrt(0)).is_none(),
        "EXEC-masked lane 1 must not export"
    );
    assert!(mem.reads().is_empty(), "flat PS must touch no memory");
}

/// task-129 first slice: a forward `s_cbranch_vccz` `if` with per-lane divergence.
///
/// This drives a hand-assembled PS whose per-lane compare (`v_cmp_lt_f32 vcc, 0.5,
/// v0`, with v0 = the launch bary_i) makes lanes diverge at the branch: a lane with
/// v0 > 0.5 falls through and runs the "bright" block; a lane with v0 <= 0.5 takes
/// the branch and skips it, keeping the pre-seeded background color. An EXEC-masked
/// lane produces no export at all. This exercises the interpreter's single-level EXEC
/// split (narrow to the fall lanes, run the body, OR-restore at the structured merge)
/// and the dropped-lane path — the divergence the recompiler degenerates per-lane.
#[test]
fn cbranch_vccz_diverges_per_lane_and_drops_masked_lanes() {
    // Hand-assembled (llvm-mc, bonaire) forward `if`, mirroring cbranch_alpha_ps but
    // with a PER-LANE compare against v0 so lanes take different paths:
    //   v_cmp_lt_f32 vcc, 0.5, v0   ; per-lane: 0.5 < v0
    //   v2..v5 = 0.25               ; seed background
    //   s_cbranch_vccz +6           ; skip the bright block when vcc==0 (v0 <= 0.5)
    //   v2=0.75 v3=0.5 v4=0.25 v5=1.0 ; bright block
    //   exp mrt0, v2..v5 done vm    ; merge
    //   s_endpgm
    let code: Vec<u32> = vec![
        0x7c0200f0, 0x7e0402ff, 0x3e800000, 0x7e0602ff, 0x3e800000, 0x7e0802ff, 0x3e800000,
        0x7e0a02ff, 0x3e800000, 0xbf860006, 0x7e0402ff, 0x3f400000, 0x7e0602f0, 0x7e0802ff,
        0x3e800000, 0x7e0a02f2, 0xf800180f, 0x05040302, 0xbf810000,
    ];
    let insts = decode_all(&code);

    // bary_i feeds v0 (the PS launch seeds v0 = bary_i per lane). Lane 0 = 0.75 (>0.5,
    // falls through → bright), lane 1 = 0.25 (<=0.5, takes branch → background), lane 2
    // = 0.75 but EXEC-masked (must not export).
    let mut bary_i = [0.0f32; WAVE_SIZE];
    bary_i[0] = 0.75;
    bary_i[1] = 0.25;
    bary_i[2] = 0.75;
    let exec = 0b011u64; // lanes 0 and 1 live; lane 2 masked off.
    let abi = LaunchAbi::Pixel(Box::new(PixelLaunch {
        user_sgprs: vec![],
        inputs: PsInputs::default(),
        bary_i,
        bary_j: [0.0; WAVE_SIZE],
        exec,
    }));
    let mem = MockMem::new(0, Vec::new());
    let exports = run(&insts, abi, &mem).expect("cbranch PS interp");

    // Lane 0 fell through → bright color.
    let l0 = find_export(&exports, 0, ExportTarget::Mrt(0)).expect("lane 0 mrt0");
    assert_eq!(
        l0.values,
        [0.75, 0.5, 0.25, 1.0],
        "lane 0 took the fall (bright) path"
    );
    // Lane 1 took the branch (skipped the bright block) → pre-seeded background.
    let l1 = find_export(&exports, 1, ExportTarget::Mrt(0)).expect("lane 1 mrt0");
    assert_eq!(
        l1.values,
        [0.25, 0.25, 0.25, 0.25],
        "lane 1 skipped to background"
    );
    // Lane 2 is EXEC-masked: no export despite its v0 > 0.5.
    assert!(
        find_export(&exports, 2, ExportTarget::Mrt(0)).is_none(),
        "EXEC-masked lane 2 must not export"
    );
}

#[test]
fn interp_ps_barycentric_matches_plane_equation() {
    let code = read_code_dwords("interp_color_ps");
    let insts = decode_all(&code);

    // attr0 RGB plane values at the three vertices (P0, P1, P2) per channel.
    let planes: [[f32; 3]; 4] = [
        [0.0, 1.0, 0.0], // R
        [0.0, 0.0, 1.0], // G
        [1.0, 0.0, 0.0], // B
        [0.0, 0.0, 0.0], // unused (alpha comes from v_mov 1.0)
    ];
    let inputs = PsInputs {
        attr_planes: vec![planes],
    };

    // Two live lanes with distinct barycentrics; lane 0 = vertex-0 corner (I=J=0).
    let mut bary_i = [0.0f32; WAVE_SIZE];
    let mut bary_j = [0.0f32; WAVE_SIZE];
    bary_i[0] = 0.0;
    bary_j[0] = 0.0; // -> P0 for every channel
    bary_i[1] = 0.25;
    bary_j[1] = 0.5;

    let exec = 0b11u64;
    let abi = LaunchAbi::Pixel(Box::new(PixelLaunch {
        user_sgprs: vec![],
        inputs,
        bary_i,
        bary_j,
        exec,
    }));

    let mem = MockMem::new(0, Vec::new());
    let exports = run(&insts, abi, &mem).expect("interp PS interp");

    // Plane equation: P0 + I*(P1-P0) + J*(P2-P0), alpha = 1.0.
    let plane_eval = |p: [f32; 3], i: f32, j: f32| p[0] + i * (p[1] - p[0]) + j * (p[2] - p[0]);

    let lane0 = find_export(&exports, 0, ExportTarget::Mrt(0)).expect("lane 0");
    assert_eq!(lane0.values, [0.0, 0.0, 1.0, 1.0], "lane 0 == P0 (R,G,B)");

    let lane1 = find_export(&exports, 1, ExportTarget::Mrt(0)).expect("lane 1");
    let (i, j) = (0.25f32, 0.5f32);
    let want = [
        plane_eval(planes[0], i, j),
        plane_eval(planes[1], i, j),
        plane_eval(planes[2], i, j),
        1.0,
    ];
    assert_eq!(lane1.values, want, "lane 1 interpolated color");
}

// ---- AC #3: loads happen exclusively via the VMM ---------------------------

#[test]
fn vs_loads_go_only_through_the_vmm() {
    let positions = [[0.5, 0.25, 0.125, 1.0]];
    let (mem, desc_addr) = build_vs_memory(&positions);

    let code = read_code_dwords("passthrough_vs");
    let insts = decode_all(&code);
    let abi = LaunchAbi::Vertex {
        user_sgprs: vec![0, 0, desc_addr as u32, (desc_addr >> 32) as u32],
        first_vertex: 0,
        num_lanes: 1,
    };

    let exports = run(&insts, abi, &mem).expect("VS interp");
    // The one live vertex round-trips its position through the mock only.
    let pos = find_export(&exports, 0, ExportTarget::Pos(0)).unwrap();
    assert_eq!(pos.values, positions[0]);

    // Exactly two VMM reads happened: the SMRD V# load, then the MUBUF fetch. No
    // other path could have supplied the bytes (get_host_ptr returns None).
    let reads = mem.reads();
    assert_eq!(
        reads.len(),
        2,
        "expected exactly SMRD + MUBUF reads: {reads:?}"
    );
    assert_eq!(
        reads[0],
        (desc_addr, 16),
        "SMRD s_load_dwordx4 (V#, 16 bytes)"
    );
    // MUBUF fetches vec4 at V#.base for vertex 0.
    assert_eq!(reads[1].1, 16, "MUBUF buffer_load_format_xyzw (16 bytes)");
}

// ---- AC #4: unsupported instruction → structured error, no panic -----------

#[test]
fn unknown_instruction_yields_structured_error_not_panic() {
    // 0xFFFF_FFFF decodes to Inst::Unknown (a garbage dword the decoder can't map).
    let insts = decode_all(&[0xFFFF_FFFF]);
    let abi = LaunchAbi::Vertex {
        user_sgprs: vec![],
        first_vertex: 0,
        num_lanes: 1,
    };
    let mem = MockMem::new(0, Vec::new());

    let err = run(&insts, abi, &mem).expect_err("must reject unknown inst");
    match err {
        InterpError::UnsupportedInst { offset, .. } => assert_eq!(offset, 0),
        other => panic!("expected UnsupportedInst, got {other:?}"),
    }
}

#[test]
fn mubuf_invalid_soffset_255_is_rejected() {
    use ps4_gcn::{Inst, Operand};
    // Hand-build a MUBUF whose soffset field is the invalid 255 marker (Raw(255)).
    // buffer_load_format_x, srsrc valid, soffset = Raw(255).
    let inst = Inst::Mubuf {
        op: 0, // BUFFER_LOAD_FORMAT_X
        vdata: Operand::Vgpr(4),
        vaddr: Operand::Vgpr(0),
        srsrc: 0,
        soffset: Operand::Raw(255),
        offset: 0,
        idxen: true,
        offen: false,
    };
    let decoded = ps4_gcn::Decoded {
        inst,
        size_dwords: 2,
        offset_dwords: 0,
    };
    let abi = LaunchAbi::Vertex {
        user_sgprs: vec![],
        first_vertex: 0,
        num_lanes: 1,
    };
    let mem = MockMem::new(0, Vec::new());
    let err = run(&[decoded], abi, &mem).expect_err("Raw(255) soffset must be rejected");
    assert!(
        matches!(
            err,
            InterpError::InvalidOperand {
                operand: Operand::Raw(255),
                ..
            }
        ),
        "got {err:?}"
    );
}

/// AC #4 (malformed operand, not just unknown opcode): an instruction whose raw
/// register-number field addresses a register past the modeled file must fault with
/// a structured `InvalidRegister`, never panic on an out-of-bounds index. Here an
/// SMRD `sbase` of 126 makes `read_sgpr_u64` reach s126/s127 — well past
/// `NUM_SGPRS` (104) — which previously panicked.
#[test]
fn out_of_range_register_field_yields_structured_error_not_panic() {
    use ps4_gcn::{Inst, NUM_SGPRS, Operand};
    let inst = Inst::Smrd {
        op: 2, // s_load_dwordx4
        sdst: Operand::Sgpr(0),
        sbase: 126, // s[126:127] — out of the modeled SGPR file
        imm: true,
        offset: 0,
    };
    let decoded = ps4_gcn::Decoded {
        inst,
        size_dwords: 1,
        offset_dwords: 0,
    };
    let abi = LaunchAbi::Vertex {
        user_sgprs: vec![],
        first_vertex: 0,
        num_lanes: 1,
    };
    let mem = MockMem::new(0, Vec::new());
    let err = run(&[decoded], abi, &mem).expect_err("out-of-range sbase must be rejected");
    match err {
        InterpError::InvalidRegister { kind, reg, max, .. } => {
            assert_eq!(kind, "sgpr");
            assert_eq!(reg, 126);
            assert_eq!(max, NUM_SGPRS);
        }
        other => panic!("expected InvalidRegister, got {other:?}"),
    }
}

/// The oracle's hardest op checked against an INDEPENDENTLY hand-computed value
/// (not the same plane formula `exec_vintrp` uses, which would let a systematic
/// VINTRP bug pass). Runs the corpus interp PS with chosen exact-in-f32 plane values
/// and I = J = 0.5, and asserts the color literals worked out by hand:
///
///   R: P=[0.25,0.75,1.25]  p1=0.25+0.5·(0.75−0.25)=0.5  p2=0.5+0.5·(1.25−0.25)=1.0
///   G: P=[1.0, 0.5, 0.0]   p1=1.0 +0.5·(0.5 −1.0)=0.75 p2=0.75+0.5·(0.0−1.0)=0.25
///   B: P=[0.0, 0.0, 2.0]   p1=0.0 +0.5·(0.0 −0.0)=0.0  p2=0.0 +0.5·(2.0−0.0)=1.0
///
/// so mrt0 = (1.0, 0.25, 1.0, 1.0). Every step is exact, so this is a bit-exact check.
#[test]
fn interp_ps_matches_independently_computed_color() {
    let code = read_code_dwords("interp_color_ps");
    let insts = decode_all(&code);

    let planes: [[f32; 3]; 4] = [
        [0.25, 0.75, 1.25], // R
        [1.0, 0.5, 0.0],    // G
        [0.0, 0.0, 2.0],    // B
        [0.0, 0.0, 0.0],    // unused (alpha from v_mov 1.0)
    ];
    let inputs = PsInputs {
        attr_planes: vec![planes],
    };

    let mut bary_i = [0.0f32; WAVE_SIZE];
    let mut bary_j = [0.0f32; WAVE_SIZE];
    bary_i[0] = 0.5;
    bary_j[0] = 0.5;

    let abi = LaunchAbi::Pixel(Box::new(PixelLaunch {
        user_sgprs: vec![],
        inputs,
        bary_i,
        bary_j,
        exec: 0b1,
    }));

    let mem = MockMem::new(0, Vec::new());
    let exports = run(&insts, abi, &mem).expect("interp PS interp");

    let lane0 = find_export(&exports, 0, ExportTarget::Mrt(0)).expect("lane 0 mrt0");
    // Hand-computed above — deliberately NOT derived from the plane formula.
    assert_eq!(lane0.values, [1.0, 0.25, 1.0, 1.0]);
}

// ---- AC #1: image_sample point + bilinear filtering (the sampling oracle) ---

/// Build a mock over a linear R8G8B8A8 texture at a 256-byte-aligned base plus the
/// T#/S# SGPR words that address it. The descriptor bit layout is HAND-LAID to match
/// the hardware layout the interpreter's `decode_t_sharp`/`decode_s_sharp` read (not
/// generated by the decoder under test). Returns `(mem, user_sgprs)` where user_sgprs
/// carries s0..s7 = T#, s8..s11 = S#.
fn build_texture(w: u32, h: u32, texels: &[u8], bilinear: bool) -> (MockMem, Vec<u32>) {
    const BASE: u64 = 0x2_0000;
    assert_eq!(texels.len(), (w * h * 4) as usize);
    assert_eq!(BASE & 0xFF, 0, "T# base must be 256-byte aligned");
    let mut t = [0u32; 8];
    t[0] = (BASE >> 8) as u32; // word0 = base >> 8
    t[1] = 10u32 << 20; // dfmt=8_8_8_8 (bits [23:20]); nfmt=UNORM=0
    t[2] = (w - 1) | ((h - 1) << 14); // width-1, height-1
    t[3] = 0; // linear tiling
    let mut s = [0u32; 4];
    if bilinear {
        s[2] = 1 << 20; // filter select
    }
    let mut user = Vec::new();
    user.extend_from_slice(&t);
    user.extend_from_slice(&s);
    (MockMem::new(BASE, texels.to_vec()), user)
}

/// Drive an image_sample-only shader that samples at a fixed UV. The `.s` is:
///   image_sample v[4:7], v[2:3], s[0:7], s[8:11] dmask:0xf ; exp mrt0; s_endpgm
/// but rather than decode a corpus we hand-assemble those two-plus dwords so the test
/// is self-contained. We set v2=u, v3=v via the launch (writing v2/v3 through the
/// pixel ABI is not exposed, so we use the corpus texture_sample_ps and constant UV
/// planes instead — see below).
fn sample_corpus(u: f32, v: f32, tex: &MockMem, user: Vec<u32>) -> [f32; 4] {
    let code = read_code_dwords("texture_sample_ps");
    let insts = decode_all(&code);
    // Constant UV planes so interpolation yields (u, v) for any barycentric.
    let planes: [[f32; 3]; 4] = [[u; 3], [v; 3], [0.0; 3], [0.0; 3]];
    let abi = LaunchAbi::Pixel(Box::new(PixelLaunch {
        user_sgprs: user,
        inputs: PsInputs {
            attr_planes: vec![planes],
        },
        bary_i: [0.5; WAVE_SIZE],
        bary_j: [0.5; WAVE_SIZE],
        exec: 0b1,
    }));
    let exports = run(&insts, abi, tex).expect("image_sample interp");
    find_export(&exports, 0, ExportTarget::Mrt(0))
        .expect("lane 0 mrt0")
        .values
}

#[test]
fn image_sample_point_reads_the_nearest_texel() {
    // 2×2 texture; the four texel colors are distinct so a wrong pick is visible.
    #[rustfmt::skip]
    let texels: [u8; 16] = [
        0,   0,   0,   255,   // (0,0)
        102, 204, 51,  255,   // (1,0)
        10,  20,  30,  255,   // (0,1)
        200, 100, 150, 255,   // (1,1)
    ];
    let (mem, user) = build_texture(2, 2, &texels, false);
    // UV=(0.75, 0.25) → texel space (1.5, 0.5) → floor → texel (1, 0).
    let got = sample_corpus(0.75, 0.25, &mem, user);
    // Expected reasoned from the texture bytes at (1,0): 102/255, 204/255, 51/255, 1.0.
    let want = [102.0 / 255.0, 204.0 / 255.0, 51.0 / 255.0, 1.0];
    assert_eq!(got, want, "point sample must read texel (1,0)");
}

#[test]
fn image_sample_bilinear_averages_four_texels_at_the_center() {
    // 2×2 texture. Sample exactly at the center of the four texels so the bilinear
    // weights are all 0.25 → the result is the arithmetic mean of the four texels.
    // Texel centers sit at +0.5; the four-texel center is at texel coord (1.0, 1.0),
    // i.e. UV (0.5, 0.5) on a 2×2. bilinear samples around (fx-0.5, fy-0.5) = (0.5,0.5)
    // → x0=y0=0, tx=ty=0.5 → mean of texels (0,0),(1,0),(0,1),(1,1).
    #[rustfmt::skip]
    let texels: [u8; 16] = [
        0,   0,   0,   255,   // (0,0)
        100, 0,   0,   255,   // (1,0)
        0,   100, 0,   255,   // (0,1)
        100, 100, 0,   255,   // (1,1)
    ];
    let (mem, user) = build_texture(2, 2, &texels, true);
    let got = sample_corpus(0.5, 0.5, &mem, user);
    // Mean of the four texels, computed by hand from the bytes:
    //   R = (0 + 100 + 0 + 100)/4 = 50   → 50/255
    //   G = (0 + 0 + 100 + 100)/4 = 50   → 50/255
    //   B = 0                             → 0
    //   A = 255                           → 1.0
    let want = [50.0 / 255.0, 50.0 / 255.0, 0.0, 1.0];
    for c in 0..4 {
        assert!(
            (got[c] - want[c]).abs() < 1e-6,
            "bilinear channel {c}: got {} want {}",
            got[c],
            want[c]
        );
    }
}

// ---- task-98: tiled-texture oracle detiles like the gnm upload path ---------

/// Like `sample_corpus` but returns the raw interpreter result so a fault (e.g. an
/// unsupported tiling mode) can be asserted instead of panicking.
fn try_sample_corpus(
    u: f32,
    v: f32,
    tex: &MockMem,
    user: Vec<u32>,
) -> Result<Vec<ExportRecord>, InterpError> {
    let code = read_code_dwords("texture_sample_ps");
    let insts = decode_all(&code);
    let planes: [[f32; 3]; 4] = [[u; 3], [v; 3], [0.0; 3], [0.0; 3]];
    let abi = LaunchAbi::Pixel(Box::new(PixelLaunch {
        user_sgprs: user,
        inputs: PsInputs {
            attr_planes: vec![planes],
        },
        bary_i: [0.5; WAVE_SIZE],
        bary_j: [0.5; WAVE_SIZE],
        exec: 0b1,
    }));
    run(&insts, abi, tex)
}

/// A T# with the given base/extent/tiling index, plus a point-filter S#. Mirrors the
/// descriptor `build_texture` lays down but lets the caller pick the tiling index (the
/// upload path and this oracle must read it the same way).
fn tiled_t_sharp(base: u64, w: u32, h: u32, tiling_index: u8) -> Vec<u32> {
    let mut t = [0u32; 8];
    t[0] = (base >> 8) as u32; // word0 = base >> 8
    t[1] = 10u32 << 20; // dfmt=8_8_8_8, nfmt=UNORM
    t[2] = (w - 1) | ((h - 1) << 14);
    t[3] = (tiling_index as u32) << 20; // word3[24:20] = tiling index
    let s = [0u32; 4]; // point filter
    let mut user = Vec::new();
    user.extend_from_slice(&t);
    user.extend_from_slice(&s);
    user
}

#[test]
fn image_sample_thin1d_reads_the_swizzled_texel_not_the_linear_one() {
    // A 4×4 1D-thin (tiling_index=1) texture. Its bytes are ONE 8×8 micro-tile (256 B),
    // texels Morton-swizzled inside it. The GPU upload path detiles these bytes before
    // sampling; the oracle must apply the SAME swizzle or it reads a different texel.
    //
    // Logical texel (3,0): its micro-tile element is the Morton interleave of x=3 (0b011),
    // y=0 → bits x0,y0,x1 set = 0b000101 = element 5 → byte offset 20. A BUGGY linear
    // read would instead use index y*4+x = 3 → byte offset 12. Place a marker at 20 and a
    // decoy at 12: a correct oracle returns the marker, the old linear oracle the decoy.
    const BASE: u64 = 0x2_0000;
    let mut bytes = vec![0u8; 256]; // one padded 8×8 micro-tile, RGBA8
    bytes[20..24].copy_from_slice(&[11, 22, 33, 44]); // swizzled (3,0) — correct
    bytes[12..16].copy_from_slice(&[200, 200, 200, 200]); // linear (3,0) — decoy
    let mem = MockMem::new(BASE, bytes);
    let user = tiled_t_sharp(BASE, 4, 4, 1);

    // UV=(0.875, 0.125) → texel space (3.5, 0.5) → point floor → texel (3, 0).
    let exports = try_sample_corpus(0.875, 0.125, &mem, user).expect("thin1d image_sample");
    let got = find_export(&exports, 0, ExportTarget::Mrt(0))
        .expect("lane 0 mrt0")
        .values;
    let want = [11.0 / 255.0, 22.0 / 255.0, 33.0 / 255.0, 44.0 / 255.0];
    assert_eq!(
        got, want,
        "thin1d oracle must read the swizzled texel (offset 20), not the linear one (12)"
    );
}

#[test]
fn image_sample_macro_tiled_faults_instead_of_mis_detiling() {
    // A genuine 2D macro-tiled texture (tiling_index=9, GB_TILE_MODE9=1D/2D-tiled — but
    // classified Macro2d in this subset) has no detiler; the GPU path defers such a draw,
    // so the oracle must fault rather than silently read the bytes as 1D-thin (task-98 AC#2
    // — no silent mis-detile). Index 8 is now handled (linear-aligned), so this uses 9.
    const BASE: u64 = 0x2_0000;
    let mem = MockMem::new(BASE, vec![0u8; 256]);
    let user = tiled_t_sharp(BASE, 4, 4, 9);
    let err = try_sample_corpus(0.5, 0.5, &mem, user).expect_err("macro tiling must fault");
    assert!(
        matches!(
            err,
            InterpError::UnsupportedTiling {
                tiling_index: 9,
                ..
            }
        ),
        "expected UnsupportedTiling, got {err:?}"
    );
}

#[test]
fn image_sample_linear_aligned_reads_over_the_padded_pitch() {
    // A linear-aligned (tiling_index=8) surface is row-major but its row PITCH is padded up
    // to align(width, 64) texels (task-153). For a 100-wide surface the pitch is 128, so
    // logical texel (0,1) lives at byte (1*128 + 0)*4 = 512, NOT (1*100)*4 = 400 as a naive
    // tight-linear read would use. Place the correct texel at 512 and a decoy at 400: the
    // oracle must apply the padded pitch and return the marker, matching the upload detiler.
    const BASE: u64 = 0x2_0000;
    let pitch = 128usize; // align(100, 64)
    let mut bytes = vec![0u8; pitch * 2 * 4]; // two padded rows
    bytes[512..516].copy_from_slice(&[11, 22, 33, 44]); // (0,1) at padded pitch — correct
    bytes[400..404].copy_from_slice(&[200, 200, 200, 200]); // (0,1) tight-linear — decoy
    let mem = MockMem::new(BASE, bytes);
    let user = tiled_t_sharp(BASE, 100, 2, 8);

    // UV=(0.0, 0.75) → texel space (0.0, 1.5) → point floor → texel (0, 1).
    let exports = try_sample_corpus(0.0, 0.75, &mem, user).expect("linear-aligned image_sample");
    let got = find_export(&exports, 0, ExportTarget::Mrt(0))
        .expect("lane 0 mrt0")
        .values;
    let want = [11.0 / 255.0, 22.0 / 255.0, 33.0 / 255.0, 44.0 / 255.0];
    assert_eq!(
        got, want,
        "linear-aligned oracle must stride the padded pitch (offset 512), not width (400)"
    );
}
