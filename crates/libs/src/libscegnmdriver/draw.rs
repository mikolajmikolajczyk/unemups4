//! libSceGnmDriver draw/dispatch command builders and async-compute queue stubs
//! (doc-2 §1). The draw builders are PM4 packet *emitters*: on real hardware the
//! gnmx `sceGnmDrawIndex*` functions write a fixed draw packet into the caller's
//! command buffer, which the guest later submits. An HLE-linked guest (a retail
//! managed runtime, e.g. Celeste) relies on us to write that packet — it never
//! hand-emits one. So (like the `sceGnmSetVsShader`/`SetPsShader` shader-set
//! builders in `shader_bind.rs`) these emit their `IT_DRAW_INDEX_*` PM4 into the
//! caller's `cmdbuf`, in stream order with the surrounding shader-set/state PM4, so
//! the executor's decode walk resolves the draw against exactly the state and
//! shaders bound before it (doc-6). The dispatch/async-compute builders are still
//! record-only stubs (compute path deferred).

use crate::context::NativeContext;
use ps4_gnm::driver::driver;
use ps4_gnm::pm4::emit;
use ps4_macros::ps4_syscall;
use ps4_syscalls::SyscallId;
use tracing::info;

/// Upper bound on a draw's guest-supplied `reserved` (the builder's `numdwords` arg). A single
/// PM4 draw packet's reserved slot on real gnmx is a handful of dwords (`IT_DRAW_INDEX_AUTO` is
/// 3, `IT_DRAW_INDEX_OFFSET_2` (0x35) is 4); this cap is orders of magnitude past any real draw
/// slot yet keeps a garbage `numdwords` from driving [`emit::pad_to_reserved`]'s
/// `Vec::with_capacity(reserved)` into a multi-gibibyte host allocation. 0x1000 dwords == 16 KiB.
const MAX_DRAW_RESERVED_DWORDS: usize = 0x1000;

/// Write an emitted draw PM4 packet into the caller's command buffer at `cmdbuf`,
/// honoring the caller's `reserved` dword count (the builder's `numdwords` arg). A null
/// cmdbuf or an over-small reservation is a clean no-op (never overflow adjacent guest
/// memory); a faulting write is logged and dropped. Mirrors `shader_bind::emit_into_cmdbuf`
/// — the draw and shader-set builders share the same emit-into-the-guest-cmdbuf contract.
///
/// task-166: when the guest reserved a LARGER slot than the draw packet needs, the packet is
/// padded to exactly `reserved` dwords with a trailing `IT_NOP` ([`emit::pad_to_reserved`]).
/// The retail `reserved` (numdwords) is the slot size the guest advances its cmdbuf cursor by,
/// so the untouched `reserved - pm4.len()` tail used to expose stale prior-frame bytes on a
/// reused command arena — our PM4 decode walk then mis-read that hole as real packets (a
/// phantom `SET_SH_REG`, a `Truncated header=0xffffffff` that dropped the frame tail). Filling
/// the slot makes the decoder land cleanly on the next real packet. Same discipline the
/// shader-set builder already applies to its own documented length.
///
/// task-115/task-138: routed through the SMC-tracked write seam ([`ps4_core::write_guest`])
/// rather than a raw `IdentityMem` store — the cmdbuf is guest-resident and may later be
/// executed, so an SMC-invisible store could leave a stale JIT translation. Headless (no seam
/// wired) degrades to a clean no-op — never a raw store; tests wire the write seam.
fn emit_draw_into_cmdbuf(cmdbuf: u64, reserved: u32, pm4: &[u32]) {
    if cmdbuf == 0 {
        return;
    }
    if reserved != 0 && (reserved as usize) < pm4.len() {
        info!(
            "[GNM]   draw reserved {} dwords < {} needed; skipping cmdbuf write at {:#x}",
            reserved,
            pm4.len(),
            cmdbuf
        );
        return;
    }
    // `reserved` is a guest-controlled `numdwords`; the checks above only reject an *under*-sized
    // slot. An absurdly large value (a buggy or adversarial guest passing, e.g., 0x2000_0000)
    // would drive `emit::pad_to_reserved`'s `Vec::with_capacity(reserved)`/`resize(reserved, 0)`
    // to request gibibytes and abort the process via `handle_alloc_error` — uncatchable, before
    // the write seam's range check can ever reject it. No real gnmx draw slot is anywhere near
    // this large, so drop a garbage `numdwords` as a clean no-op, like the undersized case.
    if (reserved as usize) > MAX_DRAW_RESERVED_DWORDS {
        info!(
            "[GNM]   draw reserved {} dwords > cap {}; skipping cmdbuf write at {:#x}",
            reserved, MAX_DRAW_RESERVED_DWORDS, cmdbuf
        );
        return;
    }
    // Fill the guest's reserved slot to exactly `reserved` dwords: `[draw packet][NOP]`. A
    // reserved of 0 (no slot size given) or == pm4.len() leaves the packet unchanged.
    let words = emit::pad_to_reserved(pm4, reserved);
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in &words {
        bytes.extend_from_slice(&w.to_le_bytes());
    }
    let res = match ps4_core::write_guest::write_guest() {
        Some(seam) => seam.write_bytes(cmdbuf, &bytes),
        // Headless: no seam wired → clean no-op (never a raw store).
        None => Ok(()),
    };
    if res.is_err() {
        info!("[GNM]   draw cmdbuf write faulted at {:#x}", cmdbuf);
    }
}

