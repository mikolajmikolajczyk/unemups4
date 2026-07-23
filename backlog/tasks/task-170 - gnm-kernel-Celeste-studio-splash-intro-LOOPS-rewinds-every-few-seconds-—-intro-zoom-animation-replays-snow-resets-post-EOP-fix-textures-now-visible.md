---
id: TASK-170
title: >-
  gnm/kernel: Celeste studio-splash intro LOOPS/rewinds every few seconds —
  intro-zoom animation replays + snow resets (post-EOP-fix, textures now
  visible)
status: Done
assignee: []
created_date: '2026-07-18 08:50'
updated_date: '2026-07-23 18:41'
labels:
  - gnm
  - kernel
  - celeste
  - retail
  - timing
dependencies: []
priority: high
ordinal: 174000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
After the EOP-fence fix (task-157) Celeste's studio splash renders textured, but the intro ANIMATION replays periodically (every ~few seconds / dozens of frames): the 'Matt Makes Games Inc.' text does its opening zoom (large->normal) + light-burst, settles for dozens of frames, then RE-PLAYS from the zoom, and the snow particles RESET/rewind with it. Confirmed by PNG oracle: frame278/281 = text stretched huge + bloom burst (intro-zoom opening), frame282/285 = normal settled text; the cycle repeats. This is the long-standing 'cofa się' the maintainer flagged all along (task-162 fixed the fast-forward; task-169 the warm-up delta; this periodic loop remains). The guest DOES eventually advance to the title screen, so it's a partial loop. ROOT-CAUSE CANDIDATES: (1) a guest time source we feed has a PERIODIC discontinuity/jump (sce_kernel_get_process_time / _counter / gettimeofday->virtual_epoch_ns / sys_clock_gettime — clock.rs); (2) the splash->advance condition the guest polls (a fade-complete timer, an asset-load flag, an event) never durably satisfies so it re-triggers the intro; (3) a delta-time spike that overshoots the animation then resets. METHOD: instrument ALL guest time reads (per-frame values, flag non-monotonic/periodic jumps), characterize the loop period + correlate to the time values, identify the advance condition. Real-PS4 scraper (task-168) can capture real Celeste's intro draw pattern as a reference (does real HW replay the intro? almost certainly not). Relates task-162/169 (clock), task-157 (textures, done).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 The periodic intro loop/rewind is root-caused to a specific our-side cause (time-source discontinuity, advance-condition, or delta-time)
- [ ] #2 The studio splash plays through ONCE and advances without replaying the intro-zoom / resetting the snow (PNG/live oracle)
<!-- AC:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Opus/worktree. Instrument WSZYSTKIE guest time sources (clock.rs HLEs: get_process_time, _counter, gettimeofday->virtual_epoch_ns, sys_clock_gettime) + sprawdzić czy gość czyta rdtsc BEZPOŚREDNIO (nie przez HLE) — jeśli tak, x86jit przepuszcza host TSC (real-rate) i mismatch z wirtualnym zegarem daje periodyczny dt-spike. Reprodukować splash, logować per-frame wartości czasu, ZMIERZYĆ okres pętli (fixpng: 278/281 stretched, 282/285 normal), znaleźć nieciągłość/spike LUB advance-condition która się nie utrwala. Real-PS4 scraper (task-168) = referencja czy real HW zapętla intro (prawie na pewno nie). Fix tylko jeśli evidence-pinned + weryfikacja PNG (splash gra RAZ, bez replay zoomu / resetu śniegu).
<!-- SECTION:PLAN:END -->

## Notes

<!-- SECTION:NOTES:BEGIN -->
### Established priors (2026-07-18, carried from prior sessions — do NOT re-chase)
- Loop period ≈ **27 flips**; clean **square wave** — an animation-progress value oscillates 0→1→0.
- **NOT a timing/clock discontinuity.** All guest HLE clocks (clock.rs `now_ns` + consumers `get_process_time`/`_counter`, `gettimeofday`→`virtual_epoch_ns`, `sys_clock_gettime`) strictly monotonic. **rdtsc in x86jit = CONSTANT 0x12345678** (NOT host TSC) → an rdtsc-driven animation would FREEZE, not oscillate; it oscillates ⇒ animation driven by the virtual monotonic clock, not rdtsc.
- **NOT the User Service** event path (login delivered once; NO_EVENT polling is normal).
- Intro plays **~9×** over ~4s before advancing to title. Real HW almost certainly plays it ONCE → advance-condition takes ~9× too long on our emu. It's a PARTIAL loop, not a hang.
- task-162 fixed fast-forward; task-169 (merged <prior-history>) capped BOOT clock delta → smoother (~40fps) but loop REMAINS.

