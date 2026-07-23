//! Env-gated aggregate profiler.
//!
//! A headless-friendly, quantitative time split — guest execution vs HLE syscall
//! dispatch — plus per-syscall totals and the run-loop exit histogram. It needs no
//! `perf` privileges and no Vulkan; it complements the `perf`/flamegraph host-symbol
//! view with numbers a driverless CI box can print.
//!
//! **Gate.** Everything is behind [`enabled`], a `OnceLock<bool>` resolved once from
//! [`PROFILE_ENV`] (`UNEMUPS4_PROFILE`), house-style like `UNEMUPS4_WATCHDOG` /
//! `UNEMUPS4_BACKEND` — no CLI flag. When the var is unset the whole subsystem is a
//! single cached branch on the run loop's hot path; the atomics are never touched and
//! the per-syscall map is never locked. `=1` enables with the default 10 s dump
//! interval; `=<secs>` enables and sets that interval.
//!
//! **Cost when on.** `drive()` returns roughly once per guest syscall (unbounded budget
//! by default), so two `Instant::now()` + a couple of relaxed `fetch_add`s per slice is
//! negligible against the syscall it brackets. High-frequency counters are relaxed
//! `AtomicU64` — never `tracing` spans (that split lives elsewhere). The per-syscall map is a
//! `Mutex<HashMap>` taken only on the (comparatively rare) syscall-exit path.

use std::cell::Cell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Environment variable enabling the aggregate profiler. Unset / empty → disabled
/// (zero overhead). `=1` → enabled, default [`DEFAULT_INTERVAL_SECS`] dump interval.
/// `=<secs>` (a positive integer) → enabled with that periodic-dump interval.
pub const PROFILE_ENV: &str = "UNEMUPS4_PROFILE";

/// Default periodic-dump interval when `UNEMUPS4_PROFILE=1` (seconds).
pub const DEFAULT_INTERVAL_SECS: u64 = 10;

/// Resolved profiler state from [`PROFILE_ENV`]. `None` (unset / empty / `0` /
/// unparseable) disables the profiler entirely.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ProfileConfig {
    interval: Duration,
}

fn resolve_config() -> Option<ProfileConfig> {
    match std::env::var(PROFILE_ENV).ok().as_deref() {
        None | Some("") => None,
        Some("1") => Some(ProfileConfig {
            interval: Duration::from_secs(DEFAULT_INTERVAL_SECS),
        }),
        Some(v) => match v.parse::<u64>() {
            Ok(secs) if secs > 0 => Some(ProfileConfig {
                interval: Duration::from_secs(secs),
            }),
            _ => {
                tracing::warn!(
                    "{PROFILE_ENV}={v:?} is not `1` or a positive integer; profiler disabled"
                );
                None
            }
        },
    }
}

fn config() -> Option<ProfileConfig> {
    static CONFIG: OnceLock<Option<ProfileConfig>> = OnceLock::new();
    *CONFIG.get_or_init(resolve_config)
}

/// Whether the aggregate profiler is enabled. Resolved once, then a cached load — the
/// only cost the default (disabled) path pays on the run loop's hot exit is this branch.
#[inline]
pub fn enabled() -> bool {
    config().is_some()
}

/// The periodic-dump interval (only meaningful when [`enabled`]). Defaults to
/// [`DEFAULT_INTERVAL_SECS`] for `=1`.
pub fn dump_interval() -> Duration {
    config()
        .map(|c| c.interval)
        .unwrap_or(Duration::from_secs(DEFAULT_INTERVAL_SECS))
}

/// Process-wide execution counters, all relaxed `AtomicU64`. Written only from the
/// run loop (`drive`) and only when [`enabled`]; read by the dump thread.
pub struct ExecStats {
    /// Nanoseconds spent inside `cpu.run(...)` (guest execution, incl. JIT dispatch).
    pub guest_ns: AtomicU64,
    /// Count of `cpu.run(...)` calls (run-loop slices).
    pub run_slices: AtomicU64,
    /// Nanoseconds spent inside the HLE syscall `dispatch(...)`.
    pub syscall_ns: AtomicU64,
    /// Count of syscalls dispatched.
    pub syscall_count: AtomicU64,
    /// Nanoseconds between `Vcpu::run` returning `Exit::Syscall` and `dispatch` being
    /// entered: GPR marshalling, the exec-context refresh and the HLE breadcrumb.
    pub pre_dispatch_ns: AtomicU64,
    /// Nanoseconds between `dispatch` returning and the next `Vcpu::run` call: the
    /// return-value write-back, the breadcrumb patch and the thread-exit check. Together
    /// with [`ExecStats::pre_dispatch_ns`] this closes the run loop's own budget, so a
    /// frame's wall time has no unattributed remainder.
    pub post_dispatch_ns: AtomicU64,
    /// Run-loop exits by kind.
    pub exits_budget: AtomicU64,
    pub exits_hlt: AtomicU64,
    pub exits_fatal: AtomicU64,
    /// Sum of `Vcpu::fast_hits()` — the JIT's fast indirect-branch resolutions (IBTC).
    ///
    /// Folded in as a DELTA, never as a re-add of the running per-vcpu value, from two
    /// places: at each frame boundary on the flipping thread, and once more when a guest
    /// call returns for whatever accrued since the last fold. It has to be both, because
    /// the main guest thread runs the entire title inside a single `run_guest_call` and
    /// would otherwise never contribute — which is the task-218 bug that left this counter
    /// frozen to the unit across windows while the emulator was plainly executing.
    ///
    /// `Vcpu::fast_hits()` is per-vcpu and not atomic in x86jit, so every read must happen
    /// on the vcpu's own thread.
    pub vcpu_fast_hits: AtomicU64,
    /// Sum of `Vcpu::retired_instructions()` — guest x86 instructions actually executed.
    ///
    /// Every other performance figure here is time. Without this one we can say a gameplay
    /// frame spends ~25 ms executing guest code, but not whether that is a normal number of
    /// instructions run slowly or an abnormal number run at a reasonable rate — and those
    /// point at completely different fixes (task-220). It also gives the average compiled
    /// unit length, as `retired / chained`, which is what says whether superblock formation
    /// has anything to work with.
    ///
    /// Folded exactly like [`Self::vcpu_fast_hits`]: deltas, from the owning thread, never
    /// a re-add of the running total.
    pub vcpu_retired: AtomicU64,
    /// Sum of `Vcpu::executed_instructions()` — guest x86 instructions executed in
    /// COMPILED code as well as interpreted, which is what [`Self::vcpu_retired`] could
    /// never see (x86jit task-281). Enabled only when this profiler is on, via
    /// `JitBackend::enable_icount` before the first compile.
    ///
    /// This is the axis every other counter here lacks: they all measure time. With it,
    /// `executed / chained` gives the average length of a compiled unit, and
    /// `executed / guest_ns` gives guest MIPS — the figure that turns "we are far from a
    /// 1.6 GHz Jaguar holding 60 fps" from an inference about wall clock into a measurement.
    ///
    /// Same fold discipline as the two above: deltas, from the owning thread.
    pub vcpu_executed: AtomicU64,
}

impl ExecStats {
    const fn new() -> ExecStats {
        ExecStats {
            guest_ns: AtomicU64::new(0),
            run_slices: AtomicU64::new(0),
            syscall_ns: AtomicU64::new(0),
            syscall_count: AtomicU64::new(0),
            pre_dispatch_ns: AtomicU64::new(0),
            post_dispatch_ns: AtomicU64::new(0),
            exits_budget: AtomicU64::new(0),
            exits_hlt: AtomicU64::new(0),
            exits_fatal: AtomicU64::new(0),
            vcpu_fast_hits: AtomicU64::new(0),
            vcpu_retired: AtomicU64::new(0),
            vcpu_executed: AtomicU64::new(0),
        }
    }
}

