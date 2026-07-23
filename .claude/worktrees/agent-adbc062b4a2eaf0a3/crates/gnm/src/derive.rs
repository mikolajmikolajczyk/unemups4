//! Register → pipeline-state derivation (doc-4 §5/§C7): read the shadow register file
//! at draw time and snapshot the pipeline-relevant bits into the backend-facing
//! [`TargetDesc`] and [`PipelineKey`]. New state is *decoding more register indices*,
//! never restructuring — this module is that "register → pipeline state" translation.
//!
//! Scope (doc-4 §5, §C3/§C9):
//! * color target from `CB_COLOR0_BASE/PITCH/INFO/ATTRIB` — format + a tiling field
//!   carried per §C3/§C9 even while the first implementation forces surfaces linear +
//!   uncompressed (HTILE/DCC off, §C9);
//! * viewport/scissor from `PA_CL_VPORT_*` / `PA_SC_SCREEN_SCISSOR_*`;
//! * blend from `CB_BLEND0_CONTROL` / `CB_COLOR_CONTROL`;
//! * depth presence from `DB_Z_INFO` / `DB_DEPTH_CONTROL`.
//!
//! The color target is mapped to the videoout framebuffer when `CB_COLOR0_BASE`
//! matches a registered display buffer (via the [`DisplayBufferSource`] seam); an
//! arbitrary RT (unregistered base) is out of scope this phase and defers the draw.
//! An unrecognized color format defers the draw cleanly (AC #3).
//!
//! `PipelineKey` carries a shader *identity* (a stable hash per stage), the vertex
//! layout, the RT format and the blend/depth bits — not a hardcoded pipeline handle
//! (doc-4 §4 "must not hardcode"), so phase 4's arbitrary shaders key on it and the
//! backend caches by value.

use ps4_core::gpu::{
    BlendKey, ColorFormat, DepthKey, DisplayBuffer, PipelineKey, TargetDesc, Tiling, VertexLayout,
    display_buffers,
};

use crate::pm4::opcodes::context_reg as ctx;
use crate::shader::source::{ShaderRef, Stage};
use crate::state::{BoundShaders, GpuState};

/// A screen-space viewport derived from `PA_CL_VPORT_*` (doc-4 §5). The GFX6 viewport
/// is programmed as scale/offset (NDC → screen); the pixel rect is
/// `x = xoffset - xscale`, `width = 2 * xscale` (and the same for Y). `f32` bits are
/// read from the register words.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Viewport {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

/// A screen scissor rect derived from `PA_SC_SCREEN_SCISSOR_TL/BR` (doc-4 §5). Each
/// register packs `x` in bits [15:0] and `y` in bits [31:16].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Scissor {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

/// Why a draw's color target could not be derived (doc-4 §5). Each variant is a
/// *clean defer*, never a crash — the draw is skipped and logged (AC #3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TargetError {
    /// `CB_COLOR0_BASE` was never programmed — no color target bound.
    NoColorBase,
    /// The color base names no registered display buffer. Arbitrary render targets
    /// are out of scope this phase (a later task maps them); the draw defers.
    UnregisteredTarget { base: u64 },
    /// The `CB_COLOR0_INFO` format field is a value the decoder does not map to a host
    /// format (AC #3): defer rather than guess.
    UnsupportedFormat { info: u32 },
}

/// The pipeline-relevant state a draw derives from the shadow register file (doc-4
/// §5): the backend-facing [`TargetDesc`] + [`PipelineKey`] plus the viewport/scissor
/// the draw sets. Produced by [`derive_draw_state`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DrawState {
    pub target: TargetDesc,
    pub pipeline: PipelineKey,
    pub viewport: Viewport,
    pub scissor: Scissor,
}

