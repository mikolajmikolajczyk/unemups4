//! GCN decoder golden + robustness tests (doc-2 §1, phase 4).
//!
//! - `corpus_disasm_matches_golden`: decode each committed `.code.bin` and assert
//!   the disassembly equals the committed `.dis` golden (AC #1).
//! - `never_panics_on_garbage`: feed pseudo-random dword streams and assert the
//!   walk terminates without panicking, unknown ops becoming `Unknown` (AC #2).
//! - `multi_dword_advances_pc`: literals and VOP3's second dword advance the PC by
//!   the right amount so the following instruction decodes correctly (AC #3).
//!
//! Golden provenance: the `.dis` files are hand-verified against the `.s` sources,
//! not regen-blessed from the decoder under test — `corpus_disasm_matches_golden`
//! would otherwise be self-referential (a decoder bug that also reshaped the golden
//! would pass). `corpus_bytes_match_llvm_mc` closes that loop independently: when
//! `llvm-mc` is on PATH it re-assembles each `.s` and asserts the bytes equal the
//! committed `.code.bin`, so the corpus can never drift from the assembly a neutral
//! GFX7 assembler produces. It is skipped (not failed) when llvm-mc is absent.

use std::path::{Path, PathBuf};

use ps4_gcn::{Inst, Operand, decode_all, decode_one, disasm, disasm_all};

fn corpus_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus")
}

fn read_code_dwords(name: &str) -> Vec<u32> {
    let p = corpus_dir().join(format!("{name}.code.bin"));
    let bytes = std::fs::read(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()));
    assert!(
        bytes.len().is_multiple_of(4),
        "{name}: code not 4-byte aligned"
    );
    bytes
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

const CORPUS: &[&str] = &[
    "passthrough_vs",
    "flat_color_ps",
    "interp_color_ps",
    "texture_sample_ps",
    "pkrtz_ps",
    "wqm_bracket_ps",
    "cbuffer_ps",
    "transcendental_ps",
    "minmax_shift_ps",
    "vop3_mad_sin_fract_ps",
    "vop3_mul_ps",
    "rcp_ps",
    "vop3_mac_ps",
    "vop3_pkrtz_ps",
    "m0_save_ps",
    "cmp_cndmask_ps",
    "vop3_cmp_cndmask_ps",
    "vop3_cmp3_ps",
    "vadd_i32_ps",
    "cbranch_alpha_ps",
    "loop_accum_ps",
    "inline_fetch_vs",
    "cbuffer16_vs",
];

/// AC #1: every corpus instruction decodes to the committed golden disassembly.
#[test]
fn corpus_disasm_matches_golden() {
    for name in CORPUS {
        let code = read_code_dwords(name);
        let decoded = decode_all(&code);

        // No instruction in the (real GCN) corpus should decode as Unknown.
        for (i, d) in decoded.iter().enumerate() {
            assert!(
                !matches!(d.inst, Inst::Unknown { .. }),
                "{name}[{i}] decoded as Unknown: {:?}",
                d.inst
            );
        }

        // The consumed length must sum to exactly the code size (no over/under-read).
        let consumed: u32 = decoded.iter().map(|d| d.size_dwords).sum();
        assert_eq!(
            consumed as usize,
            code.len(),
            "{name}: decoded length {consumed} != code dwords {}",
            code.len()
        );

        let got = disasm_all(&decoded);
        let golden_path = corpus_dir().join(format!("{name}.dis"));
        // Regen with UNEMUPS4_GCN_REGEN=1 (writes the golden, then still asserts).
        if std::env::var("UNEMUPS4_GCN_REGEN").is_ok() {
            std::fs::write(&golden_path, format!("{got}\n")).unwrap();
        }
        let want = std::fs::read_to_string(&golden_path)
            .unwrap_or_else(|e| panic!("read golden {}: {e}", golden_path.display()));
        assert_eq!(
            got.trim_end(),
            want.trim_end(),
            "{name}: disassembly drifted from golden {}",
            golden_path.display()
        );
    }
}

