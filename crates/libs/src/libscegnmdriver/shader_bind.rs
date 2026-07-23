//! libSceGnmDriver embedded-shader / set-shader command builders. Two bind routes
//! converge on the same `ps4-gnm` shadow register file (doc-2 §5):
//!
//! - **Embedded** (`sceGnmSetEmbeddedVs/PsShader`): record `Embedded{id}` into the
//!   driver-owned bound-shader view so the phase-3.5 DrawIndexAuto executor arm
//!   resolves it through the `EmbeddedShaderProvider` (doc-2 §4/§5). No PM4 emitted.
//! - **Register** (`sceGnmSetVsShader`/`sceGnmSetPsShader`): emit the documented 29/40
//!   dwords (doc-1 §2) of `SET_SH_REG` / `SET_CONTEXT_REG` PM4 from the guest full
//!   `VsStageRegisters` / `PsStageRegisters` block into the caller's cmdbuf, so a later
//!   submit decodes `SPI_SHADER_PGM_*` into the shadow SH bank (the draw path then
//!   derives a `ShaderRef::GcnBinary`) and the VS/PS pipeline state into the CONTEXT
//!   bank. This makes the HLE-linked path and a statically-linked builder converge on
//!   the register file. GCN resolve/recompile stays deferred (P4-18). The remaining
//!   `Set{Cs,Es,Gs,Hs,Ls}Shader` are still log-only stubs.

use crate::context::NativeContext;
use ps4_gnm::driver::driver;
use ps4_gnm::pm4::emit;
use ps4_macros::ps4_syscall;
use ps4_syscalls::SyscallId;
use tracing::info;

/// Write an emitted PM4 dword stream into the caller's command buffer, honoring the
/// caller's `reserved` dword count. A fault on the write is logged and dropped rather
/// than faulting the guest (the builder is best-effort, like the submit stubs).
///
/// The builder writes the documented 29/40 dwords. A guest that reserved fewer
/// dwords than the emitter produces would have adjacent guest memory clobbered, so
/// if the reservation can't hold the full packet we log once and skip the write
/// entirely rather than overflow (a partial PM4 write would also leave a malformed
/// stream). `reserved == 0` means the caller passed no bound; write the full packet.
///
/// task-166: `emit::set_vs_shader`/`set_ps_shader` already self-pad to their documented
/// 29/40 dwords with a trailing NOP, but if the guest reserved a LARGER slot than that the
/// extra `reserved - 29/40` tail would still expose stale prior-frame bytes on a reused
/// command arena (the decode walk mis-reading the hole as real packets). So pad to exactly
/// `reserved` dwords ([`emit::pad_to_reserved`]) — the same fill the draw builder applies.
///
/// task-115/task-138: the write is routed through the SMC-tracked write seam
/// ([`ps4_core::write_guest`]) rather than a raw `IdentityMem` store — the cmdbuf is
/// guest-resident and may later be executed, so an SMC-invisible store could leave a stale
/// JIT translation. Headless (no seam wired) degrades to a clean no-op — never a raw store;
/// the in-process tests wire a host-backed write seam to exercise this path.
fn emit_into_cmdbuf(cmdbuf: u64, reserved: u32, pm4: &[u32]) {
    if cmdbuf == 0 {
        return;
    }
    if reserved != 0 && (reserved as usize) < pm4.len() {
        info!(
            "[GNM]   shader-set reserved {} dwords < {} needed; skipping cmdbuf write at {:#x}",
            reserved,
            pm4.len(),
            cmdbuf
        );
        return;
    }
    // Fill the guest's reserved slot to exactly `reserved` dwords (`[shader-set PM4][NOP]`) so
    // a decode walk clears the whole slot. `reserved` of 0 or == pm4.len() leaves it unchanged.
    let words = emit::pad_to_reserved(pm4, reserved);
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in &words {
        bytes.extend_from_slice(&w.to_le_bytes());
    }
    let res = match ps4_core::write_guest::write_guest() {
        // Real run: SMC-tracked store through the live memory manager.
        Some(seam) => seam.write_bytes(cmdbuf, &bytes),
        // Headless: no seam wired → clean no-op (never a raw store).
        None => Ok(()),
    };
    if res.is_err() {
        info!("[GNM]   shader-set cmdbuf write faulted at {:#x}", cmdbuf);
    }
}

