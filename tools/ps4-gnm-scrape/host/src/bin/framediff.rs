//! `framediff` — diff a real-PS4 frame against one of OUR gpu-snapshot frames, per draw.
//!
//! The GPU bring-up loop's hardest question is "what does the console do here that we
//! don't?". Answering it by hand means replaying a captured DCB, replaying our own
//! snapshot, and eyeballing two dumps whose guest addresses do not even agree. This does
//! it in one run.
//!
//! What it does:
//!   1. Replays every `SET_*_REG` in a captured console DCB into a flat register file and
//!      snapshots that file at each draw packet — the console's state, from the bytes the
//!      GPU actually consumed.
//!   2. Reads one of our `gpu-snapshots/frame-NNNNN/draws.json` and reconstructs OUR
//!      per-draw register file by accumulating the per-draw `register_delta`s.
//!   3. Matches the two draw lists and prints, per draw: registers whose values differ,
//!      descriptors that differ (including our `descriptor_honoured` flag), and the
//!      sampled bases on both sides.
//!   4. Prints the census of registers the console writes that our register file never
//!      receives at all.
//!
//! ADDRESS CORRESPONDENCE. Console and emulator guest addresses are unrelated, so nothing
//! can be compared by address directly. The mapping is DERIVED from the draw match: two
//! frames of the same scene issue the same draws in the same order into the same-sized
//! targets, so matched draw `i`'s console target names the same surface as our draw `i`'s
//! target. The tool prints the resulting map, and the evidence for the match, so a human
//! can reject it if the two captures are not actually the same scene. Treat every
//! address-level claim as conditional on that table looking right.
//!
//! Register field layouts come from the AMD GFX6 (SI) definitions as published in Mesa
//! `src/amd/common/sid.h`; a context byte address maps to a dword index as
//! `(R_028xxx - 0x28000) / 4`.
//!
//! Usage:
//!   framediff <console-dump-dir> <our-gpu-snapshot-frame-dir> [--frame N] [--verbose]
//!
//!   <console-dump-dir>   a receiver output dir holding `frameNNNNNN_sub0_*_dcb.bin`
//!                        plus the `frameNNNNNN_bufNN_0x<addr>_<size>.bin` probes
//!   <frame-dir>          e.g. `gpu-snapshots/frame-02143`
//!   --frame N            which console frame to use (default: the last one with a DCB)
//!   --verbose            also print the full per-draw state of both sides
//!
//! Nothing here reads or writes capture data other than to open it read-only.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use ps4_gnm::pm4::decode::{OwnedPacket, decode_bytes};
use ps4_gnm::pm4::opcodes::{context_reg as cr, op, reg_base, sh_reg};

use ps4_gnm_scrape::json::Json;

// ---------------------------------------------------------------------------
// GFX6 context registers this tool names that `ps4_gnm::pm4::opcodes` does not.
// Dword index = (R_028xxx - 0x28000) / 4, from Mesa `sid.h` byte addresses.
// ---------------------------------------------------------------------------
/// `CB_BLEND_RED` — constant-blend-factor red (`R_028414`).
const CB_BLEND_RED: u32 = reg_base::CONTEXT + 0x105;
/// `CB_COLOR0_CMASK` (`R_028C78`) — a surface base.
const CB_COLOR0_CMASK: u32 = reg_base::CONTEXT + 0x31E;
/// `CB_COLOR0_FMASK` (`R_028C80`) — a surface base.
const CB_COLOR0_FMASK: u32 = reg_base::CONTEXT + 0x320;
/// `DB_HTILE_DATA_BASE` (`R_028014`) — a surface base.
const DB_HTILE_DATA_BASE: u32 = reg_base::CONTEXT + 0x005;
/// `DB_Z_READ_BASE` (`R_028048`); the three depth/stencil bases that follow it are
/// `DB_STENCIL_READ_BASE` (`R_02804C`), `DB_Z_WRITE_BASE` (`R_028050`) and
/// `DB_STENCIL_WRITE_BASE` (`R_028054`) — all surface bases.
const DB_Z_READ_BASE: u32 = reg_base::CONTEXT + 0x012;
/// One past `DB_STENCIL_WRITE_BASE`.
const DB_SURFACE_BASE_END: u32 = reg_base::CONTEXT + 0x016;

