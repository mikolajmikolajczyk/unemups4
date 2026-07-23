// GCN textured-quad homebrew — a real register-route Gnm draw over corpus .sb
// shaders that SAMPLES A TEXTURE.
//
// This extends ps4-gcn-triangle (a solid-color triangle) into a textured quad:
// it binds two REAL GCN shader binaries via the register route AND binds a T#
// (image descriptor) + S# (sampler) + a texture in guest memory, then draws a
// two-triangle quad and samples the texture per-pixel.
//
//   * passthrough_vs.sb — a Sea-Islands vertex shader that loads a 128-bit
//     vertex-buffer V# from the descriptor-set pointer in s[2:3], fetches a vec4
//     position per vertex (idxen), and exports it as pos0 + param0. The PS reads
//     param0.xy as the texture UV, so UV == the vertex position's xy.
//   * texture_sample_ps.sb — a Sea-Islands pixel shader that interpolates the
//     per-vertex UV (attr0.xy), samples a 2D texture (image_sample) through a T#
//     + S#, and exports the sampled RGBA to render target 0.
//
// Both .sb blobs are the project's own self-authored corpus shaders (ZERO
// copyrighted assets); their raw OrbShdr bytes are embedded verbatim below so
// their ShaderBinaryInfo header + GCN machine code reach the driver unmodified —
// the recompiler parses them exactly as a game's .sb would arrive.
//
// The texture is a self-generated RGBA8 checkerboard (ZERO copyrighted assets).
//
// The texture ABI the emulator resolves (CORPUS_TEXTURE_SLOT): the PS reads a
// descriptor-set pointer from its user-SGPR pair s[0:1]; that set holds the T#
// at byte offset 0 (256 bits) and the S# at byte offset 32 (128 bits). This
// mirrors the VS's fixed s[2:3] V# ABI.
//
// The draw is a genuine PS4 PM4 stream:
//
//   1. sceGnmSetVsShader / sceGnmSetPsShader emit the documented SET_SH_REG +
//      SET_CONTEXT_REG runs (PGM_LO/HI, RSRC1/2, and the VS/PS pipeline-state
//      context regs) from a VsStageRegisters / PsStageRegisters block.
//   2. SET_SH_REG programs the VS user-SGPR pair s[2:3] with the vertex-buffer
//      descriptor-set pointer, AND the PS user-SGPR pair s[0:1] with the T#/S#
//      descriptor-set pointer.
//   3. SET_CONTEXT_REG programs the render target (CB_COLOR0_* at the registered
//      videoout framebuffer), the viewport (PA_CL_VPORT_*) and the screen scissor
//      (PA_SC_SCREEN_SCISSOR_*).
//   4. DRAW_INDEX_AUTO count=6 draws the two-triangle quad; then
//      sceGnmSubmitAndFlipCommandBuffers presents it.
//
// The result is a checkerboard-textured quad over the cleared background.
//
// Address invariants (real-GNM faithful):
//   * The .sb code, the V#/vertex data, the T#/S# descriptor set and the texel
//     data live in a 256-byte-aligned arena so a PGM / T#-base address
//     (addr >> 8) round-trips. Guest pointers are identity-mapped (guest ptr ==
//     host ptr) so a malloc'd address IS the GPU address.
//   * The framebuffer is 256-byte aligned so CB_COLOR0_BASE = fb>>8 aliases the
//     registered display buffer.

#include <orbis/libkernel.h>
#include <orbis/VideoOut.h>
#include <orbis/GnmDriver.h>

#include <stdint.h>
#include <stddef.h>
#include <stdlib.h>
#include <string.h>

// --- PM4 Type-3 opcodes (bits [15:8]), exact AMD/GFX6 values. ---
#define IT_NOP 0x10
#define IT_CLEAR_STATE 0x12
#define IT_CONTEXT_CONTROL 0x28
#define IT_DRAW_INDEX_AUTO 0x2D
#define IT_SET_CONTEXT_REG 0x69
#define IT_SET_SH_REG 0x76