/// The single process-wide [`ExecStats`] instance.
pub static EXEC: ExecStats = ExecStats::new();

/// Per-syscall time + count. Keyed by raw syscall id (named at dump time via
/// `ps4_syscalls::SyscallId::as_str`, kept out of this crate to avoid a dep).
#[derive(Clone, Copy, Default)]
pub struct IdStat {
    pub count: u64,
    pub ns: u64,
}

/// Per-syscall totals. A `Mutex<HashMap>` locked only on the syscall-exit path (never
/// on the guest-exec hot path), and only when the profiler is enabled.
pub static PER_SYSCALL: Mutex<Option<HashMap<u64, IdStat>>> = Mutex::new(None);

/// Record a dispatched syscall's id and elapsed nanoseconds into [`PER_SYSCALL`].
/// Caller must have checked [`enabled`]; a poisoned lock is silently skipped (the
/// profiler is observability-only and must never abort a run).
#[inline]
pub fn record_syscall(id: u64, ns: u64) {
    if let Ok(mut guard) = PER_SYSCALL.lock() {
        let map = guard.get_or_insert_with(HashMap::new);
        let e = map.entry(id).or_default();
        e.count += 1;
        e.ns += ns;
    }
}

/// Syscalls currently IN FLIGHT: guest tid -> (syscall id, when it was entered).
///
/// The per-syscall table above records calls that RETURNED, so a thread parked inside a
/// blocking call never appears in it — which reads as "that thread makes no syscalls" and is
/// the opposite of the truth. This is the other half: at any moment it names, per thread,
/// the call that has not come back yet, which is what a stalled title needs answered
/// (task-113.2).
pub static IN_FLIGHT: Mutex<Option<HashMap<u32, (u64, std::time::Instant)>>> = Mutex::new(None);

/// Mark `id` as entered on the calling guest thread.
#[inline]
pub fn syscall_enter(tid: u32, id: u64) {
    if let Ok(mut guard) = IN_FLIGHT.lock() {
        guard
            .get_or_insert_with(HashMap::new)
            .insert(tid, (id, std::time::Instant::now()));
    }
}

/// Mark the calling guest thread as no longer inside a syscall.
#[inline]
pub fn syscall_exit(tid: u32) {
    if let Ok(mut guard) = IN_FLIGHT.lock()
        && let Some(map) = guard.as_mut()
    {
        map.remove(&tid);
    }
}

/// Every syscall still in flight, as `(tid, id, how long it has been blocked)`.
pub fn in_flight_syscalls() -> Vec<(u32, u64, std::time::Duration)> {
    let Ok(guard) = IN_FLIGHT.lock() else {
        return Vec::new();
    };
    let Some(map) = guard.as_ref() else {
        return Vec::new();
    };
    let mut out: Vec<_> = map
        .iter()
        .map(|(&tid, &(id, since))| (tid, id, since.elapsed()))
        .collect();
    // Longest wait first — the thread that has been stuck longest is the one to look at.
    out.sort_unstable_by_key(|&(_, _, waited)| std::cmp::Reverse(waited));
    out
}

/// A consistent read of the execution counters for one dump.
#[derive(Clone, Copy, Debug, Default)]
pub struct ExecSnapshot {
    pub guest_ns: u64,
    pub run_slices: u64,
    pub syscall_ns: u64,
    pub syscall_count: u64,
    pub pre_dispatch_ns: u64,
    pub post_dispatch_ns: u64,
    pub exits_budget: u64,
    pub exits_hlt: u64,
    pub exits_fatal: u64,
    pub vcpu_fast_hits: u64,
    pub vcpu_retired: u64,
    pub vcpu_executed: u64,
}

/// Snapshot the execution counters (relaxed loads — a dump is a fuzzy point-in-time
/// view, exact ordering across counters is not required).
pub fn snapshot() -> ExecSnapshot {
    ExecSnapshot {
        guest_ns: EXEC.guest_ns.load(Ordering::Relaxed),
        run_slices: EXEC.run_slices.load(Ordering::Relaxed),
        syscall_ns: EXEC.syscall_ns.load(Ordering::Relaxed),
        syscall_count: EXEC.syscall_count.load(Ordering::Relaxed),
        pre_dispatch_ns: EXEC.pre_dispatch_ns.load(Ordering::Relaxed),
        post_dispatch_ns: EXEC.post_dispatch_ns.load(Ordering::Relaxed),
        exits_budget: EXEC.exits_budget.load(Ordering::Relaxed),
        exits_hlt: EXEC.exits_hlt.load(Ordering::Relaxed),
        exits_fatal: EXEC.exits_fatal.load(Ordering::Relaxed),
        vcpu_fast_hits: EXEC.vcpu_fast_hits.load(Ordering::Relaxed),
        vcpu_retired: EXEC.vcpu_retired.load(Ordering::Relaxed),
        vcpu_executed: EXEC.vcpu_executed.load(Ordering::Relaxed),
    }
}

/// Write a previously taken [`ExecSnapshot`] back over the live counters. Used only to
/// undo the boot-time calibration stub's synthetic traffic, so it never enters a dump.
pub fn restore(s: ExecSnapshot) {
    EXEC.guest_ns.store(s.guest_ns, Ordering::Relaxed);
    EXEC.run_slices.store(s.run_slices, Ordering::Relaxed);
    EXEC.syscall_ns.store(s.syscall_ns, Ordering::Relaxed);
    EXEC.syscall_count.store(s.syscall_count, Ordering::Relaxed);
    EXEC.pre_dispatch_ns
        .store(s.pre_dispatch_ns, Ordering::Relaxed);
    EXEC.post_dispatch_ns
        .store(s.post_dispatch_ns, Ordering::Relaxed);
    EXEC.exits_budget.store(s.exits_budget, Ordering::Relaxed);
    EXEC.exits_hlt.store(s.exits_hlt, Ordering::Relaxed);
    EXEC.exits_fatal.store(s.exits_fatal, Ordering::Relaxed);
    EXEC.vcpu_fast_hits
        .store(s.vcpu_fast_hits, Ordering::Relaxed);
    EXEC.vcpu_retired.store(s.vcpu_retired, Ordering::Relaxed);
    EXEC.vcpu_executed.store(s.vcpu_executed, Ordering::Relaxed);
}