/// Registers whose value is a guest ADDRESS (or a descriptor holding one). These differ
/// between console and emulator BY CONSTRUCTION — the two processes have unrelated address
/// spaces — so a raw value difference here is noise, not a finding, and they are reported
/// separately (resolved through the derived correspondence table) rather than as diffs.
fn is_address_bearing(idx: u32) -> bool {
    if idx == cr::CB_COLOR0_BASE {
        return true;
    }
    // Every SH user-data slot and every shader program address.
    let sh_user_ps = sh_reg::SPI_SHADER_USER_DATA_PS_0;
    let sh_user_vs = sh_reg::SPI_SHADER_USER_DATA_VS_0;
    if (sh_user_ps..sh_user_ps + sh_reg::USER_DATA_SLOTS).contains(&idx)
        || (sh_user_vs..sh_user_vs + sh_reg::USER_DATA_SLOTS).contains(&idx)
    {
        return true;
    }
    // The depth/stencil and HTILE surface bases.
    if idx == DB_HTILE_DATA_BASE || (DB_Z_READ_BASE..DB_SURFACE_BASE_END).contains(&idx) {
        return true;
    }
    // NOTE: CB_COLOR0_PITCH/SLICE/VIEW/INFO/ATTRIB are deliberately NOT here. They are
    // geometry and format, not addresses, and the same guest programs the same values on
    // both sides — so a difference in one of them is a real finding.
    idx == CB_COLOR0_CMASK
        || idx == CB_COLOR0_FMASK
        || idx == sh_reg::SPI_SHADER_PGM_LO_PS
        || idx == sh_reg::SPI_SHADER_PGM_HI_PS
        || idx == sh_reg::SPI_SHADER_PGM_LO_VS
        || idx == sh_reg::SPI_SHADER_PGM_HI_VS
}

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let (mut console_dir, mut ours_dir) = (None::<PathBuf>, None::<PathBuf>);
    let (mut want_frame, mut verbose) = (None::<u32>, false);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--frame" => want_frame = args.next().and_then(|v| v.parse().ok()),
            "--verbose" | "-v" => verbose = true,
            _ if console_dir.is_none() => console_dir = Some(PathBuf::from(a)),
            _ if ours_dir.is_none() => ours_dir = Some(PathBuf::from(a)),
            other => {
                eprintln!("unexpected argument {other}");
                return ExitCode::from(2);
            }
        }
    }
    let (Some(console_dir), Some(ours_dir)) = (console_dir, ours_dir) else {
        eprintln!(
            "usage: framediff <console-dump-dir> <gpu-snapshot-frame-dir> [--frame N] [--verbose]"
        );
        return ExitCode::from(2);
    };

    let console = match ConsoleFrame::load(&console_dir, want_frame) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("console side: {e}");
            return ExitCode::from(1);
        }
    };
    let ours = match OurFrame::load(&ours_dir) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("our side: {e}");
            return ExitCode::from(1);
        }
    };

    println!("== framediff ==");
    println!(
        "   console : {} frame {} ({}), {} draws",
        console_dir.display(),
        console.frame,
        console.dcb_name,
        console.draws.len()
    );
    println!(
        "   ours    : {} frame {}, {} draws",
        ours_dir.display(),
        ours.frame,
        ours.draws.len()
    );
    println!();

    let matches = match_draws(&console, &ours);
    print_matching(&console, &ours, &matches);
    let addr_map = derive_address_map(&console, &ours, &matches);
    print_address_map(&addr_map);
    print_per_draw_diff(&console, &ours, &matches, &addr_map, verbose);
    print_census(&console, &ours);

    ExitCode::SUCCESS
}

// ===========================================================================
// Console side
// ===========================================================================

/// One console draw: the register file as of that draw, plus the bits we decode out of it.
struct ConsoleDraw {
    #[allow(dead_code)] // positional identity; the match keys on it via the index.
    ordinal: usize,
    kind: &'static str,
    verts: u32,
    target: u64,
    width: u32,
    height: u32,
    regs: BTreeMap<u32, u32>,
    /// Sampled textures, in PS descriptor order: (slot label, base, w, h). Slot 0..7 is the
    /// register-resident T#; a user-data pointer that resolves through a probe to an 8-dword
    /// descriptor contributes a memory-resident one.
    textures: Vec<ConsoleTexture>,
}

struct ConsoleTexture {
    /// Where the descriptor came from — the distinction task-199 turned on.
    origin: &'static str,
    base: u64,
    width: u32,
    height: u32,
    /// The S# (sampler descriptor) that accompanies this T#, when it could be located.
    sampler: Option<ConsoleSampler>,
}

/// A decoded GCN S#. Field positions are the GFX6/7 sampler-resource layout: `word0[2:0]`
/// = `CLAMP_X`, `word0[5:3]` = `CLAMP_Y` (the `SQ_TEX_CLAMP_*` enum, Mesa `sid.h`
/// `V_008F30_SQ_TEX_WRAP` = 0 … `_MIRROR_ONCE_BORDER` = 7), and `word2[20]` = `XY_MAG_FILTER`
/// (0 = point/nearest, 1 = bilinear).
#[derive(Clone, Copy, PartialEq, Eq)]
struct ConsoleSampler {
    bilinear: bool,
    clamp_x: u32,
    clamp_y: u32,
}

impl ConsoleSampler {
    fn decode(words: &[u32]) -> Option<Self> {
        if words.len() < 4 {
            return None;
        }
        // An all-zero quad is an absent descriptor, not a legitimate point/wrap sampler.
        if words[..4].iter().all(|w| *w == 0) {
            return None;
        }
        Some(ConsoleSampler {
            bilinear: (words[2] >> 20) & 1 == 1,
            clamp_x: words[0] & 0x7,
            clamp_y: (words[0] >> 3) & 0x7,
        })
    }

    fn filter(&self) -> &'static str {
        if self.bilinear { "LINEAR" } else { "NEAREST" }
    }
}

/// `SQ_TEX_CLAMP_*` code -> the portable address mode our backend would bind, mirroring
/// `ps4_gnm::vbuf::clamp_mode` (the subset has no border colour, so the border variants
/// collapse to clamp-to-edge and the mirror-once variants to mirror-repeat).
fn clamp_name(code: u32) -> &'static str {
    match code & 0x7 {
        0 => "Repeat",
        1 => "MirrorRepeat",
        2 => "ClampToEdge",
        3 => "MirrorRepeat",
        4 => "ClampToEdge",
        5 => "MirrorRepeat",
        6 => "ClampToEdge",
        _ => "MirrorRepeat",
    }
}

impl ConsoleDraw {
    fn reg(&self, idx: u32) -> u32 {
        self.regs.get(&idx).copied().unwrap_or(0)
    }
    fn blend(&self) -> u32 {
        self.reg(cr::CB_BLEND0_CONTROL)
    }
}

struct ConsoleFrame {
    frame: u32,
    dcb_name: String,
    draws: Vec<ConsoleDraw>,
    /// Every register index the DCB ever wrote, with its final value — for the census.
    all_regs: BTreeMap<u32, u32>,
}