/// Read a shader register-setup block (`vsregs`/`psregs`) of `fields` dwords from guest
/// memory: the full Gnm `VsStageRegisters` / `PsStageRegisters` struct the emitter maps
/// to real register writes (see `pm4::emit`), whose leading dwords are
/// `[PGM_LO, PGM_HI, PGM_RSRC1, PGM_RSRC2]`. Returns `None` on a null / unmapped / out-of-
/// bounds `regs` pointer so the caller emits nothing: a failed read must NOT fall back to a
/// zero block, or the emitter would write zero PGM regs and the draw path would derive a
/// bogus `GcnBinary{addr:0}` bind at null. No regs → no shader-set.
///
/// The `regs` pointer is **guest-supplied and untrusted**: a block that starts near an
/// unmapped page would over-read `fields * 4` bytes past its mapping if read through the
/// unbounded identity view (a host SIGSEGV, or a leak of adjacent host memory into the
/// shader registers). So the read is routed through the process-global bounds-checked
/// seam ([`ps4_core::bounded_read`]), which validates the whole range against the live VMA
/// set and rejects a straddling read cleanly.
///
/// **Headless degradation (task-138):** when no bounded-read source is wired the seam yields
/// `None`; we then read nothing (`None` → no shader-set) rather than fall back to a raw
/// identity read of an untrusted pointer. The in-process tests wire a host-backed bounded-read
/// source to exercise the read path. In the real emulator the seam is always wired, so the
/// untrusted path is always bounds-checked.
fn read_reg_block(regs: u64, fields: usize) -> Option<Vec<u32>> {
    if regs == 0 {
        return None;
    }
    match ps4_core::bounded_read::bounded_read() {
        // Real run: validate the untrusted pointer against the live VMA set. An
        // unmapped / straddling read is a clean `Err` → `None` → no shader-set.
        Some(src) => {
            let bytes = src.read_ranged(regs, fields * 4).ok()?;
            Some(
                bytes
                    .chunks_exact(4)
                    .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                    .collect(),
            )
        }
        // Headless: no seam wired → read nothing (never a raw identity read).
        None => None,
    }
}

/// `sceGnmSetEmbeddedVsShader(cmdbuf, size, shader_id, shader_modifier)` — selects
/// a built-in vertex shader. Stub: no shader bound yet.
#[ps4_syscall(
    id = SyscallId::SCE_GNM_SET_EMBEDDED_VS_SHADER,
    lib = crate::libs::LIB_SCE_GNM_DRIVER,
    name = "sceGnmSetEmbeddedVsShader"
)]
pub fn sce_gnm_set_embedded_vs_shader(
    _cmdbuf: u64,
    _size: u32,
    shader_id: u32,
    _shader_modifier: u32,
) -> i32 {
    info!("[GNM] sceGnmSetEmbeddedVsShader id={}", shader_id);
    // Record the bind so the phase-3.5 DrawIndexAuto executor arm knows which
    // embedded shader is bound and can resolve it through the EmbeddedShaderProvider
    // (doc-2 §4/§5). The bound-shader view lives in the driver-owned GpuState, so
    // reach it through driver().lock() (same lock record_submit / the executor hold).
    // Vulkan-free.
    if let Ok(mut drv) = driver().lock() {
        drv.state_mut()
            .bind_embedded_shader(ps4_gnm::shader::source::Stage::Vertex, shader_id);
    }
    0
}

/// `sceGnmSetEmbeddedPsShader(cmdbuf, size, shader_id)` — selects a built-in
/// pixel shader. Stub: no shader bound yet.
#[ps4_syscall(
    id = SyscallId::SCE_GNM_SET_EMBEDDED_PS_SHADER,
    lib = crate::libs::LIB_SCE_GNM_DRIVER,
    name = "sceGnmSetEmbeddedPsShader"
)]
pub fn sce_gnm_set_embedded_ps_shader(_cmdbuf: u64, _size: u32, shader_id: u32) -> i32 {
    info!("[GNM] sceGnmSetEmbeddedPsShader id={}", shader_id);
    if let Ok(mut drv) = driver().lock() {
        drv.state_mut()
            .bind_embedded_shader(ps4_gnm::shader::source::Stage::Pixel, shader_id);
    }
    0
}

/// `sceGnmSetVsShader(cmdbuf, size, vs_regs, shader_modifier)` — binds a vertex
/// shader's registers. Emits the documented 29 dwords (doc-1 §2) of `SET_SH_REG` /
/// `SET_CONTEXT_REG` PM4 from the full `VsStageRegisters` block into the caller's
/// `cmdbuf`, so a submit later decodes `SPI_SHADER_PGM_LO/HI/RSRC1/2_VS` into the SH
/// bank and the VS pipeline state (`SPI_VS_OUT_CONFIG`, `SPI_SHADER_POS_FORMAT`,
/// `PA_CL_VS_OUT_CNTL`) into the CONTEXT bank. The draw path derives a
/// `ShaderRef::GcnBinary` from the SH regs; the actual GCN resolve/recompile is still
/// deferred (P4-18). No shadow bind is recorded here — registers are the truth (unlike
/// the embedded route).
#[ps4_syscall(
    id = SyscallId::SCE_GNM_SET_VS_SHADER,
    lib = crate::libs::LIB_SCE_GNM_DRIVER,
    name = "sceGnmSetVsShader"
)]
pub fn sce_gnm_set_vs_shader(cmdbuf: u64, size: u32, vs_regs: u64, _shader_modifier: u32) -> i32 {
    info!("[GNM] sceGnmSetVsShader regs={:#x}", vs_regs);
    // On a null / unmapped regs block, emit nothing: a zero-filled fallback would
    // record a bogus shader bound at address 0.
    let Some(regs) = read_reg_block(vs_regs, emit::VS_STAGE_REG_FIELDS) else {
        info!("[GNM]   sceGnmSetVsShader: unreadable vs_regs, no shader-set emitted");
        return 0;
    };
    // A real register-route bind supersedes any earlier embedded bind for this stage —
    // drop the embedded shadow so derive_bound_shaders reads the registers we emit
    // (task-73). The raw-PM4 embedded corpus never calls this, so it keeps embedded.
    if let Ok(mut drv) = driver().lock() {
        drv.state_mut()
            .unbind_embedded_shader(ps4_gnm::shader::source::Stage::Vertex);
    }
    let pm4 = emit::set_vs_shader(&regs);
    emit_into_cmdbuf(cmdbuf, size, &pm4);
    0
}

