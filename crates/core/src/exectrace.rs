//! Env-gated per-thread guest execution tracer (task-170 diagnostic).
//!
//! A confirm-first observability aid: with `UNEMUPS4_EXECTRACE=1` (or `=<secs>` for a
//! custom dump interval, default [`DEFAULT_INTERVAL_SECS`]) the run loop and the blocking
//! sync HLEs feed three per-guest-thread views into here, and a background thread dumps
//! them periodically so a stuck guest surfaces WHAT each thread is doing:
//!
//! * **(a) per-thread syscall histogram** — every `Exit::Syscall` records `(tid, id)`; the
//!   dump names the id via an installed resolver and flags blocking waits (`[BLOCK]`).
//! * **(b) per-thread RIP histogram** — when enabled the run loop drives each vcpu with a
//!   block budget so `Exit::BudgetExhausted` periodically yields RIP; each sample records
//!   `(tid, rip)`. A thread with 1–3 hot RIPs is spinning; a thread whose hot RIP is a
//!   syscall stub is blocked there.
//! * **(c) host-park heartbeat** — a thread blocked *inside* a Rust handler (host
//!   `Condvar`/`Mutex` park) never returns to the run loop, so (a)/(b) miss it. The sync
//!   HLEs call [`park_enter`]/[`park_exit`]; the dump reports any thread still parked and
//!   for how long, so a never-signalled wedge stands out.
//! * **(d) main-thread backtrace** — the run loop stashes a periodic guest rbp-chain
//!   backtrace of the main thread here via [`record_backtrace`]; the dump prints it so we
//!   know where the per-frame loop body sits.
//!
//! **Gate.** Everything is behind [`enabled`], a `OnceLock<bool>` resolved once from
//! [`EXECTRACE_ENV`], house-style like `UNEMUPS4_WATCHDOG` / `UNEMUPS4_PROFILE`. When the
//! var is unset the whole subsystem is a single cached branch on the hot paths; the mutex
//! is never taken and no thread is spawned.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Environment variable enabling the per-thread execution tracer. Unset / empty / `0` →
/// disabled (zero overhead). `=1` → enabled, [`DEFAULT_INTERVAL_SECS`] dump interval.
/// `=<secs>` (a positive integer) → enabled with that periodic-dump interval.
pub const EXECTRACE_ENV: &str = "UNEMUPS4_EXECTRACE";

/// Default periodic-dump interval when `UNEMUPS4_EXECTRACE=1` (seconds).
pub const DEFAULT_INTERVAL_SECS: u64 = 5;

/// Block budget handed to the run loop when the tracer is on, so `Exit::BudgetExhausted`
/// fires periodically and the RIP histogram gets samples. Chosen large enough that the
/// per-yield overhead is negligible but small enough to sample a tight loop many times a
/// second.
pub const RIP_SAMPLE_BUDGET: u64 = 200_000;

fn resolve_config() -> Option<Duration> {
    match std::env::var(EXECTRACE_ENV).ok().as_deref() {
        None | Some("") | Some("0") => None,
        Some("1") => Some(Duration::from_secs(DEFAULT_INTERVAL_SECS)),
        Some(v) => match v.parse::<u64>() {
            Ok(secs) if secs > 0 => Some(Duration::from_secs(secs)),
            _ => {
                tracing::warn!("{EXECTRACE_ENV}={v:?} is not `1` or a positive integer; disabled");
                None
            }
        },
    }
}

fn config() -> Option<Duration> {
    static CONFIG: OnceLock<Option<Duration>> = OnceLock::new();
    *CONFIG.get_or_init(resolve_config)
}

/// Whether the tracer is enabled. Resolved once, then a cached load.
#[inline]
pub fn enabled() -> bool {
    config().is_some()
}

/// The RIP-sample block budget when the tracer is on, else `None` (unbounded run loop).
#[inline]
pub fn rip_budget() -> Option<u64> {
    enabled().then_some(RIP_SAMPLE_BUDGET)
}

/// Syscall-id → name resolver, installed once from the app's `main` (breaks a
/// `ps4-core -> ps4-syscalls` dep, like the cpu crate's fault annotator). Unset → ids
/// print numerically.
type NameResolver = fn(u64) -> Option<&'static str>;
static NAME_RESOLVER: OnceLock<NameResolver> = OnceLock::new();

