//! Pure detile/re-tile math for the resource cache upload path (doc-2 §C3).
//!
//! PS4 textures and render targets are stored **tiled (swizzled)** in guest memory,
//! not linear; the cache must detile on upload (and re-tile on readback) or every
//! texture is corrupt. This module is the `detile(bytes, layout) -> linear` step the
//! §C3 seam calls for — bytes in, bytes out, Vulkan-free and deterministic.
//!
//! Implemented tile modes:
//! - **linear-general** — identity (guest bytes already linear, pitch == width), so
//!   detile is a copy and the range is zero-copy-import eligible (host layout == guest
//!   bytes, doc-2 §8.2).
//! - **linear-aligned** — row-major, but the row *pitch* is padded up to a hardware
//!   alignment (`ps4_core::tiling::linear_aligned_pitch`). Detile copies each row and
//!   drops the trailing padding; the pitch mismatch means it is NOT zero-copy eligible
//!   (task-153: Celeste's 1500-wide font atlas is stored at pitch 1536).
//! - **GFX7 1D-thin micro-tiled** — the 8×8 micro-tile swizzle for the common power-of-
//!   two texel sizes (32bpp / 64bpp). Tiled resources are never zero-copy eligible
//!   (host linear layout ≠ guest swizzled bytes, doc-2 §C3).
//!
//! 2D macro-tiling (bank/pipe swizzle) is deliberately not implemented; the seam this
//! cements is the [`Tiling`] enum + the [`detile`]/[`tile`] dispatch, not the macro
//! math.
//!
//! Tile-mode names and geometry are the AMD GCN definitions. The three modes are the
//! `ArrayMode` enum values `ARRAY_LINEAR_GENERAL` / `ARRAY_LINEAR_ALIGNED` /
//! `ARRAY_1D_TILED_THIN1` (Mesa `src/amd/registers/gfx6.json` `ArrayMode`, values 0/1/2;
//! the same tile modes appear as `ADDR_TM_LINEAR_ALIGNED` / `ADDR_TM_1D_TILED_THIN1` in
//! Mesa `src/amd/common/ac_surface.c`). The 1D-thin micro-tile is 8×8 texels — Mesa
//! `ac_surface.c` micro-tile table row `{ADDR_TM_1D_TILED_THIN1, 13, 13, 8, 8, 1, ...}`
//! (microtile_width 8, microtile_height 8, so 64 texels per tile). The within-tile texel
//! swizzle and its byte offset live in [`ps4_core::tiling`] (see [`thin1d_texel_offset`]);
//! this module only dispatches and copies. Geometry pinned by
//! `tile_geometry_matches_amd_oracle` in the test module.

/// Data format channel width of one texel, expressed as bytes-per-element. Only the
/// power-of-two sizes the 1D-thin path handles are carried; the enum is the format
/// seam doc-2 §C3 asks for (a full `dfmt`/`nfmt` decode is deferred).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TexelSize {
    /// 32 bits per texel (e.g. R8G8B8A8, R32-float).
    Bpp32,
    /// 64 bits per texel (e.g. R16G16B16A16).
    Bpp64,
}

impl TexelSize {
    /// Bytes occupied by one texel.
    pub fn bytes(self) -> usize {
        match self {
            TexelSize::Bpp32 => 4,
            TexelSize::Bpp64 => 8,
        }
    }
}

/// GCN surface tile mode (doc-2 §C3): the `ArrayMode` enum in Mesa
/// `src/amd/registers/gfx6.json`. Only the linear modes and GFX7 1D-thin micro-tiling are
/// implemented; 2D macro-tiling variants are intentionally absent (the seam is this enum,
/// not the deferred macro math).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Tiling {
    /// Linear, no alignment constraint (`ARRAY_LINEAR_GENERAL`, Mesa `registers/gfx6.json`
    /// `ArrayMode` value 0) — bytes are already row-major linear, pitch == width.
    LinearGeneral,
    /// GFX7 linear-aligned (`ARRAY_LINEAR_ALIGNED` / `ADDR_TM_LINEAR_ALIGNED`, Mesa
    /// `registers/gfx6.json` `ArrayMode` value 1, Mesa `common/ac_surface.c`): row-major, but
    /// the row *pitch* is padded up to `ps4_core::tiling::linear_aligned_pitch` (task-153).
    /// Detile keeps only the first `width` texels of each padded row.
    LinearAligned,
    /// GFX7 1D-thin micro-tiled (`ADDR_TM_1D_TILED_THIN1`, Mesa `common/ac_surface.c`;
    /// `ArrayMode` value 2 in Mesa `registers/gfx6.json`): 8×8 micro-tiles in row-major tile
    /// order, texels swizzled inside each tile.
    Thin1d,
}

