# macOS signing and notarization runbook

One-time setup, secret rotation, and expiration tracking for the PaneFlow macOS
release pipeline. This is the only code-signing path this fork has: the
`release.yml` workflow signs and notarizes the `aarch64-apple-darwin` `.app`
produced by `scripts/bundle-macos.sh`, then wraps it in a DMG.

**Missing secrets are a hard failure on a real release.** The
`Detect macOS signing secrets` step (`.github/workflows/release.yml:985`) checks
all five `APPLE_*` secrets. When `RELEASE_MODE = "release"` (a tag push) and any
one is empty it emits `::error title=macOS signing required::` and exits 1. Only
in dry-run mode does it degrade to an unsigned build with a warning. There is no
path that silently ships an unsigned tagged release.

This document is operator-only. Application code never reads the entitlements
plists described here. It does consume `assets/Info.plist`, which
`bundle-macos.sh` templates into the shipped bundle.

Related: [`docs/release-signing.md`](../release-signing.md) explains why this is
a separate mechanism from the minisign self-update signature, and
[`docs/release-runbook.md`](../release-runbook.md) is the per-release checklist
that calls these scripts in order.

---

## 1. The four scripts and what each one needs

The macOS release path is four scripts run in this order. Only the middle two
touch credentials.

| Script | Reads secrets | Produces |
|---|---|---|
| `scripts/bundle-macos.sh` | none | `dist/PaneFlow.app` from a pre-built binary |
| `scripts/sign-macos.sh` | `APPLE_DEVELOPER_CERT_P12`, `APPLE_DEVELOPER_CERT_PASSWORD`, `APPLE_TEAM_ID` | signs the bundle in place |
| `scripts/notarize-macos.sh` | `APPLE_ID`, `APPLE_APP_SPECIFIC_PASSWORD`, `APPLE_TEAM_ID` | staples a notarization ticket into the bundle |
| `scripts/create-dmg.sh` | none | `dist/paneflow-<version>-<arch>-apple-darwin.dmg` |

`bundle-macos.sh` does **not** invoke cargo. It consumes an already-built
`target/<triple>/release/paneflow` and hard-fails if it is absent.

`-h` / `--help` works on `bundle-macos.sh`, `sign-macos.sh`, and
`create-dmg.sh`. **`notarize-macos.sh` has no flags at all**: its only argument
is an optional bundle path, so `-h` is consumed as that path and dies with
`error: bundle not found: -h`.

---

## 2. What gets signed

`scripts/bundle-macos.sh` produces a flat bundle:

```
dist/PaneFlow.app/Contents/
  MacOS/paneflow
  Info.plist
  Resources/PaneFlow.icns
```

`scripts/sign-macos.sh` issues **three** codesign phases, not two. The
dylib/executable split matters: Apple requires that a plain `.dylib` be signed
*without* entitlements.

1. **Nested dylibs** (`sign-macos.sh:196-204`). `find -d` over
   `Contents/Frameworks`, `Contents/Helpers`, `Contents/PlugIns`,
   `Contents/XPCServices` for `*.dylib`, signed
   `--force --options runtime --timestamp --sign` with **no `--entitlements`**.
2. **Nested executables and bundles** (`:206-224`). Same four directories, for
   `*.framework`, `*.xpc`, and any regular file with the user-execute bit set
   that is not a `.dylib`. Signed **with** `--entitlements`. Note the predicate
   is name-based plus `-type f -perm -u+x`, so it also matches a shell script,
   and it does **not** treat `.bundle`, `.appex`, or a nested `.app` as a bundle
   wrapper (only the executables inside them, individually).
3. **Parent bundle** (`:227-233`). Exactly one codesign call, on the `.app`
   itself, with `--force --options runtime --timestamp --entitlements --sign`.
   `--deep` is deliberately omitted. `Contents/MacOS/paneflow` is never a
   separate codesign target; it is covered by the parent seal.

Today the bundle has none of those four nested directories, so phases 1 and 2
are no-ops. They stay in place so adding a helper bundle later needs no signing
changes.

