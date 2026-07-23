//! Suballocating allocator + recycler for the resource cache's copy-path buffers
//! (task-223).
//!
//! Before this, every `CreateBuffer` took its own `vkCreateBuffer` + `vkAllocateMemory` +
//! `vkMapMemory`, and nothing was ever freed. In Celeste gameplay that was ~8 creations per
//! flip at roughly a millisecond each — 97% of the whole display-thread command walk — and
//! a live-allocation count that climbed for the entire run. A dedicated `VkDeviceMemory`
//! per buffer is the wrong shape twice over: drivers cap how many can exist at once
//! (`maxMemoryAllocationCount`) and each one is a kernel-visible object whose creation gets
//! slower as the count grows, so the cost per buffer rose with the age of the session.
//!
//! Here only the *memory* is pooled. A cache buffer is still its own `vk::Buffer` created
//! at exactly the size the cache asked for; what changes is that it is bound at an offset
//! inside a large, permanently mapped block instead of owning an allocation and a mapping.
//! Freed buffers hand their **region** back to a power-of-two size-class free list, so the
//! steady state costs one `vkCreateBuffer` and one `vkBindBufferMemory` per resource and no
//! device allocation at all.
//!
//! ## Why the buffer is sized exactly and not to its size class
//!
//! Every descriptor the backend writes for a cache buffer uses `VK_WHOLE_SIZE`. Rounding
//! the `vk::Buffer` itself up to its size class would silently widen every one of those
//! descriptors past the bytes the cache uploaded, turning a shader read past the end from a
//! robustness-defined zero into whatever the previous tenant of that region left behind.
//! Sizing the buffer exactly keeps the bounds identical to the one-allocation-per-buffer
//! shape this replaces; only the backing memory is shared.
//!
//! ## Reuse is safe against in-flight GPU work
//!
//! This type recycles nothing on its own — [`CacheBufferPool::recycle`] is only ever called
//! by the backend from its deferred-free queue, which it drains at the *start* of a command
//! list. The backend submits each list and waits on its fence inside the same call, so by
//! the time the next list begins, every draw that could have referenced a buffer freed
//! during the previous list has completed on the GPU. A recycled region is therefore never
//! handed out, and its `vk::Buffer` never destroyed, while a submit that reads it is
//! outstanding.
//!
//! Blocks are never unmapped or freed; they follow the leak-on-exit convention of the rest
//! of the Vulkan state.

use std::collections::HashMap;
use std::sync::atomic::Ordering::Relaxed;

use ash::vk;

use crate::present_profile;
use crate::vulkan::VulkanContext;

/// Block size carved per device allocation. Big enough that a session's whole live buffer
/// set fits in a handful of allocations, small enough that the first one is not a visible
/// startup cost.
const BLOCK_BYTES: u64 = 16 * 1024 * 1024;

/// Smallest size class. Below this the class granularity costs more in bookkeeping than the
/// bytes it would save — Celeste's constant buffers are 16 and 64 bytes.
const MIN_CLASS: u64 = 256;

/// One suballocation block: a single mapped device allocation handed out by bumping
/// `used`. Never freed or unmapped (leak-on-exit).
struct Block {
    mem: vk::DeviceMemory,
    ptr: *mut u8,
    size: u64,
    used: u64,
    memory_type: u32,
}

/// A reusable slice of a block: `class` bytes at `offset`, already mapped at `ptr`.
#[derive(Clone, Copy)]
struct Region {
    block: usize,
    offset: u64,
    class: u64,
    ptr: *mut u8,
}

/// A cache buffer carved from the pool: a `vk::Buffer` of exactly the requested size bound
/// inside a pooled region, plus the host pointer to its first byte.
#[derive(Clone, Copy)]
pub struct PooledBuffer {
    pub buffer: vk::Buffer,
    pub ptr: *mut u8,
    region: Region,
}

/// The display thread's cache-buffer allocator. Not `Send`/`Sync` by construction (raw
/// mapped pointers); it lives inside the backend, which is display-thread-owned.
#[derive(Default)]
pub struct CacheBufferPool {
    blocks: Vec<Block>,
    free: HashMap<u64, Vec<Region>>,
}

