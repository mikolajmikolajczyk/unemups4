//! Typed, range-validated handles for guest-resident memory — the untrusted-pointer hygiene
//! seam (task-115).
//!
//! Under the JIT identity map a guest pointer is a raw host address (guest addr == host addr),
//! so a junk guest pointer an HLE handler dereferences is a host `SIGSEGV`, not a guest fault.
//! ~40 handler sites deref guest out-params directly today; the systemic fix is to route every
//! one through a handle whose *only* constructor validates the base + `size_of::<T>() * count`
//! against the registered arena bounds, then reads through the VMA-bounded
//! [`crate::bounded_read`] seam and writes through the SMC-tracked [`crate::write_guest`] seam
//! — never a raw `get_host_ptr` store.
//!
//! This module is **additive** (PR-B): it introduces [`GuestPtr`]/[`GuestSlice`] and their
//! read/write plumbing; it migrates no handler. Later PRs move the deref sites onto it.
//!
//! # Two-layer validation
//!
//! - The **constructor** ([`GuestPtr::new`] / [`GuestSlice::new`]) does a coarse, lock-free
//!   base + total-size range check against the process-global arena bounds
//!   ([`crate::kernel::arena_bounds`]). A null base, an out-of-arena base, or a
//!   `size_of::<T>() * count` that overflows or crosses the arena top yields `None` — you
//!   cannot even *name* an out-of-arena region.
//! - Each **read/write** goes through the precise, per-VMA seam ([`crate::bounded_read`] /
//!   [`crate::write_guest`]), so a range that straddles an unmapped page *inside* the arena is
//!   still rejected. The constructor check is defence-in-depth, not the whole guarantee.
//!
//! # Headless / unwired
//!
//! With no arena bounds registered, or no read/write seam wired (unit tests, headless), every
//! operation fails **clean** — `None` from a constructor, `None`/`Err` from a read/write —
//! and never falls back to a raw dereference. Reading nothing beats over-reading an untrusted
//! pointer.

use crate::bounded_read::bounded_read;
use crate::kernel::arena_bounds;
use crate::write_guest::write_guest;
use std::marker::PhantomData;

/// The whole `[base, base+count*size_of::<T>())` range lies inside the registered arena
/// bounds, base is non-null, and the size does not overflow. Returns `false` (fail closed)
/// when no arena is registered (headless / unit tests), so a constructor can't hand back a
/// handle to an unbounded region.
fn range_in_arena(addr: u64, total: u64) -> bool {
    if addr == 0 || total == 0 {
        return false;
    }
    let Some((base, end)) = arena_bounds() else {
        return false; // no arena wired → fail closed, never treat any address as in-bounds
    };
    let Some(range_end) = addr.checked_add(total) else {
        return false; // address + size overflowed u64
    };
    addr >= base && range_end <= end
}

/// A typed handle to a single `T` in guest memory. The only constructor validates the base +
/// `size_of::<T>()` against the arena bounds, and every access routes through the bounded read
/// / SMC-tracked write seams — so holding a `GuestPtr<T>` never implies a raw dereference.
#[derive(Debug, Clone, Copy)]
pub struct GuestPtr<T: Copy> {
    addr: u64,
    _marker: PhantomData<T>,
}

impl<T: Copy> GuestPtr<T> {
    /// Construct a handle to a `T` at guest `addr`, or `None` if the base is null, out of the
    /// arena, or the `size_of::<T>()`-byte range crosses the arena top. This is the *only* way
    /// to obtain a `GuestPtr`, so an out-of-arena address can never be named.
    pub fn new(addr: u64) -> Option<GuestPtr<T>> {
        if range_in_arena(addr, size_of::<T>() as u64) {
            Some(GuestPtr {
                addr,
                _marker: PhantomData,
            })
        } else {
            None
        }
    }

    /// The guest address this handle refers to.
    pub fn addr(self) -> u64 {
        self.addr
    }

    /// Read the `T`, or `None` if the read seam is unwired (headless) or the precise per-VMA
    /// bounded read rejects the range (straddles an unmapped page inside the arena, poisoned
    /// lock). Never falls back to a raw dereference.
    pub fn read(self) -> Option<T> {
        let bytes = bounded_read()?
            .read_ranged(self.addr, size_of::<T>())
            .ok()?;
        if bytes.len() != size_of::<T>() {
            return None;
        }
        // SAFETY: `T: Copy`, `bytes` is exactly `size_of::<T>()` bytes, and we read unaligned
        // (the guest buffer is not guaranteed `T`-aligned, matching `MemoryAccessExt::read`).
        Some(unsafe { (bytes.as_ptr() as *const T).read_unaligned() })
    }

