# Deferred

Things **deliberately not implemented**. If something seems missing and is listed here, don't add it unprompted — there's a reason. Each entry: what, why deferred, when to revisit.

## Format

```markdown
### <Feature / behavior>

- **Why deferred:** <one paragraph>
- **Revisit when:** <trigger condition>
- **Tracked in:** <task-N, if any>
```

## Entries

### Aggressive guest-CPU JIT optimization / native-speed performance work

- **Why deferred:** unemups4 is a fun-and-education project, not a race for speed — there is no ambition to be a fast emulator. Gameplay is guest-CPU-bound (Celeste in-game is ~24–26 fps, dominated by interpreted guest code and a hot write barrier), and that is understood and accepted. Do **not** go optimize the JIT, add speculative fast paths, or restructure the execution core for throughput unprompted; correctness, clarity, and *explaining how each piece works* come first.
- **Revisit when:** the maintainer explicitly decides performance is a goal, or a specific title is unrunnable purely for speed reasons (not correctness). Targeted, measured wins only — the profiler (`UNEMUPS4_PROFILE`, `X86JIT_PERF_MAP`) points at the real cost before any change.
- **Tracked in:** the write-barrier / MIPS diagnostics (task-220/227); doc-7 "Where things stand".

### Import-veto assert-vs-graceful-degrade on the display thread

- **Why deferred:** `replay_import` (`crates/gpu/src/backend.rs`) asserts (fail-fast) on a runtime import decline from the display thread. The default copy-side policy makes that path unreachable, so the assert is safe. A non-copy-side import policy (e.g. a garlic/ONION import extension) could reach it and crash rather than degrade gracefully. The triangle/textured milestones (task-53/55) landed without changing the copy-side default, so the panic is still unreachable.
- **Revisit when:** a non-copy-side import policy is actually enabled; at that point design a graceful recovery path for a runtime import decline instead of the assert.
- **Tracked in:** import-policy extension (would be its own task).

<TBD: filled as decisions accumulate. Typical entries: retry/backoff machinery (premature without observed flakiness), plugin sandbox (trust model undefined), i18n (single-locale), telemetry (privacy decision pending).>

---

Resolved (kept briefly for context, remove when stale): the earlier "bounded guest-pointer *writes*" deferral is done — task-115 landed the `GuestPtr` seam (`crates/core/src/{guest_ptr,write_guest}.rs`) that does range-validated reads **and** SMC-tracked writes, so a parallel unbounded write is no longer the pattern.
