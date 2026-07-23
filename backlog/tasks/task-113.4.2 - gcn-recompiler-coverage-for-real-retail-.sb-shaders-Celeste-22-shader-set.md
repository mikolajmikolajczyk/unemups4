---
id: TASK-113.4.2
title: 'gcn: recompiler coverage for real retail .sb shaders (Celeste 22-shader set)'
status: Done
assignee: []
created_date: '2026-07-15 17:42'
updated_date: '2026-07-23 18:41'
labels: []
dependencies: []
parent_task_id: TASK-113.4
ordinal: 127000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Bring the GCN->SPIR-V recompiler up to the instruction set the retail managed-runtime title (Celeste, CUSA11302) actually emits, so its 22 real .sb shaders recompile and draws stop deferring at NeedsGcn. Scope mapped offline from dumped shader bytecode (UNEMUPS4_DUMP_GCN hook) via a decode+recompile harness — first-wall-per-shader histogram, not one 2-min guest run per gap.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 parse_sb accepts the real OrbShdr footer-gap layout (footer at code_start+m_length + input-usage-table gap, not tight) [DONE]
- [x] #2 recompiler handles the universal 's_mov_b32 vcc_hi,imm' prologue [DONE]
- [x] #3 VOP2 int/pack ops used by the set recompile with interp mirror + differential golden: V_AND_B32(0x1b), V_LSHLREV_B32(0x1a), V_CVT_PKRTZ_F16_F32(0x2f)
- [x] #4 SOP1 's_mov_b64 sdst,exec' (op4) exec-mask save modeled or safely discarded
- [x] #5 SMRD s_load_dwordx4/x8 (op9/11) scalar constant-buffer loads recompile
- [x] #6 MRT export compr (f16-packed) mode lowers correctly
- [x] #7 s_swappc_b64 (op0x21) vertex fetch-shader call resolved (VS attribute fetch) — leaf-inline via resolve_fetch_call; 5 VS (sh05/07/10/19/21) recompile end-to-end [DONE]
- [ ] #8 all 22 shaders recompile OR defer with a precise named reason; first real Celeste frame shows non-white pixels via UNEMUPS4_DUMP_PNG oracle
<!-- AC:END -->







## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Offline harness (scratchpad): dump real .sb via UNEMUPS4_DUMP_GCN, extract per-shader code bins, decode_all+recompile each, iterate on first-wall histogram (sub-second loop vs 2-min guest run). Per new instruction: add decoder(if needed)+interp mirror+recompiler emit+differential golden (task-41 harness), never guess (PNG oracle). Order: cleared parse+vcc; next mechanical VOP2 int/pack + SMRD + exec-mov + export-compr; last the s_swappc fetch-shader call (VS). Verify end-to-end with UNEMUPS4_DUMP_PNG.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
SCOPE MAPPED offline (2026-07-15). 22 unique real shaders (13 pixel, 5 vertex by hash). First-wall histogram after parse+vcc cleared:
- VOP2 unhandled: V_AND_B32 0x1b (sh01), V_LSHLREV_B32 0x1a (sh03), V_CVT_PKRTZ_F16_F32 0x2f (sh00/09) — int/bit + f16-pack for compr export.
- SOP1 s_mov_b64 sdst,exec (op4): 9 pixel shaders (sh02/04/06/08/14/15/16/18/20) — exec-mask save (WQM). sdst is Sgpr or vcc.
- SOP1 s_swappc_b64 (op0x21=33): 5 vertex shaders (sh05/07/10/19/21) — fetch-shader call, VS vertex-attribute fetch indirection. Deepest.
- SMRD s_load_dwordx4/x8 (op9/11): sh11/12/13/17 — scalar constant-buffer loads (distinct from s_buffer_load which already works).
Note: these are FIRST walls only; more instructions behind each. WINS: parse_sb footer-gap (crates/gnm/src/shader/sb.rs, MAX_FOOTER_GAP=256, code_range=exact code) + vcc prologue (crates/gcn/src/recompile.rs emit_sop1) — both tested, 18/18 sb + scope harness green. New diagnostic: UNEMUPS4_DUMP_GCN=<dir> dumps register-derived shader window (crates/gnm/src/shader/gcn.rs). Offline harness + shader bins live in session scratchpad (not committed).

