//! One generic for the process-global "registered singleton" pattern shared by every
//! HLE seam that a subsystem wires at boot and other subsystems reach without a direct
//! dependency ([`crate::kernel`], [`crate::dirty`], [`crate::gpu`], [`crate::bounded_read`]).
//!
//! Each such seam is a `RwLock<Option<Arc<dyn Trait>>>`: the owning subsystem calls
//! [`Registered::register`] once at boot (before guest threads start, so the write lock is
//! uncontended and cannot be poisoned), and consumers call [`Registered::get`], which
//! degrades to `None` when nothing is wired (headless / unit tests) so callers can degrade
//! safely. A poisoned lock is treated as "no source": [`get`](Registered::get) yields `None`
//! rather than panicking — the same degrade-to-None the four hand-rolled seams had.
//!
//! Tests that exercise the wired *and* the headless path in one process need to swap the
//! global back and forth without leaking state across tests in the same process. The
//! [`test-hooks`](crate)-gated [`Registered::override_scoped`] / [`Registered::override_none_scoped`]
//! do this with an RAII guard: it serializes on a per-instance mutex, swaps the value in, and
//! restores the *prior* value on drop — so a panic between override and restore cannot leave
//! the global wired for an unrelated test in the same process.

#[cfg(any(test, feature = "test-hooks"))]
use std::sync::Mutex;
use std::sync::{Arc, RwLock};

/// A process-global, boot-registered singleton over `Arc<T>` (`T` is usually a `dyn Trait`).
///
/// Const-constructible so it can back a `static`. `register`/`get` carry the
/// degrade-to-None + poison-tolerant semantics every seam relies on; the `test-hooks`
/// `override_scoped` family adds a panic-safe, serialized RAII override for tests.
pub struct Registered<T: ?Sized> {
    slot: RwLock<Option<Arc<T>>>,
    /// Serializes *every* test-time slot mutation on this instance so concurrent tests can't
    /// tear each other's writes: the `override_scoped` guards, plus the test-only `reset` and
    /// (in test/test-hooks builds only) `register`. Holding it for the mutation's duration is
    /// what makes an active override mutually exclusive with a racing `reset`/`register` on the
    /// same static. Compiled in only under the `test-hooks` feature (or in-crate tests), so it
    /// does not leak into plain library builds — where `register` keeps its lock-free boot path.
    /// Never taken on the `get`/`is_registered` read path.
    #[cfg(any(test, feature = "test-hooks"))]
    test_lock: Mutex<()>,
}

impl<T: ?Sized> Registered<T> {
    /// A fresh, unregistered singleton (`get` yields `None`). `const` so it can back a `static`.
    #[cfg(any(test, feature = "test-hooks"))]
    pub const fn new() -> Registered<T> {
        Registered {
            slot: RwLock::new(None),
            test_lock: Mutex::new(()),
        }
    }

    /// A fresh, unregistered singleton (`get` yields `None`). `const` so it can back a `static`.
    #[cfg(not(any(test, feature = "test-hooks")))]
    pub const fn new() -> Registered<T> {
        Registered {
            slot: RwLock::new(None),
        }
    }

    /// Wire the process-global value. Called once at boot, before guest threads start, so the
    /// write lock is uncontended and can't be poisoned in practice; a poisoned lock is
    /// recovered ([`std::sync::PoisonError::into_inner`]) so the wiring always takes effect
    /// rather than silently no-op'ing. In test/test-hooks builds it also holds `test_lock` for
    /// the write, so a test that registers on a static races no `override_scoped` guard on it;
    /// plain library builds compile the lock-free boot path below unchanged.
    pub fn register(&self, value: Arc<T>) {
        // Serialize against the override guards in test/test-hooks builds only (the field
        // doesn't exist otherwise). Held until the slot write below completes, mirroring
        // `override_inner`. Not re-entered from any `Drop` (the guard restores the slot
        // directly), so this cannot deadlock a held guard.
        #[cfg(any(test, feature = "test-hooks"))]
        let _lock = self.test_lock.lock().unwrap_or_else(|e| e.into_inner());
        // Recover a poisoned slot: a prior panic that poisoned the lock must not turn
        // `register` into a silent no-op — boot wiring has to take effect regardless.
        let mut guard = self.slot.write().unwrap_or_else(|e| e.into_inner());
        *guard = Some(value);
    }

    /// The registered value, or `None` when none is wired (headless / unit tests) or the lock
    /// is poisoned. Callers must degrade safely on `None`.
    pub fn get(&self) -> Option<Arc<T>> {
        self.slot.read().ok()?.clone()
    }

    /// Whether a value is currently wired. For the composition root's boot-time
    /// all-seams-wired assert (misregistration → boot failure instead of a runtime-silent
    /// degrade). A poisoned lock reads as *not* wired, matching [`get`](Self::get)'s
    /// degrade-to-`None`.
    pub fn is_registered(&self) -> bool {
        self.slot.read().map(|g| g.is_some()).unwrap_or(false)
    }

