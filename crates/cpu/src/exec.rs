//! The guest execution run loop (x86jit backend).
//!
//! A guest function is entered on a fresh [`Vcpu`] with the HLT gadget pushed as its return
//! address; the loop drives `Vcpu::run` and services each [`Exit`]:
//!
//! * [`Exit::Syscall`] — marshal the guest GPRs into a [`NativeContext`], hand it to the
//!   globally-installed dispatch fn, write the returned value back into guest `RAX`, and
//!   resume. A handler may call [`call_guest`] (nested guest call) or
//!   [`request_thread_exit`] (unwind the whole call).
//! * [`Exit::Hlt`] at `gadget + 1` — the guest function `ret`ed into the gadget: the call
//!   returned with its result in `RAX`.
//! * anything else — a fatal condition, reported with RIP + a decode of the faulting
//!   bytes.

use std::cell::RefCell;
use std::sync::atomic::Ordering;
use std::sync::{Arc, OnceLock};

use tracing::{error, warn};
use x86jit_core::{AccessKind, CpuMode, Exit, Reg, Vcpu, disassemble};

use crate::context::NativeContext;
use crate::guest_vm::GuestVm;
use crate::profile;

/// Outcome of a top-level [`run_guest_call`].
#[derive(Debug)]
pub enum GuestExit {
    /// The guest function returned; payload is its `RAX`.
    Returned(u64),
    /// A syscall handler called [`request_thread_exit`]; payload is the exit value.
    ThreadExit(u64),
    /// An unrecoverable condition (unmapped access, unknown instruction, unexpected
    /// exit, …). The string carries RIP + context for diagnostics.
    Fatal(String),
}

/// Guest syscall dispatch callback: `(syscall_id, &mut NativeContext) -> return_value`.
///
/// Installed once via [`set_syscall_dispatch`] from the app's `main`. A global
/// `OnceLock` — rather than a direct `ps4-cpu -> ps4-libs` dependency — breaks what would
/// otherwise be a dependency cycle (libs depends on cpu for `NativeContext`).
pub type SyscallDispatch = fn(u64, &mut NativeContext) -> u64;

static DISPATCH: OnceLock<SyscallDispatch> = OnceLock::new();

/// Install the guest syscall dispatch callback. Idempotent-safe: a second call is
/// logged and ignored rather than panicking (the `OnceLock` set fails silently), so a
/// stray double-install can never abort the process.
pub fn set_syscall_dispatch(dispatch: SyscallDispatch) {
    if DISPATCH.set(dispatch).is_err() {
        warn!("set_syscall_dispatch called more than once; ignoring the later call");
    }
}

/// Whether the guest syscall dispatcher has been installed, for the composition root's
/// boot-time all-seams-wired assert (a missing dispatcher would otherwise surface only as a
/// fatal guest fault on the first `SYSCALL`).
pub fn syscall_dispatch_installed() -> bool {
    DISPATCH.get().is_some()
}

/// Fault-address annotator: `(guest_addr) -> human-readable VMA context`.
///
/// Installed once via [`set_fault_annotator`] from the app's `main`,
/// backed by the `VmMemoryManager`'s VMA map. A boxed closure in a global `OnceLock`
/// — rather than a `ps4-cpu -> ps4-memory` dependency — breaks a dependency cycle,
/// exactly like [`SyscallDispatch`]. It is used only to enrich a fatal
/// `UnmappedMemory` report; when unset the report simply omits VMA context.
type FaultAnnotator = Box<dyn Fn(u64) -> String + Send + Sync>;

static FAULT_ANNOTATOR: OnceLock<FaultAnnotator> = OnceLock::new();

/// Install the fault-address annotator (idempotent-safe, like [`set_syscall_dispatch`]).
/// The closure is called from the run loop when a guest access traps to an unmapped
/// address, to name the nearest VMA(s) for the diagnostic report.
pub fn set_fault_annotator(annotator: FaultAnnotator) {
    if FAULT_ANNOTATOR.set(annotator).is_err() {
        warn!("set_fault_annotator called more than once; ignoring the later call");
    }
}

/// Best-effort VMA context for `addr`, via the installed [`FaultAnnotator`]. Returns
/// an empty string when no annotator is installed (e.g. in `ps4-cpu`'s own tests).
pub(crate) fn fault_context(addr: u64) -> String {
    match FAULT_ANNOTATOR.get() {
        Some(annotate) => annotate(addr),
        None => String::new(),
    }
}

/// Environment variable enabling the guest-hang watchdog. When set to a
/// positive integer `N`, the run loop drives the vcpu with a block budget of `N`
/// instead of an unbounded one: each time the budget is exhausted it logs the current
/// RIP (rate-limited) and resumes, so a hung/looping guest surfaces a periodic RIP
/// trail for debugging. Unset (the default) → `None` budget → zero overhead.
pub const WATCHDOG_ENV: &str = "UNEMUPS4_WATCHDOG";

/// Resolve the watchdog block budget from [`WATCHDOG_ENV`]. `None` (unset / empty /
/// non-positive / unparseable) disables it entirely — the run loop then uses an
/// unbounded budget exactly as before, so there is no cost when the var is absent.
fn watchdog_budget() -> Option<u64> {
    match std::env::var(WATCHDOG_ENV).ok().as_deref() {
        None | Some("") => None,
        Some(v) => match v.parse::<u64>() {
            Ok(n) if n > 0 => Some(n),
            _ => {
                warn!("{WATCHDOG_ENV}={v:?} is not a positive integer; watchdog disabled");
                None
            }
        },
    }
}