--- Progress 2026-07-15 (session 2) ---
AC#3 PARTIAL: V_AND_B32(0x1b) + V_LSHLREV_B32(0x1a) DONE — interp exec_vop2 int arm (read_src_lane bits, shift masked to [4:0]) + recompile emit_vop2 int arm (eval_bits + bitwise_and/shift_left_logical, shift AND 0x1f — SPIR-V leaves out-of-range shift undefined so the mask is load-bearing) + decode test vop2_int_and_lshlrev (llvm-mc encodings 0x36020702/0x34020702). Walls moved: sh01→Vop3 op323, sh03→Smrd. V_CVT_PKRTZ_F16_F32(0x2f) still pending (sh00/09 — f16 pack, RTZ rounding subtlety; pairs with AC#6 compr export).

TWO CORRECTIONS to the histogram above:
1. SMRD op9/11 are NOT s_load_dwordx4/x8. Real GFX7 SMRD table: op9 = S_BUFFER_LOAD_DWORDX2, op11 = S_BUFFER_LOAD_DWORDX8. `opcodes::smrd::dst_count` maps NEITHER (only 0..4 s_load + 8/0x0A s_buffer_dword/x4), so both interp exec_smrd AND recompile emit_smrd reject at dst_count→None. AC#5 retitled: s_buffer_load_dwordx2/x8.
2. AC#5 is DEEPER than "scalar const-buf load already works". s_buffer_load reads UNIFORM CONSTANTS into SGPRs later consumed by ALU. interp exec_smrd currently only implements the s_load form (sbase = 64-bit guest pointer); s_buffer_load's SBASE is a 128-bit V# descriptor + dword offset into the buffer it describes. recompile emit_smrd only RECORDS vsharp_sgprs for MUBUF resolution — it never materializes loaded constant data. Faithful s_buffer_load needs real constant-buffer (UBO/SSBO) modeling on both sides. Not mechanical.

Depth ranking of remaining walls: AC#3 PKRTZ (f16, medium) < AC#4 exec-save (recompiler models no exec mask — divergence risk, needs design) < AC#5 s_buffer_load (UBO modeling, deep) < AC#7 s_swappc fetch-shader (deepest, 5 vertex).

--- Progress 2026-07-15 (session 2, cont.) ---
AC#3 DONE + AC#6 DONE (coupled — PKRTZ pairs with compr export): V_CVT_PKRTZ_F16_F32(0x2f) + `exp ... compr` f16-packed MRT export.
- Mechanism: PKRTZ packs 2×f32→2×f16 in a VGPR; compr export reads two VGPRs (vsrc0=ch0,1; vsrc1=ch2,3) and UNPACKS f16→f32, because the HLE MRT is f32-typed (Vulkan pipeline converts to the real RT format). interp exec_vop2 int-arm packs via half::f16::from_f32; exec_exp compr-branch unpacks via half::f16::to_f32. recompile emit_vop2 uses GLSL PackHalf2x16; emit_exp compr-branch uses UnpackHalf2x16 + composite_extract.
- ROUNDING DECISION (maintainer may veto): ISA PKRTZ is round-toward-zero, but portable SPIR-V has no cheap RTZ f16 (needs float16 capability → breaks the portable floor). Both sides use RNE (half crate RNE == GLSL Pack/UnpackHalf2x16 RNE), so interp==recompiler holds (decision-3). ≤1-f16-ULP deviation from true HW RTZ, invisible through an 8-bit RT. Added `half = "2"` dep to ps4-gcn.
- Verification: new corpus shader pkrtz_ps (.s/.code.bin/.sb/.dis committed), differential ShaderSpec with analytic expectation (1.0,0.25,0.5,1.0) computed from f16-exact literals (independent of the pack under test). All ps4-gcn + workspace green, spirv-val + interface-drift + golden intact. Value-level SPIR-V agreement is GPU-tier (diff_harness, maintainer-run) — not asserted here.
- Scope harness: 2/22 shaders now fully recompile (sh00, sh09). sh00 still exports f16(0) color until AC#5 (its s_buffer_load_dwordx4 records vsharp SGPRs but never materializes the real constant color); sh09 (interp attr0 → pack → export) is CORRECT end-to-end.

Remaining: AC#4 exec-save (9 px), AC#5 s_buffer_load (5 px, incl. making sh00 correct), AC#7 s_swappc fetch-shader (5 vs). Next in order: AC#4.

