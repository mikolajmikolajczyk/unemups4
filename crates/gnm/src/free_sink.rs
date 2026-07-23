//! The guest free/unmap → resource-cache eviction bridge (doc-2 §8).
//!
//! Impls [`MemoryFreeSink`] (declared Vulkan-free in `ps4-core`) over the driver-owned
//! [`ResourceCache`](crate::cache::ResourceCache). The kernel memory manager fires
//! [`MemoryFreeSink::notify_free`] on `munmap`/`sceKernelReleaseDirectMemory`; this drops
//! every cache entry keyed on the freed range (so a free+realloc of the same address mints
//! a fresh id instead of a stale-id clean hit), unwatches it, and ships the resulting
//! [`BackendCmd::FreeResource`] teardown across the display channel so the backend frees or
//! revokes the vk resource. The kernel reaches this without depending on `ps4-gnm` — the
//! impl registers itself at boot through `ps4_core::gpu::register_memory_free_sink`,
//! exactly like the present sink.

use ps4_core::gpu::{BackendCmd, MemoryFreeSink};

use crate::driver::driver;

/// Bridges a guest free/unmap into resource-cache eviction + backend teardown. Zero-sized:
/// it reaches the driver-owned cache through the process-global `driver()` singleton and
/// the display channel through the process-global present sink, both wired at boot.
#[derive(Default)]
pub struct GnmMemoryFreeSink;

impl GnmMemoryFreeSink {
    pub fn new() -> GnmMemoryFreeSink {
        GnmMemoryFreeSink
    }
}

impl MemoryFreeSink for GnmMemoryFreeSink {
    fn notify_free(&self, addr: u64, size: u64) {
        // A zero-length free touches nothing (matches `ranges_overlap`'s half-open rule);
        // skip the lock + channel round-trip entirely.
        if size == 0 {
            return;
        }
        // The dirty source is the same seam `ResourceCache::get` watched the range through
        // (doc-2 §8.3); `free_range` unwatches the freed range against it. When none is
        // wired (headless), there is nothing to unwatch — evict + emit teardown anyway.
        let dirty = ps4_core::dirty::dirty_source();
        let mut cmds: Vec<BackendCmd> = Vec::new();
        {
            // Runs on the guest thread (the kernel munmap handler), so taking the driver
            // lock is correct — the display thread must never take it (deadlock invariant,
            // see `driver()`), but a guest-thread caller may. Held only across the cache
            // eviction, not across the channel send below.
            let mut d = driver().lock().unwrap_or_else(|e| e.into_inner());
            match dirty.as_deref() {
                Some(src) => d.free_resource_range(addr, size, src, &mut cmds),
                None => {
                    let noop = NoopDirty;
                    d.free_resource_range(addr, size, &noop, &mut cmds);
                }
            }
        }
        if cmds.is_empty() {
            return;
        }
        // Ship the teardown to the display thread over the existing command-list channel
        // (Vulkan-free — the commands are plain data). Headless (no sink) drops them: there
        // is no backend holding a vk resource to free, so eviction from the guest-side map
        // above is the whole job. Blocks on the channel like a submit; the driver lock was
        // released above, so this does not hold it across the display round-trip.
        if let Some(sink) = ps4_core::gpu::present_sink() {
            sink.run_command_list(&cmds);
        }
    }
}

/// A no-op [`DirtySource`](ps4_core::dirty::DirtySource) used when none is registered, so
/// `free_range` has a seam to `unwatch` against without special-casing the `None` path.
/// Never watches, so its `unwatch` is a no-op and `take_dirty` is empty.
struct NoopDirty;

impl ps4_core::dirty::DirtySource for NoopDirty {
    fn watch(&self, _addr: u64, _size: u64) {}
    fn unwatch(&self, _addr: u64, _size: u64) {}
    fn take_dirty(&self) -> Vec<(u64, u64)> {
        Vec::new()
    }
}
