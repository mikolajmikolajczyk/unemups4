//! Software VMA manager over the identity-mapped [`GuestVm`] arena (x86jit backend).
//!
//! `VmMemoryManager` implements the [`VirtualMemoryManager`] trait, but the whole guest
//! arena `[GUEST_BASE, span)` is *already* host-mapped once by [`GuestVm::new`] (a single
//! `MAP_NORESERVE` `mmap`).
//! So map/unmap here are pure **software bookkeeping** over a `BTreeMap` of VMAs — no
//! per-region host `mmap`/`munmap`/`mprotect` — plus the two side effects that still
//! matter physically:
//!
//! * `unmap` issues `madvise(MADV_DONTNEED)` on the page-aligned covered subrange to
//!   drop the region's RSS (untouched pages of a `NORESERVE` mapping cost no physical
//!   memory; the next touch faults in a fresh zero page).
//! * `map` zero-fills the requested range through [`GuestVm::write_bytes`]. Because the
//!   arena is mapped exactly once and reused, a region mapped *after* a previous dirty
//!   `unmap` (without a covering `madvise`, or before the next fault) could otherwise
//!   expose stale bytes. A per-region `MAP_ANONYMOUS` mapping would always hand back
//!   zeroed pages; over the shared pre-mapped arena we preserve that guarantee explicitly.
//!
//! All guest writes/reads route through `GuestVm::{write_bytes,read_bytes}` so x86jit's
//! SMC / code-page dirty tracking observes every embedder write.

use std::sync::Arc;

use ps4_core::memory::{MemoryProtection, MemoryVma, VirtualMemoryManager};
use ps4_cpu::GuestVm;
use tracing::{debug, warn};

/// Software VMA manager backed by the shared identity `GuestVm` arena.
pub struct VmMemoryManager {
    /// Shared, already-mapped guest arena. Writes/reads go through this so SMC tracking
    /// sees them; `unmap`'s `madvise` uses the identity pointer (host addr == guest addr).
    vm: Arc<GuestVm>,
    /// Live regions keyed by start address.
    allocations: std::collections::BTreeMap<u64, MemoryVma>,
    /// "Allocate anywhere" cursor. Starts at `0x4_0000_0000` (17 GiB) — the historical
    /// heap base the loader climbs from; the 64 GiB span comfortably covers it.
    heap_cursor: u64,
    /// Every arena range that has EVER been mapped, merged and non-overlapping (start -> end).
    ///
    /// Used to skip the zero-fill on virgin address space. The arena is one anonymous
    /// `MAP_NORESERVE` mapping, so a page nothing has ever mapped is untouched host memory
    /// and therefore already zero; only a range that once held a mapping can hold stale
    /// bytes. That distinction is what makes a bulk reservation cheap — a native allocator
    /// reserves megabytes at a time and zeroing them is pure waste.
    ///
    /// A single high-water mark is NOT enough, which is worth recording because it looks
    /// sufficient: `sceKernelMapDirectMemory` maps at the 36 GiB pool base, which would put
    /// the mark above every ordinary allocation and make each later map look previously-used.
    /// The address space is occupied in disjoint zones, so the set has to be too.
    ever_mapped: std::collections::BTreeMap<u64, u64>,
}

impl VmMemoryManager {
    /// Wrap a shared [`GuestVm`]. Reserves the HLT gadget page as a permanent VMA so
    /// runtime maps can never collide with it (see [`Self::map`]).
    pub fn new(vm: Arc<GuestVm>) -> Self {
        let gadget = vm.gadget_addr();
        let mut allocations = std::collections::BTreeMap::new();
        // The gadget page (single `hlt` byte at `GADGET_ADDR`) is written by
        // `GuestVm::new` and must survive for the process lifetime. Reserve the whole
        // 4 KiB page as a permanent, unremovable VMA: `is_memory_free` then rejects any
        // runtime map overlapping it, and `unmap` of it warns (nothing to `madvise`-clear
        // for a page we never want zeroed). We do NOT zero this page — it holds the gadget.
        allocations.insert(
            gadget,
            MemoryVma {
                start: gadget,
                end: gadget + 0x1000,
                size: 0x1000,
                protection: MemoryProtection::READ | MemoryProtection::EXEC,
                name: "hlt_gadget".to_string(),
            },
        );
        VmMemoryManager {
            vm,
            allocations,
            heap_cursor: 0x400000000,
            // The gadget page is written by `GuestVm::new`, so it counts as already used.
            ever_mapped: std::collections::BTreeMap::from([(gadget, gadget + 0x1000)]),
        }
    }

