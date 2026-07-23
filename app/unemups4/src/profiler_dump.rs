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

/// The previous dump's frame snapshot, so each dump reports the window since the last one
/// rather than a cumulative average dominated by boot.
static PREV_FRAME: std::sync::Mutex<Option<profile::FrameSnapshot>> = std::sync::Mutex::new(None);

/// The dump thread's own emulated-speed window. Separate from the window title's meter so
/// the two never consume each other's window.
static SPEED: std::sync::Mutex<Option<ps4_core::clock::SpeedMeter>> = std::sync::Mutex::new(None);

/// Emulated speed over the window since the previous dump: the ratio of guest-visible
/// elapsed time to real elapsed time. 100% = the guest's world runs at the rate it asked
/// for; 50% = the game is in slow motion however smooth the frames look.
fn emulated_speed() -> String {
    let pct = match SPEED.lock() {
        Ok(mut g) => g
            .get_or_insert_with(ps4_core::clock::SpeedMeter::new)
            .sample(),
        Err(_) => 0.0,
    };
    format!(
        "  emulated speed: {:5.1}% guest time / real time ({} clock)",
        pct,
        ps4_core::clock::mode().as_str()
    )
}

/// The guest frame budget of the flipping thread (task-209): the flip-to-flip wall time
/// of one guest frame, split into the phases that tile it, over the window since the
/// previous dump. The rows are measured independently of the wall, so the remainder is a
/// real unaccounted figure, not a definition.
fn frame_budget() -> Vec<String> {
    let now = profile::frame_snapshot();
    let prev = match PREV_FRAME.lock() {
        Ok(mut g) => g.replace(now).unwrap_or_default(),
        Err(_) => profile::FrameSnapshot::default(),
    };
    let frames = now.frames.saturating_sub(prev.frames);
    if frames == 0 {
        return Vec::new();
    }
    let d = |cur: u64, old: u64| cur.saturating_sub(old);
    let ms = |ns: u64| (ns as f64 / frames as f64) / 1_000_000.0;

    let wall = ms(d(now.wall_ns, prev.wall_ns));
    let guest = ms(d(now.guest_ns, prev.guest_ns));
    let flip = ms(d(now.flip_ns, prev.flip_ns));
    let other = ms(d(now.other_syscall_ns, prev.other_syscall_ns));
    let loop_ns = ms(d(now.loop_ns, prev.loop_ns));
    let rest = wall - guest - flip - other - loop_ns;
    let syscalls = d(now.syscalls, prev.syscalls) as f64 / frames as f64;

    let mut out = vec![format!(
        "  guest frame [tid {}] window: {} frames, {:.2} fps — avg {:.3} ms = guest_exec {:.3} + flip {:.3} + other_syscalls {:.3} + run_loop {:.3} + unaccounted {:.3} ({:.1}%)",
        now.tid,
        frames,
        if wall > 0.0 { 1000.0 / wall } else { 0.0 },
        wall,
        guest,
        flip,
        other,
        loop_ns,
        rest,
        if wall > 0.0 { rest / wall * 100.0 } else { 0.0 },
    )];
    // Guest instructions (task-220, x86jit task-281). Derived only from `FrameSnapshot`,
    // whose fields are all accumulated at the same frame boundaries — an earlier version
    // divided a process-wide counter read at dump time by a per-frame one and reported 0.4
    // instructions per block transition, which is impossible for a block of at least one.
    let d_exec = d(now.executed, prev.executed);
    if d_exec > 0 {
        let per_frame = d_exec as f64 / frames as f64;
        let guest_ns = d(now.guest_ns, prev.guest_ns);
        let mips = if guest_ns > 0 {
            d_exec as f64 * 1_000.0 / guest_ns as f64
        } else {
            0.0
        };
        out.push(format!(
            "    guest instructions: {per_frame:.0}/frame ({:.1}M), {mips:.0} MIPS over guest-exec time",
            per_frame / 1_000_000.0,
        ));
    }
    out.extend(scheduler_rows(
        "    ",
        wall,
        flip,
        ms(d(now.cpu_ns, prev.cpu_ns)),
        ms(d(now.flip_cpu_ns, prev.flip_cpu_ns)),
        d(now.vcsw, prev.vcsw) as f64 / frames as f64,
        d(now.ivcsw, prev.ivcsw) as f64 / frames as f64,
    ));
    match profile::calibration::round_trip_ns() {
        Some(rt) => {
            let clock = profile::calibration::clock_ns();
            let raw = profile::calibration::round_trip_unprofiled_ns().unwrap_or(0.0);
            let exit_ms = syscalls * rt / 1_000_000.0;
            let exit_raw_ms = syscalls * raw / 1_000_000.0;
            out.push(format!(
                "    vm exit/entry: {:.0} ns/round trip measured ({:.0} ns of that is this profiler's {:.0} clock reads; {:.0} ns unprofiled)",
                rt,
                profile::calibration::CLOCK_READS_PER_SYSCALL * clock,
                profile::calibration::CLOCK_READS_PER_SYSCALL,
                raw,
            ));
            out.push(format!(
                "    {:.0} syscalls/frame x round trip = {:.3} ms/frame profiled ({:.1}% of the frame), {:.3} ms/frame unprofiled",
                syscalls,
                exit_ms,
                if wall > 0.0 { exit_ms / wall * 100.0 } else { 0.0 },
                exit_raw_ms,
            ));
        }
        None => out.push(format!(
            "    {syscalls:.0} syscalls/frame (vm-exit calibration did not run)"
        )),
    }
    out
}