/// Snapshot the per-syscall totals as a plain vec of `(id, IdStat)`. Empty if the
/// profiler never recorded anything (or the lock is poisoned).
pub fn per_syscall_snapshot() -> Vec<(u64, IdStat)> {
    match PER_SYSCALL.lock() {
        Ok(guard) => guard
            .as_ref()
            .map(|m| m.iter().map(|(&id, &s)| (id, s)).collect())
            .unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

/// Forget everything recorded for one syscall id. Used to drop the calibration stub's
/// synthetic traffic (see [`calibration`]) so it never appears in a dump.
pub fn forget_syscall(id: u64) {
    if let Ok(mut guard) = PER_SYSCALL.lock()
        && let Some(map) = guard.as_mut()
    {
        map.remove(&id);
    }
}

/// Syscall ids that end a guest frame — the `sceGnmSubmitAndFlip*` pair. Registered from
/// the app (this crate deliberately has no `ps4-syscalls` dependency, same reason the
/// per-syscall names are resolved at dump time). Only consulted when [`enabled`].
static FRAME_BOUNDARY: OnceLock<Vec<u64>> = OnceLock::new();

/// Declare which syscall ids close a guest frame, enabling the per-frame budget.
pub fn set_frame_boundary_syscalls(ids: Vec<u64>) {
    let _ = FRAME_BOUNDARY.set(ids);
}

#[inline]
pub(crate) fn is_frame_boundary(id: u64) -> bool {
    FRAME_BOUNDARY.get().is_some_and(|ids| ids.contains(&id))
}

/// The frame budget of the guest thread that flips, in nanoseconds, accumulated over
/// completed frames.
///
/// The rows are disjoint and, by construction of the run loop's rolling timestamp, they
/// tile the whole interval between two consecutive flip syscalls: every nanosecond a
/// frame takes is inside exactly one of them. `wall_ns` is measured independently (flip
/// return to flip return), so `wall - (guest + flip + other + loop)` is the honest
/// unaccounted remainder rather than a definition.
pub struct FrameStats {
    /// Completed frames (flip-to-flip intervals) on the flipping thread.
    pub frames: AtomicU64,
    /// Wall time between consecutive flip-syscall returns.
    pub wall_ns: AtomicU64,
    /// Guest code execution (`Vcpu::run`) during the frame.
    pub guest_ns: AtomicU64,
    /// The flip syscall handler itself.
    pub flip_ns: AtomicU64,
    /// Every other syscall handler dispatched during the frame.
    pub other_syscall_ns: AtomicU64,
    /// The run loop's own per-syscall bookkeeping (marshalling, breadcrumb, write-back).
    pub loop_ns: AtomicU64,
    /// Syscalls dispatched during the frame (all ids, the flip included).
    pub syscalls: AtomicU64,
    /// Guest x86 instructions executed during the frame, compiled and interpreted
    /// (x86jit `enable_icount`). Accumulated HERE rather than in `ExecStats` so it is
    /// sampled at the same frame boundaries as `frames` and `guest_ns` — deriving
    /// instructions-per-frame or MIPS from a process-wide counter read at dump time
    /// against a per-frame one folded at a boundary produces nonsense (it briefly
    /// reported 0.4 instructions per block transition, which is impossible).
    pub executed: AtomicU64,
    /// Guest thread id of the flipping thread, as `ps4_core::kernel::current_tid` sees it.
    pub tid: AtomicU64,
    /// CPU time this thread actually burned over the frame (`CLOCK_THREAD_CPUTIME_ID`).
    /// Compared against [`FrameStats::wall_ns`] this separates *computing* from
    /// *descheduled*: every nanosecond of wall time that is not CPU time is time the host
    /// took the thread off a core, whether it blocked or was preempted.
    pub cpu_ns: AtomicU64,
    /// The flip handler's share of [`FrameStats::cpu_ns`]. The flip legitimately waits on
    /// the GPU, so subtracting it leaves the guest-execution remainder — the part where
    /// wall above CPU is unexplained.
    pub flip_cpu_ns: AtomicU64,
    /// Voluntary context switches over the frame: the thread gave up its core, i.e. it
    /// blocked (futex/park/IO). Non-zero here is the fingerprint of waiting on a lock.
    pub vcsw: AtomicU64,
    /// Involuntary context switches over the frame: the scheduler preempted a runnable
    /// thread, i.e. the box is oversubscribed rather than the thread blocked.
    pub ivcsw: AtomicU64,
}

impl FrameStats {
    const fn new() -> FrameStats {
        FrameStats {
            frames: AtomicU64::new(0),
            wall_ns: AtomicU64::new(0),
            guest_ns: AtomicU64::new(0),
            flip_ns: AtomicU64::new(0),
            other_syscall_ns: AtomicU64::new(0),
            loop_ns: AtomicU64::new(0),
            syscalls: AtomicU64::new(0),
            executed: AtomicU64::new(0),
            tid: AtomicU64::new(0),
            cpu_ns: AtomicU64::new(0),
            flip_cpu_ns: AtomicU64::new(0),
            vcsw: AtomicU64::new(0),
            ivcsw: AtomicU64::new(0),
        }
    }
}

/// The single process-wide per-frame budget. Written only by the flipping guest thread.
pub static FRAME: FrameStats = FrameStats::new();

/// A consistent read of the per-frame budget for one dump.
#[derive(Clone, Copy, Debug, Default)]
pub struct FrameSnapshot {
    pub frames: u64,
    pub executed: u64,
    pub wall_ns: u64,
    pub guest_ns: u64,
    pub flip_ns: u64,
    pub other_syscall_ns: u64,
    pub loop_ns: u64,
    pub syscalls: u64,
    pub tid: u64,
    pub cpu_ns: u64,
    pub flip_cpu_ns: u64,
    pub vcsw: u64,
    pub ivcsw: u64,
}

/// Add this frame's executed-instruction delta to the per-frame stats. Called from the
/// run loop at the frame boundary, on the vcpu's own thread.
#[inline]
pub fn frame_add_executed(n: u64) {
    FRAME.executed.fetch_add(n, Ordering::Relaxed);
}

/// Snapshot the per-frame budget (relaxed loads).
pub fn frame_snapshot() -> FrameSnapshot {
    FrameSnapshot {
        frames: FRAME.frames.load(Ordering::Relaxed),
        executed: FRAME.executed.load(Ordering::Relaxed),
        wall_ns: FRAME.wall_ns.load(Ordering::Relaxed),
        guest_ns: FRAME.guest_ns.load(Ordering::Relaxed),
        flip_ns: FRAME.flip_ns.load(Ordering::Relaxed),
        other_syscall_ns: FRAME.other_syscall_ns.load(Ordering::Relaxed),
        loop_ns: FRAME.loop_ns.load(Ordering::Relaxed),
        syscalls: FRAME.syscalls.load(Ordering::Relaxed),
        tid: FRAME.tid.load(Ordering::Relaxed),
        cpu_ns: FRAME.cpu_ns.load(Ordering::Relaxed),
        flip_cpu_ns: FRAME.flip_cpu_ns.load(Ordering::Relaxed),
        vcsw: FRAME.vcsw.load(Ordering::Relaxed),
        ivcsw: FRAME.ivcsw.load(Ordering::Relaxed),
    }
}

/// This thread's consumed CPU time in nanoseconds (`CLOCK_THREAD_CPUTIME_ID`).
///
/// The counterpart to the run loop's `Instant::now()`: that measures elapsed wall time,
/// which a descheduled thread accrues just as fast as a running one. Read at most a
/// couple of times per guest frame, so the syscall it costs is irrelevant against a
/// 16–40 ms frame.
#[inline]
fn thread_cpu_ns() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    if unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) } != 0 {
        return 0;
    }
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

/// This thread's `(voluntary, involuntary)` context-switch counts since it started.
/// `RUSAGE_THREAD` is a Linux extension; elsewhere the counters stay zero and the rows
/// that use them read as "not measured".
#[inline]
fn thread_ctxsw() -> (u64, u64) {
    #[cfg(target_os = "linux")]
    {
        let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
        if unsafe { libc::getrusage(libc::RUSAGE_THREAD, &mut ru) } != 0 {
            return (0, 0);
        }
        (ru.ru_nvcsw as u64, ru.ru_nivcsw as u64)
    }
    #[cfg(not(target_os = "linux"))]
    {
        (0, 0)
    }
}

/// What the host scheduler did to the flipping thread over one frame — the half of a
/// frame's cost that no wall-clock phase split can see.
#[derive(Clone, Copy, Default)]
struct Sched {
    cpu_ns: u64,
    flip_cpu_ns: u64,
    vcsw: u64,
    ivcsw: u64,
}