impl ConsoleFrame {
    fn load(dir: &Path, want_frame: Option<u32>) -> Result<Self, String> {
        // Find the DCB dumps: `frameNNNNNN_sub0_<kind>_dcb.bin`.
        let mut dcbs: Vec<(u32, PathBuf)> = Vec::new();
        let rd = std::fs::read_dir(dir).map_err(|e| format!("read {}: {e}", dir.display()))?;
        for e in rd.flatten() {
            let name = e.file_name();
            let Some(name) = name.to_str() else { continue };
            if !name.ends_with("_dcb.bin") || !name.starts_with("frame") {
                continue;
            }
            let Some(f) = name
                .strip_prefix("frame")
                .and_then(|s| s.get(..6))
                .and_then(|s| s.parse::<u32>().ok())
            else {
                continue;
            };
            dcbs.push((f, e.path()));
        }
        if dcbs.is_empty() {
            return Err(format!("no `frameNNNNNN_*_dcb.bin` in {}", dir.display()));
        }
        dcbs.sort();
        let (frame, path) = match want_frame {
            Some(w) => dcbs
                .iter()
                .find(|(f, _)| *f == w)
                .cloned()
                .ok_or_else(|| format!("no DCB for frame {w}"))?,
            // Default to the LAST captured frame: a capture's first frames are boot/splash,
            // the steady state is at the end.
            None => dcbs.last().cloned().expect("non-empty"),
        };
        let bytes = std::fs::read(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
        let probes = ProbeIndex::build(dir, frame);

        let mut regs: BTreeMap<u32, u32> = BTreeMap::new();
        let mut all_regs: BTreeMap<u32, u32> = BTreeMap::new();
        let mut draws = Vec::new();
        for p in decode_bytes(&bytes) {
            match p {
                OwnedPacket::Type3 {
                    opcode,
                    ref body,
                    count: _,
                } => {
                    if let Some(base) = ps4_gnm::pm4::opcodes::set_reg_base(opcode)
                        && let Some((&off, vals)) = body.split_first()
                    {
                        for (i, v) in vals.iter().enumerate() {
                            regs.insert(base + off + i as u32, *v);
                            all_regs.insert(base + off + i as u32, *v);
                        }
                    } else if is_draw(opcode) {
                        draws.push(build_console_draw(
                            draws.len(),
                            opcode,
                            body,
                            &regs,
                            &probes,
                        ));
                    }
                }
                OwnedPacket::Truncated { .. } => break,
                _ => {}
            }
        }
        Ok(ConsoleFrame {
            frame,
            dcb_name: path
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default(),
            draws,
            all_regs,
        })
    }
}

fn is_draw(opcode: u8) -> bool {
    opcode == op::IT_DRAW_INDEX_AUTO
        || opcode == op::IT_DRAW_INDEX_2
        || opcode == op::IT_DRAW_INDEX_OFFSET_2
}

fn draw_kind(opcode: u8) -> &'static str {
    match opcode {
        op::IT_DRAW_INDEX_AUTO => "DrawIndexAuto",
        op::IT_DRAW_INDEX_OFFSET_2 => "DrawIndexOffset",
        op::IT_DRAW_INDEX_2 => "DrawIndex2",
        _ => "Draw?",
    }
}

fn build_console_draw(
    ordinal: usize,
    opcode: u8,
    body: &[u32],
    regs: &BTreeMap<u32, u32>,
    probes: &ProbeIndex,
) -> ConsoleDraw {
    let get = |i: u32| regs.get(&i).copied().unwrap_or(0);
    let verts = match opcode {
        op::IT_DRAW_INDEX_AUTO => body.first().copied().unwrap_or(0),
        op::IT_DRAW_INDEX_OFFSET_2 => body.get(1).copied().unwrap_or(0),
        _ => body.first().copied().unwrap_or(0),
    };
    // CB_COLOR0_PITCH/SLICE hold TILE_MAX fields: width = (PITCH_TILE_MAX+1)*8, and
    // height follows from the slice tile-max over that width.
    let pitch = get(cr::CB_COLOR0_PITCH);
    let slice = get(cr::CB_COLOR0_SLICE);
    let width = ((pitch & 0x7ff) + 1) * 8;
    let height = (((slice & 0x3f_ffff) + 1) * 64)
        .checked_div(width)
        .unwrap_or(0);

    // PS user data: slots 0..7 may hold a T# inline; slots 8.. may hold pointers to
    // memory-resident descriptors the shader `s_load_dwordx8`s.
    let ud: Vec<u32> = (0..sh_reg::USER_DATA_SLOTS)
        .map(|i| get(sh_reg::SPI_SHADER_USER_DATA_PS_0 + i))
        .collect();
    let mut textures = Vec::new();
    if let Some((base, w, h)) = tsharp(&ud[0..8]) {
        // These shaders put the S# in the four user-SGPRs right after the 8-dword T#
        // (`image_sample ..., s[0:7], s[8:11]`), which is what recompiling the dumped
        // Celeste shaders reports as `s_offset` 8.
        textures.push(ConsoleTexture {
            origin: "register-resident",
            base,
            width: w,
            height: h,
            sampler: ConsoleSampler::decode(&ud[8..12]),
        });
    }
    for slot in [8usize, 12] {
        if slot + 1 >= ud.len() {
            continue;
        }
        let ptr = ((u64::from(ud[slot + 1]) & 0xffff) << 32) | u64::from(ud[slot]);
        if !GUEST_HEAP.contains(&ptr) {
            continue;
        }
        // 12 dwords, not 8: the descriptor set holds the 8-dword T# at +0 and its
        // 4-dword S# at +0x20 — the layout the shaders' own SMRD pair states
        // (`s_load_dwordx8 s[..], s[12:13], 0x0` then `s_load_dwordx4 s[..], s[12:13], 0x8`,
        // whose immediate is a DWORD index, so 0x8 dwords = 32 bytes).
        if let Some(dw) = probes.lookup(ptr, 12)
            && let Some((base, w, h)) = tsharp(&dw)
        {
            textures.push(ConsoleTexture {
                origin: "memory-resident",
                base,
                width: w,
                height: h,
                sampler: ConsoleSampler::decode(&dw[8..12]),
            });
        } else if let Some(dw) = probes.lookup(ptr, 8)
            && let Some((base, w, h)) = tsharp(&dw)
        {
            // Probe too short to reach the S#: report the texture, say the sampler is
            // unknown rather than inventing a default.
            textures.push(ConsoleTexture {
                origin: "memory-resident",
                base,
                width: w,
                height: h,
                sampler: None,
            });
        }
    }

    ConsoleDraw {
        ordinal,
        kind: draw_kind(opcode),
        verts,
        target: u64::from(get(cr::CB_COLOR0_BASE)) << 8,
        width,
        height,
        regs: regs.clone(),
        textures,
    }
}

