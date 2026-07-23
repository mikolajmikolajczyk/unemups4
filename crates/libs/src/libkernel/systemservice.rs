//! libSceSystemService: system status / user-preference queries the runtime makes
//! during startup (language, safe-area, splash, event pump). We have no real system
//! services, so report sane fixed defaults and an empty event queue.

use crate::context::NativeContext;
use ps4_core::guest_ptr::{GuestPtr, GuestSlice};
use ps4_macros::ps4_syscall;
use ps4_syscalls::SyscallId;

/// SceSystemServiceParamId values we special-case (rest default to 0).
const PARAM_ID_LANG: i32 = 1;
const PARAM_ID_ENTER_BUTTON_ASSIGN: i32 = 1000;

// sceSystemServiceParamGetInt(paramId, *out): read a system/user preference. Fill a
// sane default: English (US) language, cross = enter. Anything else -> 0.
#[ps4_syscall(id = SyscallId::SCE_SYSTEM_SERVICE_PARAM_GET_INT, lib = crate::libs::LIB_KERNEL, name = "sceSystemServiceParamGetInt")]
pub fn sce_system_service_param_get_int(param_id: i32, out: *mut i32) -> i32 {
    let Some(gp) = GuestPtr::<i32>::new(out as u64) else {
        return 0x80a10003u32 as i32; // SCE_SYSTEM_SERVICE_ERROR_PARAMETER
    };
    let value = match param_id {
        PARAM_ID_LANG => 1,                // SCE_SYSTEM_PARAM_LANG_ENGLISH_US
        PARAM_ID_ENTER_BUTTON_ASSIGN => 1, // cross
        _ => 0,
    };
    let _ = gp.write(value);
    0
}

