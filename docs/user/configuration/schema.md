# Configuration schema

Every key PaneFlow recognises today, grouped by what it controls. All
keys are optional unless noted. The authoritative machine-readable source
is the JSON Schema in this repo at
[`schemas/paneflow.schema.json`](../../../schemas/paneflow.schema.json).
Two tests in `crates/paneflow-config/src/schema.rs` diff the Rust structs
against that file, so drift fails the suite.

## File location

The path depends on the build profile, because `APP_SUBDIR`
(`crates/paneflow-config/src/loader.rs`) switches namespace under
`debug_assertions`:

| Build | Config path |
|---|---|
| Release | `~/Library/Application Support/paneflow/paneflow.json` |
| Debug (`cargo run`) | `~/Library/Application Support/paneflow-dev/paneflow.json` |

A from-source debug build therefore ignores edits to the release path.
There is no env-var override for the config location.

Unknown top-level keys are ignored by the runtime. The schema uses
`additionalProperties: false` so editors can flag typos before launch.
That strictness is an editor-side aid only; it never affects loading.

## Top-level keys

| Key | Type | Default | Notes |
|---|---|---|---|
| `$schema` | string | none | Editor-only pointer to the public schema. Ignored at runtime. |
| `$schemaVersion` | string | `1.0.0` | Logs a warning when unknown, but never blocks loading. |
| `default_shell` | string or null | platform default | Chain: configured -> `$SHELL` -> `/bin/sh`. Each candidate must be an existing file with an exec bit, otherwise it is skipped with a warning. A bare name (no `/`) is resolved via `which`, then probed in `/opt/homebrew/bin`, `/usr/local/bin`, `/usr/bin`, `/bin`, which is what makes `"fish"` work under a GUI launch with a minimal `PATH`. |
| `theme` | string or null | `PaneFlow Dark` | Bundled theme name, one preset's light or dark variant. Current values: `PaneFlow Dark`, `PaneFlow Light`, `Vercel Dark`, `Vercel Light`, `Claude Dark`, `Claude Light`, `Cursor Dark`, `Cursor Light`. Pre-preset names (`One Dark`, `Vercel`, `Claude`, `Cursor`) still resolve. |
| `theme_mode` | string or null | `dark` | `light`, `dark`, or `system`. |
| `font_family` | string or null | bundled JetBrainsMono NFM | Accepts `.PaneflowMono`, `JetBrainsMono NFM`, `.PaneflowSans`, embedded family names, or installed monospace families. |
| `font_fallbacks` | array of strings or null | none | Ordered glyph fallback families for symbols, Powerline, CJK, emoji, or Nerd Font glyphs. |
| `font_size` | number or null | `13.0` | Points, range `8.0` to `32.0`. Out-of-range values fall back to default with a warning. |
| `font_weight` | string or null | `normal` | `thin`, `extra_light`, `light`, `semi_light`, `normal`, `medium`, `semi_bold`, `bold`, `extra_bold`, `black`, `extra_black`. |
| `line_height` | number or null | `1.2` | Multiplier, range `1.0` to `2.5`. Out-of-range values revert to the default with a warning; they are not clamped. |
| `cell_width` | number or null | `0.6` | Multiplier, range `0.3` to `2.0`. Out-of-range values revert to the default with a warning; they are not clamped. |
| `unfocused_pane_opacity` | number or null | `0.7` | Opacity of panes without focus when a workspace has more than one pane, range `0.15` to `1.0`. `1.0` disables the dim. Values outside the range are clamped with a warning; non-finite values fall back to the default. |
| `reduce_motion` | boolean or null | `false` | Minimize non-essential interface motion: hover transitions settle instantly and decorative animations render a static frame. |
| `workspace_auto_sort` | boolean or null | `false` | Order the workspace sidebar automatically: pinned first, then workspaces with something running, then idle ones, alphabetically within each group. Sibling git worktrees stay contiguous. Drag-to-reorder is disabled while this is on. |
| `workspace_zed_menu_visible` | boolean or null | installed detection | Show the **Open in Zed** workspace context-menu row. `true` always shows it, `false` hides it, and null/omitted shows it only when the Zed CLI is installed. |
| `workspace_cursor_menu_visible` | boolean or null | installed detection | Show the **Open in Cursor** workspace context-menu row. `true` always shows it, `false` hides it, and null/omitted shows it only when the Cursor CLI is installed. |
| `workspace_vscode_menu_visible` | boolean or null | installed detection | Show the **Open in VS Code** workspace context-menu row. `true` always shows it, `false` hides it, and null/omitted shows it only when the VS Code CLI is installed. |
| `workspace_windsurf_menu_visible` | boolean or null | installed detection | Show the **Open in Windsurf** workspace context-menu row. `true` always shows it, `false` hides it, and null/omitted shows it only when the Windsurf CLI is installed. |
| `window_decorations` | string or null | `client` | `client` for PaneFlow chrome, `server` for OS chrome. Read once at startup; requires a restart. |
| `window_backdrop` | string or null | `auto` | Accepted: `auto`, `blurred`, `transparent`, `opaque`, `off`. Read once at startup. See the resolution table below: the values do not map one-to-one on macOS. |
| `macos_chrome_material` | boolean or null | `true` | Reveals AppKit's native Sidebar material across the whole window shell: the primary rail, panel inset, and pane gutters. Silently disabled when `window_backdrop` is `opaque`, `off`, or `transparent`. |
| `option_as_meta` | boolean or null | `false` | Option produces Unicode input by default. Set to `true` to send Option/Alt as an ESC prefix. |
| `shell_integration` | boolean or null | enabled | Master switch for shell rc injection: OSC 7 CWD reporting and OSC 133 command marks. |
| `agent_stall_detection` | boolean or null | `true` | Enables stalled-agent detection. |
| `agent_stall_threshold_secs` | integer or null | `60` | Silence threshold before a Thinking agent is marked Stalled. Clamped to `30` to `86400`. |
| `review_enabled` | boolean or null | `true` | Master switch for the Review surface. `false` hides the Review view and its sidebar tab, makes the Review shortcut a no-op, and reopens a Review-mode session in the terminal view. |
| `new_pane_shows_sessions` | boolean or null | `false` | When true, a Tab-placement New pane picker also opens the Agent sessions sidebar, scoped to the workspace cwd, so a listed session can be resumed into the new pane. Split-placement pickers leave the sidebar alone. |
| `review_prefill_delay_ms` | integer or null | `2000` | Delay before Review pre-fills a freshly launched CLI. Clamped to `250` to `10000`. |
| `submit_paste_delay_ms` | integer or null | `70` | Minimum delay between bracketed paste and submit carriage return. Clamped to `10` to `5000`. |
| `external_editor` | string or null | `auto` | `auto`, `system`, `zed`, `cursor`, `windsurf`, `code`. |
| `shortcuts` | object | `{}` | Custom keybindings: `{ "ctrl+shift+t": "new_tab" }`. |
| `terminal` | object or null | defaults below | Terminal renderer and PTY settings. |
| `commands` | array | `[]` | Command palette entries and workspace templates. |
| `claude_code_bypass_permissions` | boolean or null | `false` | Adds Claude Code `--permission-mode bypassPermissions` when launching from PaneFlow. |
| `ai_unrestricted` | boolean or null | `false` | Allows trusted automation to submit via IPC without `PANEFLOW_IPC_SCRIPTING=1`. |
| `ai_injection_fence` | boolean or null | `true` | Wraps pane reads in an untrusted-output fence. Keep enabled for AI conductors. |
| `agent_button_visibility_defaults_migrated` | boolean or null | `null` | Internal one-time marker recording that a pre-allowlist config preserved its installed launcher buttons as explicit values. Runtime visibility does not otherwise consult it. |
| `agent_panel` | object or null | defaults below | Agents-view display, profiles, and notification settings. |
| `tool_permissions` | object | `{}` | Per-tool always-allow and always-deny input patterns. |