// --- Flip-request (prepareFlip) marker packet ---
//
// Real GNM / gnmx's prepareFlip inserts a distinguished IT_NOP "data block" into
// the DCB whose first payload dword is a magic tag identifying it as the frame's
// flip request. sceGnmSubmitAndFlipCommandBuffers scans the tail of the DCB for
// this packet and turns it into the actual video-out flip. A conformant driver
// (real GNM's flip-request scan) REQUIRES the packet to be present —
// it looks at the LAST 64 dwords of the DCB and asserts the header dword equals a
// fixed 64-dword IT_NOP header, then reads the payload tag from the next dword.
//
//   header dword  = IT_NOP with a 63-dword body  == 0xC03E1000
//   payload dword = the PrepareFlip tag           == 0x68750777
//
// The remaining 62 body dwords are unused padding here (the plain PrepareFlip tag
// carries no label address, so the scanner reads only the tag). The packet is a
// pure NOP to the GPU: unemups4's PM4 executor and a real GNM host both skip
// an IT_NOP body wholesale and take the flip from the libcall / the tail scan, so
// adding it does not perturb the draw.
#define FLIP_NOP_TOTAL_DWORDS 64
#define FLIP_NOP_PAYLOAD_PREPARE_FLIP 0x68750777u

// --- GFX6 register offsets, relative to the window base the decoder resolves
//     (CONTEXT base 0xA000, SH base 0x2C00). ---

// SH: SPI_SHADER_USER_DATA_VS_0 = 0x4C; slot i is +i. s[2:3] is slot 2/3.
#define SH_SPI_SHADER_USER_DATA_VS_0 0x004C
// SH: SPI_SHADER_USER_DATA_PS_0 = 0x0C; slot i is +i. s[0:1] is slot 0/1 — the
//     T#/S# descriptor-set pointer the corpus PS reads (CORPUS_TEXTURE_SLOT).
#define SH_SPI_SHADER_USER_DATA_PS_0 0x000C

// CONTEXT: MRT0 color target (CB_COLOR0_*).
#define CTX_CB_COLOR0_BASE 0x0318
#define CTX_CB_COLOR0_INFO 0x031C

// CONTEXT: viewport 0 scale/offset (f32 bits).
#define CTX_PA_CL_VPORT_XSCALE 0x010F
#define CTX_PA_CL_VPORT_YSCALE 0x0111

// CONTEXT: screen scissor top-left / bottom-right (x[15:0], y[31:16]).
#define CTX_PA_SC_SCREEN_SCISSOR_TL 0x000D
#define CTX_PA_SC_SCREEN_SCISSOR_BR 0x000E

// Framebuffer geometry (the emulator registers every display buffer as 1920x1080).
#define FB_WIDTH 1920
#define FB_HEIGHT 1080

// ---------------------------------------------------------------------------
// Corpus shader binaries (self-authored; see crates/gcn/tests/corpus). Raw
// OrbShdr .sb bytes — ShaderBinaryInfo header + GCN machine code, verbatim.
// ---------------------------------------------------------------------------

// passthrough_vs.sb (68 bytes): loads s[0:3] V# from s[2:3], buffer_load_format
// _xyzw idxen v0, exports pos0 + param0.
static const uint8_t passthrough_vs_sb[] = {
    0x00, 0x03, 0x80, 0xc0, 0x7f, 0x00, 0x8c, 0xbf, 0x00, 0x20, 0x0c, 0xe0,
    0x00, 0x04, 0x00, 0x80, 0x70, 0x0f, 0x8c, 0xbf, 0xcf, 0x08, 0x00, 0xf8,
    0x04, 0x05, 0x06, 0x07, 0x0f, 0x02, 0x00, 0xf8, 0x04, 0x05, 0x06, 0x07,
    0x00, 0x00, 0x81, 0xbf, 0x4f, 0x72, 0x62, 0x53, 0x68, 0x64, 0x72, 0x01,
    0x04, 0x28, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x30, 0x5f, 0x53, 0x56,
    0x01, 0x00, 0x00, 0x00, 0x9c, 0x2c, 0x66, 0x11,
};