/// Per-thread execution context, installed for the duration of a [`run_guest_call`] so a
/// nested [`call_guest`] (invoked from inside a syscall handler) can find the guest VM,
/// the current guest FS base, and a safe stack pointer.
struct ExecCtx {
    vm: Arc<GuestVm>,
    fs_base: u64,
    /// The guest RSP as of the most recent syscall exit — a live, in-use stack top a
    /// nested call can carve a fresh frame below.
    cur_rsp: u64,
    /// Set by [`request_thread_exit`]; consumed by the run loop to unwind as
    /// [`GuestExit::ThreadExit`].
    exit_requested: Option<u64>,
    /// Guest address of this thread's errno slot (inside the guest arena), so a syscall
    /// handler servicing `__error`/`errno` can hand the guest a dereferenceable pointer.
    errno_addr: u64,
}

thread_local! {
    static EXEC_CTX: RefCell<Option<ExecCtx>> = const { RefCell::new(None) };
}

/// Request that the current guest call unwind: the run loop returns
/// [`GuestExit::ThreadExit`] with `value` after the in-flight syscall completes.
/// Called from a syscall handler (e.g. `sce_pthread_exit`). No-op with a warning if
/// there is no active exec context.
pub fn request_thread_exit(value: u64) {
    EXEC_CTX.with(|c| {
        if let Some(ctx) = c.borrow_mut().as_mut() {
            ctx.exit_requested = Some(value);
        } else {
            warn!("request_thread_exit({value}) with no active guest call; ignored");
        }
    });
}

