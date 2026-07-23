//! libSceGnmDriver draw/dispatch command builders and async-compute queue stubs
//! (doc-4 §1). These are PM4 packet *builders* (they write into the guest
//! cmdbuf); here they are recorded into the `GnmDriver` shadow state only — no PM4
//! is emitted or executed yet.

use crate::context::NativeContext;
use ps4_gnm::driver::driver;
use ps4_macros::ps4_syscall;
use ps4_syscalls::SyscallId;
use tracing::info;

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
pub fn sce_gnm_draw_index_auto(_cmdbuf: u64, _size: u32, index_count: u32) -> i32 {
    info!("[GNM] sceGnmDrawIndexAuto count={}", index_count);
    if let Ok(mut drv) = driver().lock() {
        drv.draw_index_auto(index_count);
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
