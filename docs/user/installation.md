# Installation

This fork is macOS-only and unpublished. There is no download page, no
DMG release, and no Homebrew tap. The only install path is building from
source.

- From-source setup (Rust, Xcode, Metal, cmake, `cargo run`):
  [`INSTALL.md`](../../INSTALL.md)
- Packaging a `.app`, Gatekeeper, and putting the CLI on `PATH`:
  [installation/macos.md](installation/macos.md)
- Symptom-first fixes: [troubleshooting.md](troubleshooting.md)

What you get is a single `paneflow` binary. No services, no daemons, no
background processes. Wrap a release build with
`scripts/bundle-macos.sh --version 0.1.0 --arch aarch64` to produce
`dist/PaneFlow.app`.

## Where the config lives

| Build | Config path |
| --- | --- |
| Release | `~/Library/Application Support/paneflow/paneflow.json` |
| Debug (`cargo run`) | `~/Library/Application Support/paneflow-dev/paneflow.json` |

Every key is optional, so an empty `{}` is valid and no config file at
all is also valid. See [configuration](configuration.md) and
[configuration/schema](configuration/schema.md).
