---
id: TASK-39
title: 'gcn: wave CPU interpreter — the correctness oracle'
status: Done
assignee: []
created_date: '2026-07-11 12:53'
updated_date: '2026-07-12 11:00'
labels:
  - gpu
  - gcn
dependencies:
  - TASK-38
priority: high
ordinal: 38000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Keystone of 4a. wave64 interpreter over P4-03 Inst stream: WaveState{sgprs,vgprs[lane],exec,vcc,scc,m0,pc}, launch-ABI init (user SGPRs, VS v0=vertex_index, PS interpolants/barycentrics simplified per VINTRP), execute triangle-subset ALU + s_load/buffer_load (memory via &dyn VirtualMemoryManager — identity in prod, Vec<u8> mock in tests) + VINTRP + EXP, with EXEC masking. Output = captured exp records (pos/param/mrt per lane). The differential oracle (decision-3) — never discarded, never needs a GPU. wave64+EXEC from day one (confirmed). Does NOT rasterize, does NOT feed draw path, does NOT do DS/MIMG/FLAT/f64/transcendental beyond corpus.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 headless: runs corpus VS over synthetic vertex buffers → expected exp pos0/param0 (golden)
- [ ] #2 headless: corpus PS → expected exp mrt0 colors incl. EXEC-masked-lane test
- [ ] #3 headless: SMRD+MUBUF loads exclusively via VirtualMemoryManager (mock-memory proves no ambient access)
- [ ] #4 headless: unsupported instr → structured InterpError, no panic
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
DONE 2026-07-12 (feat/task-39 @ 7d904ed, merged). crates/gcn/src/interp.rs + tests/interp.rs. WaveState{sgprs:[u32;104], vgprs:Vec<[u32;64]> per-reg-per-lane, exec:u64, vcc:u64, scc:bool, m0:u32, pc:u32}, WAVE_SIZE=64, EXEC-masked from first instr. LaunchAbi enum: Vertex{user_sgprs,first_vertex,num_lanes} (s[2:3]=V# ptr, v0=first_vertex+lane, exec=low num_lanes bits); Pixel(Box<PixelLaunch>){user_sgprs, PsInputs{attr_planes}, bary_i→v0/bary_j→v1, explicit exec}. Executes: VOP1 (v_mov,cvt f32↔i32/u32), VOP2 (add/sub/mul/mac/madmk/madak — honors Vop2.k), VOP3 (mad/fma/med3 + abs/neg src mods + omod 1=×2/2=×4/3=÷2 on result), SOP1 s_mov_b32(incl →m0), SMRD s_load_dwordx*(via VMM), MUBUF buffer_load_format_*(via VMM), VINTRP p1/p2/mov, EXP(captured); s_waitcnt/s_nop nop, s_endpgm halts. MUBUF V#-fetch: 128-bit V# from 4 SGPRs at srsrc — base=word0|(word1[15:0]<<32), stride=word1[29:16]; idxen → per-lane elem addr = base+index*stride+soffset+offset, reads vdata_count*4 bytes as consecutive f32; soffset==Raw(255)→InvalidOperand. **VINTRP/BARYCENTRIC MODEL (task-40 MUST MIRROR): plane eq P0 + I*(P1-P0) + J*(P2-P0); p1 writes P0+I*(P1-P0) to vdst (I=vsrc VGPR=v0); p2 reads that partial back from vdst + adds J*(P2-P0) (J=v1); mov=P0. (P0,P1,P2) per-attr/per-chan from PsInputs::attr_planes[attr][chan].** ExportRecord{lane, target:ExportTarget, values:[f32;4]}, one per EXP per live lane, masked-off never emit. InterpError (thiserror): UnsupportedInst{Box<Inst>,offset}/InvalidOperand{operand,offset,reason}/MemoryFault{addr,size,reason}, no panic. ACs: #1 vs_exports_positions_from_synthetic_buffer (SPOT-CHECKED: synthetic vbuf+V# in mock VMM → decode real passthrough_vs → pos0+param0==committed positions per lane). #2 flat PS→(1,0.25,0.5,1) + interp PS plane-eq + EXEC-masked lane 1 produces NO mrt0. #3 vs_loads_go_only_through_the_vmm (MockMem records exactly 2 reads SMRD 16B+MUBUF 16B; get_host_ptr always None → no ambient path). #4 unknown 0xFFFFFFFF→UnsupportedInst, Raw(255)→InvalidOperand, no panic. 29 gcn tests, Vulkan-free (+thiserror). Combined gate: 31 suites, oracle 6/6. ORACLE for task-41 (diff vs recompiler); task-40 consumes WaveState/ExportRecord + must mirror the VINTRP model.
<!-- SECTION:NOTES:END -->
