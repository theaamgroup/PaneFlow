# PaneFlow

**A native macOS workspace for running coding agents in parallel.**

PaneFlow keeps Claude Code, Codex, Gemini, opencode, and any other CLI agent in
real terminal panes you can see, interrupt, and take over. It tracks which agent
is thinking, waiting, stalled, failed, or done; keeps each task tied to its
workspace and git branch; and gives agents a read-only control plane when you
want them to coordinate instead of work blind.

It is written in Rust on [Zed's GPUI](https://github.com/zed-industries/zed/tree/main/crates/gpui),
uses `alacritty_terminal` for VT emulation, and renders through Metal. No
Electron, no hosted agent runtime, nothing phoning home.

> **This repository is a private, macOS-only fork owned by The AAM Group.** It is
> never published publicly, and it has no release channel, installer, or Homebrew
> tap. You build it from source. See
> [docs/fork/2026-08-25-mac-only-fork-design.md](docs/fork/2026-08-25-mac-only-fork-design.md)
> for exactly how this fork diverges from upstream and what is still in flight.

## Requirements

- macOS 13 Ventura or later, Apple Silicon. The 13.0 floor is enforced by
  `LSMinimumSystemVersion` in [assets/Info.plist](assets/Info.plist) and comes
  from GPUI's Metal requirements.
- A working build toolchain. There is no prebuilt binary, so the prerequisites
  below are not optional.

## Build from source

### Prerequisites

Four things, and two of them are easy to get wrong.

**1. Rust 1.96.1.** Pinned by [rust-toolchain.toml](rust-toolchain.toml), so
rustup selects it automatically inside the repo. Verify:

```bash
rustup show active-toolchain
# 1.96.1-aarch64-apple-darwin (overridden by '.../paneflow/rust-toolchain.toml')
```

**2. Full Xcode. Command Line Tools are NOT sufficient.** GPUI compiles Metal
shaders during `cargo build`, so the build needs the Metal shader compiler, which
only ships with Xcode. Point the active developer directory at it:

```bash
sudo xcode-select -s /Applications/Xcode.app/Contents/Developer
xcode-select -p    # must print a path inside Xcode.app, not /Library/Developer/CommandLineTools
```

**3. The Metal Toolchain component. Xcode alone is still not sufficient.**
Xcode 26 ships the Metal toolchain as a separately downloadable component, so a
fresh Xcode install still fails with `cannot execute tool 'metal' due to missing
Metal Toolchain`. Download it once:

```bash
xcodebuild -downloadComponent MetalToolchain
```

**Do not use `xcrun -f metal` as a readiness check.** It resolves and prints a
path even when the toolchain is missing, so it reports success on a machine that
cannot build. Compile something instead:

```bash
printf 'kernel void probe() {}\n' > /tmp/probe.metal
xcrun metal -c /tmp/probe.metal -o /tmp/probe.air && echo "Metal toolchain OK"
```

**4. cmake**, via Homebrew:

```bash
brew install cmake
```

### Build and run

```bash
cargo build --release -p paneflow-app
cargo run -p paneflow-app
```

GPUI and the other Zed crates are git dependencies. Cargo fetches them
automatically; no local Zed checkout is needed.

### Checks

```bash
cargo fmt --check
cargo clippy --workspace -- -D warnings
cargo test --workspace
```

`cargo fmt --check` is mandatory before every commit. CI runs it as an early
build step and a single mis-formatted line fails the whole run before anything
useful happens. See [CLAUDE.md](CLAUDE.md) for the full rationale and the rest of
the repo's working rules.

## Why it exists

Starting coding agents is easy. Keeping a reliable view of ten running sessions
is the hard part: which one is waiting on you, which branch it touched, which
test output belongs to which task, and how to hand context from one agent to
another without copy-pasting terminal scrollback.

PaneFlow is built around that coordination problem:

- Real terminal panes for every agent, so nothing is hidden behind a chat-only
  abstraction.
- Live agent state from hooks and IPC, not a vague "terminal is active"
  heuristic.
- Workspaces and git branches visible in the app chrome.
- A review surface for comparing worktree diffs side by side.
- A read-only MCP bridge so one agent can inspect another pane's output.
- A local CLI and JSON-RPC control plane for scripted orchestration.

The goal is not to replace your editor or your shell. It is to make parallel
agent work observable enough that you can supervise it without losing context.

## Core workflows

### Run agents side by side

Launch any CLI agent in a real PTY pane. PaneFlow keeps the raw terminal visible
while adding the app-level state you need for multi-agent work: workspace,
branch, title, status, notifications, and session restore.

### See what needs attention

The sidebar, tab dots, desktop notifications, and the Attention Queue
(`Cmd+Shift+K`) turn scattered agent events into a readable queue: waiting for
input, running, stalled, errored, or recently finished. `Cmd+Shift+J` jumps
straight to the next pane whose agent is waiting on you, across workspaces.

### Drive panes from the CLI

The `paneflow` CLI talks to the same local socket as the running app:

```bash
paneflow ps                                              # list panes and agent state
paneflow read cargo-run --lines 100                      # read a pane's scrollback
paneflow search <target> <pattern>                       # grep a pane
paneflow watch --type ai.stop                            # stream events
paneflow send codex-review "Review this branch"          # prefill a prompt
paneflow wait --match claude-impl --pattern "REPORT_DONE" # block on output
paneflow up                                              # spawn a declarative workspace
paneflow flow run examples/review-pipeline.flow.toml      # run a flow DAG
```

Also available: `ls`, `status`, `new`, `select`, `split`, `focus`, `key`, and the
`paneflow hooks` intercept that agent hooks call to report state.
`paneflow flow run` executes a `flow.toml` DAG with spawn, wait, send, capture,
and review steps.

By default `send` prefills the prompt and a human presses Enter. Submitting on
your behalf needs both an explicit `--submit` and the instance-side scripting
gate (`PANEFLOW_IPC_SCRIPTING=1`), so a carriage return can never be sent
silently.

**Known defect in this fork:** the bundled conductor skill, which drives a fleet
of agents through these panes, is unreliable in practice. It is kept so it can be
fixed here rather than deleted. The pattern that does work today is one headless
agent process per task, each in its own git worktree. Treat the pane-driving
model itself as suspect, not just its implementation.

### Let agents read each other safely

`paneflow mcp install` registers a local read-only MCP bridge for supported CLI
agents (`uninstall` and `status` are also available). It exposes three tools:

- `list_panes`
- `read_pane`
- `search_pane`

It also serves read-only pane resources (`pane://surface/{id}/content`). The
bridge cannot type into panes or control them. Returned terminal output is
wrapped as untrusted data so downstream agents know to analyze it, not obey it.
Tool manifests live in [mcps/paneflow/tools](mcps/paneflow/tools); behavior and
per-agent config are in [docs/mcp-bridge.md](docs/mcp-bridge.md).

### Review worktree diffs in one place

When each agent works in its own branch or worktree, PaneFlow can show the
resulting diffs side by side: one column per worktree, with hunk navigation
(`[` and `]`), unified/split toggle (`u`), synchronized scrolling (`s`),
attribution, and a local cost estimate where token usage is available (unpriced
models show tokens and no cost). There are two surfaces: a docked diff panel and
a full-screen review view (`Cmd+Shift+G`).

Branch review prompts spawn a real terminal pane in that branch's worktree with
the prompt pre-filled. They are never auto-submitted.

## Feature map

| Area | What PaneFlow gives you |
|---|---|
| Terminal workspace | Splits, tabs, resize, zoom, layout presets, session restore, markdown panes |
| Agent state | Thinking, waiting, finished, errored, stalled, desktop notifications, Attention Queue |
| Review | Worktree diff columns, hunk navigation, unified/split toggle, sync scroll, attribution, local cost estimate, review prompts |
| Automation | CLI, JSON-RPC socket, `paneflow up`, `paneflow flow run`, event stream |
| Agent context | Read-only MCP bridge with `list_panes`, `read_pane`, `search_pane` |
| Native runtime | Rust, GPUI, `alacritty_terminal` VT emulation, Metal rendering |
| Editor handoff | Open the active workspace in Zed, Cursor, VS Code, or Windsurf |
| Theming | 5 bundled themes with hot reload: One Dark (default), PaneFlow Light, Vercel, Claude, Cursor |

## Keybindings

Defaults are registered in
[src-app/src/keybindings/defaults.rs](src-app/src/keybindings/defaults.rs) and
can be overridden through the `shortcuts` object in `paneflow.json`. Bindings use
GPUI's `secondary` modifier, which resolves to `Cmd` on macOS.

| Key | Action | Context |
|---|---|---|
| `Cmd+Shift+D` / `Cmd+Shift+E` | Split horizontal / vertical | Global |
| `Cmd+Shift+W` | Close pane | Global |
| `Cmd+Shift+T` | Undo close pane | Global |
| `Cmd+Shift+Z` | Toggle zoom | Global |
| `Alt+Arrow` | Focus navigation | Global |
| `Cmd+Alt+T` / `Cmd+W` | New tab / close tab | Global |
| `Cmd+Shift+N` / `Cmd+Shift+Q` | New / close workspace | Global |
| `Cmd+Tab` / `Cmd+1` to `Cmd+9` | Next / select workspace | Global |
| `Cmd+Alt+1` to `Cmd+Alt+4` | Layout presets: even-h, even-v, main-vertical, tiled | Global |
| `Cmd+Shift+=` / `Cmd+Shift+S` | Equalize splits / swap pane | Global |
| `Cmd+Shift+A` / `Cmd+Shift+G` | Agents view / review view | Global |
| `Cmd+Alt+F` | Toggle files sidebar | Global |
| `Cmd+Shift+K` / `Cmd+Shift+J` | Attention Queue / jump to next waiting agent | Global |
| `Cmd+Shift+L` / `Cmd+Shift+Space` | Launch Pad / composer | Global |
| `Cmd+Shift+B` / `Cmd+Shift+M` | Toggle broadcast member / broadcast groups | Global |
| `Cmd+C` / `Cmd+V` | Copy / paste (also `Ctrl+Shift+C` / `Ctrl+Shift+V`) | Terminal |
| `Ctrl+Shift+F` / `Ctrl+Shift+X` | Find in buffer / copy mode | Terminal |
| `Shift+PageUp` / `Shift+PageDown` | Scroll | Terminal |
| `Cmd+Shift+Up` / `Cmd+Shift+Down` | Jump to previous / next shell prompt | Terminal |
| `Cmd+=` / `Cmd+-` / `Cmd+0` | Font size up / down / reset | Terminal |
| `Cmd+Q` | Quit | Global |

`Cmd+Tab` for next workspace collides with the macOS app switcher in practice,
so rebind it if you use it. Editor handoff sits on `Ctrl+Alt+Z` (Zed),
`Ctrl+Alt+C` (Cursor), `Ctrl+Alt+V` (VS Code), and `Ctrl+Alt+W` (Windsurf). The
full action list is in `src-app/src/app/actions.rs`; the override syntax is in
[docs/user/configuration/schema.md](docs/user/configuration/schema.md).

## Configuration

```
~/Library/Application Support/paneflow/paneflow.json
```

The file is watched and re-read on change, and themes hot-reload. An unrecognized
value falls back with a logged warning rather than discarding the whole config.
The full schema is in
[docs/user/configuration/schema.md](docs/user/configuration/schema.md).

Other paths worth knowing:

| What | Path |
|---|---|
| Restored session | `~/Library/Caches/paneflow/session.json` (`session-dev.json` in debug builds) |
| App data, including the extracted MCP bridge binary | `~/Library/Application Support/paneflow/` |
| JSON-RPC control socket | `$TMPDIR/paneflow/paneflow.sock` on a normal Mac, overridable with `PANEFLOW_SOCKET_PATH` |

## Safety model

PaneFlow is local-first by design.

- Agents run as normal CLI processes inside normal PTYs.
- The UI is a supervisor surface, not a hosted agent runtime.
- Prompt prefill is the default. Auto-submit needs an explicit `--submit` plus
  the `PANEFLOW_IPC_SCRIPTING=1` scripting gate, and the `ai_unrestricted` config
  bypass defaults to off.
- IPC writes are gated behind that same explicit scripting access.
- `paneflow key` refuses to send submitting keystrokes (`enter`, `ctrl-m`,
  `ctrl-j`) outright.
- MCP tools are read-only.
- Terminal output returned to agents is marked as untrusted.

## Docs

- [docs/fork/2026-08-25-mac-only-fork-design.md](docs/fork/2026-08-25-mac-only-fork-design.md)
  how this fork diverges from upstream, what was deleted, and the traps found on
  the way
- [CLAUDE.md](CLAUDE.md)
  build and test commands, annotated module tree, thread model, and the hard-won
  GPUI gotchas
- [ARCHITECTURE.md](ARCHITECTURE.md) - runtime architecture and thread model
- [AGENTS.md](AGENTS.md) - repository instructions for coding agents
- [docs/mcp-bridge.md](docs/mcp-bridge.md) - MCP bridge behavior and per-agent install
- [docs/hooks.md](docs/hooks.md) - agent hook wiring behind the live status
- [docs/user/configuration/schema.md](docs/user/configuration/schema.md) - full `paneflow.json` schema
- [docs/user/scripting/reference.md](docs/user/scripting/reference.md) - CLI and JSON-RPC reference
- [docs/debugging-rendering.md](docs/debugging-rendering.md) - rendering and
  latency debugging
- [docs/memory-smoke-test.md](docs/memory-smoke-test.md) - memory smoke-test procedure
- [docs/release/macos-signing.md](docs/release/macos-signing.md) - signing and notarization

## Attribution and license

This is a GPL-3.0-or-later fork of arthjean/paneflow v0.8.2.

[GPL-3.0-or-later](LICENSE)