/// The size class a region of `size` bytes lands in.
///
/// Small buffers pack into shared [`BLOCK_BYTES`] blocks by power-of-two class (a little
/// slack per buffer buys reuse across many similar allocations). A buffer larger than a
/// whole block gets its own dedicated block, so rounding it up to the next power of two
/// would only waste up to ~2x host-visible memory (a 24 MB buffer → a 32 MB block) without
/// buying any packing — its class is its exact required size instead. `next_power_of_two`
/// also panics on overflow above `2^63`; `checked_next_power_of_two` returning `None` there
/// falls through to the exact-size path (the allocation itself then fails cleanly).
fn size_class(size: u64) -> u64 {
    let want = size.max(MIN_CLASS);
    match want.checked_next_power_of_two() {
        Some(pot) if pot <= BLOCK_BYTES => pot,
        _ => want,
    }
}

impl CacheBufferPool {
    /// Create a buffer of exactly `size` bytes and back it with a pooled region, reusing a
    /// recycled region of the same size class when one fits.
    ///
    /// A reused region still holds the previous resource's bytes, and the buffer covers only
    /// `size` of them. That is not stale data leaking into a draw: the resource cache pairs
    /// every `CreateBuffer` with an `UploadBuffer` covering the same range, and the one case
    /// where the upload is skipped (the guest range became unreadable) leaves the cache
    /// entry marked dirty so it retries rather than trusting the contents.
    ///
    /// # Safety
    /// `ctx`'s device must be live and owned by the calling thread.
    ///
    /// Returns `None` when the backing device allocation fails (an oversized guest
    /// descriptor under memory pressure), so the caller degrades — skipping the resource —
    /// rather than aborting the process.
    pub unsafe fn alloc(&mut self, ctx: &VulkanContext, size: u64) -> Option<PooledBuffer> {
        // SAFETY: the caller guarantees the device is live and thread-owned.
        let buffer = unsafe { ctx.create_pool_buffer(size) };
        // SAFETY: `buffer` was just created on this device.
        let reqs = unsafe { ctx.device.get_buffer_memory_requirements(buffer) };
        let align = reqs.alignment.max(1);
        let class = size_class(reqs.size);
        let region = match self.take_free(class, align, reqs.memory_type_bits) {
            Some(region) => {
                if present_profile::enabled() {
                    present_profile::POOL.recycled.fetch_add(1, Relaxed);
                }
                region
            }
            None => {
                if present_profile::enabled() {
                    present_profile::POOL.fresh.fetch_add(1, Relaxed);
                }
                match self.reserve(
                    ctx,
                    class,
                    align,
                    ctx.host_visible_memory_type(reqs.memory_type_bits),
                ) {
                    Some(region) => region,
                    None => {
                        // The block allocation failed. Destroy the unbound buffer so it does
                        // not leak, and report failure.
                        // SAFETY: `buffer` was created here and never bound.
                        unsafe { ctx.device.destroy_buffer(buffer, None) };
                        return None;
                    }
                }
            }
        };
        // SAFETY: the region is `class >= reqs.size` bytes at an `align`-multiple offset in a
        // block whose memory type satisfies `buffer`'s requirements, and no live buffer
        // overlaps it.
        unsafe {
            ctx.device
                .bind_buffer_memory(buffer, self.blocks[region.block].mem, region.offset)
                .expect("bind cache buffer to pool region");
        }
        Some(PooledBuffer {
            buffer,
            ptr: region.ptr,
            region,
        })
    }

    /// Destroy a cache buffer and return its region to the free list. See the module doc for
    /// why this is safe against in-flight GPU work.
    ///
    /// # Safety
    /// `ctx`'s device must be live and owned by the calling thread, and no submit that reads
    /// `buf` may still be outstanding.
    pub unsafe fn recycle(&mut self, ctx: &VulkanContext, buf: PooledBuffer) {
        // SAFETY: the caller guarantees the device is live and the buffer is not in flight.
        unsafe { ctx.device.destroy_buffer(buf.buffer, None) };
        self.free
            .entry(buf.region.class)
            .or_default()
            .push(buf.region);
    }

