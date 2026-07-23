use ps4_core::kernel::MutexType;

use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::time::{Duration, Instant};
use tracing::info;

/// How long a thread may wait for a guest mutex before we name the holder in the log. Long
/// enough that ordinary contention — even a slow guest holding a lock across an I/O — never
/// trips it, short enough that a stalled title is diagnosed within a smoke-loop run.
const STUCK_LOCK_REPORT_AFTER: Duration = Duration::from_secs(5);

/// Mutex addresses already reported as stuck, so a permanently-held lock logs once per
/// waiter instead of once every five seconds forever. A deadlock that stays deadlocked has
/// nothing new to say after the first report.
static REPORTED_STUCK: Mutex<Option<std::collections::BTreeSet<(u64, u32)>>> = Mutex::new(None);

/// Name a mutex nobody is releasing: which lock, who wants it, who holds it, how long.
///
/// This is the report that turns "thirty threads are parked" into a single actionable line.
/// Thread names come from the kernel's own table, because "held by tid 19" is a fact and
/// "held by RenderThread" is a diagnosis.
fn report_stuck_lock(addr: u64, waiter: u32, holder: u32, waited: Duration) {
    {
        let mut guard = match REPORTED_STUCK.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        let seen = guard.get_or_insert_with(Default::default);
        if !seen.insert((addr, waiter)) {
            return;
        }
    }
    let name_of = |tid: u32| {
        ps4_core::kernel::get_kernel()
            .and_then(|k| k.thread_name_of(tid))
            .unwrap_or_else(|| format!("tid {tid}"))
    };
    tracing::warn!(
        "[SYNC] mutex {addr:#x} not released: tid {waiter} ({}) has waited {:.1} s for tid {holder} ({}) — if this never clears, it is a deadlock, and the holder's last HLE call is where to look",
        name_of(waiter),
        waited.as_secs_f64(),
        name_of(holder),
    );
}

#[derive(Debug)]
pub struct HostMutexState {
    pub owner: Option<u32>,
    pub locks: u32,
    /// Mutex type lives inside the lock-protected state so a re-init of an *unheld* mutex can
    /// reset it in place (owner/locks/type together) without swapping the `Arc` — see
    /// `mutex_init`. Every reader already holds `state` when it consults the type.
    pub mtype: MutexType,
}

#[derive(Debug)]
pub struct HostMutex {
    pub state: Mutex<HostMutexState>,
    pub cond: Condvar,
}

impl HostMutex {
    pub fn new(mtype: MutexType) -> Self {
        Self {
            state: Mutex::new(HostMutexState {
                owner: None,
                locks: 0,
                mtype,
            }),
            cond: Condvar::new(),
        }
    }
}

#[derive(Debug)]
pub struct HostCond {
    pub inner: Condvar,
    /// Dedicated mutex the `inner` Condvar is ALWAYS paired with. std's `Condvar` records the
    /// first `Mutex` it waits on and panics if later waited on a different one. A single guest
    /// cond may legally be bound to more than one guest mutex (POSIX allows it sequentially),
    /// so we never wait `inner` on a guest mutex's `state` guard — a waiter releases the guest
    /// mutex, then blocks on `inner` under this fixed lock (see `cond_wait`). Holding it while
    /// signalling closes the release-then-block window against lost wakeups.
    pub wait_lock: Mutex<()>,
}

impl HostCond {
    pub fn new() -> Self {
        Self {
            inner: Condvar::new(),
            wait_lock: Mutex::new(()),
        }
    }
}

pub struct SyncManager {
    mutexes: RwLock<HashMap<u64, Arc<HostMutex>>>,
    condvars: RwLock<HashMap<u64, Arc<HostCond>>>,
    rwlocks: RwLock<HashMap<u64, Arc<std::sync::RwLock<()>>>>,
}