/// Wall time against thread CPU time for the flipping thread (task-215).
///
/// Every phase in the frame budget is measured with `Instant::now()`, so it is elapsed
/// time — a thread the host takes off a core accrues it exactly as fast as one that is
/// computing. `CLOCK_THREAD_CPUTIME_ID` advances only while the thread is on a core, so
/// the gap between the two is time the frame spent not running, and the context-switch
/// counts say which kind: voluntary means the thread blocked (a wait, a lock), involuntary
/// means the scheduler preempted it while it was still runnable.
///
/// The flip is separated out because it *should* be off-core — it waits on the GPU. What
/// remains is the guest-execution side, where wall above CPU is unexplained.
fn scheduler_rows(
    indent: &str,
    wall: f64,
    flip: f64,
    cpu: f64,
    flip_cpu: f64,
    vcsw: f64,
    ivcsw: f64,
) -> Vec<String> {
    if cpu <= 0.0 {
        return Vec::new();
    }
    let pct = |num: f64, den: f64| if den > 0.0 { num / den * 100.0 } else { 0.0 };
    let guest_wall = wall - flip;
    let guest_cpu = cpu - flip_cpu;
    vec![
        format!(
            "{indent}wall vs thread cpu: cpu {cpu:.3} of {wall:.3} ms ({:.0}% on-core) — flip cpu {flip_cpu:.3} of {flip:.3} ms ({:.0}%); rest of the frame (guest_exec+other_syscalls+run_loop) cpu {guest_cpu:.3} of {guest_wall:.3} ms ({:.0}%)",
            pct(cpu, wall),
            pct(flip_cpu, flip),
            pct(guest_cpu, guest_wall),
        ),
        format!(
            "{indent}context switches/frame: {vcsw:.1} voluntary (thread blocked) + {ivcsw:.1} involuntary (preempted while runnable)"
        ),
    ]
}

/// Previous-window bucket counts, so each dump's percentiles describe only the frames
/// since the last one. Owned by the dump path (one reader) — see `profile::dist_window`.
static PREV_FRAME_HIST: std::sync::Mutex<[u64; profile::HIST_BUCKETS]> =
    std::sync::Mutex::new([0; profile::HIST_BUCKETS]);
static PREV_FLIP_HIST: std::sync::Mutex<[u64; profile::HIST_BUCKETS]> =
    std::sync::Mutex::new([0; profile::HIST_BUCKETS]);
static PREV_SLOW: std::sync::Mutex<Option<profile::SlowSnapshot>> = std::sync::Mutex::new(None);
static PREV_PRESENT: std::sync::Mutex<Option<present_profile::PresentSnapshot>> =
    std::sync::Mutex::new(None);

fn dist_row(
    label: &str,
    dist: &profile::Dist,
    prev: &std::sync::Mutex<[u64; profile::HIST_BUCKETS]>,
) -> Option<String> {
    let w = match prev.lock() {
        Ok(mut g) => profile::dist_window(dist, &mut g),
        Err(_) => return None,
    };
    if w.count == 0 {
        return None;
    }
    let ms = |ns: u64| ns as f64 / 1_000_000.0;
    Some(format!(
        "    {label:<10} n={:<5} min {:7.3} | p50 {:7.3} | p95 {:7.3} | p99 {:7.3} | max {:7.3} ms",
        w.count,
        ms(w.min_ns),
        ms(w.p50_ns),
        ms(w.p95_ns),
        ms(w.p99_ns),
        ms(w.max_ns),
    ))
}

