use crate::context::NativeContext;
use ps4_macros::ps4_syscall;
use ps4_syscalls::SyscallId;
use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::Mutex;
use tracing::info;

// main player
const USER_ID: i32 = 1;

// SCE_USER_SERVICE_ERROR_NO_EVENT — returned by the event poll when the queue is
// empty. Must be 0x8096_0007; 0x8096_0009 is NOT_LOGGED_IN (per oo_sdk
// _types/errors.h). scePlayStation4.prx's per-frame System::Update polls the pad ONLY
// when GetEvent returns exactly NO_EVENT; the wrong 0x8096_0009 sent it down the error
// branch every frame (the "Couldn't get event 0x80960009" spam), so the gamepad was
// never polled and GetState stayed disconnected — Celeste never left the attract screen
// (task-170).
const SCE_USER_SERVICE_ERROR_NO_EVENT: i32 = 0x8096_0007u32 as i32;

// SceUserServiceEventType — a login makes the initial user "active". Titles (Celeste
// via Sce.PlayStation4.dll) poll GetEvent at boot and BLOCK until they see the LOGIN
// for the initial user before advancing past the splash/loading frame; without it the
// title idles forever draining NO_EVENT.
const SCE_USER_SERVICE_EVENT_TYPE_LOGIN: i32 = 0;

// Bound the pending backlog: dynamic login/logout isn't modeled, so the ring only ever
// holds the seeded initial-user LOGIN plus slack. A login past the cap is dropped.
const EVENT_QUEUE_CAP: usize = 8;

// Bounded per-user event ring. GetEvent drains it FIFO, one event per poll; empty →
// NO_EVENT. `active` records which users are already logged in so a re-Initialize does
// not re-fire a LOGIN for an already-active user. The Mutex keeps N concurrent pollers
// correct: each drains a distinct event.
struct EventRing {
    events: VecDeque<SceUserServiceEvent>,
    active: Vec<i32>,
    seeded: bool,
}

impl EventRing {
    const fn new() -> Self {
        Self {
            events: VecDeque::new(),
            active: Vec::new(),
            seeded: false,
        }
    }

    // Queue a LOGIN for `user_id` unless that user is already active (idempotent across
    // re-Initialize) or the ring is full.
    fn login(&mut self, user_id: i32) {
        if self.active.contains(&user_id) || self.events.len() >= EVENT_QUEUE_CAP {
            return;
        }
        self.active.push(user_id);
        self.events.push_back(SceUserServiceEvent {
            event_type: SCE_USER_SERVICE_EVENT_TYPE_LOGIN,
            user_id,
        });
    }

    // Seed the initial-user LOGIN exactly once, on the first Initialize or the first
    // poll — whichever a title reaches first. Titles that poll GetEvent before calling
    // Initialize still see the boot login they block on.
    fn seed_initial(&mut self) {
        if self.seeded {
            return;
        }
        self.seeded = true;
        self.login(USER_ID);
    }

    fn pop(&mut self) -> Option<SceUserServiceEvent> {
        self.events.pop_front()
    }
}

static EVENT_RING: Mutex<EventRing> = Mutex::new(EventRing::new());

#[ps4_syscall(id = SyscallId::SCE_USER_SERVICE_INITIALIZE, lib = crate::libs::LIB_SCE_USER_SERVICE, name = "sceUserServiceInitialize")]
pub fn sce_user_service_initialize(_params: u64) -> i32 {
    // _params is usually NULL.
    info!("[USER_SERVICE] sceUserServiceInitialize");
    // Seed the initial-user LOGIN once; re-init on an already-active user is a no-op.
    EVENT_RING.lock().unwrap().seed_initial();
    0 // SCE_OK
}

#[ps4_syscall(id = SyscallId::SCE_USER_SERVICE_GET_INITIAL_USER, lib = crate::libs::LIB_SCE_USER_SERVICE, name = "sceUserServiceGetInitialUser")]
pub fn sce_user_service_get_initial_user(user_id_ptr: *mut i32) -> i32 {
    info!("[USER_SERVICE] sceUserServiceGetInitialUser");
    // Validate the full 4-byte write footprint, not a single byte: the write below stores an
    // i32, so keep T = i32 (not the `as *const u8` cast) and is_guest_ptr checks size_of::<i32>()
    // bytes, rejecting a base near the arena top before the deref (task-115 base+size check).
    if user_id_ptr.is_null() || !crate::is_guest_ptr(user_id_ptr as *const i32) {
        return -1; // Error
    }
    unsafe {
        *user_id_ptr = USER_ID;
    }
    0 // SCE_OK
}

