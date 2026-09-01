# PaneFlow: macOS-only fork

Date: 2026-08-25
Status: implemented; living decision record
Fork point: `arthjean/paneflow` v0.8.2, commit `f53f982291f75a9daf565827b3167d0e96925d0a`

## Purpose

Take PaneFlow under `theaamgroup` and strip it to macOS only, so The AAM Group
can make improvements and fixes to it. The repository began private and became
public when anonymous Sparkle appcast and DMG access became a release
requirement. The product name stays PaneFlow; a rebrand to PanesCLI was scoped
and dropped (see `docs/fork/2026-08-25-post-2c-plan.md`).

## Decisions

| Decision | Choice | Consequence |
|---|---|---|
| Fork model | Keep all 1035 commits, keep `upstream` remote | `git blame` works, upstream fixes are cherry-pickable, `.git` stays 60M (no history rewrite, since `filter-repo` would destroy the merge base) |
| Cut depth | Deep. Strip non-Mac `cfg` branches from shared source | Readable Mac-only source. Every future upstream merge conflicts across roughly 80 files. Accepted knowingly. |
| Ghostty backend | Delete entirely | Verified unreachable on macOS. See Verification below. |
| Self-update | **Sparkle 2, added by #119.** The deleted hand-rolled updater stays deleted | Hourly background checks, EdDSA + Developer ID verification, silent download, install on ordinary quit, no forced relaunch or update UI. No minisign and no `src-app/src/update/`. |
| Telemetry | **Deleted** (post-2c grind). Do not resurrect PostHog | Never set `POSTHOG_API_KEY`. Crate, app module, consent UI, and `build.rs` env directives are gone. |
| Branding | Product stays **PaneFlow**. The 2d rename to PanesCLI was scoped and dropped | Task 12 still replaced *upstream's* bundle id, authors and homepage. Binary, CLI, config dir, MCP server, conductor skill and `PANEFLOW_*` stay. See `docs/fork/2026-08-25-post-2c-plan.md` |
| gpui dependency | Pin `zed-industries/zed` by exact revision, keep the AAM fork only as a cold backup | `Cargo.lock` and all three Cargo dependency entries pin the revision, so the risk is availability, not drift. Never restore the old `arthjean/zed` source. |
| Apple signing | AAM Developer ID, signed and notarized DMG | Other AAM Macs can install without Gatekeeper warnings |

## Naming

The product stays **PaneFlow**. A full rebrand to PanesCLI was scoped (bundle
id, binary, CLI, config dir, MCP server, conductor skill, `PANEFLOW_*` env)
and dropped. See `docs/fork/2026-08-25-post-2c-plan.md`.

Task 12 still replaced *upstream's* identity so this fork is not signed as
`io.github.arthurdev44.paneflow`.

| Thing | Value |
|---|---|
| Product name | PaneFlow |
| Bundle identifier | `com.theaamgroup.paneflow` |
| Binary and CLI | `paneflow` |
| Config dir | `~/Library/Application Support/paneflow/paneflow.json`. Driven by `APP_SUBDIR` in `crates/paneflow-config/src/loader.rs` via `dirs::config_dir()`. NOT `~/.config`: that is the Linux path and an earlier draft of this spec had it wrong. |
| MCP server | `paneflow` |
| Conductor skill | `paneflow-conductor` |
| Env var prefix | `PANEFLOW_*` |

## Toolchain prerequisites