/// Plausible guest-heap band for a PS4 title (the capture plugin applies the same gate):
/// anything outside [4 GB, 64 GB) is not a descriptor base or a buffer pointer.
const GUEST_HEAP: std::ops::Range<u64> = 0x1_0000_0000..0x10_0000_0000;

/// Decode a GCN T# (image descriptor): word0 + word1[7:0] carry `base_addr >> 8`, and
/// word2 packs width/height as `-1` encodings (AMD GCN ISA, image resource layout).
fn tsharp(dw: &[u32]) -> Option<(u64, u32, u32)> {
    if dw.len() < 8 {
        return None;
    }
    let base = ((((u64::from(dw[1])) & 0xff) << 32) | u64::from(dw[0])) << 8;
    if !GUEST_HEAP.contains(&base) {
        return None;
    }
    Some((base, (dw[2] & 0x3fff) + 1, ((dw[2] >> 14) & 0x3fff) + 1))
}

/// Index of the plugin's referenced-buffer / user-data probe dumps for one frame, keyed by
/// the guest base address encoded in the filename.
#[derive(Default)]
struct ProbeIndex {
    entries: Vec<(u64, usize, PathBuf)>,
}

impl ProbeIndex {
    fn build(dir: &Path, frame: u32) -> Self {
        let mut entries = Vec::new();
        let prefix = format!("frame{frame:06}_buf");
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                let name = e.file_name();
                let Some(name) = name.to_str() else { continue };
                if !name.starts_with(&prefix) || !name.ends_with(".bin") {
                    continue;
                }
                let mut it = name.trim_end_matches(".bin").rsplitn(3, '_');
                let Some(size) = it.next().and_then(|s| s.parse::<usize>().ok()) else {
                    continue;
                };
                let Some(addr) = it
                    .next()
                    .and_then(|s| s.strip_prefix("0x"))
                    .and_then(|s| u64::from_str_radix(s, 16).ok())
                else {
                    continue;
                };
                entries.push((addr, size, e.path()));
            }
        }
        entries.sort_by_key(|(a, _, _)| *a);
        Self { entries }
    }

    /// Read `n` dwords at guest address `addr` from whichever probe covers it.
    fn lookup(&self, addr: u64, n: usize) -> Option<Vec<u32>> {
        for (base, size, path) in &self.entries {
            if addr < *base || addr >= base + *size as u64 {
                continue;
            }
            let off = (addr - base) as usize;
            let bytes = std::fs::read(path).ok()?;
            let avail = bytes.len().saturating_sub(off) / 4;
            if avail < n {
                continue;
            }
            return Some(
                (0..n)
                    .map(|i| {
                        let o = off + i * 4;
                        u32::from_le_bytes([bytes[o], bytes[o + 1], bytes[o + 2], bytes[o + 3]])
                    })
                    .collect(),
            );
        }
        None
    }
}

// ===========================================================================
// Our side (gpu-snapshots/frame-NNNNN/draws.json)
// ===========================================================================

struct OurSampled {
    /// The set-0 binding our backend wrote this descriptor at — texture 0 lands at binding
    /// 1, later ones at 7+ (task-199), so it names WHICH sample in the shader this is.
    binding: u32,
    /// The sampler for this bind as "FILTER/wrap_x/wrap_y". Read from `sampler_bound`
    /// (what our backend was actually told to create) when the snapshot carries it, and
    /// only otherwise from `s_sharp` (what the guest REQUESTED). The distinction is the
    /// point: a capture that reports only the request cannot show a bind that ignores it,
    /// which is how a hardcoded linear/repeat on the render-target path survived while every
    /// snapshot faithfully recorded a NEAREST S# (task-201).
    sampler: Option<String>,
    /// Which field the above came from, so the reader knows whether they are looking at a
    /// REQUEST or at what was BOUND.
    sampler_is_bound: bool,
    source: String,
    base: u64,
    width: u32,
    height: u32,
    descriptor_honoured: bool,
}

struct OurDraw {
    #[allow(dead_code)] // positional identity; the match keys on it via the index.
    ordinal: usize,
    kind: String,
    count: u32,
    target: u64,
    width: u32,
    height: u32,
    /// The target's PADDED pitch in texels — directly comparable with the console's
    /// `CB_COLOR0_PITCH` tile-max, which is also a padded pitch.
    pitch: u32,
    /// Target byte size, when the snapshot knows it (offscreen targets). With `pitch` this
    /// gives the padded HEIGHT, which is what the console's `CB_COLOR0_SLICE` encodes.
    size: Option<u64>,
    blend: u32,
    /// Our register file AS OF this draw, accumulated from the per-draw `register_delta`s.
    regs: BTreeMap<u32, u32>,
    sampled: Vec<OurSampled>,
}

struct OurFrame {
    frame: i64,
    draws: Vec<OurDraw>,
    /// Every register index our snapshot ever recorded — for the census.
    all_regs: BTreeSet<u32>,
}

