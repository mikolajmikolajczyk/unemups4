//! libkernel file syscalls.
//!
//! ## The sce-vs-posix error-ABI split (task-191)
//!
//! A guest imports these operations under two families of symbol names, and the two
//! families want OPPOSITE encodings of the same failure:
//!
//! * **sce** names (`sceKernelStat`, `sceKernelOpen`, …) → the SCE runtime expects a
//!   POSITIVE code `0x8002_0000 | posix_errno` (e.g. `ENOENT` -> `0x8002_0002`). Retail
//!   Mono runs `sce_to_errno(ret)`; a raw `-2` is not a valid SCE code and it crashes in
//!   an "unknown error" `%s`-fed-NULL formatter (this was Celeste's gameplay-entry crash).
//! * **posix** names (`stat`, `open`, …) → the OpenOrbis libc wrappers check the SIGN of
//!   the return; a NEGATIVE errno is the error path. A positive errno reads as a valid fd
//!   and corrupts the guest's stdio (task-101).
//!
//! A syscall stub carries only a `SyscallId`, so the handler cannot tell which import
//! name was used — the ONLY way to serve both ABIs is two distinct ids -> two stubs -> two
//! handlers. Each op therefore has a shared `*_impl` returning `Result<_, Errno>` (the
//! single positive-errno representation) plus two thin adapters: an `abi = sce` one on the
//! `SCE_KERNEL_*` id and an `abi = posix` one on the `SYS_*` id. The `#[ps4_syscall]` macro
//! projects the `Err(Errno)` into the requested ABI at expansion time. See
//! `ps4_core::errno` for the converter.
//!
//! Ops with NO distinct posix id (or that no posix alias currently imports) stay a single
//! handler — see `sce_kernel_write` below.

use crate::context::NativeContext;
use ps4_core::errno::Errno;
use ps4_core::guest_ptr::GuestSlice;
use ps4_core::kernel::get_kernel;
use ps4_macros::ps4_syscall;
use ps4_syscalls::SyscallId;
use tracing::{info, warn};

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Iovec {
    pub base: u64,  // void *
    pub len: usize, // size_t
}

// open(path, flags, mode)
fn open_impl(path: *const u8, flags: i32, mode: i32) -> Result<i32, Errno> {
    // task-115: harden the path read like the sibling path ops (stat_impl/mkdir_impl/unlink_impl).
    // The old raw `while *path.add(len)` scan + `slice::from_raw_parts` walked off host memory on
    // a junk non-null pointer (0x1) or an unterminated buffer near the arena top — a host SIGSEGV
    // that rust_syscall_handler's catch_unwind can't catch. `is_guest_ptr` + bounded `read_cstr`
    // fail clean with EFAULT and decode the same UTF-8-lossy string for a valid path.
    if !crate::is_guest_ptr(path) {
        return Err(Errno::EFAULT);
    }
    let Some(path_str) = ps4_core::guest_ptr::read_cstr(path as u64, 1024) else {
        return Err(Errno::EFAULT); // junk / unterminated guest path pointer
    };

    let Some(k) = get_kernel() else {
        return Err(Errno::EPERM);
    };
    info!(
        "[SYSCALL] sceKernelOpen('{}', flags={:#x}, mode={:#o})",
        path_str, flags, mode
    );
    match k.file_open(&path_str, flags, mode) {
        Ok(fd) => Ok(fd),
        Err(e) => Err(Errno(e)),
    }
}

#[ps4_syscall(id = SyscallId::SCE_KERNEL_OPEN, abi = sce, lib = crate::libs::LIB_KERNEL, name = "sceKernelOpen")]
pub fn sce_kernel_open(path: *const u8, flags: i32, mode: i32) -> Result<i32, Errno> {
    open_impl(path, flags, mode)
}

#[ps4_syscall(id = SyscallId::SYS_OPEN, abi = posix, lib = crate::libs::LIB_KERNEL, names = ["open", "_open"])]
pub fn posix_open(path: *const u8, flags: i32, mode: i32) -> Result<i32, Errno> {
    open_impl(path, flags, mode)
}

