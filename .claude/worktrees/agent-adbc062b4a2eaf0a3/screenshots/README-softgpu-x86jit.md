# ps4-softgpu under the x86jit interpreter (task-7 evidence)

Two pieces of evidence that `ps4-softgpu.elf` renders correctly when the guest
runs on the x86jit interpreter backend (tasks 1-6). The example (see
`examples/ps4-softgpu/ps4-softgpu/main.cpp`) double-buffers a 1920x1080 RGBA
frame: it fills the whole buffer with a background colour that alternates
blue (`0xFF0000FF`) / red (`0xFFFF0000`) every 60 frames, draws a 100x100 white
box that moves by `(frame*5, frame*3)` px per frame, then `sceVideoOutSubmitFlip`s.

## softgpu-x86jit.png — live on-screen render (primary AC #1 evidence)

Full window capture (`spectacle -b -n -a`) of the emulator window
(`unemups4 - N FPS`) while `ps4-softgpu.elf` runs live under x86jit, presenting
frames through the real Vulkan + winit/Wayland display loop. Shows the expected
solid background with the moving white box. Same capture format / resolution
(2050x1238, identical titlebar chrome) as the pre-existing native
`mandelbrot.png` / `input-test.png`, so it is directly comparable.

How produced (host session, OUTSIDE the nix devShell, which lacks a usable
Wayland runtime lib):

    cargo build --release   # or the devShell binary
    LD_LIBRARY_PATH=/usr/lib WAYLAND_DISPLAY=wayland-0 XDG_RUNTIME_DIR=/run/user/1000 \
      ./target/release/unemups4 examples/ps4-softgpu/ps4-softgpu.elf
    # then: spectacle -b -n -a -o softgpu-x86jit.png

Note: inside the nix devShell winit panics with `WaylandError(NoWaylandLib)`
(no `libwayland-client.so` on its loader path); the task-5 park-on-panic guard
keeps the guest alive there. Pointing the loader at the system `/usr/lib` makes
a real window open.

## softgpu-x86jit-fbdump-frame0.png — programmatic framebuffer dump (backup)

Frame 0 of the guest framebuffer, read straight out of the identity-mapped host
arena (guest addr == host addr) at the address the guest malloc'd and registered
via `sceVideoOutRegisterBuffers` (buffer0 @ `0x400214000` in the run that
produced this). Proves the guest's JIT-executed stores land in exactly the host
RAM the display path reads via `get_host_ptr`. Content check: 2 distinct pixel
values only — `0xFF0000FF` blue = 99.52%, `0xFFFFFFFF` white = exactly 10000 px
(= the 100x100 box), box at top-left as expected for frame 0 (boxX=boxY=0).

How produced: a TEMPORARY, uncommitted env-gated hook in
`crates/kernel/src/bridge.rs::video_out_submit_flip` read the drawn framebuffer
via `memory.read_bytes` on the first flip and wrote raw BGRA bytes; converted
with `magick -size 1920x1080 -depth 8 BGRA:fb.raw out.png`. Hook reverted after
capture.
