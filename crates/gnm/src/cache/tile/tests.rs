//! Headless detile/re-tile tests (doc-2 §C3/§C9). Pure logic, no GPU: golden 8×8
//! micro-tile patterns hand-computed independently of the production swizzle, the
//! linear-vs-tiled zero-copy predicate, and a seeded `detile ∘ tile == identity`
//! property sweep over random surfaces.

use super::*;

fn tex(width: u32, height: u32, texel: TexelSize, tiling: Tiling) -> SurfaceLayout {
    // pitch 0 → linear-aligned falls back to the align(width, 64) heuristic (task-155), the
    // behavior every pre-existing golden expects.
    tex_pitch(width, height, texel, tiling, 0)
}

fn tex_pitch(
    width: u32,
    height: u32,
    texel: TexelSize,
    tiling: Tiling,
    pitch: u32,
) -> SurfaceLayout {
    SurfaceLayout {
        texel,
        extent: Extent { width, height },
        tiling,
        compression: Compression::Off,
        pitch,
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
fn linear_general_is_identity_and_zero_copy_eligible() {
    let layout = tex(5, 3, TexelSize::Bpp32, Tiling::LinearGeneral);
    let src: Vec<u8> = (0..layout.linear_size() as u32).map(|i| i as u8).collect();
    assert_eq!(detile(&src, &layout).unwrap(), src, "detile identity");
    assert_eq!(tile(&src, &layout).unwrap(), src, "tile identity");
    assert!(
        layout.is_zero_copy_eligible(),
        "linear-general must be zero-copy eligible"
    );
    assert!(layout.tiling.is_linear());
}

#[test]
fn linear_aligned_strips_row_pitch_padding() {
    // A 100-wide 32bpp surface stores at pitch align(100, 64) == 128 (task-153): each
    // stored row is 128 texels, of which only the first 100 are on-screen. Build a padded
    // source where each row's on-screen texels encode (y*width + x) and the padding is a
    // sentinel; detile must recover the tight row-major image and drop the sentinel.
    use ps4_core::tiling::linear_aligned_pitch;
    let (w, h) = (100u32, 3u32);
    let layout = tex(w, h, TexelSize::Bpp32, Tiling::LinearAligned);
    let pitch = linear_aligned_pitch(w);
    assert_eq!(pitch, 128, "100 rounds up to 128");

    let mut padded = vec![0u8; pitch as usize * h as usize * 4];
    for y in 0..h {
        for x in 0..pitch {
            let val: u32 = if x < w { y * w + x } else { 0xDEAD_BEEF };
            let off = (y * pitch + x) as usize * 4;
            padded[off..off + 4].copy_from_slice(&val.to_le_bytes());
        }
    }

    let linear = detile(&padded, &layout).unwrap();
    assert_eq!(linear.len(), layout.linear_size());
    for y in 0..h {
        for x in 0..w {
            let off = (y * w + x) as usize * 4;
            let got = u32::from_le_bytes([
                linear[off],
                linear[off + 1],
                linear[off + 2],
                linear[off + 3],
            ]);
            assert_eq!(
                got,
                y * w + x,
                "detiled texel ({x},{y}) drops pitch padding"
            );
        }
    }

    // Pitch-padded bytes differ from a tight host-linear image, so NOT zero-copy eligible.
    assert!(!layout.is_zero_copy_eligible());
    assert!(!layout.tiling.is_linear());
}

#[test]
fn linear_aligned_uses_decoded_pitch_not_align64() {
    // task-155: a decoded T# pitch that is NOT the align(width, 64) guess must drive the
    // detile stride. width=6 (32bpp) → the heuristic would pick align(6, 64) == 64, but the
    // T# programmed pitch=8. Row 1 must start one *decoded* pitch (8 texels = 32 bytes) in,
    // not 64 texels in — the exact off-by-a-constant that shears Celeste's atlas into bands.
    let (w, h, pitch) = (6u32, 2u32, 8u32);
    let layout = tex_pitch(w, h, TexelSize::Bpp32, Tiling::LinearAligned, pitch);
    assert_eq!(
        layout.aligned_pitch(),
        8,
        "decoded pitch wins over align-64"
    );

    // Source laid out at the decoded pitch: on-screen texels encode y*w + x, padding is a
    // sentinel that must be dropped.
    let mut padded = vec![0u8; pitch as usize * h as usize * 4];
    for y in 0..h {
        for x in 0..pitch {
            let val: u32 = if x < w { y * w + x } else { 0xDEAD_BEEF };
            let off = (y * pitch + x) as usize * 4;
            padded[off..off + 4].copy_from_slice(&val.to_le_bytes());
        }
    }
    let linear = detile(&padded, &layout).unwrap();
    assert_eq!(linear.len(), layout.linear_size());
    // Row 1 texel 0 comes from source byte pitch*bpp (8*4 = 32), proving the decoded stride.
    let row1_x0 = u32::from_le_bytes([linear[w as usize * 4], linear[w as usize * 4 + 1], 0, 0]);
    assert_eq!(
        row1_x0, w,
        "row 1 starts at y*w == {w} (decoded pitch stride)"
    );
    for y in 0..h {
        for x in 0..w {
            let off = (y * w + x) as usize * 4;
            let got = u32::from_le_bytes([
                linear[off],
                linear[off + 1],
                linear[off + 2],
                linear[off + 3],
            ]);
            assert_eq!(got, y * w + x, "detiled texel ({x},{y}) at decoded pitch");
        }
    }
    // detile ∘ tile round-trips at the decoded pitch too.
    let retiled = tile(&linear, &layout).unwrap();
    assert_eq!(detile(&retiled, &layout).unwrap(), linear);
}

#[test]
fn linear_aligned_pitch_zero_falls_back_to_align64() {
    // task-155: pitch 0 (unprogrammed / short descriptor) → the align(width, 64) heuristic,
    // so nothing that relied on the guess regresses. width=100 → 128.
    let layout = tex_pitch(100, 3, TexelSize::Bpp32, Tiling::LinearAligned, 0);
    assert_eq!(
        layout.aligned_pitch(),
        128,
        "pitch 0 falls back to align(100,64)"
    );
    // A pitch narrower than width is nonsensical (drops real texels) → also fall back.
    let narrow = tex_pitch(100, 3, TexelSize::Bpp32, Tiling::LinearAligned, 50);
    assert_eq!(narrow.aligned_pitch(), 128, "too-narrow pitch falls back");
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

/// Pins the tile-mode geometry this dispatch imports from [`ps4_core::tiling`] to its AMD
/// GCN value. The 1D-thin micro-tile is 8×8 texels: Mesa `src/amd/common/ac_surface.c`
/// micro-tile table row `{ADDR_TM_1D_TILED_THIN1, 13, 13, 8, 8, 1, ...}` gives
/// microtile_width 8 and microtile_height 8, so 64 texels per micro-tile. The texel sizes
/// are the byte widths of the 32-/64-bpp formats (bits / 8). Fails if ours drift from AMD's.
#[test]
fn tile_geometry_matches_amd_oracle() {
    use ps4_core::tiling::{MICRO_TILE_DIM, MICRO_TILE_TEXELS};
    // Mesa ac_surface.c micro-tile table: ADDR_TM_1D_TILED_THIN1 → 8×8 texels.
    assert_eq!(MICRO_TILE_DIM, 8);
    assert_eq!(MICRO_TILE_TEXELS, 8 * 8);
    // Texel byte widths: 32 bpp / 8 = 4, 64 bpp / 8 = 8.
    assert_eq!(TexelSize::Bpp32.bytes(), 4);
    assert_eq!(TexelSize::Bpp64.bytes(), 8);
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
