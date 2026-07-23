# Commands

Everyday commands for this project. Keep this file **flat and copy-pasteable** — agents and humans both grep it.

## Build / run / test

```sh
cargo build                                   # debug build (whole workspace)
cargo build --release                         # optimized build
cargo check                                   # fast type-check, no codegen
cargo test                                    # run tests

# Run the emulator on a homebrew ELF
cargo run --release -p unemups4 -- path/to/homebrew.elf
cargo run --release -p unemups4 -- examples/ps4-helloworld/*.elf   # prebuilt example
```

The emulator mounts `game_data/app0` as `/app0` and `game_data/system` as `/system` for the guest.

### Running from the Nix devShell: `LD_LIBRARY_PATH=/usr/lib`

On a non-NixOS host the devShell supplies alsa/vulkan/wayland from the nix store while the
binary links the *system* glibc, so a plain run inside the shell dies with:

```text
libm.so.6: version `GLIBC_2.43' not found
```

Run it with the variable **replaced**, not extended:

```sh
LD_LIBRARY_PATH=/usr/lib cargo run --release -p unemups4 -- <elf>
```

`LD_LIBRARY_PATH=/usr/lib` puts the system libraries first, so the system libm satisfies the
binary. Writing `LD_LIBRARY_PATH="/usr/lib:$LD_LIBRARY_PATH"` does **not** work — the nix
paths stay ahead and nix's older glibc still wins. That one difference is easy to misread as
"the binary is linked wrong", and chasing it leads to inventing an explicit
`/usr/lib/ld-linux-x86-64.so.2` invocation and a separate `CARGO_TARGET_DIR`, neither of
which is needed.

Same rule for the GUI tools: the nix-built Tracy needs the system EGL vendor files too
(see [Tracy](#tracy-live-timeline---features-profile-tracy)).

### Optional build-time dependency (larger syscall table)

The syscall table generator (`crates/syscalls/build.rs`) reads symbol metadata from the OpenOrbis toolchain. It is not vendored; the build still runs without it (smaller table + a warning):

```sh
git clone https://github.com/OpenOrbis/OpenOrbis-PS4-Toolchain.git data/oo_sdk
```

## Typecheck / lint / format

```sh
cargo check                                             # type-check
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt                                               # format (rustfmt defaults)
cargo fmt --check                                       # verify formatting
```

## Pre-commit

```sh
pre-commit install                                  # one-time, per clone
pre-commit run --all-files                          # run active hooks
pre-commit run --all-files --hook-stage manual      # run staged-as-manual hooks too
```

## Backlog.md

See the [`backlog` skill](../../.agents/skills/backlog/SKILL.md) and `backlog instructions overview`
for the canonical cheat-sheet. Most-used:

```sh
backlog task create "<title>" -d "<desc>" --ac "<criterion>"
backlog task list --plain                       # AI-friendly view
backlog task list -s "In Progress" --plain
backlog task <id> --plain                        # show one task
backlog task edit <id> -s "In Progress" --plan "<approach>"
backlog task edit <id> --check-ac 1 --notes "<progress>"
backlog task edit <id> -s Done
backlog board                                    # interactive kanban
backlog browser                                  # web UI
backlog doc create "<title>"                      # → docs/doc-N - Title.md
backlog decision create "<title>"                 # → decisions/decision-N - Title.md
backlog config                                   # view/edit config.yml
```

## Profiling

Release builds carry DWARF debug info (`[profile.release] debug = true` in the
workspace `Cargo.toml`), so Linux `perf`, `cargo flamegraph`, and `hotspot`
resolve **named host frames** with zero runtime cost. Guest code compiled by the
x86jit Cranelift backend resolves as `jit_0x...` guest-block symbols once
`X86JIT_PERF_MAP=1` is set (see [JIT perf-map](#jit-perf-map-x86jit_perf_map));
without it those frames stay `[unknown]`. The interpreter backend
(`UNEMUPS4_BACKEND=interp`) is always fully symbolized as `x86jit_core::interp::*`.

**Prerequisite:** `perf` needs relaxed capture permissions —

```sh
sudo sysctl kernel.perf_event_paranoid=1     # <= 1 to sample; -1 for everything
```

### perf record / report

```sh
cargo build --release
perf record -g --call-graph dwarf,16384 -F 997 -- \
    target/release/unemups4 examples/ps4-helloworld/*.elf
perf report --no-children                    # self-time flat view
hotspot perf.data                            # GUI, if installed
```

`--call-graph dwarf,16384` unwinds via DWARF (no frame pointers needed);
`-F 997` samples ~997 Hz (a prime, to avoid aliasing with periodic work).
Expect named frames for `rust_syscall_handler`, `x86jit_core::interp`,
Cranelift compile functions, and the Vulkan present path.

### JIT perf-map (`X86JIT_PERF_MAP`)

By default the JIT'd guest blocks are anonymous mmap'd machine code with no ELF
symbols, so `perf` reports them as `[unknown]`. Setting `X86JIT_PERF_MAP=1`
makes the x86jit Cranelift backend emit a `perf`-map at `/tmp/perf-<pid>.map`
naming each compiled block — `jit_0x<guest_start>` per block, plus
`jit_region_0x<entry>` for region entries. `perf` reads that file automatically
by PID and symbolizes those frames.

```sh
cargo build --release
X86JIT_PERF_MAP=1 perf record -g --call-graph dwarf -F 997 -- \
    target/release/unemups4 examples/ps4-helloworld/*.elf
perf report --no-children                    # now shows jit_0x... blocks
```

A JIT-backend run (`UNEMUPS4_BACKEND=jit`, the default) then shows `jit_0x...`
guest-block symbols alongside the host frames, giving the full split:
`jit_0x...` guest blocks vs `x86jit_core::interp::*` (interp fallbacks) vs
Cranelift compile fns vs `rust_syscall_handler` HLE handlers vs the Vulkan
present path. An interpreter run (`UNEMUPS4_BACKEND=interp`) has no JIT blocks,
so guest work shows up as `x86jit_core::interp::*` instead.

The map is **append-only**: x86jit writes one line per block as it compiles and
never rewrites the file, so a long run accumulates entries and a fresh
`/tmp/perf-<pid>.map` appears per process. Unset `X86JIT_PERF_MAP` (or leave it
`0`) to skip emission entirely — it has no cost when off.

### Flamegraph

```sh
cargo install flamegraph                     # one-time (needs perf)
cargo flamegraph --release -p unemups4 -- examples/ps4-helloworld/*.elf
# → flamegraph.svg
```

### Caveats

- **JIT guest code is `[unknown]`** unless `X86JIT_PERF_MAP=1` is set (see [JIT
  perf-map](#jit-perf-map-x86jit_perf_map)). Alternatively, run
  `UNEMUPS4_BACKEND=interp` to see guest work as `x86jit_core::interp::*` frames.
- Headless/driverless sessions abort Vulkan init early; the present path won't
  appear in a profile there. Profile display-heavy paths on a real GPU session.

### Aggregate profiler (`UNEMUPS4_PROFILE`)

A `perf`-free, headless-friendly quantitative split — guest exec vs HLE syscalls
(+ per-syscall totals, run-loop exit histogram, x86jit cache/compile counters,
and GPU present-phase averages). Env-gated, house-style like `UNEMUPS4_WATCHDOG`;
**zero overhead when unset** (one cached branch on the hot path).

```sh
UNEMUPS4_PROFILE=1 cargo run --release -p unemups4 -- <elf>      # dump every 10s + final
UNEMUPS4_PROFILE=30 cargo run --release -p unemups4 -- <elf>     # dump every 30s
```

- `=1` enables with the default 10 s periodic dump; `=<secs>` sets that interval.
- Tables print via `tracing` (`target: unemups4::profile`) periodically; a **final**
  table is written straight to stderr from a `libc::atexit` handler so it survives the
  guest's `std::process::exit`. (The atexit path bypasses `tracing` on purpose — its
  thread-local dispatcher is gone after TLS teardown.)
- `UNEMUPS4_BACKEND=interp` shows all guest time in the interpreter with `compile_ns=0`;
  the default JIT backend attributes compile time to `compile_ns` once blocks tier up.
- GPU present-phase rows appear only on a real GPU session (headless has no frames).

### Tracy live timeline (`--features profile-tracy`)

A live, per-thread timeline in the Tracy GUI. The workspace crates emit `tracing`
spans on **low-frequency** paths (one per HLE syscall, one per present frame with
fence/acquire/fb_copy/record_submit/present children, one per guest thread, and the
boot stages) — never around hot `cpu.run()` slices (that split is the aggregate
profiler above). The spans are **unconditional and cost nothing** with no
span-consuming layer active (a cached callsite check); Tracy is wired only in the app
crate behind an off-by-default cargo feature, so the default build gains no dependency.

```sh
# 1. start the version-matched Tracy GUI from the devShell (see flake.nix pin).
#    On a non-NixOS host the nix build of Tracy links nix's libglvnd, which looks for EGL
#    vendor files in the nix store and finds no GPU driver there — it aborts with
#    "Cannot initialize EGL!". Point it at the system vendors instead:
env __EGL_VENDOR_LIBRARY_DIRS=/usr/share/glvnd/egl_vendor.d \
    LIBGL_DRIVERS_PATH=/usr/lib/dri \
    LD_LIBRARY_PATH="/usr/lib:$LD_LIBRARY_PATH" tracy &
# 2. run the emulator with the Tracy layer; it connects to the running GUI
LD_LIBRARY_PATH=/usr/lib cargo run --release --features profile-tracy -p unemups4 -- <elf>
```

Note the two `LD_LIBRARY_PATH` forms differ on purpose: Tracy needs the nix wayland/xkb
libraries kept on the path, the emulator needs them out of the way (see
[Running from the Nix devShell](#running-from-the-nix-devshell-ld_library_pathusrlib)).

Zones appear as `syscall` (grouped by id), `frame` + its present children, and one
`guest_thread` lane per guest thread. **Version lock:** the Tracy GUI protocol must
match the `tracy-client-sys` the crate pulls in — the devShell pins the matching
`tracy` package; if you bump `tracing-tracy`, re-check its compat table and re-pin.

**Fallback (headless / offline, not wired):** for a trace file instead of a live GUI,
swap `tracing_tracy::TracyLayer` for a `tracing-chrome` layer and open the resulting
`trace-*.json` in <https://ui.perfetto.dev>. Not built by default — a one-line layer
change when needed.

## GPU state snapshot (`F10` / `F9`)

An on-demand, complete dump of GPU state for a frame the maintainer picks, while the game
runs (task-185). It replaces the per-investigation env-gated probes that misled task-179:
a probe answers the question you already thought to ask, and a stuck investigation's
problem is usually that the question is wrong.

| Key | Captures |
|-----|----------|
| `F10` | the next complete frame |
| `F9`  | the next `UNEMUPS4_SNAPSHOT_FRAMES` frames (default 8) |

"Next", not "current": by the time a keypress is seen, part of the in-flight frame's draws
have already been recorded and shipped, so arming at the next frame boundary is the only
way to produce a frame that is actually complete. At 60 Hz that is a one-frame offset.

| Env var | Meaning |
|---------|---------|
| `UNEMUPS4_SNAPSHOT_FRAMES` | frames one `F9` press captures (default `8`) |
| `UNEMUPS4_SNAPSHOT_DIR` | output directory (default `gpu-snapshots/`, gitignored) |

Each captured frame writes `<dir>/frame-NNNNN/`:

- `registers.json` — **every** register the guest has written, in all four banks
  (CONTEXT / SH / UCONFIG / CONFIG), with the decoded name where this codebase has a
  constant for it and the raw index where it does not. Registers nothing on our side reads
  are dumped too: three of the four registers that mattered in task-179 were exactly those.
  Sorted by index, so two captures diff cleanly.
- `draws.json` — per draw: the derived target (base/extent/pitch/format/tiling/kind), the
  pipeline key with raw blend/depth register words, viewport, scissor, bound VS/PS
  addresses and identity hashes, the decoded T#/V#/S# descriptors the draw actually
  received, and up to 512 bytes of each constant buffer (as dwords *and* floats).
- `summary.txt` — one screen, for eyeballing without `jq`.

Costs nothing when idle: the per-draw check is a `bool` field read, and the per-frame check
is one relaxed atomic load at the flip. Capturing does not perturb the frame — it reads
only shadow state and guest memory, emits no backend command, and does **no** render-target
readback (`UNEMUPS4_RT_READBACK` is known-unreliable, task-181, and is deliberately unused
here; RT pixels are out of scope for this tool).

```sh
UNEMUPS4_SNAPSHOT_FRAMES=4 cargo run --release -p unemups4 -- <elf>
# press F9 at the moment of interest, then:
diff -r gpu-snapshots/frame-01734 gpu-snapshots/frame-01735
```

## Guest-module dump (`UNEMUPS4_DUMP_MODULES`)

An env-gated dump of every loaded, **post-relocation** guest module to disk, so a guest-side
crash can be disassembled/decompiled offline in objdump, Ghidra, or radare2 (task-113.2). It
replaces ad-hoc one-time disassembly: when a fault is reported as `eboot.bin +0x16de90`, you
open the dump at file offset `0x16de90` and read the code the guest actually ran.

Set `UNEMUPS4_DUMP_MODULES=<dir>`. After all modules are loaded and relocated (before the
guest runs), each non-HLE module writes two files to `<dir>`:

| File | Contents |
|------|----------|
| `<name>.bin` | The loaded segment image — the guest bytes at `[base_addr, base_addr + memory_size)`, exactly as they execute (post-relocation). File offset `N` == guest VA `base_addr + N`, so a `<module> +0xN` backtrace frame is at file offset `0xN`. |
| `<name>.map` | Load layout (base, size, entry abs+relative, arch `i386:x86-64`), the ELF sections, any zero-filled unreadable gaps, and the full export table (`name-or-NID  absolute  +relative`, sorted by address). A header at the top carries the exact objdump/Ghidra/radare2 recipe — the `--adjust-vma` value and architecture — so you don't reconstruct it. |

Reads go through the SMC-safe VMA read seam (`read_bytes_ranged`), never a raw host pointer:
a module whose range is partly unmapped dumps what is readable, zero-fills the holes, and
lists each hole under `[gaps]` in the `.map` — it never faults the emulator or passes a zero
fill off as guest code. HLE stub modules (base 0, size 0) span no range and are skipped.
Zero cost when the env var is unset (a single lookup). The default dir `module-dumps/` is
gitignored (as is any dir you pass); the `.bin` images also match the global `*.bin` ignore.

Flat image + `.map` only — no synthetic-ELF wrapper (a deliberate follow-up if the flat form
proves awkward). This is a dump tool, not a disassembler; it hands the bytes to external tools.

```sh
UNEMUPS4_DUMP_MODULES=module-dumps cargo run --release -p unemups4 -- /path/to/eboot.bin
# then disassemble around a backtrace offset (e.g. eboot.bin +0x16de90):
objdump -D -b binary -m i386:x86-64 --adjust-vma=$(grep '^base' module-dumps/eboot.bin.map | awk '{print $2}') \
  module-dumps/eboot.bin.bin | less
# ...or jump straight to the faulting instruction's file offset:
objdump -D -b binary -m i386:x86-64 --start-address=0x16de90 --stop-address=0x16dec0 \
  module-dumps/eboot.bin.bin
```
