//! Aggregate-profiler dump thread + atexit handler.
//!
//! Gated by `UNEMUPS4_PROFILE` (resolved in `ps4_cpu::profile`). When enabled,
//! [`start`] spawns a background thread that prints a table every dump interval and
//! registers a `libc::atexit` handler so a final table survives the guest's
//! `std::process::exit` path (atexit handlers run on `exit(3)`).
//!
//! The tables go through `tracing::info!(target: "unemups4::profile")`, so they land in
//! the normal log stream and are filterable. The dump reads only relaxed atomics and a
//! mutex snapshot — it never perturbs the guest.

use std::sync::{Arc, OnceLock};
use std::time::Instant;

use ps4_cpu::GuestVm;
use ps4_cpu::profile;
use ps4_gpu::present_profile;
use tracing::info;

/// The shared VM, stashed so both the periodic dump thread and the `extern "C"` atexit
/// handler (which takes no arguments) can reach the x86jit counters.
static VM: OnceLock<Arc<GuestVm>> = OnceLock::new();

/// Process start, captured once for wall-time-relative percentages.
static START: OnceLock<Instant> = OnceLock::new();

/// Start the profiler dump machinery if `UNEMUPS4_PROFILE` is set. No-op (and no thread,
/// no atexit) when the profiler is disabled — the default, zero-overhead path.
pub fn start(vm: Arc<GuestVm>) {
    if !profile::enabled() {
        return;
    }
    let _ = VM.set(vm);
    let _ = START.set(Instant::now());

    let interval = profile::dump_interval();
    info!(
        target: "unemups4::profile",
        "aggregate profiler enabled (UNEMUPS4_PROFILE); dump interval {:?}", interval
    );

    // Periodic dump thread.
    std::thread::Builder::new()
        .name("unemups4-profiler".into())
        .spawn(move || {
            loop {
                std::thread::sleep(interval);
                dump("periodic");
            }
        })
        .expect("spawn profiler dump thread");

    // Final dump on process exit. `std::process::exit` (the guest's exit path) runs
    // atexit handlers, so this table is printed even when the emulator thread ends the
    // process out from under the periodic thread.
    unsafe {
        libc::atexit(atexit_dump);
    }
}

/// atexit runs on the exiting thread *after* TLS destructors, so `tracing`'s
/// thread-local dispatcher is gone — logging there panics with a TLS `AccessError`
/// ("cannot access a Thread Local Storage value during or after destruction"), which is
/// then a non-unwinding abort. So the final dump writes straight to stderr, bypassing
/// tracing entirely.
extern "C" fn atexit_dump() {
    for line in build_dump("final") {
        eprintln!("{line}");
    }
}

fn ns_pct(ns: u64, wall_ns: u64) -> f64 {
    if wall_ns == 0 {
        0.0
    } else {
        (ns as f64 / wall_ns as f64) * 100.0
    }
}

/// Emit one profiler table through `tracing::info!` (the periodic-thread path, where a
/// live subscriber and its TLS are available).
fn dump(reason: &str) {
    for line in build_dump(reason) {
        info!(target: "unemups4::profile", "{line}");
    }
}

/// Build the profiler table as a list of lines. Pure snapshot reads (relaxed atomics +
/// one mutex) — no tracing, no TLS — so it is safe to call from the atexit handler.
fn build_dump(reason: &str) -> Vec<String> {
    let wall = START.get().map(|s| s.elapsed()).unwrap_or_default();
    let wall_ns = wall.as_nanos() as u64;
    let mut out = Vec::new();

    let e = profile::snapshot();
    out.push(format!(
        "===== profiler dump ({reason}) — wall {:.3}s =====",
        wall.as_secs_f64()
    ));
    out.push(format!(
        "  guest exec : {:>12} ns ({:5.1}% wall) over {} slices",
        e.guest_ns,
        ns_pct(e.guest_ns, wall_ns),
        e.run_slices
    ));
    out.push(format!(
        "  syscalls   : {:>12} ns ({:5.1}% wall) over {} calls",
        e.syscall_ns,
        ns_pct(e.syscall_ns, wall_ns),
        e.syscall_count
    ));
    out.push(format!(
        "  exits      : hlt={} budget={} fatal={}  vcpu_fast_hits={}",
        e.exits_hlt, e.exits_budget, e.exits_fatal, e.vcpu_fast_hits
    ));

    // Top syscalls by total ns.
    let mut per = profile::per_syscall_snapshot();
    per.sort_by_key(|b| std::cmp::Reverse(b.1.ns));
    if !per.is_empty() {
        out.push("  top syscalls by total ns:".to_string());
        for (id, s) in per.iter().take(15) {
            let name = ps4_syscalls::SyscallId(*id).as_str();
            let avg = s.ns.checked_div(s.count).unwrap_or(0);
            out.push(format!(
                "    {:<40} {:>10} ns  x{:<8} (avg {} ns)",
                name, s.ns, s.count, avg
            ));
        }
    }

    // x86jit counters.
    if let Some(vm) = VM.get() {
        let j = vm.jit_counters();
        out.push(format!(
            "  x86jit: hits={} misses={} chained={} regions={} ibtc_filled={} tier_bg_published={} tier_bg_rejected={} compile_ns={}",
            j.hits, j.misses, j.chained, j.regions, j.ibtc_filled,
            j.tier_bg_published, j.tier_bg_rejected, j.compile_ns
        ));
    }

    // GPU present-path.
    let g = present_profile::snapshot();
    if g.frames > 0 {
        let f = g.frames;
        let avg_ms = |ns: u64| (ns as f64 / f as f64) / 1_000_000.0;
        out.push(format!(
            "  gpu present: {} frames — avg ms/frame: fence_wait={:.3} acquire={:.3} fb_copy={:.3} record_submit={:.3} present={:.3} pace_sleep={:.3}",
            f,
            avg_ms(g.fence_wait_ns),
            avg_ms(g.acquire_ns),
            avg_ms(g.fb_copy_ns),
            avg_ms(g.record_submit_ns),
            avg_ms(g.present_ns),
            avg_ms(g.pace_sleep_ns),
        ));
    }
    out
}
