---
id: TASK-23
title: 'loader: parse decrypted SELF containers (no decryption) — extract inner ELF'
status: Done
assignee: []
created_date: '2026-07-10 18:29'
updated_date: '2026-07-10 20:08'
labels:
  - bloodborne
dependencies: []
priority: medium
ordinal: 23000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
SELF (Signed ELF) is the PS4 executable container; retail games (Bloodborne, the north star per decision-3) ship as FSELF. HARD CONSTRAINT: unemups4 operates ONLY on already-decrypted files and MUST NOT contain any decryption — no crypto keys, no SAMU emulation. That is out of scope both legally and by project ethos ("trusted, unencrypted"); shadPS4 also removed decryption. Today the loader (crates/loader, goblin) takes a bare ELF already extracted from a SELF. This task adds the ability to accept a decrypted-but-still-SELF-wrapped file: detect the SELF magic, parse the SELF header + segment table, locate and extract the inner ELF image, then hand it to the existing goblin loader path unchanged. Encrypted input must be rejected with a clear 'file must be decrypted first' error — never an attempt to decrypt. This is a parallel workstream to the phase-2 GPU tasks (20/21/22): it does NOT block them (homebrew is plain-ELF and Gnm is reached via NID imports), but it is on the critical path to loading a decrypted Bloodborne dump.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 SELF container is detected by magic; header + segment table parsed; the inner ELF is extracted and loaded through the existing goblin/linker path
- [x] #2 a plain (non-SELF) ELF still loads unchanged — input type auto-detected by magic, no regression on the six examples
- [x] #3 NO decryption code, keys, or SAMU logic anywhere in the tree; an encrypted/undecrypted SELF segment fails with an explicit 'must be decrypted first' error, not a decrypt attempt
- [x] #4 docs state the expected input is a user-supplied already-decrypted dump; no guidance on obtaining or decrypting copyrighted titles
<!-- AC:END -->



## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Detect SELF magic 0x4F153D1D at file[0]; parse SCE/SELF header + SELF segment table; reconstruct inner ELF into a Vec<u8> (copy each self-segment to its mapped ELF phdr p_offset) → feed existing PlainElf/goblin path unchanged. Insertion point: crates/kernel/src/process.rs:59 (fs::read → detect → PlainElf::new). Auto-detect by magic: 0x7F454C46 => plain ELF passthrough (no regression on 6 examples); 0x4F153D1D => SELF extract. Reject encrypted/compressed segment with explicit 'must be decrypted first' — NO crypto/keys/SAMU. Real-format reference dump: /home/mikolaj/PS4/CUSA03173/eboot.bin (Bloodborne, inner ELF at 0x120, ET_SCE_DYNEXEC) — local smoke only, never committed (copyright+89M). Committable unit test = synthesized minimal fake-SELF wrapping an example ELF. Worktree-isolated, opus subagent, NO commit.
<!-- SECTION:PLAN:END -->

## Notes

Implementation landed 2026-07-10 (worktree agent-adc941552c426e78f, NOT committed — left for maintainer review).

**Files:** new `crates/loader/src/self_container.rs` (SELF detect + inner-ELF reconstruction, no crypto); `crates/loader/src/lib.rs` (`pub mod self_container;`); `crates/kernel/src/process.rs` (load_executable: `fs::read` → `self_container::extract_elf` → `PlainElf::new`); docs `README.md` + `backlog/docs/status.md` (input = user-supplied already-decrypted file; no decrypt/obtain guidance).

**API:** `pub fn extract_elf(raw: Vec<u8>) -> Result<Vec<u8>, SelfError>`. ELF magic (0x464C457F LE) → passthrough byte-identical; SELF magic (0x1D3D154F LE) → reconstruct; else `UnknownMagic`. `SelfError` variants: `TooShort(&'static str)`, `UnknownMagic(u32)`, `Malformed(String)`, `Compressed{segment}`, `Encrypted` (message contains "must be decrypted first").

**SELF flag bits (source: shadPS4 src/core/loader/elf.h self_segment_header accessors, cross-checked against the real eboot.bin):** blocked/loadable = `flags & (1<<11)` (segment backs an ELF phdr); segment_id = `(flags>>20)&0xFFF` (indexes program headers). Encryption: there is NO reliable per-segment encrypted flag bit in a decrypted dump — detection is structural: after the segment table the inner ELF header must show `7F 45 4C 46`; if it doesn't, the payload is still ciphertext → `Encrypted` ("must be decrypted first"). Compression: the `(flags>>1)&7==2` heuristic FALSELY fires on this plaintext dump (seg flags 0x2804 give 2), so compression is detected the unambiguous way — `compressed_size != uncompressed_size` → `Compressed`. NO crypto/keys/SAMU anywhere (grep-clean; only doc-comments mention "decrypt").

**Fixture strategy:** committable tests synthesize a fake-SELF at test time by wrapping `examples/ps4-helloworld/hello_world.elf` — one blocked segment per program header (id==phdr index), each copying that phdr's file image; e_shoff/e_shnum zeroed (SELF-stripped ELFs have no shdr table) so goblin parses from phdrs. Tests: plain-ELF passthrough byte-identical; fake-SELF extracts to `\x7FELF` + goblin parses phdrs; encrypted fixture → Encrypted/"decrypted first"; compressed fixture → Compressed; junk → UnknownMagic; truncated → TooShort. NO Bloodborne bytes committed.

**Verification (worktree):** `cargo build --release` green. `cargo test` = 25 passed, 1 ignored (real-dump). `cargo clippy --all-targets --all-features -- -D warnings` clean (the 9 warnings are pre-existing OpenOrbis-SDK build-script notices, unrelated). `cargo fmt --check` clean. Examples regression `scripts/run_examples.sh check`: the ONLY diff line across all six examples is `ERROR ps4_gpu::display: Failed to initialize Vulkan: Unable to find a Vulkan driver` (headless env, no Vulkan driver) — zero guest-execution divergence → AC#2 holds.

**Real-dump smoke (manual, #[ignore]d, guarded by file existence — nothing committed):** `/home/mikolaj/PS4/CUSA03173/eboot.bin` (Bloodborne, 89.5M SELF) extracts to a 93,667,368-byte inner ELF starting `\x7FELF`; goblin parses it: e_type=0xFE10 (ET_SCE_DYNEXEC), 10 program headers. Full execution out of scope (needs GPU + hundreds of modules) — success here = clean inner-ELF extraction accepted by goblin.

**Deviations:** (1) compression detected by size inequality rather than a flag bit (flag heuristic mis-fires on real plaintext data). (2) encryption detected structurally (inner-ELF-magic absence) rather than a flag bit, since a decrypted dump has no encrypted marker and this build has no crypto to test with. Both are documented in-file. No commit (per hard rule).
