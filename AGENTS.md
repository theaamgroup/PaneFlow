# Repository Guidelines

## Project Structure & Module Organization
PaneFlow is a Rust workspace, macOS only in this fork. `src-app/` is the `paneflow` desktop binary and CLI entrypoint: UI, terminal rendering, pane management, IPC, themes, and embedded helper binaries under `src-app/assets/`. `crates/paneflow-*` holds the config, IPC client, process, ACP, shim, AI-hook, MCP, and MCP-installer crates. Top-level `assets/` holds macOS bundle inputs, `scripts/` utility scripts, `schemas/` the published config JSON Schema, and `skills/` the conductor skill.

## Build, Test, and Development Commands
Run all commands from the repository root.

- `cargo build` builds the workspace.
- `cargo build --release` builds the optimized app binary.
- `cargo run -p paneflow-app` launches the app locally.
- `RUST_LOG=info cargo run -p paneflow-app` runs with structured logging enabled.
- `cargo test --workspace` runs unit and integration tests across every crate.
- `cargo test -p paneflow-app --test flex_nchild -- --nocapture` runs the GPUI layout integration tests only.
- `cargo clippy --workspace -- -D warnings` treats lint warnings as errors.
- `cargo fmt --check` verifies formatting.

GPUI and `gpui_platform` are **not** local path dependencies. They are git dependencies pinned by exact `rev` to `zed-industries/zed` (`src-app/Cargo.toml:39-40`, plus a test-support `gpui` in `[dev-dependencies]` at `:265`) - three git deps in total, and there are no `collections` / `markdown` / `theme` / `ui` dependencies. Never reintroduce an `arthjean/zed` pin. The terminal engine is `paneflow-terminal-ghostty` (`src-app/Cargo.toml:62`), a workspace path dependency wrapping the vendored `libghostty-vt` archive under `native/libghostty/`; there is no `alacritty_terminal` (issue #184). Cargo fetches the Zed deps automatically, so no checkout has to be kept on disk. Never swap them for crates.io versions: GPUI is not published there.

Build prerequisites (Rust 1.98.0, full Xcode, and the separately downloaded Metal toolchain) are documented in `CLAUDE.md`. They are non-obvious and a missing one fails the build in a confusing way.

## Coding Style & Naming Conventions
Standard Rust formatting via `cargo fmt`: 4-space indentation, Rust defaults. Modules and files in `snake_case` (`config_writer.rs`, `service_detector.rs`), types in `UpperCamelCase`, functions and tests in `snake_case`. Prefer small, focused modules and brief doc comments where behavior is not obvious. Inline GPUI styling is the established pattern; match the existing builder-chain style instead of introducing a separate styling layer.

## Testing Guidelines
Put unit tests alongside the module when the logic is self-contained, as in `src-app/src/workspace/mod.rs` and `crates/paneflow-config/src/*.rs`. Keep broader UI and layout checks in `src-app/tests/`. Name tests descriptively, for example `test_three_children_flex_basis`. Run `cargo test --workspace`, `cargo clippy --workspace -- -D warnings`, and `cargo fmt --check` before opening a PR. UI changes still need manual verification.

## Pre-commit checks (mandatory)
**Before EVERY `git commit` and EVERY `git push` that touches Rust code, run `cargo fmt --check`.** If it reports a diff, run `cargo fmt`, re-stage, then commit. This is the cheapest guard against the most expensive CI failure on this repo: the release pipeline runs `cargo fmt --check` inside the Build job, so one mis-formatted line fails the build, skips the publish step, and burns the entire run before producing anything. A dirty tag commit is worse still: the original tagged build cannot be salvaged, so you have to delete and re-create the tag at the fix commit. Run `cargo fmt --check` one last time on the exact commit you are about to tag. rustfmt also drifts between Rust point releases, so code that was clean last week can need re-formatting after a toolchain bump.

## Commit guidelines
This is a private fork. There is no CONTRIBUTING.md, SECURITY.md, or public
advisory process. History uses Conventional Commit prefixes plus a scope, for
example `feat(app): adapt paneflow-hook for Codex PID env var`. Follow
`type(scope): description`. Use `(fork)` for anything that diverges from
upstream. Cite the GitHub issue (`#123`) when the commit addresses one.
`panic!`, `unimplemented!`, and `dbg!` are denied by workspace clippy;
`todo!` warns. Verify load-bearing claims by running them.

## Work tracking
GitHub issues are the tracker for bugs, features, and remaining work. File an
issue (`gh issue create`) when a defect is confirmed or a feature is scoped;
list open work with `gh issue list`. Markdown documents how the system works
and the decisions already made. It is not the backlog: do not add TODO.md,
ISSUES.md, ROADMAP.md, FIXES.md, or other markdown lists of open work, and do
not append remaining work to `docs/fork/STATE.md`.

## Platform
macOS only. Metal, AppKit, vendored `libghostty-vt` (the one and only terminal engine, issue #184), Unix-socket IPC, signed and notarized `.app` / `.dmg`. There is no Linux or Windows target in this fork: do not add `#[cfg(target_os = "linux")]` or `#[cfg(windows)]` branches back, and do not reintroduce a backend selector or `alacritty_terminal`. Config lives at `~/Library/Application Support/paneflow/paneflow.json`.

## Deeper reference
`CLAUDE.md` is the detailed engineering reference: annotated module tree, thread model, keystroke-to-pixel flow, GPUI Entity/Element patterns, hard-won scroll and wheel gotchas, the keybinding table, IPC methods, config shape, and gotchas. Open work lives in GitHub issues. `docs/fork/STATE.md` is the living handoff (landed work, verification commands, method rules). `docs/fork/2026-08-25-mac-only-fork-design.md` records this fork's decisions, its leak register, and the traps register. Read those before touching platform code. `DESIGN.md` is the design contract for the native UI - visual thesis, color roles, geometry, motion, component contracts, accessibility floors, and the UI delivery gate; read it before changing any surface, and update it in the same pull request as the change. Do not duplicate their content here.

## Shared agent workflow

These rules apply to Grok, Cursor, Claude, Codex, and all scheduled automations.

1. Confirm the defect, search for duplicates, then file an issue with trigger,
   impact, expected behavior, and file/line evidence.
2. Set severity, area, lens, safety categories, and a human assignee. Assign the
   last substantive human author (line history/originating PR, skipping bots
   and formatting-only changes). Fall back to a human code owner; never guess.
3. The assignee confirms, corrects, reassigns, or closes it on the issue.
4. One issue, one PR. Combine only inseparable fixes and explain the exception.
5. Incomplete PRs stay draft. Drafts get CI but no automated review.
6. Run applicable checks on the current commit; missing verification is not a pass.
7. Codex is the only automatic PR reviewer of record. Other agents discover,
   implement, and answer findings; additional code reviews require a human request.
8. The author replies to each thread with `Fixed in <sha>: <verification>` or
   `Declined: <reason and evidence>`, then resolves it. Reviewers never resolve
   their own findings. A human resolves threads on flagged agent-authored PRs.
9. A human signs off and executes every merge. Agents never self-approve, merge,
   enable auto-merge, bypass protection, or directly push main.

### Metadata and safety

Use exactly one severity, area, lens, and state:
- Severity: `severity:critical` (serious security/data/core-use impact),
  `severity:high` (material failure), `severity:medium` (demonstrated narrower
  defect), `severity:low` (minor actionable defect).
- Area: `area:app`, `area:terminal`, `area:agents`, `area:config`, `area:platform`.
- Lens: `lens:correctness`, `lens:security`, `lens:perf`, `lens:a11y`,
  `lens:arch`, `lens:data`.
- State: `needs-info`, `ready-for-agent`, `ready-for-human`, `wontfix`.

Record every applicable `safety:<category>`: `database` (stored data/schema/
destructive operations), `ui` (visible behavior/accessibility), `money`
(financial logic), `access` (credentials/auth/ownership), `integration`
(external services/API contracts), `platform-wide` (shared ungated behavior),
`release` (versions, CI, packaging, signing, deployment). Use `safety:none`
only after checking all categories.

`needs-human-review` is reserved for critical changes. Routing adds it
mechanically only for `safety:database`, `safety:money`, `safety:access`,
`safety:platform-wide`, `severity:critical` (on the item or a linked issue),
or a change to the release pipeline or this policy
(`.github/workflows/release.yml`, `agent-safety.yml`, `scripts/agent-policy*`,
every script `release.yml` executes including signing and notarization,
`packaging/`). `safety:ui`, `safety:integration`, and `safety:release` are
recorded but do not hold: nearly every change here touches `src-app/` or
`crates/`. A hold removes `ready-for-agent` and routes fully classified items
to `ready-for-human`. A PR whose linked issue a human kept as
`ready-for-human` is `ready-for-human` too, and a `wontfix` issue never
classifies a PR. Missing classification, owner, or metadata keeps the item in
`needs-info`, which blocks unattended work without demanding a human review. Tests and agent confidence never clear a
hold; a human may place one by hand, and only a human removes it. Carry issue
risk categories to its PR.

To promote a fully classified item from `needs-info`, add `ready-for-agent`
or `ready-for-human`; routing removes the previous state. Missing metadata
and safety holds still take precedence. Open `wontfix` items also require
complete metadata; otherwise they remain in `needs-info`. PRs with more than
20 resolved issue links stay in `needs-info` without individual issue
fetches. GitHub’s resolved closing references, including manual links, define
the linked issues; raw Markdown examples do not establish eligibility.

The workflow derives additional conservative path flags:
`src-app/`, `assets/`, `DESIGN.md` → ui;
`crates/`, `schemas/`, `examples/`, `mcps/` → integration;
`native/` → platform-wide;
`.github/`, `.agents/`, `.claude/`, `.cursor/`, `scripts/`, `skills/`, `packaging/`,
manifests/lockfiles, toolchains, deny/clippy configuration, and agent instructions
→ release. These are minimum flags, not exhaustive behavior classification.

Legacy labels are renamed: major → high, minor → medium, trivial → low,
blocked-human-review → needs-human-review. Do not recreate them.

### Automation responsibilities

- Find: create/update evidence-backed issues with the metadata above. Missing
  evidence or ownership means `needs-info`; no speculative finding dumps.
- Fix: recheck eligibility immediately before starting. Unattended work needs
  complete metadata, a human owner, `ready-for-agent`, `safety:none`, and no
  human-review hold. Stop if scope adds risk unless a human authorizes the work.
- Review: Codex reviews non-draft PRs and refreshes coverage after changes.
  Grok's review automation checks handoff readiness and responds as author
  where appropriate; it does not post a competing code review.
- Merge: prepare the handoff below, then stop for a human. Never invoke
  `gh pr merge`, merge APIs, auto-merge, or direct pushes to main.

### Pull requests and handoff

Start with two plain paragraphs under 150 words combined, nothing above them:

What changed: Explain the final change and why. Link the issue with `Closes #N`.

Needs your attention: State verification performed and gaps, the human decision
or inspection needed, and deployment/rollback requirements. Say “None” when no
special decision remains; human sign-off is still required.

Link detailed evidence below if necessary. No file inventories, repeated fix
histories, copied logs, generic checklists, or progress-comment streams.

The handoff automation edits at most one top-level summary. Retain the existing
`[grok-review-handoff]` marker for compatibility (reporter, not second reviewer):

> Reviewed <sha> · N fixed · N declined · N open · CI <status>
> Human action: <decision or sign-off>
> Details: <review/evidence links>

Count fixed findings only with a commit SHA. Keep declines and blockers visible,
including blockers tracked in issues. No empty reviews or “no findings” comments.
Ready for handoff requires current-head Codex review, current required checks,
zero unresolved threads, and no outstanding blockers. Pending, cancelled,
failed, or stale review is not clean. New commits need refreshed verification.
A human inspects declines/gaps and directly verifies flagged work.

### Verification and enforcement

Never weaken checks to get green. Behavior changes need tests executing the
affected behavior; access rules need allowed/denied cases. UI changes require
visual evidence and human inspection. Report unavailable verification honestly.
For policy-only changes run `node --test scripts/agent-policy.test.cjs` and
`actionlint .github/workflows/agent-safety.yml`. Rust checks above remain
applicable when Rust behavior changes.

The required aggregate CI check is `tests_pass`; its path-selected lanes live
in `.github/workflows/run_tests.yml`. GitHub protection enforces approvals,
current checks, and resolved conversations. Reviewer identity and human-only
merging also require these agent instructions; labels alone do not enforce them.
The safety workflow activates when merged to main.

## Code Review Rules

Report actionable defects introduced by this diff: specific trigger, consequence,
and file/line supported by a test or clear code path. State uncertainty honestly.
Check existing threads before posting. Use canonical severity words; prioritize
critical/high inline, summarize meaningful medium findings with links, and omit
low-priority nits. Never inflate severity to fit a tool's priority filter.
The GitHub reviewer of record (`chatgpt-codex-connector`) posts P0 and P1
inline only: critical maps to P0 and high maps to P1, while medium findings go
in the maintained summary.

One concise thread per defect; target at most ten by grouping related occurrences
and linking overflow blockers in the maintained summary. Never hide a blocker to
meet the limit. No formatting preferences, speculative improvements, duplicates,
or narration. Re-review adds only new findings/evidence. Ask product/design
questions separately from defects, once, with the decision needed.