impl Tiling {
    /// Whether this mode leaves guest bytes in host-linear order with pitch == width, so
    /// the range is a zero-copy import candidate (doc-2 §8.2): true only for
    /// [`LinearGeneral`](Tiling::LinearGeneral). Linear-aligned (pitch-padded) and the
    /// tiled modes differ from the tight host layout (doc-2 §C3), so they are repacked on
    /// upload, not imported.
    pub fn is_linear(self) -> bool {
        matches!(self, Tiling::LinearGeneral)
    }
}

/// DCC (color) / HTILE (depth) compression state carried on the surface (doc-2 §C9).
/// The first implementation forces surfaces uncompressed (correctness-first); the field
/// exists so a decompress step can land later without reshaping the key.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Compression {
    /// Uncompressed — the only state the detile path handles (doc-2 §C9).
    #[default]
    Off,
}

/// Width × height of a surface in texels (doc-2 §C3). Extents need not be a multiple of
/// the 8×8 micro-tile: the 1D-thin path pads to whole micro-tiles internally and only
/// the in-bounds texels reach the linear output.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Extent {
    pub width: u32,
    pub height: u32,
}

/// A tiled surface's full byte layout: texel size, extent, tile mode, compression
/// (doc-2 §C3/§C9). This is the argument [`detile`]/[`tile`] dispatch on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SurfaceLayout {
    pub texel: TexelSize,
    pub extent: Extent,
    pub tiling: Tiling,
    pub compression: Compression,
    /// Row pitch in texels decoded from the image T# `pitch` field (`sq_img_rsrc_word4[26:13]`
    /// — the 14-bit PITCH at descriptor bits [154:141] of the AMD Sea Islands ISA "Image
    /// Resource" table, ci-isa.pdf; stored minus-one, hence the `+ 1`), or `0` when the
    /// descriptor did not program one. Only the [`Tiling::LinearAligned`]
    /// detile consults it — via [`linear_aligned_pitch_or`], which falls back to the
    /// `align(width, 64)` heuristic when this is `0` or narrower than the width. It is part
    /// of the cache key so two views of the same bytes at different pitches key apart.
    ///
    /// [`linear_aligned_pitch_or`]: ps4_core::tiling::linear_aligned_pitch_or
    pub pitch: u32,
}

impl SurfaceLayout {
    /// The linear-aligned row pitch, in texels, this surface detiles at: the decoded T#
    /// [`pitch`](Self::pitch) when it is usable, else the `align(width, 64)` heuristic
    /// (task-155). The oracle and the upload path must resolve the identical value, so both
    /// go through this one place.
    pub fn aligned_pitch(&self) -> u32 {
        ps4_core::tiling::linear_aligned_pitch_or(self.extent.width, self.pitch)
    }

    /// Bytes of a fully linear (row-major, no padding) view of this surface.
    pub fn linear_size(&self) -> usize {
        self.extent.width as usize * self.extent.height as usize * self.texel.bytes()
    }

    /// Whether a range with this layout can be imported zero-copy (doc-2 §8.2/§C3):
    /// only uncompressed linear surfaces, whose guest bytes already match the host
    /// linear layout.
    pub fn is_zero_copy_eligible(&self) -> bool {
        self.tiling.is_linear() && self.compression == Compression::Off
    }
}

// The 8×8 micro-tile swizzle (`micro_tile_index` / `thin1d_texel_offset`) lives in
// `ps4_core::tiling` so the gcn interpreter's sampling oracle reads a tiled texel from
// the exact same byte this path detiles it from — a second copy would let the two drift
// (task-98).
use ps4_core::tiling::{
    MICRO_TILE_DIM, MICRO_TILE_TEXELS, linear_aligned_texel_offset, thin1d_texel_offset,
};

/// Total byte size of the 1D-thin tiled buffer for a surface: extent rounded up to whole
/// 8×8 micro-tiles, times texel size.
fn thin1d_tiled_size(layout: &SurfaceLayout) -> usize {
    let bytes = layout.texel.bytes();
    let tiles_x = layout.extent.width.div_ceil(MICRO_TILE_DIM);
    let tiles_y = layout.extent.height.div_ceil(MICRO_TILE_DIM);
    (tiles_x * tiles_y * MICRO_TILE_TEXELS) as usize * bytes
}

/// Total byte size of the linear-aligned buffer for a surface: the padded row pitch times
/// the height, times texel size. The last row's off-screen padding is part of the pitch.
fn linear_aligned_tiled_size(layout: &SurfaceLayout) -> usize {
    let bytes = layout.texel.bytes();
    let pitch = layout.aligned_pitch();
    pitch as usize * layout.extent.height as usize * bytes
}

