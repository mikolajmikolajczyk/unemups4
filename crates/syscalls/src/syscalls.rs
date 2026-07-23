// SPDX-License-Identifier: GPL-3.0-only
//! `SyscallId` — the name / NID / numeric-id lookup surface for PS4 library
//! imports.
//!
//! This module is pure lookup logic (binary search over sorted rodata slices);
//! it holds no table data itself. The `MAP_*` tables it searches are emitted at
//! build time by `build.rs` into `generated_syscalls.rs` (pulled in below) from
//! `data/wiki_syscalls.txt` and `data/ps4_names.txt`.
//!
//! The **NID** string paired with each symbol is the OpenOrbis OELF/SELF NID
//! hash: `SHA-1(symbol_name ++ nidSuffixKey)`, first 8 bytes byte-reversed,
//! base64-encoded with the trailing `=` dropped and `/` rewritten to `-`. That
//! algorithm is documented in the OpenOrbis OELF specification —
//! `docs/MD/OELF Specification/"PS4 ELF Specification - Dynlib Data.md"`, section
//! "Calculating NID hashes". `build.rs` applies it with the salt
//! `518D64A635DED8C1E6B039B1C3E55230`; the `nid_table_matches_openorbis_oelf_spec`
//! test below pins the generated NID this module returns against the same spec's
//! own worked example (the usleep example project lists `sceKernelUsleep()`'s NID
//! as `1jfXLRVzisc`).
//!
//! The numeric-id assignment (fixed ids from the wiki list, synthetic ids from
//! 10000 up for the dynamic-name list) and the "Unknown"/"" fall-backs are this
//! emulator's own bookkeeping, not a PS4 fact.
use std::fmt;

// Pull in the generated tables and constants
include!(concat!(env!("OUT_DIR"), "/generated_syscalls.rs"));

#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SyscallId(pub u64);

// The constants live inside generated_syscalls.rs in the impl SyscallId block

impl SyscallId {
    /// Binary search by name (O(log n))
    pub fn from_symbol_name(name: &str) -> Option<Self> {
        MAP_BY_NAME
            .binary_search_by_key(&name, |&(n, _)| n)
            .ok()
            .map(|idx| Self(MAP_BY_NAME[idx].1))
    }

    /// Binary search by NID (O(log n))
    pub fn from_nid(nid: &str) -> Option<Self> {
        MAP_BY_NID
            .binary_search_by_key(&nid, |&(n, _)| n)
            .ok()
            .map(|idx| Self(MAP_BY_NID[idx].1))
    }

    pub fn from_raw(id: u64) -> Option<Self> {
        Some(Self(id))
    }

    /// Binary search by ID (O(log n))
    pub fn as_str(&self) -> &'static str {
        MAP_BY_ID
            .binary_search_by_key(&self.0, |&(id, _)| id)
            .ok()
            .map(|idx| MAP_BY_ID[idx].1)
            .unwrap_or("Unknown")
    }

    pub fn name(&self) -> &'static str {
        self.as_str()
    }

    /// NID for this syscall id, or "" if unknown. Binary search over a sorted
    /// rodata slice (was a 94k-arm match in generated_syscalls.rs; see TASK-19).
    pub fn nid(&self) -> &'static str {
        MAP_ID_TO_NID
            .binary_search_by_key(&self.0, |&(id, _)| id)
            .ok()
            .map(|idx| MAP_ID_TO_NID[idx].1)
            .unwrap_or("")
    }
    pub fn id(&self) -> u64 {
        self.0
    }

    pub fn is_known(&self) -> bool {
        MAP_BY_ID
            .binary_search_by_key(&self.0, |&(id, _)| id)
            .is_ok()
    }
}

impl fmt::Display for SyscallId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.as_str(), self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pins the generated NID table this module searches to the OpenOrbis OELF/SELF
    /// NID algorithm. The NID string is `SHA-1(symbol_name ++ nidSuffixKey)`, first 8
    /// bytes byte-reversed, base64'd with the trailing `=` dropped and `/` → `-`,
    /// documented in the OpenOrbis OELF spec `docs/MD/OELF Specification/"PS4 ELF
    /// Specification - Dynlib Data.md"` (section "Calculating NID hashes"). The right-
    /// hand literal below is that spec's own worked example: its usleep example project
    /// lists the NID for `sceKernelUsleep()` as `1jfXLRVzisc` (the table dump reads
    /// bytes `31 6A 66 58 4C 52 56 7A 69 73 63` = "1jfXLRVzisc"). That value matches
    /// only if `build.rs`'s salt `518D64A635DED8C1E6B039B1C3E55230` and the byte order
    /// are the ones the spec describes; this test fails if either drifts.
    #[test]
    fn nid_table_matches_openorbis_oelf_spec() {
        // sceKernelUsleep is present in data/ps4_names.txt, so it lands in the
        // generated MAP_* tables this module binary-searches.
        let id = SyscallId::from_symbol_name("sceKernelUsleep")
            .expect("sceKernelUsleep should be in the generated syscall table");
        assert_eq!(
            id.nid(),
            "1jfXLRVzisc",
            "NID for sceKernelUsleep must match the OpenOrbis OELF spec's usleep example"
        );
        // NID → id → name resolves back to the same symbol (both lookup directions
        // agree with the table).
        let by_nid = SyscallId::from_nid("1jfXLRVzisc").expect("NID 1jfXLRVzisc should resolve");
        assert_eq!(by_nid, id);
        assert_eq!(id.name(), "sceKernelUsleep");
    }

    /// Lookup round-trips and unknown-id behaviour (this emulator's own bookkeeping:
    /// name↔id are inverse over the generated tables, and an id not in the table reads
    /// back as not-known / "Unknown").
    #[test]
    fn lookup_roundtrips_and_unknown_is_none() {
        let id = SyscallId::from_symbol_name("sceKernelUsleep").unwrap();
        assert_eq!(SyscallId::from_symbol_name(id.name()), Some(id));

        // A name that is not a PS4 symbol resolves to nothing.
        assert_eq!(
            SyscallId::from_symbol_name("definitely_not_a_ps4_symbol"),
            None
        );

        // An id absent from the table is not "known" and prints as "Unknown".
        let bogus = SyscallId(0xFFFF_FFFF_FFFF_FFFF);
        assert!(!bogus.is_known());
        assert_eq!(bogus.as_str(), "Unknown");
        assert_eq!(bogus.nid(), "");
    }
}
