use crate::context::NativeContext;
use ps4_core::kernel::get_kernel;
use ps4_macros::ps4_syscall;
use ps4_syscalls::SyscallId;
use tracing::info;

#[ps4_syscall(id = SyscallId::SYS_MMAP, lib = crate::libs::LIB_KERNEL, names = ["mmap", "__sys_mmap"])]
pub fn sys_mmap(addr: u64, len: usize, prot: i32, flags: i32, fd: i32, offset: i64) -> u64 {
    info!(
        "[SYSCALL] mmap(addr={:#x}, len={:#x}, prot={}, flags={:#x})",
        addr, len, prot, flags
    );

    if let Some(k) = get_kernel() {
        match k.mmap(addr, len, prot, flags, fd, offset) {
            Ok(ptr) => ptr,
            Err(e) => {
                // On failure, return -ENOMEM (negative errno), matching the kernel ABI.
                -e as u64
            }
        }
    } else {
        u64::MAX // -1
    }
}

#[ps4_syscall(id = SyscallId::SYS_MUNMAP, lib = crate::libs::LIB_KERNEL, names = ["munmap"])]
pub fn sys_munmap(addr: u64, len: usize) -> i32 {
    if let Some(k) = get_kernel() {
        k.munmap(addr, len).unwrap_or(-1)
    } else {
        -1
    }
}

// sceKernelMmap(addr, len, prot, flags, fd, offset, **res): the 7th arg (res, the
// out-pointer for the mapped address) is stack-passed beyond our 6 register args, read
// via ps4_cpu::syscall_stack_arg(6). Routes to the same kernel mmap as the POSIX one.
#[ps4_syscall(id = SyscallId::SCE_KERNEL_MMAP, lib = crate::libs::LIB_KERNEL, name = "sceKernelMmap")]
pub fn sce_kernel_mmap(addr: u64, len: usize, prot: i32, flags: i32, fd: i32, offset: i64) -> i32 {
    let res = ps4_cpu::syscall_stack_arg(6) as *mut u64;
    info!(
        "[SYSCALL] sceKernelMmap(addr={:#x}, len={:#x}, prot={}, flags={:#x}, fd={}, off={:#x})",
        addr, len, prot, flags, fd, offset
    );
    let Some(k) = get_kernel() else {
        return 0x80020001u32 as i32;
    };
    match k.mmap(addr, len, prot, flags, fd, offset) {
        Ok(ptr) => {
            if crate::is_guest_ptr(res) {
                unsafe { *res = ptr };
            }
            0
        }
        Err(e) => -e as i32,
    }
}

// munmap routes to the kernel; mprotect is a tracking no-op — the identity arena is
// pre-mapped RWX, so no host reprotection is needed for guest code to execute or GC write.
#[ps4_syscall(id = SyscallId::SCE_KERNEL_MUNMAP, lib = crate::libs::LIB_KERNEL, name = "sceKernelMunmap")]
pub fn sce_kernel_munmap(addr: u64, len: usize) -> i32 {
    if let Some(k) = get_kernel() {
        k.munmap(addr, len).unwrap_or(-1)
    } else {
        -1
    }
}

#[ps4_syscall(id = SyscallId::SCE_KERNEL_MPROTECT, lib = crate::libs::LIB_KERNEL, name = "sceKernelMprotect")]
pub fn sce_kernel_mprotect(_addr: u64, _len: usize, _prot: i32) -> i32 {
    0
}

#[ps4_syscall(id = SyscallId::SCE_KERNEL_MTYPEPROTECT, lib = crate::libs::LIB_KERNEL, name = "sceKernelMtypeprotect")]
pub fn sce_kernel_mtypeprotect(_addr: u64, _len: usize, _type: i32, _prot: i32) -> i32 {
    0
}

#[ps4_syscall(id = SyscallId::SYS_MADVISE, lib = crate::libs::LIB_KERNEL, names = ["madvise"])]
pub fn sys_madvise(_addr: u64, _len: usize, _behav: i32) -> i32 {
    // We ignore memory advice in HLE for now.
    0
}

