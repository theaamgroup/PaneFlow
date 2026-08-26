# PaneFlow fork: current state

Living handoff record. Updated 2026-08-25, after stage 2c landed.

Companion documents:
- `docs/fork/2026-08-25-post-2c-plan.md` is the **plan of record for what
  happens next** - the locked decisions, the ordering and its two real
  dependencies, the delegation machinery that worked in 2c, and the verification
  contract. The live task list (ten tasks) is in the harness; that file holds
  what a task description cannot carry.
- `docs/fork/2026-08-25-mac-only-fork-design.md` holds the **decisions**, the
  **leak register**, and a **16-item traps register**. Read it before touching
  platform code, the updater, or the config schema.
- `CLAUDE.md` holds build prerequisites, the module tree, and the commands.

This file holds only: where the work stands, what is next, and the rules the
session learned the hard way.

## Identity

| | |
|---|---|
| Local clone | `~/Github/paneflow` (directory still carries the upstream name) |
| Branch | **`main`**. Reconciled 2026-08-25: the fork point is tagged `upstream-fork-point`, `main` was fast-forwarded to the fork work (strict ancestor, no rewrite), and `mac-only-fork` remains at the same commit - delete it after the first release tag. |
| `origin` | `github.com/theaamgroup/paneflow` (private). Renamed from `panescli` on 2026-08-25 when the PanesCLI rebrand was dropped; GitHub keeps redirects. |
| `upstream` | `github.com/arthjean/paneflow` (read-only, kept for cherry-picks) |
| Fork point | v0.8.2, commit `f53f982291f75a9daf565827b3167d0e96925d0a` |
| gpui backup | `github.com/theaamgroup/zed`, holds pinned rev `3aaba57b`. `Cargo.toml` still points at `arthjean/zed` deliberately. |
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
`cargo run` build never reads the release config path. Build-from-source is now
the only workflow here, so this is the most likely source of confusion.

## Stage status

| Stage | State |
|---|---|
| 0. GitHub plumbing | **Done.** Private repo created, both branches pushed, zed backup forked and its pinned rev verified present. |
| 1. File-level deletion | **Done.** Non-macOS packaging, the two upstream-publishing workflows, non-macOS docs and scripts and assets, and upstream's project-management cruft. |
| Docs correctness pass | **Done.** 36 files, +2124/-2355. Turned out to be more a correctness fix than a platform strip. |
| 2a. Ghostty removal | **Done.** Roughly 11,600 lines. 338 stale cfg sites reduced to zero. |
| 2b. Windows unwind | **Done.** 71 files, +264/-6767, 13 commits. The real scope was 396 sites across 59 files, not the 158 recorded here: `#[cfg(windows)]` short form is the same predicate and 25 files carried ONLY that spelling. |
| 2c. Linux unwind | **Done.** 20 commits, 77 files, +832/-9559. Census zero-condition 134 -> 0. Four orchestrator increments (updater collapse to DMG-only, Linux port scanners, the Wayland/X11 backdrop, pty_session), then **twelve delegated grok batches**: eight covering all 85 census sites, then four more driven by an adversarial audit that ran after the census hit zero. Also took the last Windows residue - the WSL launcher AND its `WSLENV` environment bridge, `cmd.exe` support, `.exe`/backslash path mechanics, the NTSTATUS Ctrl+C exit code, and `UpdateError`'s AppImage/FUSE/pkexec/msiexec surface - all of it UNGATED and compiling into the macOS binary. |
| Config-schema pass | **Not started. Next.** `TerminalBackendConfig::Ghostty` (`schema.rs:568`), `windows_terminal_material` (`:57`), `windows_chrome_material` (`:60`), `windows_terminal_material_enabled` (`:310`), the `pane.rs` material call sites and their two `terminal_material_scopes_*` tests, and the loader's `set_field!` material keys. 2c deliberately left all 15 of these standing. schema.rs + schemas/paneflow.schema.json + docs/user/configuration/schema.md move together (drift test). |
| 2d. Rename to PanesCLI | **Dropped.** Scoped (273 files) then dropped; the product stays PaneFlow. See `docs/fork/2026-08-25-post-2c-plan.md`. |
| 3. Signed release | **Not started.** Needs a fresh minisign keypair, a macOS-only `release.yml`, and the six `APPLE_*` secrets. |

