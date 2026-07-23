---
id: TASK-227
title: >-
  gpu/cpu: dirty-watching the vertex ring costs 298M write-barrier helper calls
  per 10s for cache hits that never happen
status: Done
assignee: []
created_date: '2026-07-22 12:14'
updated_date: '2026-07-22 14:03'
labels:
  - gpu
  - cpu
  - perf
dependencies: []
priority: high
ordinal: 232000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
x86jit task-283 added helper-call counting. The result names the dominant cost in the whole emulator, and it is ours:

    helper calls this window: note_watched_write_helper 298320014
                              div_helper                  1207244
                              bmi_helper                   709459
                              pcmpstr_helper / string_helper  (thousands)

note_watched_write_helper outweighs everything else by about 250x. That helper is the dirty-tracking WRITE BARRIER: every guest store into a watched range leaves compiled code and calls into Rust. About 30 million calls per second. Even at 20-30 ns per round trip that is 60-90% of a core spent on the barrier alone.

This explains the measurement that has resisted every fix so far. task-220 established roughly 30 host cycles per guest instruction (129-145 MIPS) and concluded the cost was per-instruction, hence in the lift. It is per-STORE, and it is a call the Cranelift mid-end cannot remove because it is a genuine call out of the compiled block. That is also why opt_level=Speed measured as no change, why the IBTC probe measured as no change, and why superblocks gave only 5-8%.

For calibration, x86jit's own workloads run at 0.00 helper calls per kinstr on synthetics, 3.35 on sqlite, 2.55 on lua. Helper traffic is not inherent to the engine — this is ours.

WHERE IT COMES FROM: crates/gnm/src/cache/mod.rs calls dirty.watch(key.addr, key.size) for every cache entry, buffers (:735) and textures (:845) alike. Celeste's dynamic geometry lives in a ring the guest writes to continuously, and under a 64 MiB LRU budget many overlapping entries watch it at once.

WHY MOST OF IT IS PURE WASTE: task-223 established that the ring's cache key is unique BY CONSTRUCTION — the V# base is the write cursor and num_records spans cursor to end-of-ring, so addr and size move together. Measured there: 100% of vertex misses are new_base, 0% new_size, 0% recreate. Those entries can never be hit, so the dirty information collected about them can never prevent an upload. We are paying the barrier on every write into the ring to learn something we then never use.

Textures are a different case and must be treated separately: they ARE re-uploaded only when the guest changes them, so dirty tracking there earns its keep.

Work:
- confirm the split first: which watched ranges actually produce a cache hit that dirty state saved, and which never do. The counters in ResourceCache already distinguish miss reasons; extend that to attribute barrier traffic to the ranges causing it if it is not obvious.
- stop watching ranges whose entries cannot hit. For the vertex ring the upload happens every batch regardless.
- consider whether watching can be coarser or deduplicated where it is still needed — overlapping entries covering the same pages multiply the cost.
- measure helper calls per kinstr, guest_exec per frame and fps before and after. Note the per-kinstr figure is unreliable in boot and menu windows, where icount undercounts because it only sees compiled code; use gameplay windows.

Correctness caution: dirty tracking is what makes a cached texture notice a guest overwrite. Removing a watch where a hit IS possible reintroduces stale-content rendering — exactly the class of bug the maintainer caught by eye during task-223, which no test detected.

SCOPE: DO THE CHEAPEST THING ONLY. The options below were reviewed together and deliberately ordered; the decision is to implement (1) and stop there. Do not build (2)-(6) pre-emptively — there is no evidence yet that (1) is insufficient, and a self-tuning policy or a page-protection mechanism designed against a single title is exactly the kind of premature generalisation this project avoids while only one game runs.

When watching earns its keep, stated as an invariant: watch pays when writes are RARE relative to the cache hits it saves. Cost scales with writes into the range; benefit scales with uploads avoided.

  read-mostly, big        texture atlases (MiB, guest almost never writes)  ideal - large benefit, near-zero cost
  read-mostly             shader code ranges (GcnShaderProvider)            ideal
  occasional writes       long-lived constant buffers                       worthwhile
  WRITE-MOSTLY            the vertex ring                                   pure loss - every store pays, zero hits

The failure is not that watching is wrong, it is that one policy is applied to two opposite profiles.

OPTIONS CONSIDERED, cheapest first:

1. DO NOT WATCH WHAT CANNOT HIT. Per-layout policy. Cheapest, largest win, lowest risk — and it has a proven fallback, see the history note below. THIS IS THE ONE TO IMPLEMENT.

2. Adaptive per entry. Start unwatched and re-upload; promote an entry to watched after N hits without it being dirty (read-mostly, tracking will pay); demote one that is dirty on nearly every check (write-mostly, the barrier is pure cost). Self-tuning, so the next title needs no guessing — which matters for Bloodborne, whose profile will differ. Needs hysteresis to avoid flapping. NOT NOW.

3. Refcounted page dedup, so overlapping entries covering the same pages do not multiply bookkeeping. Does not reduce per-write cost. NOT NOW.

