use std::sync::RwLock;

// Global storage for missing symbol names.
static MISSING_SYMBOLS: RwLock<Vec<String>> = RwLock::new(Vec::new());

// Magic mask to distinguish these from real syscalls (0..5000)
const MAGIC_MASK: u64 = 0xC000_0000;

/// Registers a name and returns a unique Magic ID.
pub fn register_missing_symbol(name: &str) -> u64 {
    let mut lock = MISSING_SYMBOLS.write().unwrap();
    let id = MAGIC_MASK | (lock.len() as u64);
    lock.push(name.to_string());
    id
}

/// Resolves a Magic ID back to a name.
pub fn get_missing_symbol(id: u64) -> Option<String> {
    if (id & MAGIC_MASK) != MAGIC_MASK {
        return None;
    }
    let index = (id & !MAGIC_MASK) as usize;
    let lock = MISSING_SYMBOLS.read().unwrap();
    lock.get(index).cloned()
}
