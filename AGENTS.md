# AGENTS.md — unemups4

Repo-specific notes for coding agents (Claude Code, Cursor, Aider, Copilot, …). Generic software-engineering advice is out of scope.

> **CLAUDE.md** at repo root is `@AGENTS.md` plus Claude-only overrides. Other agents read this file directly.

## What this is

unemups4 is a lightweight, educational **PlayStation 4 emulator written in Rust** (edition 2024), licensed **GPL-3.0** (see `COPYING`). It runs trusted, unencrypted PIE x86-64 ELF homebrew via high-level emulation (HLE): guest code executes on the **x86jit** x86-64 engine (`Vm`/`Vcpu`) over an identity-mapped address space (guest addr == host addr, via x86jit `guest_base`), library imports (`sceKernel*`, `scePad*`, …) resolve to `SYSCALL` stubs that trap out as `Exit::Syscall` into Rust handlers, and software-rendered guest output is presented through Vulkan. It's a research/learning project, not a faithful or secure reimplementation — no SELF decryption and no real GNM/Liverpool GPU.

## Where things live

| Need | Path | When to load |
|------|------|--------------|
| **Source of truth for roadmap, tasks, backlog** | Backlog.md — `backlog task list --plain` | Always. **Don't read roadmaps from markdown.** |
| Current repo shape, data flow, file map | [`backlog/docs/architecture.md`](backlog/docs/architecture.md) | When making structural changes or unfamiliar with module layout |
| **The project's story** — the development arc, breakthroughs, setbacks, dead ends | [`backlog/docs/doc-7 - The-unemups4-story-—-a-development-history.md`](<backlog/docs/doc-7 - The-unemups4-story-—-a-development-history.md>) | Onboarding, or to understand *why* the project is the way it is. **KEEP CURRENT:** append a chapter when a new act happens. |
| Coding conventions, file naming, commit style, comment policy | [`backlog/docs/conventions.md`](backlog/docs/conventions.md) | Before writing or modifying code |
| Feature status (what works, what's in flight, what's broken) | [`backlog/docs/status.md`](backlog/docs/status.md) | When user asks "does X work?" or you're picking up work |
| Common dev commands (build, test, run, typecheck, lint) | [`backlog/docs/commands.md`](backlog/docs/commands.md) | When running build/test/dev loops |
| Tooling (devShell, direnv, pre-commit, static analysis) | [`backlog/docs/dev-setup.md`](backlog/docs/dev-setup.md) | When fixing tooling, adding hooks, or onboarding |
| Working on tasks (statuses, branch naming, session handoff) | [`backlog/docs/working-on-tasks.md`](backlog/docs/working-on-tasks.md) | Before picking up a task |
| Where to capture decisions (`backlog decision` vs task note) | [`backlog/docs/decisions.md`](backlog/docs/decisions.md) + `ls backlog/decisions/` | When making a non-trivial decision |
| Project glossary / domain terminology | [`backlog/docs/glossary.md`](backlog/docs/glossary.md) | When you hit an unfamiliar term |
| Things deliberately deferred — do NOT implement unprompted | [`backlog/docs/deferred.md`](backlog/docs/deferred.md) | Before adding features that "seem missing" |
| Retail bring-up method (smoke loop, diagnostics, wall taxonomy) + worked casebook | [`backlog/docs/doc-4 - Retail-title-bring-up-—-the-smoke-loop-method.md`](<backlog/docs/doc-4 - Retail-title-bring-up-—-the-smoke-loop-method.md>) + [`doc-5 casebook`](<backlog/docs/doc-5 - Retail-bring-up-casebook-—-worked-debugging-examples.md>) | When bringing up a retail title or debugging a hard guest-side wall. **KEEP CURRENT:** after you clear a *new class* of wall whose lesson generalizes, append a case to doc-5 (and, if it's a new shape, a row to doc-4's taxonomy table) as part of that fix's commit. |
| Retail GPU/GNM bring-up discovery log (how the GPU submission path was reverse-engineered; the workload model explained) | [`backlog/docs/doc-6 - Retail-GNM-bring-up-—-discovery-log-how-the-GPU-path-was-reverse-engineered.md`](<backlog/docs/doc-6 - Retail-GNM-bring-up-—-discovery-log-how-the-GPU-path-was-reverse-engineered.md>) | When working on retail graphics/GNM bring-up. **KEEP CURRENT:** append a dated entry (trigger → hunt → mechanism → consequence) every time you uncover a new GPU mechanism or a surprise, as part of that change's commit. |
| Backlog skill (`backlog` CLI + task/doc/decision workflow) | [`.agents/skills/backlog/SKILL.md`](.agents/skills/backlog/SKILL.md) | Auto-loaded by the backlog skill trigger; also when driving `backlog` manually |

