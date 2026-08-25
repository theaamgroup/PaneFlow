# PanesCLI fork: current state

Living handoff record. Updated 2026-08-25.

Companion documents:
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
| Branch | `mac-only-fork`, 34 commits ahead of the fork point |
| `origin` | `github.com/theaamgroup/panescli` (private) |
| `upstream` | `github.com/arthjean/paneflow` (read-only, kept for cherry-picks) |
| Fork point | v0.8.2, commit `f53f982291f75a9daf565827b3167d0e96925d0a` |
| gpui backup | `github.com/theaamgroup/zed`, holds pinned rev `3aaba57b`. `Cargo.toml` still points at `arthjean/zed` deliberately. |
| License | GPL-3.0-or-later. Keep `LICENSE` and the single attribution line in `README.md`. |

## Naming, confirmed and locked

| Thing | Current on disk | Target |
|---|---|---|
| Product | PaneFlow | PanesCLI |
| Bundle id | `io.github.arthurdev44.paneflow` | `com.theaamgroup.panescli` |
| Binary and CLI | `paneflow` | `panescli` |
| Config dir | `~/Library/Application Support/paneflow/` | `~/Library/Application Support/panescli/` |
| Debug config dir | `paneflow-dev` | `panescli-dev` |
| Env prefix | `PANEFLOW_*` | `PANESCLI_*` |
| MCP server | `paneflow` | `panescli`, needs re-registering locally |
| Conductor skill | `skills/paneflow-conductor/` | `panescli-conductor` |

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
| 2c. Linux unwind | **Not started.** 78 `target_os = "linux"` occurrences, plus 24 `not(target_os = "macos")` branches that become dead. |
| 2d. Rename to PanesCLI | **Not started.** 271 files mention `paneflow`. Must be one atomic pass across prose and code together. |
| 3. Signed release | **Not started.** Needs a fresh minisign keypair, a macOS-only `release.yml`, and the six `APPLE_*` secrets. |

## Verified green, and how to reproduce it

```bash
cargo build                                  # exit 0
cargo test --workspace                       # 1790 passed, 0 failed
cargo clippy --workspace --all-targets       # exit 0
cargo fmt --check                            # exit 0
./target/debug/paneflow --version            # paneflow 0.8.2
./scripts/win-census.sh                      # STAGE 2b ZERO-CONDITION: 0
```

**`cargo fmt --check` is now in the list and was not before.** It had been
failing since c925ece (stage 2a) with 27 hunks across four terminal files, and
nothing local caught it because the four-command block above did not include
it. The release pipeline runs it inside every build-matrix leg, so a tag push
would have burned a ~25 min run before failing. Fixed in 1b1af25.

1806 -> 1790 is 16 tests removed, every one Windows/MSI. Verify a test-count
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

## Notes for the Windows and Linux passes

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

For these passes specifically:

- `#[cfg(unix)]` appears 162 times and macOS needs nearly all of it. Only about
  6 attribute instances combine `unix` with `not(target_os = "macos")`. Do not
  confuse unix-shared with Linux-only. This is the single highest-risk
  distinction in the remaining work. **2b left 5 of the 6 standing**, having
  stripped only their `windows` arm; 2c deletes them.
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
  `crates/paneflow-config/src/schema.rs` and in `schemas/paneflow.schema.json`.
  It is now permanently dead. Decide its fate during the config-schema work, and
  note that a drift test asserts every schema key appears in
  `docs/user/configuration/schema.md`.
- The binary-size budget at `src-app/build.rs:61` and in `run_tests.yml` is
  baselined on Linux ELF. It needs a Mach-O re-baseline, not deletion.

## Biggest remaining liability

`.github/workflows/release.yml` is still 3,177 lines of upstream's
four-platform pipeline, with live references to `GPG_*`,
`AZURE_TRUSTED_SIGNING_*` and `POSTHOG_API_KEY`. Those are inert only because
the secrets do not exist in this org. Never create them here. See the leak
register in the design doc for what each one fed upstream.

## Parallel work

Do not use the `paneflow-conductor` skill for fan-out. It is a feature of this
app and it does not work reliably, which is recorded as known defect 1 in the
design doc. Use the `grok-subagents` skill, or headless agent processes as
background jobs with one git worktree per task.

Note what does and does not fan out here. The docs pass parallelised well
because the files were disjoint. The Rust passes do not: the enum removals and
feature removals are whole-workspace edits, the same files carry Windows and
Linux and test concerns at once, and every worker would need its own multi-gigabyte
`target/` to verify anything.