impl SyncManager {
    pub fn new() -> Self {
        SyncManager {
            mutexes: RwLock::new(HashMap::new()),
            condvars: RwLock::new(HashMap::new()),
            rwlocks: RwLock::new(HashMap::new()),
        }
    }

    pub fn mutex_init(&self, addr: u64, mtype: MutexType) -> Result<i32, u64> {
        let mut map = self.mutexes.write().unwrap();
        // A fresh init on a known address must SUCCEED, not return EBUSY. Our map holding the
        // address does NOT mean the mutex is live/held: `mutex_lock` LAZILY inserts an entry
        // for any never-init'd handle (see below), so a subsequent legitimate
        // `pthread_mutex_init` on that same address would spuriously see the entry and fail.
        // Mono treats EBUSY from `pthread_mutex_init` as fatal (`mono_os_mutex_init` ->
        // abort()), which aborted Celeste before geometry (task-145). Real pthread / FreeBSD
        // libthr semantics: re-initializing a mutex that is not currently locked just (re)sets
        // it to a fresh, unlocked state. So on a known address, RESET the existing entry to a
        // fresh, unlocked state of the requested type and return success — the common case
        // here is an unheld mutex (a lazily-created placeholder or a destroyed-then-reused
        // slot), and re-init resetting it is the correct HLE.
        //
        // Reset the EXISTING primitive IN PLACE rather than swapping in a new `Arc`. A thread
        // already parked in `mutex_lock`/`cond_wait` holds a clone of this same `Arc` and is
        // waiting on its `cond`; swapping the map entry would strand that waiter on an orphaned
        // mutex while later lockers acquire the fresh one, and both would then believe they
        // hold the lock — mutual exclusion silently broken. The held-vs-unheld check and the
        // reset run under the same `state` lock, so no waiter can slip in between them.
        //
        // EXCEPTION: re-init of a mutex that is *currently held* would silently destroy live
        // mutual exclusion (the holder's ownership vanishes; another thread could then acquire
        // the "fresh" mutex concurrently). Real pthread returns EBUSY for this case, so we do
        // too — leave the held mutex untouched and report EBUSY (16).
        if let Some(existing) = map.get(&addr) {
            {
                let mut state = existing.state.lock().unwrap_or_else(|e| e.into_inner());
                if state.owner.is_some() || state.locks > 0 {
                    info!(
                        "Kernel: Mutex re-init at {:#x} rejected — currently held (EBUSY)",
                        addr
                    );
                    return Ok(16); // EBUSY: cannot re-init a locked mutex
                }
                state.owner = None;
                state.locks = 0;
                state.mtype = mtype;
            }
            // Wake any parked waiter so it re-checks against the freshly-reset primitive.
            existing.cond.notify_all();
            info!("Kernel: Mutex re-init at {:#x} (type={:?})", addr, mtype);
            return Ok(0);
        }
        map.insert(addr, Arc::new(HostMutex::new(mtype)));
        info!("Kernel: Mutex created at {:#x} (type={:?})", addr, mtype);
        Ok(0)
    }

    pub fn mutex_destroy(&self, addr: u64) -> Result<i32, u64> {
        let mut map = self.mutexes.write().unwrap();
        if map.remove(&addr).is_some() {
            Ok(0)
        } else {
            Ok(22) // EINVAL: Invalid mutex
        }
    }