impl OurFrame {
    fn load(dir: &Path) -> Result<Self, String> {
        let path = dir.join("draws.json");
        let text =
            std::fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
        let root = Json::parse(&text).map_err(|e| format!("parse {}: {e}", path.display()))?;
        let frame = root.get("frame").and_then(|v| v.as_i64()).unwrap_or(-1);
        let list = root
            .get("draws")
            .and_then(|v| v.as_array())
            .ok_or("draws.json has no `draws` array")?;

        // Our register file is only ever reported as a per-draw DELTA, so the state at draw
        // i is the accumulation of every delta up to and including i. Reconstructing it here
        // is what makes a register-level comparison against the console possible at all.
        let mut regs: BTreeMap<u32, u32> = BTreeMap::new();
        let mut all_regs = BTreeSet::new();
        let mut draws = Vec::new();
        for (i, d) in list.iter().enumerate() {
            if let Some(deltas) = d.get("register_delta").and_then(|v| v.as_array()) {
                for e in deltas {
                    let Some(idx) = e.get("index").and_then(|v| v.as_i64()) else {
                        continue;
                    };
                    let Some(to) = e.get("to").and_then(|v| v.as_str()) else {
                        continue;
                    };
                    if let Some(v) = parse_hex_u32(to) {
                        regs.insert(idx as u32, v);
                        all_regs.insert(idx as u32);
                    }
                }
            }
            let target = d.get("target");
            let base = target
                .and_then(|t| t.get("base"))
                .and_then(|v| v.as_str())
                .and_then(parse_hex_u64)
                .unwrap_or(0);
            let blend = d
                .get("pipeline")
                .and_then(|p| p.get("blend"))
                .and_then(|b| b.get("control"))
                .and_then(|v| v.as_str())
                .and_then(parse_hex_u32)
                .unwrap_or(0);
            // `sampled` is an ARRAY since task-199 (one entry per texture the PS samples);
            // accept the pre-task-199 single-object form too so an older capture still diffs.
            let sampled = match d.get("sampled") {
                Some(Json::Array(a)) => a.iter().filter_map(our_sampled).collect(),
                Some(o @ Json::Object(_)) => our_sampled(o).into_iter().collect(),
                _ => Vec::new(),
            };
            draws.push(OurDraw {
                ordinal: i,
                kind: d
                    .get("kind")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?")
                    .to_string(),
                count: d.get("count").and_then(|v| v.as_i64()).unwrap_or(0) as u32,
                target: base,
                width: target
                    .and_then(|t| t.get("width"))
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0) as u32,
                height: target
                    .and_then(|t| t.get("height"))
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0) as u32,
                pitch: target
                    .and_then(|t| t.get("pitch"))
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0) as u32,
                size: target
                    .and_then(|t| t.get("size"))
                    .and_then(|v| v.as_i64())
                    .map(|v| v as u64),
                blend,
                regs: regs.clone(),
                sampled,
            });
        }
        Ok(OurFrame {
            frame,
            draws,
            all_regs,
        })
    }
}

fn our_sampled(v: &Json) -> Option<OurSampled> {
    Some(OurSampled {
        binding: v.get("binding").and_then(|x| x.as_i64()).unwrap_or(0) as u32,
        source: v
            .get("source")
            .and_then(|x| x.as_str())
            .unwrap_or("?")
            .to_string(),
        base: v
            .get("base")
            .and_then(|x| x.as_str())
            .and_then(parse_hex_u64)?,
        width: v.get("width").and_then(|x| x.as_i64()).unwrap_or(0) as u32,
        height: v.get("height").and_then(|x| x.as_i64()).unwrap_or(0) as u32,
        descriptor_honoured: v
            .get("descriptor_honoured")
            .and_then(|x| x.as_bool())
            .unwrap_or(false),
        sampler: bound_sampler(v).or_else(|| requested_sampler(v)),
        sampler_is_bound: bound_sampler(v).is_some(),
    })
}

/// The sampler our backend actually created for this bind (`sampler_bound`, task-201).
fn bound_sampler(v: &Json) -> Option<String> {
    let s = v.get("sampler_bound")?;
    let mag = s.get("mag_filter").and_then(|x| x.as_str())?;
    Some(format!(
        "{}/{}/{}",
        mag.to_uppercase(),
        s.get("address_mode_u")
            .and_then(|x| x.as_str())
            .unwrap_or("?"),
        s.get("address_mode_v")
            .and_then(|x| x.as_str())
            .unwrap_or("?"),
    ))
}

/// The S# the guest asked for. Pre-task-201 snapshots carry only this.
fn requested_sampler(v: &Json) -> Option<String> {
    let s = v.get("s_sharp")?;
    let bilinear = s.get("bilinear").and_then(|x| x.as_bool())?;
    Some(format!(
        "{}/{}/{}",
        if bilinear { "LINEAR" } else { "NEAREST" },
        s.get("clamp_x").and_then(|x| x.as_str()).unwrap_or("?"),
        s.get("clamp_y").and_then(|x| x.as_str()).unwrap_or("?"),
    ))
}

fn parse_hex_u32(s: &str) -> Option<u32> {
    u32::from_str_radix(s.trim().trim_start_matches("0x"), 16).ok()
}
fn parse_hex_u64(s: &str) -> Option<u64> {
    u64::from_str_radix(s.trim().trim_start_matches("0x"), 16).ok()
}

// ===========================================================================
// Matching, correspondence, diff
// ===========================================================================

/// How a console draw was paired with one of ours, and how well.
struct DrawMatch {
    console: usize,
    ours: usize,
    kind_ok: bool,
    dims_ok: bool,
    blend_ok: bool,
}

impl DrawMatch {
    fn confident(&self) -> bool {
        self.kind_ok && self.dims_ok && self.blend_ok
    }
}

