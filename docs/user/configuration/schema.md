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

`PANEFLOW_HOME` relocates every per-user directory PaneFlow owns. Set to an
absolute path, the config root becomes `<home>/config`, the data root
`<home>/data` (the stable `bin/` helper copies), and the cache root
`<home>/cache` (the versioned helper cache), each still namespaced by the
build profile, so a release build reads
`<home>/config/paneflow/paneflow.json` with `session.json`,
`window-state.json`, and `recents.json` beside it. Unset and empty values
are ignored; a relative value is ignored with a warning. The variable exists
for isolated runs
(the startup benchmark, a scratch instance); it does not move the IPC
socket, which `PANEFLOW_SOCKET_PATH` controls (see
[scripting](../scripting.md)).

Unknown top-level keys are ignored by the runtime. The schema uses
`additionalProperties: false` so editors can flag typos before launch.
That strictness is an editor-side aid only; it never affects loading.

> **Upgrading a tuned `line_height` or `cell_width`:** since 0.4.x the
> terminal grid is measured on the font (the cell is the face's widest
> advance by its own line height, rounded to whole device pixels) and the two
> keys are multipliers of that measured cell, defaulting to `1.0`. They used
> to be multipliers of the point size (`1.2` and `0.6`). A value carried over
> from an older config now means something else: a kept `1.2` is a grid 20%
> taller than the face's design, and a kept `0.6` is below the new floor and
> reverts to the default with a warning. Delete both keys to get the font's
> own spacing, or re-tune them against the new meaning.

## Top-level keys

