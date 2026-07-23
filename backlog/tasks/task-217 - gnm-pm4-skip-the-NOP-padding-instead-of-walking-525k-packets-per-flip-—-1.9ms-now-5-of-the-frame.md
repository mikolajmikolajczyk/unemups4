---
id: TASK-217
title: >-
  gnm/pm4: skip the NOP padding instead of walking 525k packets per flip —
  1.9ms, now 5% of the frame
status: To Do
assignee: []
created_date: '2026-07-22 05:51'
labels:
  - gnm
  - pm4
  - perf
dependencies: []
priority: medium
ordinal: 222000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
task-208 removed the 4 MB copy and the ~525k per-packet heap allocations, taking decode+free from 19.1 ms to 0.000. What it did NOT remove is the walk itself: the executor still visits every one of the ~525189 packets in Celeste's dcb per flip, and that costs 1.9 ms measured in gameplay.

    pm4 exec: 2.0 runs/flip, 525189 packets/flip — walk=1.945 ms
    flip budget: sceGnmSubmitAndFlip avg=11.783 ms = walk 1.945 + submit_wait 5.907 + flip_wait 3.625 + apply_dirty 0.278 + rest 0.028

task-208 explicitly deferred this, and was right to at the time: 1.8 ms sat behind a 5.4 ms submit_wait and a frame that was still dominated by other things. The proportions have changed. A gameplay frame is now about 40 ms, so 1.9 ms is roughly 5% of it, and it buys nothing — the padding decodes to packets the executor then ignores.

Work:
- get the packet-kind histogram first. If the 4 MB is dominated by one kind (a long run of NOPs, or type-2 filler), the walk can skip it in bulk rather than per packet; if it is heterogeneous, this is not worth doing and that is a legitimate finding
- whatever the skip mechanism is, it must not change which packets the executor acts on — the decode semantics and the non-multiple-of-4 tail behaviour stay identical
- keep the borrowed in-place decoding from task-208; this is about not visiting, not about how a visited packet is represented

Measure the walk row and frames-per-window before and after. Note the pattern this investigation has hit repeatedly: removing cost from one phase has more than once MOVED wall time elsewhere instead of shortening the frame. A shrinking walk row is not success on its own; report the frame rate too, and say so plainly if the row shrinks and the frame does not.

Context on priority: the dominant cost is now guest code, not the GPU path — guest_exec is about 25 ms of a 40 ms gameplay frame at 99% on-core, and x86jit TASK-276 (Cranelift opt_level was never set, so codegen runs with the optimizer off) is the bigger lever. This task is worth doing but should not jump ahead of that.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 the packet-kind histogram for Celeste's dcb is recorded, and the decision to skip in bulk (or not to) follows from it
- [ ] #2 padding is skipped without visiting each packet, with decode semantics and tail behaviour unchanged
- [ ] #3 measured before/after for the walk row AND frames-per-window, both reported even if the frame rate does not move
- [ ] #4 existing pm4 tests green plus a test pinning the skip; build + clippy clean, cargo test --workspace green
<!-- AC:END -->

## Notes

### 2026-07-22 — host profile confirms this is the largest native cost in the emulator

`perf record` on the guest thread, 10 s of Celeste gameplay (x86jit 8a67575, `--proc-map-timeout`
raised so the native mappings actually resolve):

```
[JIT] generated code   79.80%
unemups4 (our Rust)    17.30%
libc                    2.10%
[vdso]                  0.71%
```

Inside that 17.3%:

```
6.57%  ps4_gnm::exec::Executor::run     <- this task
4.73%  x86jit_core::vm::Vcpu::run       (the block dispatcher)
0.96%  ps4_cpu::exec::drive
0.45%  rust_syscall_handler
0.34%  x86jit_core::memory::Memory::take_dirty_ranges
```

The PM4 walk is 6.57% of guest-thread cycles — **more than x86jit's entire block dispatcher**, and
the single largest native symbol by a wide margin. It is 38% of all the time we spend in our own
code. Nothing else on our side is close.

Worth noting what that buys: the walk is over ~525,000 packets per flip, of which the overwhelming
majority is NOP padding, and task-208 already removed the copy — this is the walk itself, not the
decode. So the cost is almost entirely spent stepping over padding.

Context for prioritisation: the CPU-side ceiling is the ~30x instruction expansion in generated
code (see task-227 notes and x86jit task-282), which is not ours to fix. This one IS ours, it is
measured, and it is the biggest thing we own.
