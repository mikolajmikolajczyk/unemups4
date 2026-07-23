---
id: TASK-27
title: >-
  loader: parse PS4 SCE dynamic table + NID symbols (goblin can't) — Bloodborne
  dynamic-link blocker
status: Done
assignee: []
created_date: '2026-07-10 19:48'
updated_date: '2026-07-10 21:17'
labels:
  - bloodborne
  - loader
dependencies:
  - TASK-26
priority: high
ordinal: 27000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
SURFACED by task-23 smoke: extract_elf pulls a 93.6MB inner ELF from Bloodborne eboot.bin and goblin parses its 10 program headers — but ONLY phdrs. The dynamic-linking path is almost certainly broken for retail: PlainElf::imports/exports/relocations read goblin's elf.dynsyms/dynrelas/dynstrtab, which decode the STANDARD DT_* dynamic tags. PS4 ET_SCE_DYNEXEC binaries use SCE-SPECIFIC dynamic tags (DT_SCE_PLTGOT/DT_SCE_JMPREL/DT_SCE_PLTRELSZ/DT_SCE_RELA/DT_SCE_RELASZ/DT_SCE_SYMTAB/DT_SCE_SYMTABSZ/DT_SCE_STRTAB/DT_SCE_STRSZ/DT_SCE_HASH...) and NID-hashed symbol names (base64-ish 'name#lib#module' encoded, resolved via the 64-bit NID hash) rather than plain strings. Goblin does not know these tags, so on a retail eboot elf.dynsyms/dynrelas come back empty/wrong → zero imports resolved → nothing links. FIRST STEP: confirm the failure mode against the real eboot (dump goblin's dynsyms/dynrelas len on the extracted Bloodborne ELF — expect empty/garbage), then implement a PS4 dynamic-segment parser: walk PT_DYNAMIC / PT_SCE_DYNLIBDATA, read the SCE dynamic tags, parse the SCE symbol/rela/string tables, decode NID-encoded import/export names, and feed the existing ExecutableImage import/export/relocation model. Homebrew (current 6 examples) uses standard tags and MUST keep working — auto-detect SCE vs standard dynamic. This is the loader half of running a real game; the GPU roadmap is the other half. NON-GOAL: module/.prx loading (sce_module/*.prx), NID-name database completeness — separate follow-ups.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 Confirmed failure mode: goblin's standard dynamic parse yields empty/incorrect imports+relocs on the extracted Bloodborne inner ELF (documented before the fix)
- [x] #2 PS4 SCE dynamic tags (DT_SCE_*) parsed from PT_DYNAMIC/PT_SCE_DYNLIBDATA; SCE symbol/rela/string tables read without goblin
- [x] #3 NID-encoded symbol names decoded into the existing Import/export model; relocations resolved through the current linker path
- [x] #4 Standard-ELF homebrew (six examples) auto-detected and unchanged — no regression vs baselines
- [x] #5 clippy -D warnings + fmt + cargo test clean; NO decryption/keys (inherits task-23 constraint)
<!-- AC:END -->

## Implementation Notes
<!-- SECTION:NOTES:BEGIN -->
Done 2026-07-10 (L3 SceDynamic + NID). Landed on `crates/loader`: new `nid.rs`
(forward hash + `NidDatabase` seam + `encode_id`), `SceDynamic` added beside
`StdDynamic` in `dynamic.rs`, auto-select in `image.rs::parse` via `is_sce_image`.

DT_SCE_* tag constants (source: shadPS4 `src/core/loader/elf.h`, OpenOrbis
"PS4 ELF Specification — Dynlib Data"): SYMTAB 0x61000039 / SYMTABSZ 0x6100003f,
STRTAB 0x61000035 / STRSZ 0x61000037, RELA 0x6100002f / RELASZ 0x61000031,
JMPREL 0x61000029 / PLTRELSZ 0x6100002d, PLTGOT 0x61000027, HASH 0x61000025,
MODULE_INFO 0x6100000d, NEEDED_MODULE 0x6100000f, IMPORT_LIB 0x61000015,
EXPORT_LIB 0x61000013. PT_SCE_DYNLIBDATA 0x61000000, PT_SCE_PROCPARAM 0x61000001.
Table offsets are relative to the PT_SCE_DYNLIBDATA segment file offset; the dyn
tag array is in PT_DYNAMIC. Sym = Elf64_Sym (24B), Rela = 24B, r_sym=r_info>>32,
r_type=r_info&0xffffffff. Library/module d_val: name_offset=bits0..31,
id=bits48..63 (shadPS4 module.h).

NID forward hash (shadPS4 `StringToNid`/OpenOrbis): SHA-1(name || salt), salt =
518D64A635DED8C1E6B039B1C3E55230 (16B), first 8 bytes as LE u64, then 10 chars
`codes[(v>>(58-i*6))&0x3f]` + final `codes[(v&0xf)*4]`, Sony base64
`ABC…xyz0…9+-`. Known-pair test (authoritative oracle: idc/ps4libdoc
libkernel.sprx.json): sceKernelAllocateDirectMemory→rTXw65xmLIA,
sceKernelUsleep→1jfXLRVzisc, sceKernelLoadStartModule→wzvqT4UqKX8 — all match.
(CORRECTED by task-30: there is NO NID-scheme mismatch. ps4-syscalls' build.rs
`calculate_nid` uses the SAME salt and SHA-1(name||salt)[0..8]-as-LE-u64 → Sony
base64 as this bit-slice, so it produces the SAME canonical 11-char NID. The
generated `MAP_BY_NID` therefore IS canonical — e.g. rTXw65xmLIA→sceKernelAllocateDirectMemory,
1jfXLRVzisc→sceKernelUsleep — and retail import resolution goes straight through
it via `SyscallId::from_nid`. task-30 removed the duplicate `nid::nid_for`.)

open-Q6 resolution: imports resolve SCOPED to (library, NID). Encoded name is
`nid#libEncId#modEncId`; split on `#`, map libEncId (via encode_id of the
DT_SCE_IMPORT_LIB/EXPORT_LIB id) → library name, populating Import.lib_name
(finally non-empty). Reverse NID→name DB deferred (diagnostics-only, K2/open-Q2).
[task-30: import resolution needs NO forward hash at all — the raw on-disk NID in
Import/Relocation.symbol_name is looked up directly in the canonical MAP_BY_NID.]

Real eboot smoke (/home/mikolaj/PS4/CUSA03173/eboot.bin, ET_SCE_DYNEXEC 0xfe10,
never committed): goblin dynsyms=0 dynrelas=0 pltrelocs=0 (failure mode
confirmed, AC#1). SceDynamic decodes imports=701, relocs=234990, libraries=43.
Sample decoded (lib_name, NID→name): libSceAjm dl+4eHSzUu4→sceAjmInitialize,
libSceAjm Q3dyFuwGn64→sceAjmModuleRegister, libSceAjm -qLsfDAywIY→sceAjmBatchWait.
701 imports decoded, 449 sce*-prefixed (HLE-shaped). [task-30 verified end-to-end
HLE-registry resolution: the raw retail NID resolves directly via the canonical
MAP_BY_NID through hle.rs's `def.id.nid()`-keyed exports + `resolve_symbol` — e.g.
Bloodborne import 1G3lF1Gg1k8 (sceKernelOpen) → HLE stub. K4: lazy-stub path is
source-agnostic and unchanged for genuinely-unimplemented NIDs.]

Verification: cargo build --release green; cargo test green (loader 21 passed,
2 ignored = real-dump smoke tests); clippy --all-targets --all-features
-D warnings exit 0 (the 9 "warnings" are ps4-syscalls build.rs SDK-missing
notices, pre-existing); cargo fmt clean. Oracle scripts/run_examples.sh check:
identical FAIL set (ps4-fs/ps4-tls/ps4-thread-testing/ps4-softgpu) on BOTH clean
main and this branch — the only divergence is the pre-existing headless
"ps4_gpu::display: Failed to initialize Vulkan: Unable to find a Vulkan driver"
env artifact; hello_world + ps4-mmap OK on both. Zero added divergence →
homebrew byte-identical. No-decryption grep clean: only `sha1` added (public NID
hash), no keys/SAMU/cipher/decrypt.
<!-- SECTION:NOTES:END -->