    pub fn mutex_lock(&self, addr: u64) -> Result<i32, u64> {
        // look up under read lock first
        let mutex = {
            let map = self.mutexes.read().unwrap();
            map.get(&addr).cloned()
        };

        // not found: lazily init under write lock
        let mutex = match mutex {
            Some(m) => m,
            None => {
                let mut map = self.mutexes.write().unwrap();
                // recheck: another thread may have created it
                map.entry(addr)
                    .or_insert_with(|| {
                        tracing::info!("Kernel: Lazy initialization of Mutex at {:#x}", addr);
                        // A never-init'd handle is the default (NORMAL) type.
                        Arc::new(HostMutex::new(MutexType::Normal))
                    })
                    .clone()
            }
        };

        let tid = ps4_core::kernel::current_tid();
        let waited_since = Instant::now();
        let mut state = mutex.state.lock().unwrap();

        loop {
            if let Some(owner) = state.owner {
                if owner == tid {
                    // Self-relock: the type decides. Only ERRORCHECK returns EDEADLK.
                    // RECURSIVE counts up. NORMAL also counts up here rather than erroring
                    // or blocking: real POSIX leaves NORMAL self-relock as a deadlock (never
                    // EDEADLK), and Mono's NORMAL mono_os_mutex treats an EDEADLK from lock
                    // as fatal ("Resource deadlock avoided"). A benign recursive acquire is
                    // the correct HLE — the guest never intends a genuine self-deadlock here
                    // (doc-5).
                    match state.mtype {
                        MutexType::ErrorCheck => return Ok(11), // EDEADLK
                        MutexType::Recursive | MutexType::Normal => {
                            state.locks += 1;
                            return Ok(0);
                        }
                    }
                } else {
                    // owned by someone else, sleep
                    ps4_core::exectrace::park_enter(tid, || {
                        format!("pthread_mutex_lock @ {addr:#x}")
                    });
                    // Wake periodically to name a lock that is never coming. A blocked
                    // thread shows up in the profiler dump as "in scePthreadMutexLock",
                    // which says nothing about WHO is holding it — and the holder is the
                    // whole diagnosis. Waiting in slices costs one timed wakeup per stuck
                    // second and nothing at all on a lock that is contended normally.
                    let (next, timed_out) = mutex
                        .cond
                        .wait_timeout(state, STUCK_LOCK_REPORT_AFTER)
                        .unwrap();
                    state = next;
                    if timed_out.timed_out()
                        && let Some(holder) = state.owner
                        && holder != tid
                    {
                        report_stuck_lock(addr, tid, holder, waited_since.elapsed());
                    }
                }
            } else {
                ps4_core::exectrace::park_exit(tid);
                state.owner = Some(tid);
                state.locks = 1;
                return Ok(0);
            }
        }
    }

    pub fn mutex_unlock(&self, addr: u64) -> Result<i32, u64> {
        let mutex = {
            let map = self.mutexes.read().unwrap();
            match map.get(&addr) {
                Some(m) => m.clone(),
                None => return Ok(22), // EINVAL
            }
        };

        let tid = ps4_core::kernel::current_tid();
        let mut state = mutex.state.lock().unwrap();

        if let Some(owner) = state.owner {
            if owner != tid {
                return Ok(1); // EPERM: Not owner
            }

            state.locks -= 1;
            if state.locks == 0 {
                state.owner = None;
                mutex.cond.notify_all();
            }
            Ok(0)
        } else {
            Ok(1) // EPERM: Unlock on unlocked mutex
        }
    }
    pub fn cond_init(&self, addr: u64) -> Result<i32, u64> {
        let mut map = self.condvars.write().unwrap();
        if map.contains_key(&addr) {
            return Ok(16); // EBUSY
        }
        map.insert(addr, Arc::new(HostCond::new()));
        Ok(0)
    }

    pub fn cond_destroy(&self, addr: u64) -> Result<i32, u64> {
        let mut map = self.condvars.write().unwrap();
        if map.remove(&addr).is_some() {
            Ok(0)
        } else {
            Ok(22) // EINVAL
        }
    }

