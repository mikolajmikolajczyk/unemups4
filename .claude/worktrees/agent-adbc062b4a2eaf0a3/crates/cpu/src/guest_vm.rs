//! The identity-mapped guest virtual machine (x86jit backend).
//!
//! `GuestVm` owns the x86jit [`Vm`] plus the identity `mmap` arena. All mapping is
//! done up-front — [`Vm::map`] is `&mut self`, so we must finish the whole memory
//! layout *before* wrapping the VM in an `Arc` (the run loop and the software
//! memory-manager then share it read-only through the `Arc`). See doc-1 decisions 1/4/5.

use std::sync::Arc;

use tracing::info;
use x86jit_core::{Backend, GuestCpuFeatures, InterpreterBackend, Prot, RegionKind, Vm, VmConfig};
use x86jit_cranelift::JitBackend;

use crate::hostmem::reserve_at;

/// Environment variable selecting the execution backend. `jit` (the default) runs
/// guest code under the Cranelift JIT with hotness-gated background tier-up; `interp`
/// keeps the plain interpreter (kept for debugging / as the differential oracle).
pub const BACKEND_ENV: &str = "UNEMUPS4_BACKEND";

/// Tier-up threshold (doc-1 dec 8, matching x86jit-cli's `TIER_UP_AFTER`): interpret a
/// block this many times before JIT-compiling it, so short-lived blocks never pay a
/// compile. Only used by the JIT backend.
const TIER_UP_AFTER: u32 = 50;

/// Which execution backend `GuestVm::new` picked, resolved from [`BACKEND_ENV`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BackendKind {
    /// Cranelift JIT with background tier-up (the default).
    Jit,
    /// Plain interpreter (`UNEMUPS4_BACKEND=interp`).
    Interp,
}

impl BackendKind {
    /// Resolve the backend from [`BACKEND_ENV`]. Default (unset / empty) is [`Jit`].
    /// An unrecognized value falls back to `Jit` with a warning rather than aborting.
    fn from_env() -> BackendKind {
        match std::env::var(BACKEND_ENV).ok().as_deref() {
            None | Some("") | Some("jit") => BackendKind::Jit,
            Some("interp") => BackendKind::Interp,
            Some(other) => {
                tracing::warn!(
                    "{BACKEND_ENV}={other:?} is not a known backend (expected `jit` or \
                     `interp`); defaulting to `jit`"
                );
                BackendKind::Jit
            }
        }
    }
}

/// How `GuestVm::build` configures JIT tier-up. Production uses [`HotBackground`]
/// (`GuestVm::new`); the DirtySource integration test uses [`EagerForeground`] to make
/// the JIT store path execute deterministically. Only meaningful on the JIT backend.
#[derive(Clone, Copy)]
enum TierUp {
    /// Hotness-gated, compiled off-vcpu on the worker thread (the boot default).
    HotBackground,
    /// Compile after the first execution, in the foreground (test determinism lever).
    EagerForeground,
}

/// Low cutoff of the guest address space. `[0, GUEST_BASE)` is never mapped (a
/// null-adjacent mapping is UB to reserve and pointless — `mmap_min_addr`), so any
/// guest access below this traps. Matches the loader's heap cursor starting far above.
pub const GUEST_BASE: u64 = 0x10000;

/// Default arena span (exclusive top guest address): 64 GiB. The PS4 image + code sit
/// low, but the guest heap cursor climbs from `0x4_0000_0000` (17 GiB), so the span
/// must comfortably cover it; `MAP_NORESERVE` keeps untouched pages free (doc-1 dec 1).
pub const DEFAULT_SPAN: u64 = 64 * 1024 * 1024 * 1024;

/// Guest address of the single-byte HLT gadget page. The run loop pushes `GADGET_ADDR`
/// as a guest function's return address; when the function `ret`s it lands here and
/// executes `hlt`, which the loop reads as "the guest call returned" (doc-1 dec 3).
pub const GADGET_ADDR: u64 = 0x30000;

/// The gadget instruction: a single `hlt` (opcode `0xF4`). RIP after executing it is
/// `GADGET_ADDR + 1`, which the run loop uses to distinguish a guest-return HLT from
/// any other `hlt` the guest might execute.
pub const GADGET_BYTE: u8 = 0xF4;

