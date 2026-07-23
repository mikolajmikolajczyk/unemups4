#!/usr/bin/env bash
#
# diff_backends.sh — differential oracle for the x86jit migration (backlog doc-1,
# task-9). Runs every example ELF twice — once under the interpreter backend
# (UNEMUPS4_BACKEND=interp) and once under the Cranelift JIT (UNEMUPS4_BACKEND=jit)
# — and diffs the two NORMALIZED outputs against each other. The two backends must
# be observationally identical (the interpreter is the JIT's reference oracle in
# x86jit); any divergence is a real backend bug and this script exits nonzero.
#
# This complements run_examples.sh (which pins each backend to the committed
# baselines): here the comparison is interp-vs-jit directly, so a divergence that
# somehow matched neither-or-both baselines is still caught.
#
# It reuses run_examples.sh's normalization pipeline (strip_noise + normalize +
# sort -u) and its example table by SOURCING it — run_examples.sh only runs its
# `main` when executed directly, so sourcing just imports the helpers. The backend
# marker line ("guest execution backend: ...") is stripped by strip_noise, so it
# never shows up as a spurious interp-vs-jit diff.
#
# Usage (from a shell where `cargo` is on PATH, e.g. `nix develop`):
#   scripts/diff_backends.sh

set -uo pipefail

DIFF_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# Import strip_noise/normalize/prebuild/wayland_ld/run_one + the EXAMPLE_* arrays.
# shellcheck source=scripts/run_examples.sh
source "${DIFF_ROOT}/scripts/run_examples.sh"

# Scratch files for the two per-example backend runs. Script-global + pre-init so
# the EXIT trap can reference them under `set -u` (same pattern as run_examples.sh).
INTERP_TMP=""
JIT_TMP=""

do_diff() {
  prebuild
  local i name elf status=0
  INTERP_TMP="$(mktemp)"
  JIT_TMP="$(mktemp)"
  trap '[[ -n "${INTERP_TMP:-}" ]] && rm -f "${INTERP_TMP}"
        [[ -n "${JIT_TMP:-}" ]] && rm -f "${JIT_TMP}"' EXIT

  for i in "${!EXAMPLE_NAMES[@]}"; do
    name="${EXAMPLE_NAMES[$i]}"
    elf="${EXAMPLE_ELFS[$i]}"
    echo ">> diffing ${name} (${elf}): interp vs jit" >&2
    # run_one inherits UNEMUPS4_BACKEND from the environment we set here.
    UNEMUPS4_BACKEND=interp run_one "${elf}" "${name}" >"${INTERP_TMP}"
    UNEMUPS4_BACKEND=jit run_one "${elf}" "${name}" >"${JIT_TMP}"
    if diff -u --label "interp/${name}" --label "jit/${name}" \
      "${INTERP_TMP}" "${JIT_TMP}"; then
      echo "OK   ${name}: interp == jit" >&2
    else
      echo "FAIL ${name}: interp and jit backends diverged" >&2
      status=1
    fi
  done

  if [[ "${status}" -eq 0 ]]; then
    echo "all ${#EXAMPLE_NAMES[@]} examples match across interp and jit" >&2
  else
    echo "one or more examples diverged between backends" >&2
  fi
  return "${status}"
}

do_diff