thread_local! {
    /// Guest RSP captured at the current syscall dispatch, for reading stack-passed args.
    static SYSCALL_RSP: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };

    /// Nesting depth of [`drive`] on this thread. A handler may re-enter guest code via
    /// [`call_guest`]; the inner loop's time is already inside the outer dispatch
    /// measurement, so only depth 0 feeds the per-frame budget.
    static DRIVE_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Read the `n`-th argument of the in-flight syscall (n >= 6, i.e. beyond the 6 register
/// args). SysV passes args 7+ on the stack; at the HLE stub's SYSCALL the callee frame is
/// `[rsp]` = return address, `[rsp+8]` = arg7, so arg `n` is at `rsp + 8 + (n-6)*8`. The
/// address is identity-mapped guest memory. Only valid while servicing a syscall.
pub fn syscall_stack_arg(n: usize) -> u64 {
    debug_assert!(n >= 6, "args 0..5 are in registers");
    let rsp = SYSCALL_RSP.with(|c| c.get());
    let addr = rsp.wrapping_add(8 + ((n - 6) as u64) * 8);
    // Only dereference an address inside the guest arena. `rsp` is 0 outside an active
    // syscall (the cell is never cleared), and a corrupt guest stack could point anywhere;
    // the JIT identity-maps guest addresses, so an unchecked deref would fault the host.
    // The arena is mapped `[GUEST_BASE, DEFAULT_SPAN)` — `guest_vm::reserve_at` maps
    // `len = span - guest_base` from `GUEST_BASE`, so the exclusive top is the span
    // itself, not `GUEST_BASE + span`. Validate the whole 8-byte read: `[addr, addr + 8)`
    // must lie inside the arena, so check the read's end, not just its start (`checked_add`
    // also rejects a wrapped-around address).
    let in_arena = addr >= crate::guest_vm::GUEST_BASE
        && addr
            .checked_add(8)
            .is_some_and(|end| end <= crate::guest_vm::DEFAULT_SPAN);
    if !in_arena {
        return 0;
    }
    unsafe { *(addr as *const u64) }
}

/// Guest address of the current thread's errno slot, or `None` when called outside an
/// active [`run_guest_call`] (no exec context installed). A syscall handler servicing
/// `__error`/`errno` returns this so the guest gets a dereferenceable guest pointer.
pub fn current_errno_addr() -> Option<u64> {
    EXEC_CTX.with(|c| c.borrow().as_ref().map(|ctx| ctx.errno_addr))
}

/// Write `val` into the current thread's errno slot (the guest-arena i32 that
/// `__error`/`errno` hands back). A syscall handler returning a POSIX failure code
/// (`-1`) must set this so the guest's libc — and Mono's OS-primitive wrappers, which
/// `g_error`/abort on an unexpected errno — read the right FreeBSD errno. No-op when
/// called outside an active guest call or if the slot is outside the arena.
pub fn set_errno(val: i32) {
    if let Some(addr) = current_errno_addr() {
        // The arena is mapped `[GUEST_BASE, DEFAULT_SPAN)` — `guest_vm::reserve_at` maps
        // `len = span - guest_base` from `GUEST_BASE`, so the exclusive top is the span
        // itself, not `GUEST_BASE + span`. The errno slot is a 4-byte i32, so the whole
        // `[addr, addr + 4)` write must lie inside the arena — validate the write's end,
        // not just its start (`checked_add` also rejects a wrapped-around address).
        let in_arena = addr >= crate::guest_vm::GUEST_BASE
            && addr
                .checked_add(4)
                .is_some_and(|end| end <= crate::guest_vm::DEFAULT_SPAN);
        if in_arena {
            // SAFETY: the errno slot is a fixed i32 in the identity-mapped guest arena,
            // range-checked above; the write is the guest's own errno, no SMC concern.
            unsafe { *(addr as *mut i32) = val };
        }
    }
}

/// Enter guest code at `entry` on a fresh vcpu and run to completion.
///
/// * `rsp` — initial guest stack pointer. The gadget return address is pushed at
///   `rsp - 8`, so the callee sees a 16-byte-aligned frame per the System V ABI
///   (caller is expected to pass a 16-aligned `rsp`).
/// * `rdi` — first argument (guest `RDI`).
/// * `fs_base` — guest TLS base installed directly into the Vcpu's `Reg::FsBase`; the
///   host FS is untouched, so Rust TLS in handlers keeps working.
/// * `errno_addr` — guest address of this thread's errno slot, exposed to syscall
///   handlers via [`current_errno_addr`] so `__error`/`errno` returns a guest pointer.
pub fn run_guest_call(
    vm: &Arc<GuestVm>,
    entry: u64,
    rsp: u64,
    rdi: u64,
    fs_base: u64,
    errno_addr: u64,
) -> GuestExit {
    // Install the thread-local exec context for the duration of this call so nested
    // `call_guest`s and `request_thread_exit` can find the VM / stack / exit flag.
    let prev = EXEC_CTX.with(|c| {
        c.borrow_mut().replace(ExecCtx {
            vm: Arc::clone(vm),
            fs_base,
            cur_rsp: rsp,
            exit_requested: None,
            errno_addr,
        })
    });

    let result = drive(vm, entry, rsp, rdi, fs_base);

    // Restore any outer context (nested run_guest_call is not expected, but be correct).
    EXEC_CTX.with(|c| {
        *c.borrow_mut() = prev;
    });

    result
}

/// A nested guest call from inside a syscall handler (TLS destructors, `pthread_once`,
/// etc.). Uses the thread-local exec context: same VM, same FS base, a
/// fresh vcpu on a stack carved 128 bytes below the current guest RSP (past the red
/// zone) and aligned down to 16. `arg` lands in guest `RDI`; the return value is the
/// callee's `RAX`.
///
/// Panics if there is no active exec context (i.e. called outside a `run_guest_call`) —
/// a programming error, since nested calls only make sense while servicing a guest.
pub fn call_guest(entry: u64, arg: u64) -> u64 {
    let (vm, fs_base, base_rsp) = EXEC_CTX.with(|c| {
        let borrow = c.borrow();
        let ctx = borrow
            .as_ref()
            .expect("call_guest outside of an active guest call (no exec context)");
        (Arc::clone(&ctx.vm), ctx.fs_base, ctx.cur_rsp)
    });

    // Carve a fresh frame below the current stack, clear of the red zone, 16-aligned.
    let nested_rsp = (base_rsp - 128) & !0xF;

    match drive(&vm, entry, nested_rsp, arg, fs_base) {
        GuestExit::Returned(rax) => rax,
        GuestExit::ThreadExit(value) => {
            // A thread-exit request raised during a nested call must propagate to the
            // enclosing run loop so it unwinds too. The inner `drive` already `take()`s
            // `exit_requested` (clearing it) when it returns `ThreadExit`, so re-arm the
            // flag here — otherwise the outer `drive`'s take sees `None`, never breaks
            // `ThreadExit`, and keeps executing dead guest code past the exit. Hand the
            // value back to the immediate caller as well.
            request_thread_exit(value);
            value
        }
        GuestExit::Fatal(msg) => {
            error!("nested call_guest to {entry:#x} failed: {msg}");
            0
        }
    }
}

/// The shared run loop: set up a fresh vcpu at (`entry`, `rsp-8`, `rdi`, `fs_base`) with
/// the gadget return address pushed, then drive `Vcpu::run` until the call resolves.
fn drive(vm: &Arc<GuestVm>, entry: u64, rsp: u64, rdi: u64, fs_base: u64) -> GuestExit {
    let gadget = vm.gadget_addr();

    // Push the gadget address as the return address at rsp-8. After the guest function
    // `ret`s, RIP == gadget and it executes the single `hlt`, landing us at gadget+1.
    let ret_slot = rsp - 8;
    if let Err(e) = vm.write_bytes(ret_slot, &gadget.to_le_bytes()) {
        return GuestExit::Fatal(format!(
            "failed to push gadget return address at {ret_slot:#x}: {e:?}"
        ));
    }

    let mut cpu = vm.new_vcpu();
    cpu.set_reg(Reg::Rip, entry);
    cpu.set_reg(Reg::Rsp, ret_slot);
    cpu.set_reg(Reg::Rdi, rdi);
    cpu.set_reg(Reg::FsBase, fs_base);

    // Watchdog: unless `UNEMUPS4_WATCHDOG=<blocks>` is set, `budget` is
    // `None` and `cpu.run` runs unbounded (the normal, zero-overhead path). With a
    // budget, `Exit::BudgetExhausted` is a *cooperative* return we resume from — used
    // only to periodically log RIP so a hung/looping guest is observable.
    let watchdog = watchdog_budget();
    // Per-thread execution tracer (task-170): when `UNEMUPS4_EXECTRACE` is set it also
    // needs a block budget so `Exit::BudgetExhausted` periodically yields RIP for the
    // per-thread RIP histogram. Fold it into the same `budget` the watchdog uses — the
    // watchdog's explicit budget wins when both are set. When neither is set `budget` is
    // `None` and `cpu.run` runs unbounded (the normal, zero-overhead path).
    // `UNEMUPS4_PROFILE_RIP` (task-215) wants the same cooperative return for a different
    // reason: to sample the guest RIP of the frame in flight so the samples of the frames
    // that turn out slow can be aggregated. Lowest precedence of the three.
    let budget = watchdog
        .or_else(ps4_core::exectrace::rip_budget)
        .or_else(profile::rip_budget);
    // Count of budget exhaustions, for rate-limiting the watchdog log.
    let mut watchdog_ticks: u64 = 0;

    // Resolved once so the disabled default path pays a single cached branch per loop:
    // the aggregate profiler and the per-thread execution tracer.
    let profiling = profile::enabled();
    let exectrace = ps4_core::exectrace::enabled();
    // This thread's guest tid, resolved once (constant for the life of the run loop). Used
    // to key the per-thread tracer views. Zero for the boot thread's own calls.
    let trace_tid = if exectrace {
        ps4_core::kernel::current_tid()
    } else {
        0
    };
    // Rate-limit the main-thread backtrace sample (view d) to roughly once per second of
    // wall time so it costs nothing steady-state.
    let mut last_bt = std::time::Instant::now();

    // Per-frame accounting (task-209) belongs to the OUTERMOST drive on this thread: a
    // nested `call_guest` from inside a handler runs its own loop whose time is already
    // inside that handler's dispatch measurement, so counting it again would
    // double-book the frame. The calibration stub is excluded for the same reason — it
    // is not part of any guest frame.
    let depth = DRIVE_DEPTH.with(|d| {
        let v = d.get();
        d.set(v + 1);
        v
    });
    let frame_acct = profiling && depth == 0 && !profile::calibration::active();
    let rip_sampling = frame_acct && profile::rip_sampling();

    // A rolling timestamp: every mark is both the end of one phase and the start of the
    // next, so the phases tile the whole loop with no gap between them — that is what
    // lets a frame's budget be checked against its measured wall time (task-209).
    let mut mark = profiling.then(std::time::Instant::now);

    // `fast_hits` is a running per-vcpu total, so we fold DELTAS and remember what has
    // already been folded. Sampling only at the exit below is what froze this counter
    // (task-218): the main guest thread runs the whole title inside one `run_guest_call`
    // and never reaches that exit, so its hits were never counted.
    let mut folded_fast_hits = 0u64;
    let mut folded_retired = 0u64;
    let mut folded_executed = 0u64;

    let result = loop {
        let exit = cpu.run(vm.vm(), budget);
        if let Some(prev) = mark {
            let now = std::time::Instant::now();
            let ns = now.duration_since(prev).as_nanos() as u64;
            mark = Some(now);
            profile::EXEC.guest_ns.fetch_add(ns, Ordering::Relaxed);
            profile::EXEC.run_slices.fetch_add(1, Ordering::Relaxed);
            if frame_acct {
                profile::frame_add_guest(ns);
            }
        }
        match exit {
            // Watchdog resume: the budget ran out mid-execution (only possible when
            // `budget` is `Some`). Log RIP every 64th tick (rate-limited so a tight
            // loop doesn't flood) and continue — this is NOT a fatal condition, so it
            // must not fall through to `format_fatal` and must not break the
            // Syscall/Hlt resolution below.
            Exit::BudgetExhausted if budget.is_some() => {
                if profiling {
                    profile::EXEC.exits_budget.fetch_add(1, Ordering::Relaxed);
                }
                // (b) per-thread RIP histogram: sample where this thread is spinning.
                if exectrace {
                    ps4_core::exectrace::record_rip(trace_tid, cpu.reg(Reg::Rip));
                }
                if rip_sampling {
                    profile::frame_record_rip(cpu.reg(Reg::Rip));
                }
                watchdog_ticks += 1;
                // Only the explicit watchdog logs the periodic RIP line; when the budget is
                // there purely for exectrace sampling this stays quiet (the tracer dumps).
                if watchdog.is_some() && watchdog_ticks.is_multiple_of(64) {
                    warn!(
                        "watchdog: guest still running after {} budget windows \
                         (rip {:#x}); resuming",
                        watchdog_ticks,
                        cpu.reg(Reg::Rip)
                    );
                }
                continue;
            }
            Exit::Syscall => {
                // Refresh the nested-call stack anchor from the live vcpu — the guest
                // may have pushed/popped since entry.
                let rsp_now = cpu.reg(Reg::Rsp);
                EXEC_CTX.with(|c| {
                    if let Some(ctx) = c.borrow_mut().as_mut() {
                        ctx.cur_rsp = rsp_now;
                    }
                });

                let id = cpu.reg(Reg::Rax);
                // (a) per-thread syscall histogram, and (d) a rate-limited guest backtrace
                // of the main thread (tid 1) captured at the syscall boundary, so the dump
                // shows where the per-frame loop body sits. Both env-gated.
                if exectrace {
                    ps4_core::exectrace::record_syscall(trace_tid, id);
                    if trace_tid == 1 && last_bt.elapsed() >= std::time::Duration::from_secs(1) {
                        last_bt = std::time::Instant::now();
                        ps4_core::exectrace::record_backtrace(trace_tid, guest_backtrace(vm, &cpu));
                    }
                }
                let mut ctx = native_context_from_vcpu(&cpu);
                // Expose the syscall-time guest RSP so a >6-arg handler can read
                // stack-passed args via `syscall_stack_arg`.
                SYSCALL_RSP.with(|c| c.set(ctx.rsp));

                // Per-thread HLE breadcrumb (task-113.2), always on: push the call before
                // dispatch so a handler that faults or wedges still leaves a record, and
                // patch the return value after. Register args in SysV order — `arg3` is
                // R10, not RCX, because the syscall lift clobbers RCX.
                let crumb = ps4_core::breadcrumb::record_call(
                    id,
                    [ctx.rdi, ctx.rsi, ctx.rdx, ctx.r10, ctx.r8, ctx.r9],
                );

                // The calibration stub's magic id is answered here, above the dispatcher,
                // so a loop iteration costs one bare VM exit/entry round trip and nothing
                // else. The guest never issues it (task-209).
                let calibrating =
                    id == profile::calibration::SYSCALL_ID && profile::calibration::active();
                let dispatch = if calibrating {
                    None
                } else {
                    match DISPATCH.get() {
                        Some(dispatch) => Some(dispatch),
                        None => {
                            break GuestExit::Fatal(format!(
                                "guest issued syscall {id:#x} but no dispatch fn is installed \
                                 (set_syscall_dispatch was never called)"
                            ));
                        }
                    }
                };
                if frame_acct {
                    profile::frame_syscall_enter(id);
                    // Fold this vcpu's IBTC hits once per guest frame, on its own thread
                    // (the counter is per-vcpu and not atomic in x86jit). A frame boundary
                    // is a point the main guest thread actually reaches, unlike the exit
                    // path below (task-218).
                    if profile::is_frame_boundary(id) {
                        let total = cpu.fast_hits();
                        profile::EXEC
                            .vcpu_fast_hits
                            .fetch_add(total - folded_fast_hits, Ordering::Relaxed);
                        folded_fast_hits = total;
                        // Guest instructions actually executed (task-220): the one axis
                        // every other counter here lacks, since all of them measure time.
                        let retired = cpu.retired_instructions();
                        profile::EXEC
                            .vcpu_retired
                            .fetch_add(retired - folded_retired, Ordering::Relaxed);
                        folded_retired = retired;
                        // Compiled + interpreted, unlike `retired_instructions` above
                        // (x86jit task-281). Zero unless `enable_icount` was set at boot.
                        let executed = cpu.executed_instructions();
                        let d_exec = executed - folded_executed;
                        folded_executed = executed;
                        profile::EXEC
                            .vcpu_executed
                            .fetch_add(d_exec, Ordering::Relaxed);
                        // Also into the per-frame stats, where `frames` and `guest_ns` are
                        // accumulated, so instructions-per-frame and MIPS are derived from
                        // counters sampled at the same boundary rather than from a
                        // process-wide one read at dump time.
                        profile::frame_add_executed(d_exec);
                    }
                }
                if let Some(prev) = mark {
                    let now = std::time::Instant::now();
                    let ns = now.duration_since(prev).as_nanos() as u64;
                    mark = Some(now);
                    profile::EXEC
                        .pre_dispatch_ns
                        .fetch_add(ns, Ordering::Relaxed);
                    if frame_acct {
                        profile::frame_add_loop(ns);
                    }
                }
                let ret = if profiling {
                    let tid = ps4_core::kernel::current_tid();
                    profile::syscall_enter(tid, id);
                    let ret = dispatch.map_or(0, |d| d(id, &mut ctx));
                    profile::syscall_exit(tid);
                    let now = std::time::Instant::now();
                    let ns = mark.map_or(0, |prev| now.duration_since(prev).as_nanos() as u64);
                    mark = Some(now);
                    profile::EXEC.syscall_ns.fetch_add(ns, Ordering::Relaxed);
                    profile::EXEC.syscall_count.fetch_add(1, Ordering::Relaxed);
                    profile::record_syscall(id, ns);
                    if frame_acct {
                        profile::frame_add_syscall(id, ns);
                    }
                    ret
                } else {
                    dispatch.map_or(0, |d| d(id, &mut ctx))
                };
                ps4_core::breadcrumb::record_return(crumb, ret);
                cpu.set_reg(Reg::Rax, ret);

                // A handler may have requested a thread exit — unwind now.
                let requested = EXEC_CTX.with(|c| {
                    c.borrow_mut()
                        .as_mut()
                        .and_then(|ctx| ctx.exit_requested.take())
                });
                if let Some(value) = requested {
                    break GuestExit::ThreadExit(value);
                }
                if let Some(prev) = mark {
                    let now = std::time::Instant::now();
                    let ns = now.duration_since(prev).as_nanos() as u64;
                    mark = Some(now);
                    profile::EXEC
                        .post_dispatch_ns
                        .fetch_add(ns, Ordering::Relaxed);
                    if frame_acct {
                        profile::frame_add_loop(ns);
                    }
                }
            }
            Exit::Hlt => {
                let rip = cpu.reg(Reg::Rip);
                if rip == gadget + 1 {
                    if profiling {
                        profile::EXEC.exits_hlt.fetch_add(1, Ordering::Relaxed);
                    }
                    break GuestExit::Returned(cpu.reg(Reg::Rax));
                }
                if profiling {
                    profile::EXEC.exits_fatal.fetch_add(1, Ordering::Relaxed);
                }
                break GuestExit::Fatal(format!(
                    "unexpected HLT at {rip:#x} (not the return gadget at {:#x})",
                    gadget + 1
                ));
            }
            other => {
                if profiling {
                    profile::EXEC.exits_fatal.fetch_add(1, Ordering::Relaxed);
                }
                // Report through `tracing::error!` (matching the existing log style) and
                // carry the same content out as `GuestExit::Fatal`.
                let report = format_fatal(vm, &cpu, &other);
                error!("{report}");
                break GuestExit::Fatal(report);
            }
        }
    };

    DRIVE_DEPTH.with(|d| d.set(d.get().saturating_sub(1)));

    // Fold whatever this vcpu accrued since the last frame-boundary fold. Still needed
    // even with that fold in place: a thread that never hits a frame boundary (a worker,
    // a TLS destructor, a nested `call_guest`) reaches only this path, and the flipping
    // thread has hits after its final boundary. Reading the running total and subtracting
    // what was already folded is what keeps the two paths from double counting.
    if profiling {
        profile::EXEC.vcpu_fast_hits.fetch_add(
            cpu.fast_hits().saturating_sub(folded_fast_hits),
            Ordering::Relaxed,
        );
        profile::EXEC.vcpu_retired.fetch_add(
            cpu.retired_instructions().saturating_sub(folded_retired),
            Ordering::Relaxed,
        );
        profile::EXEC.vcpu_executed.fetch_add(
            cpu.executed_instructions().saturating_sub(folded_executed),
            Ordering::Relaxed,
        );
    }

    result
}

/// Measure one guest VM exit/entry round trip directly (task-209).
///
/// Writes a tiny guest stub — `mov eax, ID; syscall; dec rdi; jnz; ret` — into a scratch
/// page and runs it [`profile::calibration::ITERATIONS`] times. The run loop answers the
/// stub's magic id itself, so a loop iteration is a bare exit and re-entry: the wall time
/// per iteration IS the round trip. Also times a bare `Instant::now()` so the profiler's
/// own four clock reads per syscall can be subtracted.
///
/// No-op unless [`profile::enabled`]. Must run before the executable is loaded — the
/// scratch page is only guaranteed free while nothing but the HLT gadget is mapped.
pub fn calibrate_vm_exit(vm: &Arc<GuestVm>) {
    if !profile::enabled() {
        return;
    }
    const CLOCK_SAMPLES: u64 = 200_000;

    let id = profile::calibration::SYSCALL_ID as u32;
    let mut stub = vec![0xB8];
    stub.extend(id.to_le_bytes()); // mov eax, ID
    stub.extend([0x0F, 0x05]); // syscall
    stub.extend([0x48, 0xFF, 0xCF]); // dec rdi
    stub.extend([0x75, 0xF4]); // jnz -12 (back to the mov)
    stub.push(0xC3); // ret
    if let Err(e) = vm.write_bytes(crate::guest_vm::CALIBRATION_CODE_ADDR, &stub) {
        warn!("vm-exit calibration: stub write failed ({e:?}); skipping");
        return;
    }

    // The stub's traffic must not enter the aggregate — restore every counter it moves.
    let before = profile::snapshot();
    profile::calibration::ACTIVE.store(true, Ordering::Relaxed);
    let t = std::time::Instant::now();
    let exit = run_guest_call(
        vm,
        crate::guest_vm::CALIBRATION_CODE_ADDR,
        crate::guest_vm::CALIBRATION_STACK_ADDR,
        profile::calibration::ITERATIONS,
        0,
        0,
    );
    let wall_ns = t.elapsed().as_nanos() as f64;
    profile::calibration::ACTIVE.store(false, Ordering::Relaxed);
    profile::restore(before);
    profile::forget_syscall(profile::calibration::SYSCALL_ID);

    if !matches!(exit, GuestExit::Returned(_)) {
        warn!("vm-exit calibration: stub did not return cleanly ({exit:?}); skipping");
        return;
    }

    let t = std::time::Instant::now();
    let mut last = t;
    for _ in 0..CLOCK_SAMPLES {
        last = std::time::Instant::now();
    }
    let clock_ns = t.elapsed().as_nanos() as f64 / CLOCK_SAMPLES as f64;
    let _ = last;

    let round_trip = wall_ns / profile::calibration::ITERATIONS as f64;
    profile::calibration::publish(round_trip, clock_ns);
}

/// Marshal the 15 guest GPRs into a [`NativeContext`]. The field order matches what every
/// existing handler expects. The x86jit `syscall` lift is hardware-correct: it clobbers
/// RCX (<-RIP) and R11 (<-RFLAGS), so the 4th call-ABI arg (RCX) can't survive the trap.
/// The syscall stubs copy it into R10 first, so `arg3()` reads R10.
fn native_context_from_vcpu(cpu: &Vcpu) -> NativeContext {
    NativeContext {
        r15: cpu.reg(Reg::R15),
        r14: cpu.reg(Reg::R14),
        r13: cpu.reg(Reg::R13),
        r12: cpu.reg(Reg::R12),
        r11: cpu.reg(Reg::R11),
        r10: cpu.reg(Reg::R10),
        r9: cpu.reg(Reg::R9),
        r8: cpu.reg(Reg::R8),
        rdi: cpu.reg(Reg::Rdi),
        rsi: cpu.reg(Reg::Rsi),
        rdx: cpu.reg(Reg::Rdx),
        rcx: cpu.reg(Reg::Rcx),
        rbp: cpu.reg(Reg::Rbp),
        rbx: cpu.reg(Reg::Rbx),
        rax: cpu.reg(Reg::Rax),
        rsp: cpu.reg(Reg::Rsp),
    }
}

/// Build a multi-line, actionable diagnostic for a fatal exit. Each variant
/// is turned into a report a human can act on directly: the faulting RIP, access kind,
/// nearest VMA context, an exception's signal-style name, or a ready-to-file
/// disassembly + byte string for an unimplemented lift.
///
/// The guest backtrace says *where* it died; the HLE breadcrumb ring says what the guest
/// last asked us for and what we answered — in an HLE emulator a null deref inside guest
/// libc is usually one of our handlers returning `0` for a pointer (task-113.2). This runs
/// on the faulting thread, so `breadcrumb::dump` reads that thread's own ring.
fn format_fatal(vm: &Arc<GuestVm>, cpu: &Vcpu, exit: &Exit) -> String {
    format!(
        "{}{}{}",
        format_fatal_head(vm, cpu, exit),
        guest_backtrace(vm, cpu),
        ps4_core::breadcrumb::dump()
    )
}

/// Walk the guest frame-pointer chain from the current RBP for a shallow backtrace.
/// Best-effort: the PS4 CRT/libc keep RBP frame chains, so `*rbp` is the caller's RBP
/// and `*(rbp+8)` its return address. Stops at the first unreadable/misaligned link, a
/// non-increasing RBP (chain corruption / top of stack), or [`MAX_FRAMES`]. Each return
/// address is attributed through the VMA annotator so the calling module is visible —
/// invaluable for a deliberate guest abort (e.g. libc `int 0x44`) where the RIP alone
/// only names the trap, not who invoked it. Empty when RBP isn't a readable frame.
fn guest_backtrace(vm: &Arc<GuestVm>, cpu: &Vcpu) -> String {
    const MAX_FRAMES: usize = 12;
    let mut rbp = cpu.reg(Reg::Rbp);
    let mut frames = String::new();
    for i in 0..MAX_FRAMES {
        if rbp == 0 || !rbp.is_multiple_of(8) {
            break;
        }
        let mut buf = [0u8; 16];
        if vm.read_bytes(rbp, &mut buf).is_err() {
            break;
        }
        let saved_rbp = u64::from_le_bytes(buf[0..8].try_into().unwrap());
        let ret = u64::from_le_bytes(buf[8..16].try_into().unwrap());
        if ret == 0 {
            break;
        }
        let ctx = fault_context(ret);
        let tag = ctx
            .lines()
            .next()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| format!("  [{s}]"))
            .unwrap_or_default();
        frames.push_str(&format!("\n\t  #{i} ret {ret:#x}{tag}"));
        // Frames climb toward higher addresses (stack grows down); a non-increasing link
        // means the chain is corrupt or we've reached the outermost frame.
        if saved_rbp <= rbp {
            break;
        }
        rbp = saved_rbp;
    }
    if frames.is_empty() {
        frames
    } else {
        format!("\n\tguest backtrace (rbp chain):{frames}")
    }
}

