use crate::context::NativeContext;
use ps4_core::kernel::get_kernel;
use ps4_macros::ps4_syscall;
use ps4_syscalls::SyscallId;
use std::slice;
use tracing::{info, warn};

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Iovec {
    pub base: u64,  // void *
    pub len: usize, // size_t
}

#[ps4_syscall(id = SyscallId::SCE_KERNEL_OPEN, lib = crate::libs::LIB_KERNEL, names = ["sceKernelOpen", "open", "_open"])]
pub fn sce_kernel_open(path: *const u8, flags: i32, mode: i32) -> i32 {
    if path.is_null() {
        return -14; // EFAULT
    }

    let path_str = unsafe {
        let mut len = 0;
        while *path.add(len) != 0 {
            len += 1;
        }
        let slice = slice::from_raw_parts(path, len);
        String::from_utf8_lossy(slice).to_string()
    };

    if let Some(k) = get_kernel() {
        info!(
            "[SYSCALL] sceKernelOpen('{}', flags={:#x}, mode={:#o})",
            path_str, flags, mode
        );
        match k.file_open(&path_str, flags, mode) {
            Ok(fd) => fd,
            // Negative errno on failure: the OpenOrbis POSIX wrapper treats
            // ret<0 as the error path (sets errno, returns -1). A positive
            // errno here reads as a valid fd and corrupts the guest's stdio
            // (e.g. +2 ENOENT aliases stderr) — see task-101.
            Err(e) => -e,
        }
    } else {
        0x80020001u32 as i32
    }
}

// stat(path, buf): the Mono runtime stats its managed assemblies (Celeste.exe,
// mscorlib.dll, …) before loading them. Resolve + fill the SceKernelStat fields the
// runtime reads: st_mode (@0x08, u16) and st_size (@0x48, i64), plus nlink/blocks/blksize.
// Offsets per the FreeBSD-derived SceKernelStat layout.
// KNOWN LIMITATION (task-118): hardcoded magic offsets, no typed struct; fields we don't
// fill (st_mtim/st_ino) read stale — replace with a #[repr(C)] SceKernelStat + asserts.
#[ps4_syscall(id = SyscallId::SCE_KERNEL_STAT, lib = crate::libs::LIB_KERNEL, names = ["sceKernelStat", "stat", "_stat"])]
pub fn sce_kernel_stat(path: *const u8, stat_buf: *mut u8) -> i32 {
    if !crate::is_guest_ptr(path) || !crate::is_guest_ptr(stat_buf) {
        return -14; // EFAULT
    }
    let path_str = unsafe { read_cstr(path) };
    let Some(k) = get_kernel() else {
        return 0x80020001u32 as i32;
    };
    match k.file_stat(&path_str) {
        Ok((is_dir, size)) => {
            fill_sce_stat(stat_buf, is_dir, size);
            0
        }
        Err(e) => -e,
    }
}

/// Fill a SceKernelStat buffer with the fields the runtime reads. Offsets per the
/// FreeBSD-derived layout: st_mode(0x08 u16), st_nlink(0x0a), st_size(0x48 i64),
/// st_blocks(0x50), st_blksize(0x58). KNOWN LIMITATION (task-118): magic offsets, no
/// typed struct; fields we don't fill (st_mtim/st_ino) read as zero.
fn fill_sce_stat(buf: *mut u8, is_dir: bool, size: u64) {
    if buf.is_null() {
        return;
    }
    unsafe {
        std::ptr::write_bytes(buf, 0, 120);
        let mode: u16 = if is_dir {
            0x4000 | 0o755
        } else {
            0x8000 | 0o644
        };
        *(buf.add(0x08) as *mut u16) = mode;
        *(buf.add(0x0a) as *mut u16) = 1; // st_nlink
        *(buf.add(0x48) as *mut i64) = size as i64; // st_size
        *(buf.add(0x50) as *mut i64) = size.div_ceil(512) as i64; // st_blocks
        *(buf.add(0x58) as *mut u32) = 0x4000; // st_blksize
    }
}