/// `sceUserServiceGetUserName(userId, buf, size)`: the display name of a signed-in user.
///
/// A title shows this in its own UI and, more importantly here, uses it to build per-user save
/// paths and profile keys — so an empty answer is not neutral, it produces empty-named
/// directories. We report a single fixed local user, matching the one
/// `sceUserServiceGetInitialUser` hands out.
///
/// The name is truncated to fit and always NUL-terminated: the caller passes the buffer size
/// and expects a C string back, and a name that filled the buffer exactly would otherwise run
/// off the end of it.
#[ps4_syscall(id = SyscallId::SCE_USER_SERVICE_GET_USER_NAME, lib = crate::libs::LIB_SCE_USER_SERVICE, name = "sceUserServiceGetUserName")]
pub fn sce_user_service_get_user_name(_user_id: i32, buf: u64, size: usize) -> i32 {
    const USER_NAME: &[u8] = b"unemups4";
    if size == 0 {
        return -1;
    }
    let n = USER_NAME.len().min(size - 1);
    let mut out = vec![0u8; size.min(n + 1)];
    out[..n].copy_from_slice(&USER_NAME[..n]);
    let Some(gs) = ps4_core::guest_ptr::GuestSlice::<u8>::new(buf, out.len()) else {
        return -1;
    };
    let _ = gs.write_slice(&out);
    0
}

#[ps4_syscall(id = SyscallId::SCE_USER_SERVICE_GET_LOGIN_USER_ID_LIST, lib = crate::libs::LIB_SCE_USER_SERVICE, name = "sceUserServiceGetLoginUserIdList")]
pub fn sce_user_service_get_login_user_id_list(
    list_ptr: *mut SceUserServiceLoginUserIdList,
) -> i32 {
    info!("[USER_SERVICE] sceUserServiceGetLoginUserIdList");
    // Validate the full 16-byte object, not a single byte: the writes below touch all four
    // i32s, so a base near the arena top must be rejected before the deref (task-115 base+size
    // check). Keep T = SceUserServiceLoginUserIdList so is_guest_ptr checks size_of::<T>() bytes.
    if list_ptr.is_null() || !crate::is_guest_ptr(list_ptr as *const SceUserServiceLoginUserIdList)
    {
        return -1;
    }

    unsafe {
        // Clear the list first
        (*list_ptr).user_ids = [0; 4];

        // Set Player 1 as logged in
        (*list_ptr).user_ids[0] = USER_ID;
    }
    0 // SCE_OK
}

#[ps4_syscall(id = SyscallId::SCE_USER_SERVICE_GET_EVENT, lib = crate::libs::LIB_SCE_USER_SERVICE, name = "sceUserServiceGetEvent")]
pub fn sce_user_service_get_event(event: *mut c_void) -> i32 {
    // Guard the out-param like every other handler: a non-null but non-arena pointer
    // would segfault the host on the writes below (see libkernel is_guest_ptr usage).
    // Validate the full SceUserServiceEvent write footprint (8 bytes), not a single byte: the
    // write below stores the whole struct, so a base near the arena top must be rejected first
    // (task-115 base+size check).
    if event.is_null()
        || !crate::is_guest_range(event as u64, size_of::<SceUserServiceEvent>() as u64)
    {
        return -1;
    }
    // Drain one event FIFO. Titles block at boot until the seeded initial-user LOGIN
    // arrives; after the ring empties, dynamic login/logout is not modeled so polls
    // report NO_EVENT. Concurrent pollers each drain a distinct event under the lock.
    let popped = {
        let mut ring = EVENT_RING.lock().unwrap();
        ring.seed_initial();
        ring.pop()
    };
    match popped {
        Some(ev) => {
            info!(
                "[USER_SERVICE] sceUserServiceGetEvent -> type {} user {}",
                ev.event_type, ev.user_id
            );
            let out = event as *mut SceUserServiceEvent;
            unsafe {
                *out = ev;
            }
            0 // SCE_OK
        }
        None => SCE_USER_SERVICE_ERROR_NO_EVENT,
    }
}

#[ps4_syscall(id = SyscallId::SCE_USER_SERVICE_TERMINATE, lib = crate::libs::LIB_SCE_USER_SERVICE, name = "sceUserServiceTerminate")]
pub fn sce_user_service_terminate() -> i32 {
    info!("[USER_SERVICE] sceUserServiceTerminate");
    0
}

#[repr(C)]
pub struct SceUserServiceLoginUserIdList {
    pub user_ids: [i32; 4],
}

