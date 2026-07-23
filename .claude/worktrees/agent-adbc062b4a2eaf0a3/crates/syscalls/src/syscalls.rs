// SPDX-License-Identifier: GPL-3.0-only
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
