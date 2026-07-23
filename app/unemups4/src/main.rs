use ps4_core::pad::InputManager;
use ps4_cpu::{GuestVm, NativeContext};
use ps4_gpu::{GpuManager, run_display_loop};
use ps4_kernel::process::Process;
use ps4_loader::{linker::DynamicLinker, manager::ModuleManager};
use ps4_memory::vm_backend::VmMemoryManager;

use std::sync::Arc;
use tracing::{debug, error, info};
use tracing_subscriber::EnvFilter;

mod profiler_dump;

/// Thin `fn`-pointer adapter over the `extern "C"` [`ps4_libs::rust_syscall_handler`] so it
/// matches [`ps4_cpu::SyscallDispatch`] (`fn(u64, &mut NativeContext) -> u64`). The
/// x86jit run loop calls this on every guest `SYSCALL` exit.
fn syscall_dispatch(id: u64, ctx: &mut NativeContext) -> u64 {
    ps4_libs::rust_syscall_handler(id, ctx)
}

fn env_filter() -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new(
            "ps4_kernel=info,ps4_loader=info,ps4_cpu=info,ps4_libs=info,ps4_memory=info,ps4_gpu=info,ps4_gnm=info,ps4_gcn=info,ps4_core=info,unemups4=info,unemups4::profile=info,warn",
        )
    })
}

/// A `FormatEvent` that renders `TIMESTAMP LEVEL ThreadId(NN) target: fields` — the
/// previous default console format, **minus the span scope**. The low-frequency spans
/// are always entered (so a Tracy layer can consume them, no per-crate
/// feature gate), but tracing-subscriber's `Full` formatter would otherwise prepend
/// `span{fields}:` to every line and diverge the guest-behavior baselines. `Full` has no
/// toggle to drop the scope in this version, so the (small, fixed) line format is
/// reproduced here without visiting the event scope. `run_examples.sh` normalizes the
/// timestamp/thread-id, so this stays byte-identical after normalization.
#[cfg(not(feature = "profile-tracy"))]
struct NoSpanFormat;

#[cfg(not(feature = "profile-tracy"))]
impl<S, N> tracing_subscriber::fmt::FormatEvent<S, N> for NoSpanFormat
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
    N: for<'a> tracing_subscriber::fmt::FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &tracing_subscriber::fmt::FmtContext<'_, S, N>,
        mut writer: tracing_subscriber::fmt::format::Writer<'_>,
        event: &tracing::Event<'_>,
    ) -> std::fmt::Result {
        use tracing_subscriber::fmt::time::{FormatTime, SystemTime};

        let meta = event.metadata();
        // Timestamp (RFC 3339, matches the default SystemTime timer).
        SystemTime.format_time(&mut writer)?;
        writer.write_char(' ')?;
        // Level, right-space-padded to 5 like the default.
        write!(writer, "{:>5} ", meta.level())?;
        // Thread id (default `with_thread_ids(true)` renders `ThreadId(NN)`).
        write!(writer, "{:0>2?} ", std::thread::current().id())?;
        // Target, then the event fields — no span scope in between.
        write!(writer, "{}: ", meta.target())?;
        ctx.field_format().format_fields(writer.by_ref(), event)?;
        writeln!(writer)
    }
}

/// Default build: a fmt subscriber using [`NoSpanFormat`], so the always-entered
/// spans are invisible in the console (and a pure no-op without a consuming layer).
/// Output matches the previous `FmtSubscriber` after `run_examples.sh` normalization.
#[cfg(not(feature = "profile-tracy"))]
fn init_logging() {
    tracing_subscriber::fmt::Subscriber::builder()
        .with_env_filter(env_filter())
        .event_format(NoSpanFormat)
        .init();
}

/// `profile-tracy` build: a `Registry` (which stores spans) with the fmt layer plus the
/// Tracy layer, so the spans become live zones in the Tracy GUI. Under a span-storing
/// Registry the fmt layer does render span context — acceptable for this opt-in
/// profiling build (the default build above stays baseline-clean).
#[cfg(feature = "profile-tracy")]
fn init_logging() {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_target(true)
        .with_thread_ids(true)
        .with_file(false);

    tracing_subscriber::registry()
        .with(env_filter())
        .with(fmt_layer)
        .with(tracing_tracy::TracyLayer::default())
        .init();
}

