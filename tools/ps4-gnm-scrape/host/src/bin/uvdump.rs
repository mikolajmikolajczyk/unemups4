//! THROWAWAY (task-171 B1-vs-B2 oracle): decode the sprite UVs out of the real-PS4
//! KIND_VBUF captures so the maintainer can prove whether real hardware feeds
//! FRACTIONAL sub-rect UVs or whole-atlas CORNER UVs at Celeste's broken auto-reached
//! scene (frame window ~1600–1800, no input).
//!
//! ## What this reads
//!
//! The `receiver` writes two kinds of `.bin` under the dump dir:
//! * DCB/CCB command buffers  — `frameNNNNNN_subN_<kind>_<dcb|ccb>.bin`
//! * KIND_VBUF buffer CONTENT — `frameNNNNNN_bufNN_0xBASE_LEN.bin`
//!   (the receiver already stripped the 8-byte LE guest-base prefix and put the
//!   base in the filename; the file body is the raw referenced buffer bytes).
//!
//! A VBUF content file carries no stride on its own — the stride lives in the V#
//! (vertex descriptor) the draw binds. So this bin first scans every DCB for V#s
//! (the exact `ps4_gnm::vbuf::decode_v_sharp` window scan `vref` uses), building a
//! `base -> {(stride, num_records)}` map, then for each VBUF file whose correlated
//! stride is **24** — Celeste's UI sprite record
//! `{posX f32, posY f32, z f32, color u32, u f32, v f32}` (UV at offset 16) — decodes
//! the UV of the first ~8 vertices and flags whether they are exact atlas CORNERS
//! (0/1) or FRACTIONAL sub-rect coordinates.
//!
//! If a VBUF base has no correlated V# in the captured DCBs but its length is a
//! multiple of 24, the UV is still decoded with an *assumed* stride 24 (marked `24?`)
//! so a scrape that missed the owning DCB still yields an answer. Pass `--all` to
//! also dump every non-24 correlated buffer's descriptor for context.
//!
//! ## Usage
//!
//!   uvdump [DUMP_DIR] [--all]
//!     DUMP_DIR   directory of receiver dumps (default: ./dumps)
//!     --all      also list buffers whose stride != 24 (descriptor only, no UV)
//!
//! VERDICT recipe: real-PS4 uvdump shows FRACTIONAL at the same sprite draw ⇒ our
//! Mono/managed runtime diverged (it emitted corners where real HW has sub-rects) ⇒
//! B2 is a managed-runtime divergence; CORNERS on real HW ⇒ not a divergence (real
//! Celeste itself emits corners there, the bug is elsewhere).

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use ps4_gnm::vbuf::decode_v_sharp;

/// Real-HW guest heap band (matches `vref`): bufs ~0x2xx, atlas ~0x9xx, all >4 GiB.
const HEAP_LO: u64 = 0x1_0000_0000;
const HEAP_HI: u64 = 0x10_0000_0000;

fn in_heap(v: u64) -> bool {
    (HEAP_LO..HEAP_HI).contains(&v)
}

/// A parsed VBUF content file: guest base (from filename) + on-disk path + size.
struct VbufFile {
    base: u64,
    path: PathBuf,
    len: usize,
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mut dir = None;
    let mut show_all = false;
    for a in args.by_ref() {
        match a.as_str() {
            "--all" => show_all = true,
            "-h" | "--help" => {
                eprintln!("usage: uvdump [DUMP_DIR] [--all]");
                return;
            }
            _ => {
                if dir.is_none() {
                    dir = Some(PathBuf::from(a));
                }
            }
        }
    }
    let dir = dir.unwrap_or_else(|| PathBuf::from("dumps"));