// stat(path, buf): the Mono runtime stats its managed assemblies (Celeste.exe,
// mscorlib.dll, …) before loading them. Resolve + fill the SceKernelStat fields the
// runtime reads: st_mode (@0x08, u16) and st_size (@0x48, i64), plus nlink/blocks/blksize.
// Offsets per the FreeBSD-derived SceKernelStat layout.
// KNOWN LIMITATION (task-118): hardcoded magic offsets, no typed struct; fields we don't
// fill (st_mtim/st_ino) read stale — replace with a #[repr(C)] SceKernelStat + asserts.
fn stat_impl(path: *const u8, stat_buf: *mut u8) -> Result<i32, Errno> {
    if !crate::is_guest_ptr(path) || !crate::is_guest_ptr(stat_buf) {
        return Err(Errno::EFAULT);
    }
    let Some(path_str) = ps4_core::guest_ptr::read_cstr(path as u64, 1024) else {
        return Err(Errno::EFAULT); // junk / unterminated guest path pointer
    };
    let Some(k) = get_kernel() else {
        return Err(Errno::EPERM); // preserves the old 0x80020001 for the sce path
    };
    match k.file_stat(&path_str) {
        Ok((is_dir, size)) => {
            fill_sce_stat(stat_buf, is_dir, size);
            Ok(0)
        }
        Err(e) => {
            // Name the path on failure. A stat that returns ENOENT is a normal answer, but it
            // is also the last thing the guest learns before an error path it may not survive:
            // Celeste's entry into gameplay stats a file, gets ENOENT, and then crashes
            // formatting the message (`vasprintf` -> `vsnprintf` -> `strlen(NULL)`, a `%s` fed
            // a null pointer) — but only when it receives the WRONG ABI's code. Without the
            // path the breadcrumb shows the failing call but not which file it was about.
            tracing::warn!("[FS] stat('{path_str}') failed: errno {e}");
            Err(Errno(e))
        }
    }
}

#[ps4_syscall(id = SyscallId::SCE_KERNEL_STAT, abi = sce, lib = crate::libs::LIB_KERNEL, name = "sceKernelStat")]
pub fn sce_kernel_stat(path: *const u8, stat_buf: *mut u8) -> Result<i32, Errno> {
    stat_impl(path, stat_buf)
}

// task-191 RESOLVED: the retail Celeste crash path imports POSIX `stat` (not
// sceKernelStat) and IGNORES the return value — it reads `*__error()` (the guest errno
// TLS slot) instead. The `abi = posix` macro arm writes that slot via
// `ps4_cpu::set_errno` before returning `-errno`, so Mono's wrapper reads errno==2
// (ENOENT) and takes its graceful branch instead of crashing in the "unknown error"
// `%s`-fed-NULL formatter.
#[ps4_syscall(id = SyscallId::SYS_STAT, abi = posix, lib = crate::libs::LIB_KERNEL, names = ["stat", "_stat"])]
pub fn posix_stat(path: *const u8, stat_buf: *mut u8) -> Result<i32, Errno> {
    stat_impl(path, stat_buf)
}

/// Fill a SceKernelStat buffer with the fields the runtime reads. Offsets per the
/// FreeBSD-derived layout: st_mode(0x08 u16), st_nlink(0x0a), st_size(0x48 i64),
/// st_blocks(0x50), st_blksize(0x58). KNOWN LIMITATION (task-118): magic offsets, no
/// typed struct; fields we don't fill (st_mtim/st_ino) read as zero.
fn fill_sce_stat(buf: *mut u8, is_dir: bool, size: u64) {
    // Build the 120-byte struct locally (zero-init + scattered fields), then write it in one
    // range-validated, SMC-tracked shot (task-115): a bad/near-arena-top pointer fails clean
    // instead of overrunning host memory. `buf.is_null()` is subsumed by the constructor.
    let mut stat = [0u8; 120];
    let mode: u16 = if is_dir {
        0x4000 | 0o755
    } else {
        0x8000 | 0o644
    };
    stat[0x08..0x0a].copy_from_slice(&mode.to_le_bytes());
    stat[0x0a..0x0c].copy_from_slice(&1u16.to_le_bytes()); // st_nlink
    stat[0x48..0x50].copy_from_slice(&(size as i64).to_le_bytes()); // st_size
    stat[0x50..0x58].copy_from_slice(&(size.div_ceil(512) as i64).to_le_bytes()); // st_blocks
    stat[0x58..0x5c].copy_from_slice(&0x4000u32.to_le_bytes()); // st_blksize
    if let Some(gs) = GuestSlice::<u8>::new(buf as u64, 120) {
        let _ = gs.write_slice(&stat);
    }
}