/// `sceGnmDrawIndex(cmdbuf, size, index_count, index_addr, flags, type)` — a PM4
/// packet *builder* (writes into the guest cmdbuf). Recorded; no PM4 emitted yet.
#[ps4_syscall(
    id = SyscallId::SCE_GNM_DRAW_INDEX,
    lib = crate::libs::LIB_SCE_GNM_DRIVER,
    name = "sceGnmDrawIndex"
)]
pub fn sce_gnm_draw_index(_cmdbuf: u64, _size: u32, index_count: u32, index_addr: u64) -> i32 {
    info!("[GNM] sceGnmDrawIndex count={}", index_count);
    if let Ok(mut drv) = driver().lock() {
        drv.draw_index(index_count, index_addr);
    }
    0
}

/// `sceGnmDrawIndexAuto(cmdbuf, size, index_count, flags)` — auto-index draw
/// packet builder. Recorded; no PM4 emitted yet.
#[ps4_syscall(
    id = SyscallId::SCE_GNM_DRAW_INDEX_AUTO,
    lib = crate::libs::LIB_SCE_GNM_DRIVER,
    name = "sceGnmDrawIndexAuto"
)]
pub fn sce_gnm_draw_index_auto(cmdbuf: u64, size: u32, index_count: u32) -> i32 {
    info!("[GNM] sceGnmDrawIndexAuto count={}", index_count);
    // Emit the DRAW_INDEX_AUTO PM4 into the caller's cmdbuf so a later submit decodes it
    // in stream order after the shader-set/state PM4 that precedes it (the executor's
    // dispatch_draw_auto arm resolves it against the bound VS/PS). The shadow record stays
    // for bookkeeping/tests; the executor consumes the PM4, not the shadow.
    let pm4 = emit::draw_index_auto(index_count);
    emit_draw_into_cmdbuf(cmdbuf, size, &pm4);
    if let Ok(mut drv) = driver().lock() {
        drv.draw_index_auto(index_count);
    }
    0
}

/// `sceGnmDrawIndexOffset(cmdbuf, size, index_offset, index_count, flags)` —
/// offset-index draw packet builder (draws `index_count` indices starting at
/// `index_offset` into the bound index buffer). Recorded; no PM4 emitted yet.
#[ps4_syscall(
    id = SyscallId::SCE_GNM_DRAW_INDEX_OFFSET,
    lib = crate::libs::LIB_SCE_GNM_DRIVER,
    name = "sceGnmDrawIndexOffset"
)]
pub fn sce_gnm_draw_index_offset(
    cmdbuf: u64,
    size: u32,
    index_offset: u32,
    index_count: u32,
) -> i32 {
    info!(
        "[GNM] sceGnmDrawIndexOffset offset={} count={}",
        index_offset, index_count
    );
    // Emit the DRAW_INDEX_OFFSET_2 (0x35) PM4 into the caller's cmdbuf; the executor's
    // dispatch_draw_index_offset arm resolves it against the bound VS/PS and index-buffer
    // state. A draw whose index base was never bound defers cleanly in the executor.
    let pm4 = emit::draw_index_offset(index_offset, index_count);
    emit_draw_into_cmdbuf(cmdbuf, size, &pm4);
    if let Ok(mut drv) = driver().lock() {
        drv.draw_index_offset(index_offset, index_count);
    }
    0
}

/// `sceGnmDispatchDirect(cmdbuf, size, x, y, z, flags)` — compute dispatch packet
/// builder. Recorded; no PM4 emitted yet.
#[ps4_syscall(
    id = SyscallId::SCE_GNM_DISPATCH_DIRECT,
    lib = crate::libs::LIB_SCE_GNM_DRIVER,
    name = "sceGnmDispatchDirect"
)]
pub fn sce_gnm_dispatch_direct(
    _cmdbuf: u64,
    _size: u32,
    threads_x: u32,
    threads_y: u32,
    threads_z: u32,
) -> i32 {
    info!(
        "[GNM] sceGnmDispatchDirect {}x{}x{}",
        threads_x, threads_y, threads_z
    );
    if let Ok(mut drv) = driver().lock() {
        drv.dispatch_direct(threads_x, threads_y, threads_z);
    }
    0
}

/// `sceGnmMapComputeQueue(pipe_id, queue_id, ring_base, ring_size_dw, read_ptr)` —
/// async compute ring map. Recorded; no queue created yet. Returns a queue id 0.
#[ps4_syscall(
    id = SyscallId::SCE_GNM_MAP_COMPUTE_QUEUE,
    lib = crate::libs::LIB_SCE_GNM_DRIVER,
    name = "sceGnmMapComputeQueue"
)]
pub fn sce_gnm_map_compute_queue(
    pipe_id: u32,
    queue_id: u32,
    _ring_base: u64,
    _ring_size_dw: u32,
    _read_ptr: u64,
) -> i32 {
    info!(
        "[GNM] sceGnmMapComputeQueue pipe={} queue={}",
        pipe_id, queue_id
    );
    if let Ok(mut drv) = driver().lock() {
        drv.map_compute_queue(pipe_id, queue_id);
    }
    0
}

