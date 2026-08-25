# PaneFlow user docs (internal)

Internal documentation for the macOS-only private fork. These pages
describe only what this fork actually does on macOS. There is no public
website, no public download, and no public issue tracker behind them.

Fork context and the current defect list live in
[docs/fork/2026-08-25-mac-only-fork-design.md](../fork/2026-08-25-mac-only-fork-design.md).

## Contents

| Page | What it covers |
| --- | --- |
| [index](index.md) | What PaneFlow is, what it runs on, and where to go next |
| [installation](installation.md) | Build from source, config file location, verify the build |
| [installation/macos](installation/macos.md) | Full macOS prerequisites and the Metal toolchain trap |
| [features](features.md) | Capability tour: panes, workspaces, port detection, projects, diff |
| [keybindings](keybindings.md) | Default shortcuts and how to override them |
| [layouts](layouts.md) | The four one-shot layout presets |
| [themes](themes.md) | Bundled themes and hot-reload behaviour |
| [review](review.md) | The Diff view and agent-driven review |
| [settings](settings.md) | The Settings panel and which keys it writes |
| [configuration](configuration.md) | How `paneflow.json` is read |
| [configuration/schema](configuration/schema.md) | Every recognised config key, type, and default |
| [scripting](scripting.md) | CLI, JSON-RPC, event streams, workspace and flow files, MCP, hooks |
| [scripting/reference](scripting/reference.md) | Exact verbs, methods, fields, and exit codes |
| [conductor](conductor.md) | Driving agent panes from the CLI (see the status note first) |
| [conductor/reference](conductor/reference.md) | Conductor verbs, fields, events, and exit codes |
| [troubleshooting](troubleshooting.md) | Symptom-first fixes for launch, config, shortcuts, themes, PATH |

## Related developer docs

Outside this directory, and not user-facing:

* [../../ARCHITECTURE.md](../../ARCHITECTURE.md)
* [../../CLAUDE.md](../../CLAUDE.md)
* [../hooks.md](../hooks.md)
* [../mcp-bridge.md](../mcp-bridge.md)
* [../debugging-rendering.md](../debugging-rendering.md)