    let entries: Vec<PathBuf> = match std::fs::read_dir(&dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("bin"))
            .collect(),
        Err(e) => {
            eprintln!("cannot read dump dir {}: {e}", dir.display());
            std::process::exit(1);
        }
    };

    // 1) Scan every DCB/CCB for V#s → base -> {(stride, num_records)}.
    let mut strides: BTreeMap<u64, BTreeSet<(u32, u32)>> = BTreeMap::new();
    let mut dcb_count = 0usize;
    for p in &entries {
        if !is_cmd_buffer(p) {
            continue;
        }
        dcb_count += 1;
        let Ok(bytes) = std::fs::read(p) else {
            continue;
        };
        scan_vsharps(&bytes, &mut strides);
    }

    // 2) Parse the VBUF content files.
    let mut vbufs: Vec<VbufFile> = Vec::new();
    for p in &entries {
        if let Some(base) = parse_vbuf_base(p) {
            let len = std::fs::metadata(p).map(|m| m.len() as usize).unwrap_or(0);
            vbufs.push(VbufFile {
                base,
                path: p.clone(),
                len,
            });
        }
    }
    vbufs.sort_by_key(|v| (v.base, v.len));

    println!(
        "== uvdump: {} DCB/CCB scanned, {} distinct V# bases, {} VBUF content files ==",
        dcb_count,
        strides.len(),
        vbufs.len()
    );
    println!(
        "   sprite record = 24 bytes {{posX f32, posY f32, z f32, color u32, u f32, v f32}}, UV @ off 16\n"
    );

    let mut splatter = 0usize;
    let mut fractional = 0usize;
    let mut examined = 0usize;

    for v in &vbufs {
        let correlated: Vec<(u32, u32)> = strides
            .get(&v.base)
            .map(|s| s.iter().copied().collect())
            .unwrap_or_default();

        // Pick the stride to decode with: a correlated 24 wins; else an assumed 24
        // when the length divides evenly; else skip (unless --all).
        let has_24 = correlated.iter().any(|(s, _)| *s == 24);
        let assume_24 = !has_24 && v.len >= 24 && v.len.is_multiple_of(24);

        if !has_24 && !assume_24 {
            if show_all && !correlated.is_empty() {
                println!(
                    "buf base={:#x} len={} stride!=24 correlated={:?}  (skipped: no UV)",
                    v.base, v.len, correlated
                );
            }
            continue;
        }

        let (stride, num_records, stride_tag) = if has_24 {
            let (_, nr) = correlated
                .iter()
                .find(|(s, _)| *s == 24)
                .copied()
                .unwrap_or((24, 0));
            (24usize, nr, "24")
        } else {
            (24usize, (v.len / 24) as u32, "24?")
        };

        let Ok(bytes) = std::fs::read(&v.path) else {
            continue;
        };
        examined += 1;

        let f32_at = |off: usize| -> Option<f32> {
            bytes
                .get(off..off + 4)
                .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        };
        let verts = (v.len / stride).min(8);
        let mut uvs: Vec<(f32, f32)> = Vec::new();
        for i in 0..verts {
            match (f32_at(i * stride + 16), f32_at(i * stride + 20)) {
                (Some(u), Some(w)) => uvs.push((u, w)),
                _ => break,
            }
        }

        let all_corners =
            !uvs.is_empty() && uvs.iter().all(|(u, w)| is_corner(*u) && is_corner(*w));
        let any_fractional = uvs.iter().any(|(u, w)| !is_corner(*u) || !is_corner(*w));
        let flag = if all_corners {
            splatter += 1;
            "CORNERS (splatter — whole-atlas 0/1)"
        } else if any_fractional {
            fractional += 1;
            "FRACTIONAL (sub-rect — correct)"
        } else {
            "empty/short"
        };

        let uv_str: Vec<String> = uvs
            .iter()
            .map(|(u, w)| format!("({u:.4},{w:.4})"))
            .collect();
        println!(
            "buf base={:#x} stride={stride_tag} num_records={num_records} len={} uv=[{}]  <= {flag}",
            v.base,
            v.len,
            uv_str.join(" ")
        );
    }

    println!("\n== summary ==");
    println!("   VBUF stride-24 buffers examined : {examined}");
    println!("   CORNERS (splatter) buffers      : {splatter}");
    println!("   FRACTIONAL (correct) buffers    : {fractional}");
    if examined == 0 {
        println!(
            "   NOTE: no stride-24 UV buffers found. Ensure the scrape captured the broken scene\n\
             \x20        (frame window ~1600–1800, auto-reached, no input) with KIND_VBUF enabled."
        );
    }
    println!(
        "\n   VERDICT recipe: FRACTIONAL here on real HW ⇒ managed-runtime divergence confirmed (B2);\n\
         \x20                  CORNERS here on real HW ⇒ NOT a divergence (real Celeste emits corners too)."
    );
}

/// A UV component is an exact atlas corner if it is 0.0 or 1.0 (within a tiny eps).
fn is_corner(x: f32) -> bool {
    x.abs() < 1e-4 || (x - 1.0).abs() < 1e-4
}

/// Is this a DCB/CCB command-buffer dump (as opposed to a VBUF content file)?
fn is_cmd_buffer(p: &Path) -> bool {
    let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    stem.ends_with("_dcb") || stem.ends_with("_ccb")
}

/// Parse the guest base out of a VBUF content filename
/// `frameNNNNNN_bufNN_0xBASE_LEN.bin`. Returns `None` for non-VBUF files.
fn parse_vbuf_base(p: &Path) -> Option<u64> {
    let stem = p.file_stem().and_then(|s| s.to_str())?;
    // Find the `0x...` token; require a preceding `bufNN` segment so we do not
    // misparse a DCB whose name happens to contain a hex token.
    if !stem.contains("_buf") {
        return None;
    }
    for tok in stem.split('_') {
        if let Some(hex) = tok.strip_prefix("0x")
            && let Ok(b) = u64::from_str_radix(hex, 16)
        {
            return Some(b);
        }
    }
    None
}

/// Whole-buffer inline V# scan (identical criteria to `vref`): every 4-consecutive-dword
/// window that decodes to a plausible V# in the guest heap contributes its
/// `(stride, num_records)` to the `base` map.
fn scan_vsharps(bytes: &[u8], out: &mut BTreeMap<u64, BTreeSet<(u32, u32)>>) {
    let words: Vec<u32> = bytes
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();
    for w in words.windows(4) {
        let d = decode_v_sharp([w[0], w[1], w[2], w[3]]);
        if in_heap(d.base)
            && d.stride > 0
            && d.stride <= 256
            && d.num_records > 0
            && d.num_records < 1_000_000
        {
            out.entry(d.base)
                .or_default()
                .insert((d.stride, d.num_records));
        }
    }
}
