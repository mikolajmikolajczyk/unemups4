use bitflags::bitflags;
use std::ptr;

/// The `Err` a range-unaware [`VirtualMemoryManager`] returns from the default
/// [`VirtualMemoryManager::read_bytes_ranged`]. Shared so the `bounded_read` seam can
/// recognize a misregistered (non-overriding) source and warn instead of failing silently.
pub const RANGED_READ_UNIMPLEMENTED: &str = "ranged read not implemented for this backend";

/// The `Err` a range-unaware [`VirtualMemoryManager`] returns from the default
/// [`VirtualMemoryManager::write_bytes_ranged`] — the write mirror of
/// [`RANGED_READ_UNIMPLEMENTED`]. Shared so the `write_guest` seam can recognize a
/// misregistered (non-overriding) source and warn instead of silently failing every write.
pub const RANGED_WRITE_UNIMPLEMENTED: &str = "ranged write not implemented for this backend";

bitflags! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct MemoryProtection: u32 {
        const READ  = 1 << 0;
        const WRITE = 1 << 1;
        const EXEC  = 1 << 2;
    }
}

impl MemoryProtection {
    // Glue: translate our internal permission bits into the host's `mmap` `prot`
    // argument. Uses the `libc::PROT_*` symbols rather than literals, so the values are
    // the host platform's; the same permission semantics are FreeBSD 9.0's
    // `sys/sys/mman.h` (`PROT_READ 0x01`, `PROT_WRITE 0x02`, `PROT_EXEC 0x04`).
    pub fn to_native_mmap_prot(self) -> i32 {
        let mut prot = 0;
        if self.contains(MemoryProtection::READ) {
            prot |= libc::PROT_READ;
        }
        if self.contains(MemoryProtection::WRITE) {
            prot |= libc::PROT_WRITE;
        }
        if self.contains(MemoryProtection::EXEC) {
            prot |= libc::PROT_EXEC;
        }
        prot
    }
}

// ELF program-header permission flags (`p_flags` field of an `Elf64_Phdr`). Values are
// the base-ELF constants FreeBSD 9.0 (the Orbis OS base) defines in
// `sys/sys/elf_common.h`: `PF_X 0x1`, `PF_W 0x2`, `PF_R 0x4`. Pinned by
// `elf_pf_flags_match_freebsd_oracle` below. The guest ELF loader reads these from each
// PT_LOAD segment; mapping them onto our `MemoryProtection` bits (below) is our own glue.
const ELF_PF_X: u32 = 1;
const ELF_PF_W: u32 = 2;
const ELF_PF_R: u32 = 4;

impl From<u32> for MemoryProtection {
    fn from(elf_flags: u32) -> Self {
        let mut prot = MemoryProtection::empty();

        if elf_flags & ELF_PF_R != 0 {
            prot |= MemoryProtection::READ;
        }
        if elf_flags & ELF_PF_W != 0 {
            prot |= MemoryProtection::WRITE;
        }
        if elf_flags & ELF_PF_X != 0 {
            prot |= MemoryProtection::EXEC;
        }

        prot
    }
}

pub trait VirtualMemoryManager: Send + Sync {
    fn map(
        &mut self,
        addr: u64,
        size: usize,
        prot: MemoryProtection,
        name: Option<&str>,
    ) -> Result<u64, &'static str>;

