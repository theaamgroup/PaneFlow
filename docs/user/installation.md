# Installation

PaneFlow is macOS-only. Download the signed, notarized Apple Silicon DMG
from [GitHub Releases](https://github.com/theaamgroup/PaneFlow/releases/latest),
open it, and drag PaneFlow to Applications. Repository access is required for
this private fork. The current release is 0.7.2. There is no Homebrew tap.

Building and packaging from source remain available:

- From-source setup (Rust, Xcode, Metal, cmake, `cargo run`):
  [`INSTALL.md`](../../INSTALL.md)
- Packaging a `.app`, Gatekeeper, and putting the CLI on `PATH`:
  [installation/macos.md](installation/macos.md)
- Symptom-first fixes: [troubleshooting.md](troubleshooting.md)

The app bundles its helper binaries. To package a local release build, run
`scripts/bundle-macos.sh --version 0.7.2 --arch aarch64` to produce
`dist/PaneFlow.app`.

## Where the config lives

| Build | Config path |
| --- | --- |
| Release | `~/Library/Application Support/paneflow/paneflow.json` |
| Debug (`cargo run`) | `~/Library/Application Support/paneflow-dev/paneflow.json` |

Every key is optional, so an empty `{}` is valid and no config file at
all is also valid. See [configuration](configuration.md) and
[configuration/schema](configuration/schema.md).