| Key | Type | Default | Notes |
|---|---|---|---|
| `$schema` | string | none | Editor-only pointer to the public schema. Ignored at runtime. |
| `$schemaVersion` | string | `1.0.0` | Logs a warning when unknown, but never blocks loading. |
| `default_shell` | string or null | platform default | Chain: configured -> `$SHELL` -> `/bin/sh`. Each candidate must be an existing file with an exec bit, otherwise it is skipped with a warning. A bare name (no `/`) is resolved via `which`, then probed in `/opt/homebrew/bin`, `/usr/local/bin`, `/usr/bin`, `/bin`, which is what makes `"fish"` work under a GUI launch with a minimal `PATH`. |
| `theme` | string or null | `PaneFlow Dark` | Bundled theme name, one preset's light or dark variant. Current values: `PaneFlow Dark`, `PaneFlow Light`, `Vercel Dark`, `Vercel Light`, `Claude Dark`, `Claude Light`, `Cursor Dark`, `Cursor Light`. Pre-preset names (`One Dark`, `Vercel`, `Claude`, `Cursor`) still resolve. |
| `theme_mode` | string or null | `dark` | `light`, `dark`, or `system`. |
| `font_family` | string or null | bundled JetBrainsMono Nerd Font | Accepts `.PaneflowMono`, `JetBrainsMono NF`, `JetBrainsMono NFM`, `.PaneflowSans`, embedded family names, or installed monospace families. |
| `font_fallbacks` | array of strings or null | none | Ordered glyph fallback families for symbols, Powerline, CJK, emoji, or Nerd Font glyphs. |
| `font_size` | number or null | `13.0` | Points, range `8.0` to `32.0`. Out-of-range values fall back to default with a warning. |
| `font_weight` | string or null | `normal` | `thin`, `extra_light`, `light`, `semi_light`, `normal`, `medium`, `semi_bold`, `bold`, `extra_bold`, `black`, `extra_black`. |
| `line_height` | number or null | `1.0` | Multiplier of the font's own line height (ascent, descent, and line gap), range `0.8` to `2.5`. The cell is rounded to whole device pixels. Out-of-range values revert to the default with a warning; they are not clamped. |
| `cell_width` | number or null | `1.0` | Multiplier of the font's advance, range `0.8` to `2.0`. The cell is rounded to whole device pixels. Out-of-range values revert to the default with a warning; they are not clamped. |
| `unfocused_pane_opacity` | number or null | `0.7` | Opacity of panes without focus when a workspace has more than one pane, range `0.15` to `1.0`. `1.0` disables the dim. Values outside the range are clamped with a warning; non-finite values fall back to the default. |
| `reduce_motion` | boolean or null | `false` | Minimize non-essential interface motion: hover transitions settle instantly and decorative animations render a static frame. |
| `sidebar_show` | object or null | `branch` on, the rest off | What a rail row shows beyond its name, one switch per line: `branch` (boolean, default `true`) paints each terminal's current git branch beneath its name in the sidebar; split tabs show a labeled branch line per terminal; `diffstat` (boolean, default `false`) shows right-aligned insertion and deletion counts on the workspace row or a bound tab's own metadata line, drawn only when the checkout has something to report; `pr` is a retired key: PaneFlow ignores it, and the schema keeps a stub so editors do not flag an older file; it does not change the branch icon; `indent_guide` (boolean, default `false`) draws a hairline under a workspace's folder icon down its tab rows. Branches follow each terminal's current directory and refresh every two seconds. Counts read the tab's bound worktree, or its workspace's checkout when the tab is unbound. An absent object is the rail as it shipped before the switches existed. Toggled from the rail header's Customize Sidebar menu, or hand-edited; hot-reloads. |
| `new_tab_branch` | string or null | `main` | Default branch for new tabs. Empty uses the workspace checkout. Settings → Workspaces → New tabs lists branches from open workspaces. Existing tabs, splits, and restored sessions keep their directories. |
| `workspace_new_tab_branches` | object | `{}` | Branch overrides keyed by workspace cwd, e.g. `{"/projects/Aftermarket-Websites": "staging"}`. An absent entry inherits the default; an empty value uses that workspace's checkout. Set these using each workspace's branch picker in New tabs settings. |
| `new_tabs_on_main` | boolean or null | `true` | Legacy compatibility: when `new_tab_branch` is absent, false uses the workspace checkout and true/absent uses main. |
| `workspace_auto_sort` | boolean or null | `false` | Order the workspace sidebar automatically: pinned first, then workspaces with something running, then idle ones, alphabetically within each group. Sibling git worktrees stay contiguous. Drag-to-reorder is disabled while this is on. |
| `window_backdrop` | string or null | `auto` | Accepted: `auto`, `blurred`, `transparent`, `opaque`, `off`. Read once at startup. See the resolution table below: the values do not map one-to-one on macOS. |
| `macos_chrome_material` | boolean or null | `true` | Reveals AppKit's native Sidebar material across the whole window shell: the primary rail, panel inset, and pane gutters. Silently disabled when `window_backdrop` is `opaque`, `off`, or `transparent`. |
| `option_as_meta` | boolean or null | `false` | Option produces Unicode input by default. Set to `true` to send Option/Alt as an ESC prefix. |
| `shell_integration` | boolean or null | enabled | Master switch for shell rc injection: OSC 7 CWD reporting and OSC 133 command marks. |
| `agent_stall_detection` | boolean or null | `true` | Enables stalled-agent detection. |
| `agent_stall_threshold_secs` | integer or null | `60` | Silence threshold before a Thinking agent is marked Stalled. Clamped to `30` to `86400`. |
| `crash_reporting` | boolean or null | `true` | Master switch for Sentry crash reporting. `false` never initializes it. Reports are sent without default PII (`send_default_pii` is off), only a GUI launch initializes reporting (CLI subcommands never do), and the switch is read once at startup, so it requires a restart. |
| `review_enabled` | boolean or null | `true` | Master switch for the Review surface. `false` hides the Review view and its sidebar tab, makes the Review shortcut a no-op, and reopens a Review-mode session in the terminal view. |
| `new_pane_shows_sessions` | boolean or null | `false` | When true, a Tab-placement New pane picker also opens the Agent sessions sidebar, scoped to the workspace cwd, so a listed session can be resumed into the new pane. Split-placement pickers leave the sidebar alone. |
| `mcp_bridge_prompt_dismissed` | array of strings | `[]` | MCP-install agent ids (`claude-code`, `codex`, `gemini`, `opencode`) whose sidebar "Install MCP bridge" callout was dismissed. When a pane runs one of those agents and its MCP config has no `paneflow` entry, the sidebar footer offers the bridge once; the callout's `×` writes the agent id here so it never asks again for that agent. Remove an id to see the offer again. A malformed value loads as the empty list. |
| `review_prefill_delay_ms` | integer or null | `2000` | Delay before Review pre-fills a freshly launched CLI. Clamped to `250` to `10000`. |
| `submit_paste_delay_ms` | integer or null | `70` | Minimum delay between bracketed paste and submit carriage return. Clamped to `10` to `5000`. |
| `external_editor` | string or null | `auto` | Editor command (quoted paths and flags supported, without a shell), tried before `$VISUAL` and `$EDITOR`. `auto`/null starts with those variables, then probes the GUI CLIs `code`, `cursor`, `zed`, `subl`, `code-insiders`, and `windsurf`, then the macOS handler. Terminal editors (`hx`, `nvim`, `vim`, `emacs`) are not launched from that detached fallback, because a GUI launch has no TTY; set `external_editor`, `$VISUAL`, or `$EDITOR` to use one. Failed file-link commands fall through. `system` uses only the macOS handler, without line/column positioning. The workspace **Open in editor** action (`Ctrl+Alt+Z`) launches this same command in the workspace directory. |
| `shortcuts` | object | `{}` | Custom keybindings: `{ "ctrl+shift+t": "new_tab" }`. |
| `terminal` | object or null | defaults below | Terminal renderer and PTY settings. |
| `commands` | array | `[]` | Retired: legacy command palette entries and workspace templates. Accepted and ignored; see [Commands](#commands). |
| `claude_code_bypass_permissions` | boolean or null | `false` | Adds Claude Code `--permission-mode bypassPermissions` when launching from PaneFlow. |
| `ai_unrestricted` | boolean or null | `false` | Allows trusted automation to submit via IPC without `PANEFLOW_IPC_SCRIPTING=1`. |
| `ai_injection_fence` | boolean or null | `true` | Wraps pane reads in an untrusted-output fence. Keep enabled for AI clients. |
| `agent_button_visibility_defaults_migrated` | boolean or null | `null` | Internal one-time marker recording that a pre-allowlist config preserved its installed launcher buttons as explicit values. Runtime visibility does not otherwise consult it. |
| `agent_panel` | object or null | defaults below | Agents-view display, profiles, and notification settings. |

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

## Workspace context menu

**Open in editor** launches `external_editor` in that workspace's directory.
`Ctrl+Alt+Z` runs the same action. There is one row, not a row per editor.

## Agent buttons

Each button visibility key is `boolean or null`. `true` always shows the
button and `false` always hides it. For a fresh config, `null` or an omitted
key shows Claude Code, Codex, or Grok only when that CLI is installed; the
other 15 agents default off even when installed.

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
| `deepseek_harness_button_visible` | DeepSeek Harness | Off |
| `muse_button_visible` | Muse Code | Off |

## Terminal block

| Key | Type | Default | Notes |
|---|---|---|---|
| `terminal.ligatures` | boolean or null | `false` | Enables programming ligatures for fonts that ship them. |
| `terminal.integrated_glyphs` | boolean or null | `true` | Draws built-in block-element glyphs as filled quads. |
| `terminal.color_emoji` | boolean or null | `true` | Uses the platform color-emoji path. |
| `terminal.cursor_color` | string or null | theme cursor color | `#RRGGBB` or `#RGB`. |
| `terminal.scrollback_lines` | integer or null | `10000` | Range `100` to `100000`. Applies to newly created terminals. |
| `terminal.cursor_shape` | string or null | `block` | `vintage`, `block`, `beam`, `underline`, `double_underline`, or `hollow`. The loader also accepts aliases (`filled_box`, `bar`, `underscore`, `empty_box`, and camelCase spellings) that the schema `enum` rejects, so editors flag them even though they work. |
| `terminal.cursor_blink` | string or null | `terminal_controlled` | `on`, `off`, or `terminal_controlled`. |
| `terminal.env` | object or null | none | Environment variables injected into new terminals. Protected keys are filtered at PTY spawn. |
| `terminal.scroll_multiplier` | number or null | `1.0` | Mouse-wheel multiplier outside mouse-reporting and alternate-screen modes. Clamped to `0.1` to `10.0`; NaN and infinity revert to the default. |
| `terminal.minimum_contrast` | number or null | `45.0` | Minimum APCA lightness contrast (Lc) enforced between text and its cell background, on the theme's ANSI colors only. `0` disables the floor and leaves theme colors untouched (Ghostty's default); this fork defaults to Zed's `45`, the floor it always enforced, where upstream PaneFlow defaults to `0`. Range `0` to `90`; NaN and infinity disable it. Hot-reloaded. |
| `terminal.osc52_clipboard` | string or null | `copy_only` | `copy_only` lets a focused pane write the system clipboard through OSC 52; `disabled` ignores every OSC 52 write. Clipboard reads are never served. Applies to newly created terminals. |

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
| `agent_panel.notify_when_agent_waiting` | string or null | `Never` | `PrimaryScreen`, `AllScreens`, or `Never`. `AllScreens` currently behaves identically to `PrimaryScreen` at runtime. A notification is dropped only when the agent's pane is under your eye (the PaneFlow window is active and the pane's workspace and tab are on screen); an agent in another workspace or a background tab notifies even while you work elsewhere in PaneFlow. |

