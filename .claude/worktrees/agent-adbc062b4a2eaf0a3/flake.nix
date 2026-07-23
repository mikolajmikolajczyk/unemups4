{
  description = "unemups4 — Rust devShell (Backlog.md + Rust toolchain + tooling)";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    # Local-first task tracker. Provides the `backlog` CLI.
    # Do NOT add `.inputs.nixpkgs.follows` here — the bun2nix build wants its pinned nixpkgs.
    backlog-md.url = "github:MrLesk/Backlog.md";
  };

  outputs =
    {
      nixpkgs,
      flake-utils,
      backlog-md,
      ...
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs { inherit system; };
      in
      {
        devShells.default = pkgs.mkShell {
          packages = [
            backlog-md.packages.${system}.default # `backlog` — local-first task tracker
            pkgs.pre-commit
            pkgs.git

            # --- Rust toolchain ---
            pkgs.cargo
            pkgs.rustc
            pkgs.rust-analyzer
            pkgs.rustfmt
            pkgs.clippy

            # Tracy live-profiling GUI (task-14, `--features profile-tracy`). The wire
            # protocol is version-locked: tracy-client-sys 0.28.0 (via tracing-tracy
            # 0.11.4, per that crate's compat table) speaks the Tracy 0.13.1 protocol,
            # which is exactly what nixpkgs-unstable ships. If you bump tracing-tracy,
            # re-check that table and re-pin the GUI. `tracy` connects to a running
            # emulator; a mismatched GUI refuses the connection.
            pkgs.tracy

            # Linux perf for the profiling workflow (backlog/docs/commands.md).
            # Userspace tool only, works against the host's non-nix kernel;
            # kernel.perf_event_paranoid<=1 still needs root (sysctl).
            pkgs.perf
          ];

          # winit dlopen()s these at runtime (not link-time deps, so plain
          # `packages` entries are not enough) — without them the display
          # thread panics with NoWaylandLib/XKBNotFound and no window opens.
          env.LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath [
            pkgs.wayland
            pkgs.libxkbcommon
            pkgs.vulkan-loader
          ];
        };
      }
    );
}
