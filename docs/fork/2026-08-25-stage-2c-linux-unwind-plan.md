# Stage 2c: Linux unwind + updater collapse

Plan of record. Written 2026-08-25, immediately after stage 2b landed.

Read `docs/fork/STATE.md` and `CLAUDE.md` first. This file does not repeat them.

## Decisions already made (do not relitigate)

| Question | Decision |
|---|---|
| Updater scope | **Full collapse to DMG-only.** Delete `update/linux/`, collapse `InstallMethod` and `AssetFormat` to the macOS-reachable set. Same pattern as 2b's MSI cascade. |
| `not(unix)` / `not(macos)` | **Folded into 2c.** All three predicate families die in this stage. After 2c the only platform predicates left are `cfg(unix)` and `cfg(target_os = "macos")`. |
| Delegation | **Inventory fans out, execution mostly orchestrator.** ~6 read-only grok agents for inventory; 2-3 delegated batches for genuinely file-local work; enum collapse, unix-adjacent sites, `cfg!` sweep and `not(unix)` all done by the orchestrator. |
| Config-schema debt | **Not in 2c.** Its own stage between 2c and the 2d rename. |
| Branch | `mac-only-fork` directly, one commit per compiler-verified increment. |

## Measured scope (taken 2026-08-25, post-2b)

```
target_os = "linux"        78 sites / 20 files
not(target_os = "macos")   26 sites
not(unix) fallbacks        31 sites
--------------------------------------------
cfg(unix)                 216 sites  <-- STAYS. macOS IS a unix.
```

Combined per-file weight (linux + not(macos) + not(unix)), heaviest first:

```
19  workspace/ports.rs              5  terminal/pty_session.rs
10  app/constants.rs                5  terminal/input.rs
10  paneflow-shim/src/hooks.rs      5  keybindings/display.rs
 9  paneflow-shim/src/main.rs       5  app/self_update_flow.rs
 7  app/workspace_ops/mod.rs        4  app/event_handlers.rs
 6  terminal/backend_corpus.rs      3  ports/pid_resolve, update/mod,
 6  main.rs                            keybindings/defaults, ipc.rs,
                                       app/bootstrap, agents/notifications
```
plus ~17 files carrying 1-2 sites each.

Updater enum surface:

```
PackageManager      119 refs /  7 files      AppImage   110 refs / 11 files
SystemPackage        49 refs /  9 files      TarGz       40 refs /  8 files
AppBundle            26 refs /  8 files      ExternallyManaged 13 refs / 4 files
```

## Blocking question, settle this FIRST

**Is `InstallMethod::TarGz` reachable on macOS?**

`CLAUDE.md` says yes: "The tarball install method is genuinely reachable on
macOS (`$HOME/.local/paneflow.app/`); the AppImage one is not."

The code appears to say no. `classify()` checks the `.app` bundle structurally
at **step 0**, before everything else, and `install_method.rs:615` carries this
comment on a `#[cfg(target_os = "linux")]`-gated test:

> Linux-only: on macOS the `.local/paneflow.app` suffix collides with the
> `.app` bundle detector and the classifier returns `AppBundle` instead of
> `TarGz`.

**This is read, not observed.** Settle it with an actual test before writing any
code — write a temporary macOS-enabled test that calls `classify()` with a
`$HOME/.local/paneflow.app/bin/paneflow` path and assert which variant comes
back. The answer decides whether `update/linux/targz.rs` (~1,100 lines) and
`AssetFormat::TarGz` survive 2c or die with the rest.

If TarGz is unreachable, `CLAUDE.md` is wrong and must be corrected in the same
pass.

## Target end state

```rust
pub enum InstallMethod {
    AppBundle { bundle_path: PathBuf },     // the DMG updater
    ExternallyManaged { explanation: String },
    Unknown,
    // TarGz { app_dir } only if the blocking question says it is reachable
}

pub enum AssetFormat {
    Dmg,
    // TarGz likewise
}
```

**Keep `ExternallyManaged`.** It is driven by the `PANEFLOW_UPDATE_EXPLANATION`
env var (`install_method.rs:134`), not by platform, so it stays reachable on
macOS — a future Homebrew cask is exactly the case it exists for. Strip only its
Flatpak/Snap detection arms.

**Delete outright:** `AppImage`, `SystemPackage`, the whole `PackageManager`
enum, `update/linux/{mod,appimage,targz?,system_package}.rs`,
`update/migrations.rs`, `window_chrome/linux_backdrop.rs`.

## The leak, already confirmed live

`update/mod.rs:38` declares `pub mod linux;` **unconditionally** — the exact
pattern that let `update/windows/msi.rs` compile on macOS through all of 2a and
2b. It is not theoretical here either: **13 `update::linux::*` tests execute on
macOS in the current test run.** Confirm with:

