use crate::context::NativeContext;
use ps4_core::guest_ptr::{GuestPtr, GuestSlice};
use ps4_core::kernel::get_kernel;
use ps4_macros::ps4_syscall;
use ps4_syscalls::SyscallId;
use tracing::{debug, info};

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
    // Validate the out-pointer BEFORE mapping, exactly as sce_kernel_allocate_direct_memory does:
    // k.mmap consumes arena address space as a side effect, so a bad `res` must reserve nothing.
    // Otherwise the map would succeed, the write below would be skipped, and the handler would
    // return 0 while the guest reads a stale `*res` as the mapped base — corrupting unrelated
    // memory (or faulting) and leaking the mapping.
    let Some(slot) = GuestPtr::<u64>::new(res as u64) else {
        return -14; // EFAULT: null / out-of-arena out-pointer
    };
    match k.mmap(addr, len, prot, flags, fd, offset) {
        Ok(ptr) => {
            if slot.write(ptr).is_err() {
                // The out-slot passed the arena check but the write seam rejected the store:
                // undo the mapping rather than leave one the guest never learned the base of.
                let _ = k.munmap(ptr, len);
                return -14; // EFAULT
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
    alignment: usize,
    _type_: i32,
    phys_addr_out: *mut u64,
) -> i32 {
    if let Some(k) = get_kernel() {
        // Validate the out-pointer FIRST, before committing any reservation. The reservation is
        // a side effect (a physical offset is consumed and never reused); if the write later
        // fails the arena check we'd return success (0) with the reservation standing while the
        // guest never received its offset → it uses a stale/garbage offset. So a bad out-ptr is
        // EFAULT and reserves nothing.
        let Some(slot) = GuestPtr::<u64>::new(phys_addr_out as u64) else {
            return -14; // EFAULT: null / out-of-arena out-pointer
        };
        // task-148: PS4 direct memory is *physical-offset* based. Reserve a physical range in
        // the kernel's dense direct-memory pool and return its **offset** (from 0), which the
        // guest later hands to MapDirectMemory (offset -> VA) and ReleaseDirectMemory (free by
        // offset). Honour the guest's alignment: Mono SGen requests 1 MB-aligned Large Object Space
        // sections here and derives each section header by masking the object pointer
        // (`chunk & ~0xfffff`); a mis-aligned base makes that math read garbage and fault.
        match k.allocate_direct_memory(length, alignment) {
            Ok(phys_off) => {
                // Write the physical offset out through the already-validated GuestPtr seam.
                let _ = slot.write(phys_off);
                0
            }
            Err(e) => e as i32,
        }
    } else {
        -1
    }
}

/// `sceKernelAvailableDirectMemorySize(searchStart, searchEnd, alignment, *physAddrOut,
/// *sizeOut)`: how much contiguous direct memory is still reservable in the window.
///
/// A native title calls this during startup to size its pools before it reserves anything —
/// which is why it is the second wall a UE4-class title hits and Celeste never did (Mono
/// reserves blindly and copes with ENOMEM).
///
/// Both out-pointers are validated BEFORE anything is reported, and neither is written on
/// failure: a guest that reads a stale `sizeOut` after a non-zero return would size its pool
/// from whatever was on its stack.
#[ps4_syscall(id = SyscallId::SCE_KERNEL_AVAILABLE_DIRECT_MEMORY_SIZE, lib = crate::libs::LIB_KERNEL, name = "sceKernelAvailableDirectMemorySize")]
pub fn sce_kernel_available_direct_memory_size(
    search_start: u64,
    search_end: u64,
    alignment: usize,
    phys_addr_out: *mut u64,
    size_out: *mut usize,
) -> i32 {
    let Some(k) = get_kernel() else { return -1 };
    let (Some(addr_slot), Some(size_slot)) = (
        GuestPtr::<u64>::new(phys_addr_out as u64),
        GuestPtr::<usize>::new(size_out as u64),
    ) else {
        return -14; // EFAULT
    };

    match k.available_direct_memory(search_start, search_end, alignment as u64) {
        Some((off, size)) => {
            let _ = addr_slot.write(off);
            let _ = size_slot.write(size as usize);
            0
        }
        // Nothing in the window fits. SCE_KERNEL_ERROR_EAGAIN is what the real call reports
        // when it cannot satisfy the request, and it is what a caller loops or backs off on.
        None => -11, // EAGAIN
    }
}

/// The flexible-memory budget a base PS4 grants an application, reported by
/// [`sce_kernel_available_flexible_memory_size`]. 448 MiB is the console's figure; the guest
/// sizes pools against it.
const FLEXIBLE_MEMORY_BUDGET: usize = 448 * 1024 * 1024;

/// `sceKernelAvailableFlexibleMemorySize(size_t *sizeOut)`: how much flexible memory is left.
///
/// Reports the full budget without subtracting what is already mapped, because we do not
/// track flexible usage. That is a deliberate over-report, and the direction matters: unlike
/// direct memory — a fixed 5 GiB pool where claiming space we cannot hand out makes the
/// guest's own allocator commit to a budget we would then refuse — flexible memory is backed
/// by host `mmap` inside a 64 GiB arena, so a guest that believes it has the full budget can
/// actually get it. Under-reporting is the harmful direction here; it would make a title
/// shrink or refuse its pools for no reason.
///
/// Revisit if a title reads this repeatedly to watch its own consumption: it would then see
/// a number that never moves.
#[ps4_syscall(id = SyscallId::SCE_KERNEL_AVAILABLE_FLEXIBLE_MEMORY_SIZE, lib = crate::libs::LIB_KERNEL, name = "sceKernelAvailableFlexibleMemorySize")]
pub fn sce_kernel_available_flexible_memory_size(size_out: *mut usize) -> i32 {
    let Some(slot) = GuestPtr::<usize>::new(size_out as u64) else {
        return -14; // EFAULT
    };
    let _ = slot.write(FLEXIBLE_MEMORY_BUDGET);
    0
}

/// `sceKernelReserveVirtualRange(void **addr, size_t len, int flags, size_t alignment)`:
/// claim address space WITHOUT committing memory to it.
///
/// UE4's allocator reserves a large aligned range up front and then commits pieces of it, so
/// this is the first thing its memory subsystem asks for. `*addr` carries the requested base
/// in (0 = anywhere) and receives the granted base out.
///
/// In this emulator a reservation is pure bookkeeping. The guest arena is one pre-mapped
/// identity-mapped span (doc-2 §1), so there are no host pages to commit or withhold — what
/// a reservation must actually do is make the range stop being FREE, so a later "anywhere"
/// allocation cannot be handed the same addresses. Recording a VMA does exactly that.
///
/// Consequence worth stating: we cannot fault on an access to reserved-but-uncommitted
/// memory the way real hardware would, because the pages are already backed. A guest bug of
/// that shape reads zeroes here instead of crashing. Detecting it would need per-page
/// protection over the arena, which is out of scope for a reservation call.
#[ps4_syscall(id = SyscallId::SCE_KERNEL_RESERVE_VIRTUAL_RANGE, lib = crate::libs::LIB_KERNEL, name = "sceKernelReserveVirtualRange")]
pub fn sce_kernel_reserve_virtual_range(
    addr_in_out: *mut u64,
    length: usize,
    _flags: i32,
    alignment: usize,
) -> i32 {
    let Some(slot) = GuestPtr::<u64>::new(addr_in_out as u64) else {
        return 22; // EINVAL
    };
    let Some(requested) = slot.read() else {
        return 22; // EINVAL
    };
    if length == 0 {
        return 22; // EINVAL
    }
    let Some(k) = get_kernel() else { return -1 };

    // PROT_READ|WRITE with MAP_ANON|MAP_PRIVATE, honouring the requested base when non-zero.
    // The protection is nominal — the arena is already readable and writable — but claiming
    // it read-write matches what the guest will commit here and keeps the VMA description
    // honest for the fault reporter.
    match k.mmap_aligned(requested, length, alignment, 3, 0x1002, -1, 0) {
        Ok(base) => {
            let _ = slot.write(base);
            0
        }
        Err(e) => e as i32,
    }
}

/// One `sceKernelBatchMap` request entry, 32 bytes.
///
/// **The layout is a HYPOTHESIS derived at runtime, not a documented struct.** The SDK header
/// we have carries only `void sceKernelBatchMap();` — no signature, no struct — and this
/// project reverse-engineers from the dumped guest binary rather than from other emulators.
/// So [`sce_kernel_batch_map`] logs every entry it receives, and the log is what confirms or
/// refutes the field placement below: a correct reading shows `offset` equal to an offset
/// `sceKernelAllocateDirectMemory` just handed out, a page-aligned `length`, and a small
/// `op`. Anything else means this struct is wrong and the trace says how.
#[repr(C)]
#[derive(Clone, Copy)]
struct BatchMapEntry {
    start: u64,
    offset: u64,
    length: u64,
    protection: u8,
    op: u8,
    _pad: [u8; 6],
}

/// `sceKernelBatchMap(SceKernelBatchMapEntry *entries, int count, int *processed)`: apply a
/// list of map/unmap/protect operations in one call.
///
/// UE4's allocator uses this to commit many pieces of a reserved range at once. Each entry is
/// dispatched to the same kernel paths the individual calls use, so behaviour cannot drift
/// between the batch and non-batch forms.
///
/// `*processed` receives how many entries were applied before the first failure — that
/// convention is what lets the caller retry the tail, so a failure stops the loop rather than
/// pressing on and reporting a count that implies work that never happened.
#[ps4_syscall(id = SyscallId::SCE_KERNEL_BATCH_MAP, lib = crate::libs::LIB_KERNEL, name = "sceKernelBatchMap")]
pub fn sce_kernel_batch_map(entries: u64, count: i32, processed_out: *mut i32) -> i32 {
    batch_map_impl(entries, count, processed_out)
}

/// `sceKernelBatchMap2(entries, count, processed, flags)` — the same list with a flags word
/// we do not model. Sharing the body is deliberate: two spellings of one operation that
/// really are identical here, unlike the timed-wait pairs that had to be split (task-216).
#[ps4_syscall(id = SyscallId::SCE_KERNEL_BATCH_MAP2, lib = crate::libs::LIB_KERNEL, name = "sceKernelBatchMap2")]
pub fn sce_kernel_batch_map2(
    entries: u64,
    count: i32,
    processed_out: *mut i32,
    _flags: i32,
) -> i32 {
    batch_map_impl(entries, count, processed_out)
}

fn batch_map_impl(entries: u64, count: i32, processed_out: *mut i32) -> i32 {
    if count < 0 {
        return 22; // EINVAL
    }
    let count = count as usize;
    let Some(list) = GuestSlice::<BatchMapEntry>::new(entries, count) else {
        return 14; // EFAULT
    };
    let Some(list) = list.read_vec() else {
        return 14; // EFAULT
    };
    let Some(k) = get_kernel() else { return -1 };

    let mut done = 0usize;
    let mut result = 0i32;
    for e in &list {
        // Bring-up trace (task-29 follow-on): this is the evidence that decides whether the
        // struct above is read correctly. Drop it once a title has confirmed the layout.
        let raw = GuestSlice::<u8>::new(entries + (done * 32) as u64, 64)
            .and_then(|s| s.read_vec())
            .map(|b| {
                b.chunks(8)
                    .map(|c| c.iter().map(|x| format!("{x:02x}")).collect::<String>())
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default();
        tracing::info!(
            "[BATCHMAP] {}/{} op={} start={:#x} offset={:#x} len={:#x} prot={:#x} raw=[{raw}]",
            done + 1,
            count,
            e.op,
            e.start,
            e.offset,
            e.length,
            e.protection
        );

        let len = e.length as usize;
        let prot = e.protection as i32;
        // Dispatch on CONTENT, not on the op byte. The first observed entry reads
        // `prot=0x03` at +0x18 — plainly right — and `0x30` at +0x19, which matches no
        // operation code, with `0x20003003` across the whole word. Values of the shape
        // `0x2000....` show up all over this title's uninitialised argument registers, so
        // the trailing bytes are most likely stack residue the guest never sets, and the
        // operation is not readable there.
        //
        // What the entry unambiguously says is the rest: a direct-memory offset, a target
        // address, a length and a protection. A non-zero offset can only mean "map this
        // direct memory here" — the offset space exists for nothing else — so that is what
        // we act on, and everything else is logged rather than guessed at. Revisit when a
        // title issues a batch with mixed operations and the trace shows the encoding.
        let step = if e.offset != 0 {
            let va = k.map_direct_memory(e.offset, len);
            if va == 0 { 12 } else { 0 } // ENOMEM
        } else if e.start != 0 && len > 0 {
            // No offset: an anonymous commit of already-reserved address space.
            match k.mmap(e.start, len, prot, 0x1002, -1, 0) {
                Ok(_) => 0,
                Err(err) => err as i32,
            }
        } else {
            0
        };
        if step != 0 {
            result = step;
            break;
        }
        done += 1;
    }

    if let Some(slot) = GuestPtr::<i32>::new(processed_out as u64) {
        let _ = slot.write(done as i32);
    }
    result
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
    // task-115: read/write the in-out address through the validated GuestPtr seam; a junk
    // pointer fails the constructor (out of arena) and returns EINVAL instead of segfaulting.
    let Some(slot) = GuestPtr::<u64>::new(addr_in_out as u64) else {
        return 22; // EINVAL
    };
    // A failed read must NOT silently degrade to 0 ("map anywhere"): the guest asked for a
    // specific address (or 0) and a read fault means the in-out slot is unreadable — honour it
    // as EINVAL rather than fabricating an "anywhere" request over a bad pointer. Matches this
    // function's existing EINVAL (positive 22, as `k.mmap`'s error returns via `e as i32`).
    let Some(requested) = slot.read() else {
        return 22; // EINVAL
    };
    let Some(k) = get_kernel() else { return -1 };
    // MAP_ANON | MAP_PRIVATE; honour the requested address when non-zero, else "anywhere".
    match k.mmap(requested, length, prot, 0x1002, -1, 0) {
        Ok(addr) => {
            let _ = slot.write(addr);
            0
        }
        Err(e) => e as i32,
    }
}

#[ps4_syscall(id = SyscallId::SCE_KERNEL_MAP_DIRECT_MEMORY, lib = crate::libs::LIB_KERNEL, name = "sceKernelMapDirectMemory")]
pub fn sce_kernel_map_direct_memory(
    addr: *mut u64,
    length: usize,
    _prot: i32,
    _flags: i32,
    start: u64,
    _alignment: usize,
) -> i32 {
    // task-148: map the physical offset `start` (from AllocateDirectMemory) to its VA
    // (`va = POOL_BASE + start`) and back the pages, so a later ReleaseDirectMemory by offset
    // frees exactly this span. The VA is a pure function of the offset, matching how the guest
    // (Mono) computes its own phys->VA table — that is what keeps the two in lockstep.
    //
    // Validate the out-pointer BEFORE mapping, as sce_kernel_allocate_direct_memory does:
    // map_direct_memory backs pages as a side effect, so a bad `addr` must map nothing rather
    // than return 0 and leave the guest reading a stale `*addr` as the VA of a leaked mapping.
    let Some(slot) = GuestPtr::<u64>::new(addr as u64) else {
        return -14; // EFAULT: null / out-of-arena out-pointer
    };
    let va = match get_kernel() {
        Some(k) => k.map_direct_memory(start, length),
        None => start,
    };
    // task-138: write the out-param through the already-validated GuestPtr seam.
    let _ = slot.write(va);
    0
}

#[ps4_syscall(id = SyscallId::SCE_KERNEL_RELEASE_DIRECT_MEMORY, lib = crate::libs::LIB_KERNEL, name = "sceKernelReleaseDirectMemory")]
pub fn sce_kernel_release_direct_memory(start: u64, length: usize) -> i32 {
    // task-148: `start` is a **physical offset**, not a VA. Free by offset — the kernel
    // resolves it to the VA span it mapped (and unmaps that) or, for an untracked / zero /
    // already-freed offset (Mono issues hundreds of releases with start=0x0), does a clean
    // no-op that returns success. The old code blindly `munmap`ped `start` as if it were a
    // VA, desyncing Mono's phys↔VA bookkeeping and tripping its mono-mmap-orbis.c:219 assert.
    if let Some(k) = get_kernel() {
        k.release_direct_memory(start, length)
    } else {
        0
    }
}

// Report the direct-memory pool size the guest can allocate from. task-148: this MUST equal
// the physical-offset pool the kernel serves (`DIRECT_MEMORY_POOL_SIZE`) so a guest that
// sizes its own pool from this call never overflows ours.
#[ps4_syscall(id = SyscallId::SCE_KERNEL_GET_DIRECT_MEMORY_SIZE, lib = crate::libs::LIB_KERNEL, name = "sceKernelGetDirectMemorySize")]
pub fn sce_kernel_get_direct_memory_size() -> u64 {
    ps4_core::kernel::DIRECT_MEMORY_POOL_SIZE
}

// `SceKernelVirtualQueryInfo` byte offsets (Orbis ABI). Total struct is 0x48 bytes; the
// guest passes its `sizeof` as `info_size`, which we honour so we never write past a
// caller that used a smaller/older layout.
const VQ_OFF_START: usize = 0x00; // void*
const VQ_OFF_END: usize = 0x08; // void*
const VQ_OFF_OFFSET: usize = 0x10; // off_t (backing offset; 0 for anon/flexible)
const VQ_OFF_PROTECTION: usize = 0x18; // int
const VQ_OFF_MEMORY_TYPE: usize = 0x1C; // int
const VQ_OFF_BITS: usize = 0x20; // packed is_flexible/is_direct/is_stack/is_pooled/is_committed
const VQ_OFF_NAME: usize = 0x21; // char[32]
const VQ_NAME_LEN: usize = 32;
const VQ_INFO_SIZE: usize = 0x48;

// The `flags` bit that asks for the next region at/above `addr` rather than the one
// containing it (so a caller can walk the whole map). Bit 0 per the Orbis ABI.
const SCE_KERNEL_VQ_FIND_NEXT: i32 = 1;

// sceKernelVirtualQuery(const void* addr, int flags, SceKernelVirtualQueryInfo* info,
// size_t infoSize): fill *info from the VMA the memory manager tracks for `addr`. Mono's
// GC/JIT queries region metadata through this.
#[ps4_syscall(id = SyscallId::SCE_KERNEL_VIRTUAL_QUERY, lib = crate::libs::LIB_KERNEL, name = "sceKernelVirtualQuery")]
pub fn sce_kernel_virtual_query(addr: u64, flags: i32, info: *mut u8, info_size: usize) -> i32 {
    debug!(
        "[SYSCALL] sceKernelVirtualQuery(addr={:#x}, flags={:#x}, info={:p}, infoSize={:#x})",
        addr, flags, info, info_size
    );
    if info.is_null() || !crate::is_guest_ptr(info) || info_size < VQ_OFF_PROTECTION {
        return 22i32.wrapping_neg(); // -EINVAL
    }
    let Some(k) = get_kernel() else {
        return 22i32.wrapping_neg();
    };
    let find_next = flags & SCE_KERNEL_VQ_FIND_NEXT != 0;
    let Some(r) = k.virtual_query(addr, find_next) else {
        return 22i32.wrapping_neg(); // -EINVAL: addr not in any region
    };

    // Build the caller's buffer locally (zeroed, honouring info_size, then the fields that fit),
    // then write it in one range-validated, SMC-tracked shot (task-115): a bad/near-arena-top
    // `info` fails clean instead of overrunning host memory.
    let writable = info_size.min(VQ_INFO_SIZE);
    let mut out = vec![0u8; writable];
    let mut put_u64 = |off: usize, v: u64| {
        if off + 8 <= writable {
            out[off..off + 8].copy_from_slice(&v.to_le_bytes());
        }
    };
    put_u64(VQ_OFF_START, r.start);
    put_u64(VQ_OFF_END, r.end);
    put_u64(VQ_OFF_OFFSET, 0);
    let mut put_i32 = |off: usize, v: i32| {
        if off + 4 <= writable {
            out[off..off + 4].copy_from_slice(&v.to_le_bytes());
        }
    };
    put_i32(VQ_OFF_PROTECTION, r.protection);
    put_i32(VQ_OFF_MEMORY_TYPE, r.memory_type);
    // Bitfield byte: is_committed is always set for a live region; is_flexible tracks
    // anonymous/flexible mappings. Layout LSB-first: flexible(0) direct(1) stack(2)
    // pooled(3) committed(4).
    if VQ_OFF_BITS < writable {
        out[VQ_OFF_BITS] = (r.is_flexible as u8) | ((!r.is_flexible as u8) << 1) | (1 << 4);
    }
    if VQ_OFF_NAME < writable {
        let name = r.name.as_bytes();
        let n = name
            .len()
            .min(VQ_NAME_LEN - 1)
            .min(writable - VQ_OFF_NAME - 1);
        out[VQ_OFF_NAME..VQ_OFF_NAME + n].copy_from_slice(&name[..n]);
    }
    if let Some(gs) = GuestSlice::<u8>::new(info as u64, writable) {
        let _ = gs.write_slice(&out);
    }
    0
}
