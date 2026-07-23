---
id: decision-8
title: >-
  Guest timekeeping: separate time base, vblank cadence and throttle — retire
  the fixed 16.67ms-per-flip clock
date: '2026-07-21 20:06'
status: accepted
---
## Context

The original clock reported guest time as a pure function of the number of
*presented host frames* — `base + FLIP_COUNT * FRAME_NS` (with
`FRAME_NS = 16_666_667`). Guest-visible time was a frame counter wearing a
clock's clothes.

That was the right call when the emulator rendered ~1 fps: every guest clock
read real host time, so a splash sequence fast-forwarded and was gone within
one or two presented frames. Pinning virtual time to presented frames made a
1 fps emulator watchable, plus a BOOT-phase per-read cap so init spin-waits
still terminated.

Once the emulator rendered gameplay at a real frame rate, the same design
produced a different bug: the guest advanced 16.667 ms of world time per
presented frame, so at 30 presented fps a 60 Hz title ran at exactly half
speed — smooth, evenly paced, and wrong. The perceived defect scaled precisely
with the presented frame rate.

The root cause is that three distinct concepts were collapsed into the single
event "the host finished presenting an image":

1. **what time is it** — every guest time API and the wall-clock epoch base
2. **when does the display tick** — vblank / flip events / flip status
3. **when may the emulator proceed** — throttling (can only ever *delay* a flip
   running ahead; cannot help once the host falls below the target rate)

Because all three were the same event, host rendering speed directly rewrote
the guest's experience of time.

## Decision

Separate the three concepts. Each gets its own source of truth.

### 1. Time base — derived from host monotonic time, clamped, monotonic

Virtual time advances with *real* elapsed time, not with presented frames:

```text
virtual_now = virtual_anchor + clamped_accumulation(real_now - real_anchor)
```

with two properties preserved because they are what make it safe:

- **Max-delta clamp.** A single advance is bounded (a few frames' worth). A
  host hitch, a breakpoint, or a slow stretch therefore cannot fast-forward the
  guest's world — the original fast-forward protection. This is the standard
  delta-time clamp; it is not a special case.
- **Strict monotonicity** with a per-read floor, so guest spin-waits on "has
  the clock changed" always terminate (the failure a flip-only clock caused).

### 2. Modes — realtime by default, fixed-step retained deliberately

- `realtime` (default): as above.
- `fixed-step`: the N-flips-times-16.67 ms behaviour, kept as an explicit
  opt-in.

Fixed-step is **not** legacy to be deleted. Deterministic virtual time is what
makes headless oracle baselines and the PNG visual oracle reproducible, and
determinism is why x86jit's `rdtsc` returns a constant. Removing it would make
the test corpus non-reproducible. It stops being the default; it does not stop
existing.

### 3. Emulated speed is an observable

Report `d(virtual)/d(real)` over a window as a percentage, in the profiler
table and the window title. An emulator that cannot state how fast it is
running relative to the machine it emulates is missing an instrument.

### 4. Vblank cadence is generated, not observed

The guest's flip events and flip status are satisfied from a periodic signal
derived from the time base at the **guest-requested** rate, rather than from
"our host finished a present". `sceVideoOutSetFlipRate` (0 = 60 Hz, 1 = 30 Hz,
2 = 20 Hz) must be honoured rather than discarded. A host that cannot keep up
then loses frames, which is correct, instead of slowing the world, which is
not.

### 5. Known hole, recorded not fixed here

x86jit lifts `rdtsc` to a **fixed timestamp**, deliberately, for whole-program
determinism. A guest that measures elapsed time via TSC bypasses this design
and sees zero elapsed time. If a title ever depends on it, that is an
x86jit-side change filed in the x86jit backlog — never edited from this repo.

## Consequences

**Good**

- Guest world-time runs at the correct rate at any host frame rate; the
  "smooth but in slow motion" class of defect is gone rather than rescaled.
- Host performance and guest correctness are decoupled: a perf regression shows
  up as fewer frames, not as a game that plays differently.
- Emulated speed becomes measurable, so this class of bug is caught by looking
  at one number.
- The guest's declared frame cadence is honoured instead of discarded.

**Costs and risks**

- This changes timing behaviour that earlier closed fixes were tuned against
  (splash fast-forward, GPU completion timing / command-buffer recycle,
  BOOT-phase per-read cap, intro loop / rewind). Each of those scenes must be
  re-verified by the maintainer's eyes; any one may have been leaning on the
  fixed-step clock without saying so.
- Deterministic replay is no longer the default, so anything that silently
  depended on determinism must be moved to the explicit `fixed-step` mode.
- Correcting the clock changes the audio/video relationship and may expose a
  separate buffering problem that the slow clock was masking.

**Rejected alternatives**

- *Patch `FRAME_NS` to match the measured frame rate.* Makes the guest's clock
  a function of our performance in a different way; the world still runs at
  whatever speed we happen to render.
- *Return raw host time.* Reintroduces the fast-forward. The clamp is the
  non-negotiable part of the design.
- *Keep flips as the vblank source and only fix the time base.* Leaves the
  guest gated by host present completion, so a slow host still throttles guest
  logic rather than dropping frames.
