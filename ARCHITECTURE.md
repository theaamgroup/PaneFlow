# Paneflow Architecture

Paneflow is a native GPU-accelerated terminal workspace for running CLI coding
agents in parallel. One user-facing Rust binary, no web runtime: the UI is
built on a pinned Paneflow branch of
[Zed's GPUI](https://github.com/zed-industries/zed/tree/main/crates/gpui) and
terminal emulation is provided by upstream
[`alacritty_terminal`](https://crates.io/crates/alacritty_terminal) from
crates.io. Paneflow owns PTY lifecycle orchestration, rendering, and
integration with agent tracking, IPC, and the MCP bridge.

This fork is **macOS only**. Metal, AppKit, Unix-socket IPC, a signed and
notarized `.app` bundle. Fork decisions, the upstream leak register, and a
traps register live in
[`docs/fork/2026-08-25-mac-only-fork-design.md`](docs/fork/2026-08-25-mac-only-fork-design.md).
Read it before touching platform code.

This document describes how the pieces fit together. It is aimed at
contributors and at anyone curious how you build a multiplexing terminal app
without Electron.

## Workspace layout

The repo is a Cargo workspace with one binary crate and a set of small,
focused library crates:

| Crate | Path | Purpose |
|---|---|---|
| `paneflow-app` | `src-app/` | The GPUI application and `paneflow` CLI entrypoint: UI, panes, PTY sessions, IPC server |
| `paneflow-config` | `crates/paneflow-config/` | Config schema, tolerant JSON loader, file watcher |
| `paneflow-shim` | `crates/paneflow-shim/` | PATH shim wrapping 16 known agent CLIs so Paneflow can observe their lifecycle |
| `paneflow-ai-hook` | `crates/paneflow-ai-hook/` | The hook binary agent CLIs invoke to report session events back over IPC |
| `paneflow-ipc-client` | `crates/paneflow-ipc-client/` | Blocking JSON-RPC client for the local IPC socket (shared by the MCP bridge and the CLI) |
| `paneflow-mcp` | `crates/paneflow-mcp/` | Stdio MCP server exposing read-only pane access (`list_panes`, `read_pane`, `search_pane`) |
| `paneflow-mcp-install` | `crates/paneflow-mcp-install/` | GPU-free install engine for the MCP bridge: per-agent detection, idempotent config merge, backup + atomic write |
| `paneflow-process` | `crates/paneflow-process/` | Bounded external-process execution (wall-clock deadline + stdout cap) shared across crates |
| `paneflow-acp` | `crates/paneflow-acp/` | Legacy Claude/Codex identity enum plus the `CLAUDECODE` environment scrub |

`src-app` is the default workspace member, so bare `cargo run` starts the
desktop app instead of becoming ambiguous across helper binaries. The split is
deliberate: anything that runs *outside* the GUI process (shim, hook, MCP
bridge, MCP installer logic) must stay GPU-free and tiny, so it lives in its
own crate and never links GPUI.

## Thread model

```
┌─────────────────────────────────────────────────────────┐
│ Main thread - GPUI event loop                           │
│   owns all Entity state, rendering, input dispatch      │
└─────────────────────────────────────────────────────────┘
        ▲                    ▲                    ▲
        │ Backend events     │ mpsc (50ms poll)   │ channel
┌───────┴────────┐  ┌────────┴───────┐  ┌─────────┴────────┐
│ Terminal       │  │ IPC thread     │  │ Watcher threads  │
│ workers        │  │ JSON-RPC 2.0   │  │ config, theme,   │
│ (Alacritty)    │  │ socket server  │  │ git state        │
└────────────────┘  └────────────────┘  └──────────────────┘
```

- **Main thread**: the GPUI event loop. All UI state lives in `Entity<T>`
  values mutated through GPUI contexts; there are no locks around UI state.
- **Terminal workers**: one per session. Each Alacritty session owns an
  `alacritty_terminal::EventLoop` I/O thread and a shared terminal grid
  (`Arc<FairMutex<Term<ZedListener>>>`, the only cross-thread data in the app).
  Sessions publish backend-neutral events to the view and owned render
  snapshots through `TerminalSessionBackend`.
- **IPC thread**: accepts connections on a Unix socket. Stateless methods
  reply in place; stateful methods are dispatched to the main thread through a
  bounded channel and drained by the 50 ms app poll loop
  (`PaneFlowApp::process_automation_tick`).
- Blocking work (git subprocesses, filesystem walks, fleet-wide search) is
  pushed to background executors. Registering a recursive file watcher or
  scanning a monorepo on the render thread is how you get a "not responding"
  window, so the codebase treats the main thread as render-only.

## Keystroke → pixel

The full input/output pipeline, end to end:

```
KeyDownEvent
  → TerminalView::handle_key_down()
  → keys::to_esc_str() escape-sequence translation
  → PTY writer → Notifier → shell / agent CLI
  → output bytes → alacritty_terminal VTE parser → Term grid mutations
  → ZedListener::send_event(Wakeup) → TerminalBackendEvent
  → 4 ms coalescing batch → sync() → cx.notify()
  → TerminalSessionBackend::render_content() → owned neutral Content
  → TerminalElement::prepaint()
  → TerminalElement::paint()     - quads + shaped glyph runs
  → GPU (Metal)
```

Wakeups are coalesced into a 4 ms event batch (`terminal/view.rs`) so a chatty
process cannot drive one repaint per byte.

`TerminalElement` (`src-app/src/terminal/element/`) is the one place Paneflow
implements GPUI's low-level `Element` trait directly instead of composing
divs: terminal rendering wants per-cell control over background quads, glyph
runs, cursor shapes, underlines and hyperlink hitboxes. Everything else in the
app (sidebar, tabs, settings, diff viewer) is regular GPUI flex layout.

Debug builds can trace the whole pipeline: `PANEFLOW_LATENCY_PROBE=1` stamps a
keystroke at ingress and reports time-to-pixel.

## The terminal engine boundary

`TerminalSessionBackend` is the renderer-facing facade for the terminal
engine. There is exactly one implementation: upstream `alacritty_terminal`.
The facade is still worth keeping, because it is what stops borrowed terminal
state from leaking into GPUI. The rest of the app consumes Paneflow-owned
points, mode flags, cells, events, and `Content` snapshots rather than reaching
into the `Term` grid, so the render path never holds the terminal lock across a
frame.

Alacritty imports are confined to an explicit allowlist enforced by a test,
`alacritty_confined_to_backend_allowlist` in
`src-app/src/terminal/types.rs`, which keeps the VT crate from spreading
beyond the `terminal/` module (plus `search.rs`). Separately,
`src-app/tests/dependency_source_policy.rs` asserts every git source in
`Cargo.lock` is pinned to an immutable revision.

Upstream shipped a second engine (a statically linked `libghostty-vt` backend)
that was the default only on Linux and Windows x64 MSVC. macOS always used
`alacritty_terminal`, no macOS code path could reach the Ghostty backend, and
the backend is removed from this fork. The `terminal.backend` config key and
its `Ghostty` variant may survive in the JSON schema as an accepted no-op;
treat any request for it as "use Alacritty".

## Agent lifecycle tracking

The feature that makes Paneflow more than a tiling terminal: it knows what
the agents inside its panes are doing.

```
agent CLI (claude, codex, opencode, …)
  └─ launched through a PATH shim (paneflow-shim)
       └─ agent hooks fire paneflow-ai-hook on lifecycle events
            └─ ai.* JSON-RPC notifications over the local socket
                 └─ GUI: tab dots, sidebar spinners, attention queue,
                    desktop notifications carrying the actual question
```

- **Shim**: launching an agent from Paneflow puts a shim directory first in
  `PATH`. The shim records the real PID and process start time (PID-reuse
  safe), then execs the real binary. Sixteen agent CLIs are recognized by
  name; unknown tools are reported as themselves.
- **Hooks**: agents that support lifecycle hooks (Claude Code, Codex, …)
  report `session_start`, `prompt_submit`, `tool_use`, `notification`, `stop`,
  `exit`, and `session_end` through the `ai.*` IPC namespace. Agents without
  hooks fall back to process-tree and terminal-activity detection.
- **States**: thinking, waiting for input (with the actual prompt text),
  finished, errored (non-zero exit), stalled (no hook activity past a
  threshold). Each state routes to the UI, and to your own tooling, since
  the same events are observable over IPC.

The default loop is human-in-the-loop: Paneflow pre-fills prompts into real PTY
sessions and the user submits them. Auto-submit exists only as an explicit,
gated scripting path.

## IPC and the MCP bridge

A JSON-RPC 2.0 endpoint on a Unix socket exposes `system.*`, `workspace.*`,
`surface.*`, `fleet.*`, `events.*`, and `ai.*` namespaces: enough to script
workspace creation, read panes, send text behind the scripting gate, and
subscribe to agent events. The `paneflow` CLI (`paneflow up`, `paneflow flow`,
`paneflow watch`, `paneflow wait`) is built on the same socket. The socket path
is resolved by `src-app/src/runtime_paths.rs`, which on macOS lands under
`$TMPDIR` and enforces the 104-byte `sun_path` ceiling.

The MCP bridge re-exposes a read-only slice of this to agents themselves:
`paneflow mcp install` registers a stdio MCP server with Claude Code, Codex,
Gemini CLI and opencode, giving any agent the ability to *read* (never write)
other panes' scrollback. An agent debugging a failing dev server can read the
server pane's output directly instead of asking you to paste it. The bridge
binary ships embedded in the main binary and is extracted to a stable path at
launch, so there is nothing extra to install.

Ingress is treated as untrusted: session and config files are validated
structurally (layout budgets, ratio clamps, id alphabets) before they touch
app state.

## Platform surface

One target: macOS on Apple Silicon.

| Concern | Implementation |
|---|---|
| GPU | Metal (GPUI compiles its Metal shaders at build time, so a full Metal toolchain is a build prerequisite) |
| Windowing | AppKit, with client-side decorations by default |
| Terminal engine | upstream `alacritty_terminal` from crates.io |
| PTY | `alacritty_terminal::tty` |
| IPC | Unix socket under `$TMPDIR` |
| Font enumeration | Core Text (`src-app/src/fonts.rs`) |
| Config | `~/Library/Application Support/paneflow/paneflow.json` |
| Packaging | signed + notarized `.dmg` |

`#[cfg(unix)]` is still load-bearing and appears throughout: macOS needs
nearly all of it. Do not confuse unix-shared code with Linux-only code when
pruning.

## Performance discipline

Perf claims in release notes are backed by reproducible procedures, not
vibes: heaptrack-style allocation diffs for memory work, `cargo flamegraph`
for CPU work, criterion benchmarks for hot paths, and a keystroke-latency
probe in debug builds. The render thread never does blocking I/O; scans and
searches that touch the filesystem or many panes run on background executors
and report back through events.

## Building

From-source setup, the Xcode/Metal traps, and debug vs release paths are in
[`INSTALL.md`](INSTALL.md). The short version:

```bash
cargo build --release    # LTO thin, strip, codegen-units=1
cargo test --workspace
cargo clippy --workspace -- -D warnings
cargo fmt --check
```