/// The identity-mapped guest VM. Constructed once at process start, shared through an
/// `Arc` by every guest thread's `Vcpu` and the memory manager.
pub struct GuestVm {
    vm: Vm,
    span: u64,
}

impl GuestVm {
    /// Build a `GuestVm` over a fresh identity `mmap` of `[GUEST_BASE, span)`: select
    /// the execution backend from [`BACKEND_ENV`] (`jit` default | `interp`), pre-map
    /// the whole arena RWX/RAM, write the HLT gadget, select the Jaguar (x86-64-v2) ISA
    /// + `Fast` consistency, then hand back an `Arc`.
    ///
    /// Panics if the fixed `mmap` collides (a boot-time layout error — see `reserve_at`).
    pub fn new(span: u64) -> Arc<GuestVm> {
        // Production tier-up: hotness-gated background compile (doc-1 dec 8).
        Self::build(span, TierUp::HotBackground)
    }

    /// Test-only variant that forces **eager, foreground** JIT tier-up
    /// (`set_tier_up_after(Some(0))`, background off), so a block is compiled after its
    /// first execution and the second run executes JIT-compiled code deterministically —
    /// the recipe the x86jit watched-dirty tests use. Lets the DirtySource integration
    /// test exercise the JIT store path without racing the background compiler. Not on
    /// the boot path; hidden from docs.
    #[doc(hidden)]
    pub fn new_eager_jit_for_test(span: u64) -> Arc<GuestVm> {
        Self::build(span, TierUp::EagerForeground)
    }

    fn build(span: u64, tier_up: TierUp) -> Arc<GuestVm> {
        let ram = reserve_at(GUEST_BASE, span);
        debug_assert_eq!(ram.guest_base, GUEST_BASE);
        debug_assert_eq!(
            ram.ptr as u64, GUEST_BASE,
            "identity mmap lands at guest_base"
        );

        // `VmConfig::reserved(span)` sets `MemoryModel::Reserved { span }` +
        // `MemConsistency::Fast` (the field lives on VmConfig; Fast is its default and
        // exactly what we want on an x86 host — identical codegen across tiers, dec 8).
        let mut cfg = VmConfig::reserved(span);
        cfg.consistency = x86jit_core::MemConsistency::Fast;

        // Backend selection: `UNEMUPS4_BACKEND=jit|interp`, one binary, no
        // feature matrix. `jit` is the default (native-speed execution); `interp` keeps
        // the interpreter for debugging and as the differential oracle. The JIT compiles
        // guest blocks into its own executable arena and reaches this RW-mapped guest RAM
        // only through baked `host_base + guest_addr` accesses, so the pre-mapped-RWX
        // arena needs no W^X handling here (guard pages are a deferred follow-up, dec 5).
        let kind = BackendKind::from_env();
        let backend: Box<dyn Backend> = match kind {
            BackendKind::Jit => Box::new(JitBackend::new()),
            BackendKind::Interp => Box::new(InterpreterBackend),
        };
        info!("guest execution backend: {kind:?} (via {BACKEND_ENV})");
        let mut vm = Vm::with_backend_host_ram(cfg, backend, ram);

        // Hotness-gated background tier-up for the JIT (doc-1 dec 8, mirroring
        // x86jit-cli): interpret a block `TIER_UP_AFTER` times, then compile it off the
        // vcpu on the backend's worker thread and swap it in when ready — the hot
        // dispatch never stalls for a compile. `Vcpu::run` drains completed compiles
        // itself; `JitBackend`'s `Drop` joins the worker when this VM's `Arc` is released,
        // so no explicit `wait_idle`/handle lifecycle is needed (that is a test-only
        // determinism lever). No-op / harmless on the interpreter (an `Unsupported`
        // backend degrades to inline). Both setters are `&mut self`, so — like `map`
        // below — they run before the `Arc` wrap.
        if kind == BackendKind::Jit {
            match tier_up {
                TierUp::HotBackground => {
                    vm.set_tier_up_after(Some(TIER_UP_AFTER));
                    vm.set_tier_up_background(true);
                }
                // Deterministic test lever: compile after the first execution, in the
                // foreground, so the very next run executes the JIT-compiled block.
                TierUp::EagerForeground => {
                    vm.set_tier_up_after(Some(0));
                    vm.set_tier_up_background(false);
                }
            }
        }

        // Jaguar ISA level (x86-64-v2) — the PS4 CPU's advertised feature set (dec 8).
        vm.set_guest_cpu_features(GuestCpuFeatures::v2());

        // Pre-map the entire arena RWX/RAM in one shot. `Vm::map` is `&mut self`; this
        // is the last `&mut` use before the `Arc`. Runtime map/unmap is handled by the
        // software VMA layer over this already-backed span, never re-mapping.
        let arena_len = (span - GUEST_BASE) as usize;
        vm.map(GUEST_BASE, arena_len, Prot::RWX, RegionKind::Ram)
            .expect("pre-map of the whole guest arena must succeed");

        // Write the HLT gadget used as the return address for every guest call.
        vm.write_bytes(GADGET_ADDR, &[GADGET_BYTE])
            .expect("gadget write into the freshly-mapped arena must succeed");

        Arc::new(GuestVm { vm, span })
    }