## Verified green, and how to reproduce it

```bash
cargo build                                  # exit 0
cargo test --workspace                       # 1725 passed, 0 failed, 2 ignored
cargo clippy --workspace --all-targets       # exit 0, WARNING COUNT 1 (block v0.1.6)
cargo fmt --check                            # exit 0
./target/debug/paneflow --version            # paneflow 0.8.2
./scripts/win-census.sh                      # STAGE 2b ZERO-CONDITION: 0
./scripts/linux-census.sh                    # STAGE 2c ZERO-CONDITION: 0
                                             # negative control: cfg(unix) 152, cfg(macos) 77
```

The census negative control is not decoration. Read it every time: a census
printing 0 because its regex broke looks exactly like one printing 0 because
the work is done, and that has already happened twice in this project (once
when the regex matched the `update/linux/` PATH, once when it could not see
`!cfg!`).

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

1806 -> 1790 -> 1725. The 2b step removed 16 Windows/MSI tests; 2c removed 65
more and renamed 4. Every single one was accounted for BY NAME, at every
integration, by diffing the sorted name list against the previous commit's. Verify a test-count
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

- `#[cfg(unix)]` now appears **152 times** and macOS needs nearly all of it.
  `#[cfg(target_os = "macos")]` appears **77 times**. Both are live arms; both
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
- `TerminalBackendConfig::Ghostty` is still a live variant in
  `crates/paneflow-config/src/schema.rs:568` and in `schemas/paneflow.schema.json`.
  It is permanently dead: 2c reduced `terminal/view.rs`'s
  `auto_selects_ghostty_for_target()` to a literal `false`, so `auto` never
  selects it and an explicit `ghostty` logs the unavailable warning and runs
  Alacritty. Decide its fate during the config-schema work, and note that a
  drift test asserts every schema key appears in
  `docs/user/configuration/schema.md`.
- The binary-size budget at `src-app/build.rs:61` and in `run_tests.yml` is
  baselined on Linux ELF. It needs a Mach-O re-baseline, not deletion.

## Biggest remaining liability

`.github/workflows/release.yml` is still 3,177 lines of upstream's
four-platform pipeline, with live references to `GPG_*`,
`AZURE_TRUSTED_SIGNING_*` and `POSTHOG_API_KEY`. Those are inert only because
the secrets do not exist in this org. Never create them here. See the leak
register in the design doc for what each one fed upstream.

**`run_tests.yml` is the same problem and it is not inert - it runs on every
PR.** An adversarial audit at the end of 2c enumerated it (the cfg census
cannot see YAML, which is exactly why that audit exists):

- Linux legs: `check_style` (`:243`, ubuntu-22.04, installs `libwayland-dev` /
  `libx11-dev`, then `cargo clippy --workspace`), `run_tests_linux` (`:286`),
  `release_build_linux` (`:325`) whose size budget at `:405` stats an ELF at
  `target/embed-build/x86_64-unknown-linux-gnu/release-min`, and
  `linux_aarch64_check` (`:1158`).
- Windows legs: `windows_check` (`:835`), `windows_render_smoke` (`:1044`,
  downloads `paneflow.exe`), `windows_aarch64_check` (`:1267`).
- The `tests_pass` aggregator (`:1357-1371`) still `needs:` all six. Prune jobs
  without editing `needs:` and the workflow never completes; leave them and it
  goes red.

Two of those legs cannot pass any more, independently of anything 2c did:
`windows_check:956` passes `--features paneflow-app/libghostty-windows`, and no
`[features]` table or that feature exists anywhere in the workspace; and a Linux
build now fails to compile because `workspace/pid_resolve.rs` defines
`parent_of` only under `#[cfg(target_os = "macos")]` while
`resolve_surface_for_pid` calls it with no other arm. Stage 3 owns the fix; the
point of recording it here is that the CI signal is already meaningless, so a
green tree locally is currently the only real gate.

## Parallel work

Do not use the `paneflow-conductor` skill for fan-out. It is a feature of this
app and it does not work reliably, which is recorded as known defect 1 in the
design doc. Use the `grok-subagents` skill, or headless agent processes as
background jobs with one git worktree per task.

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
  `git apply` time even when their edits are six lines apart.