    /// Pop a free region of `class` whose offset satisfies `align` and whose block is of a
    /// memory type the buffer accepts. Every cache buffer is created through the one
    /// [`VulkanContext::create_pool_buffer`] path with identical usage flags, so in practice
    /// every candidate matches; the checks are here so that if that ever stops being true
    /// the pool carves a fresh region rather than producing an invalid bind.
    fn take_free(&mut self, class: u64, align: u64, type_bits: u32) -> Option<Region> {
        let blocks = &self.blocks;
        let regions = self.free.get_mut(&class)?;
        let i = regions.iter().rposition(|r| {
            r.offset.is_multiple_of(align) && (type_bits & (1 << blocks[r.block].memory_type)) != 0
        })?;
        Some(regions.swap_remove(i))
    }

    /// Carve a fresh `class`-byte region at an `align`-multiple offset, allocating a new
    /// block when none has room. Returns `None` when a required new block cannot be
    /// allocated (an oversized request under memory pressure).
    fn reserve(
        &mut self,
        ctx: &VulkanContext,
        class: u64,
        align: u64,
        memory_type: u32,
    ) -> Option<Region> {
        let fits = self.blocks.iter().position(|b| {
            b.memory_type == memory_type && b.used.next_multiple_of(align) + class <= b.size
        });
        let block = match fits {
            Some(i) => i,
            None => {
                // A class up to `BLOCK_BYTES` packs into a shared block; a larger class
                // (`size_class` returns its exact required size) gets a dedicated block of
                // exactly that size — no power-of-two rounding, so a >16 MB buffer does not
                // waste ~2x host-visible memory.
                let block_size = BLOCK_BYTES.max(class);
                // SAFETY: display thread owns the device; the allocation is mapped for its
                // whole lifetime and never freed. `None` on allocation failure.
                let (mem, ptr) = unsafe { ctx.allocate_mapped_block(block_size, memory_type) }?;
                self.blocks.push(Block {
                    mem,
                    ptr,
                    size: block_size,
                    used: 0,
                    memory_type,
                });
                if present_profile::enabled() {
                    let p = &present_profile::POOL;
                    p.blocks.fetch_add(1, Relaxed);
                    p.live_allocations.fetch_add(1, Relaxed);
                    p.alloc_bytes.fetch_add(block_size, Relaxed);
                }
                self.blocks.len() - 1
            }
        };
        let offset = self.blocks[block].used.next_multiple_of(align);
        self.blocks[block].used = offset + class;
        Some(Region {
            block,
            offset,
            class,
            // SAFETY: `offset` is inside the block's mapped `size` bytes.
            ptr: unsafe { self.blocks[block].ptr.add(offset as usize) },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_sizes_round_to_power_of_two_class() {
        assert_eq!(size_class(1), MIN_CLASS); // below MIN_CLASS floors to it
        assert_eq!(size_class(16), MIN_CLASS);
        assert_eq!(size_class(MIN_CLASS + 1), MIN_CLASS * 2);
        assert_eq!(size_class(BLOCK_BYTES), BLOCK_BYTES); // exactly a block
    }

    #[test]
    fn oversized_buffer_gets_exact_class_not_power_of_two() {
        // A 24 MB buffer (> 16 MB block) must not round up to a 32 MB power-of-two class:
        // its class is its exact size, so its dedicated block wastes no host-visible memory.
        let twenty_four_mb = 24 * 1024 * 1024;
        assert_eq!(size_class(twenty_four_mb), twenty_four_mb);
        assert_eq!(BLOCK_BYTES.max(size_class(twenty_four_mb)), twenty_four_mb);
    }

    #[test]
    fn class_does_not_panic_on_overflow() {
        // `next_power_of_two` panics above 2^63; `size_class` must fall through to the exact
        // size instead of aborting. The allocation itself then fails cleanly downstream.
        let huge = u64::MAX - 1;
        assert_eq!(size_class(huge), huge);
    }
}