// texture_sample_ps.sb (68 bytes): interpolates attr0.xy as UV, image_sample
// v[4:7], v[2:3], s[0:7], s[8:11]; exports mrt0 = the sampled RGBA.
static const uint8_t texture_sample_ps_sb[] = {
    0x00, 0x03, 0xfc, 0xbe, 0x00, 0x00, 0x08, 0xc8, 0x01, 0x00, 0x09, 0xc8,
    0x00, 0x01, 0x0c, 0xc8, 0x01, 0x01, 0x0d, 0xc8, 0x00, 0x0f, 0x80, 0xf0,
    0x02, 0x04, 0x40, 0x00, 0x0f, 0x18, 0x00, 0xf8, 0x04, 0x05, 0x06, 0x07,
    0x00, 0x00, 0x81, 0xbf, 0x4f, 0x72, 0x62, 0x53, 0x68, 0x64, 0x72, 0x01,
    0x00, 0x28, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x32, 0x5f, 0x53, 0x50,
    0x01, 0x00, 0x00, 0x00, 0xb3, 0xcb, 0xb4, 0x89,
};

// ---------------------------------------------------------------------------
// Vertex data: a quad as two triangles (6 vec4-float positions) in clip space.
// The passthrough VS fetches these through the V#; it exports each position as
// both pos0 (clip position) and param0 (the varying), so the PS's interpolated
// attr0.xy UV equals the interpolated position.xy. Spanning [-0.8, 0.8] gives a
// UV range wide enough to show several checkerboard cells under Repeat sampling.
// ---------------------------------------------------------------------------
#define Q 0.8f
static const float quad_vertices[6][4] = {
    // triangle 0: TL, BL, BR
    {-Q,  Q, 0.0f, 1.0f}, // top-left
    {-Q, -Q, 0.0f, 1.0f}, // bottom-left
    { Q, -Q, 0.0f, 1.0f}, // bottom-right
    // triangle 1: TL, BR, TR
    {-Q,  Q, 0.0f, 1.0f}, // top-left
    { Q, -Q, 0.0f, 1.0f}, // bottom-right
    { Q,  Q, 0.0f, 1.0f}, // top-right
};
#define QUAD_VERT_COUNT 6

// ---------------------------------------------------------------------------
// Checkerboard texture: an 8x8 RGBA8 image, alternating two colors per cell. A
// self-generated pattern (ZERO copyrighted assets). Filled at runtime so the
// bytes live in the identity-mapped arena the T# base points at.
// ---------------------------------------------------------------------------
#define TEX_W 8
#define TEX_H 8

static void fill_checkerboard(uint8_t *px, uint32_t w, uint32_t h) {
    for (uint32_t y = 0; y < h; y++) {
        for (uint32_t x = 0; x < w; x++) {
            uint8_t *p = px + ((size_t)y * w + x) * 4;
            int on = ((x ^ y) & 1) != 0;
            if (on) {
                p[0] = 230; p[1] = 40; p[2] = 40; p[3] = 255; // red cell
            } else {
                p[0] = 245; p[1] = 245; p[2] = 245; p[3] = 255; // white cell
            }
        }
    }
}

// ---------------------------------------------------------------------------
// VsStageRegisters / PsStageRegisters — the Sony Gnm shader register-setup blocks
// the gnmx set-shader builders consume. Field order matches the retail struct.
// ---------------------------------------------------------------------------
typedef struct {
    uint32_t pgm_lo;             // [0] SPI_SHADER_PGM_LO_VS  (addr >> 8)
    uint32_t pgm_hi;             // [1] SPI_SHADER_PGM_HI_VS  (0)
    uint32_t pgm_rsrc1;          // [2] SPI_SHADER_PGM_RSRC1_VS
    uint32_t pgm_rsrc2;          // [3] SPI_SHADER_PGM_RSRC2_VS
    uint32_t spi_vs_out_config;  // [4] SPI_VS_OUT_CONFIG
    uint32_t spi_shader_pos_fmt; // [5] SPI_SHADER_POS_FORMAT
    uint32_t pa_cl_vs_out_cntl;  // [6] PA_CL_VS_OUT_CNTL
} VsStageRegisters;

