---
id: doc-3
title: loader architecture
type: other
created_date: '2026-07-10 21:10'
---

# loader architecture

**Status:** design (no implementation). **Scope:** the target architecture for the
executable **loader** — the path from a file on disk (plain ELF, decrypted SELF,
future .prx) to a linked, running guest module — from today's single-image,
re-parse-per-method code through the phases that a real game (Bloodborne, the north
star per `decision-3`) forces: parse-once, PS4 SCE-dynamic + NID symbols, and
multi-module (eboot + N .prx) linking. Each phase plugs in as an increment, not a
rewrite.

**Ground truth for this doc** (read these, don't re-derive): `AGENTS.md` +
`conventions.md` (hard rules — **NO decryption ever**, no features beyond scope,
docs under `backlog/docs/`); `task-23` (SELF extraction, **Done/merged**), `task-26`
(parse-once), `task-27` (SCE-dynamic/NID); the current code —
`crates/loader/src/{elf.rs,self_container.rs,linker.rs,manager.rs,lib.rs}`,
`crates/core/src/img.rs` (the `ExecutableImage` trait + `LoadableSegment`/`Import`/
`Relocation`/`TlsInfo`/`Section` types), and the entrypoint
`crates/kernel/src/process.rs::load_executable`. This doc is the loader companion to
`doc-2` (GPU) and follows the same shape: current-state, layer boundaries, the
load-bearing abstraction, migration order (each step shippable), constraints-to-
design-for, introduce-now-vs-defer, phase→seam table, open questions.

All Rust signatures below are **illustrative sketches, not final API** — they pin the
*shape* of a seam, not its exact spelling.

---

## 0. Current state (what we are refactoring from)

Today's pipeline is one straight line in `process.rs::load_executable`:

```
fs::read(path) -> Vec<u8>
  -> self_container::extract_elf(raw)         [task-23: unwrap SELF | passthrough ELF]
  -> PlainElf::new(inner_elf_bytes)           [the sole ExecutableImage impl]
  -> tls_info()                               [1 goblin parse, for the TLS template]
  -> linker.load_executable(mgr, mem, &img, "eboot.bin")   [8 more goblin parses]
```

- **`ExecutableImage` (in `ps4-core/img.rs`) is a 9-method trait** —
  `segments / sections / entry_point / memory_size / imports / exports / libraries /
  relocations / tls_info`, each returning **owned** data and each calling
  `goblin::elf::Elf::parse(&self.raw_data)` **from scratch**. `PlainElf { raw_data:
  Vec<u8> }` is the only impl. A single load parses the whole file **9 times**
  (`linker.rs` calls 8 accessors; `process.rs` calls `tls_info` for the 9th). Cheap
  for a 20 KB homebrew ELF; for the **93.6 MB Bloodborne inner ELF** it is ~9× a
  full-buffer parse per boot, and it multiplies once .prx loading arrives (N modules
  × 9).

- **SELF handling (task-23, merged):** `self_container::extract_elf(raw: Vec<u8>) ->
  Result<Vec<u8>, SelfError>` detects SELF magic `0x4F153D1D`, reconstructs the inner
  ELF into a flat `Vec<u8>`, and hands it to `PlainElf`. **The SCE/SELF container
  metadata (segment table, program-info, module attributes) is discarded after
  unwrap** — only the raw inner-ELF bytes survive. No decryption: an encrypted
  payload is detected *structurally* (absent inner-ELF magic at the expected offset)
  and rejected `"must be decrypted first"`; compressed segments rejected. This
  constraint is **permanent** and inherited by everything below.

- **`linker.rs::load_executable(mgr, mem, &dyn ExecutableImage, name)`
  hardcodes the single-module world:**
  - `base_addr = 0x400_000` for the main image — a **constant**, not an allocation.
  - **`ModuleAllocator` is dead code:** `ModuleManager` constructs it
    (`ModuleAllocator::new(0x400000)`) but **nothing ever calls `.allocate()`** — the
    allocator that should hand each module a base is never used.
  - **DTPMOD relocations write module id `1`** literally (a `let val = 1usize;`),
    and `DtpOff64` writes `1`. Correct for one module; wrong the moment a second
    module has TLS.
  - Relocations applied inline; imports resolve eagerly via
    `ModuleManager::resolve_symbol`; an unresolved import gets a **lazy 32-byte
    SYSCALL stub** (`MOV EAX, magic_id; SYSCALL; RET`) that traps into
    the HLE syscall handler and, for a genuinely-missing symbol, reports a fatal.

- **`ModuleManager` already has the multi-module bones:** `modules` /
  `name_map` / `ModuleAllocator` / `hle_exports`, plus `register_module` /
  `resolve_symbol` / `register_hle_export` / `get_next_handle`. HLE library exports
  (via `#[ps4_syscall]` + `inventory`, registered in `hle.rs`) and real-module
  exports both flow through `resolve_symbol`. **So the bookkeeping for N modules
  partly exists — but the main-executable path bypasses the allocator and hardcodes
  the base and module id.**

- **Symbols are plain strings.** `PlainElf::imports/exports/relocations` read goblin's
  `elf.dynsyms` / `dynrelas` / `pltrelocs` / `dynstrtab` — the **standard `DT_*`**
  path. `Import.lib_name` is even set to `String::new()` (empty) in the loader; only
  the HLE registry populates library names. Retail PS4 binaries are
  **`ET_SCE_DYNEXEC`**, keep their dynamic tables in `PT_DYNAMIC` +
  `PT_SCE_DYNLIBDATA`, use **SCE-specific tags** (`DT_SCE_SYMTAB` / `DT_SCE_JMPREL` /
  `DT_SCE_PLTGOT` / `DT_SCE_STRTAB` / `DT_SCE_RELA` / `DT_SCE_HASH` / …) and
  **NID-hashed** symbol names (`symbol#library#module`, resolved by a 64-bit hash).
  Goblin decodes none of this, so on a retail eboot `elf.dynsyms` / `dynrelas` come
  back **empty** → zero imports → nothing links. (Bloodborne inner ELF: `ET_SCE_DYNEXEC`,
  10 program headers.)

**Four structural facts drive everything below:**

1. **One fat trait re-parsing per method** (9× parse) — a perf + correctness debt
   that grows with file size and module count.
2. **Container metadata is thrown away** — the moment a real game needs the SCE
   module attributes / dynlib-data pointers, they're gone and the shape can't carry
   them.
3. **Single-module assumptions are baked in** — hardcoded base `0x400_000`, unused
   `ModuleAllocator`, hardcoded module id `1`.
4. **Symbol *source* is hardwired to goblin's standard `DT_*`** — no seam to swap in
   the SCE-dynamic + NID reader.

None of these bite the six homebrew examples. All four bite Bloodborne. The design
below turns each into a **seam** so the fix is additive.

---

## 1. The layered pipeline (module / type boundaries)

**Recommendation: split the one straight line into four layers with a strict one-way
data flow, each a small trait or owned type, so a new container format, a new symbol
source, or a second module plugs into exactly one layer.** This mirrors `doc-2`'s
"trait in `ps4-core`, impl elsewhere, thin glue" discipline: an ELF/container
front-end, an owned parsed-module core, and a separate linker/symbol resolver.

```
        ┌─────────────────────────────────────────────────────────────────┐
        │ L1  Container      bytes on disk -> (inner ELF bytes + metadata)  │
        │     detect & unwrap: RawElf | SELF | (future fSELF / .prx entry)  │
        │     crates/loader/container/                                       │
        └───────────────────────────────┬───────────────────────────────────┘
                                         │  Container { elf_bytes, meta }
        ┌───────────────────────────────▼───────────────────────────────────┐
        │ L2  Image (parse-once)   one goblin/SCE parse -> owned ParsedImage │
        │     cheap accessors; ExecutableImage becomes accessors over cache  │
        │     crates/loader/image/                                           │
        └───────────────────────────────┬───────────────────────────────────┘
                                         │  reads via L3 for the dynamic half
        ┌───────────────────────────────▼───────────────────────────────────┐
        │ L3  Symbol / dynamic source (pluggable)                            │
        │     StdDynamic (DT_*, goblin)  |  SceDynamic (DT_SCE_* + NID)      │
        │     -> the SAME Import / Export / Relocation model                 │
        │     crates/loader/dynamic/                                         │
        └───────────────────────────────┬───────────────────────────────────┘
                                         │  Imports / Exports / Relocations
        ┌───────────────────────────────▼───────────────────────────────────┐
        │ L4  Module / linking     per-module base (allocator), module id,   │
        │     map+reloc+resolve, cross-module symbol resolution              │
        │     crates/loader/{linker.rs, manager.rs}                          │
        └─────────────────────────────────────────────────────────────────┘
```

Dependency direction is strictly downward: **L1 knows nothing of ELF internals**
(it only locates the inner ELF and copies metadata); **L2 owns the parse** and asks
**L3** for the dynamic-linking view; **L4** consumes L2/L3 outputs and does memory +
relocations. No layer reaches back up. Everything except the actual `memory.map` /
`memory.write` calls in L4 is Vulkan-free, driver-free, and `cargo test`-able with
in-memory byte buffers (exactly as `self_container`'s tests already are).

### L1 — Container (the "plug in cleanly" the maintainer asked for)

Today `extract_elf` returns `Vec<u8>` and drops everything else. Reshape it to return
the inner ELF **plus** retained container metadata, behind an extensible dispatch:

```rust
// crates/loader/container (sketch — NOT final)

pub enum ContainerKind { RawElf, Self_, /* future: FSelf, Prx */ }

/// What L1 hands to L2: the plaintext inner ELF, plus metadata we chose to KEEP
/// instead of discarding (task-23 throws this away today).
pub struct Container {
    pub kind: ContainerKind,
    pub elf_bytes: Vec<u8>,          // the inner ELF, exactly as goblin/SCE parses
    pub meta: ContainerMeta,         // retained SCE/SELF fields (empty for RawElf)
}

/// Retained container metadata. Populated for SELF; the fields a real game/module
/// graph needs (module attributes, program-authority id, the sce program
/// type that says "eboot vs prx vs lib"). Kept small — grows only as a consumer
/// needs a field.
#[derive(Default)]
pub struct ContainerMeta {
    pub sce_program_type: Option<u16>,   // eboot / prx / kernel-module discriminator
    pub module_attributes: Option<u64>,
    // ... add fields when L4 / .prx loading actually consumes them.
}

/// The extensible dispatch: magic -> kind -> unwrap. This is the ONE place a new
/// container format is added. NOT a plugin registry (see over-engineering traps).
pub fn open(raw: Vec<u8>) -> Result<Container, ContainerError>;
```

`open` subsumes today's `extract_elf`: `0x7F E L F` → `RawElf` passthrough (meta
empty); `0x4F153D1D` → SELF unwrap **and** fill `meta`; unknown magic / encrypted /
compressed → the same explicit errors as today (**no decryption, ever**). The dispatch
is a `match` on magic, not a trait-object registry — there are two real formats and a
handful of future ones; a `match` arm per format is the right granularity (doc-2's
"one impl is cheap insurance, a speculative second is waste" applied to formats).

### L2 — Image / parse-once (task-26's core)

goblin's `Elf<'a>` **borrows** the byte buffer (self-referential-struct problem), so
the clean move is a **single parse pass into an owned `ParsedImage`**, then
`ExecutableImage` accessors read from that cache instead of re-parsing.

```rust
// crates/loader/image (sketch)

/// Everything a load needs, extracted ONCE into owned structs. No goblin::Elf<'a>
/// stored (that would re-introduce the self-referential borrow); we copy out.
pub struct ParsedImage {
    pub entry: u64,
    pub memory_size: usize,
    pub segments: Vec<LoadableSegment>,
    pub sections: Vec<Section>,
    pub tls: Option<TlsInfo>,
    pub libraries: Vec<String>,
    // the dynamic-linking half comes from L3, computed once at parse time:
    pub imports: Vec<Import>,
    pub exports: HashMap<String, u64>,
    pub relocations: Vec<Relocation>,
    pub meta: ContainerMeta,      // carried down from L1
}

impl ParsedImage {
    /// The single parse. Picks the L3 dynamic source by inspecting e_type / tags.
    pub fn parse(container: Container) -> Result<ParsedImage, LoaderError>;
}
```

**How `ExecutableImage` reshapes.** Two viable forms (open question #1):

- **(A) Keep the trait, back it by the cache.** `PlainElf` holds a
  `ParsedImage` and each method returns a cheap clone/borrow of the cached field. The
  9-method surface and every caller are unchanged; only the bodies change (no more
  `Elf::parse` per method). Smallest diff, lowest risk, satisfies task-26 AC #1/#2
  exactly. The trait stays the boundary between loader and kernel.
- **(B) Collapse the trait into `ParsedImage`.** `load_executable` takes
  `&ParsedImage` directly; the trait disappears. Cleaner long-term but a wider diff
  (touches `process.rs`, `linker.rs`, tests) for no behavior gain.

**Recommendation: (A) for task-26** (zero-behavior-change refactor), keep (B) as a
follow-up only if a second `ExecutableImage` impl never materializes. Either way the
per-method `map_err(io::Error)` boilerplate collapses into one `LoaderError` path at
`parse` time (task-26 AC #3).

### L3 — Symbol / dynamic source (task-27; the pluggable seam)

The `Import` / `Export` / `Relocation` model in `img.rs` **stays**. Only the *source*
of those records swaps. Introduce a trait that L2 calls once during `parse`:

```rust
// crates/loader/dynamic (sketch)

/// Produces the dynamic-linking view of an image. Two impls; auto-selected by L2.
pub trait DynamicSource {
    fn imports(&self)    -> Result<Vec<Import>, LoaderError>;
    fn exports(&self)    -> Result<HashMap<String, u64>, LoaderError>;
    fn relocations(&self)-> Result<Vec<Relocation>, LoaderError>;
    fn libraries(&self)  -> Result<Vec<String>, LoaderError>;
}

/// Homebrew: today's goblin dynsyms/dynrelas path, unchanged. lib_name stays "".
struct StdDynamic<'a> { elf: &'a goblin::elf::Elf<'a> }

/// Retail: walks PT_DYNAMIC / PT_SCE_DYNLIBDATA, reads DT_SCE_* tables WITHOUT
/// goblin, decodes NID-hashed names into the SAME Import/Export records.
struct SceDynamic<'a> { /* raw slices into elf_bytes + parsed SCE tags */ }
```

**Auto-detection** (task-27 AC #4): `e_type == ET_SCE_DYNEXEC` (or presence of a
`PT_SCE_DYNLIBDATA` phdr / any `DT_SCE_*` tag) selects `SceDynamic`; otherwise
`StdDynamic`. Homebrew keeps taking the standard path byte-for-byte.

**What `SceDynamic` does** (the task-27 body): locate `PT_DYNAMIC` and the
`PT_SCE_DYNLIBDATA` blob; read the SCE tags to find the symbol table
(`DT_SCE_SYMTAB` / `…SZ`), string table (`DT_SCE_STRTAB` / `…SZ`), rela + jmprel
tables (`DT_SCE_RELA` / `DT_SCE_JMPREL` / `…SZ`), and the module/library name tables;
for each symbol, split the encoded name at `#` into `symbol#library#module`, and
**decode / hash the NID**. The **NID → name mapping** needs a database seam:

```rust
// crates/loader/dynamic/nid (sketch)

/// Maps a 12-char-base64 NID (or its 64-bit hash) to a human name, and computes a
/// NID from a plaintext symbol (SHA-1 of `name || nidSuffixKey`, first 8 bytes,
/// endian-swapped, Sony base64 — the OpenOrbis NID hashing algorithm).
pub trait NidDatabase {
    fn name_for(&self, nid: &str) -> Option<&str>;   // reverse lookup
    fn nid_for(&self, name: &str) -> String;          // forward hash (for HLE match)
}
```

The **forward** direction is the one that matters first: our HLE exports are keyed by
plaintext (`sceKernelAllocateDirectMemory`, …). To match a retail import's NID against
an HLE export we either (a) hash **our** export names to NIDs once at startup and
resolve NID→NID, or (b) reverse-map the guest's NID to a name via a DB then resolve by
name. **(a) is preferred** — it needs only the forward hash (no external DB) for every
symbol we actually implement; a reverse DB is only for *diagnostics* ("unknown NID
`xyz` — no HLE impl"). So the minimal task-27 needs the **hash function**, not a
complete NID→name table. (See open question #2.)

**Relocation reshape:** `Import` today carries `lib_name: String` (empty) +
`symbol_name` + `symbol_id`; `Relocation` carries `symbol_name: Option<String>`. For
SCE these become the **decoded** name (or NID) and the **library** field finally gets
populated — cross-module resolution (L4) keys on `(library, symbol)`. No new types;
existing fields, finally filled.

### L4 — Module / linking (multi-module foundation)

Today `load_executable` is single-image. Reshape it so **every image gets its base
from the allocator and its own module id**, and so a second image can be loaded and
cross-resolved against the first.

```rust
// crates/loader/linker (sketch — the shape, not the body)

impl DynamicLinker {
    /// Load ONE image into a freshly-allocated base with a real module id.
    /// (Today: base hardcoded 0x400_000, id hardcoded 1.)
    pub fn load_image(
        &self,
        mgr: &mut ModuleManager,
        mem: &mut dyn VirtualMemoryManager,
        img: &ParsedImage,
        name: &str,
    ) -> Result<ModuleHandle, LoaderError> {
        let id = mgr.get_next_handle();
        let base = mgr.allocator.allocate(img.memory_size); // FINALLY used
        // map segments at `base + seg.offset`, apply relocs with THIS base + id,
        // DTPMOD writes `id` (not literal 1), register exports shifted by base.
        // ...
    }
}
```

- **Per-module base via the (currently-dead) `ModuleAllocator`.** The allocator
  already exists; L4's job is to *call it* instead of the `0x400_000` constant. The
  main executable keeps `0x400_000` as the allocator's `start` so homebrew loads at
  the same address (no regression), but the *value comes from `allocate`*, so a
  second module gets the next slot automatically.
- **Real module id, not `1`.** `DtpMod64` writes the module's `id`
  (`mgr.get_next_handle()` result); `DtpOff64` / TLS offsets become per-module.
  This is the correctness fix for cross-module TLS.
- **Cross-module symbol resolution.** `resolve_symbol` already iterates all modules;
  L4 just needs each module registered with correct `base_addr` + `exports` before a
  dependent module relocates. `.prx` dependency ordering (load leaves first) is the
  deferred body; the *seam* is "load N images, resolve across them" — which the
  allocator + real ids + existing `resolve_symbol` already permit.

**Deferred here (design the seam, implement later):** `.prx` file discovery + a
dependency graph; `sceKernelLoadStartModule` (which re-enters `load_image` at runtime
for a named .prx, re-using the same static load path); lazy binding (today's
eager-resolve + lazy-stub-for-missing is fine and stays).

---

## 2. The load-bearing abstraction

If `doc-2`'s load-bearing seam is `GpuBackend`, the loader's is **`ParsedImage` +
`DynamicSource`**: a single owned parse (L2) whose dynamic half comes from a
swappable source (L3). That pairing is what makes every downstream need additive:

- **Parse-once** kills the 9× re-parse and, crucially, gives every later layer a
  *stable owned object* to hang fields on (the container `meta`, the NID-decoded
  symbols, per-module state) instead of re-deriving from raw bytes.
- **`DynamicSource`** is the exact point where "homebrew vs retail" forks, with the
  `Import`/`Export`/`Relocation` model as the invariant contract on both sides — so
  L4's linker never learns whether a symbol arrived as a string or a NID.

Everything else (container dispatch, allocator wiring, module ids) is plumbing around
this pair.

---

## 3. Migration order (each step independently shippable)

The **six homebrew examples must load byte-identically throughout** (task-26 AC #4,
task-27 AC #4), exactly like `doc-2` §7 keeps softgpu presenting at 60fps. Each step
compiles, passes `cargo test` + `clippy -D warnings` + `fmt`, and is a separate
landable change.

1. **[pure refactor — reshaped task-26] Parse-once + L1/L2 seam.**
   Introduce `Container` (L1: `open` returns inner ELF **+ retained `ContainerMeta`**,
   subsuming `extract_elf`) and `ParsedImage` (L2: one goblin parse into owned
   fields). Back `PlainElf`/`ExecutableImage` by the cache (form A). **Zero behavior
   change** — same six examples byte-identical, same SELF path, 1 parse not 9. This is
   task-26's core **plus** the container/image seam folded in (see §5, task breakdown).
   *No new formats, no NID — just the seam and the de-duplication.*

2. **[behavior-adding — task-27] L3 SCE-dynamic + NID.**
   Add the `DynamicSource` trait with `StdDynamic` (extract today's goblin path
   verbatim) and `SceDynamic` (DT_SCE_* + NID decode). Auto-select by `e_type`/tags.
   Add the NID **forward-hash** function; resolve retail imports against HLE by hashing
   our export names. Homebrew unchanged (takes `StdDynamic`). First step where a
   retail eboot's imports become non-empty. *Confirm the failure mode first (task-27
   AC #1): dump goblin `dynsyms`/`dynrelas` len on the extracted Bloodborne ELF.*

3. **[behavior-adding — NEW task, "L4 multi-module base + ids"] Per-module base +
   real module ids.**
   Wire `ModuleAllocator.allocate` into `load_image` (retire the `0x400_000`
   constant; keep it as the allocator start so homebrew is unmoved). `DtpMod64`/
   `DtpOff64`/TLS offsets use the real module id. Still one module loaded in practice,
   but the single-module assumptions are gone. *Pure plumbing for the corpus (still
   one image); the payoff is that step 4 becomes additive.*

4. **[behavior-adding — LATER task] .prx multi-module + `sceKernelLoadStartModule`.**
   `.prx` discovery, a dependency-ordered load loop over `load_image`, and the runtime
   `sceKernelLoadStartModule` entrypoint. Only when a game/target actually needs a
   second module. *This is the one that waits for a game; steps 1–3 make it a small
   addition.*

Steps 1 and 3 are **pure refactors** (no observable behavior change for the corpus);
steps 2 and 4 **add behavior** (new binaries link). Throughout, the plain-ELF path is
the stable floor — any homebrew that only uses standard `DT_*` symbols loads exactly
as today.

---

## 4. Constraints to design for (implement later)

Structural facts of the real format that are cheap to leave room for now and a rewrite
to retrofit. **This section does NOT expand scope** — early steps implement none of
the deferred bodies. Each states the *minimal seam now* vs. what to *genuinely defer*.

### K1. No decryption — ever (inherited from task-23)

The hard, permanent constraint. L1 detects encryption **structurally** (absent inner-
ELF magic where plaintext would sit) and rejects `"must be decrypted first"`; there
are **no keys, no SAMU, no crypto** anywhere in the tree, and none may be added. The
container `meta` carries no key material. Every layer inherits this: `SceDynamic`
reads only plaintext tables; the NID hash is a public algorithm, not decryption.
*Seam now:* the error variants already exist. *Defer:* nothing — this is a wall, not a
deferral.

### K2. NID hashing + a name database

Retail symbols are NIDs, not strings. *Seam now:* the `NidDatabase` trait with a
**forward hash** (`nid_for(name)`), so HLE exports can be matched without any external
data. *Defer:* a complete **reverse** NID→name table (thousands of entries) — needed
only for readable diagnostics of *unimplemented* symbols, never for resolution of ones
we implement. Don't vendor a giant DB before a game needs the diagnostics. (Open Q #2.)

### K3. TLS module-id correctness across modules

`DTPMOD` must name the module that owns the TLS block; `DTPOFF` is an offset within it;
the DTV (dynamic thread vector) indexes per-module TLS. Today both are hardcoded to
`1` — correct for one module, silently wrong for two. *Seam now:* thread the real
`ModuleHandle` into the DTPMOD/DTPOFF arms (step 3). *Defer:* full DTV construction and
`__tls_get_dynamic` across modules — until a multi-module TLS case exists. The
`TlsInfo` type + `tls_template` plumbing already exist per-image; the gap is the *id*,
not the data.

### K4. Lazy-stub vs real-import interplay when NIDs resolve

Today: resolve eagerly, and any unresolved import gets a SYSCALL lazy-stub that traps
(fatal for genuinely-missing). When `SceDynamic` lands, a retail import's NID may match
an HLE export (→ real resolve) **or** not (→ stub, as today). *Seam now:* the stub path
is source-agnostic — it keys on "did `resolve_symbol` return None", which is true
whether the name came as a string or a NID. So the existing lazy-stub machinery works
unchanged once L3 produces resolvable names/NIDs. *Defer:* true lazy binding (resolve
on first call via a PLT trampoline) — the eager+stub model is sufficient and simpler;
don't build PLT lazy-resolution unprompted.

### K5. .prx dependency ordering + memory-manager coupling

A multi-module game loads the eboot plus N `.prx`; a module must be mapped and its
exports registered *before* a dependent relocates against it. *Seam now:* `load_image`
loads **one** image against the shared `ModuleManager`; loading N in dependency order is
a loop over it (step 4). The allocator (step 3) guarantees non-overlapping bases.
*Defer:* the actual `.prx` discovery + topological dependency sort +
`sceKernelLoadStartModule`. **Memory-manager coupling note:** per-module bases come
from `ModuleAllocator` (a loader concern) mapped via `VirtualMemoryManager::map` (the
kernel memory manager) — the identity-mapped arena means base == host addr, so there's
no translation, but the allocator must not collide with kernel/GPU/heap regions. Keep
the allocator's `start`/range a single owned decision in `manager.rs`.

**Honest framing:** K1 is a wall (nothing to defer). K2/K3 seams are "carry a hash /
a real module id" — nearly free. K4 is already source-agnostic. K5's seam is "loop over
`load_image`" — the body (dep graph, LoadStartModule) is the only genuinely deferred
work, and it waits for a game that needs it.

---

## 5. Introduce-now vs defer (prioritized)

**Introduce NOW (cheap seams, high leverage):**

1. **Retain container metadata (L1 `ContainerMeta`).** Reshape `extract_elf` → `open`
   returning inner ELF **+ meta** instead of discarding it. Nearly free; without it,
   the SCE module attributes / program-type a real game needs are gone. *(step 1)*
2. **Parse-once `ParsedImage` (L2).** The task-26 core; kills 9× re-parse and gives
   every later layer a stable owned object. *(step 1)*
3. **`DynamicSource` trait (L3 seam), even if only `StdDynamic` ships first.** Shapes
   `Import`/`Export`/`Relocation` as source-agnostic so task-27 slots `SceDynamic` in
   without touching the linker. *(seam in step 1, second impl in step 2)*
4. **Per-module base via `ModuleAllocator` + real module ids (L4).** Retire the
   hardcoded `0x400_000` and module-id `1`. Foundation for eboot + N .prx and correct
   cross-module TLS. *(step 3)*

**Defer until its phase actually arrives:**

- **Full reverse NID→name database** — until diagnostics of unimplemented symbols
  demand it (forward hash is enough to *resolve*). *(K2)*
- **`.prx` discovery + dependency graph + `sceKernelLoadStartModule`** — until a game
  needs a second module. *(K5, step 4)*
- **True lazy binding / PLT trampolines** — the eager+stub model stays. *(K4)*
- **Form-B trait collapse** (`ExecutableImage` → `ParsedImage`) — only if a second
  image impl never appears.

**Over-engineering traps to avoid (doc-2 §b style):**

1. **No plugin registry for container formats.** There are two real formats (ELF,
   SELF) and a couple of future ones. L1 is a `match` on magic, **not** a
   trait-object/`inventory` registry. A registry for ~4 formats is fantasy-HAL.
2. **No giant NID DB up front.** Implement the **hash** and match HLE by hashing our
   own names. A vendored thousands-entry NID→name table is deferred until *diagnostics*
   need it — and even then it's data, not architecture.
3. **No `.prx` loader before a game needs it.** Design the `load_image`-loop seam
   (step 3 makes it possible), but do **not** implement discovery/dep-graph/
   LoadStartModule speculatively. The corpus is single-module; keep it that way until a
   real target isn't.
4. **No self-referential-parse gymnastics.** Don't reach for a `yoke`/`ouroboros`
   self-ref crate to store goblin's borrowing `Elf<'a>` alongside its buffer.
   **Own-extract** (copy the needed fields out into `ParsedImage`) is simpler, matches
   task-26's intent, and sidesteps the borrow entirely. (Open Q #3.)

---

## 6. Proposed task breakdown (for the maintainer to file — do NOT create here)

Reconciling the three existing tasks with the four-layer design:

| Task | Layer(s) | One-line scope | Deps |
|---|---|---|---|
| **task-23** *(Done)* | L1 (partial) | SELF unwrap → inner ELF bytes, no decryption | — |
| **task-26** *(RESHAPE)* | **L1 + L2** | Parse-once **+ container/image seam**: `open`→`Container{elf_bytes, meta}` (retain metadata, don't discard) **and** `ParsedImage` single-parse backing `ExecutableImage`. Pure refactor, six examples byte-identical. | task-23 |
| **task-27** *(keep as L3)* | **L3** | `DynamicSource` trait; `SceDynamic` (DT_SCE_* + NID decode) beside `StdDynamic`; NID forward-hash; auto-detect by `e_type`. Confirm goblin-empty failure first. | task-26 |
| **NEW: "loader: per-module base + real module ids (L4)"** | **L4** | Wire `ModuleAllocator.allocate` into a `load_image`; DTPMOD/DTPOFF/TLS use the real module id; retire hardcoded base+id. Pure plumbing (still one image). | task-26 |
| **NEW (LATER): "loader: .prx multi-module + sceKernelLoadStartModule"** | **L4** | `.prx` discovery, dependency-ordered `load_image` loop, runtime `LoadStartModule`. | task-27, the L4 task |

**Dependency order:** task-23 (done) → **task-26 (L1+L2)** → { **task-27 (L3)**,
**L4-base task** } in parallel → **.prx task (L4 later)**. The reshape of task-26 to
absorb the L1 container seam is the key move: it makes retaining metadata a *refactor*
(cheap, now) rather than a retrofit later.

> These are **proposals**. Do not create tasks from this doc — the maintainer files
> them (per `AGENTS.md` working-on-tasks flow).

---

## 7. Open questions for the maintainer

1. **Reshape `ExecutableImage`, or keep it and add a caching layer?** §1-L2 form (A)
   keeps the 9-method trait and backs it with a `ParsedImage` cache (smallest diff,
   recommended for task-26); form (B) collapses the trait into `ParsedImage` and has
   `load_executable` take it directly (cleaner, wider diff, no behavior gain). Confirm
   **(A) for the refactor**, (B) only if a second `ExecutableImage` impl never appears?

2. **Where does the NID→name database come from, and how much do we need?** Resolution
   needs only the **forward hash** (hash our HLE export names, match retail NIDs) — no
   external DB. A **reverse** NID→name table is purely for diagnostics of
   *unimplemented* symbols. Confirm: ship the forward hash in task-27, and **defer** any
   vendored reverse DB (and if/when we do it, source it from OpenOrbis/community NID
   lists as *data*, GPL-compatible)?

3. **Self-referential parse handling: owned-extract vs a yoke/ouroboros self-ref
   crate.** goblin's `Elf<'a>` borrows its buffer. Recommendation is **own-extract**
   (copy fields into `ParsedImage`, store no borrowing view) — simplest, no new dep.
   Confirm we avoid a self-ref crate (`yoke`/`ouroboros`) entirely?

4. **Design the .prx multi-module seams now, or defer even the seam?** This doc
   designs the L4 base-allocator + module-id seam **now** (step 3, cheap plumbing) and
   defers only the `.prx` *body* (discovery/dep-graph/LoadStartModule, step 4). Is
   landing the L4 base/id plumbing before any game needs a second module acceptable, or
   defer even that until a multi-module target is real?

5. **Container metadata scope.** `ContainerMeta` should retain *some* SCE/SELF fields
   (program-type "eboot vs prx", module attributes) rather than discard all of it. How
   much to retain now vs. add-a-field-when-consumed? Recommendation: retain only what a
   named consumer needs (start near-empty, grow per-need) — confirm the minimalist
   stance, so we don't model the whole SCE header speculatively.

6. **NID resolution granularity: NID-only vs `(library, module, NID)`.** Retail
   encodes `symbol#library#module`. Do we resolve on the bare NID (simpler; risks
   collisions across libraries) or the full `(library, module, NID)` tuple (matches how
   the real linker scopes symbols to a library)? Recommendation: carry the library id
   (finally populate `Import.lib_name`) and resolve scoped, but confirm before task-27
   fixes the resolution key.

---

## Layer → seam mapping (summary)

| Step | New/changed code | Seam(s) it establishes |
|---|---|---|
| 1 (task-26, refactor) | `container::open`→`Container{elf_bytes, meta}`; `ParsedImage::parse`; `ExecutableImage` backed by cache | **L1 container + retained meta**, **L2 parse-once**, **L3 `DynamicSource` trait stub** |
| 2 (task-27) | `SceDynamic` beside `StdDynamic`; NID forward-hash; auto-detect | **L3 second impl**, NID hashing |
| 3 (new L4 base task) | `ModuleAllocator.allocate` wired into `load_image`; real module ids in DTPMOD/TLS | **L4 per-module base + id** |
| 4 (later .prx task) | `.prx` discovery + dep-ordered load loop; `sceKernelLoadStartModule` | **L4 multi-module body** |

*Companion note:* if the maintainer wants a cross-cutting commitment recorded (all
loader work flows through the `Container` → `ParsedImage` → `DynamicSource` →
`load_image` pipeline, mirroring `decision-4` for GPU), a loader `decision` can capture
it — but this doc does **not** create one.

**Sources (architecture lessons, not code copied):**
[OpenOrbis PS4 ELF Specification — Dynlib Data](https://github.com/OpenOrbis/OpenOrbis-PS4-Toolchain/wiki/PS4-ELF-Specification---Dynlib-Data),
[FreeBSD `rtld-elf` dynamic linker (the SCE-dynamic model's ancestor)](https://man.freebsd.org/cgi/man.cgi?query=rtld),
[OpenOrbis PS4 ELF Specification](https://github.com/OpenOrbis/OpenOrbis-PS4-Toolchain/wiki/PS4-ELF-Specification).
