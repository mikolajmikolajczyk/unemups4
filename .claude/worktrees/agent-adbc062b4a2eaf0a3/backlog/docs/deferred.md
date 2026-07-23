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

### Bounded (range-validated) guest-pointer WRITES

- **Why deferred:** The `ps4_core::bounded_read::BoundedRead` seam (task-75) deliberately covers guest-pointer READS only. Guest-pointer WRITES — the `Set*Shader` cmdbuf emit (`crates/libs/src/libscegnmdriver/shader_bind.rs` `emit_into_cmdbuf`) and EOP/EOS label writes (`crates/gnm/src/exec.rs`) — still go through the unbounded identity store (`IdentityMem.write_bytes`). This is pre-existing, reviewed behavior; a hostile/oob write target would corrupt guest memory, but the corpus/homebrew targets are trusted and correctly sized. Adding a parallel `bounded_write` singleton now would duplicate the read seam prematurely.
- **Revisit when:** the read seam gets reshaped (Fable finding #2 / task-80 narrows `parse_sb` to `BoundedRead`) — at that point, if writes need bounding too, extend the SAME seam into a `GuestMemAccess { ranged read + ranged write }` rather than growing a second `bounded_write` global. Do NOT ad-hoc "fix" the write side in isolation.
- **Tracked in:** Fable phase-4 quality review finding #6 (2026-07-12); fold into task-80 if it touches the seam.

### Import-veto assert-vs-graceful-degrade on the display thread

- **Why deferred:** `replay_import` panics (fail-fast assert) on a runtime import decline from the display thread. The default copy-side policy makes this path unreachable, so the panic is safe for now. A non-copy-side import policy (e.g. a garlic/ONION import extension) could reach it and crash the emulator rather than degrade gracefully.
- **Revisit when:** a non-copy-side import policy is enabled (task-53 / task-55); at that point the assert-vs-graceful-degrade tradeoff for a runtime import decline must be reconsidered and an appropriate recovery path designed.
- **Tracked in:** task-53 / task-55 (import policy extension milestone)

<TBD: filled as decisions accumulate. Examples of typical entries:

- error retry / backoff machinery (premature without observed flakiness)
- plugin sandbox (trust model not yet defined)
- i18n (single-locale project for now)
- telemetry / analytics (privacy decision pending)
- DB layer (in-memory is enough for current scope)
>
