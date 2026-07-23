---
id: TASK-108
title: >-
  libs/audio: real cross-platform host-audio sink for sceAudioOut (make Doom
  audible in unemups4)
status: Done
assignee: []
created_date: '2026-07-13 14:38'
updated_date: '2026-07-13 15:00'
labels:
  - real-software
  - doom
  - audio
dependencies: []
ordinal: 107000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
task-107 landed the sceAudioOut HLE with per-thread pacing + a WAV-dump oracle, but Output is a paced no-op — no actual sound comes out of unemups4 (audio is only heard on an external sceAudioOut like shadPS4, and captured to WAV here). Add a real host-audio sink: consume the guest S16-stereo 48kHz grains from sceAudioOutOutput and play them on the host. MUST stay portable for the mac/MoltenVK north star — use a cross-platform Rust audio backend (cpal is the natural choice: Linux ALSA/PulseAudio, Windows WASAPI, macOS CoreAudio) fed by a ring buffer the Output handler writes into; keep the blocking-Output pacing as the producer clock. Verify with the WAV oracle (dump == what's played) and by ear on Doom SFX.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 sceAudioOutOutput feeds a cpal (or equivalent cross-platform) host sink via a ring buffer; Doom SFX are audible when running the audio eboot in unemups4
- [x] #2 Portable — no Linux-only audio API on the critical path; builds toward the macOS target
- [x] #3 No fps regression (still ~60fps with audio); the WAV dump still matches what is played; build/clippy/fmt/examples green
<!-- AC:END -->
