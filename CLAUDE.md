# CLAUDE.md - PaneFlow

Native Rust terminal workspace for running coding agents in parallel. Built with Zed's GPUI framework; VT emulation is Ghostty's `libghostty-vt`, statically linked from a vendored archive (`native/libghostty/`), with PaneFlow owning the PTY through `portable-pty`. **This fork is macOS only.**

Fork context, decisions, the upstream leak register, and the traps register live in `docs/fork/2026-08-25-mac-only-fork-design.md`. **Read it before touching platform code.** It records which `#[cfg]` sites are load-bearing on macOS, which look like cruft and are not, and which upstream endpoints still point at the original author's repo.

**Work tracking:** GitHub issues are the backlog. File an issue for every bug
and every feature (`gh issue create`); remaining work is `gh issue list`.
Markdown is for documentation, design, runbooks, and fixtures
(`examples/TASK.md` is load-bearing). Do not add TODO.md, ISSUES.md,
ROADMAP.md, FIXES.md, a live findings.md queue, or any other markdown list of
open work, and do not grow `docs/fork/STATE.md` into a backlog.

**Start here for method and verification:** `docs/fork/STATE.md` records what
has landed, the verification commands and their expected output, and the
method rules this project has already paid for. Read it before planning a
pass so you do not redo finished work or repeat a falsified finding. Open
work lives on GitHub issues, not in that file.

