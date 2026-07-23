---
id: TASK-113.4
title: >-
  retail FASE 3: framework + platform interop — first real-engine GNM frame +
  audio
status: To Do
assignee: []
created_date: '2026-07-14 08:28'
labels:
  - retail
  - gpu
  - hle
dependencies:
  - TASK-113.3
parent_task_id: TASK-113
priority: medium
ordinal: 116000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
FASE 3 (parent epic). Once managed code runs (FASE 2), the managed framework drives the native platform layer: video-out init, graphics-device creation (-> GNM command buffers), audio-device init (-> native audio middleware), input, timing. METHOD: same triage loop, now hitting graphics/audio/input APIs. Wire the real engine's GNM submits into the existing GNM -> SPIR-V -> Vulkan path; real (non-synthetic) shaders will surface recompiler coverage gaps -> fix. Route the audio middleware to the sceAudioOut sink (or HLE its mixer). Keep recompiled SPIR-V MoltenVK/Metal-portable. Granular gaps filed pull-driven. Boundary: no crypto; assets local + gitignored.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 the engine's video-out + graphics device init succeed and a GNM command stream reaches our backend
- [ ] #2 the first real-engine GNM frame renders to the window (PNG-by-eye oracle)
- [ ] #3 the audio middleware initializes and produces first output through our sink
- [ ] #4 real-shader recompiler gaps surfaced here are filed as follow-up tasks
<!-- AC:END -->
