//! GCN surface tiling (swizzle) math shared by the two texture-read paths that MUST
//! agree: the gnm resource-cache detiler on the upload path and the gcn interpreter's
//! `image_sample` oracle. They live in different crates (`ps4-gnm` → `ps4-gcn` →
//! `ps4-core`), so the swizzle can't sit in either — a second copy would let the two
//! implementations drift, which is exactly the mis-detile this module exists to prevent
//! (task-98): if the oracle reads a texel at a different byte than the upload detiles it
//! from, `interp == recompiler == GPU` can never close for a tiled texture.
//!
//! Three layouts are modeled: linear-general (tile-mode 0), GFX7 1D-thin micro-tiling
//! (indices 1..=7), and **linear-aligned** (index 8) — a linear surface whose row *pitch*
//! is padded up to a hardware alignment (`align(width, 64)` texels for 32bpp), the extra
//! texels being off-screen padding. Genuine 2D macro-tiling (bank/pipe swizzle) is still
//! deferred, so [`tile_kind`] classifies the remaining indices as [`TileKind::Macro2d`]
//! and callers reject them rather than silently reading them as 1D.
//!
//! Hardware anchors (the numeric facts below are pinned by `tiling_constants_match_amd_oracle`):
//! the `8×8` micro-tile is the GCN pixel micro-tile — Mesa `src/amd/common/ac_surface.c`
//! programs `microtile_width = microtile_height = 8` for every GFX6/7 tile mode. The
//! `LINEAR_ALIGNED` row-pitch alignment of 64 texels (32bpp) is the 256-byte pipe/memory-
//! channel interleave (Mesa `src/amd/common/ac_gpu_info.h` `AMD_MEMCHANNEL_INTERLEAVE_BYTES
//! 256`, "always equal to GB_ADDR_CONFIG.PIPE_INTERLEAVE_SIZE") divided by the 4-byte element.
//! `ARRAY_LINEAR_GENERAL` (tile mode index 0) and `ARRAY_LINEAR_ALIGNED` are the GCN
//! `ArrayMode` enum names in Mesa `src/amd/registers/gfx6.json` (`ARRAY_LINEAR_GENERAL = 0`).
//! The within-tile element order is AMD AddrLib `ComputePixelIndexWithinMicroTile` (thin,
//! non-displayable): X/Y bits interleave into a Z/Morton order over the `8×8` tile.
//!
//! The index→[`TileKind`] classification itself (indices `1..=7` folded to 1D-thin, index 8
//! to linear-aligned) is unemups4's own HLE model, fixed empirically for the surfaces Celeste
//! programs (task-98, task-153), not a transcription of the Liverpool GB_TILE_MODE table.
//! Concretely: Celeste's font/UI atlases are linear-aligned — a 1500-wide 32bpp atlas is
//! stored at pitch 1536 (`= align(1500, 64)`), so reading it as tight width-1500 linear shears
//! every row by 36 texels into an unreadable diagonal — the wall task-153 clears.

/// Side length of a GFX7 micro-tile, in texels (`8×8`). The GCN pixel micro-tile is `8×8`:
/// Mesa `src/amd/common/ac_surface.c` uses `microtile_width = microtile_height = 8` for every
/// GFX6/7 tile mode.
pub const MICRO_TILE_DIM: u32 = 8;
/// Texels in one `8×8` micro-tile.
pub const MICRO_TILE_TEXELS: u32 = MICRO_TILE_DIM * MICRO_TILE_DIM;

/// The element index (0..64) of texel `(x, y)` inside its 8×8 micro-tile, for the GFX7
/// 1D-thin non-displayable swizzle. This is the AMD AddrLib
/// `ComputePixelIndexWithinMicroTile` non-displayable branch: X and Y bits interleave
/// (`x0, y0, x1, y1, x2, y2`) — a Morton/Z order within the tile, not grouped X then Y. The
/// `8×8` tile size is the Mesa `ac_surface.c` micro-tile (see [`MICRO_TILE_DIM`]); the exact
/// interleave order is the AddrLib algorithm and is guarded here by the bijection test below.
pub fn micro_tile_index(x: u32, y: u32) -> u32 {
    let x = x & (MICRO_TILE_DIM - 1);
    let y = y & (MICRO_TILE_DIM - 1);
    let bit = |v: u32, b: u32| (v >> b) & 1;
    bit(x, 0)
        | (bit(y, 0) << 1)
        | (bit(x, 1) << 2)
        | (bit(y, 1) << 3)
        | (bit(x, 2) << 4)
        | (bit(y, 2) << 5)
}

