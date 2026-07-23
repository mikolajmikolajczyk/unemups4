// Syscall/HLE handlers take guest pointers — identity-mapped host addresses in
// the x86jit arena — and dereference them by design; that is the entire job of
// an HLE shim. They are only ever invoked by the generated #[ps4_syscall]
// dispatch with pointers the guest supplied, never as a general-purpose safe
// API, so `not_unsafe_ptr_arg_deref` (which would have us mark ~40 handlers
// `unsafe fn`, fighting the macro for no added safety) does not apply here.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

/// `NativeContext` now lives in ps4-cpu. This shim re-exports it under the
/// old `crate::context` path so every handler's `use crate::context::NativeContext;`
/// keeps compiling unchanged.
pub mod context {
    pub use ps4_cpu::{FromReg, NativeContext};
}
pub mod libkernel;
pub mod libs;
pub mod libscaudioout;
pub mod libsceajm;
pub mod libsceavplayer;
pub mod libscecapture;
pub mod libscecommondialog;
pub mod libscegnmdriver;
pub mod libsceime;
pub mod libscejson;
pub mod libscemouse;
pub mod libscenet;
pub mod libscenp;
pub mod libscenptrophy;
pub mod libsceoffline;
pub mod libscepad;
pub mod libscesavedata;
pub mod libsceuserservice;
pub mod libscevideoout;
pub mod libscngs2;
pub mod registry;
extern crate self as ps4_libs;
pub use crate::context::NativeContext;
pub use registry::{get_handler, register_handler};
use std::panic;
use tracing::{debug_span, error};

pub fn init() {
    // Anchor to force linking of this crate's inventory registrations.
}

/// Does the whole byte range `[ptr, ptr + len)` lie inside the identity-mapped guest arena?
///
/// This is the base+size range check (task-115 PR-C). Unlike the base-only check the old
/// [`is_guest_ptr`] performed, it rejects a range whose base is in-arena but whose `len` bytes
/// cross the arena top — so a fixed-size write near the top can no longer overrun. The bounds
/// come from the process-global arena registered at boot
/// ([`ps4_core::kernel::arena_bounds`], promoted out of `ps4-cpu` in task-115 PR-B), so this
/// no longer reaches into the cpu crate. With no arena registered (headless / unit tests) it
/// fails closed — `false` for every address.
///
/// A null base, a zero `len`, or a `ptr + len` that overflows `u64` is out of the arena.
#[inline]
pub fn is_guest_range(ptr: u64, len: u64) -> bool {
    if ptr == 0 || len == 0 {
        return false;
    }
    let Some((base, end)) = ps4_core::kernel::arena_bounds() else {
        return false; // no arena wired → fail closed, never treat an address as in-bounds
    };
    let Some(range_end) = ptr.checked_add(len) else {
        return false; // ptr + len overflowed u64
    };
    ptr >= base && range_end <= end
}