/// Reads a NUL-terminated guest C string from an identity-mapped pointer.
unsafe fn read_cstr(ptr: *const u8) -> String {
    unsafe {
        let mut len = 0;
        while *ptr.add(len) != 0 {
            len += 1;
        }
        let slice = slice::from_raw_parts(ptr, len);
        String::from_utf8_lossy(slice).to_string()
    }
}

#[ps4_syscall(id = SyscallId::SCE_KERNEL_MKDIR, lib = crate::libs::LIB_KERNEL, names = ["sceKernelMkdir", "mkdir", "_mkdir"])]
pub fn sce_kernel_mkdir(path: *const u8, mode: i32) -> i32 {
    if path.is_null() {
        return -14; // EFAULT
    }
    let path_str = unsafe { read_cstr(path) };

    if let Some(k) = get_kernel() {
        info!("[SYSCALL] sceKernelMkdir('{}', mode={:#o})", path_str, mode);
        match k.file_mkdir(&path_str, mode) {
            Ok(_) => 0,
            Err(e) => -e,
        }
    } else {
        0x80020001u32 as i32
    }
}

#[ps4_syscall(id = SyscallId::SCE_KERNEL_RMDIR, lib = crate::libs::LIB_KERNEL, names = ["sceKernelRmdir", "rmdir", "_rmdir"])]
pub fn sce_kernel_rmdir(path: *const u8) -> i32 {
    if path.is_null() {
        return -14; // EFAULT
    }
    let path_str = unsafe { read_cstr(path) };

    if let Some(k) = get_kernel() {
        info!("[SYSCALL] sceKernelRmdir('{}')", path_str);
        match k.file_rmdir(&path_str) {
            Ok(_) => 0,
            Err(e) => -e,
        }
    } else {
        0x80020001u32 as i32
    }
}

#[ps4_syscall(id = SyscallId::SCE_KERNEL_UNLINK, lib = crate::libs::LIB_KERNEL, names = ["sceKernelUnlink", "unlink", "_unlink"])]
pub fn sce_kernel_unlink(path: *const u8) -> i32 {
    if path.is_null() {
        return -14; // EFAULT
    }
    let path_str = unsafe { read_cstr(path) };

    if let Some(k) = get_kernel() {
        info!("[SYSCALL] sceKernelUnlink('{}')", path_str);
        match k.file_unlink(&path_str) {
            Ok(_) => 0,
            Err(e) => -e,
        }
    } else {
        0x80020001u32 as i32
    }
}

#[ps4_syscall(id = SyscallId::SCE_KERNEL_RENAME, lib = crate::libs::LIB_KERNEL, names = ["sceKernelRename", "rename", "_rename"])]
pub fn sce_kernel_rename(old_path: *const u8, new_path: *const u8) -> i32 {
    if old_path.is_null() || new_path.is_null() {
        return -14; // EFAULT
    }
    let old_str = unsafe { read_cstr(old_path) };
    let new_str = unsafe { read_cstr(new_path) };

    if let Some(k) = get_kernel() {
        info!("[SYSCALL] sceKernelRename('{}', '{}')", old_str, new_str);
        match k.file_rename(&old_str, &new_str) {
            Ok(_) => 0,
            Err(e) => -e,
        }
    } else {
        0x80020001u32 as i32
    }
}

#[ps4_syscall(id = SyscallId::SCE_KERNEL_CLOSE, lib = crate::libs::LIB_KERNEL, names = ["sceKernelClose", "close"])]
pub fn sce_kernel_close(fd: i32) -> i32 {
    if let Some(k) = get_kernel() {
        match k.file_close(fd) {
            Ok(_) => 0,
            Err(e) => -e, // negative errno on failure (task-101)
        }
    } else {
        0x80020001u32 as i32
    }
}

#[ps4_syscall(id = SyscallId::SCE_KERNEL_READ, lib = crate::libs::LIB_KERNEL, names = ["sceKernelRead", "read", "_read"])]
pub fn sce_kernel_read(fd: i32, ptr: u64, len: usize) -> isize {
    if let Some(k) = get_kernel() {
        match k.file_read(fd, ptr, len) {
            Ok(bytes) => bytes as isize,
            Err(e) => -(e as isize), // Return negative on error
        }
    } else {
        -1
    }
}

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

