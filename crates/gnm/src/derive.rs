//! Register → pipeline-state derivation (doc-2 §5/§C7): read the shadow register file
//! at draw time and snapshot the pipeline-relevant bits into the backend-facing
//! [`TargetDesc`] and [`PipelineKey`]. New state is *decoding more register indices*,
//! never restructuring — this module is that "register → pipeline state" translation.
//!
//! Scope (doc-2 §5, §C3/§C9):
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
//! (doc-2 §4 "must not hardcode"), so phase 4's arbitrary shaders key on it and the
//! backend caches by value.
//!
//! Every register bit layout, format enum, and tile-max encoding this module decodes is
//! the AMD GFX6 (Liverpool / GCN2) hardware definition machine-listed in Mesa
//! `src/amd/registers/gfx6.json` (MIT), with the CB pitch/slice tile-max arithmetic taken
//! from Mesa `src/amd/common/ac_descriptors.c` and the viewport scale/offset transform
//! from Mesa `src/amd/common/ac_guardband.c`. Each fact is pinned to those literals by the
//! `derive_bitfields_match_amd_oracle` test below.

use ps4_core::gpu::{
    BlendKey, ColorFormat, DepthKey, DisplayBuffer, PipelineKey, PrimitiveTopology,
    ResourceSignature, TargetDesc, TargetKind, Tiling, VertexLayout, display_buffers,
};

use crate::pm4::opcodes::context_reg as ctx;
use crate::pm4::opcodes::{di_pt, uconfig};
use crate::shader::source::{ShaderRef, Stage};
use crate::state::{BoundShaders, GpuState};

/// A screen-space viewport derived from `PA_CL_VPORT_*` (doc-2 §5). The GFX6 viewport
/// is programmed as scale/offset (NDC → screen); the pixel rect is
/// `x = xoffset - xscale`, `width = 2 * xscale` (and the same for Y). `f32` bits are
/// read from the register words.
///
/// The scale/offset ↔ screen-rect relation is Mesa `src/amd/common/ac_guardband.c`, which
/// programs `PA_CL_VPORT_?OFFSET = translate = (min + max) / 2` and
/// `PA_CL_VPORT_?SCALE = scale = max - translate = (max - min) / 2`, and inverts the
/// transform as `clip = (screen - translate) / scale` — i.e. `screen = offset + scale*ndc`,
/// so the `ndc = -1` edge is `offset - scale` and the span to `ndc = +1` is `2 * scale`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Viewport {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

/// A screen scissor rect derived from `PA_SC_SCREEN_SCISSOR_TL/BR` (doc-2 §5). Each
/// register packs `x` in bits [15:0] and `y` in bits [31:16] — Mesa
/// `src/amd/registers/gfx6.json` `PA_SC_SCREEN_SCISSOR_TL` (`TL_X` bits [15:0], `TL_Y`
/// bits [31:16]) / `_BR` (`BR_X`, `BR_Y`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Scissor {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

/// Why a draw's color target could not be derived (doc-2 §5). Each variant is a
/// *clean defer*, never a crash — the draw is skipped and logged (AC #3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TargetError {
    /// `CB_COLOR0_BASE` was never programmed — no color target bound.
    NoColorBase,
    /// The color base names no registered display buffer AND its offscreen geometry could
    /// not be derived (no `CB_COLOR0_PITCH`/`SLICE`, or a degenerate/overflowing size). A
    /// well-formed unregistered base is now an offscreen render target (task-56), not an
    /// error — this variant remains only for the degenerate case that still defers.
    UnregisteredTarget { base: u64 },
    /// The `CB_COLOR0_INFO` format field is a value the decoder does not map to a host
    /// format (AC #3): defer rather than guess.
    UnsupportedFormat { info: u32 },
}

/// The pipeline-relevant state a draw derives from the shadow register file (doc-2
/// §5): the backend-facing [`TargetDesc`] + [`PipelineKey`] plus the viewport/scissor
/// the draw sets. Produced by [`derive_draw_state`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DrawState {
    pub target: TargetDesc,
    pub pipeline: PipelineKey,
    pub viewport: Viewport,
    pub scissor: Scissor,
}

