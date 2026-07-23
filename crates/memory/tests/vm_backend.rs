//! Acceptance tests for `VmMemoryManager`.
//!
//! `VmMemoryManager` is backed by a `GuestVm`, whose `new` does a `MAP_FIXED_NOREPLACE`
//! at the process-global identity `GUEST_BASE`. Two live VMs would collide, so — exactly
//! like the ps4-cpu tests — every test serializes VM construction behind a single
//! `Mutex` and drops the VM (unmapping the arena) before the next test proceeds. ps4-memory
//! and ps4-cpu test binaries run in separate processes, so there is no cross-crate clash.

use std::sync::Mutex;

use ps4_core::memory::{MemoryAccessExt, MemoryProtection, VirtualMemoryManager};
use ps4_cpu::GuestVm;
use ps4_memory::VmMemoryManager;

/// 8 MiB arena — plenty for these tiny tests. (Must exceed `GUEST_BASE` and cover the
/// addresses used below.)
const SPAN: u64 = 0x0080_0000;
const GUEST_BASE: u64 = 0x10000;
const GADGET: u64 = 0x30000;

/// Serializes the process-global fixed identity mmap: only one `GuestVm` alive at a time.
static VM_LOCK: Mutex<()> = Mutex::new(());

fn with_manager<F: FnOnce(&mut VmMemoryManager)>(f: F) {
    let _guard = VM_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let vm = GuestVm::new(SPAN);
    let mut mgr = VmMemoryManager::new(vm);
    f(&mut mgr);
    // `mgr` (and the last Arc<GuestVm>) drop here, unmapping the arena before the guard is
    // released and the next test builds its own VM.
}

const RW: MemoryProtection = MemoryProtection::from_bits_truncate(
    MemoryProtection::READ.bits() | MemoryProtection::WRITE.bits(),
);

#[test]
fn map_write_read_zero_unmap_round_trip() {
    with_manager(|mgr| {
        let base = 0x0040_0000u64;
        let size = 0x2000;

        let addr = mgr.map(base, size, RW, Some("test")).unwrap();
        assert_eq!(addr, base);

        // Fresh map reads zero.
        let fresh = mgr.read_bytes(base, size).unwrap();
        assert!(fresh.iter().all(|&b| b == 0), "fresh map must read zero");

        // Write / read round-trip.
        let payload = [0xAA, 0xBB, 0xCC, 0xDD];
        mgr.write_bytes(base + 0x100, &payload).unwrap();
        let got = mgr.read_bytes(base + 0x100, payload.len()).unwrap();
        assert_eq!(got, payload);

        // zero_memory clears it again.
        mgr.zero_memory(base + 0x100, payload.len()).unwrap();
        let cleared = mgr.read_bytes(base + 0x100, payload.len()).unwrap();
        assert!(cleared.iter().all(|&b| b == 0), "zero_memory must clear");

        // Unmap succeeds and frees the VMA slot.
        mgr.unmap(base, size).unwrap();
        assert!(
            mgr.is_memory_free(base, size),
            "unmapped region is free again"
        );
    });
}

#[test]
fn identity_pointer_matches_manager_write() {
    with_manager(|mgr| {
        let addr = 0x0040_0000u64;
        mgr.map(addr, 0x1000, RW, None).unwrap();

        mgr.write_bytes(addr, &[0x42]).unwrap();

        // Identity: host addr == guest addr. The raw dereference must see the manager's
        // write.
        let seen = unsafe { *(addr as *const u8) };
        assert_eq!(seen, 0x42, "raw identity deref matches manager write");

        // And via get_host_ptr.
        let hp = unsafe { mgr.get_host_ptr(addr) }.expect("in-span addr resolves");
        assert_eq!(unsafe { *hp }, 0x42);
    });
}

#[test]
fn collision_map_errors() {
    with_manager(|mgr| {
        let addr = 0x0040_0000u64;
        mgr.map(addr, 0x2000, RW, None).unwrap();
        // Overlapping map must be rejected.
        assert!(mgr.map(addr + 0x1000, 0x2000, RW, None).is_err());
        assert!(mgr.map(addr, 0x1000, RW, None).is_err());
    });
}