### How `window_backdrop` resolves on macOS

The accepted strings collapse into fewer real behaviours. Parsing is
case-insensitive and trimmed (`src-app/src/app/constants.rs`); an
unrecognised value warns and falls back to `auto`. Legacy `mica` and
`acrylic` values still load (Transparent and Blurred) but are not part of
the published schema.

| Value | Effective on macOS |
|---|---|
| `auto` | Transparent |
| `blurred` | Blurred |
| `transparent` | Transparent |
| `opaque`, `off` | Opaque |

`opaque`, `off`, and `transparent` also silently switch off
`macos_chrome_material`, whatever that key is set to. If the whole-shell
material disappears after a backdrop change, this is why.

## Workspace context-menu rows

The four `workspace_*_menu_visible` controls live under **Settings >
Workspaces**. They change only whether the matching editor row is rendered in
a workspace context menu. Editor keybindings remain available and behave the
same way regardless of these visibility settings.

## Agent buttons

Each button visibility key is `boolean or null`. `true` always shows the
button and `false` always hides it. For a fresh config, `null` or an omitted
key shows Claude Code, Codex, or Grok only when that CLI is installed; the
other 13 agents default off even when installed.

On the first launch after upgrading from the old all-installed default,
PaneFlow preserves an existing valid config by writing explicit `true` values
for its installed agents and setting `agent_button_visibility_defaults_migrated`
in the same atomic write. A missing file gets only the marker, so a genuinely
fresh config uses the new allowlist. Explicit booleans and unknown keys are
preserved; an invalid config is left untouched.