/// `sceGnmSetPsShader(cmdbuf, size, ps_regs)` — binds a pixel shader's registers.
/// Emits the documented 40 dwords (doc-1 §2) of `SET_SH_REG` / `SET_CONTEXT_REG` PM4
/// from the full `PsStageRegisters` block into the caller's `cmdbuf` (the PS export /
/// interpolation state lands in the CONTEXT bank); see [`sce_gnm_set_vs_shader`].
#[ps4_syscall(
    id = SyscallId::SCE_GNM_SET_PS_SHADER,
    lib = crate::libs::LIB_SCE_GNM_DRIVER,
    name = "sceGnmSetPsShader"
)]
pub fn sce_gnm_set_ps_shader(cmdbuf: u64, size: u32, ps_regs: u64) -> i32 {
    info!("[GNM] sceGnmSetPsShader regs={:#x}", ps_regs);
    let Some(regs) = read_reg_block(ps_regs, emit::PS_STAGE_REG_FIELDS) else {
        info!("[GNM]   sceGnmSetPsShader: unreadable ps_regs, no shader-set emitted");
        return 0;
    };
    // See sceGnmSetVsShader: a register-route bind supersedes the embedded shadow.
    if let Ok(mut drv) = driver().lock() {
        drv.state_mut()
            .unbind_embedded_shader(ps4_gnm::shader::source::Stage::Pixel);
    }
    let pm4 = emit::set_ps_shader(&regs);
    emit_into_cmdbuf(cmdbuf, size, &pm4);
    0
}

/// `sceGnmSetPsShader350(cmdbuf, size, ps_regs)` — the SDK-3.50 variant of the pixel-shader
/// bind. Per `data/oo_sdk/include/orbis/GnmDriver.h` its signature is **identical** to
/// [`sce_gnm_set_ps_shader`] — `(uint32_t* cmd, uint32_t numdwords, const void* psregs)`,
/// `numdwords` must be 40 — so the "350" is purely an ABI-version tag, not a different
/// struct or an extra modifier arg. It emits the same 40-dword `PsStageRegisters` PM4, so
/// the emitter is shared verbatim with the base handler (doc-6 Entry 4). Celeste (2D
/// MonoGame) calls this variant rather than the base `sceGnmSetPsShader`.
#[ps4_syscall(
    id = SyscallId::SCE_GNM_SET_PS_SHADER350,
    lib = crate::libs::LIB_SCE_GNM_DRIVER,
    name = "sceGnmSetPsShader350"
)]
pub fn sce_gnm_set_ps_shader350(cmdbuf: u64, size: u32, ps_regs: u64) -> i32 {
    info!("[GNM] sceGnmSetPsShader350 regs={:#x}", ps_regs);
    let Some(regs) = read_reg_block(ps_regs, emit::PS_STAGE_REG_FIELDS) else {
        info!("[GNM]   sceGnmSetPsShader350: unreadable ps_regs, no shader-set emitted");
        return 0;
    };
    // See sceGnmSetVsShader: a register-route bind supersedes the embedded shadow.
    if let Ok(mut drv) = driver().lock() {
        drv.state_mut()
            .unbind_embedded_shader(ps4_gnm::shader::source::Stage::Pixel);
    }
    let pm4 = emit::set_ps_shader(&regs);
    emit_into_cmdbuf(cmdbuf, size, &pm4);
    0
}

/// `sceGnmUpdatePsShader350(cmdbuf, size, ps_regs)` — re-emit the pixel-shader registers
/// without a full rebind. On our register-route model an "update" and a "set" both just
/// write the `SPI_SHADER_PGM_*` / PS pipeline-state PM4 into the caller's cmdbuf (registers
/// are the truth — the shadow bank is overwritten either way), so Update shares the same
/// emitter path as [`sce_gnm_set_ps_shader350`]. The `350` matches SetPsShader350's ABI.
#[ps4_syscall(
    id = SyscallId::SCE_GNM_UPDATE_PS_SHADER350,
    lib = crate::libs::LIB_SCE_GNM_DRIVER,
    name = "sceGnmUpdatePsShader350"
)]
pub fn sce_gnm_update_ps_shader350(cmdbuf: u64, size: u32, ps_regs: u64) -> i32 {
    info!("[GNM] sceGnmUpdatePsShader350 regs={:#x}", ps_regs);
    let Some(regs) = read_reg_block(ps_regs, emit::PS_STAGE_REG_FIELDS) else {
        info!("[GNM]   sceGnmUpdatePsShader350: unreadable ps_regs, no shader-set emitted");
        return 0;
    };
    // Like the Set handlers, a register-route bind — including this "update" re-emit —
    // supersedes any earlier embedded shadow for the stage. Without dropping it,
    // derive_bound_shaders keeps returning the stale embedded ShaderRef and the
    // SPI_SHADER_PGM_* we emit here is ignored (task-73).
    if let Ok(mut drv) = driver().lock() {
        drv.state_mut()
            .unbind_embedded_shader(ps4_gnm::shader::source::Stage::Pixel);
    }
    let pm4 = emit::set_ps_shader(&regs);
    emit_into_cmdbuf(cmdbuf, size, &pm4);
    0
}

