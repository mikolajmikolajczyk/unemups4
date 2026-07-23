# decision-1 - Adopt agentic-project-template into existing GPL-3 repo

- **Status:** Accepted
- **Date:** 2026-07-09
- **Deciders:** Mikołaj Mikołajczyk

## Context

unemups4 is a Rust workspace (PS4 emulator) licensed GPL-3.0 (`COPYING`). We wanted the
agentic-project-template scaffold — Backlog.md task tracker, the `AGENTS.md`/`CLAUDE.md` pointer
table, the `backlog/` knowledge tree, the vendored backlog skill, pre-commit hooks, and a Nix
devShell. The template itself ships an MIT `LICENSE` (for the template, not for projects built
from it).

## Decision

Adopt the scaffold, but keep the project's own licensing:

- **The project stays GPL-3.0.** The template's MIT `LICENSE` was deliberately not copied;
  `COPYING` is untouched and canonical. Adopting MIT would silently relicense the emulator.
- **The existing README.md and COPYING remain canonical.**
- **GitHub remote stays a mirror, not the tracker** — tasks, docs, and decisions live locally in
  Backlog.md under `backlog/`, not in GitHub issues.
- Rust toolchain (`cargo`, `rustc`, `rust-analyzer`, `rustfmt`, `clippy`) added to the flake
  devShell alongside the backlog/pre-commit/git tooling.

## Consequences

- Positive: local-first task tracking + load-on-demand agent docs, reproducible devShell, license
  integrity kept.
- Negative: skill symlinks and hooks are wired up manually via `scripts/skills-bootstrap.sh` and
  `pre-commit install`.

## Trigger to revisit

If the project moves off Backlog.md, changes license, or the template's structure diverges enough
that re-syncing is easier than incremental updates.
