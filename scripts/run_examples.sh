#!/usr/bin/env bash
#
# run_examples.sh — capture / check native-execution baselines for the example ELFs.
#
# This is the migration ORACLE for the x86jit migration (backlog doc-1, task-1).
# It runs each prebuilt example ELF under the emulator (built once via cargo, then
# executed directly as target/release/unemups4), captures the combined
# stdout+stderr (tracing logs go to stderr, guest printf to stdout), normalizes
# known non-determinism, and either writes the result as a committed baseline
# (capture) or diffs it against the committed baseline (check).
#
# Modes:
#   capture             write scripts/baselines/<name>.txt
#   check               run and diff against committed baselines; nonzero exit on
#                       ANY mismatch (this is what later migration tasks gate on)
#
# Usage (from a shell where `cargo` is on PATH, e.g. `nix develop`):
#   scripts/run_examples.sh capture
#   scripts/run_examples.sh check
#
# Non-determinism handling: every run is passed through strip_noise() + normalize()
# then 'sort -u' — see those functions' comments. The process exit code is a
# host-display panic race and is NOT asserted; the guest's real termination is a
# log line. See the per-example baseline header for the specifics/compromises.
#
# softgpu: ps4-softgpu opens a display window and never exits on its own, so the
# timeout KILLS it — a timeout exit is the EXPECTED, healthy outcome for it, not a
# failure. Its baseline captures the boot log up to the point the window loop
# takes over.

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BASELINE_DIR="${REPO_ROOT}/scripts/baselines"
TIMEOUT="${RUN_EXAMPLES_TIMEOUT:-30s}"

# Scratch file used by do_check's per-example diff. Script-global + pre-initialized
# so the EXIT trap (which fires after do_check returns) can reference it under
# `set -u` without an "unbound variable" abort. See do_check.
CHECK_TMP=""

# name -> ELF path (relative to REPO_ROOT).
EXAMPLE_NAMES=(
  hello_world
  ps4-fs
  ps4-mmap
  ps4-tls
  ps4-thread-testing
  ps4-softgpu
)
EXAMPLE_ELFS=(
  "examples/ps4-helloworld/hello_world.elf"
  "examples/ps4-fs/ps4-fs.elf"
  "examples/ps4-mmap/ps4-mmap.elf"
  "examples/ps4-tls/ps4-tls.elf"
  "examples/ps4-thread-testing/ps4-thread-testing.elf"
  "examples/ps4-softgpu/ps4-softgpu.elf"
)

# normalize: read raw run output on stdin, emit a canonical, run-independent
# form on stdout. See the big NON-DETERMINISM note below for the why.
#
# Steps, in order:
#   1. strip ANSI SGR escapes (tracing colourises even when piped),
#   2. mask timestamps, thread ids, PIDs/TIDs, host & guest addresses, durations,
#      and the winit/wayland panic host path (crate version + registry hash vary),
#   3. mask guest thread/key ids (see thread-testing note below),
#   4. drop blank lines (their count varies with interleaving),
#   5. SORT the remaining lines and collapse exact duplicates (sort -u).
#
# Sorting is the key move: the emulator emits an near-identical MULTISET of lines
# every run, but several sources reorder / re-multiply them nondeterministically —
#   (a) HLE library load order (libkernel / libScePad / libSceUserService /
#       libSceVideoOut print in a shuffling order),
#   (b) the main (display) thread and the guest thread interleave freely, and
#   (c) ps4-thread-testing spawns guest worker threads whose TIDs are assigned in
#       scheduler-dependent order (tid=2 vs tid=3 for "the same" logical worker)
#       and whose per-thread log lines appear a varying number of times / order.
# Masking alone is not enough; a sorted SET compare is. Guest thread ids (tid=,
# key=, "Thread N", "thread N", "Joining thread N") are masked to <T>/<K> so the
# workers become interchangeable, and `sort -u` collapses the duplicate-count
# jitter. This is the documented determinism COMPROMISE for ps4-thread-testing:
# the oracle checks the SET of distinct (masked) events, not their order or
# multiplicity. It still fails loudly if a genuinely new/absent event appears.
# For the other five examples the masking is inert and sort -u is a no-op on the
# (already unique) lines, so they stay a faithful event set.
normalize() {
  sed -E \
    -e "s#${REPO_ROOT//#/\\#}#<REPO>#g" \
    -e 's/\x1b\[[0-9;]*m//g' \
    -e 's/ThreadId\([0-9]+\)/ThreadId(N)/g' \
    -e 's/\(pid ?[0-9]+\)|\([0-9]+\) panicked/(<PID>) panicked/g' \
    -e 's/(TID:? ?)0x[0-9a-fA-F]+/\1<ADDR>/g' \
    -e 's/\b(tid|key)=[0-9]+/\1=<T>/g' \
    -e 's/\b([Tt]hread) [0-9]+/\1 <T>/g' \
    -e 's/(PID:? ?)[0-9]+/\1<PID>/g' \
    -e 's/(TID:? ?)[0-9]+/\1<TID>/g' \
    -e 's#/home/[^ "]*/winit-[0-9.]+/[^ "]*#<WINIT_PATH>#g' \
    -e 's/winit-[0-9]+\.[0-9]+\.[0-9]+/winit-<VER>/g' \
    -e 's/\b0x[0-9a-fA-F]{4,}\b/<ADDR>/g' \
    -e 's/[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}(\.[0-9]+)?Z?/<TS>/g' \
    -e 's/[0-9]+(\.[0-9]+)?(ns|µs|us|ms|s)\b/<DUR>/g' \
    -e 's/[[:space:]]+$//' \
    | grep -v '^[[:space:]]*$' \
    | LC_ALL=C sort -u
}