    /// The sub-ranges of `[start, end)` that some earlier mapping already covered, and so
    /// may hold stale guest bytes. Virgin arena space is omitted — it has never been written
    /// and an anonymous mapping starts zeroed.
    fn previously_mapped_parts(&self, start: u64, end: u64) -> Vec<(u64, u64)> {
        let mut parts = Vec::new();
        // The interval starting at or below `start` is the only earlier one that can reach
        // into the range; everything after it is found by walking forward until past `end`.
        let first = self
            .ever_mapped
            .range(..=start)
            .next_back()
            .filter(|(_, e)| **e > start)
            .map(|(&s, _)| s)
            .unwrap_or(start);
        for (&s, &e) in self.ever_mapped.range(first..) {
            if s >= end {
                break;
            }
            let lo = s.max(start);
            let hi = e.min(end);
            if hi > lo {
                parts.push((lo, hi));
            }
        }
        parts
    }

    /// Record `[start, end)` as used, merging into any intervals it touches so the set stays
    /// small however many times a title maps and unmaps.
    fn note_ever_mapped(&mut self, start: u64, end: u64) {
        let mut lo = start;
        let mut hi = end;
        // Absorb every interval that overlaps or abuts the new one.
        let overlapping: Vec<u64> = self
            .ever_mapped
            .range(..=end)
            .filter(|(_, e)| **e >= start)
            .map(|(&s, _)| s)
            .collect();
        for s in overlapping {
            if let Some(e) = self.ever_mapped.remove(&s) {
                lo = lo.min(s);
                hi = hi.max(e);
            }
        }
        self.ever_mapped.insert(lo, hi);
    }

    /// Zero `[start, end)` through the VM in bounded chunks, so a multi-megabyte fill never
    /// allocates a host buffer of that size.
    fn zero_range(&self, start: u64, end: u64) -> Result<(), &'static str> {
        const ZERO_CHUNK: usize = 1 << 20; // 1 MiB
        let total = (end - start) as usize;
        let zeros = vec![0u8; total.min(ZERO_CHUNK)];
        let mut done = 0usize;
        while done < total {
            let n = (total - done).min(ZERO_CHUNK);
            if self
                .vm
                .write_bytes(start + done as u64, &zeros[..n])
                .is_err()
            {
                return Err("zero-fill of fresh map failed");
            }
            done += n;
        }
        Ok(())
    }

    /// Borrow the shared VM (e.g. to hand the same `Arc` to the execution core).
    pub fn vm(&self) -> &Arc<GuestVm> {
        &self.vm
    }

    /// The VMA containing `addr`, if any. The whole guest arena `[guest_base, span)` is
    /// host-mapped once, so `get_host_ptr`/`read_bytes` succeed for *any* in-arena
    /// address regardless of whether a region was ever `map`ped there — this is the only
    /// method that answers "is this address inside a region the guest actually mapped?".
    fn containing_vma(&self, addr: u64) -> Option<&MemoryVma> {
        // VMAs are keyed by start; the last one starting at/below `addr` is the only
        // candidate that can contain it.
        self.allocations
            .range(..=addr)
            .next_back()
            .map(|(_, vma)| vma)
            .filter(|vma| vma.contains(addr))
    }

    /// A VMA-aware read view onto this manager for **untrusted guest addresses**. Its
    /// [`VirtualMemoryManager::read_bytes`] is the range-validated read
    /// ([`VmMemoryManager::read_bytes_ranged`]), so a read that straddles a VMA boundary
    /// or an unmapped page is rejected instead of over-reading. This is the seam the
    /// shader-address path passes into `parse_sb` — the bare identity view's unbounded
    /// `read_bytes` must not be used for register-derived shader pointers.
    pub fn shader_read_view(&self) -> VmaBoundedView<'_> {
        VmaBoundedView { inner: self }
    }
}

/// A read-only, VMA-bounds-checked view onto a [`VmMemoryManager`]. Delegates every
/// method to the inner manager **except** `read_bytes`, which routes to the
/// range-validated [`VmMemoryManager::read_bytes_ranged`] so that reads over an untrusted
/// address cannot over-read past the mapping that starts there.
///
/// Parsers that walk untrusted guest data (the `.sb` shader parser) take a
/// `&dyn VirtualMemoryManager` and read through `read_bytes`; handing them this view makes
/// every such read fault cleanly at a VMA boundary. See
/// [`VmMemoryManager::shader_read_view`].
pub struct VmaBoundedView<'a> {
    inner: &'a VmMemoryManager,
}

