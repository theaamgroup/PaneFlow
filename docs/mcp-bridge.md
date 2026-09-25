# PaneFlow MCP bridge (`paneflow-mcp`)

Let an MCP-capable CLI agent running **inside a PaneFlow pane** read the
terminal output of **other surfaces in the current workspace** - so you can say
*"check the logs in the cargo-run pane"* instead of selecting, copying, and
pasting by hand.

`paneflow-mcp` is a small stdio [MCP](https://modelcontextprotocol.io) server.
The agent spawns it as a subprocess; it proxies each call to PaneFlow's local
JSON-RPC socket (the same one the AI-hook uses). It can list, read, and search
surfaces and report the calling pane's own identity. It is read-only: it cannot
type into or control terminals.

By default the bridge inherits `PANEFLOW_WORKSPACE_ID` from the pane that
launched the agent and filters discovery, tools, and resources to that
workspace. `surface.list` / `surface.read` / `surface.search` accept an optional
`workspace_id` and enforce membership server-side; the bridge still applies its
own client-side filter. Set `PANEFLOW_MCP_SCOPE=all` only when instance-wide
read access is intentional.

> Source: `crates/paneflow-mcp/`. The protocol is implemented by hand (not via
> `rmcp`) to keep the dependency tree tiny and the surface fully unit-tested.

## Tools

Every tool is annotated read-only.

| Tool | Arguments | Returns |
|------|-----------|---------|
| `list_panes` | - | Scoped surfaces: `surface_id`, `name`, `title`, `cwd`, `cmd`, `workspace`, `workspace_id`, plus `tab_id` / `tab_title` for the workspace tab holding the surface. Call this first to discover what to read. The result is wrapped as untrusted terminal metadata. |
| `read_pane` | `target` (name or `surface_id`), `lines?` (default 200, max 4000), `offset?` | The surface's scrollback followed by the screen it is painting, as text, paginated. A full-screen TUI has no scrollback, so the screen is what you get. |
| `search_pane` | `target`, `pattern`, `max_matches?` (default 50, max 1000) | Matching lines with their line numbers. |
| `whoami` | - | The calling pane's own identity. See [Pane identity](#pane-identity-whoami). |

`target` resolves by exact name → case-insensitive → unique prefix, or a numeric
`surface_id`. An ambiguous name returns an error listing the candidates.

> **Security.** Returned content is wrapped in an `<untrusted_terminal_output>`
> marker. A pane may contain attacker-controlled output (a server logging a
> crafted string), and pane titles can also be terminal-controlled; the agent
> is instructed to treat bridge output as data, never as instructions to
> execute. The bridge exposes no keystroke tool and no write tool.

`tab_id` is a stable identity, never a positional index, and it is omitted for
surfaces that live outside the CLI tab hierarchy (Agents threads, the bottom
dock) or when the running PaneFlow predates it. Targeting stays by surface
name or `surface_id`: the tab is context for the agent, not an addressing key.

MCP resources use stable `surface_id` URIs:
`pane://surface/{surface_id}/content`. Human names and titles stay in
`list_panes`; they are display metadata, not URI syntax.

## Pane identity (`whoami`)

An agent can ask which pane it is running in with the MCP `whoami` tool, which
calls the `agent.whoami` IPC method.

`whoami` returns a persisted `pane_id`, the current runtime `surface_id`, a
`terminal_session_id` for this terminal lifetime, workspace ID/title/cwd, current
cwd, tab ID, bound worktree path, and observed agent sessions.
`terminal_session_id` is **not** a Claude/Codex conversation ID. Observed agents
carry a process-map key, tool, state, source, and observation age. An empty list
means no agent has been mapped to this pane; it does not mean no agent is running.
PaneFlow does not pick one when several agents share a terminal.

The bridge inherits `PANEFLOW_SURFACE_ID` and `PANEFLOW_WORKSPACE_ID` from the
pane's environment. Both are required; it never guesses from focus.
`agent.whoami` requires numeric `surface_id` and `workspace_id`, rejects any
other parameter, and resolves the surface's live workspace; the response carries
that current `workspace_id`. A moved pane keeps its PTY and the old workspace ID
in its shell environment, so the call keeps working without restarting the agent,
even if the original workspace is closed. The agent's sidebar state moves with
it: a tab or pane drag carries the pane's session rows to the destination
workspace, and `ai.*` hook frames that still carry the inherited workspace ID are
routed to the surface's live workspace whenever the frame names a surface that
exists. Missing or closed surfaces remain errors.

The `whoami` tool accepts no target or workspace argument. Even with
`PANEFLOW_MCP_SCOPE=all`, it addresses only the inherited caller pane. Raw IPC
remains a same-user operation; environment IDs are routing metadata, not
authentication credentials. Peer discovery and terminal reads retain the
bridge's original workspace scope; following one's own moved pane does not grant
access to its new workspace's peers.

`pane_id` is saved per terminal surface as `surfaces[].agent_context` in
`session.json` through the normal debounced save. Session restoration and
undo-close keep it; `terminal_session_id` changes on reconstruction. Omit
`agent_context` from reusable templates; it is session-owned metadata. Sessions
saved by builds that had task assignment still carry an `agent_context.task`
key. It loads, is ignored, and is dropped on the next save; the pane keeps its
`pane_id`.

## Install (one command)

The bridge binary **ships inside PaneFlow** - no build step. On every launch,
PaneFlow extracts it to a stable, non-versioned path
(`~/Library/Application Support/paneflow/bin/paneflow-mcp`) that survives
updates.

To register the bridge with every CLI agent installed on your machine, run
this from any PaneFlow pane (`paneflow` is on every pane's `PATH`) or from a
shell where you have linked the bundle binary:

```bash
paneflow mcp install
```

It detects which agents are present (Claude Code, Codex, Gemini CLI, opencode),
writes the `paneflow` entry into each one's config, and reports per agent:

```text
claude-code: installed (/Users/you/Library/Application Support/paneflow/bin/paneflow-mcp)
codex: installed (/Users/you/Library/Application Support/paneflow/bin/paneflow-mcp)
gemini: skipped (not detected)
opencode: skipped (not detected)
```

The command is **idempotent** (re-running it is a no-op when nothing changed),
**no-clobber** (it only touches the `paneflow` entry, preserving every other MCP
server and setting), and **backed up** (the prior config is copied to
`<file>.bak` before any write). Run it again after a PaneFlow update if `status`
reports a stale path.

```bash
paneflow mcp status      # report state per agent (read-only)
paneflow mcp uninstall   # remove only the `paneflow` entry, everywhere
```

PaneFlow also offers the install from the sidebar. When a pane runs Claude
Code, Codex, Gemini CLI, or opencode and that agent's MCP config has no
`paneflow` entry, a one-time "Let <agent> see other panes - Install MCP bridge"
callout appears in the sidebar footer, beside the IPC notice. Its button runs
the same off-thread installer as Settings → MCP Servers and the callout goes
away once `status` reports the agent installed. The `×` dismisses it for that
agent only, remembered in `paneflow.json` as `mcp_bridge_prompt_dismissed` (a
list of agent ids: `claude-code`, `codex`, `gemini`, `opencode`); remove an id
from that list to see the offer again. Debug builds do not show the callout
unless `PANEFLOW_ALLOW_DEBUG_MCP_INSTALL=1`, the same gate the installer
itself honours.

`status` distinguishes five states per agent: *not detected*, *installed*,
*detected but not installed*, *stale path*, and *needs repair* when a
`paneflow` entry exists but is disabled or no longer matches PaneFlow's managed
schema. `status` never extracts or writes the bridge binary.

> Where each agent's entry lands: Claude Code: `~/.claude.json`
> (`mcpServers.paneflow`, backed up before `claude mcp add -s user`); Codex:
> `$CODEX_HOME/config.toml` when `CODEX_HOME` is set, otherwise
> `~/.codex/config.toml` (`[mcp_servers.paneflow]`, backed up before
> `codex mcp add`); Gemini CLI: `~/.gemini/settings.json`
> (`mcpServers.paneflow`, `trust: true`); opencode: `OPENCODE_CONFIG`, or
> `OPENCODE_CONFIG_DIR`, or the global `opencode.jsonc` / `opencode.json`
> config (key `mcp`, `command` as an array, `type: "local"`).

### Not supported: aider

aider does not consume MCP. There is no bridge path for it; feed it pane output
manually (e.g. `--read <file>`).

## Manual configuration (if you prefer)

`paneflow mcp install` is the recommended path. If you'd rather wire it by hand
 -  or you're working in this repo, where `.mcp.json` already registers the
server for Claude Code - use the snippets below. Build the binary first with
`cargo build -p paneflow-mcp --release` (→ `target/release/paneflow-mcp`), and
point `command` at that absolute path.

> These config shapes are **version-volatile** for Codex, Gemini, and opencode -
> their CLIs move fast. `paneflow mcp install` tracks the current format; verify
> manual snippets against each agent's current docs.

The bridge finds the running PaneFlow instance via `$PANEFLOW_SOCKET_PATH`,
injected into every pane's environment - so it must be launched from inside a
PaneFlow pane (which is exactly where your agent runs).

### Claude Code

```bash
claude mcp add -s user --transport stdio paneflow -- /absolute/path/to/paneflow-mcp
```

Or directly in `~/.claude.json` under `mcpServers.paneflow`:
`{"type": "stdio", "command": "/absolute/path/to/paneflow-mcp", "args": []}`.
Claude Code consumes MCP **tools** and resources.

### Codex CLI

`$CODEX_HOME/config.toml` when `CODEX_HOME` is set, otherwise
`~/.codex/config.toml`:

```toml
[mcp_servers.paneflow]
command = "/absolute/path/to/paneflow-mcp"
args = []
env_vars = ["PANEFLOW_SOCKET_PATH", "PANEFLOW_WORKSPACE_ID", "PANEFLOW_SURFACE_ID"]
```

Codex must forward all three variables from its pane to the bridge. Older
PaneFlow installations that omit `PANEFLOW_SURFACE_ID` are marked **needs repair**;
run `paneflow mcp install` with the updated build and restart the Codex session.
Install preserves any custom `env_vars` entries.

A static `env` value for `PANEFLOW_MCP_SCOPE`, `PANEFLOW_SOCKET_PATH`,
`PANEFLOW_WORKSPACE_ID`, or `PANEFLOW_SURFACE_ID` is also **needs repair**.
Install deletes those keys and leaves every other `env` entry.
`PANEFLOW_MCP_SCOPE = "all"` in that table would let the bridge read every
workspace's terminal scrollback instead of the pane that launched Codex.

Codex consumes **tools only** - which is why the bridge exposes everything as
tools, not MCP resources.

### Gemini CLI

`~/.gemini/settings.json`:

```json
{
  "mcpServers": {
    "paneflow": {
      "command": "/absolute/path/to/paneflow-mcp",
      "args": [],
      "trust": true
    }
  }
}
```

`paneflow mcp install` sets `trust: true`. The installer owns that flag:
repair treats any other value as damage and writes `true` back. The bridge
is a local binary you control, so Gemini's per-call confirmation adds only
friction.

### opencode

`opencode.jsonc` or `opencode.json` in opencode's global config location
(or `OPENCODE_CONFIG` / `OPENCODE_CONFIG_DIR`) - note the distinct schema
(key `mcp`, not `mcpServers`; `command` is an array; `type: "local"`):

```json
{
  "mcp": {
    "paneflow": {
      "type": "local",
      "command": ["/absolute/path/to/paneflow-mcp"],
      "enabled": true
    }
  }
}
```

## Example

In an agent running inside PaneFlow:

> *"List my panes, then read the last 100 lines of the cargo-run pane and tell
> me why the build failed."*

The agent calls `list_panes`, sees a surface named `cargo-run`, then
`read_pane(target="cargo-run", lines=100)` - no manual copy-paste.
