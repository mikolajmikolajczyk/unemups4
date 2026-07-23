---
id: TASK-228
title: >-
  savedata: Celeste reports a save FAILURE for a save that actually succeeded —
  the error is invented above the file layer
status: To Do
assignee: []
created_date: '2026-07-22 14:33'
labels:
  - hle
  - savedata
  - correctness
dependencies: []
priority: medium
ordinal: 233000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Observed by the maintainer during the task-216 smoke run: Celeste displayed a save-error message in game, yet the save had in fact been written correctly — a later session resumed from exactly the point the error appeared.

The log confirms the write side is clean. Two in-game save cycles, both complete, both fast:

    14:21:20.930  pthread_create(entry=0x1ae6190) -> worker thread 24
    14:21:20.932  sceSaveDataMount2 'SAVEDATA00' -> '/savedata0'  status=0 blocks=300
    14:21:20.934  sceKernelOpen('/savedata0/0.celeste', flags=0x601)        <- O_WRONLY|O_CREAT|O_TRUNC
    14:21:20.936  sceKernelOpen('/savedata0/settings.celeste', flags=0x601)
    14:21:20.937  sceSaveDataUmount '/savedata0'   then thread exit + pthread_join

4.5 ms end to end. No error, no fatal, no missing symbol anywhere in the run. On disk the file matches the log's timestamp to the millisecond and is a complete, well-formed document — BOM, `<SaveData>` root, closing `</SaveData>` — carrying real progress (`Time` 743 s, `TotalDeaths` 28, `TotalStrawberries` 5, `LastArea ID=2`). Loading works too: `0.celeste` is opened for reading when the slot is entered.

So the failure is REPORTED, not real, and it is invented somewhere above the byte path. The game believes something went wrong while every file operation succeeded.

WHERE TO LOOK, cheapest first:

1. Return values of the savedata entry points themselves. `sceSaveDataMount2` reports `status=0`, but the whole result struct is written by us from a layout reverse-engineered at runtime (`crates/libs/src/libscesavedata/mod.rs` header comment): `mountPoint` char[16] at +0x00, `requiredBlocks` u64 at +0x10, `mountStatus` u32 at +0x1c. A field the game reads that we never write — or write at the wrong offset — reads as whatever was on the guest stack. `requiredBlocks` is the obvious suspect: a game comparing it against free space would conclude the card is full.

2. What `sceSaveDataUmount` returns, and whether Celeste checks it. That is the call that commits a save on real hardware, so its return is the natural thing for a title to test.

3. The stubbed-missing neighbours. `sceSaveDataSetParam`, `sceSaveDataGetParam`, `sceSaveDataUmountWithBackup`, `sceSaveDataDialog*` and `sceSaveDataDirNameSearch` are all stubbed missing (linker output at boot). None was called in this run — a call would have been a hard stop — but the error path the game took may be a branch that expects one of them to have been callable earlier.

Not urgent: the data is safe and the game continues. It is a user-visible lie though, and the same missing/incorrect result field would matter more for a title that refuses to continue after it.

Evidence: session log kept at the time under scratchpad `t216.log`.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 the error the game reports is traced to a specific value we return or fail to write, named with its offset or call
- [ ] #2 no save-error message for a successful save, confirmed by the maintainer in game
- [ ] #3 build + clippy clean, cargo test --workspace green
<!-- AC:END -->
