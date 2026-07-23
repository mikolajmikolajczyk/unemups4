//! Acceptance tests for the x86jit-backed execution core.
//!
//! Guest code is hand-assembled (raw bytes written straight into guest memory — no ELF)
//! and run through the public `ps4_cpu` API. All tests share one process-global syscall
//! dispatch (installed once via `set_syscall_dispatch`, an `OnceLock`); the dispatch fn
//! routes on the syscall id so each test drives an isolated behavior.

use std::sync::{Mutex, OnceLock};

use ps4_cpu::{
    GuestExit, GuestVm, NativeContext, call_guest, request_thread_exit, run_guest_call,
    set_fault_annotator, set_syscall_dispatch,
};

// --- Layout of the hand-assembled guest programs -----------------------------------

const SPAN: u64 = 0x0080_0000; // 8 MiB arena — plenty for these tiny programs
const CODE: u64 = 0x0040_0000; // entry function
const STUB: u64 = 0x0041_0000; // MOV EAX,id; SYSCALL; RET stub
const INNER: u64 = 0x0042_0000; // nested-call target for AC (c)
const STACK_TOP: u64 = 0x0050_0000; // 16-aligned guest stack top

// Syscall ids the test dispatch routes on.
const SYS_ARGS: u64 = 100; // AC (b): capture all six args, return a sentinel
const SYS_NESTED: u64 = 101; // AC (c): call_guest into INNER and return its result
const SYS_EXIT: u64 = 102; // AC (d): request_thread_exit(7)
const SYS_ONCE: u64 = 103; // run the init routine once via call_guest (pthread_once shape)
const SYS_ARGS64: u64 = 104; // contract test: capture all six args with full 64-bit sentinels

const ARGS_RET: u64 = 0xDEAD_BEEF; // value SYS_ARGS returns into guest RAX

/// Records the arguments the dispatch fn saw for SYS_ARGS (AC (b)).
#[derive(Default, Clone, Copy)]
struct Captured {
    id: u64,
    rdi: u64,
    rsi: u64,
    rdx: u64,
    rcx: u64,
    r8: u64,
    r9: u64,
}

static CAPTURED: OnceLock<Mutex<Captured>> = OnceLock::new();

fn captured() -> &'static Mutex<Captured> {
    CAPTURED.get_or_init(|| Mutex::new(Captured::default()))
}

/// Records the six args the dispatch saw for SYS_ARGS64 (full 64-bit sentinels).
static CAPTURED64: OnceLock<Mutex<[u64; 6]>> = OnceLock::new();

fn captured64() -> &'static Mutex<[u64; 6]> {
    CAPTURED64.get_or_init(|| Mutex::new([0; 6]))
}

/// Counts how many times SYS_ONCE's guarded init routine actually ran (the
/// `scePthreadOnce` shape — nested `call_guest` guarded so `once_init` runs exactly once).
static ONCE_RUNS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
/// The `pthread_once`-style guard: 0 = not run, 2 = done.
static ONCE_GUARD: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// The single process-global dispatch fn (installed once). Routes on the syscall id.
fn dispatch(id: u64, ctx: &mut NativeContext) -> u64 {
    match id {
        SYS_ARGS => {
            *captured().lock().unwrap() = Captured {
                id,
                rdi: ctx.arg0(),
                rsi: ctx.arg1(),
                rdx: ctx.arg2(),
                rcx: ctx.arg3(), // 4th arg — carried in R10 across the SYSCALL (doc-1 dec 2)
                r8: ctx.arg4(),
                r9: ctx.arg5(),
            };
            ARGS_RET
        }
        SYS_ARGS64 => {
            *captured64().lock().unwrap() = [
                ctx.arg0(),
                ctx.arg1(),
                ctx.arg2(),
                ctx.arg3(),
                ctx.arg4(),
                ctx.arg5(),
            ];
            0
        }
        SYS_NESTED => {
            // Nested guest call from inside a handler; return its result to the guest.
            call_guest(INNER, 0)
        }
        SYS_EXIT => {
            request_thread_exit(7);
            0
        }
        SYS_ONCE => {
            // Mirror `sce_pthread_once` (libkernel/pthread.rs): guard on an atomic so the
            // nested `call_guest` init routine runs exactly once even under concurrent
            // callers. This is the pthread_once machinery exercised at the ps4-cpu level.
            use std::sync::atomic::Ordering;
            if ONCE_GUARD
                .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                ONCE_RUNS.fetch_add(1, Ordering::Release);
                // Nested guest call from inside this handler (runs INNER -> 99).
                let inner = call_guest(INNER, 0);
                ONCE_GUARD.store(2, Ordering::Release);
                inner
            } else {
                while ONCE_GUARD.load(Ordering::Acquire) != 2 {
                    std::hint::spin_loop();
                }
                99
            }
        }
        other => panic!("unexpected syscall id {other}"),
    }
}