/// Sub-buckets per octave in a [`Dist`], as a bit count. 4 → 16 per octave → a bucket is
/// at most ~6% wide, so a 16.7 ms frame and a 20 ms one can never share one.
const SUB_BITS: usize = 4;
const SUB: usize = 1 << SUB_BITS;

/// Buckets in a [`Dist`] histogram: a log2 scale with [`SUB`] linear sub-buckets per
/// octave. 512 of them reach 17 s, so no frame time can escape off the top.
pub const HIST_BUCKETS: usize = 512;

/// Bucket index for a nanosecond sample. Values below [`SUB`] get their own exact bucket;
/// above that, the [`SUB_BITS`] bits below the leading one select the sub-bucket.
#[inline]
fn bucket_of(ns: u64) -> usize {
    if (ns as usize) < SUB {
        return ns as usize;
    }
    let e = 63 - ns.leading_zeros() as usize;
    let m = ((ns >> (e - SUB_BITS)) as usize) & (SUB - 1);
    ((e - SUB_BITS + 1) * SUB + m).min(HIST_BUCKETS - 1)
}

/// Inclusive lower edge, in nanoseconds, of the bucket at `idx`.
fn bucket_low(idx: usize) -> u64 {
    if idx < SUB {
        return idx as u64;
    }
    let e = idx / SUB + SUB_BITS - 1;
    let m = (idx % SUB) as u64;
    (SUB as u64 + m) << (e - SUB_BITS)
}

/// Best-estimate representative value of a bucket: its midpoint.
fn bucket_mid(idx: usize) -> u64 {
    let lo = bucket_low(idx);
    let hi = bucket_low(idx + 1);
    lo + (hi - lo) / 2
}

/// A lock-free, allocation-free distribution of nanosecond samples: a bounded log
/// histogram plus exact min/max. Written from the frame path with relaxed
/// `fetch_add`/`fetch_min`/`fetch_max`, read only by the dump thread. Windowed count and
/// sum are derived from the per-bucket deltas in [`dist_window`], not carried here.
pub struct Dist {
    buckets: [AtomicU64; HIST_BUCKETS],
    min_ns: AtomicU64,
    max_ns: AtomicU64,
}

impl Dist {
    const fn new() -> Dist {
        Dist {
            buckets: [const { AtomicU64::new(0) }; HIST_BUCKETS],
            min_ns: AtomicU64::new(u64::MAX),
            max_ns: AtomicU64::new(0),
        }
    }

    /// Record one sample. Caller must have checked [`enabled`].
    #[inline]
    pub fn record(&self, ns: u64) {
        self.buckets[bucket_of(ns)].fetch_add(1, Ordering::Relaxed);
        self.min_ns.fetch_min(ns, Ordering::Relaxed);
        self.max_ns.fetch_max(ns, Ordering::Relaxed);
    }
}

/// Frame wall time (flip-return to flip-return) of the flipping thread.
pub static FRAME_DIST: Dist = Dist::new();

/// The flip syscall handler's own time — the dominant per-frame component.
pub static FLIP_DIST: Dist = Dist::new();

/// One window's worth of a [`Dist`]: min/max are exact, the percentiles carry the
/// histogram's bucket resolution.
#[derive(Clone, Copy, Debug, Default)]
pub struct DistWindow {
    pub count: u64,
    pub sum_ns: u64,
    pub min_ns: u64,
    pub max_ns: u64,
    pub p50_ns: u64,
    pub p95_ns: u64,
    pub p99_ns: u64,
}

/// Close one reporting window over `dist`. `prev` holds the previous window's cumulative
/// bucket counts and is updated in place, so the percentiles describe only the frames since
/// the last call. min/max are *taken* (swapped back to their identity), which is why this
/// must be called from a single reader — the dump thread.
pub fn dist_window(dist: &Dist, prev: &mut [u64; HIST_BUCKETS]) -> DistWindow {
    let mut counts = [0u64; HIST_BUCKETS];
    let mut total = 0u64;
    for i in 0..HIST_BUCKETS {
        let cur = dist.buckets[i].load(Ordering::Relaxed);
        counts[i] = cur.saturating_sub(prev[i]);
        prev[i] = cur;
        total += counts[i];
    }
    let min_ns = dist.min_ns.swap(u64::MAX, Ordering::Relaxed);
    let max_ns = dist.max_ns.swap(0, Ordering::Relaxed);
    if total == 0 {
        return DistWindow::default();
    }
    let quantile = |q: f64| -> u64 {
        let want = ((total as f64) * q).ceil().max(1.0) as u64;
        let mut seen = 0u64;
        for (i, &c) in counts.iter().enumerate() {
            seen += c;
            if seen >= want {
                return bucket_mid(i);
            }
        }
        bucket_mid(HIST_BUCKETS - 1)
    };
    DistWindow {
        count: total,
        sum_ns: counts
            .iter()
            .enumerate()
            .map(|(i, &c)| c * bucket_mid(i))
            .sum(),
        min_ns: if min_ns == u64::MAX { 0 } else { min_ns },
        max_ns,
        p50_ns: quantile(0.50),
        p95_ns: quantile(0.95),
        p99_ns: quantile(0.99),
    }
}

/// Multiple of the reference frame time above which a frame counts as slow.
/// `UNEMUPS4_PROFILE_SLOW=1.5` (the default) means "50% over the typical frame".
pub const SLOW_ENV: &str = "UNEMUPS4_PROFILE_SLOW";

/// Pins the reference frame time instead of tracking it. `UNEMUPS4_PROFILE_TARGET_MS=16.7`
/// makes "slow" mean "over 1.5 x 16.7 ms" regardless of what the title actually achieves.
pub const TARGET_MS_ENV: &str = "UNEMUPS4_PROFILE_TARGET_MS";

fn slow_mult_permille() -> u64 {
    static M: OnceLock<u64> = OnceLock::new();
    *M.get_or_init(|| match std::env::var(SLOW_ENV).ok().as_deref() {
        None | Some("") => 1500,
        Some(v) => match v.parse::<f64>() {
            Ok(m) if m > 1.0 => (m * 1000.0) as u64,
            _ => {
                tracing::warn!("{SLOW_ENV}={v:?} is not a multiple > 1; using 1.5");
                1500
            }
        },
    })
}

fn fixed_target_ns() -> Option<u64> {
    static T: OnceLock<Option<u64>> = OnceLock::new();
    *T.get_or_init(|| match std::env::var(TARGET_MS_ENV).ok().as_deref() {
        None | Some("") => None,
        Some(v) => match v.parse::<f64>() {
            Ok(ms) if ms > 0.0 => Some((ms * 1_000_000.0) as u64),
            _ => {
                tracing::warn!("{TARGET_MS_ENV}={v:?} is not a positive number; ignored");
                None
            }
        },
    })
}

/// The reference "typical" frame time in nanoseconds.
///
/// A fixed target is wrong here: a title running at 30 fps would have every one of its
/// frames flagged against a 16.7 ms budget, and the point is to find the frames that are
/// slow *for this title right now*. So the reference tracks the running median with a
/// frugal estimator — one multiplicative step towards each sample — which a 20%-duty hitch
/// tail cannot drag up the way a mean would. Single writer (the flipping thread), so plain
/// relaxed load/store is sufficient.
static REF_NS: AtomicU64 = AtomicU64::new(0);

/// Advance the reference with this frame's wall time and return the value the frame should
/// be judged against (the estimate from *before* the frame, so a frame cannot excuse
/// itself).
#[inline]
fn reference_ns(wall_ns: u64) -> u64 {
    if let Some(fixed) = fixed_target_ns() {
        return fixed;
    }
    let cur = REF_NS.load(Ordering::Relaxed);
    if cur == 0 {
        REF_NS.store(wall_ns, Ordering::Relaxed);
        return wall_ns;
    }
    let step = (cur / 64).max(1);
    let next = if wall_ns > cur {
        cur + step
    } else {
        cur.saturating_sub(step)
    };
    REF_NS.store(next, Ordering::Relaxed);
    cur
}