    /// Exclusive top guest address.
    #[inline]
    pub fn span(&self) -> u64 {
        self.span
    }

    /// Low cutoff of the guest address space (`GUEST_BASE`).
    #[inline]
    pub fn guest_base(&self) -> u64 {
        GUEST_BASE
    }

    /// Guest address of the HLT gadget.
    #[inline]
    pub fn gadget_addr(&self) -> u64 {
        GADGET_ADDR
    }

    /// Borrow the underlying x86jit `Vm` (needed to spawn vcpus and drive `run`).
    #[inline]
    pub fn vm(&self) -> &Vm {
        &self.vm
    }

    /// Spawn a fresh `Vcpu` over this VM. One per guest thread / nested call.
    #[inline]
    pub fn new_vcpu(&self) -> x86jit_core::Vcpu {
        self.vm.new_vcpu()
    }

    /// Write bytes into guest memory **through the VM** so SMC tracking observes the
    /// write (loader relocations, handler-written data, etc. — doc-1 dec 5).
    #[inline]
    pub fn write_bytes(&self, guest_addr: u64, bytes: &[u8]) -> Result<(), x86jit_core::MemError> {
        self.vm.write_bytes(guest_addr, bytes)
    }

    /// Read bytes out of guest memory through the VM.
    #[inline]
    pub fn read_bytes(&self, guest_addr: u64, buf: &mut [u8]) -> Result<(), x86jit_core::MemError> {
        self.vm.read_bytes(guest_addr, buf)
    }

    /// Snapshot the x86jit translation-cache + backend counters for the aggregate
    /// profiler. Reads the existing `vm.cache.*` / `vm.backend.compile_ns()`
    /// pub API — no x86jit changes. `GuestVm::vm` is private, so this accessor is how
    /// the app-side dump thread reaches them through the shared `Arc<GuestVm>`.
    /// Observability only; all reads are relaxed and off the hot path.
    pub fn jit_counters(&self) -> JitCounters {
        let c = &self.vm.cache;
        JitCounters {
            hits: c.hits(),
            misses: c.misses(),
            chained: c.chained(),
            regions: c.regions(),
            ibtc_filled: c.ibtc_filled(),
            tier_bg_published: c.tier_bg_published(),
            tier_bg_rejected: c.tier_bg_rejected(),
            compile_ns: self.vm.backend.compile_ns(),
        }
    }
}

/// A snapshot of x86jit translation-cache + backend counters. Plain data so
/// the profiler dump can print it without touching x86jit types directly.
#[derive(Clone, Copy, Debug, Default)]
pub struct JitCounters {
    pub hits: u64,
    pub misses: u64,
    pub chained: u64,
    pub regions: u64,
    pub ibtc_filled: u64,
    pub tier_bg_published: u64,
    pub tier_bg_rejected: u64,
    /// Total backend compile time (ns). `0` on the interpreter backend.
    pub compile_ns: u64,
}
