use bitflags::bitflags;
use std::ptr;

/// The `Err` a range-unaware [`VirtualMemoryManager`] returns from the default
/// [`VirtualMemoryManager::read_bytes_ranged`]. Shared so the `bounded_read` seam can
/// recognize a misregistered (non-overriding) source and warn instead of failing silently.
pub const RANGED_READ_UNIMPLEMENTED: &str = "ranged read not implemented for this backend";

bitflags! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct MemoryProtection: u32 {
        const READ  = 1 << 0;
        const WRITE = 1 << 1;
        const EXEC  = 1 << 2;
    }
}

impl MemoryProtection {
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
    fn is_memory_free(&self, addr: u64, size: usize) -> bool;

    /// Human-readable VMA context for a faulting guest `addr` — used by the CPU run
    /// loop to annotate an `UnmappedMemory` fault report. The default is a
    /// bare fallback; a VMA-tracking backend overrides it to name the region that
    /// contains `addr`, or the nearest region(s) below/above when it is in a gap.
    fn describe_fault_context(&self, _addr: u64) -> String {
        "no VMA information available from this memory manager".to_string()
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

    fn zero_memory(&self, addr: u64, size: usize) -> Result<(), &'static str> {
        unsafe {
            if let Some(host_ptr) = self.get_host_ptr(addr) {
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
    /// The whole array must be backed at `addr` (only `addr` itself is checked via
    /// `get_host_ptr`; the identity mapping the GPU paths rely on guarantees the run
    /// is contiguous).
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
        unsafe {
            if let Some(host_ptr) = self.get_host_ptr(addr) {
                let base = host_ptr as *const T;
                Ok((0..count).map(|i| base.add(i).read_unaligned()).collect())
            } else {
                Err("Segmentation Fault (read_array<T>)")
            }
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
}
