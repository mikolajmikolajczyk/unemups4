// GCN triangle homebrew — a real register-route Gnm draw over corpus .sb shaders.
//
// Unlike ps4-pm4-test (which binds FIRMWARE-EMBEDDED shaders by id and hand-emits
// marker PGM addresses), this sample binds two REAL GCN shader binaries via the
// register route:
//
//   * passthrough_vs.sb — a Sea-Islands vertex shader that loads a 128-bit
//     vertex-buffer V# from the descriptor-set pointer in s[2:3], fetches a vec4
//     position per vertex (idxen), and exports it as pos0 + param0.
//   * flat_color_ps.sb  — a Sea-Islands pixel shader that exports a constant RGBA
//     (1.0, 0.25, 0.5, 1.0) to render target 0.
//
// Both .sb blobs are the project's own self-authored corpus shaders (ZERO
// copyrighted assets); their raw OrbShdr bytes are embedded verbatim below so
// their ShaderBinaryInfo header + GCN machine code reach the driver unmodified —
// the recompiler (and shadPS4) parse them exactly as a game's .sb would arrive.
//
// The draw is built as a genuine PS4 PM4 stream so the SAME .elf can render in a
// real-GNM-shaped path:
//
//   1. sceGnmSetVsShader / sceGnmSetPsShader emit the documented 29 / 40-dword
//      SET_SH_REG + SET_CONTEXT_REG runs (PGM_LO/HI, RSRC1/2, and the VS/PS
//      pipeline-state context regs) from a VsStageRegisters / PsStageRegisters
//      block — the retail gnmx builder body.
//   2. SET_SH_REG programs the VS user-SGPR pair s[2:3] with the descriptor-set
//      pointer (a 128-bit vertex-buffer V# at binding 0).
//   3. SET_CONTEXT_REG programs the render target (CB_COLOR0_* at the registered
//      videoout framebuffer), the viewport (PA_CL_VPORT_*) and the screen scissor
//      (PA_SC_SCREEN_SCISSOR_*) — required for the triangle to be visible.
//   4. DRAW_INDEX_AUTO count=3 draws the triangle; then
//      sceGnmSubmitAndFlipCommandBuffers presents it.
//
// The result is a solid-colored triangle over the cleared background.
//
// Address invariants (real-GNM faithful):
//   * The .sb code and the V#/vertex data live in a 256-byte-aligned buffer so a
//     PGM address (SET_SH_REG writes addr>>8, HI forced to 0) round-trips and the
//     recompiler reads the real bytes. Guest pointers are identity-mapped
//     (guest ptr == host ptr) so a malloc'd address IS the GPU address.
//   * The framebuffer is 256-byte aligned so CB_COLOR0_BASE = fb>>8 aliases the
//     registered display buffer.

#include <orbis/libkernel.h>
#include <orbis/VideoOut.h>
#include <orbis/GnmDriver.h>

#include <stdint.h>
#include <stddef.h>
#include <stdlib.h>
#include <string.h>

// --- PM4 Type-3 opcodes (bits [15:8]), exact AMD/GFX6 values, mirror the gnm
//     crate's crates/gnm/src/pm4/opcodes.rs so the decoder renders named packets. ---
#define IT_CLEAR_STATE 0x12
#define IT_CONTEXT_CONTROL 0x28
#define IT_DRAW_INDEX_AUTO 0x2D
#define IT_SET_CONTEXT_REG 0x69
#define IT_SET_SH_REG 0x76

// --- GFX6 register offsets, relative to the window base the decoder resolves
//     (CONTEXT base 0xA000, SH base 0x2C00). Only the offsets we hand-write for
//     the descriptor pointer / RT / viewport / scissor are defined here; the
//     shader-program + pipeline-state regs are emitted by sceGnmSet{Vs,Ps}Shader. ---

// SH: SPI_SHADER_USER_DATA_VS_0 = 0x4C; slot i is +i. s[2:3] is slot 2/3.
#define SH_SPI_SHADER_USER_DATA_VS_0 0x004C

// CONTEXT: MRT0 color target (CB_COLOR0_*).
#define CTX_CB_COLOR0_BASE 0x0318
#define CTX_CB_COLOR0_PITCH 0x0319
#define CTX_CB_COLOR0_SLICE 0x031A
#define CTX_CB_COLOR0_VIEW 0x031B
#define CTX_CB_COLOR0_INFO 0x031C
#define CTX_CB_COLOR0_ATTRIB 0x031D

