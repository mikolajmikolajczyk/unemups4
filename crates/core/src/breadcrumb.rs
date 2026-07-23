//! Per-thread HLE call breadcrumb ring, dumped on a fatal guest fault (task-113.2).
//!
//! When the guest dies four frames deep inside its own libc — the classic
//! `vmovdqa (%rax),%xmm2` with `rax = 0` — the backtrace names *where* it died but not
//! *why*. In an HLE emulator the answer is almost always the last call the guest made
//! into us: a handler that returned `0` for a pointer-returning function the guest did
//! not null-check, because on real hardware that call cannot fail. This ring makes that
//! answer readable straight off the fault report, with no bisection.
//!
//! **Shape.** A fixed-size, per-thread [`Ring`] of [`Crumb`]s in a `thread_local!`. Each
//! `Exit::Syscall` pushes `(id, args)` *before* dispatch and patches the return value
//! *after*, so a call that faulted or wedged inside the handler still shows up — marked
//! `<no return>` — which is exactly the most interesting case. Costs a TLS access plus a
//! handful of stores per HLE call and never allocates; unlike [`crate::exectrace`] it is
//! **always on**, because a crash is not something you can arm an env var for in advance.
//!
//! **Fault-time read.** The dump runs on the faulting thread (the run loop's fatal arm is
//! on the same thread that dispatched the calls), so the thread-local is right there and
//! no cross-thread plumbing is needed.

use std::cell::RefCell;

/// Number of HLE calls retained per thread. Deep enough to span a level-load call
/// sequence, small enough to stay a cheap fixed-size array.
pub const RING_LEN: usize = 32;

/// Number of integer-register arguments recorded per call (the SysV register args the
/// syscall stubs carry: rdi, rsi, rdx, r10, r8, r9).
pub const ARG_COUNT: usize = 6;

/// One recorded HLE call: the dispatch id, its register arguments, and — once the handler
/// returns — its return value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Crumb {
    /// Monotonic per-thread sequence number, starting at 1. `0` marks an unused slot and
    /// doubles as the staleness check in [`Ring::complete`].
    pub seq: u64,
    /// Dispatch id (the value the guest stub put in RAX), resolvable to a name via the
    /// [`crate::exectrace`] name resolver.
    pub id: u64,
    /// The six SysV register arguments, in ABI order.
    pub args: [u64; ARG_COUNT],
    /// The handler's return value; meaningful only when `returned` is true.
    pub ret: u64,
    /// False while the call is in flight — i.e. the handler never came back (it faulted,
    /// wedged, or unwound the thread). Such an entry is the prime suspect.
    pub returned: bool,
}

impl Crumb {
    const EMPTY: Crumb = Crumb {
        seq: 0,
        id: 0,
        args: [0; ARG_COUNT],
        ret: 0,
        returned: false,
    };
}

/// A fixed-size ring of the most recent [`RING_LEN`] HLE calls on one thread.
///
/// Kept as a plain struct (rather than logic inlined into the `thread_local!`) so the
/// wraparound and return-patching rules are unit-testable without spawning threads.
pub struct Ring {
    entries: [Crumb; RING_LEN],
    /// Sequence number handed to the next push; also the write cursor modulo [`RING_LEN`].
    next_seq: u64,
}

impl Default for Ring {
    fn default() -> Self {
        Self::new()
    }
}

impl Ring {
    pub const fn new() -> Self {
        Ring {
            entries: [Crumb::EMPTY; RING_LEN],
            next_seq: 1,
        }
    }

    /// Record a call about to be dispatched. Returns its sequence number, which the caller
    /// hands back to [`complete`](Self::complete) once the handler returns.
    pub fn push(&mut self, id: u64, args: [u64; ARG_COUNT]) -> u64 {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.entries[Self::slot(seq)] = Crumb {
            seq,
            id,
            args,
            ret: 0,
            returned: false,
        };
        seq
    }

    /// Attach a return value to the call `seq` identified. Silently ignored when that
    /// entry has already been evicted — a handler that itself ran more than [`RING_LEN`]
    /// nested guest calls would otherwise patch a *newer* entry's return value and lie.
    pub fn complete(&mut self, seq: u64, ret: u64) {
        let slot = &mut self.entries[Self::slot(seq)];
        if slot.seq == seq {
            slot.ret = ret;
            slot.returned = true;
        }
    }

    /// The retained calls, oldest first.
    pub fn iter_oldest_first(&self) -> impl Iterator<Item = &Crumb> {
        let start = self.next_seq.saturating_sub(RING_LEN as u64).max(1);
        (start..self.next_seq).filter_map(move |seq| {
            let c = &self.entries[Self::slot(seq)];
            (c.seq == seq).then_some(c)
        })
    }

    const fn slot(seq: u64) -> usize {
        (seq % RING_LEN as u64) as usize
    }
}

