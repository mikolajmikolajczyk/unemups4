//! `libSceSaveData` HLE — real local persistent save mounting.
//!
//! A game initializes the subsystem (`sceSaveDataInitialize3`), then mounts a save
//! slot (`sceSaveDataMount2`) naming a directory; the mount registers a host directory
//! under a guest mount point (`/savedataN`) so the game's subsequent
//! `sceKernelOpen("/savedataN/...")` reads and writes hit real files on the host. The
//! host save root lives beside the title's dump (`<game_dir>/savedata/<dir>/`) and so
//! persists across runs. `sceSaveDataUmount2` unregisters the mount.
//!
//! Struct layout was reverse-engineered from the guest binary at runtime (see the task
//! notes): the request's `userId` is at +0x00, a `dirName` **pointer** at +0x08 (to a
//! 32-byte NUL-terminated name), `blocks` at +0x10, `mountMode` at +0x18; the result's
//! `mountPoint` is a 16-byte char array at +0x00, `requiredBlocks` a u64 at +0x10, and
//! `mountStatus` a u32 at +0x1c.

use crate::context::NativeContext;
use ps4_core::guest_ptr::{GuestPtr, GuestSlice};
use ps4_macros::ps4_syscall;
use ps4_syscalls::SyscallId;
use std::ffi::c_void;
use tracing::{info, warn};

const REQ_USER_ID: u64 = 0x00;
const REQ_DIR_NAME_PTR: u64 = 0x08;
const REQ_BLOCKS: u64 = 0x10;
const REQ_MOUNT_MODE: u64 = 0x18;

const RES_MOUNT_POINT: u64 = 0x00;
const RES_REQUIRED_BLOCKS: u64 = 0x10;
const RES_MOUNT_STATUS: u64 = 0x1c;

/// Read a fixed-size, possibly-not-NUL-terminated field (`SceSaveDataDirName` /
/// `SceSaveDataMountPoint`) from a guest pointer through the range-validated bounded-read seam
/// (task-115): the `max`-byte run is read in one VMA-checked shot (a bad/near-arena-top ptr
/// yields `None` instead of a host over-read), then truncated at the first NUL. Unlike
/// [`ps4_core::guest_ptr::read_cstr`], the field may fill its whole width with no terminator,
/// so a missing NUL means "use all `max` bytes", not `None`.
fn read_fixed_cstr(ptr: u64, max: usize) -> Option<String> {
    let bytes = GuestSlice::<u8>::new(ptr, max)?.read_vec()?;
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(max);
    std::str::from_utf8(&bytes[..end]).ok().map(str::to_string)
}

#[ps4_syscall(id = SyscallId::SCE_SAVE_DATA_INITIALIZE3, lib = crate::libs::LIB_SCE_SAVE_DATA, name = "sceSaveDataInitialize3")]
pub fn sce_save_data_initialize3(_init_param: *const c_void) -> i32 {
    info!("[SAVEDATA] sceSaveDataInitialize3");
    0
}

