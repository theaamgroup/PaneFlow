# Keybindings

Every PaneFlow command has a canonical action name. The action name is
the string you put in the `shortcuts` object in
[`paneflow.json`](configuration/schema.md#top-level-keys).

There is no keybinding table in this file. Do not add one by hand: it
would drift. The source of truth is:

| What | Where |
| --- | --- |
| Default bindings | `DEFAULTS` in `src-app/src/keybindings/defaults.rs` |
| macOS-only extras | `MACOS_ONLY_DEFAULTS` in the same file (`cmd-c`, `cmd-v`, `cmd-q`) |
| Every bindable action name | `ACTIONS` in `src-app/src/keybindings/registry.rs` |
| Wiring | `apply_keybindings()` in `src-app/src/keybindings/apply.rs` |
| Rendered chord strings | `format_keystroke()` in `src-app/src/keybindings/display.rs` |

The app also shows the live bindings in **Settings > Keyboard
Shortcuts**, which is the right place to look them up while using it.

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