Finally, `:239` runs `codesign --verify --deep --strict --verbose=2` on the
bundle and fails the script if it does not pass.

### Path defaults are asymmetric

- `--entitlements` defaults to `$REPO_ROOT/packaging/macos/paneflow.entitlements`
  (**repo-root absolute**).
- The app bundle defaults to `dist/PaneFlow.app` (**CWD-relative**).

So running `sign-macos.sh` from a subdirectory finds the entitlements but not the
bundle. Run it from the repo root.

### Identity discovery and the Team ID cross-check

`sign-macos.sh:129` runs `security find-identity -v -p codesigning` and takes
the **first** `Developer ID Application` match. If there is no such line it
dumps the full `find-identity -v` output and exits 1 (`:132-137`).

Then `:142` hard-fails if the discovered identity string does not contain the
literal substring `($APPLE_TEAM_ID)`:

```
error: signing identity team ID does not match APPLE_TEAM_ID
```

This is the guard that catches a placeholder or stale Team ID before anything is
submitted to Apple. An identity looks like
`Developer ID Application: <Name> (<TEAMID>)`. The string in the script's header
comment is an example only, not a real identity.

> **Two side effects worth knowing before you run this on your own Mac.**
> `sign-macos.sh` creates a keychain literally named `build.keychain` with a
> random `openssl rand -hex 32` password, and it **rewrites the user keychain
> search list** (`security list-keychains -d user -s`, `:106`). The `EXIT` trap
> deletes the keychain, but a `kill -9` leaves a dangling search-list entry you
> have to clean up by hand. Separately, `:82` builds the temp `.p12` path by
> appending `.p12` to a `mktemp` result, so the extension-less file `mktemp`
> actually created is never removed. Every run leaks one empty file under
> `/var/folders/`.

---

## 3. Notarization, in detail

`scripts/notarize-macos.sh` does not submit the bundle. It submits a **ZIP**:

1. `ditto -c -k --keepParent` writes `${APP%.app}.zip`, i.e.
   `dist/PaneFlow.zip` (`:28`, `:41`). The `EXIT` trap deletes it.
2. `xcrun notarytool submit` with `--output-format json`, **without `--wait`**
   (`:69-73`).
3. `python3` extracts the submission `id` from that JSON (`:76`). **python3 is a
   hard dependency of this script.** The script then echoes the submission ID
   and a recovery command, which is the single most useful line in the log if
   the job is interrupted:

   ```bash
   xcrun notarytool info <submission-id> \
     --apple-id "$APPLE_ID" --password "$APPLE_APP_SPECIFIC_PASSWORD" \
     --team-id "$APPLE_TEAM_ID"
   ```

4. A poll loop calls `notarytool info` every **30 seconds** (`POLL_INTERVAL`,
   `:89`) with a hard ceiling of **90 minutes** (`MAX_WAIT_SECONDS`, `:90`).
   Neither is env-overridable. `Accepted` breaks the loop; `Invalid` or
   `Rejected` emits `::error title=Notarization::`, dumps
   `xcrun notarytool log`, and exits 1. `In Progress` prints a `[+MM:SS]`
   heartbeat. Hitting the ceiling exits 1 with the manual recovery commands in
   the error output.
5. `xcrun stapler staple` then `xcrun stapler validate` (`:151-152`).
6. `spctl --assess --type exec --verbose` as a Gatekeeper smoke test (`:161`).

The script emits GitHub Actions `::error` workflow commands, so it is written for
CI log rendering. It still runs fine locally, the annotations are just noise.

---

## 4. The DMG

`scripts/create-dmg.sh` needs no credentials. It requires `--version` and
`--arch` (`aarch64` or `x86_64`), takes an optional `--app`, and writes:

```
dist/paneflow-<version>-<arch>-apple-darwin.dmg
```

**The `-apple-darwin` suffix is a contract, not cosmetics.** The in-app
updater's asset matcher looks releases up by that suffix, so renaming the asset
breaks self-update for every installed client.

