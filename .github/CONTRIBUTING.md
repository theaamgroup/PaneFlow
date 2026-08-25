# Contributing to Paneflow

This is a private, internal fork of Paneflow owned by The AAM Group. It is a
native macOS terminal workspace for running AI coding agents in parallel, built
in pure Rust on Zed's GPUI. This file is the working agreement for the team, not
an invitation to outside contributors.

## Before you start

- For anything larger than a small fix, open an issue first so the approach is
  agreed before anyone invests time.
- Check [open issues](https://github.com/theaamgroup/panescli/issues) so two
  people do not land the same change twice.

## Development setup

macOS on Apple Silicon. Rust is pinned via `rust-toolchain.toml`. GPUI compiles
Metal shaders at build time, so you need Xcode **and** the separately
downloadable Metal toolchain (`xcodebuild -downloadComponent MetalToolchain`).
Command Line Tools alone are not enough, and `xcrun -f metal` resolves the path
even when the toolchain is missing, so verify with an actual compile.

```bash
cargo build                              # debug build
cargo run -p paneflow-app                # run it
RUST_LOG=info cargo run -p paneflow-app  # with logs
```

## Before you open a pull request

Run the same gates CI runs. A single formatting diff fails the build, so do not
skip `cargo fmt`.

```bash
cargo fmt --all
cargo clippy --workspace -- -D warnings
cargo test --workspace
```

- **No `panic!`, `unimplemented!`, or `dbg!`** in production code (denied by
  clippy). Prefer `?`, `ok_or(...)?`, `match`, or a documented
  `expect("invariant")` only when provably infallible.
- **macOS only.** This fork does not carry Linux or Windows paths. Delete dead
  `#[cfg(target_os = "linux")]` / `#[cfg(windows)]` branches you touch rather
  than maintaining them, and do not add new ones.
- **Verify, do not assume.** If a claim about behavior is load-bearing in a PR
  description, run the thing and paste the output.

## Commit and branch conventions

```
feat(module): short description
fix(module): short description
refactor(module): short description
docs: short description
chore: short description
```

Fork-specific changes that diverge from upstream use the `(fork)` scope, for
example `chore(fork): drop non-macOS packaging scripts`, so the divergence is
greppable in the log.

Keep commits atomic. Branch from `mac-only-fork` as `feat/<description>` or
`fix/<description>`.

## License

Paneflow is licensed under [GPL-3.0-or-later](../LICENSE), and this fork stays
under those terms. It is not published publicly, but the license obligations
still travel with the code, so keep the headers and `LICENSE` intact.
