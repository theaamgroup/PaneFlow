# PaneFlow (macOS-only fork)

Internal docs for The AAM Group's private macOS-only fork of PaneFlow.
Not for publication.

PaneFlow is a native, GPU-accelerated terminal workspace built for
agentic CLI workflows: Claude Code, Codex, OpenCode, and any other agent
that speaks plain shell. Split panes, persistent sessions, dev-server
port detection, and bundled themes.

This fork drops Windows and Linux support entirely. There is no public
download, no Homebrew tap, and no public issue tracker. Build it from
source: see [INSTALL.md](../../INSTALL.md).

## Contents

**Getting it running**

* [INSTALL.md](../../INSTALL.md) - build from source, including the Metal toolchain step that is easy to miss.
* [installation/macos](installation/macos.md) - `.app` bundle, PATH symlink, Gatekeeper.
* [troubleshooting](troubleshooting.md) - symptom-first fixes.

**Using it**

* [features](features.md) - the capability tour.
* [keybindings](keybindings.md) - default shortcuts and overrides.
* [layouts](layouts.md) - the four layout presets.
* [review](review.md) - the Diff view and agent review.
* [themes](themes.md) - the bundled themes.

**Configuring it**

* [installation](installation.md) - where the config file lives for debug vs release.
* [settings](settings.md) - the Settings panel and what it writes.
* [configuration](configuration.md) - how `paneflow.json` is read.
* [configuration/schema](configuration/schema.md) - every recognised key, type, and default.

**Automating it**

* [scripting](scripting.md) - CLI, JSON-RPC, events, workspace and flow files, MCP, hooks.
* [scripting/reference](scripting/reference.md) - the exact surface to quote.
* [conductor](conductor.md) - driving agent panes from the CLI. Read the status note first: this is known-unreliable.
* [conductor/reference](conductor/reference.md) - conductor verbs, fields, and events.

## What runs on the inside

* **Pure Rust** + [GPUI](https://www.gpui.rs/), the same framework Zed runs on. Rendering goes through Metal.
* **Native VT emulation** via `alacritty_terminal` (crates.io 0.26), which also owns the PTY (`tty::new`).
* **JSON-RPC IPC** over a Unix domain socket under the user runtime directory. See [scripting/reference](scripting/reference.md#json-rpc-connection).
* **Latency probes** for cold start, keystroke-to-pixel, and pixel-coordinate tracing. Debug builds only (`#[cfg(debug_assertions)]`), opt in with `PANEFLOW_LATENCY_PROBE=1` or `PANEFLOW_PIXEL_PROBE=1`.

## System requirements

| Requirement | Value |
| --- | --- |
| OS | macOS 13 Ventura or later (`LSMinimumSystemVersion` in `assets/Info.plist`) |
| Architecture | Apple Silicon. There is no `x86_64-apple-darwin` build. |
| GPU | Metal, built in |

## Licence

GPL-3.0-or-later. GPUI is a Zed fork, so the licence is mandatory, not a
choice. See [LICENSE](../../LICENSE).

## Fork context

Fork rationale, staged cut plan, verified traps, and the running defect
list live in
[docs/fork/2026-08-25-mac-only-fork-design.md](../fork/2026-08-25-mac-only-fork-design.md).
