use crate::context::NativeContext;
use ps4_core::kernel::get_kernel;
use ps4_macros::ps4_syscall;
use ps4_syscalls::SyscallId;

#[ps4_syscall(id = SyscallId::SCE_PAD_INIT, lib = crate::libs::LIB_SCE_PAD, name = "scePadInit")]
pub fn sce_pad_init() -> i32 {
    0 // Success
}

#[ps4_syscall(id = SyscallId::SCE_PAD_OPEN, lib = crate::libs::LIB_SCE_PAD, name = "scePadOpen")]
pub fn sce_pad_open(_user_id: i32, _type: i32, _index: i32, _param: u64) -> i32 {
    1 // Return handle 1
}

#[ps4_syscall(id = SyscallId::SCE_PAD_CLOSE, lib = crate::libs::LIB_SCE_PAD, name = "scePadClose")]
pub fn sce_pad_close(_handle: i32) -> i32 {
    0
}

#[ps4_syscall(id = SyscallId::SCE_PAD_READ_STATE, lib = crate::libs::LIB_SCE_PAD, name = "scePadReadState")]
pub fn sce_pad_read_state(handle: i32, data_ptr: *mut u8) -> i32 {
    if data_ptr.is_null() {
        return -1;
    }

    if let Some(k) = get_kernel() {
        let state = k.pad_get_state(handle);
        if state.buttons != 0 {
            tracing::debug!("scePadReadState: buttons={:#06x}", state.buttons);
        }

        unsafe {
            // clear before writing fields
            std::ptr::write_bytes(data_ptr, 0, 96);

            *(data_ptr as *mut u32) = state.buttons;

            // sticks
            *data_ptr.add(4) = state.lx;
            *data_ptr.add(5) = state.ly;
            *data_ptr.add(6) = state.rx;
            *data_ptr.add(7) = state.ry;
            *data_ptr.add(8) = state.l2;
            *data_ptr.add(9) = state.r2;

            // connected flag at offset 76 so guest sees a controller
            *data_ptr.add(76) = 1;
        }
        0
    } else {
        -1
    }
}

#[ps4_syscall(id = SyscallId::SCE_PAD_READ, lib = crate::libs::LIB_SCE_PAD, name = "scePadRead")]
pub fn sce_pad_read(handle: i32, data_ptr: *mut u8, count: i32) -> i32 {
    if data_ptr.is_null() || count <= 0 {
        return -1;
    }

    if let Some(k) = get_kernel() {
        let state = k.pad_get_state(handle);
        let struct_size = 96; // OrbisPadData size

        unsafe {
            for i in 0..count {
                let current_ptr = data_ptr.add((i as usize) * struct_size);

                std::ptr::write_bytes(current_ptr, 0, struct_size);

                *(current_ptr as *mut u32) = state.buttons;

                // sticks
                *current_ptr.add(4) = state.lx;
                *current_ptr.add(5) = state.ly;
                *current_ptr.add(6) = state.rx;
                *current_ptr.add(7) = state.ry;

                // triggers
                *current_ptr.add(8) = state.l2;
                *current_ptr.add(9) = state.r2;

                // connected byte; offset depends on SDK struct layout, set all known candidates
                *current_ptr.add(12) = 1;
                *current_ptr.add(76) = 1; // OpenOrbis
                *current_ptr.add(88) = 1; // official SDK
            }
        }
        0
    } else {
        -1
    }
}

#[ps4_syscall(id = SyscallId::SCE_PAD_GET_HANDLE, lib = crate::libs::LIB_SCE_PAD, name = "scePadGetHandle")]
pub fn sce_pad_get_handle(_user_id: i32, _type: i32, _index: i32) -> i32 {
    // hardcoded handle 1
    1
}
