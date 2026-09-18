//! Command palette (issue #523, upstream `9aa03d09` part 4): one list of every
//! action that needs no key context, each row showing the Settings description
//! and its live binding, filtered on whole words, dispatched on Enter.
//!
//! Bound to `secondary-shift-o` (Cmd+Shift+O) in this fork: upstream's
//! `secondary-shift-p` is Pane Overview here (issue #339). The overlay is the
//! theme picker's shell (`menu_surface`, 544 wide, docked 96 px from the top
//! over the 0.4 scrim) and reads `effective_shortcuts`, so a user override in
//! `paneflow.json` shows up on the row without a second source of truth. The
//! palette never lists itself: opening it from itself is a toggle, not a
//! command.

use gpui::{
    AnyElement, ClickEvent, Context, CursorStyle, InteractiveElement, IntoElement, KeyDownEvent,
    MouseButton, ParentElement, SharedString, Styled, Window, deferred, div, prelude::*, px,
};

use crate::PaneFlowApp;
use crate::keybindings::{ShortcutEntry, action_is_global};
use crate::settings::components::{menu_divider_color, menu_surface, select_item};

/// The registry name of the action that opens this palette. Filtered out of
/// its own rows: a palette that lists "Command palette" would only toggle
/// itself closed.
pub(crate) const OPEN_COMMAND_PALETTE_ACTION: &str = "open_command_palette";

pub(crate) const COMMAND_PALETTE_WIDTH: f32 = 544.0;
pub(crate) const COMMAND_PALETTE_MAX_LIST_HEIGHT: f32 = 360.0;

/// One palette row: the registry action, its Settings description, and the
/// chord it currently answers to (`None` while `Unassigned`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommandMatch {
    pub(crate) action_name: &'static str,
    pub(crate) description: String,
    pub(crate) shortcut: Option<String>,
}

/// Every whitespace-separated word of `query` must occur in `haystack`, in any
/// order; both sides are compared lowercase. A prefix of a word matches, so
/// `laun` finds `Launch Pad`.
fn matches_query(haystack: &str, query: &str) -> bool {
    let haystack = haystack.to_lowercase();
    query
        .split_whitespace()
        .all(|word| haystack.contains(&word.to_lowercase()))
}

/// The rows the palette shows for `query`, from the live shortcut list:
/// context-free registry actions only (never the palette itself), sorted by
/// description so the order never depends on table position.
pub(crate) fn command_matches(entries: &[ShortcutEntry], query: &str) -> Vec<CommandMatch> {
    let mut matches: Vec<CommandMatch> = entries
        .iter()
        .filter(|entry| entry.action_name != OPEN_COMMAND_PALETTE_ACTION)
        .filter(|entry| action_is_global(entry.action_name))
        .filter(|entry| query.trim().is_empty() || matches_query(&entry.description, query))
        .map(|entry| CommandMatch {
            action_name: entry.action_name,
            description: entry.description.clone(),
            shortcut: (entry.key != "Unassigned").then(|| entry.key.clone()),
        })
        .collect();
    matches.sort_by(|a, b| a.description.cmp(&b.description));
    matches
}

impl PaneFlowApp {
    pub(crate) fn command_palette_matches(&self) -> Vec<CommandMatch> {
        command_matches(&self.effective_shortcuts, &self.command_palette_query)
    }

