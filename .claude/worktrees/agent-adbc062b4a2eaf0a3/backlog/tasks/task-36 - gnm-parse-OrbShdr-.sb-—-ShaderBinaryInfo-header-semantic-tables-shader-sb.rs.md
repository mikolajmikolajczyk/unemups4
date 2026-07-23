---
id: TASK-36
title: >-
  gnm: parse OrbShdr .sb — ShaderBinaryInfo header + semantic tables
  (shader/sb.rs)
status: Done
assignee: []
created_date: '2026-07-11 12:53'
updated_date: '2026-07-11 17:42'
labels:
  - gpu
  - gnm
dependencies: []
priority: medium
ordinal: 35000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Build crates/gnm/src/shader/sb.rs (doc-4 §1 module-tree entry): parse "OrbShdr" magic, m_type (stage), m_length (24-bit GCN code size), hashes/crc, SRT flags, and VertexInputSemantic/VertexExportSemantic/PixelInputSemantic/PixelSemanticMapping tables (doc-3 §3.3; refs GPCS4 GcnShaderBinary.h, fpPS4 ps4_shader.pas). Given ShaderRef::GcnBinary{addr} + &dyn VirtualMemoryManager, locate the shader-setup register block, return typed SbShader{stage,code_range,semantics}. Resolve the .sb address derivation from SPI_SHADER_PGM_LO/HI (addr>>8) for P4-09. Does NOT decode GCN, does NOT touch provider chain, rejects encrypted/garbage cleanly (NEVER decrypts — hard constraint). ps4-gnm stays Vulkan-free.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 headless: parses hand-built in-test OrbShdr VS+PS blob into correct stage/length/semantic tables (round-trip units)
- [x] #2 headless: malformed (bad magic/truncated/length past buffer) → Err, no panic/OOB
- [x] #3 headless: doc-comment + fixture documents PGM_LO/HI→.sb address derivation for P4-09
- [x] #4 ps4-gnm no new deps beyond ps4-core
<!-- AC:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. Add shader/sb.rs: SbShader{stage,code_range,semantics} + SbStage enum + semantic table structs (VertexInputSemantic/VertexExportSemantic/PixelInputSemantic/PixelSemanticMapping).
2. Parse ShaderBinaryInfo (28-byte OrbShdr header): magic 'OrbShdr', version, bitfields (m_pssl_or_cg/m_cached/m_type/m_source_type/m_length 24-bit), chunkUsageBaseOffsetInDW, numInputUsageSlots, SRT flags, hash0/1, crc32. Header sits AFTER GCN code; located by scanning for 'OrbShdr' magic (fpPS4/GPCS4 layout).
3. Public entry: parse_sb(addr,&dyn VirtualMemoryManager) -> Result<SbShader,SbParseError>; bounds-safe reads, reject bad magic/truncated/length-past-buffer with clean Err (NEVER decrypt).
4. Doc-comment + fixture: PGM_LO/HI -> addr = (hi:lo)<<8 derivation for task-44 (P4-09).
5. mod sb; in shader/mod.rs only. Hand-built in-test OrbShdr VS+PS blobs for round-trip + malformed tests.
Verify: cargo build --release; cargo test -p ps4-gnm; cargo test; clippy -D warnings; fmt --check; run_examples.sh check; cargo tree no vulkan deps.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Done 2026-07-11. Added crates/gnm/src/shader/sb.rs (mod wired in shader/mod.rs only). Parses OrbShdr ShaderBinaryInfo (28B header: magic/version/bitfield word m_type+m_length24/flags/hashes/crc32) via forward magic-scan from PGM code_start, validated by code_start+m_length==header_addr. Public API: parse_sb(code_start,&dyn VMM)->Result<SbShader,SbParseError>, pgm_addr(lo,hi)=((hi<<32)|lo)<<8 (P4-09), SbStage, ShaderBinaryInfo, Semantics + VertexInput/VertexExport/PixelInput/PixelSemanticMapping structs, parse_vs_semantics/parse_ps_semantics (gnmx block, for task-43/44). Never decrypts: no plaintext magic -> clean MagicNotFound. 15 unit tests + doctest, all hand-built in-test blobs. Verify: build/clippy-Dwarnings/fmt clean; cargo test 131 passed; ps4-gnm 60 tests green; run_examples 6/6; cargo tree ps4-gnm has no ash/winit/vulkan. No commit (maintainer lands).
<!-- SECTION:NOTES:END -->
