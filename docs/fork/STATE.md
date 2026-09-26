# PaneFlow fork: current state

Living handoff record: what has landed, how to verify it, and the method rules
this project has paid for. Updated 2026-09-26 for the removal round tracked in
#841, on top of the 0.7.2 cut.

**2026-09-23: the 0.7.2 cut.** 83 non-merge commits since `v0.7.1`, a patch
bump. No new surfaces. The cut fixes pointer hits on cell boundaries, keeps
an oversized grapheme from failing the pane snapshot, restores the workspace
git watch when a pane returns home, sizes symlink diffs by the link, and
keeps review, broadcast, and queued-prompt state correct for background
tabs. Agent hook launch, MCP install, OpenCode and Pi shims, Opus 4.5+
pricing, and session restore are tightened the same way. Curated notes live
in `docs/releases/v0.7.2.md`.

Pre-flight on the bump, warm `target/` cloned from the #732 worktree:
`cargo test --workspace --locked --no-fail-fast` **3,003 passed, 1 failed,
4 ignored**. The failure was
`terminal::ghostty_session::tests::a_large_paste_into_a_child_that_floods_output_without_reading_stdin_does_not_wedge_the_runtime`
(`Ghostty callback effects overflowed (768 events, 0 bytes)`). An immediate
rerun of that test passed. `cargo clippy --workspace --all-targets --locked
-- -D warnings` exit 0, **WARNING COUNT 1** (`block v0.1.6`); `cargo fmt
--check` exit 0; `cargo deny check advisories licenses sources` exit 0 ->
`advisories ok, licenses ok, sources ok` (the two GPUI crates without a
license field remain accepted warnings). `cargo build -p paneflow-app
--locked` exit 0, with the known vendored Ghostty `duplicate symbol
'_memset'` linker notice. `./target/debug/paneflow --version` reports
`paneflow 0.7.2`. The four ignored tests are
`layout::render::tests::eight_pane_gpui_input_to_paint_performance_gate`,
`startup_bench::startup_first_frame_benchmark`,
`terminal::ghostty_stress::ghostty_spawn_resize_close_stress_has_no_residual_growth`,
and `terminal::perf_bench::terminal_pipeline_benchmark` (the stress test
was deleted after the cut, #853). The version bump does not change platform
`cfg` sites, so the 0.7.0 census stands.

Companion documents:
- [archived execution-plan revision](https://github.com/theaamgroup/PaneFlow/commit/336cdada) is the **historical 2026-08-25
  execution plan** (schema, telemetry, identity, CI). Leftover-removal
  buckets 1–4 (2026-08-26) superseded its self-update “disable the feed”
  decision: the old hand-rolled updater was deleted. GitHub issue #13's bundle-id and
  Notifications smoke was completed on the installed v0.1.1 app on 2026-08-28.
- `docs/fork/2026-08-25-mac-only-fork-design.md` holds the **decisions**, the
  **leak register**, and an **18-item traps register**. Read it before touching
  platform code or the config schema. Sparkle owns self-update; the old updater
  and minisign remain gone.
- `CLAUDE.md` holds build prerequisites, the module tree, and the commands.

This file holds only where the work stands and the rules the project learned
the hard way. Open work is `gh issue list`. The dated entries for the cuts and
passes before 0.7.2 are archived in git history (this file as it stood before
#853), in `docs/releases/v*.md` and in the GitHub releases;
`src-app/tests/fork_docs_backlog_policy.rs` fails if this file regains the
work-queue role (issue #224).

Two things that pass gates but are **not** end-to-end verified:

- **No live GUI smoke was possible.** Plain keystrokes deliver via
  `CGEventPostToPid`, but modified chords never fire an action, and
  `NSRunningApplication.activate` is blocked on macOS 26 (`AXFrontmost`
  works instead). Details and the working recipe are in the
  `paneflow-live-keystroke-smoke` memory and in issue #109's
  "Verifying this" section. Rename and the zero-pane chords still want one
  human pass before release.
- **Sparkle's full installed-update path needs two consecutive signed tags.**
  The bundled framework loads and starts in a live GUI, but the first
  Sparkle-enabled release must still prove `/Applications` vN staging vN+1,
  silent install on quit from the DMG, and deliberate Developer-ID mismatch
  rejection. The release runbook carries that first-release gate.

## Identity

| | |
|---|---|
| Local clone | `~/Github/paneflow` (directory still carries the upstream name) |
| Branch | **`main`**. Reconciled 2026-08-25: the fork point is tagged `upstream-fork-point`, `main` was fast-forwarded to the fork work (strict ancestor, no rewrite), and the `mac-only-fork` branch it was reconciled with has since been deleted. |
| `origin` | `github.com/theaamgroup/paneflow` (public since 2026-08-30 so Sparkle can fetch release assets anonymously). Renamed from `panescli` on 2026-08-25 when the PanesCLI rebrand was dropped; GitHub keeps redirects. |
| `upstream` | `github.com/arthjean/paneflow` (read-only, kept for cherry-picks) |
| Fork point | v0.8.2, commit `f53f982291f75a9daf565827b3167d0e96925d0a` |
| gpui backup | `github.com/theaamgroup/zed`, holds pinned rev `3aaba57b`. `Cargo.toml` points at **`zed-industries/zed`** rev `fecc3273`; do not reintroduce an `arthjean/zed` pin (CLAUDE.md forbids it). |
| License | GPL-3.0-or-later. Keep `LICENSE` and the single attribution line in `README.md`. |

## Naming, confirmed and locked

The product stays **PaneFlow**. The 2d rename to PanesCLI was scoped and
dropped; see [archived execution-plan revision](https://github.com/theaamgroup/PaneFlow/commit/336cdada).

| Thing | Value |
|---|---|
| Product | PaneFlow |
| Bundle id | `com.theaamgroup.paneflow` (task 12 replaced upstream's `io.github.arthurdev44.paneflow`) |
| Binary and CLI | `paneflow` |
| Config dir | `~/Library/Application Support/paneflow/` |
| Debug config dir | `paneflow-dev` |
| Env prefix | `PANEFLOW_*` |
| MCP server | `paneflow` |

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
| Version | **0.7.2** (2026-09-23; tag on `1e6621a0`; notes in `docs/releases/v0.7.2.md`). Before it 0.7.1, 0.7.0, 0.6.1, 0.6.0, 0.5.0 and 0.4.0 (notes under `docs/releases/`), 0.3.1 (the 2026-09-04 deep-review sweep), 0.3.0 (upstream v0.11.0 adopted; #341), 0.2.1, and 0.2.0, the libghostty-vt engine (#184). First release tag `v0.1.0` is on `44150ff` (2026-08-26). Releases before Sparkle carried DMG + `.sha256`; Sparkle-enabled releases add `appcast.xml`. `upstream-fork-point` remains. Fork tag names collide with upstream's; CLAUDE.md's gotchas carry the tag-ownership rule. |

## Verified green, and how to reproduce it

```bash
cargo build                                  # exit 0
cargo test --workspace                       # exit 0, 2,781 passed, 0 failed, 3 ignored (2026-09-26)
cargo deny check advisories licenses sources # exit 0 (cargo-deny 0.19.9, 2026-09-26)
cargo clippy --workspace --all-targets       # exit 0, WARNING COUNT 1 (block v0.1.6)
cargo fmt --check                            # exit 0
./target/debug/paneflow --version            # paneflow 0.7.2
./scripts/win-census.sh                      # STAGE 2b ZERO-CONDITION: 0
./scripts/linux-census.sh                    # STAGE 2c ZERO-CONDITION: 0
                                             # negative control: cfg(unix) 176, cfg(macos) 90 (2026-09-26)
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
5. **YAML.** `run_tests.yml` and `release.yml` still carried the four-platform
   matrices. Both are macos-15 now, and YAML stays its own sweep: the cfg
   census cannot see it.

The generalisable rule: **a detector only measures the shape it was written
for.** The cfg census measures cfg predicates. Ungated code, enum variants,
`Cargo.toml` tables, embedded assets, workflow files and user-facing strings
each need their own sweep, and the audit is what supplies them.

**`cargo fmt --check` is now in the list and was not before.** It had been
failing since c925ece (stage 2a) with 27 hunks across four terminal files, and
nothing local caught it because the gate list at the time did not include
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
and is not ours. `cargo build` also prints the vendored Ghostty archive's
`ld: duplicate symbol '_memset'` notice (`compiler_rt.o` vs
`libghostty-vt-static_zcu.o`). It is benign (ld64 keeps the first) and a
property of the archive; only a rebuilt archive at the next `source_sha` bump
removes it (issue #194).

**Run all six CLAUDE.md gates (`cargo build`, `cargo test --workspace`, clippy,
`cargo fmt --check`, `paneflow --version`, `cargo deny`) before and after every
pass.** Two real breakages were caught by
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
12. **A doc claim is a liability, not a record.** The 2026-08-28 review
    falsified `PANEFLOW_ALLOW_MULTIPLE` (documented presence-gated; it is
    value-gated), issue #39 (documented open; fixed with a regression test),
    the census counts, the GPUI remote in `AGENTS.md`, and `.mcp.json`'s
    existence. Each was a doc that recorded a bug and was never updated when
    the bug was fixed. Closing an issue includes grepping `CLAUDE.md`,
    `STATE.md` and `AGENTS.md` for its number.
13. **Verify the agent, not just the code.** Agent-reported site counts have
    been wrong (the IPC index pattern had 2 live sites, not 5), and a
    mutation check can fail for the wrong reason (`--lib` on a binary crate
    exits 101). Re-read the source before acting on any finding.
14. **A test that pins a property is not a test that pins the call site.** A
    shortcut test asserted that `unparse()` round-trips, which held whatever
    production called. Extracting `recorded_shortcut_key()` made the call
    site itself testable without a `Window`.
15. **A wall-clock budget test takes the fastest of several passes.** libtest
    runs the binary fully parallel, so a single sample charges the code for
    whatever else is scheduled: `perf_scan_200_lines_under_budget` read 76 ms
    against a 25 ms budget for a scan that costs 431 us (#477). The fastest of
    five keeps the regression signal that `#[ignore]` would discard.
16. **The first exec of a newly written executable takes 12-20 s on this
    Mac** (a Gatekeeper / XProtect first-launch scan). It is the one cause of
    the `paneflow-ai-hook` "hook subprocess exceeded 7s" timeouts and the
    `opencode_sessions…retention_limit` failure under build load. They pass
    alone; judge a run by test name, never by the `test result:` line.
17. **`grep` in the agent's tool shell is not `/usr/bin/grep`.** It routes to
    `ugrep --ignore-files`, which skips `target/`. The census scripts run
    BSD grep under `bash` and pass `--exclude-dir=target --exclude-dir=.git`
    (`9c86912c`); without that they crawled 19 GB in 27 minutes.
18. **`pull_request: branches: [main]` starves a stacked PR of CI.** The
    filter was removed (`4c512480`) so each phase of a stack runs CI before
    the next one starts. Do not restore it.
19. **Deriving a display label breaks any equality check written against the
    stored one.** Once a tab label could come from its pane, a rename check
    that rebuilt `"Tab N"` from `Tab::title` stopped matching what the editor
    was seeded with, and Enter on an untouched agent-derived label would have
    frozen it into `Tab::title`. Compare against the displayed string the
    caller passes in.
20. **An arm-then-confirm without a settle delay is not a confirmation.**
    GPUI dispatches every click listener on each mouse-up, so a double-click
    fires both the arm and the confirm. Copy `ARM_SETTLE` in
    `app/close_guard.rs`.
21. **Re-push an undo record only on a transient refusal.**
    `handle_undo_close_pane` pops the newest record, `push_closed_record`
    appends to the newest, and workspace ids come from a monotonic
    `fetch_add`, so re-pushing a record whose workspace is gone re-promotes
    it forever and buries every record beneath it. Drop it on a permanent
    refusal.
22. **`close_workspace_tab` re-focuses only when `ws_idx == self.active_idx`.**
    A background workspace's tab row is right-clickable, so any `Window`-holding
    path that closes a background tab must hand focus back itself or it
    strands the window (the issue #108 class).
23. **GPUI at the pinned rev has `linear_gradient`, inset `BoxShadow` and
    `border_dashed`, but no `text_shadow`.** A glow must come from the box;
    do not fake a text halo with layered offset text.
24. **A shared blink phase does not blink an idle window.** Only observers of
    `BlinkPhaseGlobal` repaint on it. `PaneFlowApp` observes it for the About
    dialog's caret but notifies only while the dialog is open; an
    unconditional notify wakes the whole app every 530 ms.
25. **Never compare the startup bench against upstream's numbers.** Upstream
    measured them on Windows.

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

- `#[cfg(unix)]` and `#[cfg(target_os = "macos")]` are live arms and macOS
  needs nearly every site of both; the census negative control in the
  verified-green block carries the current counts. Both stay. This was the
  highest-risk distinction in 2c and no batch got it wrong - because every
  brief opened with the same four lines, verbatim:

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

## Parallel work

Use headless agents in separate git worktrees for batch work. The pane-driving
pipeline and skill were removed in #609; the CLI, read-only MCP bridge, and
agent lifecycle hooks remain available for interactive terminal work.

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
  `git -C <wt> checkout -B <branch> origin/main`.
- Agents never touch git. They leave edits unstaged; the orchestrator collects
  `git -C <wt> diff`, applies it to the main worktree, re-runs all six gates
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
- A chain of dependent ports does not have to wait for the lead's verify
  between links: scratch-commit each handed-off port (detached, never pushed)
  in its worker worktree and dispatch the next link there (#418 -> #419 ->
  #420 ran this way).
