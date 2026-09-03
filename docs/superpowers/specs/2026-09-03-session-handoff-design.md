# Session handoff to another harness — design

**Issue:** #334 (cluster A4 of the Blume-inspired set, comment of 2026-09-02).
**Status:** approved design, not yet implemented.
**Decisions taken with the user on 2026-09-03**, in this order: placement,
target list, gesture, handoff block. Each is recorded in §2 with the option
that was rejected and why, so a fixer does not reopen it.

Line numbers below were read on `main` at `f1d7e94f` (origin/main `6a47fea4`
plus the #339 docs commits). They drift; the symbol names do not.

## 1. Problem

A session row in the sessions sidebar can be resumed only into the **same**
CLI (`resume_command`, `src-app/src/app/sessions_sidebar.rs:1108-1172`).
When a Claude 5-hour window closes, the work stops: nothing lets the user
carry what the agent already knows into Codex, Grok, or any other enabled
launcher. Blume's "copy a summary session into a new harness" is the
reference behaviour. PaneFlow runs the agents, so the handoff here is a new
pane running the target agent with the context prefilled, never submitted.

## 2. Product decisions

Settled with the user before design:

| Decision | Chosen | Rejected and why |
|---|---|---|
| Placement | **New workspace tab**, terminal spawned in the session's cwd, running the target's launch command | Split beside the focused pane (crowds the tab); Launch Pad (always creates a worktree and needs a git repo, `launch_pad.rs:250-253`, `:346`); injecting into an existing pane (an agent mid-turn would receive the text) |
| Targets | **Enabled launchers minus the row's own agent**; reader-less targets stay selectable with a "no session history" hint | Only the 9 `SessionAgent` variants (Amp / Copilot / Factory users could not target them); all 16 regardless of the visibility setting |
| Gesture | **Right-click context menu** on the row: Resume, Copy summary, Continue in ▸ | Trailing hover buttons; both |
| Block shape | Issue's five lines **plus a `Branch:` line when known, and an explicit marker when only the identifier fallback exists** | Keeping the five lines exactly |
| Payload | `summary`, else first user message, else identifier line (per the issue's 2026-09-03 Decisions comment) | `AgentSession.last_result` (only live panes have it: two payload rules) |
| Quota line | **Not included.** #332 (Usage HUD) was closed as unbuildable for Claude | — |

Left-click and drag on a row keep today's behaviour exactly
(`sessions_sidebar.rs:580` drag, `:625` resume).

## 3. Naming and placement

```
src-app/src/app/sessions_sidebar.rs          existing; gains only the row's right-click hook
src-app/src/app/sessions_handoff.rs          pure: HandoffPayload, handoff_prompt, payload_for,
                                              is_identifier_shaped, handoff_targets, caps
src-app/src/app/sessions_context_menu.rs     the row menu: state, render, the three actions
src-app/src/app/workspace_ops/tab.rs         + open_agent_tab_at_cwd (lifted from the drop handler)
src-app/src/app/event_handlers.rs            DropSessionSplit center band calls the lifted helper
src-app/src/app/ipc_handler.rs               schedule_prompt_prefill writes through inject_text
```

Two sibling files, no directory move: `sessions_sidebar.rs` is not being
restructured, and a `git mv` would bury the real diff.

## 4. The pure layer (`sessions_handoff.rs`)

No GPUI. Everything the tests pin lives here.

### 4.1 Payload

```rust
pub(crate) enum PayloadKind { Summary, FirstUserMessage, Identifier }

pub(crate) struct HandoffPayload {
    pub kind: PayloadKind,
    pub text: String,   // already capped, already trimmed
}

pub(crate) fn payload_for(meta: &SessionMeta) -> HandoffPayload
```

`SessionMeta` (`src-app/src/agent_sessions.rs:682-712`) has no separate
first-user-message field: every reader folds it into `summary`. Claude prefers
an AI title and falls back to the first user message
(`claude_sessions.rs:550`); Codex and Pi derive `summary` from the first user
record only; OpenCode uses the stored title; Gemini and Cursor scrape a line of
CLI stdout (`command_sessions.rs:204-219`). So the chain is:

1. `summary` present and **not identifier-shaped** → `Summary`.
2. `summary` present but identifier-shaped → treated as absent (there is no
   second field to fall to; the kind is recorded so the block can say so).
3. Nothing usable → `Identifier`, text `"<agent> session <id> from <cwd>"`.

`PayloadKind::FirstUserMessage` is reserved for readers that later grow a
separate field; today the pure function never returns it, and a test pins
that so the variant is not silently dead.

### 4.2 Identifier-shaped

`is_identifier_shaped(s: &str) -> bool` is true when, after trimming, the
string is a single line **and** any of:

- it contains a path separator and no whitespace (`/Users/x/proj`,
  `~/.gemini/tmp/abc`);
- it ends in a file extension of one to five ASCII alphanumerics and has no
  whitespace (`rollout-2026.jsonl`);
- it matches `<word> session <token>` where `<token>` has no whitespace;
- it is a bare UUID, ULID, or hex token (`[0-9a-fA-F-]{16,}`).

A real title with spaces (`Fix the worktree teardown race`) is never
identifier-shaped, even if it contains a slash. Multi-line text is never
identifier-shaped.

### 4.3 Block

```rust
pub(crate) const HANDOFF_SUMMARY_CAP: usize = 4 * 1024;   // bytes, char-boundary safe
pub(crate) fn handoff_prompt(meta: &SessionMeta, target: TerminalAgent) -> String
```

Output, exactly:

```
Continue this work from a prior <source-agent> session.
Session: <id>
Cwd: <cwd>
Branch: <git_branch>            ← line present only when git_branch is non-empty
Summary:
<payload text>
```

When the payload kind is `Identifier` the last two lines become:

```
Summary: (none recorded; only the session identifier is known)
<agent> session <id> from <cwd>
```

`<source-agent>` is `SessionAgent::display_name`. `<id>` is the validated id
(the same allow-list `resume_command_spec` applies at
`sessions_sidebar.rs:1116-1172`; an id that fails it makes `handoff_prompt`
return the block with `Session: (id withheld)` rather than paste an
unvalidated token). The summary is truncated at `HANDOFF_SUMMARY_CAP` on a
char boundary with a trailing ` […]`. Line endings are `\n`; trailing
whitespace is stripped from the payload. `target` is carried for the
provenance line only and for a future per-target preamble; today every target
gets the same text, and a test pins that too.

### 4.4 Clipboard text

`copy_text(meta) -> String` returns `payload_for(meta).text`. It is the same
chain as the block, not the block itself, so "Copy summary" and "Continue
in…" cannot disagree. Never the raw JSONL, never the file path.

### 4.5 Target list

```rust
pub(crate) fn handoff_targets(config: &PaneFlowConfig, source: SessionAgent)
    -> Vec<(TerminalAgent, bool /* has_session_reader */)>
```

`TerminalAgent::visible(config)` (`agent_launcher.rs:426`) minus
`source.terminal_agent()` (`agent_sessions.rs:50`). `has_session_reader` is
whether any `SessionAgent` maps to that launcher. With defaults that is
Claude Code, Codex, and Grok (`is_default_enabled`, `agent_launcher.rs:294`)
minus the source. An empty list disables the submenu with the caption
"Enable another agent in Settings ▸ AI Agent".

## 5. The gesture (`sessions_context_menu.rs`)

Right-click on a session row opens a menu anchored at the pointer. The hook
mirrors `files_sidebar/row.rs:149-164` (`e.is_right_click()` on
`on_mouse_down`) and the paint mirrors
`app/sidebar/context_menu.rs:181-235` (deferred element, right-button
propagation stopped). Items:

1. **Resume** — exactly what left-click does today (`resume_session_from_sidebar`).
2. **Copy summary** — `cx.write_to_clipboard(copy_text(meta))`, toast
   "Summary copied".
3. **Continue in ▸** — one item per `handoff_targets` entry, labelled with
   the launcher's display name; reader-less entries carry a dimmed
   "no session history" suffix. Disabled with the §4.5 caption when empty.

Menu state is one `Option<SessionMenuState { row_key, anchor }>` on the
sessions-sidebar state; Esc, click-away, and any action clear it. The menu
never captures focus from the bound pane beyond its own lifetime (the
existing sidebar-focus rule at `sessions_sidebar.rs:592-597` holds).

**Row unavailability.** If `meta.cwd` does not exist on disk at menu-open
time, Continue in ▸ is disabled with "Directory no longer exists"; Copy
summary stays enabled. This is the only filesystem read the menu performs and
it is a single `Path::is_dir`.

## 6. Placement (`open_agent_tab_at_cwd`)

Today the only path that spawns a terminal at an arbitrary cwd is inline in
the drop handler (`event_handlers.rs:654-770`), and `open_tab_with_surface`
(`workspace_ops/tab.rs:42`) reads the cwd from the active tab's worktree
binding or the workspace root (#347). The design lifts the drop handler's
center-band arm into:

```rust
pub(crate) fn open_agent_tab_at_cwd(
    &mut self,
    ws_idx: usize,
    cwd: PathBuf,
    command: Option<String>,      // sent with send_command after spawn
    declared: Option<TerminalAgent>,
    cx: &mut Context<Self>,
) -> Option<Entity<TerminalView>>
```

Behaviour, in order:

1. `pending_worktree_teardown_conflicts(&cwd)` → toast "Worktree is still
   being retired", return `None` (same gate as every other spawn path).
2. `TerminalView::with_cwd_and_profile(ws_id, Some(cwd), None,
   TerminalSurfaceProfile::Agent, cx)`; `send_command` and `declare_agent`
   when given.
3. `create_pane` then `open_pane_in_new_workspace_tab` (`event_handlers.rs:452`);
   on `false` (tab cap) toast the launcher's cap message and return `None`.
4. **Worktree binding (#347):** if `cwd` equals one of
   `ws.bound_tab_worktrees()` (`workspace/mod.rs:426`) or lies under a
   registered worktree root, the new tab's `Tab::worktree`
   (`workspace/tab.rs:50`) is set to that path, exactly as a dropped session
   would bind. Otherwise the tab is unbound.
5. `pending_pane_focus = Some(new_pane)`, `save_session`, `cx.notify()`.

The drop handler's center-band arm then becomes a call to this helper with
the resume command; the edge-band arm is untouched. This is the one
targeted refactor and it is behaviour-preserving for drops (a test in
`event_handlers.rs` pins that a center drop still lands in a new tab at the
session cwd).

**Continue in ▸ X** calls it with `command = Some(X.launch_command(config))`
(`agent_launcher.rs:410`) and `declared = Some(X)`, then schedules the
prefill (§7). The source workspace is the one that owns the sidebar's bound
pane; a palette-bound sidebar (`sessions_bound_palette`) uses the active
workspace.

## 7. Prefill

`schedule_prompt_prefill` (`ipc_handler.rs:2435`) already waits for the
terminal to settle (`UP_PREFILL_FLOOR` 1.8 s, `UP_PREFILL_MAX` 8 s, 200 ms
poll) and then writes. Today it writes with `send_text`, a raw PTY write. The
design changes that one call to `inject_text` (`terminal/input.rs:1146`),
which wraps the text in bracketed-paste markers when the surface has enabled
`ESC[?2004h` and otherwise sends it verbatim without rewriting `\n` to `\r`.
That is what keeps a six-line block from submitting early in a TUI that
supports paste mode, and from smuggling Enters into one that does not.

Launch Pad is the other caller of `schedule_prompt_prefill`
(`launch_pad.rs:596`); it gets the same improvement for free and its
never-submits tests stay green. Nothing sends a carriage return after the
block. There is no opt-in submit on this path; the composer's
`schedule_deferred_submit` is not wired.

Text passes through `normalize_composer_text` (`composer.rs:45`) first, so
CRLF is folded and the 64 KiB ceiling applies on top of the 4 KiB summary
cap.

## 8. Error handling

| Condition | Result |
|---|---|
| Session cwd missing | Continue in ▸ disabled with reason; Copy still works |
| Worktree at that cwd being retired | Toast, nothing spawned |
| Workspace tab cap | Toast (the launcher's message), nothing spawned |
| Id fails the resume allow-list | Block still produced with `Session: (id withheld)`; Resume is a no-op, as the row click is today |
| Target binary not on PATH | Not reachable: `visible` already filters uninstalled launchers |
| Terminal never settles | Prefill proceeds best-effort after `UP_PREFILL_MAX` with the existing warn log |
| Clipboard write fails | GPUI has no failure signal; the toast still shows |

## 9. Testing

Pure, no GPUI (`sessions_handoff.rs` tests):

- `handoff_prompt` for each of the three payload kinds matches the byte-exact
  shapes in §4.3, with and without `git_branch`.
- Cap: a 10 KiB summary yields a block whose summary section is
  `HANDOFF_SUMMARY_CAP` bytes plus the marker, cut on a char boundary
  (test with multi-byte text).
- `is_identifier_shaped`: table test over paths, filenames, UUIDs,
  `gemini session abc`, and five real titles that must be false.
- `payload_for` with `summary: None` returns `Identifier` and non-empty text.
- `payload_for` never returns `FirstUserMessage` today.
- `copy_text` equals the block's payload for the same meta.
- `handoff_targets` excludes the source, respects visibility, and flags the
  seven reader-less launchers.
- `handoff_prompt` output never contains the session file path or any
  `.jsonl` substring from a meta whose summary is a path.

Existing tests that must stay green: the `resume_command` set (same-agent
resume unchanged, flag-shaped ids refused), Launch Pad's never-submits tests
(`launch_pad.rs:13`, `:152`, `:595-596`), and the composer's opt-in submit
tests.

GPUI (`TestAppContext`):

- Center drop of a session row still opens a new tab whose terminal cwd is
  the session cwd (pins the §6 refactor).
- `open_agent_tab_at_cwd` with a cwd equal to a bound worktree binds the new
  tab; with an unrelated cwd leaves it unbound.
- `open_agent_tab_at_cwd` at the tab cap returns `None` and spawns nothing.
- Prefill into a surface with `BRACKETED_PASTE` set writes
  `\x1b[200~ … \x1b[201~` around the block and no trailing `\r`; without
  the mode, writes the block verbatim with `\n` intact and no `\r`.

## 10. Documentation changes

- `CLAUDE.md` sessions-sidebar line: right-click menu with Resume / Copy
  summary / Continue in ▸; handoff is prefilled, never submitted.
- `docs/user/` sessions page (if present): one paragraph and the block shape.
- Issue #334: close with the test names and the commit.

## 11. Explicitly out of scope

Same-CLI resume changes; auto-submit; transcript, tool-result, or screenshot
replay; any model call to write a summary; quota or remaining-window text
(#332 closed); changes to close-guard or undo-close; a per-target preamble
(the `target` parameter exists so one can be added without changing the
signature); hover controls on rows.

## 12. Verification

The six gates from `CLAUDE.md` before and after, plus:

```bash
cargo test -p paneflow-app handoff -- --nocapture
cargo test -p paneflow-app resume_command
cargo test -p paneflow-app launch_pad
```

Manual: with Claude Code and Codex both enabled, right-click a Claude row,
Continue in ▸ Codex, confirm a new tab opens in the session's directory,
Codex starts, the block appears in its prompt, and nothing is submitted
until Enter is pressed by hand.
