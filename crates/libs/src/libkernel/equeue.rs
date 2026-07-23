//! Minimal event-queue GPU-completion glue for the GNM workload path (doc-6 Entry 2).
//!
//! Retail engines gate their frame loop on a GPU-completion **event queue**: they
//! `sceKernelCreateEqueue`, register a GPU-done event on it with `sceGnmAddEqEvent`,
//! submit graphics, then **block** on `sceKernelWaitEqueue` until the GPU signals the
//! event. On real hardware the GPU raises an interrupt at the end of a submit; the
//! kernel wakes the equeue waiter.
//!
//! Our executor is **synchronous** (doc-2 §C2): `Executor::run` finishes — writing the
//! embedded EOP label inline — before the submit HLE call returns. So by the time the
//! guest reaches `sceKernelWaitEqueue`, the "GPU" is already done. This module gives the
//! equeue the *shape* the guest expects (register an event, mark it triggered on submit-
//! done, report it from a wait) without a real async GPU thread: [`signal_gpu_completion`]
//! is called from the submit-done path, and [`take_triggered`] lets the wait return the
//! event count the guest polls for.
//!
//! It is intentionally minimal (Phase A): a single process-global registry, no per-queue
//! isolation, no real event-data payload. The instrumentation logs whether the guest
//! *waits* and whether a signal was pending, so the next entry can record what Celeste
//! actually does with the completion (does it block? does it read event data?).

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// One GNM event registered on an equeue via `sceGnmAddEqEvent`. Phase A tracks only the
/// identity (`id`, `event_type`) and whether a GPU completion has fired since the last
/// wait drained it — enough to make a `sceKernelWaitEqueue` return a triggered event.
#[derive(Debug, Clone, Copy)]
struct GnmEqEvent {
    /// The `eq` handle the event was registered on (the dummy handle create returns).
    eq: i64,
    /// The GNM event type passed to `sceGnmAddEqEvent` (e.g. end-of-pipe completion).
    event_type: i32,
    /// The `id` passed to `sceGnmAddEqEvent` — the guest's cookie for this event.
    id: i64,
}

/// Process-global registry of GNM equeue events + a completion counter. Mirrors the
/// `driver()`/`get_kernel` OnceLock-in-a-well-known-place pattern (doc-2 §0).
struct EqueueState {
    events: Vec<GnmEqEvent>,
}

static EQUEUE: Mutex<EqueueState> = Mutex::new(EqueueState { events: Vec::new() });

/// Count of GPU submit-completions signalled but not yet drained by a wait. A submit-done
/// bumps this; a `sceKernelWaitEqueue` that finds a registered event and a pending
/// completion drains it and reports the event as triggered. An `AtomicU64` so the submit
/// path (which already holds the driver lock) doesn't also contend the equeue mutex.
static PENDING_COMPLETIONS: AtomicU64 = AtomicU64::new(0);

/// Register a GNM completion event on an equeue (`sceGnmAddEqEvent`). Duplicate
/// registrations (same eq+id) are collapsed so a re-add doesn't grow the list unbounded.
pub fn add_event(eq: i64, event_type: i32, id: i64) {
    let mut st = EQUEUE.lock().unwrap();
    if !st.events.iter().any(|e| e.eq == eq && e.id == id) {
        st.events.push(GnmEqEvent { eq, event_type, id });
    }
}

/// Remove a GNM completion event (`sceGnmDeleteEqEvent`).
pub fn delete_event(eq: i64, id: i64) {
    let mut st = EQUEUE.lock().unwrap();
    st.events.retain(|e| !(e.eq == eq && e.id == id));
}

/// The registered event type for `(eq, id)`, or `None` if not registered
/// (`sceGnmGetEqEventType`).
pub fn event_type(eq: i64, id: i64) -> Option<i32> {
    let st = EQUEUE.lock().unwrap();
    st.events
        .iter()
        .find(|e| e.eq == eq && e.id == id)
        .map(|e| e.event_type)
}

/// The registered event type for a given `id` regardless of the equeue it was registered on.
/// `sceGnmGetEqEventType` receives only the event's `id` (the guest cookie), not the queue
/// handle, and the queue handle is now an arena-allocated cookie rather than a fixed `1`
/// (task-115), so the type must be found by `id` alone. Returns the first match, or `None`.
pub fn event_type_by_id(id: i64) -> Option<i32> {
    let st = EQUEUE.lock().unwrap();
    st.events.iter().find(|e| e.id == id).map(|e| e.event_type)
}

/// Signal a GPU submit-completion (called from the submit-done path). Because the
/// executor is synchronous this fires *after* the work is already visible, so a guest
/// that waits afterwards sees the completion immediately. A saturating bump so a burst of
/// submits without an intervening wait can't wrap.
pub fn signal_gpu_completion() {
    PENDING_COMPLETIONS.fetch_add(1, Ordering::SeqCst);
}

/// Drain one pending completion for a wait on `eq`, if there is a registered event on that
/// queue and a completion is pending. Returns the triggered event's `id` (the guest's
/// cookie) so the waiter can report it. `None` means either no event is registered on this
/// queue or no completion is pending — the caller then falls back to its timeout behaviour.
pub fn take_triggered(eq: i64) -> Option<i64> {
    let st = EQUEUE.lock().unwrap();
    let ev = st.events.iter().find(|e| e.eq == eq).copied()?;
    drop(st);
    // Consume one pending completion; if none is pending, report no trigger.
    loop {
        let cur = PENDING_COMPLETIONS.load(Ordering::SeqCst);
        if cur == 0 {
            return None;
        }
        if PENDING_COMPLETIONS
            .compare_exchange(cur, cur - 1, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            return Some(ev.id);
        }
    }
}

/// Whether any GNM event is registered (instrumentation: distinguishes "guest uses the
/// equeue-completion path" from "guest only sleeps").
pub fn has_events() -> bool {
    !EQUEUE.lock().unwrap().events.is_empty()
}