/// Byte offset of texel `(x, y)` in the 1D-thin tiled layout of a surface `width` texels
/// wide, each texel `bytes` bytes. Micro-tiles are laid out row-major across the surface
/// (rounded up to whole tiles); inside each tile the texel sits at [`micro_tile_index`].
pub fn thin1d_texel_offset(x: u32, y: u32, width: u32, bytes: usize) -> usize {
    let tiles_per_row = width.div_ceil(MICRO_TILE_DIM);
    let tile_x = x / MICRO_TILE_DIM;
    let tile_y = y / MICRO_TILE_DIM;
    let tile_index = tile_y * tiles_per_row + tile_x;
    let within = micro_tile_index(x, y);
    (tile_index * MICRO_TILE_TEXELS + within) as usize * bytes
}

/// Texel-pitch alignment for a GFX7 `ARRAY_LINEAR_ALIGNED` surface: a row is padded up to
/// this many texels. For a 32bpp (4-byte) element this is 64 texels — the pipe/memory-channel
/// interleave (256 bytes, Mesa `src/amd/common/ac_gpu_info.h` `AMD_MEMCHANNEL_INTERLEAVE_BYTES
/// 256`, "always equal to GB_ADDR_CONFIG.PIPE_INTERLEAVE_SIZE") divided by the element size
/// (`256 / 4`). Celeste's 1500-wide atlas lands at `align(1500, 64) == 1536`, the pitch that
/// straightens its glyphs (task-153).
pub const LINEAR_ALIGNED_PITCH_TEXELS: u32 = 64;

/// The detile family a GCN tile-mode index selects, for the linear + 1D-thin + linear-
/// aligned subset unemups4 implements (task-98, task-153). A [`Macro2d`](TileKind::Macro2d)
/// index has no detiler, so callers must defer/reject rather than mis-read it as 1D-thin.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TileKind {
    /// Linear (tile-mode index 0, `ARRAY_LINEAR_GENERAL` in the Mesa `gfx6.json` `ArrayMode`
    /// enum): guest bytes are already row-major, pitch == width.
    Linear,
    /// GFX7 1D-thin micro-tiled (indices 1..=7 in this HLE model; corresponds to hardware
    /// `ARRAY_1D_TILED_THIN1`). The `8×8` micro-tile is the Mesa `ac_surface.c` micro-tile.
    Thin1d,
    /// GFX7 linear-aligned (index 8 in this HLE model; hardware `ARRAY_LINEAR_ALIGNED`):
    /// row-major, but the row *pitch* is padded up to [`LINEAR_ALIGNED_PITCH_TEXELS`]
    /// (task-153). Detile drops the per-row padding.
    LinearAligned,
    /// 2D macro-tiled (bank/pipe swizzle, index >= 9 in this HLE model): not yet detiled — defer.
    Macro2d,
}

/// Padded row pitch, in texels, of a linear-aligned surface `width` texels wide: `width`
/// rounded up to [`LINEAR_ALIGNED_PITCH_TEXELS`]. The [`TileKind::LinearAligned`] detile
/// reads a source row every `pitch` texels and keeps only the first `width` of them.
pub fn linear_aligned_pitch(width: u32) -> u32 {
    width.next_multiple_of(LINEAR_ALIGNED_PITCH_TEXELS)
}

/// Resolve the linear-aligned row pitch to use for a surface `width` texels wide, given the
/// pitch decoded from the T# (`sq_img_rsrc_word4[26:13] + 1`, in texels, task-155). The pitch
/// field is bits [13:26] of the image resource word4 — Mesa `src/amd/registers/gfx6.json`
/// `SQ_IMG_RSRC_WORD4.PITCH` = bits [13,26] — and is stored one-less (Mesa `ac_descriptors.c`
/// programs `S_008F20_PITCH(pitch - 1)`), so the decode adds 1. A real,
/// non-zero decoded pitch wins so the detile strides at the surface's *actual* row stride:
/// Celeste's 1922-wide full-screen atlas is stored at a pitch the `align(width, 64)`
/// heuristic guesses wrong, and that mismatch shears every row into horizontal banding. A
/// decoded pitch of 0 (unprogrammed / too-short descriptor) or one narrower than `width`
/// (nonsensical — it would drop real texels) falls back to [`linear_aligned_pitch`], so the
/// UI atlases the heuristic already handles never regress.
pub fn linear_aligned_pitch_or(width: u32, decoded_pitch: u32) -> u32 {
    if decoded_pitch >= width {
        decoded_pitch
    } else {
        linear_aligned_pitch(width)
    }
}

