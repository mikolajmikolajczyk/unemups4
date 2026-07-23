// x86jit-backed guest execution core (doc-1). The guest runs entirely under the x86jit
// interpreter/JIT (`Vm`/`Vcpu`); syscalls trap out via `Exit::Syscall` and are dispatched
// to Rust HLE handlers. There is no native host-execution path.
mod context;
pub mod dirty;
pub mod exec;
pub mod guest_vm;
mod hostmem;
pub mod profile;

pub use context::{FromReg, NativeContext};
pub use dirty::VmDirtySource;
pub use exec::{
    GuestExit, SyscallDispatch, WATCHDOG_ENV, call_guest, current_errno_addr, request_thread_exit,
    run_guest_call, set_fault_annotator, set_syscall_dispatch, syscall_stack_arg,
};
pub use guest_vm::{GuestVm, JitCounters};
