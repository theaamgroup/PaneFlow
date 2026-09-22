## What's new

### Breaking changes

The pane-driving CLI verbs `paneflow new`, `select`, `split`, and `focus`
have been removed and now exit with a command-line parse error. Their
JSON-RPC methods `workspace.create`, `workspace.select`, `surface.split`,
and `surface.focus` return method-not-found errors.

Use the app's controls to manage workspaces and panes. Run unattended agents
headlessly in separate git worktrees. The remaining read/send interfaces are
documented in the [scripting guide](../user/scripting.md).
