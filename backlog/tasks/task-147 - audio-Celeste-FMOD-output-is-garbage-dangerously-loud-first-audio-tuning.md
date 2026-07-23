---
id: TASK-147
title: 'audio: Celeste FMOD output is garbage + dangerously loud (first-audio tuning)'
status: Done
assignee: []
created_date: '2026-07-16 14:09'
updated_date: '2026-07-16 16:41'
labels:
  - audio
  - retail
  - celeste
dependencies: []
priority: medium
ordinal: 153000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
After the x86jit VMASKMOVPS lift (task-259 / pin 942d253), Celeste's FMOD audio path produces actual output to the host (sceAudioOut) — a real milestone (first audio). BUT it is garbage/noise and dangerously LOUD (near ear-damage on headphones during testing). Likely causes (investigate once the game runs — blocked behind task-146 munmap abort for now): (a) the VMASKMOVPS masked load/store lift may have a subtle semantic bug (a masked-off lane written/read wrong) corrupting FMOD DSP buffers — cross-check x86jit task-259 Unicorn-diff coverage for the mask patterns FMOD uses; (b) sample format/channel/sample-rate mismatch between FMOD output and our sceAudioOut host submit (endianness, f32-vs-i16, interleave); (c) uninitialized/stale mix buffer. SAFETY: while debugging, clamp/attenuate host audio volume so live runs do not blast noise. Correctness/tuning, not a bring-up blocker — after Celeste renders.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 Root-caused why FMOD output is noise (masked-store lift bug / format mismatch / stale buffer)
- [x] #2 Audio output recognizable OR safely attenuated pending full fix; no ear-damage-loud output on a default run
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Done 2026-07-16 (merge f031de0). Root cause = candidate (a) FORMAT MISMATCH (our bug, NOT the VMASKMOVPS lift): FMOD opens FLOAT_8CH (fmt=5, 8ch f32, 32B/frame) + FLOAT_MONO (fmt=3) sceAudioOut ports, but the sink hard-assumed S16 (bytes_per_frame=ch*2, i16 decode) -> read f32 bytes as i16 pairs + wrong ch/frame size -> loud broadband noise. Fix (libscaudioout/mod.rs +102/-14): is_float_format(3..=5), bytes_per_sample 4/2, channels 1/2/8, per-format decode (f32 vs i16-norm), downmix to host stereo (mono dup, 8ch->front L/R first pass). SAFETY on-by-default: attenuate() every host sample = non-finite->0, *safe_gain (default 0.20 ~-14dB, env UNEMUPS4_AUDIO_GAIN clamp[0,1]), hard peak-clamp +/-0.35. 25 tests. CODE-REVIEW follow-up: the 8ch->stereo downmix takes ONLY front L/R (drops center/LFE/surround) -> wrong audio on multichannel titles (missing dialogue/bass); replace with a BS.775 matrix (Lo=L+0.707C+0.707Ls) when tuning first-audio. Recognizable-music quality = needs user's ears (audio oracle).
<!-- SECTION:NOTES:END -->
