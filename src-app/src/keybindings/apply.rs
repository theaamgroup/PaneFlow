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
    // global `TextInput` / `TextArea` widget bindings (caret movement, Home/End,
    // selection, Backspace/Delete, clipboard) that are registered once at
    // startup. Re-register them on every apply so text fields keep working after
    // a shortcut rebind, config reload, settings navigation, or IPC-driven
    // re-apply - otherwise a re-apply silently degrades every input to IME-only
    // typing (the field accepts characters but ignores arrows, selection, and
    // clipboard).
    crate::widgets::text_input::register_keybindings(cx);
    crate::widgets::text_area::register_keybindings(cx);
    // Same reasoning for the code editor's navigation bindings (EP-003
    // US-011): they are scoped to the `CodeEditor` key context, so they never
    // shadow a global shortcut, but the clear above takes them out with
    // everything else.
    crate::app::diff_dock::code::view::register_keybindings(cx);
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

    /// Issue #184 (upstream v0.10.0 port): the Attention Queue gave up
    /// `secondary-shift-k` and moved to `secondary-shift-a`, because ⇧⌘K is
    /// what kitty and Ghostty use for "clear scrollback" and that action now
    /// owns it. macOS also spells clear-scrollback as bare `cmd-k` (iTerm2,
    /// Terminal.app), so that alias lives in `MACOS_ONLY_DEFAULTS`; both
    /// reach `clear_scroll_history`. `secondary-shift-r` resets the terminal.
    /// `close_window` is gone: closing the only window is `quit` here, and a
    /// registry entry with no handler would only ever be an `Unassigned`
    /// row. Every claimant list is exact, so the queue drifting back onto
    /// ⇧⌘K, or a second action landing on any of these chords, fails loudly.
    #[test]
    fn attention_queue_is_cmd_shift_a_and_cmd_shift_k_clears_scrollback() {
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
            ("secondary-shift-a", "open_attention_queue", None),
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

        let queue_on_shift_k = DEFAULTS.iter().chain(MACOS_ONLY_DEFAULTS.iter()).any(|d| {
            d.action_name == "open_attention_queue"
                && keystrokes_conflict(d.key, "secondary-shift-k")
        });
        assert!(
            !queue_on_shift_k,
            "open_attention_queue must not drift back onto secondary-shift-k"
        );

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

    /// EP-005 US-018 (prd-file-editor-2026-Q3): the two diff-dock chords are
    /// bindable, claimed by exactly one default each, and free of any conflict
    /// with the rest of the action table. Written with `secondary-`, so the
    /// same assertion covers Ctrl on Linux/Windows and Cmd on macOS.
    #[test]
    fn diff_dock_tab_chords_are_bindable_and_do_not_collide() {
        use super::super::defaults::DEFAULTS;

        for (key, action_name) in [
            ("secondary-g", "diff_new_file_tab"),
            ("secondary-j", "diff_new_terminal_tab"),
        ] {
            let context = context_for_action(action_name);
            let action = action_from_name(action_name).expect("registered action");
            assert!(
                make_binding(key, action, context).is_some(),
                "{key} must parse into a valid KeyBinding with context {context:?}"
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

            // The chord must not reach a shell: Ctrl+G is BEL and Ctrl+J is LF,
            // and both stay the terminal's on every platform. Same for the text
            // widgets, where they are ordinary input, and for the code editor
            // the file chord itself opens.
            let context = context.expect("the dock chords must be context-scoped");
            for excluded in [
                "Terminal",
                "TextInput",
                "PaneflowTextArea",
                crate::app::diff_dock::code::view::CODE_KEY_CONTEXT,
            ] {
                assert!(
                    context.contains(&format!("!{excluded}")),
                    "{key} must be scoped away from {excluded}, got `{context}`"
                );
            }
        }
    }

    /// Issue #106: the primary rail's collapse chord is bindable, claimed by
    /// exactly one default, and free of any conflict with the rest of the
    /// table. `secondary-alt-b` sits one modifier away from
    /// `secondary-shift-b` (`toggle_broadcast_member`), and
    /// `keystrokes_conflict` normalizes modifier order, so a chord picked by
    /// eye rather than by this assertion could silently shadow broadcast
    /// membership instead of failing loudly.
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

        assert!(
            !keystrokes_conflict(key, "secondary-shift-b"),
            "{key} must stay distinct from the broadcast-member chord"
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
