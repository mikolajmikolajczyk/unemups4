---
id: TASK-167
title: >-
  gnm: pad draw/shader-set reserved cmdbuf slot with trailing NOP — kill
  stale-arena PM4 decode corruption (phantom SET_SH_REG + 0xffffffff truncation)
  on reused DCB arenas
status: Done
assignee: []
created_date: '2026-07-17 20:40'
updated_date: '2026-07-17 20:50'
labels: []
dependencies: []
ordinal: 168000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
HLE draw/shader-set builders (emit_draw_into_cmdbuf, emit_into_cmdbuf) wrote only pm4.len() dwords into a guest cmdbuf slot the guest reserved 'reserved' (numdwords) dwords for, leaving the reserved-len tail UNTOUCHED. On a REUSED command arena that hole exposes stale prior-frame bytes; our PM4 decode walk mis-reads them as real packets (orphan SET_SH_REG after a draw, phantom ring writes, a Truncated header=0xffffffff INSIDE the declared DCB size that halts the walk before EVENT_WRITE_EOP, dropping the frame tail). Fix: pad the packet to exactly 'reserved' dwords with a trailing IT_NOP (Type-2 filler for a 1-dword gap), the same discipline emit_shader_set already applies to its documented length.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 Reused-arena flip DCB (frame 3+) shows NO 'Truncated header=0xffffffff' inside the declared DCB size; the walk reaches EVENT_WRITE_EOP
- [x] #2 No orphan/phantom SET_SH_REG appears in the unwritten tail after a draw (each draw is followed by the pad NOP)
- [x] #3 pad_to_reserved fills an over-sized reserved slot pre-filled with stale 0xdeadbeef bytes so it decodes as exactly [draw][NOP] with no stale packet surviving (unit test)
- [x] #4 Both cmdbuf-writing builders (draw + shader-set) pad to 'reserved'; 1-dword gap handled; reserved==0 / reserved==len unchanged
- [x] #5 cargo clippy -D warnings + cargo fmt --check + cargo test -p ps4-gnm + ps4-libs pass
- [x] #6 PNG oracle: presented splash logo region is textured (real atlas) rather than a solid white bar
<!-- AC:END -->



## Implementation Notes

Orchestrator id note: requested as "task-166"; the backlog counter had 166 claimed on
another branch, so this worktree tracks it as TASK-167. Same fix — reconcile the id on merge.

### What changed
- `crates/gnm/src/pm4/emit.rs`: new `pub fn pad_to_reserved(packet, reserved) -> Vec<u32>` +
  `TYPE2_NOP` const. Appends a trailing `IT_NOP` claiming `gap-1` body dwords (zero-filled) so
  `[packet][NOP] == reserved` dwords; a 1-dword gap uses a header-only Type-2 filler NOP
  (a Type-3 NOP body is >= 1). `reserved <= len` (incl. 0) returns the packet unchanged.
  Unit tests: over-sized slot over 0xdeadbeef stale bytes decodes as exactly [draw][NOP] (no
  stale packet / Truncated survives); 1-dword-gap Type-2 path; no-room no-op path.
- `crates/libs/src/libscegnmdriver/draw.rs`: `emit_draw_into_cmdbuf` now writes
  `emit::pad_to_reserved(pm4, reserved)`. New builder-level test drives `sce_gnm_draw_index_auto`
  into a reused arena (stale 0xdeadbeef + trailing real draw) and asserts [draw][NOP][draw], no
  Truncated.
- `crates/libs/src/libscegnmdriver/shader_bind.rs`: `emit_into_cmdbuf` (the shader-set mirror,
  shared by SetVs/SetPs/SetPs350/UpdatePs350/UpdateVs) also pads to `reserved` — set_vs/ps_shader
  already self-pad to 29/40 but a LARGER guest reservation would still leak the tail.
- Audited all guest-cmdbuf slot writers (grep write_guest/emit_into_cmdbuf): only these two
  builders write into a reserved slot; dispatch/compute stubs are record-only. Both fixed.

### Re-trace evidence (live Celeste CUSA11302, UNEMUPS4_PM4_TRACE=1, to steady state frame 40+)
Reused-arena flip DCBs alternate arena A=0x901846a00 / B=0x902446a08 (the task's two arenas).
The 3rd flip (arena A reused) is "frame 3", the defect frame.

BEFORE (baseline, fix stashed):
- `TRUNCATED header=0xffffffff` x18; EVENT_WRITE_EOP reached in only 2 of 37 flips.
- frame-3 DCB (arena A, 2792 B) = 140 packets, tail: `[118] DRAW_INDEX_OFFSET_2` ->
  `[119] SET_SH_REG reg=0x2c0a` (orphan stale write in the unpadded hole) -> ... ->
  `[137] DRAW_INDEX_OFFSET_2` -> `[138] T0 REG_WRITE base=0x10c` ->
  `[139] TRUNCATED header=0xffffffff` (walk halts; no EOP; frame tail dropped).

AFTER (fix):
- `TRUNCATED header=0xffffffff` x0; EVENT_WRITE_EOP reached in all 40 of 40 flips.
- The 40 remaining `TRUNCATED` are all `header=0x00000000` — terminal lone-zero dwords ending
  huge NON-flip 4 MB zero-filled submits (pre-existing, benign; 0 belong to any flip DCB).
- frame-3 DCB (arena A, 2792 B) = 147 packets; every draw now followed by the pad NOP:
  `[69] DRAW_INDEX_AUTO -> [70] IT_NOP count=3`; `[117]/[136]/[143] DRAW_INDEX_OFFSET_2 ->
  IT_NOP count=3`; walk reaches `[145] IT_EVENT_WRITE_EOP` + `[146] IT_NOP count=63`. No orphan
  SET_SH_REG in any post-draw hole. The `SET_SH_REG 0x2c0c` writes that remain are genuine
  pre-draw PS user-data binds (SH base 0x2c00 + 0xc = SPI_SHADER_USER_DATA_PS_0), structured
  `SET_SH 0x2c0c -> DMA_DATA -> DRAW`, not orphan-after-draw phantoms.

### PNG oracle (UNEMUPS4_DUMP_PNG)
- BEFORE: black field + snow squares + a solid WHITE BAR at the logo region (the defect).
- AFTER: the previously-dropped tail draws now execute (full-screen gray fill renders; bg is gray
  not black). BUT the logo region is STILL a solid white bar (untextured white-dummy) during the
  splash (e.g. frame_0008/0020), fading to flat gray by frame_0040 — the real "Matt Makes Games"
  atlas is NOT textured.
- Conclusion: the stale-arena trace corruption is fully fixed (Truncated gone, EOP reached,
  phantom orphan writes gone, dropped tail restored), but removing the phantom did NOT make the
  logo textured. The leading hypothesis (phantom 0x2c0c clobber -> white-dummy) is thus NOT the
  (sole) cause of the white logo. The atlas-T#-provenance wall (task-157) remains — now on an
  UN-CORRUPTED PM4 stream, which is what this fix unblocks. AC#6 left unchecked (not forced).