/// `sceGnmUpdateVsShader(cmdbuf, size, vs_regs, shader_modifier)` — re-emit the vertex-shader
/// registers without a full rebind. Same reasoning as [`sce_gnm_update_ps_shader350`]: an
/// update re-writes the VS `SPI_SHADER_PGM_*` / pipeline-state PM4, sharing the emitter with
/// [`sce_gnm_set_vs_shader`].
#[ps4_syscall(
    id = SyscallId::SCE_GNM_UPDATE_VS_SHADER,
    lib = crate::libs::LIB_SCE_GNM_DRIVER,
    name = "sceGnmUpdateVsShader"
)]
pub fn sce_gnm_update_vs_shader(
    cmdbuf: u64,
    size: u32,
    vs_regs: u64,
    _shader_modifier: u32,
) -> i32 {
    info!("[GNM] sceGnmUpdateVsShader regs={:#x}", vs_regs);
    let Some(regs) = read_reg_block(vs_regs, emit::VS_STAGE_REG_FIELDS) else {
        info!("[GNM]   sceGnmUpdateVsShader: unreadable vs_regs, no shader-set emitted");
        return 0;
    };
    // Like the Set handlers, a register-route bind — including this "update" re-emit —
    // supersedes any earlier embedded shadow for the stage; otherwise derive_bound_shaders
    // keeps returning the stale embedded ShaderRef and the SPI_SHADER_PGM_* we emit is
    // ignored (task-73).
    if let Ok(mut drv) = driver().lock() {
        drv.state_mut()
            .unbind_embedded_shader(ps4_gnm::shader::source::Stage::Vertex);
    }
    let pm4 = emit::set_vs_shader(&regs);
    emit_into_cmdbuf(cmdbuf, size, &pm4);
    0
}

/// `sceGnmSetCsShader(cmdbuf, size, cs_regs, shader_modifier)` — binds a compute
/// shader's registers. Stub: recorded via log only; register binds deferred.
#[ps4_syscall(
    id = SyscallId::SCE_GNM_SET_CS_SHADER,
    lib = crate::libs::LIB_SCE_GNM_DRIVER,
    name = "sceGnmSetCsShader"
)]
pub fn sce_gnm_set_cs_shader(
    _cmdbuf: u64,
    _size: u32,
    _cs_regs: u64,
    _shader_modifier: u32,
) -> i32 {
    info!("[GNM] sceGnmSetCsShader");
    0
}

/// `sceGnmSetEsShader(cmdbuf, size, es_regs, shader_modifier)` — binds an export
/// shader's registers. Stub: recorded via log only; register binds deferred.
#[ps4_syscall(
    id = SyscallId::SCE_GNM_SET_ES_SHADER,
    lib = crate::libs::LIB_SCE_GNM_DRIVER,
    name = "sceGnmSetEsShader"
)]
pub fn sce_gnm_set_es_shader(
    _cmdbuf: u64,
    _size: u32,
    _es_regs: u64,
    _shader_modifier: u32,
) -> i32 {
    info!("[GNM] sceGnmSetEsShader");
    0
}

/// `sceGnmSetGsShader(cmdbuf, size, gs_regs)` — binds a geometry shader's
/// registers. Stub: recorded via log only; register binds deferred.
#[ps4_syscall(
    id = SyscallId::SCE_GNM_SET_GS_SHADER,
    lib = crate::libs::LIB_SCE_GNM_DRIVER,
    name = "sceGnmSetGsShader"
)]
pub fn sce_gnm_set_gs_shader(_cmdbuf: u64, _size: u32, _gs_regs: u64) -> i32 {
    info!("[GNM] sceGnmSetGsShader");
    0
}

/// `sceGnmSetHsShader(cmdbuf, size, hs_regs, modifier)` — binds a hull shader's
/// registers. Stub: recorded via log only; register binds deferred.
#[ps4_syscall(
    id = SyscallId::SCE_GNM_SET_HS_SHADER,
    lib = crate::libs::LIB_SCE_GNM_DRIVER,
    name = "sceGnmSetHsShader"
)]
pub fn sce_gnm_set_hs_shader(_cmdbuf: u64, _size: u32, _hs_regs: u64, _modifier: u32) -> i32 {
    info!("[GNM] sceGnmSetHsShader");
    0
}

