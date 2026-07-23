//! The cross-thread GPU-snapshot REQUEST channel (task-185).
//!
//! Retail GPU debugging in this project used to mean bolting an env-gated probe onto the
//! draw path per investigation. That method failed in task-179: two probe-derived
//! measurements were wrong, and both happened to be wrong in the direction that flattered
//! the hypothesis under test. The replacement is ONE complete dump the maintainer triggers
//! by hand — `F10` for the next complete frame, `F9` for a burst of N.
//!
//! This module is only the *request* half, and it lives in `ps4-core` for one reason: the
//! two ends are in crates that must not know about each other.
//!
//! * The keypress arrives on the **display thread** (`ps4-gpu`, `display.rs`).
//! * The GPU state being dumped lives in the **gnm executor** on the guest submit thread
//!   (`ps4-gnm`), behind the `driver()` lock.
//!
//! ## Why an atomic and nothing else
//!
//! The display thread must NEVER acquire `driver()` (task-43/task-66, restated on
//! [`ps4_gnm::driver::driver`]): the guest thread holds `driver()` across `exec.run(...)`,
//! which blocks on the display channel, so a display-side `driver()` lock is an instant,
//! silent deadlock. A snapshot request therefore may not reach into GPU state at all — it
//! may only *deposit an intent*. A single [`AtomicU32`] is exactly that: the display thread
//! does one `fetch_update`, the submit thread does one decrement at a frame boundary, and
//! neither ever waits on the other. Nothing here locks, allocates, or blocks.
//!
//! The counter is a **frame budget**, not a flag, so `F9`'s N-frame burst is the same
//! mechanism as `F10`'s single frame (`request(1)`), and two presses simply add up.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

/// Frames still owed to the maintainer: incremented by the display thread on a keypress,
/// decremented by the submit thread once per frame boundary it captures.
static PENDING_FRAMES: AtomicU32 = AtomicU32::new(0);

/// Ceiling on the outstanding request count. A held-down key must not be able to wrap the
/// counter (or queue a dump so long it looks like a hang), and no real investigation wants
/// more than a few hundred frames of state on disk.
const MAX_PENDING: u32 = 512;

/// Env var naming how many frames `F9` captures. See [`burst_frames`].
pub const BURST_FRAMES_ENV: &str = "UNEMUPS4_SNAPSHOT_FRAMES";

/// Env var naming the directory captures are written under. See [`dump_root`].
pub const DIR_ENV: &str = "UNEMUPS4_SNAPSHOT_DIR";

/// Frames `F9` captures when [`BURST_FRAMES_ENV`] is unset or unparseable. Small enough
/// that a burst finishes in well under a second of guest time (so the captured frames are
/// all from the moment the maintainer reacted to), large enough to show a producer/consumer
/// chain settling across frames.
pub const DEFAULT_BURST_FRAMES: u32 = 8;

/// Directory captures are written under when [`DIR_ENV`] is unset. Relative to the process
/// CWD (the repo root in the normal dev loop), and gitignored there.
pub const DEFAULT_DIR: &str = "gpu-snapshots";

/// Env var opting IN to sampled-texture dumping. See [`textures_enabled`].
pub const TEXTURES_ENV: &str = "UNEMUPS4_SNAPSHOT_TEXTURES";

/// Env var overriding the per-texture size cap, in bytes. See [`texture_max_bytes`].
pub const TEXTURE_MAX_BYTES_ENV: &str = "UNEMUPS4_SNAPSHOT_TEX_MAX_BYTES";

/// Per-texture size cap when [`TEXTURE_MAX_BYTES_ENV`] is unset: 16 MiB, one 2048×2048 RGBA
/// surface. Chosen to admit the atlases a 2D title actually samples while keeping a single
/// oversized descriptor from turning one keypress into a gigabyte of disk. A texture over the
/// cap is recorded with `dumped: false` and its reason — never silently omitted.
pub const DEFAULT_TEXTURE_MAX_BYTES: u64 = 16 * 1024 * 1024;

/// Env var opting IN to render-target PNG dumping. See [`render_targets_enabled`].
pub const RENDER_TARGETS_ENV: &str = "UNEMUPS4_SNAPSHOT_RENDER_TARGETS";

/// Whether render-target PNG dumping is on. OFF by default.
///
/// # What it costs, stated precisely (task-187)
///
/// Dumping a render target copies the HOST image to a staging buffer behind its own fence,
/// so the display thread waits for the GPU to finish before the copy returns. That perturbs
/// frame **TIMING** — a captured frame takes longer, and anything the guest infers from
/// wall-clock (a frame-pacing heuristic, an animation delta) sees a slower frame.
///
/// It does NOT perturb frame **CONTENT**. The copy happens after the frame's passes are
/// recorded and its fence waited, reads the image, and restores its layout; no draw, no
/// binding, no register and no guest byte is changed by it. The pixels a dumped frame puts
/// on screen are the pixels an undumped frame would have. task-185 AC #5 is about content,
/// and this lever does not violate it — but a reader who conflates the two would think it
/// does, which is why the distinction is spelled out here and repeated in `summary.txt`.
///
/// Read per capture rather than cached, matching [`textures_enabled`].
pub fn render_targets_enabled() -> bool {
    std::env::var(RENDER_TARGETS_ENV).is_ok_and(|v| v != "0" && !v.is_empty())
}

