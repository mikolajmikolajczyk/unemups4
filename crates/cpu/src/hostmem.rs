//! Host memory reservation for the identity-mapped guest arena.
//!
//! This is a self-contained replica of `x86jit_linux::hostmem::reserve_at` (studied
//! from an earlier spike). We deliberately do **not** depend on the `x86jit-linux`
//! crate: that crate is the full Linux userland *embedder* — its `lib.rs` pulls in
//! `shim.rs` (~146 KiB syscall shim), `thread.rs`, `proc.rs` and `sigsegv.rs`, none of
//! which ps4-cpu wants (unemups4 has its own HLE, threading and loader). `reserve_at`
//! is a ~30-line `mmap` wrapper, so replicating it here keeps the dependency surface
//! to just `x86jit-core` + `libc` and avoids compiling the whole shim.

use x86jit_core::HostRam;

/// Reserve a sparse span at a **fixed** host address equal to `guest_base`, backing a
/// `Reserved` VM whose guest space is `[guest_base, span)` with **host == guest
/// identity mapping**: `ptr as u64 == guest_base`, so a guest address equals its own
/// host address and a raw guest pointer can be dereferenced directly by HLE/GPU code.
///
/// `mmap(guest_base, span - guest_base, RW, PRIVATE|ANON|NORESERVE|MAP_FIXED_NOREPLACE)`.
/// `NORESERVE` leaves untouched pages uncommitted (the 64 GiB arena costs no physical
/// memory until pages are written). `MAP_FIXED_NOREPLACE` places the mapping at exactly
/// `guest_base` **without** clobbering an existing mapping — it fails loudly rather than
/// relocating if the range is taken, so a layout collision is caught at boot.
///
/// Panics if `guest_base >= span`, if `guest_base` isn't page-aligned, or if the host
/// refuses the fixed mapping (a layout collision or a strict-overcommit kernel) — each
/// is an embedder configuration error, not guest input.
pub fn reserve_at(guest_base: u64, span: u64) -> HostRam {
    assert!(
        guest_base < span,
        "guest_base (0x{guest_base:x}) must be below the span top (0x{span:x})"
    );
    assert!(
        guest_base.is_multiple_of(4096),
        "guest_base (0x{guest_base:x}) must be page-aligned"
    );
    let len = (span - guest_base) as usize;
    // SAFETY: anonymous fixed mmap at `guest_base`; fd -1, offset 0. NORESERVE leaves
    // untouched pages uncommitted. MAP_FIXED_NOREPLACE fails (MAP_FAILED) rather than
    // relocating or clobbering if the range is already mapped. Checked below.
    let ptr = unsafe {
        libc::mmap(
            guest_base as *mut libc::c_void,
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE
                | libc::MAP_ANONYMOUS
                | libc::MAP_NORESERVE
                | libc::MAP_FIXED_NOREPLACE,
            -1,
            0,
        )
    };
    assert!(
        ptr != libc::MAP_FAILED,
        "mmap(0x{guest_base:x}, {len} bytes, FIXED_NOREPLACE|NORESERVE) failed: {}",
        std::io::Error::last_os_error()
    );
    // MAP_FIXED_NOREPLACE must honor the requested address exactly (an old kernel
    // lacking the flag could fall back to a hint and relocate — reject that so the
    // identity invariant can't silently break).
    assert_eq!(
        ptr as u64, guest_base,
        "MAP_FIXED_NOREPLACE returned 0x{:x}, not the requested guest_base 0x{guest_base:x}",
        ptr as u64
    );
    HostRam {
        ptr: ptr as *mut u8,
        len,
        guest_base,
        dtor: munmap_dtor(),
        protect: None,
    }
}

/// Destructor that `munmap`s the whole span when the backing `Memory` drops.
fn munmap_dtor() -> Box<dyn FnMut(*mut u8, usize) + Send> {
    Box::new(|ptr: *mut u8, len: usize| {
        // SAFETY: `ptr`/`len` are exactly the mapping produced by `reserve_at`.
        unsafe {
            libc::munmap(ptr as *mut libc::c_void, len);
        }
    })
}
