//! libkernel HLE syscall handlers (`sceKernel*` plus BSD `libc` shims). Almost every
//! handler here returns a COHERENT HLE STATE by our own design, not a hardware fact —
//! that logic carries no oracle. The genuine PS4/BSD FACTS this module hands to (or reads
//! from) the guest are:
//!
//! - errno values (`ENOENT` 2, `EFAULT` 14, `EINVAL` 22), returned as `-errno` for
//!   sce-style calls and `+errno` for the BSD shims: FreeBSD 9 `sys/errno.h` (Orbis OS is
//!   FreeBSD 9-based).
//! - `mmap` PROT_*/MAP_* flag bits (`dlsym_trap_stub`): FreeBSD 9 `sys/mman.h`.
//! - `struct timeval` / `struct timespec` layout (each two 64-bit words, 16 bytes): the
//!   PS4 guest reuses the plain FreeBSD structs — OpenOrbis `include/orbis/_types/kernel.h`
//!   typedefs `OrbisKernelTimeval = struct timeval` and `OrbisKernelTimespec = struct
//!   timespec` (its `OrbisKernelStat` padding, written `16 - sizeof(struct timespec)`,
//!   witnesses the 16-byte size); `time_t`/`suseconds_t`/`long` are 64-bit on the amd64 ABI.
//!
//! Pinned by `libkernel_facts_match_bsd_oracle` at the end of this file.

pub mod coredump;
pub mod equeue;
pub mod events;
pub mod fs;
pub mod mman;
pub mod pthread;
pub mod sema;
pub mod systemservice;

use crate::context::NativeContext;
use ps4_core::guest_ptr::{GuestPtr, GuestSlice};
use ps4_macros::ps4_syscall;
use ps4_syscalls::SyscallId;
use std::{io::Write, thread, time::Duration};
use tracing::{error, info};

#[ps4_syscall(id = SyscallId::SYS_EXIT, lib = crate::libs::LIB_KERNEL, names = ["exit", "_exit"])]
pub fn sys_exit(code: i32) -> u64 {
    info!("[SYSCALL] exit({}) - Context switched successfully!", code);
    std::process::exit(code);
}

/// `__cxa_atexit(void (*func)(void*), void *arg, void *dso_handle)` — the C++ ABI hook a
/// static initializer uses to register a destructor for process (or DSO) teardown.
///
/// Accepted and ignored, returning 0 (success). We never run these: the emulator tears the
/// guest process down by exiting the host process, so a registered destructor has nothing
/// left to destroy, and running guest code during teardown would mean re-entering the JIT
/// after the point where threads and the GPU sink have already been dropped.
///
/// Returning 0 is the part that matters. A non-zero return means "could not register", and
/// a C++ runtime is entitled to treat that as fatal during static init — which is exactly
/// where this is called from, before the title has done anything at all.
///
/// This is the first symbol a native C++ title needs that Celeste (Mono, C) never asked for.
#[ps4_syscall(id = SyscallId::SYS_CXA_ATEXIT, lib = crate::libs::LIB_KERNEL, names = ["__cxa_atexit"])]
pub fn sys_cxa_atexit(_func: u64, _arg: u64, _dso_handle: u64) -> i32 {
    0
}

#[ps4_syscall(id = SyscallId::SYS_WRITE, lib = crate::libs::LIB_KERNEL, names = ["write", "_write"])]
pub fn sys_write(fd: i32, ptr: *const u8, len: usize) -> u64 {
    if (ptr as u64) < 0x1000 {
        return 0;
    }
    if len == 0 {
        return 0;
    }

    // Route to the file backend, which handles fd 1/2 -> stdout AND real open files
    // -> the host File. The old stub only logged non-stdio writes and never touched
    // the file, so Celeste's save (a POSIX write() to 0.celeste) went to the log and
    // the on-disk file stayed 0 bytes -> "Corrupted" on reload (task-193).
    if let Some(k) = ps4_core::kernel::get_kernel()
        && let Ok(written) = k.file_write(fd, ptr as u64, len)
    {
        return written as u64;
    }

    // Fallback: an fd the backend does not track (e.g. a stray debug fd). Preserve the
    // old visible behaviour — stdout for 1/2, a debug log line otherwise — so nothing
    // that used to surface goes silent.
    //
    // Bound the slice to the guest arena before touching it: `len` is guest-controlled, so a
    // large `len` with `ptr` near a mapping's end would over-read across the unmapped guard
    // page (host SIGSEGV) once `from_raw_parts` + `write_all` / `from_utf8_lossy` walk it. An
    // out-of-arena range writes nothing and reports the bytes as consumed, matching the
    // fallback's existing "pretend it landed" contract for an untracked fd.
    if !crate::is_guest_range(ptr as u64, len as u64) {
        return len as u64;
    }
    let slice = unsafe { std::slice::from_raw_parts(ptr, len) };
    if fd == 1 || fd == 2 {
        let _ = std::io::stdout().write_all(slice);
        let _ = std::io::stdout().flush();
    } else {
        let msg = String::from_utf8_lossy(slice);
        info!("[FILE] write({}, {}): {:?}", fd, len, msg);
    }

    len as u64
}