| Key | Agent | Fresh null/omitted default |
|---|---|---|
| `claude_code_button_visible` | Claude Code | On if installed |
| `codex_button_visible` | Codex | On if installed |
| `grok_button_visible` | Grok | On if installed |
| `opencode_button_visible` | Opencode | Off |
| `pi_button_visible` | Pi | Off |
| `hermes_agent_button_visible` | Hermes Agent | Off |
| `amp_button_visible` | Amp | Off |
| `cursor_button_visible` | Cursor | Off |
| `gemini_button_visible` | Gemini | Off |
| `kiro_button_visible` | Kiro | Off |
| `antigravity_button_visible` | Antigravity | Off |
| `copilot_button_visible` | Copilot | Off |
| `codebuddy_button_visible` | CodeBuddy | Off |
| `factory_button_visible` | Factory | Off |
| `qoder_button_visible` | Qoder | Off |
| `openclaw_button_visible` | Openclaw | Off |

## Terminal block

| Key | Type | Default | Notes |
|---|---|---|---|
| `terminal.backend` | string or null | `auto` | `auto` or `alacritty`. Both use the `alacritty_terminal` engine. Unknown runtime values, including the retired `ghostty`, fail safe to `alacritty`. Applies only to new sessions. |
| `terminal.ligatures` | boolean or null | `false` | Enables programming ligatures for fonts that ship them. |
| `terminal.integrated_glyphs` | boolean or null | `true` | Draws built-in block-element glyphs as filled quads. |
| `terminal.color_emoji` | boolean or null | `true` | Uses the platform color-emoji path. |
| `terminal.cursor_color` | string or null | theme cursor color | `#RRGGBB` or `#RGB`. |
| `terminal.scrollback_lines` | integer or null | `10000` | Range `100` to `100000`. Applies to newly created terminals. |
| `terminal.cursor_shape` | string or null | `block` | `vintage`, `block`, `beam`, `underline`, `double_underline`, or `hollow`. The loader also accepts aliases (`filled_box`, `bar`, `underscore`, `empty_box`, and camelCase spellings) that the schema `enum` rejects, so editors flag them even though they work. |
| `terminal.cursor_blink` | string or null | `terminal_controlled` | `on`, `off`, or `terminal_controlled`. |
| `terminal.env` | object or null | none | Environment variables injected into new terminals. Protected keys are filtered at PTY spawn. |
| `terminal.scroll_multiplier` | number or null | `1.0` | Mouse-wheel multiplier outside mouse-reporting and alternate-screen modes. Clamped to `0.1` to `10.0`; NaN and infinity revert to the default. |

```json
{
  "terminal": {
    "ligatures": false,
    "integrated_glyphs": true,
    "color_emoji": true,
    "cursor_shape": "block",
    "cursor_blink": "terminal_controlled",
    "scrollback_lines": 10000
  }
}
```
## Agent panel block

| Key | Type | Default | Notes |
|---|---|---|---|
| `agent_panel.max_content_width` | integer or null | `760` | Range `320` to `4000`. |
| `agent_panel.thinking_display` | string or null | `Auto` | `Auto`, `Preview`, `AlwaysExpanded`, or `AlwaysCollapsed`. |
| `agent_panel.profiles` | object | `{}` | Named agent launch profiles. |
| `agent_panel.default_profile` | string or null | none | Profile selected by default. |
| `agent_panel.notify_when_agent_waiting` | string or null | `Never` | `PrimaryScreen`, `AllScreens`, or `Never`. `AllScreens` currently behaves identically to `PrimaryScreen` at runtime. |

Profile entries under `agent_panel.profiles` can set `agent`, `model`,
`mode`, `effort`, and `tools`.

```json
{
  "agent_panel": {
    "thinking_display": "Auto",
    "notify_when_agent_waiting": "Never",
    "profiles": {
      "Write": {
        "agent": "codex",
        "model": "default",
        "mode": "default",
        "effort": "medium",
        "tools": ["read", "edit"]
      }
    }
  }
}
```

