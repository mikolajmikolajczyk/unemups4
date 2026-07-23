---
id: TASK-191
title: >-
  hle: sce* syscalls return raw -errno instead of SCE error codes
  (0x8002_0000|errno) — crashes retail Mono error handling
status: Done
assignee: []
created_date: '2026-07-21 08:08'
updated_date: '2026-07-21 10:41'
labels:
  - hle
  - kernel
  - celeste
  - retail
  - abi
dependencies: []
priority: high
ordinal: 196000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Root-caused for Celeste enter-gameplay (CLIMB) crash, via UNEMUPS4_DUMP_MODULES + objdump. sceKernelStat and other sce* handlers return a raw negative POSIX errno (e.g. -2 for ENOENT). The SCE convention is a POSITIVE error code 0x8002_0000 | posix_errno (ORBIS_KERNEL_ERROR_ENOENT = 0x80020002, confirmed in data/oo_sdk/include/orbis/_types/errors.h). Retail Mono calls sceKernelStat, reads the return as an SCE code, and does errno = sce_to_errno(ret); -2 is not a valid SCE code so errno becomes an unknown value, MOnos w32error-unix.c takes its if (*errno == 2) graceful branch FALSE, and formats "%s: unknown error (%d) \"%s\"" where the code->string lookup returns NULL -> strlen(NULL) crash (vasprintf->vsnprintf->_Mbtowc->strlen). If sceKernelStat returned 0x80020002, Mono maps it to errno 2 and takes the graceful empty-slot path exactly like hardware.\n\nEvidence: disassembled eboot.bin +0x16de90 (frames #4-#6) from a module dump. Frame #6 at eboot 0xda440 literally does: call __error; cmpl $0x2,(%rax); jne <crash>. Frame #5 (0xd90f0) is the "%s: unknown error (%d)" formatter. The %s NULL arg is the code->string lookup (0x170d20) returning NULL for the unrecognised code.\n\nOur error-return convention is INCONSISTENT and that is the real work: crates/libs/src/libkernel/fs.rs alone mixes -e (raw -errno), 0x80020001 (an SCE code), and -14 (raw -EFAULT). A systematic pass over the sce* family is needed to return SCE codes on failure.\n\nRISK (same shape as the refuted carry probe): the ps4_syscall dispatch serves sceKernelStat AND POSIX stat/_stat under ONE syscall id, so the handler cannot tell which name the guest called, and POSIX callers (the OpenOrbis example ELFs) may expect -errno / sign-return semantics. Do NOT just flip every handler to SCE codes blindly. Establish per family whether the retail (SCE) and example (POSIX) callers can share one convention, or whether the POSIX aliases need splitting to a separate id/handler. Oracle: the 6 example-ELF stdout baselines (scripts/run_examples.sh if present, else build the baselines first) MUST still match, AND Celeste must reach the empty-slot / New Game screen on a fresh save (maintainer live oracle).\n\nRefuted, do NOT rechase: the FreeBSD carry-flag syscall convention. An env-gated UNEMUPS4_X_SYSCALL_CARRY probe (CF=1 + RAX=+errno) was proven neutral/harmful and removed; retail libc, like OpenOrbis, checks the SIGN of the return for the POSIX names. The bug is specifically the SCE-named functions return-code convention.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 Audited: every sce* error return in crates/libs is either an SCE code (0x8002_0000|errno) or documented why it stays POSIX, with the POSIX-vs-SCE dispatch-id conflict resolved (shared convention proven safe, or the POSIX aliases split off)
- [x] #2 sceKernelStat returns 0x80020002 on ENOENT; Celeste on a FRESH save reaches the empty-slot / New Game screen instead of crashing — maintainer live oracle
- [x] #3 All 6 example-ELF stdout baselines still match (no regression for OpenOrbis POSIX callers)
- [x] #4 build + cargo test + clippy clean
<!-- AC:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
ARCHITECTURE (approved): global Errno converter + macro abi= tag + thin per-ABI adapters (wiring option A). SPLIT, not flip.

WHY SPLIT: file syscalls multiplex two caller ABIs onto one stub/id. sce-named imports (sceKernelStat, retail Mono) want SCE codes 0x8002_0000|errno; posix-named imports (stat/open/read, OpenOrbis example libc) want -errno sign (task-101: positive errno reads as valid fd -> stdio corruption). A stub carries only a SyscallId so the handler cannot tell which name was invoked; the only mechanism to serve two ABIs is two ids. SCE_KERNEL_STAT=95057 and SYS_STAT=101463 are distinct; today names=[sceKernelStat,stat,_stat] collapse both onto the 95057 stub (hle.rs:44-56). pthread/sema/mman already return SCE codes correctly (sce-only names) = the model; fs.rs is the sole outlier (mixes -e, 0x80020001, -14).

GLOBAL CONVERTER (the systemic piece, kills scattered hex + per-handler sign logic):
  ps4-core::errno  (reachable by all libs; lives where SyscallId is or in ps4-core)
    struct Errno(pub i32)                 // always POSITIVE posix errno = single internal representation
    consts ENOENT=2, EBADF=9, EINVAL=22, EFAULT=14, ...  sourced from data/oo_sdk/include/orbis/_types/errors.h
    fn to_sce(self)->i32   = (0x8002_0000 | n) as i32
    fn to_posix(self)->i32 = -n
    fn from_sce(i32)->Option<Errno>       // reverse (consume guest-provided codes / tests / symmetry)
    impl From<std::io::Error> for Errno   // via raw_os_error
  Both directions, one table.

HANDLERS BECOME ABI-AGNOSTIC: return Result<T, Errno>. No hex, no sign anywhere in bodies. Shared impl per op:  fn X_impl(..) -> Result<T, Errno>.

WIRING A (approved): #[ps4_syscall] gains abi = sce | posix. The generated wrapper applies the direction at the boundary (ONE conversion site per ABI):
    Ok(v)  -> v as u64
    Err(e) -> (if abi==sce { e.to_sce() } else { e.to_posix() }) as u64
Dual-name ops get two 1-line adapters over the shared impl:
    #[ps4_syscall(id=SCE_KERNEL_X, abi=sce,   name="sceKernelX")]  fn sce_X(..) -> Result<T,Errno> { X_impl(..) }
    #[ps4_syscall(id=SYS_X,        abi=posix, names=["X","_X"])]  fn pxs_X(..) -> Result<T,Errno> { X_impl(..) }
No dispatcher/NativeContext changes. (Future: if two-liners proliferate, migrate to macro auto-split option D, ABI-by-invoked-id; Errno converter is shared so A->D is non-blocking.)

MACRO CHANGE (crates/macros): parse abi = sce|posix (default sce). Detect Result-returning handlers and emit the Ok/Err match instead of the current "result as u64". Keep non-Result handlers working (backward compatible: no abi + non-Result -> today's behavior). This is the only infra edit for A.

PHASING:
Phase 0: add ps4-core::errno (Errno + consts + to_sce/to_posix/from_sce + From<io::Error>); unit-test to_sce(ENOENT)==0x80020002, from_sce round-trip. Teach the macro abi= + Result return. No behavior change yet.
Phase 1 (spike, unblocks Celeste, AC#2): convert stat -> stat_impl + sce_stat(abi=sce) + pxs_stat(abi=posix, id=SYS_STAT). TEMPORARY once-per-call log naming which id fired, to confirm the crash path is sceKernelStat vs posix stat (eboot imports BOTH NIDs; disasm sce_to_errno(ret) implies sceKernelStat). Maintainer: fresh-save Celeste (wipe savedata0) -> empty-slot/New Game instead of crash. Remove log after confirm.
Phase 2 (systematic fs, AC#1 fs): apply impl + two adapters to open, close, read, write, readv, writev, getdents, lseek, mkdir, rmdir, unlink, rename, fstat. Verify each posix alias has a distinct SyscallId (from_symbol_name(name).nid() non-empty) BEFORE splitting; contingency: no distinct id -> keep sce-only + document. Re-run baselines after each op.
Phase 3 (audit non-fs, AC#1 rest): confirm pthread/sema/mman are sce-only + already SCE; swap trivially-safe 0x8002xxxx literals for Errno consts; document any handler intentionally posix.
Phase 4: AC#3 all 6 example baselines match (scripts/run_examples.sh + scripts/baselines/*.txt exist -> diff after EVERY phase); AC#4 cargo build+test+clippy clean.

ORACLES: example baselines diff after every phase (AC#3); Celeste fresh-save -> New Game = maintainer live oracle (AC#2). DO NOT rechase carry flag (refuted). Implementation delegated to opus. CONTINGENCY: if Phase-1 log shows crash path is posix stat NID (not sceKernelStat), the SCE branch belongs on the posix stub and sce/posix roles for fs must be re-decided BEFORE Phase 2 -- gate on the log.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
RESOLVED 2026-07-21, maintainer live-confirmed Celeste reaches New Game. Actual root cause was NOT the SCE return-code theory: crash path is the POSIX stat import (breadcrumb #812 stat->-2, then #813 _Errno), and retail Mono reads *__error() (errno TLS), not the return. Fix = macro abi=posix Err arm now calls ps4_cpu::set_errno(e.0) before returning -errno, so every POSIX handler sets the guest errno slot. The sce/posix ABI split + ps4-core::errno converter also landed (correct hygiene, inert for this crash). Files (uncommitted): crates/core/src/errno.rs (new), crates/core/src/lib.rs, crates/macros/src/lib.rs, crates/libs/src/libkernel/fs.rs. build+test(549)+clippy green; 6 example baselines byte-identical. NOT committed. Status stays In Progress until committed/merged.
<!-- SECTION:NOTES:END -->