impl VirtualMemoryManager for VmaBoundedView<'_> {
    fn map(
        &mut self,
        _addr: u64,
        _size: usize,
        _prot: MemoryProtection,
        _name: Option<&str>,
    ) -> Result<u64, &'static str> {
        Err("VmaBoundedView is read-only")
    }
    fn unmap(&mut self, _addr: u64, _size: usize) -> Result<(), &'static str> {
        Err("VmaBoundedView is read-only")
    }
    fn protect(
        &mut self,
        _addr: u64,
        _size: usize,
        _prot: MemoryProtection,
    ) -> Result<(), &'static str> {
        Err("VmaBoundedView is read-only")
    }
    unsafe fn get_host_ptr(&self, addr: u64) -> Option<*mut u8> {
        unsafe { self.inner.get_host_ptr(addr) }
    }
    fn find_free_region(&mut self, _size: usize) -> u64 {
        0
    }
    fn is_memory_free(&self, addr: u64, size: usize) -> bool {
        self.inner.is_memory_free(addr, size)
    }
    fn describe_fault_context(&self, addr: u64) -> String {
        self.inner.describe_fault_context(addr)
    }
    /// The whole point of the view: an untrusted read is range-validated against the VMA
    /// set, so a straddling / unmapped read is a clean `Err` (never an over-read).
    fn read_bytes(&self, addr: u64, size: usize) -> Result<Vec<u8>, &'static str> {
        self.inner.read_bytes_ranged(addr, size)
    }
    fn read_bytes_ranged(&self, addr: u64, size: usize) -> Result<Vec<u8>, &'static str> {
        self.inner.read_bytes_ranged(addr, size)
    }
}

