# Final adversarial audit (task 21)

Run 2026-08-25 against `main` after the post-2c task list. Grok, 37
turns, no `--json-schema`. Orchestrator spot-checked the live hits.

Census: ZERO-CONDITION 0, `cfg(unix)` 151, `cfg(macos)` 77.

## Claimed end state that held

Telemetry crate and app module gone. `DEFAULT_FEED_URL = None`;
`spawn_check` returns `Disabled` without a socket. Ghostty / windows
material absent from the published schema. `panescli` absent from code.
`run_tests.yml` and `release.yml` have no Linux/Windows compile jobs.
No live `GPG_*` / `AZURE_*` / `POSTHOG_API_KEY` workflow secrets.

## Findings accepted (fixed in the same sitting unless noted)

| # | Site | Action |
|---|---|---|
| A | Help menu + `OpenHelp` opened `github.com/arthjean/paneflow#readme` | pointed at `theaamgroup/paneflow` |
| B | `paneflow --help` footer same host | same |
| C | `schemas/paneflow.schema.json` `$id` same host | same |
| D | `.github/workflows/audit.yml` `cron_audit` still `ubuntu-22.04` | **follow-up**: cargo-deny only, does not compile GPUI |
| E | `scripts/build-icons.sh` still writes `packaging/wix/` | **follow-up**: inert without the Linux/Windows icon master |
| F | `run_tests.yml` path filter still names deleted `repo_publish.yml` / `update_cask.yml` | **follow-up**: harmless, never matches |
| G | schema `font_fallbacks` description still says “on Windows” | **follow-up**: copy only |

Cmd+Tab remains in `--help` and `MANUAL-CHECKLIST.md` (task 6).
`create-dmg.sh` codesign/stapler/spctl checks fail on an unsigned
artifact; the .dmg itself was produced.

`SECURITY.md`, `CONTRIBUTING.md`, issue templates, and the PR template
are deleted. This is a private fork.

## Not findings

Zed lockfile crates named `telemetry` / `telemetry_events` (markdown
pin). ALLOWED_HOSTS tests that use arthjean URLs as fixtures. Loader
tests that feed leftover `telemetry` / `windows_*` / `"ghostty"` keys.
`.exe`-tolerant parsers. `#[cfg(unix)]`.
