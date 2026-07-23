---
id: decision-2
title: Pin x86jit as local git dependency at a fixed rev
date: '2026-07-09 16:44'
status: accepted
---
## Context

The x86jit migration initially wired x86jit as a plain path dependency
(`path = "../x86jit/..."`). Cargo then builds against the *current working
tree* of `~/src/x86jit` — including uncommitted, possibly broken changes.
x86jit is under active development in parallel with this migration, so a
breakage there would surface as confusing test failures here, with no way to
tell which side is at fault.

## Decision

Depend on x86jit via a local git URL pinned to a commit:

```toml
x86jit-core = { git = "file:///home/mikolaj/src/x86jit", rev = "<commit>" }
x86jit-cranelift = { git = "file:///home/mikolaj/src/x86jit", rev = "<commit>" }
```

Cargo checks out the committed tree at `rev`; the x86jit working tree and any
newer commits are invisible. No network involved. `Cargo.lock` records the rev,
so builds are reproducible.

Bump procedure: merge the needed change in x86jit, update `rev` in
`Cargo.toml`, run `cargo update -p x86jit-core -p x86jit-cranelift`, commit
both files. For tight iteration loops (e.g. task-5 lift fixes) a temporary
`[patch]` section pointing at the path is acceptable, but must not be
committed.

## Consequences

- unemups4 tests always run against a known-good x86jit commit; breakage in
  the x86jit working tree cannot leak in.
- One extra step (rev bump) per x86jit change consumed here — deliberate and
  visible in git history.
- The `file://` URL hardcodes the local checkout location; other machines need
  the same path or a rewrite to the GitHub URL (acceptable: solo, local-first).