/// Install the syscall-id → name resolver (idempotent-safe).
pub fn set_name_resolver(f: NameResolver) {
    let _ = NAME_RESOLVER.set(f);
}

/// Render a dispatch id as its symbol name, falling back to the raw id. Shared with
/// [`crate::breadcrumb`], which resolves the same ids through the same installed resolver.
pub(crate) fn name_of(id: u64) -> String {
    match NAME_RESOLVER.get().and_then(|f| f(id)) {
        Some(n) => n.to_string(),
        None => format!("syscall#{id:#x}"),
    }
}

/// Does this syscall name denote a blocking wait? Case-insensitive substring match over a
/// focused vocabulary; used only at dump time to flag `[BLOCK]` rows.
fn is_blocking(name: &str) -> bool {
    const NEEDLES: &[&str] = &[
        "wait",
        "umtx",
        "cond",
        "lock",
        "sleep",
        "sema",
        "poll",
        "select",
        "join",
        "futex",
        "recv",
        "accept",
        "nanosleep",
    ];
    let lower = name.to_ascii_lowercase();
    NEEDLES.iter().any(|n| lower.contains(n))
}

#[derive(Default)]
struct ThreadTrace {
    name: String,
    syscalls: HashMap<u64, u64>,
    rips: HashMap<u64, u64>,
    total_syscalls: u64,
    total_rip_samples: u64,
    /// Host-park state: `(description, since)` while blocked inside a Rust handler.
    parked: Option<(String, Instant)>,
    /// Latest periodic guest backtrace of this thread (main thread only, today).
    backtrace: Option<String>,
    /// The most recent syscall this thread dispatched, and when. The histogram above says
    /// what a thread has done *ever*; a thread that has gone silent needs the LAST thing it
    /// did, because that call is what handed it whatever it is now looping on.
    last_syscall: Option<(u64, Instant)>,
}

static TRACE: Mutex<Option<HashMap<u32, ThreadTrace>>> = Mutex::new(None);

fn with_thread<F: FnOnce(&mut ThreadTrace)>(tid: u32, f: F) {
    if let Ok(mut guard) = TRACE.lock() {
        let map = guard.get_or_insert_with(HashMap::new);
        let t = map.entry(tid).or_default();
        if t.name.is_empty() {
            t.name = std::thread::current().name().unwrap_or("?").to_string();
        }
        f(t);
    }
}

/// Record a dispatched syscall for the per-thread histogram (a). Caller checks [`enabled`].
#[inline]
pub fn record_syscall(tid: u32, id: u64) {
    with_thread(tid, |t| {
        *t.syscalls.entry(id).or_default() += 1;
        t.total_syscalls += 1;
        t.last_syscall = Some((id, Instant::now()));
    });
}

/// Record a sampled RIP for the per-thread histogram (b). Caller checks [`enabled`].
#[inline]
pub fn record_rip(tid: u32, rip: u64) {
    with_thread(tid, |t| {
        *t.rips.entry(rip).or_default() += 1;
        t.total_rip_samples += 1;
    });
}

/// Stash a periodic guest backtrace (d) for this thread.
#[inline]
pub fn record_backtrace(tid: u32, bt: String) {
    with_thread(tid, |t| t.backtrace = Some(bt));
}

/// Mark the current thread as parked inside a host sync primitive (c). No-op when the
/// tracer is off. Idempotent: only the first park's `since` is kept until [`park_exit`].
///
/// `desc` is a closure so the description — typically a `format!` — is only constructed
/// once the [`enabled`] guard passes: with the tracer off this is a single cached branch
/// and allocates nothing, even on the guest condvar/mutex wait hot path.
#[inline]
pub fn park_enter(tid: u32, desc: impl FnOnce() -> String) {
    if !enabled() {
        return;
    }
    let desc = desc();
    with_thread(tid, |t| {
        if t.parked.is_none() {
            t.parked = Some((desc, Instant::now()));
        }
    });
}

/// Clear the current thread's host-park state (c). No-op when the tracer is off.
#[inline]
pub fn park_exit(tid: u32) {
    if !enabled() {
        return;
    }
    with_thread(tid, |t| t.parked = None);
}

