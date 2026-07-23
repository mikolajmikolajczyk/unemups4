---
id: TASK-225
title: >-
  docs: 48 environment variables, 33 of them undocumented — write the reference
  and retire the dead ones
status: To Do
assignee: []
created_date: '2026-07-22 10:50'
updated_date: '2026-07-22 10:55'
labels:
  - docs
  - dx
dependencies: []
priority: medium
ordinal: 230000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
The emulator is configured almost entirely through environment variables and there is no single place that lists them. Counted across crates/, app/ and tools/: 48 distinct UNEMUPS4_* / X86JIT_* variables, of which only 15 appear anywhere in backlog/docs, README.md or AGENTS.md. The remaining 33 exist solely as a string literal next to a std::env::var call.

The practical cost is already visible: someone debugging a GPU problem cannot discover UNEMUPS4_DUMP_TEX or UNEMUPS4_PM4_TRACE without grepping the source, and a profiling session cannot discover UNEMUPS4_PROFILE_RIP or UNEMUPS4_PROFILE_SLOW the same way. Five of the undocumented ones were added during a single recent session.

This is not only a writing job. Two things need deciding as part of it:

RETIREMENT. Eight variables carry an UNEMUPS4_X_ prefix — X_ADDITIVE, X_CACHE_TRACE, X_FORCE_CONST_REUPLOAD, X_FULL_BARRIER, X_PASS_TRACE, X_RT_ALPHA_MASK, X_RT_CLEAR_ALPHA0, X_SKIP_BLOOM. The naming and the subject matter suggest one-off probes left behind by the bloom and render-target investigations (task-179, task-184). Check each against the code and the tasks that introduced it: a probe whose question has been answered should be deleted, not documented. Documenting a dead knob is worse than leaving it undocumented, because it advertises it as supported.

CLASSIFICATION. The live ones are not all the same kind of thing and the reference should say which is which, because it changes whether a reader should touch them:
- production behaviour (UNEMUPS4_BACKEND, UNEMUPS4_CLOCK, UNEMUPS4_SUPERBLOCKS, UNEMUPS4_REGION_TIER_UP) — change what the emulator does
- escape hatches, existing to back out of a regression rather than to be tuned (UNEMUPS4_SUPERBLOCKS=0, UNEMUPS4_CLOCK=fixed-step)
- measurement (UNEMUPS4_PROFILE and its satellites, UNEMUPS4_WATCHDOG, UNEMUPS4_EXECTRACE)
- artefact dumps (the UNEMUPS4_DUMP_* and UNEMUPS4_SNAPSHOT_* families)
- traces (the *_TRACE family)

For each surviving variable record: accepted values and the default, what it does, what it costs when enabled (several are documented in code as zero-cost when unset — that property is worth stating), and which task or doc it came from.

Where it belongs: backlog/docs/commands.md already carries the profiling and snapshot sections and is the natural home, unless the table grows large enough to deserve its own doc. Cross-link from AGENTS.md so an agent finds it without grepping.

Note X86JIT_* variables are read by the pinned x86jit dependency, not by this repo (X86JIT_PERF_MAP, X86JIT_OPT_LEVEL). List them as such, with a pointer that their contract lives in that repo.

ENFORCEMENT, so the reference cannot rot. A written list of 48 variables is out of date the first time someone adds a knob without touching docs — which is exactly how the current 33-undocumented gap accumulated. Add a pre-commit hook that fails when a variable read in the code is missing from the reference (and, in the other direction, when the reference names one nothing reads any more).

Follow the existing custom-hook shape: scripts/check-layer-bans.sh and scripts/uid-guard.sh are `language: script` entries in .pre-commit-config.yaml with `pass_filenames: false` and a `files:` filter. Vendor the script in scripts/ so the repo stays self-contained, and keep the failure message actionable — it should name the offending variable and the file it was found in, not just exit non-zero.

Two details worth getting right, because they decide whether the hook is trusted or worked around:
- match how the variables are actually read, not a guessed regex. Today they are string literals next to std::env::var, mostly bound to a `pub const *_ENV: &str` first; a check that only greps for `env::var("...")` would miss those.
- X86JIT_* are read by the pinned dependency, not by this repo, so they must be excluded from the code side of the check while still being listed in the reference.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 every live environment variable is documented with accepted values, default, effect, cost when enabled, and its originating task
- [ ] #2 each UNEMUPS4_X_* probe is either deleted as answered or documented with a reason for keeping it — no dead knob is documented as if supported
- [ ] #3 variables are classified (production / escape hatch / measurement / dump / trace) so a reader can tell which are safe to change
- [ ] #4 the reference is discoverable from AGENTS.md; X86JIT_* are listed as belonging to the pinned dependency
- [ ] #5 a grep for env::var across crates/, app/ and tools/ finds nothing missing from the reference
- [ ] #6 a pre-commit hook fails the commit when a variable read in the code is missing from the reference, or the reference names one nothing reads; it names the offending variable and file
<!-- AC:END -->
