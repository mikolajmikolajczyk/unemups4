use std::collections::BTreeMap;
use std::sync::RwLock;

// Global storage for missing symbol names.
static MISSING_SYMBOLS: RwLock<Vec<String>> = RwLock::new(Vec::new());

/// Every emitted import stub, keyed by its base address: `addr -> "lib!name"`.
///
/// An import that is MISSING announces itself — the stub traps and the FATAL names the
/// symbol. An import that is *implemented but answers wrongly* announces nothing: the guest
/// stores the bad result and faults somewhere else entirely, and the only trace back is a
/// stub address sitting in a GOT slot. Naming those addresses is what turns
/// "0x20002480 returned something the guest didn't like" into a symbol.
static STUB_SYMBOLS: RwLock<BTreeMap<u64, String>> = RwLock::new(BTreeMap::new());

/// Bytes reserved per emitted stub (`MOV R10,RCX; MOV EAX,id; SYSCALL; RET`, NOP-padded).
/// An address anywhere inside this span resolves to the stub, so a fault at `stub+3` — mid
/// stub, which is where a bad stub actually traps — is still nameable.
const STUB_STRIDE: u64 = 32;

/// Record that `addr` is the import stub for `name` in `lib` (HLE or lazy missing-symbol).
pub fn register_stub_symbol(addr: u64, lib: &str, name: &str) {
    if let Ok(mut map) = STUB_SYMBOLS.write() {
        map.insert(addr, format!("{lib}!{name}"));
    }
}

/// Name the import stub containing `addr`, as `"lib!name +0xN"`.
///
/// Looks up the greatest registered base at or below `addr` and accepts it only if `addr`
/// lands inside that stub's [`STUB_STRIDE`] span — so an address in unrelated memory above
/// the last stub does not get silently attributed to it.
pub fn describe_stub(addr: u64) -> Option<String> {
    let map = STUB_SYMBOLS.read().ok()?;
    let (&base, name) = map.range(..=addr).next_back()?;
    let offset = addr - base;
    (offset < STUB_STRIDE).then(|| {
        if offset == 0 {
            format!("import stub {name}")
        } else {
            format!("import stub {name} +{offset:#x}")
        }
    })
}

/// Every registered import stub as `(addr, "lib!name")`, ascending. For the module dump
/// (`UNEMUPS4_DUMP_MODULES`), so an offline reader can resolve a GOT slot to a symbol the
/// same way the live fault reporter does.
pub fn stub_symbols() -> Vec<(u64, String)> {
    STUB_SYMBOLS
        .read()
        .map(|m| m.iter().map(|(&a, n)| (a, n.clone())).collect())
        .unwrap_or_default()
}

// Magic mask to distinguish these from real syscalls (0..5000)
const MAGIC_MASK: u64 = 0xC000_0000;

/// Marker for a call through an *unresolved runtime-`dlsym` target*. Distinct from the
/// static missing-import marker (`MAGIC_MASK`): a static import that never resolves is a
/// hard link gap and stays FATAL, but a `sceKernelDlsym` MISS is a *probe* — the guest asks
/// "is symbol X present?", gets ENOENT, yet a managed P/Invoke thunk may still dispatch
/// through the (unwritten, null/garbage) function pointer and host-SIGSEGV. Handing the
/// guest a stub carrying THIS marker instead of leaving the pointer null turns that dispatch
/// into a clean, guest-visible trap that logs once and returns 0, so execution continues
/// (task-137: Celeste's `Graphics::GraphicsSystem::DrawPrimitives`/`Present`).
///
/// Chosen so `(id & MAGIC_MASK) != MAGIC_MASK` (only the top bit is set, not both), so the
/// FATAL missing-import branch in `rust_syscall_handler` does NOT claim it — it routes to the
/// benign no-op branch instead.
pub const DLSYM_TRAP_MARKER: u64 = 0xA000_0000;

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
