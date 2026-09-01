# PaneFlow

**A native macOS workspace for running coding agents in parallel.**

PaneFlow keeps Claude Code, Codex, Gemini, opencode, and any other CLI
agent in real terminal panes you can see, interrupt, and take over. It
tracks which agent is thinking, waiting, stalled, failed, or done; keeps
each task tied to its workspace and git branch; and gives agents a
read-only way to look at each other's output when you want them to
coordinate instead of work blind.

It is written in Rust, renders through Metal, and stays on your machine.
No Electron, no hosted agent runtime, nothing phoning home.

This is The AAM Group's macOS-only PaneFlow fork. Download the signed,
notarized DMG from the **[latest release](https://github.com/theaamgroup/PaneFlow/releases/latest)**,
or build it from source with **[INSTALL.md](INSTALL.md)**.

## Why it exists

Starting coding agents is easy. Keeping a reliable view of ten running
sessions is the hard part: which one is waiting on you, which branch it
touched, which test output belongs to which task, and how to hand
context from one agent to another without copy-pasting terminal
scrollback.

PaneFlow is built around that coordination problem. The goal is not to
replace your editor or your shell. It is to make parallel agent work
observable enough that you can supervise it without losing context.

## Get started

macOS 13+ on Apple Silicon. Toolchain and first build:
[INSTALL.md](INSTALL.md).

```bash
cargo run -p paneflow-app
```

The CLI is the same binary. Until you put it on your `PATH`, call it
from the tree:

```bash
./target/debug/paneflow ps
```

Config lives at:

```
~/Library/Application Support/paneflow/paneflow.json
```

A `cargo run` debug build uses `paneflow-dev` instead of `paneflow` for
that path, so it will not pick up an installed app's config. Schema:
[docs/user/configuration/schema.md](docs/user/configuration/schema.md).

## What you can do

**Run agents side by side.** Each agent is a real terminal pane, not a
chat-only abstraction. You still see the raw output, and you can type
into it at any time. Workspaces, git branches, titles, and session
restore sit on top of that.

**See what needs attention.** The sidebar, tab dots, desktop
notifications, and the Attention Queue (`Cmd+Shift+A`) turn scattered
agent events into a queue: waiting, running, stalled, errored, or
recently finished. `Cmd+Shift+J` jumps to the next pane whose agent is
waiting on you, across workspaces.

**Drive panes from the CLI.** Once the app is running (same binary as
above; `paneflow` once it is on your `PATH`):

```bash
paneflow ps                                 # list panes and agent state
paneflow read cargo-run --lines 100         # read a pane's scrollback
paneflow send codex-review "Review this"    # prefill a prompt
paneflow watch --type ai.stop               # stream events
paneflow up workspace.toml                  # spawn a declarative workspace
```

By default `send` prefills the prompt and you press Enter. Submitting on
your behalf needs both `--submit` and `PANEFLOW_IPC_SCRIPTING=1`, so a
carriage return can never be sent silently.

Full command list: [docs/user/scripting.md](docs/user/scripting.md).

**Let agents read each other.** `paneflow mcp install` registers a
read-only MCP bridge (`list_panes`, `read_pane`, `search_pane`). It
cannot type into panes. Terminal output handed to an agent is marked
untrusted, so the other agent should analyze it, not obey it. Details:
[docs/mcp-bridge.md](docs/mcp-bridge.md).

**Review worktree diffs in one place.** When each agent works in its own
branch or worktree, `Cmd+Shift+G` opens a side-by-side review: hunk
navigation, unified/split toggle, synchronized scrolling. Review prompts
open a real terminal pane with the prompt pre-filled; they are never
auto-submitted.

## Everyday shortcuts

These are the ones you will actually use in the first hour. Everything
else is in **Settings → Keyboard Shortcuts**.

| Key | What it does |
|---|---|
| `Cmd+Shift+D` / `Cmd+Shift+E` | Split pane (horizontal / vertical) |
| `Cmd+Alt+T` / `Cmd+W` | New tab / close tab |
| `Cmd+Shift+N` | New workspace |
| `Cmd+1`–`Cmd+9` | Jump to workspace |
| `Ctrl+Tab` | Next workspace |
| `Alt+Arrow` | Move focus between panes |
| `Cmd+Shift+A` / `Cmd+Shift+J` | Attention Queue / next waiting agent |
| `Cmd+Shift+G` | Review diffs |
| `Cmd+Shift+L` | Launch an agent |
| `Cmd+Alt+B` | Show / hide the sidebar (remembered next launch) |
| `Cmd+C` / `Cmd+V` | Copy / paste in a terminal |
| `Cmd+K` / `Cmd+Shift+R` | Clear scrollback / reset a terminal (`Cmd+Shift+K` also clears) |
| `Cmd+=` / `Cmd+-` | Font size |

Next-workspace is `Ctrl+Tab`, not `Cmd+Tab`: macOS owns `Cmd+Tab` for
the app switcher and never delivers it to the app.

## More

- [INSTALL.md](INSTALL.md) — build from source
- [docs/user/index.md](docs/user/index.md) — using the app
- [ARCHITECTURE.md](ARCHITECTURE.md) — how it is put together
- [docs/user/scripting.md](docs/user/scripting.md) — CLI and automation
- [docs/mcp-bridge.md](docs/mcp-bridge.md) — read-only MCP bridge

## License

GPL-3.0-or-later fork of arthjean/paneflow v0.8.2. See [LICENSE](LICENSE).