/// Derive the color [`TargetDesc`] from the `CB_COLOR0_*` context registers (doc-2
/// §5/§C3/§C9). Maps the target to the videoout framebuffer (kind
/// [`TargetKind::Videoout`]) when `CB_COLOR0_BASE` matches a registered display buffer;
/// otherwise it is an OFFSCREEN render target (kind [`TargetKind::Offscreen`], task-56)
/// whose guest *allocation* comes from `CB_COLOR0_PITCH`/`SLICE` and whose sampled
/// *content* extent comes from the viewport ([`offscreen_content_extent`], task-180). An
/// unrecognized format, or a degenerate offscreen geometry, is a clean [`TargetError`] the
/// caller defers on (AC #3).
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

    let attrib = state.ctx_regs.get(ctx::CB_COLOR0_ATTRIB).unwrap_or(0);
    let tiling = tiling_from_attrib(attrib);

    // A base matching a registered display buffer aliases the videoout framebuffer: its
    // display geometry is authoritative for width/height, and the target renders into the
    // present path (kind = Videoout). Pitch defaults to width when no distinct linear pitch
    // was programmed.
    if let Some(fb) = lookup_display_buffer(base) {
        let pitch = pitch_pixels(state).unwrap_or(fb.width);
        return Ok(TargetDesc {
            width: fb.width,
            height: fb.height,
            pitch,
            format,
            tiling,
            kind: TargetKind::Videoout,
        });
    }

    // An unregistered base is an OFFSCREEN render target (task-56 RT-as-texture): a draw
    // into it must render into a host RT keyed on its guest range, so a later draw sampling
    // that range binds the RT host-side.
    //
    // Two DIFFERENT geometries live here and must not be conflated (task-180):
    //  * the guest ALLOCATION — `pitch` × `rows`, both alignment-padded tile-max encodings.
    //    This is the byte range the RT aliases, so it is what `size` (and therefore the
    //    cache/registry key) is computed from. Celeste's bloom targets are 1024 × 576.
    //  * the CONTENT extent — the sub-rect the guest actually renders into and, crucially,
    //    the extent a later draw's T# normalizes its UVs against. Celeste's bloom targets
    //    are 960 × 540 of that 1024 × 576 allocation.
    // `TargetDesc::width`/`height` are the CONTENT extent (they size the host RT image a
    // consumer samples at UV [0,1]); `pitch` stays the padded row stride. Sizing the image
    // by the pitch instead put up to ~6% of never-written padding inside the sampled [0,1]
    // range, so each sampling hop shrank its source by that much and edged it with black —
    // compounding along a multi-hop chain (Celeste's bloom is two hops: 0.9375² ≈ 0.879).
    //
    // A degenerate geometry (no pitch/slice, or a size that overflows) still defers cleanly.
    let pitch = pitch_pixels(state).ok_or(TargetError::UnregisteredTarget { base })?;
    let rows = offscreen_rows(state, pitch).ok_or(TargetError::UnregisteredTarget { base })?;
    // Byte size of the guest range the RT covers (RGBA8 → 4 bytes/pixel) — the PADDED
    // allocation, never the content extent: the aliasing key must name the whole surface a
    // later sampled T# can land in. Saturating so a hostile geometry can't wrap the range.
    let size = (pitch as u64).saturating_mul(rows as u64).saturating_mul(4);
    if size == 0 {
        return Err(TargetError::UnregisteredTarget { base });
    }
    let (width, height) = offscreen_content_extent(state, pitch, rows);
    Ok(TargetDesc {
        width,
        height,
        pitch,
        format,
        tiling,
        kind: TargetKind::Offscreen { base, size },
    })
}

/// Derive the full [`DrawState`] (target + pipeline + viewport + scissor) from the
/// shadow register file and the bound shaders (doc-2 §5). Returns the target-derivation
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

/// Snapshot the pipeline-relevant register bits into a [`PipelineKey`] (doc-2 §4/§5).
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
        // The bound-resource signature needs the RESOLVED shader IoLayout (the declared
        // descriptors' set/binding), which lives one layer up in the draw setup — this
        // register-only derivation cannot see it. Left default here; `setup_draw`
        // populates `key.resources` from the resolved VS/PS bindings before get-or-mint
        // (task-130 slice 6).
        resources: ResourceSignature::default(),
        color_format,
        blend: derive_blend(state),
        depth: derive_depth(state),
        topology: derive_topology(state),
    }
}

/// Input-assembly topology from `VGT_PRIMITIVE_TYPE` (task-184).
///
/// Only the two types the draw path can honour are distinguished. `DI_PT_RECTLIST`
/// becomes a triangle STRIP, because a rect list's three vertices name a parallelogram
/// the hardware completes with a synthesized fourth corner — a strip over four vertices
/// tiles the same area (see [`PrimitiveTopology::TriangleStrip`] for the approximation
/// that entails). Every other type, including an unwritten register, keeps the historical
/// triangle-list behaviour rather than deferring: this layer has never modelled the
/// primitive type at all, and silently dropping draws that used to render would be a
/// regression, not a fix.
pub fn derive_topology(state: &GpuState) -> PrimitiveTopology {
    match state.uconfig_regs.get(uconfig::VGT_PRIMITIVE_TYPE) {
        Some(di_pt::RECTLIST) => PrimitiveTopology::TriangleStrip,
        _ => PrimitiveTopology::TriangleList,
    }
}

/// The vertex-input layout the pipeline is built against (doc-2 §4/§C4). The embedded
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

/// Blend bits from `CB_BLEND0_CONTROL` + `CB_COLOR_CONTROL` (doc-2 §5). `enable` is the
/// MRT0 blend-enable bit; `control` is the raw `CB_BLEND0_CONTROL` word (carried
/// verbatim so any factor/equation change re-keys the pipeline). MRT>1 is out of scope.
fn derive_blend(state: &GpuState) -> BlendKey {
    let control = state.ctx_regs.get(ctx::CB_BLEND0_CONTROL).unwrap_or(0);
    // GFX6 CB_BLEND0_CONTROL.ENABLE is bit 30 (Mesa `src/amd/registers/gfx6.json`
    // `CB_BLEND0_CONTROL` field `ENABLE` bits [30:30]).
    let enable = control & (1 << 30) != 0;
    // MRT0's colour write mask (`CB_TARGET_MASK.TARGET0_ENABLE`, bits [3:0] = R,G,B,A; Mesa
    // `src/amd/registers/gfx6.json` `CB_TARGET_MASK` field `TARGET0_ENABLE` bits [3:0]).
    // Defaults to all-enabled when the guest never programmed the register — the safe
    // direction: an unset register must not silently mask every channel off.
    let write_mask = (state
        .ctx_regs
        .get(ctx::CB_TARGET_MASK)
        .unwrap_or(0xFFFF_FFFF)
        & 0xF) as u8;
    BlendKey {
        enable,
        control,
        write_mask,
    }
}