thread_local! {
    static RING: RefCell<Ring> = const { RefCell::new(Ring::new()) };
}

/// Record an HLE call about to be dispatched on this thread; returns the token to pass to
/// [`record_return`]. Returns `0` (an inert token) if the thread-local is unavailable,
/// e.g. during thread teardown.
#[inline]
pub fn record_call(id: u64, args: [u64; ARG_COUNT]) -> u64 {
    RING.try_with(|r| r.borrow_mut().push(id, args))
        .unwrap_or(0)
}

/// Attach the handler's return value to the call `seq` identified.
#[inline]
pub fn record_return(seq: u64, ret: u64) {
    let _ = RING.try_with(|r| r.borrow_mut().complete(seq, ret));
}

/// Format this thread's breadcrumb ring for a fatal report, oldest call first, as a block
/// of tab-indented lines ready to append to the fault text. Empty when nothing was
/// recorded (e.g. a fault before the first HLE call).
pub fn dump() -> String {
    RING.try_with(|r| format_ring(&r.borrow()))
        .unwrap_or_default()
}

fn format_ring(ring: &Ring) -> String {
    let mut out = String::new();
    for c in ring.iter_oldest_first() {
        let args = c
            .args
            .iter()
            .map(|a| format!("{a:#x}"))
            .collect::<Vec<_>>()
            .join(", ");
        let ret = if c.returned {
            format!("-> {:#x}", c.ret)
        } else {
            "-> <no return: still in flight when the fault hit>".to_string()
        };
        out.push_str(&format!(
            "\n\t  #{} {}({args}) {ret}",
            c.seq,
            crate::exectrace::name_of(c.id)
        ));
    }
    if out.is_empty() {
        out
    } else {
        format!("\n\tHLE calls on this thread (last {RING_LEN}, oldest first):{out}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(n: u64) -> [u64; ARG_COUNT] {
        [n, n + 1, n + 2, n + 3, n + 4, n + 5]
    }

    #[test]
    fn push_then_complete_records_the_return_value() {
        let mut ring = Ring::new();
        let seq = ring.push(0x2a, args(1));
        ring.complete(seq, 0xdead);

        let seen: Vec<_> = ring.iter_oldest_first().collect();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].id, 0x2a);
        assert_eq!(seen[0].args, args(1));
        assert_eq!(seen[0].ret, 0xdead);
        assert!(seen[0].returned);
    }

    #[test]
    fn an_uncompleted_call_stays_marked_in_flight() {
        let mut ring = Ring::new();
        ring.push(7, args(0));
        let seen: Vec<_> = ring.iter_oldest_first().collect();
        assert!(!seen[0].returned, "handler never returned");
        assert!(
            format_ring(&ring).contains("no return"),
            "the dump flags the in-flight call"
        );
    }

    #[test]
    fn empty_ring_dumps_nothing() {
        assert!(format_ring(&Ring::new()).is_empty());
    }

    #[test]
    fn ring_keeps_the_most_recent_entries_in_order() {
        let mut ring = Ring::new();
        for i in 0..(RING_LEN as u64 + 5) {
            let seq = ring.push(i, args(i));
            ring.complete(seq, i * 2);
        }

        let seen: Vec<_> = ring.iter_oldest_first().collect();
        assert_eq!(seen.len(), RING_LEN, "the ring is full, not overgrown");
        // The 5 oldest were evicted; ids run 5..RING_LEN+5 in push order.
        assert_eq!(seen[0].id, 5);
        assert_eq!(seen[RING_LEN - 1].id, RING_LEN as u64 + 4);
        assert!(seen.windows(2).all(|w| w[0].seq < w[1].seq), "oldest first");
        assert!(
            seen.iter().all(|c| c.ret == c.id * 2),
            "returns stay paired"
        );
    }

    #[test]
    fn a_stale_completion_does_not_patch_a_newer_entry() {
        let mut ring = Ring::new();
        let evicted = ring.push(1, args(0));
        for i in 0..(RING_LEN as u64) {
            ring.push(100 + i, args(0));
        }
        // `evicted`'s slot has been reused by a newer call; completing it must not touch it.
        ring.complete(evicted, 0xbad);

        let victim = ring.entries[Ring::slot(evicted)];
        assert_ne!(victim.seq, evicted, "slot was indeed reused");
        assert!(!victim.returned, "newer entry left untouched");
        assert_eq!(victim.ret, 0);
    }

    #[test]
    fn thread_local_ring_round_trips() {
        let seq = record_call(0x1234, args(9));
        record_return(seq, 0);
        let text = dump();
        assert!(
            text.contains("0x1234") || text.contains("syscall#"),
            "{text}"
        );
        assert!(text.contains("-> 0x0"), "a zero return is visible: {text}");
    }
}
