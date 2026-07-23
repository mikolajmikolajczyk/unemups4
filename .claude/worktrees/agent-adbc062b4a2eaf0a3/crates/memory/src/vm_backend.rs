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
//! SMC / code-page dirty tracking observes every embedder write (doc-1 decision 5).

use std::sync::Arc;

use ps4_core::memory::{MemoryProtection, MemoryVma, VirtualMemoryManager};
use ps4_cpu::GuestVm;
use tracing::warn;

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
}

impl VmMemoryManager {
    /// Wrap a shared [`GuestVm`]. Reserves the HLT gadget page as a permanent VMA so
    /// runtime maps can never collide with it (see [`Self::map`]).
    pub fn new(vm: Arc<GuestVm>) -> Self {
        let gadget = vm.gadget_addr();
        let mut allocations = std::collections::BTreeMap::new();
        // The gadget page (single `hlt` byte at `GADGET_ADDR`, doc-1 dec 3) is written by
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
        }
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
        mut addr: u64,
        size: usize,
        prot: MemoryProtection,
        name: Option<&str>,
    ) -> Result<u64, &'static str> {
        if size == 0 {
            return Err("zero-length map");
        }

        if addr == 0 {
            // "Allocate anywhere" — `find_free_region` picks a slot above `heap_cursor`,
            // which is inside the span by construction.
            addr = self.find_free_region(size);
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
        let zeros = vec![0u8; size];
        if self.vm.write_bytes(addr, &zeros).is_err() {
            return Err("zero-fill of fresh map failed");
        }

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
        let page = 0x1000u64;
        let start = addr;
        let end = addr.saturating_add(size as u64);
        let aligned_start = start.next_multiple_of(page);
        let aligned_end = end & !(page - 1);
        if aligned_end > aligned_start {
            let len = (aligned_end - aligned_start) as usize;
            // SAFETY: identity mapping means host addr == guest addr; `[aligned_start,
            // aligned_end)` is a page-aligned subrange of the arena that `GuestVm` mapped.
            // MADV_DONTNEED on an anonymous private mapping only drops pages (next touch =
            // zero), never unmaps, so the arena stays valid.
            let ret = unsafe {
                libc::madvise(aligned_start as *mut libc::c_void, len, libc::MADV_DONTNEED)
            };
            if ret != 0 {
                warn!(
                    "madvise(MADV_DONTNEED) on 0x{:x}..0x{:x} failed: {}",
                    aligned_start,
                    aligned_end,
                    std::io::Error::last_os_error()
                );
            }
        }

        if self.allocations.remove(&addr).is_none() {
            warn!("Unmapping memory not tracked in VMA: 0x{:x}", addr);
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
        let align = 0x4000;

        loop {
            let addr = (self.heap_cursor + (align - 1)) & !(align - 1);
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

    fn zero_memory(&self, addr: u64, size: usize) -> Result<(), &'static str> {
        // Route zeroing through the VM too (SMC tracking parity with `write_bytes`).
        let zeros = vec![0u8; size];
        self.vm
            .write_bytes(addr, &zeros)
            .map_err(|_| "guest zero_memory failed")
    }
}
