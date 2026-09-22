# Keybindings

Every PaneFlow command has a canonical action name. The action name is
the string you put in the `shortcuts` object in
[`paneflow.json`](configuration/schema.md#top-level-keys).

The tables below document defaults. Update them together with the registry;
the source of truth is:

| What | Where |
| --- | --- |
| Default bindings | `DEFAULTS` in `src-app/src/keybindings/defaults.rs` |
| macOS-only extras | `MACOS_ONLY_DEFAULTS` in the same file (`cmd-c`, `cmd-v`, `cmd-k`, `cmd-q`) |
| Every bindable action name | `ACTIONS` in `src-app/src/keybindings/registry.rs` |
| Wiring | `apply_keybindings()` in `src-app/src/keybindings/apply.rs` |
| Rendered chord strings | `format_keystroke()` in `src-app/src/keybindings/display.rs` |

The command palette (`Cmd+Shift+O`) searches actions and shows their live
shortcuts. Other navigation entrypoints are pane overview (`Cmd+Shift+P`)
and work review (`Cmd+Shift+U`). User
overrides can change these defaults. A clicked file path opens in the
configured external editor; there is no Files sidebar.

The app also shows the live bindings in **Settings > Keyboard
Shortcuts**, which is the right place to look them up while using it.
That page is grouped by area, and its search box matches the action's
name and its chord: typing `cmd+shift+j` (or `cmd-shift-j`) finds the
row bound to ⌘⇧J. **Find by key** goes the other way - press a chord and
the page shows whichever action owns it, without dispatching it. Click a
row and press a chord to rebind it; **Reset to defaults** asks for a
confirming click before it removes every override from `paneflow.json`.

## The `secondary` modifier

Binding definitions use a `secondary` modifier rather than naming a
concrete key. This is GPUI's own shorthand, not a PaneFlow invention:
GPUI maps `secondary` to the platform modifier, which on macOS is `Cmd`.
So `secondary-shift-d` is `Cmd+Shift+D` here.

## How do I override a binding?

Set the `shortcuts` object in `paneflow.json`. Keys are keystrokes;
values are action names from `registry.rs`.

```json
{
  "shortcuts": {
    "ctrl+shift+t": "new_tab",
    "alt+1": "select_workspace_1"
  }
}
```

Bind an action to `"none"` to unbind it.

Conflicts resolve last-write-wins. User entries layer on top of the
built-in defaults, so the most recently registered binding for a given
keystroke wins. If two user entries map the same keystroke to different
actions, the later entry takes effect.

Unknown action names are skipped with a warning rather than failing the
load. `+` and `-` both parse as separators.

## Default binding reference

All registered in `keybindings::apply_keybindings()` via `cx.bind_keys()`. 81 actions total (`app/actions.rs`; `claude_md_action_count_matches_the_actions_macro` fails if this number or the one in CLAUDE.md drifts from the `actions!` block); tables in `keybindings/defaults.rs`.

**`secondary` resolves to Cmd on macOS** (`defaults.rs`), so every `secondary-*` default below is a Cmd binding here. `MACOS_ONLY_DEFAULTS` (`defaults.rs`) adds `Cmd+C`, `Cmd+V`, `Cmd+K` (Terminal: copy, paste, clear scrollback) and `Cmd+Q` (quit) on top.

**The macOS menu bar** (`app/bootstrap.rs::install_macos_menu_bar`, `#[cfg(target_os = "macos")]`) is PaneFlow (`About PaneFlow`, `Settings…`, separator, `Report an Issue`, separator, `Quit PaneFlow`) / Edit / Window (`Minimize`, `Zoom`, separator, `Show All Panes`, separator, `Next Workspace`, `Close Workspace`, `New Workspace`) / Help (`PaneFlow Help`, separator, `System Info…`). `Settings…` dispatches `OpenSettings` into `open_settings_window`. `Report an Issue` dispatches `ReportIssue` and opens `https://github.com/theaamgroup/paneflow/issues/new` in the default browser. `System Info…` dispatches `ShowSystemInfo` into `open_system_info_dialog` (`app/system_info_dialog.rs`): a copyable environment block - version, install format, OS, CPU, GPU, renderer, libghostty version - with no project path and no environment dump, collected off the render thread by `system_info.rs` (`sysctl`, `MTLCopyAllDevices`, and `sparkle::bundled_framework_binary` for the install format). Like `About` / `OpenHelp` / `OpenSettings` / `ReportIssue` it has no default chord and is absent from `keybindings/registry.rs::ACTIONS`. Theme selection lives in Settings → Appearance; there is no View menu and no modal theme picker. Every menu action needs BOTH a render-root `.on_action` in `main.rs` and an app-global fallback in `install_macos_menu_action_fallbacks`, or AppKit's `is_action_available` check paints the item permanently greyed while focus sits in a terminal. `OpenSettings` is deliberately absent from `keybindings/registry.rs::ACTIONS` (the `About` / `OpenHelp` precedent) so Settings → Keyboard Shortcuts does not grow permanently `Unassigned` rows, and **`Cmd+,` is deliberately unbound** (issue #105) - `no_default_binds_the_macos_preferences_chord` in `keybindings/apply.rs` fails if any default claims it. The sidebar's "Workspaces" header carries no `+` (issue #105); it does carry the Pane Overview button (issue #339, id `sidebar-pane-overview`), which the #105 guard test permits because it forbids only the `sidebar-new-workspace` id. New Workspace is `Cmd+Shift+N`, Window ▸ New Workspace, and the sidebar's empty-state "Open folder" button (`empty-new-ws` in `app/sidebar/mod.rs`; the sidebar can be collapsed with `Cmd+Alt+B`, so the `empty-app` copy in `main.rs` says the button is in the sidebar). The sidebar footer carries **no Settings affordance at all** - the gear that survived issue #105 is gone, so `Settings…` on the menu bar is the only entry point.

| Key | Action | Context |
|-----|--------|---------|
| `Cmd+Shift+D` / `Cmd+Shift+E` | Split horizontal / vertical | Global |
| `Cmd+Shift+W` / `Cmd+Shift+T` | Close pane / undo close pane | Global |
| `Cmd+Alt+T` / `Cmd+W` | New tab / close tab | Global |
| `Cmd+]` / `Cmd+[` | Next tab / previous tab | Global |
| `Alt+Arrow` | Focus navigation | Global |
| `Cmd+Shift+N` / `Cmd+Shift+Q` | New / close workspace | Global |
| `Ctrl+Tab` | Next workspace | Global |
| `Cmd+1`-`Cmd+9` | Select workspace | Global |
| `Cmd+Alt+1`-`4` | Layout preset: even-h, even-v, main-vertical, tiled | Global |
| `Cmd+Shift+=` / `Cmd+Shift+S` | Equalize splits / swap pane | Global |
| `Cmd+Shift+Z` | Toggle zoom | Global |
| `Cmd+Shift+J` | Jump to next waiting agent, including background tabs | Global |
| `Cmd+Shift+P` | Pane overview (every terminal pane, all workspaces and tabs) | Global |
| `Cmd+Shift+G` | Diff view | Global |
| `Cmd+J` | New terminal tab (diff dock; `secondary-j`) | Global, not Terminal/TextInput |
| `Cmd+Shift+Space` | Composer | Global |
| `Cmd+Shift+B` / `Cmd+Shift+M` | Toggle broadcast member / broadcast groups | Global |
| `Cmd+Shift+O` | Command palette (every context-free action with its live binding; `app/command_palette.rs`, #523; upstream's `Cmd+Shift+P` is Pane Overview here) | Global |
| `Cmd+Shift+F` | Maximize / restore the Changes dock (`toggle_diff_dock_maximize`; no-op while the dock is not visible) | Global |
| `Cmd+Alt+B` | Toggle primary sidebar (persisted across launches) | Global |
| `Ctrl+Alt+R` / `Ctrl+Shift+Alt+C` | Reveal in Finder / copy workspace path | Global |
| `Ctrl+Alt+Z` | Open workspace in the configured editor (`external_editor`) | Global |
| `Cmd+C` / `Cmd+V` | Copy / paste (macOS layer) | Terminal |
| `Ctrl+Shift+C` / `Ctrl+Shift+V` | Copy / paste (cross-platform layer, still bound) | Terminal |
| `Shift+PageUp` / `Shift+PageDown` | Scroll page | Terminal |
| `Cmd+Shift+Up` / `Cmd+Shift+Down` | Jump to prev / next shell prompt mark | Terminal |
| `Cmd+K` / `Cmd+Shift+K` | Clear scrollback (`clear_scroll_history`; `cmd-k` is the macOS layer, `secondary-shift-k` the alias) | Terminal |
| `Cmd+Shift+R` | Reset terminal (`reset_terminal`, RIS) | Terminal |
| `Ctrl+Shift+X` / `Ctrl+Shift+F` | Copy mode / find-in-buffer | Terminal |
| `Cmd+=` / `Cmd+-` / `Cmd+0` | Font size up / down / reset | Terminal |
| `Ctrl+Shift+C` | Copy diff hunk | DiffView |
| `Enter` / `Shift+Enter` / `Esc` | Next / prev / dismiss | Search |
| `Alt+R` | Toggle regex | Search |
| `]` / `[` / `u` / `s` / `Esc` | Next hunk / prev hunk / toggle view / toggle sync / dismiss | DiffView |
| `Cmd+Q` | Quit (macOS only) | Global |

`Cmd+Shift+K` and `Cmd+K` clear terminal scrollback; `Cmd+Shift+R` resets
the terminal. Their registry contexts and exclusive chord ownership are
covered by `cmd_shift_k_and_cmd_k_clear_scrollback` in `keybindings/apply.rs`.
`Cmd+Shift+A` is unassigned after removal of the waiting-agent list overlay.

Next-workspace is `ctrl-tab`, not the upstream `secondary-tab` (Cmd+Tab): macOS reserves Cmd+Tab for the application switcher and never delivers it to the app (issue #10; a synthetic Cmd+Tab on 2026-08-27 moved focus to another app while Cmd+1/Cmd+2 through the same path switched workspaces). A test in `keybindings/apply.rs` fails if any default binds `secondary-tab` again.

Work Review opens with `Cmd+Shift+U` (`open_work_review`) or **Window → Work Review**. The command palette (`Cmd+Shift+O`) and pane overview (`Cmd+Shift+P`) use the current defaults in `keybindings/defaults.rs`.
