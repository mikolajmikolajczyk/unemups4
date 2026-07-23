---
id: TASK-208
title: >-
  gnm/pm4: stop copying and heap-allocating the 4MB command buffer every flip —
  decode+free is 19ms of a 24ms flip
status: Done
assignee: []
created_date: '2026-07-21 19:19'
updated_date: '2026-07-21 22:01'
labels:
  - gnm
  - pm4
  - perf
dependencies: []
priority: high
ordinal: 213000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Measured in Celeste gameplay, with the task-203 instrumentation closing the flip budget to 0.0% unaccounted:

    flip budget: sceGnmSubmitAndFlip avg=23.909 ms = decode+free 19.057 + walk 1.080 + submit_wait 3.564 + flip_wait 0.067 + apply_dirty 0.111 + handler rest 0.028
    pm4 exec: 2.0 runs/flip, 525250 packets/flip — decode=13.834 packet_free=5.223

Celeste submits a dcb of 4194300 bytes per flip (confirmed in a real gameplay log: dcb=0x902046a0c (4194300 B)), overwhelmingly padding. crates/gnm/src/pm4/decode.rs:163 decode_bytes then does the two most expensive possible things with it:

1. collects the ENTIRE 4 MB into a fresh Vec<u32> via chunks_exact(4).map(...).collect() — a full copy of the buffer, per submit
2. collects the decoded stream into a Vec<OwnedPacket>, where each Type0/Type3 carries its own heap-allocated body: Vec<u32> (decode.rs:192-214, body.to_vec()) — roughly 525k individual allocations, then 525k frees

The frees alone are 5.2 ms/flip and were invisible until task-203 added an explicitly timed drop, because the Vec dropped at scope end after the old timer had stopped.

None of this is necessary. The same module already exposes decode() returning a borrowing Decoder<'_> that allocates nothing (decode.rs:156), and the guest command buffer is identity-mapped — guest addr == host addr — so it can be read in place. The OwnedPacket path exists only because of the doc comment's claim that the transient dword buffer cannot be borrowed out, which is true of the COPY the function itself makes, not of the guest buffer.

Work:
- feed the executor from the borrowing decoder over the guest buffer in place, with no Vec<u32> copy and no per-packet body allocation
- the dword reinterpretation must not assume alignment or endianness beyond what the existing code guarantees; keep the non-multiple-of-4 tail behaviour identical
- keep OwnedPacket for the callers that genuinely need ownership (snapshot/diagnostic paths), rather than deleting it
- the padding itself is worth a look: if the 4 MB is dominated by a single packet kind that decodes to nothing useful, skipping it cheaply may matter more than the allocation fix — but decide that from the packet-kind histogram, not from assumption

Measure with UNEMUPS4_PROFILE and report the decode / packet_free / flip-budget rows before and after. Beware the trap this investigation already hit twice: removing cost from one phase has so far MOVED wall time into another rather than shortening the frame. A win here means frames-per-10s-window rises, not merely that the decode row shrinks. Report both, and if the row shrinks while the frame rate does not, say so plainly — that is a finding, not a failure to be papered over.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 the per-flip 4 MB Vec<u32> copy and the ~525k per-packet body allocations are gone from the submit path; the executor decodes the guest buffer in place
- [x] #2 measured before/after for the decode, packet_free and flip-budget rows, AND for frames-per-10s-window — both reported even if the frame rate does not improve
- [x] #3 the non-multiple-of-4 tail behaviour and packet decoding are unchanged; existing pm4 tests green, plus a test pinning the no-allocation path
- [x] #4 build + clippy clean, cargo test --workspace green; maintainer confirms the scene still renders correctly
<!-- AC:END -->



## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. crates/gnm/src/pm4/decode.rs: add a borrowing view over an identity-mapped guest command buffer (guest_words) that reinterprets the bytes as dwords in place when the pointer is 4-byte aligned and the host is little-endian, falling back to the existing copy otherwise; keep the non-multiple-of-4 tail behaviour identical. Add a SubmitDecoder that chains the DCB then the CCB as borrowed Pm4Packet streams. Keep OwnedPacket/decode_bytes/decode_guest for the snapshot, trace and tools callers.
2. crates/gnm/src/exec.rs Executor::run: consume the borrowing stream instead of collecting Vec<OwnedPacket>; drop the packet_free timing (nothing to free) but keep the counter reported as 0 so the dump row stays comparable.
3. Tests: pin the in-place path (borrowed body points into the source buffer), pin the unaligned fallback, pin the tail behaviour.
4. Measure: UNEMUPS4_PROFILE=10 run before/after, report decode / packet_free / flip budget AND frames-per-10s-window. If the row shrinks and the frame rate does not move, report that plainly.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Done 2026-07-21 except AC #4's visual confirmation. Measured on the ATTRACT/menu scene (gameplay needs a pad the maintainer holds) — do NOT read these as gameplay numbers. Like-for-like steady-state windows (~210-240 s of wall, 19-20 draws/flip both sides).

BEFORE (task-209 instrumentation, task-208 not applied):
  pm4 exec: 2.0 runs/flip, 525026 packets/flip — run=23.718 = decode=13.097 + packet_free=5.038 + walk=0.805 + sink waits=4.778
  flip budget: avg=23.842 ms = decode+free 18.135 + walk 0.805 + submit_wait 4.658 + flip_wait 0.121 + apply_dirty 0.097 + rest 0.025 (0.0% unaccounted)
  guest frame [tid 1]: 252 frames/10 s window, 25.26 fps, 39.589 ms = guest_exec 14.286 + flip 24.069 + other_syscalls 0.794 + run_loop 0.440

AFTER:
  pm4 exec: 2.0 runs/flip, 525104 packets/flip — run=8.249 = decode=0.000 + packet_free=0.000 + walk=1.769 + sink waits=6.480
  flip budget: avg=8.361 ms = decode+free 0.000 + walk 1.769 + submit_wait 5.408 + flip_wait 1.072 + apply_dirty 0.086 + rest 0.024 (0.0% unaccounted)
  guest frame [tid 1]: 429 frames/10 s window, 42.94 fps, 23.288 ms = guest_exec 13.038 + flip 8.422 + other_syscalls 1.153 + run_loop 0.675

THE FRAME RATE MOVED. Unlike task-204, the time did not relocate: 253 -> 429 frames per 10 s window (+70%), frame 39.6 -> 23.3 ms. Four consecutive after-windows: 429/432/430/421. The decode row went to zero and the walk absorbed the decoding (0.805 -> 1.769 ms), so the true PM4 cost fell 18.9 -> 1.8 ms/flip.

Implementation: decode.rs gained GuestWords (InPlace | Copied) + guest_words(ptr, size). A 4-byte-aligned guest buffer is reinterpreted where it lies (identity-mapped, doc-2 §1); an unaligned one falls back to the same unaligned dword copy read_array always made, so behaviour is identical. Executor::run walks decode(dcb).chain(decode(ccb)) as borrowed Pm4Packets — no 4 MB Vec<u32>, no ~525k per-packet body allocations. OwnedPacket / decode_bytes / decode_guest / decode_submit_range are untouched for the snapshot, trace and tools callers.

decode_ns now times only obtaining the two views; packet_free_ns times dropping them. Both stay in the dump (at 0.000) so the rows remain comparable with the pre-task-208 profile.

On the padding question: the histogram bullet is now moot as a priority. 525k packets/flip cost 1.77 ms to walk in place (3.4 ns/packet) and the flip is dominated by submit_wait (5.4 ms, GPU-side). Skipping the padding is a <=1.8 ms opportunity, no longer the biggest one — not implemented (out of scope).

AC #4 left UNCHECKED: build/clippy/cargo test --workspace are all green, but the visual confirmation is the maintainer's to give. Proxy evidence that rendering is unchanged: passes and draws per flip are 19.1 before / 19.8-20.1 after (same attract scene, count drifts with scene animation), and the walk still resolves the same draw stream.

Files: crates/gnm/src/pm4/decode.rs:219 (GuestWords + guest_words + 4 tests), crates/gnm/src/exec.rs:173 (in-place walk).
<!-- SECTION:NOTES:END -->