fn format_fatal_head(vm: &Arc<GuestVm>, cpu: &Vcpu, exit: &Exit) -> String {
    let rip = cpu.reg(Reg::Rip);
    match exit {
        Exit::UnknownInstruction { addr, bytes, len } => {
            report_unknown_instruction(vm, *addr, bytes, *len, rip)
        }
        Exit::UnmappedMemory { addr, access } => report_unmapped_memory(vm, *addr, *access, rip),
        Exit::Exception { addr, vector } => report_exception(vm, *addr, *vector, rip),
        Exit::MmioRead { addr, size } => {
            format!(
                "unexpected MmioRead size {size} at {addr:#x} (rip {rip:#x}) — the guest \
                     touched an MMIO/Trap region, but no MMIO handling is wired"
            )
        }
        Exit::MmioWrite { addr, size, value } => {
            format!(
                "unexpected MmioWrite size {size} value {value:#x} at {addr:#x} (rip \
                     {rip:#x}) — the guest touched an MMIO/Trap region, but no MMIO handling \
                     is wired"
            )
        }
        Exit::BudgetExhausted => {
            // Only reachable when the watchdog is OFF (budget = None); with the watchdog on,
            // BudgetExhausted is handled cooperatively in the run loop and never lands here.
            format!(
                "BudgetExhausted at rip {rip:#x} (unexpected: the run loop uses an \
                     unbounded budget unless {WATCHDOG_ENV} is set)"
            )
        }
        Exit::PortIo {
            port,
            size,
            dir,
            value,
        } => {
            format!(
                "unexpected PortIo {dir:?} port {port:#x} size {size} value {value:#x} (rip \
                     {rip:#x}) — the guest executed an in/out instruction, but this HLE \
                     emulator wires no port-I/O handling"
            )
        }
        Exit::Syscall | Exit::Hlt => {
            format!("unhandled exit {exit:?} at rip {rip:#x}")
        }
    }
}