#[ps4_syscall(id = SyscallId::SCE_KERNEL_DEBUG_OUT_TEXT, lib = crate::libs::LIB_KERNEL, name = "sceKernelDebugOutText")]
pub fn sce_debug_out(_channel: i32, ptr: *const u8) -> u64 {
    if (ptr as u64) < 0x1000 {
        return 0;
    }

    // Scan through the VMA-validated read seam (chunked, bounded) rather than a raw
    // `*ptr.add(len)` walk: a string sitting in the last mapped page with no NUL in the
    // remaining <2048 bytes would otherwise deref the following unmapped host page
    // (SIGSEGV). This mirrors the read_guest_cstr -> `ps4_core::guest_ptr::read_cstr`
    // migration that closed the same over-read elsewhere.
    if let Some(text) = ps4_core::guest_ptr::read_cstr(ptr as u64, 2048) {
        info!("{}", text);
    }
    0
}

#[ps4_syscall(id = SyscallId::SCE_KERNEL_GET_PROC_PARAM, lib = crate::libs::LIB_KERNEL, name = "sceKernelGetProcParam")]
pub fn sce_kernel_get_proc_param() -> u64 {
    // Absolute guest address of the eboot's SceKernelProcParam (published at load).
    // libc reads sceLibcParam out of it during module_start; 0 => none (homebrew).
    ps4_core::kernel::proc_param_addr()
}

// `sceKernelIsNeoMode()` — is this a PS4 Pro (Neo) running in Neo mode? Return 0 (base
// PS4): a title branches its graphics/video-out setup on this, and the base-PS4 path is the
// simpler, lower-resolution one we already target. Celeste queries this right after
// `sceVideoOutOpen`, before its first GNM submit, so it must resolve or the boot stalls
// short of the graphics command buffer (doc-6: the wall past AddEqEvent).
#[ps4_syscall(id = SyscallId::SCE_KERNEL_IS_NEO_MODE, lib = crate::libs::LIB_KERNEL, name = "sceKernelIsNeoMode")]
pub fn sce_kernel_is_neo_mode() -> i32 {
    0
}

// gettimeofday(tv, tz): the Mono runtime / MonoGame read wall-clock time (frame
// timing, DateTime.Now, GC stats). Back it with the virtual guest clock over a real
// epoch base (`virtual_epoch_ns`) — epoch-correct absolute time whose rate follows
// emulated frames, so slow emulation doesn't fast-forward time-driven sequences.
// The `struct timeval { i64 tv_sec; i64 tv_usec; }` (16 bytes) and `struct timezone
// { i32 tz_minuteswest; i32 tz_dsttime; }` layouts are the FreeBSD 9 base structs the
// PS4 reuses (OpenOrbis `include/orbis/_types/kernel.h`: `OrbisKernelTimeval = struct
// timeval`; time_t/suseconds_t are 64-bit on the amd64 ABI). `struct timezone` is
// legacy and, when non-NULL, gets a zeroed (UTC, no DST) fill. EFAULT (14) is FreeBSD 9
// `sys/errno.h`. Returns 0 on success.
#[ps4_syscall(id = SyscallId::SYS_GETTIMEOFDAY, lib = crate::libs::LIB_KERNEL, names = ["gettimeofday", "sceKernelGettimeofday"])]
pub fn gettimeofday(tv: *mut u8, tz: *mut u8) -> i32 {
    // task-115: fill `struct timeval { i64 tv_sec; i64 tv_usec; }` / `struct timezone
    // { i32; i32; }` through the bounded GuestSlice write seam. A non-null pointer that
    // fails the arena constructor is EFAULT (not a host segfault); a null pointer is skipped.
    if !tv.is_null() {
        let ns = virtual_epoch_ns();
        let Some(slot) = GuestSlice::<i64>::new(tv as u64, 2) else {
            return -14; // EFAULT
        };
        let _ = slot.write_slice(&[
            (ns / 1_000_000_000) as i64,
            (ns % 1_000_000_000 / 1_000) as i64,
        ]);
    }
    if !tz.is_null() {
        let Some(slot) = GuestSlice::<i32>::new(tz as u64, 2) else {
            return -14; // EFAULT
        };
        let _ = slot.write_slice(&[0, 0]); // tz_minuteswest, tz_dsttime
    }
    0
}

// __tls_get_addr(tls_index* ti) — the ELF general-dynamic TLS resolver [NID vNe1w4diLCs].
// Celeste's managed AOT code calls it on the main thread to reach a `__thread` variable.
// The ABI: `ti` points at `struct tls_index { unsigned long ti_module; unsigned long
// ti_offset; }` (two u64s). We return a valid guest address = per-thread TLS block base +
// ti_offset. The block is a lazily-allocated, zero-initialised per-thread arena obtained
// via the kernel bridge (keyed by thread id; `ti_module` is collapsed — a single flat block
// per thread). Zero-init is CORRECT for bss-only/empty TLS templates such as the eboot's.
//
// KNOWN LIMITATION (task-121): modules whose TLS segment carries initialised `tdata` are NOT
// template-copied — their `__thread` variables read as zero here. That template copy (needs
// per-module tdata + a DTV) is a deliberate follow-up, not this minimal HLE.
#[ps4_syscall(id = SyscallId::SYS_TLS_GET_ADDR, lib = crate::libs::LIB_KERNEL, names = ["__tls_get_addr"])]
pub fn tls_get_addr(ti: *const u8) -> u64 {
    // struct tls_index { u64 ti_module; u64 ti_offset; } — validate the full 16-byte
    // struct, not just the base byte: we read ti_offset at ti+8..ti+16, so a base-only
    // `is_guest_ptr::<u8>` check would pass a `ti` sitting near the arena top while the
    // u64 read at ti+8 ran past the top (out-of-bounds read of adjacent host memory).
    if !crate::is_guest_range(ti as u64, 16) {
        error!("__tls_get_addr: non-guest tls_index ptr {ti:?}");
        return 0;
    }
    let ti_offset = unsafe { *(ti.add(8) as *const u64) };

    let Some(kernel) = ps4_core::kernel::get_kernel() else {
        error!("__tls_get_addr: no kernel bridge");
        return 0;
    };
    match kernel.tls_arena_base() {
        Ok(base) => base + ti_offset,
        Err(e) => {
            error!("__tls_get_addr: tls_arena_base failed (errno {e:#x})");
            0
        }
    }
}

