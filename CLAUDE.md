# CLAUDE.md - PaneFlow

Read `AGENTS.md` first. Its shared agent workflow governs issue metadata,
safety routing, PR reviews, and admin merge of a fully green pull request.

Native Rust terminal workspace for running coding agents in parallel. Built with Zed's GPUI framework; VT emulation is Ghostty's `libghostty-vt`, statically linked from a vendored archive (`native/libghostty/`), with PaneFlow owning the PTY through `portable-pty`. **This fork is macOS only.**

Fork context, decisions, the upstream leak register, and the traps register live in `docs/fork/2026-08-25-mac-only-fork-design.md`. **Read it before touching platform code.** It records which `#[cfg]` sites are load-bearing on macOS, which look like cruft and are not, and which upstream endpoints still point at the original author's repo.

**Work tracking:** GitHub issues are the backlog. File an issue for every bug
and every feature (`gh issue create`); remaining work is `gh issue list`.
Markdown is for documentation, design, runbooks, and fixtures. Do not add TODO.md, ISSUES.md,
ROADMAP.md, FIXES.md, a live findings.md queue, or any other markdown list of
open work, and do not grow `docs/fork/STATE.md` into a backlog.

**Start here for method and verification:** `docs/fork/STATE.md` records what
has landed, the verification commands and their expected output, and the
method rules this project has already paid for. Read it before planning a
pass so you do not redo finished work or repeat a falsified finding. Open
work lives on GitHub issues, not in that file.

Use [ARCHITECTURE.md](ARCHITECTURE.md) for module ownership, thread/data flow,
dependency sources, and command examples. Use [keybindings](docs/user/keybindings.md)
and [configuration runtime behavior](docs/user/configuration/runtime.md) for their
reference tables. Read [DESIGN.md](DESIGN.md) before UI changes and update it in the same PR.

The registry currently declares **79 GPUI action types**, **79 actions total**.
Update both counts when changing `app/actions.rs`; its drift test reads this file.

Settings and About are reached only from the macOS menu bar (**PaneFlow ▸
Settings…** and **About PaneFlow**). The title bar has no avatar and no
profile menu. Themes are chosen in Settings → Appearance; there is no modal
theme picker.

## Verify before claiming

Run all six, before and after any pass, and quote the actual output:

