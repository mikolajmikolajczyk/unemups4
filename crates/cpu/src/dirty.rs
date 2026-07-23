//! The real, x86jit-backed [`DirtySource`] (doc-2 §8.3).
//!
//! Wraps the shared `Arc<GuestVm>` and forwards to the x86jit VM's watched-range
//! facility (`watch_range` / `unwatch_range` / `take_dirty_ranges`), which records guest
//! writes — including JIT-inlined stores — to watched pages and drains them at a submit
//! boundary. This is the `ps4-cpu` half of the seam declared in `ps4-core`; the app wires
//! it at boot via [`ps4_core::dirty::register_dirty_source`], so `ps4-gnm` reaches it
//! without depending on `ps4-cpu`.

use std::sync::Arc;

use ps4_core::dirty::DirtySource;

use crate::guest_vm::GuestVm;

/// [`DirtySource`] over the guest VM's watched-range dirty tracking (the real impl).
///
/// All three methods are `&self` on the x86jit `Vm`, so this needs only a shared
/// `Arc<GuestVm>` — the same handle every guest thread already holds. Poll-and-drain at
/// submit boundaries only; zero write-path cost when nothing is watched.
pub struct VmDirtySource {
    vm: Arc<GuestVm>,
}

impl VmDirtySource {
    pub fn new(vm: Arc<GuestVm>) -> VmDirtySource {
        VmDirtySource { vm }
    }
}

impl DirtySource for VmDirtySource {
    fn watch(&self, addr: u64, size: u64) {
        self.vm.vm().watch_range(addr, size);
    }

    fn unwatch(&self, addr: u64, size: u64) {
        self.vm.vm().unwatch_range(addr, size);
    }

    fn take_dirty(&self) -> Vec<(u64, u64)> {
        self.vm.vm().take_dirty_ranges()
    }
}
