//! Process-global, VMA-bounds-checked read seam for untrusted guest pointers.
//!
//! Some HLE handlers (`libSceGnmDriver`'s `sceGnmSetVsShader`/`sceGnmSetPsShader`) read a
//! register-setup block from a *guest-supplied* pointer. Those handlers run with no
//! `&dyn VirtualMemoryManager` in hand and the only memory view they can reach directly is
//! the identity mapping (`IdentityMem`), whose `get_host_ptr` returns `Some` for **every**
//! address — so a read near an unmapped page over-reads into raw host memory (a SIGSEGV, or
//! a leak of adjacent host memory into shader registers).
//!
//! This seam lets those handlers reach the real, VMA-tracking memory manager — the one that
//! knows which ranges the guest actually mapped — without depending on `ps4-memory`/`ps4-kernel`.
//! The impl registers itself at boot through the global below, exactly like
//! [`crate::kernel::register_kernel`] / [`crate::dirty::register_dirty_source`] /
//! [`crate::gpu::register_present_sink`].
//!
//! When no source is wired (headless / unit tests), [`bounded_read`] returns `None` and the
//! caller must **degrade safely** — prefer reading nothing over falling back to an unbounded
//! read of an untrusted pointer.

use crate::memory::{RANGED_READ_UNIMPLEMENTED, VirtualMemoryManager};
use crate::registered::Registered;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

/// The VMA-bounds-checked read seam for untrusted guest pointers.
///
/// The sole real impl routes to the VMA-tracking [`VirtualMemoryManager::read_bytes_ranged`],
/// which validates the whole `[addr, addr+size)` range against the mapped-region set before
/// copying, so a read that straddles a VMA boundary or an unmapped page is a clean `Err`
/// instead of an over-read.
pub trait BoundedRead: Send + Sync {
    /// Read exactly `size` bytes at guest `addr`, or `Err` if the whole range is not
    /// backed by a single contiguous mapped region. Never over-reads.
    fn read_ranged(&self, addr: u64, size: usize) -> Result<Vec<u8>, &'static str>;
}

/// Blanket impl over the process's shared memory manager handle (the exact
/// `Arc<RwLock<Box<dyn VirtualMemoryManager>>>` the kernel holds). Registering
/// `process.memory.clone()` here means `read_ranged` reaches the live VMA set — the same
/// map that grows as the guest maps/unmaps memory — through the range-validated
/// `read_bytes_ranged` override, mirroring how the fault annotator clones the same handle.
impl BoundedRead for Arc<RwLock<Box<dyn VirtualMemoryManager>>> {
    fn read_ranged(&self, addr: u64, size: usize) -> Result<Vec<u8>, &'static str> {
        match self.read() {
            Ok(mem) => mem.read_bytes_ranged(addr, size),
            Err(_) => Err("guest memory lock poisoned"),
        }
    }
}

/// Blanket impl over any [`VirtualMemoryManager`]: a bounded read routes to the
/// range-validated [`VirtualMemoryManager::read_bytes_ranged`], **never** the unbounded
/// [`VirtualMemoryManager::read_bytes`]. This is the seam that lets range-checking readers
/// (the `.sb` shader parser) take a small [`BoundedRead`] while callers that already hold a
/// full memory manager — a `&dyn VirtualMemoryManager`, the VMA-bounded shader view — keep
/// working unchanged. A backend that does not override `read_bytes_ranged` yields a clean
/// `Err` here (the trait's fail-loud default), so no reader inherits an over-read.
///
/// That fail-loud default is also a footgun: registering a *non-overriding*
/// `VirtualMemoryManager` (e.g. `IdentityMem` or a boot stub) as the bounded-read source
/// makes **every** bounded read `Err` silently — shaders never bind. To make that
/// misregistration diagnosable rather than a silent reversal, the first such `Err` is logged
/// once (see `warn_once_on_unimplemented`).
impl<T: VirtualMemoryManager + ?Sized> BoundedRead for T {
    fn read_ranged(&self, addr: u64, size: usize) -> Result<Vec<u8>, &'static str> {
        let r = self.read_bytes_ranged(addr, size);
        if let Err(e) = r
            && e == RANGED_READ_UNIMPLEMENTED
        {
            warn_once_on_unimplemented();
        }
        r
    }
}

/// Warn exactly once (process-global) when a bounded read hits the non-overriding
/// [`VirtualMemoryManager::read_bytes_ranged`] default: the registered source does not
/// range-validate, so every bounded read fails and shaders stay unbound. This turns an
/// otherwise silent misregistration into a single diagnosable log line.
fn warn_once_on_unimplemented() {
    static WARNED: AtomicBool = AtomicBool::new(false);
    if WARNED
        .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
        .is_ok()
    {
        tracing::warn!(
            "bounded_read source does not override read_bytes_ranged: every bounded read \
             will fail (shaders unbound). Register a VMA-tracking memory manager \
             (see register_bounded_read)."
        );
    }
}

static BOUNDED_READ: Registered<dyn BoundedRead> = Registered::new();

