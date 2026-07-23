---
id: TASK-56
title: 'gnm/gpu: RT-as-texture host aliasing + opt-in readback (stretch, §8.5)'
status: To Do
assignee: []
created_date: '2026-07-11 12:55'
updated_date: '2026-07-16 11:07'
labels:
  - gpu
  - gnm
dependencies:
  - TASK-53
  - TASK-51
priority: medium
ordinal: 55000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Closes §8.6 phase-4 cache row: arbitrary render targets become cache entries (RenderTarget{…} keys, backend render-to-texture); when a draw samples a range a prior draw rendered, resolve host-side via backend blit/alias — no guest copy; readback(target) backend method + ReadbackPolicy env lever UNEMUPS4_RT_READBACK (default Off), re-tiling on readback reusing P4-15 inverse. Last cache step (buffers→textures→RT-readback); can slip to 4.x without blocking milestones. Does NOT add per-title config.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 headless: overlap detection — RT key + texture key over same range → exactly one host-side resolve command, zero guest writes
- [x] #2 headless: policy Off → no readback commands; On → flagged RT emits readback + marks entry clean
- [ ] #3 live GPU: two-pass corpus (render-to-target, sample it) displays correctly
<!-- AC:END -->



## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Design pass 2026-07-16 (Plan agent). DEEPEST CONSTRAINT: backend records ONE draw per submit-list into ONE fixed videoout target behind ONE draw-fence (run_command_list backend.rs:272, accumulator 277-293). RT-as-texture needs 2 draws/2 passes/producer-consumer + an offscreen target. TRACTABLE because: videoout texture_image ALREADY created SAMPLED|COLOR_ATTACHMENT|TRANSFER (vulkan.rs:752) + used as both attachment(1265) and sampled source(present blit 805) → one-image-both-roles aliasing is portability-PROVEN here. Executor ALREADY defers offscreen RT cleanly (derive_target UnregisteredTarget derive.rs:98 → exec.rs:870 'arbitrary RT out of scope'). ResLayout::RenderTarget key ALREADY exists (cache/mod.rs:124 'range aliased as RT+texture yields two entries') but nothing constructs it / no get_render_target.

(1) MECHANISM: one vk::Image per RT, usage COLOR_ATTACHMENT|SAMPLED|TRANSFER_SRC, mirror videoout image (NOT VK_IMAGE_CREATE_ALIAS_BIT / not two-images-one-memory — §8.5 aliasing = guest-range→host-RT). New backend CacheRenderTarget{image,view,mem,extent,current_layout} — RT layout CHANGES across passes so track per-RT (unlike CacheImage always SHADER_READ). BARRIERS: create UNDEFINED; write pass color attach initial=current_layout (UNDEFINED+CLEAR first, else LOAD from SHADER_READ), final=COLOR_ATTACHMENT_OPTIMAL, then barrier COLOR_ATTACHMENT→SHADER_READ (srcStage COLOR_ATTACHMENT_OUTPUT/dstStage FRAGMENT_SHADER — same as upload_image vulkan.rs:1289, proven); sample pass binds RT view as combined-image-sampler at SHADER_READ. PORTABILITY: R8G8B8A8_UNORM only (no mutableFormat views), explicit vkCmdPipelineBarrier (no sync2/dynamic_rendering), no ALIAS_BIT/sparse/external, SAMPLE_1.

(2) EXECUTOR RT REGISTRY (base,size,TargetDesc) per RT rendered this/prior submit. (a) STOP deferring draw-INTO-RT: derive_target UnregisteredTarget arm (derive.rs:98) → build TargetDesc from CB_COLOR0_* regs, mark TargetDesc.kind=Offscreen{base,size}; register + emit BackendCmd::CreateRenderTarget{id,w,h,fmt} + draw into RT id not videoout (NoColorBase embedded path exec.rs:853 unchanged). (b) RECOGNIZE sampling T#-names-RT: derive_texture_binding exec.rs:734 after decoding TextureDesc, if t.base matches/overlaps registered RT → RT-as-texture: NOT get_texture (would detile guest bytes GPU never wrote=garbage) but get_render_target_as_sampled → BindTexture at RT image view, ZERO CreateImage/UploadImage; SKIP macro-tile defer guard (exec.rs:470, RT is host-layout no guest tiling). FALLBACK: T# no RT match=plain texture unchanged; matches RT but not live=defer whole draw (needs_texture backend.rs:460), never half-bind.

