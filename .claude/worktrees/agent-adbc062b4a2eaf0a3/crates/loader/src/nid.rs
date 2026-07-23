//! SCE library/module id encoding (doc-5 K2).
//!
//! Retail PS4 symbol names are `nid#library#module`, where `library` and
//! `module` are small integer ids encoded into Sony's base64 alphabet. This
//! module carries only that id encoding — [`encode_id`] — because
//! `ps4-syscalls` does not expose it (it maps whole symbol names, not the
//! per-image lib/module ids the `#`-split resolves against).
//!
//! The forward NID hash (`SHA-1(name || salt)` → base64) is **not** here: it is
//! the same algorithm `ps4-syscalls` runs at build time (identical salt
//! `518D64A635DED8C1E6B039B1C3E55230`), so the canonical NID for any HLE export
//! is available via `ps4_syscalls::SyscallId::nid()`, and a retail import's NID
//! resolves through the generated `MAP_BY_NID` (`SyscallId::from_nid`) with no
//! second implementation. It is a hash, not decryption (doc-5 K1): no keys, no
//! SAMU.

/// Sony's base64 alphabet (note the trailing `+-`, not `+/`).
const CODES: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+-";

/// Encode a small integer library/module id into Sony base64 (shadPS4
/// `EncodeId`). The `symbol#library#module` name encodes the library and module
/// ids this way; matching a decoded id back to a `DT_SCE_*_LIB` / module entry
/// keys on this encoding.
pub fn encode_id(id: u64) -> String {
    let mut out = String::new();
    if id < 0x40 {
        out.push(CODES[id as usize] as char);
    } else if id < 0x1000 {
        out.push(CODES[((id >> 6) & 0x3f) as usize] as char);
        out.push(CODES[(id & 0x3f) as usize] as char);
    } else {
        out.push(CODES[((id >> 12) & 0x3f) as usize] as char);
        out.push(CODES[((id >> 6) & 0x3f) as usize] as char);
        out.push(CODES[(id & 0x3f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_id_matches_shadps4_widths() {
        // < 0x40 -> 1 char; < 0x1000 -> 2 chars; else 3.
        assert_eq!(encode_id(0), "A");
        assert_eq!(encode_id(1), "B");
        assert_eq!(encode_id(0x3f), "-");
        assert_eq!(encode_id(0x40).len(), 2);
        assert_eq!(encode_id(0xfff).len(), 2);
        assert_eq!(encode_id(0x1000).len(), 3);
    }

    #[test]
    fn encode_id_exact_multichar_values() {
        // 2-char branch: 0x40 = 0b000001_000000 -> CODES[1] CODES[0] = "BA".
        assert_eq!(encode_id(0x40), "BA");
        // top of the 2-char range: 0xfff = 0b111111_111111 -> CODES[63] CODES[63] = "--".
        assert_eq!(encode_id(0xfff), "--");
        // 3-char branch: 0x1000 = 0b000001_000000_000000 -> CODES[1] CODES[0] CODES[0] = "BAA".
        assert_eq!(encode_id(0x1000), "BAA");
        // exercise all three 6-bit groups distinctly: 0b000010_000011_000100.
        let id = (2u64 << 12) | (3u64 << 6) | 4u64;
        assert_eq!(encode_id(id), "CDE"); // CODES[2],CODES[3],CODES[4]
    }

    /// The loader relies on `ps4-syscalls` producing canonical NIDs (same salt +
    /// SHA-1 bit-slice as shadPS4 `StringToNid`). Guard that here so a change to
    /// the generated table can't silently break retail import resolution.
    #[test]
    fn ps4_syscalls_nids_are_canonical() {
        use ps4_syscalls::SyscallId;
        assert_eq!(
            SyscallId::from_symbol_name("sceKernelAllocateDirectMemory")
                .unwrap()
                .nid(),
            "rTXw65xmLIA"
        );
        assert_eq!(
            SyscallId::from_symbol_name("sceKernelUsleep")
                .unwrap()
                .nid(),
            "1jfXLRVzisc"
        );
    }
}