/// Whether sampled-texture dumping is on. OFF by default, because it is the only part of a
/// capture whose cost is measured in tens of milliseconds and whose output is measured in
/// megabytes: the common `F10` must stay fast and small enough to press on reflex.
///
/// Read per capture rather than cached, matching [`burst_frames`] — a frame boundary is not a
/// hot path, and a value baked in at startup would surprise a maintainer who exported the var
/// mid-session.
pub fn textures_enabled() -> bool {
    std::env::var(TEXTURES_ENV).is_ok_and(|v| v != "0" && !v.is_empty())
}

/// The per-texture size cap in bytes: [`TEXTURE_MAX_BYTES_ENV`] if it parses, else
/// [`DEFAULT_TEXTURE_MAX_BYTES`]. `0` is accepted and means "cap everything out", which is a
/// legitimate way to see which textures a frame samples without paying for their bytes.
pub fn texture_max_bytes() -> u64 {
    std::env::var(TEXTURE_MAX_BYTES_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_TEXTURE_MAX_BYTES)
}

/// Ask the GPU submit thread to capture the next `frames` complete frames.
///
/// Called from the display thread's key handler. Saturates at [`MAX_PENDING`] rather than
/// wrapping. `frames == 0` is a no-op. This is the ONLY function the display side calls,
/// and it deliberately cannot observe or touch GPU state — see the module docs for why
/// that restriction is load-bearing rather than stylistic.
pub fn request(frames: u32) {
    if frames == 0 {
        return;
    }
    // `fetch_update` rather than `fetch_add` so the saturation is atomic too: two rapid
    // presses can never race past the ceiling.
    let _ = PENDING_FRAMES.fetch_update(Ordering::AcqRel, Ordering::Relaxed, |cur| {
        Some(cur.saturating_add(frames).min(MAX_PENDING))
    });
}

/// Claim one pending frame, returning whether the caller should capture.
///
/// Called by the gnm executor at a frame boundary, and by nobody else. The claim is a
/// compare-exchange loop rather than an unconditional `fetch_sub` so the counter can never
/// go negative if this is ever called from more than one submit context.
pub fn take_frame() -> bool {
    let mut cur = PENDING_FRAMES.load(Ordering::Relaxed);
    loop {
        if cur == 0 {
            return false;
        }
        match PENDING_FRAMES.compare_exchange_weak(
            cur,
            cur - 1,
            Ordering::AcqRel,
            Ordering::Relaxed,
        ) {
            Ok(_) => return true,
            Err(observed) => cur = observed,
        }
    }
}

/// Frames still owed. Introspection / tests only — the capture path uses [`take_frame`].
pub fn pending() -> u32 {
    PENDING_FRAMES.load(Ordering::Relaxed)
}

/// Drop every outstanding request. Test-only reset so one process can exercise the state
/// machine repeatedly without leaking a budget into the next case.
#[cfg(any(test, feature = "test-hooks"))]
pub fn clear() {
    PENDING_FRAMES.store(0, Ordering::Relaxed);
}

/// How many frames one `F9` press captures: [`BURST_FRAMES_ENV`] if it parses to a non-zero
/// count, else [`DEFAULT_BURST_FRAMES`]. Read per press (not cached) so the maintainer can
/// not be surprised by a value baked in at some earlier point in the run; a keypress is not
/// a hot path.
pub fn burst_frames() -> u32 {
    std::env::var(BURST_FRAMES_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .filter(|&n| n != 0)
        .unwrap_or(DEFAULT_BURST_FRAMES)
        .min(MAX_PENDING)
}

/// Directory captures are written under: [`DIR_ENV`] if set, else [`DEFAULT_DIR`].
pub fn dump_root() -> PathBuf {
    std::env::var_os(DIR_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_DIR))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The counter is process-global, so the state-machine cases must not interleave.
    /// One test drives the whole machine rather than several racing on the same static.
    #[test]
    fn request_and_take_frame_state_machine() {
        clear();

        // Idle: nothing pending, so a frame boundary claims nothing. This is the
        // zero-cost-when-idle path — one relaxed load, no capture.
        assert_eq!(pending(), 0);
        assert!(!take_frame());

        // F10: exactly one frame, then idle again.
        request(1);
        assert_eq!(pending(), 1);
        assert!(take_frame());
        assert_eq!(pending(), 0);
        assert!(!take_frame());

        // F9: a burst is N single-frame claims, one per frame boundary.
        request(3);
        assert!(take_frame());
        assert!(take_frame());
        assert!(take_frame());
        assert!(!take_frame());

        // Presses accumulate rather than overwrite.
        request(2);
        request(2);
        assert_eq!(pending(), 4);

        // ...but they saturate: a held key cannot wrap the counter or queue forever.
        request(u32::MAX);
        assert_eq!(pending(), MAX_PENDING);

        // A zero request is a no-op, not a clear.
        request(0);
        assert_eq!(pending(), MAX_PENDING);

        clear();
        assert_eq!(pending(), 0);
    }

    #[test]
    fn burst_frames_defaults_when_env_absent_or_bad() {
        // The default applies unless the env var parses to a non-zero count. Asserting on
        // the *default* (not by mutating the process env, which would race other tests)
        // keeps this honest about the only branch that has no env dependency.
        assert_eq!(DEFAULT_BURST_FRAMES, 8);
        assert!(
            burst_frames() > 0,
            "a burst must capture at least one frame"
        );
        assert!(burst_frames() <= MAX_PENDING);
    }
}