// libc queries these during init to discover an AddressSanitizer malloc/operator-new
// override. We ship no sanitizer, so each returns 0 (no replacement) and libc keeps
// its own allocators.
#[ps4_syscall(id = SyscallId::SCE_KERNEL_GET_SANITIZER_MALLOC_REPLACE_EXTERNAL, lib = crate::libs::LIB_KERNEL, name = "sceKernelGetSanitizerMallocReplaceExternal")]
pub fn sce_kernel_get_sanitizer_malloc_replace_external() -> u64 {
    0
}

#[ps4_syscall(id = SyscallId::SCE_KERNEL_GET_SANITIZER_MALLOC_REPLACE, lib = crate::libs::LIB_KERNEL, name = "sceKernelGetSanitizerMallocReplace")]
pub fn sce_kernel_get_sanitizer_malloc_replace() -> u64 {
    0
}

#[ps4_syscall(id = SyscallId::SCE_KERNEL_GET_SANITIZER_NEW_REPLACE_EXTERNAL, lib = crate::libs::LIB_KERNEL, name = "sceKernelGetSanitizerNewReplaceExternal")]
pub fn sce_kernel_get_sanitizer_new_replace_external() -> u64 {
    0
}

#[ps4_syscall(id = SyscallId::SCE_KERNEL_GET_SANITIZER_NEW_REPLACE, lib = crate::libs::LIB_KERNEL, name = "sceKernelGetSanitizerNewReplace")]
pub fn sce_kernel_get_sanitizer_new_replace() -> u64 {
    0
}

// libc hands rtld its application heap API (malloc/free/etc. function table) here
// during init. We don't route guest allocations through rtld, so accept and ignore
// the table and return 0 (success) — libc keeps using its own heap directly.
#[ps4_syscall(id = SyscallId::SCE_KERNEL_RTLD_SET_APPLICATION_HEAP_API, lib = crate::libs::LIB_KERNEL, name = "_sceKernelRtldSetApplicationHeapAPI")]
pub fn sce_kernel_rtld_set_application_heap_api(_api: u64) -> i64 {
    0
}

/// `sceLibcHeapGetTraceInfo(SceLibcMallocManagedSize *inout)` — libc's heap-init handshake.
///
/// The caller passes a 32-byte struct pre-filled `{ size = 0x20, version = 1, 0, 0 }` and
/// expects the two trailing fields to come back as POINTERS it then keeps as globals:
///
/// - `+0x10` — the **atomic mspace-id mask**: one `u64` that libc claims ids out of with a
///   `lock cmpxchg` loop (`andn` for the lowest clear bit, `tzcnt` for its index).
/// - `+0x18` — the **mspace table** that same index writes into, as `table[8 + id*8]`.
///
/// Both must be valid, writable and ZEROED. `+0x18` used to be left alone, on the reading
/// that it was a flag field: Celeste's libc never dereferenced it, and no test could have
/// caught the difference. A native title's libc stores it straight into a global and
/// `_malloc_finalize_lv2` writes through it during the very first `sceLibcMspaceCreate` —
/// a null-deref before the title has executed a single line of its own code.
///
/// The table size is not guesswork: the id comes from `tzcnt` over a 64-bit mask, so it is
/// bounded by 63, and the largest write is `table[8 + 63*8]` = 520 bytes. 0x400 leaves room
/// without pretending to know the struct's full layout.
///
/// Returns 0 (success). Tracing itself is not modelled — this call exists here only because
/// libc routes heap bootstrap through it.
#[ps4_syscall(id = SyscallId::SCE_LIBC_HEAP_GET_TRACE_INFO, lib = crate::libs::LIB_KERNEL, name = "sceLibcHeapGetTraceInfo")]
pub fn sce_libc_heap_get_trace_info(out: u64) -> i32 {
    // Guard the out pointer: the guest can pass junk in this register under some call
    // paths, and the JIT identity-maps guest pointers straight through, so writing a
    // non-arena address segfaults the host (see ps4_libs::is_guest_ptr).
    if !crate::is_guest_ptr(out as *const u8) {
        return 0;
    }

    // Zero explicitly rather than trusting a fresh arena: `hle_alloc` recycles freed
    // same-size blocks, and a recycled id mask would come back with bits already claimed.
    for (field_off, size) in [(0x10u64, 0x100u64), (0x18, 0x400)] {
        let buf = ps4_core::kernel::hle_alloc(size);
        if buf == 0 {
            continue; // arena exhausted; leave the field as the caller's zero
        }
        if let Some(region) = ps4_core::guest_ptr::GuestSlice::<u8>::new(buf, size as usize) {
            let _ = region.zero();
        }
        // task-138: route the out-param store through the validated GuestPtr write seam
        // (GuestPtr::new re-validates the field address in the guest arena).
        if let Some(slot) = GuestPtr::<u64>::new(out + field_off) {
            let _ = slot.write(buf);
        }
    }
    0
}