    pub fn cond_wait(&self, cond_addr: u64, mutex_addr: u64) -> Result<i32, u64> {
        let cond = {
            let map = self.condvars.read().unwrap();
            match map.get(&cond_addr) {
                Some(c) => c.clone(),
                None => return Ok(22), // EINVAL
            }
        };

        let mutex = {
            let map = self.mutexes.read().unwrap();
            match map.get(&mutex_addr) {
                Some(m) => m.clone(),
                None => return Ok(22), // EINVAL
            }
        };

        let tid = ps4_core::kernel::current_tid();

        let mut state = mutex.state.lock().unwrap();

        if let Some(owner) = state.owner {
            if owner != tid {
                return Ok(1); // EPERM
            }
        } else {
            return Ok(1); // EPERM: Mutex not locked
        }

        let saved_recursion = state.locks;

        // Acquire the cond's dedicated wait-lock BEFORE releasing the guest mutex. `inner` is
        // therefore only ever paired with `wait_lock` (never a guest mutex's `state` guard), so
        // std does not panic when this cond is reused with a different guest mutex. The original
        // relied on holding the guest `state` guard until blocked to gate signalers; `wait_lock`
        // reproduces that gating — a `cond_signal` must take it too, so a signal cannot slip into
        // the window between releasing the guest mutex here and blocking on `inner` below.
        let wait_guard = cond.wait_lock.lock().unwrap();

        state.locks = 0;
        state.owner = None;
        mutex.cond.notify_all();
        drop(state); // release the guest mutex so other threads can take it

        ps4_core::exectrace::park_enter(tid, || {
            format!("pthread_cond_wait cond@{cond_addr:#x} mtx@{mutex_addr:#x}")
        });
        let wait_guard = cond.inner.wait(wait_guard).unwrap();
        drop(wait_guard); // release wait-lock before re-locking the guest mutex (lock order)

        let mut state = mutex.state.lock().unwrap();
        loop {
            if state.owner.is_none() {
                state.owner = Some(tid);
                state.locks = saved_recursion;
                break;
            } else {
                state = mutex.cond.wait(state).unwrap();
            }
        }
        ps4_core::exectrace::park_exit(tid);

        Ok(0)
    }

    pub fn cond_signal(&self, cond_addr: u64) -> Result<i32, u64> {
        let map = self.condvars.read().unwrap();
        if let Some(cond) = map.get(&cond_addr) {
            // Hold the dedicated wait-lock while notifying so a signal cannot be lost in a
            // waiter's release-then-block window (see `cond_wait`). This never nests inside a
            // guest mutex `state` guard, so it introduces no lock-order inversion.
            let _g = cond.wait_lock.lock().unwrap();
            cond.inner.notify_one();
        }
        // Unknown cond = statically-initialized (SCE_PTHREAD_COND_INITIALIZER) with no
        // registered waiters: signalling it is a no-op, not an error. Returning EINVAL
        // here made a guest's __cxa_guard_release panic ("failed to broadcast").
        Ok(0)
    }

    pub fn cond_broadcast(&self, cond_addr: u64) -> Result<i32, u64> {
        let map = self.condvars.read().unwrap();
        if let Some(cond) = map.get(&cond_addr) {
            // See `cond_signal`: hold the dedicated wait-lock while notifying to close the
            // waiter's release-then-block window against lost wakeups.
            let _g = cond.wait_lock.lock().unwrap();
            cond.inner.notify_all();
        }
        // See cond_signal: a broadcast on a statically-initialized cond with no waiters
        // succeeds as a no-op rather than returning EINVAL.
        Ok(0)
    }