### Investigation results (2026-07-18, opus headless — worktree agent-a72ec28c3e17364e9, instrumentation UNCOMMITTED)

**Phase A — per-flip DCB draw-signature timeline diff (real corpus vs our headless capture):**
- **Real HW (300 flips):** frame0 setup → **5-draw intro overlay frames 2–54 (~28 flips)** → 4-draw steady splash held the whole rest (272 flips). Pure double-buffer ping-pong; NO structural replay; corpus never reaches title (splash is a multi-second screen).
- **Ours (919 flips):** frame0 → 5-draw intro **only frames 1–4 (~5 flips)** → 4-draw steady (5–304) → 5-draw (305–636) → 6-draw (637–876) → **title menu @ flip 883.** Advances splash→title.
- **DCB-pinned divergence: intro 5-draw overlay = ~28 flips real vs ~5 ours — we drop the intro overlay ~6× too early.** (OPEN: is our 5-draw block @305–636 the intro overlay REPLAYING, or a coincidentally-5-draw different scene? needs ctx-hash/SET_SH_REG compare of flip 2 vs flip ~400 — not yet done.)

**Phase B:**
1. **Clock EXONERATED.** `UNEMUPS4_CLOCKLOG`: guest drives timing via `sceKernelGetProcessTimeCounter` (6938 calls) + `clock_gettime` (6020); across ALL reads strictly monotonic (0 non-monotonic), exactly 1 FRAME_NS/flip, no discontinuity/reset. Periodic-time-jump / delta-spike hypotheses DEAD post-169.
2. **VBHASH (referenced dynamic-buffer content hash).** Splash animation lives entirely in referenced dyn vertex/uniform buffers (Scene-B DCB byte-identical every other frame, zero inline anim data). Buffers are **by-design sawtooth loops** (~27 update-frames then hard-reset), and DIFFERENT buffers reset at DIFFERENT flips (tied to their scene block) → independent periodic animations, NOT one global scene re-creation. No settle-plateau-reset shape found headless.

**Root cause — ranked (NOT fixed):**
- **#1 (leading): content-loading splash whose looping idle animation we display for too many loops before the advance-gate resolves.** Heavy repeated content loading during splash (portrait atlas dirs Stat'd 100s of times: `ghost` 501×, `madeline` 315× — genuinely directories, our FS correct). Loop is by-design; the bug is LINGERING. Falsifiable next: trace the exact per-loop poll the splash checks (content-load-complete flag / equeue event / FMOD/audio init), confirm "not ready" for N loops then "ready" at advance; compare N to real HW's ~1.
- **#2: BOOT-phase cumulative phantom time skips the one-shot opening fade.** Boot virtual clock climbs 5.18→13.75 s over 404 boot reads (~6.7 s injected; task-169 caps PER-READ delta but 404 reads still accumulate). A one-shot fade/timer anchored during boot sees huge elapsed @flip1 → completes instantly → concrete mechanism for the 28→5-flip intro-overlay divergence. Falsifiable next: clamp CUMULATIVE boot phantom time so first-flip virtual ≈ one real frame-0 workload, re-measure intro-overlay span (expect it to grow toward ~27).

**Honest caveat:** headless faithfully runs the guest logic but did NOT reproduce the maintainer's specific "settle for dozens of frames then replay" PLATEAU (all hashed buffers are continuous loops). Arbitrating "loop is a bug" vs "loop is normal, we just linger" needs real-HW referenced-buffer hashes (corpus has DCBs only) or the live PNG oracle.

**Proposed direction (NOT implemented, confirm-first):** identify + HLE the splash advance-gate (likely async content-load-complete or an equeue/audio-ready event) so it resolves in ~1 loop like real HW; separately cap cumulative BOOT phantom time so the opening fade isn't skipped. Run the Phase-B poll trace + the #2 clamp experiment BEFORE touching code.

**Instrumentation (env-gated, uncommitted, in worktree agent-a72ec28c3e17364e9, reusable):**
- `clock.rs` `flip_count()` accessor + `UNEMUPS4_BOOTCLAMP_NS` ceiling; `libkernel/mod.rs` `clocklog()` in the 4 time HLEs (`UNEMUPS4_CLOCKLOG=1`); `libscegnmdriver/submit.rs` `vbhash_probe()` on flip DCBs (`UNEMUPS4_VBHASH=1`); `tools/ps4-gnm-scrape/host/src/bin/sig.rs` per-flip DCB-signature CSV (`sig <dir-of-*_dcb.bin>`). Scratch analysis under worktree `scratch/`.

### Follow-up experiments (2026-07-18, opus headless — DECISIVE eliminations)