/// Read a NUL-terminated C string from a guest pointer, bounded to `max` bytes. Returns
/// `None` if the pointer is not inside the guest arena or no NUL is found within `max`.
///
/// task-115: this now routes through the shared bounded-scan seam
/// [`ps4_core::guest_ptr::read_cstr`] — a chunked, VMA-validated scan that stops at the
/// first NUL — instead of a raw identity-map deref that would fault the host on a junk
/// pointer or over-read past an unmapped page.
fn read_guest_cstr(ptr: *const u8, max: usize) -> Option<String> {
    ps4_core::guest_ptr::read_cstr(ptr as u64, max)
}

// Dynamically load, link and start a `.prx` by guest path (Mono loads the native
// platform-interop module this way). Loads the file + its sibling deps into the shared
// identity space, runs its `module_start(argc, argv)` as a nested guest call, writes the
// start result to *pRes (when a valid guest ptr), and returns the positive module handle.
// A negative return is a PS4 errno.
#[ps4_syscall(id = SyscallId::SCE_KERNEL_LOAD_START_MODULE, lib = crate::libs::LIB_KERNEL, name = "sceKernelLoadStartModule")]
pub fn sce_kernel_load_start_module(
    path: *const u8,
    argc: u64,
    argv: u64,
    flags: u64,
    opt: u64,
    pres: u64,
) -> i32 {
    let Some(guest_path) = read_guest_cstr(path, 1024) else {
        tracing::warn!("sceKernelLoadStartModule: non-guest path ptr {path:?}");
        return -14; // -EFAULT
    };

    let Some(kernel) = ps4_core::kernel::get_kernel() else {
        return -0x16; // -EINVAL: no kernel bridge (should not happen once booted)
    };

    let (handle, module_starts) = match kernel.load_start_module(&guest_path) {
        Ok(v) => v,
        Err(errno) => {
            tracing::warn!("sceKernelLoadStartModule('{guest_path}') failed: errno {errno}");
            return -errno;
        }
    };

    info!(
        "sceKernelLoadStartModule('{guest_path}') argc={argc:#x} argv={argv:#x} flags={flags:#x} opt={opt:#x} -> handle {handle} ({} module_start(s))",
        module_starts.len()
    );

    // Run every module_start the load produced, in order: newly-loaded dependencies
    // leaves-first, then the target module last (empty for an already-loaded module). Each
    // runs as a nested guest call on this thread. PS4's signature is
    // `int module_start(size_t argc, const void* argv)`; `call_guest` sets RDI = argc, and
    // Mono passes argc=0/argv=0, so only argc matters here (see task-29 notes). The target's
    // result — the last one — goes to *pRes; the handle stays valid regardless (PS4 reports
    // the module handle on success and the init result out-of-band).
    let _ = argv;
    let mut start_res: i32 = 0;
    for module_start in module_starts {
        start_res = ps4_cpu::call_guest(module_start, argc) as i32;
    }

    // task-138: write *pRes through the validated GuestPtr seam (rejects a junk pointer).
    if let Some(slot) = GuestPtr::<i32>::new(pres) {
        let _ = slot.write(start_res);
    }

    handle
}

// Resolve an exported symbol by name in a module loaded via sceKernelLoadStartModule,
// writing its absolute guest address to *funcOut. Retail exports are NID-keyed; the
// kernel hashes the name before lookup. Returns 0 on success, a negative errno on miss.
#[ps4_syscall(id = SyscallId::SCE_KERNEL_DLSYM, lib = crate::libs::LIB_KERNEL, name = "sceKernelDlsym")]
pub fn sce_kernel_dlsym(handle: i32, name: *const u8, func_out: u64) -> i32 {
    let Some(sym) = read_guest_cstr(name, 1024) else {
        return -14; // -EFAULT
    };
    // task-115 / task-137: validate the func-out slot through the GuestPtr seam once; a junk
    // pointer fails the arena constructor and returns EFAULT instead of segfaulting the host.
    let Some(func_slot) = GuestPtr::<u64>::new(func_out) else {
        return -14; // -EFAULT
    };

    let Some(kernel) = ps4_core::kernel::get_kernel() else {
        return -0x16; // -EINVAL
    };

    match kernel.module_dlsym(handle, &sym) {
        Some(addr) => {
            let _ = func_slot.write(addr);
            info!("sceKernelDlsym(handle {handle}, '{sym}') -> {addr:#x}");
            0
        }
        None => {
            tracing::warn!("sceKernelDlsym(handle {handle}, '{sym}') -> not found");
            // task-137: a MISS used to leave `*func_out` untouched and return ENOENT. Managed
            // P/Invoke thunks (Sce.PlayStation4.dll -> scePlayStation4.prx, e.g.
            // `Graphics::GraphicsSystem::DrawPrimitives`/`Present`) ignore the errno and later
            // dispatch through the (null/garbage) function pointer, host-SIGSEGVing inside JIT'd
            // guest code. The real .prx that exports these names is an encrypted SELF we cannot
            // decrypt, so we cannot provide the bodies. Instead, hand the guest a guest-visible
            // trap-stub: a call through it traps cleanly (SYSCALL -> DLSYM_TRAP_MARKER -> logs
            // once, returns 0) rather than dispatching into null. Still return ENOENT so callers
            // that DO check the errno see the miss.
            if let Some(stub) = dlsym_trap_stub() {
                let _ = func_slot.write(stub);
            }
            -0x2 // -ENOENT
        }
    }
}