/// Depth bits from `DB_DEPTH_CONTROL` + `DB_Z_INFO` (doc-2 §5). Depth is present when
/// depth testing is enabled *and* a depth surface format is programmed. HTILE is off
/// (§C9), so no compression metadata is carried.
fn derive_depth(state: &GpuState) -> DepthKey {
    let control = state.ctx_regs.get(ctx::DB_DEPTH_CONTROL).unwrap_or(0);
    let z_info = state.ctx_regs.get(ctx::DB_Z_INFO).unwrap_or(0);
    // GFX6 DB_DEPTH_CONTROL.Z_ENABLE is bit 1; DB_Z_INFO.FORMAT (bits [1:0]) != 0 means
    // a real depth surface (FORMAT 0 = invalid/no surface). Bit positions from Mesa
    // `src/amd/registers/gfx6.json`: `DB_DEPTH_CONTROL` field `Z_ENABLE` bits [1:1],
    // `DB_Z_INFO` field `FORMAT` bits [1:0] (enum `ZFormat`, `Z_INVALID = 0`).
    let z_enable = control & (1 << 1) != 0;
    let has_surface = z_info & 0x3 != 0;
    DepthKey {
        enable: z_enable && has_surface,
        control,
    }
}

/// Viewport-0 pixel rect from the `PA_CL_VPORT_*` scale/offset registers (doc-2 §5).
/// Registers hold `f32` bit patterns. The GFX6 viewport transform maps NDC → screen as
/// `screen = offset + scale * ndc` with `ndc ∈ [-1, +1]`, so the two edges sit at
/// `offset - scale` (at `ndc = -1`) and `offset + scale` (at `ndc = +1`). This is the
/// transform Mesa `src/amd/common/ac_guardband.c` programs (`translate = (min+max)/2` into
/// `?OFFSET`, `scale = (max-min)/2` into `?SCALE`) and inverts (`clip = (screen-translate)/scale`).
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

/// Screen scissor from `PA_SC_SCREEN_SCISSOR_TL/BR` (doc-2 §5): each register packs
/// `x` in bits [15:0] and `y` in bits [31:16] (Mesa `src/amd/registers/gfx6.json`
/// `PA_SC_SCREEN_SCISSOR_TL`/`_BR`, `?_X` bits [15:0] / `?_Y` bits [31:16]). Width/height
/// are `BR - TL` (clamped at zero so a `BR < TL` malformed pair is an empty scissor, never
/// a wrapping size).
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
/// Field position from Mesa `src/amd/registers/gfx6.json` `CB_COLOR0_PITCH` field
/// `TILE_MAX` bits [10:0]; the `pitch / 8 - 1` encoding is Mesa
/// `src/amd/common/ac_descriptors.c` (`pitch_tile_max = nblk_x / 8 - 1`).
fn pitch_pixels(state: &GpuState) -> Option<u32> {
    let pitch = state.ctx_regs.get(ctx::CB_COLOR0_PITCH)?;
    Some(((pitch & 0x7FF) + 1) * 8)
}

/// Row count of an offscreen render target's guest ALLOCATION (task-56). GFX6 encodes the
/// surface size as `CB_COLOR0_SLICE.TILE_MAX = (pitch * rows / 64) − 1` (bits [21:0]); with
/// the pixel `pitch` already decoded, `rows = (SLICE_TILE_MAX + 1) * 64 / pitch`. `None` if
/// SLICE was never written or the arithmetic is degenerate (zero pitch/rows), so the draw
/// defers cleanly rather than deriving a bogus extent. Field position from Mesa
/// `src/amd/registers/gfx6.json` `CB_COLOR0_SLICE` field `TILE_MAX` bits [21:0]; the
/// `(nblk_x * nblk_y) / 64 - 1` encoding is Mesa `src/amd/common/ac_descriptors.c`
/// (`slice_tile_max = (nblk_x * nblk_y) / 64 - 1`).
///
/// Like `pitch`, this is the *padded* figure: Celeste's 540-row bloom targets allocate 576
/// rows. It sizes the guest byte range, NOT the sampled extent — see
/// [`offscreen_content_extent`] (task-180).
fn offscreen_rows(state: &GpuState, pitch: u32) -> Option<u32> {
    if pitch == 0 {
        return None;
    }
    let slice = state.ctx_regs.get(ctx::CB_COLOR0_SLICE)?;
    let slice_tile_max = (slice & 0x3F_FFFF) as u64;
    let total_pixels = (slice_tile_max + 1) * 64;
    let rows = total_pixels / pitch as u64;
    if rows == 0 || rows > u32::MAX as u64 {
        return None;
    }
    Some(rows as u32)
}

