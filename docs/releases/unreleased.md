## What's new

### Breaking changes

The pane-driving CLI verbs `paneflow new`, `select`, `split`, and `focus`
have been removed and now exit with a command-line parse error. Their
JSON-RPC methods `workspace.create`, `workspace.select`, `surface.split`,
and `surface.focus` return method-not-found errors.

Use the app's controls to manage workspaces and panes. Run unattended agents
headlessly in separate git worktrees. The remaining read/send interfaces are
documented in the [scripting guide](../user/scripting.md).

PaneFlow no longer ships its MCP bridge, `paneflow-mcp`. The **MCP
Servers** Settings page, the sidebar "Install MCP bridge" callout, and the
`paneflow mcp` verb are gone; `paneflow mcp` now exits with the
unknown-verb usage error (exit code `2`). Agents and scripts read panes
through the JSON-RPC socket (`surface.read`, `surface.search`,
`agent.whoami`), documented in the
[scripting reference](../user/scripting/reference.md). A leftover
`mcp_bridge_prompt_dismissed` key in `paneflow.json` is ignored.

On first launch, PaneFlow removes the `paneflow` MCP entry it wrote to
Claude Code, Codex, Gemini CLI, and opencode. It removes only a `paneflow`
entry whose command is a `paneflow-mcp` binary, at any path; a `paneflow`
entry that runs anything else is left alone. Before changing a file it saves a
backup next to it: `<file>.bak` if that name is free, otherwise
`<file>.paneflow-bak`, then `<file>.paneflow-bak.1`, `.2`, and so on. It never
overwrites an existing file. The next launch checks again and, when it finds
no entry, deletes the extracted binary at
`~/Library/Application Support/paneflow/bin/paneflow-mcp`. If an entry cannot
be removed, PaneFlow keeps the binary so the entry keeps working, and retries
on the next launch.

The cleanup always checks each agent's default config location, plus the
locations named by the environment PaneFlow was launched with and by the
`CLAUDE_CONFIG_DIR`, `CODEX_HOME`, `OPENCODE_CONFIG`, `OPENCODE_CONFIG_DIR`,
and `XDG_CONFIG_HOME` values your login shell sets. It reads those login-shell
values only on a Dock or Finder launch with launchd's default `PATH`; after
`open --env PATH=…` or `launchctl setenv PATH …` it does not, a launch from a
terminal already has them, and a relative value is ignored. An entry in a
location it cannot see has to be removed by hand.

To remove a leftover entry by hand:

- Claude Code: `claude mcp remove -s user paneflow`
- Codex: `codex mcp remove paneflow`, or delete the
  `[mcp_servers.paneflow]` table from `~/.codex/config.toml`
  (`$CODEX_HOME/config.toml` when `CODEX_HOME` is set)
- Gemini CLI: delete `mcpServers.paneflow` from `~/.gemini/settings.json`
- opencode: delete `mcp.paneflow` from `opencode.jsonc` or `opencode.json`
  in `~/.config/opencode/` (`$XDG_CONFIG_HOME/opencode/` when
  `XDG_CONFIG_HOME` is set), and from the file `OPENCODE_CONFIG` names or the
  one in `OPENCODE_CONFIG_DIR` if you set either