/// Derive the color [`TargetDesc`] from the `CB_COLOR0_*` context registers (doc-4
/// §5/§C3/§C9). Maps the target to the videoout framebuffer when `CB_COLOR0_BASE`
/// matches a registered display buffer; an unregistered base or an unrecognized
/// format is a clean [`TargetError`] the caller defers on (AC #3).
pub fn derive_target(state: &GpuState) -> Result<TargetDesc, TargetError> {
    let base_reg = state
        .ctx_regs
        .get(ctx::CB_COLOR0_BASE)
        .ok_or(TargetError::NoColorBase)?;
    // CB_COLOR0_BASE is in 256-byte units (the byte address is `<< 8`), same shift as
    // the shader PGM address.
    let base = (base_reg as u64) << 8;

    let info = state.ctx_regs.get(ctx::CB_COLOR0_INFO).unwrap_or(0);
    let format = color_format(info).ok_or(TargetError::UnsupportedFormat { info })?;

    // The color base is only trusted once it matches a registered display buffer.
    // Arbitrary RTs (unregistered base) are out of scope this phase — defer (§5).
    let fb = lookup_display_buffer(base).ok_or(TargetError::UnregisteredTarget { base })?;

    let attrib = state.ctx_regs.get(ctx::CB_COLOR0_ATTRIB).unwrap_or(0);
    let tiling = tiling_from_attrib(attrib);

    // PITCH tile-max is +1 in tile units; for a framebuffer-aliased target the display
    // geometry is authoritative for width/height, and pitch defaults to width when the
    // guest programmed no distinct linear pitch.
    let pitch = pitch_pixels(state).unwrap_or(fb.width);

    Ok(TargetDesc {
        width: fb.width,
        height: fb.height,
        pitch,
        format,
        tiling,
    })
}

/// Derive the full [`DrawState`] (target + pipeline + viewport + scissor) from the
/// shadow register file and the bound shaders (doc-4 §5). Returns the target-derivation
/// error unchanged so the executor can defer + log the specific reason (AC #3).
pub fn derive_draw_state(state: &GpuState, bound: &BoundShaders) -> Result<DrawState, TargetError> {
    let target = derive_target(state)?;
    let pipeline = derive_pipeline(state, bound, target.format);
    let viewport = derive_viewport(state);
    let scissor = derive_scissor(state);
    Ok(DrawState {
        target,
        pipeline,
        viewport,
        scissor,
    })
}

/// Snapshot the pipeline-relevant register bits into a [`PipelineKey`] (doc-4 §4/§5).
/// Carries a per-stage shader **identity** (a stable hash of the bound `ShaderRef`),
/// the vertex layout, the RT `color_format`, and the blend/depth bits — never a
/// hardcoded pipeline handle. Two draws agreeing on every field name one host
/// pipeline (AC #2).
pub fn derive_pipeline(
    state: &GpuState,
    bound: &BoundShaders,
    color_format: ColorFormat,
) -> PipelineKey {
    PipelineKey {
        vs_hash: bound.vs.map(shader_hash).unwrap_or(0),
        ps_hash: bound.ps.map(shader_hash).unwrap_or(0),
        vertex_layout: derive_vertex_layout(bound),
        color_format,
        blend: derive_blend(state),
        depth: derive_depth(state),
    }
}

/// The vertex-input layout the pipeline is built against (doc-4 §4/§C4). The embedded
/// fullscreen-quad draw reads `gl_VertexIndex` and binds no vertex buffer, so an
/// embedded VS yields `None`. A register/fetch-shader-derived layout for real shaders
/// is phase-4 work; carried as a distinct field so it re-keys the pipeline then.
fn derive_vertex_layout(bound: &BoundShaders) -> Option<VertexLayout> {
    match bound.vs {
        Some(ShaderRef::Embedded { .. }) | None => None,
        // A register-bound GCN VS would carry a fetch-shader-derived layout (phase 4).
        Some(ShaderRef::GcnBinary { .. }) => None,
    }
}

/// Blend bits from `CB_BLEND0_CONTROL` + `CB_COLOR_CONTROL` (doc-4 §5). `enable` is the
/// MRT0 blend-enable bit; `control` is the raw `CB_BLEND0_CONTROL` word (carried
/// verbatim so any factor/equation change re-keys the pipeline). MRT>1 is out of scope.
fn derive_blend(state: &GpuState) -> BlendKey {
    let control = state.ctx_regs.get(ctx::CB_BLEND0_CONTROL).unwrap_or(0);
    // GFX6 CB_BLEND0_CONTROL.ENABLE is bit 30.
    let enable = control & (1 << 30) != 0;
    BlendKey { enable, control }
}