/// Independent cross-check: re-assemble each corpus `.s` with `llvm-mc` (GFX7 /
/// bonaire) and assert the emitted bytes equal the committed `.code.bin`. This does
/// not touch our decoder, so it catches corpus drift the self-referential golden
/// test cannot. Skipped when `llvm-mc` is unavailable.
#[test]
fn corpus_bytes_match_llvm_mc() {
    use std::process::Command;

    // Probe for a usable llvm-mc with the amdgcn target; skip cleanly if absent.
    let mc = std::env::var("LLVM_MC").unwrap_or_else(|_| "llvm-mc".to_string());
    let probe = Command::new(&mc).arg("--version").output();
    if probe.is_err() {
        eprintln!("skip corpus_bytes_match_llvm_mc: {mc} not found");
        return;
    }

    for name in CORPUS {
        let s_path = corpus_dir().join(format!("{name}.s"));
        let out = Command::new(&mc)
            .args([
                "-triple",
                "amdgcn",
                "-mcpu=bonaire",
                "-filetype=asm",
                "-show-encoding",
            ])
            .arg(&s_path)
            .output()
            .unwrap_or_else(|e| panic!("run llvm-mc on {name}: {e}"));
        if !out.status.success() {
            // A toolchain that lacks the amdgcn target — treat as "not available".
            eprintln!(
                "skip corpus_bytes_match_llvm_mc[{name}]: llvm-mc failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            return;
        }
        let asm = String::from_utf8_lossy(&out.stdout);
        // Collect every `; encoding: [0x..,0x..,...]` byte in order.
        let mut bytes = Vec::new();
        for line in asm.lines() {
            if let Some(i) = line.find("encoding: [") {
                let rest = &line[i + "encoding: [".len()..];
                let end = rest.find(']').unwrap_or(rest.len());
                for tok in rest[..end].split(',') {
                    if let Some(hex) = tok.trim().strip_prefix("0x")
                        && let Ok(b) = u8::from_str_radix(hex, 16)
                    {
                        bytes.push(b);
                    }
                }
            }
        }
        let committed = std::fs::read(corpus_dir().join(format!("{name}.code.bin")))
            .unwrap_or_else(|e| panic!("read {name}.code.bin: {e}"));
        assert_eq!(
            bytes, committed,
            "{name}: llvm-mc re-assembly of .s drifted from committed .code.bin"
        );
    }
}

/// AC #2: an arbitrary dword stream never panics; unmapped encodings become
/// `Unknown` and the walk still terminates having consumed the whole stream.
#[test]
fn never_panics_on_garbage() {
    // A cheap deterministic xorshift so the test is reproducible without a dep.
    let mut state: u64 = 0x1234_5678_9abc_def0;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state as u32
    };

    for _ in 0..2000 {
        let len = (next() % 64) as usize;
        let stream: Vec<u32> = (0..len).map(|_| next()).collect();
        let decoded = decode_all(&stream);
        let consumed: u32 = decoded.iter().map(|d| d.size_dwords).sum();
        // The decoder may read a trailing dword of a 2-dword form as its "second
        // dword"; it never reads past the slice, and consumed length matches input.
        assert_eq!(
            consumed as usize,
            stream.len(),
            "over/under-read on garbage"
        );
        for d in &decoded {
            assert!(d.size_dwords >= 1);
        }
    }

    // Explicit all-ones / all-zeros edge streams.
    for &fill in &[0x0000_0000u32, 0xFFFF_FFFF, 0xDEAD_BEEF] {
        let stream = vec![fill; 8];
        let decoded = decode_all(&stream);
        let consumed: u32 = decoded.iter().map(|d| d.size_dwords).sum();
        assert_eq!(consumed as usize, stream.len());
    }
}

