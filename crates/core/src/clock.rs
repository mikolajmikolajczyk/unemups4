//! The virtual guest clock.
//!
//! Three concepts used to be collapsed into "the host finished presenting an image":
//! what time it is, when the display ticks, and when the emulator may proceed. This
//! module owns the first one only — the time base every guest-visible clock reads
//! ([`now_ns`], via `sceKernelGetProcessTime*` and the wall-clock epoch).
//!
//! Guest-visible time must not be a function of how fast the host renders. Reading raw
//! host time is equally wrong: under slow emulation a time-driven sequence fast-forwards
//! (a splash screen comes and goes within one or two presented frames). So virtual time
//! tracks real elapsed time with a **bounded** per-frame advance:
//!
//! - **BOOT phase** (before the first presented flip): [`now_ns`] tracks real elapsed
//!   host time, but each read is *capped* to at most one 60 Hz frame (`FRAME_NS`) above
//!   the previous read. A slow guest Update() therefore sees a per-read delta of at most
//!   one frame (no fast-forward past real hardware's frame0 workload), while a rapid init
//!   spin-wait still climbs +FRAME_NS per read and catches up to real time fast. Boot-time
//!   spin-waits (e.g. SystemService init polls the clock *before* any flip happens) must
//!   see time advancing, or they hang forever — a prior flip-only-clock attempt deadlocked
//!   exactly there.
//! - **RENDER phase** (after the first flip), in [`Mode::Realtime`] (the default): each
//!   presented flip re-anchors the clock to (real now, virtual now), and a read returns
//!   `virtual_anchor + min(real elapsed since the anchor, MAX_FRAME_DELTA_NS)`. Virtual
//!   time therefore runs at real speed, and one guest frame can never *observe* more than
//!   [`MAX_FRAME_DELTA_NS`] elapsing, however long the host took to produce it. The clamp
//!   is applied on the read, not retroactively at the next flip, because the guest reads
//!   the clock during the slow frame — that is where a fast-forward would be seen.
//!   Consequence when the host cannot keep up: the guest sees a longer delta and drops
//!   logic steps, i.e. the world runs at the right speed with fewer frames.
//! - **RENDER phase** in [`Mode::FixedStep`]: the anchor captured at the first flip plus
//!   `flips × FRAME_NS`. Virtual time is then a pure function of presented frames and is
//!   fully deterministic, which is what headless oracle baselines and the PNG visual
//!   oracle need to be reproducible. It is an explicit opt-in, not a legacy path.
//!
//! Both RENDER anchors chain off the *virtual* (capped) boot time rather than wall time,
//! so the BOOT→RENDER transition has no time jump. In every phase and mode each read is
//! nudged strictly above the previous one (+1 µs floor per read), so a within-frame
//! spin-wait on "clock changed" always terminates.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::time::Instant;

/// One virtual frame at 60 Hz, in nanoseconds.
const FRAME_NS: u64 = 16_666_667;

/// Upper bound on the virtual time a single guest frame may observe elapsing, in
/// [`Mode::Realtime`]. Four 60 Hz frames: comfortably above the real frame time of a
/// healthy run (so the clamp never bites during normal play and the guest sees real
/// speed), low enough that a host hitch, a breakpoint or a 1 fps stretch cannot
/// fast-forward the guest's world.
const MAX_FRAME_DELTA_NS: u64 = 4 * FRAME_NS;

/// Minimum increment between two consecutive [`now_ns`] results, so a spin-wait on "has
/// the clock changed" terminates even when no virtual time has passed.
const READ_FLOOR_NS: u64 = 1000;

/// Environment variable selecting the guest time base. `realtime` (the default) advances
/// virtual time with real elapsed host time under a max-delta clamp, so the guest's world
/// runs at the correct rate whatever the host frame rate; `fixed-step` pins it to
/// presented flips × 16.67 ms — deterministic and independent of host speed.
///
/// `realtime` became the default only after the regression it first exposed was fixed.
/// It initially collapsed Celeste from ~58 fps to under 1 fps within ~30 s, which looked
/// like a flaw in this design; it was not. The guest's frame thread was blocking ~2.2 s
/// per call in `pthread_cond_timedwait` because that POSIX entry point shared a handler
/// with Sony's `scePthreadCondTimedwait`, whose third argument is relative microseconds
/// rather than a pointer to an absolute `timespec` — so a truncated guest pointer was
/// being slept as a duration (task-214). That bug was latent under `fixed-step` too.
///
/// `fixed-step` is retained deliberately, not as legacy: deterministic virtual time is
/// what makes headless oracle baselines and the PNG visual oracle reproducible.
///
/// Note that decision-8's premise — that a fixed-timestep guest reacts to a long delta by
/// dropping logic steps — was never actually confirmed; it simply stopped being the
/// suspect once the ABI bug was found. Judder under `realtime` remains under
/// investigation (task-213).
pub const CLOCK_ENV: &str = "UNEMUPS4_CLOCK";