/// Register the process-global bounded-read source, mirroring [`crate::kernel::register_kernel`].
/// The app wires the VMA-tracking memory manager at boot; the `libSceGnmDriver` shader-set
/// handlers reach it through [`bounded_read`] to validate untrusted `regs` pointers. Called
/// once at boot, before guest threads start, so the write lock is uncontended and can't be
/// poisoned; a poisoned lock is recovered rather than silently dropping the wiring.
///
/// # Contract — the source MUST range-validate
///
/// The registered source **must** actually implement bounded reads: if it is a
/// [`VirtualMemoryManager`] it **must override**
/// [`VirtualMemoryManager::read_bytes_ranged`]. The blanket
/// `impl<T: VirtualMemoryManager> BoundedRead` routes through that method, whose default is a
/// fail-loud `Err`. Registering a *non-overriding* manager (e.g. `IdentityMem` or a boot stub)
/// therefore makes **every** bounded read return `Err` — shaders never bind, silently. To make
/// such a misregistration diagnosable, the first bounded read that hits the non-overriding
/// default logs a one-shot `tracing::warn!`. Wire the real VMA-tracking memory manager (which
/// overrides `read_bytes_ranged`), not a range-unaware view.
pub fn register_bounded_read(source: Arc<dyn BoundedRead>) {
    BOUNDED_READ.register(source);
}

/// The registered bounded-read source, or `None` when none is wired (headless / unit tests).
/// A caller that gets `None` must degrade safely — read nothing rather than fall back to an
/// unbounded read of an untrusted guest pointer.
pub fn bounded_read() -> Option<Arc<dyn BoundedRead>> {
    BOUNDED_READ.get()
}

/// **Test-only**: swap the global back to unregistered. Prefer the RAII
/// [`Registered::override_scoped`] guard on [`registered_source`] for tests that exercise
/// both the wired and the headless path — it restores the prior value even on panic. This
/// unconditional clear remains for the one-shot clear-and-assert-headless idiom.
#[cfg(any(test, feature = "test-hooks"))]
pub fn clear_bounded_read() {
    BOUNDED_READ.reset();
}

/// **Test-only**: the process-global bounded-read [`Registered`] itself, so tests can take an
/// RAII [`Registered::override_scoped`] / [`Registered::override_none_scoped`] guard that
/// serializes against other bounded-read tests and restores the prior source on drop
/// (panic-safe, no cross-test bleed).
#[cfg(any(test, feature = "test-hooks"))]
pub fn registered_source() -> &'static Registered<dyn BoundedRead> {
    &BOUNDED_READ
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ptr;

    /// A minimal bounded reader over host memory (host addr == guest ptr) with a single
    /// `[start, end)` region: a read wholly inside the region succeeds; one that would
    /// cross the region's end (or start below it) is rejected — never over-reads. This is
    /// exactly the [`BoundedRead`] shape a range-validated backend presents, without the
    /// eight-method `VirtualMemoryManager` boilerplate none of these tests need.
    struct RegionReader {
        start: u64,
        end: u64,
    }

    impl BoundedRead for RegionReader {
        fn read_ranged(&self, addr: u64, size: usize) -> Result<Vec<u8>, &'static str> {
            if size == 0 {
                return Ok(Vec::new());
            }
            let range_end = addr.checked_add(size as u64).ok_or("overflow")?;
            if addr < self.start || addr >= self.end {
                return Err("start not mapped");
            }
            if range_end > self.end {
                return Err("range crosses region boundary");
            }
            let mut buf = vec![0u8; size];
            unsafe {
                ptr::copy_nonoverlapping(addr as *const u8, buf.as_mut_ptr(), size);
            }
            Ok(buf)
        }
    }

    #[test]
    fn ranged_read_in_bounds_ok_out_of_bounds_err() {
        // A 16-byte host buffer standing in for a mapped region ending exactly at its top.
        let buf = [0xABu8; 16];
        let base = buf.as_ptr() as u64;
        let mem = RegionReader {
            start: base,
            end: base + 16,
        };
        // Wholly inside: fine.
        assert_eq!(mem.read_ranged(base, 16).unwrap(), vec![0xAB; 16]);
        // Crossing the region's end by one byte: rejected, never over-read.
        assert!(mem.read_ranged(base, 17).is_err());
        // Start below the region: rejected.
        assert!(mem.read_ranged(base - 1, 4).is_err());
    }

    #[test]
    fn registration_roundtrips() {
        // Register/read-back round-trips through the process-global seam. The RAII scoped
        // override serializes against other bounded-read tests and restores the prior source
        // on drop, so no cross-test bleed (and no manual clear).
        let buf = [0x11u8; 8];
        let base = buf.as_ptr() as u64;
        let src: Arc<dyn BoundedRead> = Arc::new(RegionReader {
            start: base,
            end: base + 8,
        });
        let _guard = BOUNDED_READ.override_scoped(src);
        let got = bounded_read().expect("registered source is retrievable");
        assert_eq!(got.read_ranged(base, 8).unwrap(), vec![0x11; 8]);
        assert!(got.read_ranged(base, 9).is_err());
    }
}