4. Change the MECHANISM rather than the policy: host page protection (mprotect read-only, SIGSEGV on first write, unprotect and mark dirty). Cost drops from one call per store to one fault per page per frame — for a densely written ring, tens of faults instead of millions of calls. Classic emulator technique, but it needs signal-handler machinery, interacts with the identity-mapped arena, and is an x86jit capability question rather than ours. NOT NOW.

5. Compare-on-use: hash the guest range when the cache is consulted instead of tracking writes. Moves cost from per-write to per-lookup-over-N-bytes. Bad for a 4 MiB texture, possibly fine for a small constant buffer. NOT NOW.

6. USE THE SIGNALS THE GUEST ALREADY GIVES US — the principled long-term answer. Real hardware cannot see CPU writes without explicit cache management either, so a PS4 title MUST tell the GPU when data is ready: GNM carries flush/invalidate packets, and the submit boundary itself is such a signal. If the guest says it wrote, we need not detect it. The console scrape (task-168) is the oracle for which packets carry it. Revisit when the GNM cache-management packets are understood; not before.

HISTORY THAT DE-RISKS OPTION 1: dirty tracking silently did nothing for a long time. watch_range no-opped above x86jit's 4 GiB watched-page window while our GPU buffers live around 41 GiB, so take_dirty returned empty on 3709 of 3709 submits (crates/libs/src/libscegnmdriver/submit.rs:318, crates/gnm/src/cache/mod.rs:619). The cache used force-re-upload instead and the title rendered. Fixed only in x86jit 873563f. So the emulator demonstrably works without watching these ranges, and the barrier cost is recent.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 the split between watched ranges that can produce a dirty-saved cache hit and those that cannot is established from the counters, not assumed
- [x] #2 ranges whose entries cannot hit are no longer watched; textures and anything that can hit keep their tracking
- [x] #3 measured before/after in gameplay: note_watched_write_helper per kinstr, guest_exec per frame, fps
- [x] #4 no stale-content regression: maintainer confirms textures and the 3D mountain still render correctly, and the BUFCACHE STALE-HIT counter stays at zero
- [x] #5 build + clippy clean, cargo test --workspace green
<!-- AC:END -->





## Notes

### 2026-07-22 — option 1 implemented, and it measures NEUTRAL. The premise is wrong.

**AC #1 (the split, from counters).** Gameplay window, `gets/flip` = clean + dirty + create:

```
vertex   57.9 =  0.0 clean + 35.7 dirty + 22.1 create (665 KiB)
const    39.0 =  0.0 clean + 37.0 dirty +  1.9 create
index    19.9 = 15.6 clean +  0.0 dirty +  4.2 create
texture   7.6 =  7.6 clean +  0.0 dirty +  0.0 create
rt       39.2 = 39.2 clean                      (never watched)
```

A clean hit is the only thing dirty state can buy. Vertex and const never reach one, so
watching them is pure cost — but the counters also OVERTURN this task's own guess: index
buffers are the opposite profile (15.6 clean/flip, zero dirty; sprite batching indexes quads
through one static buffer). Implemented accordingly: `watch_pays()` excludes VertexBuf and
ConstBuf, keeps IndexBuf, Texture, RenderTarget. Unwatched layouts never take the clean-hit
path — they re-upload unconditionally, which is the correctness pairing.

**AC #3 (before/after, gameplay).** No change:

```
                     helper calls /10s window    fps            guest_exec/frame
baseline             379M / 388M                 36.2/35.6/39.0 21.6/22.2/19.8 ms
after option 1       366M / 352M                 34.6/34.6      22.7/22.7 ms
```

The fps difference is scene noise — the emitted backend command stream is identical, since
vertex and const were already re-uploading on every get.

**WHY IT CANNOT WORK — the gate is global, not per-address.** From x86jit's own codegen
(`x86jit-cranelift/src/codegen/mod.rs:2446`, `note_watched_store`):

```rust
let wc = load(watch_count_ptr);      // Memory::watch_count — a PROCESS-WIDE count
let watched = icmp_ne(wc, 0);
brif watched -> call note_watched_write_helper(mem_self, addr, len)
```

The address is only examined inside the helper (`Memory::note_watched_write` →
`watch.is_watched(page)`). So while ANY page anywhere is watched, EVERY store out of
compiled code calls into Rust. Which layouts we watch changes nothing; only the count
being zero does.

Confirmed by measurement: `UNEMUPS4_DIRTY=always` (AlwaysDirty never calls `watch_range`,
so `watch_count` stays 0) drops helper calls from ~380M to ~2.3M per window —
`note_watched_write_helper` disappears from the table entirely.

**HOW MUCH IS IT ACTUALLY WORTH?** Not established, and the task's estimate (60-90% of a
core) is not supported. The zero-watch run is not apples-to-apples: AlwaysDirty re-uploads
every entry every submit, including detiled textures, so flip explodes to 128 ms/frame and
the run sits at 6.5 fps. In it, `guest_exec` per frame stays at 22-23.7 ms — the same as
with the barrier on. The only suggestive figure is boot/attract-phase throughput, where the
same phase measures 4425 MIPS (baseline) → 4720 (option 1) → 7007 (no watches at all); that
window is a spin-heavy phase and should not be read as a gameplay number.

