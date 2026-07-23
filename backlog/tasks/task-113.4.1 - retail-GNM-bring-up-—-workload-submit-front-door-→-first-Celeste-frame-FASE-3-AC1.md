---
id: TASK-113.4.1
title: >-
  retail GNM bring-up — workload submit front-door → first Celeste frame (FASE-3
  AC#1)
status: Done
assignee: []
created_date: '2026-07-15 08:33'
updated_date: '2026-07-23 18:41'
labels:
  - gpu
  - gnm
  - retail
dependencies:
  - TASK-113.4
parent_task_id: TASK-113.4
ordinal: 125000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Connect Celeste's GNM calls to the EXISTING GPU pipeline. Recon (done): the register-route path already works end-to-end for the examples — guest emits PM4 (draw+state) and HLE emits shader-setup PM4 → ps4_gnm::exec::Executor::run(&SubmitRange) (crates/gnm/src/exec.rs:139) decodes PM4 → GCN .sb recompiled to SPIR-V (crates/gcn) → Vulkan → sceVideoOut flip. Submit entry = record_submit in crates/libs/src/libscegnmdriver/submit.rs:73 -> Executor::run. Present sink wired at app/unemups4/src/main.rs. Celeste (2D MonoGame) uses the WORKLOAD submit variants + Eq events, none of which have handlers yet.

GAP vs Celeste imports (all currently unregistered — full list in task-113.4 notes):
1. Workload API: CreateWorkloadStream/Begin/End/DingDongForWorkload/SubmitCommandBuffersForWorkload/SubmitAndFlipCommandBuffersForWorkload/RequestFlipAndSubmitDoneForWorkload — the new front door; Submit*ForWorkload ~= existing SubmitCommandBuffers + a stream/workload id, route into Executor::run.
2. EQ events: AddEqEvent/DeleteEqEvent/GetEqEventType + real equeue GPU-completion signalling — Celeste likely gates frame progress on GPU-done events (today completion is synchronous, equeue is a stub).
3. State init: DrawInitToDefaultContextState400/DispatchInitDefaultHardwareState/SetVgtControl/ResetVgtControl (DrawInitDefaultHardwareState* exist as size-returning stubs).
4. Draw variants: DrawIndexOffset/DrawIndexMultiInstanced/DrawIndex{,Indirect}*/DrawIndirect*/DispatchIndirect — HLE handlers thin, real work = executor DECODE arms (only IT_DRAW_INDEX_AUTO + IT_DRAW_INDEX_2 handled today, exec.rs:190,193).
5. Shader updates: SetPsShader350/UpdatePsShader350/UpdateVsShader (+ Gs/Hs/Cs if pulled) as PM4 emitters extending emit::set_vs_shader/set_ps_shader (crates/gnm/src/pm4/emit.rs). Cs/Es/Gs/Hs/Ls are log-only stubs; compute + geometry/tess stages absent in the recompiler.
6. Caps/validate/debug getters: constant-returning stubs.

Docs: doc-2 (GPU architecture, main spec), doc-1 (29/40-dword shader-set layout), decision-4/6. Note GPU roadmap tasks 72/55/58/47 cover deeper GCN/texture work the retail path may pull.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 Workload submit family HLE'd: Submit{,AndFlip}CommandBuffersForWorkload route a SubmitRange into Executor::run; CreateWorkloadStream/Begin/End/DingDong/RequestFlipAndSubmitDone{,ForWorkload} wired
- [x] #2 sceGnmAddEqEvent/DeleteEqEvent/GetEqEventType + equeue GPU-completion so Celeste's frame-gating advances past submit
- [x] #3 State-init (DrawInitToDefaultContextState400 etc.), caps/validate/debug getters stubbed; guest reaches its first sceGnmSubmit*ForWorkload + sceVideoOut flip
- [ ] #4 PULL-DRIVEN: the executor runs on Celeste's real command buffer; the actual draw/shader/PM4 opcodes it emits are LOGGED and only those decode arms + shader emitters implemented — NOT all gap areas built blindly
- [ ] #5 Celeste presents its first frame (or the next concrete wall past a running executor is characterized with a PNG/log oracle per task-97)
<!-- AC:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Pull-driven smoke loop, same method as FASE-2/3. Phase A: workload front-door + EqEvents + state-init/caps stubs -> guest submits + flips -> Executor::run fires on Celeste's cmdbuf. Phase B: read the executor's decode/shader logs, implement ONLY the draw variants + shader emitters Celeste actually emits (2D MonoGame -> expect textured-quad DrawIndex + VS/PS; compute/geo/tess likely NOT pulled -> defer). Verify each step with the smoke loop; final frame needs a real PNG oracle (task-97/png-visual-oracle), not a log proxy.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Phase A DONE (2026-07-15). AC#1/#2/#3 complete; AC#4/#5 blocked UPSTREAM of the executor by
a CPU-lift gap (not a GNM issue). Nothing committed.

Handlers added (all real HLE, not dead stubs):
- Workload submit family (crates/libs/src/libscegnmdriver/submit.rs): `sceGnmSubmitCommandBuffersForWorkload`
  (NID zwY0YV91TTI's workload sibling) and `sceGnmSubmitAndFlipCommandBuffersForWorkload` — both
  read the 6 register args (workload id + the plain-submit arrays; AndFlip's trailing
  videoout/flip args are on the stack and ignored, same as the plain AndFlip handler) and
  funnel into the SAME `record_submit` -> `Executor::run` path as plain submit.
- Workload lifecycle (new crates/libs/src/libscegnmdriver/workload.rs): CreateWorkloadStream
  (writes a non-zero opaque id via out-ptr, returns 0), DestroyWorkloadStream, Begin/EndWorkload
  (Begin writes a non-zero workload id), DingDongForWorkload, RequestFlipAndSubmitDone{,ForWorkload}
  (the two RequestFlip* also signal equeue completion). Bookkeeping/no-op success per doc-6 Entry 1.
- Equeue completion (new crates/libs/src/libkernel/equeue.rs + rewritten libkernel/events.rs):
  sceGnmAddEqEvent/DeleteEqEvent/GetEqEventType track events in a process-global registry;
  sceGnmSubmitDone/RequestFlipAndSubmitDone signal a completion; sceKernelCreateEqueue now hands
  back a stable non-zero handle; sceKernelWaitEqueue drains one pending completion, reports a
  single triggered event (count to out-ptr), else keeps the 16ms VSync sleep. Instrumented.
- State-init + caps (crates/libs/src/libscegnmdriver/hwstate.rs): DrawInitToDefaultContextState{,400},
  DispatchInitDefaultHardwareState (size-returning); SetVgtControl/ResetVgtControl (0);
  ValidateOnSubmitEnabled/DisableDiagnostics{,2}/ResetState/GetVersion/GetDiagnostic{Info,s},
  GetDebugTimestamp (constant success/0).
- Boot-unblock (crates/libs/src/libkernel/mod.rs): sceKernelIsNeoMode -> 0 (base PS4). NOT GNM,
  but it was the immediate wall past AddEqEvent and blocked reaching any submit.
- Executor instrumentation (crates/gnm/src/exec.rs): `log_unhandled_opcode` logs each distinct
  decoded-but-unacted PM4 opcode once per submit (Draw mode) as "[GNM] unhandled PM4 opcode
  0xNN (NAME)" — the pull-driven seam. Verified non-fatal on the triangle regression.

Wall progression (interp backend, RUST_LOG=info):
  sceGnmAddEqEvent (was FATAL missing symbol) -> now handled
  -> sceKernelIsNeoMode (FATAL missing) -> stubbed
  -> guest CPU fault: UnknownInstruction `vextractps $0x2,%xmm0,0x2c(%rsp)`
     (bytes c4 e3 79 17 44 24 2c 02, VEX.128 map3 op 0x17) inside scePlayStation4 interop
     module +0x57ab (rip 0x15cb476), during managed-runtime setup.

CRITICAL: the fault is a CPU-lift gap in x86jit's AVX decode, UPSTREAM of Celeste's first GNM
submit — NO submit reached the executor, so Celeste's real command buffer is not yet observable
(AC#4/#5 can't be exercised until this clears). Filed x86jit TASK-168.6 (AVX: lift vextractps)
under the AVX epic. Per the x86jit-via-backlog rule, that lift is landed by the user there, not
edited here.

doc-6 entries appended: Entry 2 (does Celeste block on an equeue event? — registers type=64/id=0
but faults before any wait), Entry 3 (front door works; wall is now the vextractps CPU-lift gap,
not GNM; unhandled-opcode instrumentation proven safe on the triangle).

Status: build 0 errors; cargo test -p ps4-libs -p ps4-gnm green; clippy -p ps4-libs -p ps4-gnm
-D warnings clean; cargo fmt applied; examples/ps4-gcn-triangle NO REGRESSION (full
SetVsShader->SetPsShader->DrawIndexAuto->SubmitAndFlip->executor->SubmitDone path still renders +
flips, new unhandled-opcode logs are non-fatal).

NEXT (blocked): land x86jit TASK-168.6 (vextractps), rerun the smoke loop; when a Submit*ForWorkload
reaches the executor, read the "[GNM] unhandled PM4 opcode" + shader-set logs and implement ONLY
the draw variants + shader emitters Celeste emits (expect textured-quad DrawIndex + VS/PS). If a
compute/geo/tess op appears, STOP — that's a bigger decision, not Phase A/B.

---

Phase B session (2026-07-15). The x86jit AVX/div lifts (vextractps + div r/m8) are landed
(uncommitted rev pin in Cargo.toml/lock), so the boot reached Celeste's REAL graphics path.
Nothing committed this session either.

WALL PROGRESSION (interp backend, RUST_LOG=info):
  FATAL sceGnmSetPsShader350 [5uFKckiJYRM]  (the immediate wall)
  -> implemented -> guest marched: sceGnmSetVsShader -> sceGnmSetPsShader350 -> sceGnmDrawIndexAuto count=3
  -> FATAL sceNpTrophyCreateContext [XbkjbobZlCY]  (NOT GNM — achievement API boot-unblock)
  -> stubbed 7 trophy calls -> guest advanced ~150ms into a GUEST-SIDE FAULT inside the Mono
     managed runtime: UnmappedMemory (read) of 0x1d, `cmpb $0x0,(%rdi,%rdx)` at eboot.bin+0x104620,
     whole rbp backtrace inside eboot.bin, with `mono_os_mutex_lock: "Resource deadlock avoided"`.

FINAL WALL = a Mono-runtime null deref in the guest's own static code — a CPU/runtime wall, NOT
graphics/GNM/recompiler/Vulkan. This is the STOP point per the task's stop rules.

HANDLERS ADDED (all REAL emitters/stubs, not dead):
- crates/libs/src/libscegnmdriver/shader_bind.rs:
  * sceGnmSetPsShader350 [5uFKckiJYRM] — SDK-3.50 pixel-shader bind. GnmDriver.h confirms its
    signature is IDENTICAL to the base sceGnmSetPsShader (cmd, numdwords, psregs; numdwords=40),
    so "350" is an ABI-version tag, NOT a modifier/extra arg. Reuses emit::set_ps_shader verbatim.
  * sceGnmUpdatePsShader350 [mLVL7N7BVBg], sceGnmUpdateVsShader [V31V01UiScY] — likely siblings;
    on the register-route model Update == Set (both re-write SPI_SHADER_PGM_* PM4), share the emitter.
  * New test set_ps_shader350_matches_base_set_ps_shader: asserts the 350 emit is BYTE-IDENTICAL to
    the base emit and round-trips to a GcnBinary PS bind.
  * The 3 GNM NIDs added to the key_gnm_nids_resolve_to_registered_handlers resolution test (mod.rs).
- crates/libs/src/libscenptrophy.rs (NEW module) + LIB_SCE_NP_TROPHY in libs.rs + `pub mod` in lib.rs:
  7 boot-unblock stubs (Create/Destroy{Context,Handle}, RegisterContext, GetTrophyUnlockState,
  UnlockTrophy). Create* hand back a non-zero opaque id via out-ptr (guarded by is_guest_ptr); the
  rest are benign success. NOT GNM — same class as Phase-A's sceKernelIsNeoMode unblock. Signatures
  from data/oo_sdk/include/orbis/NpTrophy.h.

AC#4/#5 STILL UNEXERCISED: Celeste's SetVsShader/SetPsShader350/DrawIndexAuto all fire BEFORE its
first frame submit; the HLE sceGnmDrawIndexAuto records into the driver but NO Submit*/Executor::run
fired before the Mono fault. So the executor has still not run on a Celeste-built command buffer.
Reaching AC#4/#5 now depends on Mono runtime bring-up (CPU/runtime), NOT on any further GNM emitter
or decode arm. The [GNM] unhandled-opcode instrumentation stays armed for when a submit is reached.

VISUAL TRUTH: no frame reached the executor, so nothing was rendered — no PNG to dump, nothing to
verify. (Triangle regression still renders+flips; see below.)

STATUS: build 0 errors (release -p unemups4); cargo test -p ps4-libs -p ps4-gnm green (incl. the new
350 test + NID resolution); clippy -p ps4-libs -p ps4-gnm --all-targets -D warnings clean; cargo fmt
applied. REGRESSION: examples/ps4-gcn-triangle renders+flips on BOTH interp AND jit
(SetVsShader->SetPsShader->DrawIndexAuto->SubmitAndFlip->executor decoded [unhandled 0x28/0x10
non-fatal]->SubmitDone->zero-copy framebuffer imported) — no regression from the shared shader-emit
path. doc-6 Entry 4 appended.

NEXT (blocked on Mono, out of GNM scope): the Celeste boot now needs the Mono managed-runtime wall
(null deref at eboot.bin+0x104620, mutex "deadlock avoided") cleared before any frame submits. That
is a CPU/runtime track, not GNM. When the boot reaches a Submit*, rerun and read the [GNM] unhandled
PM4 opcode + shader-set logs to implement ONLY Celeste's real draw/shader mix (still expect
textured-quad DrawIndex + VS/PS; compute/geo/tess = STOP).

--- Mono-runtime mutex wall (2026-07-15, uncommitted) — RUNTIME TRACK, not GNM ---
Cleared the "Resource deadlock avoided" half of the wall above. Root cause: HLE mutex modeled
only a bool `is_recursive`, so every non-recursive (NORMAL) mutex behaved as ERRORCHECK and
returned EDEADLK(11) on owner self-relock. Mono's `mono_os_mutex` is a NORMAL mutex (init'd with
null attr — confirmed by logging) and treats EDEADLK as fatal; a NORMAL mutex must NEVER return
EDEADLK (only ERRORCHECK does). Per-handle lock/unlock/trylock logging showed Mono fast-path
`trylock`→`unlock` many times then a blocking `pthread_mutex_lock` on the still-held handle →
spurious EDEADLK → "deadlock avoided" → SIGILL in TLS teardown. Fix (smallest correct): model
the 3 POSIX/Orbis mutex types (Normal/ErrorCheck/Recursive) via a new `ps4_core::kernel::MutexType`
enum; read the real type from the init attr (ERRORCHECK=1, RECURSIVE=2, else NORMAL; null=NORMAL);
only ErrorCheck returns EDEADLK on self-relock, Recursive+Normal count up. Files: crates/kernel/src/sync.rs
(HostMutex.mtype, mutex_lock/timedlock/trylock), crates/libs/src/libkernel/pthread.rs
(scePthreadMutexInit reads type), crates/core/src/kernel.rs (MutexType + trait sig), crates/kernel/src/bridge.rs.
Verified: EDEADLK + SIGILL GONE and DETERMINISTIC on interp AND jit (3 runs each). Regression:
examples/ps4-thread-testing all 7 tests pass (Mutex/Recursive/RWLock/TryLock incl.), ps4-tls passes,
cargo test 0 failed, clippy clean on ps4-kernel/ps4-libs/ps4-core, fmt applied. doc-5 Case 13 +
doc-4 taxonomy row appended. NOT COMMITTED.

REMAINING WALL (now the sole blocker, deterministic): `UnmappedMemory (read) of 0x1d` at
rip eboot.bin+0x105620 — instr `cmpb $0x0,(%rdi,%rdx)` in a byte-scan loop where
`rdi = *(rax+0x10)` is near-null (loop `movb $0x0,(rdi,rdx); inc rdx; cmp r8,rdx; jae; rdi=*(rax+0x10); cmpb`).
This is a Mono function reading a bad pointer from a struct field at +0x10 — a runtime/data
init gap, NOT the mutex and NOT GNM. Fires right after Celeste records SetVsShader/SetPsShader350/
DrawIndexAuto count=3 but BEFORE any Submit* → executor still hasn't run on Celeste's cmdbuf
(AC#4/#5 still unexercised, unchanged). Next: characterize this deref (dump rax/rdi at fault,
disassemble the enclosing Mono fn) on the runtime track.

--- Update 2026-07-15 (0x1d wall CLEARED — Mono/memory concern, not the executor) ---
The `0x1d` deref was NOT a data-init gap — it was SGen's **Large Object Space** section math
biting an alignment we ignored. Disasm of the enclosing routine: it derives the LOS section from
a chunk pointer via `section = chunk & ~0xfffff` (round down to 1 MB), reads
`section->free_chunk_map` at `+0x10`, byte-scans it; assertion strings named it exactly
(`mono/sgen/sgen-los.c`, `section->free_chunk_map[i]`, line 189). LOS sections are 1 MB-aligned
by contract, so that mask only works if each section base is 1 MB-aligned.
CONFIRM (one-line-per-alloc logging on find_free_region / AllocateDirectMemory / map_flexible):
the alloc right before the fault was `sceKernelAllocateDirectMemory length=0x100000
alignment=0x100000` (1 MB, 1 MB-align requested) → returned 0x4cee1c000, `& 0xfffff = 0x1c000`
(NOT 1 MB-aligned). SGen computed section=0x4cee00000, read a neighbour's field → 0x1d → faulted.
Root cause: HLE dropped the guest's alignment. `sceKernelAllocateDirectMemory`/`MapDirectMemory`
ignored their `_alignment` arg; `VmMemoryManager::find_free_region` aligned only to 16 KB and
bumped `heap_cursor` by raw size (drifts off the 1 MB grid).
FIX (smallest correct — thread alignment through, honor it): added `map_aligned` +
`find_free_region_aligned` to `VirtualMemoryManager` (defaults ignore align → delegate, so the
many test/mock stubs are untouched), overridden in `VmMemoryManager` to round the
allocate-anywhere base up to `max(requested_align, 0x4000)`; added `mmap_aligned` to
`KernelInterface`+`Process` (default → plain mmap); passed `alignment` through from
`sceKernelAllocateDirectMemory`. `MapDirectMemory` needs nothing (in this HLE it only echoes the
already-placed aligned address). Files: crates/core/src/memory.rs, crates/core/src/kernel.rs,
crates/memory/src/vm_backend.rs, crates/kernel/src/process.rs, crates/kernel/src/bridge.rs,
crates/libs/src/libkernel/mman.rs.
VERIFIED: `0x1d` fault GONE on interp AND jit (fault count 0). Boot advances through TLS/mutex
init and into FS enumeration. Regression: examples/ps4-mmap all [PASS], ps4-fs PASSED
(integrity + writev), ps4-gcn-triangle no faults/errors; cargo test -p ps4-memory -p ps4-kernel
0 failed; clippy -D warnings clean on ps4-memory/ps4-kernel/ps4-core/ps4-libs; fmt applied.
doc-5 Case 14 + doc-4 taxonomy row appended. NOT COMMITTED.
GNM note (AC#4/#5): before the new wall Celeste fires real driver draws
`sceGnmSetVsShader` / `sceGnmSetPsShader350` / `sceGnmDrawIndexAuto count=3`, but STILL no
`sceGnmSubmit*` → the command-buffer executor has NOT yet run on Celeste's cmdbuf; AC#4/#5 remain
unexercised. Per the png-visual-oracle rule NO frame-correctness claim is made — those draws are
logged immediate-mode entrypoints, not a verified rendered frame.
NEW WALL (now the sole blocker, deterministic on interp): missing symbol `sceKernelGetdents`
[NID j2AIqSqJP0w] — Celeste `sceKernelOpen('/app0/Content/Tutorials', O_DIRECTORY)` succeeds then
calls Getdents to enumerate the directory; we stub it as missing → FATAL. This is a directory-read
HLE gap (implement `sceKernelGetdents` against the host FS), NOT a GNM/executor concern. (jit
reaches the same Getdents wall; a secondary SIGILL at 0x982e26 on a worker thread there is a
separate jit-lift matter — interp is the clean-fault reference.)

--- AC#4 EXERCISED: draws bridged into the executor; wall is now the GCN recompiler (uncommitted) ---
The boot now reaches a full render loop (~973 frames of Set{Vs,Ps}Shader + Draw{Auto,Offset} +
SubmitAndFlip + SubmitDone), so `Executor::run` fires per submit on Celeste's real cmdbuf.

DIAGNOSIS (PM4 trace + ps4_libs=info): the submitted DCBs carry ONLY state + shader-set PM4
(7612 SET_CONTEXT_REG, 930 SET_SH_REG, NOP/DMA_DATA/SET_UCONFIG_REG) — **ZERO draw packets** —
while the guest calls `sceGnmDrawIndexOffset` 3839× and `sceGnmDrawIndexAuto` 974× through the
HLE entrypoints. Root cause: the HLE draw builders were record-only no-ops (`let _ = …`); on real
HW `sceGnmDraw*` are gnmx builders that WRITE the draw PM4 into the caller's cmdbuf (our shader-set
builders already do this — that's why SET_SH_REG is present). An HLE-linked guest never hand-emits
the draw, so the DCB had binds but no draws → executor decoded state, resolved nothing → blank frame.
BEFORE PNG = pure white (verified).

FIX (smallest correct, matches the existing shader-set architecture, uncommitted):
- crates/gnm/src/pm4/emit.rs: `draw_index_auto` (→ IT_DRAW_INDEX_AUTO 0x2D) + `draw_index_offset`
  (→ IT_DRAW_INDEX_OFFSET_2 0x35) PM4 emitters (+ 2 round-trip tests).
- crates/libs/src/libscegnmdriver/draw.rs: `sceGnmDrawIndexAuto`/`sceGnmDrawIndexOffset` now emit
  their packet into the caller's `cmdbuf` via `emit_draw_into_cmdbuf` (mirrors shader_bind's
  `emit_into_cmdbuf`: null/undersized = clean no-op), IN STREAM ORDER with the surrounding binds
  (SpriteBatch interleaves bind→draw per group — a side-channel shadow replay would collapse all
  draws onto the last bound shader, so the draw MUST land in the cmdbuf).
- crates/gnm/src/exec.rs: added the `IT_DRAW_INDEX_OFFSET_2` (0x35) decode arm
  `dispatch_draw_index_offset` (index base/type from bound IT_INDEX_BASE/IT_INDEX_TYPE, pulls the
  offset sub-range) + an end-to-end test.

RESULT: AC#4 exercised — the executor's draw arms now fire on Celeste's real geometry
(DrawIndexAuto count=3, DrawIndexOffset count=6/300/…). Every draw defers at ONE recognized point:
`ShaderPairResolution::NeedsGcn` ("bound to a non-recompilable (.sb GCN) shader — deferring draw").
Celeste's VS/PS resolve to real .sb GCN addresses via the SH route, but the GCN→SPIR-V recompiler
can't compile THESE shaders yet. Clean defer, not a crash. AFTER PNG still white (draws defer).

AC#5 WALL CHARACTERIZED (png oracle, not log proxy): the next concrete wall past a running executor
is the **GCN recompiler on Celeste's real shader bytecode** — GPU-track work (tasks 55/58/47/72),
NOT a submit/decode/draw-arm gap. The `NeedsGcn` defer log is the pull-driven seam naming which
shader class must compile next. NO REGRESSION: ps4-gcn-triangle still renders the pink triangle
(builder emit is overwritten byte-identically by the corpus's hand-emit); cargo test -p ps4-gnm
(169) -p ps4-libs (16) green incl. 3 new tests; clippy -D warnings clean; fmt applied. doc-6
Entry 5 appended. NOT COMMITTED.

--- GPU-TIER wiring: fetch-call resolution + const-buffer bind (task-113.4.1.1, worktree branch) ---
Wired the two integration gaps so Celeste's 5 real fetch-call VS (sh05/07/10/19/21, doc-6 Entry 9)
recompile through the PROVIDER + bind, instead of deferring at NeedsGcn. The GCN recompiler already
handled the resolved instruction set (fetch-call inline + s_buffer_load_dwordx16 landed on main); this
task connected it to the ps4-gnm provider/executor + the ps4-gpu backend. Worktree branch only; NOT
merged, retail run NOT executed here (no assets in this env) — orchestrator merges + runs Celeste+PNG.

GAP 1 — resolve the fetch call before recompiling (crates/gnm/src/shader/gcn.rs, resolve_gcn):
- The fetch-shader pointer lives in VS user-SGPR s[0:1], which the ShaderProvider trait cannot read
  (no GpuState). Sourced it in GpuState::gcn_ref_from_regs (crates/gnm/src/state.rs) — reads s[0:1]
  from the SH bank for the VS stage — onto a new GcnResources.fetch_addr (crates/gnm/src/shader/
  source.rs). It does NOT feed the shader-identity hash (derive.rs hashes only addr), so no re-key.
- resolve_gcn now takes fetch_addr: after decode_all, if has_fetch_call(&insts) it reads the fetch
  body through the SAME bounded seam (new read_fetch_code: grows a window from 8→256 dwords until it
  captures the s_setpc return, so it works for both a tiny code blob and a fetch shader near a mapping
  boundary), decode_all + resolve_fetch_call to splice inline, then recompiles the resolved stream.
  Strict-or-defer: no fetch addr / unreadable window / FetchResolveError = clean ShaderUnsupported +
  warn!, never a partial recompile. The inlined idxen MUBUF lowers to the existing SSBO vertex-pull
  (io.buffers), which derive_vertex_buffers already binds via s[2:3] — the vertex path is reused.

GAP 2 — bind the constant buffer (set0/bind2 SSBO):
- Recompiler emits IoLayout.const_buffers (ConstBufferBinding{set:0,binding:2,size_dwords}) for the
  s_buffer_load matrix load. Exported ConstBufferBinding from ps4-gcn's lib.rs.
- ps4-core: CreatePipeline gained a `const_storage: Option<StorageBinding>` field and a new
  BindConstBuffer{set,binding,id} BackendCmd (no num_records — a flat uint[]).
- ps4-gpu backend: create_host_pipeline adds a 2nd VERTEX-stage STORAGE_BUFFER descriptor for
  const_storage (distinct from the vertex-pull binding), both in set-0; record_draw_list allocates a
  ConstBind, sizes the pool for storage_count = vertex-pull + CB, and writes the CB descriptor.
- ps4-gnm exec (setup_draw): a VS declaring const_buffers resolves its CB V# via derive_const_buffer —
  the V# is INLINE in user-SGPRs s[4:7] (CONST_BUFFER_SBASE_SGPR, RE'd retail SBASE), decoded directly
  with decode_v_sharp (no memory read for the descriptor); a null V# defers the WHOLE draw (same
  discipline as the sampler path — never a pipeline with an unbound descriptor). Then const_storage is
  threaded onto CreatePipeline and a BindConstBuffer pulls the CB bytes through the resource cache
  (ResLayout::ConstBuf, upload-on-use + dirty-invalidate).

TESTS (all green; LD_LIBRARY_PATH=/usr/lib): +5 new.
- gcn.rs: fetch_call_vs_resolves_through_provider (inline_fetch_vs.sb caller + fetch_pos_vs.code.bin
  callee, two-region bounded seam → HostShader with non-empty io.buffers), plus two strict-or-defer
  tests (no fetch pointer; unreadable fetch → clean defer, recompile_count 0).
- exec.rs: gcn_const_buffer_vs_binds_const_ssbo (cbuffer16_vs → CreatePipeline{const_storage:
  Some(set0/bind2)} + BindConstBuffer + CB create/upload of 64 bytes under the bound id) and
  gcn_const_buffer_vs_defers_when_vsharp_null (null s[4:7] V# → whole draw defers, empty command list).
- cargo test --workspace: 366 passed / 5 ignored. clippy -p ps4-gnm -p ps4-gpu -p ps4-core -p ps4-gcn
  --all-targets -D warnings clean (fixed a pre-existing needless_range_loop in diff_harness.rs that
  blocked the ps4-gpu gate). cargo fmt applied. doc-6 Entry 10 appended.

WHAT THE ORCHESTRATOR SHOULD SEE in the real Celeste run + PNG:
- The 5 fetch-call VS (sh05/07/10/19/21) should now RECOMPILE (no more "NeedsGcn"/"fetch shader
  s_swappc" defer) provided the driver preloads the fetch pointer into s[0:1] and the CB V# into
  s[4:7] as RE'd. Their PS partner must also recompile for a draw to land (the PS frontier —
  sh01/sh03's VCC-carry/m0 walls — is separate and out of scope here).
- Draws whose VS is a fetch-call VS AND whose PS recompiles should stop deferring and produce
  geometry; the first non-white pixels are the signal. A draw still defers cleanly if: the fetch
  pointer/CB V# is not where RE'd (s[0:1]/s[4:7]) — watch for "fetch shader ... no pointer in s[0:1]"
  or "constant-buffer V# ... null/unbound" warns, which pull-drive the ABI correction; or the PS is
  still a non-recompilable class. STILL a clean defer everywhere — never a crash. NOT COMMITTED to
  main; retail run pending on the orchestrator.

--- RETAIL RUN 2026-07-15 (provider wiring merged @<prior-history>; smoke-loop, PNG oracle) ---
Ran eboot with UNEMUPS4_DUMP_PNG. RESULT: provider wiring WORKS — the executor now REACHES draws and
BUILDS pipelines (distinct real vs_hash/ps_hash, vertex_layout: None = SSBO-pull). The VS recompile
end-to-end (fetch-call resolution + CB bind landed); draws no longer defer at NeedsGcn. This is the
wall the whole session was aimed at — cleared.
FRAME STILL WHITE, three named next walls (smoke-loop):
1. GUEST-SIDE GATE (dominant): Celeste renders ONE boot frame (~9 draws) then idles, polling
   sceUserServiceGetEvent → 0x80960009 (SCE_USER_SERVICE_ERROR_NO_EVENT, the normal "no event" return).
   It is waiting for an INITIAL-USER / login event that we never deliver, so it never advances past the
   boot/splash frame. Only 9 draws/60s. Unblocking = inject the initial-user login event (input/event
   plumbing, guest-side — NOT graphics). This gates everything downstream.
2. PS TEXTURE T#/S# = MemoryFault: the blend-enabled TEXTURED composite/sprite draws (the ones that put
   pixels on screen in a MonoGame/FNA compositor) all defer — "PS declares a sampler but the T#/S# did
   not resolve: MemoryFault". The T#/S# either sits at a different user-SGPR slot than assumed, OR points
   at an offscreen render target not aliased as a sampleable texture (task-56 RT-as-texture). Clean defer.
3. The 6 geometry draws (blend off, depth on, target 1920x1080) build a pipeline and do NOT defer, yet
   the frame is white — so either they rasterize nothing visible (recompiled-VS vertex-fetch output /
   degenerate positions — correctness, GPU-tier diff_harness territory) or render to a target not
   presented. Needs RT inspection.
Also: INTERMITTENT SIGSEGV on some runs (1 of 3 crashed ~15s; others survived 60s to timeout) — a
shutdown/present race, low priority; every guest-side unresolvable case is still a clean defer, not a
crash. NEXT (decision): (a) guest-side initial-user event injection to get past the boot gate, then
(b) PS texture / RT-as-texture path (task-56) for the compositor draws, then (c) verify geometry-draw
correctness. Provider merge committed @<prior-history>; workspace + clippy green.

--- SMOKE-LOOP SESSION 2026-07-15/16 — guest boots through init, AVX cleared, reaches render path ---
Ran the eboot repeatedly under UNEMUPS4_DUMP_PNG, clearing walls one at a time (doc-4 method). The
guest advanced from a single ~9-draw boot frame (idle on a user-event gate) to a FULL boot: it now
runs its Mono/FNA/MonoGame init to completion and reaches the GNM executor / PM4 render path,
submitting real 4 MB command buffers. Walls cleared, in order:

LIBS (mine, committed on main):
- 591e9be: sceUserServiceGetEvent now delivers one initial-user LOGIN event (the title blocked at
  boot polling for it) + sceMouseOpen stub. THIS unblocked everything downstream.
- ed2f35e: sceKernelClockGettime name registered on the existing clock_gettime handler.
- e1197ed: sceKernelGetProcessTimeCounter[Frequency] + sceKernelGetProcessTime (host-monotonic).

X86JIT (delegated to opus per the rule; work lives in /home/mikolaj/src/x86jit as a STACK of 4
UNMERGED branches, each with full Unicorn+native-AVX differential tests + backlog task; maintainer
must review/merge, then re-pin unemups4). The stack (parent→child):
  02b60cb (current unemups4 pin)
   → bafd5ee  feat/vex-vinsertps      (x86jit task-255): VEX vinsertps
   → 8f3ece1  feat/vex-float-cluster  (task-256): vblendv m128 + blendps/pd, dp*
   → ed4f98d  feat/vex-float-sweep    (task-257): 128-bit VEX rsqrt/rcp/sqrt/shuf/unpck (16 mnem)
   → 977f253  feat/vex-ymm-float-sweep(task-258): 256-bit YMM converts/arith/sqrt/shuf/unpck (23 forms)
  Each was the next UnknownInstruction the guest hit (Mono/FNA is pervasively AVX incl. 256-bit).
  CONSOLIDATED: the linear stack was fast-forwarded onto x86jit main (head fb25468, all 4 tasks
  Done, session branches deleted); unemups4 Cargo.toml is pinned to fb25468 (both x86jit-core AND
  x86jit-cranelift, commit 7bb16ff; ps4-cpu + ps4-helloworld no-regression verified). x86jit local
  main is ~6 ahead of origin/main — push the x86jit mirror if wanted (unemups4 uses the local
  file:// dep, so not required).

CURRENT WALL (the one to fix next): with AVX cleared, a run reaches the executor and then hits an
INTERMITTENT SIGSEGV (~1 of 2 runs; exit 139, NO clean guest-fault/UnknownInstruction/missing-symbol
diagnostic). Signature: crashes on the main render thread right after the executor logs the first
submit's `unhandled PM4 opcode 0x13 (IT_INDEX_BUFFER_SIZE)`. Debug-logged runs (RUST_LOG=ps4_gnm=debug)
SURVIVE 60s — so it is TIMING-SENSITIVE = a RACE, not a deterministic bad pointer. Hypothesis: the
NEW provider draw path (fetch-call resolution + CB/vertex V# resolve added in ab7eb3b) reads a
register-derived guest descriptor through the UNBOUNDED IdentityMem (get_host_ptr returns Some for
every addr), so a garbage/racing V# base dereferences bad host memory → raw segv instead of a clean
MemoryFault defer; and/or shared provider state (GcnResources.fetch_addr / CB binding / resource
cache) is mutated across the multiple guest submit threads without sync. NEXT: (1) guard the CB and
vertex V# resolve on the executor path to go through the BOUNDED seam (or validate the base against
the VMA set) so a bad descriptor DEFERS cleanly, never segfaults; (2) audit the new provider state
for a cross-thread race; (3) re-run — the run that survives should present, exposing the earlier
PS-texture/RT wall (compositor draws deferring on T#/S# MemoryFault, task-56) as the wall to actual
pixels. Repro: pin x86jit to 977f253, build --release, run eboot, ~half the runs crash.

STILL WHITE frame: even a surviving run presents white because the pixel-producing compositor draws
(blend-on, textured) defer on PS T#/S# MemoryFault; the geometry draws (blend-off, depth) build
pipelines but show nothing visible. Both are downstream of fixing the crash.

--- CRASH ROOT-CAUSED 2026-07-16 (worktree branch worktree-agent-a63bc94fa7be9716d, NOT merged) ---
The intermittent "executor SIGSEGV" is NOT our bounded-seam/race hypothesis. It is a RADV **ACO
shader-compiler crash** inside vkCreateGraphicsPipelines, on Celeste's recompiled VS/PS SPIR-V.
Proof chain: (1) gdb bt of the fault (caught first try) is inside libvulkan_radeon.so ←
ash::create_graphics_pipelines ← ps4_gpu::backend::create_host_pipeline ← run_command_list, on the
DISPLAY thread — not the submit thread, nowhere near a descriptor decode. (2) Dumped the exact VS/PS
SPIR-V; BOTH pass `spirv-val`. (3) `RADV_DEBUG=llvm` (LLVM backend, not the default ACO) → crash
VANISHES on every run that reaches the executor. spirv-val-clean + RADV-LLVM-clean, only RADV-ACO
segfaults ⇒ valid module, ACO compiler bug, heap/ASLR-sensitive (hence intermittent + masked by
logging). The recompiled VS reads a Function `uint` (%14) BEFORE initializing it (undefined-value
read — this VS has no vertex fetch to populate that VGPR); legal SPIR-V but the kind of degenerate
module that stresses a compiler optimizer.

HYPOTHESIS DISPROVEN (by audit, not just absence-of-repro): (a) the bounded seam is ALREADY watertight
on the exec draw path — every register-derived descriptor decode routes through bounded_read()
(derive_vertex_buffers / derive_texture_binding) or reads the V# straight from SGPRs (derive_const_buffer,
no memory read), and every guest-byte upload routes through BoundedMem::read_bytes_ranged; a bad V# is a
clean Err → whole-draw defer, never an unbounded host deref. (b) no cross-thread race: all submit threads
serialize through the single `static DRIVER: Mutex<GnmDriver>` (record_submit holds the lock across
Executor::run), and the submit→display RunCommandList(cmds, signal) hand-off is SYNCHRONOUS (submit blocks
on signal.recv()), SPIR-V crossing as an immutable Arc<[u32]>.

LANDED THIS SESSION (strictly-safer, in-family with the task): the ONE residual UNBOUNDED guest access on
the executor path — `write_label` (crates/gnm/src/exec.rs), the EOP/EOS label store whose address is
packet-derived and untrusted — now range-validates the 8-byte label slot against the bounded VMA set
(bounded_read().read_ranged) BEFORE the identity write. A bad label address is a clean no-op (a waiter
stalls, visibly) instead of an unbounded host over-write. Tests: the two EOP/EOS write-assertion tests now
wire a scoped bounded seam over the label slot; new test `eop_label_to_unmapped_address_is_a_clean_noop`
proves a write to an UNmapped address is skipped (independent lever: the seam maps a DIFFERENT region).
cargo test -p ps4-gnm 181 passed; clippy -p ps4-gnm --all-targets clean; fmt applied. Triangle example
(examples/ps4-gcn-triangle) no regression — reaches executor, EOP labels pass the guard (mapped), presents,
no crash.

WORKTREE HYGIENE NOTE (cost the session an hour — record for next agent): this worktree branched at 5cf0840,
LONG before the ab7eb3b provider merge + all retail work. It had a STALE Cargo.toml/Cargo.lock pinning
x86jit at 3a51293a (pre-AVX-consolidation). Running that binary hit a DETERMINISTIC Mono NULL-deref
(`filename != NULL`, gpath.c) — an AVX-lift gap masquerading as a boot regression — and never reached the
executor. FIX = `git merge main` INTO the worktree branch (from the worktree dir, `git -C <worktree>`),
which brought Cargo.toml to fb25468; rebuild. Also symlink the gitignored `data/oo_sdk` into the worktree
(SDK metadata; DO NOT stage it). After that the worktree binary behaves identically to the shared checkout.

NEXT (unchanged priority, now with a sharper wall): the RADV-ACO crash is a GPU-DRIVER-tier /
recompiler-correctness wall — upstream of "first pixel", downstream of everything this task owns.
Workaround while it stands: `RADV_DEBUG=llvm`. Real fixes (OUT of this task's scope, task-56/GCN tier):
(1) make the recompiler not emit uninitialized-VGPR reads; (2) file the ACO crash upstream against a
reduced module. THEN the earlier walls resurface: (a) the PS T#/S# MemoryFault defer on the textured
compositor draws (task-56 RT-as-texture), (b) geometry-draw correctness (white frame). doc-6 Entry 11,
doc-5 Case 17, doc-4 taxonomy row appended this session. NOT COMMITTED to main (orchestrator reviews+merges).
<!-- SECTION:NOTES:END -->