**Where this fork stands (2026-09-04):** product is PaneFlow (the PanesCLI
rename was dropped). Version **0.3.1**. Origin `theaamgroup/paneflow` on
`main`. Upstream v0.11.0 is adopted (#341: the `PublishGate` with DEC 2026
synchronized output, per-tab worktree binding with a Remove worktree row,
the Customize Sidebar menu, the `gh` pull-request marker); the verified SKIP
list (Windows shell, `timeBeginPeriod`, verbatim prefix, libghostty CI
automation, Fedora/Discord/CHANGELOG/AppStream) stays not-ported. Windows, Linux, the telemetry crate, the published
`windows_*_material` schema, and community files (`SECURITY.md`,
`CONTRIBUTING.md`) are gone. The Ghostty engine, deleted on 2026-08-25, is
back as the **only** engine since #184 Phase 2 (2026-08-31): Alacritty is gone,
every pane runs on `libghostty-vt`, and `TERM_PROGRAM` is `ghostty`. The old hand-rolled updater remains **deleted**;
Sparkle 2 performs silent hourly checks and installs verified updates only when
the user quits. First signed GitHub Release is **v0.1.0** (Developer ID
signed, notarized, stapled; #11 closed with `spctl` evidence). #13 closed on
2026-08-28 after an installed v0.1.1 signed/notarized live-app and notification
hook smoke. #10 was closed by rebinding next-workspace to `Ctrl+Tab`; #14 and
#15 were closed on 2026-08-27 by IPC-verified smokes.

## Verify before claiming

Run all six, before and after any pass, and quote the actual output:

```bash
cargo build                                # exit 0
cargo test --workspace                     # diff test names against the last landing; do not trust the integer
cargo clippy --workspace --all-targets     # exit 0, WARNING COUNT 1 (block v0.1.6)
cargo fmt --check                          # exit 0
./target/debug/paneflow --version          # paneflow 0.3.1
cargo deny check advisories licenses sources   # exit 0; same gate run_tests.yml::security_audit blocks on
```

`cargo deny` needs a one-time `cargo install cargo-deny --locked --version '^0.19'`
and network access (it fetches the RustSec DB), and it can go red with **no code
change** (a RustSec DB older than `maximum-db-staleness`, or a `deny.toml`
`ignore` entry that no longer matches any crate), which is why it belongs in the
local set and not only in CI.

If the test count moves, **diff test names** against the last landing, never trust the integer:

```bash
grep -oE '^test [a-zA-Z0-9_:]+ \.\.\.' <log> | sed 's/^test //; s/ \.\.\.$//' | sort
```

A green `cargo build` is **not** a green tree: this repo has already had a change
that built clean and failed `cargo test`, because a `#[cfg(test)]` block did
`include_str!` on a deleted file. Never pipe a command whose exit status matters
(`cargo test | tail` reports `tail`'s status), and redirect as `cmd > file 2>&1`,
never `cmd 2>&1 > file`. Clippy exits 0 with warnings, so compare the warning
count to the one known `block v0.1.6` notice. `set -e` plus `grep FAILED` on a
green log is a **false fail** (grep exits 1 when it finds nothing). `rg … | head`
is a **false fail** via SIGPIPE after a successful command.

## Delegating parallel work

Do **not** use the `paneflow-conductor` skill to grind this repo. Headless
grok in git worktrees is the mechanism that worked. (Conducting *can* drive
a live PaneFlow window when `PANEFLOW_IPC_ORCHESTRATION=1` and
`PANEFLOW_IPC_SCRIPTING=1` are set, but `paneflow read` still returns 0
lines; that path is for a human-supervised interactive agent, not batch
fan-out. See `docs/mcp-bridge.md` and the `grok-subagents` skill.)

Fan-out works when the worker does **not** have to discover anything.
Give exact `file:line` + the cfg/expression as written + the action.
Disjoint file allowlists; two batches on the same file collide at
`git apply` even when the hunks are six lines apart. Kickoff task lists
are not automatically file-disjoint: check overlap before launching.

Mechanics that made 2c and the post-2c grind cheap:

- One `git worktree` per batch, seeded with `cp -c -R target <wt>/target`
  (APFS clone: seconds, ~0 extra bytes, warm incremental rebuild).
- Three concurrent batches was the working cap on this machine.
- Agents never touch git. Orchestrator collects `git -C <wt> diff`,
  applies it, re-runs the five gates, and commits. Agent "green" is
  never the evidence.
- Call `"$HOME/.grok/bin/grok"`, never bare `grok` (that is this app's
  PATH shim). `--worktree` is ignored under `-p`; create the worktree
  yourself and pass `--cwd`.
- `--json-schema` is for **bounded** site lists. On an open-ended audit
  it can suppress the tool loop (one-turn empty report). Omit it when
  the worker has to search.
- Two `cargo` processes on one `target/` fight the lock; kill the extra
  one rather than waiting.

## Build prerequisites (macOS)

Verified on 2026-08-25. Two of these are non-obvious and each one fails the build in a confusing way.

1. **Rust 1.98.0**, pinned by `rust-toolchain.toml`. rustup honors the pin automatically. The dependency graph's actual floor is 1.92 (oo7 0.6, cosmic-text 0.17, smol_str 0.3, several wgpu crates), so anything older fails to build before tests can start.
2. **Full Xcode. Command Line Tools are NOT sufficient.** GPUI compiles its Metal shaders at build time, which needs the Metal compiler that ships only with Xcode.
3. **Xcode alone is still not sufficient.** Xcode 26 ships the Metal toolchain as a separate downloadable component, so `xcrun metal` fails with `cannot execute tool 'metal' due to missing Metal Toolchain` until you run:

   ```bash
   xcodebuild -downloadComponent MetalToolchain
   ```

   **Do not check this with `xcrun -f metal`.** That resolves the tool's path successfully even when the toolchain is absent, so it reports success on a machine that cannot build. Verify with an actual compile, or with `xcrun metal --version`.
4. `cmake` (Homebrew) for native dependencies.

`gpui_platform` **must** carry the `font-kit` feature on macOS (`src-app/Cargo.toml:40`). Without it the build succeeds, the window opens, SVG icons and cursor quads paint, and every single text glyph rasterizes as an empty box. It is declared in the default dependency table, so this only bites if someone edits the feature list.

## Commands

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
```

Performance claims about the terminal pipeline need evidence from that
suite: the ignored `terminal_pipeline_benchmark` in
`src-app/src/terminal/perf_bench.rs` measures it GPU-free under the release
profile and prints the comparison table `bench/README.md` documents. Do not
ship a perf number you did not measure, and do not publish a run that
printed `PANEFLOW_BENCH_WARNING` (another workload was competing).

Debug builds namespace themselves as `paneflow-dev` (`runtime_paths.rs`):
config, data, cache, and the default IPC socket (`paneflow-dev.sock`). A
`cargo run` debug instance should not share those with
`/Applications/PaneFlow.app`. A **release-profile** local binary
(`cargo run --release`, `./target/release/paneflow`) uses the real
`paneflow` namespace and **will** collide with the installed app's
socket. (Issue #39 is **fixed**: `window_state.rs:157-161` resolves
`window-state.json` under the same `APP_SUBDIR` as `paneflow.json`, so a
debug build writes it to `paneflow-dev`. A regression test at `:195`
pins that.)

If the singleton guard refuses to start, the installed app is holding
`paneflow.sock`. Override with both:

```bash
PANEFLOW_ALLOW_MULTIPLE=1 PANEFLOW_SOCKET_PATH=/tmp/paneflow-head-smoke.sock cargo run -p paneflow-app
```

`PANEFLOW_ALLOW_MULTIPLE` is **value-gated**: `allow_multiple_from`
(`src-app/src/ipc.rs:274`) is `matches!(value, Some("1"))`, so only `=1`
skips the guard and `=0` correctly keeps it. Issue #53 reported the
opposite (presence-gating) and was fixed in `1cfee6c7`; do not
re-transcribe the bug title as behaviour. `open -a PaneFlow` drops shell
env; use `open --env VAR=1`.

### Fork-pin maintenance (Zed Markdown widget)

The Zed git deps in `src-app/Cargo.toml` pin `zed-industries/zed@fecc3273ed32643c2ea1b04a74c8780e2c9ffaf8` (`gpui` and `gpui_platform` in `[dependencies]` at lines 39-40, plus a test-support `gpui` in `[dev-dependencies]` at line 253). `gpui_platform` must carry the `font-kit` feature on macOS. To bump: choose and freeze a tested upstream revision, update every exact `rev`, run `cargo update`, then run the workspace test, Clippy, and format gates. Do not reintroduce an `arthjean/zed` pin.

## Pre-commit checks (mandatory)

**Before EVERY `git commit` and EVERY `git push` that touches Rust code, run:**

```bash
cargo fmt --check
```

If it reports any diff, run `cargo fmt`, re-stage the touched files, then commit.

Why this is non-negotiable on this repo:

- The release pipeline (`.github/workflows/release.yml`) runs `cargo fmt --check` as a step inside the Build job. A single mis-formatted line fails the job, skips the "Publish GitHub Release" step, and burns a ~25 min CI run before producing anything. The matrix is already a single `macos-15` / `aarch64-apple-darwin` lane.
- It also blocks tag-push releases: if the tag commit is dirty, you have to delete and re-create the tag at the fix commit to retry. The original tagged build cannot be salvaged.
- rustfmt drifts between Rust point releases. Even code that compiled clean a week ago can need re-formatting after a toolchain bump.

For tag-push releases specifically: run `cargo fmt --check` *one last time* on the exact commit you are about to tag, before `git tag` and `git push origin <tag>`. This is the cheapest possible guard against a wasted 25 min release run.

## Architecture

```
PaneFlowApp (Entity<Render>)           ← src-app/src/main.rs
├── app/                               ← PaneFlowApp impl, split across modules
│   ├── actions.rs                     ← 93 GPUI action types (paneflow namespace)
│   ├── bootstrap.rs                   ← app init, window creation, GPUI setup, poll loops
│   ├── event_handlers.rs              ← title-bar/pane/terminal event subscribers + stale-PID sweep
│   ├── ipc_handler.rs                 ← JSON-RPC handler + process_automation_tick (50 ms)
│   ├── session.rs                     ← persist/restore workspaces to session.json
│   ├── settings.rs                    ← settings lifecycle: open/close, persist_setting, key handlers
│   ├── diff_dock/                     ← git diff dock (`code/` file + terminal tabs); parked per TAB
│   │                                     (`cli_diff_dock.rs` keys slots by `Tab::id`, never by workspace);
│   │                                     rendered width = min(stored, main-panel remainder), and the dock is
│   │                                     not rendered at all below the floor (remainder < 360 px dock +
│   │                                     one minimum pane); stored width only written by the resize drag,
│   │                                     and a drag pinned at the render ceiling leaves a wider preference alone
│   ├── diff_sidebar/ files_sidebar/   ← diff + file trees; Files rail is per-tab (`Tab::files_sidebar_open`),
│   │                                     CLI-cockpit only, every row (`.md` too) opens as source in the dock editor
│   ├── sidebar/ sidebar_actions_menu.rs ← sidebar list + context menus (`context_menu.rs`; Remove worktree row, #348),
│   │                                     Customize Sidebar menu (`customize_menu.rs`: `sidebar_show` toggles,
│   │                                     Expand all / Collapse all, #349); footer mode tabs
│                                         + IPC banner (no Settings affordance at all)
│   ├── agent_status.rs                ← hookless agent state: pane OSC observations + Claude session-registry sweep
│   ├── attention_queue.rs             ← "which agent needs me" queue
│   ├── broadcast.rs / composer.rs     ← multi-pane prompt fan-out, prompt composer
│   ├── fleet_search.rs                ← cross-pane search
│   ├── launch_pad.rs                  ← agent launcher UI
│   ├── pane_overview/                 ← Cmd+Shift+P expose: every terminal pane across every
│                                         workspace/tab, grouped, each bottom-cropped to its last
│                                         12 rows (rows.rs is the pure, GPUI-free row model)
│   ├── system_info_dialog.rs          ← Help ▸ System Info… modal + Copy button (report from system_info.rs)
│   ├── tab_worktree.rs                ← per-tab worktree binding (#347): cached checkout git state, branch/worktree
│   │                                     listings, bind_tab_to_branch (prepare_branch_checkout off-thread, never managed)
│   └── workspace_ops/                 ← create/close/select/rename/reveal, focus, layout, swap, tab
├── cli/                               ← `paneflow up|flow|watch|wait|send|read` over the IPC socket
├── window_chrome/
│   ├── csd.rs                         ← client-side decorations, resize edges
│   ├── macos_backdrop.rs              ← native material behind sidebar/title bar
│   └── title_bar.rs                   ← window controls, drag-to-move
├── workspace/                         ← Vec<Workspace> state
│   ├── mod.rs                         ← Workspace struct, AI agent PIDs, MAX_WORKSPACES = 20
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
│   ├── bench_corpus.rs / ghostty_stress.rs ← corpus data + counters, runtime stress (#[ignore])
│   └── element/                       ← low-level GPUI Element rendering
│       ├── mod.rs                     ← TerminalElement: layout → prepaint → paint
│       ├── color.rs                   ← ANSI→Hsla, APCA contrast
│       ├── font.rs / geometry.rs      ← font resolution + cell geometry
│       ├── hyperlink.rs               ← OSC 8 + URL scanning
│       ├── paint/                     ← background, text, cursor, selection, scrollbar, box-drawing
│       ├── thumbnail.rs               ← read-only cropped pane preview; NEVER routes through
│                                          TerminalElement (its build_layout resizes the PTY)
│       └── golden/ pixel_probe.rs     ← golden-image + pixel assertions
├── theme/                             ← theme model + hot-reload (8 bundled variants)
│   ├── model.rs                       ← TerminalTheme (36 Hsla slots + ui + syntax), UiColors
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
├── diff/                              ← git diff engine + viewer (custom Element, own hscroll)
├── markdown/                          ← streaming Markdown view (parser, security, theme); panes come from
│                                         OSC path click + session restore only (no Files-sidebar drag or click)
├── agents/                            ← agent process supervision, notifications
├── ai_hooks/                          ← ai.* hook payload extraction
├── {claude,codex,opencode,pi,command}_sessions.rs ← per-agent session-file readers
├── agent_launcher.rs / agent_sessions.rs ← spawn agents through the PATH shim
├── widgets/                           ← text_input, text_area, scrollbar, callout
├── fonts.rs                           ← load_mono_fonts (Core Text on macOS)
├── ai_types.rs                        ← AiToolState, AgentStateSource ranking, lifecycle reducer
├── claude_session_registry.rs         ← reads Claude Code's sessions/<pid>.json (state without hooks)
├── ipc.rs / ipc_events.rs             ← JSON-RPC server over `interprocess`, event bus
├── keys.rs                            ← key translation (mouse encoding lives in terminal/input.rs)
├── search.rs                          ← find-in-buffer UI glue
├── limits.rs                          ← centralized ingress/egress size caps
├── runtime_paths.rs                   ← runtime/data/config path helpers + sun_path guard
├── login_shell_env.rs                 ← adopt the login shell's PATH (GUI launch has none)
├── config_writer.rs                   ← read-modify-write paneflow.json
├── window_state.rs / editor.rs / external_open.rs
├── sidebar_title.rs                   ← sidebar label cleanup
├── system_info.rs                     ← Help ▸ System Info… collection: sysctl, Metal devices, install format, libghostty identity
└── assets.rs                          ← rust-embed asset registry (fonts, icons)
```

**libghostty-vt is the only terminal engine (issue #184, 2026-08-31).** Stage 2a (2026-08-25) had deleted the Ghostty backend because upstream never reached it on macOS; upstream v0.10.0 made macOS a Ghostty target, and this fork ported that engine in three phases: the three `paneflow-{libghostty-sys,terminal-ghostty,ghostty-smoke}` crates plus the `aarch64-apple-darwin` archive vendored from upstream v0.10.0 and stripped to macOS (`native/libghostty/`, linking unconditional, no stub), then the session-host swap that deleted `alacritty_terminal` and `polling` from `src-app`. Daily `cargo build` needs no Zig: the `-sys` build script verifies the vendored archive's hashes and links it. The model: **PaneFlow owns the PTY (`portable-pty`); libghostty-vt parses bytes and owns the grid; GPUI paints snapshots.** Ghostty does not render, does not talk Metal, and does not own the child. `terminal.backend` is gone from the config schema; a leftover key in `paneflow.json` is ignored. `alacritty_is_absent_from_the_app_crate` (`terminal/types.rs`) fails if the word comes back anywhere in `src-app` outside that file. `fuzz/` stays deleted. Linking prints `ld: duplicate symbol '_memset'` (`compiler_rt.o` vs `libghostty-vt-static_zcu.o`): a property of the vendored archive, benign, and it rides the next upstream libghostty bump rather than a local fix: regenerating the archive needs a Linux build host (Ghostty only takes the Apple `libtool` combine path on Darwin), Zig 0.16.0, and upstream's `scripts/build-libghostty-macos.sh`, none of which this fork carries. Issue #194 was closed on 2026-09-02 for that reason; re-vendor and re-check when `source_sha` next moves.

**What the fork keeps on top of upstream's host (the close-guard trap).** Upstream's `terminate_child` was `kill(-pgid, TERM)` → 100 ms → `KILL` on one group from the runtime thread, unpinned. This fork's contract is stronger and lives in `TerminalState::Drop` (`terminal/pty_session.rs`): pin every live process group in the PTY session through an **app-owned `dup()` of the master** (`SpawnedGhostty::master_fd`, taken at spawn), SIGTERM them synchronously, drop the external guards, then `GhosttySession::shutdown()`, close the dup, and SIGKILL 100 ms later with start-time pins re-checked. **The runtime thread never signals**: on an app-initiated shutdown or a natural exit it only reaps (`reap_child_bounded`); `terminate_child` survives solely for the engine-failure paths (runtime failed, `waitid` failed, the startup and panic guards). `dropping_the_state_kills_background_and_stopped_jobs_in_the_pty_session` pins the outcome with a live shell.

### Thread model

- **Main thread**: GPUI event loop, owns all Entity state, rendering, input dispatch. No locks around UI state.
- **Ghostty runtime thread** (`paneflow-ghostty-runtime`, one per terminal): owns the `!Send` `DisplayTerminal`, the PTY master and the child; drains the mailbox (input, resize, selection, shutdown) and publishes `Content` snapshots through a `PublishGate` (#343, upstream 799ab51d + 8d8a9d88 + e03972c5). The gate holds a frame for two reasons: DEC 2026 synchronized output is set (one FFI mode query per wake, `DisplayTerminal::synchronized_output`), or the previous publish is newer than **8 ms** (`MIN_PUBLISH_INTERVAL`; deferred to the interval's end through `next_wake`, never dropped). A 2026 hold expires after **150 ms** (`SYNC_OUTPUT_MAX_HOLD`) so a program that opens a frame and dies cannot freeze the pane. Resize, scroll, scrollback clear, reset, select-all, a command mark, the first frame of a session, and the frame preceding `ChildExited` bypass the gate (`publish_now`). A `Wakeup` is queued only for a frame that was actually published. Publishing converts only the rows the engine flagged (`ghostty::Content::dirty_rows` → `CellMirror`, two alternating `Arc<[Cell]>` buffers, full conversion when the render thread still holds the back buffer). The loop blocks 10 ms (`RUNTIME_IDLE_TICK`) only while output flows, a drag is held, or a child is winding down, and **100 ms** (`RUNTIME_QUIET_TICK`) once a pane has been silent for a second; the display-only runtime blocks for a second. A `Shutdown` message wakes the mailbox at once, so neither tick delays the close guard, and the gate touches nothing but the grid: it never signals or reaps (see the close-guard trap above). A sibling **PTY reader thread** (`paneflow-ghostty-pty-reader`) feeds it 32 KiB chunks through a 4-buffer pool. An OSC 8 hover lookup is a `HyperlinkHover` message answered by `GhosttyUiEvent::HyperlinkResolved`, never a blocking round trip from the UI thread.
- **IPC thread**: Unix socket server. The runtime dir resolves through a fallback chain (`runtime_paths.rs`): `$XDG_RUNTIME_DIR` → `dirs::runtime_dir()` (None on macOS) → `$TMPDIR` (the macOS path, usually `/var/folders/xx/.../T/`) → `dirs::cache_dir()/run`. Socket at `<runtime_dir>/<APP_SUBDIR>/<socket>`: `paneflow/paneflow.sock` in release, `paneflow-dev/paneflow-dev.sock` under `debug_assertions`. The composed path is rejected if it exceeds the `sockaddr_un.sun_path` ceiling (104 bytes on macOS). `PANEFLOW_SOCKET_PATH` overrides the computed path so an isolated debug instance and the panes it launches agree on one endpoint.
- **Watcher threads**: config (notify, 300 ms debounce, 1 s max-wait ceiling), theme, git state.
- **Shared state**: `parking_lot::RwLock<SharedState>` (`Content` cells + modes + metrics + kitty placements) written by the runtime thread and read by the GPUI thread; `UiEventState` slots carry title/cwd/progress/notification/clipboard events. The libghostty C handle never leaves the runtime thread.

### Data flow: keystroke → pixel

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

### Workspace crates

| Crate | Path | Type | Purpose |
|-------|------|------|---------|
| `paneflow-app` | `src-app/` | Binary | GPUI application: all UI, PTY, IPC, CLI |
| `paneflow-config` | `crates/paneflow-config/` | Library | Config schema, JSON loader, file watcher |
| `paneflow-ipc-client` | `crates/paneflow-ipc-client/` | Library | Blocking JSON-RPC client for the local socket |
| `paneflow-mcp` | `crates/paneflow-mcp/` | Binary | Read-only stdio MCP server (see below) |
| `paneflow-mcp-install` | `crates/paneflow-mcp-install/` | Library | GPU-free per-agent MCP config merge engine |
| `paneflow-agent-setup` | `crates/paneflow-agent-setup/` | Library | GPU-free rulebook inventory (instruction files, skills, rules, hooks, MCP) behind the dock's Agent setup tab (#331) |
| `paneflow-shim` | `crates/paneflow-shim/` | Binary | PATH shim wrapping 16 agent CLIs |
| `paneflow-ai-hook` | `crates/paneflow-ai-hook/` | Binary | Hook binary agents invoke to report lifecycle events |
| `paneflow-process` | `crates/paneflow-process/` | Library | Bounded subprocess execution (deadline + stdout cap) |
| `paneflow-agent-config` | `crates/paneflow-agent-config/` | Library | Shared agent config, hooks, locking, Claude hook shapes |
| `paneflow-libghostty-sys` | `crates/paneflow-libghostty-sys/` | Library | Raw libghostty-vt FFI; `build.rs` verifies and links `native/libghostty/prebuilt/aarch64-apple-darwin` (no Zig) |
| `paneflow-terminal-ghostty` | `crates/paneflow-terminal-ghostty/` | Library | Safe `DisplayTerminal` wrapper over the FFI (unwired until #184 Phase 2) |
| `paneflow-ghostty-smoke` | `crates/paneflow-ghostty-smoke/` | Binary | Headless PTY smoke against the linked archive |

There is **no** `paneflow-telemetry` crate. It was deleted in the post-2c
grind. Zed lockfile crates named `telemetry` / `telemetry_events` belong
to the markdown pin; leave them.

Everything that runs outside the GUI process must stay GPU-free and never link GPUI.

## Critical external dependencies

GPUI and `gpui_platform` are **git dependencies** pinned to `zed-industries/zed`:

```toml
gpui = { git = "https://github.com/zed-industries/zed", rev = "fecc3273ed32643c2ea1b04a74c8780e2c9ffaf8" }
gpui_platform = { git = "...", rev = "fecc3273...", features = ["font-kit"] }   # font-kit is mandatory on macOS
```

Cargo fetches GPUI from git automatically. **There is no local checkout and no path dependency.** Two crates-io patches are required by GPUI:
- `async-task` → `smol-rs/async-task` (specific git commit)
- `calloop` → `zed-industries/calloop` fork

Terminal emulation is `paneflow-terminal-ghostty` (workspace crate, `src-app/Cargo.toml`), the safe wrapper over `paneflow-libghostty-sys`, whose build script links `native/libghostty/prebuilt/aarch64-apple-darwin/lib/libghostty-vt.a` after verifying every hash in `native/libghostty/manifest.toml`. `portable-pty = "0.9"` opens the PTY and spawns the child; `image` (PNG only) decodes Kitty graphics. `cargo deny` cannot see the static archive: `native/libghostty/THIRD_PARTY_NOTICES.md` is its license inventory and ships in the bundle as `ThirdPartyLicenses/libghostty.txt`.

## GPUI patterns

- **Entity/Context model**: all mutable state lives in `Entity<T>`, mutated via `Context<Self>`. Use `cx.new()` to create, `cx.notify()` to trigger repaint, `cx.spawn()` for async tasks.
- **`actions!` macro** (`app/actions.rs`): generates zero-sized typed action structs in the `paneflow` namespace. Actions are dispatched through GPUI's focus chain.
- **`Render` trait**: implement for high-level views (PaneFlowApp, TitleBar, TerminalView). Returns a div element tree.
- **`Element` trait**: implement for low-level custom rendering (TerminalElement and DiffElement only). Has 3 phases: `request_layout()` → `prepaint()` → `paint()`.
- **Focus**: each `TerminalView` owns a `FocusHandle`. Key context `"Terminal"` scopes terminal-only keybindings; other contexts are `Search`, `Markdown`, `MarkdownSearch`, `DiffView`. Focus navigation is structural (layout-tree traversal), not spatial.
- **No `Arc`/`Mutex` for UI state**: use `Rc<Cell<f32>>` for single-threaded shared state (e.g. split ratios in render closures).

## GPUI scroll & wheel (gotchas)

Hard-won from the diff-dock horizontal-scroll saga (`src-app/src/app/diff_dock/mod.rs`). Verified against the Zed source. Do NOT re-derive these by guessing, it cost three wrong attempts.

- **Shift+wheel is axis-swapped to X at the platform layer**, before app code ever sees it. On macOS the NSEvent delivers the horizontal component natively; the other platform backends do the swap explicitly. Either way the value lands in `delta.x` with `delta.y` zeroed. So: read `delta.x` for horizontal, NEVER branch on `modifiers.shift` (reading `delta.y` under Shift reads zero). The `div.rs` `delta_x = delta.y` line is a separate fallback (fires only when `delta.x == 0`), not the Shift mechanism.
- **`overflow_hidden()` + `track_scroll()` does NOT scroll-translate children.** It only keeps the handle's bookkeeping (`offset()`/`bounds()`/`max_offset()`) live. GPUI only pushes the scroll offset onto the element-offset stack (which bakes into each child's `bounds.origin`) when the host overflow axis is `Overflow::Scroll`. A custom `Element` that positions content off its own `bounds.origin` (e.g. `DiffElement`) therefore only scrolls under `overflow_y_scroll`/`overflow_scroll`; `set_offset()` under `overflow_hidden` is stored but dead. Custom elements get the shift automatically via their passed `bounds` (no `window.element_offset()` call needed).
- **Two-axis recipe (vertical list whose items also scroll horizontally)**, the canonical Zed pattern (`data_table.rs`, `thread_view.rs`, `markdown.rs`): host = `overflow_y_scroll()` + `track_scroll(&handle)` + `element.style().restrict_scroll_to_axis = Some(true)`. The flag is a raw `StyleRefinement` mutation (no builder method, but it compiles: non-`#[refineable]` `Style` fields still become `Option<T>`). It stops a vertical wheel bleeding into a horizontal child AND stops the native Y handler back-filling `delta_y = delta.x` under Shift+wheel (the "vertical scrolls when I Shift+wheel" bug). Per-item horizontal stays custom (an `on_scroll_wheel` reading `delta.x` only); native owns vertical.

## Split / layout system (`layout/`)

The old binary `SplitNode` in `split.rs` is gone. `LayoutTree` (`layout/tree.rs`) is an N-ary tree:

- `LayoutTree::Leaf(Entity<Pane>)` | `LayoutTree::Container { direction, children: Vec<LayoutChild>, drag, container_size }`
- Each `LayoutChild` carries `node` plus `ratio: Rc<Cell<f32>>`.
- `SplitDirection::Horizontal` = **horizontal divider, panes stacked top/bottom** (`flex_col`). `Vertical` = panes side by side (`flex_row`). Counterintuitive but consistent throughout the codebase.
- Layout uses GPUI flex divs with `flex_basis(relative(ratio))`. `MIN_PANE_SIZE = 80.0`, `DIVIDER_PX = 8.0`, `DIVIDER_HIT_PX = 7.0` (`layout/tree.rs`): the divider is an unpainted shell-revealing gap with a narrower resize hitband centered inside it.
- `MAX_PANES = 32` (`layout/mod.rs:34`), `MAX_WORKSPACES = 20` (`workspace/mod.rs:53`). Both are enforced on the live create path *and* at session restore and config load; `limits.rs` documents the read/write cap pairs.
- Drag-to-resize is pixel-accurate: `Container::container_size` captures the real main-axis pixel size each frame via a `canvas()` prepaint, so there is no hardcoded container estimate (the old `split.rs` 800px guess is gone).
- Presets in `layout/presets.rs`: `from_panes_equal` (even horizontal / even vertical), `main_vertical`, `tiled`.

## Keybindings

All registered in `keybindings::apply_keybindings()` via `cx.bind_keys()`. 93 actions total (`app/actions.rs`; `claude_md_action_count_matches_the_actions_macro` fails if this number or the one in the tree above drifts from the `actions!` block); tables in `keybindings/defaults.rs`.

**`secondary` resolves to Cmd on macOS** (`defaults.rs:12-14`), so every `secondary-*` default below is a Cmd binding here. `MACOS_ONLY_DEFAULTS` (`defaults.rs`) adds `Cmd+C`, `Cmd+V`, `Cmd+K` (Terminal: copy, paste, clear scrollback) and `Cmd+Q` (quit) on top.

**The macOS menu bar** (`app/bootstrap.rs::install_macos_menu_bar`, `#[cfg(target_os = "macos")]`) is PaneFlow (`About PaneFlow`, `Settings…`, separator, `Report an Issue`, separator, `Quit PaneFlow`) / Edit / Window (`Minimize`, `Zoom`, separator, `Show All Panes`, separator, `Next Workspace`, `Close Workspace`, `New Workspace`) / Help (`PaneFlow Help`, separator, `System Info…`). `Settings…` dispatches `OpenSettings` into `open_settings_window`. `Report an Issue` dispatches `ReportIssue` and opens `https://github.com/theaamgroup/paneflow/issues/new` in the default browser. `System Info…` dispatches `ShowSystemInfo` into `open_system_info_dialog` (`app/system_info_dialog.rs`): a copyable environment block - version, install format, OS, CPU, GPU, renderer, libghostty version - with no project path and no environment dump, collected off the render thread by `system_info.rs` (`sysctl`, `MTLCopyAllDevices`, and `sparkle::bundled_framework_binary` for the install format). Like `About` / `OpenHelp` / `OpenSettings` / `ReportIssue` it has no default chord and is absent from `keybindings/registry.rs::ACTIONS`. Theme selection lives in Settings → Appearance (and the title-bar profile menu's `Themes…` row, which calls `open_theme_picker` directly); there is no View menu. Every menu action needs BOTH a render-root `.on_action` in `main.rs` and an app-global fallback in `install_macos_menu_action_fallbacks`, or AppKit's `is_action_available` check paints the item permanently greyed while focus sits in a terminal. `OpenSettings` is deliberately absent from `keybindings/registry.rs::ACTIONS` (the `About` / `OpenHelp` precedent) so Settings → Keyboard Shortcuts does not grow permanently `Unassigned` rows, and **`Cmd+,` is deliberately unbound** (issue #105) - `no_default_binds_the_macos_preferences_chord` in `keybindings/apply.rs` fails if any default claims it. The sidebar's "Workspaces" header carries no `+` (issue #105); it does carry the Pane Overview button (issue #339, id `sidebar-pane-overview`), which the #105 guard test permits because it forbids only the `sidebar-new-workspace` id. New Workspace is `Cmd+Shift+N`, Window ▸ New Workspace, the profile menu, and the empty-state "Open folder" button. The sidebar footer carries **no Settings affordance at all** - the gear that survived issue #105 is gone, so `Settings…` on the menu bar and the title-bar profile menu are the only two entry points.

| Key | Action | Context |
|-----|--------|---------|
| `Cmd+Shift+D` / `Cmd+Shift+E` | Split horizontal / vertical | Global |
| `Cmd+Shift+W` / `Cmd+Shift+T` | Close pane / undo close pane | Global |
| `Cmd+Alt+T` / `Cmd+W` | New tab / close tab | Global |
| `Cmd+]` / `Cmd+[` | Next tab / previous tab | Global |
| `Alt+Arrow` | Focus navigation | Global |
| `Cmd+Shift+N` / `Cmd+Shift+Q` | New / close workspace | Global |
| `Ctrl+Tab` | Next workspace | Global |
| `Cmd+1`-`Cmd+9` | Select workspace | Global |
| `Cmd+Alt+1`-`4` | Layout preset: even-h, even-v, main-vertical, tiled | Global |
| `Cmd+Shift+=` / `Cmd+Shift+S` | Equalize splits / swap pane | Global |
| `Cmd+Shift+Z` | Toggle zoom | Global |
| `Cmd+Shift+J` / `Cmd+Shift+A` | Jump to next waiting agent / open attention queue | Global |
| `Cmd+Shift+P` | Pane overview (every terminal pane, all workspaces and tabs) | Global |
| `Cmd+Shift+G` | Diff view | Global |
| `Cmd+G` / `Cmd+J` | New file tab / new terminal tab (diff dock; `secondary-g` / `secondary-j`) | Global, not Terminal/TextInput/CodeEditor |
| `Cmd+Shift+Space` / `Cmd+Shift+L` | Composer / launch pad | Global |
| `Cmd+Shift+B` / `Cmd+Shift+M` | Toggle broadcast member / broadcast groups | Global |
| `Cmd+Alt+F` | Toggle files sidebar for the active tab (inert in Review and Settings, where the rail is unmounted) | Global |
| `Cmd+Alt+B` | Toggle primary sidebar (persisted across launches) | Global |
| `Ctrl+Alt+R` / `Ctrl+Shift+Alt+C` | Reveal in Finder / copy workspace path | Global |
| `Ctrl+Alt+Z` / `C` / `V` / `W` | Open workspace in Zed / Cursor / VS Code / Windsurf | Global |
| `Cmd+C` / `Cmd+V` | Copy / paste (macOS layer) | Terminal |
| `Ctrl+Shift+C` / `Ctrl+Shift+V` | Copy / paste (cross-platform layer, still bound) | Terminal |
| `Shift+PageUp` / `Shift+PageDown` | Scroll page | Terminal, Markdown |
| `Cmd+Shift+Up` / `Cmd+Shift+Down` | Jump to prev / next shell prompt mark | Terminal |
| `Cmd+K` / `Cmd+Shift+K` | Clear scrollback (`clear_scroll_history`; `cmd-k` is the macOS layer, `secondary-shift-k` the alias) | Terminal |
| `Cmd+Shift+R` | Reset terminal (`reset_terminal`, RIS) | Terminal |
| `Ctrl+Shift+X` / `Ctrl+Shift+F` | Copy mode / find-in-buffer | Terminal |
| `Cmd+=` / `Cmd+-` / `Cmd+0` | Font size up / down / reset | Terminal |
| `Ctrl+F` / `Ctrl+Shift+C` | Find in buffer / copy selection | Markdown |
| `Ctrl+Shift+C` | Copy diff hunk | DiffView |
| `Enter` / `Shift+Enter` / `Esc` | Next / prev / dismiss | Search, MarkdownSearch |
| `Alt+R` / `Alt+F` | Toggle regex / fleet-wide search | Search |
| `]` / `[` / `u` / `s` / `Esc` | Next hunk / prev hunk / toggle view / toggle sync / dismiss | DiffView |
| `Cmd+Q` | Quit (macOS only) | Global |

The Attention Queue is `Cmd+Shift+A`, not `Cmd+Shift+K` (issue #184): `secondary-shift-k` is the terminal-convention clear-scrollback chord (kitty, Ghostty) and `clear_scroll_history` owns it now, with `cmd-k` as the macOS spelling. `attention_queue_is_cmd_shift_a_and_cmd_shift_k_clears_scrollback` in `keybindings/apply.rs` fails if the queue drifts back, if any of those chords gains a second claimant, or if `close_window` returns (it was removed: closing the window is `Quit`, and the title-bar close button reaches `quit_after_session_save` through `TitleBarEvent::CloseRequested`).

Next-workspace is `ctrl-tab`, not the upstream `secondary-tab` (Cmd+Tab): macOS reserves Cmd+Tab for the application switcher and never delivers it to the app (issue #10; a synthetic Cmd+Tab on 2026-08-27 moved focus to another app while Cmd+1/Cmd+2 through the same path switched workspaces). A test in `keybindings/apply.rs` fails if any default binds `secondary-tab` again.

## Config

Location on macOS: `~/Library/Application Support/paneflow/paneflow.json`, resolved via `dirs::config_dir()` in `crates/paneflow-config/src/loader.rs:55`. Debug builds use the `paneflow-dev` subdir instead. There is **no** `~/.config/paneflow/` on macOS.

```json
{
  "default_shell": "/bin/zsh",
  "theme": "PaneFlow Dark",
  "window_decorations": "client",
  "font_family": "JetBrainsMono Nerd Font Mono",
  "font_size": 13.0,
  "option_as_meta": false,
  "shortcuts": {},
  "commands": []
}
```

- **Themes**: **8 bundled variants** (`theme/builtin.rs:9-18`): `PaneFlow Dark` (default identifier, `DEFAULT_THEME`), `PaneFlow Light`, `Vercel Dark` / `Vercel Light`, `Claude Dark` / `Claude Light`, `Cursor Dark` / `Cursor Light`. Legacy alias table (`LEGACY_THEME_ALIASES`) maps `One Dark` → `PaneFlow Dark` (plus the old single-name Vercel/Claude/Cursor entries onto their dark variants). The previous `Paneflow Dark` / `Paneflow Light` spelling still resolves (names are matched case-insensitively). Hot-reload is notify-driven with a 500 ms mtime-poll fallback (`theme/watcher.rs:37`).
- **`window_decorations`**: read at startup only, requires restart. `"client"` = CSD (default), `"server"` = SSD. An invalid value logs a warning and falls back to `"client"`.
- **`shortcuts`**: wired via `keybindings::apply_keybindings()` at startup. Users can override default keybindings here. The schema stays a free-form object; Settings → Keyboard Shortcuts only reads and writes it. That page groups every registry action under a `ShortcutGroup` (`keybindings/registry.rs`), filters by action name *or* chord (`ShortcutEntry::search_key` carries the ASCII spellings, `cmd+shift+a` / `cmd-shift-a`, of the glyph key), has a "Find by key" capture mode, and is virtualized with `gpui::list` because it is the one page long enough to lag when every row was rebuilt per frame. Rebinding records through `app/settings.rs::recorded_shortcut_key` (`Keystroke::unparse()`, never `to_string()`, or the saved chord is Apple glyphs no keypress can match) and reaches the page through `App::intercept_keystrokes` (`main.rs::mount_paneflow_app`) - the only hook that runs before GPUI dispatches a matching binding, so recording Cmd+Shift+D no longer splits the pane. "Reset to defaults" is a two-step inline confirm (`step_reset_confirm`, pure); only the second click calls `config_writer::reset_shortcuts_checked()`.
- **`option_as_meta`**: **defaults to `false`**. `keys::default_option_as_meta()` returns the literal `false` (`keys.rs:69`); it used to compute `!cfg!(target_os = "macos")`, which was a runtime expression that is constant in a macOS-only fork. So out of the box Option+key composes a character (`é`, `∂`) instead of sending an Alt escape sequence, which is the macOS convention but surprises anyone expecting Alt keybindings in tmux, Emacs, or a readline prompt. Set it to `true` to get Meta behavior. The published JSON Schema and `docs/user/configuration/schema.md` both declare `false` too - they moved together in `6a7b14d` and a drift test reads the doc off disk.
- **`macos_chrome_material`**: opts the sidebar and title bar into a native AppKit material (`window_chrome/macos_backdrop.rs`). `windows_terminal_material` and `windows_chrome_material` are **gone from the published schema** and the Rust struct. The loader still accepts those leftover keys (and a leftover `telemetry` block) as ignored no-ops so existing `paneflow.json` files keep loading.
- **`review_enabled`**: master switch for the Review surface, **defaults to `true`** (`None`-is-on, like `shell_integration` and `agent_stall_detection`). Off, the footer's mode strip is not rendered **at all** - one reachable mode is not a choice, and the segment builder already drops the click handler from the active segment, so a lone "Agents" button would be dead chrome. `enter_diff_mode` (`app/diff_view_actions.rs`) is the single chokepoint every entry path funnels through, so `Cmd+Shift+G` becomes a silent no-op. Two demotion paths exist because the tick has no `Window`: the Settings toggle calls `enter_cli_mode` (restores focus), while a hand edit to `paneflow.json` lands in `leave_review_if_disabled` on the automation tick (no focus move, relying on the issue #110 `on_focus_lost` fallback). Session restore folds the switch into the existing diff-viability test in `bootstrap.rs`.
- **`sidebar_show`** (issue #349, `crates/paneflow-config/src/schema/config.rs::SidebarShow`): what a rail row shows beyond its name, one `Option<bool>` switch per line, toggled from the rail header's Customize Sidebar menu (`app/sidebar/customize_menu.rs`, "Show" submenu, plus Expand all / Collapse all) or by hand. Defaults are the rail before the menu existed, so a `paneflow.json` with no `sidebar_show` renders exactly as before: `branch` = **`true`** (the workspace's branch on its folder row, a bound tab's worktree branch on its tab row), `diffstat` = **`false`** (insertions and deletions pinned right of the branch line, from the tab's bound `CheckoutGit.stats` or else `Workspace::git_stats`, drawn only when non-zero), `pr` = **`false`** (issue #350, the `gh` pull-request marker), `indent_guide` = **`false`** (a hairline under the folder icon down the tab rows). The menu writes the whole object through `config_writer`; a hand edit hot-reloads. The fold state of each workspace row persists separately as `WorkspaceSession.sidebar_collapsed` (written only when folded; session schema stays v2).
- **`new_pane_shows_sessions`**: when `true`, every Tab-placement New pane picker also opens the Agent sessions sidebar on the right, scoped to the workspace cwd, so a listed session can be resumed into that pane. **Defaults to `false`**. Split-placement pickers leave the sidebar alone. Off (the default) keeps today's behaviour: history is only reachable from a pane-header button.
- **Sessions-sidebar row menu** (issue #334, `app/sessions_context_menu.rs`): right-click a session row for Resume / Copy summary / Continue in ▸. "Continue in" lists every visible launcher except the row's own agent (reader-less ones carry a "no session history" hint) and opens a **new workspace tab** at the session's cwd running the target's launch command (`open_agent_tab_at_cwd`, `workspace_ops/tab.rs`, the lift of the drop handler's center band, with the #347 worktree binding), then prefills the handoff block from the pure `app/sessions_handoff.rs` (`handoff_prompt`: source agent, `Session:`, `Cwd:`, `Branch:` when known, `Summary:` capped at `HANDOFF_SUMMARY_CAP` = 4 KiB; identifier fallback when nothing usable was recorded; `Session: (id withheld)` when the id fails the resume allow-list). "Copy summary" puts the same payload chain on the clipboard. The prefill (`schedule_prompt_prefill`) writes through `inject_text` - bracketed-paste markers when the surface enabled `ESC[?2004h`, verbatim otherwise - and **never submits**; no agent is ever spawned to write a summary.
- **`ConfigWatcher`** (notify crate, 300 ms debounce with a 1 s max-wait ceiling so a continuous event stream cannot starve the reload): fully wired, a background thread detects changes and deposits new config for the GPUI main thread to apply.
- `MAX_CONFIG_SIZE_BYTES` is 1 MiB (`limits.rs`); the app's own writer never approaches it.
- The full schema is published at `schemas/paneflow.schema.json` and **two tests in `crates/paneflow-config/src/schema.rs` read it off disk**, so schema drift fails the suite.

## IPC (`ipc.rs`)

Unix socket JSON-RPC 2.0 at `<runtime_dir>/paneflow/paneflow.sock` (see the thread model for the resolution chain). Methods:

| Method | Thread | Description |
|--------|--------|-------------|
| `system.ping` / `capabilities` / `identify` | Socket | Stateless health checks |
| `workspace.list` / `current` / `create` / `select` / `close` | GPUI | Workspace management; `close` uses the same live-agent confirmation gate as the UI and can return a pending-confirmation response |
| `workspace.up` / `restore_layout` | GPUI | Declarative bring-up, layout restore |
| `surface.list` / `read` / `search` / `status` | GPUI | Read pane state; `read` returns the retained history followed by the live screen (#184 Phase 3.6), so a full-screen TUI is readable |
| `surface.send_text` / `send_keystroke` | GPUI | Write into a pane (scripting-gated) |
| `surface.split` / `focus` / `rename` | GPUI | Pane operations |
| `fleet.list` | GPUI | Every surface across every workspace |
| `events.subscribe` | Socket | Streaming event subscription |
| `ai.session_start` / `prompt_submit` / `tool_use` / `notification` / `stop` / `exit` / `session_end` | GPUI | Agent lifecycle notifications from `paneflow-ai-hook` |

Stateful methods dispatch to the GPUI main thread via a channel drained by `PaneFlowApp::process_automation_tick`, which runs on a **50 ms** poll loop (`app/bootstrap.rs`, `app/ipc_handler.rs`). That same tick drains IPC requests, then surface-change broadcasts, then config reloads, so its ordering is a contract, not an accident. There is no in-app update-check.

**Agent state has three sources** (#184 Phase 3.8), ranked `Terminal < SessionRegistry < Hook` (`ai_types::AgentStateSource`): a pane's own OSC 9;4 progress and OSC 9 / 777 notifications (only for a pane whose process scan already resolved an agent), Claude Code's session registry (`<CLAUDE_CONFIG_DIR|~/.claude>/sessions/<pid>.json`, swept every **400 ms** by `app/agent_status.rs`, and only while some pane runs Claude Code), and the `ai.*` hooks. `ipc_handler::upsert_session_state` stays the single choke point; after the PID-recycle check and the `emitted_at_ms` watermark it applies `accepts_source`: a lower-ranked source only takes a session over once the higher-ranked one has been silent for `SOURCE_TAKEOVER_SILENCE` (20 s). That is what keeps the sidebar live on a machine whose managed settings disabled hooks, without an escape sequence ever talking over a hook's permission prompt.

## Styling conventions

- **All styling is inline** via GPUI's Tailwind-like builder API: `.bg(rgb(0x181825)).px_3().rounded_md()`
- **Sidebar/titlebar colors are hardcoded** dark hex values unless the active theme supplies a `UiColors` block. Legacy themes derive chrome colors from light/dark defaults; the bundled custom themes opt into exact UI tokens so the theme affects the whole app, not just ANSI colors.
- **Terminal colors** come from `TerminalTheme` (36 `Hsla` slots plus optional `ui: UiColors` and a `syntax: SyntaxPalette`, `theme/model.rs:11`) resolved via `active_theme()`. `selection_foreground` is computed at theme-load time so `apca_contrast(selection_foreground, selection) >= 45.0` holds at every observation point; if you construct a theme by hand, call `recompute_selection_foreground()`.
- **Font**: defaults to the embedded `JetBrainsMono Nerd Font Mono` at **13.0 pt** (`terminal/element/font.rs:24`, `:43`), range clamped to 8.0-32.0. Embedded families are always resolvable because `Assets::load_fonts` registers them with GPUI at boot. A configured `font_family` that is not an installed monospace family (checked against Core Text via `fonts.rs::load_mono_fonts`) logs a warning and falls back to the default.

## Gotchas

- **GPUI is not on crates.io.** It is consumed from the pinned Zed git fork above. Never replace it with a crates.io dependency, and never assume there is a local checkout to edit.
- **Never recommend iced** for this project. It was evaluated and rejected (unstable, custom WGPU glyph atlas too complex). The decision is final.
- **`SplitDirection::Horizontal`** means a horizontal divider bar (panes stacked top/bottom), NOT side-by-side. Counterintuitive but consistent.
- **No engine type crosses the `terminal/` seam.** `terminal/types.rs` (`Point`, `Cell`, `Content`, `Modes`, `SelectionGeometry`) is the contract the renderer and input consume; `ghostty_session.rs` translates libghostty values into it. `alacritty_is_absent_from_the_app_crate` fails if lower-case `alacritty` reappears anywhere under `src-app/src/` outside that file (case-sensitive on purpose: the intentional `ALACRITTY_*` env-scrub entries in `pty_session.rs` pass it), and `src-app/tests/dependency_source_policy.rs` still asserts every git source in `Cargo.lock` is pinned to an immutable revision.
- **`dirs` is a single workspace dependency at version 6** (`Cargo.toml` `[workspace.dependencies]`; both `src-app/Cargo.toml:114` and `crates/paneflow-config/Cargo.toml:14` use `dirs.workspace = true`). An older note about a 5.0/6 split between the two crates is stale.
- **Config `default_shell` is wired**: `resolve_default_shell` (`terminal/shell.rs:214`) validates the configured path is present and executable, warns and falls back if not, then uses the `$SHELL` → `/bin/sh` chain.
- **Teardown is the app's, not the engine's.** `TerminalState::Drop` owns every signal (see the close-guard trap above); `GhosttySession::shutdown()` only asks the runtime to close the PTY and reap. Never move a `kill()` back onto the runtime thread.
- **A GUI-launched app inherits launchd's minimal PATH, not the user's.** `login_shell_env.rs` runs the user's login shell once and adopts **only its `PATH`**, deliberately importing nothing else (a login profile that re-exports session variables would corrupt them). Without this, `/opt/homebrew/bin` is missing, terminals cannot find the user's tools, and agent-CLI detection (`which::which(...)`) comes up empty. Do not "simplify" it into a full env import.
- **Every `US-NNN` comment in the Rust source is a dangling breadcrumb.** They point at PRD files that lived under `tasks/`, were gitignored upstream, never committed, and `tasks/` is now deleted. Same for every `prd-*.md` and `EP-NNN` reference in a comment. Roughly 2,200 such comments across ~190 files: treat them as historical noise, do not go looking for the document, and do not add new ones.
- **Tests + CI exist**: run `cargo test --workspace`, `cargo clippy --workspace -- -D warnings`, and `cargo fmt --check`. UI changes still need manual verification.
- **A note that used to live here was wrong, and the correction is worth keeping.** It said the in-app updater's `update/macos/dmg.rs` two `#[cfg(all(test, not(target_os = "macos")))]` items should be un-gated rather than deleted. They could not be: they were the second half of a complementary pair — the real `cp -R` vs a test-host shim — so un-gating the second one is a duplicate definition, `error[E0428]`. Stage 2c deleted the shim; leftover-removal then deleted the whole updater (`src-app/src/update/` is gone). Before acting on a claim like that, check whether the two gates are complementary definitions of ONE item.
- **The binary-size budget is Mach-O now.** `src-app/build.rs` measures the three embedded helpers under `--profile release-min` on `aarch64-apple-darwin`: shim 472_368 + ai-hook 336_464 + mcp **403_008** B = **1_211_840 B** (J8 deferred; measured 2026-08-27). Cap is `EMBED_SIZE_LIMIT_BYTES = 1_400_000`, which is total + 15.5% (slack 188_160 B = 13.4% of the cap), quoted from `src-app/build.rs`; a `--release` build prints the measured total as a `cargo:warning`. Nested staging always uses `release-min`, so a debug outer build still embeds those sizes. Per-binary caps were dropped with the CI matrix (issue #3); do not re-derive a Linux ELF number.
- **License**: GPL-3.0-or-later (GPUI is a Zed fork). Keep packaging metadata in sync with the root `LICENSE` file and `Cargo.toml`.
- **`examples/review-pipeline.flow.toml` is an `include_str!` target** (`src-app/src/cli/flow_spec.rs:749`). Deleting it breaks the build. `examples/TASK.md` is its fixture. `clippy.toml` is likewise load-bearing: it carries the `allow-unwrap-in-tests` escape hatch for the workspace lint policy.
- **libproc CPU time is Mach ticks, not nanoseconds.** `TaskAllInfo.ptinfo.pti_total_user` / `pti_total_system` need `mach_timebase_info` (observed **125/3** on arm64). `Duration::from_nanos` on the raw tick count is ~50× too small (`terminal/bench_corpus.rs`).
- **`scripts/create-dmg.sh` is allowed to fail `codesign --verify --deep --strict` on an unsigned smoke.** The script writes the `.dmg` first, then the strict check exits 1 because the enclosed binary is adhoc/linker-signed. That check is for a signed+notarized release. Local artifact: `dist/paneflow-0.1.0-aarch64-apple-darwin.dmg` (~30M), `CFBundleIdentifier=com.theaamgroup.paneflow`. Gatekeeper will quarantine a copied copy.
- **Comments still mention Windows and Linux.** `runtime_paths.rs` still documents a named-pipe fallback. That is leftover copy. Do not re-implement from a comment. Ghostty identifiers in `terminal/view.rs` and `pty_session.rs` are the opposite: they are the live engine (#184) and must not be pruned.
- **Never bind `secondary-tab`.** It is Cmd+Tab on macOS and the app switcher eats it. Next-workspace moved to `ctrl-tab` (issue #10) and `next_workspace_is_bound_to_ctrl_tab_and_nothing_binds_cmd_tab` guards the table.

## MCP bridge (`paneflow-mcp`)

`crates/paneflow-mcp/` is a stdio MCP server letting CLI agents (Claude Code, Codex, Gemini, opencode) read other panes' terminal output via the existing IPC socket. Read-only (`list_panes` / `read_pane` / `search_pane`).

**Distribution (`paneflow mcp install`).** The bridge ships embedded in the `paneflow` binary (staged by `build.rs`, extracted at launch to a stable, non-versioned path under `data_dir()/paneflow/bin/` that survives updates: `runtime_paths::bridge_binary_path()`). `paneflow mcp install | uninstall | status` (intercepted in `main.rs` before GUI init) registers, removes, or inspects the `paneflow` MCP entry across every detected agent. The engine is the GPU-free `crates/paneflow-mcp-install/` crate (idempotent, no-clobber, backup + atomic write; `toml_edit` kept out of the embedded bridge per budget). Per-agent shapes: Claude Code `~/.claude.json` `mcpServers` (prefers `claude mcp add`), Codex `~/.codex/config.toml` `[mcp_servers.*]` (prefers `codex mcp add`), Gemini `~/.gemini/settings.json` `mcpServers` (`trust:true`), opencode `~/.config/opencode/opencode.json` key `mcp` (`command` array, `type:local`). There is **no** `.mcp.json` in this repo (dropped in `3e8a8464`), so working inside this project does *not* auto-wire Claude Code - run `paneflow mcp install` like anywhere else. Full setup and per-agent config: `docs/mcp-bridge.md`. There is also a Settings → AI Agent → "MCP bridge" button that runs the same install off the render thread (state-aware label: Install / Repair / Reinstall).

## Commit convention

```
feat(module): description
fix(module): description
refactor(module): description
docs: description
chore: description
```

Atomic commits per logical change. Branch naming: `feat/description`, cut from `main`. Cite the GitHub issue (`#123`) when the commit addresses one. Do not add `US-NNN` story IDs to commit messages: those PRDs are gone, and GitHub issues are the tracker.

Anything that diverges from upstream uses the `(fork)` scope, e.g. `chore(fork): drop non-macOS packaging scripts`, so the divergence stays greppable in the log. There is no CONTRIBUTING.md or SECURITY.md; this is a public, owner-maintained fork.

## Platform (macOS only)

This fork targets macOS on Apple Silicon and nothing else. Metal, AppKit, libghostty-vt (vendored `aarch64-apple-darwin` archive), Unix-socket IPC, signed and notarized `.app` / `.dmg`.

- Do not add Linux or Windows code paths back. No `#[cfg(target_os = "linux")]`, no `#[cfg(windows)]`, and no backend selector: libghostty-vt is the one terminal engine; `src-app/build.rs` refuses any target but `aarch64-apple-darwin`.
- **`#[cfg(unix)]` is not Linux-only.** It appears **172 times** and macOS needs nearly all of it - it is the single highest-risk distinction in this codebase. Do not prune unix-shared code because Linux code sat beside it. `#[cfg(target_os = "macos")]` appears **94** times. Both are live arms and both stay. Counted by the `./scripts/linux-census.sh` negative control (`cfg(unix)` / `cfg(macos)` live sites) on 2026-09-04, the same run recorded in the `docs/fork/STATE.md` verification block - **re-run it before quoting these numbers**. They drift with every pass, and the older figures still standing in the fork docs are the earlier counts of the same thing, not a disagreement to resolve: 162 in `docs/fork/2026-08-25-mac-only-fork-design.md`, and 152/77 and 138/71 in dated `STATE.md` entries.
- **After stage 2c those two are the only *cross-platform* predicates left.** No `target_os = "linux"`, no `not(unix)`, no `not(target_os = "macos")`, no `windows`. A `[target.'cfg(target_os = "macos")'.dependencies]` table **is** allowed and exists (`src-app/Cargo.toml:240`, `libproc` / `core-text` / AppKit). `./scripts/linux-census.sh` enforces the zero-condition: it exits 1 with a `FAIL:` line when the STAGE 2c total is non-zero or the negative control reads 0, and `run_tests.yml::platform_census` runs it (and `win-census.sh`) on every push and PR. It prints the `cfg(unix)`/`cfg(macos)` counts first as a negative control, because a census reading 0 with a broken regex looks exactly like one reading 0 because the work is done. A zero cfg census is also blind to ungated Windows strings (`powershell` / `.exe` / `.cmd` / `.bat` / `.ps1` / `\\?\` / `%APPDATA%`); that class is a separate reported check in the same script (issue #103) and is **not** part of the STAGE 2c integer.
- `#[cfg(all(unix, not(test)))]` still appears (in `terminal/pty_session.rs`). That is a test-isolation gate, not a platform gate. Leave it.
- Still use `std::path::PathBuf`, `std::env`, and `dirs` for filesystem and environment access. macOS-correct is not the same as hardcoded.
- **The old updater stays deleted; Sparkle 2 owns self-update.** Never recreate `src-app/src/update/`, minisign, an update prompt, or a forced relaunch. `src-app/src/sparkle.rs` dynamically loads the bundled framework, checks the AAM GitHub appcast hourly, downloads in the background, and holds installation until ordinary app termination. Packaging pins and checksum-verifies Sparkle in `scripts/sparkle-dist.sh`; release signing adds EdDSA (`SPARKLE_PRIVATE_KEY`) on top of Developer ID + notarization. Do not create `GPG_*`, `AZURE_*`, `POSTHOG_API_KEY`, `MINISIGN_SECRET_KEY`, or `PANEFLOW_MINISIGN_*`.

The full removal plan, with the paired edits that have to land together, is in `docs/fork/2026-08-25-mac-only-fork-design.md`.