**WHERE THE FIX BELONGS.** x86jit, not here: inline the per-page watch-bit test into
generated code (it already exists as `watch.is_watched(page)`; the bitmap base is stable for
the run) so an unwatched page costs a load-and-test instead of a call. Option 1 stays useful
as the enabler — once the check is inlined, the ring being unwatched is exactly what makes
the fast path skip. File against x86jit before spending anything more here.

Filed as x86jit **TASK-283** — "watch: inline the per-page watch-bit test into generated stores". Blocked on that landing before any further work here.

### 2026-07-22 (later) — x86jit TASK-283 landed (8a67575). Barrier gone. Costs nothing.

Pin bumped e776a90 -> 8a67575 (inline per-page watch-bit test in generated stores).

```
                              note_watched_write  helpers/kinstr  fps            guest_exec/frame  MIPS
baseline (watch everything)   388,369,775         342.07          36.2/35.6/39.0 21.6/22.2/19.8    135-150
option 1 only                 366,489,935         348.28          34.6/34.6      22.7/22.7         138-142
option 1 + inline check        49,686 - 74,023      2.72-2.98      35.4-37.7      21.0-22.6         130-144
```

**The barrier is gone: 388M -> ~65k per 10 s window, a 5000x drop, and total helper traffic
falls to 2.72 per kinstr — at parity with x86jit's own sqlite (3.35) and lua (2.55).**

**And it bought nothing.** fps, guest_exec per frame, instructions per frame and MIPS are all
unchanged within window-to-window noise. 38 million calls per second removed with no
measurable effect, so each cost well under a nanosecond in practice: a predicted branch to a
tiny leaf helper, absorbed in spare issue slots next to the store it guards.

The task's opening estimate — "even at 20-30 ns per round trip that is 60-90% of a core" —
was wrong by more than two orders of magnitude. Call COUNT was mistaken for call COST. The
counter that made this task look urgent (task-220 -> task-227) measured something real and
attributed it wrongly, twice in a row: first to the lift, then to the barrier.

Both halves are still worth keeping, and they are complementary rather than redundant: the
inline check makes an unwatched page cheap, and the cache policy is what makes the ring's
pages unwatched. Neither alone produces the 5000x drop. They are just not worth fps.

WHAT THIS LEAVES OPEN: guest_exec is still 21-22 ms per frame for ~3.0M guest instructions,
i.e. 130-144 MIPS — roughly an order of magnitude off what the host retires natively. With
helper traffic now at parity with x86jit's own workloads, that gap is not helper round trips.
It is the lift and the generated code itself: x86jit TASK-282's territory, which was being
held until this landed and is now unblocked.

### 2026-07-22 (later still) — host PMU: the guest thread is FRONTEND-bound

`perf stat -t <guest tid>` over 10 s of gameplay (Ryzen 7 7840HS, Zen 4; `perf_event_paranoid=2`
so all counts are user-space, which is exactly the domain of interest):

```
cycles                    39,698,982,521
instructions              40,488,120,386     IPC 1.02
stalled-cycles-frontend   20,271,859,106     51% of all cycles
iTLB-load-misses              38,094,571     0.94 per kinstr
L1-icache-load-misses         65,990,115
L1-dcache-load-misses        326,580,286     8 per kinstr — unremarkable
branch-misses                112,643,529     2.7 per kinstr
cache-misses                 823,468,996
```

Half the cycles are frontend stalls. Data memory is not the problem.

**Expansion factor ~30x.** The host retires 4.05 G instructions/s on this thread; the guest
retires ~100 M/s (2.5M per frame at 40 fps). That is 40 host instructions per guest
instruction across the whole thread, and ~30 counting only the 75% of it that is `guest_exec`.
This is the same number task-282 saw as "~30 host cycles per guest instruction" — at IPC 1.02
the two coincide, and we now know it is genuinely instructions EMITTED, not cycles stalled.

**The profile is flat.** `perf record` with `X86JIT_PERF_MAP=1`: 58,599 blocks in the map,
hottest symbol 0.37%.

```
0.37%  jit_region_0x1b1b8da
0.28%  jit_0x1b60fec
0.25%  jit_0x1b61059
0.20%  jit_0x1b60fc0
```

By DSO: ~83% in JIT-generated code, 17% in our Rust — of which `ps4_gnm::exec::Executor::run`
(the PM4 walk) is 6.9%, the largest single native cost.

**Consequence.** There is no hot loop to optimise and no point hunting one. Mono full-AOT has
an enormous code footprint, we expand it 30x, and the result fits in no level of the frontend:
not the op cache, not L1i, not the iTLB (3.8M misses/s). The only lever that scales is average
emitted-code density — the lift (x86jit task-282), where every percent applies to all 58k
blocks at once. A cheaper, independent second lever: huge pages for the JIT code arena, which
would address most of the iTLB traffic without touching lift quality.

Raw data: scratchpad `perf.log`, `guest.perf.data`, `/tmp/perf-1351875.map`.
