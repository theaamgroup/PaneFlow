# PaneFlow fork: current state

Living handoff record. Updated 2026-09-04, through the deep-review sweep of
that day (PRs #372 and #373, issues #357-#371) and the 0.3.1 cut. The prior
entry covered #341 (upstream v0.11.0 adopted: the `PublishGate`, per-tab
worktree binding, the Customize Sidebar menu, the pull-request marker, and
the 0.3.0 cut); before that, #184 Phase 4 and its audit plus the About /
app-menu GitHub links (#226, #227, #228).

**2026-09-04 deep review: 33 findings landed, 0.3.1 cut.** A `/deep-review`
pass at `e5b01e4` over ten lenses found 36 problems and split them: 18 it
fixed in place, 18 it referred for a human decision. Sixteen of the latter
became issues #356-#371. Both halves are now on `main`.

- **PR #372 (`bdd21e37`)** fixed 15 of those issues, one commit each, each
  with its own test. The thread they share is work running where it must
  not: `canonicalize_workspace_cwd` and the `surface.read` / `surface.search`
  runtime waits both left the GPUI automation tick (#358, #363; the latter
  answers through the request's own channel and returns a deferral marker
  `process_ipc_requests` recognises), and `GitDiffStats::from_cwd` now
  threads one budget through its three git probes instead of stacking three
  10 s deadlines (#365). Three more fixed a stale or unresolved on-disk
  identity: the editor's conflict stamp now comes from the handle the bytes
  were read through (#357), `session.json` publishes onto a symlink's target
  instead of replacing the link (#360), and a zero `st_birthtime` is refused
  rather than collapsing worktree identity to inode reuse (#371). The rest:
  a global cap on concurrent `gh` lookups (#359), `surface.search` returning
  a JSON-RPC error where it used to return `truncated=true` (#362),
  keyboard- and AT-reachable Settings combo boxes (#361), shaped IME caret
  geometry and macOS word chords in `TextArea` (#364, #370), a UTF-8 locale
  the child actually gets (#369), case-folded `confine_cwd` (#366), and hook
  cleanup that reports a failed delete (#367, #368).
- **PR #373 (`08a00a09`)** committed the 18 auto-fixed findings, which had
  been sitting uncommitted in one working tree and would have been lost with
  it. Seven commits grouped by subsystem: hook merges that refuse a hostile
  config shape instead of overwriting it, Pi status frames as JSON-RPC
  notifications, honest `send_keystroke` and agent-panel write failures,
  bounded session sweeps and walks, `wait` no longer reading every `-32602`
  as a closed pane, Sentry's `server_name` no longer carrying the hostname,
  and a tilde-expanded `default_shell`.
- **Five bugs the reviews of those PRs found** are fixed on top, three of
  them in code #372 had already landed. `is_utf8_locale` required a dot, so
  Terminal.app's codeset-only `LC_CTYPE=UTF-8` read as non-UTF-8 and the
  override forced `LC_ALL` over the user's other categories.
  `confine_cwd` case-folded on every volume, so on a case-sensitive one a
  sibling checkout differing only by case counted as inside; it now asks
  `pathconf(_PC_CASE_SENSITIVE)` and fails closed when the path cannot be
  probed. And a no-pid `ai.*` frame minted a fresh synthetic session row
  each time once two rows of that tool existed - each row it made left the
  next frame ambiguous too, and nothing collected them, because the stale
  sweep only probes real PIDs; the band now holds one row per tool.
- **Release `v0.3.1`** is on `a6e7043f`, Developer ID signed, notarized and
  stapled, verified from the downloaded artifact (`spctl` reports
  `source=Notarized Developer ID`, `stapler validate` passes, the `.sha256`
  matches). The `release` environment gates both jobs of `release.yml`, so a
  tag push waits for an approval before signing and again before publishing.
- **The fork's tags collide with upstream's.** `v0.3.1` already existed
  locally as `arthjean/paneflow`'s tag on an unrelated commit, and the
  fork's own `v0.3.0` on `origin` names a different commit from upstream's
  `v0.3.0`. Cutting a release means deleting the local upstream ref first
  (`git fetch upstream --tags` restores it). Unresolved: a fork tag prefix
  would end it but changes a naming contract `release.yml`, the Sparkle
  appcast, and the runbook all share.

**2026-09-04 #341: upstream v0.11.0 adopted.** Nine code sub-issues landed
one PR each (#342 through #350), then the 0.3.0 cut. What is now true, and
where the evidence is:

- **`PublishGate`** (#342, #343; `src-app/src/terminal/ghostty_session.rs`).
  The runtime thread holds a frame while DEC 2026 synchronized output is set
  (one FFI mode query per wake, `DisplayTerminal::synchronized_output` in
  `crates/paneflow-terminal-ghostty/src/engine.rs`, `SYNC_OUTPUT_MAX_HOLD`
  150 ms so a program that opens a bracket and dies cannot freeze the pane)
  or while the previous publish is under `MIN_PUBLISH_INTERVAL` (8 ms;
  deferred through `next_wake`, never dropped). Resize, scroll, scrollback
  clear, reset, select-all, a command mark, the first frame, and the frame
  before `ChildExited` bypass it (`publish_now`); a `Wakeup` is queued only
  for a frame that was published, and only the engine's dirty rows are
  converted (`CellMirror`). Pinned by
  `a_trickle_publishes_once_per_interval`,
  `synchronized_output_holds_a_frame_the_rate_limit_would_have_allowed`,
  `a_synchronized_output_hold_expires_so_a_stalled_program_cannot_freeze_the_pane`,
  `a_synchronized_redraw_is_not_published_until_the_bracket_closes_or_the_hold_expires`,
  and `a_synchronized_output_hold_parks_the_loop_instead_of_spinning_it`.
- **Cursor blink out of the layout memo, shared `FontSettings`** (#344;
  `terminal/element/font.rs`, `LayoutCacheKey` in `terminal/element/mod.rs`,
  `layout_cache_key_ignores_the_blink_phase`) and the **memoized per-cell
  contrast** (#345; `contrast_cache_get_or_insert` in
  `terminal/element/color.rs`,
  `the_contrast_cache_matches_the_uncached_search_across_the_bundled_themes`).
- **Reproducible terminal benchmark** (#346) re-baselined on this machine:
  `bench/baseline.json` and `bench/results/` hold only fork-measured JSON.
- **Per-tab worktree binding** (#347). `Tab::worktree`
  (`src-app/src/workspace/tab.rs`), `bind_tab_to_branch` in
  `src-app/src/app/tab_worktree.rs` (`prepare_branch_checkout` runs
  off-thread and owns nothing: a tab checkout is marker-less, never
  managed). Session carries it as `TabSession.worktree` (additive; a
  missing key restores unbound: `a_tab_bound_to_a_missing_worktree_restores_unbound`,
  `tab_worktree_needs_no_schema_bump`,
  `every_picked_checkout_passes_through_the_binding_gate`).
- **Remove worktree** (#348, commit `d1f79614`). A row on the tab context
  menu (`sidebar/context_menu.rs`, `tab-context-remove-worktree`, present
  only for a bound tab) into `remove_tab_worktree`. Ownership is option B:
  `is_paneflow_worktree_dir` (`workspace/worktree.rs`) decides, there is no
  owner marker. Four refusals (`removal_refusal` + the dirty check in
  `remove_checkout`): open as a workspace, managed by workspace teardown,
  not created by PaneFlow, uncommitted changes. The branch is never
  deleted. Pinned by
  `removal_is_refused_for_what_is_not_ours_open_or_reserved` and
  `a_clean_owned_checkout_is_removed_keeping_its_branch_and_a_dirty_one_is_refused`.
- **Customize Sidebar** (#349, commit `36ca19ad`;
  `sidebar/customize_menu.rs`). A "Show" submenu over `sidebar_show`
  (`crates/paneflow-config/src/schema/config.rs::SidebarShow`; defaults
  `branch` true, `diffstat` / `pr` / `indent_guide` false, so a config with
  no key renders as before) plus Expand all / Collapse all; every flip
  writes the whole object through `config_writer`
  (`every_flip_writes_the_whole_sidebar_show_object`,
  `the_customize_menu_offers_no_settings_affordance`). Per-workspace fold
  state persists as `WorkspaceSession.sidebar_collapsed`.
- **Pull-request marker** (#350, commit `e7d1a6b6`;
  `src-app/src/app/pull_request.rs`). `gh pr list --json
  number,state,isDraft` behind `sidebar_show.pr`, off by default, cached
  with a TTL, one failed repository blacklisted on its own; `gh` is never
  spawned while the switch is off
  (`the_switch_gates_the_lookup_before_gh_is_consulted`,
  `a_failed_lookup_blacklists_only_that_repository`).
- **Session schema stays v2** (`SESSION_SCHEMA_VERSION` in
  `crates/paneflow-config/src/schema/session.rs`): `TabSession.worktree`
  and `WorkspaceSession.sidebar_collapsed` are additive, written only when
  set. The "Terminal shell resolved" log line rode along
  (`terminal/pty_session.rs`).
- **Verified SKIP list**, not ported: the Windows shell work,
  `timeBeginPeriod`, the verbatim prefix, libghostty CI automation, and
  Fedora / Discord / CHANGELOG / AppStream.
- **0.3.0 cut**: `Cargo.toml` workspace version, every `paneflow-*` entry
  in `Cargo.lock`, the `--version` gates in `CLAUDE.md` and `INSTALL.md`.

**2026-09-01 #226 #227 #228: About and PaneFlow-menu GitHub links.** About
shows a **View on GitHub** button (`https://github.com/theaamgroup/paneflow`)
between Version and copyright, and an original-credit plate that opens
`https://github.com/arthjean/paneflow`. The inert contributors placeholder is
gone; the David Ayers CRT plate and AAM copyright stay. The PaneFlow menu is
About / Settings… / separator / Report an Issue / separator / Quit; Report an
Issue opens `https://github.com/theaamgroup/paneflow/issues/new` (`ReportIssue`,
menu-only). Pinned by `about_dialog_links_this_forks_github`,
`about_dialog_credits_the_original_project`,
`paneflow_menu_report_an_issue_is_wired_at_the_root_and_as_a_fallback`, and
`report_issue_stays_out_of_the_shortcut_registry`.

**2026-08-31 #184: libghostty-vt is the only engine.** Landed as a stack of
PRs (#185 toolchain 1.98.0 + chrome; #186 vendored crates + darwin archive;
the session-host PR on top). What is now true, and where the evidence is:

- `src-app` has no `alacritty_terminal` and no `polling`; every pane runs
  on `paneflow-terminal-ghostty` over `portable-pty`. `alacritty_is_absent_from_the_app_crate`
  guards the word itself. `terminal.backend` is gone from the schema; a
  leftover key is ignored (`leftover_terminal_backend_key_is_ignored`).
- The fork's teardown contract survived the swap and is stronger than
  upstream's: `TerminalState::Drop` pins every process group in the PTY
  session through an app-owned master dup, SIGTERMs, drops the guards,
  shuts the runtime down, closes the dup, SIGKILLs at 100 ms; the runtime
  thread reaps, signalling only on its own failure paths (runtime failed,
  waitid failed, startup panic), never on a user close. Pinned by
  `dropping_the_state_kills_background_and_stopped_jobs_in_the_pty_session`
  (real `/bin/sh`, background + stopped `sleep`) and, live, by the
  parent-death smoke (`kill -9` the GUI: sleep reaped, every
  `__paneflow-pty-guard` exits). All 28 survival tests named on #184 are
  present by name.
- Test names 2378 -> 2371 across the swap, every removed name accounted
  for by class on the PR (engine internals, `From<Alac*>` maps, the deleted
  `mouse.rs` encoder, inverted presence assertions, module moves); the
  fork's behaviour tests were re-created on the Ghostty display-only
  session under the same names.
- Rides along from upstream: Kitty graphics, OSC 9/777 notifications, the
  OSC 9;4 header chip, `TERM_PROGRAM=ghostty`.

**Traps this pass hit, all worth carrying:**

- **A synthetic chord does not drive a GPUI action.** `CGEventPostToPid`
  delivers printable keys into a pane's PTY (the smoke proved `a` landed),
  but `Cmd+Shift+W` posted the same way never closed the pane (the AX
  `frontmost` raise is a documented no-op on macOS 26, so the window was
  never key), and the IPC `workspace.close` hands the decision
  to a modal no keystroke can reach without a window focus move. Live
  checks of close paths therefore go through in-suite tests with real
  shells; only the parent-death path smokes live. The earlier memory note
  that "chords DO fire" was about the app receiving them, not acting.
- **`grep` in the agent's tool shell is not `/usr/bin/grep`.** It is a
  function routing to `ugrep --ignore-files`, which skips `target/`; the
  census scripts under `bash` crawled 19 GB with BSD grep and took 27 min
  cold. `--exclude-dir=target --exclude-dir=.git` (commit `9c86912c`)
  made both scripts a 2 s gate with byte-identical reports.
- **`pull_request: branches: [main]` starves a stacked PR of CI.** The
  filter is gone (`4c512480`), so each phase's "Done when" can be checked
  on CI before the next phase starts.
- **Upstream's `tests/display_terminal.rs` had no macOS arm.** Its 15
  integration tests had never run on this platform; enabled, all pass.
- **The vendored archive links with `ld: duplicate symbol '_memset'`**
  (`compiler_rt.o` vs `libghostty-vt-static_zcu.o`) and carries
  `minos 13.0`. Benign (ld64 keeps the first; `Info.plist` already floors
  at 13.0), a property of the archive. Not locally fixable: the rebuild
  needs a Linux host, Zig 0.16.0 and upstream's
  `scripts/build-libghostty-macos.sh`, which this fork does not vendor, so
  it rides the next `source_sha` bump. Issue #194 closed 2026-09-02.

**2026-08-28 deep review.** Five parallel agents (correctness, security,
architecture, performance, reality-check) swept the whole repo. The findings
are archived as a historical record in
`docs/fork/2026-08-28-deep-review-findings.md` (as of `29ac597a`); it is not
a backlog and quotes no live open count. Open work is `gh issue list`, and
`src-app/tests/fork_docs_backlog_policy.rs` fails if either document regains
the work-queue role (issue #224). Twelve landed in that pass, all gates
green, test list name-diffed 2075 -> 2079 with zero removed:

- `Tab::pane_count` ignored `saved_layout`, so a pane added while zoomed was
  destroyed by `exit_zoom` along with its PTY and agent.
- IPC coerced a malformed `index` to the ACTIVE workspace and reported
  success - a quoting mistake closed the wrong workspace.
- Opening the find bar replaced the whole `Terminal` key context (GPUI's
  `key_context` assigns, it does not merge), killing all 15 Terminal
  bindings including the `Ctrl+Shift+F` that opened it.
- `sync_attention`'s `.flatten()` erased the `Some(None)` "waiting, no
  message" case, so a blocked agent painted no ring, dot or badge.
- The Settings shortcut recorder wrote macOS HIG glyphs (`to_string()`
  yields `⌘⇧D`), which nothing validates - so every rebind registered an
  unproducible chord AND dropped the real default. Mutation-verified.
- Copy/paste had no focus guard, so Edit > Paste executed the clipboard in
  the shell while the find bar was focused.

**2026-08-28 UI cluster (six items, from a single issue list).** Landed on
top of the deep review, in three file-disjoint batches:

- **About dialog** (`app/about_dialog.rs`) grew a retro CRT credit plate
  (`> made with ❤ by david ayers`) and `© 2026 AAM USA, Inc. All rights reserved.`;
  the dialog's own "Paneflow" spellings were corrected to "PaneFlow".
  `VT323-Regular.ttf` + its OFL are in `src-app/assets/fonts/` and need no
  code change to ship (`Assets::load_fonts` iterates the embed registry).
  The inert contributors placeholder from that cluster was replaced later
  (#227); current About chrome is under **2026-09-01 #226 #227 #228** above.
- **Inactive-workspace dim** reaches sidebar tab rows, on a *second* axis from
  the existing idle tone. Foreground only, one shared quiet step
  (`IDLE_WORKSPACE_TEXT_OPACITY`), guarded by a source-scraping test.
- **Inline rename opens selected.** `rename_seeded` on `PaneFlowApp`; the
  first printable key or backspace spends the whole seeded value.
- **A tab label is now DERIVED**, never stored: manual `Tab::title`, else the
  first pane's `Pane::surface_title` (which already ranks custom name > OSC >
  agent name), else `Tab N`. Nothing is written to `session.json`.
- **The sidebar footer gear is gone**, finishing issue #105. Settings is the
  menu bar and the profile menu, and nothing else.
- **`View ▸ Themes…` is gone** (issue #120). The View menu had only that
  item, so the menu went with it. Theme selection is Settings → Appearance
  plus the title-bar profile menu; the menu bar is PaneFlow / Edit / Window /
  Help.
- **`review_enabled`** (default `true`) gates the Review surface; off, the
  whole mode strip stops rendering.

**Three traps this cluster hit, all worth carrying:**

- **GPUI at this rev has `linear_gradient`, inset `BoxShadow` and
  `border_dashed`, but NO `text_shadow`.** A neon *text* halo is not
  reproducible; glow must come from the box. Do not try to fake it with
  layered offset text.
- **A shared blink phase does not blink an idle window.** Only `TerminalView`
  observes `BlinkPhaseGlobal`, so a caret painted off it freezes in its
  last-painted state - half the time hidden. `PaneFlowApp` now observes it too
  but repaints ONLY while the About dialog is open; an unconditional notify
  would wake the whole app every 530 ms for a decoration nobody is watching.
- **Deriving a display label breaks any equality check written against the
  stored one.** `tab_rename_should_persist` used to rebuild `"Tab N"` from
  `Tab::title`; once the label could also come from a pane, that
  reconstruction stopped matching what the editor was seeded with, and
  pressing Enter on an untouched agent-derived label would have frozen it into
  `Tab::title`. It now takes the displayed string from its caller.

**Three method rules this review paid for:**

- **A doc claim is a liability, not a record.** The review falsified
  `PANEFLOW_ALLOW_MULTIPLE` (documented presence-gated; it is value-gated
  and was fixed in `1cfee6c7`), issue #39 (documented open; fixed with a
  regression test), the census counts, the GPUI remote in `AGENTS.md`, and
  `.mcp.json`'s existence. Each traces to a doc that recorded a BUG and was
  never updated when the bug was fixed. Closing an issue must include
  grepping `CLAUDE.md` / `STATE.md` / `AGENTS.md` for its number.
- **Verify the agent, not just the code.** Two agent-reported site counts
  were wrong (the IPC index pattern has 2 live sites, not 5), and one of my
  own mutation checks was invalid (`--lib` on a binary crate exits 101 for
  the wrong reason). Re-read the source before acting on any finding.
- **A test that pins a property is not a test that pins the call site.** The
  first shortcut test asserted `unparse()` round-trips - true regardless of
  what production called. Extracting `recorded_shortcut_key()` made the call
  site itself testable without a `Window`.

Companion documents:
- `docs/fork/2026-08-25-post-2c-plan.md` is the **historical 2026-08-25
  execution plan** (schema, telemetry, identity, CI). Leftover-removal
  buckets 1–4 (2026-08-26) superseded its self-update “disable the feed”
  decision: the old hand-rolled updater was deleted. GitHub issue #13's bundle-id and
  Notifications smoke was completed on the installed v0.1.1 app on 2026-08-28.
- `docs/fork/2026-08-25-mac-only-fork-design.md` holds the **decisions**, the
  **leak register**, and a **16-item traps register**. Read it before touching
  platform code or the config schema. Sparkle owns self-update; the old updater
  and minisign remain gone.
- `CLAUDE.md` holds build prerequisites, the module tree, and the commands.

This file holds only: where the work stands, what is next, and the rules the
session learned the hard way.

v0.9.0 adoption of issues **#87–#103** is on `main` and pushed. The 2026-08-27
post-adoption pass closed #11, #14, #15 (evidence on the issues), landed the
CI/test hygiene set #66–#71, and triaged #73–#85 against 925e21ce (comments
on each; #75 closed as superseded; #74/#77/#80/#82 retitled to their
residuals). Remaining standing visual/runtime checks are in
`MANUAL-CHECKLIST.md`.

The 2026-08-27 keyboard-correctness cluster closed **#108**, **#79** and the
focus-routing half of **#78** across five commits
(`ea09225c`, `2f5a1f00`, `66b2ca0b`, `bb938ae6`, `30809b70`). Test count
1900 -> 1913, name-diffed at every landing; all six gates green on each.
All three were re-verified as live defects in code before any work started,
and both `CHANGES REQUESTED` review loops (#108, #79) were closed.

The 2026-08-27 tab-close cluster closed **#83** across nine commits
(`508f7904`, `79d6b821`, `2ac394ff`, `028d47ab`, `1b6bfc47`, `ae0b800f`,
`6d20ce0e`, `dacce668`, `b6b70729`). Closing a tab running an agent now asks
first, and every user-initiated tab or pane close is undoable with
`Cmd+Shift+T`. Test count 1913 -> 1975, name-diffed at every landing; all six
gates green on each.

Three findings in that cluster are worth carrying forward, because each was a
*correct-looking* design that a review falsified:

- **Arm-then-confirm without a debounce is not a confirmation.** A double-click
  on an inline `x` fired both the arm and the confirm, so the process group died
  behind a state painted for one frame. GPUI dispatches every click listener on
  each mouse-up, and this repo already relies on that (`sidebar/mod.rs`
  double-click-to-rename). Any future arm-then-confirm needs a settle delay;
  `ARM_SETTLE` in `app/close_guard.rs` is the one to copy.
- **"Put the record back on any refusal" bricks an undo stack.**
  `handle_undo_close_pane` pops NEWEST and `push_closed_record` appends to
  NEWEST, and `next_workspace_id` is a monotonic `fetch_add` - so re-pushing a
  record whose workspace is gone re-promotes it forever and buries every record
  beneath it. Re-push only on TRANSIENT refusals; drop on permanent ones.
- **`close_workspace_tab`'s re-focus is gated on `ws_idx == self.active_idx`.**
  A background workspace's tab row is right-clickable, so any `Window`-holding
  path that closes a background tab must hand focus back itself or it strands
  the window - the issue #108 class again.

Workspace-scope close now shares the live-agent confirmation guard across
`Cmd+Shift+Q`, the sidebar folder row, the workspace context menu, and IPC.
It captures a whole-workspace undo record before removal; managed-worktree
ownership follows that record until restore, FIFO eviction, or final quit.
Cached Review and mounted/parked Diff Dock terminals are included in the same
guard and are dropped with their workspace even while their UI is unmounted.
Managed-worktree retirement is journaled before cleanup, reserves its paths
against every workspace/pane ingress, and recovers the exact marker-unlink
crash window only when the persisted macOS directory identity still matches,
so a same-path/same-branch replacement cannot inherit teardown ownership.
Terminal teardown pins every authenticated process group in the PTY session,
including stopped/background groups, for both orderly close and parent death.

Issue **#110** is implemented: the window-level focus-loss fallback commits or
cancels transient editing state even when the focused row unmounts or a
Window-less navigation path runs. It no longer belongs in the follow-up list.

Three things that pass gates but are **not** end-to-end verified:

- **No live GUI smoke was possible.** Plain keystrokes deliver via
  `CGEventPostToPid`, but modified chords never fire an action, and
  `NSRunningApplication.activate` is blocked on macOS 26 (`AXFrontmost`
  works instead). Details and the working recipe are in the
  `paneflow-live-keystroke-smoke` memory and in issue #109's
  "Verifying this" section. Rename and the zero-pane chords still want one
  human pass before release.
- **`F2` as a rename-START gesture is structurally dead** and documented as
  such at both sites: a sidebar row only joins the dispatch path once its
  rename is already live, but the `f2` branch only runs when it is not.
  Fixing it needs per-row focus handles.
- **Sparkle's full installed-update path needs two consecutive signed tags.**
  The bundled framework loads and starts in a live GUI, but the first
  Sparkle-enabled release must still prove `/Applications` vN staging vN+1,
  silent install on quit from the DMG, and deliberate Developer-ID mismatch
  rejection. The release runbook carries that first-release gate.

## Identity

| | |
|---|---|
| Local clone | `~/Github/paneflow` (directory still carries the upstream name) |
| Branch | **`main`**. Reconciled 2026-08-25: the fork point is tagged `upstream-fork-point`, `main` was fast-forwarded to the fork work (strict ancestor, no rewrite), and `mac-only-fork` remains at the same commit - delete it after the first release tag. |
| `origin` | `github.com/theaamgroup/paneflow` (public since 2026-08-30 so Sparkle can fetch release assets anonymously). Renamed from `panescli` on 2026-08-25 when the PanesCLI rebrand was dropped; GitHub keeps redirects. |
| `upstream` | `github.com/arthjean/paneflow` (read-only, kept for cherry-picks) |
| Fork point | v0.8.2, commit `f53f982291f75a9daf565827b3167d0e96925d0a` |
| gpui backup | `github.com/theaamgroup/zed`, holds pinned rev `3aaba57b`. `Cargo.toml` points at **`zed-industries/zed`** rev `fecc3273`; do not reintroduce an `arthjean/zed` pin (CLAUDE.md forbids it). |
| License | GPL-3.0-or-later. Keep `LICENSE` and the single attribution line in `README.md`. |

## Naming, confirmed and locked

The product stays **PaneFlow**. The 2d rename to PanesCLI was scoped and
dropped; see `docs/fork/2026-08-25-post-2c-plan.md`.

| Thing | Value |
|---|---|
| Product | PaneFlow |
| Bundle id | `com.theaamgroup.paneflow` (task 12 replaced upstream's `io.github.arthurdev44.paneflow`) |
| Binary and CLI | `paneflow` |
| Config dir | `~/Library/Application Support/paneflow/` |
| Debug config dir | `paneflow-dev` |
| Env prefix | `PANEFLOW_*` |
| MCP server | `paneflow` |
| Conductor skill | `skills/paneflow-conductor/` |

The debug sibling is not optional. `APP_SUBDIR` in
`crates/paneflow-config/src/loader.rs:17` switches to `paneflow-dev` under
`debug_assertions` across config, session, threads, sockets and caches, so a
`cargo run` build never reads the release config path. Building from source is
still the main development workflow, so this is a likely source of confusion
even though signed release DMGs are also available.

## Stage status

| Stage | State |
|---|---|
| 0. GitHub plumbing | **Done.** Repo created privately, made public for Sparkle on 2026-08-30 after a clean 1,399-commit gitleaks scan, both branches pushed, zed backup forked and its pinned rev verified present. |
| 1. File-level deletion | **Done.** Non-macOS packaging, the two upstream-publishing workflows, non-macOS docs and scripts and assets, and upstream's project-management cruft. |
| Docs correctness pass | **Done.** 36 files, +2124/-2355. Turned out to be more a correctness fix than a platform strip. |
| 2a. Ghostty removal | **Done.** Roughly 11,600 lines. 338 stale cfg sites reduced to zero. |
| #184 Phase 1: libghostty returns | **Done 2026-08-31.** `paneflow-{libghostty-sys,terminal-ghostty,ghostty-smoke}` and the `aarch64-apple-darwin` archive vendored from upstream v0.10.0 (hashes on the issue) and stripped to macOS; linking unconditional, no stub; wired by Phase 2 the same day. |
| #184 Phase 2: session-host swap | **Done 2026-08-31.** `src-app` runs on libghostty-vt; Alacritty and `polling` deleted; the fork's pinned teardown re-attached on top of upstream's host; Kitty graphics, OSC 9/777, the 9;4 chip and `TERM_PROGRAM=ghostty` ride along. 0.2.0 waits for the signed-DMG smoke. |
| #184 Phase 3: engine product | **Done 2026-08-31.** 3.5 (scrollback doc), 3.6 (`surface.read` = history followed by the live screen, one atomic `Transcript` runtime read; the undo record carries the screen too), 3.7 (rode along in Phase 2), 3.8 (`AgentStateSource` ranking `Terminal < SessionRegistry < Hook`, 20 s takeover silence, Claude session-registry sweep at 400 ms, OSC 9;4 / 9 / 777 pane observations). Upstream's 18 tests for it carried by name plus `extract_screen_returns_the_painted_rows_after_history` and `extract_scrollback_window_appends_the_live_screen_after_history`. |
| #184 Phase 4: chrome | **Done 2026-09-01.** One PR per row: #189 bindings (queue → ⇧⌘A, ⇧⌘K + ⌘K clear scrollback, ⇧⌘R reset, `close_window` removed), #190 Help ▸ System Info…, #191 files sidebar per tab + markdown drag-to-pane dropped, #192 Shortcuts page grouped / searchable / virtualized with a confirmed reset, #193 diff dock per tab with the rendered width clamped to the live remainder. Tab-title row deferred per the issue. Each "done when" is a named test. An audit pass (three review lenses) followed; its fixes are the `chore(fork): audit follow-ups` commits. |
| 2b. Windows unwind | **Done.** 71 files, +264/-6767, 13 commits. The real scope was 396 sites across 59 files, not the 158 recorded here: `#[cfg(windows)]` short form is the same predicate and 25 files carried ONLY that spelling. |
| 2c. Linux unwind | **Done.** 20 commits, 77 files, +832/-9559. Census zero-condition 134 -> 0. Four orchestrator increments (updater collapse to DMG-only, Linux port scanners, the Wayland/X11 backdrop, pty_session), then **twelve delegated grok batches**: eight covering all 85 census sites, then four more driven by an adversarial audit that ran after the census hit zero. Also took the last Windows residue - the WSL launcher AND its `WSLENV` environment bridge, `cmd.exe` support, `.exe`/backslash path mechanics, the NTSTATUS Ctrl+C exit code, and `UpdateError`'s AppImage/FUSE/pkexec/msiexec surface - all of it UNGATED and compiling into the macOS binary. |
| Config-schema pass | **Done.** Ghostty and `windows_*_material` dropped from the published schema, Rust struct, and docs. Loader still accepts leftover keys; a leftover `"backend"` key is ignored (`leftover_terminal_backend_key_is_ignored`). |
| Telemetry | **Gone.** `paneflow-telemetry` crate, app module, consent toasts, config block. |
| Self-update | **Sparkle 2 (#119).** The hand-rolled updater/minisign client remain deleted. Sparkle checks hourly, downloads silently, and installs only on ordinary quit. |
| Identity | **Done.** Bundle id `com.theaamgroup.paneflow`, authors The AAM Group, Help/`--help`/schema `$id` point at `theaamgroup/paneflow`. |
| CI | **Done.** `run_tests.yml` macos-15 only; `release.yml` one signed aarch64 lane. Apple secrets proven 2026-08-26; first tag `v0.1.0` published. |
| 2d. Rename to PanesCLI | **Dropped.** Product stays PaneFlow. |
| Community files | **Gone.** No `SECURITY.md`, `CONTRIBUTING.md`, or code of conduct. README is the product page; from-source setup is `INSTALL.md`; agent rules live in `AGENTS.md` / `CLAUDE.md`. |
| Version | **0.3.1** (the 2026-09-04 deep-review sweep, PRs #372 and #373; tag on `a6e7043f`, signed + notarized + stapled). Before it, **0.3.0** (upstream v0.11.0 adopted; #341), **0.2.1**, and **0.2.0** was the libghostty-vt engine (#184). First release tag `v0.1.0` is on `44150ff` (2026-08-26). Releases before Sparkle carried DMG + `.sha256`; Sparkle-enabled releases add `appcast.xml`. `upstream-fork-point` remains. Fork tag names collide with upstream's - see the 2026-09-04 entry. |

## Verified green, and how to reproduce it

```bash
cargo build                                  # exit 0
cargo test --workspace                       # 2877 names, 0 failed, 3 ignored (2026-09-04, the deep-review sweep + the 0.3.1 cut)
cargo deny check advisories licenses sources # exit 0 (cargo-deny 0.19.9, 2026-09-04)
cargo clippy --workspace --all-targets       # exit 0, WARNING COUNT 1 (block v0.1.6)
cargo fmt --check                            # exit 0
./target/debug/paneflow --version            # paneflow 0.3.1
./scripts/win-census.sh                      # STAGE 2b ZERO-CONDITION: 0
./scripts/linux-census.sh                    # STAGE 2c ZERO-CONDITION: 0
                                             # negative control: cfg(unix) 172, cfg(macos) 94 (2026-09-04)
```

The census negative control is not decoration. Read it every time: a census
printing 0 because its regex broke looks exactly like one printing 0 because
the work is done, and that has already happened twice in this project (once
when the regex matched the `update/linux/` PATH, once when it could not see
`!cfg!`).

**The negative control is a MEASURED number and it drifts legitimately. A
mismatch is not a bug to hunt - it is a doc to re-measure.** It moved 137 ->
138 in `9e7655c6` ("give runtime_paths an env seam so tests never mutate
$TMPDIR"), which split `socket_path_spec` into a thin wrapper plus a testable
`socket_path_spec_from(env: &impl Fn(&str) -> Option<OsString>)`. The extracted
inner function needs the same `#[cfg(unix)]` gate as the wrapper it came out
of, so `src-app/src/runtime_paths.rs` went 7 -> 8 sites. Correct code,
correctly gated, nothing to remove. `925e21ce` had written "137 times" into
CLAUDE.md one commit earlier, so the doc was accurate for exactly one commit
and then silently stale for twenty-two; `d007e58b` corrected CLAUDE.md and
both STATE.md copies to 138.

Two things worth keeping from tracing that one:

- **Do not bisect this with checkouts.** Replicate the `CTL_UNIX` pipeline
  (`scan` -> `content_match` on the CONTENT only -> `nocomment`) over
  `git grep -n -E 'cfg!?\(|cfg_attr' <rev> -- '*.rs'`, which reads the object
  store and needs no working tree. Validate the replica against the real script
  at a known commit BEFORE trusting it, for the same reason the negative control
  exists.
- **Diff the sites as a per-file multiset, not a set.** Keying on
  `path:content` collapses two identical `#[cfg(unix)]` lines in one file, so
  the first attempt reported zero added and zero removed while the total had
  moved by one.

**A census at 0 is not a finished platform removal, and 2c proved it.** After
the zero-condition was reached, an adversarial grok audit (run WITHOUT
`--json-schema`, because that flag suppresses the tool loop on open-ended
tasks) found five more classes the cfg scan structurally cannot see, four of
which were fixed in this stage:

1. **Ungated identifiers.** `pty_session.rs` carried the whole `WSLENV`
   environment bridge - `is_wsl_shell`, `merge_wslenv`, `augment_wslenv` and
   five tests - with no `#[cfg]` on any of it, so it compiled into the macOS
   binary and ran in the macOS suite.
2. **An enum the collapse missed.** `InstallMethod` and `AssetFormat` were
   reduced to the DMG set; `UpdateError` was not. `classify` still
   substring-matched `libfuse.so.2` and `appimage-extract-and-run` on any
   updater error, so a macOS user could be shown a toast telling them to run
   `./paneflow-*.AppImage --appimage-extract-and-run`.
3. **User-visible copy.** `paneflow --help` printed `Ctrl+Shift+D/E` and
   `Ctrl+Tab`; a sidebar toast said "install xdg-utils (Linux)"; the published
   JSON Schema told editors the config lives at `~/.config/paneflow/` on Linux
   and `%APPDATA%\paneflow\` on Windows.
4. **Dependency-graph residue.** `tar` and `flate2` were direct deps of the
   deleted tar.gz updater with zero code references; `widestring` sat in
   `[workspace.dependencies]` unused by any member.
5. **YAML.** `run_tests.yml` - see the liability section below.

The generalisable rule: **a detector only measures the shape it was written
for.** The cfg census measures cfg predicates. Ungated code, enum variants,
`Cargo.toml` tables, embedded assets, workflow files and user-facing strings
each need their own sweep, and the audit is what supplies them.

**`cargo fmt --check` is now in the list and was not before.** It had been
failing since c925ece (stage 2a) with 27 hunks across four terminal files, and
nothing local caught it because the four-command block above did not include
it. The release pipeline runs it inside every build-matrix leg, so a tag push
would have burned a ~25 min run before failing. Fixed in 1b1af25.

1806 -> 1790 -> 1725 -> **1684**. The 2b step removed 16 Windows/MSI tests; 2c removed 65
more and renamed 4; post-2c (telemetry crate, schema Ghostty, restored
macOS-shaped coverage) landed at 1684 passed / 0 failed / 2 ignored. Every
single one was accounted for BY NAME, at every integration, by diffing the
sorted name list against the previous commit's. Verify a test-count
change by DIFFING TEST NAMES, never by trusting the count:

```bash
grep -oE '^test [a-zA-Z0-9_:]+ \.\.\.' <log> | sed 's/^test //; s/ \.\.\.$//' | sort
```

The only clippy warning is a pre-existing `block v0.1.6` future-incompat notice
from a transitive dependency. It was present in the very first baseline build
and is not ours.

**Run all five before and after every pass.** Two real breakages were caught by
the test run and by nothing else, and a third (`unused_braces`, introduced when
rustfmt collapsed a ghostty leftover onto one line) was caught only by reading
clippy's WARNING COUNT rather than its exit code. Clippy exits 0 with warnings,
so "clippy exit=0" is not evidence on its own -- compare the warning count to
the one known `block v0.1.6` notice.

## The single most useful technique found

When a feature is deleted from `Cargo.toml`, every `cfg` that still references
it becomes an `unexpected_cfgs` warning. That converts a grep-and-hope job into
an enumerated worklist with an exact zero-condition:

```bash
cargo check --workspace --all-targets --message-format=short 2>&1 \
  | grep 'unexpected `cfg` condition value' \
  | sed 's/:[0-9]*:[0-9]*.*//' | sort | uniq -c | sort -rn
```

Use `--all-targets`. Without it, test-only modules are invisible: the Ghostty
count read 278 without it and 338 with it.

This does NOT apply to the Windows and Linux unwind, because `target_os` is a
real cfg value and never warns. Those passes have no compiler-provided
worklist, which makes them harder than Ghostty was, not easier.

**The substitute, proven on 2b: a committed census script with a zero
condition** (`scripts/win-census.sh`). Reuse it for 2c by swapping the
predicate. Four things made it trustworthy, and the last two are the ones that
actually caught bugs:

1. A negative control BEFORE trusting it. A census returning 0 because its
   regex is broken looks exactly like one returning 0 because the work is done.
   Confirm it still flags known-present sites first.
2. Comment-only lines counted separately. A doc comment explaining WHY an item
   is `#[cfg(unix)]`-gated legitimately names Windows; counting those as code
   made the zero condition unreachable.
3. **A multi-line pass.** Every line-oriented grep is blind to
   `cfg!(any(\n  target_os = "windows", ...))` because `cfg!(` and the arm sit
   on different lines. One real site (`terminal/view.rs`) hid there.
The 2c census ended at `STAGE 2c ZERO-CONDITION: 0` with all six components
zero (attribute gates, runtime `cfg!()`, Cargo target tables, target-triple
string checks, multi-line cfg expressions, negated `cfg!`). Adding a component
is cheap and is how the tool stays honest as new spellings turn up.

4. **A sweep over a DIFFERENT term space** - `.exe`, named pipes, `msiexec`,
   AUMID, Win32, drive letters, target triples. Re-running the cfg grep only
   reproduces its own blind spots. This is what found `window_chrome/backdrop.rs`:
   an orphaned file whose `mod` declaration had been removed, leaving it
   uncompiled and invisible to a cfg scan PRECISELY because it no longer had a
   cfg gate.

## Method rules this session paid for

1. **Never report a finding you have not observed.** Three separate claims were
   falsified by actually running something: that the keybinding scheme was never
   ported to macOS (the `secondary` shorthand disproves it), that Option+Arrow
   was broken (`alt_phys` at `keys.rs:101` disproves it), and that a missing
   `rerun-if-env-changed` was a real bug (rustc emits `env-dep` lines for
   `option_env!` and Cargo honours them). All three came from reasoning about
   code that had been read rather than behaviour that had been observed.
2. **Add a control, and if the control reproduces the positive result, the
   experiment is broken.** The `rerun-if-env-changed` "fix" was confirmed by a
   first test that changed two variables at once. The control caught it. Warm
   any build-cache experiment to a steady state first, then change exactly one
   thing.
3. **Do not pipe a command whose exit code matters.** `cargo test | tail` reports
   `tail`'s status, so a failing run was announced as a success. Same class of
   error: `cmd 2>&1 > file` sends stderr to the terminal, not the file. Use
   `cmd > file 2>&1`.
4. **Do not diagnose downstream errors while an upstream parse error stands.** A
   syntax error in `view.rs` produced four bogus `Pixels as usize` cast errors in
   `input.rs`. They vanished when the parse error was fixed.
5. **A green `cargo build` is not a green tree.** Stage 1 built clean and failed
   `cargo test`, because a `#[cfg(test)]` block did `include_str!` on a deleted
   WiX manifest and its `mod` declaration was never cfg-gated.
6. **A measurement that cannot see a whole syntactic form is not a
   measurement.** `scripts/linux-census.sh` matched the `not(target_os =
   "macos")` predicate but not operator negation `!cfg!(target_os = "macos")`.
   Two live sites in `keys.rs` were invisible to it, so "zero-condition reached
   0" would have been claimed over them. The fix is committed with its own
   before/after proof: the new `negated cfg!(target_os)` component read **2**
   on the tree that contained both sites and **0** after they were removed. A
   new detector that reads 0 on its first run has not been tested - it has been
   assumed.
7. **Before acting on a documented claim, check whether two gates are
   complementary definitions of the SAME item.** `CLAUDE.md` said to un-gate
   `dmg.rs`'s `#[cfg(all(test, not(macos)))]` items rather than delete them.
   They were the second half of a pair with `#[cfg(any(not(test), macos))]`, so
   un-gating produced `error[E0428]`, a duplicate definition. Three documented
   claims in this project have now been falsified by executing something; the
   pattern is always the same, someone reasoned about code they had read.
8. **Line numbers in an inventory go stale the moment you commit.** 76 of 2c's
   85 sites still matched `worklist.tsv` exactly, but 9 had drifted - the four
   files earlier increments had touched, plus three `Cargo.toml` tables the
   inventory never covered. Re-derive `file:line` from the live tree before
   writing a brief, and have the agent stop and report if a line does not say
   what the brief claims.
9. **`--json-schema` can suppress the grok tool loop** on an open-ended
   audit (one turn, empty structured object). Use it for bounded site
   lists. Run discovery audits without it (the final 37-turn audit that
   found Help/schema `$id` still pointing at arthjean ran this way).
10. **`set -e` plus `grep FAILED` is a false fail** when the log is
    green. `rg … | head` is a false fail via SIGPIPE after the command
    already succeeded. Two `cargo` processes on one `target/` deadlock
    on the lock; kill the extra one.
11. **Kickoff task lists are not file-disjoint.** Schema, checker, and
    identity tasks all touched overlapping files. Check overlap before
    launching parallel worktrees. Three concurrent, APFS `cp -c -R
    target`, agents never git: still the working recipe.

## Notes from the Windows and Linux passes (2b, 2c - both DONE)

The Ghostty pass used a scripted cfg pruner. It was **wrong four times**, each
caught by the compiler, and each a real Rust syntax subtlety worth knowing
before writing another one:

1. A `,` inside `Cow<'_, [u8]>` sits at paren-depth zero and looks like a
   struct-field terminator, so it cut a function signature in half.
2. `use a::{b, c};` closes its brace block **before** the semicolon, so
   brace-matching alone orphans the `;`.
3. `let x = if c { a } else { b };` does not end at the `if` block's closing
   brace.
4. `Enum::Variant { a, b } => {}` has a **braced pattern**, which is not the
   match arm's body.

What those passes proved, kept because the next platform-shaped pass will
need it:

- `#[cfg(unix)]` now appears **138 times** and macOS needs nearly all of it.
  `#[cfg(target_os = "macos")]` appears **71 times**. Both are live arms; both
  stay. This was the highest-risk distinction in 2c and no batch got it wrong -
  because every brief opened with the same four lines, verbatim:

  > `#[cfg(unix)]` is TRUE on macOS. macOS IS a unix. Never remove one.
  > `unix` is not `linux`. Only `all(unix, not(target_os = "macos"))` is Linux.

  The `all(unix, not(macos))` sites 2b left standing are gone. What remains
  spelled that way is `all(unix, not(test))` in `terminal/pty_session.rs`,
  which is a test-isolation gate, not a platform gate.
- **A single-arm survivor should be hoisted, not left gated.** When the other
  platform's arm is deleted and one `#[cfg]` block is all that is left inside a
  function, rustfmt collapses it and clippy fires `unused_braces` - or, if the
  block was `let x = ...; x`, `let_and_return`. Eight such hoists landed in 2c
  and each dropped the `cfg(macos)` count by one; that is why the negative
  control moved 92 -> 77 and every step of it is accounted for.
- `#[cfg(not(windows))]` and `#[cfg(not(unix))]` blocks are fallback arms. The
  surviving twin must be **un-gated**, not deleted alongside them.
- Predicates like `any(test, target_os = "windows")` keep code alive for tests
  on all platforms. Removing the Windows arm leaves `#[cfg(test)]`, which makes
  the item test-only rather than dead. That is a reduction, not a deletion.
- **The inverted twin is the dangerous one.** `any(test, not(windows))` reduces
  the OPPOSITE way: `not(windows)` is ALREADY TRUE on macOS, so the predicate
  is a tautology and the item becomes UNCONDITIONAL. Turning it into
  `cfg(test)` deletes live code from the release binary while every test still
  passes. 2b hit two (`ipc.rs`, `ipc-client/lib.rs`). Expect more polarities in
  2c: `any(not(windows), debug_assertions)` and `not(any(macos, windows))` both
  turned up, and each reduces differently.
- **`cfg!(...)` with the bang is a runtime expression, not a gate.** Nothing in
  the build catches a wrong edit. 2b had 31 and they were deliberately withheld
  from every parallel worker and done in one reviewed pass. The trap: three
  sites read `cfg!(macos) || cfg!(windows)`, which is TRUE on macOS - deleting
  that branch because it names windows would have silently disabled
  case-insensitive path dedup in the diff dock.
- ~~`agents/notifications.rs` dangling `windows_app_identity` references~~
  **Resolved in 2b.** Note the correction: both sat INSIDE
  `#[cfg(target_os = "windows")]` blocks, so the delete rule removed them as a
  side effect. They did not need separate handling and did not survive to 2c.
- `TerminalBackendConfig` is gone entirely (#184): a leftover `"backend"` key in
  an old `paneflow.json` is ignored, not mapped (`leftover_terminal_backend_key_is_ignored`).
- Embed size cap is Mach-O `release-min` (2026-08-27): 1,211,840 B measured
  (shim 472,368 + ai-hook 336,464 + mcp 403,008), `EMBED_SIZE_LIMIT_BYTES =
  1_400_000` = total + 15.5% (slack 188,160 B = 13.4% of the cap).

## Biggest remaining liability

`main` is pushed to `origin`. First signed GitHub Release is out:
https://github.com/theaamgroup/paneflow/releases/tag/v0.1.0
(`v0.1.0` on `44150ff`, DMG + `.sha256`). #7 and #9 are closed.

Closed by machine evidence:

- #13 (2026-08-28): the user confirmed the clean v0.1.1 build is installed
  and running. The installed `com.theaamgroup.paneflow` app passes
  `codesign --verify`, Gatekeeper accepts it as Notarized Developer ID, its
  stapled ticket validates, and live IPC returned three surfaces. A synthetic
  permission notification sent through the shipped v0.1.1 hook reached the
  app as an isolated `waiting_for_input` session; `SessionEnd` then removed
  the test row without changing the real sessions. Accessibility, Automation,
  and FDA are not request surfaces; only Notifications is in play.

- #10: next-workspace rebound from `secondary-tab` (Cmd+Tab, eaten by the
  macOS app switcher; synthetic Cmd+Tab moved focus to another app while
  Cmd+1/Cmd+2 switched workspaces) to `ctrl-tab`, with a test that fails
  if any default binds `secondary-tab` again.

- #11: the shipped v0.1.0 dmg is Developer ID signed (K7X6VGPFR8),
  notarized and stapled; `spctl -a -t exec` on a quarantined copy →
  `accepted, source=Notarized Developer ID`. The local `dist/` dmg is a
  different, adhoc build; do not smoke with it.
- #14: Cmd+Shift+D / E / W / T each moved `paneflow ls` pane counts
  3→4→5→4→5 on a debug instance with an isolated socket.
- #15: `which claude` in a pane → the paneflow-dev shim dir first on
  PATH; `status`/`ps`/`watch` tracked idle → thinking(Bash/Read) →
  finished → idle across a real `claude -p` turn.

#8 (minisign keypair) is obsolete. #12 (no GitHub Releases request) is
automatic now that the updater is deleted. #16–#19 are closed.

Product bugs continue from #20. Some early issues (#1 in particular)
were filed before `release.yml` was cut to one aarch64 lane; check the
tree before treating an issue as still open work.

The 2c-era four-platform `run_tests.yml` / `release.yml` are gone. Both
workflows are macos-15. YAML is still its own sweep: the cfg census cannot
see it.

## Parallel work

Do not use the `paneflow-conductor` skill to grind this repo. Headless grok
in git worktrees is what worked. Conducting can drive a live PaneFlow
window when both IPC env gates are on, but `paneflow read` still returns 0
lines and that path is for a human-supervised agent, not batch fan-out.

**This section used to say the Rust passes do not fan out. 2c falsified that.**
All eight of its remaining batches ran on headless grok in isolated worktrees,
and every one came back green on the first attempt with no re-brief. What
changed was not the code - it was that the briefs stopped saying "find the
Linux code" and started carrying **the exact site list**: `file:line`, the cfg
expression as written, the action, and the reduction. That came from a
read-only inventory phase (nine grok shards, 325 classified sites) run BEFORE
any edit.

So the real rule is: **fan-out works when the worker does not have to discover
anything.** A batch that must find its own targets in a codebase this
interleaved will delete a `cfg(unix)` arm sooner or later.

The mechanics that made it cheap:

- One `git worktree` per batch, seeded with `cp -c -R target <wt>/target`. On
  APFS that is a clone: 27 seconds for three worktrees of a 19 GB target dir,
  ~0 bytes on disk, and a warm incremental rebuild instead of a 15-25 minute
  cold GPUI build.
- Worktrees are reusable between waves: `git -C <wt> reset --hard` then
  `git -C <wt> checkout -B <branch> mac-only-fork`.
- Agents never touch git. They leave edits unstaged; the orchestrator collects
  `git -C <wt> diff`, applies it to the main worktree, re-runs all five gates
  itself, and writes the commit. Agent "green" claims are never the evidence -
  in 2b one batch reported green from a clippy run that predated its own final
  edits.
- Three concurrent batches was the working cap on this machine. Disjoint file
  sets are what make that safe: two batches editing the same file collide at
  `git apply` time even when their edits are six lines apart. Kickoff task
  lists are not automatically disjoint (schema + feed + identity shared
  files); check overlap before launching.
- `--json-schema` is for bounded site lists. On an open-ended audit it can
  stop grok after one turn with an empty object. The 37-turn final audit
  ran without it.
