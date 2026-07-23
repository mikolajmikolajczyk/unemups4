//! Integration test for the x86jit-backed [`VmDirtySource`].
//!
//! Runs JIT-compiled guest code that stores into a watched guest range and asserts the
//! `DirtySource` (over `GuestVm`'s `take_dirty_ranges`) reports it, while a store into an
//! *unwatched* range is not reported. The whole point of the x86jit JIT-inlined-store pin bump is
//! that JIT-inlined stores now feed watched-range tracking — so this must run under the
//! **JIT backend**, not the interpreter (which would pass even without the fix, a false
//! green). The test forces eager foreground tier-up via
//! `GuestVm::new_eager_jit_for_test` and drives the store through a compiled block, and
//! it refuses to run under `UNEMUPS4_BACKEND=interp`.

use std::sync::{Arc, Mutex};

use ps4_core::dirty::DirtySource;
use ps4_cpu::guest_vm::BACKEND_ENV;
use ps4_cpu::{GuestExit, GuestVm, VmDirtySource, run_guest_call};

const SPAN: u64 = 0x0080_0000; // 8 MiB arena
const CODE: u64 = 0x0040_0000; // entry: store into the target then ret
const WATCHED: u64 = 0x0060_0000; // page we watch — the store lands here
const UNWATCHED: u64 = 0x0070_0000; // page we do NOT watch — its store must be invisible
const STACK_TOP: u64 = 0x0050_0000; // 16-aligned guest stack top
const PROGRESS: u64 = 0x0061_0000; // loop thread bumps this each iteration; host polls it
const STOP: u64 = 0x0062_0000; // host sets this nonzero to break the loop thread out

/// Serializes VM construction: `GuestVm::new*` does a `MAP_FIXED_NOREPLACE` at the fixed
/// identity `GUEST_BASE`, so two live VMs at once would collide. Held for the whole test.
static VM_LOCK: Mutex<()> = Mutex::new(());

fn vm_guard() -> std::sync::MutexGuard<'static, ()> {
    VM_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Guest program: `mov [rdi], eax ; ret`. `run_guest_call` passes its first arg in RDI,
/// so the caller picks the store destination per run; EAX is left as-is (its value is
/// irrelevant — dirty tracking is about *which page* was written, not the bytes).
fn write_store_program(vm: &GuestVm) {
    // 89 07          mov [rdi], eax
    // c3             ret
    vm.write_bytes(CODE, &[0x89, 0x07, 0xC3]).unwrap();
}

/// Run the store block once with `rdi = dst`, expecting a clean guest return.
fn run_store(vm: &Arc<GuestVm>, dst: u64) {
    match run_guest_call(vm, CODE, STACK_TOP, dst, 0, 0) {
        GuestExit::Returned(_) => {}
        other => panic!("store block did not return cleanly: {other:?}"),
    }
}

#[test]
fn jit_store_into_watched_range_is_reported() {
    // Guard against the false green: under the interpreter this passes even without the
    // JIT-inlined-store fix, so it would not actually test the JIT path. The DEFAULT
    // (unset) backend is JIT — the intended configuration for this test.
    if let Ok(backend) = std::env::var(BACKEND_ENV)
        && backend == "interp"
    {
        panic!(
            "this test must exercise the JIT store path; re-run without \
             {BACKEND_ENV}=interp (default backend is JIT)"
        );
    }

    let _guard = vm_guard();
    // Eager foreground tier-up: the block compiles after its first (warmup) execution, so
    // the second run below executes JIT-compiled code — the inlined-store path
    // wired into watched-range tracking.
    let vm = GuestVm::new_eager_jit_for_test(SPAN);
    write_store_program(&vm);

    let dirty: Arc<dyn DirtySource> = Arc::new(VmDirtySource::new(Arc::clone(&vm)));
    dirty.watch(WATCHED, 0x1000);

    // Warmup run (interpreted): triggers the compile + swap-in. Discard its dirty output
    // so what we assert on comes strictly from the compiled block, not the warmup.
    run_store(&vm, WATCHED);
    let _ = dirty.take_dirty();

    // JIT-compiled run: the inlined store into the watched page must be recorded.
    run_store(&vm, WATCHED);
    let after_watched = dirty.take_dirty();
    assert!(
        after_watched
            .iter()
            .any(|&(a, n)| WATCHED >= a && WATCHED < a + n),
        "JIT'd store into watched range must be reported; got {after_watched:?}"
    );
    // Drain is empty once nothing new was written.
    assert!(
        dirty.take_dirty().is_empty(),
        "take_dirty drains: a second poll with no new writes is empty"
    );

    // A JIT'd store into an UNWATCHED page must not be reported.
    run_store(&vm, UNWATCHED);
    let after_unwatched = dirty.take_dirty();
    assert!(
        after_unwatched.is_empty(),
        "store into unwatched range must not be reported; got {after_unwatched:?}"
    );

    // After unwatch, even the previously-watched page stops reporting.
    dirty.unwatch(WATCHED, 0x1000);
    run_store(&vm, WATCHED);
    assert!(
        dirty.take_dirty().is_empty(),
        "store into a range after unwatch must not be reported"
    );
}