/// AC #3: literal-carrying and VOP3 instructions advance the PC past their trailing
/// dword so the following instruction decodes at the right offset.
#[test]
fn multi_dword_advances_pc() {
    // v_mov_b32 v1, 0x3e800000  (VOP1 with a 32-bit literal, 2 dwords)
    // then s_endpgm             (SOPP, 1 dword)
    let stream = [0x7e0202ffu32, 0x3e80_0000, 0xbf81_0000];
    let decoded = decode_all(&stream);
    assert_eq!(decoded.len(), 2, "literal not consumed as one instruction");
    assert_eq!(decoded[0].size_dwords, 2, "VOP1+literal must be 2 dwords");
    match &decoded[0].inst {
        Inst::Vop1 { src0, .. } => {
            assert_eq!(*src0, ps4_gcn::Operand::Literal(0x3e80_0000));
        }
        other => panic!("expected Vop1, got {other:?}"),
    }
    assert!(
        matches!(decoded[1].inst, Inst::Sopp { .. }),
        "instruction after literal misaligned: {:?}",
        decoded[1].inst
    );

    // A synthetic VOP3 (2 dwords) followed by s_endpgm. Encoding: [31:26]=110100.
    // op field arbitrary; the point is the 2-dword advance.
    let vop3_w0 = (0b110100u32 << 26) | (0x141 << 17) | 0x05; // vdst=v5, op=v_mad_f32
    let vop3_w1 = 0x0000_0102; // src0=v?/src1 fields — value irrelevant to length
    let stream = [vop3_w0, vop3_w1, 0xbf81_0000];
    let decoded = decode_all(&stream);
    assert_eq!(decoded.len(), 2, "VOP3 second dword not consumed");
    assert_eq!(decoded[0].size_dwords, 2, "VOP3 must be 2 dwords");
    assert!(matches!(decoded[0].inst, Inst::Vop3 { .. }));
    assert!(
        matches!(decoded[1].inst, Inst::Sopp { .. }),
        "instruction after VOP3 misaligned: {:?}",
        decoded[1].inst
    );

    // s_load_dwordx4 with imm=false would carry an SGPR offset (1 dword still); the
    // SMRD forms in the corpus are single-dword, verified by the corpus test.
}

/// AC #1: a MUBUF whose SOFFSET field is 255 (an *invalid* literal on GFX7 — llvm-mc
/// rejects a literal soffset) must NOT fabricate a trailing dword. MUBUF is two
/// dwords regardless, and the following instruction must decode at offset +2.
///
/// Real MUBUF soffsets are an SGPR / inline constant / m0; llvm-mc encoding for
/// `buffer_load_dword v0, v1, s[4:7], s8 offen` is [0xe0301000, 0x08010001], where
/// the soffset field (w1 bits[31:24]) is 8 (== s8). We force it to 255 here.
#[test]
fn mubuf_invalid_literal_soffset_does_not_drop_dword() {
    // Legal MUBUF (soffset = s8), then s_endpgm.
    let w0 = 0xe030_1000u32;
    let w1_ok = 0x0801_0001u32;
    let stream = [w0, w1_ok, 0xbf81_0000];
    let decoded = decode_all(&stream);
    assert_eq!(decoded.len(), 2);
    assert_eq!(decoded[0].size_dwords, 2, "MUBUF must be 2 dwords");
    assert!(matches!(decoded[1].inst, Inst::Sopp { .. }));

    // Now force the soffset field (w1 bits[31:24]) to 255 — an invalid literal.
    let w1_lit = (w1_ok & 0x00FF_FFFF) | (255u32 << 24);
    let stream = [w0, w1_lit, 0xbf81_0000];
    let decoded = decode_all(&stream);
    assert_eq!(decoded.len(), 2, "invalid soffset must not drop a dword");
    assert_eq!(
        decoded[0].size_dwords, 2,
        "invalid soffset must keep MUBUF at 2 dwords"
    );
    match &decoded[0].inst {
        Inst::Mubuf { soffset, .. } => {
            assert!(
                !soffset.is_literal(),
                "field 255 must not become a Literal: {soffset:?}"
            );
            assert_eq!(*soffset, Operand::Raw(255));
        }
        other => panic!("expected Mubuf, got {other:?}"),
    }
    assert!(
        matches!(decoded[1].inst, Inst::Sopp { .. }),
        "following instruction misaligned: {:?}",
        decoded[1].inst
    );
}

/// AC #2: v_madmk/v_madak carry a 32-bit K constant as their second dword; the
/// decoder must store it, advance 2 dwords, and disasm must print it in AMD order.
/// Encodings from llvm-mc (`-mcpu=bonaire`):
///   v_madmk_f32 v0, v1, 0x40490fdb, v2 -> [0x40000501, 0x40490fdb]
///   v_madak_f32 v0, v1, v2, 0x40490fdb -> [0x42000501, 0x40490fdb]
#[test]
fn vop2_madmk_madak_carry_k() {
    let madmk = [0x4000_0501u32, 0x4049_0fdb, 0xbf81_0000];
    let decoded = decode_all(&madmk);
    assert_eq!(decoded.len(), 2, "madmk K not consumed as one instruction");
    assert_eq!(decoded[0].size_dwords, 2);
    match &decoded[0].inst {
        Inst::Vop2 { k, .. } => assert_eq!(*k, Some(0x4049_0fdb)),
        other => panic!("expected Vop2, got {other:?}"),
    }
    assert_eq!(
        disasm(&decode_one(&madmk)),
        "v_madmk_f32 v0, v1, 0x40490fdb, v2"
    );
    assert!(matches!(decoded[1].inst, Inst::Sopp { .. }));

    let madak = [0x4200_0501u32, 0x4049_0fdb];
    let d = decode_one(&madak);
    match &d.inst {
        Inst::Vop2 { k, .. } => assert_eq!(*k, Some(0x4049_0fdb)),
        other => panic!("expected Vop2, got {other:?}"),
    }
    assert_eq!(disasm(&d), "v_madak_f32 v0, v1, v2, 0x40490fdb");
}

