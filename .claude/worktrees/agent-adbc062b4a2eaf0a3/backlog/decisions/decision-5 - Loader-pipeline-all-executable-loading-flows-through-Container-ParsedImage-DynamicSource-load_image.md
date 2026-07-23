---
id: decision-5
title: >-
  Loader pipeline: all executable loading flows through Container -> ParsedImage
  -> DynamicSource -> load_image
date: '2026-07-10 20:28'
status: accepted
---
## Context

Adding SELF support (task-23) and heading toward loading a real game (Bloodborne,
the north star per decision-3) exposed four structural liabilities in the loader:
(1) `ExecutableImage` re-parses the whole file 9× per load (once per method); (2)
`self_container::extract_elf` discards the SCE/SELF container metadata after unwrap;
(3) single-module assumptions are baked in — hardcoded base `0x400_000`, a **dead**
`ModuleAllocator` (constructed, never called), hardcoded TLS module id `1`; (4) the
symbol source is hardwired to goblin's standard `DT_*` tables, with no seam for the
PS4 `DT_SCE_*` dynamic tables + NID-hashed symbols a retail binary uses. None bite
the six homebrew examples; all four bite Bloodborne. The maintainer's mandate:
"the loader should load ELFs AND SELFs, plug in cleanly, done right now so real
games don't bite us later." Design captured in `backlog/docs/doc-5` (loader
architecture, the companion to doc-4 for GPU).

## Decision

All executable loading flows through a **four-layer pipeline**, each layer a small
trait or owned type so a new container format, a new symbol source, or a second
module plugs into exactly one layer:

- **L1 Container** — `container::open(bytes) -> Container { kind, elf_bytes, meta }`,
  a `match`-on-magic dispatch (RawElf | SELF | future fSELF/.prx) that **retains**
  container metadata (`ContainerMeta`) instead of discarding it. NOT a plugin
  registry. **No decryption, ever** (inherited from task-23, permanent).
- **L2 Image (parse-once)** — `ParsedImage::parse(container)` does a single parse
  into owned fields (own-extract, no self-referential `Elf<'a>` stored, no self-ref
  crate). `ExecutableImage` is **kept** and backed by the cache (form A) — smallest
  diff, callers unchanged.
- **L3 DynamicSource** — a trait producing the `Import`/`Export`/`Relocation` view,
  with `StdDynamic` (goblin `DT_*`, homebrew) and `SceDynamic` (`DT_SCE_*` + NID,
  retail) auto-selected by `e_type`. NID resolution needs **no hashing at all** (task-30):
  the raw on-disk NID in `symbol_name` is looked up directly in ps4-syscalls' generated
  `MAP_BY_NID`, which is canonical (same salt + SHA-1 bit-slice as shadPS4 `StringToNid`);
  hle.rs registers every HLE export under `def.id.nid()`, so `resolve_symbol(nid)` returns
  the stub. A reverse NID→name DB is diagnostics-only and deferred.
- **L4 Module/linking** — `load_image` takes each module's base from
  `ModuleAllocator.allocate` (the dead allocator, finally used) and a real module id
  from `get_next_handle`; DTPMOD/TLS use the real id, not `1`. Foundation for eboot +
  N `.prx` and correct cross-module TLS.

Seam-now / defer-body discipline: introduce the seams early (container meta,
`DynamicSource` trait, per-module base + ids) but defer the heavy bodies (full
reverse NID DB, `.prx` discovery + dependency graph + `sceKernelLoadStartModule`,
true lazy PLT binding) until a target actually needs them.

Maintainer decisions 2026-07-10: **form (A)** for `ExecutableImage` (keep + cache,
not collapse); **file the L4 base/id plumbing now** (task-28), run after task-26.

## Consequences

- **Task chain (dependency-encoded):** task-23 (Done, L1 partial) → **task-26**
  (L1 container + meta, L2 parse-once, L3 `DynamicSource` trait stub) → { **task-27**
  (L3 `SceDynamic` + NID), **task-28** (L4 per-module base + real ids) } in parallel →
  **task-29** (L4 body: `.prx` multi-module + `sceKernelLoadStartModule`, DEFERRED
  until a game needs a 2nd module).
- Steps task-26 and task-28 are **pure refactors** (six examples byte-identical);
  task-27 and task-29 add behavior (new binaries link). The plain-ELF path stays the
  stable floor throughout.
- The linker never learns whether a symbol arrived as a string or a NID — the
  `Import`/`Export`/`Relocation` model is the invariant contract across L3 impls.
- No decryption enters the tree at any layer; `SceDynamic` reads only plaintext
  tables, the NID hash is a public algorithm.
- Over-engineering explicitly rejected: no container plugin registry, no vendored NID
  DB up front, no `.prx` loader before a game needs it, no self-ref crate.

Supersedes nothing; complements decision-3 (Bloodborne north star) and decision-4
(GPU pipeline). Full design + open questions: `backlog/docs/doc-5`.