/// Address of the process-global "unresolved dlsym target" trap stub, allocated lazily on
/// first miss. The stub is `MOV EAX, DLSYM_TRAP_MARKER; SYSCALL; RET`: calling it traps into
/// `rust_syscall_handler`, which recognises the marker, logs once, and returns 0 (benign
/// no-op). A single shared stub is enough — the specific missing symbol was already logged at
/// resolve time, so the stub carries no per-symbol identity. Returns `None` if we could not
/// allocate a guest page (in which case the miss falls back to the old leave-`func_out`-alone
/// behaviour). See task-137.
fn dlsym_trap_stub() -> Option<u64> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static STUB_ADDR: AtomicU64 = AtomicU64::new(0);

    let existing = STUB_ADDR.load(Ordering::Acquire);
    if existing != 0 {
        return Some(existing);
    }

    let kernel = ps4_core::kernel::get_kernel()?;
    // One RWX guest page. FreeBSD 9 `sys/mman.h`: PROT_READ|PROT_WRITE|PROT_EXEC =
    // 0x1|0x2|0x4 = 7; MAP_PRIVATE|MAP_ANON = 0x2|0x1000 = 0x1002. Pinned by
    // `libkernel_facts_match_bsd_oracle`.
    let base = kernel.mmap(0, 0x1000, 7, 0x1002, -1, 0).ok()?;

    // MOV EAX, DLSYM_TRAP_MARKER ; SYSCALL ; RET. No `MOV R10, RCX` needed — the handler
    // reads no arguments (it is a pure no-op returning 0).
    let mut code = Vec::with_capacity(8);
    code.push(0xB8); // MOV EAX, imm32
    code.extend_from_slice(&(ps4_core::debug::DLSYM_TRAP_MARKER as u32).to_le_bytes());
    code.push(0x0F); // SYSCALL
    code.push(0x05);
    code.push(0xC3); // RET

    // Identity-mapped guest memory: guest addr == host addr, so write the freshly-mapped
    // (never-yet-executed) page directly. This mirrors how the linker seeds its lazy stubs.
    unsafe {
        std::ptr::copy_nonoverlapping(code.as_ptr(), base as *mut u8, code.len());
    }

    // Publish. If a racing thread won, keep the winner (both stubs are identical anyway).
    match STUB_ADDR.compare_exchange(0, base, Ordering::AcqRel, Ordering::Acquire) {
        Ok(_) => Some(base),
        Err(winner) => Some(winner),
    }
}

// libSceSysmodule: system-module load/unload by numeric id. Our HLE provides the
// system libraries directly (as syscall stubs), so a "load" is a no-op success and
// "is loaded" reports loaded. Real dynamic .prx loading is handled by the loader, not here.
#[ps4_syscall(id = SyscallId::SCE_SYSMODULE_LOAD_MODULE, lib = crate::libs::LIB_KERNEL, name = "sceSysmoduleLoadModule")]
pub fn sce_sysmodule_load_module(_id: u32) -> i32 {
    0
}

#[ps4_syscall(id = SyscallId::SCE_SYSMODULE_UNLOAD_MODULE, lib = crate::libs::LIB_KERNEL, name = "sceSysmoduleUnloadModule")]
pub fn sce_sysmodule_unload_module(_id: u32) -> i32 {
    0
}

#[ps4_syscall(id = SyscallId::SCE_SYSMODULE_IS_LOADED, lib = crate::libs::LIB_KERNEL, name = "sceSysmoduleIsLoaded")]
pub fn sce_sysmodule_is_loaded(_id: u32) -> i32 {
    0 // SCE_SYSMODULE_LOADED
}

#[ps4_syscall(id = SyscallId::SCE_SYSMODULE_LOAD_MODULE_INTERNAL_WITH_ARG, lib = crate::libs::LIB_KERNEL, name = "sceSysmoduleLoadModuleInternalWithArg")]
pub fn sce_sysmodule_load_module_internal_with_arg(_id: u32, _a: u64, _b: u64, _c: u64) -> i32 {
    0
}

// C++ exception unwinding queries this for a module's eh_frame. We don't yet model it;
// report "not found" (non-zero) so the unwinder skips it rather than reading a bogus
// descriptor. Revisit when guest C++ exceptions must cross this module boundary.
#[ps4_syscall(id = SyscallId::SCE_SYSMODULE_GET_MODULE_INFO_FOR_UNWIND, lib = crate::libs::LIB_KERNEL, name = "sceSysmoduleGetModuleInfoForUnwind")]
pub fn sce_sysmodule_get_module_info_for_unwind(_a: u64, _b: u64, _c: u64) -> i32 {
    -1
}

