---
id: TASK-26
title: >-
  loader: parse-once + container/image seam (L1+L2) — kill 9x re-parse, retain
  SELF meta
status: Done
assignee: []
created_date: '2026-07-10 19:34'
updated_date: '2026-07-10 20:51'
labels:
  - refactor
  - loader
dependencies:
  - TASK-23
priority: high
ordinal: 26000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
SPEC = doc-5 (loader architecture) steps L1 + L2 + the L3 trait stub; migration step 1 (pure refactor). Read doc-5 first — it is the loader design, mirroring doc-4 for GPU. Maintainer decisions 2026-07-10: form (A) — keep the ExecutableImage trait, back it with the cache (smallest diff, callers unchanged), NOT form (B) collapse.

Three moves, ZERO behavior change:

L1 — Container. Reshape self_container::extract_elf(raw) -> Vec<u8> into container::open(raw) -> Result<Container, ContainerError> where Container { kind: ContainerKind, elf_bytes: Vec<u8>, meta: ContainerMeta }. match-on-magic dispatch (0x7F454C46 RawElf passthrough, meta empty | 0x4F153D1D SELF unwrap AND fill meta). RETAIN the SCE/SELF container metadata task-23 currently discards (sce_program_type eboot/prx discriminator, module_attributes) — start meta near-empty, grow per consumer. NO decryption: same structural encrypted/compressed rejection + errors as today (inherited, permanent). NOT a plugin registry — a match arm per format.

L2 — Parse-once image. ParsedImage::parse(container) does ONE goblin parse into OWNED fields (entry, memory_size, segments, sections, tls, libraries, imports, exports, relocations, + carried meta). No goblin::Elf<'a> stored (own-extract, no self-ref crate). PlainElf holds a ParsedImage; the 9 ExecutableImage methods (segments/sections/entry_point/memory_size/imports/exports/libraries/relocations/tls_info) become cheap accessors over the cache — form A, trait + every caller (linker.rs, process.rs) unchanged, only bodies change. Kills the 9x per-load re-parse (~9x an 89MB parse for a retail eboot; ×N with .prx). Fold the per-method map_err(io::Error) boilerplate into the existing LoaderError path.

L3 seam (stub only). Introduce the DynamicSource trait (imports/exports/relocations/libraries) with StdDynamic as the SOLE impl — extract today's goblin dynsyms/dynrelas path verbatim behind it. This shapes Import/Export/Relocation as source-agnostic so task-27's SceDynamic slots in WITHOUT touching the linker. No SCE/NID logic here.

ZERO behavior change — six examples load byte-identically vs baselines, same SELF path (task-23), 1 parse not 9. NON-GOAL (separate tasks): SceDynamic / DT_SCE_* / NID decode (task-27, L3 second impl); per-module base allocation + real module ids / killing hardcoded base 0x400000 + module id 1 (the L4 task).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 L1: extract_elf reshaped to container::open(raw) -> Container{kind, elf_bytes, meta}; SELF container metadata RETAINED in ContainerMeta (not discarded); ELF passthrough + no-decryption rejection errors unchanged; match-on-magic dispatch (no plugin registry)
- [x] #2 L2: a single goblin parse per load into an owned ParsedImage; PlainElf backed by it; the 9 ExecutableImage methods are cheap accessors (form A — trait + all callers unchanged); no goblin::Elf::parse per method (1 parse, not 9)
- [x] #3 L3 seam: DynamicSource trait introduced with StdDynamic as the sole impl (today's goblin DT_* path extracted verbatim); Import/Export/Relocation shaped source-agnostic so task-27 SceDynamic slots in without touching the linker
- [x] #4 per-method map_err(io::Error) boilerplate unified; loader errors flow through LoaderError
- [x] #5 ZERO behavior change: six examples load byte-identically vs baselines; clippy -D warnings + fmt + cargo test clean
- [x] #6 NON-GOAL guard: NO SceDynamic/DT_SCE_*/NID (task-27); NO per-module base or module-id changes (L4 task); scope stays L1+L2 refactor + L3 trait stub
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Landed 2026-07-10 (worktree, uncommitted for review). L1 container.rs (renamed from self_container.rs, SELF parse + tests preserved): container::open(raw)->Container{kind,elf_bytes,meta}; match-on-magic (RawElf passthrough / SELF unwrap), no registry; ContainerError ports SelfError variants verbatim (same encrypted/compressed/unknown-magic/too-short rejection). ContainerMeta retained: num_segments=Some (already-read), sce_program_type=Some (SELF key_type/category u16 @0x08, coarse eboot/prx/lib discriminator); module_attributes=None (real word lives in SCE program-info/PT_SCE_PROCPARAM, not parsed by current unwrap; doc-5 Q5 grow-per-consumer). L2 image.rs: ParsedImage::parse does ONE goblin parse into owned fields (entry/memory_size/segments/sections/tls/libraries/imports/exports/relocations+meta); PlainElf holds ParsedImage, 9 ExecutableImage methods are cheap clones (form A, linker.rs+process.rs signatures unchanged). L3 dynamic.rs: DynamicSource trait (imports/exports/relocations/libraries)->LoaderError with StdDynamic sole impl (goblin dynsyms/dynrelas/pltrelocs verbatim); Import/Export/Relocation source-agnostic for task-27 SceDynamic. map_err(io::Error) boilerplate folded into LoaderError (added Container #[from] variant). process.rs rewired: fs::read->container::open->ParsedImage::parse->PlainElf::new. Verify: build green; cargo test green (loader 11 pass +1 ignored, incl. ported SELF tests + self_extraction_retains_container_metadata + parse_matches_direct_goblin_for_example); clippy -D warnings clean (only pre-existing 9 SDK-not-found warns); fmt clean. SINGLE-PARSE PROOF: grep Elf::parse in crates/loader+kernel src -> exactly one non-test hit at image.rs ParsedImage::parse; none in any ExecutableImage method. ORACLE: run_examples.sh check -> hello_world+ps4-mmap OK; other 4 differ ONLY by the headless 'Failed to initialize Vulkan: Unable to find a Vulkan driver' line, which reproduces identically on pristine main (verified: worktree binary vs main binary normalized output for ps4-fs is IDENTICAL). Zero guest-behavior change. NON-GOALS untouched: no SceDynamic/DT_SCE_*/NID; linker base 0x400000 + module id 1 + DtpMod64/DtpOff64 literals left as-is.
<!-- SECTION:NOTES:END -->
