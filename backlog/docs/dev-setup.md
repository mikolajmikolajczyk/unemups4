# Dev setup

Toolchain and local-environment setup. Stack-specific toolchain pinning (Nix flake, mise, asdf, rustup, nvm, uv, ...) is added at bootstrap.

The repo ships a `flake.nix` whose devShell always includes **`backlog`** (the Backlog.md CLI),
`pre-commit`, and `git`; stack packages are appended at bootstrap. Enter it with `nix develop`, or
let direnv auto-load it (`.envrc` is already `use flake`).

## direnv (optional but recommended)

`.envrc` ships in the repo. Allow it once per clone:

```sh
direnv allow
```

If you use a Nix flake (add at bootstrap), `.envrc` typically contains `use flake` and direnv auto-loads the devShell on `cd`.

## Pre-commit

```sh
pre-commit install
```

Hooks come from `.pre-commit-config.yaml`. The template ships generic hooks (whitespace, EOF, YAML/JSON checks, markdownlint, shellcheck, gitleaks, GPG UID guard). Add language-specific hooks (formatter, linter, typechecker) at bootstrap or later.

Run all hooks on demand:

```sh
pre-commit run --all-files
pre-commit run --all-files --hook-stage manual   # includes manual-staged hooks
```

## GPG signing

The `gpg-uid-guard` pre-commit hook (always active) refuses to sign when `user.email` has no matching UID on `user.signingkey`. Fix path if it fails:

```sh
git config user.email <your-email>
git config user.signingkey <key-id>
# or attach a matching UID to the key with `gpg --edit-key <key>`
```

## Stack-specific toolchain

Rust (edition 2024). Two supported paths:

- **Nix flake (recommended)**: `flake.nix`'s devShell ships `backlog`, `pre-commit`, `git`, and the Rust toolchain (`cargo`, `rustc`, `rust-analyzer`, `rustfmt`, `clippy`). Enter with `nix develop`, or let direnv auto-load it (`.envrc` is `use flake`).
- **Plain cargo**: install a recent stable toolchain via `rustup` (edition 2024 needs Rust 1.85+). `cargo build` / `cargo run` work directly; the `backlog` CLI then needs installing separately (see Backlog.md).

Notes:

- `cargo run` needs a working Vulkan loader/driver on the host for the presentation backend, and `winit` (X11/Wayland) for the window.
- Optional: clone the OpenOrbis toolchain into `data/oo_sdk/` for a fuller syscall table (see `commands.md`).
