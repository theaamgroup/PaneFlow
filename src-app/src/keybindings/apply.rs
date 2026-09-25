//! Register defaults and layer user overrides onto GPUI's keybinding registry.

use std::collections::HashMap;

use gpui::{Action, App, DummyKeyboardMapper, KeyBinding, KeyBindingContextPredicate, Keystroke};

use super::defaults::{DEFAULTS, MACOS_ONLY_DEFAULTS};
use super::registry::{action_from_name, context_for_action};

/// Normalize a user-friendly keystroke string to GPUI format.
///
/// Users may write `"ctrl+shift+d"` (plus separators) in `paneflow.json`,
/// but GPUI expects `"ctrl-shift-d"` (dash separators).
pub(super) fn normalize_keystroke(keystrokes: &str) -> String {
    keystrokes.replace('+', "-")
}

/// Canonical form of a keystroke string for *physical chord* comparison.
///
/// US-021: parsing through GPUI resolves `+`/`-` separators, modifier order,
/// and the `secondary` shorthand (→ `cmd`) into the same `Keystroke` value,
/// so `"cmd+shift+d"`, `"shift-cmd-d"`, and `"secondary-shift-d"` all
/// compare equal. Returns `None` for unparseable input (which then only
/// matches by raw equality at the call site).
pub(super) fn canonical_keystroke(keystrokes: &str) -> Option<Keystroke> {
    Keystroke::parse(&normalize_keystroke(keystrokes)).ok()
}

/// True if two keystroke strings denote the same physical chord, normalization
/// applied (see [`canonical`]). Unparseable strings only match by exact
/// equality. Used by the settings writer to collapse a rebind onto a key that
/// is already taken instead of leaving two live entries (GPUI would resolve
/// the conflict order-dependently).
pub fn keystrokes_conflict(a: &str, b: &str) -> bool {
    match (canonical_keystroke(a), canonical_keystroke(b)) {
        (Some(ka), Some(kb)) => ka == kb,
        _ => a == b,
    }
}

/// Build a `KeyBinding` from a boxed action, using `KeyBinding::load` to avoid
/// the `A: Action` bound on `KeyBinding::new`. Returns `None` on invalid keystroke.
pub(super) fn make_binding(
    keystrokes: &str,
    action: Box<dyn Action>,
    context: Option<&str>,
) -> Option<KeyBinding> {
    let normalized = normalize_keystroke(keystrokes);
    let predicate = match context {
        Some(ctx) => match KeyBindingContextPredicate::parse(ctx) {
            Ok(p) => Some(p.into()),
            Err(e) => {
                log::warn!("shortcuts: invalid context predicate '{ctx}': {e}");
                return None;
            }
        },
        None => None,
    };
    match KeyBinding::load(
        &normalized,
        action,
        predicate,
        false,
        None,
        &DummyKeyboardMapper,
    ) {
        Ok(binding) => Some(binding),
        Err(e) => {
            log::warn!("shortcuts: invalid keystroke '{keystrokes}': {e}");
            None
        }
    }
}