#[ps4_syscall(id = SyscallId::SCE_SAVE_DATA_MOUNT2, lib = crate::libs::LIB_SCE_SAVE_DATA, name = "sceSaveDataMount2")]
pub fn sce_save_data_mount2(request: u64, result: u64) -> i32 {
    if !crate::is_guest_ptr(result as *const u8) {
        return -0x16; // -EINVAL
    }

    // Bound the whole request struct up front. The field reads below form `request + REQ_*`
    // addresses; a near-u64::MAX `request` would overflow that add (panic under overflow-checks,
    // wrap under release) before the per-field arena check runs. The last field is mountMode
    // (u32) at +0x18, so the struct footprint is REQ_MOUNT_MODE + 4 (0x1c) bytes — reject a
    // request not wholly in-arena as -EINVAL, matching the `result` guard above.
    if !crate::is_guest_range(request, REQ_MOUNT_MODE + 4) {
        return -0x16; // -EINVAL
    }

    // task-115: read the request fields through the range-validated bounded seam. Each field
    // handle validates base + size against the arena, so a bad `request` (or one that straddles
    // an unmapped page) yields `None` → -EINVAL instead of a host over-read.
    let (Some(user_id), Some(dir_name_ptr), Some(blocks), Some(mount_mode)) = (
        GuestPtr::<u32>::new(request + REQ_USER_ID).and_then(GuestPtr::read),
        GuestPtr::<u64>::new(request + REQ_DIR_NAME_PTR).and_then(GuestPtr::read),
        GuestPtr::<u64>::new(request + REQ_BLOCKS).and_then(GuestPtr::read),
        GuestPtr::<u32>::new(request + REQ_MOUNT_MODE).and_then(GuestPtr::read),
    ) else {
        return -0x16; // -EINVAL
    };

    let Some(dir_name) = read_fixed_cstr(dir_name_ptr, 32) else {
        warn!("[SAVEDATA] sceSaveDataMount2: bad dirName ptr {dir_name_ptr:#x}");
        return -0x16;
    };

    let Some(kernel) = ps4_core::kernel::get_kernel() else {
        return -0x16;
    };

    match kernel.savedata_mount(user_id, &dir_name, blocks, mount_mode) {
        Ok((mount_point, mount_status, required_blocks)) => {
            // Build the result struct locally (mountPoint[16]@0x00, requiredBlocks u64@0x10,
            // mountStatus u32@0x1c), then write it in one range-validated, SMC-tracked shot
            // (task-115): a bad/near-arena-top `result` fails clean instead of overrunning.
            let mp = mount_point.as_bytes();
            let mut out = [0u8; 0x20];
            let n = mp.len().min(15);
            out[RES_MOUNT_POINT as usize..RES_MOUNT_POINT as usize + n].copy_from_slice(&mp[..n]);
            out[RES_REQUIRED_BLOCKS as usize..RES_REQUIRED_BLOCKS as usize + 8]
                .copy_from_slice(&required_blocks.to_le_bytes());
            out[RES_MOUNT_STATUS as usize..RES_MOUNT_STATUS as usize + 4]
                .copy_from_slice(&mount_status.to_le_bytes());
            if let Some(gs) = GuestSlice::<u8>::new(result + RES_MOUNT_POINT, 0x20) {
                let _ = gs.write_slice(&out);
            }
            info!(
                "[SAVEDATA] sceSaveDataMount2 dir='{dir_name}' -> mountPoint='{mount_point}' status={mount_status} blocks={required_blocks}"
            );
            0
        }
        Err(errno) => {
            warn!("[SAVEDATA] sceSaveDataMount2 dir='{dir_name}' failed: errno {errno}");
            -errno
        }
    }
}

#[ps4_syscall(id = SyscallId::SCE_SAVE_DATA_UMOUNT2, lib = crate::libs::LIB_SCE_SAVE_DATA, name = "sceSaveDataUmount2")]
pub fn sce_save_data_umount2(request: u64) -> i32 {
    savedata_umount_common("sceSaveDataUmount2", request)
}

#[ps4_syscall(id = SyscallId::SCE_SAVE_DATA_UMOUNT, lib = crate::libs::LIB_SCE_SAVE_DATA, name = "sceSaveDataUmount")]
pub fn sce_save_data_umount(mount_point: u64) -> i32 {
    // The v1 umount takes the mount-point struct directly (a 16-byte char array), not a
    // request wrapper.
    let Some(mp) = read_fixed_cstr(mount_point, 16) else {
        return -0x16;
    };
    umount_by_name("sceSaveDataUmount", &mp)
}

/// v2 umount: the request's first field is a pointer to the `SceSaveDataMountPoint`.
fn savedata_umount_common(who: &str, request: u64) -> i32 {
    // task-115: read the mount-point pointer field through the range-validated bounded seam
    // (a bad `request` yields `None` → -EINVAL instead of a host deref).
    let Some(mp_ptr) = GuestPtr::<u64>::new(request).and_then(GuestPtr::read) else {
        return -0x16;
    };
    let Some(mp) = read_fixed_cstr(mp_ptr, 16) else {
        return -0x16;
    };
    umount_by_name(who, &mp)
}

fn umount_by_name(who: &str, mount_point: &str) -> i32 {
    let Some(kernel) = ps4_core::kernel::get_kernel() else {
        return -0x16;
    };
    match kernel.savedata_umount(mount_point) {
        Ok(()) => {
            info!("[SAVEDATA] {who} '{mount_point}'");
            0
        }
        Err(errno) => {
            warn!("[SAVEDATA] {who} '{mount_point}' failed: errno {errno}");
            -errno
        }
    }
}
