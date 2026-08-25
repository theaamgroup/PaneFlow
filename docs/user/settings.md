# Settings

PaneFlow's Settings panel is the human UI for common preferences. It is
not the full configuration reference. Use it for the settings you adjust
often: editor, shell, theme, shortcuts, notifications, terminal display,
workspace templates, agent launchers, AI access, and MCP setup.

**TL;DR.** Most Settings rows write to `paneflow.json` and hot-reload
  after the file is saved. MCP Servers is different: it installs or
  repairs PaneFlow's MCP bridge in supported agent configs. Advanced keys
  remain available in [`paneflow.json`](configuration/schema.md).

## Settings map

| Page               | What it controls                                                                                                                                     | Writes to                                                                                                                                                                                                   | Applies                                                                     |
| ------------------ | ---------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------- |
| General            | Default external editor and default shell for new terminal panes.                                                                                    | `external_editor`, `default_shell`                                                                                                                                                                          | New launches. Running terminals keep their current process.                 |
| Themes             | Light, Dark, System, plus the macOS sidebar-material control.                                                                                        | `theme`, `macos_chrome_material`                                                                                                                                                                            | Theme and material changes hot-reload.                                      |
| Keyboard Shortcuts | Action bindings and reset to defaults.                                                                                                               | `shortcuts`                                                                                                                                                                                                 | Reloaded after the config save.                                             |
| Notifications      | Native OS notifications for waiting agents.                                                                                                          | `agent_panel.notify_when_agent_waiting`                                                                                                                                                                     | Hot-reloads.                                                                |
| Terminal           | Cursor shape and color, font family, font size, font weight, line height, cell width, integrated glyphs, and color emoji.                            | `terminal.cursor_shape`, `terminal.cursor_color`, `font_family`, `font_size`, `font_weight`, `line_height`, `cell_width`, `terminal.integrated_glyphs`, `terminal.color_emoji`                              | Display controls hot-reload. Cursor shape applies to the next new terminal. |
| Workspaces         | Reusable workspace templates with panes, agents, shell commands, cwd, env, and prompt prefill.                                                       | `commands[].workspace`                                                                                                                                                                                      | Templates run through the same workspace launch path as `paneflow up`.      |
| AI Agent           | Launcher button visibility, Claude Code bypass mode, AI free access, and the injection fence.                                                        | `*_button_visible`, `claude_code_bypass_permissions`, `ai_unrestricted`, `ai_injection_fence`                                                                                                               | Launcher and access changes hot-reload.                                     |
| MCP Servers        | Installs or repairs the bundled `paneflow-mcp` bridge for Claude Code, Codex, Gemini, and opencode.                                                  | Agent config files, not `paneflow.json`                                                                                                                                                                     | Re-run after a PaneFlow update or when an agent config changes.             |

## AI access vs MCP

The AI Agent page controls how PaneFlow launches agents and how much
automation a trusted conductor can perform. The conductor feature itself
is known unreliable in this fork: see [conductor.md](conductor.md).

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
* `commands[]` entries that are not workspace templates.
* Profiles, telemetry, window-decoration, and agent-panel options.
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