/// Install the shared dispatch once (idempotent across tests).
fn ensure_dispatch() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| set_syscall_dispatch(dispatch));
}

/// The guest arena's low cutoff (`GUEST_BASE`); nothing is mapped in `[0, GUEST_BASE)`
/// so any access below it is a null / low-address fault.
const GUEST_BASE: u64 = 0x10000;

/// Install a fault annotator once. In the real emulator this is backed by the
/// `VmMemoryManager`'s VMA map (installed from app `main`); here a stand-in reproduces
/// the "below guest_base" classification so the diagnostics plumbing is exercised
/// end-to-end from the run loop through `GuestExit::Fatal`.
fn ensure_annotator() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        set_fault_annotator(Box::new(|addr| {
            if addr < GUEST_BASE {
                format!(
                    "address {addr:#x} is below guest_base ({GUEST_BASE:#x}) — a null / \
                     low-address dereference"
                )
            } else {
                format!("address {addr:#x} is inside the arena")
            }
        }));
    });
}

/// Serializes VM construction: `GuestVm::new` does a `MAP_FIXED_NOREPLACE` at the fixed
/// identity `GUEST_BASE`, so two live VMs at once would collide. Each test holds this
/// lock for its whole body, and the VM is dropped (unmapping the arena) before the next
/// test acquires it. Poisoning is ignored so one failing test doesn't cascade.
static VM_LOCK: Mutex<()> = Mutex::new(());

fn vm_guard() -> std::sync::MutexGuard<'static, ()> {
    VM_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Build a fresh identity VM sized for the tests, and lay down the fixed stubs. Returns
/// the serialization guard alongside the VM: the caller must keep the guard alive for
/// the whole test so the fixed mapping isn't contended, then drop both (unmapping) at
/// the end.
fn make_vm() -> (std::sync::MutexGuard<'static, ()>, std::sync::Arc<GuestVm>) {
    let guard = vm_guard();
    let vm = GuestVm::new(SPAN);

    // Stub: MOV EAX,id ; SYSCALL ; RET  — filled per test by the callers below.
    // (written on demand so different tests use different ids on the same stub slot).

    // INNER (AC c): a guest function that returns 99.  mov eax,99 ; ret
    vm.write_bytes(INNER, &[0xB8, 0x63, 0x00, 0x00, 0x00, 0xC3])
        .unwrap();

    (guard, vm)
}

/// Write the real syscall stub at `STUB`: `MOV R10,RCX ; MOV EAX,<id> ; SYSCALL ; RET`.
/// The `MOV R10,RCX` (49 89 CA) carries the 4th call-ABI arg (RCX) into R10 before the
/// hardware-correct SYSCALL clobbers RCX; this mirrors the linker/hle emitters byte-for-byte.
fn write_syscall_stub(vm: &GuestVm, id: u32) {
    let idb = id.to_le_bytes();
    let bytes = [
        0x49, 0x89, 0xCA, // mov r10, rcx
        0xB8, idb[0], idb[1], idb[2], idb[3], // mov eax, id
        0x0F, 0x05, // syscall
        0xC3, // ret
    ];
    vm.write_bytes(STUB, &bytes).unwrap();
}