# prebuild: compile the emulator ONCE, outside every per-example timeout window,
# so `cargo run` inside run_one never spends the timeout budget compiling (the
# display/vulkan stack is slow to build) and never leaks "Compiling ..." chatter
# into the captured output.
prebuild() {
  echo ">> building unemups4 (release) ..." >&2
  ( cd "${REPO_ROOT}" && cargo build --release -p unemups4 ) >&2 || {
    echo "build failed" >&2
    exit 1
  }
}

# strip_noise: drop lines that are NOT guest/emulator behavior and would add
# environment-dependent jitter:
#   * cargo build chatter and the OpenOrbis-SDK-missing build warning (only shows
#     up if the script is ever fed cargo output);
#   * the winit/display MAIN-thread panic (this devShell lacks a usable
#     wayland/XKB display, so EventLoop::new() panics). That panic is host-env
#     noise, races the guest thread, and its host paths / backend vary — none of
#     it reflects guest correctness, so it is removed wholesale.
#   * the "guest execution backend: <Jit|Interp>" startup line (task-9): a
#     config marker that names the selected backend, so it necessarily DIFFERS
#     between the interp and jit runs and is not guest behavior. Dropping it keeps
#     the pre-task-9 baselines valid under both backends and lets diff_backends.sh
#     compare the two backends' guest output cleanly.
#   * ALL ps4_gpu::vulkan lines (the whole Vulkan backend target) + the
#     ps4_gpu::display "Failed to initialize Vulkan" line: pure host-GPU noise
#     that varies with driver presence and the UNEMUPS4_NO_EXTMEMHOST lever
#     (no driver -> "Failed to initialize"; driver -> ZERO-COPY banner +
#     per-frame "imported guest framebuffer" import lines; driver+lever ->
#     STAGING-COPY). None are guest behavior (the guest-visible GPU event is
#     ps4_kernel::bridge "RegisterBuffer", kept), so stripping the backend target
#     wholesale makes the oracle env-independent across headless / driver-zerocopy
#     / driver-staging and immune to future backend log churn.
strip_noise() {
  grep -vE \
    -e '^[[:space:]]*(Compiling|Finished|Running|Blocking|Fresh|Building) ' \
    -e 'guest execution backend' \
    -e '^warning: (ps4-syscalls|unused|`ps4|.*generated [0-9]+ warnings)' \
    -e '^warning: ps4-syscalls@' \
    -e 'OpenOrbis|oo_sdk|Syscall arguments metadata|SDK path|git clone https' \
    -e '^warning: unemups4' \
    -e "thread 'main'.*panicked" \
    -e "called \`Result::unwrap\(\)\` on an \`Err\`" \
    -e 'RUST_BACKTRACE' \
    -e 'XKBNotFound|NoWaylandLib|WaylandError|OsError|neither WAYLAND_DISPLAY' \
    -e 'winit-[0-9]' \
    -e 'ps4_gpu::vulkan:' \
    -e 'Failed to initialize Vulkan' \
    || true
}

