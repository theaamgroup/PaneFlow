# Agent notification hooks (`paneflow hooks`)

PaneFlow ships a tiny callback binary, `paneflow-ai-hook`, that an agent CLI
runs on lifecycle events (prompt submitted, tool use, stop, notification) to
report its turn state to a running PaneFlow instance over the IPC socket. That
state drives the sidebar activity indicators and the turn-end desktop
notification (EP-004, `prd-cli-agent-orchestration`).

There are two ways the hook gets registered, and a single authority rule that
keeps them from firing twice.

## Two installers

| Installer | Scope | Where it writes | Lifetime |
|-----------|-------|-----------------|----------|
| **Ephemeral shim** (`paneflow-shim`) | project | `./.claude/settings.local.json` in the launched project | written on agent launch, swept on exit |
| **Persistent setup** (`paneflow hooks setup`) | user | `~/.claude/settings.json` | written once, survives restarts and PaneFlow updates |

A managed command names the stable, non-versioned
`data_dir()/paneflow/bin/paneflow-ai-hook` when the app set
`PANEFLOW_AI_HOOK_PATH` (`runtime_paths::ai_hook_binary_path`). Otherwise it
names the version-pinned copy under `cache_dir()/paneflow/bin/<VERSION>/`.
The stable path is what survives an update. A command already stored with a
version directory is removed once that directory is gone; see below.

Both write the *byte-identical* matcher-group shape, tagged with a
`_paneflow_managed` marker, so each side recognizes the other's entries.

## Authority rule (anti double-firing)

**The persistent user-scope install wins.** When `paneflow hooks setup` has
installed managed hooks in `~/.claude/settings.json`, the shim detects them
(`persistent_claude_hooks_present`, reusing the same shape detector) and:

1. **skips** its ephemeral `./.claude/settings.local.json` injection, and
2. **sweeps** any orphan `settings.local.json` it left on a prior run.

Result: the agent fires each event exactly once (one `ai.*` frame per event, no
duplicates) and no `settings.local.json` is planted in your project tree once
you have run `hooks setup`.

If you have **not** run `hooks setup`, the shim's ephemeral injection is the
only mechanism, and it cleans up after itself on exit.

## Dead commands

Grok and Claude Code both execute `<project>/.claude/settings.local.json`.
A block left behind when a pane is killed still names whatever binary wrote
it. After an upgrade deletes that version directory, every tool call fails
with `exit code 127` and the source label `project/settings.local`.

Every wrapped agent launch, before the real CLI starts, removes PaneFlow
hook commands whose program is not on disk from that file and from
`<project>/.codex/hooks.json`. The walk stops at the git checkout that
contains the working directory. A linked worktree also checks the main
checkout, which is the file Claude Code opens. User permissions, user
hooks, and any PaneFlow command whose binary still exists stay. The file
itself is not deleted.

## Commands

```bash
paneflow hooks setup       # install persistent hooks for every supported agent
paneflow hooks status      # report per-agent install state
paneflow hooks uninstall   # remove only PaneFlow-managed hooks (no clobber)
```

Exit codes mirror `paneflow mcp`: `0` success (or no agent detected), `1` an
agent errored, `2` usage error. Writes are atomic, backed up, and refuse to
overwrite a present-but-invalid JSON config.

`uninstall` removes only the `_paneflow_managed` matcher-groups; your own hooks
and every other key in the file are left untouched. To fully revert: run
`paneflow hooks uninstall`, then (if you never want the shim's ephemeral copy
either) there is nothing else to clean up because the shim removes its own file
on exit.

## Per-agent support

Only **Claude Code** exposes a verified, file-based user-scope notification-hook
surface, so it is the only agent that receives a persistent install
(`paneflow hooks setup`). Every other integration is EPHEMERAL: injected by
the shim when the agent launches inside a PaneFlow terminal, removed when it
exits. The shim wraps all 18 `TerminalAgent` binaries; whatever has no hook
surface below still gets the universal lifecycle (`ai.exit` on crash,
`ai.session_end` on quit) plus the sidebar's "running" row from the process
scan.

