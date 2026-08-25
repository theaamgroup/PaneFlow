# Self-update signing runbook (minisign / Ed25519)

This is the **root of trust for the in-app self-updater**. It signs the release
artifact the updater downloads (the `.dmg`) so a running PaneFlow can prove a
download came from us before it mounts or executes it.

It is a **different mechanism from macOS code signing**, and the two are not
interchangeable. Developer ID codesign plus notarization
([`docs/release/macos-signing.md`](release/macos-signing.md)) is what Gatekeeper
checks when a user launches the app. minisign is what *Paneflow itself* checks
before it installs an update. See
[`docs/release-signing.md`](release-signing.md) for the side-by-side, including
why the old GPG key is not part of either path.

> **This fork needs its own keypair.** Do not reuse upstream's. Generate a fresh
> one per §1 and populate our own repository variables and environment secret.
> Inheriting upstream's key would mean anchoring our updater's trust in a
> signing key we do not control.

How it fits together:

- The **public** key(s) are baked into every release binary at build time from
  the env vars `PANEFLOW_MINISIGN_PUBKEY` and `PANEFLOW_MINISIGN_PUBKEY_NEXT`,
  read through `option_env!` at `src-app/src/update/signature.rs:71` and `:77`.
  `option_env!` is evaluated at compile time, so a build that does not set them
  embeds `None` and every update then fails closed. These are public values,
  stored as GitHub **repository variables**, never secrets.
- The **private** key signs each artifact in CI, producing a detached
  `<artifact>.minisig` uploaded alongside it. It lives only in a GitHub
  **environment secret** on the `release` environment.
- At update time the client fetches `<asset_url>.minisig`, verifies the bytes
  on disk against an embedded key, and **fails closed** on any mismatch,
  missing signature, or unsigned build (no `.sha256` fallback). Gatekeeper's
  `codesign` / `spctl` check is a second, independent layer on top.

Two public-key **slots** are embedded, current and next, so the key can be
rotated with zero downtime (see §5). A slot that fails to parse is logged and
skipped rather than aborting, so one malformed variable cannot disable the
other, good key.

> **Local-rebuild trap.** `src-app/build.rs` registers
> `cargo:rerun-if-env-changed` for `POSTHOG_API_KEY`, `POSTHOG_HOST`, and
> `PANEFLOW_SKIP_EMBED_BUILD` only. Neither minisign variable is registered, so
> changing `PANEFLOW_MINISIGN_PUBKEY` locally can reuse a cached object file and
> silently keep the previously embedded key. Force a rebuild
> (`touch src-app/src/update/signature.rs`, or `cargo clean -p paneflow-app`)
> when you change it by hand.

> Introducing this root of trust is a one-way step. The first signed release is
> the boundary: clients on it (and later) verify every future update. Clients on
> an *older* release have no embedded key, so their in-app updater fails closed
> until the user installs one signed build manually. That is the intended,
> documented cost of adding a trust anchor.

---

## 1. Generating the key (one time)

Do this on a **trusted workstation** - never on a shared machine, never on a
CI runner, never over SSH to a multi-user box.

`minisign` and `rsign2` produce wire-compatible keys; either works. Use an
**unencrypted** secret key (`-W`) so CI can sign non-interactively - the
GitHub secret is the protection. Keep an *encrypted* backup in your password
manager.

```bash
# Option A - minisign (C tool; `brew install minisign`)
minisign -G -W -p paneflow-minisign.pub -s paneflow-minisign.key
#   -W : no passphrase on the secret key (required for unattended CI signing)

# Option B - rsign2 (pure Rust: `cargo install rsign2`)
rsign generate -W -p paneflow-minisign.pub -s paneflow-minisign.key
```

`paneflow-minisign.pub` is two lines:

```
untrusted comment: minisign public key ABCDEF0123456789
RWQ…base64…                 <- THIS second line is what the binary embeds
```

The **second line** (the bare base64) is the value the verifier wants
(`PublicKey::from_base64`). The full `.key` file (all lines) is the CI secret.

---

## 2. GitHub configuration

| What | Where | Value |
|------|-------|-------|
| `PANEFLOW_MINISIGN_PUBKEY` | Repo **variable** (Settings → Secrets and variables → Actions → Variables) | the base64 **second line** of `paneflow-minisign.pub` |
| `PANEFLOW_MINISIGN_PUBKEY_NEXT` | Repo **variable** | empty until a rotation is in flight (§5) |
| `MINISIGN_SECRET_KEY` | **Environment secret** on the `release` environment | the **entire** `paneflow-minisign.key` file contents |