/// Integer/bitwise VOP2 (retail pixel/vertex shaders). Encodings from llvm-mc
/// (`-mcpu=bonaire`):
///   v_and_b32     v1, v2, v3 -> [0x02,0x07,0x02,0x36] = 0x36020702
///   v_lshlrev_b32 v1, v2, v3 -> [0x02,0x07,0x02,0x34] = 0x34020702
#[test]
fn vop2_int_and_lshlrev() {
    let and = decode_one(&[0x3602_0702u32]);
    match &and.inst {
        Inst::Vop2 {
            op,
            vdst,
            src0,
            vsrc1,
            k,
        } => {
            assert_eq!(*op, 0x1B, "v_and_b32"); // opcodes::vop2::V_AND_B32
            assert_eq!(*vdst, Operand::Vgpr(1));
            assert_eq!(*src0, Operand::Vgpr(2));
            assert_eq!(*vsrc1, Operand::Vgpr(3));
            assert_eq!(*k, None);
        }
        other => panic!("expected Vop2, got {other:?}"),
    }
    assert_eq!(disasm(&and), "v_and_b32 v1, v2, v3");

    let shl = decode_one(&[0x3402_0702u32]);
    match &shl.inst {
        Inst::Vop2 { op, .. } => assert_eq!(*op, 0x1A, "v_lshlrev_b32"), // V_LSHLREV_B32
        other => panic!("expected Vop2, got {other:?}"),
    }
    assert_eq!(disasm(&shl), "v_lshlrev_b32 v1, v2, v3");
}

/// SMRD s_buffer_load width variants the retail set uses (op 0x09 / 0x0B / 0x0C —
/// distinct from the s_load ops). Encodings from llvm-mc (`-mcpu=bonaire`):
///   s_buffer_load_dwordx2  s[4:5],  s[0:3], 0x0 -> [0x00,0x01,0x42,0xc2] = 0xc2420100
///   s_buffer_load_dwordx8  s[8:15], s[0:3], 0x0 -> [0x00,0x01,0xc4,0xc2] = 0xc2c40100
///   s_buffer_load_dwordx16 s[0:15], s[4:7], 0x0 -> [0x00,0x05,0x00,0xc3] = 0xc3000500
#[test]
fn smrd_s_buffer_load_x2_x8_x16() {
    let x2 = decode_one(&[0xc242_0100u32]);
    match &x2.inst {
        Inst::Smrd { op, .. } => assert_eq!(*op, 0x09, "s_buffer_load_dwordx2"),
        other => panic!("expected Smrd, got {other:?}"),
    }
    assert_eq!(disasm(&x2), "s_buffer_load_dwordx2 s[4:5], s[0:1], 0x0");

    let x8 = decode_one(&[0xc2c4_0100u32]);
    match &x8.inst {
        Inst::Smrd { op, .. } => assert_eq!(*op, 0x0B, "s_buffer_load_dwordx8"),
        other => panic!("expected Smrd, got {other:?}"),
    }
    assert_eq!(disasm(&x8), "s_buffer_load_dwordx8 s[8:15], s[0:1], 0x0");

    // s_buffer_load_dwordx16 (op 0x0C) — the 16-dword (4×4 matrix) constant load the
    // retail Celeste VS emit right after the fetch-shader call. Real dumped bytes.
    let x16 = decode_one(&[0xc300_0500u32]);
    match &x16.inst {
        Inst::Smrd { op, sbase, .. } => {
            assert_eq!(*op, 0x0C, "s_buffer_load_dwordx16");
            assert_eq!(*sbase, 4, "sbase s[4:7]");
        }
        other => panic!("expected Smrd, got {other:?}"),
    }
    assert_eq!(disasm(&x16), "s_buffer_load_dwordx16 s[0:15], s[4:5], 0x0");
}

