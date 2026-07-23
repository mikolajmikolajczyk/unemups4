//! Headless detile/re-tile tests (doc-4 §C3/§C9). Pure logic, no GPU: golden 8×8
//! micro-tile patterns hand-computed independently of the production swizzle, the
//! linear-vs-tiled zero-copy predicate, and a seeded `detile ∘ tile == identity`
//! property sweep over random surfaces.

use super::*;

fn tex(width: u32, height: u32, texel: TexelSize, tiling: Tiling) -> SurfaceLayout {
    SurfaceLayout {
        texel,
        extent: Extent { width, height },
        tiling,
        compression: Compression::Off,
    }
}

/// Hand-laid expected element index for each texel `(x, y)` inside an 8×8 GFX7 1D-thin
/// non-displayable micro-tile. Written out as an explicit literal — the Morton/Z-order
/// interleave `x0,y0,x1,y1,x2,y2` enumerated by hand — so a golden assertion catches a
/// wrong bit in the production `micro_tile_index` instead of re-deriving from the same
/// formula (a tautology that passes for any bijective swizzle).
#[rustfmt::skip]
const GOLDEN_MICRO_TILE: [[u32; 8]; 8] = [
    [ 0,  1,  4,  5, 16, 17, 20, 21],
    [ 2,  3,  6,  7, 18, 19, 22, 23],
    [ 8,  9, 12, 13, 24, 25, 28, 29],
    [10, 11, 14, 15, 26, 27, 30, 31],
    [32, 33, 36, 37, 48, 49, 52, 53],
    [34, 35, 38, 39, 50, 51, 54, 55],
    [40, 41, 44, 45, 56, 57, 60, 61],
    [42, 43, 46, 47, 58, 59, 62, 63],
];

/// Expected within-micro-tile element index for `(x, y)`, from the hand-laid map. Only the
/// low 3 bits of each coordinate select the slot (the tile-outer position is applied by the
/// caller), so callers may pass surface-space coordinates directly.
fn golden_micro_index(x: u32, y: u32) -> u32 {
    GOLDEN_MICRO_TILE[(y & 7) as usize][(x & 7) as usize]
}

/// Build a tiled buffer whose every texel's bytes encode that texel's *tiled element
/// index*, then detiling it must place, at each linear position `(x, y)`, the element
/// index the golden swizzle predicts for `(x, y)`. Independent of the production offset
/// math except the outer per-tile row-major order, which the multi-tile case exercises.
fn golden_single_tile(texel: TexelSize) {
    let bytes = texel.bytes();
    let layout = tex(8, 8, texel, Tiling::Thin1d);
    // One 8×8 micro-tile: 64 elements, each `bytes` wide, value == its element index.
    let mut tiled = vec![0u8; 64 * bytes];
    for elem in 0u32..64 {
        let off = elem as usize * bytes;
        tiled[off..off + 2].copy_from_slice(&(elem as u16).to_le_bytes());
    }

    let linear = detile(&tiled, &layout).unwrap();
    assert_eq!(linear.len(), 64 * bytes);
    for y in 0..8 {
        for x in 0..8 {
            let pos = (y * 8 + x) as usize * bytes;
            let got = u16::from_le_bytes([linear[pos], linear[pos + 1]]);
            assert_eq!(
                u32::from(got),
                golden_micro_index(x, y),
                "detiled texel ({x},{y}) for {texel:?}"
            );
        }
    }
}

#[test]
fn golden_8x8_32bpp() {
    golden_single_tile(TexelSize::Bpp32);
}

#[test]
fn golden_8x8_64bpp() {
    golden_single_tile(TexelSize::Bpp64);
}

