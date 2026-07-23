# ps4-gnm-scrape — real-PS4 GNM command-buffer scraper (task-168)

Ground-truth oracle for the **task-157** contradiction: our emulator's captured
steady-state Celeste DCB does **not** contain the 8-dword atlas `SET_SH_REG
0x2c0c` bind (base `0x9afc28000`), yet Mesa proves real hardware needs it
re-emitted every frame. This tool captures the **real** PM4 stream off a
jailbroken PS4 running Celeste and feeds it through our own `ps4_gnm::pm4`
decoder, so we can answer decisively: *does the real steady-state flip DCB
contain that 8-dword atlas bind every frame?*

```
[ PS4: Celeste + GoldHEN plugin ] --TCP--> [ PC: receiver -> dumps/ ] --> decode
      (hooks sceGnmSubmit*)         9010        (192.168.100.1)         (analysis)
```

Three pieces:

- **`plugin/`** — a GoldHEN `.prx` that runs inside Celeste, hooks the
  `sceGnmSubmit*` family, and streams each DCB/CCB over TCP.
- **`host/`** — a Rust crate with two bins: `receiver` (TCP server → per-frame
  dumps) and `decode` (runs a dumped DCB through the real PM4 decoder + atlas
  analysis).
- this file.

---

## 0. Prerequisites (confirmed environment)

- OpenOrbis toolchain at `data/oo_sdk` (gitignored). Tools in
  `data/oo_sdk/bin/linux/` (`create-fself`, …).
- Jailbroken PS4, FW 11.00, GoldHEN installed, Celeste = **CUSA11302**.
- Direct network cable: **PC = 192.168.100.1**, **PS4 = 192.168.100.2**.
- The plugin is the **TCP client**; the PC runs the **TCP server** on port
  **9010** (`#define PC_PORT` in `plugin/source/main.c`).

---

## 1. Build the GoldHEN SDK (once)

The plugin links `libGoldHEN_Hook.a` + `build/crtprx.o` from a built copy of the
GoldHEN Plugins SDK. Clone it to the gitignored `data/goldhen_sdk/` and build:

```sh
cd "$REPO_ROOT"                       # /home/mikolaj/src/unemups4
git clone --depth 1 https://github.com/GoldHEN/GoldHEN_Plugins_SDK.git data/goldhen_sdk
OO_PS4_TOOLCHAIN="$PWD/data/oo_sdk" make -C data/goldhen_sdk
# produces data/goldhen_sdk/libGoldHEN_Hook.a and data/goldhen_sdk/build/crtprx.o
```

(`data/goldhen_sdk/` is gitignored — MIT-licensed, vendored per-checkout, not
committed.)

## 2. Build the plugin `.prx`

```sh
cd "$REPO_ROOT"
OO_PS4_TOOLCHAIN="$PWD/data/oo_sdk" make -C tools/ps4-gnm-scrape/plugin
# produces tools/ps4-gnm-scrape/plugin/ps4-gnm-scrape.prx
```

`GOLDHEN_SDK` defaults to `$(OO_PS4_TOOLCHAIN)/../goldhen_sdk` (i.e.
`data/goldhen_sdk`); override it if you cloned elsewhere:
`GOLDHEN_SDK=/path/to/sdk make -C tools/ps4-gnm-scrape/plugin`.

## 3. Deploy the plugin to the PS4

Copy the `.prx` to the console and register it under Celeste's title id.

1. Copy `ps4-gnm-scrape.prx` to `/data/GoldHEN/plugins/` on the PS4 (FTP /
   ps4link / etc).
2. Edit `/data/GoldHEN/plugins.ini` and add it under the **CUSA11302** section
   (create the section if absent). GoldHEN loads per-title plugins from the
   section named by the running title id:

   ```ini
   [CUSA11302]
   /data/GoldHEN/plugins/ps4-gnm-scrape.prx
   ```

   > **You must set the title id yourself.** CUSA11302 is Celeste's id per the
   > task; confirm against your dump if unsure. GoldHEN also supports a
   > `[default]` section that loads for every title — do **not** use it here
   > (this plugin only makes sense inside Celeste).

## 4. Start the PC receiver **before** launching Celeste

```sh
cd "$REPO_ROOT"
cargo run -p ps4-gnm-scrape-host --bin receiver
#   defaults: bind 0.0.0.0:9010, dumps -> ./dumps
#   override: cargo run -p ps4-gnm-scrape-host --bin receiver -- 0.0.0.0:9010 /path/to/dumps
```

The plugin reconnects gracefully if the listener is down (it drops the dump on
any socket error and retries every ~120 submit calls), but starting the
receiver first captures from frame 0. Make sure the PC firewall allows inbound
TCP 9010 on the 192.168.100.1 interface.