    /// Like [`map`](Self::map) but, for an "allocate anywhere" request (`addr == 0`),
    /// the chosen base is rounded up to `align` bytes (a power of two; `0`/`1` means no
    /// extra alignment). Guest allocators that self-place metadata by masking an object
    /// pointer (e.g. Mono SGen's Large Object Space, which derives its section header via
    /// `chunk & ~0xfffff` and so requires 1 MB-aligned sections) pass this alignment
    /// through `sceKernelAllocateDirectMemory`; ignoring it makes that section math land
    /// on the wrong address and read garbage. The default ignores `align` and delegates to
    /// [`map`](Self::map); a VMA-tracking backend overrides it to honour the alignment.
    fn map_aligned(
        &mut self,
        addr: u64,
        size: usize,
        align: usize,
        prot: MemoryProtection,
        name: Option<&str>,
    ) -> Result<u64, &'static str> {
        let _ = align;
        self.map(addr, size, prot, name)
    }

    fn unmap(&mut self, addr: u64, size: usize) -> Result<(), &'static str>;

    fn protect(
        &mut self,
        addr: u64,
        size: usize,
        prot: MemoryProtection,
    ) -> Result<(), &'static str>;

    /// Translate a guest address to a host pointer into the backing memory.
    ///
    /// # Safety
    /// Returns a raw pointer aliasing guest-owned memory. The caller must ensure
    /// the guest mapping outlives the pointer and that reads/writes through it
    /// respect the guest's expectations (no concurrent mutation, in-bounds access
    /// for `addr`'s VMA). Returns `None` when `addr` is not backed.
    unsafe fn get_host_ptr(&self, addr: u64) -> Option<*mut u8>;
    fn find_free_region(&mut self, size: usize) -> u64;

    /// Like [`find_free_region`](Self::find_free_region) but the returned base is aligned
    /// to at least `align` bytes (power of two; `0`/`1` = backend default). The default
    /// ignores `align` and delegates to `find_free_region`; the VMA-tracking backend
    /// overrides it. See [`map_aligned`](Self::map_aligned) for why an alignment request
    /// must be honoured.
    fn find_free_region_aligned(&mut self, size: usize, align: usize) -> u64 {
        let _ = align;
        self.find_free_region(size)
    }

    fn is_memory_free(&self, addr: u64, size: usize) -> bool;

    /// Human-readable VMA context for a faulting guest `addr` — used by the CPU run
    /// loop to annotate an `UnmappedMemory` fault report. The default is a
    /// bare fallback; a VMA-tracking backend overrides it to name the region that
    /// contains `addr`, or the nearest region(s) below/above when it is in a gap.
    fn describe_fault_context(&self, _addr: u64) -> String {
        "no VMA information available from this memory manager".to_string()
    }

    /// The tracked VMA relevant to a `sceKernelVirtualQuery(addr, ...)`. With `find_next`
    /// false, the region containing `addr`; with it true, the nearest region starting
    /// at/above `addr`. `None` when nothing matches. A range-tracking backend overrides
    /// this; the default has no VMA set.
    fn query_region(&self, _addr: u64, _find_next: bool) -> Option<MemoryVma> {
        None
    }

    fn write_bytes(&self, addr: u64, data: &[u8]) -> Result<(), &'static str> {
        unsafe {
            if let Some(host_ptr) = self.get_host_ptr(addr) {
                ptr::copy_nonoverlapping(data.as_ptr(), host_ptr, data.len());
                Ok(())
            } else {
                Err("Invalid memory address (segfault)")
            }
        }
    }

    fn read_bytes(&self, addr: u64, size: usize) -> Result<Vec<u8>, &'static str> {
        let mut buffer = vec![0u8; size];
        unsafe {
            if let Some(host_ptr) = self.get_host_ptr(addr) {
                ptr::copy_nonoverlapping(host_ptr, buffer.as_mut_ptr(), size);
                Ok(buffer)
            } else {
                Err("Invalid memory address (segfault)")
            }
        }
    }

    /// Read `size` bytes at `addr`, validating that the **whole** `[addr, addr+size)`
    /// range is backed by a single contiguous mapping *before* copying — unlike
    /// [`read_bytes`](Self::read_bytes), whose default only checks the start address and
    /// then copies `size` bytes, silently over-reading past a mapping's end into raw host
    /// memory (a SIGSEGV, or worse, a leak of adjacent memory).
    ///
    /// This is the read that must be used for **untrusted guest addresses** — e.g. a
    /// register-derived, possibly garbage/encrypted shader-program pointer scanned by the
    /// `.sb` parser. An address whose range crosses an unmapped page or a VMA boundary
    /// returns `Err`, so the caller rejects it cleanly rather than faulting.
    ///
    /// There is **no safe default**: a VMA-tracking backend (the real guest memory
    /// manager) **must** override this to consult its VMA set. The default deliberately
    /// fails loud with `Err` rather than silently delegating to the unbounded
    /// [`read_bytes`](Self::read_bytes) — a silent delegation would let any
    /// non-overriding backend inherit the exact over-read this method exists to close
    /// (the start address checks out, then `size` bytes are copied past the mapping).
    /// A caller that only has a range-unaware backend gets a clean rejection here, which
    /// it must treat as "unreadable" (e.g. emit nothing) rather than falling back to an
    /// unbounded read.
    fn read_bytes_ranged(&self, _addr: u64, _size: usize) -> Result<Vec<u8>, &'static str> {
        Err(RANGED_READ_UNIMPLEMENTED)
    }

    /// Write `data` at `addr`, validating that the **whole** `[addr, addr+data.len())` range
    /// is backed by mapped regions *before* storing — the SMC-tracked write mirror of
    /// [`read_bytes_ranged`](Self::read_bytes_ranged). Unlike
    /// [`write_bytes`](Self::write_bytes), whose default only checks the start address and
    /// then memcpys `data.len()` bytes, silently writing past a mapping's end into an unmapped
    /// in-arena hole or raw host memory (corrupting adjacent host/guest memory), this rejects
    /// a range that crosses an unmapped page or a VMA boundary with a clean `Err`.
    ///
    /// This is the write that must be used for **untrusted guest addresses** — the path the
    /// [`crate::write_guest`] seam routes every migrated HLE out-param write through.
    ///
    /// There is **no safe default**: a VMA-tracking backend (the real guest memory manager)
    /// **must** override this to consult its VMA set and store through the *same* SMC-observed
    /// path its [`write_bytes`](Self::write_bytes) uses. The default deliberately fails loud
    /// with `Err` rather than silently delegating to the unbounded
    /// [`write_bytes`](Self::write_bytes) — a silent delegation would let any non-overriding
    /// backend inherit the exact over-write this method exists to close. A caller that only
    /// has a range-unaware backend gets a clean rejection here, which it must treat as
    /// "unwritable" rather than falling back to an unbounded store.
    fn write_bytes_ranged(&self, _addr: u64, _data: &[u8]) -> Result<(), &'static str> {
        Err(RANGED_WRITE_UNIMPLEMENTED)
    }

    fn zero_memory(&self, addr: u64, size: usize) -> Result<(), &'static str> {
        unsafe {
            if let Some(host_ptr) = self.get_host_ptr(addr) {
                // task-138: this IS the memory-manager backend (the write seam itself), not a
                // raw guest-store bypass — zeroing freshly-mapped host memory. Allow the
                // otherwise-disallowed raw `ptr::write_bytes` here (clippy.toml LOCKDOWN).
                #[allow(clippy::disallowed_methods)]
                ptr::write_bytes(host_ptr, 0, size);
                Ok(())
            } else {
                Err("Invalid memory address")
            }
        }
    }
}