/// Errors from [`detile`] / [`tile`]: the only failure is a `bytes` slice too short for
/// the layout it claims (a boundary check on external input, not internal logic).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TileError {
    /// The input buffer is smaller than `expected` bytes for this layout.
    ShortBuffer { got: usize, expected: usize },
}

/// Detile `tiled` guest bytes into a fresh row-major linear buffer (doc-2 §C3).
/// Linear-general is an identity copy of the first `linear_size` bytes; linear-aligned
/// copies each row and drops the pitch padding; 1D-thin walks each output texel and
/// gathers it from its swizzled source offset.
pub fn detile(tiled: &[u8], layout: &SurfaceLayout) -> Result<Vec<u8>, TileError> {
    match layout.tiling {
        Tiling::LinearGeneral => {
            let need = layout.linear_size();
            if tiled.len() < need {
                return Err(TileError::ShortBuffer {
                    got: tiled.len(),
                    expected: need,
                });
            }
            Ok(tiled[..need].to_vec())
        }
        Tiling::LinearAligned => {
            let need = linear_aligned_tiled_size(layout);
            if tiled.len() < need {
                return Err(TileError::ShortBuffer {
                    got: tiled.len(),
                    expected: need,
                });
            }
            let bytes = layout.texel.bytes();
            let (w, h) = (layout.extent.width, layout.extent.height);
            let pitch = layout.aligned_pitch();
            let row_bytes = w as usize * bytes;
            let mut linear = vec![0u8; layout.linear_size()];
            for y in 0..h as usize {
                let src = linear_aligned_texel_offset(0, y as u32, pitch, bytes);
                let dst = y * row_bytes;
                linear[dst..dst + row_bytes].copy_from_slice(&tiled[src..src + row_bytes]);
            }
            Ok(linear)
        }
        Tiling::Thin1d => {
            let need = thin1d_tiled_size(layout);
            if tiled.len() < need {
                return Err(TileError::ShortBuffer {
                    got: tiled.len(),
                    expected: need,
                });
            }
            let bytes = layout.texel.bytes();
            let (w, h) = (layout.extent.width, layout.extent.height);
            let mut linear = vec![0u8; layout.linear_size()];
            for y in 0..h {
                for x in 0..w {
                    let src = thin1d_texel_offset(x, y, w, bytes);
                    let dst = (y as usize * w as usize + x as usize) * bytes;
                    linear[dst..dst + bytes].copy_from_slice(&tiled[src..src + bytes]);
                }
            }
            Ok(linear)
        }
    }
}

/// Re-tile row-major `linear` bytes back into the surface's tiled layout (doc-2 §C3, the
/// readback direction). Inverse of [`detile`]: linear-general copies through; linear-
/// aligned scatters each row into the padded pitch; 1D-thin scatters each source texel to
/// its swizzled destination offset. Bytes of the tiled buffer not covered by an in-bounds
/// texel (per-row or micro-tile padding) stay zero.
pub fn tile(linear: &[u8], layout: &SurfaceLayout) -> Result<Vec<u8>, TileError> {
    let need = layout.linear_size();
    if linear.len() < need {
        return Err(TileError::ShortBuffer {
            got: linear.len(),
            expected: need,
        });
    }
    match layout.tiling {
        Tiling::LinearGeneral => Ok(linear[..need].to_vec()),
        Tiling::LinearAligned => {
            let bytes = layout.texel.bytes();
            let (w, h) = (layout.extent.width, layout.extent.height);
            let pitch = layout.aligned_pitch();
            let row_bytes = w as usize * bytes;
            let mut tiled = vec![0u8; linear_aligned_tiled_size(layout)];
            for y in 0..h as usize {
                let src = y * row_bytes;
                let dst = linear_aligned_texel_offset(0, y as u32, pitch, bytes);
                tiled[dst..dst + row_bytes].copy_from_slice(&linear[src..src + row_bytes]);
            }
            Ok(tiled)
        }
        Tiling::Thin1d => {
            let bytes = layout.texel.bytes();
            let (w, h) = (layout.extent.width, layout.extent.height);
            let mut tiled = vec![0u8; thin1d_tiled_size(layout)];
            for y in 0..h {
                for x in 0..w {
                    let src = (y as usize * w as usize + x as usize) * bytes;
                    let dst = thin1d_texel_offset(x, y, w, bytes);
                    tiled[dst..dst + bytes].copy_from_slice(&linear[src..src + bytes]);
                }
            }
            Ok(tiled)
        }
    }
}

#[cfg(test)]
mod tests;