| Requirement | State |
|---|---|
| rustup + rustc 1.98.0 (pinned by `rust-toolchain.toml`; bumped from 1.96.1 on 2026-08-31 for the libghostty port, issue #184) | installed 2026-08-31 |
| Full Xcode, for the Metal shader compiler that GPUI needs at build time | Xcode 26.6 installed 2026-08-25. **Two separate steps, and the second is easy to miss.** Command Line Tools alone are not enough (`xcrun -f metal` fails outright under CLT), and installing Xcode alone is also not enough: Xcode 26 ships the Metal toolchain as a downloadable component, so `xcrun metal` still fails with `cannot execute tool 'metal' due to missing Metal Toolchain` until you run `xcodebuild -downloadComponent MetalToolchain`. Verify with an actual compile, not with `xcrun -f metal`, which resolves the path successfully even when the toolchain is absent. |
| cmake | already present via Homebrew |
| zig | not needed once the Ghostty backend is gone |

## Do not delete

Verified load-bearing. Each of these looks like cruft and is not.

- `schemas/paneflow.schema.json`: two tests in `crates/paneflow-config/src/schema.rs` (lines 1685, 1778) read it off disk. Drift fails the suite.
- `examples/review-pipeline.flow.toml`: `include_str!` target at `src-app/src/cli/flow_spec.rs:749`. Deleting it breaks the build. `examples/TASK.md` is its fixture.
- `clippy.toml`: the `allow-unwrap-in-tests` escape hatch for the workspace lint policy in `Cargo.toml`. Without it, test code starts warning.
- `rust-toolchain.toml`: the 1.98.0 pin. The dep graph floor is 1.92 (oo7 0.6, cosmic-text 0.17, smol_str 0.3, several wgpu crates).
- `LICENSE`: GPL-3.0-or-later, mandatory. GPUI is a Zed fork.
- `CLAUDE.md`: the single most useful file in the repo. Real build and test commands, annotated module tree, thread model, keystroke-to-pixel data flow, and a Gotchas section with hard-won GPUI behaviour.
- `ARCHITECTURE.md`, `docs/hooks.md`, `docs/mcp-bridge.md`, `docs/debugging-rendering.md`, `docs/user/configuration/schema.md`, `docs/user/scripting/reference.md`.
- `src-app/assets/fonts/`: 23M of TTFs, `rust-embed`ed into the binary.
- `assets/PaneFlow.icns`, `assets/Info.plist`, `assets/dmg-background.png`: macOS bundle inputs.

## Stage 1: file-level deletion

No Rust source touched, so this needs no compiler. Working tree drops from
roughly 60M to roughly 28M.

Packaging: `packaging/{winget,wix,debian,rpm,apt,homebrew}`, `packaging/AppRun`,
`packaging/paneflow-release.asc`, top-level `debian/`, `keys/`.

CI: `.github/workflows/{repo_publish,update_cask,libghostty-linux,libghostty-windows}.yml`.
The first two are the dangerous ones. See Leak register.

Docs: `docs/user/blog/`, `docs/user/installation/{linux,windows}.md`,
`docs/release/{linux-signing,windows-signing,windows-libghostty,libghostty-linux}.md`,
`docs/{WINDOWS,WINDOWS-SMOKE-TEST,validation-aarch64,pkg-repo-runbook,release-runbook,release-signing}.md`.

Cruft: `tasks/` (4M including a 3.8M demo mp4), `CHANGELOG.md`, `BUILD_WEEK.md`,
`ABOUT.md`, `llms.txt`, `context7.json`, `assets/images/`,
`assets/icons/master/paneflow-icon-1024.png`, `assets/PaneFlow.ico`,
`assets/paneflow.desktop`, `assets/io.github.arthurdev44.paneflow.metainfo.xml`,
`assets/badges/`, `native/libghostty/prebuilt/`.

Scripts: the 5 PowerShell files, `bundle-appimage.sh`, `bundle-tarball.sh`,
`tarball-install.sh`, `build-libghostty-linux.sh`, `verify-libghostty-package.sh`,
`test-update-e2e.sh`.

`context7.json` carries upstream's Context7 API key, so it goes regardless.

`skills/paneflow-conductor/` is deliberately NOT deleted. An earlier draft of
this list included it, which contradicted the decision to keep and rename the
conductor so it can be fixed here. It was never actually removed from the tree.

## Stage 2: Rust surgery

Blocked on Xcode. Establish a green `cargo build && cargo test` baseline first,
then work in compiler-verified passes, committing after each: Ghostty removal,
then Windows unwind, then Linux unwind, then rename, then update-feed repoint.

Scale: roughly 170 `cfg` sites for Ghostty, 30 files with Windows `cfg` blocks,
plus three enum removals (`InstallMethod::WindowsMsi`, `AssetFormat::Msi`,
`PackageManager`) that each thread through about 20 call sites.

### Ghostty removal, paired edits

Deleting the four Ghostty paths alone breaks the build on macOS too, because
Cargo resolves path dependencies for all targets. These must land together:

1. `Cargo.toml` lines 8, 9, 15: drop the three workspace members.
2. `src-app/Cargo.toml`: line 16 default features, lines 21 to 35 feature
   definitions, line 295 and line 324 path deps, packaging assets at 519 and 579.
3. Delete `src-app/src/terminal/ghostty_session.rs` (4453 lines) and
   `ghostty_stress.rs` (1099 lines). Unwind `cfg` sites in `pty_session.rs` (88),
   `view.rs` (28), `backend_corpus.rs` (21), `input.rs` (20, including
   `use paneflow_terminal_ghostty as ghostty` at line 26), `service_detector.rs`
   (13), `mod.rs` (2).
4. Delete `fuzz/`. It fuzzes the Ghostty bindings and nothing else.
5. Fix `src-app/tests/dependency_source_policy.rs` lines 59 to 96. It asserts
   `libghostty-linux` is a default feature and is the only Ghostty-related test
   that fails on macOS.
6. Decide `TerminalBackendConfig::Ghostty` in `crates/paneflow-config/src/schema.rs:575`
   and `schemas/*.schema.json:152`. Compiles fine if left, but becomes a dead
   variant with a public schema contract.

## Stage 3: signed release

macOS-only `release.yml`. Wire `APPLE_DEVELOPER_CERT_P12`,
`APPLE_DEVELOPER_CERT_PASSWORD`, `APPLE_ID`, `APPLE_APP_SPECIFIC_PASSWORD`,
`APPLE_TEAM_ID`. Signed, notarized, stapled DMG. Sparkle adds
`SPARKLE_PRIVATE_KEY` (archive-signing seed); its public half is committed as
`SUPublicEDKey` in `assets/Info.plist`. New signing values belong in the
ref-restricted `release` environment, not repository-wide secrets; migrate the
legacy `APPLE_*` values there on rotation. The 2026-08-26 hand-rolled updater
and minisign client stay deleted; do not recreate `MINISIGN_SECRET_KEY`,
`PANEFLOW_MINISIGN_*`, or `src-app/src/update/`.

## Known defects to fix in this fork

This is the reason the fork exists, so defects get recorded here as they surface
rather than living only in chat.

1. **The conductor does not work reliably.** `skills/paneflow-conductor/SKILL.md`
   ships a skill that drives a fleet of CLI coding agents living in Paneflow
   panes over the `paneflow` CLI. In practice it is janky and does not work,
   confirmed across more than one attempt. The working pattern it fails to
   replace is one headless agent process per task, launched as a background job,
   each in its own git worktree, which needs no TUI to stay alive and has no
   shared-state conflicts. Treat the pane-driving model itself as suspect, not
   just its implementation. The skill is kept and renamed rather than deleted
   precisely so it can be fixed here.

2. **Two narrow keybinding issues. Earlier drafts of this entry overstated the
   problem twice, so the evidence is spelled out here.**
   The scheme IS ported to macOS: 53 bindings in
   `src-app/src/keybindings/defaults.rs` use GPUI's `secondary` shorthand, which
   resolves to Cmd on macOS and Ctrl elsewhere (`keybindings/apply.rs:21,146`).
   `MACOS_ONLY_DEFAULTS` holding only `cmd-q` is therefore expected.
   Alt+Arrow pane focus is also NOT a defect: `option_as_meta` defaults to false
   on macOS (`src-app/src/keys.rs:83`, literally `!cfg!(target_os = "macos")`),
   but `alt_phys` is read directly off the keystroke modifiers at `keys.rs:101`
   and feeds the CSI modifier code at `:113`, so Alt+Arrow emits a correct
   sequence regardless of that flag. The code comment at `keys.rs:82` says so
   explicitly. Do not "fix" this.
   What is actually left:
   - `next_workspace` is bound to `secondary-tab` (`defaults.rs:72`), which
     resolves to Cmd+Tab on macOS, a keystroke the OS reserves for the
     application switcher, and `MACOS_ONLY_DEFAULTS` does not override it.
     **Unverified whether the keystroke reaches GPUI at all**, so this needs a
     real test before it is called broken. If it is broken, rebind.
   - The `--help` output at `src-app/src/main.rs:2728` hard-codes Linux wording
     (`Ctrl+Shift+D/E`, `Alt+Arrow`, `Ctrl+Tab`) and so misreports the real
     macOS bindings. Cosmetic but user-visible. Prefer rendering it from the
     binding table rather than duplicating it as a literal string.

3. **2,357 `US-NNN` comments across 192 `.rs` files reference PRDs that do not
   exist.** They lived under `tasks/`, were gitignored upstream, and were never
   committed. They are pure noise for anyone reading this code and should not be
   extended. The commit-message convention that minted them has been removed
   from `CLAUDE.md`.

## Leak register

Everything below points at upstream and must be cut or repointed.

1. **Done (2026-08-26), still enforced after #119.** `src-app/src/update/` and
   the old in-app updater are deleted. There is no `checker.rs`, minisign,
   `--update-and-exit`, or poll of Arthur Jean's releases. Sparkle is a new,
   framework-owned path whose feed is the AAM GitHub Release appcast.
2. **Done.** `.github/workflows/repo_publish.yml` is deleted. It used to chain
   off a successful `release` run and `rclone sync` into Cloudflare R2 at
   `pkg.paneflow.dev`, then purge his Cloudflare zone.
3. **Done.** `.github/workflows/update_cask.yml` is deleted. It used to
   git-push a version bump into the public `arthjean/homebrew-paneflow` tap.
4. Never create these secrets in the new org: `R2_*`, `CLOUDFLARE_*`,
   `HOMEBREW_TAP_DEPLOY_KEY`, `GPG_*`, `POSTHOG_API_KEY`, `AZURE_*`.
5. Author identity to scrub: `src-app/src/app/about_dialog.rs:117` reads
   "(c) Arthur Jean"; `src-app/Cargo.toml:490` maintainer email; five
   `paneflow.dev` menu links at `src-app/src/app/profile_menu.rs:23` to `:27`.
   `.github/SECURITY.md`, `.github/CONTRIBUTING.md`, and
   `.github/CODE_OF_CONDUCT.md` carried upstream URLs; all three are deleted.
   The repository becoming public for Sparkle does not restore upstream's
   contribution or advisory documents. Report problems to the repository
   owner. Agent working rules live in `AGENTS.md` / `CLAUDE.md`.

## Traps register

Found during the inventory. Each one would have cost a debugging session.

1. **Historical.** `src-app/src/update/` is deleted (2026-08-26). It used to
   declare `pub mod linux;` unconditionally, so `linux/appimage.rs` and
   `linux/targz.rs` compiled on macOS.
2. **Historical.** `src-app/src/update/macos/dmg.rs` is deleted. Its two
   `#[cfg(all(test, not(target_os = "macos")))]` test modules used to run
   only on non-macOS hosts; un-gating them would have been a duplicate
   definition of `copy_bundle_to_staging`.
3. `.github/workflows/run_tests.yml:1357` `tests_pass` aggregates every job in
   its `needs:` list. Pruning jobs without editing that list yields a workflow
   that never completes.
4. The binary-size budget at `src-app/build.rs:61` and `run_tests.yml:399` is
   baselined on Linux ELF. It needs a Mach-O re-baseline, not deletion.
5. `src-app/Cargo.toml:53` warns that `gpui_platform` MUST carry the `font-kit`
   feature on macOS. Without it the build succeeds and text renders as boxes.
6. Many `#[cfg(any(test, target_os = "windows"))]` attributes keep code alive
   for tests on all platforms. Removing the Windows arm leaves `#[cfg(test)]`,
   which makes those items test-only rather than dead.
7. `#[cfg(not(unix))]` and `#[cfg(not(windows))]` blocks are fallback arms. The
   surviving twin must be un-gated, not deleted alongside them.
8. `#[cfg(unix)]` appears 162 times and macOS needs nearly all of it. Only 6
   attribute instances combine `unix` with `not(target_os = "macos")`. Do not
   confuse unix-shared with Linux-only.
9. `src-app/src/terminal/input.rs:1100` handles X11 and Wayland PRIMARY
   selection via `cx.write_to_primary`. Those calls vanish on macOS and the
   surrounding `primary_text` plumbing goes dead.
10. Config keys `windows_terminal_material` and `windows_chrome_material` are in
    the public JSON schema. The loader is lenient about unknown keys, so
    leaving them as ignored no-ops is safer than a breaking config change.
11. Every `US-NNN` comment in the Rust source points at a PRD under `tasks/`
    that was gitignored and never committed. All 13 checked are absent. They are
    permanently dangling breadcrumbs, not documentation.
12. `.gitignore` names local-only upstream files we will never have, including
    `CMUX_ANALYSIS.md`, which `CLAUDE.md:261` cites as a 417-line reference spec.

13. **Debug and release builds read different config files.** `APP_SUBDIR` in
    `crates/paneflow-config/src/loader.rs:17` is `paneflow-dev` under
    `debug_assertions` and `paneflow` otherwise, and this applies to every
    persistence surface: config, session, threads, sockets, caches. So a
    `cargo run` build does NOT read
    `~/Library/Application Support/paneflow/paneflow.json`; it reads the
    `paneflow-dev` sibling. The doc comment at `loader.rs:42` states the release
    path as though it were universal, which makes this easy to miss. This is the
    single most likely source of confusion for a build-from-source workflow,
    which is now the only workflow here.
14. **`schemas/paneflow.schema.json` disagrees with the code on
    `option_as_meta`.** The schema declares `"default": true` (line 101) while
    `src-app/src/keys.rs:83` computes `!cfg!(target_os = "macos")`, so the real
    macOS default is false. The schema description also reads as though the user
    must set false on macOS, when macOS already defaults that way. Fix the schema,
    not the code.
15. **`schemas/paneflow.schema.json` omits the surface key `path` from
    `definitions.surface`, which sets `additionalProperties: false`,** so editors
    flag a legitimate key as invalid. The schema-drift test does not catch it
    because `path` is `skip_serializing_if`.

16. **Do not add `cargo:rerun-if-env-changed` for a variable read via
    `env!` or `option_env!`.** It looks missing and it is not. rustc emits
    `# env-dep:NAME` lines into the crate's dep-info file for those macros and
    Cargo honours them, so invalidation already works. Proven here (while the
    updater still existed): with zero directives registered, changing
    `PANEFLOW_MINISIGN_PUBKEY` from a fully warm cache still recompiled
    `paneflow-app`. The PostHog `build.rs` directives (`POSTHOG_API_KEY` /
    `POSTHOG_HOST`) and the minisign pubkey bake are **gone** with telemetry
    and the old updater; do not resurrect them. Sparkle's public key lives in
    `Info.plist` and is not read by Rust compilation.

    Method note, since it generalises: the first attempt to verify this
    "confirmed" the bug, because backing the fix out edited `build.rs` in the
    middle of the sequence and thereby changed two variables at once. Any
    build-cache experiment has to warm the cache to a steady state first, then
    change exactly one thing. A control that reproduces the positive result is
    telling you the experiment is broken, not that the finding is doubly true.

17. **Ungated Windows strings (2026-08-26, #103).** Windows
    executable-suffix / PowerShell string handling is **stripped** in
    ported crates, not cfg-gated. A cfg-predicate census cannot see this
    class. `scripts/linux-census.sh` reports a separate ungated-string
    check (`powershell` / `.exe` / `.cmd` / `.bat` / `.ps1` / `\\?\` /
    `%APPDATA%`) as the detector; that count is not part of the STAGE 2c
    zero-condition integer. A Windows-shaped command in a settings file
    is no longer recognized on macOS, which is fine because nothing in
    this fork can write one.

## Verification: Ghostty is unreachable on macOS

Proven on 2026-08-25, then the remaining compiled stubs were deleted in
leftover-removal bucket 2 (2026-08-26). There is no Ghostty backend, no
`auto_selects_ghostty_for_target`, no `should_start_ghostty`, no
`GhosttySession`, and no `GhosttyBuildDiagnostics`. `TerminalBackendConfig`
is `Auto | Alacritty` only. The loader still maps leftover
`"backend": "ghostty"` to Alacritty so old `paneflow.json` files load. No
env var selects a backend.

### SearchEngine lift (2026-08-26, issues #91 / #97)

Upstream v0.9.0 promoted `paneflow-terminal-ghostty` from an optional,
target-gated backend to an unconditional core dependency, and `74dcca2`
made the shared find-in-buffer path import `SearchEngine` from it
regardless of backend. Rather than restore the crate, the engine was
**lifted into `src-app/src/search_engine.rs`**: the ~150-line
pure-`regex` core of upstream's file with no libghostty linkage,
coupled to the host crate only through
`GhosttyError`, `Point`, `SearchMatch` and `SearchResult`. `Point` and
`SearchMatch` already existed here field-identically
(`terminal/types.rs`, `search.rs`), so upstream's `from_shared_result`
translation layer was dropped and `GhosttyError` collapsed to the single
variant the engine can raise.

Three things were deliberately **not** taken:

- `MAX_SEARCH_CELLS` (12M cells) and the `SearchChunk` chunk driver. The
  budget never fires at our 10 000-line scrollback (~2M cells) and it was
  the only `cfg(target_os = "linux")` site in the upstream file. Skipping
  it is what keeps `./scripts/linux-census.sh` at zero. `SearchResult`
  therefore has no `truncated` field and the match counter never shows
  `n/m+`.
- The render-thread-freeze framing of `74dcca2`. That freeze was
  Ghostty's blocking mailbox round-trip with a 1 s `recv_timeout`; our
  path was already off-thread.
- `TerminalView::appearance_theme_generation` /
  `backend.refresh_appearance()`, which rode along in the same commit and
  are Ghostty engine calls.

What was taken: the combining-character extraction
(`Cell::zerowidth()`, verbatim - a real correctness bug that also fixed
`surface.search` and the MCP bridge's `search_pane`, which share the
extractor), and cooperative cancellation, extended past upstream to the
fleet-wide fan-out in `app/fleet_search.rs` - up to 640 sequential
full-grid scans that upstream left uncancellable.

`search_engine.rs` sits at the crate root, outside `terminal/`, and
imports no `alacritty_terminal`. It is therefore NOT in the
`alacritty_confined_to_backend_allowlist` ALLOWLIST
(`terminal/types.rs:997`) and must never be added: if the engine ever
needs an alacritty type, the code belongs back in `search.rs`.

Expect this lift again. Upstream is entangling Ghostty deeper into shared
code each release; the pattern is "lift the pure part, drop the gated
part", not "restore the crate".

## Known costs

- Deep cut plus upstream merges means conflicts across roughly 80 files on every
  `git fetch upstream` merge. This was chosen with the tradeoff stated.
- `.git` stays at 60M. Roughly 43M of the pack is deletable blob history
  (marketing images, master icons, the demo mp4, prebuilt static libs), but
  reclaiming it needs `filter-repo`, which rewrites every SHA and destroys the
  upstream merge base.
- A product-name rebrand (scoped and dropped; see
  `docs/fork/2026-08-25-post-2c-plan.md`) would orphan any existing local
  PaneFlow config and require re-registering the MCP server and the conductor
  skill. That cost is one reason it did not happen.
