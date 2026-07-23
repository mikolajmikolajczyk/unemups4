use crate::context::NativeContext;
use ps4_macros::ps4_syscall;
use ps4_syscalls::SyscallId;
use tracing::info;

// main player
const USER_ID: i32 = 1;

#[ps4_syscall(id = SyscallId::SCE_USER_SERVICE_INITIALIZE, lib = crate::libs::LIB_SCE_USER_SERVICE, name = "sceUserServiceInitialize")]
pub fn sce_user_service_initialize(_params: u64) -> i32 {
    // _params is usually NULL.
    info!("[USER_SERVICE] sceUserServiceInitialize");
    0 // SCE_OK
}

#[ps4_syscall(id = SyscallId::SCE_USER_SERVICE_GET_INITIAL_USER, lib = crate::libs::LIB_SCE_USER_SERVICE, name = "sceUserServiceGetInitialUser")]
pub fn sce_user_service_get_initial_user(user_id_ptr: *mut i32) -> i32 {
    info!("[USER_SERVICE] sceUserServiceGetInitialUser");
    if user_id_ptr.is_null() {
        return -1; // Error
    }
    unsafe {
        *user_id_ptr = USER_ID;
    }
    0 // SCE_OK
}

#[ps4_syscall(id = SyscallId::SCE_USER_SERVICE_GET_LOGIN_USER_ID_LIST, lib = crate::libs::LIB_SCE_USER_SERVICE, name = "sceUserServiceGetLoginUserIdList")]
pub fn sce_user_service_get_login_user_id_list(
    list_ptr: *mut SceUserServiceLoginUserIdList,
) -> i32 {
    info!("[USER_SERVICE] sceUserServiceGetLoginUserIdList");
    if list_ptr.is_null() {
        return -1;
    }

    unsafe {
        // Clear the list first
        (*list_ptr).user_ids = [0; 4];

        // Set Player 1 as logged in
        (*list_ptr).user_ids[0] = USER_ID;
    }
    0 // SCE_OK
}

#[ps4_syscall(id = SyscallId::SCE_USER_SERVICE_TERMINATE, lib = crate::libs::LIB_SCE_USER_SERVICE, name = "sceUserServiceTerminate")]
pub fn sce_user_service_terminate() -> i32 {
    info!("[USER_SERVICE] sceUserServiceTerminate");
    0
}

#[repr(C)]
pub struct SceUserServiceLoginUserIdList {
    pub user_ids: [i32; 4],
}