(3) CACHE (cache/mod.rs): get_render_target(key,surface,fmt,out) parallel get_texture — emits CreateRenderTarget first use, NEVER upload (GPU fills); RT Entry never dirty-driven → drain_dirty(547) skips is_rt like imported(553). Overlap (AC#1) reuses ranges_overlap(626): RenderTarget key + Texture key over same (addr,size) = 2 distinct ResourceKey → 2 entries, executor resolves sampled bind to RT entry id, ONE BindTexture ZERO uploads. Backend: render_targets:HashMap parallel images, create_render_target mirror create_image(670), BindTexture resolves image_id from images OR render_targets(419), FreeResource frees either.

(4) READBACK (§8.5): ReadbackPolicy{Off,All} from UNEMUPS4_RT_READBACK default Off. Off=zero readback cmds. All=after draw-into-flagged-RT emit BackendCmd::ReadbackRenderTarget{id,addr,size}; backend readback() copies RT SHADER_READ→TRANSFER_SRC, blit to host-visible staging, RE-TILE (inverse tile.rs P4-15), write guest range, mark clean. AC#2 headless: Off→0 cmds, All→1/flagged-RT + entry clean. PNG oracle ALREADY exists: UNEMUPS4_DUMP_PNG (backend.rs:1926 dump_present_png) — two-pass result lands in composited swapchain so captures AC#3 with NO new code; optional UNEMUPS4_DUMP_RT_PNG per-RT debug behind own flag.

(5) SYNC (hard part, single-fence model): PREFERRED within-one-submit — STOP collapsing list to one draw: run_command_list(272) records a Vec<RecordedPass> into ONE cmdbuf/ONE submit/ONE fence: pass A into RT (+barrier from §1), pass B into videoout sampling RT; inter-pass barrier in same cmdbuf = correct write-then-sample NO extra fence, present path(945) unchanged. New BackendCmd::SetRenderTarget{id} opens pass, each Draw closes it. ACROSS submits: RT image persists in render_targets map (leaked like images), current_layout honored (list B sample takes SHADER_READ list A left); existing fence-then-record(1893) serializes A before B — no timeline semaphore this milestone. RACE GUARD: RT initial_layout in list-B write pass MUST equal tracked current_layout or validation fault → current_layout mandatory, never hardcode UNDEFINED on reuse (would discard cross-frame accumulation).

(6) TESTS: STRUCTURAL headless (mirror exec.rs:2926): draw-to-RT then sample-RT PM4 → assert exactly 1 CreateRenderTarget, 2nd draw BindTexture.image_id==RT ResourceId, ZERO CreateImage/UploadImage (AC#1). ReadbackPolicy test Off→0 / All→1+clean (AC#2). cache/tests.rs overlap: RenderTarget+Texture same (addr,size)→2 ids, RT skipped by drain_dirty. GPU/PNG (AC#3): two-pass corpus render-pattern-into-RT then sample→videoout, UNEMUPS4_DUMP_PNG vs golden.

(7) SEQUENCE (indep mergeable): 1 cache+core-types (get_render_target+is_rt+drain_dirty skip + BackendCmd CreateRenderTarget/ReadbackRenderTarget + ReadbackPolicy, unused, headless). 2 executor recognition (RT registry + derive_target offscreen + derive_texture RT-alias, gives AC#1 structural, backend ignores variants harmlessly). 3 backend RT resource (CacheRenderTarget+create+BindTexture from render_targets). 4 backend MULTI-PASS refactor run_command_list (largest/riskiest, LAST behind AC#3 corpus). 5 readback (indep, Off-gated). 1/2/5 headless-mergeable.

(8) RISKS: single-draw-per-list(272-495) deepest — generalizing to pass-seq touches present latch (embedded_drawn:89,819); mitigate keep videoout as FINAL pass, only PREPEND offscreen. MoltenVK: one-image COLOR+SAMPLED proven(752); non-RGBA8 RT defer clean(UnsupportedFormat 863). Layout desync across submits→validation fault; reset current_layout on Create + on fence-timeout(1901) treat UNDEFINED next use. Fixed-videoout assumption baked (RES_W/H 1671, one EmbeddedTarget 139) — offscreen RT of different size needs own render-pass+framebuffer sized to RT extent (per-RT EmbeddedTarget-like, easy to miss). Readback perf cliff — Off default. Partial T#/RT overlap (sub-rect/mip) OUT OF SCOPE — only exact-base/full-containment = RT-as-texture else defer. NOTE: this is the next Celeste wall after task-134 (compositor samples offscreen RTs). Full design in agent transcript.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
steps 1-4 merged @main (9bdec69). Step 4: run_command_list refactored to a pass SEQUENCE (RecordedPass/PassTarget/record_passes/record_pass_into) — offscreen RT draw (preceded by new BackendCmd::SetRenderTarget{id}) renders into RT's own render-pass+framebuffer(extent), COLOR_ATTACHMENT_OUTPUT->FRAGMENT_SHADER barrier to SHADER_READ, videoout draw stays FINAL pass (present untouched), one cmdbuf/one fence. current_layout drives initial layout (UNDEFINED+CLEAR first, LOAD-from-SHADER_READ reuse), rolled back to UNDEFINED on fence-timeout. embedded_drawn armed only by videoout pass. VERIFIED via PNG oracle on main: ps4-gcn-textured-quad renders a correct red/white checkerboard (orchestrator Read the PNG). No regression. Celeste PNG still all-black — blocked EARLIER on Graphics::GraphicsSystem dlsym wall, never submits an offscreen-RT workload. NEXT: step 5 (readback replay + re-tile, Off-gated) + a live RT-pixel re-verify once a title/2-pass-corpus actually submits an offscreen RT. RT producer->consumer path is verified structurally only (no live example drives an offscreen RT yet).

Step 5 (opt-in readback) MERGED @main (a013a27, 2026-07-16). ReadbackPolicy::from_env(UNEMUPS4_RT_READBACK), default Off = zero readback cmds. Executor queues one ReadbackRenderTarget per flagged RT (deduped by id), flushed at submit tail (after producer draw). Backend readback(): copy RT SHADER_READ->TRANSFER_SRC->back, re-tile via shared cache::tile::tile (inverse of upload detile), write guest range via SMC-observed write_bytes seam (never raw store); bounded copy fence (timeout->clean skip), staging buffer/mem/fence/cmdbuf freed. AC#1+#2 checked (headless proven: rt_readback_policy_off_emits_none_all_emits_one_and_leaves_entry_clean; 200 gpu+gnm tests green). AC#3 (live 2-pass corpus displays) STILL OPEN — no live title/corpus drives an offscreen RT yet; needs a render-into-RT-then-sample corpus + PNG oracle. Note: Celeste NOW reaches GNM submit on main (Graphics:: dlsym was NOT the wall — see task-135 / retail-bringup-epic memory), but frame is white/no-geometry pending PM4 IT_DMA_DATA/IT_INDEX_BUFFER_SIZE (task-135) — still no offscreen-RT submit from it.
<!-- SECTION:NOTES:END -->
