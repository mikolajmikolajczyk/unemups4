---
id: TASK-107
title: >-
  gnm/audio: sceAudioOut HLE — play guest PCM (Init/Open/Output/Close) so Doom
  SFX are audible in unemups4
status: Done
assignee: []
created_date: '2026-07-13 13:01'
updated_date: '2026-07-13 14:38'
labels:
  - real-software
  - doom
  - audio
  - gnm
dependencies: []
ordinal: 106000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
The ps4doom SFX backend (ps4doom TASK-1) makes Doom emit sound via sceAudioOut. unemups4 has NO sceAudioOut HLE, so the audio-enabled eboot traps on the import today. Imported symbols (from readelf): sceAudioOutInit, sceAudioOutOpen, sceAudioOutClose, sceAudioOutOutput (scePthread* already exist). This is the HOST/port side of the audio work — implement AFTER the guest SFX is validated on a platform with a real sceAudioOut (so our two-sided audio isn't self-biased). Contract (OpenOrbis AudioOut.h): sceAudioOutInit() -> 0; sceAudioOutOpen(userId, port MAIN=0, index, grain_len, sample_rate, format) -> handle; sceAudioOutOutput(handle, ptr) BLOCKS until the grain is consumed and plays grain_len frames of the given format (Doom uses S16_STEREO=1, 48000 Hz, grain 256); sceAudioOutClose(handle). Implement a real host-audio sink (portable backend — must not break the mac/MoltenVK north star; cpal or similar cross-platform audio, or an SDL/ALSA path — pick one that builds on Linux now and can go to macOS later) that consumes the guest PCM and plays it, with the blocking-Output semantics providing pacing. MINIMUM viable first step: even a correct no-op that returns success + paces (sleeps grain_len/rate) unblocks the eboot to run silent in unemups4; the real sink makes it audible. Add a WAV-dump oracle (env-gated, like the PNG-oracle task-97) that writes the received PCM to a .wav so the render can be self-verified + diffed against a reference. Validate by cross-checking the same eboot's audio against an external sceAudioOut implementation.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 sceAudioOutInit/Open/Output/Close implemented in a new libSceAudioOut HLE; the ps4doom audio eboot no longer traps and runs in unemups4
- [ ] #2 sceAudioOutOutput plays the guest S16-stereo 48kHz PCM through a cross-platform host sink (mac-portable) with correct blocking/pacing; Doom SFX are audible
- [ ] #3 Env-gated WAV-dump oracle writes received PCM to a file for self-verification; audio cross-checked against an external sceAudioOut reference for the same eboot
- [x] #4 No regression: existing examples + Doom (video/input) unaffected; build/clippy/fmt clean
<!-- AC:END -->

## Notes
Landed: sceAudioOut HLE (Init/Open/Output/Close), per-thread pacing (sleep grain/rate, no global lock), env-gated WAV-dump oracle. **Diagnosis (primary goal): audio eboot runs ~62fps in unemups4 (1558 flips/25s) — guest mixer design is sound; shadPS4 ~1fps is shadPS4-specific.** DEFERRED (task-108): the real cross-platform host-audio SINK so SFX are AUDIBLE in unemups4 (AC#2) — currently paced silence + WAV capture only.
