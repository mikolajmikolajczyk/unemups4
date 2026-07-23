---
id: TASK-190
title: >-
  gcn: regen.sh output drifts with the LLVM version — regenerating the corpus
  silently changes committed bytes
status: To Do
assignee: []
created_date: '2026-07-20 18:41'
labels:
  - gcn
  - tooling
  - tests
dependencies: []
priority: low
ordinal: 194000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Running crates/gcn/tests/corpus/regen.sh under the LLVM currently on this machine regenerates cbranch_select_ps.code.bin as 80 bytes where the committed file is 84. The script documents that it was verified against LLVM 22, so this is toolchain drift rather than a defect in our code — but the consequence is that nobody can safely run regen.sh today: doing so would silently replace corpus bytes with different ones, and the .dis goldens were hand-verified against the ORIGINAL bytes.

That matters more than a stale artifact normally would, because of how this corpus earns its trust. The goldens are deliberately NOT blessed from the decoder under test — they are read off the .s assembly by hand, precisely so a decoder bug cannot pin itself in place. task-182 showed that failing exactly: two goldens had recorded abs-stripped text while their own .s sources read |v1|, so the regression tests were preserving the bug. If regeneration can quietly change the bytes underneath a hand-verified golden, that safeguard weakens.

Found while implementing task-188; the agent reverted the regenerated file so it stayed out of that diff.

Establish what actually differs first — whether the newer LLVM encodes an instruction differently, drops a nop/padding, or the script extraction picks up something version-specific. The answer decides the fix: pin the toolchain, make the script version-tolerant, or re-verify and re-commit the goldens against current output.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 The cause of the 84-vs-80 byte difference is identified, not merely worked around
- [ ] #2 regen.sh either produces byte-identical output to what is committed, or the corpus is re-verified against current output and the goldens re-checked BY HAND against the .s sources (never blessed from the decoder)
- [ ] #3 The LLVM version requirement is stated and, if practical, checked by the script so a mismatched toolchain fails loudly instead of silently rewriting corpus bytes
- [ ] #4 build + cargo test + clippy clean
<!-- AC:END -->
