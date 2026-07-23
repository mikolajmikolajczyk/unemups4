# decision-1 - Adopt agentic-project-template into existing GPL-3 repo

- **Status:** Accepted
- **Date:** 2026-07-09
- **Deciders:** Mikołaj Mikołajczyk

## Context

unemups4 is a pre-existing Rust workspace (PS4 emulator) with real git history and a GPL-3.0
license (`COPYING`). We wanted the agentic-project-template scaffold — Backlog.md task tracker,
the `AGENTS.md`/`CLAUDE.md` pointer table, the `backlog/` knowledge tree, the vendored backlog
skill, pre-commit hooks, and a Nix devShell — without disturbing the existing repo.

The template ships an `init.sh` and a `BOOTSTRAP.md` designed for a *fresh clone*: `init.sh`
runs `rm -rf .git` to start clean history, and the template's `LICENSE` is MIT (for the template
itself, not for projects built from it).

## Decision

Adopt the scaffold into the existing repo on a branch (`add-agentic-template`), preserving all
history. Specifically:

- **History preserved** — no fresh `git init`. `init.sh` was intentionally **not run and not
  copied** into the repo (it would have destroyed history).
- **MIT LICENSE not adopted** — the project stays **GPL-3.0** (`COPYING` untouched). The
  template's MIT `LICENSE` was deliberately not copied.
- **README.md and COPYING left as-is** — the existing project README and license are canonical.
- **BOOTSTRAP.md applied non-interactively then removed** — its interactive flow was resolved from
  known repo facts (name `unemups4`, stack Rust/edition 2024, license GPL-3, tracker Backlog.md)
  and the file deleted, since bootstrap is complete.
- **GitHub remote stays a mirror, not the tracker** — the repo has an existing remote
  (`origin` → `github.com/mikolajmikolajczyk/unemups4`). Per the template's philosophy it is an
  optional code mirror only; tasks, docs, and decisions live locally in Backlog.md under
  `backlog/`, not in GitHub issues.
- Rust toolchain (`cargo`, `rustc`, `rust-analyzer`, `rustfmt`, `clippy`) added to the flake
  devShell alongside the backlog/pre-commit/git tooling.

## Alternatives considered

- **Run `init.sh` / bootstrap fresh:** rejected — it wipes `.git` and would destroy the emulator's
  history.
- **Adopt the template's MIT LICENSE:** rejected — the project is GPL-3; adopting MIT would
  silently relicense.

## Consequences

- Positive: local-first task tracking + load-on-demand agent docs, reproducible devShell, no
  history loss, license integrity kept.
- Negative: the template's fresh-clone conveniences (`init.sh`) are unavailable; skill symlinks and
  hooks were wired up manually via `scripts/skills-bootstrap.sh` and `pre-commit install`.

## Trigger to revisit

If the project moves off Backlog.md, changes license, or the template's structure diverges enough
that re-syncing is easier than incremental updates.