/// A 10×6 surface is not a multiple of the 8×8 micro-tile: it pads to 2×1 tiles. Each
/// output texel must come from `tile_index * 64 + micro_index`, with `tile_index` in
/// row-major tile order and only in-bounds texels present in the linear output.
#[test]
fn golden_non_tile_aligned_extent() {
    let (w, h) = (10u32, 6u32);
    let texel = TexelSize::Bpp32;
    let bytes = texel.bytes();
    let layout = tex(w, h, texel, Tiling::Thin1d);

    let tiles_per_row = w.div_ceil(8); // 2
    let tile_rows = h.div_ceil(8); // 1 (h <= 8)
    let total_elems = tiles_per_row * tile_rows * 64;
    let mut tiled = vec![0u8; total_elems as usize * bytes];
    for elem in 0..total_elems {
        let off = elem as usize * bytes;
        tiled[off..off + 4].copy_from_slice(&elem.to_le_bytes());
    }

    let linear = detile(&tiled, &layout).unwrap();
    assert_eq!(linear.len(), (w * h) as usize * bytes);
    for y in 0..h {
        for x in 0..w {
            let tile_x = x / 8;
            let tile_y = y / 8;
            let tile_index = tile_y * tiles_per_row + tile_x;
            let expected = tile_index * 64 + golden_micro_index(x, y);
            let pos = (y * w + x) as usize * bytes;
            let got = u32::from_le_bytes([
                linear[pos],
                linear[pos + 1],
                linear[pos + 2],
                linear[pos + 3],
            ]);
            assert_eq!(got, expected, "detiled texel ({x},{y}) non-aligned");
        }
    }
}

#[test]
fn linear_modes_are_identity_and_zero_copy_eligible() {
    for mode in [Tiling::LinearGeneral, Tiling::LinearAligned] {
        let layout = tex(5, 3, TexelSize::Bpp32, mode);
        let src: Vec<u8> = (0..layout.linear_size() as u32).map(|i| i as u8).collect();
        assert_eq!(
            detile(&src, &layout).unwrap(),
            src,
            "detile identity {mode:?}"
        );
        assert_eq!(tile(&src, &layout).unwrap(), src, "tile identity {mode:?}");
        assert!(
            layout.is_zero_copy_eligible(),
            "linear must be zero-copy eligible {mode:?}"
        );
        assert!(layout.tiling.is_linear());
    }
}

#[test]
fn tiled_is_not_zero_copy_eligible() {
    let layout = tex(16, 16, TexelSize::Bpp32, Tiling::Thin1d);
    assert!(!layout.is_zero_copy_eligible());
    assert!(!layout.tiling.is_linear());
}

#[test]
fn short_buffer_is_reported() {
    let layout = tex(8, 8, TexelSize::Bpp32, Tiling::Thin1d);
    let short = vec![0u8; 4];
    assert!(matches!(
        detile(&short, &layout),
        Err(TileError::ShortBuffer { .. })
    ));
}

/// Deterministic splitmix64 — fixed-seed PRNG so the property sweep is reproducible with
/// no wall-clock / ambient randomness and no extra dependency.
struct SplitMix64(u64);

impl SplitMix64 {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn range(&mut self, lo: u32, hi: u32) -> u32 {
        lo + (self.next_u64() % u64::from(hi - lo + 1)) as u32
    }
}

/// `detile ∘ tile == identity` on random surfaces (AC #3): re-tile a random linear
/// surface then detile it back and require the original bytes. Covers both texel sizes,
/// both linear modes (trivially) and 1D-thin over non-tile-aligned extents.
#[test]
fn detile_of_tile_is_identity() {
    let mut rng = SplitMix64(0x5EED_1D0C_0FFE_E123);
    for _ in 0..256 {
        let texel = if rng.next_u64() & 1 == 0 {
            TexelSize::Bpp32
        } else {
            TexelSize::Bpp64
        };
        let tiling = match rng.range(0, 2) {
            0 => Tiling::LinearGeneral,
            1 => Tiling::LinearAligned,
            _ => Tiling::Thin1d,
        };
        let w = rng.range(1, 20);
        let h = rng.range(1, 20);
        let layout = tex(w, h, texel, tiling);

        let linear: Vec<u8> = (0..layout.linear_size())
            .map(|_| rng.next_u64() as u8)
            .collect();

        let tiled = tile(&linear, &layout).unwrap();
        let round = detile(&tiled, &layout).unwrap();
        assert_eq!(round, linear, "roundtrip {w}x{h} {texel:?} {tiling:?}");
    }
}