#[test]
fn out_of_span_and_below_base_maps_error() {
    with_manager(|mgr| {
        // Below guest_base.
        assert!(mgr.map(0x1000, 0x1000, RW, None).is_err());
        assert!(mgr.map(GUEST_BASE - 0x1000, 0x1000, RW, None).is_err());
        // Crossing the top of the span.
        assert!(mgr.map(SPAN - 0x800, 0x1000, RW, None).is_err());
        // Fully beyond the span.
        assert!(mgr.map(SPAN + 0x1000, 0x1000, RW, None).is_err());
    });
}

#[test]
fn gadget_page_is_reserved() {
    with_manager(|mgr| {
        // The gadget page is a permanent VMA — any overlapping map is a collision.
        assert!(!mgr.is_memory_free(GADGET, 0x1000));
        assert!(mgr.map(GADGET, 0x1000, RW, None).is_err());
    });
}

#[test]
fn fresh_map_after_dirty_unmap_reads_zero() {
    with_manager(|mgr| {
        let addr = 0x0040_0000u64;
        let size = 0x2000;

        // Map, dirty the whole range with non-zero bytes.
        mgr.map(addr, size, RW, None).unwrap();
        let dirty = vec![0xFFu8; size];
        mgr.write_bytes(addr, &dirty).unwrap();
        assert_eq!(mgr.read_bytes(addr, 1).unwrap()[0], 0xFF);

        // Unmap (madvise the covered pages).
        mgr.unmap(addr, size).unwrap();

        // Re-map the same range: it must read back all zeros, not the stale 0xFF, even
        // though the arena was only host-mapped once (NORESERVE) and reused.
        mgr.map(addr, size, RW, None).unwrap();
        let after = mgr.read_bytes(addr, size).unwrap();
        assert!(
            after.iter().all(|&b| b == 0),
            "reused region must read zero after a dirty unmap"
        );
    });
}

#[test]
fn read_bytes_ranged_rejects_straddling_and_unmapped() {
    // AC #1: the range-validated read must reject a read whose `[addr, addr+size)` range
    // leaves the mapped VMA — over the REAL VmMemoryManager, whose whole arena is
    // host-mapped, so the unbounded `read_bytes` would happily over-read.
    with_manager(|mgr| {
        let base = 0x0040_0000u64;
        let size = 0x1000usize;
        mgr.map(base, size, RW, Some("region")).unwrap();

        // Fully inside the region: ranged read succeeds.
        assert!(mgr.read_bytes_ranged(base, size).is_ok());
        assert!(mgr.read_bytes_ranged(base + 0x800, 0x800).is_ok());

        // Straddling the VMA end by a single byte: rejected (no over-read), even though the
        // plain `read_bytes` succeeds because the arena is contiguously host-mapped.
        assert!(
            mgr.read_bytes(base, size + 1).is_ok(),
            "plain read_bytes over-reads (arena is host-mapped) — the vulnerability"
        );
        assert!(
            mgr.read_bytes_ranged(base, size + 1).is_err(),
            "ranged read must reject a range crossing the VMA end"
        );
        assert!(
            mgr.read_bytes_ranged(base + size as u64 - 1, 2).is_err(),
            "a read spanning the boundary is rejected"
        );

        // An address in an unmapped gap (in-arena but no VMA) is rejected outright.
        let gap = base + 0x10_0000;
        assert!(mgr.is_memory_free(gap, 0x1000));
        assert!(
            mgr.read_bytes_ranged(gap, 4).is_err(),
            "read from an unmapped gap must fault cleanly"
        );

        // Zero-length read is trivially in-bounds.
        assert_eq!(mgr.read_bytes_ranged(gap, 0).unwrap(), Vec::<u8>::new());
    });
}

