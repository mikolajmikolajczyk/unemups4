//! Env-gated PM4 submit-execution counters for the aggregate profiler.
//!
//! Mirrors `ps4_gpu::present_profile` exactly — a `OnceLock<bool>` resolved once from
//! `UNEMUPS4_PROFILE`, then a cached branch. This crate depends on neither `ps4-cpu` nor
//! `ps4-gpu` (and must not: it is the Vulkan-free command processor, decision-4), so it
//! duplicates the tiny env read rather than crossing a crate boundary — the env var is the
//! shared contract.
//!
//! These counters cover the GUEST-THREAD half of a flip syscall: decoding the submitted
//! DCB/CCB into packets and walking them. The display-thread half is
//! `ps4_gpu::present_profile`. Together they account for the whole
//! `sceGnmSubmitAndFlipCommandBuffers` call.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

/// Environment variable enabling the profiler (shared with `ps4_cpu::profile`).
const PROFILE_ENV: &str = "UNEMUPS4_PROFILE";

/// Whether submit-execution timing is enabled. Resolved once from [`PROFILE_ENV`].
#[inline]
pub fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| match std::env::var(PROFILE_ENV).ok().as_deref() {
        None | Some("") | Some("0") => false,
        Some(v) => v.parse::<u64>().map(|n| n > 0).unwrap_or(false),
    })
}

/// Cumulative submit-execution counters, all relaxed `AtomicU64`.
pub struct ExecStats {
    /// The `sceGnmSubmit*` handler body, end to end (`ps4-libs`'s `record_submit`) —
    /// the outermost thing the flip syscall's own per-call average can be compared to.
    pub submit_ns: AtomicU64,
    /// Blocking on the process-wide driver mutex inside that handler.
    pub lock_ns: AtomicU64,
    /// Draining the guest-CPU dirty set and applying it to the shader + resource caches,
    /// once per submit before the packets are walked.
    pub dirty_ns: AtomicU64,
    /// `Executor::run` calls (one per submitted DCB, so several per flip syscall).
    pub runs: AtomicU64,
    /// `Executor::run`, end to end — decode + packet walk + the blocking sink calls.
    pub run_ns: AtomicU64,
    /// `decode_submit_range`: turning the raw DCB/CCB bytes into a packet vector.
    pub decode_ns: AtomicU64,
    /// Freeing that packet vector (one heap block per packet).
    pub packet_free_ns: AtomicU64,
    /// Packets that decode produced.
    pub packets: AtomicU64,
}

impl ExecStats {
    const fn new() -> ExecStats {
        ExecStats {
            submit_ns: AtomicU64::new(0),
            lock_ns: AtomicU64::new(0),
            dirty_ns: AtomicU64::new(0),
            runs: AtomicU64::new(0),
            run_ns: AtomicU64::new(0),
            decode_ns: AtomicU64::new(0),
            packet_free_ns: AtomicU64::new(0),
            packets: AtomicU64::new(0),
        }
    }
}

/// The single process-wide submit-execution counters.
pub static EXEC: ExecStats = ExecStats::new();

/// A consistent read of the submit-execution counters for one dump.
#[derive(Clone, Copy, Debug, Default)]
pub struct ExecSnapshot {
    pub submit_ns: u64,
    pub lock_ns: u64,
    pub dirty_ns: u64,
    pub runs: u64,
    pub run_ns: u64,
    pub decode_ns: u64,
    pub packet_free_ns: u64,
    pub packets: u64,
}

/// Snapshot the submit-execution counters (relaxed loads).
pub fn snapshot() -> ExecSnapshot {
    ExecSnapshot {
        submit_ns: EXEC.submit_ns.load(Ordering::Relaxed),
        lock_ns: EXEC.lock_ns.load(Ordering::Relaxed),
        dirty_ns: EXEC.dirty_ns.load(Ordering::Relaxed),
        runs: EXEC.runs.load(Ordering::Relaxed),
        run_ns: EXEC.run_ns.load(Ordering::Relaxed),
        decode_ns: EXEC.decode_ns.load(Ordering::Relaxed),
        packet_free_ns: EXEC.packet_free_ns.load(Ordering::Relaxed),
        packets: EXEC.packets.load(Ordering::Relaxed),
    }
}

/// Resource kinds the cache keys on, indexed by [`kind_index`]. `rt` is a render target.
pub const RES_KINDS: [&str; 5] = ["vertex", "index", "const", "texture", "rt"];

/// The [`RES_KINDS`] slot a cache key's layout occupies.
#[inline]
pub fn kind_index(layout: &crate::cache::ResLayout) -> usize {
    use crate::cache::ResLayout as L;
    match layout {
        L::VertexBuf => 0,
        L::IndexBuf => 1,
        L::ConstBuf => 2,
        L::Texture { .. } => 3,
        L::RenderTarget { .. } => 4,
    }
}