// mkdir(path, mode)
fn mkdir_impl(path: *const u8, mode: i32) -> Result<i32, Errno> {
    if path.is_null() {
        return Err(Errno::EFAULT);
    }
    let Some(path_str) = ps4_core::guest_ptr::read_cstr(path as u64, 1024) else {
        return Err(Errno::EFAULT); // junk / unterminated guest path pointer
    };

    let Some(k) = get_kernel() else {
        return Err(Errno::EPERM);
    };
    info!("[SYSCALL] sceKernelMkdir('{}', mode={:#o})", path_str, mode);
    match k.file_mkdir(&path_str, mode) {
        Ok(_) => Ok(0),
        Err(e) => Err(Errno(e)),
    }
}

#[ps4_syscall(id = SyscallId::SCE_KERNEL_MKDIR, abi = sce, lib = crate::libs::LIB_KERNEL, name = "sceKernelMkdir")]
pub fn sce_kernel_mkdir(path: *const u8, mode: i32) -> Result<i32, Errno> {
    mkdir_impl(path, mode)
}

#[ps4_syscall(id = SyscallId::SYS_MKDIR, abi = posix, lib = crate::libs::LIB_KERNEL, names = ["mkdir", "_mkdir"])]
pub fn posix_mkdir(path: *const u8, mode: i32) -> Result<i32, Errno> {
    mkdir_impl(path, mode)
}

// rmdir(path)
fn rmdir_impl(path: *const u8) -> Result<i32, Errno> {
    if path.is_null() {
        return Err(Errno::EFAULT);
    }
    let Some(path_str) = ps4_core::guest_ptr::read_cstr(path as u64, 1024) else {
        return Err(Errno::EFAULT); // junk / unterminated guest path pointer
    };

    let Some(k) = get_kernel() else {
        return Err(Errno::EPERM);
    };
    info!("[SYSCALL] sceKernelRmdir('{}')", path_str);
    match k.file_rmdir(&path_str) {
        Ok(_) => Ok(0),
        Err(e) => Err(Errno(e)),
    }
}

#[ps4_syscall(id = SyscallId::SCE_KERNEL_RMDIR, abi = sce, lib = crate::libs::LIB_KERNEL, name = "sceKernelRmdir")]
pub fn sce_kernel_rmdir(path: *const u8) -> Result<i32, Errno> {
    rmdir_impl(path)
}

#[ps4_syscall(id = SyscallId::SYS_RMDIR, abi = posix, lib = crate::libs::LIB_KERNEL, names = ["rmdir", "_rmdir"])]
pub fn posix_rmdir(path: *const u8) -> Result<i32, Errno> {
    rmdir_impl(path)
}

// unlink(path)
fn unlink_impl(path: *const u8) -> Result<i32, Errno> {
    if path.is_null() {
        return Err(Errno::EFAULT);
    }
    let Some(path_str) = ps4_core::guest_ptr::read_cstr(path as u64, 1024) else {
        return Err(Errno::EFAULT); // junk / unterminated guest path pointer
    };

    let Some(k) = get_kernel() else {
        return Err(Errno::EPERM);
    };
    info!("[SYSCALL] sceKernelUnlink('{}')", path_str);
    match k.file_unlink(&path_str) {
        Ok(_) => Ok(0),
        Err(e) => Err(Errno(e)),
    }
}

#[ps4_syscall(id = SyscallId::SCE_KERNEL_UNLINK, abi = sce, lib = crate::libs::LIB_KERNEL, name = "sceKernelUnlink")]
pub fn sce_kernel_unlink(path: *const u8) -> Result<i32, Errno> {
    unlink_impl(path)
}

#[ps4_syscall(id = SyscallId::SYS_UNLINK, abi = posix, lib = crate::libs::LIB_KERNEL, names = ["unlink", "_unlink"])]
pub fn posix_unlink(path: *const u8) -> Result<i32, Errno> {
    unlink_impl(path)
}

