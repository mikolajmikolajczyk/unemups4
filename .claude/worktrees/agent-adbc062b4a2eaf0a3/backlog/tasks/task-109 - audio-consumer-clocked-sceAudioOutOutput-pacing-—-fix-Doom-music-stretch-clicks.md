---
id: TASK-109
title: >-
  audio: consumer-clocked sceAudioOutOutput pacing — fix Doom music stretch +
  clicks
status: Done
assignee: []
created_date: '2026-07-13 18:15'
updated_date: '2026-07-13 18:16'
labels:
  - audio
  - doom
dependencies: []
ordinal: 108000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
The cpal host sink (task-108) paced sceAudioOutOutput with a fixed software wall-clock, an independent second clock from the cpal DAC. Under OPL music (heavier render) this stretched audio ~2x, and even at real-time the two clocks drifted → periodic clicks. Make the DAC the sole master via ring backpressure; keep a wall-clock fallback for headless/WAV runs.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 sceAudioOutOutput blocks on ring occupancy (DAC-clocked) when a host sink is active; wall-clock deadline fallback when none (headless/CI/WAV)
- [x] #2 Prefill cushion + re-prime on underrun in the cpal callback so producer jitter doesn't starve the consumer
- [x] #3 Doom (ps4doom) audio plays at correct tempo/pitch, SFX + OPL music together, no stretch and no clicks (user-verified)
- [x] #4 No regression: full speed, no panic; WAV oracle still ~real-time
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Done 2026-07-13. Root cause: task-108 paced Output on a fixed software clock independent of the cpal DAC → (a) fixed post-render sleep double-counted OPL render time → ~2x stretch; (b) two-clock drift → periodic clicks. Fix: DAC-clocked ring backpressure (Output blocks until consumer drains to cushion) + prefill/re-prime in the cpal callback; wall-clock deadline fallback for headless/WAV. User-verified clean with ps4doom (no stretch, no clicks, full speed). WAV oracle ~real-time.
<!-- SECTION:NOTES:END -->
