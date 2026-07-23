//! Env-gated present-path counters for the aggregate profiler.
//!
//! Mirrors the `crates/cpu` profiler gate exactly — a `OnceLock<bool>` resolved once
//! from `UNEMUPS4_PROFILE`, then a cached branch. The gpu crate has no dependency on
//! `ps4-cpu`, so it duplicates the tiny env read rather than crossing a crate boundary
//! (the env var is the shared contract). When disabled, the present loop never touches
//! these atomics.
//!
//! Phase timings are accumulated in nanoseconds across the whole run; the dump thread
//! divides by `frames` to report an average per phase per frame.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// Environment variable enabling the profiler (shared with `ps4_cpu::profile`). Any
/// enabling value (`1` or a positive integer) turns present-path timing on; the gpu
/// crate does not interpret the interval (that's the dump thread's job).
const PROFILE_ENV: &str = "UNEMUPS4_PROFILE";

/// Whether present-path timing is enabled. Resolved once from [`PROFILE_ENV`].
#[inline]
pub fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| match std::env::var(PROFILE_ENV).ok().as_deref() {
        None | Some("") | Some("0") => false,
        Some(v) => v.parse::<u64>().map(|n| n > 0).unwrap_or(false),
    })
}

/// Cumulative present-path phase counters, all relaxed `AtomicU64`.
pub struct PresentStats {
    /// Completed present iterations (frames drawn).
    pub frames: AtomicU64,
    pub fence_wait_ns: AtomicU64,
    pub acquire_ns: AtomicU64,
    pub fb_copy_ns: AtomicU64,
    pub record_submit_ns: AtomicU64,
    pub present_ns: AtomicU64,
    /// Time spent in the frame-pacing `sleep` (not GPU work — the intentional cap).
    pub pace_sleep_ns: AtomicU64,
    /// Wall time between consecutive `queue_present` calls, and its extremes.
    pub present_gap_ns: AtomicU64,
    pub present_gap_min_ns: AtomicU64,
    pub present_gap_max_ns: AtomicU64,
    /// Presents issued less than one 60 Hz refresh period after the previous one — see
    /// [`note_present`] for why this is the closest thing to a MAILBOX discard count.
    pub presents_within_vblank: AtomicU64,
}

impl PresentStats {
    const fn new() -> PresentStats {
        PresentStats {
            frames: AtomicU64::new(0),
            fence_wait_ns: AtomicU64::new(0),
            acquire_ns: AtomicU64::new(0),
            fb_copy_ns: AtomicU64::new(0),
            record_submit_ns: AtomicU64::new(0),
            present_ns: AtomicU64::new(0),
            pace_sleep_ns: AtomicU64::new(0),
            present_gap_ns: AtomicU64::new(0),
            present_gap_min_ns: AtomicU64::new(u64::MAX),
            present_gap_max_ns: AtomicU64::new(0),
            presents_within_vblank: AtomicU64::new(0),
        }
    }
}

/// The single process-wide present-path counters.
pub static PRESENT: PresentStats = PresentStats::new();

/// One refresh period at 60 Hz. Used only as the "could a discard even happen" boundary
/// below; the actual display refresh is not queried.
const VBLANK_NS: u64 = 16_000_000;

/// Timebase for the inter-present interval, and the previous present's offset into it.
/// Written only by the display thread.
static PRESENT_EPOCH: OnceLock<Instant> = OnceLock::new();
static LAST_PRESENT_NS: AtomicU64 = AtomicU64::new(0);

