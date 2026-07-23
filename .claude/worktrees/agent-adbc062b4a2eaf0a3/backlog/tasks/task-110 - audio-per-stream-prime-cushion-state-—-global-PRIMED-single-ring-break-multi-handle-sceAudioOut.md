---
id: TASK-110
title: >-
  audio: per-stream prime/cushion state — global PRIMED + single ring break
  multi-handle sceAudioOut
status: To Do
assignee: []
created_date: '2026-07-13 19:54'
labels:
  - audio
dependencies: []
priority: low
ordinal: 109000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Session review (opus) MEDIUM. libscaudioout uses a single process-wide PRIMED AtomicBool and one shared RING for all sceAudioOutOpen handles, though it presents as multi-handle (per-handle PORTS/DEADLINES). If a 2nd port opens (or one closes+reopens) while another streams, an underrun on one flips PRIMED=false for the callback serving ALL handles → forced re-prime (silence gap) on a healthy stream; two concurrent pushes also mix into one ring incorrectly. Latent: Doom opens one stereo port. Fix: per-ring prime/cushion + per-handle ring (or explicitly reject a 2nd concurrent port).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 prime/cushion state is per-ring, not a process-global; a 2nd stream's underrun cannot force a re-prime on another stream
- [ ] #2 either per-handle rings/streams, or a 2nd concurrent sceAudioOutOpen is explicitly declined (no silent sample mixing)
<!-- AC:END -->
