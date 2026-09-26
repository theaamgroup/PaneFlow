# Settings

PaneFlow's Settings panel is the human UI for common preferences. It is
not the full configuration reference. Use it for the settings you adjust
often: editor, shell, theme, shortcuts, notifications, terminal display,
sidebar order, new-tab branches, agent launchers, AI access, and MCP setup.

**TL;DR.** Most Settings rows write to `paneflow.json` and hot-reload
  after the file is saved. MCP Servers is different: it installs or
  repairs PaneFlow's MCP bridge in supported agent configs. Advanced keys
  remain available in [`paneflow.json`](configuration/schema.md).

## Settings map

| Page               | What it controls                                                                                                                                     | Writes to                                                                                                                                                                                                   | Applies                                                                     |
| ------------------ | ---------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------- |
| General            | Permissions (Claude Code bypass mode), AI free access, the default external editor (file links and Open in editor), the default shell for new terminal panes, the Review view, and native OS notifications for waiting agents. | `claude_code_bypass_permissions`, `ai_unrestricted`, `external_editor`, `default_shell`, `review_enabled`, `agent_panel.notify_when_agent_waiting` | Editor and shell apply to new launches; running terminals keep their current process. The toggles hot-reload. |
| Appearance         | Light, Dark, or System mode and the theme preset, reduce motion, unfocused pane opacity, and the macOS sidebar-material control. | `theme_mode`, `theme`, `reduce_motion`, `unfocused_pane_opacity`, `macos_chrome_material` | Hot-reloads. |
| Keyboard Shortcuts | Every action's binding, grouped by area (panes, workspaces, tabs, terminal, search, git diff, agents, application). Search by action name or by chord (`cmd+shift+j`), or press **Find by key** and type the chord to see what owns it. Click a row to record a new binding. **Reset to defaults** asks once before it rewrites every binding. | `shortcuts`                                                                                                                                                                                                 | Reloaded after the config save.                                             |
| Terminal           | Cursor shape and color, font family, font size, font weight, line height, cell width, integrated glyphs, and color emoji.                            | `terminal.cursor_shape`, `terminal.cursor_color`, `font_family`, `font_size`, `font_weight`, `line_height`, `cell_width`, `terminal.integrated_glyphs`, `terminal.color_emoji`                              | Display controls hot-reload. Cursor shape applies to the next new terminal. |
| Workspaces         | Sidebar auto-sort, and the default branch for new tabs, including a per-workspace override.                                                          | `workspace_auto_sort`, `new_tab_branch`, `workspace_new_tab_branches`                                                                                                                                       | Hot-reloads. Open terminals keep their checkout.    |
| AI Agent           | Whether the Agent sessions sidebar opens beside the New pane picker, and launcher button visibility. | `new_pane_shows_sessions`, `*_button_visible` | Hot-reloads. |
| MCP Servers        | Installs or repairs the bundled `paneflow-mcp` bridge for Claude Code, Codex, Gemini, and opencode.                                                  | Agent config files, not `paneflow.json`                                                                                                                                                                     | Re-run after a PaneFlow update or when an agent config changes.             |

## AI access vs MCP

The General page's Permissions and AI access sections control how
PaneFlow launches Claude Code and how much automation a trusted CLI client
can perform. MCP remains read-only.

`claude_code_bypass_permissions` only affects Claude Code launches. When
enabled, PaneFlow launches Claude Code with
`--permission-mode bypassPermissions`. It does not change Codex,
OpenCode, Gemini, or MCP behavior.

MCP Servers is a separate operational page. It registers the bundled
`paneflow-mcp` server so supported agents can list, read, and search
PaneFlow panes. It touches the agents' own config files and can be run
again safely.

## Config-only controls

Use [`paneflow.json`](configuration.md) when you need a setting that
is intentionally not in the primary Settings UI.

Common examples:

* `terminal.scrollback_lines` for per-terminal scrollback history.
* `terminal.ligatures`, `terminal.cursor_blink`, `terminal.env`, and
  `terminal.scroll_multiplier` for advanced terminal behavior.
* `terminal.minimum_contrast` for the APCA contrast floor (`0` to `90`)
  that rewrites the theme's ANSI colours against the cell background;
  `45` by default (the floor PaneFlow always enforced), `0` turns it off and
  shows the theme's literal colours. Hot-reloads.
* `terminal.osc52_clipboard` to stop programs from writing the system
  clipboard through OSC 52 (`"disabled"`; the default is `"copy_only"`).
* `commands`, a leftover array that is accepted and ignored.
* Profiles, window-decoration, and agent-panel options.
* `option_as_meta`, which defaults to off on macOS. Set it to `true` if you want Option to send an ESC prefix instead of producing Unicode input.

PaneFlow reloads valid config changes through its watcher (a `notify`
watcher with a 300 ms debounce and a 1 s max-wait ceiling). If the JSON
is malformed, the running app keeps the previous valid config instead of
falling back to defaults. The JSON Schema flags unknown keys in editors;
the runtime stays lenient so older builds can ignore newer keys.

## See also

* [Configuration](configuration.md) - file location, schema setup, and runtime behavior.
* [Schema](configuration/schema.md) - every key, type, default, and stability.
* [Shortcuts and actions](keybindings.md) - action names for `shortcuts` overrides.
* [Themes](themes.md) - bundled theme names and hot-reload behavior.