The `release` job in `release.yml` declares `environment: release`; add a
required reviewer to that environment if you want a manual approval gate
before any tag is signed and published.

Wiring already in `release.yml`:

- The per-target **Build** step passes both public vars into the compile so
  `option_env!` bakes them in.
- The **Publish** job's *Sign release artifacts (minisign)* step writes the
  secret to a temp file and signs every primary artifact (skipping `.sha256`
  and `.minisig` siblings) → `<artifact>.minisig`, which
  `files: release-assets/*` then uploads.

If `MINISIGN_SECRET_KEY` is unset the signing step fails the release job.
Clients with an embedded key refuse unsigned update assets, so publishing a
release without detached `.minisig` siblings would ship a broken self-update
path.

---

## 3. Verifying a release by hand

```bash
# pub.txt = the 2-line public key file
minisign -V -p pub.txt -m paneflow-0.3.9-aarch64-apple-darwin.dmg
#   → "Signature and comment signature verified" on success.
```

The in-app path is `update::signature::fetch_and_verify`: fetch
`<asset_url>.minisig`, stream-hash the artifact, verify with each embedded
key (accept if either matches), fail closed otherwise.

---

## 4. Threat model (what this does and does not cover)

Covers: a compromised mirror / CDN / MITM swapping the artifact (and its
`.sha256`) on the download channel - the forged bytes fail signature
verification and are never run. This is the RCE-class hole the old same-host
`.sha256` left open.

Does **not** cover: a compromised *signing key* or a compromised *build host*
that signs malicious bytes. Mitigations: keep the secret key only as an
environment secret with reviewer protection; the dual-key rotation below; and
Apple code signing plus notarization (`codesign` + `spctl`), which chains to
Apple's root independently of minisign. Server-compromise-resistant schemes
(TUF, sigstore transparency logs) are out of scope.

---

## 5. Key rotation (no downtime)

Because clients accept a signature from **either** embedded slot, you can
rotate without an online revocation step:

1. **Generate** a new keypair (§1) → call it `next`.
2. **Embed both**: set `PANEFLOW_MINISIGN_PUBKEY_NEXT` (repo variable) to the
   new public base64. Leave `PANEFLOW_MINISIGN_PUBKEY` (current) as-is. Cut a
   release. Now shipping clients trust **both** keys, but artifacts are still
   signed by `current`.
3. **Switch signing** to `next`: replace the `MINISIGN_SECRET_KEY` environment
   secret with the new `.key`. Cut a release. Artifacts are now signed by
   `next`; clients from step 2 onward accept them via the `_NEXT` slot.
4. **Promote**: after 2-3 releases (enough time for users to update through a
   build that carries both keys), move `next`'s public base64 into
   `PANEFLOW_MINISIGN_PUBKEY` and clear `PANEFLOW_MINISIGN_PUBKEY_NEXT`. Cut a
   release. The old key is now fully retired.

Never skip step 2: a client must learn the new key (via an update signed by a
key it already trusts) **before** it sees an artifact signed only by the new
key, or it would fail closed.

---

## 6. Compromise response

If the **private key** leaks:

1. Immediately rotate `MINISIGN_SECRET_KEY` to a freshly generated key and run
   the §5 rotation, but compress the timeline - ship the dual-key build and
   the re-signed build back to back.
2. Treat any release the leaked key could have signed as suspect. Apple
   notarization is an independent second factor: an attacker holding only the
   minisign key still cannot produce a bundle that passes `spctl`, so a user
   launching the app is protected even if the updater were fooled.
3. Once every supported client has updated past a build carrying the new key,
   drop the compromised key from both slots so a stolen-key signature no
   longer verifies anywhere.

If a **build host** is compromised, minisign cannot help (it would sign
whatever the host produces) - rebuild from a clean host, rotate the key, and
audit the release artifacts against the source tree.

---

## 7. Quick checklist (per release)

- [ ] `PANEFLOW_MINISIGN_PUBKEY` repo variable set (and `_NEXT` if rotating).
- [ ] `MINISIGN_SECRET_KEY` environment secret present on `release`.
- [ ] After the run: every primary artifact has a sibling `.minisig` in the
      GitHub release. `minisign -V -p pub.txt -m <artifact>` passes for each.
- [ ] An install of the *new* release can self-update to a hand-built DMG
      fixture. **There is no CI coverage for the macOS DMG update path** -
      upstream's `auto-update-e2e` job exercised the Linux tar.gz path only, so
      this check is manual until a macOS e2e job exists.
