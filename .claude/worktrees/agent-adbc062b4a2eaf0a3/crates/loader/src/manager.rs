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