/// Pair draws BY ORDINAL, then score each pair on the three independent signals the
/// investigation used by hand: draw kind, target dimensions, and CB_BLEND0_CONTROL. Ordinal
/// is the hypothesis; the three signals are the evidence for it. A frame where they all
/// agree on every draw is the same scene issued the same way, which is what licenses the
/// address correspondence below.
fn match_draws(console: &ConsoleFrame, ours: &OurFrame) -> Vec<DrawMatch> {
    let n = console.draws.len().min(ours.draws.len());
    (0..n)
        .map(|i| {
            let c = &console.draws[i];
            let o = &ours.draws[i];
            DrawMatch {
                console: i,
                ours: i,
                kind_ok: c.kind == o.kind,
                dims_ok: dims_compatible(c, o),
                blend_ok: c.blend() == o.blend,
            }
        })
        .collect()
}

/// The console's `CB_COLOR0_PITCH`/`SLICE` are TILE_MAX fields, i.e. the surface's PADDED
/// pitch and padded slice — 384x192 for a 320x180 target. Our snapshot reports the LOGICAL
/// extent plus that same padded `pitch` (and, for an offscreen target, the byte size), so
/// the honest comparison is padded-vs-padded: pitch against pitch, and slice-derived height
/// against `size / (pitch * 4)`.
///
/// A videoout target has no byte size in the snapshot; there the padded height is unknown,
/// so accept a console height at or above ours by less than one 64-row macro-tile.
fn dims_compatible(c: &ConsoleDraw, o: &OurDraw) -> bool {
    let w_ok = c.width == o.pitch || c.width == o.width;
    let h_ok = match (o.size, o.pitch) {
        (Some(size), pitch) if pitch > 0 => c.height as u64 == size / (u64::from(pitch) * 4),
        _ => c.height >= o.height && c.height - o.height < 64,
    };
    w_ok && h_ok
}

fn print_matching(console: &ConsoleFrame, ours: &OurFrame, matches: &[DrawMatch]) {
    println!("-- draw matching heuristic --");
    println!(
        "   pairing: BY ORDINAL, scored on (draw kind, target WxH, CB_BLEND0_CONTROL).\n   \
         Console and emulator guest addresses are unrelated, so the match — not any address —\n   \
         is what identifies a surface. Reject the run if this table looks wrong."
    );
    let confident = matches.iter().filter(|m| m.confident()).count();
    println!(
        "   {confident}/{} draws matched on all three signals ({} console draws, {} ours)",
        matches.len(),
        console.draws.len(),
        ours.draws.len()
    );
    if console.draws.len() != ours.draws.len() {
        println!(
            "   !! DRAW COUNT DIFFERS ({} console vs {} ours) — the frames are not the same \
             workload; every mapping below is suspect",
            console.draws.len(),
            ours.draws.len()
        );
    }
    for m in matches.iter().filter(|m| !m.confident()) {
        let c = &console.draws[m.console];
        let o = &ours.draws[m.ours];
        let mut why = Vec::new();
        if !m.kind_ok {
            why.push(format!("kind {} vs {}", c.kind, o.kind));
        }
        if !m.dims_ok {
            why.push(format!(
                "dims {}x{} vs {}x{}",
                c.width, c.height, o.width, o.height
            ));
        }
        if !m.blend_ok {
            why.push(format!("blend {:#010x} vs {:#010x}", c.blend(), o.blend));
        }
        println!("   ?? draw {:>3}: {}", m.console, why.join(", "));
    }
    println!();
}

/// Console guest address -> our guest address, derived from the matched draws' targets and
/// from every sampled texture whose console base is a target some matched draw rendered into.
fn derive_address_map(
    console: &ConsoleFrame,
    ours: &OurFrame,
    matches: &[DrawMatch],
) -> BTreeMap<u64, u64> {
    let mut map = BTreeMap::new();
    for m in matches.iter().filter(|m| m.confident()) {
        let c = &console.draws[m.console];
        let o = &ours.draws[m.ours];
        if c.target != 0 && o.target != 0 {
            map.insert(c.target, o.target);
        }
    }
    // Sampled textures give further correspondences — an atlas is never a render target, so
    // it would otherwise stay unmapped. Only accept a pair when BOTH sides bound a texture
    // at the same index of a confidently matched draw AND the dimensions agree; anything
    // weaker would invent a mapping out of a coincidence of ordering.
    for m in matches.iter().filter(|m| m.confident()) {
        let c = &console.draws[m.console];
        let o = &ours.draws[m.ours];
        for (ct, os) in c.textures.iter().zip(o.sampled.iter()) {
            if ct.width == os.width && ct.height == os.height {
                map.entry(ct.base).or_insert(os.base);
            }
        }
    }
    map
}

fn print_address_map(map: &BTreeMap<u64, u64>) {
    println!("-- derived address correspondence (console -> ours) --");
    if map.is_empty() {
        println!("   (none — no confidently matched draw wrote a colour target)");
    }
    for (c, o) in map {
        println!("   {c:#x}  ->  {o:#x}");
    }
    println!();
}