```bash
cargo test --workspace 2>&1 | grep -c '^test update::linux::'
```

`window_chrome/mod.rs:9` gates `linux_backdrop` properly, but stage 2b found
`window_chrome/backdrop.rs` orphaned on disk after its `mod` line was removed.
**When you delete a module declaration, delete the file in the same commit** and
re-run the census, which now catches orphans only via the different-term-space
sweep, not the cfg scan.

## Why 2c is harder than 2b, not easier

2b's risk was deleting a `not(windows)` fallback twin. **2c's risk is the
opposite and worse:** `cfg(unix)` is the *live* arm, it appears 216 times, and
it sits directly beside the 78 Linux sites. A worker that pattern-matches on
"platform gate near Linux code" will delete macOS's own PTY, IPC, signal and
filesystem paths. Every brief must lead with:

> `#[cfg(unix)]` is TRUE on macOS. macOS IS a unix. Never remove one.
> `unix` is not `linux`. Only `all(unix, not(target_os = "macos"))` is Linux.

The 5 surviving `all(unix, not(macos))` sites (2 in `update/mod.rs`, 3 in
`agents/notifications.rs`) had only their Windows arm stripped in 2b. They are
now pure Linux and die in 2c.

## Phases

**Phase 0 — baseline + blocking question.** Run all five gates and quote real
output. Settle the TarGz question with an executed test. Do not start on a red
tree; `cargo fmt --check` is now one of the five and was red for all of 2a.

**Phase 1 — inventory (read-only, ~6 grok agents, no worktrees).** Shard the 40
files. Classification schema must separate, as its own categories:
`LINUX_DELETE`, `NOT_UNIX_UNGATE_OR_DELETE`, `NOT_MACOS_REDUCE`,
`UNIX_KEEP` (loudly), `RUNTIME_CFG_MACRO` (report only, never edit),
`ENUM_CASCADE` (report only). Require file:line and the exact cfg expression.

This phase is **not optional and must run on grok**, not on Claude's own Agent
tool. It is the part of 2b that worked best: 9 agents returned 302 occurrences
with exactly one medium-confidence item and no file-scope violations, and the
agents independently found four things the orchestrator's census had missed
(a target-triple string check, an `any(not(windows), debug_assertions)`
polarity, a `not(any(macos, windows))` polarity, and the `all(test, windows)`
vs `any(test, windows)` distinction). Launch all shards concurrently as
background jobs in a single message.

**Phase 2 — 2-3 delegated grok batches**, disjoint file sets, each in a `git
worktree` seeded by `cp -c -R target` (APFS clone: 6.7s, ~0 bytes, verified in
2b — a cold GPUI build is 15-25 min, the clone rebuild is 30s). Delegate only
files with no `cfg(unix)` neighbours and no enum references. Candidates:
`keybindings/{display,defaults,apply}.rs`, `widgets/`, `terminal/backend_corpus.rs`.

"Mostly orchestrator" means the *hard* sites stay with you — it does not mean
skipping delegation. Run the batches on grok.

## Grok mechanics (verified working in 2b)

Use the `grok-subagents` skill. Call the real binary — bare `grok` resolves to
Paneflow's own PATH shim, which churns telemetry hooks on every invocation.

Inventory shard (read-only, structured output — this shape worked):

```bash
GROK="$HOME/.grok/bin/grok"
"$GROK" --prompt-file "$OUT/S1-ports.md" \
  --cwd /Users/dayers/Github/paneflow \
  --max-turns 60 \
  --disallowed-tools "Edit,Write,write,edit" \
  --json-schema "$(cat $OUT/schema.json)" \
  > "$OUT/out-S1.json" 2>"$OUT/err-S1.txt"
```

Read results with `jq '.structuredOutput'`. Never parse prose.

Execution batch (mutating — `--worktree` is IGNORED under `-p`, so make it
yourself):

```bash
git worktree add /Users/dayers/Github/pf-B1 -b linux/b1-keybindings
cp -c -R target /Users/dayers/Github/pf-B1/target      # APFS clone, ~0 bytes
"$GROK" --prompt-file "$OUT/exec-B1.md" \
  --cwd /Users/dayers/Github/pf-B1 \
  --max-turns 80 \
  --json-schema "$(cat $OUT/schema-exec.json)" \
  > "$OUT/exec-out-B1.json" 2>"$OUT/exec-err-B1.txt"
```

Launch every job with `run_in_background: true`, one Bash call per unit, all in
a single message so they run concurrently. The harness re-invokes you as each
exits — do not poll.

Rules learned the hard way:

- Grok inherits **none** of this session's context. Every brief must be
  self-contained: absolute paths, the exact rule to apply, the output contract.
- `permission_mode = "always-approve"` is global, so headless grok runs tools
  without prompting. Restrict read-only phases with `--disallowed-tools`.
