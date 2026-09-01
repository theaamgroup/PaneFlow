# Features

PaneFlow is a native, GPU-accelerated terminal workspace built for agentic
CLI workflows. This page walks through what it does, capability by
capability. Start with [index.md](index.md) for the overview.

## Split panes and layouts

Split the focused pane horizontally (`Cmd+Shift+D`) or vertically
(`Cmd+Shift+E`); each new pane spawns a fresh shell in the same
working directory. Move focus structurally across the tree with
`Alt+Arrow`. Reshape the whole workspace in one keystroke with the four
[built-in presets](layouts.md) - even horizontal, even vertical,
main vertical, and tiled.

## Agent-first panes

Launch Claude Code, Codex, OpenCode, or any CLI agent as a first-class
pane with a branch-aware workspace badge. The "AI agent" pane is a UI
affordance, not a dependency: PaneFlow runs whatever is on your `PATH`,
with no login, no API key, and no model picker inside the app itself.

## Workspaces

PaneFlow's mental model is two layers - **workspaces** (independent
project contexts) and **panes** (terminal splits inside a workspace).
Add a workspace with `Cmd+Shift+N`, jump between them with
`Cmd+1-9`, or cycle with `Ctrl+Tab`. Each workspace is named after the directory it opened
from, so the window always tells you where you are.

## Persistent sessions

Workspaces and their pane trees persist across launches, so reopening
PaneFlow drops you back into the layout you left - splits, working
directories, and running shells intact. Pick up a long-running agent
thread or a dev server exactly where you stopped.

## Dev-server port detection

When a process inside a pane binds a port, PaneFlow surfaces it
automatically: no manual wiring, no external tools. A burst of
terminal activity schedules a scan (debounced 500 ms, then re-run at
+2 s and +6 s to catch slow-binding servers) that enumerates per-PID
socket file descriptors through `libproc` and keeps the TCP listeners,
cross-referenced with the pane's process tree. Each port is attributed
to the pane whose shell owns it.

In parallel, terminal output is matched against 23 startup-banner
patterns covering 21 frameworks. Eight get a clickable URL in the
sidebar: Next.js (including Turbopack), Vite, Nuxt, Remix, Astro,
Webpack, Angular, and React (`react-scripts`). Backend frameworks from
Express, Flask, and Django to Axum and Spring get a labeled port. The
OS-side scan is authoritative: a banner match for a port the socket
scan did not classify is downgraded to a plain labeled port.

## Headless scripting (CLI, MCP and JSON-RPC)

Drive PaneFlow from outside the GUI: the `paneflow` binary is also a
CLI over a local JSON-RPC IPC socket. Scripts can list panes, read and
search scrollback, inspect agent state, stream lifecycle events,
split, focus, stage prompts behind an explicit write gate, spawn
declarative workspaces (`paneflow up`), and run multi-agent pipelines
(`paneflow flow`). A read-only MCP bridge exposes pane reads to
agents, so an assistant can inspect another pane without copy-paste.
The guide and command reference live on the [scripting and automation page](scripting.md).

## Custom action buttons

Pin a frequent command to a one-click button at the top of the window.
Give it a name, a shell command - say `clear && cargo run` for a dev
server - and an icon, then click it to run that command in the focused
pane. Stop retyping your build, test, or lint invocation on every loop.

## File tree sidebar

Open a sidebar on the current workspace to browse the full file tree of
your codebase. Click any file - a markdown spec or a PRD included - and it
opens as source in the dock editor next to your Claude Code session, so the
document stays in view while the agent works. The sidebar is per tab: opening
it in one tab leaves your other tabs as they were, and it steps aside while
you are in Review or Settings. Copy any file's absolute or relative path in
one click, ready to paste into a prompt or a command.

The dock itself is per tab too. Two tabs of the same folder each get their
own dock - open a shell or a diff in one and the other stays as it was - and
switching tabs brings each tab's dock back the way you left it. Closing a tab
closes its dock; the dock never follows a tab into a session restore. When a
right-hand rail is open or the window is narrow, the dock shrinks to fit the
space the panes can spare and returns to the width you chose as soon as the
room comes back.

## Projects

Point PaneFlow at a codebase folder and it becomes a project: the file
tree opens in the sidebar, the active Git branch shows in the header,
and new panes are scoped to that directory. It's the working context
every session, split, and action runs inside.

## Multiple projects in one window

Open several codebases as tabs in the same window and switch between
them without juggling OS windows. Each project keeps its own panes,
branch, and file tree, so a Claude Code session in one repo stays put
while you jump to another.

## Git worktrees

Create a Git worktree to run an experiment or an independent task
without touching your main checkout. PaneFlow opens the worktree as its
own context - isolated branch, isolated files - so you can run parallel
work side by side and throw it away cleanly when you're done.

## Agent chat

Open a dedicated chat that launches Claude Code, Codex, or any agent CLI
from your home directory instead of a project. It's a
ChatGPT- or Claude-style conversation surface that runs in the terminal
with the full power of a coding agent behind it - for research,
planning, or quick questions that don't belong to a specific codebase.
A sidebar keeps your past chats one click away.

## Project threads, any agent

Inside a project, run multiple threads - independent agent sessions -
and choose which CLI drives each one: Claude Code, Codex, OpenCode, Pi,
Hermes, Openclaw, Factory's Droid, or anything else on your `PATH`. Mix
them per thread, so one thread can plan with Codex while another
implements with Claude Code, side by side in the same project.

## Keep exploring

* [Keybindings](keybindings.md) - the default shortcuts and how to remap any action.
* [Configuration](configuration.md) - where `paneflow.json` lives and what every key does.
* [Troubleshooting](troubleshooting.md) - the Metal toolchain, Gatekeeper, PATH, config paths, and themes.
