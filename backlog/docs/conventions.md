# Coding conventions

Generic conventions that apply regardless of stack. Stack-specific rules (language idioms, framework patterns, formatter config) go in the **Stack-specific** section at the bottom — filled at bootstrap.

## File naming

- Pick **one** casing per category and stick with it (e.g. PascalCase for components, kebab-case for scripts, snake_case for modules). Document the choice in Stack-specific.
- One unit per file (one component, one class, one primary export). Co-locate tightly related sibling files (CSS module next to component, test next to source).

## Imports

- Cross-folder imports go through a folder's barrel / public entry, not into its internals. The barrel is the contract; internals are not.
- Prefer path aliases (`@core`, `@services`, ...) over deep relative paths once the project grows past ~3 directory levels.

## Comments

- **Default: no comment.** Names do the work.
- Add only when the *why* is non-obvious: hidden constraint, subtle invariant, workaround for a specific bug, surprising behavior.
- Never explain *what* the code does — well-named identifiers already do that.
- Don't reference the current task / fix / PR ("added for X", "handles case from #123") — that belongs in the commit message, not the source file.

## Commits

- Conventional Commits by default (`feat:`, `fix:`, `chore:`, `docs:`, `refactor:`, `test:`, `release:`). If your project uses a different convention, document in Stack-specific.
- GPG-signed. The `gpg-uid-guard` pre-commit hook refuses to sign if `user.email` has no matching UID on `user.signingkey`.
- **Never commit without explicit user request.** This rule supersedes any plan acceptance.

## Phase / scope discipline

- Don't pre-empt later milestones. If a task is scoped to a later milestone, don't half-implement it during earlier work.
- If a refactor would be cleaner alongside a bug fix but isn't required, defer it — create a backlog task instead.
- Don't add error handling, fallbacks, or validation for scenarios that can't happen at the call site. Trust internal code; validate only at system boundaries (user input, external APIs).

## UI / output text (if applicable)

- Terse. Lowercase. No emoji in UI text or logs unless the project explicitly opts in.

## When in doubt

- Read the relevant decision: `ls backlog/decisions/` (see [`decisions.md`](decisions.md)).
- Check active work: `backlog task list --plain`.
- Ask the user. Solo project — they're the only deciding authority.

---

## Stack-specific

**Rust**

- Edition **2024** across every workspace crate. Cargo workspace, `resolver = "2"`.
- Format with **rustfmt defaults** (no custom `rustfmt.toml`) — run `cargo fmt`.
- Lint with `cargo clippy --all-targets --all-features -- -D warnings`.
- Errors: `thiserror` for library error types (`thiserror` is a workspace dependency); no `anyhow` in the workspace deps.
- Crate naming: library crates are prefixed `ps4-` (e.g. `ps4-core`, `ps4-cpu`); the app crate is `unemups4`. Module files are `snake_case`.
- Syscall/library handlers register via the `#[ps4_syscall]` proc-macro + the `inventory` crate — add handlers that way, don't hand-edit a dispatch table.
- Keep the crate graph acyclic: the `KernelInterface` trait lives in `ps4-core`; concrete `KernelBridge` lives in `ps4-kernel` (see [`architecture.md`](architecture.md)).

**Licensing**

- Project is **GPL-3.0** (`COPYING`). Keep new files compatible; don't introduce GPL-incompatible dependencies.

**Test strategy**

- Standard `cargo test`. No integration harness beyond that yet; the `examples/` ELFs double as manual smoke tests via `cargo run`.