/// How the RENDER phase derives virtual time. See [`CLOCK_ENV`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Real elapsed host time, clamped to [`MAX_FRAME_DELTA_NS`] per presented frame.
    Realtime,
    /// Presented flips × [`FRAME_NS`]. Deterministic; independent of host speed.
    FixedStep,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Realtime => "realtime",
            Mode::FixedStep => "fixed-step",
        }
    }

    fn code(self) -> u8 {
        match self {
            Mode::Realtime => MODE_REALTIME,
            Mode::FixedStep => MODE_FIXED_STEP,
        }
    }
}

const MODE_UNRESOLVED: u8 = 0;
const MODE_REALTIME: u8 = 1;
const MODE_FIXED_STEP: u8 = 2;

/// The resolved [`Mode`], cached after the first read so the hot path never touches the
/// environment. [`MODE_UNRESOLVED`] until then.
static MODE: AtomicU8 = AtomicU8::new(MODE_UNRESOLVED);

fn parse_mode(v: Option<&str>) -> Mode {
    match v {
        None | Some("") | Some("realtime") => Mode::Realtime,
        Some("fixed-step") => Mode::FixedStep,
        Some(other) => {
            tracing::warn!(
                "{CLOCK_ENV}={other:?} is not a known clock mode (expected `realtime` or \
                 `fixed-step`); defaulting to `realtime`"
            );
            Mode::Realtime
        }
    }
}

/// The active guest time base. Resolved once from [`CLOCK_ENV`], then a cached load.
pub fn mode() -> Mode {
    match MODE.load(Ordering::Relaxed) {
        MODE_REALTIME => Mode::Realtime,
        MODE_FIXED_STEP => Mode::FixedStep,
        _ => {
            static RESOLVED: OnceLock<Mode> = OnceLock::new();
            let m = *RESOLVED.get_or_init(|| parse_mode(std::env::var(CLOCK_ENV).ok().as_deref()));
            MODE.store(m.code(), Ordering::Relaxed);
            m
        }
    }
}

/// Number of presented flips so far.
static FLIP_COUNT: AtomicU64 = AtomicU64::new(0);

/// Virtual time at the current anchor: the [`now_ns`] value captured at the most recent
/// flip ([`Mode::Realtime`]) or at the first flip ([`Mode::FixedStep`]). 0 = no flip seen
/// yet (BOOT phase), so it doubles as the phase sentinel — hence it is always stored last
/// and never stored as a raw 0. It is the *flag* of a message-passing pair: stored with
/// [`Ordering::Release`] (after the paired [`REAL_ANCHOR_NS`] data store) and loaded with
/// [`Ordering::Acquire`], so a reader that observes a fresh anchor is guaranteed to also
/// observe the real anchor it was published with — never a torn (fresh-virt, stale-real)
/// pair, which weak memory (aarch64) would otherwise permit under `Relaxed`.
static VIRT_ANCHOR_NS: AtomicU64 = AtomicU64::new(0);

/// Real host time at the current anchor. [`Mode::Realtime`] only.
static REAL_ANCHOR_NS: AtomicU64 = AtomicU64::new(0);

/// The last value [`now_ns`] returned — the monotonic guard that keeps every read
/// strictly greater than the previous one across the BOOT→RENDER phase switch and
/// within a frame.
static LAST_NS: AtomicU64 = AtomicU64::new(0);

/// Real host time elapsed since a process-start anchor (captured lazily at first use),
/// in nanoseconds.
fn real_ns() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_nanos() as u64
}

