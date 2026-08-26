# Build PaneFlow from source

This fork has no download, installer, or Homebrew tap. Building from
source is the only way to run it.

You need macOS 13 Ventura or later on Apple Silicon. Two of the
toolchain steps below are easy to get wrong and fail in a misleading
way. If `cargo build` dies on Metal shaders, skip to the Xcode section.

For how the pieces fit together after it builds, see
[ARCHITECTURE.md](ARCHITECTURE.md). For bundling a `.app`, Gatekeeper,
and putting `paneflow` on your `PATH`, see
[docs/user/installation/macos.md](docs/user/installation/macos.md).

## Prerequisites

**1. Rust 1.96.1.** Pinned by [rust-toolchain.toml](rust-toolchain.toml),
so rustup selects it automatically inside the repo.

```bash
rustup show active-toolchain
# 1.96.1-aarch64-apple-darwin (overridden by '.../paneflow/rust-toolchain.toml')
```

**2. Full Xcode. Command Line Tools are not enough.** GPUI compiles Metal
shaders during `cargo build`, and that compiler only ships with Xcode.

```bash
sudo xcode-select -s /Applications/Xcode.app/Contents/Developer
xcode-select -p    # must print a path inside Xcode.app, not /Library/Developer/CommandLineTools
```

**3. The Metal Toolchain component. Xcode alone is still not enough.**
Xcode 26 ships Metal as a separate download, so a fresh Xcode install
still fails with `cannot execute tool 'metal' due to missing Metal
Toolchain`.

```bash
xcodebuild -downloadComponent MetalToolchain
```

Do not check readiness with `xcrun -f metal`. It prints a path even when
the toolchain is missing. Compile something instead:

```bash
printf 'kernel void probe() {}\n' > /tmp/probe.metal
xcrun metal -c /tmp/probe.metal -o /tmp/probe.air && echo "Metal toolchain OK"
```

**4. cmake**, via Homebrew:

```bash
brew install cmake
```

## Build and run

```bash
cargo run -p paneflow-app          # debug build, launches the app
cargo build --release -p paneflow-app

RUST_LOG=info cargo run -p paneflow-app
PANEFLOW_LATENCY_PROBE=1 cargo run -p paneflow-app   # keystroke-to-pixel, debug only
```

GPUI and the other Zed crates are git dependencies. Cargo fetches them
automatically; no local Zed checkout is needed.

A debug `cargo run` and an installed release build do **not** share
state. Debug builds write config and sockets under `paneflow-dev`
instead of `paneflow`:

| Surface | Release | Debug (`cargo run`) |
|---|---|---|
| Config | `~/Library/Application Support/paneflow/paneflow.json` | `~/Library/Application Support/paneflow-dev/paneflow.json` |
| IPC socket | `$TMPDIR/paneflow/paneflow.sock` | `$TMPDIR/paneflow-dev/paneflow-dev.sock` |

If you edit `paneflow.json` and a `cargo run` instance ignores it, you
edited the release path.

A release-profile local binary (`cargo run --release`,
`./target/release/paneflow`) uses the real `paneflow` namespace and
**will** collide with `/Applications/PaneFlow.app` if that is already
running. Isolate it with:

```bash
PANEFLOW_ALLOW_MULTIPLE=1 PANEFLOW_SOCKET_PATH=/tmp/paneflow-head.sock \
  cargo run --release -p paneflow-app
```

`PANEFLOW_ALLOW_MULTIPLE` is presence-gated: `=0` still skips the
singleton guard.

After a debug build, the CLI is `./target/debug/paneflow`. After a
release build, `./target/release/paneflow`. The `.app` bundle does not
put either on your `PATH`.

## Checks

```bash
cargo fmt --check
cargo clippy --workspace -- -D warnings
cargo test --workspace
./target/debug/paneflow --version    # paneflow 0.1.0
```

`cargo fmt --check` must pass before every commit that touches Rust; CI
fails the whole release job on a single mis-formatted line.

Symptom-first fixes (empty glyph boxes, ignored config, Gatekeeper) live
in [docs/user/troubleshooting.md](docs/user/troubleshooting.md).
