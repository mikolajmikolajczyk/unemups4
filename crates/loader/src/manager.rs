use ps4_core::img::{Import, Section};
use std::{collections::HashMap, sync::Arc};

pub type ModuleHandle = i32;

pub struct Module {
    pub id: ModuleHandle,
    pub name: String,
    pub path: String,

    pub base_addr: u64,
    pub memory_size: usize,
    pub entry_point: u64,
    pub exports: HashMap<String, u64>,
    pub sections: Vec<Section>,
    pub imports: Vec<Import>,

    pub is_hle: bool,
}

impl Module {
    pub fn new(id: ModuleHandle, name: &str, exports: HashMap<String, u64>) -> Self {
        Module {
            id,
            name: name.to_string(),
            path: format!("/system/common/lib/{}", name),
            base_addr: 0,
            memory_size: 0,
            entry_point: 0,
            exports,
            imports: Vec::new(),
            is_hle: false,
            sections: Vec::new(),
        }
    }
    pub fn new_hle(id: ModuleHandle, name: &str, exports: HashMap<String, u64>) -> Self {
        Module {
            id,
            name: name.to_string(),
            path: format!("/system/common/lib/{}", name),
            base_addr: 0,
            memory_size: 0,
            entry_point: 0,
            exports,
            imports: Vec::new(),
            is_hle: true,
            sections: Vec::new(),
        }
    }
}

/// Render an export-table key for a human. Retail modules key exports by NID, so try the
/// generated NID → name table first and keep the NID alongside the recovered name so the
/// attribution stays auditable. A key that is not a NID (homebrew keeps plain names) is
/// printed as-is; an unrecognised NID is printed as `NID <nid>` rather than passed off as
/// a symbol name — a wrong name in a fault report is worse than no name.
fn display_export_name(key: &str) -> String {
    match ps4_syscalls::SyscallId::from_nid(key).map(|s| s.as_str()) {
        Some(name) if !name.is_empty() && name != "Unknown" => format!("{name} [NID {key}]"),
        _ if looks_like_nid(key) => format!("NID {key}"),
        _ => key.to_string(),
    }
}

/// Is `key` a Sony NID — 11 characters of the base64 alphabet the NID encoding uses? Only
/// a presentation heuristic (it picks the `NID <key>` wording over a bare key); a false
/// negative just prints the key unlabelled.
fn looks_like_nid(key: &str) -> bool {
    key.len() == 11
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'-')
}

pub struct ModuleAllocator {
    current_addr: u64,
    align: u64,
}

impl ModuleAllocator {
    pub fn new(start: u64) -> Self {
        ModuleAllocator {
            current_addr: start,
            align: 0x4000,
        }
    }

    pub fn allocate(&mut self, size: usize) -> u64 {
        let addr = self.current_addr;
        let size_aligned = (size as u64 + self.align - 1) & !(self.align - 1);
        self.current_addr = addr + size_aligned;
        addr
    }
}

pub struct ModuleManager {
    pub modules: HashMap<ModuleHandle, Arc<Module>>,
    pub name_map: HashMap<String, ModuleHandle>,
    pub allocator: ModuleAllocator,
    pub hle_exports: HashMap<String, HashMap<String, u32>>,
    next_id: ModuleHandle,
}

impl Default for ModuleManager {
    fn default() -> Self {
        Self::new()
    }
}

impl ModuleManager {
    pub fn new() -> Self {
        ModuleManager {
            modules: HashMap::new(),
            name_map: HashMap::new(),
            allocator: ModuleAllocator::new(0x400000),
            hle_exports: HashMap::new(),
            next_id: 1,
        }
    }