| Agent | Mechanism | Where the shim writes | Events mapped |
|-------|-----------|----------------------|---------------|
| Claude Code | Claude hooks (matcher groups) | `./.claude/settings.local.json` | UserPromptSubmit, Notification, Stop, Pre/PostToolUse, SubagentStart/Stop |
| Codex | hooks.json + TOML feature flag | `./.codex/hooks.json` | SessionStart, UserPromptSubmit, Stop, Pre/PostToolUse, PermissionRequest, SubagentStart/Stop |
| CodeBuddy | Claude-compatible clone | `./.codebuddy/settings.local.json` | same seven as Claude Code |
| Qoder | Claude-compatible clone | `./.qoder/settings.local.json` | four (no Notification) |
| Gemini CLI | matcher-group hooks in settings | `~/.gemini/settings.json` | BeforeAgent→UserPromptSubmit, AfterAgent→Stop, Before/AfterTool→Pre/PostToolUse |
| Cursor | flat hooks.json (`version: 1`) | `~/.cursor/hooks.json` | beforeSubmitPrompt, stop, pre/postToolUse, subagentStart/Stop |
| OpenCode | TS plugin + `plugin` entry | `~/.config/opencode/plugins/paneflow-status.ts` + `opencode.json` | chat.message, tool.execute.before/after, session.created/status/idle (child sessions as subagents), permission.asked |
| Pi | TS extension (auto-loaded) | `~/.pi/agent/extensions/paneflow-status.ts` | agent_start/end, tool_execution_start/end |
| Hermes | marked YAML block | `~/.hermes/config.yaml` | pre/post_llm_call, pre/post_tool_call, pre_approval_request |
| Grok | dedicated merged hook file (wholly PaneFlow-owned) | `~/.grok/hooks/paneflow.json` | UserPromptSubmit, Stop, Pre/PostToolUse, PermissionRequest, SubagentStart/Stop |
| DeepSeek Harness | `--patch` overlay + Claude-compatible bridge | `~/.dsh/paneflow/{hooks.json,paneflow-overlay.yml}` | UserPromptSubmit, Pre/PostToolUse, Stop |
| Muse Code | managed hook file (`managed_hooks_path` + `managed_hooks_env_vars` merged into `settings.json`; hooks run with a cleared environment) | `~/.config/muse/{paneflow-hooks.json,settings.json}` | UserPromptSubmit, Pre/PostToolUse, PermissionRequest, PostLLMCall (as Stop), Stop |

Subagent events become `ai.subagent_start` / `ai.subagent_stop`, never
`ai.stop`: a subagent finishing is not the parent's turn ending. Each frame
carries the agent's own pairing id as `hook_payload.subagent_id` (Claude Code
and Codex `agent_id`, Cursor `subagent_id`, Grok `subagentId`); a frame
without one is dropped, since an unpaired start would hold the sidebar's
running count up until the agent exits. The count keys ids per agent PID, so a
start that arrives twice (Grok also runs the Claude and Cursor hook files) is
counted once. Grok's id field is read from its binary, not its docs.

Safety properties shared by every ephemeral installer: idempotent merge,
ownership detection by command basename (`paneflow-ai-hook`), orphan sweep on
the next launch after a SIGKILL, and refusal paths that protect user files -
a symlinked config dir, any present-but-unparseable JSON config (the
project-local `settings.local.json` / `hooks.json` files included, left
byte-identical - the agent still launches, hookless), an unparseable PRIMARY
config (`opencode.json`, `~/.hermes/config.yaml` with an existing `hooks:`
key), or a `.jsonc`-only OpenCode setup all skip the install instead of
clobbering. The TS bridges are
env-gated on `PANEFLOW_SOCKET_PATH`, so they are inert when the CLI runs
outside a PaneFlow terminal.

Deliberately not integrated (no safe surface): **Copilot CLI** (no hooks, no
JSON stream), **Factory Droid** (dashboard-managed hooks), **Kiro** (hooks
live inside per-agent definition files - no per-session surface),
**Antigravity / Openclaw** and the remaining launchers (no stable public
hook surface). They still get the universal exit/session-end lifecycle and
the "running" row.

## Parent-death and interrupt guards

The shim (`paneflow-shim`) wraps each agent so two reliability gaps are closed
(EP-005 US-017):

- **Orphan guard** (PaneFlow is hard-killed, e.g. `kill -9`): the agent must not
  survive and keep burning API tokens. kqueue `NOTE_EXIT` is not arm-able from
  the post-`execve` child, so the shim runs a tiny thread that polls
  `getppid()`; a reparent to `launchd` means PaneFlow exited and the agent is
  `SIGKILL`ed (`spawn_parent_death_guard`, `crates/paneflow-shim/src/exec.rs`).
  The guard is told the child was reaped before its PID can be recycled, so a
  late tick never signals a reused PID.
- **Interrupt guard** (the user Ctrl+C's an agent mid-turn, which interrupts the
  turn WITHOUT the agent exiting or firing a Stop hook): the sidebar loader must
  not stick. A blocked-`SIGINT` + `sigwait` thread emits one `ai.stop` per
  Ctrl+C.

Both guards still need a one-time RUNTIME smoke on real hardware:

- **Orphan smoke**: launch an agent in a pane, note its PID (`paneflow ps`),
  `kill -9` the PaneFlow process, then confirm the agent PID is gone within
  ~1 s (`ps -p <pid>` returns nothing). PASS = no orphan.
- **Interrupt smoke**: launch an agent, start a turn so the sidebar shows the
  "thinking" loader, press `Ctrl+C` to interrupt mid-turn (the agent stays
  alive at its prompt), then confirm the loader clears within ~5 s. PASS = no
  stuck spinner.

## Configuration lease storage

Agent-configuration leases always use the OS user configuration directory
under `paneflow/agent-config-leases`, independent of `PANEFLOW_HOME` and build
profile. This intentional exception to app-directory isolation keeps one lock
and ownership record for external agent files shared by multiple instances.
The final holder removes its `.lock` file. Acquirers recheck the locked inode
against the path before using a lease, so an opener racing cleanup retries
instead of holding an obsolete lock. Ownership `.created` markers survive
until the final resource cleanup consumes them.

When upgrading from a build with persistent lease files, quit PaneFlow and
stop its agent/shim processes before starting the new build. With all those
processes stopped, old `.lock` files in the `agent-config-leases` directory
can be removed; retain `.created` files, which record ownership of managed
configuration. Never clear lease files while an older process is running.
