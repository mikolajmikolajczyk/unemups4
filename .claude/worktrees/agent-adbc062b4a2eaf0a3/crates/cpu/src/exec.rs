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
//!   bytes (doc-1 dec 3).

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
fn fault_context(addr: u64) -> String {
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
    let arena =
        crate::guest_vm::GUEST_BASE..crate::guest_vm::GUEST_BASE + crate::guest_vm::DEFAULT_SPAN;
    if !arena.contains(&addr) {
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

/// Enter guest code at `entry` on a fresh vcpu and run to completion.
///
/// * `rsp` — initial guest stack pointer. The gadget return address is pushed at
///   `rsp - 8`, so the callee sees a 16-byte-aligned frame per the System V ABI
///   (caller is expected to pass a 16-aligned `rsp`).
/// * `rdi` — first argument (guest `RDI`).
/// * `fs_base` — guest TLS base installed directly into the Vcpu's `Reg::FsBase`; the
///   host FS is untouched, so Rust TLS in handlers keeps working (doc-1 dec 4).
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
/// etc. — doc-1 dec 3). Uses the thread-local exec context: same VM, same FS base, a
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
            // A thread-exit request raised during a nested call propagates: leave the
            // flag set so the outer run loop also unwinds, and hand the value back.
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
    let budget = watchdog_budget();
    // Count of budget exhaustions, for rate-limiting the watchdog log.
    let mut watchdog_ticks: u64 = 0;

    // Aggregate profiler: resolved once here so the disabled default path pays
    // a single cached branch per loop iteration and never touches the atomics. When on,
    // we bracket `cpu.run` and `dispatch` with `Instant`s and fold the per-slice / per-
    // syscall counters into the process-wide `profile::EXEC` totals.
    let profiling = profile::enabled();

    // The loop resolves the call by `break`ing with the `GuestExit`, so `fast_hits`
    // accumulation happens once at the single exit below rather than at every return.
    let result = loop {
        let slice_start = profiling.then(std::time::Instant::now);
        let exit = cpu.run(vm.vm(), budget);
        if let Some(start) = slice_start {
            profile::EXEC
                .guest_ns
                .fetch_add(start.elapsed().as_nanos() as u64, Ordering::Relaxed);
            profile::EXEC.run_slices.fetch_add(1, Ordering::Relaxed);
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
                watchdog_ticks += 1;
                if watchdog_ticks.is_multiple_of(64) {
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
                let mut ctx = native_context_from_vcpu(&cpu);
                // Expose the syscall-time guest RSP so a >6-arg handler can read
                // stack-passed args via `syscall_stack_arg`.
                SYSCALL_RSP.with(|c| c.set(ctx.rsp));

                let dispatch = match DISPATCH.get() {
                    Some(dispatch) => dispatch,
                    None => {
                        break GuestExit::Fatal(format!(
                            "guest issued syscall {id:#x} but no dispatch fn is installed \
                             (set_syscall_dispatch was never called)"
                        ));
                    }
                };
                let ret = if profiling {
                    let start = std::time::Instant::now();
                    let ret = dispatch(id, &mut ctx);
                    let ns = start.elapsed().as_nanos() as u64;
                    profile::EXEC.syscall_ns.fetch_add(ns, Ordering::Relaxed);
                    profile::EXEC.syscall_count.fetch_add(1, Ordering::Relaxed);
                    profile::record_syscall(id, ns);
                    ret
                } else {
                    dispatch(id, &mut ctx)
                };
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

    // Fold this vcpu's fast-resolve cache hits into the process total once, on the way
    // out (the counter is per-vcpu and not atomic in x86jit, so it's read here).
    if profiling {
        profile::EXEC
            .vcpu_fast_hits
            .fetch_add(cpu.fast_hits(), Ordering::Relaxed);
    }

    result
}

/// Marshal the 15 guest GPRs into a [`NativeContext`]. The field order matches what every
/// existing handler expects. The x86jit `syscall` lift is hardware-correct: it clobbers
/// RCX (<-RIP) and R11 (<-RFLAGS), so the 4th call-ABI arg (RCX) can't survive the trap.
/// The syscall stubs copy it into R10 first, so `arg3()` reads R10 (doc-1 dec 2).
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
fn format_fatal(vm: &Arc<GuestVm>, cpu: &Vcpu, exit: &Exit) -> String {
    format!(
        "{}{}",
        format_fatal_head(vm, cpu, exit),
        guest_backtrace(vm, cpu)
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
            report_unknown_instruction(*addr, bytes, *len, rip)
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
    let ctx = fault_context(addr);
    if !ctx.is_empty() {
        out.push_str(&format!("\n\tVMA context: {ctx}"));
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
             \ttrapping instruction: rip {insn_addr:#x}{}",
            insn_at(vm, insn_addr)
        )
    } else {
        format!(
            "guest fault: Exception vector {vector} ({mnemonic} — {name}, {signal}) at {addr:#x}\n\
             \tfaulting instruction: rip {rip:#x}{}",
            insn_at(vm, rip)
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
fn report_unknown_instruction(addr: u64, bytes: &[u8; 15], len: u8, rip: u64) -> String {
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
         \tfaulting bytes: [{}]\n\
         \tACTION: file a task in the x86jit backlog to lift this opcode, with the bytes:\n\
         \t        {}",
        hex(faulting),
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