    /// Acquire `addr` within `micros`, or return ETIMEDOUT.
    ///
    /// The timeout is RELATIVE microseconds, exactly like [`Self::cond_timedwait`]. Both
    /// spellings of this call reach the seam that way: Sony's `scePthreadMutexTimedlock`
    /// is declared with `OrbisKernelUseconds` and passes it straight through, while POSIX
    /// `pthread_mutex_timedlock` takes an absolute deadline that the libs layer converts
    /// against [`virtual_epoch_ns`] before calling here (task-216).
    ///
    /// Keeping the seam relative is what makes the clock question unaskable at this level.
    /// This used to read an absolute guest timespec and compare it against
    /// `SystemTime::now()` — host wall time — while the deadline the guest computed came
    /// from `clock_gettime`, which we back with the VIRTUAL clock. The two run at different
    /// rates under `UNEMUPS4_CLOCK` (decision-8), so the timeout was wrong by whatever the
    /// current speed ratio happened to be.
    ///
    /// [`virtual_epoch_ns`]: ps4_core::clock::virtual_epoch_ns
    pub fn mutex_timedlock(&self, addr: u64, tid: u32, micros: u32) -> Result<i32, u64> {
        // A relative deadline is fixed at entry, not recomputed per wakeup: a spurious
        // wakeup or a lock handed to another thread must not extend the total wait.
        let deadline = Instant::now() + Duration::from_micros(micros as u64);

        let mutex = {
            let map = self.mutexes.read().unwrap();
            match map.get(&addr) {
                Some(m) => m.clone(),
                None => return Ok(22),
            }
        };

        let mut state = mutex.state.lock().unwrap();

        loop {
            if let Some(owner) = state.owner {
                if owner == tid {
                    // Same self-relock semantics as mutex_lock: only ERRORCHECK returns
                    // EDEADLK; NORMAL and RECURSIVE count up and succeed.
                    match state.mtype {
                        MutexType::ErrorCheck => return Ok(11), // EDEADLK
                        MutexType::Recursive | MutexType::Normal => {
                            state.locks += 1;
                            return Ok(0);
                        }
                    }
                }
            } else {
                state.owner = Some(tid);
                state.locks = 1;
                return Ok(0);
            }

            let now = Instant::now();
            if now >= deadline {
                return Ok(60); // ETIMEDOUT (FreeBSD; Linux's 110 makes Mono g_error/abort)
            }
            let dur = deadline - now;

            let (new_state, result) = mutex.cond.wait_timeout(state, dur).unwrap();
            state = new_state;

            if result.timed_out() {
                return Ok(60); // ETIMEDOUT (FreeBSD)
            }
        }
    }

    pub fn cond_timedwait(
        &self,
        cond_addr: u64,
        mutex_addr: u64,
        tid: u32,
        micros: u32,
    ) -> Result<i32, u64> {
        // look up cond and mutex
        let cond = {
            let map = self.condvars.read().unwrap();
            match map.get(&cond_addr) {
                Some(c) => c.clone(),
                None => return Ok(22), // EINVAL
            }
        };

        let mutex = {
            let map = self.mutexes.read().unwrap();
            match map.get(&mutex_addr) {
                Some(m) => m.clone(),
                None => return Ok(22),
            }
        };

        let mut state = mutex.state.lock().unwrap();

        if let Some(owner) = state.owner {
            if owner != tid {
                return Ok(1);
            } // EPERM
        } else {
            return Ok(1); // EPERM
        }

        let saved_recursion = state.locks;

        // Same protocol as `cond_wait`: block on `inner` under the cond's dedicated `wait_lock`,
        // never a guest mutex's `state` guard, so std does not panic when this cond is reused
        // with a different guest mutex. Hold `wait_lock` across releasing the guest mutex to
        // gate signalers against a lost wakeup.
        let wait_guard = cond.wait_lock.lock().unwrap();

        state.locks = 0;
        state.owner = None;
        mutex.cond.notify_all();
        drop(state); // release the guest mutex so other threads can take it

        let dur = Duration::from_micros(micros as u64);

        let (wait_guard, result) = cond.inner.wait_timeout(wait_guard, dur).unwrap();
        drop(wait_guard); // release wait-lock before re-locking the guest mutex (lock order)

        // ETIMEDOUT is 60 on FreeBSD/PS4 (Linux's 110 makes Mono's mono_os_cond_timedwait
        // g_error/abort — it only tolerates 0 and the FreeBSD ETIMEDOUT).
        let ret_val = if result.timed_out() { 60 } else { 0 };

        let mut state = mutex.state.lock().unwrap();
        loop {
            if state.owner.is_none() {
                state.owner = Some(tid);
                state.locks = saved_recursion;
                break;
            } else {
                state = mutex.cond.wait(state).unwrap();
            }
        }

        Ok(ret_val)
    }