// sceSystemServiceGetStatus(*status): nothing pending (no exit/menu/etc. request).
#[ps4_syscall(id = SyscallId::SCE_SYSTEM_SERVICE_GET_STATUS, lib = crate::libs::LIB_KERNEL, name = "sceSystemServiceGetStatus")]
pub fn sce_system_service_get_status(status: *mut u8) -> i32 {
    // SceSystemServiceStatus is ~0x28 bytes; a fully-zero status = idle, no request.
    if let Some(gs) = GuestSlice::<u8>::new(status as u64, 0x28) {
        let _ = gs.zero();
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
    if let Some(gs) = GuestSlice::<u8>::new(info as u64, 0x20) {
        let _ = gs.zero();
        // SceSystemServiceDisplaySafeAreaInfo.ratio (f32 @ 0x00) = 1.0 (no inset).
        if let Some(ratio) = GuestPtr::<f32>::new(info as u64) {
            let _ = ratio.write(1.0);
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

/// `sceDiscMapIsRequestOnHDD(path, offset, size, int *result)`: is this byte range of this
/// file resident on the HDD rather than still on the Blu-ray?
///
/// A title asks before a read so it can choose between a fast path and a
/// spin-up-and-seek path, or show a loading hint. Every file we serve comes from the host
/// filesystem, so the answer is always yes and the fast path is the honest one.
///
/// The SDK header we have is a bare `void sceDiscMapIsRequestOnHDD();`, so the argument list
/// is inferred from the name and from how the answer is delivered — a 4th out-parameter,
/// which is this library's convention. Returning 0 with `*result = 1` is the combination that
/// says "call succeeded, range is local"; a title that reads only the return value still gets
/// success.
#[ps4_syscall(id = SyscallId::SCE_DISC_MAP_IS_REQUEST_ON_HDD, lib = crate::libs::LIB_KERNEL, name = "sceDiscMapIsRequestOnHDD")]
pub fn sce_disc_map_is_request_on_hdd(
    _path: *const u8,
    _offset: i64,
    _size: u64,
    result: *mut i32,
) -> i32 {
    if let Some(gp) = GuestPtr::<i32>::new(result as u64) {
        let _ = gp.write(1); // on HDD
    }
    0
}

/// `sceAppContentTemporaryDataMount2(option, SceAppContentMountPoint *out)`: mount the
/// title's temporary-data area and report its mount point.
///
/// Temporary data is the console's scratch area — shader caches, streaming spill, anything a
/// title can rebuild — and is explicitly NOT savedata: the system may clear it between runs.
/// A native title mounts it during startup and writes there before it draws anything, so it
/// cannot be stubbed away with a success code and no mount point; the guest would then open
/// paths under an empty string.
///
/// `SceAppContentMountPoint` is a 16-byte char array, the same shape savedata's result uses.
/// The `option` (none / format) is ignored: we never clear the directory, so there is nothing
/// for a format request to do differently. That is a deliberate choice, not an omission —
/// keeping the previous run's scratch is far more useful while bringing a title up, and the
/// API permits the system to clear rather than requiring it.
#[ps4_syscall(id = SyscallId::SCE_APP_CONTENT_TEMPORARY_DATA_MOUNT2, lib = crate::libs::LIB_KERNEL, name = "sceAppContentTemporaryDataMount2")]
pub fn sce_app_content_temporary_data_mount2(_option: u32, mount_point_out: u64) -> i32 {
    let Some(kernel) = ps4_core::kernel::get_kernel() else {
        return 0x80D90002u32 as i32; // SCE_APP_CONTENT_ERROR_PARAMETER
    };
    match kernel.tempdata_mount() {
        Ok(mount_point) => {
            let mut out = [0u8; 16];
            let mp = mount_point.as_bytes();
            let n = mp.len().min(15);
            out[..n].copy_from_slice(&mp[..n]);
            let Some(gs) = GuestSlice::<u8>::new(mount_point_out, out.len()) else {
                return 0x80D90002u32 as i32;
            };
            let _ = gs.write_slice(&out);
            tracing::info!("[APPCONTENT] temporary data mounted at '{mount_point}'");
            0
        }
        Err(e) => {
            tracing::warn!("[APPCONTENT] temporary data mount failed: errno {e}");
            0x80D90002u32 as i32
        }
    }
}

// ---------------------------------------------------------------------------
// libScePlayGo — progressive install / chunk residency.
//
// PlayGo lets a title start from a partial download and ask, chunk by chunk, what has
// arrived. Everything we serve is a complete local dump, so every answer is the same one:
// fully installed, locally, at full speed, nothing left to fetch. A title uses these answers
// to gate content — a level it believes is still downloading stays locked — so the values
// matter even though the mechanism does not exist here.
//
// The SDK headers carry no signatures for these, so argument lists follow the family's
// conventions (a handle from `Open`, an id array plus count, an out array). Each is written to
// fail safe: if an out-pointer does not validate we return an error rather than claiming a
// success the guest would read uninitialised memory for.
// ---------------------------------------------------------------------------

/// SCE_PLAYGO_ERROR_PARAMETER — the family's generic bad-argument code.
const PLAYGO_ERROR_PARAMETER: i32 = 0x80B20002u32 as i32;

/// The error a caller uses to learn it has run off the end of the chunk table.
///
/// Not guessed — read out of the guest's own code. The single call site of
/// `scePlayGoGetLocus` in the title tests for exactly this value and leaves its enumeration
/// loop on it:
///
/// ```text
/// de12a0:  call   scePlayGoGetLocus
/// de12a9:  cmp    $0x80b2000c,%ecx
/// de12af:  je     0xde12fb          ; loop exit
/// ```
const PLAYGO_ERROR_BAD_CHUNK_ID: i32 = 0x80B2000Cu32 as i32;

/// How many chunks we claim the title has.
///
/// A complete local dump is not chunked at all, so there is exactly one: id 0, holding
/// everything. The count matters because it is the only way a caller can discover where the
/// table ends — it enumerates ids until one is rejected. Answering success for every id makes
/// that loop unbounded, which is what a UE4 title did here: 3.5 million `GetLocus` calls a
/// second, each followed by real hash-table work, forever.
const PLAYGO_CHUNK_COUNT: u16 = 1;

/// `SCE_PLAYGO_LOCUS_LOCAL_FAST`: the chunk is installed and on fast storage. Reporting the
/// slow variant would push a title onto its "stream carefully" path for no reason.
const PLAYGO_LOCUS_LOCAL_FAST: u8 = 3;

/// The single handle `scePlayGoOpen` ever returns. There is one title and one install, so
/// there is nothing for a second handle to distinguish; a fixed non-zero value keeps a guest
/// that checks `handle > 0` happy and makes a stale-handle bug obvious rather than subtle.
const PLAYGO_HANDLE: i32 = 1;

#[ps4_syscall(id = SyscallId::SCE_PLAY_GO_INITIALIZE, lib = crate::libs::LIB_KERNEL, name = "scePlayGoInitialize")]
pub fn sce_play_go_initialize(_param: u64) -> i32 {
    0
}

#[ps4_syscall(id = SyscallId::SCE_PLAY_GO_TERMINATE, lib = crate::libs::LIB_KERNEL, name = "scePlayGoTerminate")]
pub fn sce_play_go_terminate() -> i32 {
    0
}

#[ps4_syscall(id = SyscallId::SCE_PLAY_GO_OPEN, lib = crate::libs::LIB_KERNEL, name = "scePlayGoOpen")]
pub fn sce_play_go_open(handle_out: *mut i32, _param: u64) -> i32 {
    let Some(gp) = GuestPtr::<i32>::new(handle_out as u64) else {
        return PLAYGO_ERROR_PARAMETER;
    };
    let _ = gp.write(PLAYGO_HANDLE);
    0
}

#[ps4_syscall(id = SyscallId::SCE_PLAY_GO_CLOSE, lib = crate::libs::LIB_KERNEL, name = "scePlayGoClose")]
pub fn sce_play_go_close(_handle: i32) -> i32 {
    0
}

/// Every requested chunk is installed on fast storage.
///
/// `ScePlayGoLocus` is ONE BYTE per entry, and getting that wrong is not a cosmetic error:
/// writing a `u32` per entry into a caller's byte array smashes the three bytes after it. The
/// out-pointer observed here is `0x4002130f9` — unaligned, i.e. a plain `char` on the stack —
/// so the over-write landed on the guest's own loop state and the title spun in
/// `scePlayGoGetLocus` 28 million times in 20 seconds. Chunk ids are 16-bit for the same
/// reason a `u32` read of them produced `0x10001`-strided nonsense.
#[ps4_syscall(id = SyscallId::SCE_PLAY_GO_GET_LOCUS, lib = crate::libs::LIB_KERNEL, name = "scePlayGoGetLocus")]
pub fn sce_play_go_get_locus(_handle: i32, chunk_ids: u64, count: u32, loci_out: *mut u8) -> i32 {
    let Some(ids) = GuestSlice::<u16>::new(chunk_ids, count as usize).and_then(|s| s.read_vec())
    else {
        return PLAYGO_ERROR_PARAMETER;
    };
    // Reject the whole request if any id is past the end of our (single-chunk) table, and
    // write nothing — this is the answer a caller enumerating the table is waiting for.
    if ids.iter().any(|&id| id >= PLAYGO_CHUNK_COUNT) {
        return PLAYGO_ERROR_BAD_CHUNK_ID;
    }
    let Some(gs) = GuestSlice::<u8>::new(loci_out as u64, count as usize) else {
        return PLAYGO_ERROR_PARAMETER;
    };
    let _ = gs.write_slice(&vec![PLAYGO_LOCUS_LOCAL_FAST; count as usize]);
    0
}

/// Install progress: `{ u64 progressSize; u64 totalSize; }`, reported complete.
///
/// Both fields carry the same non-zero value rather than zero: a title computing a percentage
/// divides by `totalSize`, and zero there is a division by zero on a path that only runs when
/// the install is being watched.
#[ps4_syscall(id = SyscallId::SCE_PLAY_GO_GET_PROGRESS, lib = crate::libs::LIB_KERNEL, name = "scePlayGoGetProgress")]
pub fn sce_play_go_get_progress(
    _handle: i32,
    _chunk_ids: u64,
    _count: u32,
    progress_out: *mut u8,
) -> i32 {
    let Some(gs) = GuestSlice::<u8>::new(progress_out as u64, 16) else {
        return PLAYGO_ERROR_PARAMETER;
    };
    let mut out = [0u8; 16];
    out[0..8].copy_from_slice(&1u64.to_le_bytes()); // progressSize
    out[8..16].copy_from_slice(&1u64.to_le_bytes()); // totalSize
    let _ = gs.write_slice(&out);
    0
}

/// Nothing left to install: zero entries written, zero reported.
#[ps4_syscall(id = SyscallId::SCE_PLAY_GO_GET_TO_DO_LIST, lib = crate::libs::LIB_KERNEL, name = "scePlayGoGetToDoList")]
pub fn sce_play_go_get_to_do_list(
    _handle: i32,
    _todo_out: u64,
    _count: u32,
    out_count: *mut u32,
) -> i32 {
    if let Some(gp) = GuestPtr::<u32>::new(out_count as u64) {
        let _ = gp.write(0);
    }
    0
}

/// Estimated time to completion: zero, because nothing is pending.
#[ps4_syscall(id = SyscallId::SCE_PLAY_GO_GET_ETA, lib = crate::libs::LIB_KERNEL, name = "scePlayGoGetEta")]
pub fn sce_play_go_get_eta(_handle: i32, _chunk_ids: u64, _count: u32, eta_out: *mut u64) -> i32 {
    if let Some(gp) = GuestPtr::<u64>::new(eta_out as u64) {
        let _ = gp.write(0);
    }
    0
}

/// Install speed is a request to the background downloader; there is none. Accept and report
/// the "full speed" setting back, so a title that sets then reads sees what it asked for.
#[ps4_syscall(id = SyscallId::SCE_PLAY_GO_SET_INSTALL_SPEED, lib = crate::libs::LIB_KERNEL, name = "scePlayGoSetInstallSpeed")]
pub fn sce_play_go_set_install_speed(_handle: i32, _speed: u32) -> i32 {
    0
}

#[ps4_syscall(id = SyscallId::SCE_PLAY_GO_GET_INSTALL_SPEED, lib = crate::libs::LIB_KERNEL, name = "scePlayGoGetInstallSpeed")]
pub fn sce_play_go_get_install_speed(_handle: i32, speed_out: *mut u32) -> i32 {
    if let Some(gp) = GuestPtr::<u32>::new(speed_out as u64) {
        let _ = gp.write(1); // SCE_PLAYGO_INSTALL_SPEED_FULL
    }
    0
}

/// Prefetch reorders a download queue. Nothing is queued, so this is a no-op success.
#[ps4_syscall(id = SyscallId::SCE_PLAY_GO_PREFETCH, lib = crate::libs::LIB_KERNEL, name = "scePlayGoPrefetch")]
pub fn sce_play_go_prefetch(_handle: i32, _chunk_ids: u64, _count: u32, _speed: u32) -> i32 {
    0
}

/// Synthetic syscall ids for imports we serve by RAW NID because their name is unknown.
///
/// The generated table is built from `data/ps4_names.txt`, so a symbol absent from it has no
/// id to borrow. Numbering these above the generated range keeps them from ever colliding
/// with a real one, and keeps them obvious in a profiler dump.
const SYNTHETIC_ID_DISCMAP_PACKAGE_LOCATION: u64 = 120_000;

/// `libSceDiscMap` symbol `fJgP+wqifno`: report a package's physical (LBA) location.
///
/// THE NAME IS UNKNOWN and cannot be recovered — a NID is a one-way hash and this symbol is
/// not in our 94k-name table; ~134k dictionary candidates found no preimage. What it does was
/// read out of the caller instead, in `libSceFios2`:
///
/// ```text
/// 52c4f7:  lea    -0x38(%rbp),%rcx      ; out1
/// 52c4fb:  lea    -0x40(%rbp),%r8       ; out2
/// 52c4ff:  lea    -0x48(%rbp),%r9       ; out3
/// 52c503:  mov    $0x0,%esi
/// 52c508:  mov    %r14,%rdx
/// 52c50b:  call   <this>
/// 52c515:  test   %ecx,%ecx             ; non-zero = failure
/// 52c519:  mov    -0x38(%rbp),%rsi
/// 52c51d:  cmp    $0x2,%rsi             ; and at the other call site:
///          ...                          ; "FIOS2 WARNING: An unexpected value of %ld was
///          ...                          ;  returned for the LBA package location"
/// ```
///
/// So: three out-values, 0 on success, and the caller warns when the first is >= 2 — meaning
/// the expected answers are 0 and 1. Everything we serve comes from the host filesystem with
/// no disc layout at all, so 0 is the truthful answer to "where does this package sit on the
/// medium": nowhere in particular. Zeroing the other two matches how the caller pre-zeroes
/// them before the call.
#[ps4_syscall(
    id = ps4_syscalls::SyscallId(SYNTHETIC_ID_DISCMAP_PACKAGE_LOCATION),
    lib = crate::libs::LIB_KERNEL,
    nids = ["fJgP+wqifno"]
)]
pub fn sce_disc_map_package_location(
    _arg0: u64,
    _arg1: u64,
    _handle: u64,
    out1: *mut u64,
    out2: *mut u64,
    out3: *mut u64,
) -> i32 {
    for slot in [out1, out2, out3] {
        if let Some(gp) = GuestPtr::<u64>::new(slot as u64) {
            let _ = gp.write(0);
        }
    }
    0
}

/// `SCE_APP_CONTENT_APPPARAM_ID_SKU_FLAG` — "is this the full game or a trial?".
const APPPARAM_ID_SKU_FLAG: i32 = 0;

/// The SKU answer: **full game**, not a trial.
///
/// The evidence is in the package itself: `sce_sys/param.sfo` carries `CATEGORY = "gd"`,
/// which is a full digital application (a trial would be `gde`). So "full" is the truth
/// about what is installed here, and a trial answer would silently lock content a title
/// otherwise ships.
///
/// The *encoding* is the part we are less sure of — this is the documented enum's "full"
/// value as we understand it, not something we have verified against hardware. If a title
/// ever behaves as a demo (content gated, a nag screen, an early exit), this constant is the
/// first thing to try changing.
const APPPARAM_SKUFLAG_FULL: i32 = 3;

/// `sceAppContentAppParamGetInt(paramId, int *outValue)` — read one of the integer
/// parameters the package declares. Only the SKU flag is answered; the rest are the
/// title's own `USER_DEFINED_PARAM_*` slots, which this package does not define, and a
/// fabricated value there would be a number the title never wrote.
#[ps4_syscall(id = SyscallId::SCE_APP_CONTENT_APP_PARAM_GET_INT, lib = crate::libs::LIB_KERNEL, name = "sceAppContentAppParamGetInt")]
pub fn sce_app_content_app_param_get_int(param_id: i32, out_value: u64) -> i32 {
    if param_id != APPPARAM_ID_SKU_FLAG {
        return -1;
    }
    let Some(gp) = GuestPtr::<i32>::new(out_value) else {
        return -1;
    };
    let _ = gp.write(APPPARAM_SKUFLAG_FULL);
    0
}

/// `sceAppContentGetAddcontInfo(SceNpUnifiedEntitlementLabel *label, SceAppContentAddcontInfo
/// *out)` — is this piece of downloadable content installed?
///
/// Refused: none is. Nothing is written into the out-struct either, because a zeroed
/// `AddcontInfo` still describes an entitlement — one with an empty label and a status the
/// title would read as meaningful. "There is no such add-on" is the honest answer, and it is
/// the one every DLC-aware title already handles, since players routinely own none.
#[ps4_syscall(id = SyscallId::SCE_APP_CONTENT_GET_ADDCONT_INFO, lib = crate::libs::LIB_KERNEL, name = "sceAppContentGetAddcontInfo")]
pub fn sce_app_content_get_addcont_info() -> i32 {
    -1
}