/// The CONTENT extent of an offscreen render target — the sub-rect of the `pitch × rows`
/// allocation the guest actually renders into, and therefore the extent a later draw's T#
/// normalizes its sampled UVs against (task-180).
///
/// **Why the viewport.** The `CB_COLOR0_*` block carries no such figure: `PITCH` and `SLICE`
/// are alignment-padded tile-max encodings of the *allocation* (Celeste's bloom targets:
/// 1024 × 576 holding 960 × 540 of content), and no de-alignment can recover 960 from 1024.
/// The viewport is the register-file's only statement of where rasterized pixels land, and
/// it is exactly what the consumer agrees with — measured on Celeste's menu, every producer
/// draw into a bloom target programs `PA_CL_VPORT_*` as a 960 × 540 rect, and the consumer's
/// blur constants are a `1/960` / `1/540` texel step, i.e. UV 1.0 == the last CONTENT texel.
/// Sizing the host RT image by the padded pitch instead left ~6% never-written padding
/// inside the sampled `[0,1]` range on each hop.
///
/// **Fallback and clamp.** An unwritten/degenerate viewport (`< 1` pixel, non-finite) yields
/// the full allocation, so a target programmed with no viewport behaves exactly as before.
/// Each axis is clamped to the allocation: a viewport larger than the surface must never
/// size an image past the range the aliasing key covers.
///
/// A Y-flipped viewport carries its flip in the *sign* of `height` (see [`derive_viewport`]),
/// so the extent is the magnitude.
///
/// KNOWN LIMITATION (task-180): one content extent per target, taken from the viewport of
/// whichever draw derived the target. A title that renders sub-rects of one RT under several
/// smaller viewports would re-key the RT per viewport (the extent is part of the resource
/// key). Celeste programs a single stable viewport per RT base; a title that does not needs
/// a per-base extent high-water mark, which is deliberately not built speculatively.
fn offscreen_content_extent(state: &GpuState, pitch: u32, rows: u32) -> (u32, u32) {
    let viewport = derive_viewport(state);
    let span = |v: f32, allocated: u32| -> u32 {
        let v = v.abs();
        if !v.is_finite() || v < 1.0 {
            return allocated;
        }
        // `as u32` on a float saturates in Rust, so an absurd viewport lands at u32::MAX and
        // the clamp brings it back to the allocation rather than wrapping.
        (v.round() as u32).clamp(1, allocated)
    };
    (span(viewport.width, pitch), span(viewport.height, rows))
}

/// Map `CB_COLOR0_INFO` (FORMAT bits [6:2] + COMP_SWAP bits [12:11]) to a host
/// [`ColorFormat`], or `None` for a value the decoder does not support this phase (the
/// draw then defers, AC #3). Only the formats the corpus needs are mapped; the set grows
/// as draws need each one.
///
/// The channel order is NOT implied by the FORMAT enum — `COLOR_8_8_8_8` is just "four
/// 8-bit channels". `CB_COLOR0_INFO.COMP_SWAP` [12:11] selects which physical channel each
/// exported component lands in (task-154 residual #2). Celeste programs `COMP_SWAP = ALT`
/// (its videoout framebuffer is BGRA), which the previous hardcoded BGRA return happened to
/// match; but a title programming STD wants RGBA, so we now decode it rather than assume:
///   - STD (0)    → RGBA (component 0 → R)
///   - ALT (1)    → BGRA (component 0 → B) — the videoout framebuffer order
///   - STD_RV (2) → ABGR, ALT_RV (3) → ARGB (alpha-first reversed): not modeled by
///     [`ColorFormat`] yet, so they map to the nearest base order (RGBA for STD_RV,
///     BGRA for ALT_RV) until a title needs the exact reversed layout.
fn color_format(info: u32) -> Option<ColorFormat> {
    // GFX6 CB_COLOR0_INFO.FORMAT is the 5-bit `ColorFormat` enum in bits [6:2] (Mesa
    // `src/amd/registers/gfx6.json` `CB_COLOR0_INFO` field `FORMAT` bits [2:6], enum
    // `ColorFormat`). `COLOR_8_8_8_8` = 10 (0x0A) is the videoout framebuffer format. We
    // read only the low 4 bits of the field: within the enum's 0..=23 range no value other
    // than 0x0A masks to 0x0A, so a 4-bit compare uniquely identifies COLOR_8_8_8_8 and any
    // other (unsupported) format still fails the compare and defers.
    const COLOR_8_8_8_8: u32 = 0x0A;
    if (info >> 2) & 0xF != COLOR_8_8_8_8 {
        return None;
    }
    // COMP_SWAP (bits [12:11]) — Mesa `src/amd/registers/gfx6.json` `CB_COLOR0_INFO` field
    // `COMP_SWAP` bits [11:12], enum `SurfaceSwap`: 0=STD, 1=ALT, 2=STD_RV, 3=ALT_RV.
    let comp_swap = (info >> 11) & 0x3;
    let order = match comp_swap {
        0 | 2 => ColorFormat::R8G8B8A8Unorm, // STD / STD_RV → RGBA base order
        1 | 3 => ColorFormat::B8G8R8A8Unorm, // ALT / ALT_RV → BGRA (videoout) order
        _ => unreachable!("2-bit field"),
    };
    Some(order)
}

/// Tiling from `CB_COLOR0_ATTRIB` (doc-2 §C3/§C9). GFX6/7 packs `TILE_MODE_INDEX` in bits
/// [4:0] (FMASK_TILE_MODE_INDEX sits above it at [9:5]); index 0 is the linear array mode.
/// Field positions from Mesa `src/amd/registers/gfx6.json` `CB_COLOR0_ATTRIB`
/// (`TILE_MODE_INDEX` bits [4:0], `FMASK_TILE_MODE_INDEX` bits [9:5]). The mode is *carried*
/// even while the first implementation forces surfaces linear (§C9) — the deferred detile
/// step keys on it.
fn tiling_from_attrib(attrib: u32) -> Tiling {
    let tile_mode_index = attrib & 0x1F;
    if tile_mode_index == 0 {
        Tiling::Linear
    } else {
        Tiling::Tiled { tile_mode_index }
    }
}

