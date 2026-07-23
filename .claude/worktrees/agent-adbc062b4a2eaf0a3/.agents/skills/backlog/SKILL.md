---
name: backlog
description: >-
  Local-first task, doc, and decision tracking with the Backlog.md CLI (`backlog`).
  Use whenever the user talks about tasks, the board, backlog, project status, milestones,
  drafts, or decisions — or runs any `backlog …` command. Covers the task lifecycle
  (create → In Progress → Done), acceptance criteria, `--plain` reads, docs, decisions,
  and the forge-agnostic git flow. Not for Radicle or GitHub issues — this repo tracks
  work locally in `backlog/`.
---

# Backlog.md

[Backlog.md](https://github.com/MrLesk/Backlog.md) keeps tasks, docs, and decisions as plain
markdown **inside the repo** (`backlog/`), so everything is local-first, versioned with the code,
and forge-agnostic. The CLI is `backlog`.

> **Authoritative source:** run `backlog instructions overview` and `backlog <cmd> --help` for the
> exact, version-current flags. This skill is the map, not the spec — if a flag here disagrees with
> `--help`, trust `--help`.

## Layout

```text
backlog/
  config.yml            # project_name, statuses, task_prefix, definition_of_done, …
  tasks/                # task-N - Title.md (the board)
  drafts/               # parked tasks (--draft)
  completed/  archive/  # finished / retired
  docs/                 # guides & reference — backlog doc create → doc-N - Title.md
  decisions/            # architecture/tech records — backlog decision create → decision-N - Title.md
```

## Statuses

Default columns: **To Do**, **In Progress**, **Done** (customizable via `statuses` in
`config.yml`). One status per task at a time. Set `In Progress` **before** writing code; set `Done`
only after the work has merged into the default branch.

## Task lifecycle (the loop agents follow)

```sh
backlog task create "<title>" -d "<description>" --ac "<criterion>"   # define acceptance up front
backlog task edit <id> -s "In Progress" --plan "<how you'll do it>"    # claim + record a plan
# … write code, commit …
backlog task edit <id> --check-ac 1 --notes "<what landed / surprises>" # tick criteria, log notes
backlog task edit <id> -s Done                                        # only after it merges
```

Always read tasks with `--plain` — it's the AI-friendly, grep-able view:

```sh
backlog task list --plain
backlog task list -s "In Progress" --plain
backlog task <id> --plain
```

## Create / edit flags (cheat-sheet — confirm with `--help`)

`create` and `edit` share most flags: `-d/--description`, `--ac <criterion>` (repeatable),
`-l/--labels`, `--priority`, `-s/--status`, `-a/--assignee`, `--plan`, `--notes`,
`--dep <ids>` (dependencies), `--dod`. `create`-only: `-p <parent-id>` (parent), `--draft`.
`edit` adds `--check-ac <n>` / `--uncheck-ac <n>` and `--comment`.

## Board & browser

```sh
backlog board                 # interactive kanban in the terminal
backlog board export          # markdown snapshot
backlog browser [--port N]    # web UI
```

## Docs & decisions

```sh
backlog doc create "<title>" [-p <subpath>]   # project documentation → backlog/docs/
backlog doc list
backlog decision create "<title>"             # architecture/tech decision → backlog/decisions/
ls backlog/decisions/                          # list existing (no `decision list` subcommand)
```

Decision vs task note: cross-cutting / architecture-grade → `backlog decision`; bound to one task →
`backlog task edit <id> --notes "…"`. See `backlog/docs/decisions.md`.

## Session handoff

Persist state on the task itself so the next session (you or another agent) resumes without reading
the diff:

```sh
backlog task edit <id> --notes "Session pause $(date -I). Done: <X>. Next: <Y>. Blocker: <Z|none>."
```

## Forge-agnostic git

Tasks are local; git is plain branches merged into the default branch. A remote (GitHub / GitLab /
Codeberg / Radicle / none) is an **optional** mirror — no issue or patch flow is tied to it. Branch
naming: [Conventional Branch](https://conventionalbranch.org/) (`feat/`, `bugfix/`, `chore/`, …).

## MCP (optional)

`backlog mcp start` exposes tasks/docs/decisions over MCP; Backlog.md ships connectors for Claude
Code, Codex, Gemini CLI, Kiro, and Cursor. Not required — the CLI is enough.