#[test]
fn read_bytes_ranged_spans_adjacent_mapped_vmas() {
    // A read whose `[addr, addr+size)` crosses from one region into an immediately
    // adjacent, both-mapped region (two back-to-back mmaps: e.g. code + read-only data)
    // must succeed — every byte is mapped, so it is NOT an over-read.
    with_manager(|mgr| {
        let base = 0x0040_0000u64;
        let size = 0x1000usize;
        // Two contiguous regions: [base, base+0x1000) and [base+0x1000, base+0x2000).
        mgr.map(base, size, RW, Some("code")).unwrap();
        mgr.map(base + size as u64, size, RW, Some("rodata"))
            .unwrap();

        // Sanity: the two regions are truly adjacent (no gap).
        // A read straddling the shared boundary spans both mapped regions and succeeds.
        let got = mgr.read_bytes_ranged(base + 0x800, 0x1000).unwrap();
        assert_eq!(got.len(), 0x1000, "cross-VMA read returns all bytes");

        // A read covering the full two-region span also succeeds.
        assert!(
            mgr.read_bytes_ranged(base, 2 * size).is_ok(),
            "read across two contiguous mapped VMAs is allowed"
        );

        // But reading one byte PAST the second region (into the gap above) still faults —
        // the over-read protection is preserved at the true end of the mapped run.
        assert!(
            mgr.read_bytes_ranged(base, 2 * size + 1).is_err(),
            "read past the last contiguous mapping still rejected"
        );
    });
}

#[test]
fn read_bytes_ranged_rejects_gap_between_two_mapped_vmas() {
    // Two mapped regions with an unmapped hole between them: a read spanning the hole
    // must fault even though both endpoints are mapped — a byte falls in the gap.
    with_manager(|mgr| {
        let base = 0x0040_0000u64;
        let size = 0x1000usize;
        // [base, base+0x1000)   mapped
        // [base+0x1000, base+0x2000)  GAP (unmapped)
        // [base+0x2000, base+0x3000)  mapped
        mgr.map(base, size, RW, Some("lo")).unwrap();
        mgr.map(base + 2 * size as u64, size, RW, Some("hi"))
            .unwrap();

        // A read starting in `lo` that reaches into `hi` crosses the unmapped hole.
        assert!(
            mgr.read_bytes_ranged(base, 3 * size).is_err(),
            "read spanning an unmapped gap between two VMAs is rejected"
        );
        // A read landing entirely in the gap is rejected outright.
        assert!(
            mgr.read_bytes_ranged(base + size as u64, 4).is_err(),
            "read starting inside the gap is rejected"
        );
    });
}

#[test]
fn read_bytes_ranged_zero_len_and_addr_at_vma_end() {
    // Edge cases must not panic: a zero-length read is always OK; an addr exactly at a
    // VMA end is the start of a gap (unless the next region is contiguous) and rejected.
    with_manager(|mgr| {
        let base = 0x0040_0000u64;
        let size = 0x1000usize;
        mgr.map(base, size, RW, Some("region")).unwrap();

        // Zero-length read anywhere (even outside any VMA) is trivially in-bounds.
        assert_eq!(mgr.read_bytes_ranged(base, 0).unwrap(), Vec::<u8>::new());
        assert_eq!(
            mgr.read_bytes_ranged(base + size as u64, 0).unwrap(),
            Vec::<u8>::new(),
            "zero-length read at a VMA end does not panic and succeeds"
        );

        // `base + size` is exclusive-end of the region → the first byte of an unmapped
        // gap. A non-empty read there is rejected (no contiguous next region).
        assert!(
            mgr.read_bytes_ranged(base + size as u64, 1).is_err(),
            "read starting exactly at the VMA end (gap start) is rejected"
        );
    });
}

#[test]
fn shader_read_view_read_bytes_is_range_validated() {
    // AC #3 seam: the VMA-aware view passed into parse_sb routes `read_bytes`
    // through the range-validated path, so a parser reading through it faults at a VMA
    // boundary instead of over-reading.
    with_manager(|mgr| {
        let base = 0x0040_0000u64;
        let size = 0x1000usize;
        mgr.map(base, size, RW, Some("shader")).unwrap();

        let view = mgr.shader_read_view();
        assert!(view.read_bytes(base, size).is_ok());
        assert!(
            view.read_bytes(base, size + 1).is_err(),
            "the view's read_bytes is the bounded read"
        );
        // An unmapped start address is rejected too.
        assert!(view.read_bytes(base + 0x10_0000, 4).is_err());
    });
}