```bash
cargo build                                # exit 0
cargo test --workspace                     # diff test names against the last landing; do not trust the integer
cargo clippy --workspace --all-targets     # exit 0; compare warnings against the baseline
cargo fmt --check                          # exit 0
./target/debug/paneflow --version          # paneflow 0.7.2
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
never `cmd 2>&1 > file`. Clippy exits 0 with warnings, so inspect them and also run
`cargo clippy --workspace -- -D warnings`. Report known dependency/linker
notices separately from new warnings. `set -e` plus `grep FAILED` on a
green log is a **false fail** (grep exits 1 when it finds nothing). `rg … | head`
is a **false fail** via SIGPIPE after a successful command.

## Delegating parallel work

Use headless agents in separate git worktrees for batch fan-out. PaneFlow's
terminal panes, read-only MCP bridge, and lifecycle hooks remain available
for interactive work. See `docs/mcp-bridge.md`.

Fan-out works when the worker does **not** have to discover anything.
Give exact `file:line` + the cfg/expression as written + the action.
Disjoint file allowlists; two batches on the same file collide at
`git apply` even when the hunks are six lines apart. Kickoff task lists
are not automatically file-disjoint: check overlap before launching.

When delegating:

- One `git worktree` per batch, seeded with `cp -c -R target <wt>/target`
  (APFS clone: seconds, ~0 extra bytes, warm incremental rebuild).
- Use at most three concurrent batches on this machine.
- Agents never touch git. Orchestrator collects `git -C <wt> diff`,
  applies it, re-runs all six gates, and commits. Agent "green" is
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

Verify all prerequisites before diagnosing build failures.

1. **Rust 1.98.0**, pinned by `rust-toolchain.toml`. rustup honors the pin automatically. The dependency graph's actual floor is 1.92 (oo7 0.6, cosmic-text 0.17, smol_str 0.3, several wgpu crates), so anything older fails to build before tests can start.
2. **Full Xcode. Command Line Tools are NOT sufficient.** GPUI compiles its Metal shaders at build time, which needs the Metal compiler that ships only with Xcode.
3. **Xcode alone is still not sufficient.** Xcode 26 ships the Metal toolchain as a separate downloadable component, so `xcrun metal` fails with `cannot execute tool 'metal' due to missing Metal Toolchain` until you run:

   ```bash
   xcodebuild -downloadComponent MetalToolchain
   ```

   **Do not check this with `xcrun -f metal`.** That resolves the tool's path successfully even when the toolchain is absent, so it reports success on a machine that cannot build. Verify with an actual compile, or with `xcrun metal --version`.
4. `cmake` (Homebrew) for native dependencies.

`gpui_platform` **must** carry the `font-kit` feature on macOS (`src-app/Cargo.toml`). Without it the build succeeds, the window opens, SVG icons and cursor quads paint, and every single text glyph rasterizes as an empty box. It is declared in the default dependency table, so this only bites if someone edits the feature list.

## Commands and isolation

Run commands from the repository root. Use `cargo run -p paneflow-app` for a
debug launch and `RUST_LOG=info cargo run -p paneflow-app` for logs. Release
builds use the installed app’s namespace; isolate smoke runs with an absolute
`PANEFLOW_HOME`, `PANEFLOW_ALLOW_MULTIPLE=1`, and `PANEFLOW_SOCKET_PATH`.
Only the value `1` bypasses the singleton guard. `PANEFLOW_HOME` does not move
the socket. See [build and benchmark commands](ARCHITECTURE.md#building).

Never publish a performance claim without the terminal or startup benchmark
suite evidence in `bench/README.md`. Reject runs carrying `PANEFLOW_BENCH_WARNING`.
Keep all three GPUI git revisions in `src-app/Cargo.toml` identical and immutable;
keep `gpui_platform`’s macOS `font-kit` feature. Do not restore the removed Markdown
widget dependency graph or the `arthjean/zed` pin.

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

## GPUI patterns

- **Entity/Context model**: all mutable state lives in `Entity<T>`, mutated via `Context<Self>`. Use `cx.new()` to create, `cx.notify()` to trigger repaint, `cx.spawn()` for async tasks.
- **`actions!` macro** (`app/actions.rs`): generates zero-sized typed action structs in the `paneflow` namespace. Actions are dispatched through GPUI's focus chain.
- **`Render` trait**: implement for high-level views (PaneFlowApp, TitleBar, TerminalView). Returns a div element tree.
- **`Element` trait**: implement for low-level custom rendering (terminal and diff elements). Has 3 phases: `request_layout()` → `prepaint()` → `paint()`.
- **Focus**: each `TerminalView` owns a `FocusHandle`. Key context `"Terminal"` scopes terminal-only keybindings; other contexts are `Search` and `DiffView`. There is no in-app code editor and no in-app Markdown viewer. A clicked file path, including `.md`, opens in the external editor. Focus navigation is structural (layout-tree traversal), not spatial.
- **No `Arc`/`Mutex` for UI state**: use `Rc<Cell<f32>>` for single-threaded shared state (e.g. split ratios in render closures).

## GPUI scroll & wheel (gotchas)

Hard-won from the diff-dock horizontal-scroll saga; the surviving two-axis host is `src-app/src/diff/view/render.rs`. Verified against the Zed source. Do NOT re-derive these by guessing, it cost three wrong attempts.

- **Shift+wheel is axis-swapped to X at the platform layer**, before app code ever sees it. On macOS the NSEvent delivers the horizontal component natively; the other platform backends do the swap explicitly. Either way the value lands in `delta.x` with `delta.y` zeroed. So: read `delta.x` for horizontal, NEVER branch on `modifiers.shift` (reading `delta.y` under Shift reads zero). The `div.rs` `delta_x = delta.y` line is a separate fallback (fires only when `delta.x == 0`), not the Shift mechanism.
- **`overflow_hidden()` + `track_scroll()` does NOT scroll-translate children.** It only keeps the handle's bookkeeping (`offset()`/`bounds()`/`max_offset()`) live. GPUI only pushes the scroll offset onto the element-offset stack (which bakes into each child's `bounds.origin`) when the host overflow axis is `Overflow::Scroll`. A custom `Element` that positions content off its own `bounds.origin` (e.g. `DiffElement`) therefore only scrolls under `overflow_y_scroll`/`overflow_scroll`; `set_offset()` under `overflow_hidden` is stored but dead. Custom elements get the shift automatically via their passed `bounds` (no `window.element_offset()` call needed).
- **Two-axis recipe (vertical list whose items also scroll horizontally)**, the canonical Zed pattern (`data_table.rs`, `thread_view.rs`, `markdown.rs`): host = `overflow_y_scroll()` + `track_scroll(&handle)` + `element.style().restrict_scroll_to_axis = Some(true)`. The flag is a raw `StyleRefinement` mutation (no builder method, but it compiles: non-`#[refineable]` `Style` fields still become `Option<T>`). It stops a vertical wheel bleeding into a horizontal child AND stops the native Y handler back-filling `delta_y = delta.x` under Shift+wheel (the "vertical scrolls when I Shift+wheel" bug). Per-item horizontal stays custom (an `on_scroll_wheel` reading `delta.x` only); native owns vertical.