// rename(old, new)
fn rename_impl(old_path: *const u8, new_path: *const u8) -> Result<i32, Errno> {
    if old_path.is_null() || new_path.is_null() {
        return Err(Errno::EFAULT);
    }
    let Some(old_str) = ps4_core::guest_ptr::read_cstr(old_path as u64, 1024) else {
        return Err(Errno::EFAULT); // junk / unterminated guest path pointer
    };
    let Some(new_str) = ps4_core::guest_ptr::read_cstr(new_path as u64, 1024) else {
        return Err(Errno::EFAULT); // junk / unterminated guest path pointer
    };

    let Some(k) = get_kernel() else {
        return Err(Errno::EPERM);
    };
    info!("[SYSCALL] sceKernelRename('{}', '{}')", old_str, new_str);
    match k.file_rename(&old_str, &new_str) {
        Ok(_) => Ok(0),
        Err(e) => Err(Errno(e)),
    }
}

#[ps4_syscall(id = SyscallId::SCE_KERNEL_RENAME, abi = sce, lib = crate::libs::LIB_KERNEL, name = "sceKernelRename")]
pub fn sce_kernel_rename(old_path: *const u8, new_path: *const u8) -> Result<i32, Errno> {
    rename_impl(old_path, new_path)
}

#[ps4_syscall(id = SyscallId::SYS_RENAME, abi = posix, lib = crate::libs::LIB_KERNEL, names = ["rename", "_rename"])]
pub fn posix_rename(old_path: *const u8, new_path: *const u8) -> Result<i32, Errno> {
    rename_impl(old_path, new_path)
}

// close(fd)
fn close_impl(fd: i32) -> Result<i32, Errno> {
    let Some(k) = get_kernel() else {
        return Err(Errno::EPERM);
    };
    match k.file_close(fd) {
        Ok(_) => Ok(0),
        Err(e) => Err(Errno(e)),
    }
}

#[ps4_syscall(id = SyscallId::SCE_KERNEL_CLOSE, abi = sce, lib = crate::libs::LIB_KERNEL, name = "sceKernelClose")]
pub fn sce_kernel_close(fd: i32) -> Result<i32, Errno> {
    close_impl(fd)
}

#[ps4_syscall(id = SyscallId::SYS_CLOSE, abi = posix, lib = crate::libs::LIB_KERNEL, names = ["close"])]
pub fn posix_close(fd: i32) -> Result<i32, Errno> {
    close_impl(fd)
}

// read(fd, ptr, len)
fn read_impl(fd: i32, ptr: u64, len: usize) -> Result<isize, Errno> {
    let Some(k) = get_kernel() else {
        return Err(Errno::EPERM);
    };
    match k.file_read(fd, ptr, len) {
        Ok(bytes) => Ok(bytes as isize),
        Err(e) => Err(Errno(e)),
    }
}

#[ps4_syscall(id = SyscallId::SCE_KERNEL_READ, abi = sce, lib = crate::libs::LIB_KERNEL, name = "sceKernelRead")]
pub fn sce_kernel_read(fd: i32, ptr: u64, len: usize) -> Result<isize, Errno> {
    read_impl(fd, ptr, len)
}

#[ps4_syscall(id = SyscallId::SYS_READ, abi = posix, lib = crate::libs::LIB_KERNEL, names = ["read", "_read"])]
pub fn posix_read(fd: i32, ptr: u64, len: usize) -> Result<isize, Errno> {
    read_impl(fd, ptr, len)
}

// write(fd, ptr, len): NOT split (task-191). No posix alias currently imports this — the
// only registered name is the sce `sceKernelWrite` — so there is no posix caller whose
// sign convention would diverge, and inventing one is out of scope. Kept as a single
// handler with its established (negative-errno) behavior; the SYS_WRITE id is unused here.
#[ps4_syscall(id = SyscallId::SCE_KERNEL_WRITE, lib = crate::libs::LIB_KERNEL, name = "sceKernelWrite")]
pub fn sce_kernel_write(fd: i32, ptr: u64, len: usize) -> isize {
    if let Some(k) = get_kernel() {
        match k.file_write(fd, ptr, len) {
            Ok(bytes) => bytes as isize,
            Err(e) => -(e as isize),
        }
    } else {
        -1
    }
}