#[test]
fn unmap_of_untracked_range_returns_ok() {
    // task-146 AC #1: POSIX `munmap` semantics — unmapping a range that was never mapped
    // (no VMA) is NOT an error; it succeeds (returns Ok / 0). Mono's `mono_vfree`
    // (mono-mmap-orbis.c:219) asserts `res == 0` on the free and calls `abort()` otherwise,
    // so an `Err` here previously killed the guest thread.
    with_manager(|mgr| {
        let untracked = 0x0040_0000u64;
        // Nothing mapped here.
        assert!(mgr.is_memory_free(untracked, 0x2000));
        // Unmapping it succeeds and remains a no-op on the VMA set.
        assert!(
            mgr.unmap(untracked, 0x2000).is_ok(),
            "unmap of an untracked range must return Ok (POSIX munmap)"
        );
        assert!(mgr.is_memory_free(untracked, 0x2000));

        // Already-unmapped (double free) is likewise fine.
        let base = 0x0050_0000u64;
        mgr.map(base, 0x1000, RW, Some("once")).unwrap();
        assert!(mgr.unmap(base, 0x1000).is_ok());
        assert!(
            mgr.unmap(base, 0x1000).is_ok(),
            "second unmap of an already-freed range must still return Ok"
        );
    });
}

#[test]
fn unmap_of_interior_subrange_keeps_the_larger_vma_tracked() {
    // A partial/interior guest free inside a LARGER tracked region must NOT drop that whole
    // region's VMA: its other pages stay resident and live. Dropping it would blind
    // `is_memory_free` and let a later "allocate anywhere" map alias live memory. Only VMAs
    // FULLY CONTAINED in the unmap range are evicted; a partial overlap stays (over-tracking
    // is safe; under-tracking aliases).
    with_manager(|mgr| {
        let base = 0x0040_0000u64;
        let size = 0x4000usize; // 4 pages
        mgr.map(base, size, RW, Some("big")).unwrap();
        assert!(!mgr.is_memory_free(base, size));

        // Free a page in the middle — start != VMA key, and the range does NOT fully contain
        // the larger VMA.
        assert!(
            mgr.unmap(base + 0x1000, 0x1000).is_ok(),
            "unmap of an interior subrange succeeds (POSIX munmap)"
        );
        // The larger VMA stays tracked — the untouched pages are still live, so the region
        // must NOT read as free.
        assert!(
            !mgr.is_memory_free(base, size),
            "partial interior unmap must not drop the larger region's VMA (would alias live memory)"
        );
        // Specifically the untouched tail is still occupied.
        assert!(
            !mgr.is_memory_free(base + 0x3000, 0x1000),
            "the untouched pages of the partially-unmapped region stay tracked"
        );
    });
}

#[test]
fn unmap_of_fully_contained_vma_clears_it() {
    // A guest free whose range FULLY CONTAINS a tracked VMA drops that VMA — including when
    // the free starts before the VMA (Mono frees a slab that wraps a sub-region). After the
    // free the contained region reads as free again.
    with_manager(|mgr| {
        let base = 0x0040_0000u64;
        let size = 0x2000usize; // 2 pages
        mgr.map(base, size, RW, Some("contained")).unwrap();
        assert!(!mgr.is_memory_free(base, size));

        // Free a strictly larger range that fully contains the VMA.
        assert!(
            mgr.unmap(base - 0x1000, size + 0x2000).is_ok(),
            "unmap of a range fully containing a VMA succeeds"
        );
        assert!(
            mgr.is_memory_free(base, size),
            "a fully-contained VMA is dropped and reads as free again"
        );
    });
}

#[test]
fn unmap_never_evicts_the_gadget_page() {
    // The permanent HLT gadget VMA must survive even a guest unmap that straddles it.
    with_manager(|mgr| {
        assert!(!mgr.is_memory_free(GADGET, 0x1000));
        // Unmap a span covering the gadget page.
        assert!(mgr.unmap(GADGET - 0x1000, 0x3000).is_ok());
        assert!(
            !mgr.is_memory_free(GADGET, 0x1000),
            "gadget page VMA is never evicted by an overlapping unmap"
        );
    });
}

#[test]
fn allocate_anywhere_and_typed_access() {
    with_manager(|mgr| {
        // addr == 0 means "allocate anywhere" via the heap cursor (>= 0x4_0000_0000);
        // but that is far above our 8 MiB span, so use a small span-safe explicit map to
        // exercise the typed MemoryAccessExt path instead.
        let addr = 0x0040_0000u64;
        mgr.map(addr, 0x1000, RW, None).unwrap();
        mgr.write::<u32>(addr, 0xDEAD_BEEF).unwrap();
        assert_eq!(mgr.read::<u32>(addr).unwrap(), 0xDEAD_BEEF);
    });
}