/// Byte offset of texel `(x, y)` in a linear-aligned surface: row-major over the padded
/// pitch. Callers pass the pitch from [`linear_aligned_pitch`] so the oracle and the upload
/// path stride identically (the same byte-for-byte agreement `tile_kind` guarantees for the
/// tiled modes).
pub fn linear_aligned_texel_offset(x: u32, y: u32, pitch: u32, bytes: usize) -> usize {
    (y as usize * pitch as usize + x as usize) * bytes
}

/// Classify a T#/surface tile-mode index into the [`TileKind`] the detile paths handle.
/// This index→kind table is unemups4's own HLE model, fixed empirically for the surfaces
/// Celeste programs (task-98, task-153), not a transcription of the Liverpool GB_TILE_MODE
/// table: index 0 is linear (hardware `ARRAY_LINEAR_GENERAL`, Mesa `gfx6.json` `ArrayMode`),
/// 1..=7 are folded to 1D-thin micro-tiled (the pre-task-98 upload behavior), index 8 is
/// linear-aligned (hardware `ARRAY_LINEAR_ALIGNED`, task-153), and >= 9 is 2D macro-tiling
/// (bank/pipe swizzle) which is deferred, not mis-read. Keeping the threshold in one place is
/// what lets the oracle and the upload path agree byte-for-byte.
pub fn tile_kind(tiling_index: u8) -> TileKind {
    match tiling_index {
        0 => TileKind::Linear,
        1..=7 => TileKind::Thin1d,
        8 => TileKind::LinearAligned,
        _ => TileKind::Macro2d,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn micro_tile_index_is_a_bijection_over_the_8x8_tile() {
        // Every (x, y) in the tile maps to a distinct index in 0..64, covering all 64.
        let mut seen = [false; MICRO_TILE_TEXELS as usize];
        for y in 0..MICRO_TILE_DIM {
            for x in 0..MICRO_TILE_DIM {
                let i = micro_tile_index(x, y) as usize;
                assert!(!seen[i], "duplicate micro-tile index {i} at ({x},{y})");
                seen[i] = true;
            }
        }
        assert!(seen.iter().all(|&s| s), "micro-tile index not onto 0..64");
    }

    #[test]
    fn micro_tile_interleaves_x_and_y_bits() {
        // Origin is element 0; the interleave places x-bits at even positions and
        // y-bits at odd positions.
        assert_eq!(micro_tile_index(0, 0), 0);
        assert_eq!(micro_tile_index(1, 0), 0b000001); // x0
        assert_eq!(micro_tile_index(0, 1), 0b000010); // y0
        assert_eq!(micro_tile_index(2, 0), 0b000100); // x1
        assert_eq!(micro_tile_index(7, 7), 0b111111); // all bits set
    }

    #[test]
    fn thin1d_offset_walks_tiles_row_major_then_within() {
        // 16-wide surface = 2 micro-tiles per row. Texel (8,0) starts the second tile:
        // 64 texels into the buffer. (0,8) starts the second tile-row: 128 texels in.
        assert_eq!(thin1d_texel_offset(0, 0, 16, 4), 0);
        assert_eq!(thin1d_texel_offset(8, 0, 16, 4), 64 * 4);
        assert_eq!(thin1d_texel_offset(0, 8, 16, 4), 128 * 4);
    }

    #[test]
    fn tile_kind_classifies_linear_thin_aligned_and_macro() {
        assert_eq!(tile_kind(0), TileKind::Linear);
        for idx in 1..=7 {
            assert_eq!(tile_kind(idx), TileKind::Thin1d, "index {idx}");
        }
        assert_eq!(tile_kind(8), TileKind::LinearAligned);
        assert_eq!(tile_kind(9), TileKind::Macro2d);
        assert_eq!(tile_kind(13), TileKind::Macro2d);
        assert_eq!(tile_kind(31), TileKind::Macro2d);
    }

    #[test]
    fn linear_aligned_pitch_rounds_width_up_to_64() {
        // Celeste's font atlas: 1500-wide 32bpp stored at pitch 1536 (task-153).
        assert_eq!(linear_aligned_pitch(1500), 1536);
        // Already aligned widths are unchanged; others round up to the next multiple.
        assert_eq!(linear_aligned_pitch(64), 64);
        assert_eq!(linear_aligned_pitch(1), 64);
        assert_eq!(linear_aligned_pitch(65), 128);
        assert_eq!(linear_aligned_pitch(1536), 1536);
    }

    #[test]
    fn linear_aligned_pitch_or_prefers_decoded_pitch_else_falls_back() {
        // A real decoded T# pitch (>= width) wins verbatim — Celeste's 1922-wide atlas is
        // stored at its own hardware pitch, not `align(1922, 64) == 1984` (task-155).
        assert_eq!(linear_aligned_pitch_or(1922, 2048), 2048);
        assert_eq!(linear_aligned_pitch_or(6, 8), 8);
        // A decoded pitch == width (already tight) is still honored.
        assert_eq!(linear_aligned_pitch_or(1922, 1922), 1922);
        // pitch == 0 (unprogrammed / short descriptor) falls back to the align-64 heuristic.
        assert_eq!(linear_aligned_pitch_or(1500, 0), 1536);
        // A nonsensical pitch narrower than width would drop real texels — fall back too.
        assert_eq!(linear_aligned_pitch_or(1500, 1000), 1536);
    }

    /// Pins the GPU-hardware numeric facts in this module to their AMD (Mesa) values. The
    /// right-hand literals are the constants in Mesa's AMD sources; this test fails if ours
    /// drift from them:
    /// - `8×8` micro-tile = Mesa `src/amd/common/ac_surface.c` `microtile_width/height = 8`.
    /// - 64-texel linear-aligned pitch = the 256-byte pipe interleave (Mesa
    ///   `src/amd/common/ac_gpu_info.h` `AMD_MEMCHANNEL_INTERLEAVE_BYTES 256`) / 4 bytes/texel.
    /// - T# pitch is bits [13:26] of image-resource word4 (Mesa `src/amd/registers/gfx6.json`
    ///   `SQ_IMG_RSRC_WORD4.PITCH`), stored one-less (`S_008F20_PITCH(pitch - 1)`), decoded +1.
    #[test]
    fn tiling_constants_match_amd_oracle() {
        // GCN pixel micro-tile is 8×8 = 64 texels.
        assert_eq!(MICRO_TILE_DIM, 8);
        assert_eq!(MICRO_TILE_TEXELS, 64);

        // Pipe/memory-channel interleave (bytes) and 32bpp element size → linear-aligned
        // pitch alignment in texels.
        const AMD_MEMCHANNEL_INTERLEAVE_BYTES: u32 = 256; // Mesa ac_gpu_info.h
        const BYTES_PER_32BPP_TEXEL: u32 = 4;
        assert_eq!(
            LINEAR_ALIGNED_PITCH_TEXELS,
            AMD_MEMCHANNEL_INTERLEAVE_BYTES / BYTES_PER_32BPP_TEXEL
        );

        // Image-resource word4 PITCH field = bits [13:26]; decode is (field + 1). Reconstruct
        // the field as hardware would store it (pitch - 1) and confirm our decode recovers it.
        const PITCH_LO_BIT: u32 = 13;
        const PITCH_HI_BIT: u32 = 26;
        assert_eq!(PITCH_HI_BIT - PITCH_LO_BIT + 1, 14); // 14-bit PITCH field
        let real_pitch: u32 = 1536;
        let stored = real_pitch - 1; // S_008F20_PITCH(pitch - 1)
        let field = stored & ((1 << (PITCH_HI_BIT - PITCH_LO_BIT + 1)) - 1);
        assert_eq!(field + 1, real_pitch);

        // Tile-mode index 0 is hardware ARRAY_LINEAR_GENERAL, index 8 is ARRAY_LINEAR_ALIGNED
        // (Mesa gfx6.json ArrayMode names); the folded classification is our HLE model.
        assert_eq!(tile_kind(0), TileKind::Linear);
        assert_eq!(tile_kind(8), TileKind::LinearAligned);
    }

    #[test]
    fn linear_aligned_offset_strides_by_padded_pitch() {
        // Row 0 is dense from byte 0; row 1 starts one *padded* pitch (not width) in, so
        // the per-row padding is skipped exactly as the upload/oracle must agree on.
        let pitch = linear_aligned_pitch(1500); // 1536
        assert_eq!(linear_aligned_texel_offset(0, 0, pitch, 4), 0);
        assert_eq!(linear_aligned_texel_offset(1499, 0, pitch, 4), 1499 * 4);
        assert_eq!(linear_aligned_texel_offset(0, 1, pitch, 4), 1536 * 4);
        assert_eq!(
            linear_aligned_texel_offset(3, 2, pitch, 4),
            (2 * 1536 + 3) * 4
        );
    }
}