/// `SceUserServiceEvent` (doc: 8 bytes) — the event the guest reads from `GetEvent`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SceUserServiceEvent {
    /// `SceUserServiceEventType`: 0 = LOGIN, 1 = LOGOUT.
    pub event_type: i32,
    /// The `SceUserServiceUserId` the event refers to.
    pub user_id: i32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    #[test]
    fn seed_delivers_initial_login_once() {
        let mut ring = EventRing::new();
        ring.seed_initial();
        let ev = ring.pop().expect("initial login present");
        assert_eq!(ev.event_type, SCE_USER_SERVICE_EVENT_TYPE_LOGIN);
        assert_eq!(ev.user_id, USER_ID);
        assert!(ring.pop().is_none());
    }

    #[test]
    fn reinitialize_does_not_refire_login_for_active_user() {
        let mut ring = EventRing::new();
        ring.seed_initial();
        assert!(ring.pop().is_some());
        // Model repeated Initialize calls on the already-active initial user.
        ring.seed_initial();
        ring.login(USER_ID);
        assert!(ring.pop().is_none(), "no duplicate LOGIN for active user");
    }

    #[test]
    fn ring_is_bounded() {
        let mut ring = EventRing::new();
        for uid in 0..(EVENT_QUEUE_CAP as i32 + 4) {
            ring.login(uid);
        }
        let mut drained = 0;
        while ring.pop().is_some() {
            drained += 1;
        }
        assert_eq!(drained, EVENT_QUEUE_CAP);
    }

    #[test]
    fn get_login_list_rejects_base_plus_size_overrun() {
        use ps4_core::kernel::set_arena_bounds;
        let _t = crate::arena_test_lock();
        // Arena is exactly this 16-byte object. A base that is in-arena but whose full
        // 16-byte write footprint crosses the arena top must be rejected before the deref
        // (task-115 base+size check). base+4 passes the old base-only (1-byte) check yet
        // overruns the arena by 12 bytes — the exact case the typed guard now catches.
        let mut obj = SceUserServiceLoginUserIdList { user_ids: [9; 4] };
        let base = &mut obj as *mut SceUserServiceLoginUserIdList as u64;
        set_arena_bounds(base, size_of::<SceUserServiceLoginUserIdList>() as u64);

        // list_ptr = base + 4: [base+4, base+20) crosses the top → rejected, no write.
        let bad = (base + 4) as *mut SceUserServiceLoginUserIdList;
        assert_eq!(sce_user_service_get_login_user_id_list(bad), -1);
        assert_eq!(obj.user_ids, [9; 4], "rejected call must not write");

        set_arena_bounds(0, 0);
    }

    #[test]
    fn get_login_list_writes_when_object_fits_arena() {
        use ps4_core::kernel::set_arena_bounds;
        let _t = crate::arena_test_lock();
        // Whole 16-byte object inside the arena → in-bounds; correct-input behavior is
        // unchanged: Player 1 marked logged in, the rest cleared.
        let mut obj = SceUserServiceLoginUserIdList { user_ids: [9; 4] };
        let base = &mut obj as *mut SceUserServiceLoginUserIdList as u64;
        set_arena_bounds(base, size_of::<SceUserServiceLoginUserIdList>() as u64);

        assert_eq!(sce_user_service_get_login_user_id_list(&mut obj), 0);
        assert_eq!(obj.user_ids, [USER_ID, 0, 0, 0]);

        set_arena_bounds(0, 0);
    }

    #[test]
    fn concurrent_pollers_drain_distinct_events() {
        let ring = Arc::new(Mutex::new(EventRing::new()));
        {
            let mut guard = ring.lock().unwrap();
            for uid in 1..=EVENT_QUEUE_CAP as i32 {
                guard.login(uid);
            }
        }

        let got = Arc::new(AtomicUsize::new(0));
        let empties = Arc::new(AtomicUsize::new(0));
        let seen = Arc::new(Mutex::new(Vec::new()));

        let mut handles = Vec::new();
        for _ in 0..8 {
            let ring = Arc::clone(&ring);
            let got = Arc::clone(&got);
            let empties = Arc::clone(&empties);
            let seen = Arc::clone(&seen);
            handles.push(std::thread::spawn(move || {
                // Poll until the shared ring is drained; each successful pop is unique.
                loop {
                    let ev = ring.lock().unwrap().pop();
                    match ev {
                        Some(ev) => {
                            got.fetch_add(1, AtomicOrdering::SeqCst);
                            seen.lock().unwrap().push(ev.user_id);
                        }
                        None => {
                            empties.fetch_add(1, AtomicOrdering::SeqCst);
                            break;
                        }
                    }
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(got.load(AtomicOrdering::SeqCst), EVENT_QUEUE_CAP);
        // Empty poll yields NO_EVENT (modeled here as a None pop) for every thread.
        assert_eq!(empties.load(AtomicOrdering::SeqCst), 8);
        let mut ids = seen.lock().unwrap().clone();
        ids.sort_unstable();
        let expected: Vec<i32> = (1..=EVENT_QUEUE_CAP as i32).collect();
        assert_eq!(
            ids, expected,
            "each event drained exactly once, no duplicates"
        );
    }
}