/// Is `ptr` a plausible guest pointer — i.e. does the whole `size_of::<T>()`-byte object it
/// points at lie inside the identity-mapped guest arena?
///
/// Thin base+size wrapper over [`is_guest_range`] (task-115 PR-C): closes the old base-only gap
/// where a `T` whose base sat just below the arena top passed the check yet overran on deref.
/// Existing callers stay transparent.
///
/// Handlers must check optional pointer
/// args (a debug name, an out-param) before dereferencing: under a POSIX alias the
/// guest often leaves junk in the "extra" argument register (a small integer, a huge
/// negative), and the JIT identity-maps guest pointers straight through, so reading
/// one segfaults the host instead of raising a guest fault. The whole arena is
/// `MAP_NORESERVE`-backed, so any in-range address is safe to read (zero pages).
#[inline]
pub fn is_guest_ptr<T>(ptr: *const T) -> bool {
    is_guest_range(ptr as u64, size_of::<T>() as u64)
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_syscall_handler(id: u64, ctx: &mut NativeContext) -> u64 {
    // Low-frequency-path span: one HLE syscall dispatch. With no span-
    // consuming layer active (the default) this is a cached callsite check that records
    // nothing — so the span is unconditional, no feature gate. Under a Tracy layer it
    // becomes a zone; the raw id lets the viewer group by syscall.
    let _span = debug_span!("syscall", id).entered();
    let result = panic::catch_unwind(panic::AssertUnwindSafe(|| {
        if (id & 0xC000_0000) == 0xC000_0000 {
            let name = ps4_core::debug::get_missing_symbol(id)
                .unwrap_or_else(|| "Unknown/Corrupted".to_string());
            error!(
                "[FATAL ERROR] The application crashed because it called a missing symbol: {}",
                name
            );
            error!(
                "To fix this, implement the syscall for '{}' or alias it in libkernel.",
                name
            );

            std::process::exit(1);
        }
        if id == ps4_core::debug::DLSYM_TRAP_MARKER {
            // A managed P/Invoke thunk dispatched through an unresolved `sceKernelDlsym`
            // target (task-137). The specific symbol name was already logged (WARN) at
            // dlsym-miss time; here we can only note the class of event. Returning 0 makes
            // the unimplemented native graphics call a benign no-op so execution continues
            // to the next wall instead of a host SIGSEGV dispatching into null/garbage.
            static DLSYM_TRAP_LOGGED: std::sync::atomic::AtomicBool =
                std::sync::atomic::AtomicBool::new(false);
            if !DLSYM_TRAP_LOGGED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                error!(
                    "[HLE] Guest called an unresolved sceKernelDlsym target (no-op stub). \
                     The missing symbol was logged at resolve time; treating the call as a \
                     no-op returning 0. Further such calls are silent."
                );
            }
            return 0;
        }
        if let Some(handler) = ps4_libs::get_handler(id) {
            handler(ctx)
        } else {
            let sys_id = ps4_syscalls::SyscallId(id);
            // Dump the six SysV integer args (rdi, rsi, rdx, r10, r8, r9) alongside the id so
            // an unimplemented call names what the guest passed, not just which syscall it was
            // (task-113.2 AC#4). r10 not rcx: the SYSCALL lift clobbers rcx, so the 4th arg
            // rides r10 per the kernel ABI (see NativeContext::arg3).
            error!(
                "[SYSCALL] UNIMPLEMENTED: {} ({}) args=[{:#x}, {:#x}, {:#x}, {:#x}, {:#x}, {:#x}]",
                sys_id.as_str(),
                id,
                ctx.rdi,
                ctx.rsi,
                ctx.rdx,
                ctx.r10,
                ctx.r8,
                ctx.r9,
            );
            0x80020002 // SCE_KERNEL_ERROR_ESRCH-style generic failure code returned to the guest
        }
    }));

    match result {
        Ok(ret) => ret,
        Err(payload) => {
            error!("[SYSCALL] handler for id {} panicked: {:?}", id, payload);
            // Return a generic error rather than success for unimplemented calls.
            0x80020001
        }
    }
}

