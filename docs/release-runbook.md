# PaneFlow release runbook

Step-by-step checklist for cutting a new PaneFlow release. Written for the
maintainer coming back cold after a month away: every step has a time budget, a
clear pass/fail signal, and a "what to do if it breaks" box.

**Target total time: about 40 minutes for a happy-path release**, most of it
spent waiting on Apple's notarization queue. If a step pushes you past its
budget, check that step's troubleshooting box before plowing on. The runbook has
probably already anticipated the failure.

**Apple signing path last validated on:** 2026-08-26, tag `v0.1.0`
(`44150ff`). Signed `paneflow-0.1.0-aarch64-apple-darwin.dmg` + `.sha256`.
Workflow run https://github.com/theaamgroup/paneflow/actions/runs/33010849870.
The Sparkle appcast path is new in #119 and must receive its first full
installed-update validation on the first Sparkle-enabled release.

Related runbooks:

- [`docs/release-signing.md`](./release-signing.md) explains what signs what, and
  which secrets must never exist in this org.
- [`docs/release/macos-signing.md`](./release/macos-signing.md) is the deep dive
  on codesign, entitlements, notarization, and the DMG.

Prerequisites (one-time, not part of the per-release cadence):

- `gh` CLI authenticated with `repo` scope (`gh auth status`).
- GitHub signing **secrets** populated as in
  [Required signing material](#required-signing-material) below.
  The user adds these; the workflow does not create them.
- A local macOS build environment: full Xcode plus the separately downloadable
  Metal toolchain. See the build prerequisites in `CLAUDE.md`.

The release workflow is a single signed `aarch64-apple-darwin` lane:

| Job id | Runner | What it does |
|---|---|---|
| `build` | `macos-15` | version guard, Rust gates, release binary, Sparkle-enabled `.app`, Developer ID sign, notarize, staple, DMG, `.sha256`, signed appcast |
| `release` | `macos-15` | attach DMG + `.sha256` + `appcast.xml`, then publish the GitHub Release. Runs only on a `v*` tag push. |

There is no Intel (`x86_64-apple-darwin`) leg. Dry-run is `workflow_dispatch` on
the same `build` job; it skips `release`.

Never create `GPG_*`, `AZURE_*`, or `POSTHOG_API_KEY` in this org. Those names
fed upstream's Linux packages, Windows Authenticode, and product analytics.

---

## Required signing material

Populate these before the first Sparkle-enabled tag. That tag is the
end-to-end test of the update path; a dry-run without Apple secrets will build
an unsigned `.app` and stop there.

### Secrets the workflow reads

| Secret | Where | Used by | What it is |
|---|---|---|---|
| `APPLE_DEVELOPER_CERT_P12` | repo Actions secret (legacy) | `scripts/sign-macos.sh` | Base64 of the exported Developer ID Application certificate + private key (`.p12`). The `P12` suffix is PKCS#12, not a truncated name — see below. |
| `APPLE_DEVELOPER_CERT_PASSWORD` | repo Actions secret (legacy) | `scripts/sign-macos.sh` | Password set when exporting that `.p12`. Independent of the cert blob. |
| `APPLE_ID` | repo Actions secret (legacy) | `scripts/notarize-macos.sh` | Apple Developer account email |
| `APPLE_APP_SPECIFIC_PASSWORD` | repo Actions secret (legacy) | `scripts/notarize-macos.sh` | App-specific password from appleid.apple.com, **not** the Apple ID login password |
| `APPLE_TEAM_ID` | repo Actions secret (legacy) | both Apple scripts | 10-character Team ID. `sign-macos.sh` hard-fails if the identity's common name does not contain `(TEAMID)`. |
| `SPARKLE_PRIVATE_KEY` | `release` environment secret | `generate_appcast` | Base64 Ed25519 private seed exported by Sparkle's `generate_keys`. It signs the DMG enclosure and never enters the app bundle. |
| `GITHUB_TOKEN` | injected by Actions | `gh release view` / `gh release edit` | Do **not** create this. The workflow requests `permissions: contents: write` so the default token can attach assets and undraft the release. |

Generate the Sparkle key once, using the pinned tool the release consumes:

```bash
SPARKLE_DIST="$(scripts/sparkle-dist.sh)"
"$SPARKLE_DIST/bin/generate_keys" --account com.theaamgroup.paneflow
"$SPARKLE_DIST/bin/generate_keys" --account com.theaamgroup.paneflow -p
"$SPARKLE_DIST/bin/generate_keys" --account com.theaamgroup.paneflow \
  -x /tmp/paneflow-sparkle-private-key

gh secret set SPARKLE_PRIVATE_KEY -R theaamgroup/PaneFlow --env release \
  < /tmp/paneflow-sparkle-private-key
rm -P /tmp/paneflow-sparkle-private-key
```

Commit the printed public key as `SUPublicEDKey` in `assets/Info.plist`; it is
not a secret or an Actions variable. The value in the plist and the private
seed in `SPARKLE_PRIVATE_KEY` must always remain a pair.

Keep all newly provisioned signing material in the `release` environment,
whose deployment policy admits only `main` and `v*` tags. Never put new
credentials in repository-wide Actions secrets: a branch-controlled workflow
can otherwise read them without passing the release environment's ref policy.
The environment also requires approval from the designated release owner, so
both dry-runs and tag releases pause before the credential-bearing build job.
The existing `APPLE_*` values predate that policy and remain repository-scoped
until an owner re-enters them in the environment; GitHub does not expose stored
secret values for an automated migration. Move them during the next Apple
credential rotation, then delete the repository-scoped copies.

For rotation, first ship a release whose plist trusts the replacement public
key while releases are still signed by the old key. Wait for that bridge build
to reach the installed fleet before switching the CI secret and retiring the
old key. A one-step replacement strands every install that still trusts only
the old key.

The repository is public because Sparkle fetches the appcast and DMG without
GitHub credentials. It was made public on 2026-08-30 only after gitleaks scanned
all 1,399 commits and its ten findings were confirmed as false-positive
`action_name` field literals. Do not make release assets private without first
moving them and the appcast to an anonymous public mirror.

Updater policy for the initial rollout:

- No prompt, forced relaunch, release-notes window, Settings toggle, phased
  rollout, or delta archives. A user who never quits intentionally remains on
  the running version until the next ordinary quit/launch cycle.
- `PANEFLOW_DISABLE_SPARKLE=1` is an exact-value diagnostic escape hatch used
  by bundle render tests; it is not a `paneflow.json` product preference.
- Roll back a bad release by pulling its appcast/release before more clients
  stage it, then publishing a higher patch version that reverts the change.
  Never republish a changed archive under an existing version or signature.

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

1. Verify `${RELEASE_TAG#v}` matches the inherited `paneflow-app` Cargo version.
   A mismatch fails before compilation.
2. `cargo fmt --check` (hard fail; the cheapest guard against burning a tagged
   run).
3. `cargo clippy --workspace --all-targets --locked --target aarch64-apple-darwin -- -D warnings`.
4. `cargo test --workspace --locked --target aarch64-apple-darwin`.
5. `cargo build --release --target aarch64-apple-darwin`.
6. `scripts/bundle-macos.sh` checksum-verifies the pinned Sparkle distribution,
   embeds `Sparkle.framework`, and produces `dist/PaneFlow.app`.
7. `Detect macOS signing secrets`. On a tag push, a missing `APPLE_*` secret is
   a hard failure here, not a downgrade to unsigned. Dry-run may continue
   unsigned and uploads `dist/PaneFlow.app` as a workflow artifact.
8. `scripts/sign-macos.sh` codesigns it (Sparkle helpers inside-out, other
   nested dylibs and executables,
   parent seal) with the hardened runtime and the release entitlements.
9. `scripts/notarize-macos.sh` zips it with `ditto`, submits to `notarytool`,
   polls, staples the ticket, and runs `spctl --assess`.
10. `scripts/create-dmg.sh` builds
   `paneflow-<semver>-aarch64-apple-darwin.dmg` and independently re-verifies
   `codesign`, `stapler validate`, and `spctl` against the bundle mounted from
   the finished image. A `.sha256` sibling is staged next to it.
11. Sparkle's `generate_appcast` signs the DMG with `SPARKLE_PRIVATE_KEY`,
    embeds GitHub-generated release notes, and stages `appcast.xml`.
12. The `release` job (tag-push only) attaches all three assets, verifies the
    exact remote asset set while the release is still a draft, then publishes.

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
appcast.xml
paneflow-X.Y.Z-aarch64-apple-darwin.dmg
paneflow-X.Y.Z-aarch64-apple-darwin.dmg.sha256
```

### Troubleshooting - Step 4

| Symptom | Top 3 recoveries |
|---|---|
| Asset name has the wrong suffix | 1. The staging step in `release.yml` renames to the canonical form; a missing rename is a workflow regression. 2. Patch the staging step, cut a new patch tag. |
| Pre-release ended up on `latest` | 1. The tag contains `rc`/`beta`/`alpha` but the workflow's prerelease boolean is false. Check the `contains(...)` expression in `release.yml`. 2. Manually flip it: `gh release edit vX.Y.Z --prerelease`. 3. Fix the workflow expression in a follow-up commit. |

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
| Announced, then discovered a critical bug | 1. Flip the release to pre-release: `gh release edit vX.Y.Z --prerelease`. 2. Pin a known-issue note at the top of the release notes and link a tracking issue. 3. Cut a patch release. |
| Forgot to promote an `-rc.N` tag to the final release | 1. Run Steps 1 to 5 with the non-rc tag; the workflow produces a fresh set of artifacts. 2. Do not delete the `-rc.N` release, it stays as a historical pre-release record. 3. Make sure the new final release is marked `latest` (`gh release edit vX.Y.Z --latest`). |

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
