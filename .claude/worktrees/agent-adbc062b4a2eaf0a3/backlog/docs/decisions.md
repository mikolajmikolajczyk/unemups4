# Where to capture a decision

This project records decisions in **two** places. Pick the right one — don't invent a third.

| Surface | Use when | Lifetime |
|---------|----------|----------|
| **Backlog decision** (`backlog/decisions/`) | The choice constrains app shape, public contracts, layering, error/test/runtime semantics, or is a cross-cutting tool/library/process call. Hard to reverse, or affects future contributors. This is the home for what other projects call ADRs. | Project-lifetime, append-only in spirit |
| **Task note / comment** | The decision is bound to one task. Example: "for `task-12` we chose encoding X over Y because of constraint Z." | Bound to that task |

If a decision outlives the immediate task, it's a `backlog decision`. If it only explains one
task's implementation, it's a task note. There is no separate ADR folder — architecture-grade
decisions live in `backlog/decisions/` alongside lighter ones (tag or title them so they're easy
to find).

## Recording a decision

```sh
backlog decision create "<one-line decision summary>"   # → backlog/decisions/decision-N - <Title>.md
ls backlog/decisions/                                    # list existing (no `decision list` subcommand)
```

Fill the generated file. Suggested sections (keep it short — a durable trace, not a research
paper; if it grows past ~250 lines, scope it down):

- **Status** — Proposed / Accepted / Superseded by decision-NNN
- **Date** — ISO date of acceptance
- **Deciders** — names
- **Context** — the situation
- **Decision** — what we picked + why
- **Alternatives considered** — briefly, with why each lost
- **Consequences** — positive and negative
- **Trigger to revisit** — what would re-open this

To reverse an accepted decision, write a new one that supersedes it and update the old file's
Status line to `Superseded by decision-NNN` with a back-link. Editing typos is fine; editing the
substance of an accepted decision is not.

## Recording a task-bound decision

Attach it to the task instead — it stays next to the work:

```sh
backlog task edit <id> --notes "Decided: <choice> over <alt> — <one-sentence reason>. Revisit if <condition>."
# or, as a threaded comment:
backlog task edit <id> --comment "Decided: <choice> over <alt> — <reason>."
```

## When it's weighty enough to record at all

Write a `backlog decision` when the choice:

- **Constrains the shape of the app or public contracts** — downstream code will depend on it.
- **Is hard to reverse** — undoing it needs a migration, not a refactor.
- **Is cross-cutting** — touches multiple layers/modules/milestones.
- **Was contested or non-obvious** — there were real alternatives and future-you will want the why.
- **Has stakeholder implications** — onboarding, distribution, licensing, hosting.

Skip it (a task note or commit message is enough) when the choice:

- Is a **tool swap** doable in a day (formatter, linter, package manager, devShell tech).
- Is **DX convenience** with no behavioral effect (editor config, direnv, shell aliases).
- Is a **library swap** in a single layer with no contract change.
- Is a **status update or roadmap item** — those are tasks, not decisions.

### Edge cases — record a decision if the answer is "yes"

- **Tool choice with lock-in:** "Build *requires* Nix" → record. "Nix is primary, npm works as
  fallback" → skip.
- **Library swap that changes a public interface:** downstream notices → record; purely internal →
  skip.
- **Process/workflow choice** (e.g. "release from `main` only"): durable contract with
  collaborators → record; personal preference → skip.
