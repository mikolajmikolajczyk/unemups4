//! GCN surface tiling (swizzle) math shared by the two texture-read paths that MUST
//! agree: the gnm resource-cache detiler on the upload path and the gcn interpreter's
//! `image_sample` oracle. They live in different crates (`ps4-gnm` → `ps4-gcn` →
//! `ps4-core`), so the swizzle can't sit in either — a second copy would let the two
//! implementations drift, which is exactly the mis-detile this module exists to prevent
//! (task-98): if the oracle reads a texel at a different byte than the upload detiles it
//! from, `interp == recompiler == GPU` can never close for a tiled texture.
//!
//! Only the linear and GFX7 1D-thin micro-tile layouts are modeled; 2D macro-tiling
//! (bank/pipe swizzle) is deferred, so [`tile_kind`] classifies a macro index as
//! [`TileKind::Macro2d`] and callers reject it rather than silently reading it as 1D.
//! Reference: AMD AddrLib `ComputePixelIndexWithinMicroTile` (thin, non-displayable).

/// Side length of a GFX7 micro-tile, in texels (`8×8`).
pub const MICRO_TILE_DIM: u32 = 8;
/// Texels in one `8×8` micro-tile.
pub const MICRO_TILE_TEXELS: u32 = MICRO_TILE_DIM * MICRO_TILE_DIM;

/// The element index (0..64) of texel `(x, y)` inside its 8×8 micro-tile, for the GFX7
/// 1D-thin non-displayable swizzle. This is the AddrLib
/// `ComputePixelIndexWithinMicroTile` non-displayable branch: X and Y bits interleave
/// (`x0, y0, x1, y1, x2, y2`) — a Morton/Z order within the tile, not grouped X then Y.
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

/// The detile family a GCN tile-mode index selects, for the linear + 1D-thin subset
/// unemups4 implements (task-98). A [`Macro2d`](TileKind::Macro2d) index has no detiler,
/// so callers must defer/reject rather than mis-read it as 1D-thin.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TileKind {
    /// Linear (tile-mode index 0): guest bytes are already row-major.
    Linear,
    /// GFX7 1D-thin micro-tiled (indices 1..=7 in this HLE model).
    Thin1d,
    /// 2D macro-tiled (bank/pipe swizzle, index >= 8): not yet detiled — defer.
    Macro2d,
}

/// Classify a T#/surface tile-mode index into the [`TileKind`] the detile paths handle.
/// Index 0 is linear, 1..=7 are treated as 1D-thin micro-tiled (the pre-task-98 upload
/// behavior), and >= 8 is 2D macro-tiling (GFX bank/pipe swizzle) which is deferred, not
/// mis-read as 1D. Keeping the threshold in one place is what lets the oracle and the
/// upload path agree byte-for-byte.
pub fn tile_kind(tiling_index: u8) -> TileKind {
    match tiling_index {
        0 => TileKind::Linear,
        1..=7 => TileKind::Thin1d,
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
    fn tile_kind_classifies_linear_thin_and_macro() {
        assert_eq!(tile_kind(0), TileKind::Linear);
        for idx in 1..=7 {
            assert_eq!(tile_kind(idx), TileKind::Thin1d, "index {idx}");
        }
        assert_eq!(tile_kind(8), TileKind::Macro2d);
        assert_eq!(tile_kind(13), TileKind::Macro2d);
        assert_eq!(tile_kind(31), TileKind::Macro2d);
    }
}
