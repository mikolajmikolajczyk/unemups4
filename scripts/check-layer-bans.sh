#!/usr/bin/env bash
# Layer-boundary enforcement (task-132 AC#3).
#
# The GPU/kernel crate graph has hard layering rules that were convention-only:
#   * ps4-gnm  must NOT depend on `ash`   — gnm is the Vulkan-free draw layer; only
#                                           ps4-gpu (the display thread) may name ash.
#   * ps4-kernel must NOT depend on ps4-gpu — the kernel reaches the present path through
#                                           the `VideoOutSink`/`PresentSink` seams in
#                                           ps4-core, never the concrete Vulkan crate.
#
# cargo-deny's `bans` section can ban a crate globally but cannot express "crate A must not
# depend on crate B" (a per-parent edge ban), and cargo-deny is not always installed. So this
# is the equivalent CI check the task allows: it walks the real resolved dependency graph via
# `cargo metadata` and fails if either forbidden edge is reachable. Runs with just cargo + jq
# (both in the devShell), wired as a pre-commit hook and suitable for CI.
#
# A companion `deny.toml` documents the same intent for anyone who does run `cargo deny`.

set -euo pipefail

cd "$(dirname "$0")/.."

# Forbidden edges as "dependent -> banned" pairs.
edges=(
  "ps4-gnm:ash"
  "ps4-kernel:ps4-gpu"
)

meta="$(cargo metadata --format-version 1 --no-deps 2>/dev/null)" || {
  echo "check-layer-bans: 'cargo metadata' failed" >&2
  exit 1
}

fail=0
for edge in "${edges[@]}"; do
  dependent="${edge%%:*}"
  banned="${edge##*:}"

  # Direct dependencies of `dependent` (from --no-deps: only workspace members carry a full
  # deps list, which is exactly the direct edges we want to forbid).
  depends="$(
    jq -r --arg d "$dependent" --arg b "$banned" '
      .packages[]
      | select(.name == $d)
      | .dependencies[]?.name
      | select(. == $b)
    ' <<<"$meta"
  )"

  if [[ -n "$depends" ]]; then
    echo "check-layer-bans: FORBIDDEN EDGE — '$dependent' depends on '$banned'" >&2
    fail=1
  fi
done

if [[ "$fail" -ne 0 ]]; then
  echo "check-layer-bans: layer boundary violated (see backlog/docs/architecture.md 'GPU layering')" >&2
  exit 1
fi

echo "check-layer-bans: ok (ps4-gnm !-> ash, ps4-kernel !-> ps4-gpu)"
