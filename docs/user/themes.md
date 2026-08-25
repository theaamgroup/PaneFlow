# Themes

> Choose PaneFlow's bundled themes from Settings or paneflow.json. One Dark, PaneFlow Light, Vercel, Claude, and Cursor ship today and hot-reload without restart.

PaneFlow reads a top-level `"theme"` value from `paneflow.json`.
The current bundled themes are `One Dark`, `PaneFlow Light`, `Vercel`, `Claude`, and `Cursor`.
Omit the key, set it to `null`, or reset it from Settings to use
the default `One Dark` theme.

> **TL;DR.** Use **Settings -> Themes** for the UI, or set
> `"theme": "One Dark"` / `"theme": "PaneFlow Light"` / `"theme": "Vercel"` / `"theme": "Claude"` / `"theme": "Cursor"` in
> `paneflow.json`. Theme changes hot-reload after save, with no app
> restart.

## How do I switch the active theme?

Open **Settings -> Themes** and choose one of the three segments:

| Segment | What PaneFlow saves |
| --- | --- |
| Light | `"PaneFlow Light"` |
| Dark | `"One Dark"` |
| System | The matching concrete theme at click time: `"PaneFlow Light"` for a light OS appearance, otherwise `"One Dark"`. |

`System` is not stored as a persistent follow-the-OS mode today.
After you choose it, PaneFlow writes one of the concrete bundled
theme names above.

You can also edit `paneflow.json` directly:

```json
{
  "theme": "PaneFlow Light"
}
```

The config file lives at:

| Build | Path |
| --- | --- |
| Release | `~/Library/Application Support/paneflow/paneflow.json` |
| Debug (`cargo run`) | `~/Library/Application Support/paneflow-dev/paneflow.json` |

The schema lists the canonical spellings. Runtime lookup is
case-insensitive, but using the canonical names keeps editor
autocomplete and review diffs clean. Unknown names fall back to
`One Dark` and emit a log warning the next time the file is parsed.
The full set of recognised keys lives in the
[configuration schema](configuration/schema.md).

## Which themes ship with PaneFlow today?

Stable in current builds.

| Name | Description |
| --- | --- |
| `"One Dark"` | Dark theme, default. Inspired by Atom's One Dark palette, then adjusted for PaneFlow's terminal and app chrome. |
| `"PaneFlow Light"` | Light theme with a white work surface, light app shell, and a dedicated light syntax palette. |
| `"Vercel"` | Monochrome dark theme inspired by Vercel's black, white, and precise accent style. Includes terminal, app chrome, settings, diff, and syntax palettes. |
| `"Claude"` | Claude Desktop-style dark theme: graphite surfaces, ivory text, muted controls, and Claude's orange accent. Keeps PaneFlow's canonical diff/status red, green, and yellow. |
| `"Cursor"` | Cursor IDE-style dark theme: near-black workspace, compact graphite sidebar, dark composer surface, and pale blue accent. Keeps PaneFlow's canonical diff/status red, green, and yellow. |

A PaneFlow theme defines 36 terminal colour slots: a 24-colour ANSI
palette (8 hues x 3 intensities: normal, bright, dim), 5 base
background/foreground colours, cursor, selection plus a derived
selection foreground, scrollbar thumb, link text, and 2 title-bar
colours. PaneFlow also keeps a separate syntax palette for the Diff
surface.

The selection foreground is not hand-tuned. PaneFlow recomputes it
at theme load until it clears APCA Lc >= 45 against the selection
background, so selected text stays legible.

## What does a theme affect?

Themes affect more than terminal ANSI colours:

* terminal background, foreground, cursor, selection, scrollbar, links, and ANSI palette;
* the app chrome palette derived from the active theme;
* markdown panes and tables;
* Diff and Review syntax colours;
* title-bar colours, and alignment with the native macOS Sidebar material when `macos_chrome_material` is on.

Two related settings are separate from the theme name:

* `terminal.cursor_color` overrides only the terminal cursor. `null`
  uses the cursor colour from the active theme.
* `commands[].workspace.color` is a workspace-template colour, not a
  theme accent.

## How does theme hot-reload work?

When you save `paneflow.json`, PaneFlow re-resolves the active theme
on the fly. No restart and no window reload are needed.

Two mechanisms drive the reload:

* **Event-driven path.** A `notify` watcher watches the config
  directory and debounces file events by 300 ms.
* **Polling fallback.** The theme cache also carries a 500 ms `mtime`
  poll (`THEME_CHECK_INTERVAL` in `src-app/src/theme/watcher.rs`) for when
  the OS watcher cannot start. Note that this fallback belongs to the
  theme cache specifically. The general config watcher has no poll
  fallback; it has a 1 s max-wait ceiling instead, so an event flood
  cannot starve the reload.

You will see the new palette take effect on the next render frame
after the watcher or fallback fires. If the reload silently fails on
your machine, the [troubleshooting page](troubleshooting.md#why-is-my-theme-not-hot-reloading)
walks through the common causes.

> Theme and typography keys hot-reload from `paneflow.json`. The
> `window_decorations` and `window_backdrop` keys are read once at
> startup, so changing either still requires a restart.

## How do I create a custom theme?

Custom user-supplied theme files are not supported yet. PaneFlow does
not load `theme.json`, and there is no custom palette format in
`paneflow.json` today.

Bundled themes are defined in the `THEMES` table in
`src-app/src/theme/builtin.rs`. Adding a palette to this fork means
adding a constructor there and an entry in that table, plus the theme
name in the `theme` enum in `schemas/paneflow.schema.json`.