/// Depth bits from `DB_DEPTH_CONTROL` + `DB_Z_INFO` (doc-4 §5). Depth is present when
/// depth testing is enabled *and* a depth surface format is programmed. HTILE is off
/// (§C9), so no compression metadata is carried.
fn derive_depth(state: &GpuState) -> DepthKey {
    let control = state.ctx_regs.get(ctx::DB_DEPTH_CONTROL).unwrap_or(0);
    let z_info = state.ctx_regs.get(ctx::DB_Z_INFO).unwrap_or(0);
    // GFX6 DB_DEPTH_CONTROL.Z_ENABLE is bit 1; DB_Z_INFO.FORMAT (bits [1:0]) != 0 means
    // a real depth surface (FORMAT 0 = invalid/no surface).
    let z_enable = control & (1 << 1) != 0;
    let has_surface = z_info & 0x3 != 0;
    DepthKey {
        enable: z_enable && has_surface,
        control,
    }
}

/// Viewport-0 pixel rect from the `PA_CL_VPORT_*` scale/offset registers (doc-4 §5).
/// Registers hold `f32` bit patterns. The GFX6 viewport transform maps NDC → screen as
/// `screen = offset + scale * ndc` with `ndc ∈ [-1, +1]`, so the two edges sit at
/// `offset - scale` (at `ndc = -1`) and `offset + scale` (at `ndc = +1`).
///
/// A `vk::Viewport` describes the same transform as `screen = y + (ndc*0.5 + 0.5)*height`,
/// so its `x`/`y` fields are the screen coordinate at `ndc = -1` and its `width`/`height`
/// are the signed span to `ndc = +1`. Matching the two transforms term-for-term therefore
/// requires the *signed* GCN edge at `ndc = -1`, NOT its absolute value:
///
/// * X (never flipped in the corpus): `x = xoffset - xscale`, `width = 2 * xscale`.
/// * Y: `y = yoffset - yscale`, `height = 2 * yscale`.
///
/// A Y-flipped GCN viewport (`yscale < 0`, e.g. `YSCALE = -H/2`, `YOFFSET = H/2`) thus
/// yields `y = H`, `height = -H` — a region spanning `[0, H]` with the flip carried in the
/// height's sign. That is exactly the portable Vulkan negative-height Y-flip (VK 1.1 core /
/// `KHR_maintenance1`, MoltenVK/Metal-safe): the backend passes the signed rect straight to
/// `vkCmdSetViewport`, so the negative height *is* the Y-flip — no separate matrix. Taking
/// `|yscale|` for `y` instead (as an earlier revision did) places a flipped viewport's
/// origin at `[-H, 0]`, entirely above the framebuffer, so nothing rasterizes.
///
/// Unwritten registers read as 0 → a zero rect.
pub fn derive_viewport(state: &GpuState) -> Viewport {
    let f = |idx: u32| f32::from_bits(state.ctx_regs.get(idx).unwrap_or(0));
    let xscale = f(ctx::PA_CL_VPORT_XSCALE);
    let xoffset = f(ctx::PA_CL_VPORT_XOFFSET);
    let yscale = f(ctx::PA_CL_VPORT_YSCALE);
    let yoffset = f(ctx::PA_CL_VPORT_YOFFSET);
    Viewport {
        x: xoffset - xscale,
        y: yoffset - yscale,
        width: 2.0 * xscale,
        height: 2.0 * yscale,
    }
}

/// Screen scissor from `PA_SC_SCREEN_SCISSOR_TL/BR` (doc-4 §5): each register packs
/// `x` in bits [15:0] and `y` in bits [31:16]. Width/height are `BR - TL` (clamped at
/// zero so a `BR < TL` malformed pair is an empty scissor, never a wrapping size).
pub fn derive_scissor(state: &GpuState) -> Scissor {
    let tl = state
        .ctx_regs
        .get(ctx::PA_SC_SCREEN_SCISSOR_TL)
        .unwrap_or(0);
    let br = state
        .ctx_regs
        .get(ctx::PA_SC_SCREEN_SCISSOR_BR)
        .unwrap_or(0);
    let (tl_x, tl_y) = (signed16(tl & 0xFFFF), signed16(tl >> 16));
    let (br_x, br_y) = (signed16(br & 0xFFFF), signed16(br >> 16));
    Scissor {
        x: tl_x,
        y: tl_y,
        width: (br_x - tl_x).max(0) as u32,
        height: (br_y - tl_y).max(0) as u32,
    }
}

/// `CB_COLOR0_PITCH` in pixels: GFX6 packs `TILE_MAX` (pitch/8 − 1) in bits [10:0], so
/// the pixel pitch is `(TILE_MAX + 1) * 8`. `None` if the register was never written.
fn pitch_pixels(state: &GpuState) -> Option<u32> {
    let pitch = state.ctx_regs.get(ctx::CB_COLOR0_PITCH)?;
    Some(((pitch & 0x7FF) + 1) * 8)
}

