use crate::context::NativeContext;
use ps4_macros::ps4_syscall;
use ps4_syscalls::SyscallId;
use std::thread;
use std::time::Duration;

#[ps4_syscall(id = SyscallId::SCE_KERNEL_CREATE_EQUEUE, lib = crate::libs::LIB_KERNEL, name = "sceKernelCreateEqueue")]
pub fn sce_kernel_create_equeue(_eq: *mut u64, _name: *const u8) -> i32 {
    // Return a dummy event-queue handle (0).
    0
}

#[ps4_syscall(id = SyscallId::SCE_KERNEL_WAIT_EQUEUE, lib = crate::libs::LIB_KERNEL, name = "sceKernelWaitEqueue")]
pub fn sce_kernel_wait_equeue(_eq: i32, _ev: u64, _num: i32, _out: u64, _timeout: u64) -> i32 {
    // Simulate VSync wait (approx 60 FPS)
    thread::sleep(Duration::from_millis(16));
    0
}