## Split / layout system (`layout/`)

The old binary `SplitNode` in `split.rs` is gone. `LayoutTree` (`layout/tree.rs`) is an N-ary tree:

- `LayoutTree::Leaf(Entity<Pane>)` | `LayoutTree::Container { direction, children: Vec<LayoutChild>, drag, container_size }`
- Each `LayoutChild` carries `node` plus `ratio: Rc<Cell<f32>>`.
- `SplitDirection::Horizontal` = **horizontal divider, panes stacked top/bottom** (`flex_col`). `Vertical` = panes side by side (`flex_row`). Counterintuitive but consistent throughout the codebase.
- Layout uses GPUI flex divs with `flex_basis(relative(ratio))`. `MIN_PANE_SIZE = 80.0`, `DIVIDER_PX = 8.0`, `DIVIDER_HIT_PX = 7.0` (`layout/tree.rs`): the divider is an unpainted shell-revealing gap with a narrower resize hitband centered inside it.
- `MAX_PANES = 32` (`layout/mod.rs`), `MAX_WORKSPACES = 32` (`workspace/mod.rs`). Both are enforced on the live create path *and* at session restore and config load; `limits.rs` documents the read/write cap pairs.
- Drag-to-resize is pixel-accurate: `Container::container_size` captures the real main-axis pixel size each frame via a `canvas()` prepaint, so there is no hardcoded container estimate (the old `split.rs` 800px guess is gone).
- Presets in `layout/presets.rs`: `from_panes_equal` (even horizontal / even vertical), `main_vertical`, `tiled`.

## Reference changes

Keep the IPC capabilities and [scripting reference](docs/user/scripting/reference.md)
in sync. Update the action registry, default-key table, and [keybinding reference](docs/user/keybindings.md)
together. Configuration changes must update the Rust schema, published JSON Schema,
and [schema documentation](docs/user/configuration/schema.md) in the same PR.
Follow DESIGN.md for styling, accessibility, and visual verification.

## Styling changes

Follow DESIGN.md and match the existing inline GPUI builder style. Use theme
and `ui_colors()` roles rather than introducing a separate styling layer.
Keep font measurement tied to the selected face and preserve embedded defaults.
Detailed styling conventions are in ARCHITECTURE.md.

## Gotchas

- Keep GPUI as an immutable Zed git dependency; it is not published on crates.io. Do not replace it with a local path. Never recommend iced for this project.
- `SplitDirection::Horizontal` means a horizontal divider, with panes stacked top/bottom.
- Keep engine types behind `terminal/`. Rendering/input consume the neutral types in `terminal/types.rs`; session code translates the engine values. Preserve dependency-source and engine-absence tests.
- Keep `dirs` as a single workspace dependency. Validate configured shells as present/executable, then fall back through `$SHELL` to `/bin/sh`.
- Import only `PATH` from the login shell. Importing session variables from a GUI user's shell profile corrupts inherited agent identity.
- Treat `US-NNN`, `EP-NNN`, and `prd-*.md` source comments as historical breadcrumbs to uncommitted upstream plans. Do not invent or add those identifiers.
- Check complementary cfg definitions before removing a gate: enabling both halves can create duplicate definitions.
- Measure embedded helpers as Mach-O with the `release-min` profile. Respect `EMBED_SIZE_LIMIT_BYTES` in `src-app/build.rs`; do not reuse Linux ELF size budgets.
- Keep GPL-3.0-or-later packaging metadata consistent with `LICENSE` and Cargo manifests.
- Convert libproc CPU Mach ticks with `mach_timebase_info`; raw `Duration::from_nanos` gives incorrect CPU measurements.
- A DMG smoke built from an unsigned binary can fail strict codesign verification after creating the image. That is not signed-release verification; never weaken the release check.
- Verify behavior in code rather than restoring Windows/Linux paths mentioned by historical comments. Ghostty code is live and must stay.
- **Upstream tags collide with fork releases.** Never fetch upstream tags into the bare `vX.Y.Z` namespace. Keep `remote.upstream.tagOpt = --no-tags`. A bare release tag belongs to this fork only if it is on `origin`; before every tag push verify `git rev-parse <tag>^{commit}` equals `HEAD`. A `[new tag]` push message does not establish tag ownership.
- **Never bind `secondary-tab`.** It is Cmd+Tab and macOS consumes it. Next workspace uses `ctrl-tab`; preserve the regression test.

### Close-guard contract