/// The current reference frame time (0 until the first frame closes).
pub fn reference_frame_ns() -> u64 {
    fixed_target_ns().unwrap_or_else(|| REF_NS.load(Ordering::Relaxed))
}

/// The configured slow-frame threshold as a plain multiple.
pub fn slow_multiple() -> f64 {
    slow_mult_permille() as f64 / 1000.0
}

/// Frames that exceeded [`slow_multiple`] x [`reference_frame_ns`], with the same phase
/// split the whole-window rows carry — so a hitch can be attributed instead of merely
/// counted — plus the statistics that turn "it stutters every so often" into a period.
pub struct SlowStats {
    pub frames: AtomicU64,
    pub wall_ns: AtomicU64,
    pub guest_ns: AtomicU64,
    pub flip_ns: AtomicU64,
    pub other_syscall_ns: AtomicU64,
    pub loop_ns: AtomicU64,
    /// Slow-frame *runs*: a burst of consecutive slow frames counts once.
    pub hitches: AtomicU64,
    /// Frames between consecutive hitch starts.
    pub gap_count: AtomicU64,
    pub gap_sum: AtomicU64,
    pub gap_min: AtomicU64,
    pub gap_max: AtomicU64,
    /// Length, in frames, of the completed bursts.
    pub burst_sum: AtomicU64,
    pub burst_max: AtomicU64,
    /// The scheduler's view of the same frames — see [`FrameStats::cpu_ns`].
    pub cpu_ns: AtomicU64,
    pub flip_cpu_ns: AtomicU64,
    pub vcsw: AtomicU64,
    pub ivcsw: AtomicU64,
}

impl SlowStats {
    const fn new() -> SlowStats {
        SlowStats {
            frames: AtomicU64::new(0),
            wall_ns: AtomicU64::new(0),
            guest_ns: AtomicU64::new(0),
            flip_ns: AtomicU64::new(0),
            other_syscall_ns: AtomicU64::new(0),
            loop_ns: AtomicU64::new(0),
            hitches: AtomicU64::new(0),
            gap_count: AtomicU64::new(0),
            gap_sum: AtomicU64::new(0),
            gap_min: AtomicU64::new(u64::MAX),
            gap_max: AtomicU64::new(0),
            burst_sum: AtomicU64::new(0),
            burst_max: AtomicU64::new(0),
            cpu_ns: AtomicU64::new(0),
            flip_cpu_ns: AtomicU64::new(0),
            vcsw: AtomicU64::new(0),
            ivcsw: AtomicU64::new(0),
        }
    }
}

/// The single process-wide slow-frame accounting.
pub static SLOW: SlowStats = SlowStats::new();

/// A consistent read of [`SLOW`] for one dump.
#[derive(Clone, Copy, Debug, Default)]
pub struct SlowSnapshot {
    pub frames: u64,
    pub wall_ns: u64,
    pub guest_ns: u64,
    pub flip_ns: u64,
    pub other_syscall_ns: u64,
    pub loop_ns: u64,
    pub hitches: u64,
    pub gap_count: u64,
    pub gap_sum: u64,
    pub gap_min: u64,
    pub gap_max: u64,
    pub burst_sum: u64,
    pub burst_max: u64,
    pub cpu_ns: u64,
    pub flip_cpu_ns: u64,
    pub vcsw: u64,
    pub ivcsw: u64,
}

/// Snapshot the slow-frame counters (relaxed loads).
pub fn slow_snapshot() -> SlowSnapshot {
    let gap_min = SLOW.gap_min.load(Ordering::Relaxed);
    SlowSnapshot {
        frames: SLOW.frames.load(Ordering::Relaxed),
        wall_ns: SLOW.wall_ns.load(Ordering::Relaxed),
        guest_ns: SLOW.guest_ns.load(Ordering::Relaxed),
        flip_ns: SLOW.flip_ns.load(Ordering::Relaxed),
        other_syscall_ns: SLOW.other_syscall_ns.load(Ordering::Relaxed),
        loop_ns: SLOW.loop_ns.load(Ordering::Relaxed),
        hitches: SLOW.hitches.load(Ordering::Relaxed),
        gap_count: SLOW.gap_count.load(Ordering::Relaxed),
        gap_sum: SLOW.gap_sum.load(Ordering::Relaxed),
        gap_min: if gap_min == u64::MAX { 0 } else { gap_min },
        gap_max: SLOW.gap_max.load(Ordering::Relaxed),
        burst_sum: SLOW.burst_sum.load(Ordering::Relaxed),
        burst_max: SLOW.burst_max.load(Ordering::Relaxed),
        cpu_ns: SLOW.cpu_ns.load(Ordering::Relaxed),
        flip_cpu_ns: SLOW.flip_cpu_ns.load(Ordering::Relaxed),
        vcsw: SLOW.vcsw.load(Ordering::Relaxed),
        ivcsw: SLOW.ivcsw.load(Ordering::Relaxed),
    }
}

/// Slots in the slow-frame offender table. A fixed table keeps the frame path free of both
/// allocation and the per-syscall mutex; overflow past this many distinct ids is counted
/// separately rather than silently dropped.
const OFFENDER_SLOTS: usize = 16;

/// `id + 1` of each offender (0 = free). Written only by the flipping thread.
static OFFENDER_ID: [AtomicU64; OFFENDER_SLOTS] = [const { AtomicU64::new(0) }; OFFENDER_SLOTS];
static OFFENDER_COUNT: [AtomicU64; OFFENDER_SLOTS] = [const { AtomicU64::new(0) }; OFFENDER_SLOTS];
static OFFENDER_NS: [AtomicU64; OFFENDER_SLOTS] = [const { AtomicU64::new(0) }; OFFENDER_SLOTS];
static OFFENDER_OVERFLOW: AtomicU64 = AtomicU64::new(0);

/// Credit the longest single syscall of a slow frame to the offender table.
#[inline]
fn record_offender(id: u64, ns: u64) {
    let key = id.wrapping_add(1);
    for i in 0..OFFENDER_SLOTS {
        let cur = OFFENDER_ID[i].load(Ordering::Relaxed);
        if cur == 0 {
            OFFENDER_ID[i].store(key, Ordering::Relaxed);
        } else if cur != key {
            continue;
        }
        OFFENDER_COUNT[i].fetch_add(1, Ordering::Relaxed);
        OFFENDER_NS[i].fetch_add(ns, Ordering::Relaxed);
        return;
    }
    OFFENDER_OVERFLOW.fetch_add(1, Ordering::Relaxed);
}

/// The offender table as `(syscall id, slow frames it topped, total ns in those frames)`,
/// plus the count of frames whose offender did not fit the table. Cumulative since start.
pub fn offender_snapshot() -> (Vec<(u64, u64, u64)>, u64) {
    let mut out = Vec::new();
    for i in 0..OFFENDER_SLOTS {
        match OFFENDER_ID[i].load(Ordering::Relaxed) {
            0 => break,
            key => out.push((
                key - 1,
                OFFENDER_COUNT[i].load(Ordering::Relaxed),
                OFFENDER_NS[i].load(Ordering::Relaxed),
            )),
        }
    }
    (out, OFFENDER_OVERFLOW.load(Ordering::Relaxed))
}

