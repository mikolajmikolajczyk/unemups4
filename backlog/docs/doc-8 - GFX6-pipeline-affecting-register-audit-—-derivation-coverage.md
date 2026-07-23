---
id: doc-8
title: GFX6 pipeline-affecting register audit — derivation coverage
type: other
created_date: '2026-07-23 18:54'
---

# GFX6 pipeline-affecting register audit — derivation coverage (task-183)

## Why this exists

Three of the four register bugs the retail bring-up hit recently were of one shape: a
register the guest **wrote** and nothing on our side **read**. No probe was watching it,
so the wall took hours to localize (task-179). This audit closes that blind spot for the
GNM→pipeline derivation by (1) writing down every register `crates/gnm/src/pm4/opcodes.rs`
DEFINES, (2) recording, for each, whether the derivation actually consumes it or a cited
reason it is safe to ignore, and (3) backing it with an **enforcing test** so a
newly-added-but-unread register trips CI instead of silently hiding.

The enforcing artifact is `pipeline_register_coverage_is_audited` in
`crates/gnm/src/pm4/opcodes.rs` (test module). It partitions the hand-maintained catalog
`reg::ALL_PIPELINE_REGS` into `READ_BY_DERIVATION` and `IGNORED_WITH_REASON` and fails if
any register is in neither (or both), naming the offender. `ALL_PIPELINE_REGS` is also the
source of truth `reg_name` resolves scalar names from, so a register cannot be added,
named, or dumped without appearing in the catalog — which forces it through the audit test.

## What "read by the derivation" means

A register counts as **READ** only if a value fetched from the shadow register file
(`crates/gnm/src/state.rs` `RegFile`) is consumed to build pipeline/draw state:
`derive.rs` (`TargetDesc`/`PipelineKey`/viewport/scissor), `state.rs`
(`derive_bound_shaders`/`gcn_ref_from_regs` — shader identity, resources, PS routing),
`vbuf.rs`/`exec.rs` (vertex/descriptor pull from the `SPI_SHADER_USER_DATA_*` blocks).

A read that only feeds a **diagnostic probe** (e.g. the `UNEMUPS4_DRAWTEX_TRACE` block in
`exec.rs`) or that only **emits** the register into a command buffer (the HLE shader-setup
builder in `pm4/emit.rs` writes many registers the derivation never reads back) does **not**
count as consumed — those registers are effectively ignored for pipeline purposes and are
classified accordingly.

Every hardware fact below (which registers are pipeline-affecting, their field semantics) is
the AMD GFX6 (Liverpool / GCN2) definition machine-listed in Mesa
`src/amd/registers/gfx6.json` and named in Mesa `src/amd/common/sid.h`; occupancy/scheduling
semantics are the AMD GCN ISA `SPI_SHADER_PGM_RSRC*` sections. Citations are by name (clean
primary source), forward-only.

## Coverage table

Legend — **READ**: consumed by pipeline/draw derivation. **SAFE-TO-IGNORE**: defined but not
consumed, with a cited reason it does not change rendered output on the current path.
**GAP**: genuinely pipeline-affecting, currently ignored, tracked by a follow-up task.
**read-nowhere**: defined in `opcodes.rs` but referenced by no code except the `reg_name`
reverse index — the exact class that hid the recent bugs; each is flagged here and in the
test's `IGNORED_WITH_REASON` allow-list.

### SH bank — shader program setup (`sh_reg`)

| Register | Defined | Read | Status |
|---|---|---|---|
| `SPI_SHADER_PGM_LO/HI_{PS,VS}` | ✓ | ✓ | READ — `.sb` code address `(hi:lo)<<8` (state.rs) |
| `SPI_SHADER_PGM_RSRC1/2_{PS,VS}` | ✓ | ✓ | READ — VGPR/SGPR/user-SGPR footprint (state.rs) |
| `SPI_SHADER_PGM_RSRC3_PS` | ✓ | ✗ | SAFE-TO-IGNORE (read-nowhere) — CU-enable/wave-limit occupancy mask; scheduling hint, no effect on rendered output (GCN ISA `SPI_SHADER_PGM_RSRC3_*`). |
| `SPI_SHADER_PGM_RSRC3_VS` | ✓ | ✗ | SAFE-TO-IGNORE (read-nowhere) — as above. |
| `SPI_SHADER_USER_DATA_{PS,VS}_0..15` | ✓ | ✓ | READ — user-SGPR block: fetch-shader ptr (VS s[0:1]) + descriptor pointers (vbuf.rs/exec.rs). |

### CONTEXT bank — VS/PS pipeline state (`context_reg`)

