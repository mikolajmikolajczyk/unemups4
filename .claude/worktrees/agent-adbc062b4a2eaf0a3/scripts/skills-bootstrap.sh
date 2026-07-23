#!/usr/bin/env bash
# skills-bootstrap — mirror vendored skills into .claude/skills/ for Claude Code auto-discovery.
#
# Skills are vendored (committed) under .agents/skills/<name>/. Claude Code scans
# .claude/skills/, not .agents/skills/, so this symlinks each vendored skill into place.
#
# Idempotent, no network. Re-run after adding/removing a skill under .agents/skills/.

set -euo pipefail

if [[ ! -d .agents/skills ]]; then
  echo "error: .agents/skills/ not found (run from project root)" >&2
  exit 1
fi

mkdir -p .claude/skills

# Only symlinks are pruned — a real file/dir here is user-made, leave it alone.
for link in .claude/skills/*; do
  [[ -L "$link" ]] || continue
  name=$(basename "$link")
  if [[ ! -d ".agents/skills/$name" ]]; then
    rm -f "$link"
    echo "• pruned stale .claude/skills/$name"
  fi
done

for skill_dir in .agents/skills/*/; do
  [[ -d "$skill_dir" ]] || continue
  name=$(basename "$skill_dir")
  ln -sfn "../../.agents/skills/$name" ".claude/skills/$name"
  echo "• .claude/skills/$name → .agents/skills/$name"
done

echo "✓ skills linked"