/// Enables guest-RIP sampling inside slow frames. `UNEMUPS4_PROFILE_RIP=1` uses
/// [`DEFAULT_RIP_BUDGET`]; `=<blocks>` sets the budget. Opt-in on top of `UNEMUPS4_PROFILE`
/// rather than implied by it, because a block budget makes the run loop leave the vcpu
/// periodically and that would perturb the very frame budget printed beside it.
pub const RIP_ENV: &str = "UNEMUPS4_PROFILE_RIP";

/// Translation blocks between RIP samples when `UNEMUPS4_PROFILE_RIP=1`.
///
/// The budget is per `Vcpu::run` call and a guest that issues ~800 syscalls per frame
/// returns to the run loop every few hundred blocks, so a large budget (the tracer's
/// 200 000, say) never trips and samples nothing at all. A thousand yields a few tens of
/// thousands of samples a second — enough to fill a slow frame — while a sampled slice
/// costs one extra vcpu round trip of ~175 ns.
pub const DEFAULT_RIP_BUDGET: u64 = 1_000;

fn rip_config() -> Option<u64> {
    static C: OnceLock<Option<u64>> = OnceLock::new();
    *C.get_or_init(|| match std::env::var(RIP_ENV).ok().as_deref() {
        None | Some("") | Some("0") => None,
        Some("1") => Some(DEFAULT_RIP_BUDGET),
        Some(v) => match v.parse::<u64>() {
            Ok(blocks) if blocks > 0 => Some(blocks),
            _ => {
                tracing::warn!("{RIP_ENV}={v:?} is not `1` or a positive integer; disabled");
                None
            }
        },
    })
}

/// The block budget the run loop should drive the vcpu with so `Exit::BudgetExhausted`
/// yields a RIP sample, or `None` when slow-frame RIP sampling is off.
#[inline]
pub fn rip_budget() -> Option<u64> {
    enabled().then(rip_config).flatten()
}

/// Whether slow-frame RIP sampling is on.
#[inline]
pub fn rip_sampling() -> bool {
    rip_budget().is_some()
}

/// Samples held for the frame in flight. A frame's samples are only worth keeping once the
/// frame is known to be slow, and that is only known at its flip — so they are buffered
/// here and either committed or dropped there. Fixed size: no allocation on the frame path.
const RIP_RING: usize = 2048;

struct RipRing {
    buf: [u64; RIP_RING],
    len: usize,
    dropped: u64,
}

thread_local! {
    static RIPS: std::cell::RefCell<RipRing> = const {
        std::cell::RefCell::new(RipRing { buf: [0; RIP_RING], len: 0, dropped: 0 })
    };
}

/// Slots in the aggregate slow-frame RIP table. Open-addressed, single writer (the
/// flipping thread), read by the dump thread.
const RIP_SLOTS: usize = 2048;
const RIP_PROBES: usize = 16;

/// `rip + 1` of each sampled address (0 = free), so a legitimate guest RIP of 0 is a
/// distinct key rather than aliasing the empty-slot sentinel — as the offender table
/// (`OFFENDER_ID`) tags its ids.
static RIP_ADDR: [AtomicU64; RIP_SLOTS] = [const { AtomicU64::new(0) }; RIP_SLOTS];
static RIP_HITS: [AtomicU64; RIP_SLOTS] = [const { AtomicU64::new(0) }; RIP_SLOTS];
static RIP_TOTAL: AtomicU64 = AtomicU64::new(0);
static RIP_OVERFLOW: AtomicU64 = AtomicU64::new(0);
static RIP_LOST: AtomicU64 = AtomicU64::new(0);
static RIP_FRAMES: AtomicU64 = AtomicU64::new(0);

/// Buffer one sampled guest RIP for the frame in flight. Caller has checked
/// [`rip_sampling`] and that this is the outermost run loop on this thread.
#[inline]
pub fn frame_record_rip(rip: u64) {
    RIPS.with(|r| {
        let mut r = r.borrow_mut();
        if r.len == RIP_RING {
            r.dropped += 1;
            return;
        }
        let n = r.len;
        r.buf[n] = rip;
        r.len = n + 1;
    });
}

fn rip_insert(rip: u64) {
    // Store the tagged key (`rip + 1`); the hash still keys off the raw `rip` so the slot
    // distribution is unchanged.
    let key = rip.wrapping_add(1);
    let mut idx = (rip.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 32) as usize % RIP_SLOTS;
    for _ in 0..RIP_PROBES {
        match RIP_ADDR[idx].load(Ordering::Relaxed) {
            0 => RIP_ADDR[idx].store(key, Ordering::Relaxed),
            cur if cur != key => {
                idx = (idx + 1) % RIP_SLOTS;
                continue;
            }
            _ => {}
        }
        RIP_HITS[idx].fetch_add(1, Ordering::Relaxed);
        RIP_TOTAL.fetch_add(1, Ordering::Relaxed);
        return;
    }
    RIP_OVERFLOW.fetch_add(1, Ordering::Relaxed);
}

/// Fold the frame in flight's buffered samples into the aggregate (`slow`) or discard them,
/// and reset the buffer either way.
fn rip_close_frame(slow: bool) {
    RIPS.with(|r| {
        let mut r = r.borrow_mut();
        if slow {
            for i in 0..r.len {
                rip_insert(r.buf[i]);
            }
            if r.len > 0 || r.dropped > 0 {
                RIP_FRAMES.fetch_add(1, Ordering::Relaxed);
            }
            RIP_LOST.fetch_add(r.dropped, Ordering::Relaxed);
        }
        r.len = 0;
        r.dropped = 0;
    });
}

/// The aggregate slow-frame RIP histogram, plus `(total samples, slow frames sampled,
/// samples lost to a full ring, samples lost to a full table)`. Cumulative since start.
pub fn rip_snapshot() -> (Vec<(u64, u64)>, u64, u64, u64, u64) {
    let mut out = Vec::new();
    for i in 0..RIP_SLOTS {
        match RIP_ADDR[i].load(Ordering::Relaxed) {
            0 => continue,
            key => out.push((key - 1, RIP_HITS[i].load(Ordering::Relaxed))),
        }
    }
    out.sort_unstable_by_key(|&(_, c)| std::cmp::Reverse(c));
    (
        out,
        RIP_TOTAL.load(Ordering::Relaxed),
        RIP_FRAMES.load(Ordering::Relaxed),
        RIP_LOST.load(Ordering::Relaxed),
        RIP_OVERFLOW.load(Ordering::Relaxed),
    )
}

/// Name a sampled guest address through the fault annotator the app already installs,
/// reduced to the `module!symbol +offset` tail it ends with. `None` when no annotator is
/// installed or it had nothing to say.
pub fn describe_guest_addr(addr: u64) -> Option<String> {
    let ctx = crate::exec::fault_context(addr);
    let line = ctx
        .lines()
        .next()
        .map(str::trim)
        .filter(|s| !s.is_empty())?;
    let tail = line.split_once(" — ").map_or(line, |(_, t)| t);
    Some(tail.split(" (").next().unwrap_or(tail).to_string())
}

/// Recent frame wall times, in microseconds, newest overwriting oldest. 256 entries is
/// ~8 s at 30 fps — long enough for a period of a few dozen frames to be read straight off
/// the row.
const FRAME_RING: usize = 256;
static FRAME_RING_BUF: [AtomicU32; FRAME_RING] = [const { AtomicU32::new(0) }; FRAME_RING];
static FRAME_RING_POS: AtomicU64 = AtomicU64::new(0);

/// The last completed inter-hitch gaps, in frames.
const GAP_RING: usize = 32;
static GAP_RING_BUF: [AtomicU32; GAP_RING] = [const { AtomicU32::new(0) }; GAP_RING];
static GAP_RING_POS: AtomicU64 = AtomicU64::new(0);