/// Record one `queue_present`, for the inter-present interval and the MAILBOX-discard
/// bound.
///
/// **Vulkan cannot tell us how many images MAILBOX discarded.** `queue_present` returns
/// `SUCCESS` whether the image reached the display or was replaced in the queue, and the
/// extensions that would report it — `VK_KHR_present_wait` (when a present became visible)
/// and `VK_GOOGLE_display_timing` (`actualPresentTime` per present id) — are not enabled on
/// this swapchain. What *is* observable is the necessary condition: MAILBOX can only drop
/// an image when a newer one is queued before the next vblank, so a present issued less
/// than one refresh period after the previous one is the only kind that can be discarded.
/// A zero here is therefore a proof of absence; a non-zero is an upper bound, not a count.
#[inline]
pub fn note_present() {
    let epoch = PRESENT_EPOCH.get_or_init(Instant::now);
    let now = epoch.elapsed().as_nanos() as u64;
    let prev = LAST_PRESENT_NS.swap(now, Ordering::Relaxed);
    if prev == 0 {
        return;
    }
    let gap = now.saturating_sub(prev);
    PRESENT.present_gap_ns.fetch_add(gap, Ordering::Relaxed);
    PRESENT.present_gap_min_ns.fetch_min(gap, Ordering::Relaxed);
    PRESENT.present_gap_max_ns.fetch_max(gap, Ordering::Relaxed);
    if gap < VBLANK_NS {
        PRESENT
            .presents_within_vblank
            .fetch_add(1, Ordering::Relaxed);
    }
}

/// Cumulative GNM submit-path counters, all relaxed `AtomicU64`.
///
/// The present counters above cover only what happens inside `AshBackend::present`. A
/// guest flip syscall is much wider than that: it decodes the submit, ships one command
/// list per draw batch to the display thread and blocks on each, then blocks once more on
/// the flip. These counters cover that wider path, split guest-thread side (the channel
/// round trips) from display-thread side (the work the round trip is waiting for).
pub struct SubmitStats {
    /// Guest-thread `run_command_list` round trips (channel send + block until recorded).
    pub guest_submit_calls: AtomicU64,
    pub guest_submit_ns: AtomicU64,
    /// Guest-thread `submit_flip` round trips (block until the display thread presented).
    pub guest_flip_ns: AtomicU64,
    /// Display-thread `AshBackend::run_command_list`, end to end.
    pub backend_ns: AtomicU64,
    /// The `BackendCmd` walk inside it (resource cache + pipeline creation), i.e.
    /// `backend_ns` minus `record_passes_ns` and `readback_ns`.
    pub walk_ns: AtomicU64,
    /// `record_passes`, end to end.
    pub record_passes_ns: AtomicU64,
    /// The pass loop inside it (command recording), transient creation included.
    pub record_ns: AtomicU64,
    /// Creating the transient render passes / framebuffers / descriptor pools.
    pub transient_create_ns: AtomicU64,
    /// Destroying them (plus freeing the command buffer) after the fence wait.
    pub transient_destroy_ns: AtomicU64,
    pub queue_submit_ns: AtomicU64,
    /// The per-submit `wait_for_fences` that blocks until the GPU drained the list.
    pub draw_fence_ns: AtomicU64,
    /// Render-target readbacks + diagnostic PNG dumps, run after `record_passes`.
    pub readback_ns: AtomicU64,
    pub passes: AtomicU64,
    pub draws: AtomicU64,
    pub transient_render_passes: AtomicU64,
    pub transient_framebuffers: AtomicU64,
    pub descriptor_pools: AtomicU64,
}

impl SubmitStats {
    const fn new() -> SubmitStats {
        SubmitStats {
            guest_submit_calls: AtomicU64::new(0),
            guest_submit_ns: AtomicU64::new(0),
            guest_flip_ns: AtomicU64::new(0),
            backend_ns: AtomicU64::new(0),
            walk_ns: AtomicU64::new(0),
            record_passes_ns: AtomicU64::new(0),
            record_ns: AtomicU64::new(0),
            transient_create_ns: AtomicU64::new(0),
            transient_destroy_ns: AtomicU64::new(0),
            queue_submit_ns: AtomicU64::new(0),
            draw_fence_ns: AtomicU64::new(0),
            readback_ns: AtomicU64::new(0),
            passes: AtomicU64::new(0),
            draws: AtomicU64::new(0),
            transient_render_passes: AtomicU64::new(0),
            transient_framebuffers: AtomicU64::new(0),
            descriptor_pools: AtomicU64::new(0),
        }
    }
}