/// Advance the virtual clock by one frame. Call exactly once per presented flip (the
/// display-side flip choke point). In [`Mode::Realtime`] this re-anchors the clamp window
/// so the next frame gets a fresh [`MAX_FRAME_DELTA_NS`] budget; in [`Mode::FixedStep`]
/// only the first call matters (it captures the anchor the flip count is added to).
pub fn advance_frame() {
    let first = FLIP_COUNT.fetch_add(1, Ordering::Relaxed) == 0;

    // Anchor to the virtual (capped) boot clock — the last value `now_ns` returned — not
    // wall time, so there's no jump at the BOOT→RENDER transition. `.max(1)`: 0 is the
    // "no flip yet" sentinel, and LAST_NS is still 0 if `now_ns` was never called before
    // this flip; never store a raw 0 anchor.
    if mode() == Mode::FixedStep {
        if first {
            // Release-publish the flag (see `VIRT_ANCHOR_NS`): an Acquire reader that sees
            // this fresh anchor also sees the FLIP_COUNT increment sequenced before it.
            VIRT_ANCHOR_NS.store(LAST_NS.load(Ordering::Relaxed).max(1), Ordering::Release);
        }
        return;
    }

    let virt = if first {
        LAST_NS.load(Ordering::Relaxed).max(1)
    } else {
        peek_ns().max(1)
    };
    // Message passing: the real anchor is the data, the virtual anchor the flag. The data
    // store is sequenced first (it may stay Relaxed — the Release on the flag publishes it);
    // the flag is stored last with Release. A reader that Acquire-loads the fresh virtual
    // anchor is thereby guaranteed to also observe this real anchor, so it can only ever see
    // a *shorter* elapsed span (the new real anchor against the old virtual one), which the
    // per-read floor covers — never a longer one. Program order alone would not hold here:
    // on a weakly-ordered CPU (aarch64) Relaxed permits a reader to pair the fresh virtual
    // anchor with the stale/initial-zero real anchor, yielding a ~MAX_FRAME_DELTA_NS forward
    // jump. Release/Acquire (not sequencing) is what forbids that torn pair.
    REAL_ANCHOR_NS.store(real_ns(), Ordering::Relaxed);
    VIRT_ANCHOR_NS.store(virt, Ordering::Release);
}

/// The number of presented flips so far. Used to label diagnostic dumps and GPU-state
/// snapshots with the frame they were captured on.
pub fn flip_count() -> u64 {
    FLIP_COUNT.load(Ordering::Relaxed)
}

/// Virtual time as the phase and mode define it, before the monotonic floor is applied.
/// `last` is the caller's view of [`LAST_NS`] (the BOOT-phase cap is relative to it).
fn computed_ns(last: u64) -> u64 {
    // The single Acquire load of the flag (`VIRT_ANCHOR_NS`), reused for the sentinel check
    // and both mode branches below. Acquiring the flag here gates every dependent read: if
    // this observes a fresh anchor, the paired `REAL_ANCHOR_NS` store (and the FLIP_COUNT
    // bump) are guaranteed visible, so the Relaxed reads that follow are correctly
    // ordered-after and cannot form a torn pair.
    let virt_anchor = VIRT_ANCHOR_NS.load(Ordering::Acquire);
    if virt_anchor == 0 {
        // BOOT phase (before the first flip): real time, capped to at most one frame above
        // the previous read. Slow guest Update() sees delta <= one frame (no fast-forward),
        // yet a rapid spin-wait still climbs +FRAME_NS per read and catches up, so init
        // polls terminate.
        return real_ns().min(last + FRAME_NS);
    }
    match mode() {
        Mode::Realtime => {
            let real_anchor = REAL_ANCHOR_NS.load(Ordering::Relaxed);
            virt_anchor
                + real_ns()
                    .saturating_sub(real_anchor)
                    .min(MAX_FRAME_DELTA_NS)
        }
        Mode::FixedStep => virt_anchor + FLIP_COUNT.load(Ordering::Relaxed) * FRAME_NS,
    }
}

/// Current virtual guest time in nanoseconds since process start. Always strictly
/// increasing across reads (+1 µs floor), so guest spin-waits on the clock terminate in
/// either phase and either mode.
pub fn now_ns() -> u64 {
    let mut last = LAST_NS.load(Ordering::Relaxed);
    loop {
        // The BOOT-phase cap depends on `last`, which the CAS loop reloads on contention,
        // so the computed value is recomputed each iteration against the *current* `last`.
        let next = computed_ns(last).max(last + READ_FLOOR_NS);
        match LAST_NS.compare_exchange_weak(last, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return next,
            Err(cur) => last = cur,
        }
    }
}

/// Virtual time *without* advancing the clock: no monotonic floor is consumed and no
/// state is written, so an observer cannot perturb what the guest sees. Never below the
/// last value [`now_ns`] returned.
pub fn peek_ns() -> u64 {
    let last = LAST_NS.load(Ordering::Relaxed);
    computed_ns(last).max(last)
}