pub trait MemoryAccessExt: VirtualMemoryManager {
    fn write<T: Copy>(&self, addr: u64, val: T) -> Result<(), &'static str>;
    fn read<T: Copy>(&self, addr: u64) -> Result<T, &'static str>;

    /// Read `count` consecutive `T` values from the guest array at `addr`.
    ///
    /// A null address or zero count reads nothing (empty `Vec`). Elements are read
    /// **unaligned** (`read_unaligned`), so a guest array need not be `T`-aligned.
    ///
    /// The **whole span** `[addr, addr + count*size_of::<T>())` is validated **once, up
    /// front**, before any element is read — a guest-controlled `count`/`addr` (the GNM
    /// submit path derives `count` from an untrusted `dcb_size`) must not be able to run the
    /// linear read off the backed region into unmapped host memory (a SIGSEGV) or over-read
    /// adjacent guest memory. An out-of-bounds span returns `Err` and reads nothing. The
    /// in-bounds fast path is unchanged (the identity mapping keeps the run contiguous).
    fn read_array<T: Copy>(&self, addr: u64, count: usize) -> Result<Vec<T>, &'static str>;
}

impl<M: VirtualMemoryManager + ?Sized> MemoryAccessExt for M {
    fn write<T: Copy>(&self, addr: u64, val: T) -> Result<(), &'static str> {
        unsafe {
            if let Some(host_ptr) = self.get_host_ptr(addr) {
                let typed_ptr = host_ptr as *mut T;
                // Unaligned: guest pointers are not guaranteed `T`-aligned (matches
                // `read_array`'s contract; EOP/EOS label stores route through here).
                typed_ptr.write_unaligned(val);
                Ok(())
            } else {
                Err("Segmentation Fault (write<T>)")
            }
        }
    }

    fn read<T: Copy>(&self, addr: u64) -> Result<T, &'static str> {
        unsafe {
            if let Some(host_ptr) = self.get_host_ptr(addr) {
                let typed_ptr = host_ptr as *const T;
                // Unaligned: guest pointers are not guaranteed `T`-aligned.
                Ok(typed_ptr.read_unaligned())
            } else {
                Err("Segmentation Fault (read<T>)")
            }
        }
    }

    fn read_array<T: Copy>(&self, addr: u64, count: usize) -> Result<Vec<T>, &'static str> {
        if addr == 0 || count == 0 {
            return Ok(Vec::new());
        }
        // Validate the WHOLE span `[addr, addr + count*size_of::<T>())` once, up front,
        // before reading any element (finding #3: the linear read below would otherwise run a
        // guest-controlled `count` off the backed region into unmapped host memory → SIGSEGV,
        // or over-read adjacent guest memory as PM4). Two cheap up-front guards, no per-element
        // check — this is the hot PM4 decode path:
        //
        //  (1) `get_host_ptr` on the base AND the last byte. On a bounds-aware backend (the
        //      real `VmMemoryManager`) `get_host_ptr` is `Some` only inside the backed arena,
        //      and that arena is contiguous, so a backed base *and* a backed last byte prove
        //      every element between is backed — a span running off the arena top is rejected.
        //  (2) The registered arena bounds, which catch a backend whose `get_host_ptr` is
        //      *unbounded* (the identity view the PM4 decoder uses answers `Some` for every
        //      address). When the read's start is recognized as arena-resident, the whole span
        //      must stay at/under the arena top; an inflated `dcb_size` that overruns it is
        //      rejected. When no arena is registered (headless / unit tests) or the start is
        //      outside it, this guard defers to (1) — it never rejects a read it can't judge.
        let total = (size_of::<T>() as u64)
            .checked_mul(count as u64)
            .ok_or("read_array span overflows the address space")?;
        let last_off = total.checked_sub(1); // `None` only for a zero-size element type
        unsafe {
            let Some(host_ptr) = self.get_host_ptr(addr) else {
                return Err("Segmentation Fault (read_array<T>)");
            };
            // Guard (1): the last byte of the span must be backed too. Skipped for a ZST run
            // (it touches no memory).
            if let Some(off) = last_off {
                let last = addr
                    .checked_add(off)
                    .ok_or("read_array span overflows the address space")?;
                if self.get_host_ptr(last).is_none() {
                    return Err("Segmentation Fault (read_array<T>)");
                }
            }
            // Guard (2): if the start is inside the registered arena, the whole span must stay
            // within it (catches an unbounded identity backend running off the arena top).
            if let Some((base, end)) = crate::kernel::arena_bounds()
                && addr >= base
                && addr < end
            {
                let span_end = addr
                    .checked_add(total)
                    .ok_or("read_array span overflows the address space")?;
                if span_end > end {
                    return Err("Segmentation Fault (read_array<T>)");
                }
            }
            let base = host_ptr as *const T;
            Ok((0..count).map(|i| base.add(i).read_unaligned()).collect())
        }
    }
}

