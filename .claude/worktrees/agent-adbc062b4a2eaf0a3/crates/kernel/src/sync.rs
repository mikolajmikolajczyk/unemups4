use ps4_core::memory::VirtualMemoryManager;

use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::time::{Duration, SystemTime};
use tracing::info;

#[derive(Debug)]
pub struct HostMutexState {
    pub owner: Option<u32>,
    pub locks: u32,
}

#[derive(Debug)]
pub struct HostMutex {
    pub state: Mutex<HostMutexState>,
    pub cond: Condvar,
    pub is_recursive: bool,
}

impl HostMutex {
    pub fn new(recursive: bool) -> Self {
        Self {
            state: Mutex::new(HostMutexState {
                owner: None,
                locks: 0,
            }),
            cond: Condvar::new(),
            is_recursive: recursive,
        }
    }
}

#[derive(Debug)]
pub struct HostCond {
    pub inner: Condvar,
}

impl HostCond {
    pub fn new() -> Self {
        Self {
            inner: Condvar::new(),
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

    fn read_timespec(memory: &dyn VirtualMemoryManager, addr: u64) -> Option<SystemTime> {
        let data = memory.read_bytes(addr, 16).ok()?;

        let sec = u64::from_le_bytes(data[0..8].try_into().unwrap());
        let nsec = u64::from_le_bytes(data[8..16].try_into().unwrap());

        std::time::UNIX_EPOCH.checked_add(std::time::Duration::new(sec, nsec as u32))
    }

    pub fn mutex_init(&self, addr: u64, recursive: bool) -> Result<i32, u64> {
        let mut map = self.mutexes.write().unwrap();
        if map.contains_key(&addr) {
            return Ok(16); // EBUSY
        }

        let mutex = Arc::new(HostMutex::new(recursive));
        map.insert(addr, mutex);

        info!(
            "Kernel: Mutex created at {:#x} (recursive={})",
            addr, recursive
        );
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
                        Arc::new(HostMutex::new(false)) // non-recursive default
                    })
                    .clone()
            }
        };

        let tid = ps4_core::kernel::current_tid();
        let mut state = mutex.state.lock().unwrap();

        loop {
            if let Some(owner) = state.owner {
                if owner == tid {
                    if mutex.is_recursive {
                        state.locks += 1;
                        return Ok(0);
                    } else {
                        // EDEADLK
                        return Ok(11);
                    }
                } else {
                    // owned by someone else, sleep
                    state = mutex.cond.wait(state).unwrap();
                }
            } else {
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
        state.locks = 0;
        state.owner = None;
        mutex.cond.notify_all();

        state = cond.inner.wait(state).unwrap();

        loop {
            if state.owner.is_none() {
                state.owner = Some(tid);
                state.locks = saved_recursion;
                break;
            } else {
                state = mutex.cond.wait(state).unwrap();
            }
        }

        Ok(0)
    }

    pub fn cond_signal(&self, cond_addr: u64) -> Result<i32, u64> {
        let map = self.condvars.read().unwrap();
        if let Some(cond) = map.get(&cond_addr) {
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
            cond.inner.notify_all();
        }
        // See cond_signal: a broadcast on a statically-initialized cond with no waiters
        // succeeds as a no-op rather than returning EINVAL.
        Ok(0)
    }

    pub fn mutex_timedlock(
        &self,
        addr: u64,
        tid: u32,
        abstime_ptr: u64,
        memory: &dyn VirtualMemoryManager,
    ) -> Result<i32, u64> {
        let target_time = match Self::read_timespec(memory, abstime_ptr) {
            Some(t) => t,
            None => return Ok(14), // EFAULT
        };

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
                    if mutex.is_recursive {
                        state.locks += 1;
                        return Ok(0);
                    } else {
                        return Ok(11); // EDEADLK
                    }
                }
            } else {
                state.owner = Some(tid);
                state.locks = 1;
                return Ok(0);
            }

            let now = SystemTime::now();
            if now >= target_time {
                return Ok(110);
            }
            let dur = target_time.duration_since(now).unwrap_or(Duration::ZERO);

            let (new_state, result) = mutex.cond.wait_timeout(state, dur).unwrap();
            state = new_state;

            if result.timed_out() {
                return Ok(110);
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
        state.locks = 0;
        state.owner = None;
        mutex.cond.notify_all();

        let dur = Duration::from_micros(micros as u64);

        let (new_state, result) = cond.inner.wait_timeout(state, dur).unwrap();
        state = new_state;

        let ret_val = if result.timed_out() { 110 } else { 0 }; // ETIMEDOUT

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
                    if mutex.is_recursive {
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