/// Map `CB_COLOR0_INFO.FORMAT` (bits [5:2]) to a host [`ColorFormat`], or `None` for a
/// value the decoder does not support this phase (the draw then defers, AC #3). Only
/// the formats the corpus needs are mapped; the set grows as draws need each one.
fn color_format(info: u32) -> Option<ColorFormat> {
    // GFX6 CB color FORMAT enum (bits [5:2]). COLOR_8_8_8_8 = 0x0A is the videoout
    // framebuffer format; channel order (BGRA vs RGBA) comes from the number/swap bits
    // but the videoout fb is BGRA, matching the present path.
    const COLOR_8_8_8_8: u32 = 0x0A;
    match (info >> 2) & 0xF {
        COLOR_8_8_8_8 => Some(ColorFormat::B8G8R8A8Unorm),
        _ => None,
    }
}

/// Tiling from `CB_COLOR0_ATTRIB` (doc-4 §C3/§C9). GFX6/7 packs `TILE_MODE_INDEX` in bits
/// [4:0] (FMASK_TILE_MODE_INDEX sits above it at [9:5]); index 0 is the linear array mode.
/// The mode is *carried* even while the first implementation forces surfaces linear (§C9)
/// — the deferred detile step keys on it.
fn tiling_from_attrib(attrib: u32) -> Tiling {
    let tile_mode_index = attrib & 0x1F;
    if tile_mode_index == 0 {
        Tiling::Linear
    } else {
        Tiling::Tiled { tile_mode_index }
    }
}

/// A stable 64-bit identity for a bound shader (doc-4 §4 "PipelineKey carries a shader
/// *identity*"). For an embedded shader it is the (stage, id) pair; for a GCN binary it
/// is the guest code address — both value-derived so the same bind hashes the same and
/// a different bind re-keys the pipeline (AC #2). FNV-1a over the discriminating bytes.
fn shader_hash(r: ShaderRef) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut h = FNV_OFFSET;
    let mut mix = |b: u64| {
        for byte in b.to_le_bytes() {
            h ^= byte as u64;
            h = h.wrapping_mul(FNV_PRIME);
        }
    };
    match r {
        ShaderRef::Embedded { stage, id } => {
            mix(0); // discriminant: embedded
            mix(stage_tag(stage));
            mix(id as u64);
        }
        ShaderRef::GcnBinary { addr, .. } => {
            mix(1); // discriminant: GCN binary
            mix(addr);
        }
    }
    h
}

/// A stable numeric tag for a [`Stage`], for hashing (avoids relying on enum layout).
fn stage_tag(stage: Stage) -> u64 {
    match stage {
        Stage::Vertex => 0,
        Stage::Pixel => 1,
    }
}

/// Sign-extend a 16-bit scissor coordinate (GFX6 scissor coords are signed).
fn signed16(v: u32) -> i32 {
    (v & 0xFFFF) as i16 as i32
}

