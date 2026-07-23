---
id: TASK-28
title: >-
  loader: per-module base + real module ids (L4) — wire the dead
  ModuleAllocator, kill hardcoded base+id
status: Done
assignee: []
created_date: '2026-07-10 20:28'
updated_date: '2026-07-10 21:41'
labels:
  - refactor
  - loader
  - bloodborne
dependencies:
  - TASK-26
priority: high
ordinal: 28000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
SPEC = doc-5 (loader architecture) layer L4 + migration step 3 (pure plumbing). Read doc-5 first. Maintainer decision 2026-07-10: file now, run after task-26 — do the multi-module foundation upfront so .prx becomes additive, not a linker retrofit ('do it right so real games don't bite us'). Today linker.rs::load_executable bakes in the single-module world: base_addr = 0x400_000 is a CONSTANT (ModuleManager constructs ModuleAllocator::new(0x400000) but nothing ever calls .allocate() — it is DEAD CODE); DTPMOD/DtpOff64 relocations write module id 1 literally. Reshape into load_image(mgr, mem, &ParsedImage, name) -> ModuleHandle: id = mgr.get_next_handle(); base = mgr.allocator.allocate(memory_size) (allocator FINALLY used); map segments at base+offset; apply relocs with THIS base + id; DTPMOD writes the real id (not literal 1); register exports shifted by base. Keep the allocator start at 0x400_000 so homebrew loads at the SAME address (no regression) but the value now COMES FROM allocate() so a second module gets the next slot automatically. cross-module resolve_symbol already iterates all modules — the seam is 'load N images, resolve across them', which real ids + the allocator + existing resolve_symbol permit. Still ONE image loaded in practice (corpus is single-module) — this is pure plumbing; the payoff is the .prx task becomes a small addition. NON-GOAL (the later .prx task): .prx file discovery, dependency-ordered load loop, sceKernelLoadStartModule, full DTV/__tls_get_dynamic across modules.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 load_executable reshaped to a load_image that takes the module's base from ModuleAllocator.allocate (the dead allocator is now called), not the 0x400_000 constant; allocator start stays 0x400_000 so the six examples load at the same address (byte-identical vs baselines)
- [x] #2 DTPMOD64 (and DtpOff64/TLS offsets) write the real ModuleHandle from get_next_handle(), not the literal 1
- [x] #3 each loaded image is registered with its allocated base + shifted exports before a dependent relocates; cross-module resolve_symbol path exercised (even with one module)
- [x] #4 ZERO behavior change for the corpus: six examples byte-identical; clippy -D warnings + fmt + cargo test clean
- [x] #5 NON-GOAL guard: NO .prx discovery, NO dependency graph, NO sceKernelLoadStartModule, NO multi-module DTV — those stay in the later .prx task
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
L4 base+id plumbing landed (In Progress; maintainer finalizes Done on merge). FLOW: load_image(mgr,mem,img,name)->ModuleHandle now allocates module_id=get_next_handle() and base=allocator.allocate(total_size) (dead allocator finally called); load_executable kept as thin wrapper returning entry_point for process.rs (caller untouched). Allocator start stays 0x400_000 in manager.rs::ModuleManager::new (single owned range decision; proven non-colliding for the main image), so FIRST image still lands at 0x400_000 (verified raw log: Loading 'eboot.bin' at 0x400000) — no regression; a 2nd image gets the next 0x4000-aligned slot. DTPMOD/DTPOFF FIX: was hardcoded 'let val = 1usize' / write 1; now both write module_id (real ModuleHandle). Corpus main module id is 1 numerically so byte-identical, but sourced from the handle. RESOLVE BUG FOUND+FIXED: Module::exports stores ABSOLUTE addrs (load_image base-shifts before insert; HLE stubs already absolute w/ base 0) but resolve_symbol added base_addr AGAIN — double-counted base for real modules. Only latent because single-module homebrew resolves its own symbols via symbol_value, never cross-module by name; HLE base=0 hid it. Fixed resolve_symbol to return the stored absolute value; behavior-neutral for corpus. CROSS-MODULE SEAM TEST: three new unit tests in linker.rs w/ a MockMemory VMM + FakeImage: (1) first_image_base_comes_from_allocator_at_0x400000 (base from allocate, 2nd image past 1st), (2) dtpmod_writes_real_module_id_not_literal_one (loads a 2nd module, asserts DTPMOD/DTPOFF write id 2 not 1), (3) second_module_resolves_symbol_exported_by_first (2nd module Absolute64 reloc resolves to 1st module's base-shifted export). VERIFICATION: cargo build --release green; cargo test 28 passed 1 ignored (12 in ps4-loader); clippy --all-targets --all-features -D warnings clean (9 warnings are ps4-syscalls build-script SDK-missing, env-only); cargo fmt no drift. ORACLE scripts/run_examples.sh check: only diff vs baselines is the known headless 'Failed to initialize Vulkan: Unable to find a Vulkan driver' line (baselines not re-captured on main); ZERO loader-related diff (module-id log routed to debug! to keep info-level baseline byte-identical). hello_world+ps4-mmap fully OK. The task-20 'HLE: Loaded libSceGnmDriver.so' known diff is absent because task-20 is not on this branch. FILES: crates/loader/src/linker.rs (load_image+id+allocator+DTPMOD/DTPOFF id+wrapper+tests), crates/loader/src/manager.rs (resolve_symbol double-base fix). NON-GOALS untouched: no .prx discovery, no dep graph, no sceKernelLoadStartModule, no multi-module DTV.
<!-- SECTION:NOTES:END -->