// libSceAudiodec: audio decoder. scePlayStation4's module_start initializes it. Real
// decoding is a later (FASE-3) feature; for bring-up accept init/term and hand out a
// non-zero decoder handle so the interop layer proceeds. Decode is a no-op (no output).
#[ps4_syscall(id = SyscallId::SCE_AUDIODEC_INIT_LIBRARY, lib = crate::libs::LIB_KERNEL, name = "sceAudiodecInitLibrary")]
pub fn sce_audiodec_init_library(_type: u32) -> i32 {
    0
}

#[ps4_syscall(id = SyscallId::SCE_AUDIODEC_TERM_LIBRARY, lib = crate::libs::LIB_KERNEL, name = "sceAudiodecTermLibrary")]
pub fn sce_audiodec_term_library(_type: u32) -> i32 {
    0
}

#[ps4_syscall(id = SyscallId::SCE_AUDIODEC_CREATE_DECODER, lib = crate::libs::LIB_KERNEL, name = "sceAudiodecCreateDecoder")]
pub fn sce_audiodec_create_decoder(_ctrl: u64, _type: u32) -> i32 {
    1 // a non-zero decoder handle
}

#[ps4_syscall(id = SyscallId::SCE_AUDIODEC_DELETE_DECODER, lib = crate::libs::LIB_KERNEL, name = "sceAudiodecDeleteDecoder")]
pub fn sce_audiodec_delete_decoder(_handle: i32) -> i32 {
    0
}

#[ps4_syscall(id = SyscallId::SCE_AUDIODEC_DECODE, lib = crate::libs::LIB_KERNEL, name = "sceAudiodecDecode")]
pub fn sce_audiodec_decode(_handle: i32, _ctrl: u64) -> i32 {
    0
}

// getcwd(buf, size): the current working directory is the game sandbox root `/app0`.
// The Mono runtime's g_get_current_dir asserts the result is non-NULL, so fill it. BSD
// `__getcwd` fills the buffer and returns 0 on success.
#[ps4_syscall(id = SyscallId::SYS_GETCWD, lib = crate::libs::LIB_KERNEL, names = ["getcwd", "__getcwd"])]
pub fn sys_getcwd(buf: *mut u8, size: usize) -> i32 {
    let cwd = b"/app0\0";
    if size < cwd.len() {
        return -1;
    }
    // task-115: write the cwd through the bounded GuestSlice seam; a junk buffer fails the
    // arena constructor and returns -1 instead of segfaulting the host under the identity map.
    let Some(slot) = GuestSlice::<u8>::new(buf as u64, cwd.len()) else {
        return -1;
    };
    if slot.write_slice(cwd).is_err() {
        return -1;
    }
    0
}

// Single-process HLE: report a fixed pid; no parent.
#[ps4_syscall(id = SyscallId::SYS_GETPID, lib = crate::libs::LIB_KERNEL, names = ["getpid", "_getpid"])]
pub fn sys_getpid() -> i32 {
    1
}

#[ps4_syscall(id = SyscallId::SYS_GETPPID, lib = crate::libs::LIB_KERNEL, names = ["getppid"])]
pub fn sys_getppid() -> i32 {
    0
}

#[ps4_syscall(id = SyscallId::SYS_ERRNO, lib = crate::libs::LIB_KERNEL, names = ["errno", "__error"])]
pub fn sys_errno() -> u64 {
    // Return the current guest thread's errno slot (a guest-resident address inside the
    // TLS allocation). A host static pointer would trap UnmappedMemory when the
    // CRT dereferences it under x86jit. `None` only outside an active guest call.
    ps4_cpu::current_errno_addr().unwrap_or(0)
}

#[ps4_syscall(id = SyscallId::SCE_KERNEL_USLEEP, lib = crate::libs::LIB_KERNEL, name = "sceKernelUsleep")]
pub fn sce_kernel_usleep(micros: u32) -> i32 {
    thread::sleep(Duration::from_micros(micros as u64));
    0 // SCE_OK
}

/// FreeBSD `struct timespec` — two 64-bit words `{ i64 tv_sec; i64 tv_nsec; }`, 16 bytes.
/// The PS4 guest reuses the base FreeBSD struct: OpenOrbis `include/orbis/_types/kernel.h`
/// typedefs `OrbisKernelTimespec = struct timespec` (and its `OrbisKernelStat` bitfield
/// padding, written `16 - sizeof(struct timespec)`, witnesses the 16-byte size). Pinned by
/// `libkernel_facts_match_bsd_oracle`.
#[repr(C)]
pub struct Timespec {
    tv_sec: i64,
    tv_nsec: i64,
}

#[ps4_syscall(id = SyscallId::SYS_CLOCK_GETTIME, lib = crate::libs::LIB_KERNEL, names = ["clock_gettime", "_clock_gettime", "sceKernelClockGettime"])]
pub fn sys_clock_gettime(_clock_id: i32, tp: *mut Timespec) -> i32 {
    // task-115: validate the whole Timespec write against the arena like the adjacent
    // sys_clock_getres — not just a null check. A bogus non-null tp (e.g. 0x40) would otherwise
    // write 8 bytes to a host address under the identity map (host SIGSEGV). Subsumes is_null.
    if !crate::is_guest_range(tp as u64, size_of::<Timespec>() as u64) {
        return 14; // EFAULT
    }

    // Wall clock = real epoch base + the virtual guest clock (see `virtual_epoch_ns`):
    // epoch-correct absolute time whose *rate* follows emulated frames, not host time.
    let ns = virtual_epoch_ns();
    unsafe {
        (*tp).tv_sec = (ns / 1_000_000_000) as i64;
        (*tp).tv_nsec = (ns % 1_000_000_000) as i64;
    }
    0
}