/// Frame-time distribution and slow-frame attribution (task-213).
///
/// A mean cannot tell steady 33 ms frames from an alternating 16/50 ms pattern, and those
/// feel completely different. These rows carry the shape of the window instead: the
/// percentiles, then — for the frames that exceeded the threshold — the same phase split
/// the average row prints, the syscall that dominated them, and how far apart they were.
fn frame_distribution() -> Vec<String> {
    let mut out = Vec::new();
    let frame_row = dist_row("frame", &profile::FRAME_DIST, &PREV_FRAME_HIST);
    let flip_row = dist_row("flip call", &profile::FLIP_DIST, &PREV_FLIP_HIST);
    if frame_row.is_none() && flip_row.is_none() {
        return out;
    }
    out.push("  frame distribution (this window):".to_string());
    out.extend(frame_row);
    out.extend(flip_row);

    let now = profile::slow_snapshot();
    let prev = match PREV_SLOW.lock() {
        Ok(mut g) => g.replace(now).unwrap_or_default(),
        Err(_) => profile::SlowSnapshot::default(),
    };
    let d = |cur: u64, old: u64| cur.saturating_sub(old);
    let slow = d(now.frames, prev.frames);
    let reference = profile::reference_frame_ns();
    out.push(format!(
        "    slow frames: {} over {} hitches — threshold {:.2} x {:.3} ms reference (running median)",
        slow,
        d(now.hitches, prev.hitches),
        profile::slow_multiple(),
        reference as f64 / 1_000_000.0,
    ));
    if slow > 0 {
        let ms = |ns: u64| (ns as f64 / slow as f64) / 1_000_000.0;
        let wall = ms(d(now.wall_ns, prev.wall_ns));
        let guest = ms(d(now.guest_ns, prev.guest_ns));
        let flip = ms(d(now.flip_ns, prev.flip_ns));
        let other = ms(d(now.other_syscall_ns, prev.other_syscall_ns));
        let loop_ms = ms(d(now.loop_ns, prev.loop_ns));
        out.push(format!(
            "    slow frame avg {:.3} ms = guest_exec {:.3} + flip {:.3} + other_syscalls {:.3} + run_loop {:.3} + unaccounted {:.3}",
            wall,
            guest,
            flip,
            other,
            loop_ms,
            wall - guest - flip - other - loop_ms,
        ));
        out.extend(scheduler_rows(
            "    slow frame ",
            wall,
            flip,
            ms(d(now.cpu_ns, prev.cpu_ns)),
            ms(d(now.flip_cpu_ns, prev.flip_cpu_ns)),
            d(now.vcsw, prev.vcsw) as f64 / slow as f64,
            d(now.ivcsw, prev.ivcsw) as f64 / slow as f64,
        ));
    }
    let gaps = d(now.gap_count, prev.gap_count);
    if gaps > 0 {
        out.push(format!(
            "    hitch period: {} gaps this window, mean {:.1} frames (min {} max {} since start), burst {:.1} frames avg (max {} since start)",
            gaps,
            d(now.gap_sum, prev.gap_sum) as f64 / gaps as f64,
            now.gap_min,
            now.gap_max,
            d(now.burst_sum, prev.burst_sum) as f64
                / d(now.hitches, prev.hitches).max(1) as f64,
            now.burst_max,
        ));
        let ring = profile::gap_ring();
        if !ring.is_empty() {
            out.push(format!(
                "    recent hitch gaps (frames): {}",
                ring.iter()
                    .map(u32::to_string)
                    .collect::<Vec<_>>()
                    .join(" ")
            ));
        }
    }

    let (mut offenders, overflow) = profile::offender_snapshot();
    offenders.sort_by_key(|o| std::cmp::Reverse(o.1));
    if !offenders.is_empty() {
        out.push(format!(
            "    longest syscall in each slow frame (cumulative{}):",
            if overflow > 0 {
                format!(", {overflow} frames past the {}-id table", offenders.len())
            } else {
                String::new()
            }
        ));
        for (id, count, ns) in offenders.iter().take(6) {
            out.push(format!(
                "      {:<40} topped {:>6} slow frames (avg {:.3} ms)",
                ps4_syscalls::SyscallId(*id).as_str(),
                count,
                (*ns as f64 / *count as f64) / 1_000_000.0,
            ));
        }
    }

    out.extend(slow_frame_rips());

    // The raw recent-frame ring: a periodic hitch is visible here as a repeating column,
    // which no summary statistic can show.
    let ring = profile::frame_ring();
    if !ring.is_empty() {
        out.push(format!(
            "    recent frame times (ms, oldest first, {} frames):",
            ring.len()
        ));
        for chunk in ring.chunks(20) {
            out.push(format!(
                "      {}",
                chunk
                    .iter()
                    .map(|us| format!("{:6.1}", *us as f64 / 1000.0))
                    .collect::<Vec<_>>()
                    .join("")
            ));
        }
    }
    out
}