## 5. Launch Celeste, drive to the scene, then trigger the capture

Launch Celeste on the PS4. **Capture is OFF at load** — the plugin hooks the
submit path but streams nothing until you trigger it (task-198). Drive to the
target scene with the pad, then press **R1 + L1 + X** (edge-detected toggle) to
START; press the same combo again to STOP. A hard safety cap auto-stops the
capture ~10 s after START regardless of the toggle (`CAPTURE_MAX_SECONDS = 10`
wall-clock, primary; `CAPTURE_MAX_FLIPS = 700` flip backstop — a `#define` in
`plugin/source/main.c`); this plus the zero-run RLE (below) keeps the ~4 MB
mostly-zero DCB from flooding the wire.

> **(Re)start the PC receiver (section 4) before each triggered capture** — a
> toggle-START resets the plugin's frame numbering to 0, so a stale receiver
> session would collide old and new `frameNNNNNN` files.

`Ctrl-C` the receiver when done. (If pad init fails, the plugin logs a warning
and falls back to the old always-on capture so the tool is never silently
disabled.)

Dumps land as (already de-RLE'd, raw command-buffer bytes):

```
dumps/frame000000_sub0_flip_dcb.bin   + .txt   (metadata sidecar)
dumps/frame000000_sub1_flip_dcb.bin   + .txt
dumps/frame000000_sub0_flip_ccb.bin   ...       (only if a CCB was submitted)
```

`frameNNNNNN` = the plugin's monotonic per-submit-call counter, `subN` = index
within the submit batch (Celeste submits `count=2`), `flip`/`submit`/`flipwl`/
`submitwl` = which entry point, `dcb`/`ccb` = buffer kind.

## 6. Decode a captured DCB

```sh
cd "$REPO_ROOT"
cargo run -p ps4-gnm-scrape-host --bin decode -- dumps/frame000300_sub1_flip_dcb.bin
#   optional overrides: decode <file.bin> [ATLAS_BASE_HEX] [TARGET_SH_REG_HEX]
#   defaults: atlas base 0x9afc28000, target SH reg 0x2c0c
```

The output lists every `SET_SH_REG` write to the target reg (`0x2c0c`), decodes
any 8-dword T# and checks its base against the atlas base, lists PS program
binds and draws, scans the raw bytes for the atlas base address, and ends with a
one-line **VERDICT**:

- `PRESENT` — this DCB re-emits the 8-dword atlas bind (⇒ our emulation makes
  the guest build a shorter list = **our bug**, likely GPU-readback / fence
  timing; and we now have a reference stream).
- `ABSENT` — no write to the target SH reg (⇒ the GPU keeps the atlas resident
  differently than we model, or the visual expectation is wrong).
- `PARTIAL` — the reg is written but not as an 8-dword T# (inspect the dword
  counts printed above).

### Warm-up vs steady-state

The first ~2 flips are **warm-up**: Celeste emits a longer setup stream (full
render-state init, all descriptor binds) before it settles. The task-157
contradiction is about the **steady-state** logo frames, so decode a DCB from a
few hundred frames in (e.g. `frame000300_*`), not `frame000000_*`. Compare an
early warm-up frame against a late steady-state frame with the same `decode`
command: if the atlas 0x2c0c bind is PRESENT in warm-up but ABSENT in
steady-state on real hardware too, that matches our capture and the wall moves to
"how does real HW keep the atlas resident across frames". If it is PRESENT every
steady-state frame, the guest builds it every frame on real HW and our capture is
missing it — our bug.

Batch the whole capture:

```sh
for f in dumps/frame*_dcb.bin; do echo "== $f =="; \
  cargo run -q -p ps4-gnm-scrape-host --bin decode -- "$f" | tail -1; done
```

---

## 7. Diff a console frame against one of OUR frames (`framediff`)

`decode` answers questions about the console alone. **`framediff` answers the
question you usually actually have** — *what does the console do here that we
don't?* — by diffing a captured console frame against one of our own GPU
snapshots, per draw.

```sh
cd "$REPO_ROOT"
# 1. capture one of our frames (F9 in-game, or UNEMUPS4_SNAPSHOT_*; see
#    backlog/docs — this writes gpu-snapshots/frame-NNNNN/)
# 2. diff it against the console capture:
cargo run -p ps4-gnm-scrape-host --bin framediff -- \
    dumps/scrape2 gpu-snapshots/frame-02143 --frame 4
#   dumps/scrape2            a receiver output dir (DCBs + probe dumps)
#   gpu-snapshots/frame-NNNNN  one of our snapshot frame dirs
#   --frame N                which console frame to use (default: the last one)
#   --verbose                also print draws that are identical
```

It prints four sections:

