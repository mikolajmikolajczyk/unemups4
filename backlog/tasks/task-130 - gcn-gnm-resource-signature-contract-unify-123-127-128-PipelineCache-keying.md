---
id: TASK-130
title: >-
  gcn/gnm: resource-signature contract (unify 123/127/128 + PipelineCache
  keying)
status: Done
assignee: []
created_date: '2026-07-16 06:48'
updated_date: '2026-07-16 12:17'
labels:
  - from-audit
  - arch
  - gcn
  - gnm
dependencies:
  - TASK-123
  - TASK-127
  - TASK-128
ordinal: 136000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Fable architecture review — top structural investment #1. The recompiler resolves descriptors symbolically but THROWS AWAY the provenance (which user-SGPR slot, which set/binding, what vertex stride), so the executor re-derives them from Celeste-shaped constants (the whole Tier-1 silent-wrong-output class: tasks 123 CB-sbase, 127 SGPR ABI, 128 vertex stride). Fix as ONE contract change instead of three patches: recompile() emits a per-shader resource signature — a table of {descriptor kind, user-SGPR slot(s), set/binding, element stride or spec-constant id} on IoLayout; exec.rs/vbuf.rs consume it generically. Second-order: PipelineCache keys MUST include the full signature or varying strides/layouts cause silent wrong-pipeline reuse. Umbrella over 123/127/128.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 IoLayout carries per-descriptor provenance (SGPR slot, set/binding, stride) emitted by recompile()
- [x] #2 executor binds all descriptors from the signature, zero hardcoded s[..]/binding/stride constants (closes 123/127/128)
- [x] #3 PipelineCache key includes the resource signature (no wrong-pipeline reuse across differing layouts)
<!-- AC:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Design pass 2026-07-16 (Plan agent). NEW TYPE in recompile.rs (re-exported lib.rs): enum DescriptorSource { InlineVSharp{sgpr} (CB: s_buffer_load SBASE names SGPR quad holding V#), SetPointer{sgpr,desc_offset} (vertex/texture: SMRD sbase names desc-set POINTER pair, desc at offset) }. Add per-field 'source' to the 3 binding structs (ConstBufferBinding/BufferBinding/SamplerBinding) — IoLayout itself IS the signature once bindings carry provenance (no parallel table; keeps .first()/.iter() pairing coupled).

POPULATE (recompiler resolve sites, currently discarded): CB @recompile.rs:1846 push add source=InlineVSharp{sgpr:sbase} (already have sbase via emit_s_buffer_load/ensure_const_buffer, private ConstBuffer.sbase:498). VERTEX provenance is at the SMRD not MUBUF: emit_smrd s_load branch (~1711) currently records only sdst into vsharp_sgprs:411 — extend that HashSet<u8> to HashMap<u8,(sbase,offset*4)>; emit_mubuf:1910 () looks up srsrc→(sbase,desc_offset)→BufferBinding.source=SetPointer; unmapped srsrc = Unsupported defer. TEXTURE emit_mimg:2120 () same SMRD-map lookup → SamplerBinding.source + s_offset. STRIDE: emit as SPIR-V OpSpecConstant (recommended, keeps stride OUT of pipeline key) not baked VB_ELEMENT_STRIDE=16 @2570 — BufferBinding gets stride_spec_id.

EXEC.rs CONSUME (delete the consts): derive_const_buffer:707 read binding.source InlineVSharp{sgpr}→user.slot(sgpr..+4), del CONST_BUFFER_SBASE_SGPR:926. fetch_layout_of:942 build BufferSlot from b.source SetPointer, del DESC_SET_USER_SGPR:937 (vbuf resolve_slot/derive_buffer_ranges already consume user_sgpr/desc_offset generically). derive_texture_binding:737 from sampler.source, del CORPUS_TEXTURE_SLOT vbuf.rs:586. .first()→.iter() @422/434/458 (bind ALL descriptors not just idx0). Stride defer:601 removed once spec-constant flows through bind. Preserve strict-or-defer: out-of-range SGPR → defer draw (None), never partial bind.

PIPELINE KEY (core/src/gpu.rs:198 PipelineKey): silent-wrong-reuse risk — same VS addr + different bound stride/layout = byte-identical key → HIT wrong-stride pipeline (created_count stays 1). Add ResourceSignature{storage,const_storage,texture (all already Hash), vertex_stride}; populate @exec.rs:533 before get_or_mint. DECISION: spec-constant stride ⇒ stride OUT of key (one pipeline all strides, stride via bind), set/binding provenance IN key; baked stride ⇒ stride IN key. Recommend spec-constant + set/binding-only key.

CLOSES: 123 (CB source+derive_const_buffer+corpus SBASE!=4). 127 (DescriptorSource on all 3 + all derive_* read source + 3 consts deleted + shifted-SGPR corpus). 128 (stride spec-constant + non-16 renders no-defer + key no-collide).

CORPUS (the missing divergence): shifted_cbuffer16_vs (SBASE s[8:11] not s[4:7]) + exec test programming s[8:11] must FAIL against hardcoded 4. nonstd_stride_vs (stride 12/24/32, must not defer). shifted_texture_ps. pipeline_cache test: same vs/ps hash + different ResourceSignature.stride/binding must both MISS (created_count==2).

SEQUENCE (mergeable slices): (1) DescriptorSource enum + fields + populate, executor still ignores = additive, no behavior change, update IoLayout goldens. (2) exec.rs CB+vertex-ptr provenance, del 2 consts, closes 123+SGPR-half-127. (3) exec.rs texture provenance, del CORPUS_TEXTURE_SLOT. (4) .first()→.iter(). (6) PipelineKey ResourceSignature. (5) stride spec-constant (SPIR-V body change, spirv-val+MoltenVK, largest, last). Order 1→2→3→4→6→5.

RISK: HIGH COLLISION with in-flight Celeste exec.rs work (derive_const_buffer/derive_texture_binding/fetch_layout_of = exactly the Entry-9/10 fns) — land (1) FIRST (pure ps4-gcn additive, no exec.rs touch, merges independently), sequence 2-4 AFTER Celeste executor stabilizes, one fn per change for localized rebases. IoLayout PartialEq forces golden updates (compile-forced not silent). spec-constant changes vertex SSBO type vec4[]→byte-indexed → validate spirv-val + portability subset. Full design in agent transcript.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Done 2026-07-16 (merge 9df9edb). Slices 1-4 earlier; slices 5-6 this round. Slice 5: vertex-fetch SSBO now byte-indexed runtimearray uint (ArrayStride 4), stride = SpecId-0 OpSpecConstant (default 16); fetch = IMul(idx,stride)/UDiv 4 -> load uint -> Bitcast f32. Non-16 no longer defers; real V# stride flows onto StorageBinding.stride. Slice 6: PipelineKey.resources = ResourceSignature{storage,const_storage,texture} (set/binding), stride OUT of key. 315 tests, golden+spirv-val+task-122 oracle (incl nonstd_stride_vs @stride24) green, clippy+fmt clean. GAP -> task-140: backend must specialize StorageBinding.stride into VkSpecializationInfo(SpecId 0); until then GPU uses default 16 (corpus=16 so fine, Celeste crashes at RADV submit first). Tension: spec-const bakes at pipeline-create vs stride-out-of-key -> resolve in 140 (push-constant recommended).
<!-- SECTION:NOTES:END -->