/// A stable 64-bit identity for a bound shader (doc-2 §4 "PipelineKey carries a shader
/// *identity*"). For an embedded shader it is the (stage, id) pair; for a GCN binary it
/// is the guest code address PLUS the PS input routing (`SPI_PS_INPUT_CNTL`) — the same
/// PS binary under a different routing recompiles to a different SPIR-V module, so two
/// such draws must not share a pipeline. Both value-derived so the same bind hashes the
/// same and a different bind re-keys the pipeline (AC #2). FNV-1a over the discriminating
/// bytes.
pub(crate) fn shader_hash(r: ShaderRef) -> u64 {
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
        ShaderRef::GcnBinary {
            addr, ps_input_map, ..
        } => {
            mix(1); // discriminant: GCN binary
            mix(addr);
            for n in 0..ps4_gcn::PS_INPUT_SLOTS {
                mix(ps_input_map.location_for(n as u8) as u64);
            }
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
/// [`DisplayBufferSource`] seam (doc-2 §5). `None` when the seam is unwired (headless)
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
        // CB_COLOR0_INFO.FORMAT = COLOR_8_8_8_8 (0x0A) in bits [5:2] → 0x0A << 2, plus
        // COMP_SWAP = ALT (1) in bits [12:11] → 1 << 11: the videoout framebuffer's BGRA
        // channel order (task-154 residual #2), which the corpus's videoout target uses.
        s.ctx_regs.set(ctx::CB_COLOR0_INFO, (0x0A << 2) | (1 << 11));
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
    fn color_format_honors_comp_swap() {
        // task-154 residual #2: CB_COLOR0_INFO.COMP_SWAP [12:11] selects channel order,
        // NOT the FORMAT enum. COLOR_8_8_8_8 = 0x0A in bits [5:2].
        let fmt = 0x0A << 2;
        // STD (0) → RGBA (COMP_SWAP bits clear).
        assert_eq!(color_format(fmt), Some(ColorFormat::R8G8B8A8Unorm));
        // ALT (1) → BGRA (the videoout framebuffer order — Celeste's case).
        assert_eq!(
            color_format(fmt | (1 << 11)),
            Some(ColorFormat::B8G8R8A8Unorm)
        );
        // STD_RV (2) → nearest base RGBA; ALT_RV (3) → nearest base BGRA.
        assert_eq!(
            color_format(fmt | (2 << 11)),
            Some(ColorFormat::R8G8B8A8Unorm)
        );
        assert_eq!(
            color_format(fmt | (3 << 11)),
            Some(ColorFormat::B8G8R8A8Unorm)
        );
        // An unsupported FORMAT still defers regardless of COMP_SWAP.
        assert_eq!(color_format((0x1 << 2) | (1 << 11)), None);
    }

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
                kind: TargetKind::Videoout,
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

    /// Two draws sharing a PS code address but programming a different
    /// `SPI_PS_INPUT_CNTL` routing recompile to different SPIR-V, so they must not share a
    /// pipeline: the routing is part of the shader identity, not just of its inputs.
    #[test]
    fn pipeline_key_changes_when_ps_input_routing_changes() {
        use ps4_gcn::{PS_INPUT_SLOTS, PsInputMap};

        let s = default_rt_state();
        let gcn_ps = |ps_input_map| {
            let mut b = embedded_bound();
            b.set(
                Stage::Pixel,
                ShaderRef::GcnBinary {
                    addr: 0x0020_0000,
                    res: crate::shader::source::GcnResources::default(),
                    ps_input_map,
                },
            );
            b
        };

        let mut offsets = [0u8; PS_INPUT_SLOTS];
        for (n, o) in offsets.iter_mut().enumerate() {
            *o = n as u8;
        }
        let identity = derive_pipeline(
            &s,
            &gcn_ps(PsInputMap::from_offsets(offsets)),
            ColorFormat::B8G8R8A8Unorm,
        );
        offsets[0] = 1;
        let routed = derive_pipeline(
            &s,
            &gcn_ps(PsInputMap::from_offsets(offsets)),
            ColorFormat::B8G8R8A8Unorm,
        );

        assert_ne!(
            identity.ps_hash, routed.ps_hash,
            "same PS address, different SPI_PS_INPUT_CNTL routing must re-key"
        );
        assert_ne!(identity, routed);
    }

    /// `CB_TARGET_MASK.TARGET0_ENABLE` (bits [3:0]) reaches `BlendKey::write_mask`, and an
    /// unprogrammed register defaults to all-channels-enabled (the hardware reset value) —
    /// defaulting to 0 would silently mask every channel off and render nothing.
    #[test]
    fn blend_key_carries_target_write_mask() {
        let s = default_rt_state();
        let b = embedded_bound();

        // Unset register → all four channels enabled.
        let base = derive_pipeline(&s, &b, ColorFormat::B8G8R8A8Unorm);
        assert_eq!(base.blend.write_mask, 0xF);

        // Alpha masked off (RGB only). MRT1+ enables (bits [7:4]) must not leak into MRT0.
        let mut s_rgb = s.clone();
        s_rgb.ctx_regs.set(ctx::CB_TARGET_MASK, 0xF7);
        let k_rgb = derive_pipeline(&s_rgb, &b, ColorFormat::B8G8R8A8Unorm);
        assert_eq!(k_rgb.blend.write_mask, 0x7);
        assert_ne!(base.blend, k_rgb.blend, "a write-mask change must re-key");
    }

    /// `VGT_PRIMITIVE_TYPE = DI_PT_RECTLIST` builds a triangle-STRIP pipeline and re-keys
    /// (task-184). A rect list's three vertices name a parallelogram the hardware completes
    /// with a synthesized fourth corner; rasterizing them as a triangle list covers only
    /// half the target, which is how Celeste's bloom-target clears silently did nothing.
    /// Every other value — including an unwritten register — keeps triangle list, so
    /// modelling the primitive type cannot regress a title that never sets it.
    #[test]
    fn rectlist_primitive_type_selects_triangle_strip_and_rekeys() {
        use crate::pm4::opcodes::{di_pt, uconfig};

        let s = default_rt_state();
        let b = embedded_bound();

        // Unwritten register → the historical triangle list.
        let base = derive_pipeline(&s, &b, ColorFormat::B8G8R8A8Unorm);
        assert_eq!(base.topology, PrimitiveTopology::TriangleList);

        let mut s_tri = s.clone();
        s_tri
            .uconfig_regs
            .set(uconfig::VGT_PRIMITIVE_TYPE, di_pt::TRILIST);
        assert_eq!(
            derive_pipeline(&s_tri, &b, ColorFormat::B8G8R8A8Unorm).topology,
            PrimitiveTopology::TriangleList
        );

        let mut s_rect = s.clone();
        s_rect
            .uconfig_regs
            .set(uconfig::VGT_PRIMITIVE_TYPE, di_pt::RECTLIST);
        let k_rect = derive_pipeline(&s_rect, &b, ColorFormat::B8G8R8A8Unorm);
        assert_eq!(k_rect.topology, PrimitiveTopology::TriangleStrip);
        assert_ne!(base, k_rect, "a primitive-type change must re-key");
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
    fn unregistered_base_with_geometry_is_offscreen_rt() {
        // task-56: a well-formed base that is NOT the registered framebuffer, with a
        // programmed PITCH+SLICE, derives an OFFSCREEN render target (no longer a defer).
        // Geometry is reasoned from the tile-max encodings, not from derive_target.
        let _fb = with_fb();
        let mut s = default_rt_state();
        let other = 0xD000_0000u64;
        s.ctx_regs.set(ctx::CB_COLOR0_BASE, (other >> 8) as u32);
        // PITCH tile-max = pitch/8 − 1: (31+1)*8 = 256.
        s.ctx_regs.set(ctx::CB_COLOR0_PITCH, 31);
        // SLICE tile-max = pitch*height/64 − 1 = 256*128/64 − 1 = 511 → height 128.
        s.ctx_regs.set(ctx::CB_COLOR0_SLICE, 511);
        let target = derive_target(&s).expect("offscreen RT derives");
        // No viewport programmed → the content extent falls back to the full allocation.
        assert_eq!(
            target.width, 256,
            "width == pitch with no viewport programmed"
        );
        assert_eq!(target.height, 128);
        assert_eq!(target.pitch, 256);
        assert_eq!(
            target.kind,
            TargetKind::Offscreen {
                base: other,
                // 256 * 128 * 4 bytes (RGBA8).
                size: 256 * 128 * 4,
            }
        );
    }

    /// Program a Y-flipped `w × h` viewport at the origin, the shape Celeste's producers use
    /// (`PA_CL_VPORT_YSCALE = -h/2`, `YOFFSET = h/2` → the rect `[0, h]` with the flip in the
    /// sign of the derived height).
    fn set_flipped_viewport(s: &mut GpuState, w: f32, h: f32) {
        s.ctx_regs.set(ctx::PA_CL_VPORT_XSCALE, (w / 2.0).to_bits());
        s.ctx_regs
            .set(ctx::PA_CL_VPORT_XOFFSET, (w / 2.0).to_bits());
        s.ctx_regs
            .set(ctx::PA_CL_VPORT_YSCALE, (-h / 2.0).to_bits());
        s.ctx_regs
            .set(ctx::PA_CL_VPORT_YOFFSET, (h / 2.0).to_bits());
    }

    /// An offscreen RT state at `base` with a `pitch × rows` guest allocation.
    fn padded_offscreen_state(base: u64, pitch: u32, rows: u32) -> GpuState {
        let mut s = default_rt_state();
        s.ctx_regs.set(ctx::CB_COLOR0_BASE, (base >> 8) as u32);
        s.ctx_regs.set(ctx::CB_COLOR0_PITCH, pitch / 8 - 1);
        s.ctx_regs
            .set(ctx::CB_COLOR0_SLICE, (pitch * rows) / 64 - 1);
        s
    }

    #[test]
    fn padded_offscreen_rt_extent_is_the_content_not_the_pitch() {
        // task-180 AC #1/#2/#3, the measured Celeste bloom target: a 960 × 540 content rect
        // in a 1024 × 576 alignment-padded allocation. The host RT image must be sized to the
        // CONTENT, so a consumer sampling at UV 1.0 reads the last content texel rather than
        // ~6% of never-written padding on each axis; `pitch` stays the padded row stride and
        // `size` stays the padded allocation (the aliasing key must cover the whole surface).
        let _fb = with_fb();
        let base = 0xD000_0000u64;
        let mut s = padded_offscreen_state(base, 1024, 576);
        set_flipped_viewport(&mut s, 960.0, 540.0);

        let target = derive_target(&s).expect("offscreen RT derives");
        assert_eq!(
            (target.width, target.height),
            (960, 540),
            "the sampled extent is the viewport's content rect, not the padded pitch/rows"
        );
        assert_eq!(target.pitch, 1024, "pitch stays the padded row stride");
        assert_eq!(
            target.kind,
            TargetKind::Offscreen {
                base,
                // The guest range still spans the FULL padded allocation: 1024 * 576 * 4.
                size: 1024 * 576 * 4,
            },
            "the aliasing range covers the padded allocation, not the content rect"
        );
    }

    #[test]
    fn padded_offscreen_rt_padded_on_one_axis_only() {
        // Celeste's scene target: 1920 pitch is exact, but 1080 rows of content are allocated
        // as 1088. The axes are derived independently — a uniform scale would be wrong.
        let _fb = with_fb();
        let base = 0xD100_0000u64;
        let mut s = padded_offscreen_state(base, 1920, 1088);
        set_flipped_viewport(&mut s, 1920.0, 1080.0);

        let target = derive_target(&s).expect("offscreen RT derives");
        assert_eq!((target.width, target.height), (1920, 1080));
        assert_eq!(target.pitch, 1920);
        assert_eq!(
            target.kind,
            TargetKind::Offscreen {
                base,
                size: 1920 * 1088 * 4,
            }
        );
    }

    #[test]
    fn offscreen_content_extent_clamps_to_the_allocation() {
        // A viewport LARGER than the surface must never size the host image past the guest
        // range the aliasing key covers — clamp per axis instead.
        let _fb = with_fb();
        let base = 0xD200_0000u64;
        let mut s = padded_offscreen_state(base, 256, 128);
        set_flipped_viewport(&mut s, 4096.0, 4096.0);

        let target = derive_target(&s).expect("offscreen RT derives");
        assert_eq!((target.width, target.height), (256, 128));
    }

    #[test]
    fn offscreen_degenerate_viewport_falls_back_to_the_allocation() {
        // A sub-pixel / never-programmed viewport carries no usable content extent, so the
        // target keeps the full allocation — the pre-task-180 behavior, unchanged.
        let _fb = with_fb();
        let base = 0xD300_0000u64;
        let mut s = padded_offscreen_state(base, 256, 128);
        set_flipped_viewport(&mut s, 0.0, 0.0);

        let target = derive_target(&s).expect("offscreen RT derives");
        assert_eq!((target.width, target.height), (256, 128));
    }

    #[test]
    fn unregistered_base_without_geometry_defers() {
        // An unregistered base with no PITCH/SLICE programmed still defers cleanly (a
        // degenerate offscreen geometry is not usable).
        let _fb = with_fb();
        let mut s = default_rt_state();
        let other = 0xD000_0000u64;
        s.ctx_regs.set(ctx::CB_COLOR0_BASE, (other >> 8) as u32);
        // Clear PITCH so no offscreen geometry can be derived (default_rt_state sets none,
        // but be explicit): with no PITCH the offscreen path defers.
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
    fn headless_unwired_seam_treats_base_as_offscreen_without_geometry_defers() {
        // With no display-buffer source wired, a base is never "videoout"; it is an
        // offscreen RT candidate. default_rt_state programs no PITCH/SLICE, so the offscreen
        // geometry is degenerate and the draw defers (UnregisteredTarget). A base WITH
        // geometry would derive an offscreen RT even unwired — see the offscreen test above.
        let _none = registered_display_buffers().override_none_scoped();
        let s = default_rt_state();
        assert_eq!(
            derive_target(&s),
            Err(TargetError::UnregisteredTarget { base: FB_BASE })
        );
    }

    #[test]
    fn offscreen_rt_derives_even_with_seam_unwired() {
        // task-56: an offscreen RT does not depend on the display-buffer seam — a base with
        // PITCH+SLICE derives an Offscreen TargetDesc whether or not a videoout source is
        // wired (the videoout mapping only applies when the base MATCHES a registered fb).
        let _none = registered_display_buffers().override_none_scoped();
        let mut s = default_rt_state();
        s.ctx_regs.set(ctx::CB_COLOR0_PITCH, 31); // pitch 256
        s.ctx_regs.set(ctx::CB_COLOR0_SLICE, 511); // height 128
        let target = derive_target(&s).expect("offscreen RT derives unwired");
        assert!(matches!(target.kind, TargetKind::Offscreen { .. }));
        assert_eq!((target.width, target.height), (256, 128));
    }

    /// Pins every register bit layout, format enum, and tile-max encoding this module
    /// decodes to its AMD GFX6 hardware value. The right-hand literals are the field bit
    /// ranges and enum values machine-listed in Mesa `src/amd/registers/gfx6.json`, plus the
    /// tile-max arithmetic in Mesa `src/amd/common/ac_descriptors.c` and the viewport
    /// scale/offset transform in Mesa `src/amd/common/ac_guardband.c`. Each case drives the
    /// oracle field to a known value and checks our decoder reads exactly that field.
    #[test]
    // The `0 <<` shifts spell out zero-valued bitfields (COMP_SWAP=STD, tile-mode index)
    // beside their `1 <<` / non-zero siblings so the bit positions stay legible against
    // the cited AMD oracle; keep them literal.
    #[allow(clippy::identity_op)]
    fn derive_bitfields_match_amd_oracle() {
        // --- CB_COLOR0_INFO.FORMAT bits [6:2], ColorFormat COLOR_8_8_8_8 = 10 (0x0A) ---
        // COLOR_8_8_8_8 at bit 2 with COMP_SWAP=STD → RGBA; neighbouring ENDIAN [1:0] and
        // NUMBER_TYPE [10:8] must not leak into the decode.
        assert_eq!(
            color_format(
                (0x0A << 2) | 0b11 /*ENDIAN*/ | (0x5 << 8) /*NUMBER_TYPE*/
            ),
            Some(ColorFormat::R8G8B8A8Unorm),
            "FORMAT is bits [6:2] with COMP_SWAP=STD → RGBA; neighbouring fields must not leak"
        );
        // A format with bit 6 set (e.g. COLOR_5_6_5 = 16 = 0x10) is NOT COLOR_8_8_8_8.
        assert_eq!(
            color_format(0x10 << 2),
            None,
            "COLOR_5_6_5 (0x10) is unsupported"
        );

        // --- CB_COLOR0_INFO.COMP_SWAP bits [11:12], SurfaceSwap 0..3 ---
        let fmt = 0x0A << 2;
        assert_eq!(
            color_format(fmt | (0 << 11)),
            Some(ColorFormat::R8G8B8A8Unorm)
        ); // STD
        assert_eq!(
            color_format(fmt | (1 << 11)),
            Some(ColorFormat::B8G8R8A8Unorm)
        ); // ALT

        // --- CB_COLOR0_ATTRIB.TILE_MODE_INDEX bits [4:0]; FMASK_TILE_MODE_INDEX [9:5] ---
        assert_eq!(tiling_from_attrib(0), Tiling::Linear, "index 0 = linear");
        assert_eq!(
            tiling_from_attrib(13 | (0b101 << 5)),
            Tiling::Tiled {
                tile_mode_index: 13
            },
            "TILE_MODE_INDEX is bits [4:0]; FMASK_TILE_MODE_INDEX [9:5] must not leak"
        );

        // --- CB_COLOR0_PITCH.TILE_MAX bits [10:0] = pitch/8 - 1 (ac_descriptors.c) ---
        {
            let mut s = GpuState::default();
            // pitch 1920 → TILE_MAX = 1920/8 - 1 = 239, in low 11 bits. A stray high bit
            // (FMASK_TILE_MAX [30:20]) must not change the pitch.
            s.ctx_regs.set(ctx::CB_COLOR0_PITCH, 239 | (0x7FF << 20));
            assert_eq!(pitch_pixels(&s), Some(1920));
        }

        // --- CB_COLOR0_SLICE.TILE_MAX bits [21:0] = pitch*rows/64 - 1 (ac_descriptors.c) ---
        {
            let mut s = GpuState::default();
            // pitch 256, rows 128 → SLICE_TILE_MAX = 256*128/64 - 1 = 511.
            s.ctx_regs.set(ctx::CB_COLOR0_SLICE, 511);
            assert_eq!(offscreen_rows(&s, 256), Some(128));
        }

        // --- CB_BLEND0_CONTROL.ENABLE bit 30 ---
        {
            let mut s = GpuState::default();
            s.ctx_regs.set(ctx::CB_BLEND0_CONTROL, 1 << 30);
            assert!(derive_blend(&s).enable, "ENABLE is bit 30");
            // A neighbouring SEPARATE_ALPHA_BLEND [29:29] must not read as enable.
            let mut s2 = GpuState::default();
            s2.ctx_regs.set(ctx::CB_BLEND0_CONTROL, 1 << 29);
            assert!(!derive_blend(&s2).enable);
        }

        // --- CB_TARGET_MASK.TARGET0_ENABLE bits [3:0] ---
        {
            let mut s = GpuState::default();
            // TARGET0 = 0x7 (RGB), TARGET1 [7:4] = 0xF must not leak into MRT0.
            s.ctx_regs.set(ctx::CB_TARGET_MASK, 0xF7);
            assert_eq!(derive_blend(&s).write_mask, 0x7);
        }

        // --- DB_DEPTH_CONTROL.Z_ENABLE bit 1; DB_Z_INFO.FORMAT bits [1:0] (Z_INVALID=0) ---
        {
            let mut s = GpuState::default();
            s.ctx_regs.set(ctx::DB_DEPTH_CONTROL, 1 << 1); // Z_ENABLE
            s.ctx_regs.set(ctx::DB_Z_INFO, 0x2); // FORMAT = Z_24 (non-invalid)
            assert!(derive_depth(&s).enable);
            // FORMAT 0 (Z_INVALID) → no surface even with Z_ENABLE set.
            let mut s2 = GpuState::default();
            s2.ctx_regs.set(ctx::DB_DEPTH_CONTROL, 1 << 1);
            s2.ctx_regs.set(ctx::DB_Z_INFO, 0);
            assert!(!derive_depth(&s2).enable);
        }

        // --- PA_SC_SCREEN_SCISSOR: X bits [15:0], Y bits [31:16] ---
        {
            let mut s = GpuState::default();
            s.ctx_regs.set(ctx::PA_SC_SCREEN_SCISSOR_TL, 0);
            s.ctx_regs
                .set(ctx::PA_SC_SCREEN_SCISSOR_BR, 1920 | (1080 << 16));
            let sc = derive_scissor(&s);
            assert_eq!((sc.x, sc.y, sc.width, sc.height), (0, 0, 1920, 1080));
        }

        // --- PA_CL_VPORT scale/offset: screen = offset + scale*ndc (ac_guardband.c) ---
        {
            let mut s = GpuState::default();
            // translate=(min+max)/2=960, scale=(max-min)/2=960 for [0,1920].
            s.ctx_regs.set(ctx::PA_CL_VPORT_XOFFSET, 960.0f32.to_bits());
            s.ctx_regs.set(ctx::PA_CL_VPORT_XSCALE, 960.0f32.to_bits());
            let vp = derive_viewport(&s);
            assert_eq!(vp.x, 0.0, "x = offset - scale (screen at ndc -1)");
            assert_eq!(vp.width, 1920.0, "width = 2*scale (span to ndc +1)");
        }
    }
}