#[derive(Clone, Debug)]
pub struct MemoryVma {
    pub start: u64,
    pub end: u64,
    pub size: usize,
    pub protection: MemoryProtection,
    pub name: String,
}

impl MemoryVma {
    pub fn contains(&self, addr: u64) -> bool {
        addr >= self.start && addr < self.end
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Identity-mapped view (`get_host_ptr(addr) == addr as *mut u8`, mirroring the
    /// GPU paths' `IdentityMem`) so `read_array` can be exercised over host arrays
    /// whose address doubles as the guest pointer. A null address is unbacked.
    struct IdentityView;

    impl VirtualMemoryManager for IdentityView {
        fn map(
            &mut self,
            _addr: u64,
            _size: usize,
            _prot: MemoryProtection,
            _name: Option<&str>,
        ) -> Result<u64, &'static str> {
            Err("unsupported")
        }
        fn unmap(&mut self, _addr: u64, _size: usize) -> Result<(), &'static str> {
            Err("unsupported")
        }
        fn protect(
            &mut self,
            _addr: u64,
            _size: usize,
            _prot: MemoryProtection,
        ) -> Result<(), &'static str> {
            Err("unsupported")
        }
        unsafe fn get_host_ptr(&self, addr: u64) -> Option<*mut u8> {
            if addr == 0 {
                None
            } else {
                Some(addr as *mut u8)
            }
        }
        fn find_free_region(&mut self, _size: usize) -> u64 {
            0
        }
        fn is_memory_free(&self, _addr: u64, _size: usize) -> bool {
            false
        }
    }

    /// Pins the ELF program-header permission flags to their FreeBSD 9.0 (Orbis OS base)
    /// values. The right-hand literals are `PF_X`/`PF_W`/`PF_R` from
    /// `sys/sys/elf_common.h`; this test fails if `ELF_PF_*` drift from those. The
    /// `From<u32>` mapping onto `MemoryProtection` is our own glue, checked alongside.
    #[test]
    fn elf_pf_flags_match_freebsd_oracle() {
        // FreeBSD 9.0 sys/sys/elf_common.h: PF_X 0x1, PF_W 0x2, PF_R 0x4.
        assert_eq!(ELF_PF_X, 0x1);
        assert_eq!(ELF_PF_W, 0x2);
        assert_eq!(ELF_PF_R, 0x4);

        // Our glue lifts each ELF flag to the matching MemoryProtection bit.
        assert_eq!(MemoryProtection::from(ELF_PF_R), MemoryProtection::READ);
        assert_eq!(MemoryProtection::from(ELF_PF_W), MemoryProtection::WRITE);
        assert_eq!(MemoryProtection::from(ELF_PF_X), MemoryProtection::EXEC);
        assert_eq!(
            MemoryProtection::from(ELF_PF_R | ELF_PF_W | ELF_PF_X),
            MemoryProtection::READ | MemoryProtection::WRITE | MemoryProtection::EXEC
        );
    }

    #[test]
    fn read_array_null_or_zero_count_is_empty() {
        let m = IdentityView;
        assert_eq!(m.read_array::<u64>(0, 4).unwrap(), Vec::<u64>::new());
        let data: [u32; 3] = [1, 2, 3];
        assert_eq!(
            m.read_array::<u32>(data.as_ptr() as u64, 0).unwrap(),
            Vec::<u32>::new()
        );
    }

    #[test]
    fn read_array_reads_u32_and_u64_runs() {
        let m = IdentityView;
        let u32s: [u32; 4] = [0xAAAA_AAAA, 0xBBBB_BBBB, 0xCCCC_CCCC, 0xDDDD_DDDD];
        assert_eq!(
            m.read_array::<u32>(u32s.as_ptr() as u64, 4).unwrap(),
            u32s.to_vec()
        );

        // 64-bit values above 4 GB must survive intact (the Gnm submit ABI motive).
        let u64s: [u64; 2] = [0x4_0021_4000, 0x5_00AB_0000];
        assert_eq!(
            m.read_array::<u64>(u64s.as_ptr() as u64, 2).unwrap(),
            u64s.to_vec()
        );
    }

    #[test]
    fn read_array_reads_unaligned() {
        // A u32 run at a deliberately odd offset: `read_unaligned` must still recover
        // the exact values a byte-wise write laid down.
        let mut buf = [0u8; 13];
        let want: [u32; 3] = [0x1122_3344, 0x5566_7788, 0x99AA_BBCC];
        for (i, w) in want.iter().enumerate() {
            buf[1 + i * 4..1 + i * 4 + 4].copy_from_slice(&w.to_ne_bytes());
        }
        let m = IdentityView;
        let addr = unsafe { buf.as_ptr().add(1) } as u64;
        assert_eq!(m.read_array::<u32>(addr, 3).unwrap(), want.to_vec());
    }

    /// Identity view bounded to `[base, end)` (host addr == guest ptr, but `get_host_ptr`
    /// returns `None` outside the window) — the shape the real backend presents, so a
    /// `read_array` whose span leaves the backed region can be rejected without a real
    /// arena. Exercises `read_array`'s up-front span guard (finding #3) deterministically,
    /// without touching the process-global `arena_bounds`.
    struct BoundedView {
        base: u64,
        end: u64,
    }

    impl VirtualMemoryManager for BoundedView {
        fn map(
            &mut self,
            _addr: u64,
            _size: usize,
            _prot: MemoryProtection,
            _name: Option<&str>,
        ) -> Result<u64, &'static str> {
            Err("unsupported")
        }
        fn unmap(&mut self, _addr: u64, _size: usize) -> Result<(), &'static str> {
            Err("unsupported")
        }
        fn protect(
            &mut self,
            _addr: u64,
            _size: usize,
            _prot: MemoryProtection,
        ) -> Result<(), &'static str> {
            Err("unsupported")
        }
        unsafe fn get_host_ptr(&self, addr: u64) -> Option<*mut u8> {
            if addr >= self.base && addr < self.end {
                Some(addr as *mut u8)
            } else {
                None
            }
        }
        fn find_free_region(&mut self, _size: usize) -> u64 {
            0
        }
        fn is_memory_free(&self, _addr: u64, _size: usize) -> bool {
            false
        }
    }

    #[test]
    fn read_array_rejects_span_running_off_backed_region() {
        // A 4-element u32 buffer standing in for a mapped region ending exactly at its top.
        let buf: [u32; 4] = [0x1111_1111, 0x2222_2222, 0x3333_3333, 0x4444_4444];
        let base = buf.as_ptr() as u64;
        let m = BoundedView {
            base,
            end: base + std::mem::size_of_val(&buf) as u64,
        };
        // Exactly fills the region: reads byte-identically.
        assert_eq!(m.read_array::<u32>(base, 4).unwrap(), buf.to_vec());
        // One element past the top: the last byte is unbacked → rejected up front, never
        // over-reads the adjacent stack. (Old code checked only the base and would over-read.)
        assert!(m.read_array::<u32>(base, 5).is_err());
        // A wildly inflated count (the inflated-`dcb_size` shape) is likewise rejected, not a
        // SIGSEGV walking off the region.
        assert!(m.read_array::<u32>(base, 1 << 20).is_err());
        // A base below the region is rejected by the start check.
        assert!(m.read_array::<u32>(base - 4, 1).is_err());
    }
}