// CONTEXT: viewport 0 scale/offset (f32 bits).
#define CTX_PA_CL_VPORT_XSCALE 0x010F
#define CTX_PA_CL_VPORT_XOFFSET 0x0110
#define CTX_PA_CL_VPORT_YSCALE 0x0111
#define CTX_PA_CL_VPORT_YOFFSET 0x0112

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

// flat_color_ps.sb (60 bytes): v_mov constants, exports mrt0 = (1.0,0.25,0.5,1.0).
static const uint8_t flat_color_ps_sb[] = {
    0xf2, 0x02, 0x00, 0x7e, 0xff, 0x02, 0x02, 0x7e, 0x00, 0x00, 0x80, 0x3e,
    0xf0, 0x02, 0x04, 0x7e, 0xf2, 0x02, 0x06, 0x7e, 0x0f, 0x18, 0x00, 0xf8,
    0x00, 0x01, 0x02, 0x03, 0x00, 0x00, 0x81, 0xbf, 0x4f, 0x72, 0x62, 0x53,
    0x68, 0x64, 0x72, 0x01, 0x00, 0x20, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x30, 0x5f, 0x53, 0x50, 0x01, 0x00, 0x00, 0x00, 0x06, 0xe7, 0x3a, 0xe6,
};

// ---------------------------------------------------------------------------
// Vertex data: 3 vec4-float positions forming a triangle in clip space.
// The passthrough VS fetches these through the V#; the recompiled input is a
// Location=0 vec4 the backend feeds from vertex-buffer slot 0.
// ---------------------------------------------------------------------------
static const float triangle_vertices[3][4] = {
    { 0.0f,  0.5f, 0.0f, 1.0f}, // top
    {-0.5f, -0.5f, 0.0f, 1.0f}, // bottom-left
    { 0.5f, -0.5f, 0.0f, 1.0f}, // bottom-right
};

// ---------------------------------------------------------------------------
// VsStageRegisters / PsStageRegisters — the Sony Gnm shader register-setup blocks
// the gnmx set-shader builders consume. Field order matches the retail struct the
// driver reads: [PGM_LO, PGM_HI, RSRC1, RSRC2, ...pipeline-state...]. PGM_HI is 0
// (retail invariant: the driver rejects a non-zero high half). PGM_LO is addr>>8.
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
// body = reg_off + n values -> body_dwords = n + 1.
static uint32_t *emit_set_reg(uint32_t *cmd, uint8_t opcode, uint32_t reg_off,
                              const uint32_t *values, uint32_t n) {
    *cmd++ = pm4_type3(opcode, n + 1);
    *cmd++ = reg_off;
    for (uint32_t i = 0; i < n; i++) {
        *cmd++ = values[i];
    }
    return cmd;
}

// The default-hardware-state preamble every Gnm frame opens with: CLEAR_STATE
// then CONTEXT_CONTROL. Stands in for sceGnmDrawInitDefaultHardwareState350's PM4.
static uint32_t *emit_default_hw_state(uint32_t *cmd) {
    *cmd++ = pm4_type3(IT_CLEAR_STATE, 1);
    *cmd++ = 0;
    *cmd++ = pm4_type3(IT_CONTEXT_CONTROL, 2);
    *cmd++ = 0x80000000; // LOAD_CONTROL: load everything
    *cmd++ = 0x80000000; // SHADOW_ENABLE
    return cmd;
}

// Build the four little-endian dwords of a vec4-float vertex-buffer V# (dfmt 14 =
// _32_32_32_32, nfmt 7 = float, identity swizzle x=4 y=5 z=6 w=7). This is the
// descriptor the passthrough VS fetches; binding 0 sits at desc-set offset 0.
static void build_vec4_vsharp(uint32_t out[4], uint64_t base, uint32_t stride,
                              uint32_t num_records) {
    out[0] = (uint32_t)(base & 0xFFFFFFFF);
    out[1] = ((uint32_t)(base >> 32) & 0xFFFF) | ((stride & 0x3FFF) << 16);
    out[2] = num_records;
    out[3] = 4u | (5u << 3) | (6u << 6) | (7u << 9) | (7u << 12) | (14u << 15);
}

#define DCB_DWORDS 256

