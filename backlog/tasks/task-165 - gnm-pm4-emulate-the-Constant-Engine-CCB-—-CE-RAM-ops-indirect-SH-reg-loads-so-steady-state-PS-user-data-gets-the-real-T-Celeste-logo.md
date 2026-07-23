---
id: TASK-165
title: >-
  gnm/exec: execute mem→register DMA_DATA so steady-state PS user-data gets the
  real atlas T# (Celeste logo white bar)
status: Done
assignee: []
created_date: '2026-07-17 18:15'
updated_date: '2026-07-17 20:50'
labels:
  - gnm
  - pm4
  - gcn
  - celeste
  - retail
  - constant-engine
dependencies: []
priority: high
ordinal: 171000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
REFRAMED 2026-07-17 (CE premise DISPROVEN by live trace + fable review). Celeste's studio-splash LOGO (PS 0x98166f700, inline InlineVSharp{s0..7}, zero SMRD — disasm CONFIRMED correct) renders as an opaque WHITE BAR every steady-state frame; first-use frames of each of the 2 flip arenas render it correctly (the '6 atlas-binds' = 2 arenas × 3 first-use frames). ROOT MECHANISM (fable review a7206052, code-verified): the guest delivers the real atlas T# (base 0x9afc28000) into SPI_SHADER_USER_DATA_PS_* via IT_DMA_DATA with DAS=1 (memory→REGISTER), NOT via SET_SH_REG. On first-use frames the guest emits a plain 8-dword SET_SH_REG 0x2c0c atlas bind; after arena REUSE (frame 3+) it switches to a dirty-cache path and delivers the descriptor via two back-to-back IT_DMA_DATA packets in the exact stream position the SET_SH_REG used to occupy — and NEVER emits the SET_SH_REG atlas bind again. Our dispatch_dma_data (crates/gnm/src/exec.rs:1710-1739) DEFERS (no-ops) every non-mem→mem DMA — its own doc-comment (exec.rs:1696) already admits 'Celeste's DMA_DATA stream is uniformly memory→register (DAS=1), so it takes the defer path.' So we drop the descriptor upload; s[0:7] retains the fade AUTO-draw's 4-dword ring V# (word0 ping-pongs 0x01846d24<->0x02446d2c) mixed with a stale atlas upper-half (s4=0x00f3e000) → decode_t_sharp reads a degenerate 2×1 → white-dummy → white bar. NOT Constant Engine (ccb_ptr=0 every submit, zero CE opcodes in ~1M packets — the CE hypothesis is dead). FIX: decode the DMA_DATA word0 (DST_SEL[21:20], SRC_SEL[30:29] per Mesa sid_full.h:249-275 — dispatch_dma_data currently DISCARDS word0/_engine) + the command word's DAS/SAS, and EXECUTE the memory→register variant: copy BYTE_COUNT bytes from src guest memory into the destination SH register bank (SPI_SHADER_USER_DATA_*), routing through the existing SH-reg shadow that UserData::from_regs (crates/gnm/src/vbuf.rs:444) reads, so the real atlas T# lands in s[0:7] each steady frame. GDS/other register-dest variants can stay deferred if Celeste doesn't use them (confirm from trace). RE from OUR guest packets + Mesa PM4 (NOT other emulators). Confirmed prerequisite experiment: task step 1 = log DMA_DATA word0+src+dst+first 64B of src on live Celeste; if src holds 0x09afc280 0x00a00000 0x703185db 0x94800fac (atlas T#) the mechanism is proven and dst names the register base to emulate. Relates task-157 (provenance, resolved here), task-163 (present), the padding defect (separate task), doc-6 Entry 20/21.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 The Constant Engine CE-RAM ops (WRITE/DUMP_CONST_RAM) + the indirect SH-reg load Celeste uses are emulated, populating SPI_SHADER_USER_DATA_PS_* from the CE ring
- [ ] #2 Steady-state PS user-data s[0:7] holds the real atlas T# every frame (not the 2x1 ring placeholder)
- [ ] #3 Celeste logo + textured sprites render every frame, not just warm-up (PNG oracle)
- [ ] #4 The DMA_DATA mem→register (DAS=1) variant is decoded (word0 DST_SEL + command DAS) and executed, copying src guest memory into the destination SH user-data register bank; The confirming experiment shows Celeste's steady-state DMA_DATA src holds the real atlas T# (base 0x9afc28000) and it lands in SPI_SHADER_USER_DATA_PS_* each frame; Steady-state PS user-data s[0:7] holds the real atlas T# every frame, not the 2×1 ring placeholder; Celeste logo + textured sprites render every steady-state frame, not just each arena's first-use frame (PNG oracle + maintainer live-test)
<!-- AC:END -->



## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Opus/worktree, 2 fazy w jednym agencie (kontekst trace determinuje impl):
FAZA 1 RECON — dodać CCB/DCB trace (env-gated), uruchomić Celeste steady-state, zrzucić strumień pakietów wokół count=6 logo draw. Ustalić: (a) DOKŁADNE opcody CE które ładują SPI_SHADER_USER_DATA_PS_* (WRITE/DUMP_CONST_RAM + SET_SH_REG_OFFSET czy LOAD_SH_REG_INDEX), (b) KRYTYCZNE: ORDERING — decode_submit_range merguje DCB-potem-CCB, ale CE biegnie PRZED DE (INCREMENT/WAIT_ON_CE_COUNTER sync). Trace musi pokazać czy CCB trzeba przetworzyć PRZED draw (prawdopodobnie tak: CE-ahead → CCB first).
FAZA 2 IMPL — CE-RAM byte buffer w state; WRITE_CONST_RAM→CE RAM; DUMP_CONST_RAM→guest ring; SET_SH_REG_OFFSET/indirect load→sh_regs shadow (UserData::from_regs vbuf.rs:444). CE↔DE counters = no-op JEŚLI ordering rozwiązany przez CCB-first replay. Reference: /home/mikolaj/src/mesa-ref/sid_full.h (opcody), ac_descriptors.c, radeonsi CUE path (gh api stary fork).
ORACLE: agent self-check = trace pokazuje real T# 0x9afc28000 trafia do s[0:7] każdą klatkę; PNG dump logo; finalna weryfikacja = live-test Mikołaja (motion/steady-state).
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
REFERENCE SOURCES (use these — do NOT guess PM4/CE semantics; see memory [[mesa-hw-format-reference]] for the gh-api method since freedesktop gitlab is Anubis-blocked):

1. CE PACKET OPCODES — already in the local Mesa ref /home/mikolaj/src/mesa-ref/sid_full.h (grep PKT3_): WRITE_CONST_RAM=0x81, DUMP_CONST_RAM=0x83, LOAD_CONST_RAM=0x80, SET_SH_REG_OFFSET=0x77 (the indirect SH-reg load we DON'T handle), SET_SH_REG=0x76 (direct, we DO handle), INDIRECT_BUFFER_CONST=0x33, INCREMENT_CE_COUNTER=0x84, WAIT_ON_CE_COUNTER=0x86. Also our own crates/gnm/src/pm4/opcodes.rs (IT_* consts) — add the missing ones.

2. CE DATA-FLOW (how the driver uses the CE for user-data/descriptors) — Mesa radeonsi, GFX6-8 era ONLY (GFX9 removed the CE; PS4=GFX7 has it). The Constant Update Engine: WRITE_CONST_RAM builds the descriptor/user-data table in on-chip CE RAM, DUMP_CONST_RAM copies CE RAM -> a per-frame ring in memory, SET_SH_REG_OFFSET / an indirect load copies from that ring into SPI_SHADER_USER_DATA_PS_*. Fetch the emission code from a GitHub Mesa mirror via gh api (e.g. repos/ValveSoftware/steamos_mesa or anholt/mesa, src/gallium/drivers/radeonsi/si_descriptors.c — the si_ce_* / si_emit_shader_pointers CUE path). NOTE: modern Mesa (25.x) dropped CE entirely, so use an OLD fork (mesa-17-era sid_full.h from harrisonlab/popgen already has the opcodes).

3. EXACT PACKET FIELD SEMANTICS — AMD public PM4 packet spec (Sea Islands / GCN2). Mesa's emission usually makes the fields clear enough; consult the spec for edge cases (CE RAM offsets, byte counts, the SET_SH_REG_OFFSET addressing mode).

4. GROUND TRUTH — our OWN guest's CCB packet trace (task-165 step 1). Mesa/AMD tell us what the packets MEAN; only Celeste's actual CCB stream tells us WHICH it uses and with what operands. RE from our guest (NOT other emulators — [[no-copying-other-emulators]]).

EMULATION SIMPLIFICATION: we process DCB+CCB packets SERIALLY (already merged in pm4/decode.rs:248), so the CE<->DE sync counters (INCREMENT_CE_COUNTER 0x84 / WAIT_ON_CE_COUNTER 0x86) can be NO-OPS — the data is already available in order. Model: CE RAM = a byte buffer (few KB); WRITE_CONST_RAM writes into it; DUMP_CONST_RAM copies CE-RAM -> guest memory (the ring); SET_SH_REG_OFFSET / the indirect load reads guest memory -> our state.sh_regs shadow (UserData::from_regs, vbuf.rs:444). Watch CCB vs DCB packet ordering — the CE runs AHEAD of the DE on real HW, but serial replay in submit order should be equivalent for this use.

2026-07-17: filed after task-157 root-cause. Not started.
<!-- SECTION:NOTES:END -->
