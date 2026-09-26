# Paneflow Architecture

Paneflow is a native GPU-accelerated terminal workspace for running CLI coding
agents in parallel. One user-facing Rust binary, no web runtime: the UI is
built on a pinned revision of
[Zed's GPUI](https://github.com/zed-industries/zed/tree/main/crates/gpui) and
terminal emulation is provided by a vendored
[`libghostty-vt`](https://github.com/ghostty-org/ghostty) static archive
(Ghostty `f2d5758f`, wrapped by the `paneflow-terminal-ghostty` crate; issue
#184). Paneflow owns the PTY through `portable-pty`, rendering, and
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

```
PaneFlowApp (Entity<Render>)           ← src-app/src/main.rs
├── app/                               ← PaneFlowApp impl, split across modules
│   ├── actions.rs                     ← 76 GPUI action types (paneflow namespace)
│   ├── bootstrap.rs                   ← app init, window creation, GPUI setup, poll loops
│   ├── event_handlers.rs              ← title-bar/pane/terminal event subscribers + stale-PID sweep
│   ├── ipc_handler.rs                 ← JSON-RPC handler + process_automation_tick (50 ms)
│   ├── session.rs                     ← persist/restore workspaces to session.json
│   ├── settings.rs                    ← settings lifecycle: open/close, persist_setting, key handlers
│   ├── review/                        ← Review mode: Workspaces rail (220 px), Changes rail (300 px),
│   │                                     independent single-subject diff panes in LayoutTree (MAX_REVIEW_PANES = 6);
│   │                                     mode.rs gates entry, grid.rs handles opening/split/move/zoom,
│   │                                     session.rs persists subjects + geometry + collapsed repository groups
│   ├── diff_sidebar/                  ← Review Changes rail (git file list, not an in-app file tree)
│   ├── sidebar/ sidebar_actions_menu.rs ← sidebar list + context menus (`context_menu.rs`; Remove worktree row, #348;
│   │                                     tab Mark as read clears waiting/errored/stalled session badges, #408;
│   │                                     workspace Mark as read clears completions, Mute/Unmute notifications persists, #493),
│   │                                     Customize Sidebar menu (`customize_menu.rs`: `sidebar_show` toggles,
│   │                                     Expand all / Collapse all, #349); footer mode tabs
│                                         + IPC banner (no Settings affordance at all)
│   ├── agent_status.rs                ← hookless agent state: pane OSC observations + Claude session-registry sweep
│   ├── pane_overview/                 ← Cmd+Shift+P expose: every terminal pane across every
│                                         workspace in a compact grid; tabs stay adjacent with
│                                         split-pane labels and eight-row previews (rows.rs: packing/navigation)
│   ├── system_info_dialog.rs          ← Help ▸ System Info… modal + Copy button (report from system_info.rs)
│   ├── command_palette.rs             ← Cmd+Shift+O palette: every context-free registry action with its live
│   │                                     binding, whole-word filter, Enter dispatches, never lists itself (#523)
│   ├── tab_worktree.rs                ← per-tab worktree binding (#347): cached checkout git state, branch/worktree
│   │                                     listings, bind_tab_to_branch (prepare_branch_checkout off-thread)
│   └── workspace_ops/                 ← create/close/select/rename/reveal, focus, layout, swap, tab
├── cli/                               ← CLI commands over the IPC socket
├── window_chrome/
│   ├── shell.rs                       ← native macOS window content shell
│   ├── macos_backdrop.rs              ← native material behind sidebar/title bar
│   └── title_bar.rs                   ← window controls, drag-to-move
├── workspace/                         ← Vec<Workspace> state
│   ├── mod.rs                         ← Workspace struct, AI agent PIDs, MAX_WORKSPACES = 32
│   ├── git.rs / worktree.rs           ← branch detection for badges, worktree support
│   ├── pid_resolve.rs                 ← PID-reuse-safe process identity
│   ├── ports.rs                       ← TCP port scan (macOS libproc)
│   └── surface_naming.rs              ← auto-naming panes from their process
├── layout/                            ← N-ary tree of panes (replaced the old binary SplitNode)
│   ├── tree.rs                        ← LayoutTree::{Leaf, Container}, DragState, size consts
│   ├── mutations.rs / navigation.rs / close.rs
│   ├── presets.rs                     ← from_panes_equal, main_vertical, tiled
│   ├── render.rs                      ← GPUI flex emission + divider hitboxes
│   └── queries.rs / serde.rs          ← MAX_PANES = 32 lives in layout/mod.rs
├── pane.rs / pane_drag.rs             ← Pane: tab strip + active terminal; drag-to-split
├── terminal/                          ← PTY session + VT emulation + rendering
│   ├── view.rs                        ← TerminalView (Entity<Render>), 4 ms wakeup coalescing
│   ├── ghostty_session.rs             ← GhosttySession: runtime thread owns DisplayTerminal + PTY, publishes snapshots
│   ├── pty_session.rs                 ← TerminalState: GPUI-facing host, env, pinned Drop ladder, scrollback
│   ├── clipboard_gate.rs / input.rs   ← OSC 52 policy gate, key/mouse encoding through libghostty
│   ├── kitty.rs                       ← Kitty graphics placements (PNG decode, 32 MiB/pane cap)
│   ├── search.rs / marks.rs           ← find-in-buffer, shell-integration prompt marks
│   ├── service_detector.rs / shell.rs ← dev-server detection, shell resolution
│   ├── blink.rs / types.rs            ← cursor blink, shared terminal types
│   ├── bench_corpus.rs / perf_bench.rs ← deterministic VT corpus, terminal bench (#[ignore], scripts/bench-terminal.sh)
│   ├── ghostty_stress.rs / test_allocator.rs ← runtime stress (#[ignore]); the test binary's one #[global_allocator]
│   └── element/                       ← low-level GPUI Element rendering
│       ├── mod.rs                     ← TerminalElement: layout → prepaint → paint
│       ├── color.rs                   ← ANSI→Hsla, APCA contrast
│       ├── font.rs / geometry.rs      ← font resolution + cell geometry
│       ├── hyperlink.rs               ← OSC 8 + URL scanning
│       ├── sprites.rs                 ← glyphs the renderer draws itself: box drawing, shades, braille, Powerline
│       ├── paint/                     ← background, text, cursor, selection, scrollbar, sprites
│       ├── thumbnail.rs               ← read-only cropped pane preview; NEVER routes through
│                                          TerminalElement (its build_layout resizes the PTY)
│       └── golden/ pixel_probe.rs     ← golden-image + pixel assertions
├── theme/                             ← theme model + hot-reload (8 bundled variants)
│   ├── model.rs                       ← TerminalTheme (26 Hsla slots + ui + syntax), UiColors
│   ├── builtin.rs                     ← THEMES table + theme_by_name
│   └── watcher.rs                     ← 500 ms mtime cache + notify events, active_theme()
├── keybindings/
│   ├── defaults.rs                    ← DEFAULTS + MACOS_ONLY_DEFAULTS tables
│   ├── apply.rs                       ← apply_keybindings() wires cx.bind_keys
│   └── registry.rs / display.rs       ← action registry, human-readable binding strings
├── settings/                          ← embedded Codex-style settings (inline, not a window)
│   ├── chrome.rs                      ← grouped nav rail + content panel (impl PaneFlowApp)
│   ├── components.rs / nav_header.rs  ← shared cards/toggles/section headers
│   └── tabs/                          ← general, appearance, shortcuts, terminal, ai_agent, mcp,
│                                        workspaces. shortcuts is the one virtualized tab
│                                        (gpui::list, owns its scroll): ~80 rows × ~8 nodes
│                                        rebuilt every frame made the whole settings surface lag
├── diff/                              ← git diff engine + single-ReviewSubject viewer (custom Element, own hscroll);
│                                         per-pane base + unified/split display, no embedded terminals or scope/sync layer
├── text_sanitize.rs                   ← strip bidi and zero-width characters from untrusted labels;
│                                         normalize a prompt for the session-handoff PTY prefill
├── agents/                            ← agent process supervision, notifications
├── ai_hooks/                          ← ai.* hook payload extraction
├── {claude,codex,opencode,pi,command}_sessions.rs ← per-agent session-file readers
├── agent_launcher.rs / agent_sessions.rs ← spawn agents through the PATH shim
├── widgets/                           ← text_input, scrollbar, callout
├── fonts.rs                           ← load_mono_fonts (Core Text on macOS)
├── ai_types.rs                        ← AiToolState, AgentStateSource ranking, lifecycle reducer
├── claude_session_registry.rs         ← reads Claude Code's sessions/<pid>.json (state without hooks)
├── ipc.rs                            ← JSON-RPC server over `interprocess`
├── keys.rs                            ← key translation (mouse encoding lives in terminal/input.rs)
├── search.rs                          ← find-in-buffer UI glue
├── limits.rs                          ← centralized ingress/egress size caps
├── release_notes.rs                   ← `last-launched-version` cache marker (hand-parsed x.y.z); first launch of a newer version raises the sticky release-notes toast (#526)
├── runtime_paths.rs                   ← runtime/data/config path helpers + sun_path guard
├── login_shell_env.rs                 ← adopt the login shell's PATH (GUI launch has none)
├── config_writer.rs                   ← read-modify-write paneflow.json
├── window_state.rs / editor.rs / external_open.rs
├── sidebar_title.rs                   ← sidebar label cleanup
├── startup_trace.rs / startup_bench.rs ← PANEFLOW_STARTUP_TRACE probe (marks in main / mount / new / render, writes
│                                         JSON at the measured frame, quits); cfg(test) first-frame bench (#519)
├── system_info.rs                     ← Help ▸ System Info… collection: sysctl, Metal devices, install format, libghostty identity
├── bench_harness.rs                   ← cfg(test): Metric, measure, comparison table, publish(), libproc counters shared by the terminal and startup benches
└── assets.rs                          ← rust-embed asset registry (fonts, icons)
```

### Workspace crates


| Crate | Path | Type | Purpose |
|-------|------|------|---------|
| `paneflow-app` | `src-app/` | Binary | GPUI application: all UI, PTY, IPC, CLI |
| `paneflow-config` | `crates/paneflow-config/` | Library | Config schema, JSON loader, file watcher |
| `paneflow-ipc-client` | `crates/paneflow-ipc-client/` | Library | Blocking JSON-RPC client for the local socket |
| `paneflow-mcp` | `crates/paneflow-mcp/` | Binary | Read-only stdio MCP server (see below) |
| `paneflow-mcp-install` | `crates/paneflow-mcp-install/` | Library | GPU-free per-agent MCP config merge engine |
| `paneflow-shim` | `crates/paneflow-shim/` | Binary | PATH shim wrapping 18 agent CLIs |
| `paneflow-ai-hook` | `crates/paneflow-ai-hook/` | Binary | Hook binary agents invoke to report lifecycle events |
| `paneflow-process` | `crates/paneflow-process/` | Library | Bounded subprocess execution (deadline + stdout cap) |
| `paneflow-agent-config` | `crates/paneflow-agent-config/` | Library | Shared agent config, hooks, locking, Claude hook shapes |
| `paneflow-libghostty-sys` | `crates/paneflow-libghostty-sys/` | Library | Raw libghostty-vt FFI; `build.rs` verifies and links `native/libghostty/prebuilt/aarch64-apple-darwin` (no Zig) |
| `paneflow-terminal-ghostty` | `crates/paneflow-terminal-ghostty/` | Library | Safe `DisplayTerminal` wrapper over the FFI |
| `paneflow-ghostty-smoke` | `crates/paneflow-ghostty-smoke/` | Binary | Headless PTY smoke against the linked archive |

There is **no** `paneflow-telemetry` crate, and the lockfile contains no
Zed `markdown`, `telemetry`, or `telemetry_events` packages. GPUI is the
remaining Zed dependency; do not restore the removed Markdown-widget graph.

Everything that runs outside the GUI process must stay GPU-free and never link GPUI.

`clippy.toml` is load-bearing: it allows unwrap and expect in tests while
keeping the workspace lint policy strict in production code.

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
│ (libghostty)   │  │ socket server  │  │ git state        │
└────────────────┘  └────────────────┘  └──────────────────┘
```

- **Main thread**: GPUI event loop, owns all Entity state, rendering, input dispatch. No locks around UI state.
- **Ghostty runtime thread** (`paneflow-ghostty-runtime`, one per terminal): owns the `!Send` `DisplayTerminal`, the PTY master and the child; drains the mailbox (input, resize, selection, shutdown) and publishes `Content` snapshots through a `PublishGate` (#343, upstream 799ab51d + 8d8a9d88 + e03972c5). The gate holds a frame for two reasons: DEC 2026 synchronized output is set (one FFI mode query per wake, `DisplayTerminal::synchronized_output`), or the previous publish is newer than **8 ms** (`MIN_PUBLISH_INTERVAL`; deferred to the interval's end through `next_wake`, never dropped). A 2026 hold expires after **150 ms** (`SYNC_OUTPUT_MAX_HOLD`) so a program that opens a frame and dies cannot freeze the pane. Resize, scroll, scrollback clear, reset, select-all, a command mark, the first frame of a session, and the frame preceding `ChildExited` bypass the gate (`publish_now`). A `Wakeup` is queued only for a frame that was actually published. Publishing converts only the rows the engine flagged (`ghostty::Content::dirty_rows` → `CellMirror`, two alternating `Arc<[Cell]>` buffers, full conversion when the render thread still holds the back buffer). The loop blocks 10 ms (`RUNTIME_IDLE_TICK`) only while output flows, a drag is held, or a child is winding down, and **100 ms** (`RUNTIME_QUIET_TICK`) once a pane has been silent for a second; the display-only runtime blocks for a second. A `Shutdown` message wakes the mailbox at once, so neither tick delays the close guard, and the gate touches nothing but the grid: it never signals or reaps (see the close-guard trap above). A sibling **PTY reader thread** (`paneflow-ghostty-pty-reader`) feeds it 32 KiB chunks through a 4-buffer pool. An OSC 8 hover lookup is a `HyperlinkHover` message answered by `GhosttyUiEvent::HyperlinkResolved`, never a blocking round trip from the UI thread.
- **IPC thread**: Unix-socket server; stateless methods reply in place and stateful requests reach the main thread through a bounded channel drained every 50 ms. `runtime_paths.rs` uses an existing UTF-8 `$TMPDIR`, then `dirs::cache_dir()/run`; it ignores XDG runtime paths. `PANEFLOW_SOCKET_PATH` is an absolute-path override. Release sockets use `paneflow/paneflow.sock`, debug sockets `paneflow-dev/paneflow-dev.sock`; reject paths reaching the 104-byte macOS limit. Keep the IPC-client resolver in lockstep.
- **Watcher threads**: config (notify, 300 ms debounce, 1 s max-wait ceiling), theme, git state.
- **Shared state**: `parking_lot::RwLock<SharedState>` (`Content` cells + modes + metrics + kitty placements) written by the runtime thread and read by the GPUI thread; `UiEventState` slots carry title/cwd/progress/notification/clipboard events. The libghostty C handle never leaves the runtime thread.

Blocking git, filesystem walks, recursive watcher registration, and fleet-wide search run off the render thread.

## Opening a file

There is no in-app file tree, no in-app editor, and no in-app Markdown
viewer. A clicked file path, including `.md`, opens in the configured
external editor (`editor::open_at_location`). The right rail is the Sessions
sidebar only. Git diff viewing lives in Review mode and Work Review; there is
no diff dock. Tree-sitter still highlights Markdown in those diffs.

## Diff syntax highlighting

Review diffs use `diff/highlighter.rs`: grammar selection and capture
resolution use one path.
Fifteen Zed highlighting queries are compiled with `include_str!` from
`diff/queries/` (issue #433). TOML, HTML, Java and Ruby keep their grammar's
stock queries. JavaScript uses the JavaScript query on the existing TSX
grammar. Markdown runs block and inline passes; fenced code does not inject
another language.

Captures are ordered by byte start, preserving query order at equal starts.
Each styled capture goes onto a stack. The last active capture paints until
its end or the next capture, including when it is wider than an earlier one.
Captures without a palette role do not enter the stack. Each row caps input
at 4,096 captures. Theme changes rebuild color tables without querying or
reparsing the retained trees. Variables and namespaces use the text
color; constructors share the function role. `diff/parity_tests.rs` holds the
independent byte oracle, the token expectations per language and the frozen
`fixtures/stock-priority-audit.txt` of every byte the stock-to-Zed switch
recolored on the corpus.

`diff/queries/MANIFEST.toml` records the upstream commit, source paths, SHA-256
hashes, license evidence and the JavaScript grammar deviation; a unit test
verifies every hash against the vendored bytes. `NOTICE` attributes Zed
Industries and ships in the bundle as `ThirdPartyLicenses/zed-queries.txt`.
Git preserves LF bytes for these imports.

Set `ZED_DIR` to a local Zed checkout, then run
`scripts/sync-zed-queries.sh --check`. Check mode compares every query and the
manifest byte for byte with the pinned Git objects, checks the provenance
notice and lists any drift. Omit the check option to restore those bytes,
including the notice's revision. To resync, pass `--commit <revision>`, then
review the upstream license evidence, compile the queries and run the parity
tests. The script resolves revisions to an immutable full SHA. No source
checkout files or runtime configuration are read by the app.

## Keystroke → pixel

```
KeyDownEvent → TerminalView::handle_key_down() → input::ghostty_key_input()
→ write_ghostty_key() → RuntimeMessage::KeyInput → runtime thread → DisplayTerminal::encode_key() → PTY write
→ shell output → pty-reader thread → RuntimeMessage::Output → DisplayTerminal::feed()
→ PublishGate::request(): held while DEC 2026 is set (150 ms max) or the last frame is < 8 ms old
   (deferred via next_wake, never dropped); resize/scroll/first frame/pre-ChildExited bypass it
→ commit(): snapshot (dirty rows only, CellMirror) → RwLock<SharedState> with a new Content::generation;
   queue_wakeup → GhosttyUiEvent::Wakeup (only on a real publish)
→ 4ms coalescing batch (terminal/view.rs; a timer for event bursts, not the frame gate)
→ process_backend_wakeup() → dirty=true → cx.notify()
→ TerminalElement::prepaint() → session_backend().render_content() (RwLock read + Arc<[Cell]> clone)
→ build_layout(): LayoutCacheKey (Content::generation + theme generation + bounds/font/cursor/focus/
   search/exit inputs) hit → Arc<LayoutState> clone; miss → layout_from_snapshot()
→ TerminalElement::paint() → paint_quad + shape_line (+ kitty placements) → Metal
```

Debug builds can trace ingress-to-paint latency with `PANEFLOW_LATENCY_PROBE=1`. Terminal and diff elements implement GPUI’s low-level `Element` contract; ordinary chrome uses flex layout.

## The terminal engine boundary

`TerminalSessionBackend` is the renderer-facing facade for the terminal
engine. There is exactly one implementation: the vendored `libghostty-vt`
archive, wrapped by `paneflow-terminal-ghostty` and hosted by
`src-app/src/terminal/ghostty_session.rs`. The facade is what stops borrowed
terminal state from leaking into GPUI: the rest of the app consumes
Paneflow-owned points, mode flags, cells, events, and `Content` snapshots
rather than reaching into the engine, so the render path never blocks on the
runtime thread.

No engine type crosses the `terminal/` seam. `src-app/src/terminal/types.rs` is
the contract; `ghostty_session.rs` translates libghostty values into it.
`alacritty_is_absent_from_the_app_crate` (`terminal/types.rs`) fails if
`alacritty` reappears under `src-app/src/`. Separately,
`src-app/tests/dependency_source_policy.rs` asserts every git source in
`Cargo.lock` is pinned to an immutable revision, and the libghostty build
script hash-verifies the archive, the bindings, the third-party notice, and
the SBOM against `native/libghostty/manifest.toml`.

Stage 2a (2026-08-25) deleted a Ghostty backend that no macOS code path could
reach at the time. Upstream v0.10.0 made macOS a Ghostty target, and issue
#184 (2026-08-31) brought libghostty-vt back as the sole engine, deleting
`alacritty_terminal` and the `terminal.backend` selector. A leftover
`"backend"` key in an old config is ignored.

## Agent lifecycle tracking

The feature that makes Paneflow more than a tiling terminal: it knows what
the agents inside its panes are doing.

```
agent CLI (claude, codex, opencode, …)
  └─ launched through a PATH shim (paneflow-shim)
       └─ agent hooks fire paneflow-ai-hook on lifecycle events
            └─ ai.* JSON-RPC notifications over the local socket
                 └─ GUI: tab dots, sidebar spinners, waiting-agent navigation,
                    desktop notifications carrying the actual question
```

- **Shim**: launching an agent from Paneflow puts a shim directory first in
  `PATH`. The shim records the real PID and process start time (PID-reuse
  safe), then execs the real binary. Eighteen agent CLIs are recognized by
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
`surface.*`, `fleet.*`, and `ai.*` namespaces: enough to script
workspace creation, read panes, and send text behind the scripting gate.
The `paneflow` CLI uses the same socket. The socket path
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
| Windowing | AppKit, with native window decorations |
| Terminal engine | vendored `libghostty-vt` (`native/libghostty`, Ghostty `f2d5758f`) via `paneflow-terminal-ghostty` |
| PTY | `portable-pty`, owned by Paneflow; libghostty only parses |
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

```bash
# Build
cargo build
cargo build --release          # LTO thin, strip, codegen-units=1

# Run
cargo run                      # debug build (src-app is the default workspace member)
RUST_LOG=info cargo run        # with logging (env_logger)
PANEFLOW_LATENCY_PROBE=1 cargo run  # keystroke→pixel latency tracing (debug only)

# Test
cargo test --workspace         # all workspace tests
cargo test -p paneflow-config  # config crate tests only
cargo test -p paneflow-app --test flex_nchild -- --nocapture  # GPUI layout integration tests
cargo test <test_name> -- --nocapture  # single test with output

# Lint
cargo clippy --workspace -- -D warnings
cargo fmt --check

# Benchmark (release profile; see bench/README.md)
scripts/bench-terminal.sh                # terminal pipeline benchmark: writes bench/results/<stamp>-<sha>.json,
                                         # prints a Markdown comparison against bench/baseline.json when it exists
scripts/bench-terminal.sh --set-baseline # same run, then make it the baseline
scripts/bench-startup.sh                 # time to first frame: builds the release binary, launches it against two
                                         # seeded PANEFLOW_HOME fixtures, writes bench/results/startup-<stamp>-<sha>.json,
                                         # compares against bench/startup-baseline.json
scripts/bench-startup.sh --set-baseline  # same run, then make it the baseline; refused when the core-share probe < 0.90
```

Performance claims about the terminal pipeline or startup
need evidence from those suites: the ignored `terminal_pipeline_benchmark` in
`src-app/src/terminal/perf_bench.rs` measures the terminal GPU-free under
the release profile through `src-app/src/bench_harness.rs` and
prints the comparison table `bench/README.md` documents; the ignored
`startup_bench::startup_first_frame_benchmark` (#519) launches the release
binary with `PANEFLOW_STARTUP_TRACE=<file>` set, which makes
`src-app/src/startup_trace.rs` record a mark per launch stage, write the
timeline once the measured frame is presented (the first frame, or the frame
after the last #156 restore batch when a session was restored), and quit.
Do not ship a perf number you did not measure, and do not publish a run that
printed `PANEFLOW_BENCH_WARNING` (another workload was competing);
`--set-baseline` refuses such a run for the startup suite.

`PANEFLOW_HOME=<absolute dir>` (#519, `paneflow_config::loader::HOME_ENV`)
relocates every per-user directory the app owns: the config root becomes
`<home>/config`, the data root `<home>/data`, and the cache root
`<home>/cache`, each still joined with `APP_SUBDIR`, so a release binary
reads `<home>/config/paneflow/paneflow.json` and `session.json` beside it.
`runtime_paths::{config_dir, cache_dir, data_dir}` are the app-side
resolvers; every `dirs::config_dir()` / `dirs::cache_dir()` site for
app-owned state goes through them. The IPC socket does not follow it:
`PANEFLOW_SOCKET_PATH` keeps its own precedence, and the startup bench sets
both.

Every pane's `PANEFLOW_BIN_DIR` (`~/Library/Caches/paneflow/bin/<version>/`)
holds the 18 agent shims, `paneflow-ai-hook`, and a `paneflow` symlink to the
running executable (`ai_hooks/extract.rs::link_cli_into`, #440), so `paneflow
send` / `paneflow mcp install` work inside a pane without the user linking the
bundle binary onto their login PATH. The link is re-pointed at launch when
`current_exe()` moves.

Debug builds namespace themselves as `paneflow-dev` (`runtime_paths.rs`):
config, data, cache, and the default IPC socket (`paneflow-dev.sock`). A
`cargo run` debug instance should not share those with
`/Applications/PaneFlow.app`. A **release-profile** local binary
(`cargo run --release`, `./target/release/paneflow`) uses the real
`paneflow` namespace and **will** collide with the installed app's
socket. (Issue #39 is **fixed**: `window_state.rs` resolves
`window-state.json` under the same `APP_SUBDIR` as `paneflow.json`, so a
debug build writes it to `paneflow-dev`. A regression test in that module pins it.)

If the singleton guard refuses to start, the installed app is holding
`paneflow.sock`. Override with both:

```bash
PANEFLOW_ALLOW_MULTIPLE=1 PANEFLOW_SOCKET_PATH=/tmp/paneflow-head-smoke.sock cargo run -p paneflow-app
```

`PANEFLOW_ALLOW_MULTIPLE` is **value-gated**: `allow_multiple_from`
(`src-app/src/ipc.rs`) is `matches!(value, Some("1"))`, so only `=1`
skips the guard and `=0` correctly keeps it. Issue #53 reported the
opposite (presence-gating) and was fixed in `1cfee6c7`; do not
re-transcribe the bug title as behaviour. `open -a PaneFlow` drops shell
env; use `open --env VAR=1`.

### Fork-pin maintenance (GPUI)

The Zed git deps in `src-app/Cargo.toml` pin `zed-industries/zed@fecc3273ed32643c2ea1b04a74c8780e2c9ffaf8` (`gpui` and `gpui_platform` in `[dependencies]` at lines 33-34, plus a test-support `gpui` in `[dev-dependencies]` at line 215). `gpui_platform` must carry the `font-kit` feature on macOS. To bump: choose and freeze a tested upstream revision, update every exact `rev`, run `cargo update`, then run the workspace test, Clippy, and format gates. Do not reintroduce an `arthjean/zed` pin.

## Dependency sources

GPUI and `gpui_platform` are **git dependencies** pinned to `zed-industries/zed`:

```toml
gpui = { git = "https://github.com/zed-industries/zed", rev = "fecc3273ed32643c2ea1b04a74c8780e2c9ffaf8" }
gpui_platform = { git = "...", rev = "fecc3273...", features = ["font-kit"] }   # font-kit is mandatory on macOS
```

Cargo fetches GPUI from git automatically. **There is no local checkout and no path dependency.** Two crates-io patches are required by GPUI:
- `async-task` → `smol-rs/async-task` (specific git commit)
- `calloop` → `zed-industries/calloop` fork

Terminal emulation is `paneflow-terminal-ghostty` (workspace crate, `src-app/Cargo.toml`), the safe wrapper over `paneflow-libghostty-sys`, whose build script links `native/libghostty/prebuilt/aarch64-apple-darwin/lib/libghostty-vt.a` after verifying every hash in `native/libghostty/manifest.toml`. `portable-pty = "0.9"` opens the PTY and spawns the child; `image` (PNG only) decodes Kitty graphics. `cargo deny` cannot see the static archive: `native/libghostty/THIRD_PARTY_NOTICES.md` is its license inventory and ships in the bundle as `ThirdPartyLicenses/libghostty.txt`.


## Styling conventions

**`DESIGN.md` at the repo root is the design contract** and outranks this
section on anything visual: the tokens, the geometry and radius tables, the
motion rules, the per-component contracts, the accessibility floors, and the
delivery gate a UI change has to clear. Read it before touching chrome, and
update it in the same PR as any visual change. The notes below are the
engineering summary, not the contract.

- **All styling is inline** via GPUI's Tailwind-like builder API: `.bg(rgb(0x181825)).px_3().rounded_md()`
- **Sidebar/titlebar colors are hardcoded** dark hex values unless the active theme supplies a `UiColors` block. Legacy themes derive chrome colors from light/dark defaults; the bundled custom themes opt into exact UI tokens so the theme affects the whole app, not just ANSI colors.
- **Terminal colors** come from `TerminalTheme` (26 `Hsla` slots plus optional `ui: UiColors` and a `syntax: SyntaxPalette`, `theme/model.rs`) resolved via `active_theme()`. `selection_foreground` is computed at theme-load time so `apca_contrast(selection_foreground, selection) >= 45.0` holds at every observation point; if you construct a theme by hand, call `recompute_selection_foreground()`.
- **Font**: defaults to the embedded `JetBrainsMono Nerd Font` at **13.0 pt** (`terminal/element/font.rs`, the related definition), range clamped to 8.0-32.0. The regular (non-Mono) Nerd Font variant is bundled since #420 (upstream `73e51a01`): its icons keep their designed size and the renderer constrains them instead (Ghostty's `Glyph.zig` `fit_cover1` / `center1`, `paint/text.rs::constrain_icon`): a Private Use Area glyph is laid out as a `SymbolGlyph { span }`, scaled to cover one cell, or left at its designed size over two cells when the cell after it is empty, it does not follow another icon, and it is not the last column; ink bounds come from the embedded face's `glyf` table (`face_tables.rs::embedded_glyph_ink`). The legacy names `JetBrainsMono Nerd Font Mono` and `JetBrainsMono NFM` (and `JetBrainsMono NF`) keep resolving to the bundled family with no warning. The cell grid is measured on the face (#418, upstream `7706b771`): `terminal/element/face_tables.rs` reads the embedded faces' `hhea` / `post` / `OS/2` tables through `ttf-parser`, `font.rs::cell_metrics_from_face` ports Ghostty's `Metrics.calc` (widest ASCII advance by the face's own line height, each rounded to whole **device** pixels, baseline on a pixel row), and `CellGeometry` carries the resulting `CellMetrics` so every paint pass computes edges as `floor(origin) + col * cell`. `line_height` / `cell_width` are multipliers of that measured cell and default to **`1.0`** (ranges 0.8-2.5 / 0.8-2.0); at 13 pt JetBrains Mono the cell is 10x23 px. Underlines and strikethroughs (`paint/decorations.rs`: single, double, dotted, dashed, curly, all from the font tables) and the bar / underline / hollow cursors (`paint/cursor.rs`, `CellMetrics::cursor_thickness`) are sized from those metrics too. The Pane Overview thumbnail measures the same way without a `Window` (`font.rs::cell_metrics_without_window`). Embedded families are always resolvable because `Assets::load_fonts` registers them with GPUI at boot. A configured `font_family` that is not an installed monospace family (checked against Core Text via `fonts.rs::load_mono_fonts`) logs a warning and falls back to the default.