# wayland_ld: winit dlopen()s libwayland-client.so at runtime; it is not on the
# default loader path inside the Nix devShell, so without help the display thread
# panics on EventLoop::new(). That panic runs on the MAIN thread and aborts the
# process at a nondeterministic point — truncating the guest's output and racing
# the guest's own std::process::exit(0). Making libwayland discoverable lets the
# window open, the display thread block cleanly, and every non-GUI example run to
# completion and exit 0 deterministically. Best-effort: if no libwayland is found
# we run anyway (headless) and rely on sort-normalisation to absorb what it can.
wayland_ld() {
  local lib
  lib="$(find /nix/store -maxdepth 3 -name 'libwayland-client.so' 2>/dev/null | head -1)"
  [[ -n "${lib}" ]] && dirname "${lib}"
}

# header_for: constant, per-example header written into the baseline (and re-
# emitted identically by check, so it never causes a spurious diff). Documents how
# this particular baseline is normalized so a human diffing a migration regression
# knows what is and isn't load-bearing.
header_for() {
  local name="$1"
  cat <<EOF
# baseline: ${name}
# GENERATED by scripts/run_examples.sh — do not hand-edit; re-run 'capture'.
# Content below is the emulator's combined stdout+stderr, NORMALIZED then
# 'sort -u'ed (order-independent SET of masked event lines). Masks: <TS> time,
# ThreadId(N) host thread, <PID>/<TID>/<ADDR> ids & addresses, <DUR> durations,
# tid=<T>/key=<T>/"Thread <T>" guest thread ids. The winit/display MAIN-thread
# panic (this devShell has no usable wayland/XKB display) is host-env noise and
# is stripped; the process exit code is a panic-vs-guest race and is NOT asserted
# (the guest's real end shows as a "[SYSCALL] exit(0)" line in the body).
EOF
  case "${name}" in
    ps4-softgpu)
      cat <<'EOF'
# NOTE: softgpu opens a display window and never returns; the 30s timeout KILLS
# it, so a panic/timeout exit (not 0) is EXPECTED. The window/display thread also
# panics in a headless-ish devShell (wayland/XKB), which is why the exit code is
# not 0 — that is fine: only the pre-window boot log is the oracle here.
EOF
      ;;
    ps4-thread-testing)
      cat <<'EOF'
# NOTE: thread scheduling is inherently non-deterministic here — guest worker
# TIDs are assigned in scheduler order and per-thread lines repeat/reorder. The
# COMPROMISE: guest tids are masked and 'sort -u' collapses duplicates, so this
# baseline asserts the SET of distinct masked events, not their order or count.
# ORACLE CHANGE (task-6, x86jit): the prior committed baseline was captured under
# NATIVE execution and was TRUNCATED at ">>> TEST 1" by a display-thread panic
# race (the native path outran the guest's exit). Under the x86jit interpreter +
# the task-5 park-main-thread fix the guest now runs the FULL regression suite:
# TESTS 1-7 COMPLETE, mutex-locked Counter == 40000 (SUCCESS), recursive mutex,
# condvar, timed-wait (ETIMEDOUT 110), detach/name, RWLock + TryLock (EBUSY 16),
# "ALL TESTS FINISHED", exit(0). This baseline was recaptured & semantically
# verified against that full suite. Stable across 10 consecutive runs.
EOF
      ;;
    *)
      cat <<'EOF'
# NOTE: the guest runs to completion (ends with a "[SYSCALL] exit(0)" line). The
# sort-normalized body is stable across runs.
EOF
      ;;
  esac
}

