//! Per-process registry of pthread TLS keys — the HLE backing for the guest's
//! `scePthreadKeyCreate` / `scePthreadKeySetspecific` / `scePthreadKeyGetspecific`
//! family. A *key* is a small integer handle the guest allocates once and then uses
//! to store one machine word per thread; this module owns the key namespace and each
//! key's optional destructor. The actual per-thread value slots live in
//! `thread.rs::tls_specific`.
//!
//! Facts this file leans on (all else here is our HLE design):
//! - The key handle is `OrbisPthreadKey`, i.e. `pthread_key_t` — a small integer
//!   (OpenOrbis SDK `include/orbis/_types/pthread.h:42`). We model it as `u32`.
//! - A conforming guest allocates at most `ORBIS_PTHREAD_KEYS_MAX = 256` keys
//!   (OpenOrbis SDK `include/orbis/_types/pthread.h:7`). We do NOT enforce that cap —
//!   the counter and backing `Vec` grow on demand; the bound is context, not a check
//!   we impose.
//! - `scePthreadKeyCreate(OrbisPthreadKey*, void(*destructor)(void*))`
//!   (OpenOrbis SDK `include/orbis/libkernel.h:577`) takes a destructor *function
//!   pointer*; a NULL pointer (guest value `0`) means "no destructor", which is why
//!   [`TlsKeys::create_key`] folds `dtor == 0` to `None`.
//!
//! The allocation strategy (monotonic counter, dense `Vec` indexed by key) is our
//! own; the guest only observes that keys are distinct small integers.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};

pub struct TlsKeys {
    next: AtomicU32,
    destructors: Mutex<Vec<Option<u64>>>,
}

impl TlsKeys {
    pub fn new() -> Self {
        Self {
            next: AtomicU32::new(0),
            destructors: Mutex::new(Vec::new()),
        }
    }

    /// Allocate a fresh key and record its destructor. `dtor` is the guest function
    /// pointer passed to `scePthreadKeyCreate` (OpenOrbis SDK `include/orbis/libkernel.h:577`,
    /// `void(*destructor)(void*)`); a NULL pointer (`0`) means "no destructor" and is
    /// stored as `None`. Key numbering (monotonic from 0) is our HLE design.
    pub fn create_key(&self, dtor: u64) -> u32 {
        let key = self.next.fetch_add(1, Ordering::Relaxed);
        let mut vec = self.destructors.lock().unwrap();
        if key as usize >= vec.len() {
            vec.resize(key as usize + 1, None);
        }
        vec[key as usize] = if dtor == 0 { None } else { Some(dtor) };
        key
    }

    /// The destructor recorded for `key`, or `None` if the key has none or was never
    /// allocated. Callers walk this at thread exit to run each key's destructor.
    pub fn get_dtor(&self, key: u32) -> Option<u64> {
        let vec = self.destructors.lock().unwrap();
        vec.get(key as usize).and_then(|x| *x)
    }

    /// One past the highest key allocated — the bound a thread-exit destructor sweep
    /// iterates over.
    pub fn max_key(&self) -> usize {
        self.destructors.lock().unwrap().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Witness for the one guest-value fact this module interprets: `scePthreadKeyCreate`'s
    /// destructor argument is a `void(*)(void*)` function pointer (OpenOrbis SDK
    /// `include/orbis/libkernel.h:577`), so a NULL pointer — guest value `0` — means
    /// "no destructor". A non-NULL pointer is retained verbatim for the exit sweep.
    #[test]
    fn null_destructor_pointer_is_no_destructor() {
        let keys = TlsKeys::new();
        // 0 == NULL function pointer == no destructor.
        let k0 = keys.create_key(0);
        assert_eq!(keys.get_dtor(k0), None);
        // A non-NULL pointer round-trips unchanged.
        let k1 = keys.create_key(0xDEAD_BEEF);
        assert_eq!(keys.get_dtor(k1), Some(0xDEAD_BEEF));
    }

    /// The key handle is a small integer (`OrbisPthreadKey` / `pthread_key_t`,
    /// OpenOrbis SDK `include/orbis/_types/pthread.h:42`) allocated distinctly from 0;
    /// `max_key` tracks one past the top. A conforming guest stays under
    /// `ORBIS_PTHREAD_KEYS_MAX = 256` (`include/orbis/_types/pthread.h:7`), a bound we
    /// document but do not enforce — keys keep growing past it here by design.
    #[test]
    fn keys_are_distinct_small_integers_from_zero() {
        let keys = TlsKeys::new();
        let a = keys.create_key(0);
        let b = keys.create_key(0);
        assert_eq!(a, 0);
        assert_eq!(b, 1);
        assert_ne!(a, b);
        assert_eq!(keys.max_key(), 2);
        // Not enforcing the 256 cap is deliberate: allocating past it still succeeds.
        for _ in 0..300 {
            keys.create_key(0);
        }
        assert!(keys.max_key() > 256);
    }
}