    pub(crate) fn open_command_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.command_palette_open {
            self.close_command_palette_and_restore_focus(window, cx);
            return;
        }
        self.dismiss_transient_surfaces();
        self.command_palette_open = true;
        self.command_palette_query.clear();
        self.command_palette_selected = 0;
        self.command_palette_scroll = gpui::ScrollHandle::new();
        self.command_palette_focus.focus(window, cx);
        cx.notify();
    }

    pub(crate) fn close_command_palette(&mut self, cx: &mut Context<Self>) {
        if !self.command_palette_open {
            return;
        }
        self.command_palette_open = false;
        self.command_palette_query.clear();
        self.command_palette_selected = 0;
        cx.notify();
    }

    /// Close and hand focus back to the active workspace's first pane, or to
    /// the empty-workspace placeholder (issue #108: an overlay that closes
    /// with nothing focused leaves every global chord without a handler).
    pub(crate) fn close_command_palette_and_restore_focus(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_command_palette(cx);
        let focused = match self.workspaces.get(self.active_idx) {
            Some(ws) => ws.focus_first(window, cx),
            None => false,
        };
        if !focused {
            window.focus(&self.empty_workspace_focus, cx);
        }
    }

    pub(crate) fn handle_open_command_palette(
        &mut self,
        _: &crate::OpenCommandPalette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_command_palette(window, cx);
    }

    /// Enter or a click on row `idx`: close, restore focus so the action lands
    /// on the pane the user was in, then dispatch through the same registry
    /// factory the keymap uses.
    fn command_palette_run(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(action) = self
            .command_palette_matches()
            .get(idx)
            .and_then(|entry| crate::keybindings::action_for_name(entry.action_name))
        else {
            return;
        };
        self.close_command_palette_and_restore_focus(window, cx);
        window.dispatch_action(action, cx);
    }

    fn command_palette_select(&mut self, idx: usize, cx: &mut Context<Self>) {
        self.command_palette_selected = idx;
        self.command_palette_scroll.scroll_to_item(idx);
        cx.notify();
    }

    pub(crate) fn handle_command_palette_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let len = self.command_palette_matches().len();
        match event.keystroke.key.as_str() {
            "escape" => self.close_command_palette_and_restore_focus(window, cx),
            "enter" => {
                if self.command_palette_selected < len {
                    self.command_palette_run(self.command_palette_selected, window, cx);
                }
            }
            "up" => {
                if self.command_palette_selected > 0 {
                    self.command_palette_select(self.command_palette_selected - 1, cx);
                }
            }
            "down" => {
                if self.command_palette_selected + 1 < len {
                    self.command_palette_select(self.command_palette_selected + 1, cx);
                }
            }
            "backspace" => {
                if self.command_palette_query.pop().is_some() {
                    self.command_palette_select(0, cx);
                }
            }
            _ => {
                if let Some(ch) = &event.keystroke.key_char
                    && !ch.is_empty()
                    && !event.keystroke.modifiers.control
                    && !event.keystroke.modifiers.platform
                    && !event.keystroke.modifiers.alt
                {
                    self.command_palette_query.push_str(ch);
                    self.command_palette_select(0, cx);
                }
            }
        }
    }

    pub(crate) fn render_command_palette(&self, cx: &mut Context<Self>) -> AnyElement {
        let ui = crate::theme::ui_colors();
        let matches = self.command_palette_matches();

        let query_text: SharedString = if self.command_palette_query.is_empty() {
            "Execute a command…".into()
        } else {
            format!("{}|", self.command_palette_query).into()
        };
        let query_color = if self.command_palette_query.is_empty() {
            ui.muted
        } else {
            ui.text
        };

        let search_input = div()
            .px(px(14.))
            .py(px(10.))
            .text_size(px(13.))
            .text_color(query_color)
            .border_b_1()
            .border_color(menu_divider_color(ui))
            .child(query_text);

        let mut list = div()
            .id("command-palette-list")
            .flex()
            .flex_col()
            .gap(px(1.))
            .p(px(4.))
            .max_h(px(COMMAND_PALETTE_MAX_LIST_HEIGHT))
            .overflow_y_scroll()
            .track_scroll(&self.command_palette_scroll);

        if matches.is_empty() {
            list = list.child(
                div()
                    .px(px(8.))
                    .py(px(12.))
                    .text_size(px(12.))
                    .text_color(ui.muted)
                    .child("No matching command"),
            );
        } else {
            for (idx, entry) in matches.iter().enumerate() {
                let is_selected = idx == self.command_palette_selected;
                list = list.child(
                    select_item(
                        SharedString::from(format!("command-palette-row-{idx}")),
                        is_selected,
                        ui,
                    )
                    .cursor(CursorStyle::PointingHand)
                    .justify_between()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.command_palette_run(idx, window, cx);
                        cx.stop_propagation();
                    }))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_x_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .text_color(ui.text)
                            .child(entry.description.clone()),
                    )
                    .when_some(entry.shortcut.clone(), |row, key| {
                        row.child(
                            div()
                                .flex_none()
                                .pl(px(8.))
                                .text_size(px(11.))
                                .text_color(ui.muted)
                                .child(key),
                        )
                    }),
                );
            }
        }

        deferred(
            div()
                .id("command-palette-backdrop")
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .flex()
                .items_start()
                .justify_center()
                .pt(px(96.))
                .bg(gpui::hsla(0., 0., 0., 0.4))
                .child(crate::ui_primitives::menu_reveal(
                    "command-palette-reveal",
                    menu_surface(div().id("command-palette"), ui)
                        .occlude()
                        .track_focus(&self.command_palette_focus)
                        .on_key_down(cx.listener(Self::handle_command_palette_key_down))
                        .on_mouse_down_out(cx.listener(|this, _, window, cx| {
                            this.close_command_palette_and_restore_focus(window, cx);
                        }))
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
                        .w(px(COMMAND_PALETTE_WIDTH))
                        .flex()
                        .flex_col()
                        .overflow_hidden()
                        .child(search_input)
                        .child(list),
                )),
        )
        .with_priority(7)
        .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::{OPEN_COMMAND_PALETTE_ACTION, command_matches, matches_query};
    use crate::keybindings::{ShortcutEntry, ShortcutGroup, effective_shortcuts};
    use std::collections::HashMap;

    #[test]
    fn a_query_matches_a_description_in_any_word_order() {
        assert!(matches_query("split horizontal", "horizontal split"));
    }

    #[test]
    fn a_query_matches_on_a_prefix_of_a_word() {
        assert!(matches_query("Launch Pad", "laun"));
    }

    #[test]
    fn a_query_rejects_a_word_the_description_lacks() {
        assert!(!matches_query("split horizontal", "split vertical"));
    }

    fn entry(action_name: &'static str, key: &str, description: &str) -> ShortcutEntry {
        ShortcutEntry {
            key: key.to_string(),
            fixed: false,
            description: description.to_string(),
            action_name,
            group: ShortcutGroup::Application,
            search_key: String::new(),
        }
    }

    #[test]
    fn the_palette_lists_global_actions_and_never_itself() {
        let entries = vec![
            entry("split_horizontally", "⌘⇧D", "Split horizontal"),
            entry("terminal_copy", "⌘C", "Copy"),
            entry(OPEN_COMMAND_PALETTE_ACTION, "⌘⇧O", "Command palette"),
            entry("open_launch_pad", "Unassigned", "Launch Pad"),
        ];
        let rows = command_matches(&entries, "");
        let names: Vec<&str> = rows.iter().map(|m| m.action_name).collect();
        assert_eq!(
            names,
            vec!["open_launch_pad", "split_horizontally"],
            "context-free actions only, sorted by description, never the palette itself"
        );
        assert_eq!(rows[0].shortcut, None, "an Unassigned row carries no chord");
        assert_eq!(rows[1].shortcut.as_deref(), Some("⌘⇧D"));
    }

    #[test]
    fn the_query_filters_rows_on_whole_words() {
        let entries = vec![
            entry("split_horizontally", "⌘⇧D", "Split horizontal"),
            entry("split_vertically", "⌘⇧E", "Split vertical"),
            entry("open_launch_pad", "⌘⇧L", "Launch Pad"),
        ];
        let rows = command_matches(&entries, "vert split");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].action_name, "split_vertically");
        assert!(command_matches(&entries, "zzz").is_empty());
    }

    /// The live list, not a fixture: every default-bound global action is a
    /// row, no contextual one is, and the palette's own action is absent even
    /// though it is bound.
    #[test]
    fn the_live_shortcut_list_yields_the_palette_rows() {
        let entries = effective_shortcuts(&HashMap::new());
        assert!(
            entries
                .iter()
                .any(|e| e.action_name == OPEN_COMMAND_PALETTE_ACTION && e.key != "Unassigned"),
            "the palette must have a default chord"
        );
        let rows = command_matches(&entries, "");
        assert!(rows.iter().any(|m| m.action_name == "split_horizontally"));
        assert!(rows.iter().any(|m| m.action_name == "open_pane_overview"));
        assert!(
            !rows.iter().any(|m| m.action_name == "terminal_copy"),
            "a Terminal-scoped action must not be listed"
        );
        assert!(
            !rows
                .iter()
                .any(|m| m.action_name == OPEN_COMMAND_PALETTE_ACTION),
            "the palette never lists itself"
        );
        let mut sorted = rows.clone();
        sorted.sort_by(|a, b| a.description.cmp(&b.description));
        assert_eq!(rows, sorted, "rows are sorted by description");
    }

    /// The sidebar empty state reaches the palette (issue #523's "reachable
    /// from the sidebar empty state" clause) and the render root registers
    /// the handler. Source-text assertions, like the #105 and #521 guards in
    /// `app/sidebar/mod.rs`: `PaneFlowApp` cannot be built in a unit test.
    #[test]
    fn the_palette_is_wired_into_the_sidebar_empty_state_and_the_render_root() {
        let sidebar = include_str!("sidebar/mod.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("production half of the sidebar module");
        assert!(
            sidebar.contains(".id(\"empty-command-palette\")"),
            "the sidebar empty state must carry the Command palette row"
        );
        assert!(
            sidebar.contains("this.open_command_palette(w, cx);"),
            "the empty-state row must open the palette"
        );
        let main = include_str!("../main.rs");
        assert!(
            main.contains(".on_action(cx.listener(Self::handle_open_command_palette))"),
            "the render root must handle OpenCommandPalette"
        );
        assert!(
            main.contains("self.render_command_palette(cx)"),
            "the render root must mount the palette overlay"
        );
    }
}