    pub fn get_next_handle(&mut self) -> ModuleHandle {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    pub fn register_module(&mut self, module: Module) -> Arc<Module> {
        let id = module.id;
        let name = module.name.clone();
        let arc = Arc::new(module);

        self.modules.insert(id, arc.clone());
        self.name_map.insert(name, id);

        arc
    }

    pub fn get_by_name(&self, name: &str) -> Option<Arc<Module>> {
        self.name_map
            .get(name)
            .and_then(|id| self.modules.get(id).cloned())
    }

    pub fn resolve_symbol(&self, symbol_name: &str) -> Option<u64> {
        // `Module::exports` holds ABSOLUTE addresses: the linker base-shifts real
        // module exports before registering, and HLE stub exports are already
        // absolute (their module base is 0). Return the stored value as-is; adding
        // `base_addr` again would double-count the base for a real (non-HLE) module.
        for module in self.modules.values() {
            if let Some(&addr) = module.exports.get(symbol_name) {
                return Some(addr);
            }
        }
        None
    }
    /// Attribute a code address to `module!symbol +offset`, for a fault report (task-113.2).
    ///
    /// A raw `libc +0x4ea0d` names nothing actionable; `libc!strlen +0x2d` names the
    /// culprit. Finds the loaded module whose image contains `addr`, then the largest
    /// export at or below it. PS4 modules export by **NID**, so the name is recovered
    /// through the generated NID table ([`display_export_name`]) — and when it cannot be,
    /// the NID itself is reported rather than a guess.
    ///
    /// Best-effort and deliberately un-indexed: exports carry no `st_size`, so "largest
    /// export ≤ addr" is the only available answer and it can be far off inside a module
    /// that exports little (a stripped retail eboot exports almost nothing — its frames
    /// will report the module and offset, which is the honest answer). Called only on a
    /// fatal fault, so the linear scan costs nothing steady-state.
    ///
    /// `None` when `addr` belongs to no loaded module. HLE stub modules are skipped: their
    /// `base_addr`/`memory_size` are zero, so they span no address range.
    pub fn nearest_symbol(&self, addr: u64) -> Option<String> {
        let module = self.modules.values().find(|m| {
            !m.is_hle
                && m.memory_size > 0
                && addr >= m.base_addr
                && addr < m.base_addr + m.memory_size as u64
        })?;

        // `Module::exports` holds absolute addresses (see `resolve_symbol`), so compare
        // against `addr` directly.
        match module
            .exports
            .iter()
            .filter(|&(_, &sym)| sym <= addr)
            .max_by_key(|&(_, &sym)| sym)
        {
            Some((key, &sym)) => Some(format!(
                "{}!{} +{:#x}",
                module.name,
                display_export_name(key),
                addr - sym
            )),
            None => Some(format!(
                "{} +{:#x} (no exported symbol at or below this address; {} exports — stripped?)",
                module.name,
                addr - module.base_addr,
                module.exports.len()
            )),
        }
    }

    pub fn register_hle_export(&mut self, lib: &str, name: &str, id: u32) {
        self.hle_exports
            .entry(lib.to_string())
            .or_default()
            .insert(name.to_string(), id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn module_with_exports(id: ModuleHandle, name: &str, exports: &[(&str, u64)]) -> Module {
        let map = exports
            .iter()
            .map(|(k, v)| (k.to_string(), *v))
            .collect::<HashMap<_, _>>();
        Module::new(id, name, map)
    }

    /// A loaded module occupying `[base, base + size)`, as the linker registers it.
    fn mapped_module(
        id: ModuleHandle,
        name: &str,
        base: u64,
        size: usize,
        exports: &[(&str, u64)],
    ) -> Module {
        let mut m = module_with_exports(id, name, exports);
        m.base_addr = base;
        m.memory_size = size;
        m
    }

    #[test]
    fn nearest_symbol_picks_the_largest_export_at_or_below() {
        let mut mgr = ModuleManager::new();
        mgr.register_module(mapped_module(
            1,
            "libc",
            0x92c000,
            0x112000,
            &[
                ("strlen", 0x97a9e0),
                ("memcpy", 0x97b400),
                ("malloc", 0x930000),
            ],
        ));

        let s = mgr.nearest_symbol(0x97aa0d).unwrap();
        assert_eq!(s, "libc!strlen +0x2d", "names the enclosing export");

        // Exactly on an export start is offset 0, not the previous symbol.
        assert_eq!(mgr.nearest_symbol(0x97b400).unwrap(), "libc!memcpy +0x0");
    }

    #[test]
    fn nearest_symbol_is_none_outside_every_loaded_module() {
        let mut mgr = ModuleManager::new();
        mgr.register_module(mapped_module(
            1,
            "libc",
            0x92c000,
            0x1000,
            &[("f", 0x92c000)],
        ));

        assert!(
            mgr.nearest_symbol(0).is_none(),
            "a null deref names no module"
        );
        assert!(mgr.nearest_symbol(0x92b000).is_none(), "below the image");
        assert!(mgr.nearest_symbol(0x92d000).is_none(), "at/above the image");
    }

    #[test]
    fn nearest_symbol_skips_hle_stub_modules() {
        let mut mgr = ModuleManager::new();
        // HLE modules keep base 0 / size 0, so they must span no address at all.
        mgr.register_module(Module::new_hle(1, "libkernel_hle", HashMap::new()));
        assert!(mgr.nearest_symbol(0).is_none());
        assert!(mgr.nearest_symbol(0x2000_0000).is_none());
    }

    #[test]
    fn nearest_symbol_reports_module_and_offset_when_nothing_is_exported() {
        let mut mgr = ModuleManager::new();
        mgr.register_module(mapped_module(1, "eboot.bin", 0x1988000, 0x2807000, &[]));

        let s = mgr.nearest_symbol(0x1af5e90).unwrap();
        assert!(s.starts_with("eboot.bin +0x16de90"), "{s}");
        assert!(s.contains("no exported symbol"), "says so plainly: {s}");
    }

    #[test]
    fn display_export_name_labels_an_unrecognised_nid_instead_of_guessing() {
        // 11 base64 chars that are not in the generated NID table: report the NID, never
        // invent a symbol name.
        assert_eq!(display_export_name("ZZZZZZZZZZZ"), "NID ZZZZZZZZZZZ");
        // A plain (homebrew) name passes through unchanged.
        assert_eq!(display_export_name("main"), "main");
    }

    #[test]
    fn display_export_name_recovers_a_known_nid_and_keeps_it() {
        let nid = ps4_syscalls::SyscallId::from_symbol_name("sceKernelUsleep")
            .map(|s| s.nid().to_string())
            .filter(|n| !n.is_empty());
        let Some(nid) = nid else {
            return; // no NID table entry in this build; nothing to assert
        };
        let shown = display_export_name(&nid);
        assert!(shown.starts_with("sceKernelUsleep "), "{shown}");
        assert!(
            shown.contains(&format!("[NID {nid}]")),
            "keeps the NID: {shown}"
        );
    }

    #[test]
    fn allocator_hands_out_nonoverlapping_aligned_bases() {
        let mut alloc = ModuleAllocator::new(0x40_0000);

        let a = alloc.allocate(0x1000);
        let b = alloc.allocate(0x4001); // rounds up to two align units
        let c = alloc.allocate(0x1);

        assert_eq!(a, 0x40_0000, "first base is the start address");

        // Every base is 0x4000-aligned.
        for base in [a, b, c] {
            assert_eq!(base & 0x3FFF, 0, "base {base:#x} must be 0x4000-aligned");
        }

        // Bases are strictly increasing (non-overlapping): a's slot is one align
        // unit (0x1000 rounds up to 0x4000), b's is two (0x4001 rounds up to 0x8000).
        assert_eq!(b, a + 0x4000, "0x1000 rounds up to one 0x4000 unit");
        assert_eq!(c, b + 0x8000, "0x4001 rounds up to two 0x4000 units");
    }

    #[test]
    fn allocator_size_zero_does_not_advance_cursor() {
        let mut alloc = ModuleAllocator::new(0x1000);
        let first = alloc.allocate(0);
        let second = alloc.allocate(0);
        assert_eq!(first, 0x1000);
        assert_eq!(
            second, 0x1000,
            "a size-0 allocation consumes no space; next base is unchanged"
        );
    }

    #[test]
    fn get_next_handle_increments_from_one() {
        let mut mgr = ModuleManager::new();
        assert_eq!(mgr.get_next_handle(), 1);
        assert_eq!(mgr.get_next_handle(), 2);
        assert_eq!(mgr.get_next_handle(), 3);
    }

    #[test]
    fn register_and_get_by_name() {
        let mut mgr = ModuleManager::new();
        let module = module_with_exports(1, "libkernel.prx", &[]);
        mgr.register_module(module);

        let got = mgr.get_by_name("libkernel.prx").expect("registered module");
        assert_eq!(got.id, 1);
        assert_eq!(got.name, "libkernel.prx");
        assert!(mgr.get_by_name("nope.prx").is_none(), "unknown name misses");
    }

    #[test]
    fn resolve_symbol_finds_export_across_modules_and_misses() {
        let mut mgr = ModuleManager::new();
        mgr.register_module(module_with_exports(1, "a.prx", &[("sym_a", 0x1000)]));
        mgr.register_module(module_with_exports(2, "b.prx", &[("sym_b", 0x2000)]));

        // A lookup after loading A+B finds an export in either module.
        assert_eq!(mgr.resolve_symbol("sym_a"), Some(0x1000));
        assert_eq!(mgr.resolve_symbol("sym_b"), Some(0x2000));
        // A miss returns None.
        assert_eq!(mgr.resolve_symbol("sym_missing"), None);
    }

    #[test]
    fn resolve_symbol_returns_stored_absolute_value_unmodified() {
        // resolve_symbol must return the stored export value as-is (exports are
        // already absolute); it must not add base_addr again.
        let mut mgr = ModuleManager::new();
        let mut module = module_with_exports(1, "a.prx", &[("sym", 0xDEAD_0000)]);
        module.base_addr = 0x40_0000;
        mgr.register_module(module);
        assert_eq!(mgr.resolve_symbol("sym"), Some(0xDEAD_0000));
    }

    /// `sceKernelDlsym` receives a plain C name but a retail `.prx`'s exports are keyed
    /// by NID. Guard the two-step lookup a module does: try the plain name, then the
    /// name hashed to its NID via `ps4-syscalls`. A synthetic module keyed only by the
    /// NID must still resolve from the plain name.
    #[test]
    fn nid_keyed_export_resolves_from_plain_name() {
        use ps4_syscalls::SyscallId;

        let nid = SyscallId::from_symbol_name("sceKernelUsleep")
            .unwrap()
            .nid()
            .to_string();

        let module = module_with_exports(7, "scePlayStation4.prx", &[(nid.as_str(), 0xABC0_0000)]);

        // The plain name is absent from the exports map...
        assert_eq!(module.exports.get("sceKernelUsleep"), None);
        // ...but resolves once hashed to its NID (the dlsym fallback).
        let resolved = module
            .exports
            .get("sceKernelUsleep")
            .copied()
            .or_else(|| module.exports.get(nid.as_str()).copied());
        assert_eq!(resolved, Some(0xABC0_0000));
    }

    #[test]
    fn register_hle_export_groups_by_library() {
        let mut mgr = ModuleManager::new();
        mgr.register_hle_export("libkernel", "sceKernelUsleep", 0x11);
        mgr.register_hle_export("libkernel", "sceKernelSleep", 0x22);
        mgr.register_hle_export("libc", "malloc", 0x33);

        let libkernel = mgr.hle_exports.get("libkernel").expect("libkernel group");
        assert_eq!(libkernel.get("sceKernelUsleep"), Some(&0x11));
        assert_eq!(libkernel.get("sceKernelSleep"), Some(&0x22));
        assert_eq!(libkernel.len(), 2, "same-lib exports share one group");

        let libc = mgr.hle_exports.get("libc").expect("libc group");
        assert_eq!(libc.get("malloc"), Some(&0x33));
    }
}