After `hdiutil create` and `hdiutil verify`, the script mounts the image
read-only and re-runs three independent checks against the bundle *inside the
finished image*: `codesign --verify --deep --strict`, `xcrun stapler validate`,
and `spctl --assess --type exec --verbose` (`:136-147`). Any failure detaches and
dies. This is the real gate on whether a user can open the thing.

> **Trap.** The verification mount point is hard-coded as `/Volumes/PaneFlow`
> (`:135`). If a volume by that name is already mounted, `hdiutil` mounts the
> new one at `/Volumes/PaneFlow 1` and all three verifications then inspect the
> *wrong* bundle. Unmount any stale PaneFlow volume before running this.

`create-dmg.sh` does not use the `create-dmg` Homebrew/npm tool despite its
name, and no longer uses `osascript` for Finder window layout.

---

## 5. Entitlements files

Three entitlements plists live under `packaging/macos/`. Each is a distinct file
so future variant-specific tweaks can land in isolation. **The release variant
ships to users; the dev variant is local-only and must never be sent to
notarytool.**

| File | When to use | Keys |
|---|---|---|
| `paneflow.entitlements` | Tagged releases. The default in `sign-macos.sh`, and what `release.yml` uses. | 4 keys: `app-sandbox=false`, `automation.apple-events`, `cs.allow-jit`, `cs.allow-unsigned-executable-memory`. |
| `paneflow.nightly.entitlements` | Nothing, today. | Same 4 keys as release. **No script or workflow ever passes this file**, and there is no nightly bundle ID in the repo. It is a forked placeholder, not a live pipeline. |
| `paneflow.dev.entitlements` | **Local only.** Attaching `lldb` to a signed build on your own machine. | 6 keys: the release 4, plus `com.apple.security.get-task-allow` **and** `com.apple.security.cs.allow-dyld-environment-variables`. **Notarization rejects any bundle carrying `get-task-allow`** - never use for distribution. (The file's own comment claims get-task-allow is the only addition. It is wrong; there are two.) |

The `cs.*` block is required for any GPUI app under the hardened runtime: GPUI
compiles `MTLComputePipelineState` objects at first use, which Apple classifies
as JIT.

`release.yml` passes **no** `--entitlements` flag. The step is simply
`bash scripts/sign-macos.sh dist/PaneFlow.app`, relying on the script's
repo-root-absolute default. That omission is deliberate and commented in the
workflow, so if you are hunting a wrong-entitlements bug, do not go looking for
a flag in the workflow that was never there.

---

## 6. One-time onboarding

1. **Apple Developer account.** An account owned by The AAM Group, or an
   individual account under it. Record the owner Apple ID somewhere internal, in
   the password manager alongside the credentials. Do not commit it here.
2. **Generate a Developer ID Application certificate.**
   - Xcode → Settings → Accounts → Manage Certificates → `+` → "Developer ID
     Application".
   - The common name will be `Developer ID Application: <name> (<TEAMID>)`.
   - Note the **Created** and **Expires** dates. Set a calendar reminder for 60
     days before expiry. Gatekeeper rejects releases signed with an expired
     certificate.
3. **Export to `.p12`.** Keychain Access → Login → expand the cert →
   right-click the private key (the disclosure-triangle child) → Export. Choose
   `.p12`, set a strong password, save to a temp file. Put the password in the
   password manager **before** you upload anything.
4. **Encode for GitHub Secrets.** Use `gh` rather than the clipboard (Universal
   Clipboard syncs to every signed-in Apple device, and most clipboard managers
   retain history):
   ```bash
   gh secret set APPLE_DEVELOPER_CERT_P12 \
     -R theaamgroup/panescli < <(base64 -i developer-id.p12)
   ```
   If `gh` is unavailable, write to a temp file, paste it into the GitHub
   Secrets UI, and erase:
   ```bash
   base64 -i developer-id.p12 > /tmp/cert.b64
   # paste into APPLE_DEVELOPER_CERT_P12 in the repo's Actions secrets
   rm -P /tmp/cert.b64
   ```
5. **Generate an app-specific password** at <https://appleid.apple.com> →
   Sign-In and Security → App-Specific Passwords. Label it "PaneFlow
   Notarization". Save the 16-character output to the password manager.
6. **Locate the Team ID** at <https://developer.apple.com/account> under
   Membership Details → Team ID. 10 alphanumeric characters.
7. **Populate the five GitHub Secrets** under repo Settings → Secrets and
   variables → Actions:

   | Secret | Source | Notes |
   |---|---|---|
   | `APPLE_DEVELOPER_CERT_P12` | the `base64` output from step 4 | Embedded newlines are fine (`base64 -D` tolerates them), but there is no sanitization: a whitespace-only value passes the script's presence check and then fails opaquely at `security import`. |
   | `APPLE_DEVELOPER_CERT_PASSWORD` | the password set during `.p12` export | |
   | `APPLE_ID` | the Apple ID that owns the membership | |
   | `APPLE_APP_SPECIFIC_PASSWORD` | step 5 | NOT the Apple ID login password. |
   | `APPLE_TEAM_ID` | step 6 | Plain text, no quotes, no spaces. Must match the `(TEAMID)` inside the certificate's common name or signing hard-fails. |

8. **Re-run the release workflow.** The `Detect macOS signing secrets` step
   logs `All 5 Apple signing secrets are present - will sign + notarize.` The
   `signing_available=true` value goes to `$GITHUB_OUTPUT`, not the log, so do
   not go looking for it in the run output.
9. **First-time verification.** Download the published `.dmg` on a clean macOS
   machine, open it, drag to `/Applications`, double-click. Expected: no
   Gatekeeper prompt, app launches. If you see a prompt, the notarization ticket
   is missing. Read the `notarytool log` output dumped by the notarize step.

> **Known fork item.** `assets/Info.plist` still carries upstream's bundle
> identifier, which embeds upstream's GitHub handle. It needs to become our own
> reverse-DNS identifier before the first release under our Developer ID. That
> is part of the pending rename pass, not something to patch piecemeal here.

---

## 7. Local dev signing (no CI)

To sign a build on your own machine, typically to attach `lldb` to a
hardened-runtime binary, use the `.dev` entitlements:

```bash
cargo build --release --target aarch64-apple-darwin -p paneflow-app
bash scripts/bundle-macos.sh --version 0.0.0 --arch aarch64

# Provide your secrets via env (replace with real values):
export APPLE_DEVELOPER_CERT_P12="$(base64 -i ~/secrets/dev-id.p12)"
export APPLE_DEVELOPER_CERT_PASSWORD='...'
export APPLE_TEAM_ID='<your 10-char Team ID>'

bash scripts/sign-macos.sh \
    --entitlements packaging/macos/paneflow.dev.entitlements \
    dist/PaneFlow.app

# DO NOT notarize a dev build - get-task-allow guarantees rejection.
```

Then `lldb -- dist/PaneFlow.app/Contents/MacOS/paneflow`.

Use a plain `N.N.N` version here. `bundle-macos.sh` substitutes `--version`
verbatim into both `CFBundleVersion` and `CFBundleShortVersionString`, and
something like `0.0.0-dev` is not valid Apple version syntax.

Remember `sign-macos.sh` mutates your real user keychain search list while it
runs (§2).

---

## 8. Periodic maintenance

| Cadence | Task |
|---|---|
| **Every 12 months** | Rotate `APPLE_APP_SPECIFIC_PASSWORD`. App-specific passwords have no hard expiry, but a yearly rotation matches the cert renewal cycle and limits credential blast radius. |
| **Per cert expiry (typically 1-3 years)** | Re-run §6 steps 2-4 to mint a new `.p12`, then update `APPLE_DEVELOPER_CERT_P12` and `APPLE_DEVELOPER_CERT_PASSWORD`. **Releases already in the wild keep working**: notarization tickets are timestamped and remain valid past cert expiry. |
| **On Apple ID password change** | Old app-specific passwords are not auto-revoked by an Apple ID password change (Apple's design), but rotate anyway: revoke at appleid.apple.com → App-Specific Passwords → trash icon, generate a new one, update `APPLE_APP_SPECIFIC_PASSWORD`. |
| **On Team ID change** | Regenerate the cert under the new team and update both `APPLE_TEAM_ID` and `APPLE_DEVELOPER_CERT_P12`. The signing script's Team ID cross-check will hard-fail until both are consistent. |

---

## 9. Troubleshooting

- **`error: no 'Developer ID Application' identity in <keychain>`.** The `.p12`
  imported but produced no codesigning identity, or the import silently failed.
  The script dumps the full `security find-identity -v` output right before
  exiting; read it. A whitespace-only `APPLE_DEVELOPER_CERT_P12` reaches this
  point looking "present".
- **`error: signing identity team ID does not match APPLE_TEAM_ID`.** The
  certificate's common name does not contain `(<APPLE_TEAM_ID>)`. Either the
  secret is stale/placeholder, or the `.p12` came from a different team.
- **`notarytool submit` returns `Invalid` with `The binary is not signed`.** The
  ZIP reached Apple but a nested binary was unsigned. Run
  `codesign --verify --deep --strict --verbose=2 dist/PaneFlow.app` on the
  pre-submitted bundle to find the offender. If a new helper was added, confirm
  it lives under `Contents/Frameworks`, `Contents/Helpers`, `Contents/PlugIns`,
  or `Contents/XPCServices` so the nested walk picks it up, and remember the
  walk keys off name patterns plus the execute bit.
- **`The signature does not include a secure timestamp`.** `--timestamp` is
  unconditionally present on all three codesign calls, so this can only mean
  Apple's timestamp authority was unreachable. Re-run the leg; transient TSA
  failures usually clear within minutes.
- **`The executable requests the com.apple.security.get-task-allow
  entitlement`.** A dev-signed build reached notarytool. Since `release.yml`
  passes no `--entitlements` at all and the script default is the release file,
  this means either the bundle was signed locally with the dev entitlements
  before the workflow ran, or `packaging/macos/paneflow.entitlements` itself was
  edited to add the key.
- **Notarization hits the 90-minute ceiling.** The script exits 1 and prints the
  `notarytool info` and `stapler staple` recovery commands with the real
  submission ID. Apple queue backlogs of over an hour do happen. Do not re-tag:
  poll the existing submission with the printed command, and if it eventually
  reports `Accepted`, staple by hand.
- **`error reading entitlements` during codesign.** The plist is malformed XML.
  `plutil -lint packaging/macos/*.entitlements`. The sign script lints only the
  file it was told to use, so lint all three when in doubt.
- **`spctl --assess` returns `rejected`** after a successful staple. The ticket
  attached but Apple's revocation feed considers the cert invalid. Check
  certificate validity at developer.apple.com and the local clock (a
  desynchronized runner clock can spuriously reject valid tickets).
- **`create-dmg.sh` fails its post-mount verification** even though signing and
  notarization both passed. Check for a stale `/Volumes/PaneFlow` mount first
  (see §4); the verification may be inspecting a different image entirely.

---

## 10. Related files

- `scripts/bundle-macos.sh` - builds the `.app` from a pre-built binary. Needs
  `assets/Info.plist` and `assets/PaneFlow.icns` (generated by
  `scripts/build-icons.sh`).
- `scripts/sign-macos.sh` - codesign driver: nested dylib pass, nested
  executable pass, parent seal, then `--verify --deep --strict`.
- `scripts/notarize-macos.sh` - ditto to ZIP, notarytool submit, 30 s poll to a
  90 min ceiling, staple, validate, `spctl --assess`.
- `scripts/create-dmg.sh` - the published asset, plus the independent
  codesign / stapler / Gatekeeper re-verification against the mounted image.
- `.github/workflows/release.yml` - `Detect macOS signing secrets`,
  `Sign macOS .app bundle`, `Notarize + staple macOS .app bundle`,
  `Record unsigned macOS build in job summary`, `Produce .app bundle`,
  `Free disk space before .dmg packaging`, `Produce .dmg`, `Stage macOS .dmg`.
- `packaging/macos/paneflow.entitlements`, `paneflow.dev.entitlements`,
  `paneflow.nightly.entitlements` - the three entitlements variants.
- `assets/Info.plist` - templated into the bundle, carries the bundle ID.