// --- AC (a): mov eax,42; ret -> Returned(42) ----------------------------------------

#[test]
fn ac_a_returns_immediate() {
    ensure_dispatch();
    let (_guard, vm) = make_vm();
    // mov eax, 42 ; ret
    vm.write_bytes(CODE, &[0xB8, 0x2A, 0x00, 0x00, 0x00, 0xC3])
        .unwrap();

    let exit = run_guest_call(&vm, CODE, STACK_TOP, 0, 0, 0);
    match exit {
        GuestExit::Returned(v) => assert_eq!(v, 42),
        other => panic!("expected Returned(42), got {other:?}"),
    }
}

// --- AC (b): CALL a SYSCALL stub; all six args incl. RCX readable; ret in RAX --------

#[test]
fn ac_b_syscall_args_and_return() {
    ensure_dispatch();
    let (_guard, vm) = make_vm();
    write_syscall_stub(&vm, SYS_ARGS as u32);

    // A guest function that loads the six SysV arg registers with sentinels, CALLs the
    // stub, and returns the stub's RAX (the dispatch return value) unchanged.
    //
    //   mov edi, 0x11 ; mov esi, 0x22 ; mov edx, 0x33
    //   mov ecx, 0x44 ; mov r8d, 0x55 ; mov r9d, 0x66
    //   mov rax, STUB ; call rax ; ret
    let mut prog: Vec<u8> = Vec::new();
    prog.extend_from_slice(&[0xBF, 0x11, 0x00, 0x00, 0x00]); // mov edi, 0x11
    prog.extend_from_slice(&[0xBE, 0x22, 0x00, 0x00, 0x00]); // mov esi, 0x22
    prog.extend_from_slice(&[0xBA, 0x33, 0x00, 0x00, 0x00]); // mov edx, 0x33
    prog.extend_from_slice(&[0xB9, 0x44, 0x00, 0x00, 0x00]); // mov ecx, 0x44
    prog.extend_from_slice(&[0x41, 0xB8, 0x55, 0x00, 0x00, 0x00]); // mov r8d, 0x55
    prog.extend_from_slice(&[0x41, 0xB9, 0x66, 0x00, 0x00, 0x00]); // mov r9d, 0x66
    prog.extend_from_slice(&[0x48, 0xB8]); // mov rax, imm64
    prog.extend_from_slice(&STUB.to_le_bytes());
    prog.extend_from_slice(&[0xFF, 0xD0]); // call rax
    prog.push(0xC3); // ret
    vm.write_bytes(CODE, &prog).unwrap();

    let exit = run_guest_call(&vm, CODE, STACK_TOP, 0, 0, 0);
    match exit {
        GuestExit::Returned(v) => assert_eq!(v, ARGS_RET, "stub return propagates to final RAX"),
        other => panic!("expected Returned({ARGS_RET:#x}), got {other:?}"),
    }

    let c = *captured().lock().unwrap();
    assert_eq!(c.id, SYS_ARGS);
    assert_eq!(c.rdi, 0x11, "arg0/RDI");
    assert_eq!(c.rsi, 0x22, "arg1/RSI");
    assert_eq!(c.rdx, 0x33, "arg2/RDX");
    assert_eq!(
        c.rcx, 0x44,
        "arg3 — 4th call-ABI arg (RCX) carried through R10"
    );
    assert_eq!(c.r8, 0x55, "arg4/R8");
    assert_eq!(c.r9, 0x66, "arg5/R9");
}

// --- AC (c): nested call_guest from inside a handler returns inner value -------------

