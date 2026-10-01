## What's new

### Removed: Gemini CLI

Gemini CLI is no longer a built-in agent (issue #1132). Its launcher button,
its row in **Settings > AI Agent**, its Agent sessions sidebar group, and its
per-session hook injection are gone. A `gemini` you run in a pane still works,
as a plain program: no agent pill, no hooks, no sessions-sidebar entry.
Antigravity and every other built-in agent are unchanged.

A leftover `gemini_button_visible` key in `paneflow.json` is ignored. Editors
that validate against the published schema flag it, and you can delete it. A
saved session that recorded Gemini in a pane restores without the pill.

Earlier builds added PaneFlow hooks to `~/.gemini/settings.json` for each
Gemini session and removed them when the session ended. A session that was
killed could leave them behind. To clean up by hand, look under
`hooks.BeforeAgent`, `hooks.AfterAgent`, `hooks.BeforeTool`, and
`hooks.AfterTool`. Remove each hook entry named `paneflow-status` (its
command runs `paneflow-ai-hook`), then delete its matcher group if the group
is now empty. PaneFlow does not edit this file for you, and it never touches
your Gemini CLI installation or its data.