/// The single process-wide submit-path counters.
pub static SUBMIT: SubmitStats = SubmitStats::new();

/// Names of the [`BackendCmd`](ps4_core::gpu::BackendCmd) variants, indexed by
/// [`cmd_index`]. The order is the enum's own declaration order.
pub const CMD_NAMES: [&str; 18] = [
    "CreatePipeline",
    "BindPipeline",
    "DrawAuto",
    "BindVertexBuffer",
    "BindStorageBuffer",
    "BindConstBuffer",
    "DrawIndexed",
    "SetViewport",
    "SetScissor",
    "CreateBuffer",
    "UploadBuffer",
    "ImportBuffer",
    "FreeResource",
    "CreateImage",
    "UploadImage",
    "CreateSampler",
    "BindTexture",
    "other",
];

/// The index [`CMD_NAMES`] and the per-variant counters use for one command. The tail
/// variants (render-target create/readback/dump/set) share the `other` slot: they are
/// either off by default or emitted a handful of times per flip, and giving each its own
/// row would bury the ones that fire thousands of times.
#[inline]
pub fn cmd_index(cmd: &ps4_core::gpu::BackendCmd) -> usize {
    use ps4_core::gpu::BackendCmd as C;
    match cmd {
        C::CreatePipeline { .. } => 0,
        C::BindPipeline { .. } => 1,
        C::DrawAuto { .. } => 2,
        C::BindVertexBuffer { .. } => 3,
        C::BindStorageBuffer { .. } => 4,
        C::BindConstBuffer { .. } => 5,
        C::DrawIndexed { .. } => 6,
        C::SetViewport(_) => 7,
        C::SetScissor(_) => 8,
        C::CreateBuffer { .. } => 9,
        C::UploadBuffer { .. } => 10,
        C::ImportBuffer { .. } => 11,
        C::FreeResource { .. } => 12,
        C::CreateImage { .. } => 13,
        C::UploadImage { .. } => 14,
        C::CreateSampler { .. } => 15,
        C::BindTexture { .. } => 16,
        _ => 17,
    }
}

/// Per-variant command counts and time inside the display-thread `BackendCmd` walk
/// (task-222), indexed by [`cmd_index`]. Written only by the display thread, and only
/// when the profiler is on — the walk reads the clock once per command and charges the
/// interval to the command that just ran, so the whole breakdown costs one `Instant::now`
/// per command rather than two.
pub struct CmdStats {
    pub count: [AtomicU64; CMD_NAMES.len()],
    pub ns: [AtomicU64; CMD_NAMES.len()],
    /// Bytes handed to `UploadBuffer`, to tell a slow memcpy from many small ones.
    pub upload_bytes: AtomicU64,
}

impl CmdStats {
    const fn new() -> CmdStats {
        #[allow(clippy::declare_interior_mutable_const)]
        const Z: AtomicU64 = AtomicU64::new(0);
        CmdStats {
            count: [Z; CMD_NAMES.len()],
            ns: [Z; CMD_NAMES.len()],
            upload_bytes: AtomicU64::new(0),
        }
    }
}

/// The single process-wide per-variant walk counters.
pub static CMDS: CmdStats = CmdStats::new();

/// Cache-buffer allocator counters (task-223), all relaxed `AtomicU64`.
///
/// `CreateBuffer` was 97% of the display-thread walk at roughly a millisecond each, which
/// is two orders of magnitude more than a `vkCreateBuffer` costs. The suspicion these
/// counters test is the *shape* of the allocation rather than its rate: one dedicated
/// `VkDeviceMemory` plus one persistent mapping per buffer, none of them ever freed, so the
/// live-allocation count climbs for the whole run and every new allocation is slower than
/// the last. `live_allocations` next to `live_buffers` says whether that is what happens,
/// and `recycled` vs `fresh` says how much of the steady state a free list absorbs.
pub struct PoolStats {
    /// Cache buffers the backend currently holds (gauge).
    pub live_buffers: AtomicU64,
    /// Device allocations backing them (gauge). One per buffer is the pathology.
    pub live_allocations: AtomicU64,
    /// Bytes those device allocations cover (gauge).
    pub alloc_bytes: AtomicU64,
    /// `CreateBuffer`s served from the recycle free list — no Vulkan call at all.
    pub recycled: AtomicU64,
    /// `CreateBuffer`s that had to build a fresh `vk::Buffer`.
    pub fresh: AtomicU64,
    /// Suballocation blocks carved for those fresh buffers.
    pub blocks: AtomicU64,
}

