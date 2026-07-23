//! libSceGnmDriver HLE (doc-4 §1). Gnm/Gnmx are statically linked into
//! games; the only interceptable surface is libSceGnmDriver's submit/draw/dispatch
//! entry points, which hand over guest-memory PM4 command buffers (doc-2 §3).
//!
//! These handlers are *thin* (doc-4 §1): the submit stubs extract the DCB/CCB
//! pointer/size ranges from guest memory and record them into the `GnmDriver`
//! (in `ps4-gnm`), which retains them for the PM4 trace decoder. No PM4
//! is decoded or executed here — everything is log-and-return-success. Guest ptrs
//! are identity-mapped (guest ptr == host ptr, doc-2 §1), so the arrays are read
//! directly.
//!
//! The handlers are split by cohesion across submodules; the `#[ps4_syscall]` +
//! `inventory` registration is location-independent, so the split is purely
//! organizational:
//!
//! - [`submit`] — submit / submit-and-flip / submit-done + guest-array readers.
//! - [`draw`] — draw / dispatch / compute-queue command builders.
//! - [`shader_bind`] — embedded-shader and `sceGnmSet*Shader` register binds.
//! - [`hwstate`] — HW-state-init preamble, submit gating, markers, cache flush.

pub mod draw;
pub mod hwstate;
pub mod shader_bind;
pub mod submit;

#[cfg(test)]
mod tests {
    use super::hwstate::{sce_gnm_are_submits_allowed, sce_gnm_draw_init_default_hardware_state};
    use super::submit::sce_gnm_submit_command_buffers;
    use ps4_gnm::driver::driver;
    use ps4_syscalls::SyscallId;
    use std::sync::Mutex;

    /// Serializes the tests that drain the process-global `driver()`, since
    /// `cargo test` runs them concurrently and they share that OnceLock.
    static SUBMIT_TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn are_submits_allowed_returns_one() {
        assert_eq!(sce_gnm_are_submits_allowed(), 1);
    }

    #[test]
    fn init_default_hw_state_returns_size() {
        assert_eq!(sce_gnm_draw_init_default_hardware_state(0x1000, 42), 42);
    }

    #[test]
    fn submit_handler_records_ranges_in_driver() {
        let _guard = SUBMIT_TEST_LOCK.lock().unwrap();
        // Build guest-memory-style arrays (identity-mapped: host addr == guest ptr).
        // Address arrays are 64-bit `void*[]`; size arrays are `uint32_t*`.
        let dcb_addrs: [u64; 1] = [0xDEAD_0000];
        let dcb_sizes: [u32; 1] = [512];
        let ccb_addrs: [u64; 1] = [0xBEEF_0000];
        let ccb_sizes: [u32; 1] = [128];

        // Drain any prior state so this test is order-independent.
        driver().lock().unwrap().take_submissions();

        let ret = sce_gnm_submit_command_buffers(
            1,
            dcb_addrs.as_ptr() as u64,
            dcb_sizes.as_ptr() as u64,
            ccb_addrs.as_ptr() as u64,
            ccb_sizes.as_ptr() as u64,
        );
        assert_eq!(ret, 0);

        let recorded = driver().lock().unwrap().take_submissions();
        assert_eq!(recorded.len(), 1);
        let r = recorded[0];
        assert_eq!(r.dcb_ptr, 0xDEAD_0000);
        assert_eq!(r.dcb_size, 512);
        assert_eq!(r.ccb_ptr, 0xBEEF_0000);
        assert_eq!(r.ccb_size, 128);
        assert!(!r.flip);
    }

    /// A malloc'd command buffer lives above 4 GB; the address arrays are 64-bit
    /// `void*[]`, so the high bits must survive (reading them as 32-bit truncates
    /// the buffer to all-zeros).
    #[test]
    fn submit_handler_preserves_high_addresses() {
        let _guard = SUBMIT_TEST_LOCK.lock().unwrap();
        // OpenOrbis malloc-style pointers: identity-mapped, above 4 GB.
        let dcb_addrs: [u64; 1] = [0x4_0021_4000];
        let dcb_sizes: [u32; 1] = [512];
        let ccb_addrs: [u64; 1] = [0x5_00AB_0000];
        let ccb_sizes: [u32; 1] = [128];

        driver().lock().unwrap().take_submissions();

        let ret = sce_gnm_submit_command_buffers(
            1,
            dcb_addrs.as_ptr() as u64,
            dcb_sizes.as_ptr() as u64,
            ccb_addrs.as_ptr() as u64,
            ccb_sizes.as_ptr() as u64,
        );
        assert_eq!(ret, 0);

        let recorded = driver().lock().unwrap().take_submissions();
        assert_eq!(recorded.len(), 1);
        let r = recorded[0];
        assert_eq!(r.dcb_ptr, 0x4_0021_4000, "high DCB address truncated");
        assert_eq!(r.dcb_size, 512);
        assert_eq!(r.ccb_ptr, 0x5_00AB_0000, "high CCB address truncated");
        assert_eq!(r.ccb_size, 128);
    }

    /// The seam the PM4 decoder consumes: after HLE bootstrap wires the inventory defs into
    /// the syscall table, the registered stub is resolvable by the Gnm NIDs.
    #[test]
    fn key_gnm_nids_resolve_to_registered_handlers() {
        crate::init();
        ps4_libs_bootstrap_for_test();

        for nid in [
            "zwY0YV91TTI", // sceGnmSubmitCommandBuffers
            "xbxNatawohc", // sceGnmSubmitAndFlipCommandBuffers
            "yvZ73uQUqrk", // sceGnmSubmitDone
            "HlTPoZ-oY7Y", // sceGnmDrawIndex
            "GGsn7jMTxw4", // sceGnmDrawIndexAuto
            "0BzLGljcwBo", // sceGnmDispatchDirect
            "29oKvKXzEZo", // sceGnmMapComputeQueue
            "bX5IbRvECXk", // sceGnmDingDong
            "+AFvOEXrKJk", // sceGnmSetEmbeddedVsShader
            "X9Omw9dwv5M", // sceGnmSetEmbeddedPsShader
            "gAhCn6UiU4Y", // sceGnmSetVsShader
            "bQVd5YzCal0", // sceGnmSetPsShader
            "KXltnCwEJHQ", // sceGnmSetCsShader
            "FUHG8sQ3R58", // sceGnmSetEsShader
            "UJwNuMBcUAk", // sceGnmSetGsShader
            "VJNjFtqiF5w", // sceGnmSetHsShader
            "vckdzbQ46SI", // sceGnmSetLsShader
        ] {
            let id =
                SyscallId::from_nid(nid).unwrap_or_else(|| panic!("NID {nid} has no SyscallId"));
            assert!(
                crate::get_handler(id.0).is_some(),
                "NID {nid} (id {}) has no registered handler",
                id.0
            );
        }
    }

    /// Register every collected HLE def into the syscall table so `get_handler`
    /// sees the Gnm stubs (mirrors what `HleBootstrap::install` does, minus the
    /// guest-memory stub emission which needs a full memory manager).
    fn ps4_libs_bootstrap_for_test() {
        for def in inventory::iter::<crate::registry::HleSyscallDef> {
            crate::register_handler(def.id.0, def.handler);
        }
    }
}
