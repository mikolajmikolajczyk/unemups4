---
id: TASK-144
title: >-
  diag/x86jit: characterize Celeste CPU-side wall — UnknownInstruction at guest
  0xd5bc00 (SIGILL), then file x86jit lift task
status: Done
assignee: []
created_date: '2026-07-16 13:15'
updated_date: '2026-07-16 13:35'
labels:
  - diag
  - x86jit
  - celeste
  - retail
dependencies: []
priority: high
ordinal: 150000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Exposed by task-141 (which cleared the VS spirv-val wall). Celeste (CUSA11302) now builds pipelines but the guest hits an x86jit UnknownInstruction at guest VA 0xd5bc00 (reported SIGILL 'Exception vector 68 at 0x982e26') BEFORE it submits its geometry draws, so the frame stays black (PNG oracle, 100% #000000). This is an x86jit lift gap, NOT a GPU/recompiler problem.

Per the x86jit-changes-via-backlog rule, x86jit fixes go through x86jit's OWN backlog (never edit ~/src/x86jit directly; user lands, then bump the rev pin). But FIRST identify the instruction: (1) map guest VA 0xd5bc00 -> file offset in /home/mikolaj/PS4/CUSA11302/eboot.bin (PIE ELF; account for the load base the loader uses) and disassemble the bytes there (objdump -D -b binary -m i386:x86-64 at the offset, or map through the loaded module base); (2) also decode 0x982e26 (the 'vector 68' site) — vector 68 = 0x44; confirm what x86jit's UnknownInstruction reporter means by that. Celeste is Mono full-AOT = pervasively AVX/SSE, so the missing op is likely a VEX/AVX or an SSE4/BMI shape not yet lifted (the session already lifted a big VEX-float surface in x86jit tasks 255-258 — this is a NEW gap past those). (3) Get the exact opcode + operands, then file a precise x86jit backlog task ('lift <insn>') with the bytes + a Unicorn-differential expectation, per the established x86jit workflow. Improve the emulator-side reporter too if it didn't name the bytes (relates to task-113.2 why-did-it-stop). Assets gitignored, never commit; keep RUST_LOG sane.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 The instruction at guest 0xd5bc00 is disassembled (exact bytes + mnemonic + operands), and the 0x982e26 / 'vector 68' site is explained
- [x] #2 A precise x86jit backlog task is filed to lift the missing instruction (bytes + differential expectation), per x86jit-changes-via-backlog — NOT edited directly here
- [x] #3 The emulator-side UnknownInstruction reporter names the faulting bytes + guest VA (so the next such wall is self-describing; coordinate with task-113.2)
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Done 2026-07-16 (merge 8c4510e). Premise was two SEPARATE faults on two threads. (1) x86jit gap = VMASKMOVPS ymm[rcx],ymm8,ymm2 (bytes c4 e2 3d 2e 11, VEX.256.66.0F38.W0 op 0x2E, AVX1 masked STORE, group 0x2C-0x2F) in libfmod Thread 12 — NEW family past x86jit 255-258 (distinct from existing EVEX k-mask VMaskMov). Filed x86jit backlog TASK-259 (bytes + 2C-2F map + sign-bit-per-lane semantics + mask-0-no-fault + Unicorn-diff AC). USER LANDS it + bumps rev pin per x86jit-changes-via-backlog. (2) 'vector 68 @0x982e26' = NOT a CPU gap — our own abort stub (int 0x44 -> Trap vector 68) trapping deliberately, triggered upstream by Mono mono_os_mutex_init: pthread_mutex_init failed 'Device busy' (16)=EBUSY -> HLE bug, filed task-145. Reporter improvement merged: crates/cpu/src/exec.rs guest_hexdump wired into report_unknown_instruction+report_exception -> 'guest bytes @ 0xADDR (16B): [..]' (bounded vm.read_bytes, feeds task-113.2). 12 cpu tests.
<!-- SECTION:NOTES:END -->