fn ring_read(buf: &[AtomicU32], pos: &AtomicU64) -> Vec<u32> {
    let written = pos.load(Ordering::Relaxed);
    let n = (written as usize).min(buf.len());
    let start = written as usize - n;
    (start..written as usize)
        .map(|i| buf[i % buf.len()].load(Ordering::Relaxed))
        .collect()
}

/// Recent frame wall times in microseconds, oldest first.
pub fn frame_ring() -> Vec<u32> {
    ring_read(&FRAME_RING_BUF, &FRAME_RING_POS)
}

/// Recent inter-hitch gaps in frames, oldest first.
pub fn gap_ring() -> Vec<u32> {
    ring_read(&GAP_RING_BUF, &GAP_RING_POS)
}

/// Frame index of the previous slow frame, and of the previous hitch (burst) start.
static LAST_SLOW_IDX: AtomicU64 = AtomicU64::new(0);
static LAST_HITCH_IDX: AtomicU64 = AtomicU64::new(0);
/// Length so far of the burst in flight.
static BURST_LEN: AtomicU64 = AtomicU64::new(0);

/// Record the distribution samples and, if this frame was slow, its attribution. Called
/// once per closed frame from [`frame_add_syscall`], on the flipping thread only.
#[inline]
fn record_frame_distribution(
    index: u64,
    wall_ns: u64,
    acc: &FrameAcc,
    flip_ns: u64,
    sched: Sched,
) -> bool {
    FRAME_DIST.record(wall_ns);
    FLIP_DIST.record(flip_ns);
    let slot = FRAME_RING_POS.fetch_add(1, Ordering::Relaxed) as usize % FRAME_RING;
    FRAME_RING_BUF[slot].store(
        (wall_ns / 1_000).min(u32::MAX as u64) as u32,
        Ordering::Relaxed,
    );

    let reference = reference_ns(wall_ns);
    if reference == 0 || wall_ns * 1000 <= reference.saturating_mul(slow_mult_permille()) {
        return false;
    }

    SLOW.cpu_ns.fetch_add(sched.cpu_ns, Ordering::Relaxed);
    SLOW.flip_cpu_ns
        .fetch_add(sched.flip_cpu_ns, Ordering::Relaxed);
    SLOW.vcsw.fetch_add(sched.vcsw, Ordering::Relaxed);
    SLOW.ivcsw.fetch_add(sched.ivcsw, Ordering::Relaxed);
    SLOW.frames.fetch_add(1, Ordering::Relaxed);
    SLOW.wall_ns.fetch_add(wall_ns, Ordering::Relaxed);
    SLOW.guest_ns.fetch_add(acc.guest_ns, Ordering::Relaxed);
    SLOW.flip_ns.fetch_add(flip_ns, Ordering::Relaxed);
    SLOW.other_syscall_ns
        .fetch_add(acc.other_syscall_ns, Ordering::Relaxed);
    SLOW.loop_ns.fetch_add(acc.loop_ns, Ordering::Relaxed);
    if acc.top_ns > 0 {
        record_offender(acc.top_id, acc.top_ns);
    }

    let prev_slow = LAST_SLOW_IDX.swap(index, Ordering::Relaxed);
    if prev_slow + 1 == index {
        BURST_LEN.fetch_add(1, Ordering::Relaxed);
        return true;
    }
    match BURST_LEN.swap(1, Ordering::Relaxed) {
        0 => {}
        len => {
            SLOW.burst_sum.fetch_add(len, Ordering::Relaxed);
            SLOW.burst_max.fetch_max(len, Ordering::Relaxed);
        }
    }
    SLOW.hitches.fetch_add(1, Ordering::Relaxed);
    let prev_hitch = LAST_HITCH_IDX.swap(index, Ordering::Relaxed);
    if prev_hitch != 0 {
        let gap = index - prev_hitch;
        SLOW.gap_count.fetch_add(1, Ordering::Relaxed);
        SLOW.gap_sum.fetch_add(gap, Ordering::Relaxed);
        SLOW.gap_min.fetch_min(gap, Ordering::Relaxed);
        SLOW.gap_max.fetch_max(gap, Ordering::Relaxed);
        let slot = GAP_RING_POS.fetch_add(1, Ordering::Relaxed) as usize % GAP_RING;
        GAP_RING_BUF[slot].store(gap.min(u32::MAX as u64) as u32, Ordering::Relaxed);
    }
    true
}

/// The in-flight frame of one guest thread. Process-wide sums cannot answer "where does a
/// frame go" — several guest threads run concurrently and their totals exceed wall time —
/// so the split is accumulated per thread and only published when *this* thread flips.
#[derive(Clone, Copy, Default)]
struct FrameAcc {
    open: Option<Instant>,
    guest_ns: u64,
    other_syscall_ns: u64,
    loop_ns: u64,
    syscalls: u64,
    /// Longest single non-flip syscall of the frame — what a slow frame is blamed on.
    top_id: u64,
    top_ns: u64,
    /// Thread CPU time and context-switch counts as the frame opened, and the thread CPU
    /// time as the flip handler was entered — the three readings a frame's wall-vs-CPU
    /// split needs.
    cpu_open_ns: u64,
    vcsw_open: u64,
    ivcsw_open: u64,
    flip_cpu_in_ns: u64,
}

thread_local! {
    static FRAME_ACC: Cell<FrameAcc> = const { Cell::new(FrameAcc {
        open: None,
        guest_ns: 0,
        other_syscall_ns: 0,
        loop_ns: 0,
        syscalls: 0,
        top_id: 0,
        top_ns: 0,
        cpu_open_ns: 0,
        vcsw_open: 0,
        ivcsw_open: 0,
        flip_cpu_in_ns: 0,
    }) };
}

/// Add a guest-execution slice to this thread's in-flight frame.
#[inline]
pub fn frame_add_guest(ns: u64) {
    FRAME_ACC.with(|c| {
        let mut a = c.get();
        a.guest_ns += ns;
        c.set(a);
    });
}

/// Add run-loop bookkeeping (pre/post dispatch) to this thread's in-flight frame.
#[inline]
pub fn frame_add_loop(ns: u64) {
    FRAME_ACC.with(|c| {
        let mut a = c.get();
        a.loop_ns += ns;
        c.set(a);
    });
}

/// Note that the flip handler is about to be entered, so its own CPU time can be
/// separated from the frame's. Called before dispatch; a no-op for every other syscall.
#[inline]
pub fn frame_syscall_enter(id: u64) {
    if !is_frame_boundary(id) {
        return;
    }
    let cpu = thread_cpu_ns();
    FRAME_ACC.with(|c| {
        let mut a = c.get();
        a.flip_cpu_in_ns = cpu;
        c.set(a);
    });
}

