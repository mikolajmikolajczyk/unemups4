//! Process-global, SMC-tracked write seam for guest-resident memory.
//!
//! The mirror of [`crate::bounded_read`]: where that seam gives untrusted-pointer *reads* a
//! VMA-validated path, this one gives *writes* the single, SMC-observed path through the real
//! memory manager. HLE handlers that write a guest out-param today reach for either a raw
//! store through the identity map (`IdentityMem`, which SMC-invalidation never sees) or a bare
//! `*ptr = val`. A write that bypasses the memory manager's `write_bytes` is invisible to the
//! JIT's self-modifying-code tracking, so if the guest later executes code from a page an HLE
//! handler wrote, the stale translation is used.
//!
//! This seam routes every migrated write through
//! [`VirtualMemoryManager::write_bytes_ranged`] — the write mirror of
//! [`crate::bounded_read`]'s `read_bytes_ranged`. The real backend
//! (`VmMemoryManager::write_bytes_ranged`) walks its VMA set and rejects a range that enters
//! an unmapped in-arena hole, then stores through the same SMC-observed path
//! `GuestVm::write_bytes` uses — so a handler that writes through [`write_guest`] gets the
//! same per-VMA validation reads get *and* stays correct under code that is later modified
//! and re-run. A backend that does not override `write_bytes_ranged` fails loud (a one-shot
//! warning), never a silent raw store.
//!
//! Like [`crate::bounded_read`], the impl registers itself at boot through the global below.
//! When no source is wired (headless / unit tests), [`write_guest`] returns `None` and the
//! caller must **degrade safely** — write nothing rather than fall back to a raw store of an
//! untrusted pointer.

use crate::memory::{RANGED_WRITE_UNIMPLEMENTED, VirtualMemoryManager};
use crate::registered::Registered;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

/// The SMC-tracked write seam for guest-resident memory.
///
/// The sole real impl routes to the VMA-tracking [`VirtualMemoryManager::write_bytes_ranged`],
/// which validates the whole `[addr, addr+len)` range against the mapped-region set before
/// storing (so a write straddling a VMA boundary or an unmapped in-arena hole is a clean
/// `Err`, not silent corruption) through the backend's SMC-observed store — so a migrated
/// write to a guest page that is later executed does not leave a stale JIT translation behind.
pub trait WriteGuest: Send + Sync {
    /// Write `data` at guest `addr`, or `Err` if the whole range is not backed by mapped
    /// regions (or the backend otherwise rejects it). The write goes through the
    /// range-validated, SMC-tracked `write_bytes_ranged`, never a raw store.
    fn write_bytes(&self, addr: u64, data: &[u8]) -> Result<(), &'static str>;
}

/// Blanket impl over the process's shared memory manager handle (the exact
/// `Arc<RwLock<Box<dyn VirtualMemoryManager>>>` the kernel holds). Registering
/// `process.memory.clone()` here means [`write_bytes`](WriteGuest::write_bytes) routes through
/// the live VMA-tracking backend's range-validated, SMC-observed `write_bytes_ranged`,
/// mirroring how [`crate::bounded_read`] registers the same handle for reads.
///
/// This is the exact shape the app wires (`register_write_guest(Arc::new(process.memory.clone()))`),
/// so — like the read side's concrete impl — it must run the same fail-loud sentinel check as
/// the blanket impl below; otherwise a misregistered non-overriding backend would fail every
/// write *silently* for the real wiring (finding #4).
impl WriteGuest for Arc<RwLock<Box<dyn VirtualMemoryManager>>> {
    fn write_bytes(&self, addr: u64, data: &[u8]) -> Result<(), &'static str> {
        match self.read() {
            // `VirtualMemoryManager::write_bytes_ranged` takes `&self` (interior mutability of
            // the arena), so a read lock on the `RwLock` suffices — the same shape
            // `bounded_read` uses for `read_bytes_ranged`.
            Ok(mem) => {
                let r = mem.write_bytes_ranged(addr, data);
                if let Err(e) = r
                    && e == RANGED_WRITE_UNIMPLEMENTED
                {
                    warn_once_on_unimplemented();
                }
                r
            }
            Err(_) => Err("guest memory lock poisoned"),
        }
    }
}