fn print_per_draw_diff(
    console: &ConsoleFrame,
    ours: &OurFrame,
    matches: &[DrawMatch],
    addr_map: &BTreeMap<u64, u64>,
    verbose: bool,
) {
    println!("-- per-draw diff --");
    println!(
        "   Console textures are decoded from the PS user-data registers in effect at the\n   draw. Those registers persist across draws, so a draw whose PS samples nothing still\n   shows the previous draw's descriptor; such lines are labelled stale. Only draws with\n   a difference are listed unless --verbose."
    );
    let mut clean = 0usize;
    for m in matches {
        let c = &console.draws[m.console];
        let o = &ours.draws[m.ours];

        // Registers both sides recorded, whose values differ and which are not address-bearing.
        let mut reg_diffs: Vec<(u32, u32, u32)> = Vec::new();
        for (idx, cv) in &c.regs {
            if is_address_bearing(*idx) {
                continue;
            }
            if let Some(ov) = o.regs.get(idx)
                && ov != cv
            {
                reg_diffs.push((*idx, *cv, *ov));
            }
        }

        // Descriptors: compare console textures (mapped through the correspondence) against
        // what our snapshot recorded as actually bound.
        let mut desc_lines: Vec<String> = Vec::new();
        let n = c.textures.len().max(o.sampled.len());
        for i in 0..n {
            match (c.textures.get(i), o.sampled.get(i)) {
                (Some(ct), Some(os)) => {
                    let expect = addr_map.get(&ct.base).copied();
                    let agree = expect == Some(os.base);
                    // The S# governs whether an upscaled 320x180 pixel-art frame stays
                    // crisp; a filter disagreement is as much a rendering bug as binding
                    // the wrong image, and is invisible in a picture-free log (task-201).
                    let csamp = match &ct.sampler {
                        Some(cs) => format!(
                            "{}/{}/{}",
                            cs.filter(),
                            clamp_name(cs.clamp_x),
                            clamp_name(cs.clamp_y)
                        ),
                        None => "(no S# located)".to_string(),
                    };
                    let osamp = match (&os.sampler, os.sampler_is_bound) {
                        (Some(x), true) => format!("{x} (bound)"),
                        (Some(x), false) => format!(
                            "{x} (REQUESTED; this snapshot predates \
                                                    sampler_bound, so what was actually bound \
                                                    is not recorded)"
                        ),
                        (None, _) => "(none)".to_string(),
                    };
                    let samp_verdict = match (&ct.sampler, &os.sampler) {
                        (Some(_), Some(o)) if &csamp == o => "OK",
                        (Some(_), Some(_)) => "<<< SAMPLER MISMATCH",
                        (Some(_), None) => "<<< ours recorded NO sampler",
                        _ => "?",
                    };
                    desc_lines.push(format!(
                        "tex{i}: console {:#x} {}x{} ({}) {}| ours bind{} {:#x} {}x{} {} honoured={} {}",
                        ct.base,
                        ct.width,
                        ct.height,
                        ct.origin,
                        match expect {
                            Some(e) => format!("-> {e:#x} "),
                            None => "-> (unmapped) ".to_string(),
                        },
                        os.binding,
                        os.base,
                        os.width,
                        os.height,
                        os.source,
                        os.descriptor_honoured,
                        if expect.is_none() {
                            "?"
                        } else if agree {
                            "OK"
                        } else {
                            "<<< MISMATCH"
                        }
                    ));
                    desc_lines.push(format!(
                        "      S#: console {csamp} | ours {osamp}  {samp_verdict}"
                    ));
                }
                (Some(ct), None) if o.sampled.is_empty() => {
                    // Our PS bound NOTHING, so it samples nothing. The console-side list is
                    // decoded from residual PS user-data, which a non-sampling draw (a
                    // full-screen clear, say) inherits unchanged from the previous draw — so
                    // this is stale register state, not a missing bind. Reported, never as a
                    // finding.
                    desc_lines.push(format!(
                        "tex{i}: console {:#x} {}x{} ({}) | ours: PS samples nothing \
                         (console registers still hold the previous draw's descriptor — \
                         stale, not a missing bind)",
                        ct.base, ct.width, ct.height, ct.origin
                    ));
                }
                (Some(ct), None) => desc_lines.push(format!(
                    "tex{i}: console {:#x} {}x{} ({}) | ours NOT BOUND  <<< we sample fewer \
                     textures than the console",
                    ct.base, ct.width, ct.height, ct.origin
                )),
                (None, Some(os)) => desc_lines.push(format!(
                    "tex{i}: console (none decoded) | ours bind{} {:#x} {}x{} {} honoured={}",
                    os.binding, os.base, os.width, os.height, os.source, os.descriptor_honoured
                )),
                (None, None) => {}
            }
        }

        if reg_diffs.is_empty() && desc_lines.is_empty() && m.confident() && !verbose {
            clean += 1;
            continue;
        }
        let ours_target = if o.target == 0 {
            "videoout".to_string()
        } else {
            format!("{:#x}", o.target)
        };
        println!(
            "draw {:>3}  {} verts={}  console {:#x} {}x{} | ours {} {}x{}{}",
            m.console,
            c.kind,
            c.verts,
            c.target,
            c.width,
            c.height,
            ours_target,
            o.width,
            o.height,
            if m.confident() {
                ""
            } else {
                "   [UNCONFIRMED MATCH]"
            }
        );
        if reg_diffs.is_empty() {
            println!("     registers : identical on every register both sides recorded");
        } else {
            for (idx, cv, ov) in &reg_diffs {
                println!(
                    "     REG DIFF  : {:#06x} {:<28} console {cv:#010x}  ours {ov:#010x}",
                    idx,
                    ps4_gnm::pm4::opcodes::reg_name(*idx).unwrap_or_else(|| "<unnamed>".into())
                );
            }
        }
        for l in &desc_lines {
            println!("     {l}");
        }
        if verbose {
            println!(
                "     blend     : console {:#010x}  ours {:#010x}",
                c.blend(),
                o.blend
            );
            println!("     count     : console {}  ours {}", c.verts, o.count);
            let cbr = c.reg(CB_BLEND_RED);
            if cbr != 0 {
                println!("     CB_BLEND_RED = {cbr:#010x} (console)");
            }
        }
        println!();
    }
    if clean > 0 {
        println!("   ({clean} draws identical on every compared register and descriptor)");
    }
    println!();
}

