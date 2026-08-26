# PaneFlow release runbook

Step-by-step checklist for cutting a new PaneFlow release. Written for the
maintainer coming back cold after a month away: every step has a time budget, a
clear pass/fail signal, and a "what to do if it breaks" box.

**Target total time: about 40 minutes for a happy-path release**, most of it
spent waiting on Apple's notarization queue. If a step pushes you past its
budget, check that step's troubleshooting box before plowing on. The runbook has
probably already anticipated the failure.

**Last validated on:** _never. This fork has not cut a release yet. Update this
line after the first one._

Related runbooks:

- [`docs/release-signing.md`](./release-signing.md) explains what signs what, and
  which secrets must never exist in this org.
- [`docs/release/macos-signing.md`](./release/macos-signing.md) is the deep dive
  on codesign, entitlements, notarization, and the DMG.
- [`docs/self-update-signing.md`](./self-update-signing.md) covers the minisign
  keypair and its two-slot rotation.

Prerequisites (one-time, not part of the per-release cadence):

- `gh` CLI authenticated with `repo` scope (`gh auth status`).
- GitHub **secrets** and **variables** populated as in
  [Required secrets and variables](#required-secrets-and-variables) below.
  The user adds these; the workflow does not create them.
- A local macOS build environment: full Xcode plus the separately downloadable
  Metal toolchain. See the build prerequisites in `CLAUDE.md`.

The release workflow is a single signed `aarch64-apple-darwin` lane:

| Job id | Runner | What it does |
|---|---|---|
| `build` | `macos-15` | `cargo fmt --check`, clippy, release binary, `.app`, Developer ID sign, notarize, staple, DMG, `.sha256` sibling |
| `release` | `macos-15` | minisign-sign the DMG, create the GitHub Release. Runs only on a `v*` tag push. Uses the `release` environment. |

There is no Intel (`x86_64-apple-darwin`) leg. Dry-run is `workflow_dispatch` on
the same `build` job; it skips `release`.

Never create `GPG_*`, `AZURE_*`, or `POSTHOG_API_KEY` in this org. Those names
fed upstream's Linux packages, Windows Authenticode, and product analytics.

---

## Required secrets and variables

Populate these before the first tag. The first real tag is the end-to-end
test of this path; a dry-run without secrets will build an unsigned `.app` and
stop there.

### Secrets the workflow reads

| Secret | Where | Used by | What it is |
|---|---|---|---|
| `APPLE_DEVELOPER_CERT_P12` | repo Actions secrets | `scripts/sign-macos.sh` | Base64 of the exported Developer ID Application certificate + private key (`.p12`). The `P12` suffix is PKCS#12, not a truncated name — see below. |
| `APPLE_DEVELOPER_CERT_PASSWORD` | repo Actions secrets | `scripts/sign-macos.sh` | Password set when exporting that `.p12`. Independent of the cert blob. |
| `APPLE_ID` | repo Actions secrets | `scripts/notarize-macos.sh` | Apple Developer account email |
| `APPLE_APP_SPECIFIC_PASSWORD` | repo Actions secrets | `scripts/notarize-macos.sh` | App-specific password from appleid.apple.com, **not** the Apple ID login password |
| `APPLE_TEAM_ID` | repo Actions secrets | both Apple scripts | 10-character Team ID. `sign-macos.sh` hard-fails if the identity's common name does not contain `(TEAMID)`. |
| `MINISIGN_SECRET_KEY` | **`release` environment** secret, not repo-scope | `release` job | Entire contents of the unencrypted minisign `.key` (`minisign -G -W`). Keep it on the environment so a required reviewer can gate publish. |
| `GITHUB_TOKEN` | injected by Actions | `gh release view` / `gh release edit` | Do **not** create this. The workflow requests `permissions: contents: write` so the default token can attach assets and undraft the release. |

### Variables (public, not secrets)

| Variable | What it is |
|---|---|
| `PANEFLOW_MINISIGN_PUBKEY` | Base64 second line of the minisign `.pub`, baked into the binary at build time via `option_env!` |
| `PANEFLOW_MINISIGN_PUBKEY_NEXT` | Empty except during a key rotation |

### `APPLE_DEVELOPER_CERT_P` diagnosis: split, not a typo

A grep for `APPLE_DEVELOPER_CERT_P` hits `APPLE_DEVELOPER_CERT_P12` because
`P12` starts with `P`. That is **not** a truncated secret name and **not** a
third value that got split across a line wrap.

The signing script has always taken two independent inputs:

1. `APPLE_DEVELOPER_CERT_P12` — the PKCS#12 file, base64-encoded. `P12` is the
   format (`.p12`).
2. `APPLE_DEVELOPER_CERT_PASSWORD` — the passphrase that decrypts that file.

Do not create `APPLE_DEVELOPER_CERT_P`. Creating only the password, or only a
name that stops at `_P`, will pass a presence check you wrote yourself and then
fail at `security import` twenty minutes into a tagged run.

Onboarding steps for the Apple half: [`docs/release/macos-signing.md`](./release/macos-signing.md) §6.
Minisign keygen: [`docs/self-update-signing.md`](./self-update-signing.md).

---

## Supported release targets

Authoritative status of every target this fork ships. Cross-reference
`.github/workflows/release.yml` (`build` + `release` jobs).

| Target | Status | Ships | Gate level | Restore requires |
|---|---|---|---|---|
| `aarch64-apple-darwin` | **Active** | `.dmg` | Hard-required (gates the whole release) | - |
| `x86_64-apple-darwin` | **Closed** | - | Not in the workflow | Reopen only when Intel DMG signing is provisioned and someone actually needs an Intel build |

**Interpretation:**

- *Hard-required*: a failure blocks the release. The `release` job's `needs:
  [build]` waits for a green result.
- *Closed*: deliberately not in the workflow. Do not silently re-add a closed
  target. Adding one back requires a committed artifact path, a signing path, a
  docs update, and a release-gate decision in the same change.

This fork is macOS only. Linux and Windows targets are not "closed pending
work", they are out of scope, and their packaging, signing, and publishing paths
have been deleted rather than disabled.

---

## Step 1 - Pre-flight (about 3 min, manual judgement required)

Work on `main`. All changes for this release must already be merged.

```bash
git switch main
git pull --ff-only
git status                       # working tree clean? if not, stop.
cargo fmt --check
cargo clippy --workspace -- -D warnings
cargo test --workspace
```

**Manual judgement:** read the test output, not just the exit code. `cargo test`
exits 0 even when a test is marked `#[ignore]` and quietly skipped. Scan the
summary for unexpected ignores, new warnings, or flaky tests that passed this run
but failed the previous one.

Bump the version. The single source of truth is the workspace `Cargo.toml`:

```bash
# Run this block from the repository root. The `sed` uses a relative path and
# silently misbehaves if CWD is a subdirectory.
cd "$(git rev-parse --show-toplevel)"

# Set the new version ONCE, then reuse via the shell var. Pasting the block
# verbatim without setting VERSION fails loudly: the guard below enforces
# a valid semver.
VERSION="X.Y.Z"   # <-- EDIT THIS before running. No `v` prefix.
[[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] \
  || { echo "invalid VERSION='$VERSION' (expected N.N.N)"; return 1 2>/dev/null || exit 1; }

# Only the workspace root Cargo.toml carries a literal `version = "..."`;
# src-app/Cargo.toml and crates/*/Cargo.toml use `version.workspace = true`
# and inherit automatically. This sed targets the workspace root only.
# Note: BSD sed needs the empty -i argument.
sed -i '' "s/^version = \".*\"$/version = \"$VERSION\"/" Cargo.toml

# Let cargo rewrite Cargo.lock for the new version
cargo build -p paneflow-app --release 2>/dev/null || true
cargo check --workspace

# Commit the bump
git add Cargo.toml Cargo.lock
git commit -m "chore: bump version to v$VERSION"
git push origin main
```

Pass signal: the commit lands via a green push (pre-commit hooks or required
checks do not block).

### Troubleshooting - Step 1

| Symptom | Top 3 recoveries |
|---|---|
| `cargo test` fails on a flaky test | 1. Re-run the specific test with `cargo test <name> -- --nocapture`. 2. If genuinely flaky, file an issue and mark `#[ignore]` in a separate commit BEFORE tagging. Do not tag a known-broken release. 3. If the failure is real, fix it and restart Step 1. |
| Working tree not clean (leftover unstaged changes) | 1. `git stash` to park the noise. 2. `git diff` to audit each change: uncommitted work from a different branch should be committed or stashed, never force-discarded. 3. Only after `git status` is clean do you proceed. |
| `sed -i` errors with `invalid command code` | You used the GNU form. BSD sed on macOS requires an explicit empty backup suffix: `sed -i '' ...`. |

---

## Step 2 - Tag and push (about 1 min)

```bash
# Tag the bump commit with an annotated tag
git tag -a vX.Y.Z -m "Release vX.Y.Z"
git push origin vX.Y.Z
```

**Pre-release convention:** if you want to validate on real hardware before
promoting to `latest`, tag as `vX.Y.Z-rc.1` first. The `release.yml` workflow
matches `rc`/`beta`/`alpha` in the tag name and publishes as `prerelease: true`.
After validation, retag as `vX.Y.Z` to cut the final release.

### Troubleshooting - Step 2

| Symptom | Top 3 recoveries |
|---|---|
| Tag already exists on the remote | 1. Do NOT `git push --force`: that overwrites an immutable release marker and breaks anyone's `git fetch`. 2. Decide if the existing tag should be consumed as-is (rare) or if you need a new version number (usual). 3. If a broken tag was pushed and the release workflow produced bad artifacts, bump to the next patch and re-tag. The dud release stays on GitHub as a historical record. |
| Push rejected because the branch moved after you committed | 1. `git pull --rebase origin main`, then re-inspect `git log` to confirm your bump commit is still the tip. 2. Re-run `cargo test` locally: a racing merge may have introduced conflicts that Git resolved cleanly but the test suite does not. 3. Only then `git push` and re-`git push origin vX.Y.Z`. |
| Pushed the tag but forgot to commit the version bump | 1. **IRREVERSIBLE for anyone who already fetched.** `git tag -d vX.Y.Z` locally and `git push --delete origin vX.Y.Z` to remove the remote tag. 2. If the release workflow already created a GitHub Release object, delete that separately: `gh release delete vX.Y.Z --cleanup-tag --yes`. 3. Make the version-bump commit, re-tag on the new commit, push. |

---

## Step 3 - Monitor release.yml (about 30 min, manual judgement required)

```bash
# `gh run list --branch=` does NOT match tag refs. For tag-triggered workflows
# we filter by event=push and take the most recent run. The tag name is
# informational only, to sanity-check that the run you are watching is the one
# you just pushed.
RUN_ID=$(gh run list --workflow=release.yml --event=push --limit=1 \
          --json databaseId,headBranch --jq '.[0].databaseId')
gh run watch --exit-status "$RUN_ID"
```

Wall-clock is dominated by notarization, not compilation. The build itself is
roughly 10 to 15 minutes on a `macos-15` runner with Xcode 16.4
(`DEVELOPER_DIR=/Applications/Xcode_16.4.app/Contents/Developer`). The Metal
preflight proves the compiler with `xcrun metal --version` (never `xcrun -f
metal` / `xcrun --find metal`, which succeed when the toolchain component is
absent) and falls back to `xcodebuild -downloadComponent MetalToolchain`.
`notarize-macos.sh` then polls Apple every 30 seconds with a **hard 90-minute
ceiling**, so a busy Apple queue can stretch this step well past its budget
without anything actually being wrong.

The `build` job runs, in order:

1. `cargo fmt --check` (hard fail; the cheapest guard against burning a tagged
   run).
2. `cargo clippy --workspace --locked --target aarch64-apple-darwin -- -D warnings`.
3. `cargo build --release --target aarch64-apple-darwin`, with
   `PANEFLOW_MINISIGN_PUBKEY` (and `_NEXT`) in the environment so `option_env!`
   bakes the update-verification keys into the binary.
4. `scripts/bundle-macos.sh` produces `dist/PaneFlow.app`.
5. `Detect macOS signing secrets`. On a tag push, a missing `APPLE_*` secret is
   a hard failure here, not a downgrade to unsigned. Dry-run may continue
   unsigned and uploads `dist/PaneFlow.app` as a workflow artifact.
6. `scripts/sign-macos.sh` codesigns it (nested dylibs, nested executables,
   parent seal) with the hardened runtime and the release entitlements.
7. `scripts/notarize-macos.sh` zips it with `ditto`, submits to `notarytool`,
   polls, staples the ticket, and runs `spctl --assess`.
8. `scripts/create-dmg.sh` builds
   `paneflow-<semver>-aarch64-apple-darwin.dmg` and independently re-verifies
   `codesign`, `stapler validate`, and `spctl` against the bundle mounted from
   the finished image. A `.sha256` sibling is staged next to it.
9. The `release` job (tag-push only, `release` environment) minisign-signs the
   DMG and creates the GitHub Release.

**Manual judgement:** a green run with a `::warning::` annotation on the signing
or notarization leg deserves a read before you proceed. A warning there is often
a near-miss that would have shipped something users cannot open.

### Troubleshooting - Step 3

| Symptom | Top 3 recoveries |
|---|---|
| `::error title=macOS signing required::One or more APPLE_* secrets are missing` | 1. The run log names which check failed. Re-populate from the password manager per [`docs/release/macos-signing.md`](./release/macos-signing.md) §6. 2. Secrets are routed through `env:` so an empty secret reads as empty rather than as a literal expression: an empty value and an absent value behave the same. 3. Re-run the failed job. Do not re-tag. |
| `error: signing identity team ID does not match APPLE_TEAM_ID` | 1. `APPLE_TEAM_ID` does not appear as `(TEAMID)` inside the certificate's common name. 2. Either the secret is stale or the `.p12` was minted under a different team. 3. Fix the mismatched half and re-run the job. |
| Notarization hits the 90-minute ceiling | 1. The script prints the submission ID and the exact `xcrun notarytool info` recovery command. Poll the existing submission rather than re-tagging. 2. If it eventually reports `Accepted`, staple by hand with `xcrun stapler staple` and re-run only the DMG and publish steps. 3. Apple queue backlogs over an hour do happen and are not a repo problem. |
| Notarization returns `Invalid` | 1. The step dumps `xcrun notarytool log`. Read the actual rejection reason before changing anything. 2. `The binary is not signed` means a nested binary escaped the walk: run `codesign --verify --deep --strict --verbose=2` on the local bundle. 3. `requests the com.apple.security.get-task-allow entitlement` means a dev-entitlements build reached Apple. |

---

## Step 4 - Verify artifacts attached (about 2 min)

```bash
gh release view vX.Y.Z --json assets --jq '.assets[].name' | sort
```

Expected assets:

```
paneflow-X.Y.Z-aarch64-apple-darwin.dmg
paneflow-X.Y.Z-aarch64-apple-darwin.dmg.minisig
paneflow-X.Y.Z-aarch64-apple-darwin.dmg.sha256
```

The `-apple-darwin` suffix is a contract, not cosmetics: the in-app updater's
asset matcher looks releases up by that suffix
(`src-app/src/update/checker.rs`). A missing or renamed asset breaks self-update
for every installed client.

The `.minisig` is not optional. Clients with an embedded public key **refuse**
an update asset that has no detached signature, so publishing without it ships a
release nobody can auto-update to.

Verify the signature yourself before announcing:

```bash
gh release download vX.Y.Z --pattern '*.dmg' --pattern '*.dmg.minisig'
minisign -V -p pub.txt -m paneflow-X.Y.Z-aarch64-apple-darwin.dmg
# -> "Signature and comment signature verified"
```

### Troubleshooting - Step 4

| Symptom | Top 3 recoveries |
|---|---|
| DMG present but no `.minisig` sibling | 1. `MINISIGN_SECRET_KEY` is missing from the `release` environment, or the signing step was skipped. 2. Do NOT announce: installed clients will fail closed on this release. 3. Re-run the publish job after populating the secret; it re-uploads without re-tagging. |
| Asset name has the wrong suffix | 1. The staging step in `release.yml` renames to the canonical form; a missing rename is a workflow regression. 2. Do NOT publish: the updater will not find the asset and user upgrades break. 3. Patch the staging step, cut a new patch tag. |
| Pre-release ended up on `latest` | 1. The tag contains `rc`/`beta`/`alpha` but the workflow's prerelease boolean is false. Check the `contains(...)` expression in `release.yml`. 2. Manually flip it: `gh release edit vX.Y.Z --prerelease`. 3. Fix the workflow expression in a follow-up commit. |
| `minisign -V` fails locally | 1. Confirm `pub.txt` is the current public key, not a rotated-out one. 2. Confirm you downloaded the `.minisig` for the same asset (not a stale local copy). 3. If both are right, the artifact and its signature genuinely disagree: do not ship, and treat it as a possible key or pipeline compromise per [`docs/self-update-signing.md`](./self-update-signing.md) §6. |

---

## Step 5 - Install smoke test on a clean Mac (about 5 min)

CI already verified the signature chain three times (in `sign-macos.sh`, in
`notarize-macos.sh`, and again against the mounted image in `create-dmg.sh`).
This step exercises the **published** release from a user's perspective: does the
DMG a user actually downloads open without a Gatekeeper fight?

Do it on a Mac that has never built this project, or at minimum from a fresh
download so the quarantine attribute is present. A locally built bundle carries
no quarantine flag and will pass even when a real download would not.

```bash
gh release download vX.Y.Z --pattern '*.dmg'
xattr -p com.apple.quarantine paneflow-X.Y.Z-aarch64-apple-darwin.dmg
# ^ expect a value. No quarantine attribute means this is not a realistic test.

hdiutil attach paneflow-X.Y.Z-aarch64-apple-darwin.dmg
spctl --assess --type exec --verbose "/Volumes/PaneFlow/PaneFlow.app"
xcrun stapler validate "/Volumes/PaneFlow/PaneFlow.app"
hdiutil detach "/Volumes/PaneFlow"
```

Then do it by hand, because the CLI checks do not exercise Gatekeeper's UI path:
open the DMG, drag to `/Applications`, double-click. Expected: no prompt, app
launches, `paneflow --version` reports the tagged version.

### Troubleshooting - Step 5

| Symptom | Top 3 recoveries |
|---|---|
| `"PaneFlow" cannot be opened because the developer cannot be verified` | 1. The notarization ticket did not staple. `xcrun stapler validate` on the bundle to confirm. 2. Check the notarize step's log for a submission that reported `Accepted` but whose staple silently failed. 3. Staple and re-upload, or cut a patch release. Do not tell users to right-click-open. |
| `spctl --assess` says `rejected` on a stapled bundle | 1. Apple's revocation feed considers the certificate invalid. Check validity at developer.apple.com. 2. Check the local clock: a desynchronized machine can spuriously reject valid tickets. 3. If the cert really is revoked, every release signed with it is affected, not just this one. |
| The DMG mounts somewhere other than `/Volumes/PaneFlow` | A volume by that name is already mounted, so this one landed at `/Volumes/PaneFlow 1` and your verification commands are inspecting the wrong bundle. Detach the stale volume and retry. |
| `paneflow --version` prints an unexpected version | 1. You launched an older copy still in `/Applications`. 2. Two release runs overwrote each other: check which run actually produced the asset you downloaded. 3. The release was cut from the wrong commit: abandon and re-cut at a patch bump. |

---

## Step 6 - Announce (about 2 min, manual judgement required)

Write the release notes on the GitHub Release page. `release.yml` sets
`generate_release_notes: true`, so GitHub has pre-filled the changelog from
merged PRs since the previous tag. Your job is to polish that default, not write
it from scratch.

Suggested structure:

```markdown
## Highlights

- One-sentence summary of the biggest user-visible change.
- Second highlight.

## Install

Download the `.dmg`, open it, drag PaneFlow to `/Applications`.
Requires macOS on Apple Silicon.

## Validation

- CI sign + notarize + staple: PASS (link to workflow run)
- minisign signature verified locally: PASS
- Clean-Mac install smoke: PASS <date / who>

## Full changelog

{the auto-generated list GitHub prepended}
```

**Manual judgement:** read the auto-generated changelog end to end. Re-order so
user-visible changes come first (refactors and chores go last), and drop
pure-noise entries.

### Troubleshooting - Step 6

| Symptom | Top 3 recoveries |
|---|---|
| Auto-generated release notes are empty | 1. No PRs merged since the previous tag, so the generator has nothing to list. Write a manual entry. 2. The previous tag used a different naming scheme and the generator did not find it: supply `--notes-start-tag=<previous-tag>` via `gh release edit`. 3. Fall back to `git log --oneline <prev-tag>..vX.Y.Z`. |
| Announced, then discovered a critical bug | 1. Flip the release to pre-release: `gh release edit vX.Y.Z --prerelease`. Clients checking `latest` fall back to the previous stable. 2. Pin a known-issue note at the top of the release notes and link a tracking issue. 3. Cut a patch release; the in-app updater carries most users forward without manual intervention. |
| Forgot to promote an `-rc.N` tag to the final release | 1. Run Steps 1 to 5 with the non-rc tag; the workflow produces a fresh set of artifacts. 2. Do not delete the `-rc.N` release, it stays as a historical pre-release record. 3. Make sure the new final release is marked `latest` (`gh release edit vX.Y.Z --latest`). |

---

## Self-update on macOS

Users update by clicking the "Update available" pill in the title bar. The
runner is `src-app/src/update/macos/dmg.rs`. There is no privilege escalation
prompt anywhere in this flow: `/Applications` is user-writable, so no `sudo`,
`pkexec`, or authorization dialog is involved.

### What actually happens

1. Download the `.dmg` to `~/Library/Caches/paneflow` as `update-<pid>.dmg`,
   with a 30-second per-call HTTP timeout.
2. **Verify the detached minisign signature before mounting anything.** A
   missing or invalid `.minisig` deletes the partial download and bails. There is
   no `.sha256` fallback: a same-host hash is worthless against a mirror that can
   swap both files.
3. `hdiutil attach -nobrowse -readonly -mountpoint <tmp>` to a deterministic
   mount point under `/private/tmp/`, so a detach is trivially scoped and two
   concurrent updates cannot collide.
4. `codesign --verify` plus `spctl --assess` on the extracted `.app`, as a
   belt-and-braces check on top of the signature.
5. `cp -R <mount>/PaneFlow.app /Applications/PaneFlow.app.new`.
6. Atomic swap: rename `PaneFlow.app` to `PaneFlow.app.old`, then `.new` to
   `PaneFlow.app`, then `rm -rf` the old one. If the second rename fails the
   first is rolled back, so `/Applications/PaneFlow.app` never disappears.
7. `hdiutil detach` runs unconditionally through an RAII guard, so a mid-flow
   error still cleans up the mounted volume.
8. The `.app` bundle path (not the inner Mach-O) is handed to
   `cx.set_restart_path()`. GPUI's macOS `restart()` runs `open "<path>"`, which
   relaunches a bundle but not a bare executable.
9. Workspaces, layouts, and CWDs are restored from `session.json`.

### Pre-release acceptance checklist

Run this whenever the self-update dispatcher or the macOS runner
(`src-app/src/app/self_update_flow.rs`, `src-app/src/update/macos/dmg.rs`)
changed since the previous release. Skip it for releases that do not touch the
update path.

| # | Scenario | Expected |
|---|---|---|
| 1 | Happy path: newer signed version available | Click, download, verify, swap, restart with the session intact |
| 2 | `.minisig` absent from the release | Fails closed with the "corrupt or tampered" class of error. No mount, no swap. |
| 3 | `.minisig` present but signed by an untrusted key | Same failure. The message distinguishes "not signed by any key trusted by this build" from a content mismatch. |
| 4 | Artifact bytes tampered after signing | Fails closed with "does not match its signature". |
| 5 | A running build with no embedded key (a local `cargo build`) | Refuses before spending bandwidth, rather than downloading and then failing. |
| 6 | `/Applications/PaneFlow.app` running from a non-standard location (`~/Applications`, or wherever the user dragged it) | Detected structurally; the swap targets the bundle actually running. |
| 7 | Interrupt mid-copy (kill the app during `cp -R`) | `/Applications/PaneFlow.app` still present and launchable. No `.new` or `.old` left behind on the next run. |
| 8 | Workspace with 6 panes and running shells survives the restart | All 6 panes restored with correct CWDs |
| 9 | Version string from the release fails `^v?\d+\.\d+\.\d+$` | Dispatcher refuses up front. This is the defence against a compromised GitHub tag. |

A single failure among the applicable scenarios is a release blocker. Do not tag
`vX.Y.Z` until the matrix is green.

**There is no CI coverage for this flow.** Until a macOS e2e job exists, this
checklist is manual.

### Troubleshooting - Self-update

| Symptom | Top 3 recoveries |
|---|---|
| Update fails closed on a release you know is good | 1. Check the release actually has a `.minisig` sibling (Step 4). 2. Check whether the *installed* build embeds a key at all: a locally built binary has none and correctly refuses everything. 3. If you rotated the minisign key, confirm the installed build carries a slot that trusts the new key. Skipping the dual-key release in the rotation causes exactly this. |
| Update succeeds but the app never restarts | 1. `session.json` may have failed to write: check `~/Library/Application Support/paneflow`. 2. The restart path must be the `.app` bundle; a bare Mach-O path silently does nothing under `open`. 3. Check `~/Library/Logs/paneflow/` (or the console) for the "restarting into" line. |
| A `PaneFlow.app.new` or `.old` is left in `/Applications` | The swap was interrupted. The running bundle is intact by design. Delete the leftover by hand; the next update recreates what it needs. |
| Mount point collision | The runner uses a deterministic path under `/private/tmp/` scoped per update, so a stale `/Volumes/PaneFlow` from a manual DMG inspection does not interfere with self-update. It *does* interfere with the manual verification in Step 5. |

---

## Dry-run validation

This runbook is considered validated when a maintainer has executed it end to end
for a real release AND the published DMG installed cleanly from a fresh download
on a Mac that did not build it. The first execution should treat any friction as
a bug in the runbook, not in the maintainer: open a PR to fix the step that went
wrong.

Keep the "Last validated on" line at the top current, so a maintainer returning
after a long break knows whether the runbook still reflects reality.

Last validated on: _never. Update after the first release with tag, date,
workflow run, and smoke-test evidence._
