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

fn main() -> Result<(), Box<dyn std::error::Error>> {
    init_logging();
    info!("--- PS4 Native Emulator Boot (Graphics Enabled) ---");

    ps4_libs::init();

    // Install the guest syscall dispatcher into the x86jit run loop before any guest code
    // can trap a SYSCALL.
    ps4_cpu::set_syscall_dispatch(syscall_dispatch);

    let linker = DynamicLinker::new();
    let modules = ModuleManager::new();

    // Build the identity-mapped guest VM (x86jit backend) and the software VMA manager over
    // its shared arena. Both hold the same `Arc<GuestVm>` (the VM's arena is mapped once).
    let guest_vm = GuestVm::new(ps4_cpu::guest_vm::DEFAULT_SPAN);

    // Wire the resource cache's dirty-tracking seam (doc-4 §8.3): the real x86jit-backed
    // source over this shared VM, unless `UNEMUPS4_DIRTY=always` forces the conservative
    // `AlwaysDirty` fallback (re-upload every submit). Registered once here, before guest
    // threads start, mirroring `register_kernel`/`register_present_sink`.
    if std::env::var("UNEMUPS4_DIRTY").as_deref() == Ok("always") {
        ps4_core::dirty::register_dirty_source(Arc::new(ps4_core::dirty::AlwaysDirty::new()));
    } else {
        ps4_core::dirty::register_dirty_source(Arc::new(ps4_cpu::VmDirtySource::new(Arc::clone(
            &guest_vm,
        ))));
    }

    // Aggregate profiler: no-op unless UNEMUPS4_PROFILE is set. Started here so
    // the dump thread + atexit handler hold the shared VM and can read x86jit counters.
    profiler_dump::start(Arc::clone(&guest_vm));

    let mut memory_backend = Box::new(VmMemoryManager::new(Arc::clone(&guest_vm)));
    linker.init_stubs(&mut *memory_backend)?;

    // manager goes to the kernel, receiver to the window loop
    let (gpu_manager, gpu_receiver) = GpuManager::new();
    let gpu_manager_arc = Arc::new(gpu_manager);

    // Wire the PM4 executor's present sink (doc-4 §3): the Gnm submit
    // handler drives `SubmitAndFlip` through the same block-until-vsync present
    // path videoout uses, reaching the display channel via this `GpuManager`.
    ps4_core::gpu::register_present_sink(gpu_manager_arc.clone());
    // Wire the display-buffer seam (doc-4 §5): the draw path's render-target derivation
    // maps a CB_COLOR0_BASE to the videoout framebuffer it aliases through this source, so
    // a register-route GCN draw into a registered display buffer resolves instead of
    // deferring as an arbitrary RT.
    ps4_core::gpu::register_display_buffers(gpu_manager_arc.display_buffer_source());
    // Wire the guest free/unmap → resource-cache eviction seam (doc-4 §8): the kernel
    // memory manager fires this on `munmap`/`sceKernelReleaseDirectMemory`, and the gnm
    // impl evicts any cache entry keyed on the freed range (so a realloc of the same
    // address mints a fresh id, not a stale-id clean hit) and revokes any zero-copy import
    // before its backing host pages are freed. Registered before guest threads start,
    // mirroring the present-sink wiring above.
    ps4_core::gpu::register_memory_free_sink(std::sync::Arc::new(
        ps4_gnm::free_sink::GnmMemoryFreeSink::new(),
    ));
    let input_manager = InputManager::new();
    // Process takes memory_backend and wraps it in Arc<RwLock<...>>
    let process = Process::new(
        Arc::clone(&guest_vm),
        memory_backend,
        modules,
        linker.clone(),
        Some(gpu_manager_arc),
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
    if let Some(exe) = std::env::args().nth(1) {
        if let Some(dir) = std::path::Path::new(&exe).parent() {
            if !dir.as_os_str().is_empty() {
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
        }
    }

    info!("--- Kernel: Process created (PID: {}) ---", process.id);

    let bridge = ps4_kernel::bridge::KernelBridge::new(process.clone());
    ps4_core::kernel::register_kernel(bridge);

    // Wire the bounds-checked read seam so HLE handlers that read a guest-supplied
    // register block (`sceGnmSetVsShader`/`sceGnmSetPsShader`) validate the pointer
    // against the live VMA set instead of over-reading through the unbounded identity
    // view. Same shared handle as the fault annotator, so it reflects the live map;
    // registered once here before guest threads start, mirroring `register_kernel`.
    ps4_core::bounded_read::register_bounded_read(Arc::new(process.memory.clone()));

    // Install the fault-address annotator: when the x86jit run loop hits an
    // `UnmappedMemory` fault it calls this to name the nearest VMA(s) for the report.
    // The closure holds the same `Arc<RwLock<..>>` as the process, so it reflects the
    // live VMA map. `ps4-cpu` stays free of a `ps4-memory`/`ps4-core` dependency —
    // the annotator is a boxed closure in a global `OnceLock`, mirroring the syscall
    // dispatcher.
    {
        let fault_mem = process.memory.clone();
        ps4_cpu::set_fault_annotator(Box::new(move |addr| match fault_mem.read() {
            Ok(mem) => mem.describe_fault_context(addr),
            Err(_) => "VMA map lock poisoned; no context".to_string(),
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
    // The main thread's guest RDI is set to its stack pointer in `Thread::execute` (it
    // points at the on-stack argv/env frame, per the SysV entry contract). The old
    // host-static `&ARGC_ZERO` pointer is dropped — a host pointer outside the guest span
    // would be a guest-visible fault under x86jit (doc-1 dec 4). Worker threads pass their
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
                return;
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