// writev(fd, iov, iovcnt)
fn writev_impl(fd: i32, iov_ptr: *const Iovec, iovcnt: i32) -> Result<isize, Errno> {
    if iov_ptr.is_null() || iovcnt < 0 {
        return Err(Errno::EINVAL);
    }

    let count = iovcnt as usize;
    if count == 0 {
        return Ok(0);
    }

    // Validate the iov array through the arena seam before touching it — never form a raw
    // slice over an unvalidated guest pointer. A junk or over-long iov_ptr (e.g. writev(fd,
    // 0x1, 1), or an iovcnt that runs past the mapped region) faults clean here instead of
    // dereferencing unmapped host memory (uncatchable SIGSEGV). Mirrors batch_map_impl (mman.rs).
    let Some(iovecs) = GuestSlice::<Iovec>::new(iov_ptr as u64, count).and_then(|s| s.read_vec())
    else {
        return Err(Errno::EFAULT);
    };

    let Some(k) = get_kernel() else {
        return Err(Errno::EPERM);
    };

    let mut total_written = 0;

    // real writev is atomic; this loop isn't (interleaved writes possible), good enough for HLE logging
    for iov in iovecs {
        if iov.len == 0 {
            continue;
        }

        match k.file_write(fd, iov.base, iov.len) {
            Ok(n) => {
                total_written += n;
                // If we wrote less than requested, usually we stop,
                // but for files it usually writes all unless disk full.
                if n < iov.len {
                    break;
                }
            }
            Err(e) => {
                if total_written > 0 {
                    return Ok(total_written as isize);
                }
                return Err(Errno(e));
            }
        }
    }

    Ok(total_written as isize)
}

#[ps4_syscall(id = SyscallId::SCE_KERNEL_WRITEV, abi = sce, lib = crate::libs::LIB_KERNEL, name = "sceKernelWritev")]
pub fn sce_kernel_writev(fd: i32, iov_ptr: *const Iovec, iovcnt: i32) -> Result<isize, Errno> {
    writev_impl(fd, iov_ptr, iovcnt)
}

#[ps4_syscall(id = SyscallId::SYS_WRITEV, abi = posix, lib = crate::libs::LIB_KERNEL, names = ["writev", "_writev"])]
pub fn posix_writev(fd: i32, iov_ptr: *const Iovec, iovcnt: i32) -> Result<isize, Errno> {
    writev_impl(fd, iov_ptr, iovcnt)
}

// getdents(fd, buf, nbytes): the Mono runtime enumerates the game's content dirs
// (Directory.GetFiles/GetDirectories -> readdir -> getdents) once assets start
// loading. Fills `buf` with FreeBSD `struct dirent` records; returns bytes written
// (0 == end of dir). Layout + semantics live in the fs backend (kernel/src/fs.rs).
fn getdents_impl(fd: i32, buf: u64, nbytes: u32) -> Result<i32, Errno> {
    if !crate::is_guest_ptr(buf as *const u8) {
        return Err(Errno::EFAULT);
    }
    let Some(k) = get_kernel() else {
        return Err(Errno::EPERM);
    };
    match k.file_getdents(fd, buf, nbytes as usize) {
        Ok(written) => Ok(written as i32),
        Err(e) => Err(Errno(e)),
    }
}

#[ps4_syscall(id = SyscallId::SCE_KERNEL_GETDENTS, abi = sce, lib = crate::libs::LIB_KERNEL, name = "sceKernelGetdents")]
pub fn sce_kernel_getdents(fd: i32, buf: u64, nbytes: u32) -> Result<i32, Errno> {
    getdents_impl(fd, buf, nbytes)
}

#[ps4_syscall(id = SyscallId::SYS_GETDENTS, abi = posix, lib = crate::libs::LIB_KERNEL, names = ["getdents"])]
pub fn posix_getdents(fd: i32, buf: u64, nbytes: u32) -> Result<i32, Errno> {
    getdents_impl(fd, buf, nbytes)
}

// lseek(fd, offset, whence)
fn lseek_impl(fd: i32, offset: i64, whence: i32) -> Result<i64, Errno> {
    let Some(k) = get_kernel() else {
        return Err(Errno::EPERM);
    };
    match k.file_lseek(fd, offset, whence) {
        Ok(pos) => Ok(pos as i64),
        Err(e) => Err(Errno(e)),
    }
}

/// `pread(fd, buf, nbytes, offset)`: read at an absolute offset without touching the fd's
/// shared cursor.
///
/// A title reaches for this precisely so several threads can read one file handle without
/// coordinating on its position — which is why the kernel side is a real positional read and
/// not seek-read-seek (see `FileSystem::pread`).
#[ps4_syscall(id = SyscallId::SCE_KERNEL_PREAD, abi = sce, lib = crate::libs::LIB_KERNEL, name = "sceKernelPread")]
pub fn sce_kernel_pread(fd: i32, buf: u64, nbytes: usize, offset: i64) -> i64 {
    pread_impl(fd, buf, nbytes, offset)
}

