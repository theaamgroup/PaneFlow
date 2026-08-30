# Release signing: what signs what

This fork uses two independent trust anchors: Apple Developer ID codesign plus
notarization for the app bundle, and Sparkle EdDSA for each update archive. The
old hand-rolled updater and minisign client remain deleted.

| Mechanism | Protects | Verified by | Keys live |
|---|---|---|---|
| Developer ID codesign + Apple notarization | The `.app` a user launches | Gatekeeper (`spctl`) | `APPLE_*` GitHub secrets |
| Sparkle EdDSA | The DMG offered by the appcast | Sparkle before extraction | `SPARKLE_PRIVATE_KEY` GitHub secret; public half committed as `SUPublicEDKey` in `assets/Info.plist` |

Runbook: [`docs/release/macos-signing.md`](release/macos-signing.md).

---

## The GPG release key is gone. Do not bring it back.

Upstream carried a 4096-bit RSA GPG key whose only job was signing `.deb` and
`.rpm` packages and the apt/dnf repository metadata published on upstream's
public package host. This fork builds none of those artifacts and publishes to
none of that infrastructure, so the key has been removed along with them:

- `keys/` (deleted)
- `packaging/paneflow-release.asc` (deleted)
- the deb/rpm packaging trees and the repository-publishing workflow (deleted)

The GPG key never touched macOS signing. If you find yourself reaching for
`gpg` in a release step, you are almost certainly rebuilding a Linux
packaging path that was deliberately removed.

---

## GitHub secrets and variables

The macOS release path needs exactly these.

### Secrets

| Secret | Used by | What it is |
|---|---|---|
| `APPLE_DEVELOPER_CERT_P12` | `scripts/sign-macos.sh` | Base64 of the exported Developer ID Application certificate + private key (`.p12`) |
| `APPLE_DEVELOPER_CERT_PASSWORD` | `scripts/sign-macos.sh` | Password set when exporting that `.p12` |
| `APPLE_ID` | `scripts/notarize-macos.sh` | Apple ID of the account that owns the Developer Program membership |
| `APPLE_APP_SPECIFIC_PASSWORD` | `scripts/notarize-macos.sh` | App-specific password generated at appleid.apple.com, not the account password |
| `APPLE_TEAM_ID` | both scripts | 10-character Team ID; `sign-macos.sh` hard-fails if the discovered signing identity does not contain it |
| `SPARKLE_PRIVATE_KEY` | `generate_appcast` in `release.yml` | Base64 Ed25519 private seed exported by Sparkle's `generate_keys`; never commit it |

Populate multi-line secrets from a file, never from a pipe:

```bash
base64 -i DeveloperID.p12 -o /tmp/cert.p12.b64
chmod 600 /tmp/cert.p12.b64
gh secret set APPLE_DEVELOPER_CERT_P12 -R theaamgroup/paneflow < /tmp/cert.p12.b64
rm -P /tmp/cert.p12.b64
```

A pipe (`base64 -i cert.p12 | gh secret set ...`) has been observed to truncate a
long value at a buffer boundary, storing a malformed blob that fails at import
time with no useful error. Single-line values (`APPLE_TEAM_ID`, `APPLE_ID`) can
use `--body` safely.

---

## Secrets to never create in this org

These names all belonged to upstream's publishing infrastructure. A workflow
that finds them populated will push our builds to someone else's servers, or
sign artifacts with a key that has no business existing here. Do not create
them, and treat their presence in a workflow file as a bug to remove:

| Name | What it fed upstream |
|---|---|
| `R2_ACCESS_KEY_ID`, `R2_SECRET_ACCESS_KEY`, `R2_ENDPOINT`, `R2_BUCKET` | Cloudflare R2 bucket behind upstream's public package host |
| `CLOUDFLARE_*` | Cache purges and DNS for that same domain |
| `HOMEBREW_TAP_DEPLOY_KEY` | Push access to upstream's Homebrew tap |
| `GPG_PRIVATE_KEY`, `GPG_PASSPHRASE`, `GPG_KEY_ID` | Signing `.deb`/`.rpm` packages and apt/dnf repo metadata |
| `AZURE_CLIENT_ID`, `AZURE_CLIENT_SECRET`, `AZURE_TENANT_ID`, `AZURE_TRUSTED_SIGNING_*` | Windows Authenticode signing via Azure Trusted Signing |
| `POSTHOG_API_KEY` | Upstream product analytics |

Never create `GPG_*`, `AZURE_*`, or `POSTHOG_API_KEY`. `.github/workflows/release.yml` is a single signed `macos-15` / `aarch64-apple-darwin` lane.