/// `sceGnmSetLsShader(cmdbuf, size, ls_regs, modifier)` — binds a local shader's
/// registers. Stub: recorded via log only; register binds deferred.
#[ps4_syscall(
    id = SyscallId::SCE_GNM_SET_LS_SHADER,
    lib = crate::libs::LIB_SCE_GNM_DRIVER,
    name = "sceGnmSetLsShader"
)]
pub fn sce_gnm_set_ls_shader(_cmdbuf: u64, _size: u32, _ls_regs: u64, _modifier: u32) -> i32 {
    info!("[GNM] sceGnmSetLsShader");
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use ps4_core::bounded_read::{BoundedRead, registered_source};
    use ps4_gnm::pm4::decode::{self, OwnedPacket};
    use ps4_gnm::pm4::opcodes::{set_reg_base, sh_reg};
    use ps4_gnm::state::GpuState;
    use std::sync::Arc;

    // task-138: `read_reg_block` / `emit_into_cmdbuf` now route through the process-global
    // bounded-read and write seams, with no raw-identity fallback. Tests that exercise a
    // successful read wire a host-backed `BoundedRead` source over the regs array; tests that
    // exercise a successful cmdbuf write wire a host-backed `WriteGuest`. Each override is a
    // RAII scoped guard on the seam's `Registered` (restored on drop, panic-safe). The
    // bounded-read `Registered` and the write `Registered` are DIFFERENT instances, so taking
    // one scoped override on each within a single test cannot self-deadlock.

    /// A host-backed write seam: guest ptr == host addr, so `write_bytes` copies straight into
    /// the caller's cmdbuf host array. Used to exercise the emit path without a real VM.
    struct TestWrite;

    impl ps4_core::write_guest::WriteGuest for TestWrite {
        fn write_bytes(&self, addr: u64, data: &[u8]) -> Result<(), &'static str> {
            if addr == 0 {
                return Err("null");
            }
            unsafe {
                std::ptr::copy_nonoverlapping(data.as_ptr(), addr as *mut u8, data.len());
            }
            Ok(())
        }
    }

    /// Wire a host-backed write seam covering the whole process for the duration of the guard.
    fn wire_write()
    -> ps4_core::registered::ScopeGuard<'static, dyn ps4_core::write_guest::WriteGuest> {
        ps4_core::write_guest::registered_source().override_scoped(Arc::new(TestWrite))
    }

    /// Wire a host-backed bounded-read source over `[base, base+len)` for the guard's lifetime.
    fn wire_read(
        base: u64,
        len: usize,
    ) -> ps4_core::registered::ScopeGuard<'static, dyn BoundedRead> {
        let mem: Arc<dyn BoundedRead> = Arc::new(OneRegionMem {
            start: base,
            end: base + len as u64,
        });
        registered_source().override_scoped(mem)
    }

    /// Decode a host command buffer (identity-mapped: host addr == guest ptr) and
    /// apply its SET_*_REG packets into a fresh GpuState — the same shadow-register
    /// path the executor's `run` uses. Returns the resulting state.
    fn decode_and_apply(cmd: &[u32]) -> GpuState {
        let mut bytes = Vec::with_capacity(cmd.len() * 4);
        for w in cmd {
            bytes.extend_from_slice(&w.to_le_bytes());
        }
        let mut state = GpuState::default();
        for pkt in decode::decode_bytes(&bytes) {
            if let OwnedPacket::Type3 { opcode, body, .. } = pkt
                && let Some(base) = set_reg_base(opcode)
            {
                state.apply_set_reg(base, &body);
            }
        }
        state
    }

    #[test]
    fn set_vs_shader_writes_pm4_that_round_trips() {
        // AC #2: sceGnmSetVsShader reads a vsregs block from guest memory and writes
        // SET_SH_REG PM4 into the caller's cmdbuf that the decoder round-trips into
        // the same PGM_LO/HI/RSRC1/2_VS register values. Both buffers are real host
        // memory (host addr == guest ptr); the read seam is wired over the vsregs array
        // and the write seam copies straight into the host-backed cmdbuf (task-138).
        // Full VsStageRegisters block ([1]=PGM_HI must be 0; the emitter forces it).
        let vs_regs: [u32; emit::VS_STAGE_REG_FIELDS] = [
            0x0000_2000,
            0x0000_0000,
            0x00AB_CDEF,
            0x0000_00A0,
            0x0000_0005,
            0x0000_0004,
            0x0000_00FF,
        ];
        let mut cmdbuf = [0u32; emit::SET_VS_SHADER_DWORDS];

        let _rd = wire_read(vs_regs.as_ptr() as u64, std::mem::size_of_val(&vs_regs));
        let _wr = wire_write();

        let rc = sce_gnm_set_vs_shader(
            cmdbuf.as_mut_ptr() as u64,
            emit::SET_VS_SHADER_DWORDS as u32,
            vs_regs.as_ptr() as u64,
            0,
        );
        assert_eq!(rc, 0);

        let state = decode_and_apply(&cmdbuf);
        assert_eq!(
            state.sh_regs.get(sh_reg::SPI_SHADER_PGM_LO_VS),
            Some(vs_regs[0])
        );
        assert_eq!(state.sh_regs.get(sh_reg::SPI_SHADER_PGM_HI_VS), Some(0));
        assert_eq!(
            state.sh_regs.get(sh_reg::SPI_SHADER_PGM_RSRC1_VS),
            Some(vs_regs[2])
        );
        assert_eq!(
            state.sh_regs.get(sh_reg::SPI_SHADER_PGM_RSRC2_VS),
            Some(vs_regs[3])
        );
    }

    #[test]
    fn set_ps_shader_writes_pm4_that_round_trips() {
        // Full PsStageRegisters block ([1]=PGM_HI must be 0; the emitter forces it).
        let ps_regs: [u32; emit::PS_STAGE_REG_FIELDS] = [
            0x0000_3000,
            0x0000_0000,
            0x0012_3456,
            0x0000_0040,
            0x0000_0000,
            0x0000_0004,
            0x0000_000F,
            0x0000_000F,
            0x0000_0001,
            0x0000_0002,
            0x0000_0010,
            0x0000_000F,
        ];
        let mut cmdbuf = [0u32; emit::SET_PS_SHADER_DWORDS];

        let _rd = wire_read(ps_regs.as_ptr() as u64, std::mem::size_of_val(&ps_regs));
        let _wr = wire_write();

        let rc = sce_gnm_set_ps_shader(
            cmdbuf.as_mut_ptr() as u64,
            emit::SET_PS_SHADER_DWORDS as u32,
            ps_regs.as_ptr() as u64,
        );
        assert_eq!(rc, 0);

        let state = decode_and_apply(&cmdbuf);
        assert_eq!(
            state.sh_regs.get(sh_reg::SPI_SHADER_PGM_LO_PS),
            Some(ps_regs[0])
        );
        assert_eq!(
            state.sh_regs.get(sh_reg::SPI_SHADER_PGM_RSRC2_PS),
            Some(ps_regs[3])
        );
        // A draw after this emit would derive a GcnBinary ref at (hi:lo)<<8; HI is
        // forced to 0 by the emitter (retail invariant).
        let bound = state.derive_bound_shaders();
        assert!(matches!(
            bound.ps,
            Some(ps4_gnm::shader::source::ShaderRef::GcnBinary { addr, .. })
                if addr == ps4_gnm::shader::sb::pgm_addr(ps_regs[0], 0)
        ));
    }

    #[test]
    fn set_ps_shader350_matches_base_set_ps_shader() {
        // sceGnmSetPsShader350 has the SAME signature + PsStageRegisters layout as the
        // base sceGnmSetPsShader (SDK GnmDriver.h): the "350" is an ABI-version tag, not a
        // different struct. So the 40-dword PM4 it emits must be byte-identical to the base
        // handler's for the same regs block. Emit both into separate cmdbufs and compare.
        let ps_regs: [u32; emit::PS_STAGE_REG_FIELDS] = [
            0x0000_3000,
            0x0000_0000,
            0x0012_3456,
            0x0000_0040,
            0x0000_0000,
            0x0000_0004,
            0x0000_000F,
            0x0000_000F,
            0x0000_0001,
            0x0000_0002,
            0x0000_0010,
            0x0000_000F,
        ];
        let mut cmd_base = [0u32; emit::SET_PS_SHADER_DWORDS];
        let mut cmd_350 = [0u32; emit::SET_PS_SHADER_DWORDS];

        let _rd = wire_read(ps_regs.as_ptr() as u64, std::mem::size_of_val(&ps_regs));
        let _wr = wire_write();

        assert_eq!(
            sce_gnm_set_ps_shader(
                cmd_base.as_mut_ptr() as u64,
                emit::SET_PS_SHADER_DWORDS as u32,
                ps_regs.as_ptr() as u64,
            ),
            0
        );
        assert_eq!(
            sce_gnm_set_ps_shader350(
                cmd_350.as_mut_ptr() as u64,
                emit::SET_PS_SHADER_DWORDS as u32,
                ps_regs.as_ptr() as u64,
            ),
            0
        );
        assert_eq!(
            cmd_base, cmd_350,
            "SetPsShader350 must emit the same PM4 as the base SetPsShader"
        );
        // And the emitted stream round-trips to a GcnBinary PS bind, same as the base.
        let state = decode_and_apply(&cmd_350);
        let bound = state.derive_bound_shaders();
        assert!(matches!(
            bound.ps,
            Some(ps4_gnm::shader::source::ShaderRef::GcnBinary { addr, .. })
                if addr == ps4_gnm::shader::sb::pgm_addr(ps_regs[0], 0)
        ));
    }

    #[test]
    fn null_cmdbuf_is_a_clean_noop() {
        // A null cmdbuf (no reserved space) must not fault — just return success. Wire the
        // read seam so the regs read succeeds and we reach the null-cmdbuf guard in emit.
        let vs_regs = [1u32; emit::VS_STAGE_REG_FIELDS];
        let _rd = wire_read(vs_regs.as_ptr() as u64, std::mem::size_of_val(&vs_regs));
        assert_eq!(sce_gnm_set_vs_shader(0, 29, vs_regs.as_ptr() as u64, 0), 0);
    }

    #[test]
    fn undersized_reservation_skips_write_no_overflow() {
        // A guest that reserved fewer dwords than the emitter produces must NOT have
        // adjacent memory clobbered: the write is skipped entirely. Back the cmdbuf
        // with a guard word past the reservation and assert it stays untouched.
        let vs_regs: [u32; emit::VS_STAGE_REG_FIELDS] = [
            0x0000_2000,
            0x0000_0000,
            0x00AB_CDEF,
            0x0000_00A0,
            0x0000_0005,
            0x0000_0004,
            0x0000_00FF,
        ];
        // Reserve only 4 dwords; append a sentinel the emitter must never reach.
        let mut cmdbuf = [0u32; emit::SET_VS_SHADER_DWORDS];
        let sentinel = 0xDEAD_BEEF;
        cmdbuf[4] = sentinel;

        // Wire the read seam so the regs read succeeds; the write is then gated off by the
        // undersized reservation before reaching the write seam (so no write seam needed).
        let _rd = wire_read(vs_regs.as_ptr() as u64, std::mem::size_of_val(&vs_regs));

        let rc = sce_gnm_set_vs_shader(cmdbuf.as_mut_ptr() as u64, 4, vs_regs.as_ptr() as u64, 0);
        assert_eq!(rc, 0);

        // Nothing was written: the whole buffer (including the header slot) is intact.
        assert_eq!(
            cmdbuf[0], 0,
            "header written despite undersized reservation"
        );
        assert_eq!(
            cmdbuf[4], sentinel,
            "sentinel clobbered by overflowing write"
        );
        // And decoding the (untouched) buffer derives no shader bind.
        let state = decode_and_apply(&cmdbuf);
        assert_eq!(state.sh_regs.get(sh_reg::SPI_SHADER_PGM_LO_VS), None);
    }

    #[test]
    fn oversized_reservation_does_not_over_allocate() {
        // A guest that reserves an absurd `numdwords` (here u32::MAX) must NOT drive a
        // multi-gibibyte host allocation. The oversized reservation clears the undersized
        // guard (it is >= the packet), then reaches `emit::pad_to_reserved`, which caps the
        // fill gap at `MAX_NOP_FILL_DWORDS` (crates/gnm/src/pm4/emit.rs — derived from the AMD
        // PM4 14-bit `IT_NOP` body-count field): a `reserved` wider than one NOP can span is
        // left unpadded, so the emitter writes exactly the documented 29-dword packet and never
        // hits `Vec::with_capacity(~4.29e9)` / `handle_alloc_error`. Reaching the assertions
        // (rather than an OOM-abort of the test process) witnesses that the huge-`size` path
        // degrades to the real packet.
        let vs_regs: [u32; emit::VS_STAGE_REG_FIELDS] = [
            0x0000_2000,
            0x0000_0000,
            0x00AB_CDEF,
            0x0000_00A0,
            0x0000_0005,
            0x0000_0004,
            0x0000_00FF,
        ];
        let mut cmdbuf = [0u32; emit::SET_VS_SHADER_DWORDS];

        let _rd = wire_read(vs_regs.as_ptr() as u64, std::mem::size_of_val(&vs_regs));
        let _wr = wire_write();

        let rc = sce_gnm_set_vs_shader(
            cmdbuf.as_mut_ptr() as u64,
            u32::MAX,
            vs_regs.as_ptr() as u64,
            0,
        );
        assert_eq!(rc, 0);

        // The real 29-dword packet was written and round-trips to the same PGM regs.
        let state = decode_and_apply(&cmdbuf);
        assert_eq!(
            state.sh_regs.get(sh_reg::SPI_SHADER_PGM_LO_VS),
            Some(vs_regs[0]),
            "oversized reservation must still write the real shader-set packet"
        );
    }

    #[test]
    fn null_regs_emits_no_pm4_no_bind() {
        // A null / unmapped regs pointer must produce NO shader-set: no PM4 written,
        // read_reg_block returns None for a null regs ptr before any seam is consulted.
        // so no bogus GcnBinary{addr:0} bind is derived. The cmdbuf stays all-zero
        // and the decoder finds no SH regs (thus no shader bound at null).
        let mut cmdbuf = [0u32; emit::SET_VS_SHADER_DWORDS];
        let rc = sce_gnm_set_vs_shader(
            cmdbuf.as_mut_ptr() as u64,
            emit::SET_VS_SHADER_DWORDS as u32,
            0, // null vs_regs
            0,
        );
        assert_eq!(rc, 0);
        assert!(cmdbuf.iter().all(|&w| w == 0), "PM4 emitted for null regs");

        let state = decode_and_apply(&cmdbuf);
        assert_eq!(state.sh_regs.get(sh_reg::SPI_SHADER_PGM_LO_VS), None);
        let bound = state.derive_bound_shaders();
        assert!(bound.vs.is_none(), "a shader was bound from null regs");
    }

    /// A minimal bounded reader over host memory (host addr == guest ptr) with a single
    /// `[start, end)` region. Its `read_ranged` validates the whole `[addr, addr+size)`
    /// range against that region before copying, so a read straddling the region's end is a
    /// clean `Err` — the property `read_reg_block` relies on for an untrusted `regs`
    /// pointer. This is the exact [`ps4_core::bounded_read::BoundedRead`] shape the seam
    /// consumes; no `VirtualMemoryManager` boilerplate is needed for a read-only test.
    struct OneRegionMem {
        start: u64,
        end: u64,
    }

    impl BoundedRead for OneRegionMem {
        fn read_ranged(&self, addr: u64, size: usize) -> Result<Vec<u8>, &'static str> {
            if size == 0 {
                return Ok(Vec::new());
            }
            let range_end = addr.checked_add(size as u64).ok_or("overflow")?;
            if addr < self.start || addr >= self.end {
                return Err("start not mapped");
            }
            if range_end > self.end {
                return Err("range crosses region boundary");
            }
            let mut buf = vec![0u8; size];
            unsafe {
                std::ptr::copy_nonoverlapping(addr as *const u8, buf.as_mut_ptr(), size);
            }
            Ok(buf)
        }
    }

    #[test]
    fn out_of_bounds_regs_emits_nothing_and_never_over_reads() {
        // AC #1/#2: with a real bounds-checked source wired, a vs_regs pointer whose
        // VS_STAGE_REG_FIELDS-dword block would over-read past its mapping produces NO
        // shader-set (no PM4, no bind) instead of a SIGSEGV / adjacent-memory leak — and an
        // in-bounds block still binds, proving the VMA-checked source is actually consulted.

        // Back the "guest" with a host arena; map only its first half as a VMA. A regs block
        // placed so its tail crosses the mapped end exercises the over-read guard.
        let arena = vec![0x11u8; 256];
        let base = arena.as_ptr() as u64;
        let region_end = base + 128; // only [base, base+128) is "mapped"

        let mem: Arc<dyn BoundedRead> = Arc::new(OneRegionMem {
            start: base,
            end: region_end,
        });
        // Out-of-bounds phase: scope the override to its own block. The guard holds the
        // serialization lock and restores the prior (unwired) source when the block ends — even
        // on panic — so no raw `register` ever runs under a guard and the headless-path tests
        // still see `None`.
        {
            let _guard = registered_source().override_scoped(mem);

            // Start the block so its VS_STAGE_REG_FIELDS*4 bytes run past the mapped end.
            // read_reg_block must reject it → no PM4 emitted → no bind.
            let over_read_bytes = emit::VS_STAGE_REG_FIELDS * 4;
            let oob_regs = region_end - (over_read_bytes as u64 / 2); // straddles the boundary
            let mut cmdbuf_oob = [0u32; emit::SET_VS_SHADER_DWORDS];
            let rc = sce_gnm_set_vs_shader(
                cmdbuf_oob.as_mut_ptr() as u64,
                emit::SET_VS_SHADER_DWORDS as u32,
                oob_regs,
                0,
            );
            assert_eq!(rc, 0);
            assert!(
                cmdbuf_oob.iter().all(|&w| w == 0),
                "PM4 emitted for an out-of-bounds regs pointer (would over-read)"
            );
            let state = decode_and_apply(&cmdbuf_oob);
            assert_eq!(
                state.sh_regs.get(sh_reg::SPI_SHADER_PGM_LO_VS),
                None,
                "a shader was bound from an over-reading regs pointer"
            );
        }

        // In-bounds phase: a block wholly inside the region binds normally — proves the source
        // is consulted (not silently short-circuited to None for everything). A fresh scoped
        // override wires the second source through the RAII guard (no raw `register`), and its
        // drop restores the unwired source for the headless-path tests.
        let vs_regs: [u32; emit::VS_STAGE_REG_FIELDS] = [
            0x0000_2000,
            0x0000_0000,
            0x00AB_CDEF,
            0x0000_00A0,
            0x0000_0005,
            0x0000_0004,
            0x0000_00FF,
        ];
        let mut in_arena = vec![0u8; 256];
        for (i, w) in vs_regs.iter().enumerate() {
            in_arena[i * 4..i * 4 + 4].copy_from_slice(&w.to_le_bytes());
        }
        let in_base = in_arena.as_ptr() as u64;
        let mem2: Arc<dyn BoundedRead> = Arc::new(OneRegionMem {
            start: in_base,
            end: in_base + 256,
        });
        let _guard = registered_source().override_scoped(mem2);
        // Wire a host-backed write seam so the in-bounds emit lands in the host cmdbuf. The
        // write `Registered` is a DIFFERENT instance from the bounded-read one, so this second
        // scoped override cannot self-deadlock against `_guard` (task-138 rule).
        let _wr = wire_write();

        let mut cmdbuf_ok = [0u32; emit::SET_VS_SHADER_DWORDS];
        let rc = sce_gnm_set_vs_shader(
            cmdbuf_ok.as_mut_ptr() as u64,
            emit::SET_VS_SHADER_DWORDS as u32,
            in_base,
            0,
        );
        assert_eq!(rc, 0);
        let state = decode_and_apply(&cmdbuf_ok);
        assert_eq!(
            state.sh_regs.get(sh_reg::SPI_SHADER_PGM_LO_VS),
            Some(vs_regs[0]),
            "in-bounds regs block did not bind through the wired source"
        );

        // `_guard` restores the global to unwired on drop, so the other (headless-path) tests
        // still see `None` — no manual clear, and panic-safe.
    }
}