## Tool permissions

`tool_permissions` is keyed by tool kind. Each entry accepts
`always_allow` and `always_deny` arrays of string patterns.

```json
{
  "tool_permissions": {
    "read": {
      "always_allow": ["src/**"],
      "always_deny": ["secrets/**"]
    }
  }
}
```

## Commands and workspace templates

`commands` is an array of definitions. Every entry requires `name` and
may set `description`, `keywords`, exactly one of `command` or
`workspace`.

Workspace definitions can set `name`, `cwd`, `layout_preset`, `color`,
and `layout`. `layout_preset` accepts `even_h`, `even_v`,
`main_vertical`, or `tiled`. Layout nodes are either:

| Node | Required keys | Optional keys |
|---|---|---|
| pane | `type`, `surfaces` | none |
| split | `type`, `direction`, `children` | `ratio`, `ratios` |

Surface definitions accept `surface_type`, `name`, `custom_name`,
`command`, `prompt`, `cwd`, `path`, `env`, `focus`, `scrollback`,
`agent`, and per-surface `font_size`.

```json
{
  "commands": [
    {
      "name": "API + Codex",
      "description": "Open the API project with Codex and tests",
      "keywords": ["api", "codex"],
      "workspace": {
        "name": "API",
        "cwd": "~/projects/api",
        "layout_preset": "even_h",
        "layout": {
          "type": "split",
          "direction": "horizontal",
          "children": [
            {
              "type": "pane",
              "surfaces": [
                {
                  "surface_type": "terminal",
                  "name": "Codex",
                  "agent": "codex",
                  "prompt": "Review the API changes",
                  "cwd": "~/projects/api",
                  "focus": true
                }
              ]
            },
            {
              "type": "pane",
              "surfaces": [
                {
                  "surface_type": "terminal",
                  "name": "Tests",
                  "command": "cargo test --workspace"
                }
              ]
            }
          ]
        }
      }
    }
  ]
}
```

## Complete example

```json
{
  "$schema": "./schemas/paneflow.schema.json",
  "$schemaVersion": "1.0.0",
  "default_shell": null,
  "theme": "PaneFlow Dark",
  "theme_mode": "dark",
  "font_family": null,
  "font_fallbacks": [],
  "font_size": 13.0,
  "font_weight": "normal",
  "line_height": 1.2,
  "cell_width": 0.6,
  "unfocused_pane_opacity": 0.7,
  "reduce_motion": false,
  "workspace_auto_sort": false,
  "workspace_zed_menu_visible": null,
  "workspace_cursor_menu_visible": null,
  "workspace_vscode_menu_visible": null,
  "workspace_windsurf_menu_visible": null,
  "window_decorations": "client",
  "window_backdrop": "auto",
  "macos_chrome_material": true,
  "option_as_meta": false,
  "shell_integration": true,
  "agent_stall_detection": true,
  "agent_stall_threshold_secs": 60,
  "review_enabled": true,
  "new_pane_shows_sessions": false,
  "review_prefill_delay_ms": 2000,
  "submit_paste_delay_ms": 70,
  "external_editor": "auto",
  "shortcuts": {},
  "terminal": {
    "ligatures": false,
    "integrated_glyphs": true,
    "color_emoji": true,
    "cursor_color": null,
    "scrollback_lines": 10000,
    "cursor_shape": "block",
    "cursor_blink": "terminal_controlled",
    "env": {},
    "scroll_multiplier": 1.0
  },
  "agent_panel": {
    "max_content_width": 760,
    "thinking_display": "Auto",
    "profiles": {},
    "default_profile": null,
    "notify_when_agent_waiting": "Never"
  },
  "tool_permissions": {},
  "commands": [],
  "claude_code_bypass_permissions": false,
  "ai_unrestricted": false,
  "ai_injection_fence": true,
  "agent_button_visibility_defaults_migrated": null,
  "claude_code_button_visible": null,
  "codex_button_visible": null,
  "opencode_button_visible": null,
  "pi_button_visible": null,
  "hermes_agent_button_visible": null,
  "grok_button_visible": null,
  "amp_button_visible": null,
  "cursor_button_visible": null,
  "gemini_button_visible": null,
  "kiro_button_visible": null,
  "antigravity_button_visible": null,
  "copilot_button_visible": null,
  "codebuddy_button_visible": null,
  "factory_button_visible": null,
  "qoder_button_visible": null,
  "openclaw_button_visible": null
}
```
