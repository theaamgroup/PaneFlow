# Build PaneFlow on macOS

> Build the macOS-only fork from source. Covers the Rust pin, the two-step Xcode Metal toolchain requirement, cmake, and how to produce a runnable `.app`.

This fork ships no download. There is no public release, no DMG, and no
Homebrew tap. Building from source is the only install path.

Nothing here is a two-minute install. Budget time for the Xcode
prerequisites, which are non-obvious and fail in a misleading way.

## Prerequisites

| Requirement | Version | Notes |
| --- | --- | --- |
| macOS | 13 Ventura or later | `LSMinimumSystemVersion` in `assets/Info.plist` |
| Architecture | Apple Silicon | No `x86_64-apple-darwin` build exists |
| Rust | 1.96.1 | Pinned by `rust-toolchain.toml`. `rustup` honours the pin automatically. |
| Xcode | Full Xcode, plus the Metal Toolchain component | Two separate steps. See below. |
| cmake | any recent | `brew install cmake` |

The dependency graph floor is rustc 1.92 (oo7 0.6, cosmic-text 0.17,
smol_str 0.3, several wgpu and zbus crates), but the toolchain file pins
1.96.1 to match CI. Do not build on an older toolchain.

## Xcode: two steps, and the second one is easy to miss

GPUI compiles Metal shaders during the build, so the build needs the
Metal shader compiler. Getting it takes two separate installs.

**Step 1. Install full Xcode.** Command Line Tools are not sufficient.
Under CLT alone there is no `metal` tool to resolve at all and the build
fails outright.

**Step 2. Download the Metal Toolchain component.** Xcode 26 ships the
Metal toolchain as a separately downloadable component, so even with
full Xcode installed, `xcrun metal` fails with:

```
cannot execute tool 'metal' due to missing Metal Toolchain
```

Fix it with:

```bash
sudo xcode-select -s /Applications/Xcode.app/Contents/Developer
xcodebuild -downloadComponent MetalToolchain
```

### Do not use `xcrun -f metal` as the readiness check

`xcrun -f metal` resolves and prints the tool path successfully even
when the Metal toolchain is absent. It will tell you everything is fine
while the build is still broken. Compile something instead:

```bash
printf '#include <metal_stdlib>\nkernel void k() {}\n' > /tmp/probe.metal
xcrun metal -c /tmp/probe.metal -o /tmp/probe.air && echo "Metal toolchain OK"
```

If that command prints `Metal toolchain OK`, the toolchain is really
present. If it prints the missing-toolchain error, go back to step 2.

## Build and run

```bash
cargo build                     # debug
cargo run                       # debug build, launches the app
cargo build --release           # LTO thin, strip, codegen-units=1
```

Logging and probes:

```bash
RUST_LOG=info cargo run
PANEFLOW_LATENCY_PROBE=1 cargo run   # keystroke-to-pixel tracing, debug builds only
```

Tests and lints:

```bash
cargo test --workspace
cargo clippy --workspace -- -D warnings
cargo fmt --check
```

## Debug builds use a separate config and socket namespace

This catches people out. Debug builds namespace every persistence
surface under `paneflow-dev` instead of `paneflow`, so a `cargo run`
build and an installed release build do not share state:

| Surface | Release build | Debug build |
| --- | --- | --- |
| Config file | `~/Library/Application Support/paneflow/paneflow.json` | `~/Library/Application Support/paneflow-dev/paneflow.json` |
| IPC socket | `<runtime dir>/paneflow/paneflow.sock` | `<runtime dir>/paneflow-dev/paneflow-dev.sock` |

The rule lives in `APP_SUBDIR` in `src-app/src/runtime_paths.rs`, mirrored
by `paneflow_config::APP_SUBDIR` and `paneflow_threads::APP_SUBDIR`. If you
edit `paneflow.json` and a `cargo run` build ignores it, you edited the
release path.

## Assemble an .app bundle

`cargo build` produces a bare binary, not a bundle. To get a launchable
`PaneFlow.app`:

```bash
cargo build --release
scripts/bundle-macos.sh --version 0.8.2 --arch aarch64
```

That writes `dist/PaneFlow.app` with the executable at
`Contents/MacOS/paneflow`, `Info.plist` (with the version substituted),
and `Resources/PaneFlow.icns`.

Signing, notarization, and DMG creation are separate scripts:
`scripts/sign-macos.sh`, `scripts/notarize-macos.sh`,
`scripts/create-dmg.sh`. See [../../release/macos-signing.md](../../release/macos-signing.md).

## Verify the build

```bash
target/release/paneflow --version
```

Or, from an assembled bundle:

```bash
dist/PaneFlow.app/Contents/MacOS/paneflow --version
```

## Put the CLI on your PATH

The app bundle does not add `paneflow` to `PATH`. Symlink it once:

```bash
sudo ln -sf /Applications/PaneFlow.app/Contents/MacOS/paneflow /usr/local/bin/paneflow
paneflow --version
```

## What if macOS blocks the app?

An unsigned locally built bundle has no quarantine attribute, so it
normally launches without a Gatekeeper prompt. If it is blocked (for
example after being copied off another machine, or downloaded from an
internal share), use one of these:

1. In Finder, Control-click the app, choose **Open**, then confirm **Open**.
2. In **System Settings** > **Privacy & Security**, find the blocked app notice and choose **Open Anyway**.
3. Remove the quarantine attribute:

```bash
xattr -dr com.apple.quarantine /Applications/PaneFlow.app
```

## Intel Macs

Not supported. `scripts/bundle-macos.sh` still accepts `--arch x86_64`,
but no Intel build is produced or tested in this fork and there is no
fallback target to point at. Apple Silicon only.