/// An access to an unmapped guest address: report the faulting RIP, the access kind
/// (read/write/exec), the address, the disassembly of the faulting instruction, and
/// the nearest-VMA context supplied by the installed fault annotator.
fn report_unmapped_memory(vm: &Arc<GuestVm>, addr: u64, access: AccessKind, rip: u64) -> String {
    let kind = match access {
        AccessKind::Read => "read",
        AccessKind::Write => "write",
        AccessKind::Execute => "exec",
    };
    let mut out = format!(
        "guest fault: UnmappedMemory ({kind}) of {addr:#x}\n\
         \tfaulting instruction: rip {rip:#x}{}",
        insn_at(vm, rip)
    );
    // Attribute the faulting CODE, not just the faulting address. For the common
    // null/low-address deref the address is `0` and describing *it* says only "below
    // guest_base"; what names the culprit is which module and function RIP sits in — the
    // same treatment every backtrace frame gets.
    let code_ctx = fault_context(rip);
    if !code_ctx.is_empty() {
        out.push_str(&format!("\n\tfaulting code: {code_ctx}"));
    }
    let ctx = fault_context(addr);
    if !ctx.is_empty() {
        out.push_str(&format!("\n\tfaulting address: {ctx}"));
    }
    out
}

/// A guest CPU exception: map the x86 vector to its mnemonic (`#UD`, `#GP`, …) and the
/// signal an HLE kernel would raise for it (SIGILL/SIGSEGV/…).
///
/// `addr` is x86jit's saved RIP, which follows the fault/trap convention:
/// for a **fault** (`#UD`, `#DE`, …) it sits ON the faulting instruction, so
/// disassembling there names it directly. For a **trap** (`#BP`/int3, `#DB`/int1) the
/// saved RIP resumes *past* the instruction, so a forward disassembly at `addr` would
/// show the WRONG (next) instruction. For traps we back off by the known 1-byte length
/// of int3/int1 to disassemble the actual trapping instruction, and annotate that the
/// reported RIP is the after-instruction resume address.
fn report_exception(vm: &Arc<GuestVm>, addr: u64, vector: u8, rip: u64) -> String {
    let (mnemonic, name, signal) = exception_names(vector);
    if is_trap_vector(vector) {
        // Trap: `addr`/`rip` point just past the 1-byte int3 (#BP) / int1 (#DB); back up
        // one byte to name the instruction that actually trapped.
        let insn_addr = rip.wrapping_sub(1);
        format!(
            "guest fault: Exception vector {vector} ({mnemonic} — {name}, {signal}) at {addr:#x}\n\
             \ttrap — RIP is the resume address AFTER the instruction\n\
             \ttrapping instruction: rip {insn_addr:#x}{}{}",
            insn_at(vm, insn_addr),
            guest_hexdump(vm, insn_addr, 16),
        )
    } else {
        format!(
            "guest fault: Exception vector {vector} ({mnemonic} — {name}, {signal}) at {addr:#x}\n\
             \tfaulting instruction: rip {rip:#x}{}{}",
            insn_at(vm, rip),
            guest_hexdump(vm, rip, 16),
        )
    }
}

