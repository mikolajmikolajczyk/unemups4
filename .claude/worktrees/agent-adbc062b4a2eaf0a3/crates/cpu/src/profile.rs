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

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

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
    /// Run-loop exits by kind.
    pub exits_budget: AtomicU64,
    pub exits_hlt: AtomicU64,
    pub exits_fatal: AtomicU64,
    /// Sum of `Vcpu::fast_hits()` accumulated from vcpus as `drive` returns.
    pub vcpu_fast_hits: AtomicU64,
}

impl ExecStats {
    const fn new() -> ExecStats {
        ExecStats {
            guest_ns: AtomicU64::new(0),
            run_slices: AtomicU64::new(0),
            syscall_ns: AtomicU64::new(0),
            syscall_count: AtomicU64::new(0),
            exits_budget: AtomicU64::new(0),
            exits_hlt: AtomicU64::new(0),
            exits_fatal: AtomicU64::new(0),
            vcpu_fast_hits: AtomicU64::new(0),
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

/// A consistent read of the execution counters for one dump.
#[derive(Clone, Copy, Debug, Default)]
pub struct ExecSnapshot {
    pub guest_ns: u64,
    pub run_slices: u64,
    pub syscall_ns: u64,
    pub syscall_count: u64,
    pub exits_budget: u64,
    pub exits_hlt: u64,
    pub exits_fatal: u64,
    pub vcpu_fast_hits: u64,
}

/// Snapshot the execution counters (relaxed loads — a dump is a fuzzy point-in-time
/// view, exact ordering across counters is not required).
pub fn snapshot() -> ExecSnapshot {
    ExecSnapshot {
        guest_ns: EXEC.guest_ns.load(Ordering::Relaxed),
        run_slices: EXEC.run_slices.load(Ordering::Relaxed),
        syscall_ns: EXEC.syscall_ns.load(Ordering::Relaxed),
        syscall_count: EXEC.syscall_count.load(Ordering::Relaxed),
        exits_budget: EXEC.exits_budget.load(Ordering::Relaxed),
        exits_hlt: EXEC.exits_hlt.load(Ordering::Relaxed),
        exits_fatal: EXEC.exits_fatal.load(Ordering::Relaxed),
        vcpu_fast_hits: EXEC.vcpu_fast_hits.load(Ordering::Relaxed),
    }
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