/// The composition root's single wiring point: register every process-global host seam with
/// its concrete impl, then assert all are wired before any guest thread starts. Each
/// `Registered<>` seam and the cpu syscall dispatch degrade to a runtime no-op / fatal fault
/// when unwired; folding registration into one function and failing the boot on a gap turns
/// a silent misregistration into a loud boot failure (task-132). Called once, on the boot
/// thread, before the guest thread is spawned — so the writes are uncontended.
fn wire_host_services(
    guest_vm: &Arc<GuestVm>,
    gpu: &Arc<GpuManager>,
    process: &Arc<Process>,
    bridge: Arc<ps4_kernel::bridge::KernelBridge>,
) {
    // Guest syscall dispatch (cpu `OnceLock`): the x86jit run loop calls this on every
    // guest `SYSCALL` exit. Installed here rather than earlier so all host wiring lives in
    // one place; nothing executes guest code before this returns.
    ps4_cpu::set_syscall_dispatch(syscall_dispatch);

    // Per-frame budget (task-209): tell the run loop which syscalls end a guest frame, so
    // it can split the flipping thread's flip-to-flip wall time. `ps4-cpu` has no
    // `ps4-syscalls` dependency (same reason syscall names are resolved at dump time), so
    // the ids are pushed in from here. No-op unless UNEMUPS4_PROFILE is set.
    ps4_cpu::profile::set_frame_boundary_syscalls(vec![
        ps4_syscalls::SyscallId::SCE_GNM_SUBMIT_AND_FLIP_COMMAND_BUFFERS.0,
        ps4_syscalls::SyscallId::SCE_GNM_SUBMIT_AND_FLIP_COMMAND_BUFFERS_FOR_WORKLOAD.0,
    ]);

    // Per-thread execution tracer (task-170, `UNEMUPS4_EXECTRACE`): install the syscall-id
    // → name resolver (breaks a `ps4-core -> ps4-syscalls` dep, like the fault annotator)
    // and start the periodic per-thread dump. Both are no-ops when the env gate is unset.
    ps4_core::exectrace::set_name_resolver(|id| {
        match ps4_syscalls::SyscallId::from_raw(id).map(|s| s.name()) {
            Some("Unknown") | None => None,
            Some(name) => Some(name),
        }
    });
    ps4_core::exectrace::start();

    // Kernel HLE seam: `ps4-libs` reaches the kernel bridge through this global.
    ps4_core::kernel::register_kernel(bridge);

    // Resource-cache dirty tracking (doc-2 §8.3): the real x86jit-backed source over the
    // shared VM, unless `UNEMUPS4_DIRTY=always` forces the conservative `AlwaysDirty`
    // fallback (re-upload every submit).
    if std::env::var("UNEMUPS4_DIRTY").as_deref() == Ok("always") {
        ps4_core::dirty::register_dirty_source(Arc::new(ps4_core::dirty::AlwaysDirty::new()));
    } else {
        ps4_core::dirty::register_dirty_source(Arc::new(ps4_cpu::VmDirtySource::new(Arc::clone(
            guest_vm,
        ))));
    }

    // PM4 executor present sink (doc-2 §3): the Gnm submit handler drives `SubmitAndFlip`
    // through the block-until-vsync present path videoout uses, via this `GpuManager`.
    ps4_core::gpu::register_present_sink(gpu.clone());
    // Display-buffer seam (doc-2 §5): the draw path maps a `CB_COLOR0_BASE` to the videoout
    // framebuffer it aliases so a register-route GCN draw into a registered display buffer
    // resolves instead of deferring as an arbitrary RT.
    ps4_core::gpu::register_display_buffers(gpu.display_buffer_source());
    // Videoout present seam (doc-2 §3): `sceVideoOutRegisterBuffers`/`sceVideoOutSubmitFlip`
    // reach the display channel through this trait, so `ps4-kernel` needs no `ps4-gpu` dep.
    ps4_core::videoout::register_video_out_sink(gpu.clone());
    // Guest free/unmap → resource-cache eviction seam (doc-2 §8): the kernel memory manager
    // fires this on `munmap`/`sceKernelReleaseDirectMemory`; the gnm impl evicts any cache
    // entry keyed on the freed range and revokes any zero-copy import before its backing
    // host pages are freed.
    ps4_core::gpu::register_memory_free_sink(
        Arc::new(ps4_gnm::free_sink::GnmMemoryFreeSink::new()),
    );

    // Bounds-checked read seam: HLE handlers that read a guest-supplied register block
    // (`sceGnmSetVsShader`/`sceGnmSetPsShader`) validate the pointer against the live VMA set
    // instead of over-reading through the unbounded identity view.
    ps4_core::bounded_read::register_bounded_read(Arc::new(process.memory.clone()));

    // SMC-tracked write seam (task-115): the mirror of the read seam. Migrated HLE handlers
    // write guest out-params through this — the SAME VMA-tracking memory manager handle —
    // so every write goes through `write_bytes` (SMC-observed) instead of a raw identity-map
    // store the JIT's self-modifying-code tracking never sees.
    ps4_core::write_guest::register_write_guest(Arc::new(process.memory.clone()));

    // Guest arena bounds (task-115): promote the cpu's `GUEST_BASE`/`DEFAULT_SPAN` into a
    // ps4-core process-global so `GuestPtr::new` can range-check without reaching into ps4-cpu.
    ps4_core::kernel::set_arena_bounds(
        ps4_cpu::guest_vm::GUEST_BASE,
        ps4_cpu::guest_vm::DEFAULT_SPAN,
    );

    // All process-global seams must be wired before guest threads start: a gap here would
    // otherwise surface only as a runtime-silent degrade (a `None` seam) or a fatal guest
    // fault on the first `SYSCALL`. Assert each so misregistration is a boot failure.
    assert!(
        ps4_cpu::syscall_dispatch_installed(),
        "syscall dispatch not installed"
    );
    assert!(
        ps4_core::kernel::get_kernel().is_some(),
        "KERNEL_INTERFACE not wired"
    );
    assert!(
        ps4_core::dirty::dirty_source().is_some(),
        "DIRTY_SOURCE not wired"
    );
    assert!(
        ps4_core::gpu::present_sink().is_some(),
        "PRESENT_SINK not wired"
    );
    assert!(
        ps4_core::gpu::display_buffers().is_some(),
        "DISPLAY_BUFFERS not wired"
    );
    assert!(
        ps4_core::videoout::video_out_sink_wired(),
        "VIDEO_OUT_SINK not wired"
    );
    assert!(
        ps4_core::gpu::memory_free_sink().is_some(),
        "MEMORY_FREE_SINK not wired"
    );
    assert!(
        ps4_core::bounded_read::bounded_read().is_some(),
        "BOUNDED_READ not wired"
    );
    assert!(
        ps4_core::write_guest::write_guest().is_some(),
        "WRITE_GUEST not wired"
    );
    assert!(
        ps4_core::kernel::arena_bounds().is_some(),
        "ARENA_BOUNDS not wired"
    );
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    init_logging();
    info!("--- PS4 Native Emulator Boot (Graphics Enabled) ---");

    ps4_libs::init();

    let linker = DynamicLinker::new();
    let modules = ModuleManager::new();

    // Build the identity-mapped guest VM (x86jit backend) and the software VMA manager over
    // its shared arena. Both hold the same `Arc<GuestVm>` (the VM's arena is mapped once).
    let guest_vm = GuestVm::new(ps4_cpu::guest_vm::DEFAULT_SPAN);

    // Aggregate profiler: no-op unless UNEMUPS4_PROFILE is set. Started here so
    // the dump thread + atexit handler hold the shared VM and can read x86jit counters.
    profiler_dump::start(Arc::clone(&guest_vm));

    // Direct VM exit/entry measurement (task-209). Runs a tiny guest stub while the low
    // arena still holds nothing but the HLT gadget, so its scratch pages cannot collide
    // with the loaded image. No-op unless UNEMUPS4_PROFILE is set.
    ps4_cpu::calibrate_vm_exit(&guest_vm);

    let mut memory_backend = Box::new(VmMemoryManager::new(Arc::clone(&guest_vm)));
    linker.init_stubs(&mut *memory_backend)?;

    // manager goes to the kernel, receiver to the window loop
    let (gpu_manager, gpu_receiver) = GpuManager::new();
    let gpu_manager_arc = Arc::new(gpu_manager);

    let input_manager = InputManager::new();
    // Process takes memory_backend and wraps it in Arc<RwLock<...>>
    let process = Process::new(
        Arc::clone(&guest_vm),
        memory_backend,
        modules,
        linker.clone(),
        input_manager.clone(),
    );

    // map guest paths onto host game_data dirs
    process
        .fs
        .mount("/app0", std::env::current_dir()?.join("game_data/app0"));
    process
        .fs
        .mount("/system", std::env::current_dir()?.join("game_data/system"));

    // Also union-mount the loaded title's own directory onto /app0: a retail eboot's
    // assemblies/content sit next to it (that dir IS the guest's /app0 on real hardware),
    // while examples keep creating scratch files under game_data/app0. Existing files
    // resolve from whichever mount holds them (see FileSystem::translate).
    if let Some(exe) = std::env::args().nth(1)
        && let Some(dir) = std::path::Path::new(&exe).parent()
        && !dir.as_os_str().is_empty()
    {
        let dir = dir.to_path_buf();
        process.fs.mount("/app0", dir.clone());
        // Bring-up heuristic (task-113.4 will generalize asset layout): a Mono
        // title dumps its managed assemblies (mscorlib.dll, …) at the package
        // root, but the runtime searches the framework dirs `mono/<ver>/` and
        // `lib/mono/<ver>/`. Alias those onto the dump dir so the assemblies
        // resolve. Longer prefixes win in FileSystem::translate.
        for sub in ["/app0/mono/4.5", "/app0/lib/mono/4.5"] {
            process.fs.mount(sub, dir.clone());
        }
    }

    info!("--- Kernel: Process created (PID: {}) ---", process.id);

    let bridge = ps4_kernel::bridge::KernelBridge::new(process.clone());

    // Composition root: register every process-global host seam in one place and assert all
    // are wired before any guest thread starts. This is the only spot in the tree that knows
    // the concrete impls (Vulkan `GpuManager`, x86jit `VmDirtySource`, the kernel bridge …);
    // routing them all through here — and failing the boot if one is missing — turns a
    // silent misregistration (a seam that would degrade to `None` at runtime) into a loud
    // boot failure. The fault annotator is wired separately below: it is enrichment-only
    // (a fatal report gains VMA context), not a behavioral seam, so it is not asserted.
    wire_host_services(&guest_vm, &gpu_manager_arc, &process, bridge);

    // Install the fault-address annotator: when the x86jit run loop hits an
    // `UnmappedMemory` fault it calls this to name the nearest VMA(s) for the report.
    // The closure holds the same `Arc<RwLock<..>>` as the process, so it reflects the
    // live VMA map. `ps4-cpu` stays free of a `ps4-memory`/`ps4-core` dependency —
    // the annotator is a boxed closure in a global `OnceLock`, mirroring the syscall
    // dispatcher.
    // It also names the nearest exported symbol (`libc!strlen +0x2d`) from the module
    // tree, so a fault inside guest libc identifies the routine rather than a bare
    // module offset (task-113.2). `try_read` on both the VMA and module maps,
    // deliberately: an HLE handler can hold `memory.write()` and re-enter guest code
    // (module_start/pthread-init/tls-dtor callbacks) via `ps4_cpu::call_guest`; a fault
    // inside that nested guest run calls this annotator on the same thread, and a
    // blocking `read()` on a lock that same thread already write-holds would deadlock.
    // A faulting thread must still get a report, not a hang.
    {
        let fault_mem = process.memory.clone();
        let fault_mods = process.modules.clone();
        ps4_cpu::set_fault_annotator(Box::new(move |addr| {
            let vma = match fault_mem.try_read() {
                Ok(mem) => mem.describe_fault_context(addr),
                Err(_) => "VMA map lock unavailable; no context".to_string(),
            };
            // An import stub is not in any module's symbol table — it is memory we emitted —
            // so resolve it first, or the report says only "inside VMA HLE_Stubs" for an
            // address that has an exact name (task-113.2).
            if let Some(stub) = ps4_core::debug::describe_stub(addr) {
                return format!("{vma} — {stub}");
            }
            match fault_mods
                .try_read()
                .ok()
                .and_then(|mods| mods.nearest_symbol(addr))
            {
                Some(sym) => format!("{vma} — {sym}"),
                None => vma,
            }
        }));
    }

    {
        // Boot-stage span: a one-shot, low-frequency zone under Tracy; a
        // cached callsite check with no span-consuming layer.
        let _span = tracing::info_span!("hle_install").entered();
        info!("--- Booting Kernel HLE ---");
        let mut mem_guard = process.memory.write().unwrap();
        let mut mod_guard = process.modules.write().unwrap();
        ps4_kernel::hle::HleBootstrap::install(&mut **mem_guard, &mut mod_guard)?;
    }

    // first cli arg is the homebrew elf/self path
    let filename = std::env::args()
        .nth(1)
        .ok_or("usage: unemups4 <path-to-homebrew.elf>")?;
    info!("--- Loading Executable: {} ---", filename);
    let entry_point = {
        let _span = tracing::info_span!("load_executable").entered();
        process.load_executable(&filename)?
    };

    // Env-gated module dump (`UNEMUPS4_DUMP_MODULES=<dir>`): now that every module is loaded
    // and relocated, write each non-HLE module's loaded segment image + `.map` sidecar so a
    // guest-side fault (`eboot.bin +0x16de90`) can be disassembled/decompiled offline in
    // objdump/Ghidra/radare2. A single env lookup when unset; reads guest bytes through the
    // SMC-safe VMA read seam, never a raw pointer. See `ps4_loader::dump`.
    {
        let mods = process.modules.read().unwrap();
        let mem = process.memory.read().unwrap();
        ps4_loader::dump::maybe_dump_modules(&mods, &**mem);
    }
    // The main thread's guest RDI is set to its stack pointer in `Thread::execute` (it
    // points at the on-stack argv/env frame, per the SysV entry contract). The old
    // host-static `&ARGC_ZERO` pointer is dropped — a host pointer outside the guest span
    // would be a guest-visible fault under x86jit. Worker threads pass their
    // real entry argument, so a placeholder is only needed for the main thread; use 0.
    let entry_arg = 0u64;

    // guest runs on a background thread; main thread stays free for the display loop
    let process_thread = process.clone();

    std::thread::spawn(move || {
        info!("--- [Emulator Thread] Launching Guest ---");

        let main_thread = match process_thread.create_thread(
            entry_point,
            entry_arg,
            Arc::downgrade(&process_thread),
        ) {
            Ok(t) => t,
            Err(e) => {
                error!("Failed to create main thread: {:?}", e);
                // No guest thread runs on this path, so nothing will ever reach the
                // `std::process::exit(0)` below — and the main OS thread parks forever
                // waiting for the guest's own `exit()` syscall (see the headless-park
                // comment near the end of `main`). Exit directly rather than `return`,
                // which would leave the parked main thread keeping a dead process alive.
                std::process::exit(1);
            }
        };

        info!(
            "Kernel: Main Thread Created (TID: {}) Stack: {:#x}, TLS: {:#x}",
            main_thread.id, main_thread.stack_base, main_thread.tls_base
        );

        if let Err(e) = main_thread.execute() {
            error!("Guest execution failed: {:?}", e);
        }

        // block this emulator thread on the guest, not the main OS thread
        if let Some(handle) = main_thread.host_thread.lock().unwrap().take()
            && let Err(e) = handle.join()
        {
            error!("Guest thread panicked: {:?}", e);
        }

        info!("--- [Emulator Thread] Execution Finished ---");
        std::process::exit(0);
    });

    // takes over the main thread; passes process.memory so the gpu can read the framebuffer
    info!("--- [Main Thread] Starting Window Loop ---");
    let display_mem = process.memory.clone();
    let display = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run_display_loop(gpu_receiver, display_mem, input_manager);
    }));

    // In a headless devShell the display backend (winit/Wayland) is unavailable and
    // `run_display_loop` panics on this main thread. That panic must NOT abort the
    // process: the guest runs on the emulator thread and ends the process itself via
    // its own `exit()` syscall (`std::process::exit`). Returning from `main` here —
    // or letting the panic unwind out of `main` — would kill that still-running guest
    // thread mid-execution and truncate its output. So on a display failure we park
    // this thread and let the guest reach its natural exit.
    if display.is_err() {
        // debug! (below the default info filter) so this host-env diagnostic never
        // enters the guest-behavior baseline captured by scripts/run_examples.sh.
        debug!("--- [Main Thread] Display loop unavailable (headless); parking for guest ---");
        loop {
            std::thread::park();
        }
    }

    Ok(())
}
