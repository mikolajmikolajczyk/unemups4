---
id: TASK-106
title: >-
  cpu/syscall-abi: SYSCALL clobbers RCX — move 4th arg via R10 (x86jit became
  hardware-correct)
status: Done
assignee: []
created_date: '2026-07-13 12:08'
updated_date: '2026-07-13 12:30'
labels:
  - real-software
  - doom
  - syscall-abi
  - x86jit
  - bug
dependencies: []
ordinal: 105000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Bisect of the x86jit freeze (unemups4 pinned bf630673, playable; bumping to f32bb87 froze Doom to ~1 flip/20s) pinpointed x86jit commit 35abc06 'fix(core): SYSCALL RCX/R11 (amd64-only)' (task-222). That is a CORRECTNESS fix in x86jit: the real SYSCALL instruction clobbers RCX (<-RIP) and R11 (<-RFLAGS). unemups4's syscall ABI relied on the OLD (incorrect) behavior where the x86jit lift preserved RCX: NativeContext::arg3() reads self.rcx (crates/cpu/src/context.rs:75-76), and exec.rs:376 comments 'RCX is preserved by the x86jit syscall lift, so arg3() (RCX) is valid'. With 35abc06, RCX is garbage after SYSCALL, so every syscall's 4th argument (arg3) is corrupt. Concretely this froze Doom: sceVideoOutSubmitFlip(handle,index,flip_mode,arg) reads arg (the flip arg) via arg3()=RCX=garbage, so the flip loop's 'st.flipArg == s_flip_arg' never matches and the shim spins its full 1,000,000-iter timeout (with usleep) every frame. FIX (unemups4 side; x86jit is correct now): preserve RCX into R10 (the standard syscall-ABI 4th-arg register, which SYSCALL does NOT clobber) inside the stub, and read arg3 from R10. The args are the function-call ABI (rdi,rsi,rdx,rcx,r8,r9); only arg3(RCX) is affected. R10 is free (not an arg register, caller-saved; exec.rs already captures Reg::R10 into the context). This makes the ABI robust and unblocks bumping x86jit forward (movmskpd lift etc.).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 Both syscall-stub emitters (crates/kernel/src/hle.rs:~79 and crates/loader/src/linker.rs:~197) prepend 'MOV R10, RCX' (bytes 49 89 CA) before the MOV EAX,id / SYSCALL / RET, still NOP-padded to 32 bytes; the linker stub-shape unit test (linker.rs:~683) updated for the new prefix
- [x] #2 NativeContext::arg3() reads R10 instead of RCX (context.rs); the exec.rs:376 + decision-2 comments corrected to reflect that SYSCALL clobbers RCX/R11 and the 4th arg travels via R10
- [x] #3 With the fix AND the x86jit pin bumped to f32bb87 (which carries 35abc06 + the MOVMSKPS/MOVMSKPD lift): the 6 example ELFs match baselines, AND Doom runs at full speed (hundreds of flips/20s, NOT ~1) — the freeze is gone
- [x] #4 build 0, clippy -D warnings 0, fmt clean
<!-- AC:END -->

## Implementation Notes
Landed merge (main). Stubs prepend MOV R10,RCX (49 89 CA); arg3()->R10; contract test syscall_args_survive_through_real_stub. Bumped x86jit pin to f32bb87. Verified: examples 6/6 at bf630673 AND f32bb87; Doom flips ~392-1425/20s (was 1=frozen); gate green incl. contract test. Root cause x86jit 35abc06 (SYSCALL RCX/R11 amd64-correct); x86jit TASK-241 closed (not their bug).