/// Apply keybindings: clear all, register defaults, then layer user overrides.
///
/// User shortcuts map keystroke strings to action names. Special values:
/// - `"none"` - unbinds the key (no action registered for it)
/// - Any valid action name - overrides or adds a binding for that key
pub fn apply_keybindings(cx: &mut App, user_shortcuts: &HashMap<String, String>) {
    cx.clear_key_bindings();

    // Keys the user explicitly unbound via "none". US-021: canonicalized so
    // that an unbind written as "ctrl+shift+d" or "secondary-shift-d" actually
    // suppresses the matching default (whose key string uses the `secondary`
    // shorthand), instead of failing the raw `==` comparison and leaving the
    // default live.
    let unbound_canonical: std::collections::HashSet<Keystroke> = user_shortcuts
        .iter()
        .filter(|(_, v)| v.as_str() == "none")
        .filter_map(|(k, _)| canonical_keystroke(k))
        .collect();

    // Actions the user remapped to a different key (drop their default key).
    let remapped_actions: std::collections::HashSet<&str> = user_shortcuts
        .iter()
        .filter(|(_, v)| v.as_str() != "none")
        .filter_map(|(_, action_name)| {
            if action_from_name(action_name).is_some() {
                Some(action_name.as_str())
            } else {
                None
            }
        })
        .collect();

    // Keys the user bound to some real action. US-021: a default that shares
    // one of these keys (for a *different* action) would otherwise stay active
    // alongside the override → GPUI-ambiguous double binding (the root cause at
    // the old `apply.rs:86`, e.g. a default `ctrl-shift-f → toggle_search`
    // surviving next to a user `ctrl-shift-f → close_pane`). Drop it: a chord
    // belongs to exactly one action, last writer wins.
    let user_bound_canonical: std::collections::HashSet<Keystroke> = user_shortcuts
        .iter()
        .filter(|(_, v)| v.as_str() != "none")
        .filter(|(_, action_name)| action_from_name(action_name).is_some())
        .filter_map(|(k, _)| canonical_keystroke(k))
        .collect();

    let is_unbound =
        |key: &str| canonical_keystroke(key).is_some_and(|k| unbound_canonical.contains(&k));
    let is_user_claimed =
        |key: &str| canonical_keystroke(key).is_some_and(|k| user_bound_canonical.contains(&k));

    // Register defaults, skipping unbound keys, remapped actions, and keys the
    // user reassigned to another action.
    // US-010: chain macOS-only defaults (cmd-c/cmd-v in Terminal context).
    let default_bindings: Vec<KeyBinding> = DEFAULTS
        .iter()
        .chain(MACOS_ONLY_DEFAULTS.iter())
        .filter(|d| !is_unbound(d.key))
        .filter(|d| !remapped_actions.contains(d.action_name))
        .filter(|d| !is_user_claimed(d.key))
        .filter_map(|d| {
            let action = action_from_name(d.action_name)?;
            make_binding(d.key, action, d.context)
        })
        .collect();
    cx.bind_keys(default_bindings);

    // Layer user overrides
    for (key, action_name) in user_shortcuts {
        if action_name == "none" {
            continue;
        }
        let Some(action) = action_from_name(action_name) else {
            log::warn!("shortcuts: unknown action '{action_name}' for key '{key}', skipping");
            continue;
        };
        let context = context_for_action(action_name);
        if let Some(binding) = make_binding(key, action, context) {
            cx.bind_keys([binding]);
        }
    }

    // `cx.clear_key_bindings()` at the top wiped EVERY binding, including the
    // global `TextInput` widget bindings (caret movement, Home/End,
    // selection, Backspace/Delete, clipboard) that are registered once at
    // startup. Re-register them on every apply so text fields keep working after
    // a shortcut rebind, config reload, settings navigation, or IPC-driven
    // re-apply - otherwise a re-apply silently degrades every input to IME-only
    // typing (the field accepts characters but ignores arrows, selection, and
    // clipboard).
    crate::widgets::text_input::register_keybindings(cx);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SplitHorizontally;

    #[test]
    fn normalize_keystroke_converts_plus_to_dash() {
        assert_eq!(normalize_keystroke("ctrl+shift+d"), "ctrl-shift-d");
        assert_eq!(normalize_keystroke("alt+left"), "alt-left");
    }

    #[test]
    fn normalize_keystroke_already_dashed_unchanged() {
        assert_eq!(normalize_keystroke("ctrl-shift-d"), "ctrl-shift-d");
    }

    #[test]
    fn keystrokes_conflict_ignores_separator_and_order() {
        // US-021: `+`/`-` separators and modifier order are normalized away.
        assert!(keystrokes_conflict("ctrl+shift+f", "ctrl-shift-f"));
        assert!(keystrokes_conflict("shift-ctrl-f", "ctrl-shift-f"));
        assert!(!keystrokes_conflict("ctrl-shift-f", "ctrl-shift-g"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn keystrokes_conflict_resolves_secondary_on_macos() {
        // `secondary` resolves to cmd (platform) on macOS.
        assert!(keystrokes_conflict("secondary-shift-d", "cmd-shift-d"));
        assert!(!keystrokes_conflict("secondary-shift-d", "ctrl-shift-d"));
    }

    #[test]
    fn secondary_binding_parses_successfully() {
        // AC2/AC3: make_binding accepts the `secondary` prefix on both
        // platforms. GPUI's Keystroke::parse resolves it internally.
        let binding = make_binding("secondary-shift-d", Box::new(SplitHorizontally), None);
        assert!(
            binding.is_some(),
            "secondary-shift-d must parse into a valid KeyBinding"
        );
    }

    #[test]
    fn cmd_override_parses_on_any_platform() {
        // AC5: a user writing `"split_horizontally": "cmd-shift-d"` in
        // paneflow.json must produce a valid binding (GPUI accepts `cmd`
        // as a synonym for the platform modifier).
        let binding = make_binding("cmd-shift-d", Box::new(SplitHorizontally), None);
        assert!(
            binding.is_some(),
            "cmd-shift-d override must parse on any platform"
        );
    }

    /// US-020 (prd-cli-tab-hierarchy): the two tab-cycling defaults parse,
    /// claim free chords, and leave `ctrl-tab` (next *workspace*) alone.
    #[test]
    fn tab_cycling_defaults_are_bindable_and_do_not_collide() {
        use super::super::defaults::DEFAULTS;

        for (key, action_name) in [("secondary-]", "next_tab"), ("secondary-[", "previous_tab")] {
            let action = action_from_name(action_name).expect("registered action");
            assert!(
                make_binding(key, action, None).is_some(),
                "{key} must parse into a valid KeyBinding"
            );
            let claimants: Vec<&str> = DEFAULTS
                .iter()
                .filter(|d| keystrokes_conflict(d.key, key))
                .map(|d| d.action_name)
                .collect();
            assert_eq!(
                claimants,
                vec![action_name],
                "{key} must be claimed by exactly one default"
            );
        }

        // `ctrl-tab` keeps meaning "next workspace".
        assert!(
            DEFAULTS
                .iter()
                .any(|d| d.key == "ctrl-tab" && d.action_name == "next_workspace"),
            "the tab shortcuts must not steal ctrl-tab from next_workspace"
        );
    }

    /// Issue #10: macOS owns Cmd+Tab for the application switcher, so
    /// `next_workspace` lives on `ctrl-tab`. The chord must parse, must be
    /// claimed by exactly one default, and nothing may bind `secondary-tab`
    /// (which resolves to Cmd+Tab here and never reaches the app).
    #[test]
    fn next_workspace_is_bound_to_ctrl_tab_and_nothing_binds_cmd_tab() {
        use super::super::defaults::DEFAULTS;

        let action = action_from_name("next_workspace").expect("registered action");
        assert!(
            make_binding("ctrl-tab", action, None).is_some(),
            "ctrl-tab must parse into a valid KeyBinding"
        );
        let claimants: Vec<&str> = DEFAULTS
            .iter()
            .filter(|d| keystrokes_conflict(d.key, "ctrl-tab"))
            .map(|d| d.action_name)
            .collect();
        assert_eq!(
            claimants,
            vec!["next_workspace"],
            "ctrl-tab must be claimed by next_workspace and nothing else"
        );
        let cmd_tab: Vec<&str> = DEFAULTS
            .iter()
            .filter(|d| keystrokes_conflict(d.key, "secondary-tab"))
            .map(|d| d.action_name)
            .collect();
        assert!(
            cmd_tab.is_empty(),
            "secondary-tab is the macOS app switcher; no default may bind it, found {cmd_tab:?}"
        );
    }

    /// Clear-scrollback aliases and reset retain exclusive ownership of
    /// their terminal chords after overlay removal.
    #[test]
    fn cmd_shift_k_and_cmd_k_clear_scrollback() {
        use super::super::defaults::{DEFAULTS, MACOS_ONLY_DEFAULTS};

        let claimants = |key: &str| -> Vec<(&'static str, Option<&'static str>)> {
            DEFAULTS
                .iter()
                .chain(MACOS_ONLY_DEFAULTS.iter())
                .filter(|d| keystrokes_conflict(d.key, key))
                .map(|d| (d.action_name, d.context))
                .collect()
        };

        for (key, action_name, context) in [
            (
                "secondary-shift-k",
                "clear_scroll_history",
                Some("Terminal"),
            ),
            ("cmd-k", "clear_scroll_history", Some("Terminal")),
            ("secondary-shift-r", "reset_terminal", Some("Terminal")),
        ] {
            assert_eq!(
                context_for_action(action_name),
                context,
                "{action_name} must keep its registry context"
            );
            let action = action_from_name(action_name).expect("registered action");
            assert!(
                make_binding(key, action, context).is_some(),
                "{key} must parse into a valid KeyBinding"
            );
            assert_eq!(
                claimants(key),
                vec![(action_name, context)],
                "{key} must be claimed by {action_name} and nothing else"
            );
        }

        let close_window_defaults: Vec<&str> = DEFAULTS
            .iter()
            .chain(MACOS_ONLY_DEFAULTS.iter())
            .filter(|d| d.action_name == "close_window")
            .map(|d| d.key)
            .collect();
        assert!(
            close_window_defaults.is_empty(),
            "close_window is quit; no default may name it, found {close_window_defaults:?}"
        );
        assert!(
            action_from_name("close_window").is_none(),
            "close_window must be gone from the registry, not merely unbound"
        );
    }

    /// Issue #523: the command palette is `secondary-shift-o`, not upstream's
    /// `secondary-shift-p` (Pane Overview here, #339). Both chords must keep
    /// exactly one claimant, and the palette's action must stay context-free,
    /// or it could not open from a focused terminal - the only place it is
    /// useful.
    #[test]
    fn command_palette_is_cmd_shift_o_and_pane_overview_keeps_cmd_shift_p() {
        use super::super::defaults::{DEFAULTS, MACOS_ONLY_DEFAULTS};

        let claimants = |key: &str| -> Vec<(&'static str, Option<&'static str>)> {
            DEFAULTS
                .iter()
                .chain(MACOS_ONLY_DEFAULTS.iter())
                .filter(|d| keystrokes_conflict(d.key, key))
                .map(|d| (d.action_name, d.context))
                .collect()
        };

        for (key, action_name) in [
            ("secondary-shift-o", "open_command_palette"),
            ("secondary-shift-p", "open_pane_overview"),
        ] {
            assert_eq!(
                context_for_action(action_name),
                None,
                "{action_name} must be context-free"
            );
            let action = action_from_name(action_name).expect("registered action");
            assert!(
                make_binding(key, action, None).is_some(),
                "{key} must parse into a valid KeyBinding"
            );
            assert_eq!(
                claimants(key),
                vec![(action_name, None)],
                "{key} must be claimed by {action_name} and nothing else"
            );
        }
    }

    /// Issue #105: Settings gained a menu-bar item but deliberately did NOT
    /// gain `Cmd+,`. The issue resolved that explicitly, and it is the right
    /// call here: a global default on that chord would swallow the comma from
    /// every focused terminal running a program that wants it. Modelled on the
    /// `secondary-tab` prohibition above; checks both tables, because a macOS
    /// convention chord would most plausibly be added to the macOS-only layer.
    #[test]
    fn no_default_binds_the_macos_preferences_chord() {
        use super::super::defaults::{DEFAULTS, MACOS_ONLY_DEFAULTS};

        let claimants: Vec<&str> = DEFAULTS
            .iter()
            .chain(MACOS_ONLY_DEFAULTS.iter())
            .filter(|d| {
                keystrokes_conflict(d.key, "cmd-,") || keystrokes_conflict(d.key, "secondary-,")
            })
            .map(|d| d.action_name)
            .collect();
        assert!(
            claimants.is_empty(),
            "issue #105 resolved that Settings does not claim cmd-,; bound to: {claimants:?}"
        );
    }

    /// US-020: a user who already bound `secondary-]` to something else keeps
    /// it. `apply_keybindings` drops the default sharing a user-claimed chord
    /// before registering it, so no ambiguous double binding - and no
    /// error-level conflict - is produced. Issue #304: this drives the real
    /// `apply_keybindings` against a live GPUI keymap with a user chord that
    /// actually collides (`cmd+]` is `secondary-]` on macOS), then reads the
    /// registered bindings back, so removing the user-claimed filter fails
    /// here instead of leaving `next_tab` silently dead on the user's chord.
    #[gpui::test]
    fn user_override_of_a_tab_shortcut_wins_over_the_default(cx: &mut gpui::TestAppContext) {
        use super::super::defaults::DEFAULTS;

        let user_key = "cmd+]";
        let user_action = "split_horizontally";
        let user_claimed = canonical_keystroke(user_key).expect("a parsable user chord");

        // The premise: the user's chord really is the `next_tab` default.
        let colliding: Vec<&str> = DEFAULTS
            .iter()
            .filter(|d| canonical_keystroke(d.key).is_some_and(|k| k == user_claimed))
            .map(|d| d.action_name)
            .collect();
        assert_eq!(
            colliding,
            vec!["next_tab"],
            "{user_key} must collide with exactly the next_tab default"
        );

        let user_shortcuts: HashMap<String, String> =
            HashMap::from([(user_key.to_string(), user_action.to_string())]);
        cx.update(|cx| apply_keybindings(cx, &user_shortcuts));

        let bound: Vec<&'static str> = cx
            .update(|cx| cx.all_bindings_for_input(std::slice::from_ref(&user_claimed)))
            .iter()
            .map(|binding| binding.action().name())
            .collect();
        let expected = action_from_name(user_action)
            .expect("registered action")
            .name();
        assert_eq!(
            bound,
            vec![expected],
            "{user_key} must reach only the user's {user_action}; the next_tab default \
             sharing that chord must be dropped, got {bound:?}"
        );
    }

    /// Issue #808: the diff dock and its two actions are gone. A
    /// `paneflow.json` written while they existed still names them under
    /// `shortcuts`; it must keep loading (the sibling settings survive, not a
    /// fallback to defaults), and neither chord may bind to anything.
    #[gpui::test]
    fn removed_dock_actions_in_config_load_and_bind_nothing(cx: &mut gpui::TestAppContext) {
        let json = r#"{
            "font_size": 17,
            "shortcuts": {
                "cmd-shift-f": "toggle_diff_dock_maximize",
                "cmd-j": "diff_new_terminal_tab"
            }
        }"#;
        let config = paneflow_config::loader::try_parse_and_validate(json)
            .expect("a config naming removed actions still loads");
        assert_eq!(
            config.font_size,
            Some(17.0),
            "the rest of the file must load, not fall back to defaults"
        );
        assert_eq!(
            config.shortcuts.get("cmd-shift-f").map(String::as_str),
            Some("toggle_diff_dock_maximize")
        );
        assert_eq!(
            config.shortcuts.get("cmd-j").map(String::as_str),
            Some("diff_new_terminal_tab")
        );
        for removed in ["toggle_diff_dock_maximize", "diff_new_terminal_tab"] {
            assert!(
                action_from_name(removed).is_none(),
                "{removed} must no longer be a registered action"
            );
        }

        cx.update(|cx| apply_keybindings(cx, &config.shortcuts));
        for key in ["cmd-shift-f", "cmd-j"] {
            let chord = canonical_keystroke(key).expect("a parsable chord");
            let bound: Vec<&'static str> = cx
                .update(|cx| cx.all_bindings_for_input(std::slice::from_ref(&chord)))
                .iter()
                .map(|binding| binding.action().name())
                .collect();
            assert!(bound.is_empty(), "{key} must bind nothing, got {bound:?}");
        }
    }

    /// Issue #809: the Composer and broadcast groups are gone with their three
    /// actions. A `paneflow.json` that still maps them under `shortcuts` must
    /// keep loading (the sibling settings survive, not a fallback to
    /// defaults), and none of the chords may bind to anything.
    #[gpui::test]
    fn removed_composer_and_broadcast_actions_in_config_load_and_bind_nothing(
        cx: &mut gpui::TestAppContext,
    ) {
        let json = r#"{
            "font_size": 17,
            "shortcuts": {
                "cmd-shift-space": "open_composer",
                "cmd-shift-b": "toggle_broadcast_member",
                "cmd-shift-m": "open_broadcast_groups"
            }
        }"#;
        let config = paneflow_config::loader::try_parse_and_validate(json)
            .expect("a config naming removed actions still loads");
        assert_eq!(
            config.font_size,
            Some(17.0),
            "the rest of the file must load, not fall back to defaults"
        );
        let removed = [
            ("cmd-shift-space", "open_composer"),
            ("cmd-shift-b", "toggle_broadcast_member"),
            ("cmd-shift-m", "open_broadcast_groups"),
        ];
        for (key, action) in removed {
            assert_eq!(config.shortcuts.get(key).map(String::as_str), Some(action));
            assert!(
                action_from_name(action).is_none(),
                "{action} must no longer be a registered action"
            );
        }

        cx.update(|cx| apply_keybindings(cx, &config.shortcuts));
        for (key, _) in removed {
            let chord = canonical_keystroke(key).expect("a parsable chord");
            let bound: Vec<&'static str> = cx
                .update(|cx| cx.all_bindings_for_input(std::slice::from_ref(&chord)))
                .iter()
                .map(|binding| binding.action().name())
                .collect();
            assert!(bound.is_empty(), "{key} must bind nothing, got {bound:?}");
        }
    }

    /// Issue #106: the primary rail's collapse chord is bindable, claimed by
    /// exactly one default, and free of any conflict with the rest of the
    /// table. `keystrokes_conflict` normalizes modifier order, so a chord
    /// picked by eye rather than by this assertion could silently shadow
    /// another default instead of failing loudly.
    #[test]
    fn primary_sidebar_chord_is_bindable_and_does_not_collide() {
        use super::super::defaults::DEFAULTS;

        let key = "secondary-alt-b";
        let action_name = "toggle_primary_sidebar";

        let context = context_for_action(action_name);
        assert_eq!(
            context, None,
            "the rail toggle is global: scoping it would make it dead while a \
             terminal holds focus, which is nearly always"
        );
        let action = action_from_name(action_name).expect("registered action");
        assert!(
            make_binding(key, action, context).is_some(),
            "{key} must parse into a valid KeyBinding"
        );

        let claimants: Vec<&str> = DEFAULTS
            .iter()
            .chain(MACOS_ONLY_DEFAULTS.iter())
            .filter(|d| keystrokes_conflict(d.key, key))
            .map(|d| d.action_name)
            .collect();
        assert_eq!(
            claimants,
            vec![action_name],
            "{key} must be claimed by exactly one default on this platform"
        );
    }

    /// The rule every per-chord test above instantiates once, stated for the
    /// whole table: within one key context a chord belongs to exactly one
    /// action, because GPUI resolves two bindings on the same chord in the
    /// same context order-dependently and the loser is silently dead. The
    /// same chord in different contexts is fine (context precedence, not a
    /// collision), and `None` - global - is a context of its own. Chords are
    /// compared through `keystrokes_conflict`, so `secondary-`, `cmd-`, and
    /// modifier order cannot hide a duplicate.
    #[test]
    fn no_two_default_actions_claim_the_same_chord_in_the_same_context() {
        use super::super::defaults::{DEFAULTS, DefaultBinding, MACOS_ONLY_DEFAULTS};

        let table: Vec<&DefaultBinding> =
            DEFAULTS.iter().chain(MACOS_ONLY_DEFAULTS.iter()).collect();
        let mut collisions = Vec::new();
        for (i, a) in table.iter().enumerate() {
            for b in &table[i + 1..] {
                if a.context == b.context
                    && a.action_name != b.action_name
                    && keystrokes_conflict(a.key, b.key)
                {
                    collisions.push(format!(
                        "{} -> {} and {} -> {} (context {:?})",
                        a.key, a.action_name, b.key, b.action_name, a.context
                    ));
                }
            }
        }
        assert!(
            collisions.is_empty(),
            "a chord may map to one action per context; found {collisions:#?}"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn us010_cmd_c_parses_as_binding() {
        use crate::TerminalCopy;
        let binding = make_binding("cmd-c", Box::new(TerminalCopy), Some("Terminal"));
        assert!(binding.is_some(), "cmd-c must parse as a valid KeyBinding");
    }
}