/// Crate-wide serialization guard for tests that mutate the **process-global** arena bounds
/// (`set_arena_bounds`) — which has no scoped restore, so a parallel test in *any* module could
/// reset it under another's feet. Every such test (this crate's `is_guest_range` tests, the
/// `libscenet` `sceNetInetPton` seam test) takes this one lock so the bounds + wired seams are a
/// single serialized unit across module boundaries. `pub(crate)` so the other test modules share
/// it. (The read/write seam overrides serialize on their own `Registered` mutex; this covers the
/// bounds that don't.)
#[cfg(test)]
pub(crate) fn arena_test_lock() -> std::sync::MutexGuard<'static, ()> {
    use std::sync::Mutex;
    static ARENA_TEST_GUARD: Mutex<()> = Mutex::new(());
    ARENA_TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ps4_core::bounded_read::{BoundedRead, registered_source as read_source};
    use ps4_core::guest_ptr::read_cstr;
    use ps4_core::kernel::set_arena_bounds;
    use ps4_core::write_guest::{WriteGuest, registered_source as write_source};
    use std::ptr;
    use std::sync::Arc;

    fn lock() -> std::sync::MutexGuard<'static, ()> {
        super::arena_test_lock()
    }

    /// A single-region host-backed read seam over `[start, end)` (host addr == guest ptr): a
    /// read wholly inside the region succeeds; one that crosses the end (or starts below) is
    /// rejected — it never over-reads. Used to exercise the migrated `read_cstr` path.
    struct RegionRead {
        start: u64,
        end: u64,
    }

    impl BoundedRead for RegionRead {
        fn read_ranged(&self, addr: u64, size: usize) -> Result<Vec<u8>, &'static str> {
            if size == 0 {
                return Ok(Vec::new());
            }
            let range_end = addr.checked_add(size as u64).ok_or("overflow")?;
            if addr < self.start || addr >= self.end || range_end > self.end {
                return Err("out of region");
            }
            let mut buf = vec![0u8; size];
            // SAFETY: the range is inside the host-backed `buf` this test keeps alive.
            unsafe { ptr::copy_nonoverlapping(addr as *const u8, buf.as_mut_ptr(), size) };
            Ok(buf)
        }
    }

    /// A single-region host-backed *write* seam over `[start, end)` (host addr == guest ptr): a
    /// write wholly inside the region succeeds; one that crosses the end (or starts below) is
    /// rejected — it never over-writes. Used to prove migrated struct writes route through the
    /// seam and fail clean on a bad out-param.
    struct RegionWrite {
        start: u64,
        end: u64,
    }

    impl WriteGuest for RegionWrite {
        fn write_bytes(&self, addr: u64, data: &[u8]) -> Result<(), &'static str> {
            if data.is_empty() {
                return Ok(());
            }
            let range_end = addr.checked_add(data.len() as u64).ok_or("overflow")?;
            if addr < self.start || addr >= self.end || range_end > self.end {
                return Err("out of region");
            }
            // SAFETY: the range is inside the host-backed buffer the test keeps alive.
            unsafe { ptr::copy_nonoverlapping(data.as_ptr(), addr as *mut u8, data.len()) };
            Ok(())
        }
    }

    #[test]
    fn is_guest_range_rejects_base_plus_size_at_arena_top() {
        let _t = lock();
        // Arena is exactly this 16-byte buffer. A range whose base is in-arena but whose length
        // crosses the top must be rejected — the base+size gap the old base-only check missed.
        let buf = [0u8; 16];
        let base = buf.as_ptr() as u64;
        set_arena_bounds(base, buf.len() as u64);

        // Whole range inside the arena → in-bounds.
        assert!(is_guest_range(base, 16));
        assert!(is_guest_range(base + 12, 4)); // last 4 bytes exactly fit.
        // Base in-arena but the 4-byte range crosses the top → rejected (the closed gap).
        assert!(!is_guest_range(base + 13, 4));
        // Base exactly at the top is out of the arena.
        assert!(!is_guest_range(base + 16, 1));
        // A base below the arena is rejected.
        assert!(!is_guest_range(base - 1, 1));
        // Null base, zero len, and a length that overflows u64 are all rejected.
        assert!(!is_guest_range(0, 8));
        assert!(!is_guest_range(base, 0));
        assert!(!is_guest_range(base, u64::MAX));

        // The typed wrapper inherits the base+size check: a u32 one byte below the top overruns.
        assert!(!is_guest_ptr::<u32>((base + 13) as *const u32));
        assert!(is_guest_ptr::<u32>((base + 12) as *const u32));

        // Clear bounds so a later parallel-scheduled test doesn't inherit this buffer's window.
        set_arena_bounds(0, 0);
    }

    #[test]
    fn is_guest_range_fails_closed_with_no_arena() {
        let _t = lock();
        // No arena registered → every range is out of bounds (headless / unit test path).
        set_arena_bounds(0, 0);
        let stack = 0u64;
        assert!(!is_guest_range(&stack as *const u64 as u64, 8));
    }

    #[test]
    fn migrated_read_cstr_reads_name_through_seam() {
        let _t = lock();
        // Exercise the exact path the migrated pthread name reads now take: arena bounds set,
        // a single read seam wired, `read_cstr` scans up to the NUL. Only ONE `override_scoped`
        // guard is taken on the read seam in this scope (stacking two on the same seam would
        // re-lock its test-lock and deadlock — the guest_ptr.rs caveat).
        let mut buf = [0u8; 64];
        let s = b"worker-thread\0trailing-junk";
        buf[..s.len()].copy_from_slice(s);
        let base = buf.as_ptr() as u64;
        let end = base + buf.len() as u64;
        set_arena_bounds(base, buf.len() as u64);
        let _rg = read_source()
            .override_scoped(Arc::new(RegionRead { start: base, end }) as Arc<dyn BoundedRead>);

        // Valid guest C-string → scanned up to the NUL.
        assert_eq!(read_cstr(base, 256).as_deref(), Some("worker-thread"));
        // A junk / out-of-arena "name" (the POSIX-alias case) fails clean instead of crashing.
        assert_eq!(read_cstr(0x44, 256), None);
        assert_eq!(read_cstr(base - 1, 256), None);

        set_arena_bounds(0, 0);
    }

    #[test]
    fn migrated_get_status_struct_write_rejects_out_of_arena() {
        let _t = lock();
        // The migrated `sceSystemServiceGetStatus` zeroes a 0x28-byte status through a
        // range-validated, SMC-tracked GuestSlice. Prove: (a) a valid in-arena pointer is
        // zero-filled through the seam, and (b) a near-arena-top pointer whose 0x28-byte range
        // crosses the top is rejected clean — no panic, no host overrun, handler still returns 0
        // and the bytes past the arena top stay untouched.
        //
        // The buffer is 0x28 + a trailing guard byte; the arena is only the first 0x28 bytes, so
        // a status base one byte in overruns the arena top and must be refused.
        const N: usize = 0x28;
        let mut buf = [0xAAu8; N + 1];
        let base = buf.as_ptr() as u64;
        set_arena_bounds(base, N as u64); // arena = [base, base+0x28); guard byte is outside.
        let _wg = write_source().override_scoped(Arc::new(RegionWrite {
            start: base,
            end: base + N as u64,
        }) as Arc<dyn WriteGuest>);

        // Valid pointer: the 0x28 status is zeroed through the seam, guard byte untouched.
        assert_eq!(
            libkernel::systemservice::sce_system_service_get_status(base as *mut u8),
            0
        );
        assert!(buf[..N].iter().all(|&b| b == 0));
        assert_eq!(buf[N], 0xAA, "write must not spill past the struct");

        // Near-arena-top pointer: base+1 gives a 0x28 range crossing the arena top → rejected
        // clean. No panic, handler still returns 0, and the guard byte past the top is untouched.
        buf = [0xAAu8; N + 1];
        assert_eq!(
            libkernel::systemservice::sce_system_service_get_status((base + 1) as *mut u8),
            0
        );
        assert_eq!(buf[N], 0xAA, "out-of-arena out-param must not be written");

        set_arena_bounds(0, 0);
    }

    #[test]
    fn migrated_flip_status_struct_write_rejects_out_of_arena() {
        let _t = lock();
        // The migrated `sceVideoOutGetFlipStatus` writes a 64-byte status (flipArg @ +24) through
        // a range-validated GuestSlice. Prove the same clean-rejection property for a 64-byte
        // out-param: valid pointer writes through the seam; a near-arena-top pointer is refused
        // without panicking or overrunning.
        const N: usize = 64;
        let mut buf = [0xAAu8; N + 1];
        let base = buf.as_ptr() as u64;
        set_arena_bounds(base, N as u64);
        let _wg = write_source().override_scoped(Arc::new(RegionWrite {
            start: base,
            end: base + N as u64,
        }) as Arc<dyn WriteGuest>);

        // Valid pointer: 64-byte status written through the seam (zeroed body; flipArg==0 here),
        // guard byte untouched.
        assert_eq!(
            libscevideoout::sce_video_out_get_flip_status(0, base as *mut u8),
            0
        );
        assert!(buf[..N].iter().all(|&b| b == 0));
        assert_eq!(buf[N], 0xAA, "write must not spill past the struct");

        // A base one byte in gives a 64-byte range crossing the arena top → rejected clean.
        buf = [0xAAu8; N + 1];
        assert_eq!(
            libscevideoout::sce_video_out_get_flip_status(0, (base + 1) as *mut u8),
            0
        );
        assert_eq!(buf[N], 0xAA, "out-of-arena out-param must not be written");

        set_arena_bounds(0, 0);
    }
}