// Static DCB (kept below 4 GB so a 32-bit read of the dcb-address array yields the
// identity-mapped pointer, mirroring ps4-pm4-test's design note).
static uint32_t g_dcb[DCB_DWORDS];

// 256-align a guest pointer upward.
static uint64_t align_up_256(uint64_t p) {
    return (p + 0xFF) & ~(uint64_t)0xFF;
}

int main(void) {
    sceKernelDebugOutText(0, "[GUEST] GCN triangle: register-route .sb VS/PS + DrawIndexAuto\n");

    // --- Framebuffer + videoout (so there is a registered target to draw into) ---
    // 256-align so CB_COLOR0_BASE = fb>>8 round-trips to the registered address.
    size_t fb_bytes = (size_t)FB_WIDTH * FB_HEIGHT * 4;
    void *fb_raw = malloc(fb_bytes + 256);
    uint8_t *fb = (uint8_t *)align_up_256((uint64_t)fb_raw);
    memset(fb, 0, fb_bytes); // cleared background

    int vo_handle = sceVideoOutOpen(0xFF, 1, 0, 0);
    void *fb_list[1] = { fb };
    sceVideoOutRegisterBuffers(vo_handle, 0, fb_list, 1, 0);

    // --- Shader + vertex + descriptor arena (one 256-aligned malloc region) ---
    // Layout: [VS .sb][PS .sb][vertex data][V#], each sub-block 256-aligned where a
    // PGM address is needed. Guest ptr == GPU addr (identity-mapped).
    size_t arena_bytes = sizeof(passthrough_vs_sb) + sizeof(flat_color_ps_sb) +
                         sizeof(triangle_vertices) + 16 + 4 * 256;
    void *arena_raw = malloc(arena_bytes);
    uint64_t cur = align_up_256((uint64_t)arena_raw);

    uint64_t vs_addr = cur;
    memcpy((void *)vs_addr, passthrough_vs_sb, sizeof(passthrough_vs_sb));
    cur = align_up_256(vs_addr + sizeof(passthrough_vs_sb));

    uint64_t ps_addr = cur;
    memcpy((void *)ps_addr, flat_color_ps_sb, sizeof(flat_color_ps_sb));
    cur = align_up_256(ps_addr + sizeof(flat_color_ps_sb));

    // Vertex buffer: 3 vec4-float vertices, 16-byte stride.
    uint64_t vtx_addr = (cur + 0xF) & ~(uint64_t)0xF;
    memcpy((void *)vtx_addr, triangle_vertices, sizeof(triangle_vertices));

    // Descriptor set: one vec4 V# at binding 0 (base=vtx_addr, stride 16, 3 records).
    uint64_t desc_addr = (vtx_addr + sizeof(triangle_vertices) + 0xF) & ~(uint64_t)0xF;
    build_vec4_vsharp((uint32_t *)desc_addr, vtx_addr, 16, 3);

    // --- Shader register-setup blocks (real gnmx VsStageRegisters/PsStageRegisters) ---
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
    // RSRC1: VGPRS=(4-1)/4=0 -> 4 VGPRs. RSRC2: no user SGPRs.
    ps_regs.pgm_rsrc1 = 0x00000000;
    ps_regs.pgm_rsrc2 = 0x00000000;
    ps_regs.spi_shader_z_fmt = 0x00000000;   // no depth export
    ps_regs.spi_shader_col_fmt = 0x00000009; // MRT0 = 32_32_32_32 (FP16_ABGR-class slot)
    ps_regs.spi_ps_input_ena = 0x00000000;
    ps_regs.spi_ps_input_addr = 0x00000000;
    ps_regs.spi_ps_in_control = 0x00000000;
    ps_regs.spi_baryc_cntl = 0x00000000;
    ps_regs.db_shader_control = 0x00000000;
    ps_regs.cb_shader_mask = 0x0000000F; // MRT0 all four components

    // --- Build the DCB ---
    uint32_t *dcb = g_dcb;
    uint32_t *cmd = dcb;

    cmd = emit_default_hw_state(cmd);

    // (1) Register-route VS/PS binds: the gnmx builders emit the documented
    //     29 / 40-dword SET_SH_REG + SET_CONTEXT_REG runs into the DCB.
    sceGnmSetVsShader(cmd, 29, &vs_regs, 0);
    cmd += 29;
    sceGnmSetPsShader(cmd, 40, &ps_regs);
    cmd += 40;

    // (2) VS user-SGPR s[2:3] = descriptor-set pointer (64-bit, low/high dwords).
    uint32_t desc_ptr[2] = {
        (uint32_t)(desc_addr & 0xFFFFFFFF),
        (uint32_t)(desc_addr >> 32),
    };
    cmd = emit_set_reg(cmd, IT_SET_SH_REG, SH_SPI_SHADER_USER_DATA_VS_0 + 2, desc_ptr, 2);

    // (3a) Render target: CB_COLOR0_BASE aliases the registered framebuffer.
    uint32_t cb_base[1] = { (uint32_t)((uint64_t)fb >> 8) };
    cmd = emit_set_reg(cmd, IT_SET_CONTEXT_REG, CTX_CB_COLOR0_BASE, cb_base, 1);
    // CB_COLOR0_INFO.FORMAT (bits [5:2]) = COLOR_8_8_8_8 (0x0A) -> the videoout
    // framebuffer format. FORMAT 0 is COLOR_INVALID, so it must be encoded, not left 0.
    uint32_t cb_info[1] = { 0x0A << 2 };
    cmd = emit_set_reg(cmd, IT_SET_CONTEXT_REG, CTX_CB_COLOR0_INFO, cb_info, 1);

    // (3b) Viewport 0: map NDC [-1,1] to the full framebuffer. Y scale negated for
    //      the top-left origin convention. scale = dim/2, offset = dim/2.
    float xscale = (float)FB_WIDTH * 0.5f;
    float yscale = -(float)FB_HEIGHT * 0.5f;
    float xoffset = (float)FB_WIDTH * 0.5f;
    float yoffset = (float)FB_HEIGHT * 0.5f;
    uint32_t vp_x[2], vp_y[2];
    memcpy(&vp_x[0], &xscale, 4);
    memcpy(&vp_x[1], &xoffset, 4);
    memcpy(&vp_y[0], &yscale, 4);
    memcpy(&vp_y[1], &yoffset, 4);
    // XSCALE, XOFFSET are contiguous (0x10F, 0x110); YSCALE, YOFFSET (0x111, 0x112).
    cmd = emit_set_reg(cmd, IT_SET_CONTEXT_REG, CTX_PA_CL_VPORT_XSCALE, vp_x, 2);
    cmd = emit_set_reg(cmd, IT_SET_CONTEXT_REG, CTX_PA_CL_VPORT_YSCALE, vp_y, 2);

    // (3c) Screen scissor: full framebuffer (x[15:0], y[31:16]).
    uint32_t sc_tl[1] = { 0 };
    uint32_t sc_br[1] = { (uint32_t)FB_WIDTH | ((uint32_t)FB_HEIGHT << 16) };
    cmd = emit_set_reg(cmd, IT_SET_CONTEXT_REG, CTX_PA_SC_SCREEN_SCISSOR_TL, sc_tl, 1);
    cmd = emit_set_reg(cmd, IT_SET_CONTEXT_REG, CTX_PA_SC_SCREEN_SCISSOR_BR, sc_br, 1);

    // (4) Draw the triangle: DRAW_INDEX_AUTO count=3, then the draw-initiator flags.
    sceGnmDrawIndexAuto(cmd, 7, 3, (OrbisGnmDrawFlags){.asuint = 0});
    *cmd++ = pm4_type3(IT_DRAW_INDEX_AUTO, 2);
    *cmd++ = 3; // vertex/index count
    *cmd++ = 0; // draw initiator flags

    // --- Submit + flip ---
    uint32_t dcb_bytes = (uint32_t)((cmd - dcb) * sizeof(uint32_t));
    void *dcb_addrs[1] = { dcb };
    uint32_t dcb_sizes[1] = { dcb_bytes };
    void *ccb_addrs[1] = { NULL };
    uint32_t ccb_sizes[1] = { 0 };
    sceGnmSubmitAndFlipCommandBuffers(1, dcb_addrs, dcb_sizes, ccb_addrs, ccb_sizes,
                                      vo_handle, 0, 1, 0);
    sceGnmSubmitDone();

    sceKernelDebugOutText(0, "[GUEST] GCN triangle: submitted.\n");

    // Keep the process alive so the window can present the flipped frame.
    for (;;) {
        sceKernelUsleep(100000);
    }
    return 0;
}
