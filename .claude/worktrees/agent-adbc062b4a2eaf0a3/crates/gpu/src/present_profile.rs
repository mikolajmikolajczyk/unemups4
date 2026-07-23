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
        }
    }
}

/// The single process-wide present-path counters.
pub static PRESENT: PresentStats = PresentStats::new();

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
}

/// Snapshot the present-path counters (relaxed loads).
pub fn snapshot() -> PresentSnapshot {
    PresentSnapshot {
        frames: PRESENT.frames.load(Ordering::Relaxed),
        fence_wait_ns: PRESENT.fence_wait_ns.load(Ordering::Relaxed),
        acquire_ns: PRESENT.acquire_ns.load(Ordering::Relaxed),
        fb_copy_ns: PRESENT.fb_copy_ns.load(Ordering::Relaxed),
        record_submit_ns: PRESENT.record_submit_ns.load(Ordering::Relaxed),
        present_ns: PRESENT.present_ns.load(Ordering::Relaxed),
        pace_sleep_ns: PRESENT.pace_sleep_ns.load(Ordering::Relaxed),
    }
}
