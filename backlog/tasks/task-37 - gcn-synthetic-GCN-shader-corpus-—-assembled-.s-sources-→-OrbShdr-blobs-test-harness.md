---
id: TASK-37
title: >-
  gcn: synthetic GCN shader corpus — assembled .s sources → OrbShdr blobs + test
  harness
status: Done
assignee: []
created_date: '2026-07-11 12:53'
updated_date: '2026-07-11 18:15'
labels:
  - gpu
  - gcn
dependencies:
  - TASK-36
priority: medium
ordinal: 36000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Testability backbone. Committed corpus of hand-written GCN (GFX7/Sea-Islands): pass-through VS (positions via V# buffer_load_format_xyzw, exp pos0 + exp param0), flat + interpolating color PS (v_interp_p1/p2_f32, exp mrt0), assembled via `llvm-mc -triple amdgcn -mcpu=bonaire` (or hand-encoded dword arrays where llvm-mc unavailable), wrapped in OrbShdr headers by a tiny in-repo test-only builder (crates/gcn/tests/ or xtask). Commit BOTH .s source AND blob bytes — fully self-authored, ZERO copyright, no psbc. Feeds P4-03/04/05/06 + P4-19. Does NOT pull freegnm/psbc; does NOT need OO SDK.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 headless: ≥3 corpus shaders (VS, flat PS, interp PS) with committed .s + OrbShdr blobs, loadable by P4-01 parser
- [ ] #2 headless: cargo-test harness enumerates corpus, asserts header integrity (magic/type/length vs assembly)
- [ ] #3 module doc records regen (llvm-mc invocation) + no-copyrighted-assets rule
- [ ] #4 Corpus blobs are GENUINE runnable OrbShdr (valid header + real GCN at addressable locations), so a Tier-C ELF built from them runs on an external emulator (task-58), not just our own parser
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
DONE 2026-07-11 (feat/task-37 @<prior-history>, merged <prior-history>). 3 corpus shaders in crates/gcn/tests/corpus/: passthrough_vs (V# buffer_load_format_xyzw→exp pos0+param0), flat_color_ps (const RGBA→exp mrt0), interp_color_ps (v_interp_p1/p2→exp mrt0). Each committed as .s + .code.bin + .sb. Assembled via llvm-mc 22.1.6 -triple amdgcn -mcpu=bonaire (regen.sh records invocation + no-copyright rule). OrbShdr wrapper builder in corpus.rs (single source for 28-byte header). AC#1 loadability test lives in crates/gnm/tests/corpus_load.rs (only place parser+corpus meet without gcn←gnm cycle) — drives all 3 through real parse_sb. Real GCN, s_endpgm-terminated (guarded). All 4 ACs ticked. Verify: build 0, ps4-gcn 3+1ign, clippy 0, fmt clean, gcn tree Vulkan-free. Combined main gate: 28 suites ok, oracle 6/6.
<!-- SECTION:NOTES:END -->
