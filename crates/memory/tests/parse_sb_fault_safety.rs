//! Regression: `parse_sb` over the REAL VMA-aware read view must reject a bogus /
//! near-unmapped shader address cleanly (no host SIGSEGV).
//!
//! The `.sb` parser's rejection design assumes reads fault at end-of-mapping. Over the
//! real [`VmMemoryManager`] the whole guest arena is host-mapped once, so an *unbounded*
//! `read_bytes` never faults and `scan_for_magic` would walk up to 1 MiB of raw host
//! memory — a SIGSEGV once it runs off the arena top. The fix routes the parser through
//! [`VmMemoryManager::shader_read_view`], whose `read_bytes` is range-validated against
//! the VMA set. These tests exercise exactly that seam.
//!
//! Same single-live-VM discipline as `vm_backend.rs`: the fixed identity mmap is
//! process-global, so VM construction serializes behind a `Mutex`.

use std::sync::Mutex;

use ps4_core::memory::{MemoryProtection, VirtualMemoryManager};
use ps4_cpu::GuestVm;
use ps4_gnm::shader::sb::{SbParseError, parse_sb};
use ps4_memory::VmMemoryManager;

const SPAN: u64 = 0x0080_0000;

static VM_LOCK: Mutex<()> = Mutex::new(());

fn with_manager<F: FnOnce(&mut VmMemoryManager)>(f: F) {
    let _guard = VM_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let vm = GuestVm::new(SPAN);
    let mut mgr = VmMemoryManager::new(vm);
    f(&mut mgr);
}

const RW: MemoryProtection = MemoryProtection::from_bits_truncate(
    MemoryProtection::READ.bits() | MemoryProtection::WRITE.bits(),
);

#[test]
fn parse_sb_over_unmapped_gap_rejects_cleanly() {
    // A shader address pointing into an in-arena but UNMAPPED gap (no VMA). Through the
    // VMA-aware view every read faults at once, so the parser rejects without touching
    // host memory. (Over the raw manager this same read would silently succeed.)
    with_manager(|mgr| {
        let bogus = 0x0050_0000u64; // inside [guest_base, span), but never mapped
        assert!(mgr.is_memory_free(bogus, 0x1000));
        let view = mgr.shader_read_view();
        let err = parse_sb(bogus, &view).unwrap_err();
        assert!(
            matches!(err, SbParseError::MagicNotFound | SbParseError::MemoryFault),
            "unmapped shader addr must reject cleanly, got {err:?}"
        );
    });
}

#[test]
fn parse_sb_near_vma_end_does_not_overread() {
    // Map a small region and fill it with garbage that contains NO OrbShdr magic. The
    // parser scans forward from a code_start near the region's end; the moment the scan
    // window would cross the VMA boundary the range-validated read faults, so the scan
    // stops at the mapping end instead of walking ~1 MiB into the unmapped arena (which,
    // near the arena top, is a host SIGSEGV). Result: a clean rejection, no crash.
    with_manager(|mgr| {
        let base = 0x0040_0000u64;
        let size = 0x2000usize;
        mgr.map(base, size, RW, Some("shader_region")).unwrap();
        // Fill with a non-magic byte pattern so the scan finds nothing legitimate.
        let garbage = vec![0xABu8; size];
        mgr.write_bytes(base, &garbage).unwrap();

        // Start the scan a few bytes before the VMA end: the very first full window
        // already runs past `base+size` into the unmapped gap.
        let code_start = base + size as u64 - 8;
        let view = mgr.shader_read_view();
        let err = parse_sb(code_start, &view).unwrap_err();
        assert!(
            matches!(err, SbParseError::MagicNotFound | SbParseError::MemoryFault),
            "near-boundary garbage must reject cleanly, got {err:?}"
        );
    });
}

#[test]
fn parse_sb_valid_blob_inside_vma_still_parses_through_view() {
    // Sanity: the range-validated view does not break the happy path — a real OrbShdr
    // blob fully inside a mapped region parses through the same VMA-bounded seam.
    with_manager(|mgr| {
        let base = 0x0040_0000u64;
        let code_len = 0x40u32;
        // Assemble a minimal VS `.sb`: code_len filler bytes + a 28-byte header. The parser
        // requires the code to end in s_endpgm (0xBF810000), so stamp it as the last dword.
        let mut blob = vec![0x90u8; code_len as usize];
        let n = code_len as usize;
        blob[n - 4..n].copy_from_slice(&0xBF81_0000u32.to_le_bytes());
        blob.extend_from_slice(&build_header(1 /* VS */, code_len));
        // Map a region large enough to hold the blob, then write it in.
        mgr.map(base, 0x1000, RW, Some("shader_region")).unwrap();
        mgr.write_bytes(base, &blob).unwrap();

        let view = mgr.shader_read_view();
        let sb = parse_sb(base, &view).expect("valid blob parses through the view");
        assert_eq!(sb.code_range, base..(base + code_len as u64));
    });
}

/// Build a 28-byte `ShaderBinaryInfo` header for the given stage + code length,
/// mirroring the packed layout the parser expects (magic + bitfield word).
fn build_header(m_type: u32, code_len: u32) -> Vec<u8> {
    let mut h = Vec::with_capacity(28);
    h.extend_from_slice(b"OrbShdr");
    h.push(1); // m_version
    // bitfield word at 0x08: m_type at bits 2..6, m_length at bits 8..32.
    let word = ((m_type & 0xF) << 2) | ((code_len & 0x00FF_FFFF) << 8);
    h.extend_from_slice(&word.to_le_bytes());
    h.push(0x03); // m_chunkUsageBaseOffsetInDW
    h.push(0x02); // m_numInputUsageSlots
    h.push(0x00); // flags
    h.push(0x00); // m_reserved3
    h.extend_from_slice(&0u32.to_le_bytes()); // hash0
    h.extend_from_slice(&0u32.to_le_bytes()); // hash1
    h.extend_from_slice(&0u32.to_le_bytes()); // crc32
    assert_eq!(h.len(), 28);
    h
}