Legacy `agent_panel.max_content_width`, `thinking_display`, `profiles`,
`default_profile`, and top-level `tool_permissions` remain accepted and ignored.
They configure the removed Agents view and have no effect on terminal agents.
`notify_when_agent_waiting` remains active.

## Commands

`commands` is accepted and ignored (issues #607 and #817). PaneFlow does
not parse the value, launch workspace templates from it, or edit it, so an
older file carrying the array (or any other value under the key) still
loads and the rest of the file applies. The published schema keeps a
deprecated `commands` stub with unchecked entries so an editor does not
flag the key. Repeatable layouts come from session restore.

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
  "line_height": 1.0,
  "cell_width": 1.0,
  "unfocused_pane_opacity": 0.7,
  "reduce_motion": false,
  "sidebar_show": {
    "branch": true,
    "diffstat": false,
    "indent_guide": false
  },
  "workspace_auto_sort": false,
  "new_tab_branch": "main",
  "workspace_new_tab_branches": {},
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
    "scroll_multiplier": 1.0,
    "minimum_contrast": 45.0
  },
  "agent_panel": {
    "notify_when_agent_waiting": "Never"
  },
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
  "openclaw_button_visible": null,
  "deepseek_harness_button_visible": null,
  "muse_button_visible": null
}
```

`agent_context` on a surface stores the session-owned pane UUID and current agent task.
Assign tasks through `paneflow task assign`;
see [Agent context](../../agent-context.md) for the API and persistence contract.

Legacy `window_decorations` values are accepted and ignored by the loader.
The editor schema omits this retired setting; macOS always supplies native
window decorations.