/// Per-kind resource-cache hit/miss counters, all relaxed `AtomicU64` (task-223).
///
/// A `CreateBuffer` costs far more than any other command in the display-thread walk, so
/// what matters is not only how many the cache emits but **why it could not hit**. The
/// three miss classes below are mutually exclusive and answer that directly: a miss at a
/// base never seen means the guest moved the data (a ring buffer), a miss at a known base
/// with a new size means only the extent changed, and a miss on an exact key seen before
/// means the entry was evicted and is being rebuilt. `sub_range` overlays those: the new
/// range lies inside a still-live entry of the same kind, i.e. the bytes are already on the
/// GPU and only an offset was needed.
pub struct CacheStats {
    pub gets: [AtomicU64; RES_KINDS.len()],
    pub clean_hits: [AtomicU64; RES_KINDS.len()],
    pub dirty_hits: [AtomicU64; RES_KINDS.len()],
    pub creates: [AtomicU64; RES_KINDS.len()],
    /// Create at a base address never requested under this kind.
    pub miss_new_base: [AtomicU64; RES_KINDS.len()],
    /// Create at a known base with a size never requested there.
    pub miss_new_size: [AtomicU64; RES_KINDS.len()],
    /// Create for an exact key requested before — the entry was evicted since.
    pub miss_recreate: [AtomicU64; RES_KINDS.len()],
    /// Of those creates, the ones whose range lies wholly inside a live entry's range.
    pub miss_sub_range: [AtomicU64; RES_KINDS.len()],
    /// Bytes the creates asked for.
    pub create_bytes: [AtomicU64; RES_KINDS.len()],
    /// Live cache entries (gauge, written on every insert/evict).
    pub live_entries: AtomicU64,
    /// Distinct `(base, kind)` pairs the cache has ever been asked for (gauge).
    pub distinct_bases: AtomicU64,
}

impl CacheStats {
    const fn new() -> CacheStats {
        #[allow(clippy::declare_interior_mutable_const)]
        const Z: AtomicU64 = AtomicU64::new(0);
        CacheStats {
            gets: [Z; RES_KINDS.len()],
            clean_hits: [Z; RES_KINDS.len()],
            dirty_hits: [Z; RES_KINDS.len()],
            creates: [Z; RES_KINDS.len()],
            miss_new_base: [Z; RES_KINDS.len()],
            miss_new_size: [Z; RES_KINDS.len()],
            miss_recreate: [Z; RES_KINDS.len()],
            miss_sub_range: [Z; RES_KINDS.len()],
            create_bytes: [Z; RES_KINDS.len()],
            live_entries: AtomicU64::new(0),
            distinct_bases: AtomicU64::new(0),
        }
    }
}

/// The single process-wide resource-cache counters.
pub static CACHE: CacheStats = CacheStats::new();

/// A consistent read of the resource-cache counters for one dump.
#[derive(Clone, Copy, Debug)]
pub struct CacheSnapshot {
    pub gets: [u64; RES_KINDS.len()],
    pub clean_hits: [u64; RES_KINDS.len()],
    pub dirty_hits: [u64; RES_KINDS.len()],
    pub creates: [u64; RES_KINDS.len()],
    pub miss_new_base: [u64; RES_KINDS.len()],
    pub miss_new_size: [u64; RES_KINDS.len()],
    pub miss_recreate: [u64; RES_KINDS.len()],
    pub miss_sub_range: [u64; RES_KINDS.len()],
    pub create_bytes: [u64; RES_KINDS.len()],
    pub live_entries: u64,
    pub distinct_bases: u64,
}

impl Default for CacheSnapshot {
    fn default() -> CacheSnapshot {
        CacheSnapshot {
            gets: [0; RES_KINDS.len()],
            clean_hits: [0; RES_KINDS.len()],
            dirty_hits: [0; RES_KINDS.len()],
            creates: [0; RES_KINDS.len()],
            miss_new_base: [0; RES_KINDS.len()],
            miss_new_size: [0; RES_KINDS.len()],
            miss_recreate: [0; RES_KINDS.len()],
            miss_sub_range: [0; RES_KINDS.len()],
            create_bytes: [0; RES_KINDS.len()],
            live_entries: 0,
            distinct_bases: 0,
        }
    }
}

/// Snapshot the resource-cache counters (relaxed loads).
pub fn cache_snapshot() -> CacheSnapshot {
    let mut s = CacheSnapshot::default();
    for i in 0..RES_KINDS.len() {
        s.gets[i] = CACHE.gets[i].load(Ordering::Relaxed);
        s.clean_hits[i] = CACHE.clean_hits[i].load(Ordering::Relaxed);
        s.dirty_hits[i] = CACHE.dirty_hits[i].load(Ordering::Relaxed);
        s.creates[i] = CACHE.creates[i].load(Ordering::Relaxed);
        s.miss_new_base[i] = CACHE.miss_new_base[i].load(Ordering::Relaxed);
        s.miss_new_size[i] = CACHE.miss_new_size[i].load(Ordering::Relaxed);
        s.miss_recreate[i] = CACHE.miss_recreate[i].load(Ordering::Relaxed);
        s.miss_sub_range[i] = CACHE.miss_sub_range[i].load(Ordering::Relaxed);
        s.create_bytes[i] = CACHE.create_bytes[i].load(Ordering::Relaxed);
    }
    s.live_entries = CACHE.live_entries.load(Ordering::Relaxed);
    s.distinct_bases = CACHE.distinct_bases.load(Ordering::Relaxed);
    s
}