/// Start the background dump thread. Idempotent; a no-op when the tracer is off. Call once
/// from the app's `main` after host wiring.
pub fn start() {
    let Some(interval) = config() else {
        return;
    };
    static STARTED: OnceLock<()> = OnceLock::new();
    if STARTED.set(()).is_err() {
        return;
    }
    std::thread::Builder::new()
        .name("exectrace-dump".into())
        .spawn(move || {
            // Previous per-tid totals, for rate computation between dumps.
            let mut prev: HashMap<u32, (u64, u64)> = HashMap::new();
            let start = Instant::now();
            loop {
                std::thread::sleep(interval);
                dump(start.elapsed(), interval, &mut prev);
            }
        })
        .ok();
}

fn dump(elapsed: Duration, interval: Duration, prev: &mut HashMap<u32, (u64, u64)>) {
    let guard = match TRACE.lock() {
        Ok(g) => g,
        Err(_) => return,
    };
    let Some(map) = guard.as_ref() else {
        return;
    };
    let secs = interval.as_secs_f64().max(0.001);
    let mut tids: Vec<u32> = map.keys().copied().collect();
    tids.sort_unstable();

    let mut out = String::new();
    out.push_str(&format!(
        "\n===== [EXECTRACE] t+{:.0}s ({} guest threads) =====\n",
        elapsed.as_secs_f64(),
        tids.len()
    ));
    for tid in tids {
        let t = &map[&tid];
        let (psys, prip) = prev.get(&tid).copied().unwrap_or((0, 0));
        let sys_rate = (t.total_syscalls.saturating_sub(psys)) as f64 / secs;
        let rip_rate = (t.total_rip_samples.saturating_sub(prip)) as f64 / secs;
        prev.insert(tid, (t.total_syscalls, t.total_rip_samples));

        out.push_str(&format!(
            "\n[tid {tid} \"{}\"] syscalls={} ({:.0}/s) rip_samples={} ({:.0}/s)\n",
            t.name, t.total_syscalls, sys_rate, t.total_rip_samples, rip_rate
        ));

        // (c) host-park heartbeat — the loudest wedge signal.
        if let Some((desc, since)) = &t.parked {
            out.push_str(&format!(
                "    PARKED {:.0}s on {desc}  <-- host Condvar/Mutex, never returned to run loop\n",
                since.elapsed().as_secs_f64()
            ));
        }

        // The last call before the silence. A thread whose syscall rate has fallen to zero
        // is looping on something it was handed; naming that call — and how long ago — is
        // usually the whole lead, and the histogram cannot show it because it has no order.
        if let Some((id, at)) = &t.last_syscall {
            let ago = at.elapsed().as_secs_f64();
            if ago > 1.0 {
                out.push_str(&format!(
                    "    last syscall {:.1}s ago: {}  <-- nothing since; whatever it is doing now started here\n",
                    ago,
                    name_of(*id)
                ));
            }
        }

        // (a) top syscalls, blocking flagged.
        let mut sc: Vec<(u64, u64)> = t.syscalls.iter().map(|(&k, &v)| (k, v)).collect();
        sc.sort_unstable_by_key(|&(_, c)| std::cmp::Reverse(c));
        for (id, count) in sc.iter().take(8) {
            let name = name_of(*id);
            let flag = if is_blocking(&name) { " [BLOCK]" } else { "" };
            out.push_str(&format!("    syscall {name:<28} {count:>10}{flag}\n"));
        }

        // (b) top RIPs — spin vs varied.
        if t.total_rip_samples > 0 {
            let mut rp: Vec<(u64, u64)> = t.rips.iter().map(|(&k, &v)| (k, v)).collect();
            rp.sort_unstable_by_key(|&(_, c)| std::cmp::Reverse(c));
            let distinct = rp.len();
            let top_share =
                rp.first().map(|(_, c)| *c).unwrap_or(0) as f64 / t.total_rip_samples.max(1) as f64;
            let shape = if distinct <= 4 || top_share > 0.6 {
                "SPIN?"
            } else {
                "varied"
            };
            out.push_str(&format!(
                "    rip: {distinct} distinct, top {:.0}% -> {shape}\n",
                top_share * 100.0
            ));
            for (rip, count) in rp.iter().take(5) {
                out.push_str(&format!("      rip {rip:#014x} {count:>10}\n"));
            }
        }

        // (d) main-thread backtrace.
        if let Some(bt) = &t.backtrace {
            out.push_str(&format!("    backtrace:{bt}\n"));
        }
    }
    tracing::info!("{out}");
}