/// Where the flipping thread's guest RIP was during the frames that turned out slow
/// (task-215, `UNEMUPS4_PROFILE_RIP`).
///
/// The sampler runs on a block budget, but a sample is only *kept* if the frame it was
/// taken in crossed the slow threshold — so this is the slow-frame predicate driving the
/// aggregation, not a wall clock. Addresses are named through the module map the fault
/// annotator already carries; a stripped module resolves to `name +offset` only.
fn slow_frame_rips() -> Vec<String> {
    let (rips, total, frames, lost, overflow) = profile::rip_snapshot();
    if total == 0 {
        return Vec::new();
    }
    let mut out = vec![format!(
        "    slow-frame guest rip: {total} samples over {frames} slow frames (cumulative; {lost} lost to a full frame buffer, {overflow} to a full table) — an address resolves to module+offset only where that module exports no symbol to name it"
    )];
    for (rip, count) in rips.iter().take(12) {
        out.push(format!(
            "      {rip:#014x} {count:>8} ({:4.1}%)  {}",
            *count as f64 / total as f64 * 100.0,
            profile::describe_guest_addr(*rip).unwrap_or_else(|| "<unresolved>".to_string()),
        ));
    }
    out
}

/// Previous window's per-variant walk snapshot and the flip count it was taken at, so the
/// breakdown below is a per-window delta rather than a cumulative average dominated by
/// boot (which is where every pipeline/image/sampler create happens).
static PREV_CMD: std::sync::Mutex<Option<(present_profile::CmdSnapshot, u64)>> =
    std::sync::Mutex::new(None);

/// The per-`BackendCmd`-variant breakdown of the display-thread walk (task-222), over the
/// window since the previous dump. `flips` is the cumulative flip denominator the caller
/// already computed; the delta against the previous dump's value is this window's.
///
/// Only variants seen this window are printed, ns-descending — the point of the row is
/// which command the walk is actually spending its time in, and a table of zeroes hides it.
fn cmd_walk_rows(flips: u64) -> Vec<String> {
    let now = present_profile::cmd_snapshot();
    let prev = match PREV_CMD.lock() {
        Ok(mut g) => g.replace((now, flips)),
        Err(_) => return Vec::new(),
    };
    let (prev, prev_flips) = prev.unwrap_or_default();
    let flips = flips.saturating_sub(prev_flips);
    if flips == 0 {
        return Vec::new();
    }
    let mut rows: Vec<(usize, u64, u64)> = (0..present_profile::CMD_NAMES.len())
        .map(|i| {
            (
                i,
                now.count[i].saturating_sub(prev.count[i]),
                now.ns[i].saturating_sub(prev.ns[i]),
            )
        })
        .filter(|(_, count, _)| *count > 0)
        .collect();
    rows.sort_by_key(|(_, _, ns)| std::cmp::Reverse(*ns));
    let total_count: u64 = rows.iter().map(|(_, c, _)| c).sum();
    let total_ns: u64 = rows.iter().map(|(_, _, ns)| ns).sum();
    let mut out = vec![format!(
        "    cmd walk [window]: {:.0} cmds/flip, {:.3} ms/flip attributed, {} MiB/flip uploaded",
        total_count as f64 / flips as f64,
        (total_ns as f64 / flips as f64) / 1_000_000.0,
        format_args!(
            "{:.2}",
            (now.upload_bytes.saturating_sub(prev.upload_bytes) as f64 / flips as f64)
                / (1024.0 * 1024.0)
        ),
    )];
    for (i, count, ns) in rows {
        out.push(format!(
            "      {:<18} {:>7.1}/flip  {:7.3} ms/flip ({:4.1}%)  avg {:>8.0} ns each",
            present_profile::CMD_NAMES[i],
            count as f64 / flips as f64,
            (ns as f64 / flips as f64) / 1_000_000.0,
            if total_ns > 0 {
                ns as f64 / total_ns as f64 * 100.0
            } else {
                0.0
            },
            ns as f64 / count as f64,
        ));
    }
    out
}

/// Previous window's resource-cache snapshot and the flip count it was taken at.
static PREV_CACHE: std::sync::Mutex<Option<(ps4_gnm::profile::CacheSnapshot, u64)>> =
    std::sync::Mutex::new(None);