#[test]
fn ac_c_nested_call_guest() {
    ensure_dispatch();
    let (_guard, vm) = make_vm();
    write_syscall_stub(&vm, SYS_NESTED as u32);

    // Guest: call the SYS_NESTED stub, return whatever it produced (INNER's 99).
    //   mov rax, STUB ; call rax ; ret
    let mut prog: Vec<u8> = Vec::new();
    prog.extend_from_slice(&[0x48, 0xB8]);
    prog.extend_from_slice(&STUB.to_le_bytes());
    prog.extend_from_slice(&[0xFF, 0xD0]); // call rax
    prog.push(0xC3); // ret
    vm.write_bytes(CODE, &prog).unwrap();

    let exit = run_guest_call(&vm, CODE, STACK_TOP, 0, 0, 0);
    match exit {
        GuestExit::Returned(v) => assert_eq!(v, 99, "nested call_guest(INNER) returned 99"),
        other => panic!("expected Returned(99), got {other:?}"),
    }
}

// --- AC (d): request_thread_exit(7) -> ThreadExit(7) ---------------------------------

#[test]
fn ac_d_thread_exit() {
    ensure_dispatch();
    let (_guard, vm) = make_vm();
    write_syscall_stub(&vm, SYS_EXIT as u32);

    // Guest: call the SYS_EXIT stub (which requests thread exit). Control should never
    // return to the guest; run_guest_call unwinds as ThreadExit(7).
    let mut prog: Vec<u8> = Vec::new();
    prog.extend_from_slice(&[0x48, 0xB8]);
    prog.extend_from_slice(&STUB.to_le_bytes());
    prog.extend_from_slice(&[0xFF, 0xD0]); // call rax
    prog.push(0xC3); // ret
    vm.write_bytes(CODE, &prog).unwrap();

    let exit = run_guest_call(&vm, CODE, STACK_TOP, 0, 0, 0);
    match exit {
        GuestExit::ThreadExit(v) => assert_eq!(v, 7),
        other => panic!("expected ThreadExit(7), got {other:?}"),
    }
}

// --- nested call_guest (pthread_once shape) from WORKER-thread contexts -------
//
// The emulator runs every guest thread on its own host thread with a fresh `Vcpu` over a
// shared `Arc<GuestVm>` (thread.rs::execute). `sce_pthread_once`/TLS destructors then do a
// nested `call_guest` from *inside* a syscall handler on that worker host thread. This
// test reproduces that exact shape: several host threads each enter the same guest via
// `run_guest_call`, each calls the SYS_ONCE stub, whose handler runs a guarded nested
// `call_guest(INNER)`. It proves (a) nested call_guest works off the main host thread,
// (b) the thread-local exec context is correctly per-thread (no cross-thread bleed), and
// (c) once-semantics hold: the init routine runs exactly once across all workers.
#[test]
fn ac_e_pthread_once_nested_from_workers() {
    use std::sync::atomic::Ordering;

    ensure_dispatch();
    let (_guard, vm) = make_vm();
    ONCE_RUNS.store(0, Ordering::Release);
    ONCE_GUARD.store(0, Ordering::Release);
    write_syscall_stub(&vm, SYS_ONCE as u32);

    // Guest: call the SYS_ONCE stub, return whatever it produced (INNER's 99).
    //   mov rax, STUB ; call rax ; ret
    let mut prog: Vec<u8> = Vec::new();
    prog.extend_from_slice(&[0x48, 0xB8]);
    prog.extend_from_slice(&STUB.to_le_bytes());
    prog.extend_from_slice(&[0xFF, 0xD0]); // call rax
    prog.push(0xC3); // ret
    vm.write_bytes(CODE, &prog).unwrap();

    // Spawn several WORKER host threads, each with its own guest stack slice (so their
    // frames don't collide — mirrors per-thread stacks in thread.rs), each entering the
    // guest via its own `run_guest_call`. `GuestVm` is `Sync` (shared read-only via Arc).
    const WORKERS: usize = 8;
    let mut handles = Vec::new();
    for w in 0..WORKERS {
        let vm = std::sync::Arc::clone(&vm);
        // Give each worker a distinct 16-aligned stack top well clear of the others.
        let stack_top = STACK_TOP - (w as u64) * 0x1000;
        handles.push(std::thread::spawn(move || {
            match run_guest_call(&vm, CODE, stack_top, 0, 0, 0) {
                GuestExit::Returned(v) => v,
                other => panic!("worker {w}: expected Returned, got {other:?}"),
            }
        }));
    }

    for h in handles {
        // Every worker sees 99 — whether it ran the init or waited for the winner.
        assert_eq!(
            h.join().unwrap(),
            99,
            "every worker returns the once result"
        );
    }

    assert_eq!(
        ONCE_RUNS.load(Ordering::Acquire),
        1,
        "the nested init routine ran exactly once across all worker threads"
    );
}