/// Blanket impl over any [`VirtualMemoryManager`]: a guest write routes to the manager's
/// range-validated [`write_bytes_ranged`](VirtualMemoryManager::write_bytes_ranged), the
/// SMC-observed store on the real backend — **never** the unbounded
/// [`write_bytes`](VirtualMemoryManager::write_bytes). This lets a raw
/// `&dyn VirtualMemoryManager` present a [`WriteGuest`] without extra boilerplate.
///
/// A backend that does not override `write_bytes_ranged` yields a clean `Err` here (the
/// trait's fail-loud default). That default is also a footgun — registering a *non-overriding*
/// manager as the write source makes **every** guest write `Err` silently (out-params never
/// land). To make that misregistration diagnosable, the first such `Err` is logged once (see
/// `warn_once_on_unimplemented`), mirroring [`crate::bounded_read`].
impl<T: VirtualMemoryManager + ?Sized> WriteGuest for T {
    fn write_bytes(&self, addr: u64, data: &[u8]) -> Result<(), &'static str> {
        let r = VirtualMemoryManager::write_bytes_ranged(self, addr, data);
        if let Err(e) = r
            && e == RANGED_WRITE_UNIMPLEMENTED
        {
            warn_once_on_unimplemented();
        }
        r
    }
}

/// Warn exactly once (process-global) when a guest write hits the non-overriding
/// [`VirtualMemoryManager::write_bytes_ranged`] default: the registered source does not
/// range-validate, so every guest write fails and migrated out-params never land. This turns
/// an otherwise silent misregistration into a single diagnosable log line — the write mirror
/// of [`crate::bounded_read`]'s `warn_once_on_unimplemented`.
fn warn_once_on_unimplemented() {
    static WARNED: AtomicBool = AtomicBool::new(false);
    if WARNED
        .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
        .is_ok()
    {
        tracing::warn!(
            "write_guest source does not override write_bytes_ranged: every guest write \
             will fail (out-params never land). Register a VMA-tracking memory manager \
             (see register_write_guest)."
        );
    }
}

static WRITE_GUEST: Registered<dyn WriteGuest> = Registered::new();

/// Register the process-global guest-write source, mirroring
/// [`crate::bounded_read::register_bounded_read`]. The app wires the same VMA-tracking memory
/// manager it wires for reads (`process.memory.clone()`) at boot; migrated HLE writes reach it
/// through [`write_guest`] so every one stays SMC-tracked. Called once at boot, before guest
/// threads start.
///
/// # Contract — the source MUST range-validate
///
/// The registered source **must** actually implement ranged writes: if it is a
/// [`VirtualMemoryManager`] it **must override**
/// [`VirtualMemoryManager::write_bytes_ranged`]. The blanket
/// `impl<T: VirtualMemoryManager> WriteGuest` routes through that method, whose default is a
/// fail-loud `Err`. Registering a *non-overriding* manager therefore makes **every** guest
/// write return `Err` — migrated out-params never land, silently. To make such a
/// misregistration diagnosable, the first write that hits the non-overriding default logs a
/// one-shot `tracing::warn!`. Wire the real VMA-tracking memory manager (which overrides
/// `write_bytes_ranged`), not a range-unaware view.
pub fn register_write_guest(source: Arc<dyn WriteGuest>) {
    WRITE_GUEST.register(source);
}

/// The registered guest-write source, or `None` when none is wired (headless / unit tests). A
/// caller that gets `None` must degrade safely — write nothing rather than fall back to a raw
/// store of an untrusted guest pointer.
pub fn write_guest() -> Option<Arc<dyn WriteGuest>> {
    WRITE_GUEST.get()
}