**Exp 1 — structural-replay disambiguation: RULED OUT.** Shader-program bind fingerprint (VS/PS `SPI_SHADER_PGM_LO` low-12, load-base-stable), calibrated on real HW (intro f30 = VS e59/PS 47f; steady f150 = 6f8/6f7):

| flip | VS | PS | scene |
|---|---|---|---|
| OUR intro f3 | e59 | 47f | intro overlay |
| OUR 5-draw@400 | 6f8 | 6f7 | steady splash |
| REAL intro f30 | e59 | 47f | intro overlay |
| REAL steady f150 | 6f8 | 6f7 | steady splash |

Our recurring 5-draw@305–636 is a DIFFERENT scene (6f8/6f7 == real HW's steady splash), NOT the intro overlay re-entering. Intro overlay appears exactly ONCE (f0–4), same as real HW. **Structural scene replay is ruled out** — the visible "zoom rewind" is the referenced dynamic-buffer sawtooth, not a scene re-render.

**Exp 2 — BOOT cumulative-phantom-time clamp (`UNEMUPS4_BOOTCLAMP_NS`): NON-VIABLE.** Any absolute BOOT ceiling deadlocks boot — 1 frame / 0.5 s / 2 s all → maxflip=0. CLOCKLOG shows the boot spin-wait polls `gettimeofday`/`clock_gettime` and needs >2 s of virtual boot time to terminate (exceeds the 0.47 s / 28-flip intro), so no ceiling can both let boot proceed AND preserve the intro. Empirically confirms task-169's stall warning. Candidate #2 (boot phantom time → intro overlay 5 vs 28 flips) is a real but SEPARATE minor cosmetic divergence, NOT fixable by boot clamping.

**Combined verdict + the remaining gap:** clock ruled out (Phase B), structural replay ruled out (Exp 1), boot-clamp fix ruled out (Exp 2). The visible "cofa się" is the referenced dynamic-buffer sawtooth (ramp→hard-reset every ~27 frames) shown across the multi-second splash. **CRITICAL UNKNOWN: we cannot tell whether that sawtooth is FAITHFUL to real HW — the scraper corpus captures DCBs only, NOT the referenced vertex/uniform buffer CONTENT, so "sawtooth is by-design" is an assumption, not evidence.** Note real HW ALSO holds the splash multi-second (300+ flips, never reached title in the 5 s corpus) — so "we linger too long / advance-gate late" is likely a MISFRAME; the splash is meant to be long. The open question is purely whether OUR per-frame animation-buffer content matches real HW's or diverges (jarring snap vs smooth loop).

**Next decisive options (need user direction — headless DCB analysis is exhausted):**
1. **Extend the scraper (task-168) to also dump real-HW referenced vertex/uniform buffer CONTENT during the splash** → frame-by-frame compare our buffer content vs real HW's = ground truth on whether the sawtooth is faithful. Requires the user's PS4 + a scraper enhancement.
2. **Guest-side Mono-runtime RE:** find what re-triggers/restarts the animation timeline each loop (the value or call that resets animation phase to 0) — same shaders, so it's a buffer-content driver, not a scene change.
3. **Live PNG/video oracle (maintainer's eyes):** characterize the visible artifact (smooth loop vs jarring snap) + what real Celeste's studio splash should do (one-shot zoom-then-hold vs looping), to pick between 1 and 2.

### MAJOR REFRAME (2026-07-18, task-172 real-HW buffer-content oracle — VERDICT FAITHFUL)
Built the scraper extension (task-172) and captured real-HW referenced-buffer CONTENT (600 flips), then diffed real-vs-ours keyed on role. **The animation-buffer content is FAITHFUL to real hardware:**
- Real HW's steady transform CB is ITSELF a ~28-flip ramp-and-reset SAWTOOTH (float[0] -0.0364→-0.0371 then hard-reset), matching our ~27-flip loop in shape AND period — **the periodic "cofa się" loop happens on real hardware too. It is by-design, NOT our bug.**
- UI quad geometry byte-identical (ours `(210,448)(1710,448)(210,647)(1710,647)` == real). Intro transform is a one-shot smooth ramp (no reset). Snow/sprite dynamic on both.
- So the referenced-buffer content is NOT the cause; we do not compute it wrong.

**What actually remains as our-side divergence (the only evidence-pinned one):** the INTRO-OVERLAY DURATION. Real HW eases the opening "Matt Makes Games" zoom over ~27 flips (~0.45s) then drops the overlay; ours drops it after ~4 flips — the boot-clock phantom time (virtual clock ~13.7s at flip 1) compresses the guest's one-shot ease so the dramatic zoom plays near-instantly. This is NOT fixable by boot-clamping (task-172 Exp 2: any cumulative BOOT ceiling deadlocks boot, which legitimately needs >2s of virtual time). A different fix (faster real boot so wall-boot ≈ real HW's, or HLE-ing the specific boot gettimeofday wait so it terminates without needing seconds of virtual time) would be needed — pursue only if visually significant.

**Open ONLY on the live/PNG oracle now (numbers are exhausted — content matches real HW either way):** is the maintainer's "cofa się" (a) the FAITHFUL ~27-flip sawtooth loop (i.e. normal Celeste, not a bug — possibly just more jarring at our lower/variable framerate, or a sampler wrap-mode rendering diff making a seamless scroll snap), or (b) the compressed intro-overlay ease (zoom too fast then vanishing)? The eye decides whether there is still a bug to fix and which one. If (a)-normal → task-170 may largely be a non-bug; if sampler-wrap snap → relates task-171/task-56 (RT/sampler); if (b) → the intro-duration item above.

### LIVE ORACLE (2026-07-18, maintainer's eyes) — decisive
Maintainer ran main (task-169) and reported: **(1) the snow/background scrolls SMOOTHLY then JUMPS/snaps back** (~27-flip) — a JARRING SNAP, not a smooth loop → matches the sampler wrap-mode hypothesis (faithful sawtooth scroll offset + our sampler CLAMP instead of REPEAT = visible snap at the wrap). **(2) "Matt Makes Games" no longer stretches — STABLE now** → the intro-ease-compression concern is resolved in practice (task-169 + faithful content); drop that thread. **(3) scenes overlap + textures flicker + eventually the WHOLE texture atlas flashes onscreen** (screenshot) → that is **task-171** (RT/compositing — draws that should target offscreen render-targets or be composited all land on the final framebuffer).
**So task-170 splits cleanly:** the intro-zoom part is DONE (stable); the snow "cofa" is a SAMPLER WRAP-MODE bug (headless check dispatched — decode real S# wrap fields for draw1/6f8 vs our VkSampler address mode); the atlas-splatter/flicker is task-171, the dominant remaining visual bug. task-170's residual = the sampler-wrap fix (if confirmed divergent).

### Sampler check RESULT (2026-07-18, headless) — snow-snap sampler hypothesis REFUTED
Decoded real-HW S# wrap fields (word0[2:0]=CLAMP_X, [5:3]=CLAMP_Y), consistent across frames 30/150/300:
- **draw2 (SNOW, 134×126 tile): real = WRAP/WRAP.** Ours: `decode_s_sharp` (vbuf.rs:426) reads only the filter bit; all 3 `SamplerDesc` in exec.rs (:1048/:1149/:1235) **hardcode `SamplerAddressMode::Repeat` (=WRAP)**. → **real WRAP ↔ ours REPEAT = MATCH.** Sampler is NOT the snow-snap cause.
- draw3 (atlas, 1922×1082): real = WRAP → also matches ours.
- **draw1 (banner/backdrop, 1500×199): real = CLAMP_EDGE, ours = REPEAT (we ignore the S# wrap field entirely).** This IS a real bug — but the OPPOSITE direction (we wrap where real clamps → *less* snapping, not more), so it cannot cause a snap real HW lacks. It affects the backdrop, not the snow; may contribute to task-171 edge/bleed artifacts.

**Snow snap conclusion:** NOT content (task-172 faithful sawtooth on both sides), NOT sampler (WRAP matches). Remaining possibilities: (a) NORMAL — real HW's snow also snaps at the reset (the buffer content says it might); (b) a UV-precision / per-particle-reset detail in our recompiled snow PS/vertex path. Decisive next step needs a real-HW snow VIDEO/PNG side-by-side (static frames insufficient for motion). LOW priority vs task-171. **Snow snap PARKED (sampler+content exonerated).**

**Spun out: the ignore-S#-wrap bug** (we hardcode REPEAT, never decode CLAMP_X/Y) → its own follow-up (decode word0[2:0]/[5:3] → address mode incl. MirrorRepeat, thread through SamplerState/Desc, set from S# at the 3 exec.rs sites, extend vk_address_mode). Small, well-specified, corrects draw1 + any CLAMP texture; relates task-171 edge artifacts.

### === THE REAL BLOCKER — SESSION HANDOFF 2026-07-18 (next agent: START HERE) ===
Celeste now boots past the studio splash and REACHES the 2D CELESTE attract/title screen (mountain + CELESTE logo + snow + gradient — screenshot Image#19). It renders but with two cosmetic bugs (below) AND, critically, **does not advance to the interactive menu / never reads input**, so we're stuck here.

**The blocker, precisely (all ruled out this session — do NOT re-chase these):**
- Guest calls `scePadInit()`→0 and `scePadOpen(1,0,0)`→handle, then **NEVER polls the pad**: 0 `scePadReadState`/`…Ext`, 0 `scePadGetControllerInformation`, 0 setter calls (verified via UNEMUPS4_PAD_TRACE, full 130s run). Pressing Enter/X does nothing because there is no read.
- **Input is NOT the blocker.** Host→guest wiring is proven correct (one shared Arc<RwLock<PadState>>, CROSS=0x4000, Enter/LCtrl→CROSS). We even added the missing Ext handlers (scePadReadStateExt/ReadExt/GetControllerInformation) — a real gap Celeste needs at the menu, uncommitted in worktree agent-aa80d32e202cf6be4 — but the guest still never polls.
- **Handle value NOT the blocker.** Our scePadOpen returns a large arena handle 0x1000001; forced small handle (1, like real HW) via UNEMUPS4_PAD_SMALL_HANDLE=1 — maintainer tested, no change.
- **User Service NOT the gate.** Our HLE is complete + Celeste-tuned (delivers the initial-user LOGIN — that's WHY we reach attract); the `Couldn't get event from User Service 0x80960009` NO_EVENT spam is normal steady-state polling.
- **Controller::Init `for(;;)` NOT hit.** The homebrew init pattern (scePadInit==0 && scePadOpen>=0) succeeds on our side.
- **Splash animation loop = FAITHFUL** (task-172 real-HW buffer-content oracle: real HW's transform CB is itself a ~27-flip sawtooth). Intro-zoom now stable. The "cofa się" is NOT the blocker.

**Conclusion:** the guest opens the pad but its per-frame INPUT-POLL loop never runs on the attract screen — the game-logic/Mono thread is stuck/blocked BEFORE the input-reading state, or the attract→menu transition is gated on something (async init? a timer? a Mono exception/deadlock? a worker thread?) that never completes. **NEXT STEP = a GUEST-SIDE EXECUTION TRACE:** what is the guest main/logic thread doing during attract? Is it in a `for(;;)` spin (constant RIP / tight backward jmp)? Blocked on a specific syscall (which one, repeatedly)? A worker thread wedged? A managed-runtime (Mono full-AOT) deadlock/exception swallowed? Instrument the x86jit main-thread RIP / syscall pattern during attract and find where it's stuck. This is THE keystone — clearing it → reach menu → input works → validate/land task-171/174 (RT dual-CB) → progress.

**Two cosmetic bugs on the reachable attract screen (parallel, lower priority):**
- **task-175 R/B color swap** — attract renders WARM (pink mountain/gold bg) vs correct COOL (blue/navy, console-capture ref). Root-caused (NOT a present-path fix): splash direct-scanout color is BGRA; the global `swap_rb` (backend.rs:1662-67) is a splash-era hack that wrongly swaps every RGBA (texture) scene = the title. Fix = make the splash color SOURCE emit logical RGBA (likely a packed color in a constant buffer unpacked BGRA) then DROP the global swap. See task-175 notes.
- **Snow jitter** — snow drifts L-R in place instead of falling smoothly (maintainer live). Not yet investigated; particle/vertex or time-pacing. No task filed yet.

**State of tree:** main @<prior-history>. Uncommitted/pending: task-177 Ext-handler fix + PAD_TRACE (worktree agent-aa80d32e202cf6be4, worth landing — real gap + diagnostic). The maintainer's throwaway UNEMUPS4_PAD_SMALL_HANDLE edit is in the main checkout (env-gated, revert or ignore). task-174 dual-CB probe documented (UNEMUPS4_DUAL_CB_VS_ONLY), worktree removed.

### GUEST-SIDE EXECUTION TRACE — per-thread verdict (2026-07-18, opus headless, worktree agent-ab10a383bb75161e9, instrumentation UNCOMMITTED, rebased onto main @<prior-history>)

**Tooling built (env-gated `UNEMUPS4_EXECTRACE=1`, or `=<secs>` dump interval, default 5s):** per-thread (a) syscall histogram (blocking waits flagged `[BLOCK]`), (b) RIP histogram (budget-driven sampling → spin vs varied), (c) host-park heartbeat (park_enter/exit around the sync HLEs → "tid X PARKED Ns on <primitive>@<addr>"), (d) main-thread rbp-chain backtrace sample. Files (all uncommitted): NEW `crates/core/src/exectrace.rs`; edits `crates/core/src/lib.rs`, `crates/cpu/src/exec.rs` (run-loop: budget from `exectrace::rip_budget`, RIP sample in the BudgetExhausted arm, syscall record + main-thread bt in the Syscall arm), `crates/kernel/src/sync.rs` (park_enter/exit around mutex_lock + cond_wait), `app/unemups4/src/main.rs` (install syscall-id→name resolver + `exectrace::start`). Build + clippy clean gate-off. (NB: this worktree started BEHIND main at 5cf0840 where Celeste crashes at boot with the old Mono argv/dirname-NULL wall; fast-forwarded to main to reproduce attract.)

**Run:** 155s headless, 23 dumps, 14 guest threads at attract steady-state (t+60–115s). **Zero fatals** — Celeste boots, reaches attract, keeps rendering (draws advance every dump). Repro on this box: Nix-built binary vs system glibc skew → `patchelf --set-interpreter /usr/lib64/ld-linux-x86-64.so.2 --set-rpath /usr/lib scratch/unemups4.sys`, then `LD_LIBRARY_PATH= UNEMUPS4_EXECTRACE=5 RUST_LOG="warn,ps4_core::exectrace=info" ./scratch/unemups4.sys <eboot>`. (In the maintainer's normal devShell: just `UNEMUPS4_EXECTRACE=1 cargo run --release`.)

**Per-thread at attract (t+115s):**
- **tid 1 (MAIN / logic+render): RUNNING HOT — not hung, not parked, not frozen-spinning.** 52,700 syscalls/s, overwhelmingly Mono managed churn (scePthreadGetspecific 3.1M, mutex lock/unlock/trylock ~1.8M, scePthreadSelf 616k, scePthreadYield 71k, clock_gettime 76k) PLUS a steady stream of **sceGnmDrawIndexOffset (14,750→15,850, ~220 draws/s = frames advancing).** Hot RIPs all inside ONE function `0x3333b7c–0x3333e6e` (the Mono managed loop body). RIP samples froze (0/s new) because the loop is so syscall-dense (a syscall ≈ every 20k insns) the 200k budget never exhausts. The game's main loop ticks and renders every frame.
- **tid 2 (direct-memory/resource loader; RIP 0x1aaaxxx; did sceKernelAllocate/MapDirectMemory):** active during boot (woke periodically), then **from t≈58s PARKED CONTINUOUSLY on pthread_cond_wait cond@0x45b9c68 mtx@0x45b9c60 for the remaining 57s, never signalled** (rip_samples frozen, 0 syscalls). Idle worker, queue drained. **Main thread runs FREE and does not wait on it → this park does not gate progress.**
- **tid 5:** 8.5M scePthreadGetspecific, varied 271 RIPs @0x246axxx — second hot managed thread. Running.
- **tid 4 (sceNgs2SystemRender+sceAudioOutOutputs) + tid 8 (sceAudioOutOutputs+SignalSema):** audio mixer/output, running every frame. **tid 6/7/9/14:** Mono/job workers (mutex + GetProcessTimeCounter + WaitSema/SignalSema), running. **tid 11/12/13:** file-stream/asset threads actively sceKernelRead/Open/Fstat/Lseek at attract — **loading works** (FS "No such file" errors are BOOT-ONLY: 47, all in first ~1s = Mono probing optional paths; none during attract).

**System-wide:** **ZERO scePad* calls anywhere in the trace** (no ReadState/…Ext/GetControllerInformation — confirms input never polled). **sceUserServiceGetEvent returns NO_EVENT (0x80960009) EVERY FRAME — 2081× continuously through attract.**

**RANKED VERDICT:**
1. **H4 (loop runs + ticks, input-poll behind a game-state gate) — CONFIRMED.** Main thread renders + ticks every frame; audio+streaming+workers all run; nothing hung. The game never enters the state that polls input.
2. **H1 (busy-wait spin on a guest flag) — shape only, NOT a frozen spin:** the tight syscall-dense managed loop *looks* spin-like, but draws advance → it is a running loop, not a stuck spinner.
3. **H3 (worker wedge blocks main) — NO:** tid 2 permanently idle-parks at attract, but main is independent of it.
4. **H2 (main blocked in a syscall) — NO:** main is the busiest thread, never durably blocked.

**Why input-poll / attract→menu never fires:** the per-frame logic runs but stays non-interactive; the strongest evidence-pinned correlate for the gate is the **perpetual sceUserServiceGetEvent → NO_EVENT** stream — the game polls the User Service event queue each frame and (after the one-time login delivery) never receives another event. ⚠️ This partly conflicts with the earlier "User Service ruled out (NO_EVENT normal)" prior — but that was assessed for the SPLASH loop; here it is the top attract-gate correlate and warrants re-examination for the attract→menu transition specifically.

**EVIDENCE-PINNED FIX DIRECTION (NOT implemented — confirm-first):**
- **Instrument the guest's sceUserServiceGetEvent consumer**: log call site + the event TYPE the guest branches on, and whether it loops until a specific event. Determine if the game awaits a follow-up event our HLE never synthesizes after the single login (e.g. a second login, a controller-bound/USB event, a "system-ready" event). If so, deliver it.
- **Instrument the pad-open→first-read path**: MonoGame's PS4 input backend gates GamePad polling on a "controller connected for the active user" check; since 0 GetControllerInformation AND 0 ReadState occur, that subsystem is never enabled. Trace what its init waits on (likely a User Service / pad-connection event) and whether feeding a "controller connected" event unblocks it.
- **Secondary:** confirm tid 2's cond@0x45b9c68 idle-park is benign (empty queue), not a load-completion the main thread awaits — log who is expected to signal cond@0x45b9c68 and whether main ever reads a flag that worker sets.
- **Limitation:** view-(d) main-thread rbp-chain backtrace came back EMPTY — Mono AOT/JIT frames don't keep a walkable rbp chain at the syscall boundary; naming the 0x3333xxx loop's callers needs a stack-scan return-address sampler instead.

### === KEYSTONE RE-ROOT-CAUSED: the game's bundled native interop prx (scePlayStation4.prx) is loaded but never linked/started — a LOADER gap (task-29), NOT a pad/UserService bug === (2026-07-19, opus overnight; instrumentation UNCOMMITTED in worktrees agent-ae79d03697efda626 + agent-aa80d32e202cf6be4)
Three overnight investigation passes (per-thread exec trace, pad-handle resolution, managed-IL decompile) converged on this. Each earlier lever was FALSIFIED, then the true cause surfaced:
- **Managed side has NO input gate (IL-proven, monodis).** Celeste `MInput.Update()` calls XNA `GamePad.GetState(0..3)` every frame unconditionally; MonoGame `PlatformGetState`'s first IL instr calls `Sce.PlayStation4.Input.GamePad::GetState`. `Sce.PlayStation4.dll` is a MANAGED CppSharp thin binding (confirmed cor20) with ZERO connection logic — every method is a bare P/Invoke into the NATIVE `scePlayStation4.prx`.
- **`scePlayStation4.prx` is a REAL native module bundled in the game** (`/home/mikolaj/PS4/CUSA11302/scePlayStation4.prx` exists; so do `libfmod.prx`, `libfmodstudio.prx`). It provides the 252 native exports the managed bindings call — GamePad::GetState, ALL Graphics::* (GNM), UserService::Update, Audio, etc. **The entire native interop surface (input + graphics + audio + user-service) lives in this prx.**
- **Our loader loads it as a 0-export STUB:** `sceKernelLoadStartModule('/app0/scePlayStation4.prx') → handle 19, 0 module_start(s)`; all 252 CppSharp P/Invokes → `sceKernelDlsym → not found`. So the prx's own native GamePad::GetState code — which is what would call the system `scePadReadState` we HLE — is never linked or run.
- **Decisive negative that redirects from HLE-stubbing to the loader:** the task-137 no-op-trap for a failed-dlsym call NEVER fires (0× across input/graphics/FMOD runs), and an env-gated experiment that DID resolve GamePad::GetState to a real Rust HLE via a dlsym hook (`UNEMUPS4_EXP_PADHLE`) was **never called** — 0 invocations, reads stayed 0. So Mono full-AOT does NOT dispatch these P/Invokes through the guest `sceKernelDlsym` path at all; stubbing individual exports can't fix it. The game never reaches `GamePad.GetState` through any path we can currently service **because the module that implements it was never properly loaded/linked/started.**
- **All three HLE levers refuted headlessly (reads 0→0):** UserService re-login (`EXP_RELOGIN`), proactive-connected GetControllerInformation, system focus/resume event (`EXP_FOCUS`). scePadGetHandle called 0×; userId consistently 1 — pad-handle-consistency lead moot.

**UNRESOLVED CONTRADICTION (the first thing next session must settle):** draws DO advance (~220/s, sceGnmDrawIndexOffset increments) and audio runs — so SOME native interop works — yet the input P/Invoke path appears never to execute. Either (a) graphics/audio reach our HLE via *separate* system libs (sceGnmDriver/sceAudioOut/sceNgs2 we HLE directly) while input is only in the un-started prx, and Celeste's Game.Update input path genuinely never runs (a LOADING/scene-init wall upstream of the interactive tick loop), or (b) the tick loop runs but the input P/Invoke is dispatched by Mono-AOT to the un-linked prx export and silently no-ops. Resolve with a **scePadOpen/GetState caller-RIP probe** (managed-Mono JIT/AOT region vs native-prx region) + confirm whether Celeste's `Game.Draw` (tick loop) is actually executing vs a pre-Game bootstrap loop rendering the attract.

**NEW KEYSTONE / NEXT DIRECTION (needs maintainer's architectural call):** finish loading + dynamic-linking + starting **`scePlayStation4.prx`** (and likely `libfmod*.prx`) so its exports resolve and its `module_start` runs — this is **task-29** (loader: .prx multi-module load + dynamic link + sceKernelLoadStartModule, retail FASE 1). If the prx genuinely can't be run (needs system prx deps we lack), the fallback is to HLE the specific `scePlayStation4.prx` exports the game imports — but that's a large surface (252) and the dlsym-bypass finding says naive export-stubbing won't be reached anyway; the loader path is the coherent fix. **task-170's input symptom is downstream of task-29.** Cross-links: task-29, task-113.2 (why-did-it-stop), task-137 (dlsym trap).

**Overnight instrumentation built (all env-gated, UNCOMMITTED, in worktrees; landed only `UNEMUPS4_EXECTRACE` on main @<prior-history>):** `UNEMUPS4_INPUT_TRACE` (pad/userservice/systemservice call trace), `UNEMUPS4_EXP_{RELOGIN,FOCUS,PADHLE}` (falsified experiments), new scePad Ext/GetControllerInformation/IsDS4Connected handlers (worktree agent-aa80d32e — genuine gap, worth landing regardless). Snow: see task-178 (axis-bug DISPROVEN — snow data-path faithful; separate real finding = `UNEMUPS4_DUMP_PNG` per-present device_wait_idle freezes the guest, so the PNG oracle is untrustworthy for late frames; added `UNEMUPS4_DUMP_PNG_EVERY`).

### === CORRECTION (pass-4 module-attributed probe, 2026-07-19): the "prx loaded-but-never-started / task-29" reframe above is REFUTED === (worktree agent-a835bc2dddeaa9a3d, instrumentation UNCOMMITTED)
Extended exectrace with a per-module RIP histogram + **syscall caller-module attribution** (reads `[rsp]` = the return address into whoever CALLed the `MOV EAX,id;SYSCALL;RET` stub, buckets by owning VMA). 155s attract run, 30 dumps. Decisive:
- **scePlayStation4.prx EXECUTES and is the live caller of everything that works.** Caller-module counts (main thread): `sceGnmDrawIndexOffset` ←prx ×17918, `sceGnmDrawIndexAuto` ←prx ×15386, `sceGnmSubmit*`/`SubmitDone` ←prx, **`scePadOpen` ←prx ×1**, `sceUserServiceGetEvent` ←prx ×2229 (all NO_EVENT); only `sceKernelLoadStartModule` ←eboot.bin. (The 0 RIP-samples inside the prx range is a sampling blind spot — RIP is sampled only at the 200k-block budget window and the prx is so syscall-dense it always hits a SYSCALL first; the per-syscall caller attribution is the authoritative signal and puts the prx on the live stack.)
- **Mono-AOT tick loop IS running** (main 54k syscalls/s, hot RIPs in the 0x3333xxx managed loop, ~220 draws/s) — NOT parked upstream on a loading wall.
- The `sceKernelDlsym handle-19 not-found` warnings that drove the pass-3 reframe are the game's OWN C++ symbols (`Audio::SoundSystem`, `System::UserService`, `SaveData`) on a separate path — a **red herring for input**. The managed→prx P/Invoke binding WORKS (scePadOpen reached HLE from inside the prx).
- **So it is NOT task-29 (prx loader) and NOT a Mono-AOT binding bug.** The gate is INSIDE `scePlayStation4.prx`'s own `Input::GamePad::GetState`: it runs every frame but returns a DISCONNECTED state WITHOUT ever issuing `scePadReadState`/`GetControllerInformation` (both 0×) — i.e. the prx decides "no controller for this player" BEFORE reading. Sole evidence-pinned correlate remains the perpetual `sceUserServiceGetEvent→NO_EVENT` (2229×) issued from inside the prx. This sharpens pass-2's H4 (game-state gate) and returns the keystone to: **why does the prx's GetState short-circuit to disconnected — what post-`scePadOpen` signal (a UserService event? a cached connection flag set by a call we mishandle?) does it await that our HLE never satisfies.** Note the public OpenOrbis scePad API (data/oo_sdk/include/orbis/Pad.h) is per-call syscalls (no shared mapped state buffer), so the disconnect decision is internal prx state, not a buffer we fail to populate via the documented API. DECISIVE next step needs the prx's GetState entry address (resolve the managed CppSharp P/Invoke target / prx export for GamePad::GetState at its known load base) → then either an in-memory RIP/branch trace of GetState to read the exact disconnected-condition, or a causality hack (force GetState connected+button in-memory → does the menu advance?).
<!-- SECTION:NOTES:END -->
