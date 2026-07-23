---
id: TASK-21
title: 'gnm: PM4 Type-3 packet trace decoder'
status: Done
assignee: []
created_date: '2026-07-10 18:24'
updated_date: '2026-07-10 21:19'
labels:
  - gnm
  - gpu
dependencies:
  - TASK-20
priority: high
ordinal: 21000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Phase 2 / doc-2 D1 (see decision-3). With the libSceGnmDriver submit stubs (task-20) surfacing guest command-buffer pointers, add a HEADLESS-testable PM4 Type-3 packet trace decoder. Given a command buffer, walk it and decode Type-3 packet headers (opcode + count) into a structured trace log — decode only, NO execution and NO Vulkan work, so it runs in the headless devShell (no Vulkan driver), same as task-18's constraint. Gate the trace behind an env var (e.g. UNEMUPS4_PM4_TRACE=1) so normal runs are silent. Unknown/unhandled opcodes are LOGGED, not fatal — the guest keeps running. Model the packet formats from the AMD PM4 docs + Mesa src/amd (doc-2 §3, §5-D1). This is the correctness-oracle-style artifact doc-2 D1 describes: it makes the guest's GPU intent visible in logs, analogous to the syscall table naming NIDs from ps4_names.txt. Precedes any present/sync execution (a later phase-3 task) and the shader work.\n\nIdentity mapping asset (doc-2 §1): guest ptr == host ptr, so the decoder reads command buffers straight out of guest memory with no translation layer.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 PM4 Type-3 decoder walks a command buffer and decodes packet headers (opcode name, count) into a structured trace log; decode-only, no execution, no Vulkan — runs headless
- [x] #2 Trace output is gated behind an env var (e.g. UNEMUPS4_PM4_TRACE=1); default runs emit nothing
- [x] #3 Unknown opcodes are logged and skipped, never fatal; the guest continues
- [x] #4 Unit tests over hand-crafted command buffers round-trip / decode as expected; trace output for the hand-written PM4 test ELF matches expectations
<!-- AC:END -->

## Implementation Notes
<!-- SECTION:NOTES:BEGIN -->
Impl 2026-07-10. PM4 Type-3 trace decoder in `crates/gnm/src/pm4/` (Vulkan-free, headless).

- **opcodes.rs**: `op::IT_*` u8 constants (NOP, CLEAR_STATE, INDEX_BUFFER_SIZE, DISPATCH_DIRECT/INDIRECT, INDEX_BASE, DRAW_INDEX_2, CONTEXT_CONTROL, INDEX_TYPE, DRAW_INDEX_AUTO, NUM_INSTANCES, DRAW_INDEX_OFFSET_2, WRITE_DATA, WAIT_REG_MEM, INDIRECT_BUFFER, PFP_SYNC_ME, EVENT_WRITE/_EOP/_EOS, DMA_DATA, ACQUIRE_MEM, SET_CONFIG/CONTEXT/SH/UCONFIG_REG). Values match the AMD PM4 IT_* opcode definitions (Mesa src/amd / Linux radeon headers). `reg_base` window consts (CONFIG 0x2000, CONTEXT 0xA000, SH 0x2C00, UCONFIG 0xC000). `name()` / `set_reg_base()` lookups.
- **decode.rs**: `Pm4Packet<'a>` enum (Type3{opcode,count,body}, Type0{base_index,count,body}, Type2, Truncated{header}) + `OwnedPacket`. `Decoder` iterator. Header: type=[31:30], Type-3 opcode=[15:8] count=[29:16] body=count+1; bounds-checked, truncation yields `Truncated` and stops (never panics on untrusted guest data). Entry points: `decode(&[u32])` (tests), `decode_bytes(&[u8])`, `unsafe decode_guest(ptr,size)`, `unsafe decode_submit_range(&SubmitRange)` (DCB then CCB, identity-mapped guest ptr==host ptr).
- **trace.rs**: `enabled()` reads `UNEMUPS4_PM4_TRACE` (unset/empty/"0" => off). `trace()`/`trace_owned()` render one line (opcode name or `UNKNOWN(0xNN)`, count, and for SET_*_REG the absolute reg = base+offset). `unsafe trace_submit_range(&SubmitRange)` = env-gated + non-fatal emit via `tracing::info!`.
- Wired into `crates/libs/src/libscegnmdriver/mod.rs::record_submit`: after recording, calls `trace_submit_range` (OFF by default, non-fatal).
- Env-gate: default runs emit nothing; unknown opcodes render raw hex and are skipped by count (guest continues).

Verification (worktree, rebased onto main w/ task-20): `cargo build --release` green; `cargo test` 50 passed / 1 ignored; `cargo clippy --all-targets --all-features -D warnings` clean (only pre-existing ps4-syscalls OpenOrbis-SDK-missing warnings); `cargo fmt` no drift. Oracle `scripts/run_examples.sh check`: only the two known pre-existing baseline deltas appear (`HLE: Loaded libSceGnmDriver.so` from task-20 ×6, headless `Failed to initialize Vulkan` ×4) — zero new diff from this change (trace env-gated OFF). ps4-gnm dep graph has no ash/winit. ACs #1-#4 checked. Status left In Progress for merge review.
<!-- SECTION:NOTES:END -->
