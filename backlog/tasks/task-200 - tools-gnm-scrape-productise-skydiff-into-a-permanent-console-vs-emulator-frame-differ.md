---
id: TASK-200
title: >-
  tools/gnm-scrape: productise skydiff into a permanent console-vs-emulator
  frame differ
status: Done
assignee: []
created_date: '2026-07-21 15:18'
updated_date: '2026-07-21 17:44'
labels:
  - tools
  - gnm-scrape
  - gpu
  - diag
  - dx
dependencies: []
priority: medium
ordinal: 205000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
The throwaway tools/ps4-gnm-scrape/host/src/bin/skydiff.rs written during the task-199 investigation is the single most useful GPU-debugging instrument we have built: it replays a real-PS4 DCB's register writes and prints, PER DRAW, the full colour/blend state (CB_COLOR0_{BASE,PITCH,SLICE,INFO,ATTRIB}, CB_BLEND0_CONTROL, CB_COLOR_CONTROL, CB_TARGET_MASK, CB_SHADER_MASK, SPI_SHADER_COL_FORMAT/Z_FORMAT, DB_DEPTH/SHADER_CONTROL), the PS/VS program addresses, and the full 16-slot PS user-data block decoded as T#/S#/pointer — resolving memory-resident T#s through the plugin's probe dumps — plus a register census naming everything via our own reg_name. It answered in one run a question three sessions of emulator-side reasoning got wrong, and it is what proved the console and we agree on every colour register.\n\nProductise it so the next GPU wall costs minutes, not a session:\n- rename to something honest (e.g. "framediff"), drop the throwaway marker, document it in tools/ps4-gnm-scrape/SETUP.md and in backlog/docs (doc-6 discovery log references it)\n- take BOTH sides as inputs: a console capture dir + one of OUR gpu-snapshot frame dirs, and emit a real DIFF (matching draws by ordinal/kind/target-dimensions/blend, as the investigation did by hand) rather than making the human eyeball two dumps\n- report, per draw: registers that differ, descriptors that differ (including our snapshot's descriptor_honoured flag), and a census of registers the console writes that our register file never receives (the investigation found 33, all traceable to the sceGnmDrawInitDefaultHardwareState* stubs in crates/libs/src/libscegnmdriver/hwstate.rs)\n- keep the address-correspondence heuristic explicit and printable — console and our guest addresses differ, so the 1:1 ordinal/blend/dimension match IS the mapping, and it should be shown so a human can sanity-check it\n- a smoke test over a committed tiny fixture would be ideal, but do NOT commit any capture data (dumps/ and gpu-snapshots/ stay untracked)\n\nProvenance: register/field layouts from AMD GCN ISA / Mesa sid.h / llvm-mc only; never another PS4 emulator.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 a single command takes a console capture dir + one of our gpu-snapshot frame dirs and prints a per-draw DIFF (registers, descriptors, sampled bases), not two dumps to eyeball
- [x] #2 prints the console-writes-but-we-never-receive register census, and the draw-matching heuristic it used, so a human can verify the correspondence
- [x] #3 documented in tools/ps4-gnm-scrape/SETUP.md; no capture data committed; build + clippy clean
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Landed in the working tree (NOT committed) 2026-07-21.

`tools/ps4-gnm-scrape/host/src/bin/skydiff.rs` (throwaway) replaced by
`tools/ps4-gnm-scrape/host/src/bin/framediff.rs`; `[[bin]] framediff` in the host Cargo.toml.

    cargo run -p ps4-gnm-scrape-host --bin framediff -- dumps/scrape2 gpu-snapshots/frame-02143 --frame 4

Takes BOTH sides. Console side: replays the DCB's SET_*_REG writes and snapshots the register file at
each draw, decoding PS user-data as T#/S#/pointer and resolving memory-resident T#s through the
plugin's probe dumps. Our side: parses gpu-snapshots/<frame>/draws.json and RECONSTRUCTS our per-draw
register file by accumulating the per-draw `register_delta`s — which is what makes a register-level
comparison possible at all.

Prints: (1) the draw-matching heuristic — pairs BY ORDINAL, scores each pair on draw kind, target
extent and CB_BLEND0_CONTROL, and says N/N so a human can reject a bad correspondence; (2) the derived
console->ours address map (targets plus sampled textures whose extents agree); (3) the per-draw diff —
differing registers (address-bearing ones excluded, since they differ by construction) and per-slot
descriptors including our `descriptor_honoured` flag; (4) the census of registers the console writes
that our register file never receives, with the `sceGnmDrawInitDefaultHardwareState*` /
`sceGnmDrawInitToDefaultContextState*` stub explanation.

On the task-199 pair it reports 29/29 matched, `registers : identical on every register both sides
recorded` throughout, 146 console registers vs 113 ours with 33 never reaching us, and lands the
diagnosis directly:
    draw 14  tex0: console 0x2bcee8000 -> 0x9afb10000 | ours 0x9afc30000  <<< MISMATCH
             tex1: console 0x2bd008000 (memory-resident) | ours NOT BOUND
    draw 28  tex1: console 0x2a89e9100 256x16 (memory-resident) | ours NOT BOUND

Two honesty features worth keeping: console-side textures come from residual PS user-data registers,
so a draw whose PS samples nothing is labelled *stale* rather than reported as a missing bind; and the
console's padded extent (CB_COLOR0_PITCH/SLICE are TILE_MAX) is compared against our padded pitch and
size-derived height, not naively against our logical extent.

No new dependencies: a small read-only JSON reader lives at
tools/ps4-gnm-scrape/host/src/json.rs (lib module, with its own unit tests) rather than pulling serde
into the workspace.

Tests (no capture data committed — dumps/ and gpu-snapshots/ stay untracked, opened read-only):
6 unit tests in the bin + lib covering the JSON shapes draws.json uses, escapes/unicode, error paths,
padded-vs-logical extent matching, address-bearing register classification, and T# decode/rejection.
Documented in tools/ps4-gnm-scrape/SETUP.md §7, and doc-6 Entry 27 references it.
Build + clippy clean (`cargo clippy -p ps4-gnm-scrape-host --all-targets -- -D warnings`).
<!-- SECTION:NOTES:END -->