impl VirtualMemoryManager for VmMemoryManager {
    fn map(
        &mut self,
        addr: u64,
        size: usize,
        prot: MemoryProtection,
        name: Option<&str>,
    ) -> Result<u64, &'static str> {
        self.map_aligned(addr, size, 0, prot, name)
    }

    fn map_aligned(
        &mut self,
        mut addr: u64,
        size: usize,
        align: usize,
        prot: MemoryProtection,
        name: Option<&str>,
    ) -> Result<u64, &'static str> {
        if size == 0 {
            return Err("zero-length map");
        }

        if addr == 0 {
            // "Allocate anywhere" — `find_free_region_aligned` picks a slot above
            // `heap_cursor`, rounded up to `align` (SGen LOS asks for 1 MB-aligned
            // sections; a mis-aligned base makes its `chunk & ~0xfffff` section math read
            // garbage). `align == 0` keeps the default 16 KB placement.
            //
            // That search only checks each candidate against the live VMA set, never the
            // arena top, so it can hand back a base whose region runs past `span`. A region
            // can never exceed the whole backed arena `[guest_base, span)`; reject a `size`
            // that large up front — this also forecloses the `addr + size` wrap the search's
            // own arithmetic would suffer for a `size` near `u64::MAX`. Then hold the chosen
            // base to the SAME fit invariant the explicit branch enforces below (no
            // `base + size` overflow, and `base + size <= span`), returning the identical
            // errors — otherwise a large `len` yields `Ok(base)` for a region past the arena
            // and the caller's `from_raw_parts_mut(base, len).fill(0)` writes off its end.
            if size as u64 > self.vm.span().saturating_sub(self.vm.guest_base()) {
                return Err("map beyond arena span");
            }
            addr = self.find_free_region_aligned(size, align);
            let end = match addr.checked_add(size as u64) {
                Some(e) => e,
                None => return Err("map size overflow"),
            };
            if end > self.vm.span() {
                return Err("map beyond arena span");
            }
        } else {
            // Explicit placement: must sit fully inside the backed arena and not collide.
            if addr < self.vm.guest_base() {
                return Err("map below guest_base");
            }
            let end = match addr.checked_add(size as u64) {
                Some(e) => e,
                None => return Err("map size overflow"),
            };
            if end > self.vm.span() {
                return Err("map beyond arena span");
            }
            if !self.is_memory_free(addr, size) {
                return Err("Memory collision");
            }
        }

        // No host mmap — the arena is already backed. Zero the range so a fresh map never
        // exposes bytes left dirty by a prior region (MAP_ANONYMOUS parity). Routed
        // through the VM so SMC tracking sees the zeroing write.
        //
        // In CHUNKS, over one reusable buffer. This used to be `vec![0u8; size]`, which is
        // fine while every mapping is a few pages and catastrophic once a title reserves
        // address space in bulk: a native allocator asks for hundreds of megabytes at a
        // time through `sceKernelReserveVirtualRange`, so the emulator allocated a host
        // buffer of that size, on the heap, purely to copy zeroes out of it. Measured at
        // 16 ms per reserve call — the most expensive syscall in the whole run — and a
        // plausible source of the host-side heap corruption seen on exit.
        let map_end = addr + size as u64;
        for (zs, ze) in self.previously_mapped_parts(addr, map_end) {
            self.zero_range(zs, ze)?;
        }
        self.note_ever_mapped(addr, map_end);

        let vma = MemoryVma {
            start: addr,
            end: addr + size as u64,
            size,
            protection: prot,
            name: name
                .map(|s| s.to_string())
                .unwrap_or_else(|| "dynamic_alloc".to_string()),
        };

        self.allocations.insert(addr, vma);

        Ok(addr)
    }

    fn unmap(&mut self, addr: u64, size: usize) -> Result<(), &'static str> {
        // Free the physical pages: `madvise(MADV_DONTNEED)` on the page-aligned subrange
        // fully covered by `[addr, addr+size)`. Untouched-after this, the next access
        // faults a fresh zero page (matching `munmap` + fresh `mmap` RSS behavior). We
        // only advise the *aligned interior* so a partial edge page of a neighbouring
        // still-mapped region is never accidentally discarded.
        //
        // task-148: EXCEPT inside the direct-memory pool window. Mono's `mono_vfree` `munmap`s a
        // VA sub-range that lands in the *middle* of a still-live larger direct-memory region
        // (it manages its own sub-chunks of one big mapping and frees them individually).
        // `madvise(DONTNEED)` on that interior would zero pages Mono still references (its
        // GC/heap sub-chunks live in the same region), corrupting the heap and tripping its
        // `mono-mmap-orbis.c:219 g_assert(res==0)` nondeterministically. The old identity model
        // never zeroed freed direct memory either. Since direct-memory offsets are never reused,
        // keeping the pages resident costs only RSS, not correctness — so we skip the
        // page-discard for any unmap inside the pool and do the VMA-tracking removal only.
        let page = 0x1000u64;
        let start = addr;
        let end = addr.saturating_add(size as u64);
        let pool_base = ps4_core::kernel::DIRECT_MEMORY_POOL_BASE;
        let pool_end = pool_base + ps4_core::kernel::DIRECT_MEMORY_POOL_SIZE;
        let in_direct_memory_pool = start < pool_end && end > pool_base;
        // `addr` is guest-controlled (kernel `munmap` passes it through with no range check), so an
        // `addr` in `(u64::MAX - page + 1, u64::MAX]` has no page-multiple `>= addr` that fits u64.
        // `next_multiple_of` panics on that overflow in an overflow-checked build; the checked form
        // saturates to `u64::MAX` instead, which fails the `aligned_end > aligned_start` guard below
        // (no aligned end can exceed it) and skips the discard for an out-of-range address.
        let aligned_start = start.checked_next_multiple_of(page).unwrap_or(u64::MAX);
        let aligned_end = end & !(page - 1);
        if !in_direct_memory_pool && aligned_end > aligned_start {
            // Never discard the permanent HLT gadget page (see `VmMemoryManager::new`). A
            // guest `munmap` whose page-aligned interior covers `[gadget, gadget+page)` would
            // otherwise `MADV_DONTNEED` it, dropping the resident `GADGET_BYTE` to a fresh zero
            // page. The run loop pushes `gadget` as the sentinel return address; once its byte
            // reads `00` instead of `hlt`, exec.rs never breaks with the guest-return HLT and
            // the CPU runs off into garbage. The VMA-removal filter below already spares the
            // gadget's VMA — mirror that here for the physical page. Advise *around* it: the
            // gadget is exactly one page, so the discard splits into an up-to-two-part range
            // that excludes it, collapsing to the original single call when it isn't covered.
            let gadget = self.vm.gadget_addr();
            let gadget_end = gadget + page;
            for (adv_start, adv_end) in [
                (aligned_start, aligned_end.min(gadget)),
                (aligned_start.max(gadget_end), aligned_end),
            ] {
                if adv_end <= adv_start {
                    continue;
                }
                let len = (adv_end - adv_start) as usize;
                // SAFETY: identity mapping means host addr == guest addr; `[adv_start,
                // adv_end)` is a page-aligned subrange of the arena that `GuestVm` mapped.
                // MADV_DONTNEED on an anonymous private mapping only drops pages (next touch =
                // zero), never unmaps, so the arena stays valid.
                let ret = unsafe {
                    libc::madvise(adv_start as *mut libc::c_void, len, libc::MADV_DONTNEED)
                };
                if ret != 0 {
                    warn!(
                        "madvise(MADV_DONTNEED) on 0x{:x}..0x{:x} failed: {}",
                        adv_start,
                        adv_end,
                        std::io::Error::last_os_error()
                    );
                }
            }
        }

        // task-148: an unmap inside the direct-memory pool touches NOTHING — not the pages
        // (skipped above) and not the VMA tracking. Mono `munmap`s a sub-range in the middle of
        // a still-live direct-memory region; dropping that region's VMA would let a later
        // `MapDirectMemory` (which zero-fills a *fresh* collision-free map) re-zero the whole
        // region under Mono's feet. Direct-memory offsets are never reused, so the region's VMA
        // is legitimately permanent for the process lifetime (mirroring the identity model,
        // which never truly freed direct memory). Leave it tracked and return success.
        if in_direct_memory_pool {
            return Ok(());
        }

        // Drop VMA tracking only for regions FULLY CONTAINED in `[addr, addr+size)`. Mono
        // (`mono_vfree` / mono-mmap-orbis) frees sub-ranges and guard pages that begin partway
        // inside a larger VMA; if we evicted every *overlapping* VMA, an interior/partial munmap
        // of a larger region would drop that whole region's VMA while its other pages stay
        // resident and live. `is_memory_free` would then go blind and a later "allocate anywhere"
        // mmap could alias live memory. Over-tracking is safe (a partially-overlapped VMA that
        // stays is at worst a benign leak); under-tracking aliases — so a VMA that only partially
        // overlaps STAYS tracked. POSIX `munmap` semantics: unmapping an untracked, partial, or
        // already-unmapped range is NOT an error — it succeeds (returns 0). Only genuinely invalid
        // params (validated at the syscall/param layer) fail with EINVAL. So we always return
        // `Ok(())` here; a range that fully contains nothing removes nothing. Erroring on an
        // untracked free made Mono's `mono-mmap-orbis.c:219 res == 0` assert fire → guest
        // `abort()` (task-146).
        let unmap_end = end; // saturating end computed above
        let gadget = self.vm.gadget_addr();
        let overlapping: Vec<u64> = self
            .allocations
            .range(..unmap_end)
            // Full-containment test: VMA `[vma.start, vma.end)` lies entirely within `[start,
            // end)`. Never evict the permanent HLT gadget page: it must survive the whole process
            // lifetime (see `VmMemoryManager::new`), so any unmap keeps its VMA.
            .filter(|(key, vma)| **key != gadget && vma.start >= start && vma.end <= unmap_end)
            .map(|(key, _)| *key)
            .collect();
        if overlapping.is_empty() {
            // Common and benign: Mono frees sub-ranges / guard pages we never tracked as
            // distinct VMAs. Keep this quiet (it floods) — it is not an error.
            debug!(
                "Unmapping range not tracked in VMA: 0x{:x}..0x{:x}",
                start, unmap_end
            );
        } else {
            for key in overlapping {
                self.allocations.remove(&key);
            }
        }
        Ok(())
    }

    fn protect(
        &mut self,
        addr: u64,
        size: usize,
        prot: MemoryProtection,
    ) -> Result<(), &'static str> {
        // Tracking-only: the whole arena is pre-mapped RWX by `GuestVm::new` and guest
        // code runs under the JIT with `Fast` consistency, so no host `mprotect` is issued
        // (this matches the native path's *effective* behavior today — the loader mapped
        // segments RWX and never trapped on prot). We record the intent in the VMA for
        // diagnostics; enforcement (guard pages / real prot) is a deferred follow-up.
        if let Some(vma) = self.allocations.get_mut(&addr) {
            if vma.size == size {
                vma.protection = prot;
            } else {
                warn!("Partial protect not fully supported in VMA tracker yet (different sizes)");
            }
        } else {
            warn!(
                "protect on an untracked region 0x{:x} (different start address and sizes)",
                addr
            );
        }
        Ok(())
    }

    unsafe fn get_host_ptr(&self, addr: u64) -> Option<*mut u8> {
        // Identity: host addr == guest addr, but only inside the backed arena. Outside
        // `[guest_base, span)` there is no mapping, so return None rather than a wild ptr.
        if addr >= self.vm.guest_base() && addr < self.vm.span() {
            Some(addr as *mut u8)
        } else {
            None
        }
    }

    fn find_free_region(&mut self, size: usize) -> u64 {
        self.find_free_region_aligned(size, 0)
    }

    fn find_free_region_aligned(&mut self, size: usize, align: usize) -> u64 {
        // Base placement granularity is 16 KB; a caller-requested `align` (power of two)
        // that exceeds it wins so a guest allocator's pointer-masking metadata math holds
        // (e.g. SGen LOS needs 1 MB-aligned sections). `align <= 1` means "no extra
        // alignment" → the default 16 KB step.
        let align = (align as u64).max(0x4000);

        let pool_base = ps4_core::kernel::DIRECT_MEMORY_POOL_BASE;
        let pool_end = pool_base + ps4_core::kernel::DIRECT_MEMORY_POOL_SIZE;
        loop {
            let addr = (self.heap_cursor + (align - 1)) & !(align - 1);
            // task-148: never hand out an "allocate anywhere" region overlapping the
            // direct-memory pool window `[POOL_BASE, POOL_BASE+POOL_SIZE)`. That window is a
            // fixed VA range owned by the physical-offset pool (`va = POOL_BASE + phys_off`);
            // if the climbing cursor wandered into it a plain anonymous/flexible map would
            // collide with a future direct-memory map. Skip to the pool top instead.
            let end = addr + size as u64;
            if addr < pool_end && end > pool_base {
                self.heap_cursor = pool_end;
                continue;
            }
            if self.is_memory_free(addr, size) {
                self.heap_cursor = addr + size as u64;
                return addr;
            }
            self.heap_cursor += align;
        }
    }

    fn is_memory_free(&self, addr: u64, size: usize) -> bool {
        let end = addr + size as u64;
        for vma in self.allocations.values() {
            if addr < vma.end && end > vma.start {
                return false;
            }
        }
        true
    }

    fn query_region(&self, addr: u64, find_next: bool) -> Option<MemoryVma> {
        if let Some(vma) = self.containing_vma(addr) {
            return Some(vma.clone());
        }
        if find_next {
            // Nearest region starting strictly above `addr` (the containing case already
            // covered `start <= addr < end`).
            return self
                .allocations
                .range(addr..)
                .next()
                .map(|(_, vma)| vma.clone());
        }
        None
    }

    fn describe_fault_context(&self, addr: u64) -> String {
        let base = self.vm.guest_base();
        let span = self.vm.span();

        // First: is the address even inside the backed identity arena? An address
        // below `guest_base` is the classic null-adjacent deref; one at/above `span`
        // (or that a host pointer would land at) never had a mapping at all — a strong
        // signal a host pointer leaked into the guest.
        if addr < base {
            return format!(
                "address {addr:#x} is below guest_base ({base:#x}) — a null / \
                 low-address dereference (nothing is ever mapped in [0, {base:#x}))",
            );
        }
        if addr >= span {
            return format!(
                "address {addr:#x} is at/above the arena span ({span:#x}) — outside \
                 [guest_base {base:#x}, span {span:#x}); a host pointer leaked to the \
                 guest?",
            );
        }

        // Inside the arena. Report the containing VMA, or — in a gap between regions —
        // the nearest region below and above so the fault is placed relative to the
        // known layout.
        if let Some(vma) = self.containing_vma(addr) {
            return format!(
                "inside VMA \"{}\" [{:#x}, {:#x}) prot={:?} (offset +{:#x})",
                vma.name,
                vma.start,
                vma.end,
                vma.protection,
                addr - vma.start,
            );
        }

        // Unmapped gap inside the arena: find the nearest region ending at/below addr
        // (preceding) and the nearest starting above addr (following).
        let preceding = self
            .allocations
            .values()
            .filter(|v| v.end <= addr)
            .max_by_key(|v| v.end);
        let following = self
            .allocations
            .values()
            .filter(|v| v.start > addr)
            .min_by_key(|v| v.start);
        let prev = match preceding {
            Some(v) => format!(
                "after \"{}\" [{:#x}, {:#x}) (gap +{:#x})",
                v.name,
                v.start,
                v.end,
                addr - v.end
            ),
            None => "no mapped region below".to_string(),
        };
        let next = match following {
            Some(v) => format!(
                "before \"{}\" [{:#x}, {:#x}) (gap +{:#x})",
                v.name,
                v.start,
                v.end,
                v.start - addr
            ),
            None => "no mapped region above".to_string(),
        };
        format!("in an unmapped gap: {prev}; {next}")
    }

    fn write_bytes(&self, addr: u64, data: &[u8]) -> Result<(), &'static str> {
        // Route through the VM so x86jit SMC / code-page dirty tracking observes the write
        // (loader relocations, handler-written data, …). Do NOT raw-memcpy the identity
        // pointer — that would bypass invalidation.
        self.vm
            .write_bytes(addr, data)
            .map_err(|_| "guest write_bytes failed")
    }

    fn read_bytes(&self, addr: u64, size: usize) -> Result<Vec<u8>, &'static str> {
        let mut buffer = vec![0u8; size];
        self.vm
            .read_bytes(addr, &mut buffer)
            .map_err(|_| "guest read_bytes failed")?;
        Ok(buffer)
    }

    fn read_bytes_ranged(&self, addr: u64, size: usize) -> Result<Vec<u8>, &'static str> {
        // A zero-length read touches nothing and is trivially in-bounds.
        if size == 0 {
            return Ok(Vec::new());
        }
        // The whole arena `[guest_base, span)` is host-mapped once, so `read_bytes` alone
        // would succeed for any in-arena address — even one in an unmapped VMA gap — and a
        // read straddling the arena top would over-read into raw host memory. Validate the
        // *entire* `[addr, addr+size)` range against the VMA set first: every byte must be
        // backed by a mapped region. The range MAY span several regions as long as they are
        // contiguous (each next region starts exactly where the previous ends, no gap) — a
        // read straddling two back-to-back mmaps (e.g. code + read-only data) is legitimate.
        // Reject only when a byte falls in an unmapped gap or past the last mapping.
        let end = addr
            .checked_add(size as u64)
            .ok_or("read range overflows the address space")?;
        // Walk contiguous adjacent VMAs starting at `addr`. `covered` is the highest
        // address proven backed so far; advance it region by region until it reaches `end`.
        let mut covered = addr;
        while covered < end {
            let vma = self
                .containing_vma(covered)
                .ok_or("read range enters unmapped memory (gap or past the last mapping)")?;
            // `contains(covered)` held, so `vma.end > covered`: this strictly advances,
            // and the next iteration probes exactly `vma.end` — the start of the next
            // region, which must be mapped and contiguous, or the read faults there.
            covered = vma.end;
        }
        self.read_bytes(addr, size)
    }

    fn write_bytes_ranged(&self, addr: u64, data: &[u8]) -> Result<(), &'static str> {
        // The SMC-tracked write mirror of `read_bytes_ranged`. A zero-length write touches
        // nothing and is trivially in-bounds.
        if data.is_empty() {
            return Ok(());
        }
        // The whole arena `[guest_base, span)` is host-mapped once, so the raw `write_bytes`
        // alone would succeed for any in-arena address — even one in an unmapped VMA gap — and
        // a write straddling the arena top would store into raw host memory. Validate the
        // *entire* `[addr, addr+len)` range against the VMA set first, exactly as
        // `read_bytes_ranged` does: every byte must be backed by a mapped region. The range
        // MAY span several regions as long as they are contiguous (each next region starts
        // exactly where the previous ends). Reject only when a byte falls in an unmapped gap
        // or past the last mapping.
        let end = addr
            .checked_add(data.len() as u64)
            .ok_or("write range overflows the address space")?;
        let mut covered = addr;
        while covered < end {
            let vma = self
                .containing_vma(covered)
                .ok_or("write range enters unmapped memory (gap or past the last mapping)")?;
            // `contains(covered)` held, so `vma.end > covered`: this strictly advances, and
            // the next iteration probes exactly `vma.end` — the start of the next region,
            // which must be mapped and contiguous, or the write faults there.
            covered = vma.end;
        }
        // Fully mapped: store through the SAME SMC-tracked path `write_bytes` uses so x86jit's
        // code-page dirty tracking observes the write (an out-param to a page later executed).
        self.write_bytes(addr, data)
    }

    fn zero_memory(&self, addr: u64, size: usize) -> Result<(), &'static str> {
        // Route zeroing through the VM too (SMC tracking parity with `write_bytes`).
        let zeros = vec![0u8; size];
        self.vm
            .write_bytes(addr, &zeros)
            .map_err(|_| "guest zero_memory failed")
    }
}

