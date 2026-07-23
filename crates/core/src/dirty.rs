//! Guest-memory dirty-tracking seam (doc-2 §8.3).
//!
//! The resource cache (`ps4-gnm`) polls this seam at submit boundaries to learn which
//! watched guest ranges the guest wrote since the last poll, so a clean range can be
//! re-used instead of re-uploaded. The trait lives here — not in `ps4-gnm` — precisely
//! so `ps4-gnm` reaches the real, x86jit-backed impl **without depending on `ps4-cpu`**:
//! the impl registers itself at boot through the global below, exactly like
//! [`crate::kernel::register_kernel`] / [`crate::gpu::register_present_sink`].
//!
//! Two impls exist (doc-2 §8.3): the real x86jit-backed one over the guest VM's
//! watched-range facility (`ps4-cpu`), and an [`AlwaysDirty`] fallback that reports
//! "everything dirty" so the cache stays correct when no VM is wired (headless tests)
//! or when forced off via an env lever. Polling is drain-on-read at submit boundaries
//! only, needing no ordering beyond `MemConsistency::Fast`.

use std::sync::{Arc, RwLock};

use crate::registered::Registered;

/// The dirty-tracking seam the resource cache polls (doc-2 §8.3).
///
/// Impls: the x86jit watched-range facility (`ps4-cpu`, the real one) or [`AlwaysDirty`]
/// (the conservative fallback). `watch`/`unwatch` register the guest ranges the cache
/// backs; `take_dirty` drains the ranges written since the last call.
pub trait DirtySource: Send + Sync {
    /// Start recording guest writes to `[addr, addr + size)`.
    fn watch(&self, addr: u64, size: u64);
    /// Stop recording writes to a range previously passed to [`Self::watch`].
    fn unwatch(&self, addr: u64, size: u64);
    /// Drain the watched ranges written since the last call, as `(addr, byte_len)`.
    fn take_dirty(&self) -> Vec<(u64, u64)>;
}

static DIRTY_SOURCE: Registered<dyn DirtySource> = Registered::new();

/// Register the process-global dirty source, mirroring [`crate::kernel::register_kernel`].
/// The app wires the `ps4-cpu` x86jit-backed impl at boot; the `ps4-gnm` cache reaches it
/// through [`dirty_source`] at submit time. Called once at boot, before guest threads
/// start, so the write lock is uncontended and can't be poisoned; a failed lock is
/// silently ignored rather than logged.
pub fn register_dirty_source(source: Arc<dyn DirtySource>) {
    DIRTY_SOURCE.register(source);
}

/// The registered dirty source, or `None` when none is wired (headless: the cache then
/// falls back to [`AlwaysDirty`], re-uploading every submit — correct, not incremental).
pub fn dirty_source() -> Option<Arc<dyn DirtySource>> {
    DIRTY_SOURCE.get()
}

/// The conservative fallback: reports every watched range as dirty on every poll, so the
/// cache re-uploads unconditionally (doc-2 §8.3). Selected when no VM is wired (headless)
/// or via an env lever; keeps correctness when the real x86jit source is absent.
///
/// It cannot enumerate "everything" without a range set, so it remembers the watched
/// ranges and returns them all on each [`DirtySource::take_dirty`] — matching the current
/// per-submit "assume all dirty" behavior over exactly the ranges the cache backs.
#[derive(Default)]
pub struct AlwaysDirty {
    watched: RwLock<Vec<(u64, u64)>>,
}

impl AlwaysDirty {
    pub fn new() -> AlwaysDirty {
        AlwaysDirty::default()
    }
}

impl DirtySource for AlwaysDirty {
    fn watch(&self, addr: u64, size: u64) {
        if let Ok(mut w) = self.watched.write()
            && !w.contains(&(addr, size))
        {
            w.push((addr, size));
        }
    }

    fn unwatch(&self, addr: u64, size: u64) {
        if let Ok(mut w) = self.watched.write() {
            w.retain(|&r| r != (addr, size));
        }
    }

    fn take_dirty(&self) -> Vec<(u64, u64)> {
        // "Everything dirty": every watched range, every poll. Not drained — the cache
        // must treat all of it as written again next submit until a real source is wired.
        self.watched.read().map(|w| w.clone()).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A mock impl over an in-memory page set, exercising the seam without a VM:
    /// `watch`→simulated write→`take_dirty` drains once (AC #1).
    #[derive(Default)]
    struct MockDirty {
        watched: RwLock<Vec<(u64, u64)>>,
        written: RwLock<Vec<(u64, u64)>>,
    }

    impl MockDirty {
        /// Simulate a guest write, recorded only if it falls inside a watched range.
        fn simulate_write(&self, addr: u64, size: u64) {
            let watched = self.watched.read().unwrap();
            if watched
                .iter()
                .any(|&(a, n)| addr >= a && addr.saturating_add(size) <= a + n)
            {
                self.written.write().unwrap().push((addr, size));
            }
        }
    }

    impl DirtySource for MockDirty {
        fn watch(&self, addr: u64, size: u64) {
            self.watched.write().unwrap().push((addr, size));
        }
        fn unwatch(&self, addr: u64, size: u64) {
            self.watched.write().unwrap().retain(|&r| r != (addr, size));
        }
        fn take_dirty(&self) -> Vec<(u64, u64)> {
            std::mem::take(&mut *self.written.write().unwrap())
        }
    }

    #[test]
    fn mock_watch_write_drain() {
        let src = MockDirty::default();
        src.watch(0x1000, 0x100);

        // A write inside the watched range shows up once, then the drain empties it.
        src.simulate_write(0x1000, 8);
        assert_eq!(src.take_dirty(), vec![(0x1000, 8)]);
        assert!(src.take_dirty().is_empty(), "drain leaves nothing behind");

        // A write outside any watched range is never reported.
        src.simulate_write(0x9000, 8);
        assert!(src.take_dirty().is_empty(), "unwatched write not reported");

        // After unwatch, writes to the old range are ignored.
        src.unwatch(0x1000, 0x100);
        src.simulate_write(0x1000, 8);
        assert!(
            src.take_dirty().is_empty(),
            "write after unwatch not reported"
        );
    }

    #[test]
    fn always_dirty_reports_all_watched_every_poll() {
        let src = AlwaysDirty::new();
        assert!(
            src.take_dirty().is_empty(),
            "nothing watched → nothing dirty"
        );

        src.watch(0x2000, 0x1000);
        src.watch(0x4000, 0x1000);
        // Every poll reports every watched range (conservative), not drained.
        assert_eq!(src.take_dirty(), vec![(0x2000, 0x1000), (0x4000, 0x1000)]);
        assert_eq!(
            src.take_dirty(),
            vec![(0x2000, 0x1000), (0x4000, 0x1000)],
            "AlwaysDirty keeps reporting until unwatched (not drained)"
        );

        src.unwatch(0x2000, 0x1000);
        assert_eq!(src.take_dirty(), vec![(0x4000, 0x1000)]);
    }

    #[test]
    fn registration_roundtrips() {
        // Registration mirrors register_kernel: register, then read back the same Arc.
        let src: Arc<dyn DirtySource> = Arc::new(AlwaysDirty::new());
        register_dirty_source(Arc::clone(&src));
        let got = dirty_source().expect("registered source is retrievable");
        got.watch(0x8000, 0x10);
        assert_eq!(got.take_dirty(), vec![(0x8000, 0x10)]);
    }
}
