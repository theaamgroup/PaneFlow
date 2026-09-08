# PaneFlow

**A native macOS workspace for running coding agents side by side.**

Run Claude Code, Codex, Gemini CLI, opencode, or another command-line agent
in terminal panes you can watch and type into. Keep projects organized,
see which agents need your attention, and review their changes without
switching between terminal windows.

This is The AAM Group's macOS-only fork of PaneFlow. It is built with Rust,
Zed's GPUI framework, and Ghostty's terminal engine, with Metal rendering.
Your shells and agent processes run on your Mac; agents use their own
services and accounts.

[Download the latest release](https://github.com/theaamgroup/PaneFlow/releases/latest)
· [Build from source](INSTALL.md)
· [User guide](docs/user/index.md)

## Get started

PaneFlow requires **macOS 13 Ventura or later on Apple Silicon**.

1. Download the signed, notarized DMG from the release link above.
2. Open the DMG, drag **PaneFlow.app** into **Applications**, and launch it.
3. Use your installed coding agents. Install and sign in to each agent you
   want to use before launching it in PaneFlow.

### Your first session

A **workspace** groups a project's tabs. Each **tab** contains one or more
terminal **panes**, where you run a shell, an agent, or a command such as
your test runner.

1. Create a workspace with `Cmd+Shift+N` and navigate to your project in
   its terminal.
2. Start an agent from the terminal, or open the agent launcher with
   `Cmd+Shift+L`.
3. Split the pane with `Cmd+Shift+D` or `Cmd+Shift+E` to run another agent,
   tests, or a development server alongside it.
4. Open the **Attention Queue** with `Cmd+Shift+A` to see agent activity,
   or press `Cmd+Shift+J` to jump to an agent waiting for input.
5. Open **Review** with `Cmd+Shift+G` to inspect changes in your repositories
   and worktrees.

## What PaneFlow helps you do

- **Keep parallel work organized.** Group terminals into workspaces and
  tabs, associate tabs with Git worktrees, and restore your workspace
  layout between launches.
- **See what needs attention.** Agent status indicators, desktop
  notifications, and the Attention Queue help you spot agents that are
  running, waiting, stalled, finished, or reporting an error.
- **Review changes together.** Arrange repository and worktree diffs in a
  grid, switch between unified and split views, and navigate change by
  change. **Review with agent** opens an agent in a workspace tab with a
  prepared prompt; you press Enter to submit it.
- **Share context between agents.** The optional MCP bridge lets agents
  read and search other panes in their workspace. They can also read an
  assigned task and report progress on it.
- **Automate repeatable setups.** Use the CLI to inspect panes, follow
  events, or create a workspace from a TOML file.

## Everyday shortcuts

`Cmd` is Command (⌘); `Option` is Alt (⌥). You can browse and customize
shortcuts in **Settings → Keyboard Shortcuts**.

| Action | Shortcut |
| --- | --- |
| New workspace | `Cmd+Shift+N` |
| Jump to workspace 1–9 | `Cmd+1`–`Cmd+9` |
| Next workspace | `Ctrl+Tab` |
| New tab / close tab | `Cmd+Option+T` / `Cmd+W` |
| Split horizontally / vertically | `Cmd+Shift+D` / `Cmd+Shift+E` |
| Close pane | `Cmd+Shift+W` |
| Move focus between panes | `Option+Arrow` |
| Launch an agent | `Cmd+Shift+L` |
| Open Attention Queue | `Cmd+Shift+A` |
| Jump to next waiting agent | `Cmd+Shift+J` |
| Open Review | `Cmd+Shift+G` |
| Show or hide sidebar | `Cmd+Option+B` |
| Copy / paste | `Cmd+C` / `Cmd+V` |
| Clear scrollback / reset terminal | `Cmd+K` / `Cmd+Shift+R` |
| Increase / decrease font size | `Cmd+=` / `Cmd+-` |

## Use the CLI

The app and CLI share the `paneflow` binary. **Inside a PaneFlow pane,
`paneflow` is already on your PATH.** Run these commands while the app is open:

```bash
paneflow ps                          # List panes and agent state
paneflow read <pane-id> --lines 100    # Read recent terminal output
paneflow watch --type ai.stop         # Stream agent stop events
```

Replace `<pane-id>` with an ID from `paneflow ps`. You can also target panes
by name.

To create a repeatable workspace, define its panes and commands in a TOML
file, then run `paneflow up workspace.toml`. The
[scripting guide](docs/user/scripting.md) includes a sample file and
explains prompt delivery, submission controls, and event streams.

From an external terminal, use the installed binary's full path or
[add it to your PATH](docs/user/installation/macos.md#put-the-cli-on-your-path).
For a local debug build, use `./target/debug/paneflow` from the repository root.

### Let agents read other panes

From a PaneFlow pane, run:

```bash
paneflow mcp install
```

This registers the bundled **Model Context Protocol (MCP)** bridge with
supported agents detected on your machine. It updates each agent's MCP
configuration and backs up the previous configuration.

The bridge lets agents list panes, read output, and search text in their
workspace. It can record progress on the calling pane's assigned task,
but cannot type into terminals. Peer terminal output is marked as
untrusted data.

See the [MCP setup guide](docs/mcp-bridge.md) for supported agents and
installation details, and [agent context](docs/agent-context.md) for task
assignments and progress reports.

## Settings and configuration

Use Settings to customize appearance, terminal behavior, and shortcuts.
The installed app stores its configuration at:

```text
~/Library/Application Support/paneflow/paneflow.json
```

Debug builds use `paneflow-dev` in place of `paneflow`, keeping development
settings separate from the installed app. See the
[configuration reference](docs/user/configuration/schema.md) for available
options and defaults.

## Build from source

You need **Rust 1.98.0**, **full Xcode**, the **Metal Toolchain component**,
and **CMake**. Follow [INSTALL.md](INSTALL.md) to prepare these dependencies;
Xcode Command Line Tools alone are insufficient.

Then, from the repository root:

```bash
cargo run -p paneflow-app            # Build and launch the development app
cargo build --release -p paneflow-app
```

Cargo fetches the pinned GPUI dependencies automatically. The Ghostty
terminal library is vendored, so building PaneFlow does not require Zig.

For development checks and troubleshooting, see [INSTALL.md](INSTALL.md).
For the code structure and runtime design, see [ARCHITECTURE.md](ARCHITECTURE.md).

## Documentation

- [User guide](docs/user/index.md) — features, layouts, themes, and settings
- [Troubleshooting](docs/user/troubleshooting.md) — help with common problems
- [CLI and automation](docs/user/scripting.md) — commands, events, and workspace files
- [MCP bridge](docs/mcp-bridge.md) — connect agents to pane output
- [Agent context](docs/agent-context.md) — task assignments and progress reports
- [Architecture](ARCHITECTURE.md) — how the application is built

## License

PaneFlow is licensed under **GPL-3.0-or-later**. This fork is based on
arthjean/paneflow v0.8.2. See [LICENSE](LICENSE).