impl PoolStats {
    const fn new() -> PoolStats {
        PoolStats {
            live_buffers: AtomicU64::new(0),
            live_allocations: AtomicU64::new(0),
            alloc_bytes: AtomicU64::new(0),
            recycled: AtomicU64::new(0),
            fresh: AtomicU64::new(0),
            blocks: AtomicU64::new(0),
        }
    }
}

/// The single process-wide cache-buffer allocator counters.
pub static POOL: PoolStats = PoolStats::new();

/// A consistent read of the cache-buffer allocator counters for one dump.
#[derive(Clone, Copy, Debug, Default)]
pub struct PoolSnapshot {
    pub live_buffers: u64,
    pub live_allocations: u64,
    pub alloc_bytes: u64,
    pub recycled: u64,
    pub fresh: u64,
    pub blocks: u64,
}

/// Snapshot the cache-buffer allocator counters (relaxed loads).
pub fn pool_snapshot() -> PoolSnapshot {
    PoolSnapshot {
        live_buffers: POOL.live_buffers.load(Ordering::Relaxed),
        live_allocations: POOL.live_allocations.load(Ordering::Relaxed),
        alloc_bytes: POOL.alloc_bytes.load(Ordering::Relaxed),
        recycled: POOL.recycled.load(Ordering::Relaxed),
        fresh: POOL.fresh.load(Ordering::Relaxed),
        blocks: POOL.blocks.load(Ordering::Relaxed),
    }
}

/// A consistent read of the per-variant walk counters for one dump.
#[derive(Clone, Copy, Debug)]
pub struct CmdSnapshot {
    pub count: [u64; CMD_NAMES.len()],
    pub ns: [u64; CMD_NAMES.len()],
    pub upload_bytes: u64,
}

impl Default for CmdSnapshot {
    fn default() -> CmdSnapshot {
        CmdSnapshot {
            count: [0; CMD_NAMES.len()],
            ns: [0; CMD_NAMES.len()],
            upload_bytes: 0,
        }
    }
}

/// Snapshot the per-variant walk counters (relaxed loads).
pub fn cmd_snapshot() -> CmdSnapshot {
    let mut snap = CmdSnapshot::default();
    for i in 0..CMD_NAMES.len() {
        snap.count[i] = CMDS.count[i].load(Ordering::Relaxed);
        snap.ns[i] = CMDS.ns[i].load(Ordering::Relaxed);
    }
    snap.upload_bytes = CMDS.upload_bytes.load(Ordering::Relaxed);
    snap
}

/// A consistent read of the present-path counters for one dump.
#[derive(Clone, Copy, Debug, Default)]
pub struct PresentSnapshot {
    pub frames: u64,
    pub fence_wait_ns: u64,
    pub acquire_ns: u64,
    pub fb_copy_ns: u64,
    pub record_submit_ns: u64,
    pub present_ns: u64,
    pub pace_sleep_ns: u64,
    pub present_gap_ns: u64,
    pub present_gap_min_ns: u64,
    pub present_gap_max_ns: u64,
    pub presents_within_vblank: u64,
}

/// Take the present-interval extremes, resetting them so the next reader sees only the
/// intervals since this call. Single reader (the dump thread) — the whole-run extremes are
/// dominated by boot, which tells nobody anything about judder.
pub fn take_present_gap_extremes() -> (u64, u64) {
    let min = PRESENT.present_gap_min_ns.swap(u64::MAX, Ordering::Relaxed);
    (
        if min == u64::MAX { 0 } else { min },
        PRESENT.present_gap_max_ns.swap(0, Ordering::Relaxed),
    )
}

