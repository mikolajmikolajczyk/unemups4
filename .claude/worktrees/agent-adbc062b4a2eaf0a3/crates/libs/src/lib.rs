// Syscall/HLE handlers take guest pointers — identity-mapped host addresses in
// the x86jit arena — and dereference them by design; that is the entire job of
// an HLE shim. They are only ever invoked by the generated #[ps4_syscall]
// dispatch with pointers the guest supplied, never as a general-purpose safe
// API, so `not_unsafe_ptr_arg_deref` (which would have us mark ~40 handlers
// `unsafe fn`, fighting the macro for no added safety) does not apply here.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

/// `NativeContext` now lives in ps4-cpu (doc-1 dec 3). This shim re-exports it under the
/// old `crate::context` path so every handler's `use crate::context::NativeContext;`
/// keeps compiling unchanged.
pub mod context {
    pub use ps4_cpu::{FromReg, NativeContext};
}
pub mod libkernel;
pub mod libs;
pub mod libscaudioout;
pub mod libscegnmdriver;
pub mod libscenet;
pub mod libscepad;
pub mod libsceuserservice;
pub mod libscevideoout;
pub mod registry;
extern crate self as ps4_libs;
pub use crate::context::NativeContext;
pub use registry::{get_handler, register_handler};
use std::panic;
use tracing::{debug_span, error};

pub fn init() {
    // Anchor to force linking of this crate's inventory registrations.
}

/// Is `ptr` a plausible guest pointer — i.e. inside the identity-mapped arena
/// `[GUEST_BASE, GUEST_BASE + DEFAULT_SPAN)`?
///
/// KNOWN LIMITATION (task-115): this guard is applied ad-hoc per handler; ~11 other
/// handlers still deref guest pointers unchecked. The systemic fix is a single
/// `read_guest_cstr(ptr) -> Option<String>` (range-check + bounded scan) used everywhere.
///
/// Handlers must check optional pointer
/// args (a debug name, an out-param) before dereferencing: under a POSIX alias the
/// guest often leaves junk in the "extra" argument register (a small integer, a huge
/// negative), and the JIT identity-maps guest pointers straight through, so reading
/// one segfaults the host instead of raising a guest fault. The whole arena is
/// `MAP_NORESERVE`-backed, so any in-range address is safe to read (zero pages).
#[inline]
pub fn is_guest_ptr<T>(ptr: *const T) -> bool {
    let p = ptr as u64;
    (ps4_cpu::guest_vm::GUEST_BASE..ps4_cpu::guest_vm::GUEST_BASE + ps4_cpu::guest_vm::DEFAULT_SPAN)
        .contains(&p)
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
        if let Some(handler) = ps4_libs::get_handler(id) {
            handler(ctx)
        } else {
            let sys_id = ps4_syscalls::SyscallId(id);
            error!("[SYSCALL] UNIMPLEMENTED: {} ({})", sys_id.as_str(), id);
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