/// `sceGnmDingDong(gnm_vqid, next_offs_dw)` — async compute doorbell ring.
/// Recorded; no queue driven yet.
#[ps4_syscall(
    id = SyscallId::SCE_GNM_DING_DONG,
    lib = crate::libs::LIB_SCE_GNM_DRIVER,
    name = "sceGnmDingDong"
)]
pub fn sce_gnm_ding_dong(gnm_vqid: u32, next_offs_dw: u32) -> i32 {
    info!(
        "[GNM] sceGnmDingDong vqid={} off={}",
        gnm_vqid, next_offs_dw
    );
    if let Ok(mut drv) = driver().lock() {
        drv.ding_dong(gnm_vqid, next_offs_dw);
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use ps4_gnm::pm4::decode::{self, OwnedPacket};
    use ps4_gnm::pm4::opcodes::op;
    use std::sync::Arc;

    /// A host-backed write seam (guest ptr == host addr) — copies straight into the caller's
    /// cmdbuf array so the emit path can be exercised without a real VM. Mirrors the seam in
    /// `shader_bind`'s tests.
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

    fn wire_write()
    -> ps4_core::registered::ScopeGuard<'static, dyn ps4_core::write_guest::WriteGuest> {
        ps4_core::write_guest::registered_source().override_scoped(Arc::new(TestWrite))
    }

    #[test]
    fn draw_auto_pads_oversized_reserved_slot_over_stale_bytes() {
        // task-166: sceGnmDrawIndexAuto into an over-sized reserved slot pre-filled with stale
        // 0xdeadbeef prior-frame bytes must fill the whole slot ([draw][NOP]) so a later decode
        // walk lands cleanly on the next real packet — no stale SET_*_REG / Truncated conjured
        // from the hole. Simulates a REUSED command arena (the frame-3 defect).
        let reserved = 10u32;
        // Reused arena: the slot is dirty, and a real DRAW follows it.
        let mut arena = vec![0xDEAD_BEEFu32; reserved as usize];
        arena.push(t3_header_draw());
        arena.extend_from_slice(&[7, 0]);

        let _wr = wire_write();
        let rc = sce_gnm_draw_index_auto(arena.as_mut_ptr() as u64, reserved, 3);
        assert_eq!(rc, 0);

        let mut bytes = Vec::with_capacity(arena.len() * 4);
        for w in &arena {
            bytes.extend_from_slice(&w.to_le_bytes());
        }
        let pkts = decode::decode_bytes(&bytes);
        assert_eq!(pkts.len(), 3, "expected [draw][nop][draw], got {pkts:?}");
        assert!(
            matches!(&pkts[0], OwnedPacket::Type3 { opcode, body, .. }
                if *opcode == op::IT_DRAW_INDEX_AUTO && body[0] == 3),
            "slot draw missing/wrong: {:?}",
            pkts[0]
        );
        assert!(
            matches!(&pkts[1], OwnedPacket::Type3 { opcode, .. } if *opcode == op::IT_NOP),
            "filler NOP missing: {:?}",
            pkts[1]
        );
        assert!(
            matches!(&pkts[2], OwnedPacket::Type3 { opcode, body, .. }
                if *opcode == op::IT_DRAW_INDEX_AUTO && body[0] == 7),
            "trailing draw dropped by stale-byte hole: {:?}",
            pkts[2]
        );
        assert!(
            !pkts
                .iter()
                .any(|p| matches!(p, OwnedPacket::Truncated { .. })),
            "no Truncated may appear inside the reserved slot: {pkts:?}"
        );
    }

    #[test]
    fn draw_auto_absurd_reserved_is_clean_noop() {
        // A guest passing a garbage `numdwords` (here 0x2000_0000 ≈ 512M dwords) must not drive
        // an unbounded host allocation in the emit path — `pad_to_reserved`'s
        // `Vec::with_capacity(reserved)` would request ~2 GiB and abort the process via
        // `handle_alloc_error` (uncatchable, so the handler's catch_unwind can't save it). The
        // draw is dropped as a clean no-op: the caller's cmdbuf is left byte-for-byte untouched.
        let mut arena = vec![0xDEAD_BEEFu32; 4];
        let before = arena.clone();

        let _wr = wire_write();
        let rc = sce_gnm_draw_index_auto(arena.as_mut_ptr() as u64, 0x2000_0000, 3);
        assert_eq!(rc, 0);
        assert_eq!(
            arena, before,
            "an absurd reserved must be a clean no-op — the cmdbuf stays untouched"
        );
    }

    /// The DRAW_INDEX_AUTO header the trailing-draw arena uses (header + 2 body dwords).
    fn t3_header_draw() -> u32 {
        ps4_gnm::pm4::opcodes::t3_header(op::IT_DRAW_INDEX_AUTO, 2)
    }
}