typedef struct {
    uint32_t pgm_lo;             // [0]  SPI_SHADER_PGM_LO_PS  (addr >> 8)
    uint32_t pgm_hi;             // [1]  SPI_SHADER_PGM_HI_PS  (0)
    uint32_t pgm_rsrc1;          // [2]  SPI_SHADER_PGM_RSRC1_PS
    uint32_t pgm_rsrc2;          // [3]  SPI_SHADER_PGM_RSRC2_PS
    uint32_t spi_shader_z_fmt;   // [4]  SPI_SHADER_Z_FORMAT
    uint32_t spi_shader_col_fmt; // [5]  SPI_SHADER_COL_FORMAT
    uint32_t spi_ps_input_ena;   // [6]  SPI_PS_INPUT_ENA
    uint32_t spi_ps_input_addr;  // [7]  SPI_PS_INPUT_ADDR
    uint32_t spi_ps_in_control;  // [8]  SPI_PS_IN_CONTROL
    uint32_t spi_baryc_cntl;     // [9]  SPI_BARYC_CNTL
    uint32_t db_shader_control;  // [10] DB_SHADER_CONTROL
    uint32_t cb_shader_mask;     // [11] CB_SHADER_MASK
} PsStageRegisters;

// PM4 Type-3 header: type=3, opcode in [15:8], count=(body_dwords-1) in [29:16].
static inline uint32_t pm4_type3(uint8_t opcode, uint32_t body_dwords) {
    uint32_t count = (body_dwords - 1) & 0x3FFF;
    return (0x3u << 30) | (count << 16) | ((uint32_t)opcode << 8);
}

// Emit a SET_*_REG writing `n` consecutive values starting at reg_off.
static uint32_t *emit_set_reg(uint32_t *cmd, uint8_t opcode, uint32_t reg_off,
                              const uint32_t *values, uint32_t n) {
    *cmd++ = pm4_type3(opcode, n + 1);
    *cmd++ = reg_off;
    for (uint32_t i = 0; i < n; i++) {
        *cmd++ = values[i];
    }
    return cmd;
}

// The default-hardware-state preamble every Gnm frame opens with.
static uint32_t *emit_default_hw_state(uint32_t *cmd) {
    *cmd++ = pm4_type3(IT_CLEAR_STATE, 1);
    *cmd++ = 0;
    *cmd++ = pm4_type3(IT_CONTEXT_CONTROL, 2);
    *cmd++ = 0x80000000; // LOAD_CONTROL: load everything
    *cmd++ = 0x80000000; // SHADOW_ENABLE
    return cmd;
}

// Emit the prepareFlip-request IT_NOP marker as the DCB's final 64 dwords. The
// header is a 63-dword-body IT_NOP (0xC03E1000); the first payload dword carries
// the PrepareFlip tag the driver's flip scanner matches. The rest is zeroed
// padding so the whole packet is exactly 64 dwords and lands at the buffer tail.
static uint32_t *emit_prepare_flip(uint32_t *cmd) {
    *cmd++ = pm4_type3(IT_NOP, FLIP_NOP_TOTAL_DWORDS - 1);
    *cmd++ = FLIP_NOP_PAYLOAD_PREPARE_FLIP;
    for (uint32_t i = 2; i < FLIP_NOP_TOTAL_DWORDS; i++) {
        *cmd++ = 0;
    }
    return cmd;
}