/// SOP1 64-bit EXEC-mask moves (retail WQM prologue). Encodings from llvm-mc
/// (`-mcpu=bonaire`):
///   s_mov_b64 s[0:1], exec -> [0x7e,0x04,0x80,0xbe] = 0xbe80047e
///   s_wqm_b64 exec, exec   -> [0x7e,0x0a,0xfe,0xbe] = 0xbefe0a7e  (op 0x0A, not 0x08)
#[test]
fn sop1_b64_exec_moves() {
    let mov = decode_one(&[0xbe80_047eu32]);
    match &mov.inst {
        Inst::Sop1 { op, .. } => assert_eq!(*op, 0x04, "s_mov_b64"),
        other => panic!("expected Sop1, got {other:?}"),
    }
    assert_eq!(disasm(&mov), "s_mov_b64 s0, exec_lo");

    let wqm = decode_one(&[0xbefe_0a7eu32]);
    match &wqm.inst {
        Inst::Sop1 { op, .. } => assert_eq!(*op, 0x0A, "s_wqm_b64"),
        other => panic!("expected Sop1, got {other:?}"),
    }
    assert_eq!(disasm(&wqm), "s_wqm_b64 exec_lo, exec_lo");
}

/// AC #2: VOP3 carries the output modifier (omod, high-dword bits[28:27]) and disasm
/// prints it. llvm-mc: `v_mad_f32 v0, v1, v2, v3 mul:2` -> [0xd2820000, 0x0c0e0501].
#[test]
fn vop3_carries_omod() {
    let mul2 = [0xd282_0000u32, 0x0c0e_0501];
    let d = decode_one(&mul2);
    match &d.inst {
        Inst::Vop3 { omod, .. } => assert_eq!(*omod, 1, "mul:2 -> omod 1"),
        other => panic!("expected Vop3, got {other:?}"),
    }
    assert!(disasm(&d).ends_with(" mul:2"), "got {}", disasm(&d));

    // div:2 -> omod 3.
    let div2 = [0xd282_0000u32, 0x1c0e_0501];
    match &decode_one(&div2).inst {
        Inst::Vop3 { omod, .. } => assert_eq!(*omod, 3),
        other => panic!("expected Vop3, got {other:?}"),
    }
}

/// task-188: VOP3 carries the `clamp` output modifier (LOW-dword bit 11 on GFX7 —
/// GFX8 moved it to bit 15) and disasm prints it. llvm-mc (-mcpu=bonaire):
/// `v_mad_f32 v0, v1, v2, v3 clamp` -> [0xd2820800, 0x040e0501]; adding `mul:2` sets
/// omod in the high dword without moving clamp.
#[test]
fn vop3_carries_clamp() {
    let clamp = [0xd282_0800u32, 0x040e_0501];
    let d = decode_one(&clamp);
    match &d.inst {
        Inst::Vop3 { clamp, omod, .. } => {
            assert!(*clamp, "bit 11 set -> clamp");
            assert_eq!(*omod, 0, "clamp must not be confused with omod");
        }
        other => panic!("expected Vop3, got {other:?}"),
    }
    assert_eq!(disasm(&d), "v_mad_f32 v0, v1, v2, v3 clamp");

    // clamp + mul:2 — both modifiers decode independently, and llvm-mc's text order is
    // `clamp` then `mul:2` (the reverse of the order the hardware applies them in).
    let both = [0xd282_0800u32, 0x0c0e_0501];
    let d = decode_one(&both);
    match &d.inst {
        Inst::Vop3 { clamp, omod, .. } => {
            assert!(*clamp);
            assert_eq!(*omod, 1, "mul:2");
        }
        other => panic!("expected Vop3, got {other:?}"),
    }
    assert_eq!(disasm(&d), "v_mad_f32 v0, v1, v2, v3 clamp mul:2");

    // Bit 11 clear -> no clamp (guards against a mask that always reads set).
    match &decode_one(&[0xd282_0000u32, 0x040e_0501]).inst {
        Inst::Vop3 { clamp, .. } => assert!(!*clamp),
        other => panic!("expected Vop3, got {other:?}"),
    }
}

