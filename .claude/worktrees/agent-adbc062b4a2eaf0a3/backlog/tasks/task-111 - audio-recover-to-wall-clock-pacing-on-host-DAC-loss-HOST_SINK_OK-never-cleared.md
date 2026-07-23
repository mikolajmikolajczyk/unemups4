---
id: TASK-111
title: >-
  audio: recover to wall-clock pacing on host DAC loss (HOST_SINK_OK never
  cleared)
status: To Do
assignee: []
created_date: '2026-07-13 19:54'
labels:
  - audio
dependencies: []
priority: low
ordinal: 110000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Session review (opus) LOW/MEDIUM. sceAudioOutOutput backpressure waits until ring drains or period*4 elapses. HOST_SINK_OK is latched true once and never reset, so if the cpal callback stops draining mid-run (device removed/suspended) every Output blocks the full period*4 before the safety valve, and push_to_host's RING_CAP trim drops the just-submitted grain → guest audio clock slows to ~1/4 real-time with dropped audio, no recovery to the deadline fallback. Fix: detect a stalled consumer (e.g. cpal error/disconnect callback, or N consecutive safety-valve hits) → clear HOST_SINK_OK so Output reverts to wall-clock deadline pacing.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 a mid-run host-device loss reverts pacing to the wall-clock deadline fallback (no sustained ~1/4-rate stall)
- [ ] #2 recovery is driven by a real stall signal (cpal err/disconnect or repeated safety-valve), not a fixed timeout that would misfire under normal jitter
<!-- AC:END -->
