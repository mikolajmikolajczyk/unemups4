// Hand-written PM4 test ELF — the phase-2 Gnm corpus (task-22, doc-2 D1).
//
// OpenOrbis ships no working native Gnm 3D sample and no shader compiler / .sb
// blobs (doc-3), so this is a HAND-WRITTEN PM4 example. The DCB (draw command
// buffer) PM4 stream is built by hand with the exact IT_* opcodes task-21's
// decoder recognises, then submitted via the raw sceGnmSubmit* NIDs stubbed in
// task-20. task-21's decoder walks the submitted buffer and traces it — that
// trace is the validation (AC #3/#4).
//
// Two design points forced by this emulator (both faithful to what the corpus is
// meant to exercise — the submit path + the PM4 decoder, not GPU execution):
//
//  1. The DCB lives in a STATIC (global) buffer, not malloc'd. OpenOrbis malloc
//     returns a >4 GB host pointer (e.g. 0x4_0021_4000), but task-20's submit
//     stub reads the guest's dcb-address array as 32-bit GPU addresses (doc-4:
//     "u32* dcb_gpu_addrs[]" — the PS4 passes onion/garlic offsets that fit 32
//     bits). A global lands below 4 GB (~0x41_0000), so its low 32 bits equal
//     the full identity-mapped address and the decoder reads the real buffer.
//
//  2. The OpenOrbis GnmDriver.h per-call PM4 builders
//     (sceGnmDrawInitDefaultHardwareState350, sceGnmDrawIndexAuto,
//     sceGnmSetEmbeddedVsShader/PsShader) are HLE stubs in this emulator
//     (task-20/24/31): they return but write NO PM4. We still call the ones the
//     emulator stubs, to exercise their NIDs and the [GNM] log, but the actual
//     PM4 the decoder walks is what we hand-write — the exact packets those
//     builders emit on real hardware. In particular the embedded-shader builders
//     record the bound embedded shader id, which the phase-3.5 executor
//     (task-24) resolves to a hardcoded host pipeline; Tier B calls them so that
//     bound-shader → DrawIndexAuto path runs end-to-end, and still hand-emits the
//     equivalent VS/PS SH-register PM4 for task-21's decoder (no .sb blob).
//
// TWO TIERS in one ELF, submitted as two separate flips:
//
//   Tier A — trace/present, NO shader:
//       default-HW-state preamble -> set clear color -> SubmitAndFlip.
//       A pure PM4 command stream, no draw, no shader. Minimum viable corpus.
//
//   Tier B — real draw with EMBEDDED shaders, still NO .sb blob:
//       + embedded fullscreen-quad VS regs (SET_SH_REG, id 0)
//       + embedded R/G-export PS regs (SET_SH_REG + SET_CONTEXT_REG, id 1)
//       + DRAW_INDEX_AUTO. Firmware-embedded shaders need no shader binary
//       (doc-3); recognised later by the phase-3.5 embedded-shader draw path.

#include <orbis/libkernel.h>
#include <orbis/VideoOut.h>
#include <orbis/GnmDriver.h>

#include <stdint.h>
#include <stddef.h>

// --- PM4 Type-3 opcodes (bits [15:8]), exact AMD/GFX6 values, mirror task-21's
//     crates/gnm/src/pm4/opcodes.rs so the trace renders named packets. ---
#define IT_CLEAR_STATE 0x12
#define IT_CONTEXT_CONTROL 0x28
#define IT_DRAW_INDEX_AUTO 0x2D
#define IT_SET_CONTEXT_REG 0x69
#define IT_SET_SH_REG 0x76

// --- GFX6 register offsets, relative to the window base the decoder resolves
//     (CONTEXT base 0xA000, SH base 0x2C00). ---
#define CONTEXT_CB_COLOR0_CLEAR_WORD0 0x02B0 // clear color a present/clear consumes
#define SH_SPI_SHADER_PGM_LO_VS 0x0048       // embedded VS program address
#define SH_SPI_SHADER_PGM_LO_PS 0x0008       // embedded PS program address
#define CONTEXT_SPI_SHADER_COL_FORMAT 0x01C5 // PS R/G color-export format

// PM4 Type-3 header: type=3, opcode in [15:8], count=(body_dwords-1) in [29:16].
static inline uint32_t pm4_type3(uint8_t opcode, uint32_t body_dwords) {
    uint32_t count = (body_dwords - 1) & 0x3FFF;
    return (0x3u << 30) | (count << 16) | ((uint32_t)opcode << 8);
}

