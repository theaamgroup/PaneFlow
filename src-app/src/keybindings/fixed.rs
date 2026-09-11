//! Fixed bindings handled by focused widgets or local key handlers.
//! Keep their context in the label: the same chord can do different things
//! in a terminal, an editor, or a picker. These rows never enter the rebind writer.

use super::display::ascii_key_forms;
use super::{ShortcutEntry, ShortcutGroup, effective_shortcuts, format_keystroke};
use std::collections::HashMap;

pub(super) const FIXED: &[(&str, &str)] = &[
    ("backspace", "Text field · backspace"),
    ("delete", "Text field · delete"),
    ("left", "Text field · left"),
    ("right", "Text field · right"),
    ("shift-left", "Text field · select left"),
    ("shift-right", "Text field · select right"),
    ("home", "Text field · line start"),
    ("end", "Text field · line end"),
    ("shift-home", "Text field · select home"),
    ("shift-end", "Text field · select end"),
    ("alt-left", "Text field · word left"),
    ("alt-right", "Text field · word right"),
    ("alt-shift-left", "Text field · select word left"),
    ("alt-shift-right", "Text field · select word right"),
    ("alt-backspace", "Text field · delete previous word"),
    ("cmd-left", "Text field · line start"),
    ("cmd-right", "Text field · line end"),
    ("cmd-shift-left", "Text field · select home"),
    ("cmd-shift-right", "Text field · select end"),
    ("cmd-a", "Text field · select all"),
    ("cmd-c", "Text field · copy"),
    ("cmd-v", "Text field · paste"),
    ("cmd-x", "Text field · cut"),
    ("ctrl-cmd-space", "Text field · emoji and symbols"),
    ("backspace", "Text area / Composer · backspace"),
    ("delete", "Text area / Composer · delete"),
    ("left", "Text area / Composer · left"),
    ("right", "Text area / Composer · right"),
    ("up", "Text area / Composer · up"),
    ("down", "Text area / Composer · down"),
    ("shift-left", "Text area / Composer · select left"),
    ("shift-right", "Text area / Composer · select right"),
    ("shift-up", "Text area / Composer · select up"),
    ("shift-down", "Text area / Composer · select down"),
    ("home", "Text area / Composer · line start"),
    ("end", "Text area / Composer · line end"),
    ("shift-home", "Text area / Composer · select home"),
    ("shift-end", "Text area / Composer · select end"),
    ("enter", "Text area / Composer · submit / queue prompt"),
    ("shift-enter", "Text area / Composer · insert newline"),
    ("escape", "Text area / Composer · dismiss"),
    (
        "secondary-enter",
        "Text area / Composer · send prompt immediately",
    ),
    ("alt-left", "Text area / Composer · word left"),
    ("alt-right", "Text area / Composer · word right"),
    ("alt-shift-left", "Text area / Composer · select word left"),
    (
        "alt-shift-right",
        "Text area / Composer · select word right",
    ),
    (
        "alt-backspace",
        "Text area / Composer · delete previous word",
    ),
    ("cmd-left", "Text area / Composer · line start"),
    ("cmd-right", "Text area / Composer · line end"),
    ("cmd-shift-left", "Text area / Composer · select home"),
    ("cmd-shift-right", "Text area / Composer · select end"),
    ("cmd-a", "Text area / Composer · select all"),
    ("cmd-c", "Text area / Composer · copy"),
    ("cmd-v", "Text area / Composer · paste"),
    ("cmd-x", "Text area / Composer · cut"),
    (
        "cmd-shift-enter",
        "Text area / Composer · send prompt immediately",
    ),
    ("left", "Code editor · left"),
    ("right", "Code editor · right"),
    ("up", "Code editor · up"),
    ("down", "Code editor · down"),
    ("shift-left", "Code editor · select left"),
    ("shift-right", "Code editor · select right"),
    ("shift-up", "Code editor · select up"),
    ("shift-down", "Code editor · select down"),
    ("home", "Code editor · line start"),
    ("end", "Code editor · line end"),
    ("shift-home", "Code editor · select home"),
    ("shift-end", "Code editor · select end"),
    ("pageup", "Code editor · page up"),
    ("pagedown", "Code editor · page down"),
    ("shift-pageup", "Code editor · select page up"),
    ("shift-pagedown", "Code editor · select page down"),
    ("secondary-a", "Code editor · select all"),
    ("backspace", "Code editor · backspace"),
    ("delete", "Code editor · delete"),
    ("enter", "Code editor · insert newline"),
    ("tab", "Code editor · increase indent"),
    ("shift-tab", "Code editor · decrease indent"),
    ("secondary-z", "Code editor · undo"),
    ("secondary-shift-z", "Code editor · redo"),
    ("secondary-c", "Code editor · copy"),
    ("secondary-x", "Code editor · cut"),
    ("secondary-v", "Code editor · paste"),
    ("secondary-s", "Code editor · save"),
    ("escape", "Code editor · dismiss"),
    ("alt-left", "Code editor · word left"),
    ("alt-right", "Code editor · word right"),
    ("alt-shift-left", "Code editor · select word left"),
    ("alt-shift-right", "Code editor · select word right"),
    ("cmd-up", "Code editor · document start"),
    ("cmd-down", "Code editor · document end"),
    ("cmd-shift-up", "Code editor · select document start"),
    ("cmd-shift-down", "Code editor · select document end"),
    ("f2", "Sidebar · rename focused workspace or tab"),
    ("enter", "Sidebar rename · confirm"),
    ("escape", "Sidebar rename · cancel"),
    ("left", "Pane overview · select pane to the left"),
    ("right", "Pane overview · select pane to the right"),
    ("up", "Pane overview · select pane above"),
    ("down", "Pane overview · select pane below"),
    ("enter", "Pane overview · open selected pane"),
    ("escape", "Pane overview · close"),
    ("backspace", "Pane overview · delete filter character"),
    (
        "up",
        "Attention queue, Fleet Search, theme picker, broadcast groups · previous result",
    ),
    (
        "down",
        "Attention queue, Fleet Search, theme picker, broadcast groups · next result",
    ),
    (
        "enter",
        "Attention queue, Fleet Search, theme picker, broadcast groups · activate selection",
    ),
    (
        "escape",
        "Attention queue, Fleet Search, theme picker, broadcast groups · close",
    ),
    (
        "backspace",
        "Theme picker, broadcast groups · delete filter character",
    ),
    ("up", "Files and Sessions sidebars · previous row"),
    ("down", "Files and Sessions sidebars · next row"),
    ("home", "Files and Sessions sidebars · first row"),
    ("end", "Files and Sessions sidebars · last row"),
    (
        "enter",
        "Files and Sessions sidebars · open / resume selection",
    ),
    (
        "space",
        "Files and Sessions sidebars · open / resume selection",
    ),
    (
        "escape",
        "Files and Sessions sidebars · dismiss menu, clear filter, then close",
    ),
    ("up", "Work review · previous checkout"),
    ("down", "Work review · next checkout"),
    ("enter", "Work review · visit checkout"),
    ("r", "Work review · review selected checkout"),
    ("escape", "Work review · close"),
    ("up", "New pane picker · previous preset"),
    ("down", "New pane picker · next preset"),
    ("enter", "New pane picker · launch selected preset"),
    ("escape", "New pane picker · dismiss branch menu or close"),
    ("enter", "Launch Pad · load issue or launch agent"),
    ("tab", "Launch Pad · next text field"),
    ("escape", "Launch Pad · cancel"),
    ("escape", "Dialogs and menus · dismiss"),
    ("enter", "About and close confirmation · confirm"),
    ("up", "Editor Controls · previous option"),
    ("down", "Editor Controls · next option"),
    ("enter", "Editor Controls · choose option"),
    ("left", "Copy mode · move left"),
    ("right", "Copy mode · move right"),
    ("up", "Copy mode · move up"),
    ("down", "Copy mode · move down"),
    ("shift-left", "Copy mode · extend selection left"),
    ("shift-right", "Copy mode · extend selection right"),
    ("shift-up", "Copy mode · extend selection up"),
    ("shift-down", "Copy mode · extend selection down"),
    ("enter", "Copy mode · copy selection and exit"),
    ("escape", "Copy mode · exit"),
    ("q", "Copy mode · exit"),
    ("tab", "Editor Controls · next option"),
    ("space", "Editor Controls · choose option"),
    ("enter", "Diff branch menu · switch to typed branch"),
    ("up", "Custom Buttons · previous button"),
    ("down", "Custom Buttons · next button"),
    ("enter", "Custom Buttons · edit, create, or save button"),
    ("delete", "Custom Buttons · delete selected button"),
    ("backspace", "Custom Buttons · delete selected button"),
    ("tab", "Custom Buttons editor · next field"),
    ("escape", "Settings · dismiss dropdown or close"),
    ("escape", "Shortcut recording and capture · cancel"),
    (
        "backspace",
        "Settings font picker · delete filter character",
    ),
];