#[ps4_syscall(id = SyscallId::SCE_KERNEL_WRITEV, lib = crate::libs::LIB_KERNEL, names = ["sceKernelWritev", "writev", "_writev"])]
pub fn sce_kernel_writev(fd: i32, iov_ptr: *const Iovec, iovcnt: i32) -> isize {
    if iov_ptr.is_null() || iovcnt < 0 {
        return -22; // EINVAL
    }

    let count = iovcnt as usize;
    if count == 0 {
        return 0;
    }

    // iov_ptr is valid in host address space (mapped by memory manager), read directly
    let iovecs = unsafe { slice::from_raw_parts(iov_ptr, count) };

    let k = match get_kernel() {
        Some(k) => k,
        None => return -1,
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
                    return total_written as isize;
                }
                return -(e as isize);
            }
        }
    }

    total_written as isize
}

#[ps4_syscall(id = SyscallId::SCE_KERNEL_LSEEK, lib = crate::libs::LIB_KERNEL, names = ["sceKernelLseek", "lseek", "_lseek"])]
pub fn sce_kernel_lseek(fd: i32, offset: i64, whence: i32) -> i64 {
    if let Some(k) = get_kernel() {
        match k.file_lseek(fd, offset, whence) {
            Ok(pos) => pos as i64,
            Err(e) => -e as i64,
        }
    } else {
        -1
    }
}

// fstat(fd, buf): the Mono runtime fstats an assembly's fd to get its SIZE, then mmaps
// exactly that many bytes — a zeroed stat (st_size=0) made it map 0 bytes and reject the
// assembly as an invalid CIL image. Fill real fd metadata. stdio fds (0/1/2) or a bad fd
// fall back to a zeroed char-device-ish stat.
#[ps4_syscall(id = SyscallId::SYS_FSTAT, lib = crate::libs::LIB_KERNEL, names = ["fstat", "_fstat"])]
pub fn sys_fstat(fd: i32, stat_buf: *mut u8) -> i32 {
    sce_kernel_fstat(fd, stat_buf)
}

#[ps4_syscall(id = SyscallId::SCE_KERNEL_FSTAT, lib = crate::libs::LIB_KERNEL, name = "sceKernelFstat")]
pub fn sce_kernel_fstat(fd: i32, stat_buf: *mut u8) -> i32 {
    if !crate::is_guest_ptr(stat_buf) {
        return -14; // EFAULT
    }
    match get_kernel().and_then(|k| k.file_fstat(fd).ok()) {
        Some((is_dir, size)) => {
            fill_sce_stat(stat_buf, is_dir, size);
            0
        }
        // Unknown/stdio fd: a zeroed stat (character-device-ish) — enough for non-file fds.
        None => {
            unsafe { std::ptr::write_bytes(stat_buf, 0, 120) };
            0
        }
    }
}

#[ps4_syscall(id = SyscallId::SYS_FCNTL, lib = crate::libs::LIB_KERNEL, names = ["fcntl", "_fcntl"])]
pub fn sys_fcntl(_fd: i32, _cmd: i32, _arg: i64) -> i32 {
    warn!("fcntl stubbed!");
    0 // Stub: Success
}

#[ps4_syscall(id = SyscallId::SCE_KERNEL_READV, lib = crate::libs::LIB_KERNEL, names = ["sceKernelReadv", "readv", "_readv"])]
pub fn sce_kernel_readv(fd: i32, iov_ptr: *const Iovec, iovcnt: i32) -> isize {
    if iov_ptr.is_null() || iovcnt < 0 {
        return -22; // EINVAL
    }

    let count = iovcnt as usize;
    if count == 0 {
        return 0;
    }

    // Read the Iovec array from guest memory
    let iovecs = unsafe { slice::from_raw_parts(iov_ptr, count) };

    let k = match get_kernel() {
        Some(k) => k,
        None => return -1,
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
                    return total_read as isize;
                }
                return -(e as isize);
            }
        }
    }

    total_read as isize
}
