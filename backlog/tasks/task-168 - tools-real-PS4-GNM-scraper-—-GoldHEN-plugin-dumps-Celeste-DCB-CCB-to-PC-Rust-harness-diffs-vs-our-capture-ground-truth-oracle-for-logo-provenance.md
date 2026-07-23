---
id: TASK-168
title: >-
  tools: real-PS4 GNM scraper — GoldHEN plugin dumps Celeste DCB/CCB to PC, Rust
  harness diffs vs our capture (ground-truth oracle for logo provenance)
status: Done
assignee: []
created_date: '2026-07-17 22:12'
updated_date: '2026-07-17 22:47'
labels:
  - tools
  - retail
  - celeste
  - gnm
  - reverse-engineering
dependencies: []
priority: high
ordinal: 172000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
GROUND-TRUTH ORACLE for task-157 (Celeste steady-state logo white bar). Six emulator-side hypotheses eliminated (CE/DMA/arena-artifact/nested-IB/persistence/HLE-gap); the atlas 8-dword SET_SH_REG 0x2c0c (base 0x9afc28000) is genuinely absent from OUR captured steady-state DCB, but Mesa proves real HW needs it re-emitted per frame — a model contradiction only real-hardware data can settle. Plan: a GoldHEN plugin (.prx) runs inside real Celeste on the user's jailbroken PS4 (FW 11.00, GoldHEN installed), hooks sceGnmSubmitAndFlipCommandBuffers (+ plain SubmitCommandBuffers + the *ForWorkload variants), memcpy's each DCB[i]/CCB[i] (sizes from dcb_sizes[]/ccb_sizes[]) and streams them over TCP to the PC, tagged with a frame/flip counter. PC receiver saves per-frame dumps; a Rust harness feeds the REAL DCB bytes into ps4_gnm::pm4 decode + the 0x2c0c/atlas-scan analysis we already ran on our own capture. DECISIVE: does the real steady-state DCB contain the 8-dword atlas bind every frame (=> our emulation makes the guest build a shorter list = OUR bug, candidate GPU-readback/fence timing; and we get a reference stream) OR not (=> GPU keeps the atlas resident differently than we model / visual expectation wrong). Environment (confirmed): OpenOrbis at data/oo_sdk (OO_PS4_TOOLCHAIN=data/oo_sdk, system clang target x86_64-pc-freebsd12-elf, ld.lld link.x); PC=192.168.100.1, PS4=192.168.100.2 direct cable; GoldHEN Plugins SDK (github.com/GoldHEN/GoldHEN_Plugins_SDK) hook API HOOK_INIT/HOOK/UNHOOK/HOOK_CONTINUE/final_printf, plugin lifecycle plugin_load/plugin_unload + module_start/module_stop (mirror GoldHEN_Plugins_Repository plugin_src/plugin_template + fliprate_remover). Celeste = CUSA11302. Deploy: .prx to /data/GoldHEN/plugins/ + register in /data/GoldHEN/plugins.ini under the Celeste title id. Method: real-HW PM4 capture -> feed our decoder -> diff.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 GoldHEN plugin (.prx) builds against OpenOrbis (data/oo_sdk) + GoldHEN SDK, hooks the sceGnmSubmit* family, and streams tagged DCB/CCB bytes over TCP to 192.168.100.1
- [x] #2 PC-side receiver saves per-frame/flip DCB+CCB dumps with frame-index metadata
- [x] #3 Rust harness feeds a captured real DCB into ps4_gnm::pm4 decode and reports the SET_SH_REG 0x2c0c writes + atlas 0x9afc28 presence per flip (same analysis as our own-capture recon)
- [x] #4 Captured a real steady-state Celeste flip DCB and determined whether it contains the 8-dword atlas bind every frame — settling the task-157 contradiction
- [ ] #5 SETUP.md documents the full build+deploy+capture loop (OO_PS4_TOOLCHAIN, GoldHEN SDK clone, plugins.ini registration, PC listener, harness run)
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Built + deployed + captured 2026-07-18. Plugin loaded on real PS4 (FW11, GoldHEN), 4 hooks installed, 600 flip DCBs captured to data/celeste-real-dcb/ (gitignored). decode confirms real HW re-emits 8-dword T# bind every steady frame => Celeste logo white bar is OUR bug (see task-157 GROUND TRUTH). Merged to main. AC#5 (SETUP.md) done. Permanent ground-truth oracle for retail bring-up.
<!-- SECTION:NOTES:END -->
