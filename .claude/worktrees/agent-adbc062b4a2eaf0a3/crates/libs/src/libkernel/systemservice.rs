//! libSceSystemService: system status / user-preference queries the runtime makes
//! during startup (language, safe-area, splash, event pump). We have no real system
//! services, so report sane fixed defaults and an empty event queue.

use crate::context::NativeContext;
use ps4_macros::ps4_syscall;
use ps4_syscalls::SyscallId;

/// SceSystemServiceParamId values we special-case (rest default to 0).
const PARAM_ID_LANG: i32 = 1;
const PARAM_ID_ENTER_BUTTON_ASSIGN: i32 = 1000;

// sceSystemServiceParamGetInt(paramId, *out): read a system/user preference. Fill a
// sane default: English (US) language, cross = enter. Anything else -> 0.
#[ps4_syscall(id = SyscallId::SCE_SYSTEM_SERVICE_PARAM_GET_INT, lib = crate::libs::LIB_KERNEL, name = "sceSystemServiceParamGetInt")]
pub fn sce_system_service_param_get_int(param_id: i32, out: *mut i32) -> i32 {
    if !crate::is_guest_ptr(out) {
        return 0x80a10003u32 as i32; // SCE_SYSTEM_SERVICE_ERROR_PARAMETER
    }
    let value = match param_id {
        PARAM_ID_LANG => 1,                // SCE_SYSTEM_PARAM_LANG_ENGLISH_US
        PARAM_ID_ENTER_BUTTON_ASSIGN => 1, // cross
        _ => 0,
    };
    unsafe { *out = value };
    0
}

// sceSystemServiceGetStatus(*status): nothing pending (no exit/menu/etc. request).
#[ps4_syscall(id = SyscallId::SCE_SYSTEM_SERVICE_GET_STATUS, lib = crate::libs::LIB_KERNEL, name = "sceSystemServiceGetStatus")]
pub fn sce_system_service_get_status(status: *mut u8) -> i32 {
    if crate::is_guest_ptr(status) {
        // SceSystemServiceStatus is ~0x28 bytes; a fully-zero status = idle, no request.
        unsafe { std::ptr::write_bytes(status, 0, 0x28) };
    }
    0
}

// sceSystemServiceReceiveEvent(*event): the runtime pumps the system event queue. We
// have none, so report empty rather than a spurious event.
#[ps4_syscall(id = SyscallId::SCE_SYSTEM_SERVICE_RECEIVE_EVENT, lib = crate::libs::LIB_KERNEL, name = "sceSystemServiceReceiveEvent")]
pub fn sce_system_service_receive_event(_event: *mut u8) -> i32 {
    0x80a10004u32 as i32 // SCE_SYSTEM_SERVICE_ERROR_NO_EVENT
}

// sceSystemServiceGetDisplaySafeAreaInfo(*info): report the full display (ratio 1.0).
#[ps4_syscall(id = SyscallId::SCE_SYSTEM_SERVICE_GET_DISPLAY_SAFE_AREA_INFO, lib = crate::libs::LIB_KERNEL, name = "sceSystemServiceGetDisplaySafeAreaInfo")]
pub fn sce_system_service_get_display_safe_area_info(info: *mut u8) -> i32 {
    if crate::is_guest_ptr(info) {
        unsafe {
            std::ptr::write_bytes(info, 0, 0x20);
            // SceSystemServiceDisplaySafeAreaInfo.ratio (f32 @ 0x00) = 1.0 (no inset).
            *(info as *mut f32) = 1.0;
        }
    }
    0
}

#[ps4_syscall(id = SyscallId::SCE_SYSTEM_SERVICE_HIDE_SPLASH_SCREEN, lib = crate::libs::LIB_KERNEL, name = "sceSystemServiceHideSplashScreen")]
pub fn sce_system_service_hide_splash_screen() -> i32 {
    0
}

// sceAppContentInitialize(*init): app-content (add-ons / temp data) service. We provide
// none; accept init so the runtime proceeds (temp-data mount lands under /app0 via FS).
#[ps4_syscall(id = SyscallId::SCE_APP_CONTENT_INITIALIZE, lib = crate::libs::LIB_KERNEL, name = "sceAppContentInitialize")]
pub fn sce_app_content_initialize(_init: *const u8, _out: *mut u8) -> i32 {
    0
}