/// Is `vector` a trap (as opposed to a fault)? For the vectors x86jit can currently
/// surface via `Exit::Exception`, the traps are `#BP` (3, int3) and `#DB` (1, int1):
/// their saved RIP resumes *past* the instruction. Faults (`#DE` 0,
/// `#UD` 6, …) leave RIP on the instruction. Kept in sync with [`exception_names`].
fn is_trap_vector(vector: u8) -> bool {
    matches!(vector, 1 | 3)
}

/// An instruction the x86jit lift does not implement: dump the surrounding bytes with a
/// disassembly window and a ready-to-paste byte string for filing an x86jit backlog
/// task.
fn report_unknown_instruction(
    vm: &Arc<GuestVm>,
    addr: u64,
    bytes: &[u8; 15],
    len: u8,
    rip: u64,
) -> String {
    let len = (len as usize).min(bytes.len());
    let faulting = &bytes[..len.max(1)];
    let hex = |b: &[u8]| {
        b.iter()
            .map(|x| format!("{x:02x}"))
            .collect::<Vec<_>>()
            .join(" ")
    };

    // The faulting instruction, disassembled from the engine-supplied bytes. The PS4
    // guest is a 64-bit PIE, so decode in `CpuMode::Long64`.
    let disasm = disassemble(faulting, addr, CpuMode::Long64)
        .into_iter()
        .map(|d| d.text)
        .collect::<Vec<_>>()
        .join("; ");

    format!(
        "guest fault: UnknownInstruction at {addr:#x} (rip {rip:#x})\n\
         \tunimplemented lift in x86jit for: {disasm}\n\
         \tfaulting bytes: [{}]{}\n\
         \tACTION: file a task in the x86jit backlog to lift this opcode, with the bytes:\n\
         \t        {}",
        hex(faulting),
        // Also dump straight from guest memory at the faulting VA: the engine only ships the
        // bytes it managed to consume, so a wider window names any prefix/suffix the decoder
        // dropped and gives the lift task the raw encoding.
        guest_hexdump(vm, addr, 16),
        hex(faulting),
    )
}