> **Skills location.** Canonical: `.agents/skills/<name>/` (agent-agnostic, **vendored/committed**). `.claude/skills/` are symlinks created by `scripts/skills-bootstrap.sh` so Claude Code can auto-trigger them. Skills are not fetched from anywhere — to refresh the symlinks (e.g. after adding a skill), re-run `scripts/skills-bootstrap.sh`.

## Load-on-demand rule

Don't read every `backlog/docs/` file at session start. Pick the file matching the task — they are sized to be loaded individually. The table above tells you *when* to load *what*.

## Session handoff

When ending a session mid-task, record what's done, what's next, and any blocker on the active task:

```sh
backlog task edit <id> --notes "Session pause $(date -I). Done: <X>. Next: <Y>. Blocker: <Z|none>."
```

When starting a session, read the most-recently-touched in-progress task (`backlog task list -s "In Progress" --plain`, then `backlog task <id> --plain`) before doing anything else. Local, versioned in-repo, agent-agnostic.

Details: [`backlog/docs/working-on-tasks.md`](backlog/docs/working-on-tasks.md).

## Working on tasks

This repo tracks work **locally** with [Backlog.md](https://github.com/MrLesk/Backlog.md) — tasks are markdown under `backlog/`, no external tracker. Read [`.agents/skills/backlog/SKILL.md`](.agents/skills/backlog/SKILL.md) (and run `backlog instructions overview`) before driving `backlog`. It's **forge-agnostic**: git is plain branches merged into the default branch; a remote is an optional mirror, not an issue tracker. This repo's `origin` is GitHub (`github.com/mikolajmikolajczyk/unemups4`) — a **code mirror only**; don't use GitHub issues for tracking, and never push without explicit user request.

## Quick dev loop

```sh
cargo build --release
cargo run --release -p unemups4 -- path/to/homebrew.elf   # e.g. examples/*/*.elf
cargo test
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt
```

Full list in [`backlog/docs/commands.md`](backlog/docs/commands.md).

## Hard rules (don't violate)

- **Never commit without explicit user request.** Even mid-task, after accepting a plan, stop and ask. Acceptance of plan ≠ acceptance of commit.
- **Don't add features, refactor, or introduce abstractions beyond what the task requires.** Bug fix = bug fix, not surrounding cleanup.
- **Don't pre-empt later milestones.** If a task is scoped to a later milestone (Backlog.md label/milestone), don't half-implement it during earlier work.
- **All project docs live under `backlog/docs/`.** That's the single knowledge tree (tasks, docs, and decisions all live under `backlog/`).
- **Provenance — derive facts only from clean primary sources.** This is an original-work, GPL-3 project (clean-room). Every PS4/hardware/OS fact — GCN opcodes and semantics, PM4 packets, T#/V#/S# descriptor layouts, register offsets, tiling, SELF/OELF/dynamic layout, NID hashing, syscalls/errno — must be derived from and cite a **clean primary source**: the AMD GCN ISA manuals, Mesa `src/amd`, the Linux kernel AMD/radeon headers, the OpenOrbis SDK + OELF spec, the Vulkan spec, FreeBSD (the Orbis OS base), the real PS4 console capture, or the dumped guest binary. Reproduce a fact against ground truth and pin it with a witness test. **Do not copy from, transcribe, or consult another PS4 emulator's source, headers, or docs.** Comments and tests cite the clean source and state what it is — forward-only; they never narrate what a fact is *not* derived from.

## Reproducing the clean-oracle stash (`oracles/`)

The hardware/OS facts in the tree are cited to clean primary sources (AMD GCN ISA, Mesa, the Linux kernel AMD headers, the OpenOrbis SDK + OELF spec, FreeBSD, the real PS4 console capture). Those sources are reproduced locally under **`oracles/`** — only the reproducer (`fetch-oracles.sh`) and the manifest (`MANIFEST.md`) are committed; the fetched content (AMD PDFs [copyright], Mesa/OpenOrbis clones, kernel/FreeBSD headers) is gitignored. Anyone can rebuild the stash on a fresh checkout:

```sh
./oracles/fetch-oracles.sh fetch     # download all sources to pinned refs, write SHA256SUMS
./oracles/fetch-oracles.sh verify    # sha256 of fixed files + git HEADs vs pinned commits
```

`oracles/MANIFEST.md` documents every source — name, version/commit, sha256, license, and which fact it witnesses. The in-code witness tests pin constants to these sources **by value**, so `cargo test` runs without the stash present; the stash is for human verification and for deriving new facts (open a PDF, decode a capture with `dcbdump`). See the provenance Hard rule above for the allowed-sources policy.

## Code ownership

Maintainer / sole deciding authority: **Mikołaj Mikołajczyk**. Solo project.