    pub fn mutex_trylock(&self, addr: u64) -> Result<i32, u64> {
        let mutex = {
            let map = self.mutexes.read().unwrap();
            match map.get(&addr) {
                Some(m) => m.clone(),
                None => return Ok(22), // EINVAL
            }
        };

        let tid = ps4_core::kernel::current_tid();

        // try-lock the internal state without blocking
        if let Ok(mut state) = mutex.state.try_lock() {
            if let Some(owner) = state.owner {
                if owner == tid {
                    // Only RECURSIVE re-acquires without blocking. NORMAL/ERRORCHECK
                    // trylock of an already-owned mutex reports EBUSY (trylock never
                    // deadlocks, so there is no EDEADLK path here).
                    if state.mtype == MutexType::Recursive {
                        state.locks += 1;
                        Ok(0)
                    } else {
                        Ok(16) // EBUSY (already locked by us, non-recursive)
                    }
                } else {
                    Ok(16) // EBUSY (locked by someone else)
                }
            } else {
                state.owner = Some(tid);
                state.locks = 1;
                Ok(0)
            }
        } else {
            Ok(16) // EBUSY
        }
    }

    pub fn rwlock_init(&self, addr: u64) -> Result<i32, u64> {
        let mut map = self.rwlocks.write().unwrap();
        if map.contains_key(&addr) {
            return Ok(16);
        } // EBUSY
        map.insert(addr, Arc::new(std::sync::RwLock::new(())));
        Ok(0)
    }

    pub fn rwlock_destroy(&self, addr: u64) -> Result<i32, u64> {
        let mut map = self.rwlocks.write().unwrap();
        if map.remove(&addr).is_some() {
            Ok(0)
        } else {
            Ok(22) // EINVAL
        }
    }

    pub fn rwlock_rdlock(&self, addr: u64) -> Result<i32, u64> {
        // RwLock is modeled as an exclusive mutex (HLE simplification).
        self.mutex_lock(addr)
    }