/// Map an x86 exception `vector` to its `(mnemonic, description, signal-style name)`.
/// Verified against x86jit's `Exit::Exception` semantics: the engine emits vector 0
/// (`#DE`, div) and — with the lift additions — 6 (`#UD`, ud2), 3 (`#BP`,
/// int3), 1 (`#DB`, int1). The rest are listed so an unexpected vector still names
/// itself rather than printing a bare number.
fn exception_names(vector: u8) -> (&'static str, &'static str, &'static str) {
    match vector {
        0 => ("#DE", "divide error", "SIGFPE"),
        1 => ("#DB", "debug", "SIGTRAP"),
        3 => ("#BP", "breakpoint", "SIGTRAP"),
        4 => ("#OF", "overflow", "SIGSEGV"),
        5 => ("#BR", "bound range exceeded", "SIGSEGV"),
        6 => ("#UD", "invalid opcode", "SIGILL"),
        7 => ("#NM", "device not available", "SIGFPE"),
        8 => ("#DF", "double fault", "SIGSEGV"),
        13 => ("#GP", "general protection", "SIGSEGV"),
        14 => ("#PF", "page fault", "SIGSEGV"),
        16 => ("#MF", "x87 floating-point", "SIGFPE"),
        17 => ("#AC", "alignment check", "SIGBUS"),
        19 => ("#XM", "SIMD floating-point", "SIGFPE"),
        _ => ("#??", "unknown exception vector", "SIGILL"),
    }
}

