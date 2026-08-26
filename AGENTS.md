# Repository Guidelines

## Project Structure & Module Organization
PaneFlow is a Rust workspace, macOS only in this fork. `src-app/` is the `paneflow` desktop binary and CLI entrypoint: UI, terminal rendering, pane management, IPC, themes, and embedded helper binaries under `src-app/assets/`. `crates/paneflow-*` holds the config, IPC client, process, ACP, shim, AI-hook, MCP, and MCP-installer crates. Top-level `assets/` holds macOS bundle inputs, `scripts/` utility scripts, `schemas/` the published config JSON Schema, and `skills/` the conductor skill.

## Build, Test, and Development Commands
Run all commands from the repository root.

- `cargo build` builds the workspace.
- `cargo build --release` builds the optimized app binary.
- `cargo run -p paneflow-app` launches the app locally.
- `RUST_LOG=info cargo run -p paneflow-app` runs with structured logging enabled.
- `cargo test --workspace` runs unit and integration tests across every crate.
- `cargo test -p paneflow-app --test flex_nchild -- --nocapture` runs the GPUI layout integration tests only.
- `cargo clippy --workspace -- -D warnings` treats lint warnings as errors.
- `cargo fmt --check` verifies formatting.

GPUI and the Alacritty VT crate are **not** local path dependencies. GPUI and its five sibling Zed crates (`gpui_platform`, `collections`, `markdown`, `theme`, `ui`) are git dependencies pinned by exact `rev` to `arthjean/zed` (`src-app/Cargo.toml:64-84`, plus a test-support `gpui` in `[dev-dependencies]` at `:417`), and `alacritty_terminal` comes from crates.io (`src-app/Cargo.toml:87`). Cargo fetches both automatically, so no checkout has to be kept on disk. Never swap the Zed git deps for crates.io versions: GPUI is not published there.

Build prerequisites (Rust 1.96.1, full Xcode, and the separately downloaded Metal toolchain) are documented in `CLAUDE.md`. They are non-obvious and a missing one fails the build in a confusing way.

## Coding Style & Naming Conventions
Standard Rust formatting via `cargo fmt`: 4-space indentation, Rust defaults. Modules and files in `snake_case` (`config_writer.rs`, `service_detector.rs`), types in `UpperCamelCase`, functions and tests in `snake_case`. Prefer small, focused modules and brief doc comments where behavior is not obvious. Inline GPUI styling is the established pattern; match the existing builder-chain style instead of introducing a separate styling layer.

## Testing Guidelines
Put unit tests alongside the module when the logic is self-contained, as in `src-app/src/workspace/mod.rs` and `crates/paneflow-config/src/*.rs`. Keep broader UI and layout checks in `src-app/tests/`. Name tests descriptively, for example `test_three_children_flex_basis`. Run `cargo test --workspace`, `cargo clippy --workspace -- -D warnings`, and `cargo fmt --check` before opening a PR. UI changes still need manual verification.

## Pre-commit checks (mandatory)
**Before EVERY `git commit` and EVERY `git push` that touches Rust code, run `cargo fmt --check`.** If it reports a diff, run `cargo fmt`, re-stage, then commit. This is the cheapest guard against the most expensive CI failure on this repo: the release pipeline runs `cargo fmt --check` inside the Build job, so one mis-formatted line fails the build, skips the publish step, and burns the entire run before producing anything. A dirty tag commit is worse still: the original tagged build cannot be salvaged, so you have to delete and re-create the tag at the fix commit. Run `cargo fmt --check` one last time on the exact commit you are about to tag. rustfmt also drifts between Rust point releases, so code that was clean last week can need re-formatting after a toolchain bump.

## Commit guidelines
This is a private fork. There is no CONTRIBUTING.md, SECURITY.md, or public
advisory process. History uses Conventional Commit prefixes plus a scope, for
example `feat(app): adapt paneflow-hook for Codex PID env var`. Follow
`type(scope): description`. Use `(fork)` for anything that diverges from
upstream. `panic!`, `unimplemented!`, and `dbg!` are denied by workspace clippy;
`todo!` warns. Verify load-bearing claims by running them.

## Platform
macOS only. Metal, AppKit, `alacritty_terminal`, Unix-socket IPC, signed and notarized `.app` / `.dmg`. There is no Linux or Windows target in this fork: do not add `#[cfg(target_os = "linux")]` or `#[cfg(windows)]` branches back, and do not reintroduce the Ghostty backend. Config lives at `~/Library/Application Support/paneflow/paneflow.json`.

## Deeper reference
`CLAUDE.md` is the detailed engineering reference: annotated module tree, thread model, keystroke-to-pixel flow, GPUI Entity/Element patterns, hard-won scroll and wheel gotchas, the keybinding table, IPC methods, config shape, and gotchas. `docs/fork/2026-08-25-mac-only-fork-design.md` records this fork's decisions, its leak register, and a 12-item traps register. Read both before touching platform code. Do not duplicate their content here.