1. **Draw matching heuristic.** Draws are paired **by ordinal** and each pair is
   scored on three independent signals — draw kind, target extent, and
   `CB_BLEND0_CONTROL`. Console and emulator guest addresses are unrelated, so
   the *match*, not any address, is what identifies a surface. If this section
   does not say `N/N draws matched on all three signals`, the two captures are
   not the same workload and **everything below is void** — recapture first.
2. **Derived address correspondence.** The console→ours address map that falls
   out of the match (targets, plus sampled textures whose extents agree). This
   is the table that makes any address-level claim possible; eyeball it.
3. **Per-draw diff.** Registers whose values differ (address-bearing registers
   are excluded — they differ by construction), and descriptors: what the
   console binds at each texture slot vs what our snapshot recorded, including
   our `descriptor_honoured` flag. Only differing draws are listed unless
   `--verbose`.
4. **Register census.** Every register the console writes that our register file
   **never receives**. The usual explanation is the default-hardware-state
   preamble: `sceGnmDrawInitDefaultHardwareState*` /
   `sceGnmDrawInitToDefaultContextState*` in
   `crates/libs/src/libscegnmdriver/hwstate.rs` are stubs that return the dword
   count and emit no PM4, so nothing the real builders would program reaches us.

### Reading it

A clean run looks like `registers : identical on every register both sides
recorded` on every draw — that is a strong statement, and it is what proved the
Celeste yellow-sky bug was **not** a blend/format/swizzle problem (task-199).
The finding then showed up in the descriptor lines instead:

```
draw  14  DrawIndexOffset  console 0x2bcf30000 384x192 | ours 0x9afb58000 320x180
     registers : identical on every register both sides recorded
     tex0: console 0x2bcee8000 (register-resident) -> 0x9afb10000 | ours 0x9afc30000  <<< MISMATCH
     tex1: console 0x2bd008000 (memory-resident)   | ours NOT BOUND
```

Two caveats the tool states in its own output, and which you must respect:

- Console-side textures are decoded from the **PS user-data registers in effect
  at the draw**. Those persist across draws, so a draw whose PS samples nothing
  still shows the previous draw's descriptor; those lines are labelled *stale*
  rather than reported as a missing bind.
- The console reports a target's **padded** extent (`CB_COLOR0_PITCH`/`SLICE`
  are TILE_MAX fields — 384x192 for a 320x180 target); our snapshot reports the
  logical extent plus the same padded pitch. The tool compares padded-to-padded;
  that is why the two columns legitimately print different numbers.

Register field layouts come from the AMD GCN ISA and Mesa `src/amd/common/sid.h`
only (a context byte address is dword `(R_028xxx - 0x28000) / 4`).

**No capture data is committed.** `dumps/` and `gpu-snapshots/` are untracked;
`framediff` only ever opens them read-only.

---

## Wire format (framing + RLE)

Canonical definition + the RLE algorithm live in `host/src/lib.rs` (module
docs); the plugin's C encoder in `plugin/source/main.c` mirrors it byte-for-byte
(verified by the `ps4_gnm_scrape` round-trip tests and a C↔Rust cross-check).

Per buffer: a 20-byte little-endian header

```
magic u32 = 0x344D4E47 ("GNM4") | frame u32 | kind u8 | buf_index u8 |
is_ccb u8 | flip u8 | raw_size u32 | comp_size u32
```

followed by `comp_size` bytes of zero-run RLE payload (chunks of
`op u8 (0=literal,1=zero-run) | len u32 | [literal body]`; runs of ≥ 8 zeros are
collapsed). This collapses Celeste's ~4 MB mostly-zero DCB to a few dozen bytes
while preserving every non-zero byte verbatim.

## Troubleshooting

- **No connection / no dumps.** Firewall on the PC blocking inbound TCP 9010;
  wrong title id in `plugins.ini`; cable/IP misconfig (`ping 192.168.100.2`).
  Check the PS4 klog (`[gnm-scrape]` lines) via the GoldHEN log viewer.
- **Nothing captured.** Capture is OFF until you press **R1+L1+X** (section 5).
  Check the klog for `capture STARTED`. Each run also self-stops at the hard cap
  (`CAPTURE_MAX_SECONDS` / `CAPTURE_MAX_FLIPS`); raise those `#define`s and
  rebuild for longer captures.
- **Sockets fail on-console.** This plugin uses BSD sockets (libkernel), the
  standard GoldHEN payload-logging path. If a firmware quirk blocks them, switch
  the socket calls to the `sceNet*` API (`sceNetInit` + `sceNetSocket` /
  `sceNetConnect` / `sceNetSend`, headers in `data/oo_sdk/include/orbis/Net.h`)
  and add `-lSceNet` to the plugin `Makefile` `LIBS`.