/// Account one dispatched syscall. A flip id closes the frame and publishes the split;
/// anything else just accumulates. `ns` is the handler's own time.
#[inline]
pub fn frame_add_syscall(id: u64, ns: u64) {
    let boundary = is_frame_boundary(id);
    if !boundary {
        FRAME_ACC.with(|c| {
            let mut a = c.get();
            a.syscalls += 1;
            a.other_syscall_ns += ns;
            if ns > a.top_ns {
                a.top_ns = ns;
                a.top_id = id;
            }
            c.set(a);
        });
        return;
    }

    let now = Instant::now();
    let cpu_now = thread_cpu_ns();
    let (vcsw_now, ivcsw_now) = thread_ctxsw();
    let sampling = rip_sampling();
    let mut slow = false;
    FRAME_ACC.with(|c| {
        let mut a = c.get();
        a.syscalls += 1;
        if let Some(open) = a.open {
            let wall_ns = now.duration_since(open).as_nanos() as u64;
            let sched = Sched {
                cpu_ns: cpu_now.saturating_sub(a.cpu_open_ns),
                flip_cpu_ns: cpu_now.saturating_sub(a.flip_cpu_in_ns),
                vcsw: vcsw_now.saturating_sub(a.vcsw_open),
                ivcsw: ivcsw_now.saturating_sub(a.ivcsw_open),
            };
            FRAME
                .tid
                .store(ps4_core::kernel::current_tid() as u64, Ordering::Relaxed);
            let index = FRAME.frames.fetch_add(1, Ordering::Relaxed) + 1;
            slow = record_frame_distribution(index, wall_ns, &a, ns, sched);
            FRAME.wall_ns.fetch_add(wall_ns, Ordering::Relaxed);
            FRAME.guest_ns.fetch_add(a.guest_ns, Ordering::Relaxed);
            FRAME.flip_ns.fetch_add(ns, Ordering::Relaxed);
            FRAME
                .other_syscall_ns
                .fetch_add(a.other_syscall_ns, Ordering::Relaxed);
            FRAME.loop_ns.fetch_add(a.loop_ns, Ordering::Relaxed);
            FRAME.syscalls.fetch_add(a.syscalls, Ordering::Relaxed);
            FRAME.cpu_ns.fetch_add(sched.cpu_ns, Ordering::Relaxed);
            FRAME
                .flip_cpu_ns
                .fetch_add(sched.flip_cpu_ns, Ordering::Relaxed);
            FRAME.vcsw.fetch_add(sched.vcsw, Ordering::Relaxed);
            FRAME.ivcsw.fetch_add(sched.ivcsw, Ordering::Relaxed);
        }
        c.set(FrameAcc {
            open: Some(now),
            cpu_open_ns: cpu_now,
            vcsw_open: vcsw_now,
            ivcsw_open: ivcsw_now,
            ..FrameAcc::default()
        });
    });
    if sampling {
        rip_close_frame(slow);
    }
}

/// Direct measurement of one guest VM exit/entry round trip.
///
/// A guest stub of `mov eax, ID / syscall / dec rdi / jnz / ret` is run for
/// [`ITERATIONS`] iterations at boot. The run loop recognises [`SYSCALL_ID`] and answers
/// it with a constant without entering the HLE dispatcher, so the wall time divided by
/// the iteration count is exactly the fixed cost of leaving guest code and re-entering
/// it — the number that converts an observed syscall *rate* into milliseconds per frame.
///
/// The figure is a floor, deliberately: a tight loop keeps the translation cache hot,
/// where a real title's scattered syscall sites do not.
pub mod calibration {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    /// Magic syscall id the calibration stub issues. Outside the id space of any real
    /// PS4 syscall or NID-derived import.
    pub const SYSCALL_ID: u64 = 0x5CA1_B0FF;

    /// Iterations the stub runs. Large enough that the block is JIT-compiled for the
    /// overwhelming majority of them and the clock read at each end is noise.
    pub const ITERATIONS: u64 = 400_000;

    /// Whether the calibration stub is running (so the run loop short-circuits its id).
    pub(crate) static ACTIVE: AtomicBool = AtomicBool::new(false);

    /// Whether the run loop should answer [`SYSCALL_ID`] itself.
    #[inline]
    pub(crate) fn active() -> bool {
        ACTIVE.load(Ordering::Relaxed)
    }

    /// Measured round trip and host clock-read cost, both as `ns * 1000` so a
    /// sub-nanosecond figure survives an integer atomic. `0` until calibration ran.
    static ROUND_TRIP: AtomicU64 = AtomicU64::new(0);
    static CLOCK: AtomicU64 = AtomicU64::new(0);

    /// Nanoseconds per exit/entry round trip **as the profiler sees it** — including the
    /// four `Instant::now()` reads the profiled run loop itself performs per syscall.
    /// `None` if calibration never ran.
    pub fn round_trip_ns() -> Option<f64> {
        match ROUND_TRIP.load(Ordering::Relaxed) {
            0 => None,
            v => Some(v as f64 / 1000.0),
        }
    }

    /// Nanoseconds for one `Instant::now()` on this host, measured the same way.
    pub fn clock_ns() -> f64 {
        CLOCK.load(Ordering::Relaxed) as f64 / 1000.0
    }

    /// Clock reads the profiled run loop performs per syscall (guest-slice end,
    /// pre-dispatch end, dispatch end, post-dispatch end).
    pub const CLOCK_READS_PER_SYSCALL: f64 = 4.0;

    /// The round trip with the profiler's own clock reads subtracted — what an
    /// unprofiled run pays.
    pub fn round_trip_unprofiled_ns() -> Option<f64> {
        round_trip_ns().map(|rt| (rt - CLOCK_READS_PER_SYSCALL * clock_ns()).max(0.0))
    }

    pub(crate) fn publish(round_trip: f64, clock: f64) {
        ROUND_TRIP.store((round_trip * 1000.0) as u64, Ordering::Relaxed);
        CLOCK.store((clock * 1000.0) as u64, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buckets_are_monotone_and_contain_their_samples() {
        let mut prev = 0;
        for idx in 0..HIST_BUCKETS {
            let lo = bucket_low(idx);
            assert!(lo >= prev, "bucket {idx} low {lo} < previous {prev}");
            prev = lo;
            assert_eq!(bucket_of(lo), idx, "bucket {idx} does not own its own edge");
        }
        for ns in [
            0u64,
            1,
            7,
            8,
            15,
            16,
            1_000,
            16_666_667,
            33_000_000,
            2_000_000_000,
        ] {
            let idx = bucket_of(ns);
            assert!(
                bucket_low(idx) <= ns && ns < bucket_low(idx + 1),
                "{ns} outside bucket {idx}"
            );
        }
    }

    #[test]
    fn top_bucket_reaches_ten_seconds() {
        assert!(bucket_low(HIST_BUCKETS - 1) > 10_000_000_000);
    }

    #[test]
    fn window_percentiles_split_a_bimodal_distribution() {
        let d = Dist::new();
        for _ in 0..95 {
            d.record(16_000_000);
        }
        for _ in 0..5 {
            d.record(100_000_000);
        }
        let mut prev = [0u64; HIST_BUCKETS];
        let w = dist_window(&d, &mut prev);
        assert_eq!(w.count, 100);
        assert_eq!(w.min_ns, 16_000_000);
        assert_eq!(w.max_ns, 100_000_000);
        // The mean (~20 ms) hides the tail that p95/p99 expose — the whole point.
        assert!(
            (14_000_000..18_000_000).contains(&w.p50_ns),
            "p50 {}",
            w.p50_ns
        );
        assert!(w.p99_ns > 90_000_000, "p99 {}", w.p99_ns);

        // A second window over an unchanged histogram is empty.
        let w2 = dist_window(&d, &mut prev);
        assert_eq!(w2.count, 0);
    }

    #[test]
    fn window_is_a_delta_not_a_cumulative_total() {
        let d = Dist::new();
        let mut prev = [0u64; HIST_BUCKETS];
        for _ in 0..10 {
            d.record(1_000_000);
        }
        assert_eq!(dist_window(&d, &mut prev).count, 10);
        for _ in 0..3 {
            d.record(50_000_000);
        }
        let w = dist_window(&d, &mut prev);
        assert_eq!(w.count, 3);
        assert!(w.p50_ns > 40_000_000, "p50 {}", w.p50_ns);
    }
}