// --- the thread-exit flag is reset between a run_guest_call and a follow-up -----
//
// A worker's main call may `request_thread_exit` (via `sce_pthread_exit`), unwinding as
// `ThreadExit`. thread.rs then runs TLS destructors as fresh `run_guest_call`s on the same
// host thread. The old native path zeroed `should_exit` before the dtor calls; here the
// invariant is that each `run_guest_call` installs a *fresh* exec context with the exit
// flag cleared, so a prior thread-exit request cannot leak into the dtor calls. This test
// proves that: a first call requests exit (ThreadExit), a second call on the SAME host
// thread runs to a normal Return — it is NOT spuriously unwound by the stale flag.
#[test]
fn ac_f_exit_flag_reset_between_calls() {
    ensure_dispatch();
    let (_guard, vm) = make_vm();

    // Program A: call SYS_EXIT stub (requests thread exit -> ThreadExit(7)).
    write_syscall_stub(&vm, SYS_EXIT as u32);
    let mut prog: Vec<u8> = Vec::new();
    prog.extend_from_slice(&[0x48, 0xB8]);
    prog.extend_from_slice(&STUB.to_le_bytes());
    prog.extend_from_slice(&[0xFF, 0xD0]); // call rax
    prog.push(0xC3); // ret
    vm.write_bytes(CODE, &prog).unwrap();

    match run_guest_call(&vm, CODE, STACK_TOP, 0, 0, 0) {
        GuestExit::ThreadExit(v) => assert_eq!(v, 7),
        other => panic!("first call: expected ThreadExit(7), got {other:?}"),
    }

    // Program B (dtor-shaped): mov eax,42 ; ret — no exit request. On the SAME host
    // thread, immediately after the ThreadExit above. Must Return(42), proving the
    // fresh exec context cleared the previous thread-exit request.
    vm.write_bytes(CODE, &[0xB8, 0x2A, 0x00, 0x00, 0x00, 0xC3])
        .unwrap();
    match run_guest_call(&vm, CODE, STACK_TOP, 0, 0, 0) {
        GuestExit::Returned(v) => assert_eq!(v, 42, "second call returns normally"),
        other => panic!("second call: stale exit flag leaked? got {other:?}"),
    }
}

// --- null-deref -> UnmappedMemory report with RIP + addr + VMA ctx --

#[test]
fn ac_g_null_deref_report() {
    ensure_dispatch();
    ensure_annotator();
    let (_guard, vm) = make_vm();

    // A guest that dereferences address 0 (below guest_base): `mov rax, [0]`.
    //   48 8b 04 25 00 00 00 00   mov rax, qword ptr [0]
    vm.write_bytes(CODE, &[0x48, 0x8B, 0x04, 0x25, 0x00, 0x00, 0x00, 0x00])
        .unwrap();

    let exit = run_guest_call(&vm, CODE, STACK_TOP, 0, 0, 0);
    let GuestExit::Fatal(report) = exit else {
        panic!("expected Fatal from a null deref, got {exit:?}");
    };

    // The report must name the fault, the faulting RIP, the address, and the VMA
    // context ("below guest_base"). RIP is CODE (the deref is the first instruction).
    assert!(report.contains("UnmappedMemory"), "kind named: {report}");
    assert!(report.contains("read"), "access kind named: {report}");
    assert!(
        report.contains(&format!("{CODE:#x}")),
        "faulting RIP present: {report}"
    );
    assert!(report.contains("0x0"), "faulting address present: {report}");
    assert!(
        report.contains("below guest_base"),
        "VMA context present: {report}"
    );
}