/// Snapshot the present-path counters (relaxed loads).
pub fn snapshot() -> PresentSnapshot {
    let gap_min = PRESENT.present_gap_min_ns.load(Ordering::Relaxed);
    PresentSnapshot {
        present_gap_ns: PRESENT.present_gap_ns.load(Ordering::Relaxed),
        present_gap_min_ns: if gap_min == u64::MAX { 0 } else { gap_min },
        present_gap_max_ns: PRESENT.present_gap_max_ns.load(Ordering::Relaxed),
        presents_within_vblank: PRESENT.presents_within_vblank.load(Ordering::Relaxed),
        frames: PRESENT.frames.load(Ordering::Relaxed),
        fence_wait_ns: PRESENT.fence_wait_ns.load(Ordering::Relaxed),
        acquire_ns: PRESENT.acquire_ns.load(Ordering::Relaxed),
        fb_copy_ns: PRESENT.fb_copy_ns.load(Ordering::Relaxed),
        record_submit_ns: PRESENT.record_submit_ns.load(Ordering::Relaxed),
        present_ns: PRESENT.present_ns.load(Ordering::Relaxed),
        pace_sleep_ns: PRESENT.pace_sleep_ns.load(Ordering::Relaxed),
    }
}

/// A consistent read of the submit-path counters for one dump.
#[derive(Clone, Copy, Debug, Default)]
pub struct SubmitSnapshot {
    pub guest_submit_calls: u64,
    pub guest_submit_ns: u64,
    pub guest_flip_ns: u64,
    pub backend_ns: u64,
    pub walk_ns: u64,
    pub record_passes_ns: u64,
    pub record_ns: u64,
    pub transient_create_ns: u64,
    pub transient_destroy_ns: u64,
    pub queue_submit_ns: u64,
    pub draw_fence_ns: u64,
    pub readback_ns: u64,
    pub passes: u64,
    pub draws: u64,
    pub transient_render_passes: u64,
    pub transient_framebuffers: u64,
    pub descriptor_pools: u64,
}

/// Snapshot the submit-path counters (relaxed loads).
pub fn submit_snapshot() -> SubmitSnapshot {
    SubmitSnapshot {
        guest_submit_calls: SUBMIT.guest_submit_calls.load(Ordering::Relaxed),
        guest_submit_ns: SUBMIT.guest_submit_ns.load(Ordering::Relaxed),
        guest_flip_ns: SUBMIT.guest_flip_ns.load(Ordering::Relaxed),
        backend_ns: SUBMIT.backend_ns.load(Ordering::Relaxed),
        walk_ns: SUBMIT.walk_ns.load(Ordering::Relaxed),
        record_passes_ns: SUBMIT.record_passes_ns.load(Ordering::Relaxed),
        record_ns: SUBMIT.record_ns.load(Ordering::Relaxed),
        transient_create_ns: SUBMIT.transient_create_ns.load(Ordering::Relaxed),
        transient_destroy_ns: SUBMIT.transient_destroy_ns.load(Ordering::Relaxed),
        queue_submit_ns: SUBMIT.queue_submit_ns.load(Ordering::Relaxed),
        draw_fence_ns: SUBMIT.draw_fence_ns.load(Ordering::Relaxed),
        readback_ns: SUBMIT.readback_ns.load(Ordering::Relaxed),
        passes: SUBMIT.passes.load(Ordering::Relaxed),
        draws: SUBMIT.draws.load(Ordering::Relaxed),
        transient_render_passes: SUBMIT.transient_render_passes.load(Ordering::Relaxed),
        transient_framebuffers: SUBMIT.transient_framebuffers.load(Ordering::Relaxed),
        descriptor_pools: SUBMIT.descriptor_pools.load(Ordering::Relaxed),
    }
}