// Emit an opcode with a single body dword.
static uint32_t *emit_op1(uint32_t *cmd, uint8_t opcode, uint32_t arg0) {
    *cmd++ = pm4_type3(opcode, 1);
    *cmd++ = arg0;
    return cmd;
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
// then CONTEXT_CONTROL. Stands in for sceGnmDrawInitDefaultHardwareState350's
// PM4 (which the emulator stub omits).
static uint32_t *emit_default_hw_state(uint32_t *cmd) {
    cmd = emit_op1(cmd, IT_CLEAR_STATE, 0);
    *cmd++ = pm4_type3(IT_CONTEXT_CONTROL, 2);
    *cmd++ = 0x80000000; // LOAD_CONTROL: load everything
    *cmd++ = 0x80000000; // SHADOW_ENABLE
    return cmd;
}

#define DCB_DWORDS 256

// Static DCBs — see design note #1 (kept below 4 GB so task-20's 32-bit address
// read yields the correct identity-mapped pointer).
static uint32_t g_dcb_a[DCB_DWORDS];
static uint32_t g_dcb_b[DCB_DWORDS];

static int open_video_out(void) {
    // A videohandle for SubmitAndFlip. Non-fatal for the trace path (the flip is
    // wired in a later phase); pass whatever handle we get.
    return sceVideoOutOpen(0xFF, 1, 0, 0);
}

static void submit_and_flip(uint32_t *dcb, uint32_t *cmd, int vo_handle) {
    uint32_t dcb_bytes = (uint32_t)((cmd - dcb) * sizeof(uint32_t));
    void *dcb_addrs[1] = {dcb};
    uint32_t dcb_sizes[1] = {dcb_bytes};
    void *ccb_addrs[1] = {NULL};
    uint32_t ccb_sizes[1] = {0};
    sceGnmSubmitAndFlipCommandBuffers(1, dcb_addrs, dcb_sizes, ccb_addrs, ccb_sizes,
                                      vo_handle, 0, 1, 0);
    sceGnmSubmitDone();
}

// Tier A: default HW state -> set clear color -> flip. No shader, no draw.
static void run_tier_a(int vo_handle) {
    sceKernelDebugOutText(0, "[GUEST] --- Tier A: init + clear + SubmitAndFlip (no shader) ---\n");

    uint32_t *dcb = g_dcb_a;
    uint32_t *cmd = dcb;

    // Call the real OpenOrbis HW-state builder NID (routes to task-20's stub) so
    // its NID is exercised; then hand-emit the equivalent PM4 into the DCB.
    sceGnmDrawInitDefaultHardwareState350(dcb, 256);
    cmd = emit_default_hw_state(cmd);

    // Clear color: cornflower-blue 0xFF6495ED into CB_COLOR0_CLEAR_WORD0/1.
    uint32_t clear_color[2] = {0xFF6495ED, 0x00000000};
    cmd = emit_set_reg(cmd, IT_SET_CONTEXT_REG, CONTEXT_CB_COLOR0_CLEAR_WORD0, clear_color, 2);

    submit_and_flip(dcb, cmd, vo_handle);
}

// Tier B: default HW state -> embedded VS regs -> embedded PS regs ->
// DRAW_INDEX_AUTO -> flip. Real geometry using firmware-embedded shaders.
static void run_tier_b(int vo_handle) {
    sceKernelDebugOutText(0, "[GUEST] --- Tier B: embedded VS/PS + DrawIndexAuto (real draw) ---\n");

    uint32_t *dcb = g_dcb_b;
    uint32_t *cmd = dcb;

    sceGnmDrawInitDefaultHardwareState350(dcb, 256);
    cmd = emit_default_hw_state(cmd);

    // Select the firmware-embedded shaders by id (doc-3 §3.4). These NIDs are now
    // HLE-stubbed (task-20/24): the emulator stub writes NO PM4 but RECORDS the
    // bound embedded id, which the phase-3.5 DrawIndexAuto executor arm (task-24)
    // resolves to a hardcoded host SPIR-V pipeline — no .sb blob. `cmd` is NOT
    // advanced (the stub emits nothing); the equivalent SH-register PM4 is still
    // hand-emitted below for task-21's decoder/trace.
    sceGnmSetEmbeddedVsShader(cmd, 29, 0, 0); // id 0 = fullscreen quad
    sceGnmSetEmbeddedPsShader(cmd, 40, 1);    // id 1 = 32-bit R/G export

    // Embedded VS shader id 0 (fullscreen quad): program address into the VS SH
    // register window. On real HW the firmware VS blob address; here a marker.
    uint32_t vs_pgm[2] = {0x0000E000, 0x00000000}; // LO, HI
    cmd = emit_set_reg(cmd, IT_SET_SH_REG, SH_SPI_SHADER_PGM_LO_VS, vs_pgm, 2);

    // Embedded PS shader id 1 (exports 32-bit R and G): program address + the
    // R/G color-export format.
    uint32_t ps_pgm[2] = {0x0000E100, 0x00000000}; // LO, HI
    cmd = emit_set_reg(cmd, IT_SET_SH_REG, SH_SPI_SHADER_PGM_LO_PS, ps_pgm, 2);
    uint32_t col_fmt[1] = {0x00000004}; // SPI_SHADER_COL_FORMAT: one 32_32 export
    cmd = emit_set_reg(cmd, IT_SET_CONTEXT_REG, CONTEXT_SPI_SHADER_COL_FORMAT, col_fmt, 1);

    // Real geometry: exercise the (task-20-stubbed) DrawIndexAuto NID, then
    // hand-emit the DRAW_INDEX_AUTO PM4 packet (index_count=3, then flags).
    sceGnmDrawIndexAuto(dcb, 7, 3, (OrbisGnmDrawFlags){.asuint = 0});
    *cmd++ = pm4_type3(IT_DRAW_INDEX_AUTO, 2);
    *cmd++ = 3; // index count
    *cmd++ = 0; // draw initiator flags

    submit_and_flip(dcb, cmd, vo_handle);
}

int main(void) {
    sceKernelDebugOutText(0, "[GUEST] PM4 test ELF (task-22): hand-written PM4 corpus\n");

    int vo_handle = open_video_out();

    run_tier_a(vo_handle);
    run_tier_b(vo_handle);

    sceKernelDebugOutText(0, "[GUEST] PM4 test ELF done.\n");
    return 0;
}
