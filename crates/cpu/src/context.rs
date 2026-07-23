#[repr(C)]
#[derive(Debug)]
pub struct NativeContext {
    pub r15: u64,
    pub r14: u64,
    pub r13: u64,
    pub r12: u64,
    pub r11: u64,
    pub r10: u64,
    pub r9: u64,
    pub r8: u64,
    pub rdi: u64,
    pub rsi: u64,
    pub rdx: u64,
    pub rcx: u64,
    pub rbp: u64,
    pub rbx: u64,
    pub rax: u64,
    /// Guest RSP at the syscall. On entry to the HLE stub (which does no push/pop before
    /// SYSCALL) this is the callee's frame: `[rsp]` = return address, `[rsp+8]` = the 7th
    /// SysV argument, etc. Exposed to handlers via `ps4_cpu::syscall_stack_arg` so a
    /// >6-argument syscall (e.g. sceKernelMmap) can read stack-passed args.
    pub rsp: u64,
}

pub trait FromReg {
    fn from_reg(val: u64) -> Self;
}

impl FromReg for u64 {
    fn from_reg(val: u64) -> Self {
        val
    }
}

impl FromReg for i64 {
    fn from_reg(val: u64) -> Self {
        val as i64
    }
}

impl FromReg for u32 {
    fn from_reg(val: u64) -> Self {
        val as u32
    }
}

impl FromReg for i32 {
    fn from_reg(val: u64) -> Self {
        val as i32
    }
}
impl FromReg for usize {
    fn from_reg(val: u64) -> Self {
        val as usize
    }
}
impl<T> FromReg for *const T {
    fn from_reg(val: u64) -> Self {
        val as *const T
    }
}

impl<T> FromReg for *mut T {
    fn from_reg(val: u64) -> Self {
        val as *mut T
    }
}

impl NativeContext {
    pub fn arg0<T: FromReg>(&self) -> T {
        T::from_reg(self.rdi)
    }
    pub fn arg1<T: FromReg>(&self) -> T {
        T::from_reg(self.rsi)
    }
    pub fn arg2<T: FromReg>(&self) -> T {
        T::from_reg(self.rdx)
    }
    pub fn arg3<T: FromReg>(&self) -> T {
        T::from_reg(self.r10)
    }
    pub fn arg4<T: FromReg>(&self) -> T {
        T::from_reg(self.r8)
    }
    pub fn arg5<T: FromReg>(&self) -> T {
        T::from_reg(self.r9)
    }
}
