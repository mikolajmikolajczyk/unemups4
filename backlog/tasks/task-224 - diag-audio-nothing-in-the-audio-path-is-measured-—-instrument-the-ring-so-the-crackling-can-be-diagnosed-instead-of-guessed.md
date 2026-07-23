---
id: TASK-224
title: >-
  diag/audio: nothing in the audio path is measured — instrument the ring so the
  crackling can be diagnosed instead of guessed
status: To Do
assignee: []
created_date: '2026-07-22 10:47'
labels:
  - diag
  - audio
  - perf
dependencies: []
priority: high
ordinal: 229000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
The maintainer reports crackling in Celeste audio. There are at least three plausible causes and no data to choose between them, because crates/libs/src/libscaudioout/mod.rs contains no counters at all — not one AtomicU64, no underrun count, no drop count, no ring-occupancy figure.

What the code review found, none of it yet confirmed by measurement:

1. THREE audio ports, ONE ring, appended rather than mixed. Celeste calls sceAudioOutOpen three times. push_to_host (mod.rs:235) pushes every port's grain into a single global RING with push_back. Real hardware MIXES N ports into one output; concatenating them would interleave fragments of three streams in time, which sounds exactly like crackling. NOT verified: whether all three ports actually stream, or only one is used. That check comes first, because it decides between a missing mixer and something else entirely.

2. The backpressure is not in steady state. grain=256 at 48000 Hz is 5.333 ms per grain, so a healthy sceAudioOutOutput should block about one grain period. Measured average is 19-20 ms, against a safety valve at period * 4 = 21.3 ms (mod.rs:350). The guest is routinely blocking to the valve and breaking out of it, which means the ring is not draining to the target cushion.

3. Silent mid-stream sample loss. On overflow the ring drops the OLDEST samples (while guard.len() > RING_CAP { pop_front() }, mod.rs:257). Dropping from the middle of a stream is a click, not a graceful degradation, and nothing counts it.

A figure that does NOT fit any of the above and needs explaining: sceAudioOutOutputs was called about 10142 times in a run, which at 5.33 ms per grain is roughly 54 s of audio. If that run was around 250 s, we fed the DAC in about 22% of real time — which points at STARVATION, the opposite of the over-full ring implied by (1) and (2). Either the call batches several ports per invocation (the plural API takes an array), or the ring genuinely runs dry. Only a measurement separates these.

Instrument, following the house pattern (relaxed AtomicU64 behind the resolved UNEMUPS4_PROFILE gate, zero cost when unset, printed from app/unemups4/src/profiler_dump.rs, per-window deltas not cumulative):

- ring occupancy: min / mean / max over the window, in samples and in milliseconds of audio
- underruns: how many times PRIMED dropped back to false, and total silence emitted while unprimed
- overflow drops: how many samples pop_front discarded
- per-port submissions: count and total frames per audio handle, so it is immediately visible whether three ports stream or one
- how often the period * 4 safety valve fires, since a valve that fires routinely means the pacing model is not holding

This task is measurement only. Do not change the mixing, the ring size, the cushion or the pacing — the point is to stop guessing between three hypotheses that all predict crackling.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 ring occupancy (min/mean/max, samples and ms), underruns, overflow-dropped samples, per-port submission counts and safety-valve firings are reported per window
- [ ] #2 the per-port row makes it unambiguous whether Celeste streams one port or three
- [ ] #3 the numbers either explain the 10142-calls-vs-run-length discrepancy or state plainly that it remains unexplained
- [ ] #4 zero cost when UNEMUPS4_PROFILE is unset; no change to mixing, ring size, cushion or pacing
- [ ] #5 build + clippy clean, cargo test --workspace green
<!-- AC:END -->
