# Installation

This fork is macOS-only and unpublished. There is no download page, no
DMG release, and no Homebrew tap. The only install path is building from
source.

Read [installation/macos.md](installation/macos.md) for the full
walkthrough. The short version:

```bash
brew install cmake
sudo xcode-select -s /Applications/Xcode.app/Contents/Developer
xcodebuild -downloadComponent MetalToolchain
cargo build --release
```

Full Xcode is required (Command Line Tools are not enough), and the Metal
Toolchain is a separate download on top of Xcode. Both traps are
explained in the macOS guide, along with why `xcrun -f metal` is not a
valid readiness check.

## What you get

A single `paneflow` binary at `target/release/paneflow`. Run
`scripts/bundle-macos.sh` to wrap it into `dist/PaneFlow.app`. No
services, no daemons, no background processes.

## Where the config lives

| Build | Config path |
| --- | --- |
| Release | `~/Library/Application Support/paneflow/paneflow.json` |
| Debug (`cargo run`) | `~/Library/Application Support/paneflow-dev/paneflow.json` |

Every key is optional, so an empty `{}` is valid and no config file at
all is also valid. See [configuration](configuration.md) and
[configuration/schema](configuration/schema.md).

## Need help?

* PATH and CLI access: [installation/macos.md](installation/macos.md#put-the-cli-on-your-path)
* Gatekeeper blocks: [installation/macos.md](installation/macos.md#what-if-macos-blocks-the-app)
* Anything else: [troubleshooting.md](troubleshooting.md)