/// Guest loop program that hammers a target page from a JIT-compiled hot block:
///
/// ```text
///   mov rsi, PROGRESS        ; host-visible iteration counter
///   mov rdx, STOP            ; host-writable break flag
/// loop:
///   inc dword [rsi]          ; bump progress
///   mov  [rdi], eax          ; store into the target page (rdi passed by run_guest_call)
///   mov  ecx, [rdx]          ; load STOP
///   test ecx, ecx
///   jz   loop                ; spin until the host sets STOP
///   ret
/// ```
///
/// `run_guest_call` only feeds `rdi`, so the two pointers are baked as imm64s. The hot
/// back-edge tiers up under `new_eager_jit_for_test`, so the thread spends its run inside
/// JIT-compiled code — the state the mid-run watch must reach.
fn write_loop_program(vm: &GuestVm) {
    let mut code = vec![0x48, 0xBE]; // mov rsi, imm64
    code.extend_from_slice(&PROGRESS.to_le_bytes());
    code.extend_from_slice(&[0x48, 0xBA]); // mov rdx, imm64
    code.extend_from_slice(&STOP.to_le_bytes());
    // loop body (11 bytes): inc[rsi]; mov[rdi],eax; mov ecx,[rdx]; test; jz loop (-10); ret
    code.extend_from_slice(&[
        0xFF, 0x06, 0x89, 0x07, 0x8B, 0x0A, 0x85, 0xC9, 0x74, 0xF6, 0xC3,
    ]);
    vm.write_bytes(CODE, &code).unwrap();
}

/// task-217 repro at the `VmDirtySource` seam: a JIT-looping thread stores into a page
/// while `watch_count == 0` at its run start, and another thread installs the FIRST watch
/// on that page mid-run (0 -> nonzero). With the live-gate fix (x86jit-217, in the pinned
/// rev) the loop thread's post-watch JIT'd stores are recorded; the old run-start snapshot
/// left the gate off for the whole run and silently lost them.
#[test]
fn jit_store_seen_when_watched_mid_run_by_another_thread() {
    // Same false-green guard as above: the interpreter would pass without the JIT fix.
    if let Ok(backend) = std::env::var(BACKEND_ENV)
        && backend == "interp"
    {
        panic!(
            "this test must exercise the JIT store path; re-run without \
             {BACKEND_ENV}=interp (default backend is JIT)"
        );
    }

    let _guard = vm_guard();
    let vm = GuestVm::new_eager_jit_for_test(SPAN);
    write_loop_program(&vm);
    vm.write_bytes(PROGRESS, &0u32.to_le_bytes()).unwrap();
    vm.write_bytes(STOP, &0u32.to_le_bytes()).unwrap();

    let dirty: Arc<dyn DirtySource> = Arc::new(VmDirtySource::new(Arc::clone(&vm)));

    let read_u32 = |addr: u64| {
        let mut b = [0u8; 4];
        vm.read_bytes(addr, &mut b).unwrap();
        u32::from_le_bytes(b)
    };

    // Thread A runs the loop; its run starts with watch_count == 0 (nothing watched yet).
    let vm_a = Arc::clone(&vm);
    let a =
        std::thread::spawn(
            move || match run_guest_call(&vm_a, CODE, STACK_TOP, WATCHED, 0, 0) {
                GuestExit::Returned(_) => {}
                other => panic!("loop block did not return cleanly: {other:?}"),
            },
        );

    // Wait until A is deep in the loop — hot enough to be JIT-compiled and running.
    while read_u32(PROGRESS) < 500_000 {
        std::hint::spin_loop();
    }

    // Install the FIRST watch on the page A is hammering, mid-run (0 -> 1).
    dirty.watch(WATCHED, 0x1000);
    let at_watch = read_u32(PROGRESS);

    // Let A execute many more stores strictly AFTER the watch went live.
    while read_u32(PROGRESS) < at_watch + 500_000 {
        std::hint::spin_loop();
    }

    // Break A out of the loop and join.
    vm.write_bytes(STOP, &1u32.to_le_bytes()).unwrap();
    a.join().unwrap();

    // Guard against a false green: the loop must actually have run JIT-compiled.
    assert!(
        vm.jit_counters().hits > 0,
        "loop must execute JIT-compiled (JIT hits > 0) or the race is not exercised"
    );

    // The post-watch JIT'd stores into WATCHED must be visible via the DirtySource.
    let seen = dirty.take_dirty();
    assert!(
        seen.iter().any(|&(a, n)| WATCHED >= a && WATCHED < a + n),
        "store into a range watched mid-run by another thread must be reported (task-217); \
         got {seen:?}"
    );
}
