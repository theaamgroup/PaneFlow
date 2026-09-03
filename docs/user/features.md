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

A tab bound to a Git worktree (see [Git worktrees](#git-worktrees))
remembers that binding too: `session.json` carries the checkout path per
tab, so the tab comes back on its branch. A session written by an older
PaneFlow restores with every tab unbound, and a tab whose checkout was
removed between two runs restores unbound with a warning in the log
rather than pointing at a missing directory.

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
room comes back; dragging its edge while it is squeezed never narrows that
choice unless you drag it narrower than the room allows. When there is no room
for a readable dock beside a pane at all, the panes win and the dock steps
aside until there is.

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

Each tab can stand on a checkout of its own. The **New pane** palette
(`Cmd+Alt+T`, or the folder row's `+`) and a tab's right-click menu list
the repository's local branches: pick one that already has a worktree
and the tab reuses it; pick one that has none and PaneFlow checks it out
under `<workspace>.worktrees/<branch>` next to the repository, then
starts the pane there. Picking the branch the repository itself is on
unbinds the tab. The sidebar shows a bound tab's branch under its title,
and the diff dock and Review scope follow the active tab's checkout.

The binding decides where the *next* pane lands: a split or a new pane
in a bound tab always starts inside that worktree, and a running pane is
never moved. When an agent creates a branch and `cd`s into its worktree
from inside a pane, only that pane's tab takes on the new checkout - the
sibling tabs keep the branch they were on.

A checkout made from the palette or the tab menu is yours: closing the
workspace never removes it (`git worktree list` still shows it), unlike
the managed worktrees `paneflow up` or the Launch Pad create, which are
torn down with their workspace when clean. Remove it with
`git worktree remove` when you are done.

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

When one agent's window closes on you - a Claude usage cap, say - the
work does not have to stop with it. Right-click a row in the Agent
sessions sidebar for **Resume**, **Copy summary**, or **Continue in ▸**,
which lists every other enabled agent. Picking one opens a new tab in
that session's directory, starts the agent, and prefills a short handoff
block in its prompt (never submitted - press Enter when you are ready):

```
Continue this work from a prior Claude Code session.
Session: <id>
Cwd: <directory>
Branch: <branch, when known>
Summary:
<the session's title or first message, capped at 4 KiB>
```

Only the summary travels - never the transcript - and PaneFlow never
starts an agent to write one.

## System Info

**Help ▸ System Info…** opens a small dialog with the environment block a
bug report needs - PaneFlow version, install format, macOS version, chip,
GPU and renderer, and the terminal engine's version - and a **Copy**
button that puts it on the clipboard. It carries no project path and no
environment variables, so it is safe to paste as-is. See
[Troubleshooting](troubleshooting.md#what-should-i-capture-for-a-bug)
for what else to include.

## Keep exploring

* [Keybindings](keybindings.md) - the default shortcuts and how to remap any action.
* [Configuration](configuration.md) - where `paneflow.json` lives and what every key does.
* [Troubleshooting](troubleshooting.md) - the Metal toolchain, Gatekeeper, PATH, config paths, and themes.
