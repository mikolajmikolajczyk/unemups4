// SPDX-License-Identifier: GPL-3.0-only
use crate::SyscallId;

// all fields 'static so the table lands in .rodata
#[derive(Debug, Clone, Copy)]
pub struct SyscallMeta {
    pub name: &'static str,
    pub arg_count: usize,
    // (type, arg_name) pairs
    pub args: &'static [(&'static str, &'static str)],
}

// defines METADATA_TABLE
include!(concat!(env!("OUT_DIR"), "/generated_metadata.rs"));

impl SyscallId {
    /// metadata (arg types/names) for a syscall, binary search over the sorted table.
    pub fn get_metadata(&self) -> Option<&'static SyscallMeta> {
        let name = self.name();
        METADATA_TABLE
            .binary_search_by_key(&name, |meta| meta.name)
            .ok()
            .map(|idx| &METADATA_TABLE[idx])
    }
}