    pub fn rwlock_unlock(&self, addr: u64) -> Result<i32, u64> {
        self.mutex_unlock(addr)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Snapshot the `Arc<HostMutex>` currently tracked for `addr` (test-only reach into the
    /// private map to prove identity across a re-init).
    fn tracked(mgr: &SyncManager, addr: u64) -> Arc<HostMutex> {
        mgr.mutexes.read().unwrap().get(&addr).unwrap().clone()
    }

    /// Whether a mutex is currently held (an owner or a non-zero recursion count).
    fn is_held(m: &Arc<HostMutex>) -> bool {
        let state = m.state.lock().unwrap();
        state.owner.is_some() || state.locks > 0
    }

    #[test]
    fn reinit_of_a_held_mutex_returns_ebusy_and_does_not_swap() {
        let mgr = SyncManager::new();
        let addr = 0x1000u64;

        // Init then lock (this test thread becomes the owner → held).
        assert_eq!(mgr.mutex_init(addr, MutexType::Normal), Ok(0));
        assert_eq!(mgr.mutex_lock(addr), Ok(0));
        let before = tracked(&mgr, addr);
        assert!(is_held(&before), "mutex is held after lock");

        // Re-init of a HELD mutex must return EBUSY (16) and leave the underlying mutex intact.
        assert_eq!(
            mgr.mutex_init(addr, MutexType::Recursive),
            Ok(16),
            "re-init of a held mutex returns EBUSY"
        );
        let after = tracked(&mgr, addr);
        assert!(
            Arc::ptr_eq(&before, &after),
            "the held mutex must NOT be swapped by a rejected re-init"
        );
        assert!(
            is_held(&after),
            "the mutex is still held (mutual exclusion preserved)"
        );
        // The original owner can still unlock it.
        assert_eq!(mgr.mutex_unlock(addr), Ok(0));
    }

    #[test]
    fn reinit_of_an_unheld_mutex_succeeds_and_resets_in_place() {
        let mgr = SyncManager::new();
        let addr = 0x2000u64;

        assert_eq!(mgr.mutex_init(addr, MutexType::Normal), Ok(0));
        let before = tracked(&mgr, addr);
        assert!(!is_held(&before), "freshly-init'd mutex is not held");
        assert_eq!(before.state.lock().unwrap().mtype, MutexType::Normal);

        // Re-init of an UNHELD mutex succeeds (Ok(0)) and RESETS the existing primitive in
        // place — the Arc identity must be PRESERVED so a thread already parked on it (in
        // mutex_lock/cond_wait, holding a clone of this same Arc) is not stranded on an
        // orphaned mutex. Swapping in a fresh Arc here silently breaks mutual exclusion: the
        // parked waiter would take the old mutex while later lockers take the new one, and
        // both would believe they hold the lock.
        assert_eq!(
            mgr.mutex_init(addr, MutexType::Recursive),
            Ok(0),
            "re-init of an unheld mutex succeeds"
        );
        let after = tracked(&mgr, addr);
        assert!(
            Arc::ptr_eq(&before, &after),
            "an unheld mutex is reset in place, not swapped, so parked waiters stay coherent"
        );
        // The requested type is honored on the same primitive, and it is left unlocked.
        let state = after.state.lock().unwrap();
        assert_eq!(
            state.mtype,
            MutexType::Recursive,
            "re-init installs the requested type in place"
        );
        assert_eq!(state.owner, None, "re-init leaves the mutex unowned");
        assert_eq!(state.locks, 0, "re-init resets the lock count");
    }

    /// A single guest cond bound to two different guest mutexes (POSIX-legal sequentially).
    /// std's `Condvar` records the first `Mutex` it waits on and panics if later waited on a
    /// different one, so pairing one `HostCond`'s `inner` with two guest mutexes' `state`
    /// guards aborted the waiter. The dedicated `wait_lock` means `inner` is only ever paired
    /// with that one lock, so reuse across mutexes completes cleanly (no panic).
    #[test]
    fn cond_reused_with_two_different_mutexes_does_not_panic() {
        let mgr = Arc::new(SyncManager::new());
        let cond = 0xC000u64;
        let mtx_a = 0xA000u64;
        let mtx_b = 0xB000u64;
        assert_eq!(mgr.cond_init(cond), Ok(0));
        assert_eq!(mgr.mutex_init(mtx_a, MutexType::Normal), Ok(0));
        assert_eq!(mgr.mutex_init(mtx_b, MutexType::Normal), Ok(0));

        // A waiter locks `mtx`, waits on the shared cond, then unlocks (lock+wait on one thread
        // so the ownership check sees a consistent tid).
        let spawn_waiter = |mtx: u64| {
            let mgr = mgr.clone();
            std::thread::spawn(move || {
                assert_eq!(mgr.mutex_lock(mtx), Ok(0));
                assert_eq!(mgr.cond_wait(cond, mtx), Ok(0));
                assert_eq!(mgr.mutex_unlock(mtx), Ok(0));
            })
        };

        // Broadcast until the waiter has returned. A broadcast that races the waiter before it
        // parks is a no-op, so retry; once parked, the wait_lock gating makes the next broadcast
        // wake it. The loop terminates because the waiter parks promptly (uncontended lock).
        let drain = |handle: std::thread::JoinHandle<()>| {
            while !handle.is_finished() {
                let _ = mgr.cond_broadcast(cond);
                std::thread::sleep(Duration::from_millis(1));
            }
            handle.join().expect("waiter thread must not panic");
        };

        // First bind: cond ⇄ mutex A.
        drain(spawn_waiter(mtx_a));
        // Second bind: SAME cond ⇄ mutex B — the reuse that used to abort. Must complete.
        drain(spawn_waiter(mtx_b));
    }
}