/// Emulated speed: `d(virtual) / d(real)` over the window since the previous sample.
///
/// Each consumer owns its own meter, so a 1 Hz window-title read and a 10 s profiler dump
/// measure their own windows instead of consuming each other's. Sampling uses [`peek_ns`]
/// and never perturbs the guest clock.
pub struct SpeedMeter {
    virt: u64,
    real: u64,
}

impl SpeedMeter {
    pub fn new() -> SpeedMeter {
        SpeedMeter {
            virt: peek_ns(),
            real: real_ns(),
        }
    }

    /// Percentage of real time the guest's world advanced by since the previous call.
    /// 100 = the guest experiences one second per real second.
    pub fn sample(&mut self) -> f64 {
        let (virt, real) = (peek_ns(), real_ns());
        let d_virt = virt.saturating_sub(self.virt);
        let d_real = real.saturating_sub(self.real);
        self.virt = virt;
        self.real = real;
        if d_real == 0 {
            0.0
        } else {
            d_virt as f64 / d_real as f64 * 100.0
        }
    }
}

impl Default for SpeedMeter {
    fn default() -> SpeedMeter {
        SpeedMeter::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread::sleep;
    use std::time::Duration;

    /// Force the mode and rewind the process-global clock state, so one test function can
    /// exercise several phase/mode combinations from a known start.
    fn reset(m: Mode) {
        MODE.store(m.code(), Ordering::Relaxed);
        FLIP_COUNT.store(0, Ordering::Relaxed);
        VIRT_ANCHOR_NS.store(0, Ordering::Relaxed);
        REAL_ANCHOR_NS.store(0, Ordering::Relaxed);
        LAST_NS.store(0, Ordering::Relaxed);
    }

    #[test]
    fn mode_parses_from_env_value() {
        // Unset / empty / unrecognized all resolve to `realtime`: guest world-time must run
        // at the correct rate by default, and an unparseable value should not silently opt a
        // run into the deterministic-but-host-speed-dependent mode.
        assert_eq!(parse_mode(None), Mode::Realtime);
        assert_eq!(parse_mode(Some("")), Mode::Realtime);
        assert_eq!(parse_mode(Some("realtime")), Mode::Realtime);
        assert_eq!(parse_mode(Some("fixed-step")), Mode::FixedStep);
        assert_eq!(parse_mode(Some("nonsense")), Mode::Realtime);
    }

    /// Single test on purpose: `MODE`/`FLIP_COUNT`/`VIRT_ANCHOR_NS`/`REAL_ANCHOR_NS`/
    /// `LAST_NS` are process-global statics, so splitting the phases and modes into
    /// separate `#[test]` fns would let cargo's parallel runner cross-contaminate them (a
    /// flip in one test flips the phase for the other). Everything runs here in order
    /// instead, with `reset` between sections.
    #[test]
    fn boot_cap_then_render_in_both_modes() {
        // ---- fixed-step: the pre-realtime behaviour, reproduced exactly ----
        reset(Mode::FixedStep);

        // (a) BOOT phase: rapid poll is strictly monotonic and each read rises by at most
        // one frame + floor. A tight loop barely advances real time, so the +1 µs floor
        // drives it — but it must never stall at a fixed value.
        let first = now_ns();
        let mut prev = first;
        for _ in 0..200 {
            let cur = now_ns();
            let delta = cur - prev;
            assert!(
                delta >= 1,
                "boot read must strictly increase (got {cur} after {prev})"
            );
            assert!(
                delta <= FRAME_NS + READ_FLOOR_NS,
                "boot per-read delta must be capped to one frame + floor (got {delta})"
            );
            prev = cur;
        }
        // (b) The rapid loop climbed rather than freezing at `first`.
        assert!(
            prev > first,
            "rapid boot poll must climb, not stall (first={first} last={prev})"
        );

        // (c) Cap under real elapsed time: sleep several frames of wall time, then a single
        // read must advance by at most one frame (cap held, no fast-forward) yet by close
        // to a full frame (the spin-wait catches up +FRAME_NS per read, not just +floor).
        let before = now_ns();
        sleep(Duration::from_millis(60)); // ~3.6 frames of real time
        let after = now_ns();
        let jump = after - before;
        assert!(
            jump <= FRAME_NS + READ_FLOOR_NS,
            "cap must hold across 60 ms of real time (got {jump}, one frame is {FRAME_NS})"
        );
        assert!(
            jump >= FRAME_NS / 2,
            "read after elapsed real time must catch up by ~one frame, not stall at floor (got {jump})"
        );

        // (d) BOOT→RENDER transition anchors to the virtual boot time: RENDER continues
        // from where the capped BOOT clock left off with no backward jump.
        let boot_last = now_ns();
        advance_frame(); // first flip: anchors VIRT_ANCHOR_NS = LAST_NS (virtual boot time)
        let render = now_ns();
        assert!(
            render >= boot_last,
            "RENDER must not jump backwards (boot={boot_last} render={render})"
        );
        assert_eq!(
            render,
            boot_last + FRAME_NS,
            "RENDER = virtual boot anchor + 1 flip x FRAME_NS"
        );

        // (e) fixed-step is a pure function of the flip count and ignores real time.
        for flips in 2..=5u64 {
            advance_frame();
            sleep(Duration::from_millis(5));
            assert_eq!(
                now_ns(),
                boot_last + flips * FRAME_NS,
                "fixed-step RENDER = anchor + flips x FRAME_NS, independent of real time"
            );
        }

        // ---- realtime: virtual time follows real time, clamped per frame ----
        reset(Mode::Realtime);

        // (f) BOOT phase is unchanged by the mode.
        let boot_before = now_ns();
        sleep(Duration::from_millis(60));
        let boot_jump = now_ns() - boot_before;
        assert!(
            boot_jump <= FRAME_NS + READ_FLOOR_NS,
            "boot cap is mode-independent (got {boot_jump})"
        );

        // (g) RENDER: real elapsed time, not flips. No flip happens during the sleep, yet
        // virtual time advances by ~the real span.
        advance_frame();
        let t0 = now_ns();
        sleep(Duration::from_millis(30));
        let elapsed = now_ns() - t0;
        assert!(
            (25_000_000..=45_000_000).contains(&elapsed),
            "realtime RENDER must track ~30 ms of real time without any flip (got {elapsed})"
        );

        // (h) The max-delta clamp bounds what one frame can observe, however long it took.
        advance_frame();
        let t1 = now_ns();
        sleep(Duration::from_millis(200)); // a hitch, ~12 frames of real time
        let hitch = now_ns() - t1;
        assert!(
            hitch <= MAX_FRAME_DELTA_NS + READ_FLOOR_NS,
            "a 200 ms hitch must be clamped to the max frame delta (got {hitch})"
        );

        // (i) The next flip re-anchors, so the clamp budget is per frame, not cumulative.
        advance_frame();
        let t2 = now_ns();
        sleep(Duration::from_millis(30));
        let after_reanchor = now_ns() - t2;
        assert!(
            after_reanchor >= 25_000_000,
            "a flip must re-arm the clamp window (got {after_reanchor})"
        );

        // (j) Strict monotonicity holds in RENDER too.
        let mut prev = now_ns();
        for _ in 0..200 {
            let cur = now_ns();
            assert!(
                cur > prev,
                "render read must strictly increase ({cur} <= {prev})"
            );
            prev = cur;
        }

        // (k) `peek_ns` observes without consuming the floor.
        let peek_a = peek_ns();
        let peek_b = peek_ns();
        assert!(peek_b >= peek_a, "peek must be monotonic");
        assert!(
            LAST_NS.load(Ordering::Relaxed) == prev,
            "peek must not write the clock"
        );

        // (l) The speed meter reports ~100% while the clamp is not biting.
        let mut meter = SpeedMeter::new();
        sleep(Duration::from_millis(50));
        advance_frame();
        sleep(Duration::from_millis(50));
        advance_frame();
        let speed = meter.sample();
        assert!(
            (80.0..=120.0).contains(&speed),
            "realtime with flips inside the clamp window must run at ~100% speed (got {speed})"
        );

        // (m) fixed-step at a host frame rate other than 60 Hz reports the wrong speed —
        // the defect this mode is retained despite, and the instrument that shows it.
        reset(Mode::FixedStep);
        advance_frame();
        let mut meter = SpeedMeter::new();
        for _ in 0..3 {
            sleep(Duration::from_millis(33)); // ~30 flips/s
            advance_frame();
        }
        let speed = meter.sample();
        assert!(
            speed < 70.0,
            "fixed-step at ~30 flips/s must report ~50% emulated speed (got {speed})"
        );
    }
}