The contract lives in `TerminalState::Drop` (`terminal/pty_session.rs`): pin every live process group in the PTY session through an **app-owned `dup()` of the master** (`SpawnedGhostty::master_fd`, taken at spawn), SIGTERM them synchronously, drop the external guards, then `GhosttySession::shutdown()`, close the dup, and SIGKILL 100 ms later with start-time pins re-checked. **The runtime thread never signals**: on an app-initiated shutdown or a natural exit it only reaps (`reap_child_bounded`); `terminate_child` survives solely for the engine-failure paths (runtime failed, `waitid` failed, the startup and panic guards). `dropping_the_state_kills_background_and_stopped_jobs_in_the_pty_session` pins the outcome with a live shell.

## MCP and crash reporting

Keep the MCP bridge GPU-free and read-only (`list_panes`, `read_pane`, `search_pane`).
The embedded bridge installs through `paneflow mcp install` or **Settings → MCP Servers**,
with status/repair handled off the render thread. Preserve idempotent, no-clobber,
backup-and-atomic-write behavior. See `docs/mcp-bridge.md`.

Sentry initializes only on GUI launch when `crash_reporting` is enabled. Preserve
`send_default_pii(false)`, the fixed server name, home-path redaction, and the
CLI early-exit boundary. The setting is read at startup; document restart behavior.

## Commit convention

Use `type(scope): description`, one atomic logical change per commit. Use `(fork)`
for divergence from upstream and cite the GitHub issue. Do not add historical story
IDs. Use a feature branch; follow the shared PR and merge workflow in
`AGENTS.md`. This is a private fork with no public advisory or contribution process.

## Platform (macOS only)

This fork targets macOS on Apple Silicon and nothing else. Metal, AppKit, libghostty-vt (vendored `aarch64-apple-darwin` archive), Unix-socket IPC, signed and notarized `.app` / `.dmg`.

- Do not add Linux or Windows code paths back. No `#[cfg(target_os = "linux")]`, no `#[cfg(windows)]`, and no backend selector: libghostty-vt is the one terminal engine; `src-app/build.rs` refuses any target but `aarch64-apple-darwin`.
- **`#[cfg(unix)]` is not Linux-only.** macOS needs the Unix-shared paths - it is the single highest-risk distinction in this codebase. Do not prune unix-shared code because Linux code sat beside it. `#[cfg(target_os = "macos")]` also selects live code. Both are live arms and both stay. Re-run `./scripts/linux-census.sh` before quoting counts; its `cfg(unix)` / `cfg(macos)` rows are the live-site negative control.
- **Only the Unix and macOS platform predicates are allowed.** No `target_os = "linux"`, no `not(unix)`, no `not(target_os = "macos")`, no `windows`. A `[target.'cfg(target_os = "macos")'.dependencies]` table **is** allowed and exists (`src-app/Cargo.toml`, `libproc` / `core-text` / AppKit). `./scripts/linux-census.sh` enforces the zero-condition: it exits 1 with a `FAIL:` line when the STAGE 2c total is non-zero or the negative control reads 0, and `run_tests.yml::platform_census` runs it (and `win-census.sh`) on every push and PR. It prints the `cfg(unix)`/`cfg(macos)` counts first as a negative control, because a census reading 0 with a broken regex looks exactly like one reading 0 because the work is done. A zero cfg census is also blind to ungated Windows strings (`powershell` / `.exe` / `.cmd` / `.bat` / `.ps1` / `\\?\` / `%APPDATA%`); that class is a separate reported check in the same script (issue #103) and is **not** part of the STAGE 2c integer.
- `#[cfg(all(unix, not(test)))]` still appears (in `terminal/pty_session.rs`). That is a test-isolation gate, not a platform gate. Leave it.
- Still use `std::path::PathBuf`, `std::env`, and `dirs` for filesystem and environment access. macOS-correct is not the same as hardcoded.
- **The old updater stays deleted; Sparkle 2 owns self-update.** Never recreate `src-app/src/update/`, minisign, an update prompt, or a forced relaunch. `src-app/src/sparkle.rs` dynamically loads the bundled framework, checks the AAM GitHub appcast hourly, downloads in the background, and holds installation until ordinary app termination. Packaging pins and checksum-verifies Sparkle in `scripts/sparkle-dist.sh`; release signing adds EdDSA (`SPARKLE_PRIVATE_KEY`) on top of Developer ID + notarization. Do not create `GPG_*`, `AZURE_*`, `POSTHOG_API_KEY`, `MINISIGN_SECRET_KEY`, or `PANEFLOW_MINISIGN_*`.

The full removal plan, with the paired edits that have to land together, is in `docs/fork/2026-08-25-mac-only-fork-design.md`.