// --- ud2 -> Exception report naming #UD ------------------------------

#[test]
fn ac_h_ud2_exception_report() {
    ensure_dispatch();
    let (_guard, vm) = make_vm();

    // `ud2` (0f 0b): architecturally raises #UD. x86jit lifts it to a trap that surfaces
    // as Exit::Exception { vector: 6 }, which the run loop reports with #UD naming.
    vm.write_bytes(CODE, &[0x0F, 0x0B]).unwrap();

    let exit = run_guest_call(&vm, CODE, STACK_TOP, 0, 0, 0);
    let GuestExit::Fatal(report) = exit else {
        panic!("expected Fatal from ud2, got {exit:?}");
    };

    assert!(report.contains("Exception"), "exception named: {report}");
    assert!(report.contains("#UD"), "vector mnemonic named: {report}");
    assert!(
        report.contains("invalid opcode"),
        "vector description named: {report}"
    );
    assert!(
        report.contains("SIGILL"),
        "signal-style name present: {report}"
    );
    assert!(
        report.contains(&format!("{CODE:#x}")),
        "faulting address/RIP present: {report}"
    );
    // #UD is a FAULT: the saved RIP stays ON the instruction, so the report must NOT
    // claim it is an after-instruction resume address (that annotation is trap-only).
    assert!(
        !report.contains("resume address"),
        "ud2 is a fault, not a trap — no after-instruction annotation: {report}"
    );
}

// --- int3 -> #BP TRAP report; RIP is AFTER the instruction ----------
//
// int3 (#BP) and int1 (#DB) are *traps*: x86jit's saved RIP resumes PAST the 1-byte
// instruction, unlike faults (#UD/#DE) which leave RIP on it. This
// test proves the run-loop report handles the trap case: it names #BP, reports the
// after-instruction resume address (CODE+1), annotates that RIP is past the instruction,
// and back-disassembles at CODE (not CODE+1) to name the actual trapping `int3`.

#[test]
fn ac_i_int3_trap_report() {
    ensure_dispatch();
    let (_guard, vm) = make_vm();

    // `int3` (cc): architecturally raises #BP, a trap. x86jit lifts it to
    // Exit::Exception { vector: 3 } with the saved RIP resuming past the 1-byte insn.
    vm.write_bytes(CODE, &[0xCC]).unwrap();

    let exit = run_guest_call(&vm, CODE, STACK_TOP, 0, 0, 0);
    let GuestExit::Fatal(report) = exit else {
        panic!("expected Fatal from int3, got {exit:?}");
    };

    assert!(report.contains("Exception"), "exception named: {report}");
    assert!(report.contains("#BP"), "vector mnemonic named: {report}");
    assert!(
        report.contains("breakpoint"),
        "vector description named: {report}"
    );
    assert!(
        report.contains("SIGTRAP"),
        "signal-style name present: {report}"
    );
    // Trap: the saved/reported RIP is the AFTER-instruction resume address (CODE+1).
    assert!(
        report.contains(&format!("{:#x}", CODE + 1)),
        "after-instruction resume RIP (CODE+1) present: {report}"
    );
    assert!(
        report.contains("AFTER the instruction"),
        "trap annotation present: {report}"
    );
    // The trapping instruction is back-disassembled at CODE (not CODE+1), so the report
    // names `int3` — disassembling at CODE+1 would show garbage / the wrong instruction.
    assert!(
        report.contains(&format!("{CODE:#x}")),
        "trapping-instruction address (CODE) present: {report}"
    );
    assert!(
        report.to_lowercase().contains("int3"),
        "back-disassembled trapping instruction (int3) named: {report}"
    );
}