#[cfg(test)]
mod tests {
    use super::VmMemoryManager;
    use ps4_core::memory::{MemoryProtection, VirtualMemoryManager};
    use ps4_cpu::GuestVm;

    /// A small arena whose top sits just 1 MiB above the "allocate anywhere" heap cursor
    /// (`0x4_0000_0000`), so a modest `len` overruns the remaining span deterministically —
    /// and far below the direct-memory pool window (36 GiB), which therefore never skews
    /// placement here.
    fn small_vm() -> std::sync::Arc<GuestVm> {
        GuestVm::new(0x4_0010_0000)
    }

    fn rw() -> MemoryProtection {
        MemoryProtection::READ | MemoryProtection::WRITE
    }

    /// `GuestVm::new` maps the guest arena at a FIXED host address (`MAP_FIXED_NOREPLACE` at
    /// `GUEST_BASE`), so at most one live `GuestVm` may exist in the process at a time. Each
    /// test below builds its own, and cargo runs them on parallel threads in one binary — so
    /// serialize them through this lock. Acquire it *before* declaring `vm`: locals drop in
    /// reverse, so the arena is munmapped (last Arc dropped) before the guard releases and the
    /// next test maps. `into_inner` on poison keeps a panicking test from cascading here.
    static VM_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn allocate_anywhere_is_bounded_by_the_arena_span() {
        let _vm_guard = VM_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let vm = small_vm();
        let span = vm.span();
        let guest_base = vm.guest_base();

        // Normal in-bounds allocate-anywhere: succeeds and the whole region fits the arena.
        {
            let mut mm = VmMemoryManager::new(vm.clone());
            let base = mm
                .map(0, 0x1000, rw(), Some("t"))
                .expect("small allocate-anywhere map should succeed");
            assert!(base >= guest_base, "base below guest_base");
            assert!(
                base.checked_add(0x1000).unwrap() <= span,
                "returned region must fit inside the arena"
            );
        }

        // `len` larger than the room left above the heap cursor: the chosen base would run
        // past `span`, so the map is rejected with the same error the explicit branch uses
        // (rather than returning Ok(base) for a region off the end of the backed arena).
        {
            let mut mm = VmMemoryManager::new(vm.clone());
            assert_eq!(
                mm.map(0, 0x20_0000, rw(), Some("t")),
                Err("map beyond arena span"),
            );
        }

        // `len` larger than the whole arena capacity: rejected up front, before the search
        // (which would otherwise overflow its own `addr + size` arithmetic for a huge len).
        {
            let mut mm = VmMemoryManager::new(vm.clone());
            let too_big = (span - guest_base) as usize + 0x1000;
            assert_eq!(
                mm.map(0, too_big, rw(), Some("t")),
                Err("map beyond arena span"),
            );
        }
    }