- **Tell execution agents to run `cargo fmt` BEFORE `cargo clippy`.** 2b's brief
  said fmt last, so rustfmt collapsed a surviving block onto one line and
  `unused_braces` fired where no agent could see it.
- **`--json-schema` suppresses the tool loop on open-ended tasks.** It is right
  for inventory ("read these listed files, classify each site") because the work
  is bounded. It failed three times for an adversarial audit ("go find what I
  missed"): grok returned `num_turns: 1` with a fabricated verdict and zero
  searches. Run exploratory audits yourself, or without the schema.

**Phase 3 — orchestrator, sequential.** The enum collapse, every file where
Linux and unix gates interleave (`ports.rs`, `pty_session.rs`, `ipc.rs`,
`shim/hooks.rs`, `shim/main.rs`), all `not(unix)` arms, all `cfg!()` runtime
macros, and integration of the Phase 2 batches one at a time with the full gate
set between each.

## Verification

Reuse `scripts/win-census.sh` — copy it to `scripts/linux-census.sh` and swap
the predicate. It already carries the four things that made 2b trustworthy, and
**the last two are the ones that actually caught bugs**:

1. Negative control before trusting a zero.
2. Comment-only lines counted separately, non-blocking.
3. **Multi-line pass** (paren-balancing across newlines). Line greps cannot see
   `cfg!(any(\n target_os = "linux", ...))`. One real site hid there in 2b.
4. **Different-term-space sweep.** For Linux: `/proc`, `/sys`, `XDG_`,
   `.desktop`, `AppImage`, `dpkg`, `rpm`, `apt`, `dnf`, `pkexec`, `polkit`,
   `flatpak`, `snap`, `zsync`, `systemd`, `wayland`, `x11`, `dbus`, `notify-rust`
   hints, `ostree`. Re-running the cfg grep only reproduces its own blind spots;
   this is what found the orphaned file in 2b.

Gate set, before and after every increment, quoting real output:

```bash
cargo build                                # exit 0
cargo test --workspace                     # expect 1790 minus removed linux tests
cargo clippy --workspace --all-targets     # exit 0, compare WARNING COUNT to 1
cargo fmt --check                          # exit 0
./target/debug/paneflow --version
./scripts/linux-census.sh                  # must reach 0
```

**Clippy exits 0 with warnings.** "clippy exit=0" is not evidence. Compare the
warning count against the single known `block v0.1.6` notice.

**Verify any test-count change by diffing test NAMES, never the count:**

```bash
grep -oE '^test [a-zA-Z0-9_:]+ \.\.\.' <log> | sed 's/^test //; s/ \.\.\.$//' | sort
```

Every removed test must be identifiably Linux/AppImage/package-manager. In 2b
this caught nothing wrong but proved all 16 removals were intentional, which a
count alone cannot.

## Traps carried forward from 2b

- **Agent "green" claims are not evidence.** One 2b batch reported green from a
  clippy run that predated its own final edits. Re-verify every batch yourself
  at integration.
- **Do not mutate the tree while read-only inventory agents are scanning it.**
  Doing so in 2b invalidated one shard's line numbers and forced a re-run.
- **Tell agents to run `cargo fmt` BEFORE clippy, not after.** 2b's brief said
  fmt last, so rustfmt collapsed surviving blocks onto one line and made
  `unused_braces` fire where no agent could see it.
- **Headless grok ignores its tool loop under `--json-schema`** — three audit
  attempts returned `num_turns: 1` with a fabricated verdict and zero searches.
  Use structured output for inventory (it worked, 302 clean occurrences) but run
  adversarial audits yourself, or without the schema and parse prose.
- `#[cfg(any(test, target_os = "linux"))]` reduces to `cfg(test)`. Its inverted
  twin `any(test, not(linux))` becomes **unconditional**. Expect
  `not(any(macos, linux))` and `any(not(linux), debug_assertions)` too — 2b hit
  four distinct polarities and each reduces differently.

## Follow-on stages

- **Config-schema pass** (next, before the rename): `TerminalBackendConfig::Ghostty`,
  the `windows_terminal_material` / `windows_chrome_material` no-op fields.
  `schema.rs` + `schemas/paneflow.schema.json` + `docs/user/configuration/schema.md`
  must move together — a drift test enforces it.
- **2d rename to PanesCLI**: 273 files (202 `.rs`, 40 `.md`, 13 `.toml`). One
  atomic pass across prose and code.
- **Stage 3 signed release**: `release.yml` is still 3,177 lines of upstream's
  four-platform pipeline with live `GPG_*`, `AZURE_TRUSTED_SIGNING_*` and
  `POSTHOG_API_KEY` references. Also needs the Mach-O re-baseline of the
  binary-size budget (`src-app/build.rs`, `run_tests.yml`), which is still
  calibrated on a Linux ELF.
