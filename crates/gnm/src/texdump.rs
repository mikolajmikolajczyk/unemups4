//! DIAGNOSTIC: env-gated per-texture PNG dump of the DETILED RGBA that the resource cache
//! is about to `UploadImage`. Gated behind `UNEMUPS4_DUMP_TEX=<dir>`; dedup by texture base
//! so a re-uploaded atlas writes one PNG. Self-contained PNG writer (stored DEFLATE, no crate
//! dep) so `ps4-gnm` stays Vulkan/dep-free. Companion to `UNEMUPS4_DUMP_GCN` (shader dumps)
//! and `UNEMUPS4_DUMP_PNG` (swapchain frames): lets you see what texels we actually upload.

use std::collections::HashSet;
use std::sync::Mutex;

use crate::cache::{ResLayout, ResourceKey, SurfaceLayout};

static SEEN: Mutex<Option<HashSet<u64>>> = Mutex::new(None);

/// Dump `linear` (row-major RGBA8) for texture `key` to `$UNEMUPS4_DUMP_TEX/tex_<base>_<w>x<h>_dfmt<dfmt>_tile<tiling>.png`.
/// No-op unless the env var is set. Dedup by `key.addr`.
pub fn dump_texture(key: ResourceKey, surface: &SurfaceLayout, linear: &[u8]) {
    let Some(dir) = std::env::var_os("UNEMUPS4_DUMP_TEX") else {
        return;
    };
    let (dfmt, nfmt) = match key.layout {
        ResLayout::Texture { format, .. } | ResLayout::RenderTarget { format, .. } => {
            (format.dfmt, format.nfmt)
        }
        _ => (255, 255),
    };
    let tiling = surface.tiling;
    let w = surface.extent.width;
    let h = surface.extent.height;

    // Dedup by base: skip an addr a PNG was already written for. Only a CONTAINS check here —
    // the addr is marked seen after a SUCCESSFUL write below, so an addr whose first attempt is
    // skipped by the disk-cap or the short-buffer guard is retried on a later, valid upload
    // rather than silently dropped.
    if SEEN
        .lock()
        .unwrap()
        .as_ref()
        .is_some_and(|set| set.contains(&key.addr))
    {
        return;
    }

    // Disk cap: stop dumping once the dir exceeds ~400MB.
    let dir = std::path::PathBuf::from(dir);
    if let Ok(rd) = std::fs::read_dir(&dir) {
        let total: u64 = rd
            .flatten()
            .filter_map(|e| e.metadata().ok())
            .map(|m| m.len())
            .sum();
        if total > 400 * 1024 * 1024 {
            tracing::warn!(
                base = format_args!("{:#x}", key.addr),
                "UNEMUPS4_DUMP_TEX: dump dir over 400MB, skipping further dumps"
            );
            return;
        }
    }
    let _ = std::fs::create_dir_all(&dir);

    let name = format!(
        "tex_{:#x}_{}x{}_dfmt{}_nfmt{}_tile{:?}.png",
        key.addr, w, h, dfmt, nfmt, tiling
    );
    let path = dir.join(name);

    // Guard against a short/oversized detile buffer (should be w*h*4).
    let want = (w as usize) * (h as usize) * 4;
    if linear.len() < want || w == 0 || h == 0 {
        tracing::warn!(
            base = format_args!("{:#x}", key.addr),
            got = linear.len(),
            want,
            "UNEMUPS4_DUMP_TEX: detiled buffer shorter than w*h*4; skipping"
        );
        return;
    }

    match write_rgba_png(&path, w, h, &linear[..want]) {
        Ok(()) => {
            // Mark seen only now that a PNG actually landed — a skipped attempt above never gets
            // here, so it stays retryable.
            SEEN.lock()
                .unwrap()
                .get_or_insert_with(HashSet::new)
                .insert(key.addr);
            tracing::info!(
                base = format_args!("{:#x}", key.addr),
                w,
                h,
                dfmt,
                nfmt,
                path = %path.display(),
                "UNEMUPS4_DUMP_TEX: wrote texture PNG"
            );
        }
        Err(e) => tracing::warn!(
            base = format_args!("{:#x}", key.addr),
            "UNEMUPS4_DUMP_TEX: PNG write failed: {e}"
        ),
    }
}

/// Minimal RGBA8 PNG writer (stored/uncompressed DEFLATE). Mirrors `ps4-gpu`'s
/// `write_rgba_png` so `ps4-gnm` needs no image/png crate. Shared with the GPU-state
/// snapshot (task-185), which writes the same RGBA8 for its detiled sampled textures — one
/// encoder, so a bug in it shows up identically in both dumps rather than in only one.
pub(crate) fn write_rgba_png(
    path: &std::path::Path,
    w: u32,
    h: u32,
    pixels: &[u8],
) -> std::io::Result<()> {
    use std::io::Write;

    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc: u32 = 0xFFFF_FFFF;
        for &b in bytes {
            crc ^= b as u32;
            for _ in 0..8 {
                let mask = (crc & 1).wrapping_neg();
                crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
            }
        }
        !crc
    }

    fn adler32(bytes: &[u8]) -> u32 {
        let (mut a, mut b): (u32, u32) = (1, 0);
        for &x in bytes {
            a = (a + x as u32) % 65521;
            b = (b + a) % 65521;
        }
        (b << 16) | a
    }

    fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let mut crc_input = Vec::with_capacity(4 + data.len());
        crc_input.extend_from_slice(kind);
        crc_input.extend_from_slice(data);
        out.extend_from_slice(kind);
        out.extend_from_slice(data);
        out.extend_from_slice(&crc32(&crc_input).to_be_bytes());
    }

    // Raw scanlines: each row prefixed by a filter byte (0 = None).
    let mut raw = Vec::with_capacity((w as usize * 4 + 1) * h as usize);
    let row_bytes = w as usize * 4;
    for y in 0..h as usize {
        raw.push(0);
        let start = y * row_bytes;
        raw.extend_from_slice(&pixels[start..start + row_bytes]);
    }

    // zlib stream wrapping stored DEFLATE blocks (no compression).
    let mut zlib = Vec::new();
    zlib.push(0x78);
    zlib.push(0x01);
    let mut off = 0usize;
    while off < raw.len() {
        let remaining = raw.len() - off;
        let block = remaining.min(0xFFFF);
        let is_last = off + block >= raw.len();
        zlib.push(if is_last { 1 } else { 0 });
        zlib.extend_from_slice(&(block as u16).to_le_bytes());
        zlib.extend_from_slice(&(!(block as u16)).to_le_bytes());
        zlib.extend_from_slice(&raw[off..off + block]);
        off += block;
    }
    zlib.extend_from_slice(&adler32(&raw).to_be_bytes());

    let mut png = Vec::new();
    png.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&w.to_be_bytes());
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.push(8);
    ihdr.push(6);
    ihdr.extend_from_slice(&[0, 0, 0]);
    chunk(&mut png, b"IHDR", &ihdr);
    chunk(&mut png, b"IDAT", &zlib);
    chunk(&mut png, b"IEND", &[]);

    let mut f = std::fs::File::create(path)?;
    f.write_all(&png)?;
    Ok(())
}