// Build the four dwords of a vec4-float vertex-buffer V# (dfmt 14 =
// _32_32_32_32, nfmt 7 = float, identity swizzle x=4 y=5 z=6 w=7). Matches the
// gnm crate's decode_v_sharp layout.
static void build_vec4_vsharp(uint32_t out[4], uint64_t base, uint32_t stride,
                              uint32_t num_records) {
    out[0] = (uint32_t)(base & 0xFFFFFFFF);
    out[1] = ((uint32_t)(base >> 32) & 0xFFFF) | ((stride & 0x3FFF) << 16);
    out[2] = num_records;
    out[3] = 4u | (5u << 3) | (6u << 6) | (7u << 9) | (7u << 12) | (14u << 15);
}

// Build the eight dwords of a linear RGBA8 T# (256-bit image resource), matching
// the gnm crate's decode_t_sharp GFX6/7 layout:
//   word0 = base[39:8]            (base >> 8)
//   word1 = base[47:40] | (dfmt<<20) | (nfmt<<26); dfmt 10 = COLOR_8_8_8_8,
//           nfmt 0 = UNORM
//   word2 = (width-1)[13:0] | ((height-1)<<14)
//   word3 = tiling index [22:20] (0 = linear)
static void build_rgba8_tsharp(uint32_t out[8], uint64_t base, uint32_t w,
                               uint32_t h) {
    for (int i = 0; i < 8; i++) {
        out[i] = 0;
    }
    out[0] = (uint32_t)(base >> 8);
    out[1] = ((uint32_t)(base >> 40) & 0xFF) | (10u << 20); // dfmt=10 (RGBA8), nfmt=0 (unorm)
    out[2] = (w - 1) | ((h - 1) << 14);
    out[3] = 0; // linear
}

// Build the four dwords of an S# (128-bit sampler). Only word2[20] is read by
// the subset: 0 = point/nearest filter, 1 = bilinear.
static void build_ssharp(uint32_t out[4], int bilinear) {
    out[0] = 0;
    out[1] = 0;
    out[2] = bilinear ? (1u << 20) : 0u;
    out[3] = 0;
}

#define DCB_DWORDS 256

// Static DCB (kept below 4 GB so a 32-bit read of the dcb-address array yields the
// identity-mapped pointer).
static uint32_t g_dcb[DCB_DWORDS];

// 256-align a guest pointer upward.
static uint64_t align_up_256(uint64_t p) {
    return (p + 0xFF) & ~(uint64_t)0xFF;
}