/// Look up the display buffer whose base equals `base` through the registered
/// [`DisplayBufferSource`] seam (doc-4 §5). `None` when the seam is unwired (headless)
/// or `base` names no registered framebuffer — the caller then defers the draw.
///
/// [`DisplayBufferSource`]: ps4_core::gpu::DisplayBufferSource
fn lookup_display_buffer(base: u64) -> Option<DisplayBuffer> {
    display_buffers()?.lookup(base)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pm4::opcodes::context_reg as ctx;
    use ps4_core::gpu::{DisplayBufferSource, registered_display_buffers};
    use std::sync::Arc;

    /// A single-framebuffer [`DisplayBufferSource`] for the RT-mapping tests: `lookup`
    /// returns the buffer iff the base matches, standing in for the videoout
    /// registration the real backend wires at boot.
    struct OneBuffer(DisplayBuffer);
    impl DisplayBufferSource for OneBuffer {
        fn lookup(&self, base: u64) -> Option<DisplayBuffer> {
            (self.0.base == base).then_some(self.0)
        }
    }

    /// The 1080p framebuffer the corpus registers, based at a 256-byte-aligned addr.
    const FB_BASE: u64 = 0xC000_0000;
    fn fb() -> DisplayBuffer {
        DisplayBuffer {
            base: FB_BASE,
            width: 1920,
            height: 1080,
        }
    }

    /// RAII: wire `OneBuffer(fb())` as the process-global display-buffer source for the
    /// guard's lifetime (panic-safe, serialized, restored on drop).
    fn with_fb() -> ps4_core::registered::ScopeGuard<'static, dyn DisplayBufferSource> {
        let src: Arc<dyn DisplayBufferSource> = Arc::new(OneBuffer(fb()));
        registered_display_buffers().override_scoped(src)
    }

    /// Build the "default hardware state + RT setup" register stream a
    /// `DrawInitDefaultHardwareState` + color-target setup leaves in the CONTEXT bank:
    /// a COLOR_8_8_8_8 linear target based at `FB_BASE`, a 1920x1080 viewport, and a
    /// full-screen scissor.
    fn default_rt_state() -> GpuState {
        let mut s = GpuState::default();
        // CB_COLOR0_BASE is in 256-byte units → base >> 8.
        s.ctx_regs.set(ctx::CB_COLOR0_BASE, (FB_BASE >> 8) as u32);
        // CB_COLOR0_INFO.FORMAT = COLOR_8_8_8_8 (0x0A) in bits [5:2] → 0x0A << 2.
        s.ctx_regs.set(ctx::CB_COLOR0_INFO, 0x0A << 2);
        // CB_COLOR0_ATTRIB tile-mode index 0 → linear.
        s.ctx_regs.set(ctx::CB_COLOR0_ATTRIB, 0);
        // Viewport 1920x1080 centered: xscale=960, xoffset=960 → x=0, width=1920.
        s.ctx_regs.set(ctx::PA_CL_VPORT_XSCALE, 960.0f32.to_bits());
        s.ctx_regs.set(ctx::PA_CL_VPORT_XOFFSET, 960.0f32.to_bits());
        s.ctx_regs.set(ctx::PA_CL_VPORT_YSCALE, 540.0f32.to_bits());
        s.ctx_regs.set(ctx::PA_CL_VPORT_YOFFSET, 540.0f32.to_bits());
        // Screen scissor [0,0]-[1920,1080].
        s.ctx_regs.set(ctx::PA_SC_SCREEN_SCISSOR_TL, 0);
        s.ctx_regs
            .set(ctx::PA_SC_SCREEN_SCISSOR_BR, 1920 | (1080 << 16));
        s
    }

    // ---- AC #1: RT-setup register stream → expected TargetDesc + viewport ----

    #[test]
    fn default_state_derives_expected_target_and_viewport() {
        let _fb = with_fb();
        let s = default_rt_state();

        let target = derive_target(&s).expect("registered COLOR_8_8_8_8 target derives");
        assert_eq!(
            target,
            TargetDesc {
                width: 1920,
                height: 1080,
                pitch: 1920,
                format: ColorFormat::B8G8R8A8Unorm,
                tiling: Tiling::Linear,
            }
        );

        let vp = derive_viewport(&s);
        assert_eq!(
            vp,
            Viewport {
                x: 0.0,
                y: 0.0,
                width: 1920.0,
                height: 1080.0,
            }
        );

        let sc = derive_scissor(&s);
        assert_eq!(
            sc,
            Scissor {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            }
        );
    }

    #[test]
    fn pitch_derives_from_cb_color0_pitch_when_programmed() {
        let _fb = with_fb();
        let mut s = default_rt_state();
        // TILE_MAX (bits [10:0]) = pitch/8 - 1; program 239 → (239+1)*8 = 1920.
        s.ctx_regs.set(ctx::CB_COLOR0_PITCH, 239);
        assert_eq!(derive_target(&s).unwrap().pitch, 1920);
    }

    #[test]
    fn tiled_target_carries_tile_mode_index() {
        // §C3/§C9: the tiling field is carried even though the first impl forces
        // linear on upload — a nonzero TILE_MODE_INDEX in CB_COLOR0_ATTRIB is retained.
        let _fb = with_fb();
        let mut s = default_rt_state();
        // Hardware layout: TILE_MODE_INDEX is bits [4:0], so index 13 is the literal
        // value 13 in the low bits (written independently of the decode shift). A stray
        // bit in FMASK_TILE_MODE_INDEX [9:5] must not leak into the decoded index.
        s.ctx_regs.set(ctx::CB_COLOR0_ATTRIB, 13 | (0b101 << 5));
        assert_eq!(
            derive_target(&s).unwrap().tiling,
            Tiling::Tiled {
                tile_mode_index: 13
            }
        );
    }

    // ---- Flipped viewport (negative YSCALE) → valid Vulkan signed-height rect ----

    /// A `vk::Viewport`'s `y` is the screen coordinate at NDC −1 and `height` the signed
    /// span to NDC +1; the flip is carried in the height's sign. Both the upright and the
    /// Y-flipped 1080-tall viewport must therefore span the SAME on-screen pixels `[0,1080]`
    /// — only the mapping direction (height sign) differs. Expected values are reasoned from
    /// the GFX6 scale/offset semantics (screen at NDC −1 is `offset − scale`, signed), NOT
    /// from `derive_viewport`'s own arithmetic.
    #[test]
    fn viewport_flip_encoded_in_signed_height() {
        // Upright: YSCALE = +540, YOFFSET = 540. Screen at NDC −1 is 540 − 540 = 0, at
        // NDC +1 is 540 + 540 = 1080 → y = 0, height = +1080, region [0, 1080].
        let mut upright = GpuState::default();
        upright
            .ctx_regs
            .set(ctx::PA_CL_VPORT_YSCALE, 540.0f32.to_bits());
        upright
            .ctx_regs
            .set(ctx::PA_CL_VPORT_YOFFSET, 540.0f32.to_bits());
        let vp_up = derive_viewport(&upright);
        assert_eq!(vp_up.y, 0.0, "upright viewport y at NDC -1");
        assert_eq!(vp_up.height, 1080.0, "upright viewport height (unflipped)");

        // Flipped: YSCALE = -540, YOFFSET = 540. Screen at NDC −1 is 540 − (−540) = 1080,
        // at NDC +1 is 540 + (−540) = 0 → y = 1080, height = −1080. A Vulkan negative-height
        // viewport whose region is still [0, 1080] (y + height = 0), just inverted. A `y = 0`
        // here (the earlier `|yscale|` bug) would place the region at [−1080, 0], entirely
        // above the framebuffer, so nothing would rasterize.
        let mut flipped = GpuState::default();
        flipped
            .ctx_regs
            .set(ctx::PA_CL_VPORT_YSCALE, (-540.0f32).to_bits());
        flipped
            .ctx_regs
            .set(ctx::PA_CL_VPORT_YOFFSET, 540.0f32.to_bits());
        let vp_fl = derive_viewport(&flipped);
        assert_eq!(
            vp_fl.y, 1080.0,
            "flipped viewport y at NDC -1 (bottom edge)"
        );
        assert_eq!(vp_fl.height, -1080.0, "flip encoded as negative height");

        // Both cover the same on-screen span [0, 1080]; only the height sign differs.
        assert_eq!(vp_up.y, 0.0);
        assert_eq!(vp_up.y + vp_up.height, 1080.0);
        assert_eq!(vp_fl.y + vp_fl.height, 0.0);
        assert_eq!(vp_up.height, -vp_fl.height);
    }

    // ---- AC #2: PipelineKey changes iff a key-relevant register changed ----

    fn embedded_bound() -> BoundShaders {
        let mut b = BoundShaders::default();
        b.set(
            Stage::Vertex,
            ShaderRef::Embedded {
                stage: Stage::Vertex,
                id: 0,
            },
        );
        b.set(
            Stage::Pixel,
            ShaderRef::Embedded {
                stage: Stage::Pixel,
                id: 1,
            },
        );
        b
    }

    #[test]
    fn pipeline_key_stable_when_no_key_register_changes() {
        let s = default_rt_state();
        let b = embedded_bound();
        let k1 = derive_pipeline(&s, &b, ColorFormat::B8G8R8A8Unorm);
        // Re-derive from an identical state → identical key (cache-identity).
        let k2 = derive_pipeline(&s, &b, ColorFormat::B8G8R8A8Unorm);
        assert_eq!(k1, k2);

        // A non-key-relevant register (the clear color) changes → key unchanged.
        let mut s2 = s;
        s2.ctx_regs.set(ctx::CB_COLOR0_BASE + 0x100, 0xDEAD_BEEF);
        let k3 = derive_pipeline(&s2, &b, ColorFormat::B8G8R8A8Unorm);
        assert_eq!(k1, k3, "an unrelated register must not re-key the pipeline");
    }

    #[test]
    fn pipeline_key_changes_when_shader_bind_changes() {
        let s = default_rt_state();
        let base = derive_pipeline(&s, &embedded_bound(), ColorFormat::B8G8R8A8Unorm);

        let mut b2 = embedded_bound();
        b2.set(
            Stage::Pixel,
            ShaderRef::Embedded {
                stage: Stage::Pixel,
                id: 2,
            },
        );
        let changed = derive_pipeline(&s, &b2, ColorFormat::B8G8R8A8Unorm);
        assert_ne!(base.ps_hash, changed.ps_hash, "PS rebind must re-key");
        assert_ne!(base, changed);
    }

    #[test]
    fn pipeline_key_changes_on_blend_depth_and_format() {
        let s = default_rt_state();
        let b = embedded_bound();
        let base = derive_pipeline(&s, &b, ColorFormat::B8G8R8A8Unorm);

        // Blend enable bit (CB_BLEND0_CONTROL bit 30) → re-key.
        let mut s_blend = s.clone();
        s_blend.ctx_regs.set(ctx::CB_BLEND0_CONTROL, 1 << 30);
        let k_blend = derive_pipeline(&s_blend, &b, ColorFormat::B8G8R8A8Unorm);
        assert_ne!(base.blend, k_blend.blend);
        assert_ne!(base, k_blend);

        // Depth test (DB_DEPTH_CONTROL bit 1) + a depth surface (DB_Z_INFO fmt) → re-key.
        let mut s_depth = s.clone();
        s_depth.ctx_regs.set(ctx::DB_DEPTH_CONTROL, 1 << 1);
        s_depth.ctx_regs.set(ctx::DB_Z_INFO, 0x1);
        let k_depth = derive_pipeline(&s_depth, &b, ColorFormat::B8G8R8A8Unorm);
        assert!(k_depth.depth.enable);
        assert_ne!(base.depth, k_depth.depth);

        // RT format → re-key.
        let k_fmt = derive_pipeline(&s, &b, ColorFormat::R8G8B8A8Unorm);
        assert_ne!(base.color_format, k_fmt.color_format);
        assert_ne!(base, k_fmt);
    }

    #[test]
    fn depth_absent_without_surface_even_if_test_enabled() {
        // DB_DEPTH_CONTROL.Z_ENABLE set but DB_Z_INFO.FORMAT = 0 (no surface) → not
        // present (HTILE off, §C9; a real surface is required).
        let s = default_rt_state();
        let mut s2 = s;
        s2.ctx_regs.set(ctx::DB_DEPTH_CONTROL, 1 << 1);
        s2.ctx_regs.set(ctx::DB_Z_INFO, 0);
        assert!(
            !derive_pipeline(&s2, &embedded_bound(), ColorFormat::B8G8R8A8Unorm)
                .depth
                .enable
        );
    }

    // ---- AC #3: unknown/unsupported RT format defers cleanly ----

    #[test]
    fn unsupported_format_defers_with_error() {
        let _fb = with_fb();
        let mut s = default_rt_state();
        // A FORMAT the decoder does not map (0x1 is not COLOR_8_8_8_8).
        s.ctx_regs.set(ctx::CB_COLOR0_INFO, 0x1 << 2);
        assert_eq!(
            derive_target(&s),
            Err(TargetError::UnsupportedFormat { info: 0x1 << 2 })
        );
    }

    #[test]
    fn unregistered_base_defers_as_arbitrary_rt() {
        let _fb = with_fb();
        let mut s = default_rt_state();
        // A base that is not the registered framebuffer → arbitrary RT (out of scope).
        let other = 0xD000_0000u64;
        s.ctx_regs.set(ctx::CB_COLOR0_BASE, (other >> 8) as u32);
        assert_eq!(
            derive_target(&s),
            Err(TargetError::UnregisteredTarget { base: other })
        );
    }

    #[test]
    fn no_color_base_is_no_color_base_error() {
        // The embedded fullscreen-quad corpus binds no explicit RT.
        let _fb = with_fb();
        let s = GpuState::default();
        assert_eq!(derive_target(&s), Err(TargetError::NoColorBase));
    }

    #[test]
    fn headless_unwired_seam_defers_registered_target_as_unregistered() {
        // With no display-buffer source wired, even a well-formed base is "unregistered"
        // — the draw defers rather than trusting an unvalidated RT address.
        let _none = registered_display_buffers().override_none_scoped();
        let s = default_rt_state();
        assert_eq!(
            derive_target(&s),
            Err(TargetError::UnregisteredTarget { base: FB_BASE })
        );
    }
}