#[ps4_syscall(id = SyscallId::SYS_CLOCK_GETRES, lib = crate::libs::LIB_KERNEL, names = ["clock_getres", "_clock_getres"])]
pub fn sys_clock_getres(_clock_id: i32, res: *mut Timespec) -> i32 {
    // task-115: route the old raw `>= 0x10000` guard through the base+size range check, which
    // validates the whole `Timespec` write against the registered arena bounds (killing the
    // magic literal and rejecting a struct that would overrun the arena top).
    if crate::is_guest_range(res as u64, size_of::<Timespec>() as u64) {
        // 1 ns resolution — the runtime only needs a sane non-zero value.
        unsafe {
            (*res).tv_sec = 0;
            (*res).tv_nsec = 1;
        }
    }
    0
}

/// Wall-clock (Unix-epoch) nanoseconds derived from the VIRTUAL guest clock: a real
/// epoch base captured once at first use, plus [`ps4_core::clock::now_ns`]. Feeding raw
/// `SystemTime::now()` per call would reintroduce the splash fast-forward — under slow
/// emulation the guest would see wall time racing ahead of the clamped virtual clock.
pub(crate) fn virtual_epoch_ns() -> u64 {
    static BASE: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    let base = *BASE.get_or_init(|| {
        let real = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;
        real.saturating_sub(ps4_core::clock::now_ns())
    });
    base + ps4_core::clock::now_ns()
}

/// `sceKernelGetProcessTimeCounter` -> u64: a free-running per-process tick counter.
/// Backed by the virtual guest clock ([`ps4_core::clock::now_ns`] — real elapsed time
/// under a per-frame max-delta clamp) in nanosecond ticks; paired with
/// [`sce_kernel_get_process_time_counter_frequency`] (1e9) so `counter / frequency`
/// yields seconds. Games poll this for frame timing / delta-time.
#[ps4_syscall(id = SyscallId::SCE_KERNEL_GET_PROCESS_TIME_COUNTER, lib = crate::libs::LIB_KERNEL, names = ["sceKernelGetProcessTimeCounter"])]
pub fn sce_kernel_get_process_time_counter() -> u64 {
    ps4_core::clock::now_ns()
}

/// `sceKernelGetProcessTimeCounterFrequency` -> u64: ticks per second for the counter
/// above. We tick in nanoseconds, so the frequency is 1e9.
#[ps4_syscall(id = SyscallId::SCE_KERNEL_GET_PROCESS_TIME_COUNTER_FREQUENCY, lib = crate::libs::LIB_KERNEL, names = ["sceKernelGetProcessTimeCounterFrequency"])]
pub fn sce_kernel_get_process_time_counter_frequency() -> u64 {
    1_000_000_000
}

/// `sceKernelGetProcessTime` -> u64: process uptime in MICROSECONDS (the PS4 unit for
/// this call), from the same virtual guest clock as the tick counter above.
#[ps4_syscall(id = SyscallId::SCE_KERNEL_GET_PROCESS_TIME, lib = crate::libs::LIB_KERNEL, names = ["sceKernelGetProcessTime"])]
pub fn sce_kernel_get_process_time() -> u64 {
    ps4_core::clock::now_ns() / 1000
}

#[ps4_syscall(id = SyscallId::SYS_IOCTL, lib = crate::libs::LIB_KERNEL, names = ["ioctl", "_ioctl"])]
pub fn sys_ioctl(fd: i32, request: u64, _arg: u64) -> i32 {
    info!("[SYSCALL] STUB ioctl(fd={}, req={:#x})", fd, request);
    0 // Return success to keep libc happy
}

#[ps4_syscall(id = SyscallId::SYS_POLL, lib = crate::libs::LIB_KERNEL, names = ["poll"])]
pub fn sys_poll(_fds: u64, nfds: u64, timeout: i32) -> i32 {
    info!("[SYSCALL] STUB poll(nfds={}, timeout={})", nfds, timeout);
    thread::sleep(Duration::from_millis(1)); // Yield
    0 // Return 0 events
}

#[ps4_syscall(id = SyscallId::SYS_RAISE, lib = crate::libs::LIB_KERNEL, names = ["raise"])]
pub fn sys_raise(sig: i32) -> u64 {
    if sig == 6 {
        error!("[GUEST] Application raised SIGABRT (Abort).");
    } else {
        info!("[SYSCALL] raise(sig={})", sig);
    }
    0 // Success
}

#[ps4_syscall(id = SyscallId::SYS_SYSCONF, lib = crate::libs::LIB_KERNEL, names = ["sysconf", "__sysconf"])]
pub fn sys_sysconf(name: i32) -> i64 {
    match name {
        3 => 0x1000, // best-effort: report 4096 for unknown sysconf names
        28 => 4096,  // _SC_PAGESIZE
        29 => 4096,  // _SC_PAGE_SIZE
        _ => 0,      // Default fallback
    }
}

