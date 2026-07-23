---
id: TASK-112
title: 'audio: handle non-stereo negotiated cpal channel count (hardcoded channels:2)'
status: To Do
assignee: []
created_date: '2026-07-13 19:54'
labels:
  - audio
dependencies: []
priority: low
ordinal: 111000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Session review (opus) LOW. build_output_stream requests StreamConfig{channels:2} unconditionally and drain_primed fills data.len() assuming 2-ch interleave from a stereo ring. If the default device only supports mono (or coerces the config), L/R interleaving is wrong (pitch/speed artifact). Typical desktop outputs accept stereo so this is latent. Fix: read the device's default_output_config channel count; if not 2, either down/up-mix correctly or decline the host sink (HOST_SINK_OK=false → WAV/pacing still work).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 a non-stereo negotiated output either mixes correctly or falls back to no-host-sink (HOST_SINK_OK=false) instead of mis-interleaving
<!-- AC:END -->