#[ps4_syscall(id = SyscallId::SYS_PREAD, abi = posix, lib = crate::libs::LIB_KERNEL, names = ["pread", "_pread"])]
pub fn posix_pread(fd: i32, buf: u64, nbytes: usize, offset: i64) -> i64 {
    pread_impl(fd, buf, nbytes, offset)
}

fn pread_impl(fd: i32, buf: u64, nbytes: usize, offset: i64) -> i64 {
    if offset < 0 {
        return -22; // EINVAL: a negative absolute offset is meaningless
    }
    let Some(k) = ps4_core::kernel::get_kernel() else {
        return -5; // EIO
    };
    match k.file_pread(fd, buf, nbytes, offset as u64) {
        Ok(n) => n as i64,
        Err(e) => -(e as i64),
    }
}

#[ps4_syscall(id = SyscallId::SCE_KERNEL_PWRITE, abi = sce, lib = crate::libs::LIB_KERNEL, name = "sceKernelPwrite")]
pub fn sce_kernel_pwrite(fd: i32, buf: u64, nbytes: usize, offset: i64) -> i64 {
    pwrite_impl(fd, buf, nbytes, offset)
}

#[ps4_syscall(id = SyscallId::SYS_PWRITE, abi = posix, lib = crate::libs::LIB_KERNEL, names = ["pwrite", "_pwrite"])]
pub fn posix_pwrite(fd: i32, buf: u64, nbytes: usize, offset: i64) -> i64 {
    pwrite_impl(fd, buf, nbytes, offset)
}

fn pwrite_impl(fd: i32, buf: u64, nbytes: usize, offset: i64) -> i64 {
    if offset < 0 {
        return -22; // EINVAL
    }
    let Some(k) = ps4_core::kernel::get_kernel() else {
        return -5; // EIO
    };
    match k.file_pwrite(fd, buf, nbytes, offset as u64) {
        Ok(n) => n as i64,
        Err(e) => -(e as i64),
    }
}

#[ps4_syscall(id = SyscallId::SCE_KERNEL_LSEEK, abi = sce, lib = crate::libs::LIB_KERNEL, name = "sceKernelLseek")]
pub fn sce_kernel_lseek(fd: i32, offset: i64, whence: i32) -> Result<i64, Errno> {
    lseek_impl(fd, offset, whence)
}

#[ps4_syscall(id = SyscallId::SYS_LSEEK, abi = posix, lib = crate::libs::LIB_KERNEL, names = ["lseek", "_lseek"])]
pub fn posix_lseek(fd: i32, offset: i64, whence: i32) -> Result<i64, Errno> {
    lseek_impl(fd, offset, whence)
}

// fstat(fd, buf): the Mono runtime fstats an assembly's fd to get its SIZE, then mmaps
// exactly that many bytes — a zeroed stat (st_size=0) made it map 0 bytes and reject the
// assembly as an invalid CIL image. Fill real fd metadata. stdio fds (0/1/2) or a bad fd
// fall back to a zeroed char-device-ish stat.
fn fstat_impl(fd: i32, stat_buf: *mut u8) -> Result<i32, Errno> {
    if !crate::is_guest_ptr(stat_buf) {
        return Err(Errno::EFAULT);
    }
    match get_kernel().and_then(|k| k.file_fstat(fd).ok()) {
        Some((is_dir, size)) => {
            fill_sce_stat(stat_buf, is_dir, size);
            Ok(0)
        }
        // Unknown/stdio fd: a zeroed stat (character-device-ish) — enough for non-file fds.
        None => {
            if let Some(gs) = GuestSlice::<u8>::new(stat_buf as u64, 120) {
                let _ = gs.zero();
            }
            Ok(0)
        }
    }
}

#[ps4_syscall(id = SyscallId::SCE_KERNEL_FSTAT, abi = sce, lib = crate::libs::LIB_KERNEL, name = "sceKernelFstat")]
pub fn sce_kernel_fstat(fd: i32, stat_buf: *mut u8) -> Result<i32, Errno> {
    fstat_impl(fd, stat_buf)
}

#[ps4_syscall(id = SyscallId::SYS_FSTAT, abi = posix, lib = crate::libs::LIB_KERNEL, names = ["fstat", "_fstat"])]
pub fn posix_fstat(fd: i32, stat_buf: *mut u8) -> Result<i32, Errno> {
    fstat_impl(fd, stat_buf)
}