/// **Test-only**: the process-global guest-write [`Registered`] itself, so tests can take an
/// RAII [`Registered::override_scoped`] guard that serializes against other write-guest tests
/// and restores the prior source on drop (panic-safe, no cross-test bleed). Mirrors
/// [`crate::bounded_read::registered_source`].
#[cfg(any(test, feature = "test-hooks"))]
pub fn registered_source() -> &'static Registered<dyn WriteGuest> {
    &WRITE_GUEST
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::MemoryProtection;
    use std::ptr;

    /// A minimal [`VirtualMemoryManager`] that **overrides** `write_bytes_ranged` over a single
    /// `[start, end)` region (host addr == guest ptr): a write wholly inside stores; one that
    /// crosses the end (or starts below) is rejected — never over-writes. The range-validated
    /// write mirror of `bounded_read`'s `RegionReader`.
    struct RegionVmm {
        start: u64,
        end: u64,
    }

    /// A [`VirtualMemoryManager`] that does **not** override `write_bytes_ranged` (its
    /// `get_host_ptr` is an unbounded identity map, so a raw store *would* silently succeed).
    /// Registering this as the write source must fail loud with the sentinel, never a raw store.
    struct BareVmm;

    macro_rules! unsupported_mapping_methods {
        () => {
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
            fn find_free_region(&mut self, _size: usize) -> u64 {
                0
            }
            fn is_memory_free(&self, _addr: u64, _size: usize) -> bool {
                false
            }
        };
    }

    impl VirtualMemoryManager for RegionVmm {
        unsupported_mapping_methods!();
        unsafe fn get_host_ptr(&self, addr: u64) -> Option<*mut u8> {
            if addr >= self.start && addr < self.end {
                Some(addr as *mut u8)
            } else {
                None
            }
        }
        fn write_bytes_ranged(&self, addr: u64, data: &[u8]) -> Result<(), &'static str> {
            if data.is_empty() {
                return Ok(());
            }
            let end = addr.checked_add(data.len() as u64).ok_or("overflow")?;
            if addr < self.start || addr >= self.end || end > self.end {
                return Err("out of region");
            }
            unsafe { ptr::copy_nonoverlapping(data.as_ptr(), addr as *mut u8, data.len()) };
            Ok(())
        }
    }

    impl VirtualMemoryManager for BareVmm {
        unsupported_mapping_methods!();
        unsafe fn get_host_ptr(&self, addr: u64) -> Option<*mut u8> {
            // Unbounded identity: a raw fallback store would succeed — exactly the silent
            // corruption the fail-loud sentinel prevents.
            if addr == 0 {
                None
            } else {
                Some(addr as *mut u8)
            }
        }
    }

    #[test]
    fn ranged_write_in_bounds_ok_out_of_bounds_err() {
        // The blanket `impl<T: VirtualMemoryManager> WriteGuest` routes through the range-
        // validated `write_bytes_ranged`, never the unbounded `write_bytes`.
        let mut buf = [0u8; 16];
        let base = buf.as_mut_ptr() as u64;
        let vmm = RegionVmm {
            start: base,
            end: base + 16,
        };
        // Wholly inside: stores and lands byte-for-byte.
        WriteGuest::write_bytes(&vmm, base, &[0xAB; 16]).unwrap();
        assert_eq!(buf, [0xAB; 16]);
        // Crossing the region end by one byte: rejected, buffer untouched.
        assert!(WriteGuest::write_bytes(&vmm, base, &[0xCD; 17]).is_err());
        assert_eq!(buf, [0xAB; 16]);
        // Start below the region: rejected.
        assert!(WriteGuest::write_bytes(&vmm, base - 1, &[0x11; 4]).is_err());
    }

    #[test]
    fn concrete_shared_handle_range_validates_and_fails_loud() {
        // The exact shape the app wires: `Arc<RwLock<Box<dyn VirtualMemoryManager>>>`. Its
        // concrete `WriteGuest` impl must route through `write_bytes_ranged` (validating) AND
        // surface the fail-loud sentinel for a non-overriding backend (finding #4).
        let mut buf = [0u8; 16];
        let base = buf.as_mut_ptr() as u64;
        let handle: Arc<RwLock<Box<dyn VirtualMemoryManager>>> =
            Arc::new(RwLock::new(Box::new(RegionVmm {
                start: base,
                end: base + 16,
            })));
        WriteGuest::write_bytes(&handle, base, &[0x5A; 16]).unwrap();
        assert_eq!(buf, [0x5A; 16]);
        assert!(WriteGuest::write_bytes(&handle, base, &[0x5A; 17]).is_err());

        // A non-overriding backend behind the same concrete shape yields the sentinel, not a
        // silent raw store through its unbounded identity `get_host_ptr`.
        let scratch = [0u8; 8];
        let addr = scratch.as_ptr() as u64;
        let bare: Arc<RwLock<Box<dyn VirtualMemoryManager>>> =
            Arc::new(RwLock::new(Box::new(BareVmm)));
        assert_eq!(
            WriteGuest::write_bytes(&bare, addr, &[1, 2, 3, 4]),
            Err(RANGED_WRITE_UNIMPLEMENTED)
        );
    }

    #[test]
    fn non_overriding_backend_fails_loud_with_sentinel() {
        // The blanket impl over a non-overriding backend hits the trait's fail-loud default:
        // a clean sentinel `Err`, never a raw store (finding #7).
        let scratch = [0u8; 8];
        let addr = scratch.as_ptr() as u64;
        let vmm = BareVmm;
        assert_eq!(
            WriteGuest::write_bytes(&vmm, addr, &[1, 2, 3, 4]),
            Err(RANGED_WRITE_UNIMPLEMENTED)
        );
    }
}