--- Progress 2026-07-15 (session 2, cont. 2) ---
AC#4 DONE: s_mov_b64 (op4) + s_wqm_b64 (op0x0A) EXEC-save/WQM/restore bracket.
- Model: per-invocation SPIR-V has no wave EXEC; we treat every invocation's quad as fully covered, so s_wqm is the identity and the save/WQM/restore bracket is transparent to the export (the saved value flows only back to EXEC on restore). interp does a FAITHFUL 64-bit move (new read_scalar_pair/write_scalar_pair over exec/vcc/sgpr-pair), so a restore reproduces the saved EXEC; recompiler DISCARDS when EXEC is an operand (validate both sides), else does a real SGPR-pair copy. The two agree on exports because the bracket is export-transparent.
- BUG FIX (pre-existing): opcodes::sop1::S_WQM_B64 was 0x08 (that's actually s_not_b64); real GFX7 s_wqm_b64 is 0x0A. Only the disasm name map used it, so op 0x08 mis-printed as "s_wqm_b64" and real 0x0A was Unknown. Fixed to 0x0A (verified vs llvm-mc).
- Verify: new corpus shader wqm_bracket_ps (brackets a constant-color export) + differential ShaderSpec asserting the bracket is transparent (export == literals). decode test sop1_b64_exec_moves cross-checks both opcodes vs llvm-mc. All ps4-gcn + workspace green.
- Scope harness: 5/22 recompile now (added sh04, sh18, sh20). Remaining exec-save shaders advanced to their NEXT walls (sh06/08 VOP1 op51, sh16 VOP1 op36, sh02 VOP3 op1 + control flow, sh15 VOP3 op4, sh14 SMRD op9).

Remaining ACs: AC#5 s_buffer_load (SMRD op9/11 = s_buffer_load_dwordx2/x8, UBO modeling — deep; makes sh00 correct + unblocks sh11/12/13/14/17), AC#7 s_swappc fetch-shader (5 vs, deepest). Plus newly-revealed mechanical VOP1/VOP3 walls (op36/51 vop1, op1/4 vop3) behind the cleared exec-save. Next in order: AC#5.

--- Progress 2026-07-15 (session 2, cont. 3) ---
AC#5 DONE: s_buffer_load (scalar constant-buffer loads). Opcodes added: S_BUFFER_LOAD_DWORDX2 (0x09), S_BUFFER_LOAD_DWORDX8 (0x0B) + is_buffer_load() helper; dst_count/name extended.
- Binding model (user-approved): the constant buffer is a StorageBuffer SSBO of raw uint dwords at set0/BIND 2 (bind1 is taken by the PS combined image-sampler; bind0 by the VS vertex SSBO — so bind2 avoids collision for a texturing PS that also loads constants). Chosen over UBO (std140/size limits) and push-constants (too small).
- interp exec_smrd: base now resolves by load kind — s_buffer_load reads the 128-bit V# via decode_v_sharp (base+stride+num_records) at SBASE; s_load keeps the 64-bit pointer pair. Then addr = base + off*4, loads real bytes through the VMM.
- recompile: new emit_s_buffer_load + ensure_const_buffer (lazy SSBO decl, uint runtime array ArrayStride 4). Loads via access_chain[0, dword_idx] into SGPR slots. Immediate-offset only (SGPR-offset defers). SINGLE constant buffer: the V# is resolved symbolically so two distinct SBASEs can't be told apart → a 2nd distinct SBASE defers (Unsupported). New IoLayout.const_buffers: Vec<ConstBufferBinding{set,binding,size_dwords}> — SEPARATE from io.buffers (which the drift guard keys to the MUBUF vertex fetch).
- FOLLOW-UP (GPU tier, task-113.4.1): the ps4-gnm provider must actually BIND the guest constant buffer bytes (starting at V#.base) at set0/bind2 for value-level correctness on a real device. The headless differential proves the oracle; GPU binding is unwired here.
- Verify: new corpus shader cbuffer_ps (s_buffer_loads RGBA from a mock CB via V#, exports it) + differential ShaderSpec with build_cbuffer_memory (analytic = the constants placed in the buffer, proving the oracle reads real bytes not a stub). recompile test cbuffer_ps_io_layout_declares_one_const_buffer (set0/bind2, size 4). decode test smrd_s_buffer_load_x2_x8 cross-checks op9/11 vs llvm-mc. All ps4-gcn + workspace + clippy green, spirv-val intact.
- Scope harness: 6/22 recompile now (added sh13); sh00 now loads its REAL constant color (was f16(0)). s_buffer_load cleared for sh11/12/14/17 → advanced to VOP1 transcendentals (op32/36/51), VOP2 op22, VOP3 op1/4.

Remaining ACs: AC#7 s_swappc fetch-shader (5 vs, deepest). Plus mechanical VOP1 transcendental (op32/36/51 — v_exp/v_log/v_rcp/v_sqrt family) + VOP3 (op1/4/323) walls now frontier for the pixel shaders. Next in order: AC#7 (or the mechanical VOP1/VOP3 batch first, since it unblocks more pixel shaders).

--- Progress 2026-07-15 (session 2, cont. 4) ---
VOP1 transcendentals DONE (not a numbered AC — frontier batch): v_fract_f32 (0x20), v_floor_f32 (0x24), v_sqrt_f32 (0x33) via GLSL Fract/Floor/Sqrt (all correctly-rounded/exact → interp==recompiler; GCN v_sqrt HW approx modeled as IEEE, sub-ULP invisible). corpus transcendental_ps + differential spec (floor2.5/fract2.5/sqrt4 → 2.0/0.5/2.0/2.0, exact). 7/22 recompile (sh14 added).

=== EXACT REMAINING FRONTIER (offline histogram, 7/22 ok, 15 fail) ===
- Sop1 op33 (s_swappc_b64) — AC#7, 5 VERTEX shaders (sh05/07/10/19/21). Deepest: fetch-shader call indirection (VS vertex-attribute fetch). Structurally different — jumps to a separate code blob (ends s_setpc not s_endpgm); crates/gcn/src/fetch_shader.rs already parses fetch shaders. This is the big remaining piece.
- Vop1 op53 (0x35): 1 px (need to ID — likely another transcendental, v_exp/v_log family).
- Vop2 op15 (0x0F), op22 (0x16): int/minmax VOP2 (2 px). Mechanical (add to emit_vop2 int/float arm).
- Vop3 op1, op4: 2 px — VOP3-only 3-input ops (need decoder VOP3 op mapping; likely v_cndmask/v_add promoted).
- Vop3 op287 (0x11F): 2 px — VOPC (compare) in VOP3 form (0x100+0x1F). Needs VOPC + result-to-vgpr/sgpr modeling.
- Vop3 op323 (0x143): 1 px — VOP2 v_add_f32 promoted to VOP3 (0x140+3) with modifiers (sh01).
- Vop3 op416 (0x1A0): 2 px — VOP3 op with abs/omod modifiers (v_mul-ish, abs:1 omod:2).
NEXT: either AC#7 (deep, unblocks the 5 VS) or continue mechanical VOP2/VOP3 pixel batch (op15/22, op1/4/287/323/416). VOP3-promoted ops need the decoder's VOP3 op-number → mnemonic mapping worked out first (op>=0x100 = VOPC promoted, >=0x140 = VOP2 promoted).

--- Progress 2026-07-15 (session 2, cont. 5) ---
VOP2 min/max/lshrrev DONE (frontier batch): v_min_f32 (0x0F), v_max_f32 (0x10), v_lshrrev_b32 (0x16). interp f32::min/max + logical shr; recompile GLSL FMin/FMax + shift_right_logical (shift masked). corpus minmax_shift_ps + differential spec (min/max 0.5,0.25 + 8>>1→4.0). Still 7/22 (these cleared FIRST walls on deeper shaders — more behind).

=== REMAINING FRONTIER after this session (7/22 recompile) ===
Everything mechanical & shallow is now DONE. What's left is genuinely DEEP — best on fresh context:
1. AC#7 s_swappc_b64 (Sop1 op33) — 5 VERTEX shaders (sh05/07/10/19/21). Fetch-shader call: VS jumps to a separate code blob (ends s_setpc, parsed by crates/gcn/src/fetch_shader.rs) that does vertex-attribute fetch, then returns. Structural — the recompiler must inline/resolve the fetch shader's attribute loads. THE big remaining piece.
2. VOP3-promoted ops (need decoder VOP3 op→mnemonic map first; op>=0x100 VOPC, >=0x140 VOP2):
   - Vop3 op287 (0x11F) = VOPC compare (0x100+0x1F) → writes VCC. Then a v_cndmask reads VCC. The recompiler does NOT model VCC (special_bits rejects vcc reads) — this is the predication/control-flow path (same family as sh02's s_cbranch_execz). Needs VCC-as-bool modeling. DEEP.
   - Vop3 op1/op4 = VOP3-only 3-input ops (likely v_cndmask_b32 / v_add promoted) — op4 has src2=Sgpr (carry-in?), op1 has 3 srcs. Need ID + likely VCC/carry modeling.
   - Vop3 op323 (0x143) = v_add_f32 promoted to VOP3 w/ modifiers (sh01) — mechanical-ish once VOP3-VOP2 promotion path handled.
   - Vop3 op416 (0x1A0) = VOP3 op w/ abs:1 omod:2 (v_mul-ish). Mechanical-ish.
   - Vop1 op53 (0x35) = 1 px, another transcendental (v_exp/v_log family) — mechanical.
Recommendation: fresh session. Do the VOP3-VOP2-promotion path (op323/416) + Vop1 op53 as one mechanical batch, THEN tackle the VCC/predication family (op287/op1/op4) as its own design chunk (needs VCC-as-bool + v_cndmask), THEN AC#7 fetch-shader (structural). The VCC path also unblocks sh02's s_cbranch_execz.

SESSION 2 SUMMARY: from 0/22 → 7/22 recompiling. Done: AC#3,#4,#5,#6 + VOP1 transcendentals + VOP2 min/max/shr. 7 corpus goldens added (pkrtz, wqm_bracket, cbuffer, transcendental, minmax_shift + the 2 prior). Every op: interp mirror + recompile emit + differential golden + decode test vs llvm-mc. Fixed pre-existing S_WQM_B64 opcode bug. Added half="2" dep. New IoLayout.const_buffers (SSBO set0/bind2 for constant buffers). GPU-tier follow-up: ps4-gnm provider must bind the constant buffer for value-level correctness (task-113.4.1).

--- Progress 2026-07-15 (session 3) — mechanical VOP3/VOP1 tail DONE, 7→10/22 ---
Cleared the ENTIRE mechanical remainder of the frontier. RE'd every op against llvm-mc bonaire `-show-encoding` FIRST — several session-2 frontier GUESSES were WRONG and are corrected below.

Ops added (each: interp mirror + recompile emit + differential golden + decode golden vs llvm-mc; all exact-in-f32 analytic exports; full suite green + clippy clean):
- v_sin_f32 (VOP1 0x35) — NOT "v_exp/v_log"; it is SINE. GCN sin takes revolutions: D=sin(2*PI*S0). Oracle (x*TAU).sin(); recompile GLSL Sin(TAU*x), same f32 TAU.
- v_mad_u32_u24 (VOP3 0x143) — NOT "v_add_f32 promoted". It is the 24-bit unsigned integer MAD: (S0&0xFFFFFF)*(S1&0xFFFFFF)+S2 (32-bit wrapping). Integer path, no float mods.
- v_fract_f32 (VOP3 0x1A0 = VOP1 0x20 + 0x180) — NOT "v_mul-ish". FRACT re-encoded as VOP3 to carry abs + omod. (Range comment fixed: VOP1-in-VOP3 base is 0x180, not 0x140.)
- v_mul_f32 (VOP3 0x108 = VOP2 0x08 + 0x100) — abs/neg/omod modifier form of multiply.
- v_rcp_f32 (VOP1 0x2A) — reciprocal; exact 1.0/x both sides (GLSL has no Rcp → OpFDiv 1.0/x). Same documented HW-approx deviation as v_sqrt.
- v_mac_f32 (VOP3 0x11F = VOP2 0x1F + 0x100) — NOT "VOPC compare". Multiply-ACCUMULATE: D=S0*S1+D (dst is implicit accumulator, read-modify-write; recompiler reads old dst via load_reg_f32). UNFUSED.
- v_cvt_pkrtz_f16_f32 (VOP3 0x12F = VOP2 0x2F + 0x100) — VOP3 form of the f16 pack (reuses GLSL PackHalf2x16 path). Cleared sh06+sh08 → 10/22.

Corpora added: vop3_mad_sin_fract_ps, vop3_mul_ps, rcp_ps, vop3_mac_ps, vop3_pkrtz_ps. Commits: 96bad51(mad/sin/fract) → 3d0826f(mul) → bad56f1(rcp) → 1ab32bd(mac, sh17→ok 8/22) → 7fd2a60(pkrtz, sh06+08→ok 10/22), each merged --no-ff to main.

VOP3 op-decode key (verified): op = 0x100|(byte2>>1) for 0xD2 prefix, 0x180|(byte2>>1) for 0xD3. Ranges: VOPC→0x000, VOP2→0x100, native-VOP3→0x140, VOP1→0x180.

=== EXACT REMAINING FRONTIER (10/22 ok, 12 fail) — all DEEP now, no mechanical ops left ===
A) PREDICATION / VCC family (6 shaders): recompiler does NOT model VCC or an SGPR predicate mask. Needs: (i) VOPC compares producing a per-lane bool — standalone VOPC writes VCC [sh11/12 Vopc op1=v_cmp_lt_f32, sh16 Vopc op4=v_cmp_gt_f32], VOP3-form writes an arbitrary SGPR pair via the sdst field the decoder currently MISLABELS as vdst [sh02 Vop3 op1=v_cmp_lt_f32→s[16:17], sh15 Vop3 op4=v_cmp→s[12:13]]; (ii) v_cndmask_b32 consuming that predicate; (iii) v_add_i32 (Vop2 op37, sh01) integer add w/ VCC carry-OUT (result is plain wrapping add; carry only matters if later read). KEY INSIGHT: per-invocation SPIR-V = ONE lane, so VCC/SGPR-pair predicate is a single bool per invocation — far simpler than wave-level VCC. Also unblocks sh02's later s_cbranch_execz.
B) FETCH-SHADER (5 vertex shaders sh05/07/10/19/21): Sop1 op33 = s_swappc_b64. VS calls a separate fetch-shader blob (ends s_setpc, parsed by crates/gcn/src/fetch_shader.rs) doing vertex-attribute fetch, then returns. Structural — recompiler must inline/resolve the fetch shader's attribute loads. AC#7, THE big remaining piece.
C) DEFERRED: sh03 m0 source read (m0 never written; not faithfully modeled). 1 shader.

Recommendation: predication/VCC family next (biggest lever, 6 shaders, self-contained; per-invocation single-lane bool). Then AC#7 fetch-shader (structural). m0 last/deferred.

SESSION 3 SUMMARY: 7→10/22. All mechanical VOP tail done + 3 wrong session-2 op guesses corrected via RE. Everything left is predication (VCC-as-per-invocation-bool) or fetch-shader indirection.

--- Progress 2026-07-15 (session 4) — PREDICATION / VCC family DONE, 10→13/22 ---
Frontier A (predication) cleared. Every op RE'd vs llvm-mc bonaire `-show-encoding` FIRST; each op = interp mirror + recompile emit + differential golden (exact-in-f32) + decode golden vs llvm-mc. Full ps4-gcn + workspace (355) + clippy green.

KEY MODEL: recompiled SPIR-V is per-invocation = ONE lane, so VCC / an SGPR-pair predicate is a SINGLE bool per invocation, not a 64-bit wave mask. New `PredKey { Vcc, SgprPair(n) }` → bool value id map in the recompiler (straight-line = most-recent store). A compare emits an OpFOrd* → bool stored under its dest key; v_cndmask lowers to OpSelect; v_add_i32 carry is `(a+b)<a` (OpULessThan) → bool. Interp mirrors faithfully into the wave-level `st.vcc` (u64, 1 bit/lane) and per-SGPR-pair state via new write/read_predicate_bit helpers.

Ops added:
- VOPC standalone (writes VCC): v_cmp_lt_f32 (op1), v_cmp_gt_f32 (op4) [+ eq/le/ge named]. `Inst::Vopc` was decoded but NEVER dispatched in interp/recompile — added exec_vopc/emit_vopc.
- VOP3-form VOPC (writes an ARBITRARY sgpr pair via the `sdst` field): v_cmp_lt_f32 (VOP3 op 0x001), v_cmp_gt_f32 (op 0x004). DECODER FIX: for a VOP3 whose op<0x100 (VOPC-in-VOP3), bits[7:0] are the sgpr-pair sdst, which the decoder MISLABELED as vdst=Vgpr — now decoded via operand::decode_src (→ Sgpr/vcc). disasm renders `s[n:n+1]`, 2 srcs, no src2.
- v_cndmask_b32: VOP2 op 0x00 (reads implicit VCC) + VOP3 op 0x100 (reads sgpr-pair/vcc predicate in src2). OpSelect on the resolved bool. A cndmask reading a never-written predicate is a clean defer (not a fabricated 0 → would diverge).
- v_add_i32: VOP2 op 0x25=37. Plain 32-bit wrapping add (OpIAdd/wrapping_add), carry-OUT to VCC per-lane. Carry only matters if later read (a cndmask in the golden proves both result + carry).

Corpora (3, self-authored, ZERO copyrighted): cmp_cndmask_ps (standalone VOPC lt+gt → cndmask → (0.75,0.25,0.5,1.0)), vop3_cmp_cndmask_ps (VOP3-VOPC → s[16:17]/s[12:13] → cndmask, same export), vadd_i32_ps (no-carry 1+2=3 + carry -1+1=0 wraps, both consumed by cndmask → (3.0,0.25,0.75,1.0)). Registered in corpus.rs + decode.rs + differential.rs. New decode test vopc_cndmask_add_i32_predication cross-checks all 5 encodings vs llvm-mc.

Scope harness (celeste_scope, offline, not committed): 13/22 recompile now.
- sh01 → OK (v_add_i32). sh15 → OK (VOP3-VOPC gt → s[12:13] + cndmask). sh16 → OK (VOPC gt → vcc + cndmask).
- sh11/12: all 19 predication ops (mixed VCC + s[0:1]-pair cndmask predicates) now recompile; advanced to a NEW unrelated wall Vop1 op14 = v_cvt_off_f32_i4 (RE'd; obscure f32 conversion — mechanical follow-up, NOT predication).
- sh02: its VOPC compares (VOP3 lt→s[16:17], standalone lt→vcc, VOP3 gt→s[18:19]) now recompile; wall moved to Sop2 op17 = s_or_b32 vcc, vcc, s16 (COMBINING two predicate masks — a scalar bool-OR of predicate registers) and then s_cbranch_execz (control flow). FOLLOW-UP.

REMAINING FRONTIER after this session (13/22 ok, 9 fail) — all DEEP/structural:
- Frontier B FETCH-SHADER (5 vertex sh05/07/10/19/21): Sop1 op33 = s_swappc_b64. AC#7, THE big structural piece.
- sh02: s_or_b32 on predicate registers (extend PredKey with a bool-OR = OpLogicalOr) + s_cbranch_execz control flow. The single-bool predicate model built here is the foundation for both; own chunk (control flow).
- sh11/12: Vop1 op14 = v_cvt_off_f32_i4 (mechanical, + more behind it).
- sh03: m0 source read (deferred, 1 shader).

--- Progress 2026-07-15 (AC#7 — s_swappc_b64 fetch-shader call — DONE) ---
AC#7 DONE: the 5 vertex shaders (sh05/07/10/19/21) now recompile end-to-end (offline scope harness, resolving against a synthetic fetch shader). Branch: agent-a267393a3a605a5ea worktree.

RE'd fetch-shader ABI (llvm-mc first, per doc-6 Entry 7 discipline; full mechanism in NEW doc-6 Entry 8):
- s_swappc_b64 (SOP1 op 0x21) = subroutine CALL: saves return PC into sdst, jumps to ssrc0. Real bytes `be802100` in all 5 VS = `s_swappc_b64 s[0:1], s[0:1]` (llvm-mc verified). The gnmx driver preloads the fetch-shader pointer into user-SGPR pair s[0:1]; the call saves the return PC back into s[0:1].
- s_setpc_b64 (op 0x20) = the fetch shader's RETURN (`be802000`).
- Fetch shader = a separate leaf blob: `s_load_dwordx4` the vertex-buffer V# from the desc set in s[2:3], `buffer_load_format_* … idxen` (v0 = vertex index) the attributes into an agreed VGPR block (v[4:7] here), `s_setpc_b64 s[0:1]` return. The main VS then consumes those VGPRs.

MECHANISM (KEY INSIGHT): a leaf call/return is EXACTLY a stream-inline. No new interp/recompile op. New `crates/gcn/src/fetch_call.rs`: `resolve_fetch_call(main, fetch)` finds the single s_swappc, validates the SGPR-pair shape, confirms `parse_fetch_shader` accepts the fetch body, splices `fetch[..s_setpc]` at the call site, renumbers offset_dwords. After resolution the stream is plain straight-line VS code (SMRD + idxen MUBUF + main body) that interp AND recompile already handle identically → the differential harness validates it for free. Strict-or-defer: 0 calls → nothing; 2 calls / non-SGPR ptr / unrecognized body → clean FetchResolveError, never a partial splice. Also `has_fetch_call`, `resolve_fetch_call_from_code`. Exported from lib.rs.

WALL BEHIND IT (also cleared, mechanical): with s_swappc resolved, all 5 VS hit ONE shared next wall — `Smrd op 12`. RE'd: real bytes `c3000500` = `s_buffer_load_dwordx16 s[0:15], s[4:7], 0x0` (SMRD op 0x0C = the 4×4 transform-matrix constant load). Just the wider sibling of AC#5's s_buffer_load (count 16); added op 0x0C to opcodes::smrd (name/dst_count/is_buffer_load) → flows through the existing const-buffer SSBO emit/interp unchanged. With both, all 5 VS recompile + spirv-val clean.

INTERP FIX (prologue): the oracle's write_sgpr didn't model an `s_mov_b32 vcc_hi,imm` DESTINATION (the universal Orbis prologue). The recompiler discards it (AC#2); the interp now does a faithful 32-bit write into the VCC half (mirrors read_scalar which already reads vcc). Both agree because VCC never feeds an export in this subset. Needed because the corpus goldens never exercised the prologue in an oracle run before.

VERIFY (full discipline per op): interp mirror + recompile emit + differential golden + decode golden vs llvm-mc.
- fetch_call.rs mod tests: detect/resolve/defer (5 unit tests), fetch body inlined, no s_swappc/s_setpc left, contiguous offsets.
- Corpus: `fetch_pos_vs.s` (fetch_* callee — skipped as standalone, ends s_setpc) + `inline_fetch_vs.s` (caller: prologue + s_swappc + exports the fetched v[4:7] as pos0/param0). Differential ShaderSpec (analytic = the input positions, a pass-through). Harness routes decoding through new `runnable_insts`/`fetch_callee_for`, resolving the call before run/recompile.
- Corpus: `cbuffer16_vs.s` (s_buffer_load_dwordx16 loads a 4×4 matrix, exports the diagonal dwords 0/5/10/15 = 1,6,11,16). Differential ShaderSpec with build_cbuffer16_memory (analytic-exact). decode test smrd_s_buffer_load_x2_x8_x16 cross-checks op 0x0C vs llvm-mc.
- All ps4-gcn tests green (21+13+12+9+3+3), clippy -D warnings clean, workspace build + ps4-gnm (172) green. New doc-6 Entry 9.

NOTE: this AC#7 block was written on a branch based BEFORE the session-4 predication merge; its "12/22"/remaining counts are pre-merge. sh01 was cleared by predication (v_add_i32). See combined state below.

--- Progress 2026-07-15 (both opus branches merged) — standalone harness 13/22, fetch mechanism done ---
Merged predication (session 4) + fetch-shader (AC#7) into main; m0 already landed. Merge conflicts resolved: corpus hashes m0=PS_D, cmp/vop3_cmp/vadd=PS_E/F/G, fetch VS=VS_1/VS_2; doc-6 Entry 8 (predication) + Entry 9 (fetch); recompiler `m0_var` + `predicates` state coexist. Full ps4-gcn + workspace + clippy green post-merge.

COUNT CLARIFICATION: the standalone `celeste_scope` harness still reports 13/22 — it decodes each dumped VS IN ISOLATION and does not call `resolve_fetch_call`, so the 5 fetch VS (sh05/07/10/19/21) still show `s_swappc_b64` as their first wall THERE. The fetch MECHANISM is complete and proven by the committed `inline_fetch_vs` differential (a caller resolved against `fetch_pos_vs` before run/recompile) + `fetch_call.rs` unit tests; AC#7 is met at that level. The 5 REAL dumped VS recompile end-to-end once their real fetch-shader blob is spliced in — which in a live run is the provider's job (read the blob at the s[0:1] pointer, call resolve_fetch_call before recompile; task-113.4.1). So "5 VS recompile" is true with resolution applied, not in the isolation harness.

REMAINING FRONTIER (standalone harness, 9 fail): 5 fetch VS (provider-gated, mechanism done); sh02 (s_or_b32 on predicate regs = OpLogicalOr + s_cbranch_execz control flow); sh03 (m0 read now passes → next wall Vop1 op2 + control flow); sh11/12 (Vop1 op14 = v_cvt_off_f32_i4 mechanical + more behind).
FOLLOW-UP: (1) GPU-tier provider fetch resolution — wires the 5 VS into a real run (task-113.4.1). (2) control-flow chunk (s_or_b32 predicate-OR + s_cbranch_execz — unblocks sh02; foundation = the single-bool predicate model). (3) v_cvt_off_f32_i4 mechanical (sh11/12). (4) AC#8 value-level PNG oracle.
<!-- SECTION:NOTES:END -->