/// Best-effort disassembly of the instruction at `rip` for a fatal report, formatted as
/// a trailing `" (mov …)"`. Empty when the bytes can't be read.
fn insn_at(vm: &Arc<GuestVm>, rip: u64) -> String {
    let mut buf = [0u8; 15];
    if vm.read_bytes(rip, &mut buf).is_ok()
        && let Some(insn) = disassemble(&buf, rip, CpuMode::Long64).into_iter().next()
    {
        return format!(" ({})", insn.text);
    }
    String::new()
}

/// A ready-to-paste hexdump of up to `n` bytes read straight from guest memory at `addr`,
/// prefixed with the VA and length. The guest is identity-mapped (guest addr == host addr),
/// so at a fault the faulting RIP is directly readable; this names the exact opcode bytes an
/// x86jit lift task needs even when the engine's own decoder shipped nothing (an
/// `UnknownInstruction` only carries the bytes it managed to consume; an `Exception` — e.g. a
/// bad/synthetic vector — carries none). Returns an empty string when the memory can't be read.
/// See task-113.2 / task-144.
fn guest_hexdump(vm: &Arc<GuestVm>, addr: u64, n: usize) -> String {
    let n = n.min(32);
    let mut buf = [0u8; 32];
    let slice = &mut buf[..n];
    if vm.read_bytes(addr, slice).is_err() {
        return String::new();
    }
    let hex = slice
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(" ");
    format!("\n\tguest bytes @ {addr:#x} ({n}B): [{hex}]")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guest_vm::DEFAULT_SPAN;

    /// `syscall_stack_arg` must reject any read whose 8-byte window is not wholly inside
    /// the mapped arena `[GUEST_BASE, DEFAULT_SPAN)`, returning 0 instead of dereferencing
    /// unmapped host memory. All cases below stay out of the arena, so the guard's
    /// return-0 path is the only one exercised (no arena is mapped in the test process).
    #[test]
    fn syscall_stack_arg_rejects_out_of_arena_reads() {
        // With `n == 6`, addr == rsp + 8, so `rsp` sets `addr` directly.
        // Below the arena floor: addr == 8 < GUEST_BASE.
        SYSCALL_RSP.with(|c| c.set(0));
        assert_eq!(syscall_stack_arg(6), 0, "addr below GUEST_BASE");

        // Just above the mapped top: addr == DEFAULT_SPAN. This is the historic
        // false-positive — it fell inside the too-high `[GUEST_BASE, GUEST_BASE+span)`
        // bound and would have dereferenced unmapped memory.
        SYSCALL_RSP.with(|c| c.set(DEFAULT_SPAN - 8));
        assert_eq!(syscall_stack_arg(6), 0, "addr at the arena top");

        // Straddling the top: addr == DEFAULT_SPAN - 4, so `[addr, addr+8)` crosses the
        // mapped top even though `addr` itself is in range.
        SYSCALL_RSP.with(|c| c.set(DEFAULT_SPAN - 12));
        assert_eq!(syscall_stack_arg(6), 0, "read straddles the arena top");

        // Restore the cell so no later same-thread use sees a stale rsp.
        SYSCALL_RSP.with(|c| c.set(0));
    }
}