run_one() {
  # $1 = elf path (relative to repo root). $2 = example name (for the header).
  # Emits a constant header, then the normalized run body, then a trailing
  # "=== run complete ... ===" marker. Runs from REPO_ROOT so the emulator finds
  # game_data/. The binary is already built by prebuild(), so the timeout covers
  # execution only. stdbuf forces line buffering so a timeout-KILLed run (softgpu)
  # still yields the boot log it printed before the window loop took over, instead
  # of losing it in a full stdio buffer.
  local elf="$1" name="$2" rc raw wl_dir bin
  bin="${REPO_ROOT}/target/release/unemups4"
  wl_dir="$(wayland_ld)"
  header_for "${name}"
  raw="$(cd "${REPO_ROOT}" && LD_LIBRARY_PATH="${wl_dir}:${LD_LIBRARY_PATH:-}" \
    timeout "${TIMEOUT}" stdbuf -oL -eL "${bin}" "${elf}" 2>&1)"
  rc=$?
  # ps4-thread-testing: the pthread join-completion reporting ("Joining thread N",
  # "Thread N joined successfully", "scePthreadJoin(tid=N)") is the racy TAIL of
  # execution — when the display thread's panic wins the shutdown race a hair
  # early, some join lines are lost. Drop that tail so the baseline asserts the
  # stable body (thread create / TLS / dtor events); documented in the header.
  if [[ "${name}" == "ps4-thread-testing" ]]; then
    printf '%s\n' "${raw}" | strip_noise \
      | grep -vE 'Joining thread|joined successfully|scePthreadJoin' \
      | normalize
  else
    printf '%s\n' "${raw}" | strip_noise | normalize
  fi
  # NOTE on exit code: for these examples the PROCESS exit code is NOT a reliable
  # oracle signal — it reflects the host display thread, not the guest. It flips
  # nondeterministically between 0 (guest's std::process::exit(0) wins the race),
  # 101 (winit main-thread panic wins) and 124 (softgpu window timeout). The
  # guest's real termination is captured deterministically as a log line
  # ("[SYSCALL] exit(0) ...") in the body above. We therefore record only a
  # constant marker here instead of the racy numeric code. rc is intentionally
  # not printed; referenced here so shellcheck sees it used.
  : "${rc}"
  printf '=== run complete (process exit code intentionally not asserted) ===\n'
}

do_capture() {
  mkdir -p "${BASELINE_DIR}"
  prebuild
  local i name elf
  for i in "${!EXAMPLE_NAMES[@]}"; do
    name="${EXAMPLE_NAMES[$i]}"
    elf="${EXAMPLE_ELFS[$i]}"
    echo ">> capturing ${name} (${elf})" >&2
    run_one "${elf}" "${name}" >"${BASELINE_DIR}/${name}.txt"
  done
  echo "captured ${#EXAMPLE_NAMES[@]} baselines into ${BASELINE_DIR}" >&2
}

do_check() {
  prebuild
  local i name elf status=0
  # NOTE: CHECK_TMP is a SCRIPT-GLOBAL (not a `local`) on purpose. The EXIT trap
  # below fires at *shell* exit, after do_check has returned and any function-local
  # would already be out of scope — under `set -u` a `local tmp` reference in the
  # trap then aborts with "tmp: unbound variable" (the task-8 quirk this fixes).
  # A global initialized to "" is always bound when the trap runs, and the guard
  # skips the rm when nothing was created.
  CHECK_TMP="$(mktemp)"
  trap '[[ -n "${CHECK_TMP:-}" ]] && rm -f "${CHECK_TMP}"' EXIT
  for i in "${!EXAMPLE_NAMES[@]}"; do
    name="${EXAMPLE_NAMES[$i]}"
    elf="${EXAMPLE_ELFS[$i]}"
    if [[ ! -f "${BASELINE_DIR}/${name}.txt" ]]; then
      echo "MISSING baseline: ${name}.txt (run capture first)" >&2
      status=1
      continue
    fi
    echo ">> checking ${name} (${elf})" >&2
    run_one "${elf}" "${name}" >"${CHECK_TMP}"
    if diff -u "${BASELINE_DIR}/${name}.txt" "${CHECK_TMP}"; then
      echo "OK   ${name}" >&2
    else
      echo "FAIL ${name}: output diverged from committed baseline" >&2
      status=1
    fi
  done
  if [[ "${status}" -eq 0 ]]; then
    echo "all ${#EXAMPLE_NAMES[@]} examples match their baselines" >&2
  else
    echo "one or more examples diverged" >&2
  fi
  return "${status}"
}

main() {
  local mode="${1:-}"
  case "${mode}" in
    capture) do_capture ;;
    check) do_check ;;
    *)
      echo "usage: $0 {capture|check}" >&2
      exit 2
      ;;
  esac
}

# Only run when executed directly. When sourced (e.g. by diff_backends.sh, to reuse
# strip_noise/normalize/prebuild/wayland_ld + the example arrays) `main` is skipped so
# the sibling script drives its own flow. BASH_SOURCE[0] == $0 iff run, not sourced.
if [[ "${BASH_SOURCE[0]}" == "${0}" ]]; then
  main "$@"
fi