#[ps4_syscall(id = SyscallId::SYS_FCNTL, lib = crate::libs::LIB_KERNEL, names = ["fcntl", "_fcntl"])]
pub fn sys_fcntl(_fd: i32, _cmd: i32, _arg: i64) -> i32 {
    warn!("fcntl stubbed!");
    0 // Stub: Success
}

// readv(fd, iov, iovcnt)
fn readv_impl(fd: i32, iov_ptr: *const Iovec, iovcnt: i32) -> Result<isize, Errno> {
    if iov_ptr.is_null() || iovcnt < 0 {
        return Err(Errno::EINVAL);
    }

    let count = iovcnt as usize;
    if count == 0 {
        return Ok(0);
    }

    // Validate the iov array through the arena seam before touching it — never form a raw
    // slice over an unvalidated guest pointer. A junk or over-long iov_ptr (e.g. readv(fd,
    // 0x1, 1), or an iovcnt that runs past the mapped region) faults clean here instead of
    // dereferencing unmapped host memory (uncatchable SIGSEGV). Mirrors batch_map_impl (mman.rs).
    let Some(iovecs) = GuestSlice::<Iovec>::new(iov_ptr as u64, count).and_then(|s| s.read_vec())
    else {
        return Err(Errno::EFAULT);
    };

    let Some(k) = get_kernel() else {
        return Err(Errno::EPERM);
    };

    let mut total_read = 0;

    for iov in iovecs {
        if iov.len == 0 {
            continue;
        }

        // Call the kernel's read function for each buffer
        match k.file_read(fd, iov.base, iov.len) {
            Ok(n) => {
                total_read += n;
                // If we read fewer bytes than requested (e.g. EOF), stop reading.
                if n < iov.len {
                    break;
                }
            }
            Err(e) => {
                // If we already read some data, return that count.
                // Otherwise return the error.
                if total_read > 0 {
                    return Ok(total_read as isize);
                }
                return Err(Errno(e));
            }
        }
    }

    Ok(total_read as isize)
}

#[ps4_syscall(id = SyscallId::SCE_KERNEL_READV, abi = sce, lib = crate::libs::LIB_KERNEL, name = "sceKernelReadv")]
pub fn sce_kernel_readv(fd: i32, iov_ptr: *const Iovec, iovcnt: i32) -> Result<isize, Errno> {
    readv_impl(fd, iov_ptr, iovcnt)
}

#[ps4_syscall(id = SyscallId::SYS_READV, abi = posix, lib = crate::libs::LIB_KERNEL, names = ["readv", "_readv"])]
pub fn posix_readv(fd: i32, iov_ptr: *const Iovec, iovcnt: i32) -> Result<isize, Errno> {
    readv_impl(fd, iov_ptr, iovcnt)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// task-115 regression: `open_impl` must fail clean (EFAULT) on a junk non-null path
    /// pointer instead of walking off host memory in the old raw `while *path.add(len)` scan.
    /// With no arena registered (this unit-test process) `is_guest_ptr` fails closed, so a low
    /// junk pointer takes the EFAULT branch before any deref — no host SIGSEGV. (0x1 is below
    /// any plausible arena base, so this holds even if a parallel test has an arena registered.)
    #[test]
    fn open_junk_path_faults_clean() {
        assert_eq!(
            open_impl(std::ptr::without_provenance(0x1), 0, 0),
            Err(Errno::EFAULT)
        );
    }

    /// Memory-safety regression: `writev_impl`/`readv_impl` must validate the guest iov array
    /// through the arena seam (`GuestSlice`) before touching it, so a junk non-null `iov_ptr`
    /// faults clean (EFAULT) instead of forming a raw `slice::from_raw_parts` over unmapped host
    /// memory and taking an uncatchable SIGSEGV. With no arena registered (this unit-test
    /// process) `GuestSlice::new` fails closed, so a low junk pointer never reaches a deref.
    /// (0x1 is below any plausible arena base, so this holds even if a parallel test registers one.)
    #[test]
    fn writev_readv_junk_iov_faults_clean() {
        let junk: *const Iovec = std::ptr::without_provenance(0x1);
        assert_eq!(writev_impl(1, junk, 1), Err(Errno::EFAULT));
        assert_eq!(readv_impl(0, junk, 1), Err(Errno::EFAULT));
    }
}
