pub mod events;
pub mod fs;
pub mod mman;
pub mod pthread;
pub mod sema;
pub mod systemservice;

use crate::context::NativeContext;
use ps4_macros::ps4_syscall;
use ps4_syscalls::SyscallId;
use std::{io::Write, thread, time::Duration};
use tracing::{error, info};

#[ps4_syscall(id = SyscallId::SYS_EXIT, lib = crate::libs::LIB_KERNEL, names = ["exit", "_exit"])]
pub fn sys_exit(code: i32) -> u64 {
    info!("[SYSCALL] exit({}) - Context switched successfully!", code);
    std::process::exit(code);
}

#[ps4_syscall(id = SyscallId::SYS_WRITE, lib = crate::libs::LIB_KERNEL, names = ["write", "_write"])]
pub fn sys_write(fd: i32, ptr: *const u8, len: usize) -> u64 {
    if (ptr as u64) < 0x1000 {
        return 0;
    }
    if len == 0 {
        return 0;
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

    let mut len = 0;
    while len < 2048 && unsafe { *ptr.add(len) } != 0 {
        len += 1;
    }

    let slice = unsafe { std::slice::from_raw_parts(ptr, len) };
    info!("{}", String::from_utf8_lossy(slice));
    0
}

#[ps4_syscall(id = SyscallId::SCE_KERNEL_GET_PROC_PARAM, lib = crate::libs::LIB_KERNEL, name = "sceKernelGetProcParam")]
pub fn sce_kernel_get_proc_param() -> u64 {
    // Absolute guest address of the eboot's SceKernelProcParam (published at load).
    // libc reads sceLibcParam out of it during module_start; 0 => none (homebrew).
    ps4_core::kernel::proc_param_addr()
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

// libc calls this during heap init with `out` = a descriptor struct it then reads:
// it stores out+0x10 into a global and dereferences it (`movq $0,(*out+0x10)`), so
// that field must be a valid writable guest pointer even when tracing is off. Point it
// at a small zeroed HLE object; leave the flag fields (out+0x0c, +0x18) as the caller's
// zeroed defaults. Returns 0 (success).
#[ps4_syscall(id = SyscallId::SCE_LIBC_HEAP_GET_TRACE_INFO, lib = crate::libs::LIB_KERNEL, name = "sceLibcHeapGetTraceInfo")]
pub fn sce_libc_heap_get_trace_info(out: u64) -> i32 {
    // Guard the out pointer: the guest can pass junk in this register under some call
    // paths, and the JIT identity-maps guest pointers straight through, so writing a
    // non-arena address segfaults the host (see ps4_libs::is_guest_ptr).
    if crate::is_guest_ptr(out as *const u8) {
        let buf = ps4_core::kernel::hle_alloc(0x100);
        unsafe {
            *((out + 0x10) as *mut u64) = buf;
        }
    }
    0
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
    if !crate::is_guest_ptr(buf) || size < cwd.len() {
        return -1;
    }
    unsafe {
        std::ptr::copy_nonoverlapping(cwd.as_ptr(), buf, cwd.len());
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

#[repr(C)]
pub struct Timespec {
    tv_sec: i64,
    tv_nsec: i64,
}

#[ps4_syscall(id = SyscallId::SYS_CLOCK_GETTIME, lib = crate::libs::LIB_KERNEL, names = ["clock_gettime", "_clock_gettime"])]
pub fn sys_clock_gettime(_clock_id: i32, tp: *mut Timespec) -> i32 {
    if tp.is_null() {
        return 14;
    } // EFAULT

    // Rust SystemTime to Unix Epoch
    let start = std::time::SystemTime::now();
    let since_epoch = start
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or(std::time::Duration::ZERO);

    unsafe {
        (*tp).tv_sec = since_epoch.as_secs() as i64;
        (*tp).tv_nsec = since_epoch.subsec_nanos() as i64;
    }
    0
}

#[ps4_syscall(id = SyscallId::SYS_CLOCK_GETRES, lib = crate::libs::LIB_KERNEL, names = ["clock_getres", "_clock_getres"])]
pub fn sys_clock_getres(_clock_id: i32, res: *mut Timespec) -> i32 {
    if (res as usize) >= 0x10000 {
        // 1 ns resolution — the runtime only needs a sane non-zero value.
        unsafe {
            (*res).tv_sec = 0;
            (*res).tv_nsec = 1;
        }
    }
    0
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
    if req.is_null() {
        return 14; // EFAULT
    }
    unsafe {
        let sec = (*req).tv_sec;
        let nsec = (*req).tv_nsec;
        // Limit max sleep to avoid freezing if guest passes garbage
        if sec < 0 || nsec < 0 {
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