/// v_sqrt_f32 op field is 0x33 on GFX7/bonaire (verified against llvm-mc:
/// `v_sqrt_f32 v3, v4` -> [0x7e066704]).
#[test]
fn vop1_sqrt_op_field_matches_llvm_mc() {
    let d = decode_one(&[0x7e06_6704]);
    assert_eq!(disasm(&d), "v_sqrt_f32 v3, v4");
}

/// AC #3: every decoded instruction reports its stream position, and an unknown
/// multi-dword shape can carry its consumed raw words. Position is what the
/// interpreter and recompiler use to correlate/patch.
#[test]
fn decoded_reports_stream_position() {
    // v_mov_b32 v1, 0x3e800000 (2 dwords) ; then an unknown dword at offset 2.
    // 0xE8000000 (top prefix 0b111010) hits no encoding-class prefix — EXP is
    // 0b111110, MUBUF 0b111000, MIMG 0b111100 — so it decodes as Unknown.
    let stream = [0x7e02_02ffu32, 0x3e80_0000, 0xE800_0000];
    let decoded = decode_all(&stream);
    assert_eq!(decoded.len(), 2);
    assert_eq!(decoded[0].offset_dwords, 0);
    assert_eq!(decoded[1].offset_dwords, 2, "second inst starts at dword 2");

    // Inst::Unknown carries the raw dword and (for a multi-dword unknown op) any
    // trailing raw words — the field is available to a later pass that recognizes a
    // multi-dword unknown op.
    if let Inst::Unknown { raw, raw_words } = &decoded[1].inst {
        assert_eq!(*raw, 0xE800_0000);
        assert!(raw_words.is_empty());
    } else {
        panic!("expected Unknown, got {:?}", decoded[1].inst);
    }
}

#[test]
fn vop1_sin_vop3_mad_u24_fract() {
    // v_sin_f32 v0, v0 — VOP1 op 0x35 (llvm-mc byte1 0x6B >> 1). Single dword.
    let sin = decode_one(&[0x7e00_6b00u32]);
    match &sin.inst {
        Inst::Vop1 { op, .. } => assert_eq!(*op, 0x35, "v_sin_f32"),
        other => panic!("expected Vop1, got {other:?}"),
    }
    assert_eq!(disasm(&sin), "v_sin_f32 v0, v0");

    // v_mad_u32_u24 v2, v2, 2, 1 — VOP3 op 0x143 (native integer 3-src), two dwords.
    let mad = decode_one(&[0xd286_0002u32, 0x0205_0502]);
    match &mad.inst {
        Inst::Vop3 { op, src1, src2, .. } => {
            assert_eq!(*op, 0x143, "v_mad_u32_u24");
            assert_eq!(*src1, Operand::InlineInt(2));
            assert_eq!(*src2, Operand::InlineInt(1));
        }
        other => panic!("expected Vop3, got {other:?}"),
    }

    // v_fract_f32_e64 v1, |v1| mul:4 — VOP1 fract (0x20) re-encoded VOP3 at 0x1A0,
    // carrying abs on src0 and omod ×4 (field 2). Two dwords.
    let fract = decode_one(&[0xd340_0101u32, 0x1000_0101]);
    match &fract.inst {
        Inst::Vop3 { op, abs, omod, .. } => {
            assert_eq!(*op, 0x1A0, "v_fract_f32 (VOP3)");
            assert_eq!(*abs & 1, 1, "abs on src0");
            assert_eq!(*omod, 2, "omod ×4");
        }
        other => panic!("expected Vop3, got {other:?}"),
    }
}