/// Why the guest-side resource cache had to emit a `CreateBuffer`/`CreateImage`, and what
/// the display side's allocator did with it (task-223), over the window since the previous
/// dump. Per-window like `cmd walk`, not cumulative.
///
/// The per-kind row separates the two questions a create raises. `hit/dirty/create` says
/// how often the key was reusable at all. The miss breakdown says why it was not:
/// `new_base` is the guest putting the data somewhere it has never been (a rotating ring
/// — a key containing that address cannot hit by construction), `new_size` is the same
/// base with a different extent (a key that *could* hit if the size were bucketed), and
/// `recreate` is an evicted entry being rebuilt. `sub_range` counts the creates whose
/// bytes were already resident inside a live entry.
fn res_cache_rows(flips: u64) -> Vec<String> {
    let now = ps4_gnm::profile::cache_snapshot();
    let prev = match PREV_CACHE.lock() {
        Ok(mut g) => g.replace((now, flips)),
        Err(_) => return Vec::new(),
    };
    let (prev, prev_flips) = prev.unwrap_or_default();
    let flips = flips.saturating_sub(prev_flips);
    if flips == 0 {
        return Vec::new();
    }
    let d = |cur: u64, old: u64| cur.saturating_sub(old) as f64 / flips as f64;
    let mut out = Vec::new();
    for i in 0..ps4_gnm::profile::RES_KINDS.len() {
        let creates = d(now.creates[i], prev.creates[i]);
        let gets = d(now.gets[i], prev.gets[i]);
        if gets == 0.0 {
            continue;
        }
        out.push(format!(
            "      {:<8} {:>6.1} gets/flip = {:>5.1} clean + {:>5.1} dirty + {:>5.1} create ({:.0} KiB) — miss: new_base {:>5.1}, new_size {:>5.1}, recreate {:>5.1}; of those {:>5.1} sub-ranges of a live entry",
            ps4_gnm::profile::RES_KINDS[i],
            gets,
            d(now.clean_hits[i], prev.clean_hits[i]),
            d(now.dirty_hits[i], prev.dirty_hits[i]),
            creates,
            d(now.create_bytes[i], prev.create_bytes[i]) / 1024.0,
            d(now.miss_new_base[i], prev.miss_new_base[i]),
            d(now.miss_new_size[i], prev.miss_new_size[i]),
            d(now.miss_recreate[i], prev.miss_recreate[i]),
            d(now.miss_sub_range[i], prev.miss_sub_range[i]),
        ));
    }
    if out.is_empty() {
        return out;
    }
    let p = present_profile::pool_snapshot();
    out.insert(
        0,
        format!(
            "    res cache [window]: {} live entries over {} distinct (base,kind) — backend holds {} buffers in {} device allocations ({:.1} MiB, {} pool blocks); {:.1} creates/flip recycled, {:.1} fresh",
            now.live_entries,
            now.distinct_bases,
            p.live_buffers,
            p.live_allocations,
            p.alloc_bytes as f64 / (1024.0 * 1024.0),
            p.blocks,
            d(p.recycled, prev_pool_recycled()),
            d(p.fresh, prev_pool_fresh()),
        ),
    );
    store_prev_pool(p);
    out
}

/// The previous window's allocator gauges, kept beside [`PREV_CACHE`] so the recycled/fresh
/// figures in its header row are per-window like everything else on it.
static PREV_POOL: std::sync::Mutex<present_profile::PoolSnapshot> =
    std::sync::Mutex::new(present_profile::PoolSnapshot {
        live_buffers: 0,
        live_allocations: 0,
        alloc_bytes: 0,
        recycled: 0,
        fresh: 0,
        blocks: 0,
    });

fn prev_pool_recycled() -> u64 {
    PREV_POOL.lock().map(|p| p.recycled).unwrap_or(0)
}

fn prev_pool_fresh() -> u64 {
    PREV_POOL.lock().map(|p| p.fresh).unwrap_or(0)
}

