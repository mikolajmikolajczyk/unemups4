---
id: TASK-198
title: >-
  tools/gnm-scrape: pad-triggered capture toggle (R1+L1+X) + hard 10s safety cap
  — reach any scene on console then scrape it
status: Done
assignee: []
created_date: '2026-07-21 12:27'
updated_date: '2026-07-21 17:44'
labels:
  - tools
  - gnm-scrape
  - retail
  - dx
dependencies: []
priority: high
ordinal: 203000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
The GNM scraper (tools/ps4-gnm-scrape/plugin/source/main.c) currently auto-starts at plugin load and stops after MAX_FLIPS (3000, ~50s). That prevents capturing a scene you must first navigate to with the pad (e.g. Celeste's in-game night sky — no scrape exists for it because you can't reach it before the auto-capture ends). Add pad control so the maintainer drives to the target scene, then triggers the capture.\n\nRequirements:\n1. Poll the DualShock via the OpenOrbis SDK (data/oo_sdk/include/orbis/Pad.h, _types/pad.h, UserService.h). Init once (lazy on first use or in module_start): scePadInit(); sceUserServiceGetInitialUser(&userId); handle = scePadOpen(userId, ORBIS_PAD_PORT_TYPE_STANDARD, 0, NULL). Read each submit/flip: scePadReadState(handle, &data); inspect data.buttons.\n2. A single toggle combo R1+L1+X = ORBIS_PAD_BUTTON_R1|L1|CROSS = 0x0800|0x0400|0x4000 = 0x4C00. EDGE-DETECT it (fire only on the not-pressed -> pressed transition, so holding does not re-toggle) to flip a g_capturing gate.\n3. Replace the auto-start-at-load behaviour: capture is DISABLED until the combo starts it. On START: reset g_frame, g_flips, g_capture_done, and record the start wall-clock time so each capture is a fresh numbered sequence. Toggling again STOPS it.\n4. HARD SAFETY CAP (the maintainer is worried about data volume): auto-stop the capture at most ~10 seconds after it STARTED, regardless of the toggle. Use wall-clock (gettimeofday is already included via sys/time.h) as the primary cap — stop when now - start >= CAPTURE_MAX_SECONDS (define = 10). Keep a flip-count backstop too (e.g. CAPTURE_MAX_FLIPS ~700) in case timing is odd; stop on whichever hits first. Make both #defines with sane values and a LOG line when the cap fires.\n5. Add -lScePad -lSceUserService to the Makefile LIBS; add the includes. Rebuild the .prx (OO SDK + GoldHEN SDK toolchain present; data/oo_sdk, data/goldhen_sdk).\n\nNotes: scePadReadState on our own handle reads controller state independently and does NOT steal input from the game (the combo also reaches the game — that is acceptable). The host receiver (tools/ps4-gnm-scrape/host) is unchanged; a toggle-start resets frame numbering, so the maintainer restarts the host receiver before triggering (document this in a one-line LOG or SETUP.md note).\n\nProvenance: use ONLY the OpenOrbis SDK headers for the pad API.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 R1+L1+X (edge-detected) toggles capture start/stop; capture is OFF until first triggered (no auto-start at load)
- [x] #2 capture auto-stops at most ~10s after it started (wall-clock primary cap, flip-count backstop), with a LOG line when the cap fires
- [x] #3 on start, frame/flip counters reset so each capture is a fresh sequence; Makefile links -lScePad -lSceUserService and the .prx rebuilds clean
- [x] #4 SETUP.md (or a LOG line) notes that the host receiver should be (re)started before each triggered capture
<!-- AC:END -->