| Register | Defined | Read | Status |
|---|---|---|---|
| `SPI_PS_INPUT_CNTL_0..31` | ✓ | ✓ | READ — PS input routing → shader identity (state.rs). |
| `CB_COLOR0_BASE/PITCH/SLICE/INFO/ATTRIB` | ✓ | ✓ | READ — color target derivation (derive.rs). |
| `CB_COLOR0_VIEW` | ✓ | ✗ | SAFE-TO-IGNORE (read-nowhere) — MRT0 array-slice range; current targets are single-slice 2D (SLICE_START=0), no array selection applies (`CB_COLOR0_VIEW`). |
| `PA_CL_VPORT_{X,Y}{SCALE,OFFSET}` | ✓ | ✓ | READ — viewport (derive.rs). |
| `PA_SC_SCREEN_SCISSOR_TL/BR` | ✓ | ✓ | READ — scissor (derive.rs). |
| `CB_BLEND0_CONTROL` | ✓ | ✓ | READ — blend key (derive.rs). |
| `CB_TARGET_MASK` | ✓ | ✓ | READ — per-channel write mask (derive.rs). |
| `DB_DEPTH_CONTROL` / `DB_Z_INFO` | ✓ | ✓ | READ — depth key (derive.rs). |
| `CB_COLOR_CONTROL` | ✓ | ✗ (diagnostic only) | SAFE-TO-IGNORE — global CB mode/ROP; current titles program a normal blend mode + ROP=copy, diagnostic-only read suffices (`CB_COLOR_CONTROL`). |
| `CB_SHADER_MASK` | ✓ | ✗ (diagnostic only) | SAFE-TO-IGNORE — PS output-component mask; redundant with `CB_TARGET_MASK` (read) on the single-MRT RGBA8 path (`CB_SHADER_MASK`). |
| `SPI_VS_OUT_CONFIG` | ✓ | ✗ (emit-only) | SAFE-TO-IGNORE — VS export-param count; the recompiler derives the export layout from the shader binary (`SPI_VS_OUT_CONFIG`). |
| `SPI_SHADER_POS_FORMAT` | ✓ | ✗ (emit-only) | SAFE-TO-IGNORE — VS position-export format; recompiler emits `gl_Position` directly (`SPI_SHADER_POS_FORMAT`). |
| `PA_CL_VS_OUT_CNTL` | ✓ | ✗ (emit-only) | SAFE-TO-IGNORE — clip/cull-distance output enables; the software raster path applies no user clip/cull planes (`PA_CL_VS_OUT_CNTL`). |
| `SPI_PS_INPUT_ENA` / `SPI_PS_INPUT_ADDR` | ✓ | ✗ (emit-only) | **GAP → task-234** — PS interpolant enable/address masks; derivation routes PS inputs via `SPI_PS_INPUT_CNTL` only. |
| `SPI_PS_IN_CONTROL` | ✓ | ✗ (emit-only) | **GAP → task-234** — PS input count (`NUM_INTERP`). |
| `SPI_BARYC_CNTL` | ✓ | ✗ (emit-only) | **GAP → task-234** — barycentric/interpolation mode. |
| `SPI_SHADER_COL_FORMAT` | ✓ | ✗ (emit-only) | **GAP → task-235** — PS colour-export numeric format per MRT; colour format taken from `CB_COLOR0_INFO` (RGBA8 path). |
| `SPI_SHADER_Z_FORMAT` | ✓ | ✗ (emit-only) | **GAP → task-235** — PS depth-export format; PS depth export not yet modelled. |
| `DB_SHADER_CONTROL` | ✓ | ✗ (emit-only) | **GAP → task-235** — Z-export/kill/mask control; PS discard + depth export not yet modelled. |

### UCONFIG bank (`uconfig`)

| Register | Defined | Read | Status |
|---|---|---|---|
| `VGT_PRIMITIVE_TYPE` | ✓ | ✓ | READ — input-assembly topology (derive.rs, task-184). |

## Totals

- **42** pipeline registers catalogued in `reg::ALL_PIPELINE_REGS` (the two `SPI_SHADER_USER_DATA_*` blocks and `SPI_PS_INPUT_CNTL_*` are counted once, by slot-0 base).
- **27** READ by the derivation.
- **15** IGNORED-with-reason, of which **3** are *read-nowhere* (`SPI_SHADER_PGM_RSRC3_PS`, `SPI_SHADER_PGM_RSRC3_VS`, `CB_COLOR0_VIEW`) — all three benign (occupancy hint / single-slice targets).
- **6** of the ignored registers are genuine forward-looking **GAPs**, grouped into two follow-up tasks:
  - **task-234** — PS-interpolation registers (`SPI_PS_INPUT_ENA/ADDR`, `SPI_PS_IN_CONTROL`, `SPI_BARYC_CNTL`).
  - **task-235** — PS export-format/kill registers (`SPI_SHADER_COL_FORMAT`, `SPI_SHADER_Z_FORMAT`, `DB_SHADER_CONTROL`).

No GAP is implemented here: this task is the audit plus the anti-drift test (CLAUDE.md scope
discipline — audit = audit). Each GAP is benign for the embedded corpus and current titles
(Celeste, ps4doom) and becomes correctness-relevant only for real GCN shaders / non-RGBA8
targets in retail bring-up; that is what the follow-up tasks track.

## The enforcing test

`pipeline_register_coverage_is_audited` (`crates/gnm/src/pm4/opcodes.rs`, test module):

1. asserts `READ_BY_DERIVATION` and `IGNORED_WITH_REASON` are disjoint;
2. asserts every entry of `reg::ALL_PIPELINE_REGS` is classified by exactly one list —
   failing with a message that names the offending register and tells the maintainer to
   model it or add a reasoned `IGNORED_WITH_REASON` entry;
3. asserts every `IGNORED_WITH_REASON` reason is non-empty and cites its register name;
4. cross-checks that each catalogued index resolves through `reg_name` to its listed name,
   tying the audit list to the live register constants.

Because `reg_name` resolves scalar names *from* `ALL_PIPELINE_REGS`, adding a new register
constant without cataloguing it leaves it unnamed/undumped — and once catalogued it must be
classified, or the test fails. That is the anti-drift guard AC#3 requires.