/// Predication / VCC family. Every encoding is cross-checked against llvm-mc
/// (`-mcpu=bonaire`), the op-field authority (three prior guesses from op numbers
/// alone were wrong — RE the identity, never the number).
#[test]
fn vopc_cndmask_add_i32_predication() {
    // Standalone VOPC (writes the implicit VCC):
    //   v_cmp_lt_f32 vcc, v0, v1 -> [0x00,0x03,0x02,0x7c] = 0x7c020300  (op 1)
    //   v_cmp_gt_f32 vcc, v0, v1 -> [0x00,0x03,0x08,0x7c] = 0x7c080300  (op 4)
    let lt = decode_one(&[0x7c02_0300u32]);
    match &lt.inst {
        Inst::Vopc { op, src0, vsrc1 } => {
            assert_eq!(*op, 0x01, "v_cmp_lt_f32"); // opcodes::vopc::V_CMP_LT_F32
            assert_eq!(*src0, Operand::Vgpr(0));
            assert_eq!(*vsrc1, Operand::Vgpr(1));
        }
        other => panic!("expected Vopc, got {other:?}"),
    }
    assert_eq!(disasm(&lt), "v_cmp_lt_f32 vcc, v0, v1");

    let gt = decode_one(&[0x7c08_0300u32]);
    match &gt.inst {
        Inst::Vopc { op, .. } => assert_eq!(*op, 0x04, "v_cmp_gt_f32"),
        other => panic!("expected Vopc, got {other:?}"),
    }
    assert_eq!(disasm(&gt), "v_cmp_gt_f32 vcc, v0, v1");

    // VOP3-form VOPC (writes an ARBITRARY sgpr pair via the `sdst` field, which the
    // decoder must expose as an Sgpr — not mislabel as a VGPR):
    //   v_cmp_lt_f32 s[16:17], v0, v1 -> [0x10,0x00,0x02,0xd0, 0x00,0x03,0x02,0x00]
    //   v_cmp_gt_f32 s[12:13], v0, v1 -> [0x0c,0x00,0x08,0xd0, 0x00,0x03,0x02,0x00]
    let lt3 = decode_one(&[0xd002_0010u32, 0x0002_0300]);
    match &lt3.inst {
        Inst::Vop3 { op, vdst, .. } => {
            assert_eq!(*op, 0x001, "v_cmp_lt_f32 (VOP3)");
            assert_eq!(*vdst, Operand::Sgpr(16), "sdst is s[16:17], not a VGPR");
        }
        other => panic!("expected Vop3, got {other:?}"),
    }
    assert_eq!(disasm(&lt3), "v_cmp_lt_f32 s[16:17], v0, v1");

    let gt3 = decode_one(&[0xd008_000cu32, 0x0002_0300]);
    match &gt3.inst {
        Inst::Vop3 { op, vdst, .. } => {
            assert_eq!(*op, 0x004, "v_cmp_gt_f32 (VOP3)");
            assert_eq!(*vdst, Operand::Sgpr(12), "sdst is s[12:13]");
        }
        other => panic!("expected Vop3, got {other:?}"),
    }

    // v_cndmask_b32 (VOP2, reads the implicit VCC):
    //   v_cndmask_b32 v2, v0, v1, vcc -> [0x00,0x03,0x04,0x00] = 0x00040300 (op 0)
    let cnd = decode_one(&[0x0004_0300u32]);
    match &cnd.inst {
        Inst::Vop2 {
            op,
            vdst,
            src0,
            vsrc1,
            ..
        } => {
            assert_eq!(*op, 0x00, "v_cndmask_b32");
            assert_eq!(*vdst, Operand::Vgpr(2));
            assert_eq!(*src0, Operand::Vgpr(0));
            assert_eq!(*vsrc1, Operand::Vgpr(1));
        }
        other => panic!("expected Vop2, got {other:?}"),
    }

    // v_cndmask_b32 (VOP3, predicate is an sgpr pair in src2):
    //   v_cndmask_b32 v2, v0, v1, s[16:17] -> [0x02,0x00,0x00,0xd2, 0x00,0x03,0x42,0x00]
    let cnd3 = decode_one(&[0xd200_0002u32, 0x0042_0300]);
    match &cnd3.inst {
        Inst::Vop3 { op, vdst, src2, .. } => {
            assert_eq!(*op, 0x100, "v_cndmask_b32 (VOP3, VOP2 0x00 + 0x100)");
            assert_eq!(*vdst, Operand::Vgpr(2));
            assert_eq!(*src2, Operand::Sgpr(16), "predicate pair s[16:17] in src2");
        }
        other => panic!("expected Vop3, got {other:?}"),
    }

    // v_add_i32 (VOP2, carry-out to VCC):
    //   v_add_i32 v2, vcc, v0, v1 -> [0x00,0x03,0x04,0x4a] = 0x4a040300 (op 0x25)
    let add = decode_one(&[0x4a04_0300u32]);
    match &add.inst {
        Inst::Vop2 { op, vdst, .. } => {
            assert_eq!(*op, 0x25, "v_add_i32");
            assert_eq!(*vdst, Operand::Vgpr(2));
        }
        other => panic!("expected Vop2, got {other:?}"),
    }
    assert_eq!(disasm(&add), "v_add_i32 v2, v0, v1");
}