// --- contract: all six syscall args survive the REAL stub end-to-end -----------------
//
// Regression guard for the class of bug where a hardware-correct SYSCALL clobbers a
// register that carries a syscall arg. This runs the actual stub bytes the linker/hle
// emitters produce (`MOV R10,RCX ; MOV EAX,id ; SYSCALL ; RET`) with full 64-bit distinct
// sentinels in the six call-ABI arg registers, and asserts the dispatch observed every one
// intact. It fails loudly in CI — instead of as a mysterious in-game freeze — if the
// `MOV R10,RCX` is ever dropped from the stub or `arg3()` is reverted to reading RCX.

#[test]
fn syscall_args_survive_through_real_stub() {
    ensure_dispatch();
    let (_guard, vm) = make_vm();
    write_syscall_stub(&vm, SYS_ARGS64 as u32);

    const A0: u64 = 0x1111_1111_1111_1111;
    const A1: u64 = 0x2222_2222_2222_2222;
    const A2: u64 = 0x3333_3333_3333_3333;
    const A3: u64 = 0x4444_4444_4444_4444;
    const A4: u64 = 0x5555_5555_5555_5555;
    const A5: u64 = 0x6666_6666_6666_6666;

    // Load the six SysV arg registers with full-width sentinels, CALL the real stub, ret.
    //   mov rdi,A0 ; mov rsi,A1 ; mov rdx,A2 ; mov rcx,A3 ; mov r8,A4 ; mov r9,A5
    //   mov rax,STUB ; call rax ; ret
    let mut prog: Vec<u8> = Vec::new();
    prog.extend_from_slice(&[0x48, 0xBF]); // mov rdi, imm64
    prog.extend_from_slice(&A0.to_le_bytes());
    prog.extend_from_slice(&[0x48, 0xBE]); // mov rsi, imm64
    prog.extend_from_slice(&A1.to_le_bytes());
    prog.extend_from_slice(&[0x48, 0xBA]); // mov rdx, imm64
    prog.extend_from_slice(&A2.to_le_bytes());
    prog.extend_from_slice(&[0x48, 0xB9]); // mov rcx, imm64
    prog.extend_from_slice(&A3.to_le_bytes());
    prog.extend_from_slice(&[0x49, 0xB8]); // mov r8, imm64
    prog.extend_from_slice(&A4.to_le_bytes());
    prog.extend_from_slice(&[0x49, 0xB9]); // mov r9, imm64
    prog.extend_from_slice(&A5.to_le_bytes());
    prog.extend_from_slice(&[0x48, 0xB8]); // mov rax, imm64 (STUB)
    prog.extend_from_slice(&STUB.to_le_bytes());
    prog.extend_from_slice(&[0xFF, 0xD0]); // call rax
    prog.push(0xC3); // ret
    vm.write_bytes(CODE, &prog).unwrap();

    let exit = run_guest_call(&vm, CODE, STACK_TOP, 0, 0, 0);
    assert!(
        matches!(exit, GuestExit::Returned(_)),
        "expected Returned, got {exit:?}"
    );

    let args = *captured64().lock().unwrap();
    assert_eq!(args[0], A0, "arg0/RDI");
    assert_eq!(args[1], A1, "arg1/RSI");
    assert_eq!(args[2], A2, "arg2/RDX");
    // The load-bearing assertion: arg3 is the value passed in RCX, which the hardware-
    // correct SYSCALL clobbers (RCX <- RIP). It only arrives intact because the stub
    // copies RCX -> R10 before the trap and arg3() reads R10.
    assert_eq!(
        args[3], A3,
        "arg3 — RCX, clobbered by SYSCALL, carried via R10"
    );
    assert_eq!(args[4], A4, "arg4/R8");
    assert_eq!(args[5], A5, "arg5/R9");
}
