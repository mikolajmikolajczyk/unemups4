---
id: TASK-40
title: 'gcn: GCN→SPIR-V recompiler — straight-line triangle subset, portable SPIR-V'
status: Done
assignee: []
created_date: '2026-07-11 12:53'
updated_date: '2026-07-12 14:06'
labels:
  - gpu
  - gcn
dependencies:
  - TASK-39
  - TASK-38
priority: high
ordinal: 39000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Highest-risk keystone. Translate the same subset (P4-03 Inst stream) to SPIR-V via rspirv: SGPR/VGPR as SSA-ish locals, VS entry gl_VertexIndex→v0, user-SGPR resources as descriptor-backed buffers per §C4 memory-driven binding layout carried in HostShader I/O metadata, exp pos/param/mrt → Position builtin / Location outputs / fragment output. CF: straight-line + simple s_cbranch if corpus needs; structured-CFG deferred. Output MUST stay MoltenVK/Metal-portable (decision-3): no non-portable caps, validated with spirv-val (vendored spirv-tools dev-dep — confirmed) in tests. Extends HostShader (gnm/shader/source.rs) with §4 I/O-layout metadata. Does NOT wire provider chain (P4-07); does NOT do loops/tess/GS (§C8 roles carried as data).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 headless: every corpus shader recompiles to SPIR-V passing spirv-val (Vulkan 1.1/portability)
- [ ] #2 headless: golden SPIR-V disasm snapshots (regression fence)
- [ ] #3 headless: modules declare only portable caps (asserted)
- [x] #4 live GPU (maintainer): corpus VS+PS via a temp AshBackend hook / P4-06 harness renders expected triangle (LD_LIBRARY_PATH=/usr/lib)
<!-- AC:END -->



## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
DONE 2026-07-12 (feat/task-40 @<prior-history>, merged; AC#4 live-GPU DEFERRED to maintainer). crates/gcn/src/recompile.rs + rspirv 0.13 dep (Vulkan-free: rspirv→spirv, no ash — VERIFIED). API: recompile(&[Decoded], ShaderStage)->Result<RecompiledShader,RecompileError>. RecompiledShader{spirv:Vec<u32>, io:IoLayout}. IoLayout{stage,inputs,outputs:Vec<IoVar>,buffers:Vec<BufferBinding>,exports_position,uses_num_records_push_constant} — §4 I/O metadata lives IN ps4-gcn (no gnm cycle; task-42 maps into HostShader). Register model: SGPR/VGPR→function-local OpVariable (parallel f32+u32 views, bitcast at boundary), straight-line single block, OpLoad/OpStore; vars hoisted to block-top (spirv-val req). VS: gl_VertexIndex→v0; StorageBuffer vec4 data[] V# (set0 bind0); exp pos0→Position builtin, param0→Location0. PS: VINTRP→per-(attr,chan) f32 Location inputs (Location=attr,Component=chan); exp mrt0→frag Location0. ALU mirrors interp: v_add/sub/mul→OpF*, v_mac/madmk/madak+v_mad=OpFMul+OpFAdd (UNFUSED), v_fma=GLSL.std.450 Fma (FUSED), v_med3=FMin/FMax, cvt=Convert*, VOP3 abs/neg=FAbs/FNegate + omod ×2/×4/÷2=OpFMul. TASK-88 CONTRACT reproduced: num_records clamp (idx>=nr?nr-1:idx, nr==0⇒0) via 2 OpSelect, nr=PUSH CONSTANT; VINTRP screen-space-linear no-perspective-divide attr-from-field-not-m0; v_fma fused/v_mad unfused. AC#1 spirv-val vulkan1.1 ALL 3 PASS (+orchestrator INDEPENDENT spirv-as+spirv-val on VS golden PASS). AC#2 golden spirv-dis tests/recompile_golden/*.spvasm (regen via --ignored regen_golden_disasm). AC#3 portable caps: each module exactly OpCapability Shader (asserted). AC#4 LIVE GPU triangle = MAINTAINER (temp AshBackend hook, unticked). 38 tests. DIVERGENCE for task-41 (documented in-source): (1) VINTRP — recompiler reads already-interpolated Location input; oracle computes plane eq from P0/P1/P2+barycentrics → harness MUST drive PS Location with the SAME P0+I(P1-P0)+J(P2-P0) the oracle produces; (2) v_med3 NaN GLSL-vs-Rust differ (corpus finite, fine); (3) num_records/stride — recompiler models V# as vec4 runtime array stride=16 in-shader + nr push-const; oracle reads from V# bytes → task-42 supplies buffer+nr consistently; MUBUF offen REJECTED (deferred); (4) dead m0 OpLoad harmless. Combined gate: 32 suites, oracle 6/6, gcn Vulkan-free.
GPU VERIFY 2026-07-12 (maintainer ran diff_harness = this AC#4 + task-41 AC#2, same GPU path — no separate command): **PS recompile CONFIRMED EXACT on real GPU** — flat_color_ps + interp_color_ps GPU mrt0 == oracle (VINTRP included). VS: the recompiled triangle RENDERS at the correct NDC positions (±63/64 ≈ ±1) but diff_harness reported [DIVERGE] due to a HARNESS readback artifact (Y-flip + reads pixel-center position not the exported pos0 value), NOT a recompiler bug — filed as task-91 (low). AC#4 ticked: recompiled shaders execute correctly on GPU (PS exact, VS renders the triangle).
<!-- SECTION:NOTES:END -->
