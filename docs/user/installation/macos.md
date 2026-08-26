# Package PaneFlow on macOS

> Turn a release binary into a launchable `.app`, put `paneflow` on your
> `PATH`, and deal with Gatekeeper. Prerequisites and `cargo build` live
> in [`INSTALL.md`](../../../INSTALL.md).

This fork ships no download. Build from source first, then come here if
you want a bundle you can drop in `/Applications`.

## Assemble an .app bundle

`cargo build` produces a bare binary, not a bundle. To get a launchable
`PaneFlow.app`:

```bash
cargo build --release
scripts/bundle-macos.sh --version 0.1.0 --arch aarch64
```

That writes `dist/PaneFlow.app` with the executable at
`Contents/MacOS/paneflow`, `Info.plist` (with the version substituted),
and `Resources/PaneFlow.icns`.

Signing, notarization, and DMG creation are separate scripts:
`scripts/sign-macos.sh`, `scripts/notarize-macos.sh`,
`scripts/create-dmg.sh`. See
[../../release/macos-signing.md](../../release/macos-signing.md).

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
but no Intel build is produced or tested in this fork. Apple Silicon
only.
