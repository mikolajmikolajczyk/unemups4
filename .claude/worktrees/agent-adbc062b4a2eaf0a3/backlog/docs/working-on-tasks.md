# Working on tasks

How **this project** drives [Backlog.md](https://github.com/MrLesk/Backlog.md). The
[`backlog` skill](../../.agents/skills/backlog/SKILL.md) covers the CLI in general; this page is
the project-specific overlay. `backlog instructions overview` is the authoritative workflow guide.

## Statuses we use

Backlog.md ships three statuses; this project uses all three, nothing more.

| Status | Meaning |
|--------|---------|
| `To Do` | Filed, scoped, not started. Default for every new task. |
| `In Progress` | Actively being worked. Set it **before** you start writing code. |
| `Done` | Merged into the default branch. |

Conventions:

- **Exactly one status at a time.** Picking up a task: `backlog task edit <id> -s "In Progress"`.
  Finishing (after it lands on the default branch): `backlog task edit <id> -s Done`.
- **No extra columns.** Solo work doesn't need a review column; adding one just makes the board
  lie. If a second contributor joins, add a status in `config.yml` then.
- **Blocked?** Say so in the task — `backlog task edit <id> --notes "Blocked on <what>"` — and
  leave it in `To Do`. A naked blocked state nobody can see helps no one.
- **Read with `--plain`.** `backlog task <id> --plain` and `backlog task list --plain` are the
  AI-friendly views — use them instead of the interactive board when scripting or reading.

## Task lifecycle

```sh
# 1. Create (or pick up an existing task)
backlog task create "<title>" -d "<description>" --ac "<acceptance criterion>"

# 2. Start
backlog task edit <id> -s "In Progress" --plan "<how you'll approach it>"

# 3. Work + commit (Conventional Commits, GPG-signed)
git commit -m "feat: <subject>"

# 4. Tick acceptance criteria and record what you learned
backlog task edit <id> --check-ac 1 --notes "<what landed, any surprises>"

# 5. After the branch merges into the default branch
backlog task edit <id> -s Done
```

Don't mark a task `Done` until the default branch actually contains the merge — solving early
misleads the board.

## Branch naming — Conventional Branch

We use [conventionalbranch.org](https://conventionalbranch.org/) for any branch that isn't the
default branch.

```
<type>/<short-slug>
```

Types: `feat`, `bugfix`, `hotfix`, `chore`, `docs`, `test`, `release`.

Optional task hint: append the task id if it helps you find the branch later.

```
feat/multi-format-loader
feat/task-14-multi-format-loader     # with task hint
chore/eslint-boundaries
docs/decision-2-layering
```

Why a convention at all on a solo project: future-me, AI agents, and `git branch --list 'feat/*'`
queries all want predictability.

Conventional Branch is **not** Conventional Commits — commit messages still follow Conventional
Commits separately (`feat:`, `fix:`, `docs:`, `chore:`, `refactor:`, `release:`).

## Forge-agnostic git flow

This template is forge-agnostic. Tasks live locally in `backlog/`; git is plain branches merged
into the default branch. A git remote (GitHub / GitLab / Codeberg / Radicle / none) is an
**optional** mirror — no issue tracker or patch flow is tied to it.

```sh
git checkout -b feat/<slug>
# … work, commit …
git checkout main && git merge --no-ff feat/<slug>     # or fast-forward, your call
git push                                               # only if a remote is configured
```

## Decision capture

For a decision tied to one task, record it on the task:

```sh
backlog task edit <id> --notes "Decided: <choice> over <alternative> — <one-sentence reason>."
```

For a cross-cutting or architecture-grade decision, use `backlog decision create`. See
[`decisions.md`](decisions.md) for the split.

## Session handoff

When ending a coding session mid-task, leave the state on the task itself:

```sh
backlog task edit <id> --notes "Session pause $(date -I). Done: <X>. Next: <Y>. Blocker: <Z|none>."
```

The next session (you or an agent) reads `backlog task <id> --plain` and picks up without
rediscovering state from the diff. Local, agent-agnostic, versioned in-repo.