int main(void) {
    sceKernelDebugOutText(0, "[GUEST] GCN textured quad: register-route .sb VS/PS + T#/S# + DrawIndexAuto\n");

    // --- Framebuffer + videoout (so there is a registered target to draw into) ---
    size_t fb_bytes = (size_t)FB_WIDTH * FB_HEIGHT * 4;
    void *fb_raw = malloc(fb_bytes + 256);
    uint8_t *fb = (uint8_t *)align_up_256((uint64_t)fb_raw);
    memset(fb, 0, fb_bytes); // cleared background

    // Open the main video-out port. The canonical OpenOrbis sequence
    // (samples/_common/graphics.cpp) opens user ORBIS_VIDEO_USER_MAIN (0xFF) on
    // bus ORBIS_VIDEO_OUT_BUS_MAIN (0) with index 0 and no param — a conformant
    // emulator asserts on the user/bus pair, so these args must be exact.
    int vo_handle = sceVideoOutOpen(ORBIS_VIDEO_USER_MAIN, ORBIS_VIDEO_OUT_BUS_MAIN, 0, 0);

    // Describe the display buffer before registering it. Mirrors graphics.cpp's
    // sceVideoOutSetBufferAttribute(&attr, 0x80000000, 1, 0, w, h, w): a linear
    // A8B8G8R8 SRGB surface at the framebuffer geometry. RegisterBuffers takes a
    // non-NULL attribute so the video-out port knows the buffer's format/pitch.
    OrbisVideoOutBufferAttribute attr;
    sceVideoOutSetBufferAttribute(&attr, ORBIS_VIDEO_OUT_PIXEL_FORMAT_A8B8G8R8_SRGB,
                                  ORBIS_VIDEO_OUT_TILING_MODE_LINEAR,
                                  ORBIS_VIDEO_OUT_ASPECT_RATIO_16_9,
                                  FB_WIDTH, FB_HEIGHT, FB_WIDTH);

    void *fb_list[1] = { fb };
    sceVideoOutRegisterBuffers(vo_handle, 0, fb_list, 1, &attr);

    // 60 Hz flip rate, matching the canonical sequence.
    sceVideoOutSetFlipRate(vo_handle, ORBIS_VIDEO_OUT_FLIP_60HZ);

    // --- Shader + vertex + descriptor + texture arena (one 256-aligned region) ---
    // Layout: [VS .sb][PS .sb][vertex data][VS V#][texture texels][PS desc set:
    // T#+S#], each sub-block aligned where an addr>>8 round-trip is needed.
    size_t tex_bytes = (size_t)TEX_W * TEX_H * 4;
    size_t arena_bytes = sizeof(passthrough_vs_sb) + sizeof(texture_sample_ps_sb) +
                         sizeof(quad_vertices) + 16 + tex_bytes + 64 + 8 * 256;
    void *arena_raw = malloc(arena_bytes);
    uint64_t cur = align_up_256((uint64_t)arena_raw);

    uint64_t vs_addr = cur;
    memcpy((void *)vs_addr, passthrough_vs_sb, sizeof(passthrough_vs_sb));
    cur = align_up_256(vs_addr + sizeof(passthrough_vs_sb));

    uint64_t ps_addr = cur;
    memcpy((void *)ps_addr, texture_sample_ps_sb, sizeof(texture_sample_ps_sb));
    cur = align_up_256(ps_addr + sizeof(texture_sample_ps_sb));

    // Vertex buffer: 6 vec4-float vertices, 16-byte stride.
    uint64_t vtx_addr = (cur + 0xF) & ~(uint64_t)0xF;
    memcpy((void *)vtx_addr, quad_vertices, sizeof(quad_vertices));
    cur = vtx_addr + sizeof(quad_vertices);

    // VS descriptor set: one vec4 V# at binding 0 (base=vtx_addr, stride 16).
    uint64_t vs_desc_addr = (cur + 0xF) & ~(uint64_t)0xF;
    build_vec4_vsharp((uint32_t *)vs_desc_addr, vtx_addr, 16, QUAD_VERT_COUNT);
    cur = vs_desc_addr + 16;

    // Texture texels: 256-aligned so the T# base (addr>>8) round-trips.
    uint64_t tex_addr = align_up_256(cur);
    fill_checkerboard((uint8_t *)tex_addr, TEX_W, TEX_H);
    cur = tex_addr + tex_bytes;

    // PS descriptor set: T# (32 bytes) at offset 0, S# (16 bytes) at offset 32.
    // 256-aligned so its own address is clean.
    uint64_t ps_desc_addr = align_up_256(cur);
    build_rgba8_tsharp((uint32_t *)ps_desc_addr, tex_addr, TEX_W, TEX_H);
    build_ssharp((uint32_t *)(ps_desc_addr + 32), 0 /* point filter */);

    // --- Shader register-setup blocks ---
    VsStageRegisters vs_regs;
    vs_regs.pgm_lo = (uint32_t)(vs_addr >> 8);
    vs_regs.pgm_hi = 0;
    // RSRC1: VGPRS=(8-1)/4=1, SGPRS=(4-1)/8=0 -> 8 VGPRs, 8 SGPRs allocated.
    vs_regs.pgm_rsrc1 = 0x00000001;
    // RSRC2: USER_SGPR=4 (s0..s3, s[2:3] = descriptor-set pointer) in bits [5:1].
    vs_regs.pgm_rsrc2 = (4u << 1);
    vs_regs.spi_vs_out_config = 0x00000000; // one param export (param0)
    vs_regs.spi_shader_pos_fmt = 0x00000004; // POS0 = 32_32_32_32
    vs_regs.pa_cl_vs_out_cntl = 0x00000000;

    PsStageRegisters ps_regs;
    ps_regs.pgm_lo = (uint32_t)(ps_addr >> 8);
    ps_regs.pgm_hi = 0;
    // RSRC1: VGPRS=(8-1)/4=1 -> 8 VGPRs (the PS uses v0..v7).
    ps_regs.pgm_rsrc1 = 0x00000001;
    // RSRC2: USER_SGPR=2 (s0..s1 = T#/S# descriptor-set pointer) in bits [5:1].
    ps_regs.pgm_rsrc2 = (2u << 1);
    ps_regs.spi_shader_z_fmt = 0x00000000;   // no depth export
    ps_regs.spi_shader_col_fmt = 0x00000009; // MRT0 = 32_32_32_32
    ps_regs.spi_ps_input_ena = 0x00000003;   // enable barycentric I/J (v0,v1)
    ps_regs.spi_ps_input_addr = 0x00000003;
    ps_regs.spi_ps_in_control = 0x00000001;  // one interpolated input (attr0)
    ps_regs.spi_baryc_cntl = 0x00000000;
    ps_regs.db_shader_control = 0x00000000;
    ps_regs.cb_shader_mask = 0x0000000F; // MRT0 all four components

    // --- Build the DCB ---
    uint32_t *dcb = g_dcb;
    uint32_t *cmd = dcb;

    cmd = emit_default_hw_state(cmd);

    // (1) Register-route VS/PS binds.
    sceGnmSetVsShader(cmd, 29, &vs_regs, 0);
    cmd += 29;
    sceGnmSetPsShader(cmd, 40, &ps_regs);
    cmd += 40;

    // (2a) VS user-SGPR s[2:3] = vertex-buffer descriptor-set pointer.
    uint32_t vs_desc_ptr[2] = {
        (uint32_t)(vs_desc_addr & 0xFFFFFFFF),
        (uint32_t)(vs_desc_addr >> 32),
    };
    cmd = emit_set_reg(cmd, IT_SET_SH_REG, SH_SPI_SHADER_USER_DATA_VS_0 + 2, vs_desc_ptr, 2);

    // (2b) PS user-SGPR s[0:1] = T#/S# descriptor-set pointer (CORPUS_TEXTURE_SLOT).
    uint32_t ps_desc_ptr[2] = {
        (uint32_t)(ps_desc_addr & 0xFFFFFFFF),
        (uint32_t)(ps_desc_addr >> 32),
    };
    cmd = emit_set_reg(cmd, IT_SET_SH_REG, SH_SPI_SHADER_USER_DATA_PS_0 + 0, ps_desc_ptr, 2);

    // (3a) Render target: CB_COLOR0_BASE aliases the registered framebuffer.
    uint32_t cb_base[1] = { (uint32_t)((uint64_t)fb >> 8) };
    cmd = emit_set_reg(cmd, IT_SET_CONTEXT_REG, CTX_CB_COLOR0_BASE, cb_base, 1);
    // CB_COLOR0_INFO.FORMAT (bits [5:2]) = COLOR_8_8_8_8 (0x0A) -> videoout format.
    uint32_t cb_info[1] = { 0x0A << 2 };
    cmd = emit_set_reg(cmd, IT_SET_CONTEXT_REG, CTX_CB_COLOR0_INFO, cb_info, 1);

    // (3b) Viewport 0: map NDC [-1,1] to the full framebuffer, Y negated.
    float xscale = (float)FB_WIDTH * 0.5f;
    float yscale = -(float)FB_HEIGHT * 0.5f;
    float xoffset = (float)FB_WIDTH * 0.5f;
    float yoffset = (float)FB_HEIGHT * 0.5f;
    uint32_t vp_x[2], vp_y[2];
    memcpy(&vp_x[0], &xscale, 4);
    memcpy(&vp_x[1], &xoffset, 4);
    memcpy(&vp_y[0], &yscale, 4);
    memcpy(&vp_y[1], &yoffset, 4);
    cmd = emit_set_reg(cmd, IT_SET_CONTEXT_REG, CTX_PA_CL_VPORT_XSCALE, vp_x, 2);
    cmd = emit_set_reg(cmd, IT_SET_CONTEXT_REG, CTX_PA_CL_VPORT_YSCALE, vp_y, 2);

    // (3c) Screen scissor: full framebuffer (x[15:0], y[31:16]).
    uint32_t sc_tl[1] = { 0 };
    uint32_t sc_br[1] = { (uint32_t)FB_WIDTH | ((uint32_t)FB_HEIGHT << 16) };
    cmd = emit_set_reg(cmd, IT_SET_CONTEXT_REG, CTX_PA_SC_SCREEN_SCISSOR_TL, sc_tl, 1);
    cmd = emit_set_reg(cmd, IT_SET_CONTEXT_REG, CTX_PA_SC_SCREEN_SCISSOR_BR, sc_br, 1);

    // (4) Draw the quad: DRAW_INDEX_AUTO count=6 (two triangles).
    sceGnmDrawIndexAuto(cmd, 7, QUAD_VERT_COUNT, (OrbisGnmDrawFlags){.asuint = 0});
    *cmd++ = pm4_type3(IT_DRAW_INDEX_AUTO, 2);
    *cmd++ = QUAD_VERT_COUNT; // vertex/index count
    *cmd++ = 0;               // draw initiator flags

    // (5) Flip-request marker: the DCB's final 64 dwords. Must be the LAST packet
    // so the driver's tail scan (DCB end - 64) lands on its header. Remember the
    // packet's first dword so the loop can re-arm the tag each frame (see below).
    uint32_t *flip_pkt = cmd;
    cmd = emit_prepare_flip(cmd);

    // --- Submit + flip, in a per-frame render loop ---
    // A real PS4 title never submits once and returns — it runs a flip LOOP
    // forever, re-submitting a draw + submit-and-flip each frame so the display
    // keeps a fresh frame on screen. A driver that presents only the LAST
    // submit-and-flip shows nothing if the guest falls off the end
    // after a single flip; a continuous re-submit keeps the framebuffer
    // presented. The DCB (its draw + prepareFlip packet) and the shader/vertex/
    // texture arena are all persistent, so the same command buffer is valid to
    // re-submit verbatim every frame.
    uint32_t dcb_bytes = (uint32_t)((cmd - dcb) * sizeof(uint32_t));
    void *dcb_addrs[1] = { dcb };
    uint32_t dcb_sizes[1] = { dcb_bytes };
    void *ccb_addrs[1] = { NULL };
    uint32_t ccb_sizes[1] = { 0 };

    sceKernelDebugOutText(0, "[GUEST] GCN textured quad: entering flip loop.\n");

    for (;;) {
        // Re-arm the prepareFlip marker each frame. A conformant flip driver
        // a real GNM host consumes the request by PATCHING the tag in place — after a
        // submit it rewrites PrepareFlip (0x68750777) to PatchedFlip
        // (0x68750776). Re-submitting the same DCB verbatim would then present a
        // PatchedFlip tag, which the tail scan rejects. Rewriting the header +
        // tag back to a fresh PrepareFlip request before every submit makes each
        // frame's flip request valid again. (Transparent to unemups4: the packet
        // is an IT_NOP whose payload tag our PM4 decoder ignores regardless.)
        flip_pkt[0] = pm4_type3(IT_NOP, FLIP_NOP_TOTAL_DWORDS - 1); // 0xC03E1000
        flip_pkt[1] = FLIP_NOP_PAYLOAD_PREPARE_FLIP;                // 0x68750777

        sceGnmSubmitAndFlipCommandBuffers(1, dcb_addrs, dcb_sizes, ccb_addrs,
                                          ccb_sizes, vo_handle, 0, 1, 0);
        sceGnmSubmitDone();
        // Pace the loop to ~60 Hz. sceKernelUsleep is the portable wait both
        // unemups4 and a real GNM host honor; it also yields so the display thread can
        // present the flipped frame between submissions.
        sceKernelUsleep(16000);
    }
    return 0;
}