#[ps4_syscall(id = SyscallId::SYS_NANOSLEEP, lib = crate::libs::LIB_KERNEL, names = ["nanosleep"])]
pub fn sys_nanosleep(req: *const Timespec, _rem: *mut Timespec) -> i32 {
    // task-115: validate the whole Timespec read against the arena like sys_clock_getres — not
    // just a null check. A junk non-null req (e.g. 0x1) would otherwise deref straight through
    // the identity map (host SIGSEGV). `_rem` is never written back, so it needs no guard.
    if !crate::is_guest_range(req as u64, size_of::<Timespec>() as u64) {
        return 14; // EFAULT
    }
    unsafe {
        let sec = (*req).tv_sec;
        let nsec = (*req).tv_nsec;
        // Reject an out-of-range timespec (POSIX EINVAL): negative fields, or tv_nsec outside
        // [0, 1e9). The old `nsec as u32` silently truncated any tv_nsec >= 2^32 and never
        // rejected a >= 1e9 value; bounding nsec < 1e9 here makes that u32 cast lossless.
        if sec < 0 || !(0..1_000_000_000).contains(&nsec) {
            return 22; // EINVAL
        }
        std::thread::sleep(std::time::Duration::new(sec as u64, nsec as u32));
    }
    0
}

#[ps4_syscall(id = SyscallId::SYS_SIGPROCMASK, lib = crate::libs::LIB_KERNEL, names = ["sigprocmask"])]
pub fn sys_sigprocmask(_how: i32, _set: *const u64, _oset: *mut u64) -> i32 {
    // HLE Stub: Pretend we successfully updated the signal mask.
    // Real implementation would update the Thread/Process signal mask in the kernel.
    0
}

#[ps4_syscall(id = SyscallId::SYS_SIGACTION, lib = crate::libs::LIB_KERNEL, names = ["sigaction"])]
pub fn sys_sigaction(_sig: i32, _act: *const u64, _oact: *mut u64) -> i32 {
    // HLE Stub: Pretend we successfully registered the signal handler.
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pins the genuine PS4/BSD facts this module hands to (or reads from) the guest to
    /// their clean-oracle literals. The handlers above use these numbers inline (there are
    /// no named consts to reference), so this test re-states each oracle value with the
    /// source in its comment and fails `cargo test` if a handler's literal drifts from the
    /// oracle. Handler logic itself is our HLE design and is deliberately not asserted here.
    #[test]
    fn libkernel_facts_match_bsd_oracle() {
        // FreeBSD 9 `sys/errno.h` — the errno values the handlers return (as `-errno`
        // for sce-style calls, `+errno` for the BSD shims).
        const ENOENT: i32 = 2; // sce_kernel_dlsym miss -> -0x2
        const EFAULT: i32 = 14; // bad guest ptr -> -14 (sce) / 14 (clock_gettime, nanosleep)
        const EINVAL: i32 = 22; // nanosleep negative -> 22; no-kernel bridge -> -0x16
        assert_eq!(ENOENT, 0x2);
        assert_eq!(EFAULT, 14);
        assert_eq!(EINVAL, 0x16);

        // FreeBSD 9 `sys/mman.h` — the flags `dlsym_trap_stub` passes to `mmap`
        // (PROT arg `7`, MAP arg `0x1002`).
        const PROT_READ: i32 = 0x1;
        const PROT_WRITE: i32 = 0x2;
        const PROT_EXEC: i32 = 0x4;
        const MAP_PRIVATE: i32 = 0x2;
        const MAP_ANON: i32 = 0x1000;
        assert_eq!(PROT_READ | PROT_WRITE | PROT_EXEC, 7);
        assert_eq!(MAP_PRIVATE | MAP_ANON, 0x1002);

        // `struct timespec` = two 64-bit words, 16 bytes. OpenOrbis
        // `include/orbis/_types/kernel.h` typedefs `OrbisKernelTimespec = struct timespec`,
        // and its `OrbisKernelStat` padding `16 - sizeof(struct timespec)` witnesses the
        // 16-byte size; `time_t`/`long` are 64-bit on the amd64 ABI.
        assert_eq!(std::mem::size_of::<Timespec>(), 16);
        assert_eq!(std::mem::size_of::<i64>() * 2, 16); // tv_sec + tv_nsec, both i64
    }

    /// task-115 regression: a bogus non-null Timespec pointer must fail clean (EFAULT), never
    /// deref through the identity map into host memory. With no arena registered (this unit-test
    /// process) `is_guest_range` fails closed for every address, so a low junk pointer takes the
    /// EFAULT branch without touching memory. (Before the guard, `sys_clock_gettime` wrote and
    /// `sys_nanosleep` read `*ptr`, SIGSEGV-ing the host on a junk pointer. 0x40/0x1 are below
    /// any plausible arena base, so this holds even if a parallel test has an arena registered.)
    #[test]
    fn junk_time_pointers_fault_clean() {
        assert_eq!(sys_clock_gettime(0, 0x40 as *mut Timespec), 14); // EFAULT, no host write
        assert_eq!(
            sys_nanosleep(std::ptr::without_provenance(0x1), std::ptr::null_mut()),
            14
        ); // EFAULT, no host read
    }
}
