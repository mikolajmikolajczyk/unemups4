//! A zero-state `VirtualMemoryManager` over the identity mapping (guest ptr == host
//! ptr, doc-2 §1). The executor runs on the guest thread with no `&dyn
//! VirtualMemoryManager` in hand, but the `ShaderProvider` seam takes one (phase 4's
//! `GcnShaderProvider` reads `.sb` bytes from guest memory through it). Under the
//! identity mapping a guest address IS a host pointer, so this shim resolves memory
//! with no translation table — exactly the property the PM4 decoder already relies on
//! (`decode_guest`). The embedded provider ignores `mem`; this keeps the single
//! all-binds-through-a-provider route (doc-4 §4) without threading a heavyweight
//! memory manager into the submit handler.

use ps4_core::bounded_read::bounded_read;
use ps4_core::memory::{MemoryProtection, VirtualMemoryManager};

/// Identity-mapped memory view: `get_host_ptr(addr) == addr as *mut u8`. Holds no
/// state. Only the read side (`get_host_ptr` and the trait's default `read_bytes`
/// built on it) is meaningful; the mapping-mutation methods are not supported here
/// (the executor never maps/unmaps).
pub struct IdentityMem;

impl VirtualMemoryManager for IdentityMem {
    fn map(
        &mut self,
        _addr: u64,
        _size: usize,
        _prot: MemoryProtection,
        _name: Option<&str>,
    ) -> Result<u64, &'static str> {
        Err("IdentityMem does not map")
    }
    fn unmap(&mut self, _addr: u64, _size: usize) -> Result<(), &'static str> {
        Err("IdentityMem does not unmap")
    }
    fn protect(
        &mut self,
        _addr: u64,
        _size: usize,
        _prot: MemoryProtection,
    ) -> Result<(), &'static str> {
        Err("IdentityMem does not protect")
    }

    /// The identity mapping: the guest address is already a valid host pointer. A
    /// null address is treated as unbacked.
    ///
    /// # Safety
    /// The caller must uphold the trait contract: `addr` names live, guest-owned
    /// memory of the intended size. Callers here (shader resolution) pass addresses
    /// that came from the guest's own PM4 stream, valid for the submit's duration.
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

/// A `VirtualMemoryManager` whose **only** meaningful operation is a range-validated
/// `read_bytes_ranged` routed through the process-global bounded seam
/// ([`bounded_read`]). The resource cache reads a range's bytes through
/// `read_bytes_ranged` to snapshot an upload (doc-4 §8.2); those addresses are
/// register-derived and untrusted (a V#/index base), so the read must be range-validated
/// against the live VMA set — exactly what the bounded seam provides. Passing this
/// (instead of the unbounded [`IdentityMem`]) to `ResourceCache::get` keeps the upload
/// snapshot on the bounded seam. When no seam is wired (headless), reads fail cleanly and
/// the cache leaves the entry dirty for a later retry — never an unbounded over-read.
pub struct BoundedMem;

impl VirtualMemoryManager for BoundedMem {
    fn map(
        &mut self,
        _addr: u64,
        _size: usize,
        _prot: MemoryProtection,
        _name: Option<&str>,
    ) -> Result<u64, &'static str> {
        Err("BoundedMem does not map")
    }
    fn unmap(&mut self, _addr: u64, _size: usize) -> Result<(), &'static str> {
        Err("BoundedMem does not unmap")
    }
    fn protect(
        &mut self,
        _addr: u64,
        _size: usize,
        _prot: MemoryProtection,
    ) -> Result<(), &'static str> {
        Err("BoundedMem does not protect")
    }

    /// Route the range read through the bounded seam so an untrusted register-derived
    /// address is range-validated, never over-read. `Err` (unmapped / no seam wired) is a
    /// clean fault the cache handles by keeping the entry dirty.
    fn read_bytes_ranged(&self, addr: u64, size: usize) -> Result<Vec<u8>, &'static str> {
        match bounded_read() {
            Some(reader) => reader.read_ranged(addr, size),
            None => Err("bounded read seam not wired"),
        }
    }

    unsafe fn get_host_ptr(&self, _addr: u64) -> Option<*mut u8> {
        // Not used: the cache reads via read_bytes_ranged, never a bare pointer.
        None
    }

    fn find_free_region(&mut self, _size: usize) -> u64 {
        0
    }
    fn is_memory_free(&self, _addr: u64, _size: usize) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_host_ptr_is_identity() {
        let m = IdentityMem;
        let x: u64 = 0xDEAD_BEEF;
        let p = &x as *const u64 as u64;
        assert_eq!(unsafe { m.get_host_ptr(p) }, Some(p as *mut u8));
        assert_eq!(unsafe { m.get_host_ptr(0) }, None);
    }
}
