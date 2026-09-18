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
    AnyElement, App, ClickEvent, Context, CursorStyle, Entity, Focusable, InteractiveElement,
    IntoElement, KeyDownEvent, MouseButton, ParentElement, SharedString, Styled, Window, deferred,
    div, prelude::*, px,
};

use crate::PaneFlowApp;
use crate::keybindings::{ShortcutEntry, action_is_global};
use crate::pane::Pane;
use crate::settings::components::{menu_divider_color, menu_surface, select_option};

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

/// Every whitespace-separated term of `query` must be a prefix of some word
/// of `haystack`, in any order; both sides are compared lowercase. `laun`
/// finds `Launch Pad`; `riz` does not find `Split horizontal`, because the
/// filter is on whole words, not substrings.
fn matches_query(haystack: &str, query: &str) -> bool {
    let words: Vec<String> = haystack.split_whitespace().map(str::to_lowercase).collect();
    query.split_whitespace().all(|term| {
        let term = term.to_lowercase();
        words.iter().any(|word| word.starts_with(&term))
    })
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
        // Stacking: the chord can arrive while another overlay owns the
        // focus (Pane Overview, the Attention Queue, the theme picker, the
        // broadcast picker, fleet search, the Launch Pad). None of those is a
        // descendant of a pane, so capturing focus now would hand it back to
        // the overlay before dispatch and leave Split / Close pane without a
        // target. Fold each one first, through its own focus-restoring close
        // where it has one, so the capture below sees the pane the overlay
        // was opened from. A Launch Pad mid-run keeps its modal up (it
        // refuses Escape too), so the palette does not open over it.
        if self.launch_pad.as_ref().is_some_and(|lp| lp.running) {
            return;
        }
        if self.command_palette_blocked_by_modal() {
            return;
        }
        // The four focus-only overlays remembered the pane they were opened
        // from (`overlay_origin_pane`); that pane, not the first leaf, is
        // what the chosen action must land on.
        // Taken BEFORE the closes below: each of those four closers clears
        // `overlay_origin_pane` so a stale pane is never reused, which means
        // a take after them always reads `None`.
        let origin_pane = self.overlay_origin_pane.take().and_then(|p| p.upgrade());
        let mut folded_without_restore = false;
        if self.launch_pad.is_some() {
            self.launch_pad_cancel(cx);
            folded_without_restore = true;
        }
        // Pane Overview and the Attention Queue restore focus themselves,
        // but onto the first leaf (issue #108), so the pane that owns focus
        // after the fold is not the origin; the taken origin wins here too.
        if self.pane_overview.is_some() {
            self.close_pane_overview_and_restore_focus(window, cx);
            folded_without_restore = true;
        }
        if self.attention_queue_open {
            self.close_attention_queue_and_restore_focus(window, cx);
            folded_without_restore = true;
        }
        if self.show_theme_picker {
            self.close_theme_picker(cx);
            folded_without_restore = true;
        }
        if self.broadcast_picker_open {
            self.close_broadcast_picker(cx);
            folded_without_restore = true;
        }
        if self.fleet_search.is_some() {
            self.close_fleet_search(cx);
            folded_without_restore = true;
        }
        // Only a fold consults the recorded origin, and then it outranks
        // whatever pane a restoring close just focused.
        let origin_pane = origin_pane.filter(|_| folded_without_restore);
        self.dismiss_transient_surfaces();
        // Remember where the user was before the palette takes focus:
        // `close_command_palette_and_restore_focus` hands focus back there
        // before the chosen action dispatches, so Close pane / Split / Toggle
        // zoom act on that pane and not on the first leaf. The pane entity is
        // kept beside the raw handle so a pane that leaves the tree while the
        // palette is open is not re-focused (issue #108). An overlay folded
        // above without a restoring close still holds the focus handle of an
        // element that is gone next frame, so that handle is not kept.
        self.command_palette_return_focus = if folded_without_restore {
            None
        } else {
            window.focused(cx)
        };
        // After a fold the taken origin comes first: a restoring close has
        // just put the focus on the first leaf, which is exactly the pane the
        // action must not target. Without a fold `origin_pane` is `None` and
        // the pane owning the focus leads as before.
        self.command_palette_return_pane = origin_pane
            .or_else(|| self.pane_owning_focus(window, cx))
            .or_else(|| self.command_palette_default_pane())
            .map(|pane| pane.downgrade());
        self.command_palette_open = true;
        self.command_palette_query.clear();
        self.command_palette_selected = 0;
        self.command_palette_scroll = gpui::ScrollHandle::new();
        self.command_palette_focus.focus(window, cx);
        cx.notify();
    }

    /// The pane that owns the focus right now: the leaf whose handle is
    /// focused *or contains* the focused element (`contains_focused`), so a
    /// pane whose find bar holds focus still counts as the user's pane.
    /// `LayoutTree::focused_pane` is the exact-match lookup Split and Close
    /// use, which is why the run path re-focuses the pane's own handle before
    /// dispatching. The active workspace's visible tab in the CLI cockpit,
    /// the Review grid in Review mode.
    pub(crate) fn pane_owning_focus(&self, window: &Window, cx: &App) -> Option<Entity<Pane>> {
        let root = if self.mode == paneflow_config::schema::AppMode::Diff {
            self.review.layout.as_ref()?
        } else {
            self.workspaces
                .get(self.active_idx)?
                .active_tab()
                .root
                .as_ref()?
        };
        let mut owner = None;
        root.any_leaf(&mut |pane| {
            if pane.read(cx).focus_handle(cx).contains_focused(window, cx) {
                owner = Some(pane.clone());
                true
            } else {
                false
            }
        });
        owner
    }

    /// A modal dialog keeps the chord inert. Each of these owns the focus
    /// and paints at or above the palette's `with_priority(7)`, so a palette
    /// opened underneath would take the keys while staying invisible and
    /// leave the visible dialog unresponsive. They are dialogs the user asked
    /// for, or a destructive decision, so the palette refuses rather than
    /// folding them. The next dialog registers here:
    ///
    /// - Custom Buttons (`custom_buttons_modal`, priority 8)
    /// - About (`show_about_dialog`, priority 10)
    /// - System Info (`system_info_dialog`, priority 10)
    /// - the modal close-confirm (`pending_close` with `ConfirmStyle::Modal`,
    ///   priority 11); an inline close arm is not a dialog
    /// - Work Review (`work_review`, an occluding focus-owning surface)
    pub(crate) fn command_palette_blocked_by_modal(&self) -> bool {
        self.custom_buttons_modal.is_some()
            || self.show_about_dialog
            || self.system_info_dialog.is_some()
            || self
                .pending_close
                .as_ref()
                .is_some_and(|p| p.style == crate::app::close_guard::ConfirmStyle::Modal)
            || self.work_review.is_some()
    }

    /// Last resort when no pane contains the focus (the sidebar or a folded
    /// overlay had it): the first leaf of the tree the chosen action would
    /// act on, so a pane-targeting command still has a target. Settings owns
    /// the whole window, so nothing is captured there.
    fn command_palette_default_pane(&self) -> Option<Entity<Pane>> {
        if self.settings_section.is_some() {
            return None;
        }
        if self.mode == paneflow_config::schema::AppMode::Diff {
            return self
                .review
                .layout
                .as_ref()
                .and_then(|root| root.first_leaf());
        }
        self.workspaces
            .get(self.active_idx)?
            .active_tab()
            .root
            .as_ref()?
            .first_leaf()
    }

    /// Record the pane a focus-only overlay (theme picker, broadcast picker,
    /// fleet search, Launch Pad) is being opened from, for a command palette
    /// that later folds it. Only a pane that owns the focus right now is
    /// written: a focus-only overlay opened over another focus-only overlay
    /// keeps the pane the first one came from, because the first overlay,
    /// not a pane, owns the focus at that moment.
    pub(crate) fn remember_overlay_origin(&mut self, window: &Window, cx: &App) {
        if let Some(pane) = self.pane_owning_focus(window, cx) {
            self.overlay_origin_pane = Some(pane.downgrade());
        }
    }

    /// Whether `pane` is still a leaf of the tree focus would return to.
    fn command_palette_pane_is_live(&self, pane: &Entity<Pane>) -> bool {
        if self.mode == paneflow_config::schema::AppMode::Diff {
            return self
                .review
                .layout
                .as_ref()
                .is_some_and(|root| root.contains_leaf(pane));
        }
        self.workspaces
            .get(self.active_idx)
            .and_then(|ws| ws.active_tab().root.as_ref())
            .is_some_and(|root| root.contains_leaf(pane))
    }

    pub(crate) fn close_command_palette(&mut self, cx: &mut Context<Self>) {
        if !self.command_palette_open {
            return;
        }
        self.command_palette_open = false;
        self.command_palette_query.clear();
        self.command_palette_selected = 0;
        self.command_palette_return_pane = None;
        self.command_palette_return_focus = None;
        cx.notify();
    }

    /// Cancel (Escape, outside click, the toggle chord): close and put focus
    /// back exactly where it was.
    pub(crate) fn close_command_palette_and_restore_focus(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_command_palette_restoring(false, window, cx);
    }

    /// Close and hand focus back to where the palette was opened from. With
    /// `for_dispatch` the originating pane's own handle is focused (a find bar
    /// inside it held the focus, say), so the action about to dispatch
    /// resolves that pane through the exact-match `focused_pane` lookup;
    /// without it the element that held focus gets it back unchanged. Either
    /// way a pane that left the tree while the palette was open is skipped,
    /// then the fallback is the non-pane element that held focus (dock
    /// editor, sidebar, placeholder), the active workspace's first pane, and
    /// last the empty-workspace placeholder (issue #108: an overlay that
    /// closes with nothing focused leaves every global chord without a
    /// handler).
    fn close_command_palette_restoring(
        &mut self,
        for_dispatch: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let return_pane = self.command_palette_return_pane.take();
        let return_focus = self.command_palette_return_focus.take();
        self.close_command_palette(cx);
        // The weak handle upgrades while the pane entity is alive anywhere
        // (a render closure may hold one for a frame after a close), so the
        // tree membership check below is what decides.
        match return_pane.and_then(|pane| pane.upgrade()) {
            Some(pane) if self.command_palette_pane_is_live(&pane) => {
                match return_focus {
                    Some(handle) if !for_dispatch => window.focus(&handle, cx),
                    _ => pane.read(cx).focus_handle(cx).focus(window, cx),
                }
                return;
            }
            // The pane is gone: fall through to the first-leaf chain rather
            // than re-focus a handle inside it.
            Some(_) => {}
            None => {
                if let Some(handle) = return_focus {
                    window.focus(&handle, cx);
                    return;
                }
            }
        }
        // Review mode has its own grid: its first live diff pane is the
        // fallback there, never the CLI workspace's pane behind it.
        let focused = if self.mode == paneflow_config::schema::AppMode::Diff {
            match self
                .review
                .layout
                .as_ref()
                .and_then(|root| root.first_leaf())
            {
                Some(pane) => {
                    pane.read(cx).focus_handle(cx).focus(window, cx);
                    true
                }
                None => false,
            }
        } else {
            match self.workspaces.get(self.active_idx) {
                Some(ws) => ws.focus_first(window, cx),
                None => false,
            }
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
        self.close_command_palette_restoring(true, window, cx);
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

        // DESIGN.md 7.2: a select list is a `ListBox` of `ListBoxOption`s
        // carrying `aria_selected`, so VoiceOver announces which command
        // Enter will run while focus stays on the palette container.
        let mut list = div()
            .id("command-palette-list")
            .role(gpui::Role::ListBox)
            .aria_label("Commands")
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
                let label = match &entry.shortcut {
                    Some(key) => format!("{}, {key}", entry.description),
                    None => entry.description.clone(),
                };
                list = list.child(
                    select_option(
                        SharedString::from(format!("command-palette-row-{idx}")),
                        is_selected,
                        ui,
                    )
                    .aria_label(label)
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

    /// Whole words: a term must start a word, not merely occur inside one.
    #[test]
    fn a_query_does_not_match_inside_a_word() {
        assert!(!matches_query("Split horizontal", "riz"));
        assert!(matches_query("Split horizontal", "hor spl"));
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
        // The pane that opened the palette is the one the chosen action must
        // land on (Codex P1 on #583): opening captures it, restore prefers it
        // while it is still a leaf, and closing forgets it.
        let palette = include_str!("command_palette.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("production half of the palette module");
        for needle in [
            // Stacking: sibling overlays fold before the capture, and a
            // mid-run Launch Pad keeps the palette closed.
            "if self.launch_pad.as_ref().is_some_and(|lp| lp.running) {",
            // A modal dialog keeps the palette closed.
            "if self.command_palette_blocked_by_modal() {",
            "self.custom_buttons_modal.is_some()",
            "|| self.show_about_dialog",
            "|| self.system_info_dialog.is_some()",
            ".is_some_and(|p| p.style == crate::app::close_guard::ConfirmStyle::Modal)",
            "|| self.work_review.is_some()",
            "self.launch_pad_cancel(cx);",
            "self.close_pane_overview_and_restore_focus(window, cx);",
            "self.close_attention_queue_and_restore_focus(window, cx);",
            "self.close_theme_picker(cx);",
            "self.close_broadcast_picker(cx);",
            "self.close_fleet_search(cx);",
            "let origin_pane = self.overlay_origin_pane.take().and_then(|p| p.upgrade());",
            "let origin_pane = origin_pane.filter(|_| folded_without_restore);",
            "self.command_palette_return_pane = origin_pane",
            ".or_else(|| self.pane_owning_focus(window, cx))",
            ".or_else(|| self.command_palette_default_pane())",
            // DESIGN.md 7.2: the rows are a listbox of options for VoiceOver.
            ".role(gpui::Role::ListBox)",
            "select_option(",
            ".aria_label(label)",
            "self.command_palette_return_focus = if folded_without_restore {",
            "window.focused(cx)",
            ".map(|pane| pane.downgrade());",
            "if pane.read(cx).focus_handle(cx).contains_focused(window, cx) {",
            "match return_pane.and_then(|pane| pane.upgrade()) {",
            "Some(pane) if self.command_palette_pane_is_live(&pane) => {",
            "Some(handle) if !for_dispatch => window.focus(&handle, cx),",
            "_ => pane.read(cx).focus_handle(cx).focus(window, cx),",
            "self.close_command_palette_restoring(true, window, cx);\n        window.dispatch_action(action, cx);",
            "self.command_palette_return_pane = None;",
            "self.command_palette_return_focus = None;",
        ] {
            assert!(
                palette.contains(needle),
                "restore must return focus to the originating pane: missing `{needle}`"
            );
        }
        // The origin is taken BEFORE any closer runs: every closer clears
        // `overlay_origin_pane`, so a take after them always reads `None`.
        let take_at = palette
            .find("let origin_pane = self.overlay_origin_pane.take()")
            .expect("the palette takes the overlay origin");
        for closer in [
            "self.launch_pad_cancel(cx);",
            "self.close_theme_picker(cx);",
            "self.close_broadcast_picker(cx);",
            "self.close_fleet_search(cx);",
        ] {
            let close_at = palette.find(closer).expect(closer);
            assert!(
                take_at < close_at,
                "`{closer}` clears overlay_origin_pane, so the take must come before it"
            );
        }
        // The four focus-only overlays remember the pane they were opened
        // from, at a point where that pane still owns the focus.
        let capture = "self.remember_overlay_origin(window, cx);";
        assert!(
            palette.contains("if let Some(pane) = self.pane_owning_focus(window, cx) {"),
            "remember_overlay_origin must only overwrite the origin when a pane owns focus, \
             so stacked focus-only overlays keep the first origin"
        );
        for (module, src) in [
            ("theme_picker.rs", include_str!("theme_picker.rs")),
            ("broadcast.rs", include_str!("broadcast.rs")),
            ("launch_pad.rs", include_str!("launch_pad.rs")),
            ("pane_overview/mod.rs", include_str!("pane_overview/mod.rs")),
            ("attention_queue.rs", include_str!("attention_queue.rs")),
        ] {
            assert!(
                src.contains(capture),
                "{module} must remember its origin pane for the command palette"
            );
        }
        let main = include_str!("../main.rs");
        assert!(
            main.contains(capture),
            "the deferred fleet-search focus must remember its origin pane first"
        );
        assert!(
            main.contains(".on_action(cx.listener(Self::handle_open_command_palette))"),
            "the render root must handle OpenCommandPalette"
        );
        assert!(
            main.contains("self.render_command_palette(cx)"),
            "the render root must mount the palette overlay"
        );
        // A modal opened over the palette folds it at the next frame.
        let mount = main
            .find("if self.command_palette_open {")
            .map(|at| &main[at..at + 400])
            .expect("the render root gates the palette on command_palette_open");
        assert!(
            mount.contains("if self.command_palette_blocked_by_modal() {")
                && mount.contains("self.close_command_palette(cx);"),
            "a modal dialog opened over the palette must close it at the next frame: {mount}"
        );
    }
}