#[ps4_syscall(id = SyscallId::SCE_KERNEL_ALLOCATE_DIRECT_MEMORY, lib = crate::libs::LIB_KERNEL, name = "sceKernelAllocateDirectMemory")]
pub fn sce_kernel_allocate_direct_memory(
    _search_start: u64,
    _search_end: u64,
    length: usize,
    _alignment: usize,
    _type_: i32,
    phys_addr_out: *mut u64,
) -> i32 {
    if let Some(k) = get_kernel() {
        // In HLE, we treat "Direct Memory" just like a large mmap.
        // We use MAP_ANONYMOUS | MAP_PRIVATE.
        // 0x1000 = MAP_ANON (BSD/Linux approx), 0x2 = MAP_PRIVATE
        // PROT_READ|WRITE = 3
        match k.mmap(0, length, 3, 0x1002, -1, 0) {
            Ok(addr) => {
                unsafe {
                    if !phys_addr_out.is_null() {
                        // In HLE, Virtual = Physical for our GPU register function
                        *phys_addr_out = addr;
                    }
                }
                0
            }
            Err(e) => e as i32,
        }
    } else {
        -1
    }
}

// Flexible (physical) memory: the Mono runtime / GC maps its heaps through this. It is a
// real mapping the runtime reads/writes heavily, so back it with an actual mmap. `addr_in_out`
// carries the requested address in (0 = anywhere) and receives the mapped address out.
#[ps4_syscall(id = SyscallId::SCE_KERNEL_MAP_NAMED_FLEXIBLE_MEMORY, lib = crate::libs::LIB_KERNEL, name = "sceKernelMapNamedFlexibleMemory")]
pub fn sce_kernel_map_named_flexible_memory(
    addr_in_out: *mut u64,
    length: usize,
    prot: i32,
    _flags: i32,
    _name: *const u8,
) -> i32 {
    map_flexible(addr_in_out, length, prot)
}

#[ps4_syscall(id = SyscallId::SCE_KERNEL_MAP_FLEXIBLE_MEMORY, lib = crate::libs::LIB_KERNEL, name = "sceKernelMapFlexibleMemory")]
pub fn sce_kernel_map_flexible_memory(
    addr_in_out: *mut u64,
    length: usize,
    prot: i32,
    _flags: i32,
) -> i32 {
    map_flexible(addr_in_out, length, prot)
}

fn map_flexible(addr_in_out: *mut u64, length: usize, prot: i32) -> i32 {
    if addr_in_out.is_null() {
        return 22; // EINVAL
    }
    let requested = unsafe { *addr_in_out };
    let Some(k) = get_kernel() else { return -1 };
    // MAP_ANON | MAP_PRIVATE; honour the requested address when non-zero, else "anywhere".
    match k.mmap(requested, length, prot, 0x1002, -1, 0) {
        Ok(addr) => {
            unsafe { *addr_in_out = addr };
            0
        }
        Err(e) => e as i32,
    }
}

#[ps4_syscall(id = SyscallId::SCE_KERNEL_MAP_DIRECT_MEMORY, lib = crate::libs::LIB_KERNEL, name = "sceKernelMapDirectMemory")]
pub fn sce_kernel_map_direct_memory(
    addr: *mut u64,
    _length: usize,
    _prot: i32,
    _flags: i32,
    start: u64,
    _alignment: usize,
) -> i32 {
    // In our simplified HLE, AllocateDirectMemory already mapped the memory.
    // MapDirectMemory is just supposed to return that address to the user.
    unsafe {
        if !addr.is_null() {
            *addr = start;
        }
    }
    0
}

#[ps4_syscall(id = SyscallId::SCE_KERNEL_RELEASE_DIRECT_MEMORY, lib = crate::libs::LIB_KERNEL, name = "sceKernelReleaseDirectMemory")]
pub fn sce_kernel_release_direct_memory(start: u64, length: usize) -> i32 {
    if let Some(k) = get_kernel() {
        k.munmap(start, length).unwrap_or(-1)
    } else {
        -1
    }
}

// Stub for getting size (just return a huge number)
#[ps4_syscall(id = SyscallId::SCE_KERNEL_GET_DIRECT_MEMORY_SIZE, lib = crate::libs::LIB_KERNEL, name = "sceKernelGetDirectMemorySize")]
pub fn sce_kernel_get_direct_memory_size() -> u64 {
    512 * 1024 * 1024 // 512 MB
}