    /// A guest `munmap` whose page-aligned interior covers the permanent HLT gadget page must
    /// NOT `MADV_DONTNEED` that page: doing so drops the resident `hlt` byte to a fresh zero
    /// page, and the run loop's pushed sentinel return address (`GADGET_ADDR`) then executes
    /// as `00 00` instead of `hlt` — exec.rs never breaks with the guest-return HLT and the
    /// CPU runs off into garbage. The discard must skip the gadget page while still advising
    /// the neighbouring pages it legitimately covers.
    #[test]
    fn unmap_keeps_the_hlt_gadget_page_resident() {
        let _vm_guard = VM_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let vm = small_vm();
        let gadget = vm.gadget_addr();
        let page = 0x1000u64;

        // Exact-page unmap of the gadget page (the reported scenario): its `hlt` byte survives.
        {
            let mut mm = VmMemoryManager::new(vm.clone());
            let before = mm.read_bytes(gadget, 1).expect("gadget read");
            assert_ne!(
                before[0], 0,
                "gadget byte should be the hlt opcode, not zero"
            );
            mm.unmap(gadget, page as usize).expect("unmap succeeds");
            let after = mm.read_bytes(gadget, 1).expect("gadget read after unmap");
            assert_eq!(
                after, before,
                "gadget hlt byte must survive an unmap that covers exactly its page"
            );
        }

        // A larger unmap straddling the gadget page: the discard splits around it — the pages
        // below and above are still dropped (marker bytes zero on next touch), the gadget byte
        // stays resident.
        {
            let mut mm = VmMemoryManager::new(vm.clone());
            let gadget_byte = mm.read_bytes(gadget, 1).expect("gadget read")[0];
            let below = gadget - page;
            let above = gadget + page;
            mm.write_bytes(below, &[0xAB]).expect("mark page below");
            mm.write_bytes(above, &[0xCD]).expect("mark page above");
            mm.unmap(below, (3 * page) as usize)
                .expect("unmap succeeds");
            assert_eq!(
                mm.read_bytes(gadget, 1).expect("gadget read after unmap")[0],
                gadget_byte,
                "gadget hlt byte must survive an unmap straddling its page"
            );
            assert_eq!(
                mm.read_bytes(below, 1).expect("below read after unmap")[0],
                0,
                "the page below the gadget must still be discarded by the split"
            );
            assert_eq!(
                mm.read_bytes(above, 1).expect("above read after unmap")[0],
                0,
                "the page above the gadget must still be discarded by the split"
            );
        }
    }

    /// A guest `munmap` with an `addr` near `u64::MAX` must not panic. The kernel passes `addr`
    /// straight through with no range check, so the page round-up of `addr` can overflow u64:
    /// there is no multiple of the page size `>= addr` that fits. `next_multiple_of` panics on
    /// that overflow in an overflow-checked build, aborting the emulator on guest input; the
    /// saturating round-up must instead skip the (out-of-arena) discard and succeed as a no-op.
    #[test]
    fn unmap_of_a_near_u64_max_addr_does_not_overflow() {
        let _vm_guard = VM_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let vm = small_vm();
        let mut mm = VmMemoryManager::new(vm.clone());
        // `addr` sits inside the final partial page, so no page-multiple `>= addr` fits u64.
        mm.unmap(u64::MAX, 0x1000)
            .expect("unmap of an out-of-range addr must succeed as a no-op, not panic");
    }
}