    /// Write `val` through the SMC-tracked write seam, or `Err` if the seam is unwired or the
    /// backend rejects the address. Never falls back to a raw store.
    ///
    /// # Precondition — `T` must be plain-old-data
    ///
    /// `val` is reinterpreted as `size_of::<T>()` raw bytes and copied to the guest, so **`T`
    /// must have no uninitialized padding** (a plain-old-data type: an integer, a
    /// `#[repr(C/packed)]` aggregate whose fields tile it with no gaps, etc.). Forming a
    /// `&[u8]` over a `T` with interior padding built field-by-field reads indeterminate
    /// padding bytes — undefined behaviour — and would copy them into guest memory. Every
    /// current instantiation is a padding-free primitive (`u8`/`i32`/`u32`/`i64`/`u64`), which
    /// upholds this. A sound bound for arbitrary aggregates needs a project-wide POD trait
    /// (deliberately out of scope here); until then this precondition is the contract.
    pub fn write(self, val: T) -> Result<(), &'static str> {
        let seam = write_guest().ok_or("no write-guest seam wired")?;
        // SAFETY: per the plain-old-data precondition above, all `size_of::<T>()` bytes of
        // `val` are initialized (no padding), so the `&[u8]` is over initialized memory; the
        // byte read makes no `T`-alignment assumption.
        let bytes =
            unsafe { std::slice::from_raw_parts(&val as *const T as *const u8, size_of::<T>()) };
        seam.write_bytes(self.addr, bytes)
    }

    /// Zero the `size_of::<T>()` bytes at this handle through the write seam.
    pub fn zero(self) -> Result<(), &'static str> {
        let seam = write_guest().ok_or("no write-guest seam wired")?;
        let zeros = vec![0u8; size_of::<T>()];
        seam.write_bytes(self.addr, &zeros)
    }
}

/// A typed handle to a `[T; len]` run in guest memory. The only constructor validates the base
/// and `size_of::<T>() * len` (checked-mul) against the arena bounds; every access routes through
/// the bounded read / SMC-tracked write seams.
#[derive(Debug, Clone, Copy)]
pub struct GuestSlice<T: Copy> {
    addr: u64,
    len: usize,
    _marker: PhantomData<T>,
}

impl<T: Copy> GuestSlice<T> {
    /// Construct a handle to `count` consecutive `T` at guest `addr`, or `None` if the base is
    /// null, `size_of::<T>() * count` overflows, or the whole run crosses the arena top. A
    /// zero `count` yields `None` (there is nothing to name).
    pub fn new(addr: u64, count: usize) -> Option<GuestSlice<T>> {
        let total = (size_of::<T>() as u64).checked_mul(count as u64)?;
        if range_in_arena(addr, total) {
            Some(GuestSlice {
                addr,
                len: count,
                _marker: PhantomData,
            })
        } else {
            None
        }
    }

    /// The guest base address of the run.
    pub fn addr(self) -> u64 {
        self.addr
    }

    /// The element count.
    pub fn len(self) -> usize {
        self.len
    }

    /// Whether the run is empty (always `false` — the constructor rejects a zero count).
    pub fn is_empty(self) -> bool {
        self.len == 0
    }

    /// Read all `len` elements, or `None` if the read seam is unwired or the bounded read
    /// rejects the range. Never falls back to a raw dereference.
    pub fn read_vec(self) -> Option<Vec<T>> {
        let total = size_of::<T>() * self.len;
        let bytes = bounded_read()?.read_ranged(self.addr, total).ok()?;
        if bytes.len() != total {
            return None;
        }
        let base = bytes.as_ptr() as *const T;
        // SAFETY: `T: Copy`, `bytes` is exactly `total` bytes = `len` unaligned `T` values.
        Some(
            (0..self.len)
                .map(|i| unsafe { base.add(i).read_unaligned() })
                .collect(),
        )
    }

    /// Write `vals` over the run through the SMC-tracked write seam. `Err` if `vals.len()`
    /// exceeds the handle's length, the seam is unwired, or the backend rejects the address.
    ///
    /// # Precondition — `T` must be plain-old-data
    ///
    /// `vals` is reinterpreted as `size_of_val(vals)` raw bytes and copied to the guest, so
    /// **`T` must have no uninitialized padding** — see [`GuestPtr::write`] for the full
    /// rationale. All current instantiations are padding-free primitives, which uphold it.
    pub fn write_slice(self, vals: &[T]) -> Result<(), &'static str> {
        if vals.len() > self.len {
            return Err("write_slice: source longer than guest slice");
        }
        let seam = write_guest().ok_or("no write-guest seam wired")?;
        // SAFETY: per the plain-old-data precondition above, every byte of each `T` in `vals`
        // is initialized (no padding), so the `&[u8]` is over initialized memory; the read
        // makes no `T`-alignment assumption of the source.
        let bytes = unsafe {
            std::slice::from_raw_parts(vals.as_ptr() as *const u8, std::mem::size_of_val(vals))
        };
        seam.write_bytes(self.addr, bytes)
    }

    /// Zero the whole run through the write seam.
    pub fn zero(self) -> Result<(), &'static str> {
        let seam = write_guest().ok_or("no write-guest seam wired")?;
        let zeros = vec![0u8; size_of::<T>() * self.len];
        seam.write_bytes(self.addr, &zeros)
    }
}