    /// **Test-only**: swap the value back to unregistered unconditionally. Prefer
    /// [`Registered::override_scoped`] — the RAII guard restores the prior value even on
    /// panic, whereas this leaves the global cleared. Retained only for the one-shot
    /// clear-and-assert-headless test idiom.
    #[cfg(any(test, feature = "test-hooks"))]
    pub fn reset(&self) {
        // Serialize against the override guards, exactly as `override_inner` does, so a
        // concurrent `override_scoped` on this static can't have its write torn by this clear
        // (and vice versa). Held until the slot write completes. Not re-entered from any `Drop`
        // (the guard restores the slot directly), so this cannot deadlock a held guard.
        let _lock = self.test_lock.lock().unwrap_or_else(|e| e.into_inner());
        // Recover a poisoned slot so the clear always takes effect (see `register`).
        let mut guard = self.slot.write().unwrap_or_else(|e| e.into_inner());
        *guard = None;
    }

    /// **Test-only**: override the global to `value` for the returned guard's lifetime,
    /// restoring the prior value on drop. Serializes on a per-instance mutex so concurrent
    /// tests in the same process don't observe each other's override, and is panic-safe: the
    /// restore runs in `Drop`, so a panic between override and drop can't leave the global
    /// wired for an unrelated test.
    #[cfg(any(test, feature = "test-hooks"))]
    pub fn override_scoped(&self, value: Arc<T>) -> ScopeGuard<'_, T> {
        self.override_inner(Some(value))
    }

    /// **Test-only**: like [`Registered::override_scoped`], but forces the global to `None`
    /// (the headless path) for the guard's lifetime, restoring the prior value on drop. Lets a
    /// headless-path test serialize against the wired-path test on the same instance.
    #[cfg(any(test, feature = "test-hooks"))]
    pub fn override_none_scoped(&self) -> ScopeGuard<'_, T> {
        self.override_inner(None)
    }

    #[cfg(any(test, feature = "test-hooks"))]
    fn override_inner(&self, value: Option<Arc<T>>) -> ScopeGuard<'_, T> {
        // Serialize overrides. Tolerate poisoning: a prior test that panicked while holding
        // the guard leaves the mutex poisoned, but its Drop already restored the prior value,
        // so recovering the guard is safe.
        let lock = self.test_lock.lock().unwrap_or_else(|e| e.into_inner());
        // Recover a poisoned slot lock: if a prior test panicked while holding it, the
        // *true* prior value still lives behind the poison. Capturing it (rather than
        // defaulting to `None`) is what lets `Drop` always restore the real prior — a
        // poison-lost prior would leave this override live for an unrelated test.
        let mut guard = self.slot.write().unwrap_or_else(|e| e.into_inner());
        let prior = std::mem::replace(&mut *guard, value);
        drop(guard);
        ScopeGuard {
            registered: self,
            prior: Some(prior),
            _lock: lock,
        }
    }
}

impl<T: ?Sized> Default for Registered<T> {
    fn default() -> Registered<T> {
        Registered::new()
    }
}

/// RAII override handle from [`Registered::override_scoped`] / [`Registered::override_none_scoped`].
/// Holds the per-instance serialization lock and restores the prior value on drop.
#[cfg(any(test, feature = "test-hooks"))]
pub struct ScopeGuard<'a, T: ?Sized> {
    registered: &'a Registered<T>,
    /// The value to restore on drop (always `Some` until `Drop` takes it).
    prior: Option<Option<Arc<T>>>,
    /// Held for the guard's lifetime to serialize concurrent overrides.
    _lock: std::sync::MutexGuard<'a, ()>,
}

#[cfg(any(test, feature = "test-hooks"))]
impl<T: ?Sized> Drop for ScopeGuard<'_, T> {
    fn drop(&mut self) {
        if let Some(prior) = self.prior.take() {
            // Recover a poisoned slot: skipping the restore on poison would leave the
            // global at this guard's override value, bleeding into an unrelated test.
            let mut guard = self
                .registered
                .slot
                .write()
                .unwrap_or_else(|e| e.into_inner());
            *guard = prior;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A poisoned slot lock must not lose the prior value: `override_scoped` captures the true
    /// prior (recovering the poison) and its drop restores it, so a panic that poisoned the
    /// slot in an earlier test can't bleed the override value into a later one.
    #[test]
    fn poisoned_slot_still_captures_and_restores_prior() {
        let reg: Registered<u32> = Registered::new();
        reg.register(Arc::new(7));

        // Poison the slot lock the way a panicking test would: panic while holding the write
        // guard. The prior value (7) survives behind the poison.
        std::thread::scope(|s| {
            let h = s.spawn(|| {
                let _g = reg.slot.write().unwrap();
                panic!("poison the slot");
            });
            assert!(h.join().is_err());
        });
        assert!(reg.slot.is_poisoned(), "slot should be poisoned");

        // Read the slot's inner value directly, recovering the poison — `get` degrades a
        // poisoned lock to `None` by design, which would mask what the override actually left
        // behind. This assertion targets the restore, not the degrade-to-None path.
        let slot_value = |r: &Registered<u32>| -> Option<u32> {
            r.slot
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .map(|a| **a)
        };

        // A scoped override on the poisoned slot must capture the real prior (7) and restore it
        // on drop, not leave the override value (99) behind.
        {
            let _guard = reg.override_scoped(Arc::new(99));
            assert_eq!(slot_value(&reg), Some(99), "override did not take effect");
        }
        assert_eq!(
            slot_value(&reg),
            Some(7),
            "poisoned-slot override left the global at the override value (cross-test bleed)"
        );
    }
}