fn store_prev_pool(p: present_profile::PoolSnapshot) {
    if let Ok(mut g) = PREV_POOL.lock() {
        *g = p;
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

/// Serializes the whole of [`build_dump`] so the periodic dump thread and the atexit
/// handler cannot interleave. Both paths consume the same per-window `PREV_*` snapshots
/// (the replace-old-with-new / report(new − old) dance), and `exit(3)` runs atexit
/// handlers WITHOUT suspending the still-looping periodic thread — so without this guard
/// whichever call reaches a `PREV_*` mutex first consumes the window and the other sees a
/// ~0 delta and prints garbage per-window figures. `into_inner` on poison because a panic
/// unwinding out of the `extern "C"` atexit handler is a non-unwinding abort.
static DUMP_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Build the profiler table as a list of lines. Pure snapshot reads (relaxed atomics +
/// one mutex) — no tracing, no TLS — so it is safe to call from the atexit handler.
fn build_dump(reason: &str) -> Vec<String> {
    let _dump_guard = DUMP_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let wall = START.get().map(|s| s.elapsed()).unwrap_or_default();
    let wall_ns = wall.as_nanos() as u64;
    let mut out = Vec::new();

    let e = profile::snapshot();
    out.push(format!(
        "===== profiler dump ({reason}) — wall {:.3}s =====",
        wall.as_secs_f64()
    ));
    out.push(emulated_speed());
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
        "  run loop   : {:>12} ns ({:5.1}% wall) marshalling + breadcrumb around dispatch",
        e.pre_dispatch_ns + e.post_dispatch_ns,
        ns_pct(e.pre_dispatch_ns + e.post_dispatch_ns, wall_ns)
    ));
    // `vcpu_fast_hits` also as a per-window delta: a cumulative total hides a counter that
    // has stopped moving behind a large number, which is precisely how this one stayed
    // frozen unnoticed (task-218). A `+0` in a live run is now visible on sight.
    let fast_hits_delta = {
        static PREV: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let prev = PREV.swap(e.vcpu_fast_hits, std::sync::atomic::Ordering::Relaxed);
        e.vcpu_fast_hits.saturating_sub(prev)
    };
    out.push(format!(
        "  exits      : hlt={} budget={} fatal={}  vcpu_fast_hits={} (+{} this window)",
        e.exits_hlt, e.exits_budget, e.exits_fatal, e.vcpu_fast_hits, fast_hits_delta
    ));
    out.extend(frame_budget());
    out.extend(frame_distribution());

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

    // Syscalls that have NOT returned. The table above counts completed calls, so a thread
    // parked in a blocking wait is invisible there — this is where a stalled title shows up,
    // named, with how long it has been waiting (task-113.2).
    let mut in_flight = ps4_cpu::profile::in_flight_syscalls();
    if !in_flight.is_empty() {
        // A stalled engine parks its whole worker pool in the same wait, so an unordered
        // list truncated at N is all pool threads and none of the interesting ones. Sort
        // longest-blocked first — the thread that has been stuck since before everything
        // else went quiet is the one holding the answer — and summarise the rest by call.
        in_flight.sort_by_key(|x| std::cmp::Reverse(x.2));
        let mut by_call: std::collections::BTreeMap<&str, usize> =
            std::collections::BTreeMap::new();
        for (_, id, _) in &in_flight {
            *by_call
                .entry(ps4_syscalls::SyscallId(*id).as_str())
                .or_default() += 1;
        }
        let summary = by_call
            .iter()
            .map(|(name, n)| format!("{n}x {name}"))
            .collect::<Vec<_>>()
            .join(", ");
        out.push(format!(
            "  in-flight syscalls: {} thread(s) inside a call that has not returned — {summary}",
            in_flight.len()
        ));
        // Show the longest waiter of EVERY distinct call before filling the rest by
        // duration. A stalled engine's list is dominated by one idle pool; the single
        // thread stuck in a different call — a mutex nobody released, an output that never
        // drained — is the one worth printing, and duration alone buries it.
        let mut seen_call: std::collections::BTreeSet<u64> = std::collections::BTreeSet::new();
        let mut ordered: Vec<&(u32, u64, std::time::Duration)> = Vec::new();
        for entry in &in_flight {
            if seen_call.insert(entry.1) {
                ordered.push(entry);
            }
        }
        let firsts: std::collections::BTreeSet<u32> = ordered.iter().map(|e| e.0).collect();
        ordered.extend(in_flight.iter().filter(|e| !firsts.contains(&e.0)));

        let kernel = ps4_core::kernel::get_kernel();
        for (tid, id, waited) in ordered.iter().take(16) {
            // The thread's own name is what turns "tid 17 is stuck" into "the render thread
            // is stuck" — worth a lookup per line in a dump that prints once a window.
            let name = kernel
                .as_ref()
                .and_then(|k| k.thread_name_of(*tid))
                .unwrap_or_default();
            out.push(format!(
                "    tid {:<4} {:<24} {:<40} blocked {:.3} s",
                tid,
                name,
                ps4_syscalls::SyscallId(*id).as_str(),
                waited.as_secs_f64()
            ));
        }
        if in_flight.len() > 16 {
            out.push(format!(
                "    … and {} more (see the per-call summary above)",
                in_flight.len() - 16
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

        // Calls out of compiled code into interpreter helpers (x86jit task-283).
        // Normalized per thousand guest instructions, because an absolute count says
        // nothing without the work it accompanies. x86jit's calibration: synthetic
        // workloads 0.00 per kinstr, sqlite 3.35, lua 2.55 — helper traffic is not
        // inherent, so a high reading here is a finding, not the expected baseline. Near
        // zero rules helpers out and sends the ~30-host-cycles-per-guest-instruction
        // question (x86jit task-282) to block disassembly instead.
        let helpers = vm.helper_calls();
        if !helpers.is_empty() {
            static PREV_H: std::sync::Mutex<(Vec<(&'static str, u64)>, u64)> =
                std::sync::Mutex::new((Vec::new(), 0));
            if let Ok(mut prev) = PREV_H.lock() {
                let (before, p_exec) = &mut *prev;
                let d_exec = e.vcpu_executed.saturating_sub(*p_exec);
                let mut rows: Vec<String> = Vec::new();
                let mut total = 0u64;
                for (name, count) in &helpers {
                    let was = before
                        .iter()
                        .find(|(n, _)| n == name)
                        .map_or(0, |(_, c)| *c);
                    let d = count.saturating_sub(was);
                    total += d;
                    if d > 0 {
                        rows.push(format!("{name}={d}"));
                    }
                }
                *before = helpers;
                *p_exec = e.vcpu_executed;
                if d_exec > 0 {
                    out.push(format!(
                        "  helper calls: {total} this window, {:.2} per kinstr — {}",
                        total as f64 * 1_000.0 / d_exec as f64,
                        if rows.is_empty() {
                            "none".to_string()
                        } else {
                            rows.join(" ")
                        }
                    ));
                }
            }
        }

        // INTERPRETER-retired guest instructions only (task-220). x86jit's counter
        // deliberately does not tick inside compiled blocks — charging retirement there
        // would need codegen changes it avoids — so on our path this counts single-steps
        // (MMIO retry, pre-tier-up execution), NOT the JIT-executed bulk. It therefore
        // cannot answer "is 25 ms of guest execution many instructions run slowly, or an
        // abnormal number run at a fair rate"; that needs a compiled-path count, filed as
        // x86jit task-281. Kept and labelled honestly because a spike here is still a real
        // signal — it means tier-up is failing or something is single-stepping.
        // Deltas, because a cumulative count says nothing about the current scene.
        static PREV: std::sync::Mutex<(u64, u64)> = std::sync::Mutex::new((0, 0));
        if let Ok(mut prev) = PREV.lock() {
            let (p_ret, p_chained) = *prev;
            *prev = (e.vcpu_retired, j.chained);
            let d_ret = e.vcpu_retired.saturating_sub(p_ret);
            let d_chained = j.chained.saturating_sub(p_chained);
            // Ratio against block transitions, to show at a glance how small this is next
            // to the compiled bulk: near zero is the healthy steady state, a rising share
            // means execution is falling back to the interpreter.
            let per_transition = if d_chained > 0 {
                d_ret as f64 / d_chained as f64
            } else {
                0.0
            };
            out.push(format!(
                "  interp-retired instructions: +{d_ret} this window ({per_transition:.4} per block transition) — EXCLUDES compiled blocks, so this is not the guest's instruction count"
            ));
        }
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
        // MAILBOX can only discard an image when the next present arrives inside the same
        // refresh period; Vulkan reports no discard count (see `present_profile::note_present`),
        // so this bounds it rather than measuring it.
        let (gap_min, gap_max) = present_profile::take_present_gap_extremes();
        let prev = match PREV_PRESENT.lock() {
            Ok(mut lock) => lock.replace(g).unwrap_or_default(),
            Err(_) => present_profile::PresentSnapshot::default(),
        };
        let gaps = g.frames.saturating_sub(prev.frames);
        if gaps > 0 {
            out.push(format!(
                "    present interval [window]: avg {:.3} ms (min {:.3} max {:.3}) — {} of {} presents landed inside one 60 Hz period (upper bound on MAILBOX discards, which Vulkan does not report)",
                (g.present_gap_ns.saturating_sub(prev.present_gap_ns) as f64 / gaps as f64)
                    / 1_000_000.0,
                gap_min as f64 / 1_000_000.0,
                gap_max as f64 / 1_000_000.0,
                g.presents_within_vblank
                    .saturating_sub(prev.presents_within_vblank),
                gaps,
            ));
        }
    }

    // GNM submit path. Denominated per flip-syscall CALL (not per present) so the rows are
    // directly comparable to that syscall's own per-call average in the table above — the
    // arithmetic check that the submit path is fully accounted for.
    let (flip_ns, flip_calls) = per
        .iter()
        .filter(|(id, _)| {
            *id == ps4_syscalls::SyscallId::SCE_GNM_SUBMIT_AND_FLIP_COMMAND_BUFFERS.0
                || *id
                    == ps4_syscalls::SyscallId::SCE_GNM_SUBMIT_AND_FLIP_COMMAND_BUFFERS_FOR_WORKLOAD
                        .0
        })
        .fold((0u64, 0u64), |(ns, c), (_, st)| (ns + st.ns, c + st.count));
    let s = present_profile::submit_snapshot();
    let n = if flip_calls > 0 { flip_calls } else { g.frames };
    if n > 0 && s.guest_submit_calls > 0 {
        let ms = |v: u64| (v as f64 / n as f64) / 1_000_000.0;
        let each = |v: u64| v as f64 / n as f64;
        out.push(format!(
            "  gpu submit: {} lists ({:.1}/flip, {:.1} passes, {:.1} draws) — avg ms/flip: guest_submit_wait={:.3} guest_flip_wait={:.3}",
            s.guest_submit_calls,
            each(s.guest_submit_calls),
            each(s.passes),
            each(s.draws),
            ms(s.guest_submit_ns),
            ms(s.guest_flip_ns),
        ));
        out.push(format!(
            "    display side: backend={:.3} = walk={:.3} + record_passes={:.3} + readback={:.3}",
            ms(s.backend_ns),
            ms(s.walk_ns),
            ms(s.record_passes_ns),
            ms(s.readback_ns),
        ));
        out.push(format!(
            "    record_passes: record={:.3} (transient_create={:.3}) queue_submit={:.3} draw_fence={:.3} transient_destroy={:.3}",
            ms(s.record_ns),
            ms(s.transient_create_ns),
            ms(s.queue_submit_ns),
            ms(s.draw_fence_ns),
            ms(s.transient_destroy_ns),
        ));
        out.push(format!(
            "    transients/flip: render_pass={:.1} framebuffer={:.1} desc_pool={:.1}",
            each(s.transient_render_passes),
            each(s.transient_framebuffers),
            each(s.descriptor_pools),
        ));
        out.extend(cmd_walk_rows(n));
        out.extend(res_cache_rows(n));
        // The guest-thread half of the same syscall: PM4 decode + the packet walk that
        // produced those lists. `run_ns` brackets the two sink round trips above, so the
        // walk is what is left after subtracting them and the decode.
        let x = ps4_gnm::profile::snapshot();
        let walk_ns = x
            .run_ns
            .saturating_sub(x.decode_ns)
            .saturating_sub(x.packet_free_ns)
            .saturating_sub(s.guest_submit_ns)
            .saturating_sub(s.guest_flip_ns);
        if x.runs > 0 {
            out.push(format!(
                "  pm4 exec: {} runs ({:.1}/flip, {:.0} packets/flip) — avg ms/flip: handler={:.3} (lock_wait={:.3} apply_dirty={:.3}) run={:.3} = decode={:.3} + packet_free={:.3} + walk={:.3} + sink waits={:.3}",
                x.runs,
                each(x.runs),
                each(x.packets),
                ms(x.submit_ns),
                ms(x.lock_ns),
                ms(x.dirty_ns),
                ms(x.run_ns),
                ms(x.decode_ns),
                ms(x.packet_free_ns),
                ms(walk_ns),
                ms(s.guest_submit_ns + s.guest_flip_ns),
            ));
        }
        if flip_calls > 0 {
            let avg = flip_ns as f64 / flip_calls as f64 / 1_000_000.0;
            let rest = avg - ms(x.submit_ns);
            out.push(format!(
                "  flip budget: sceGnmSubmitAndFlip avg={:.3} ms over {} calls = decode+free {:.3} + walk {:.3} + submit_wait {:.3} + flip_wait {:.3} + apply_dirty {:.3} + handler rest {:.3} + syscall overhead {:.3} ({:.1}% unaccounted)",
                avg,
                flip_calls,
                ms(x.decode_ns + x.packet_free_ns),
                ms(walk_ns),
                ms(s.guest_submit_ns),
                ms(s.guest_flip_ns),
                ms(x.dirty_ns),
                ms(x.submit_ns.saturating_sub(x.run_ns).saturating_sub(x.dirty_ns)),
                rest,
                if avg > 0.0 { rest / avg * 100.0 } else { 0.0 },
            ));
        }
    }
    out
}