pub fn settings_shortcuts(shortcuts: &HashMap<String, String>) -> Vec<ShortcutEntry> {
    let mut entries = effective_shortcuts(shortcuts);
    // The action editor normally chooses one representative chord. Also show
    // live alternatives (for example Cmd+C and Ctrl+Shift+C), so key capture
    // can find every binding. Editing either row rebinds the same action.
    let mut candidates: Vec<(&str, &str)> = shortcuts
        .iter()
        .map(|(key, action)| (key.as_str(), action.as_str()))
        .collect();
    candidates.sort_unstable();
    candidates.extend(
        super::defaults::DEFAULTS
            .iter()
            .chain(super::defaults::MACOS_ONLY_DEFAULTS)
            .filter(|binding| {
                !shortcuts
                    .values()
                    .any(|action| action == binding.action_name)
                    && !shortcuts.iter().any(|(key, action)| {
                        (action == "none"
                            || super::registry::ACTIONS
                                .iter()
                                .any(|meta| meta.name == action))
                            && super::keystrokes_conflict(key, binding.key)
                    })
            })
            .map(|binding| (binding.key, binding.action_name)),
    );
    for (key, action) in candidates {
        let formatted = format_keystroke(key);
        if entries
            .iter()
            .any(|entry| entry.action_name == action && entry.key == formatted)
        {
            continue;
        }
        if let Some(primary) = entries.iter().find(|entry| entry.action_name == action) {
            entries.push(ShortcutEntry {
                key: formatted,
                fixed: false,
                description: format!(
                    "{} (alternate binding)",
                    super::registry::action_description(primary.action_name)
                ),
                action_name: primary.action_name,
                group: primary.group,
                search_key: ascii_key_forms(key),
            });
        }
    }
    entries.extend(FIXED.iter().map(|(key, description)| ShortcutEntry {
        key: format_keystroke(key),
        fixed: true,
        description: (*description).into(),
        action_name: "",
        group: ShortcutGroup::Contextual,
        search_key: ascii_key_forms(key),
    }));
    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn widget_bindings_are_all_documented() {
        let pattern =
            regex::Regex::new(r#"KeyBinding::new\(\s*"([^"]+)""#).expect("binding pattern");
        for (source, context) in [
            (include_str!("../widgets/text_input.rs"), "Text field"),
            (
                include_str!("../widgets/text_area.rs"),
                "Text area / Composer",
            ),
            (include_str!("../app/diff_dock/code/view.rs"), "Code editor"),
        ] {
            for binding in pattern.captures_iter(source) {
                assert!(
                    FIXED.iter().any(|(key, description)| *key == &binding[1]
                        && description.starts_with(context)),
                    "Missing {context}: {}",
                    &binding[1]
                );
            }
        }
    }

    #[test]
    fn alternative_bindings_are_visible_only_while_live() {
        let entries = settings_shortcuts(&HashMap::new());
        let copy_keys: Vec<_> = entries
            .iter()
            .filter(|entry| entry.action_name == "terminal_copy")
            .map(|entry| entry.key.clone())
            .collect();
        assert!(copy_keys.contains(&format_keystroke("cmd-c")));
        assert!(copy_keys.contains(&format_keystroke("ctrl-shift-c")));
        let entries = settings_shortcuts(&HashMap::from([("ctrl-shift-c".into(), "none".into())]));
        assert!(
            !entries
                .iter()
                .any(|entry| entry.action_name == "terminal_copy"
                    && entry.key == format_keystroke("ctrl-shift-c"))
        );
        let entries = settings_shortcuts(&HashMap::from([(
            "cmd-alt-c".into(),
            "terminal_copy".into(),
        )]));
        assert_eq!(
            entries
                .iter()
                .filter(|entry| entry.action_name == "terminal_copy")
                .count(),
            1
        );
    }

    #[test]
    fn fixed_bindings_survive_global_overrides() {
        let entries = settings_shortcuts(&HashMap::from([("cmd-s".into(), "none".into())]));
        assert!(entries.iter().any(|entry| entry.fixed
            && entry.description == "Code editor · save"
            && entry.key == format_keystroke("secondary-s")));
        assert!(entries.iter().any(|entry| !entry.fixed
            && entry.action_name == "open_pane_overview"
            && entry.description.contains("Show all panes")));
    }
}