/// Registers the console programs that our register file never receives.
///
/// This is not a curiosity: our shadow register file starts at zero for anything the guest
/// never writes THROUGH US, so an unreceived register silently reads as 0 rather than as
/// the hardware default the console established.
fn print_census(console: &ConsoleFrame, ours: &OurFrame) {
    println!("-- registers the console writes that our register file never receives --");
    let missing: Vec<(u32, u32)> = console
        .all_regs
        .iter()
        .filter(|(idx, _)| !ours.all_regs.contains(idx))
        .map(|(i, v)| (*i, *v))
        .collect();
    println!(
        "   console touched {} registers; we recorded {}; {} never reach us",
        console.all_regs.len(),
        ours.all_regs.len(),
        missing.len()
    );
    for (idx, v) in &missing {
        println!(
            "   {idx:#06x}  {:<28} = {v:#010x}",
            ps4_gnm::pm4::opcodes::reg_name(*idx).unwrap_or_else(|| "<unnamed>".into())
        );
    }
    let ours_only: Vec<u32> = ours
        .all_regs
        .iter()
        .filter(|i| !console.all_regs.contains_key(i))
        .copied()
        .collect();
    if !ours_only.is_empty() {
        println!(
            "   ({} registers we record that this console DCB never wrote: {})",
            ours_only.len(),
            ours_only
                .iter()
                .map(|i| format!("{i:#06x}"))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    if !missing.is_empty() {
        println!(
            "   NOTE: the default-hardware-state preamble is the usual explanation — \n   \
             `sceGnmDrawInitDefaultHardwareState*` / `sceGnmDrawInitToDefaultContextState*` in\n   \
             crates/libs/src/libscegnmdriver/hwstate.rs are stubs that return the dword count\n   \
             and write NO PM4, so everything the real builders would emit is absent here."
        );
    }
    println!();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn our(width: u32, height: u32, pitch: u32, size: Option<u64>) -> OurDraw {
        OurDraw {
            ordinal: 0,
            kind: "DrawIndexOffset".into(),
            count: 6,
            target: 0x9afb58000,
            width,
            height,
            pitch,
            size,
            blend: 0x4501_0501,
            regs: BTreeMap::new(),
            sampled: Vec::new(),
        }
    }

    fn console(width: u32, height: u32) -> ConsoleDraw {
        ConsoleDraw {
            ordinal: 0,
            kind: "DrawIndexOffset",
            verts: 6,
            target: 0x2bcf30000,
            width,
            height,
            regs: BTreeMap::new(),
            textures: Vec::new(),
        }
    }

    /// The console reports a target's PADDED extent (`CB_COLOR0_PITCH`/`SLICE` are TILE_MAX
    /// fields); our snapshot reports the LOGICAL extent plus the same padded pitch. Comparing
    /// them naively makes every draw look mismatched, which is exactly the failure that
    /// would silently void the whole address correspondence.
    #[test]
    fn padded_console_extent_matches_our_logical_extent() {
        // Celeste's offscreen target: 320x180 logical, 384x192 padded, 294912 bytes.
        assert!(dims_compatible(
            &console(384, 192),
            &our(320, 180, 384, Some(294_912))
        ));
        // A genuinely different target must NOT match.
        assert!(!dims_compatible(
            &console(384, 192),
            &our(640, 360, 640, Some(921_600))
        ));
        // Videoout: no byte size recorded, so the padded height is unknown and a console
        // height within one 64-row macro-tile above ours is accepted.
        assert!(dims_compatible(
            &console(1920, 1088),
            &our(1920, 1080, 1920, None)
        ));
        assert!(!dims_compatible(
            &console(1920, 2160),
            &our(1920, 1080, 1920, None)
        ));
    }

    /// Address-bearing registers hold guest pointers, which differ between console and
    /// emulator by construction. Reporting them as diffs would bury the real findings under
    /// ~20 lines of noise per draw; failing to classify one does the same.
    #[test]
    fn address_bearing_registers_are_excluded_from_diffs() {
        for idx in [
            cr::CB_COLOR0_BASE,
            CB_COLOR0_CMASK,
            CB_COLOR0_FMASK,
            DB_HTILE_DATA_BASE,
            DB_Z_READ_BASE,
            DB_SURFACE_BASE_END - 1,
            sh_reg::SPI_SHADER_PGM_LO_PS,
            sh_reg::SPI_SHADER_PGM_HI_VS,
            sh_reg::SPI_SHADER_USER_DATA_PS_0,
            sh_reg::SPI_SHADER_USER_DATA_PS_0 + sh_reg::USER_DATA_SLOTS - 1,
            sh_reg::SPI_SHADER_USER_DATA_VS_0 + 3,
        ] {
            assert!(is_address_bearing(idx), "{idx:#06x} should be excluded");
        }
        // The colour/blend state that the task-199 investigation compared MUST stay in.
        for idx in [
            cr::CB_BLEND0_CONTROL,
            cr::CB_COLOR_CONTROL,
            cr::CB_TARGET_MASK,
            cr::CB_SHADER_MASK,
            cr::CB_COLOR0_INFO,
            cr::CB_COLOR0_ATTRIB,
            cr::CB_COLOR0_PITCH,
            cr::SPI_SHADER_COL_FORMAT,
            cr::DB_DEPTH_CONTROL,
        ] {
            assert!(!is_address_bearing(idx), "{idx:#06x} must be compared");
        }
    }

    /// A T# base lives in word0 + word1[7:0], shifted left by 8. A descriptor that does not
    /// decode into the guest heap band is not a texture and must be rejected rather than
    /// producing a bogus correspondence entry.
    #[test]
    fn tsharp_decodes_base_and_extent_and_rejects_junk() {
        // base 0x2bd008000 -> word0 = base>>8 = 0x02bd0080; 320x180 in word2.
        let dw = [0x02bd_0080, 0x00a0_0000, (179 << 14) | 319, 0, 0, 0, 0, 0];
        assert_eq!(tsharp(&dw), Some((0x2_bd00_8000, 320, 180)));
        // All-zero descriptor: base 0, outside the heap band.
        assert_eq!(tsharp(&[0u32; 8]), None);
        // Too short to be a T#.
        assert_eq!(tsharp(&[0x02bd_0080, 0, 0, 0]), None);
    }
}