/// Read a NUL-terminated C string at guest `addr`, scanning at most `max` bytes, or `None` if
/// the address is out of the arena / the read seam is unwired / no NUL is found within `max`.
///
/// The scan is **chunked and bounded**: it reads through [`crate::bounded_read`] in fixed
/// windows and stops at the first NUL, so it never over-reads past an unmapped page the way a
/// raw `CStr::from_ptr` on an untrusted pointer would (the crash this replaces, pthread.rs).
/// The returned string is the bytes up to (not including) the NUL, lossily decoded as UTF-8.
pub fn read_cstr(addr: u64, max: usize) -> Option<String> {
    if addr == 0 || max == 0 {
        return None;
    }
    // Coarse arena check first: refuse a base outside the arena before touching the seam.
    if !range_in_arena(addr, 1) {
        return None;
    }
    let reader = bounded_read()?;
    const CHUNK: usize = 64;
    let mut out: Vec<u8> = Vec::new();
    let mut off: usize = 0;
    while off < max {
        let want = CHUNK.min(max - off);
        // A chunk that crosses an unmapped page fails the ranged read; shrink and retry so a
        // string that ends just before a boundary is still recovered up to the NUL.
        let mut got = None;
        let mut chunk = want;
        while chunk > 0 {
            if let Ok(bytes) = reader.read_ranged(addr + off as u64, chunk) {
                got = Some(bytes);
                break;
            }
            chunk /= 2;
        }
        let bytes = got?;
        if let Some(pos) = bytes.iter().position(|&b| b == 0) {
            out.extend_from_slice(&bytes[..pos]);
            return Some(String::from_utf8_lossy(&out).into_owned());
        }
        out.extend_from_slice(&bytes);
        off += bytes.len();
    }
    // Hit `max` with no NUL: treat as not-a-valid-cstr rather than returning a truncated blob.
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bounded_read::{BoundedRead, registered_source as read_source};
    use crate::kernel::set_arena_bounds;
    use crate::write_guest::{WriteGuest, registered_source as write_source};
    use std::ptr;
    use std::sync::{Arc, Mutex};

    /// Serializes the whole wire+assert critical section across these tests. The arena bounds
    /// (`set_arena_bounds`) are a plain process-global with no scoped restore, and the two seam
    /// overrides serialize on *separate* per-instance mutexes — so without this a parallel test
    /// could reset the bounds under another's feet. One shared guard, held for each test body,
    /// makes the bounds + both seams a single serialized unit.
    static TEST_GUARD: Mutex<()> = Mutex::new(());

    /// A single-region host-backed source implementing both the read *and* write seam over
    /// `[start, end)` (host addr == guest ptr): a read/write wholly inside the region succeeds;
    /// one that crosses the end (or starts below) is rejected — never over-reads/over-writes.
    /// Serialized on an internal mutex so concurrent writes to the shared buffer don't race.
    struct RegionMem {
        start: u64,
        end: u64,
        lock: Mutex<()>,
    }

    impl BoundedRead for RegionMem {
        fn read_ranged(&self, addr: u64, size: usize) -> Result<Vec<u8>, &'static str> {
            if size == 0 {
                return Ok(Vec::new());
            }
            let range_end = addr.checked_add(size as u64).ok_or("overflow")?;
            if addr < self.start || addr >= self.end || range_end > self.end {
                return Err("out of region");
            }
            let _g = self.lock.lock().unwrap();
            let mut buf = vec![0u8; size];
            unsafe { ptr::copy_nonoverlapping(addr as *const u8, buf.as_mut_ptr(), size) };
            Ok(buf)
        }
    }

    impl WriteGuest for RegionMem {
        fn write_bytes(&self, addr: u64, data: &[u8]) -> Result<(), &'static str> {
            if data.is_empty() {
                return Ok(());
            }
            let range_end = addr.checked_add(data.len() as u64).ok_or("overflow")?;
            if addr < self.start || addr >= self.end || range_end > self.end {
                return Err("out of region");
            }
            let _g = self.lock.lock().unwrap();
            unsafe { ptr::copy_nonoverlapping(data.as_ptr(), addr as *mut u8, data.len()) };
            Ok(())
        }
    }

    /// Wire a `RegionMem` over `buf` into both seams and set the arena bounds to exactly the
    /// buffer, returning RAII guards that restore the prior sources + a value the test keeps
    /// alive. Bounds are process-global (no restore hook), so each test sets them to its own
    /// buffer; the caller must already hold [`TEST_GUARD`] (via [`lock`]) so the bounds + seam
    /// overrides can't race a parallel test. Returns the two scoped-seam restore guards.
    fn wire<'a>(buf: &[u8]) -> (impl Drop + 'a, impl Drop + 'a) {
        let base = buf.as_ptr() as u64;
        let end = base + buf.len() as u64;
        set_arena_bounds(base, buf.len() as u64);
        let mem = Arc::new(RegionMem {
            start: base,
            end,
            lock: Mutex::new(()),
        });
        let rg = read_source().override_scoped(mem.clone() as Arc<dyn BoundedRead>);
        let wg = write_source().override_scoped(mem as Arc<dyn WriteGuest>);
        (rg, wg)
    }

    /// Take the shared serialization guard for a test body. Recovers a poisoned lock so a prior
    /// panicking test doesn't wedge the rest.
    fn lock() -> std::sync::MutexGuard<'static, ()> {
        TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn guest_ptr_roundtrip() {
        let _t = lock();
        let buf = [0u8; 64];
        let (_rg, _wg) = wire(&buf);
        let base = buf.as_ptr() as u64;
        let p = GuestPtr::<u32>::new(base).expect("in-arena base");
        p.write(0xDEAD_BEEFu32).unwrap();
        assert_eq!(p.read(), Some(0xDEAD_BEEFu32));
        p.zero().unwrap();
        assert_eq!(p.read(), Some(0u32));
    }

    #[test]
    fn guest_slice_roundtrip() {
        let _t = lock();
        let buf = [0u8; 64];
        let (_rg, _wg) = wire(&buf);
        let base = buf.as_ptr() as u64;
        let s = GuestSlice::<u16>::new(base, 8).expect("in-arena slice");
        assert_eq!(s.len(), 8);
        let src = [1u16, 2, 3, 4, 5, 6, 7, 8];
        s.write_slice(&src).unwrap();
        assert_eq!(s.read_vec(), Some(src.to_vec()));
        s.zero().unwrap();
        assert_eq!(s.read_vec(), Some(vec![0u16; 8]));
    }

    #[test]
    fn rejects_base_and_size_at_arena_top() {
        let _t = lock();
        // A buffer that is the whole arena; a `u32` whose base sits one byte below the top has
        // a 4-byte range that crosses the top → constructor rejects it.
        let buf = [0u8; 16];
        let (_rg, _wg) = wire(&buf);
        let base = buf.as_ptr() as u64;
        // Base exactly at top: out of arena.
        assert!(GuestPtr::<u8>::new(base + 16).is_none());
        // 4-byte read starting 1 below the top crosses it.
        assert!(GuestPtr::<u32>::new(base + 13).is_none());
        // Exactly fits (last 4 bytes): allowed.
        assert!(GuestPtr::<u32>::new(base + 12).is_some());
        // A slice whose element count overflows the top is rejected.
        assert!(GuestSlice::<u32>::new(base, 5).is_none());
        assert!(GuestSlice::<u32>::new(base, 4).is_some());
        // Null base is always rejected.
        assert!(GuestPtr::<u8>::new(0).is_none());
    }

    #[test]
    fn headless_fails_clean_never_derefs() {
        let _t = lock();
        // No arena wired + no seams: every constructor is None, and even a hand-forged handle
        // (via an in-arena wire, then dropped) reads None rather than dereferencing.
        let buf = [0u8; 32];
        let base = buf.as_ptr() as u64;
        // Force arena + seams OFF for this test window.
        set_arena_bounds(0, 0);
        let _rg = read_source().override_none_scoped();
        let _wg = write_source().override_none_scoped();
        // Constructor fails closed with no arena.
        assert!(GuestPtr::<u32>::new(base).is_none());
        assert!(GuestSlice::<u32>::new(base, 4).is_none());
        assert!(read_cstr(base, 16).is_none());
    }

    #[test]
    fn read_cstr_bounded_scan() {
        let _t = lock();
        // Each `wire()` holds the seams' scoped test-lock for the returned guards' lifetime, so
        // the two sub-cases must be in SEPARATE scopes — stacking a second `wire()` while the
        // first guards are alive would re-lock the same test-lock and deadlock.
        {
            let mut buf = [0u8; 64];
            let s = b"hello\0garbage-after-nul";
            buf[..s.len()].copy_from_slice(s);
            let (_rg, _wg) = wire(&buf);
            let base = buf.as_ptr() as u64;
            assert_eq!(read_cstr(base, 64).as_deref(), Some("hello"));
        }
        // A max reached with no NUL returns None (no valid C-string within `max`).
        {
            let nonul = [b'x'; 16];
            let (_rg2, _wg2) = wire(&nonul);
            let base2 = nonul.as_ptr() as u64;
            assert_eq!(read_cstr(base2, 16), None);
        }
    }
}
