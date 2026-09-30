//! Settings lifecycle + persistence + key handlers for `PaneFlowApp`.
//!
//! The settings *UI* - the Codex-style nav rail, the content panel, and the
//! per-section bodies - lives in `crate::settings` (`chrome` + `tabs::*`). This
//! module owns the glue on `PaneFlowApp`:
//! - [`PaneFlowApp::open_settings_window`] / [`PaneFlowApp::close_settings`] -
//!   toggle the embedded settings (set/clear `settings_section`).
//! - [`PaneFlowApp::persist_setting`] - the shared cache-mutate + repaint +
//!   off-thread write used by every settings control.
//! - [`PaneFlowApp::handle_settings_key_down`] - key routing for the
//!   font-picker typeahead and Escape handling.
//! - [`PaneFlowApp::intercept_shortcut_keystroke`] /
//!   [`PaneFlowApp::handle_shortcut_recording`] - the Shortcuts page's
//!   rebind recording and find-by-key capture, fed by the app-wide keystroke
//!   interceptor so a chord is seen before GPUI dispatches it as an action.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use gpui::{Context, Focusable as _, KeyDownEvent, Keystroke, ScrollHandle, Window};

use crate::app::overlay_origin::OverlayKind;
use crate::settings::tabs::terminal::{FontKeyEffect, apply_font_typeahead_key};
use crate::widgets::scrollbar;
use crate::{PaneFlowApp, SettingsSection, config_writer, keybindings};

/// Guard that keeps a settings persist marked in-flight until the off-thread
/// write finishes (or the task is dropped). `Drop` records the generation
/// before decrementing so a tick cannot observe `in_flight == 0` with a stale
/// `last_persist_gen`.
#[must_use]
pub(crate) struct ConfigPersistInFlight {
    persist_gen: u64,
    last_persist_gen: Arc<AtomicU64>,
    in_flight: Arc<AtomicUsize>,
}

impl Drop for ConfigPersistInFlight {
    fn drop(&mut self) {
        self.last_persist_gen
            .fetch_max(self.persist_gen, Ordering::SeqCst);
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Config keys whose value the terminal renderer reads through the font
/// cache (`terminal/element/font.rs`): a change to any of them has to
/// refresh that cache and repaint every terminal, not only the chrome.
pub(crate) fn is_font_block_key(nested: bool, key: &str) -> bool {
    match nested {
        false => matches!(
            key,
            "font_family"
                | "font_size"
                | "font_weight"
                | "font_fallbacks"
                | "line_height"
                | "cell_width"
        ),
        true => key == "ligatures",
    }
}

/// Open settings selects, in the order Escape folds them.
///
/// The Appearance Preset menu is the same kind of select as the Terminal,
/// General, and New-tab menus. It folds before Settings itself closes.
#[derive(Clone, Copy, PartialEq)]
struct SettingsEscapeState {
    section: Option<SettingsSection>,
    theme_dropdown_open: bool,
    terminal_dropdown_open: bool,
    general_dropdown_open: bool,
    new_tab_branch_dropdown_open: bool,
}

impl SettingsEscapeState {
    fn from_app(app: &PaneFlowApp) -> Self {
        Self {
            section: app.settings_section,
            theme_dropdown_open: app.theme_dropdown_open,
            terminal_dropdown_open: app.terminal_dropdown.is_some(),
            general_dropdown_open: app.general_dropdown.is_some(),
            new_tab_branch_dropdown_open: app.new_tab_branch_dropdown.is_some(),
        }
    }

    /// Escape folds the frontmost menu. With none open, Settings closes.
    /// Any other key leaves the menus alone.
    fn dispatch(&mut self, keystroke: &Keystroke) {
        if keystroke.key != "escape" {
            return;
        }
        if self.terminal_dropdown_open {
            self.terminal_dropdown_open = false;
        } else if self.general_dropdown_open {
            self.general_dropdown_open = false;
        } else if self.new_tab_branch_dropdown_open {
            self.new_tab_branch_dropdown_open = false;
        } else if self.theme_dropdown_open {
            self.theme_dropdown_open = false;
        } else {
            self.section = None;
        }
    }

    fn write_back(self, app: &mut PaneFlowApp, cx: &mut Context<PaneFlowApp>) {
        if !self.terminal_dropdown_open {
            app.terminal_dropdown = None;
        }
        if !self.general_dropdown_open {
            app.general_dropdown = None;
        }
        if !self.new_tab_branch_dropdown_open {
            app.new_tab_branch_dropdown = None;
        }
        app.theme_dropdown_open = self.theme_dropdown_open;
        if self.section.is_none() {
            app.close_settings(cx);
        }
    }
}

impl PaneFlowApp {
    /// Open the embedded settings (Codex-style). The macOS menu bar
    /// (PaneFlow ▸ Settings…) routes here; it sets `settings_section`, and
    /// `main.rs` then swaps the left rail for the settings nav and the content
    /// area for the section panel. The name is kept for call-site compatibility
    /// there is no separate settings *window* anymore.
    pub(crate) fn open_settings_window(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_settings_at(SettingsSection::General, window, cx);
    }

    /// Open Settings directly on `section`.
    ///
    /// EP-005 US-017: the preset palette's "Manage presets..." entry lands on
    /// the page that owns the selected preset's source, so the user does not
    /// have to find it again after seeing it in the palette.
    pub(crate) fn open_settings_at(
        &mut self,
        section: SettingsSection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.workspace_menu_open = None;
        // Issue #1096: record the pane Settings opens from while it still
        // owns the focus, so every close hands the focus back to it. A
        // re-open while Settings is up keeps the first origin: whatever holds
        // the focus then (Settings, or a pane a workspace chord focused under
        // it) is not where the user came from.
        if self.settings_section.is_none() {
            self.remember_overlay_origin(OverlayKind::Settings, window, cx);
        }
        self.settings_section = Some(section);
        self.reset_settings_scroll();
        self.terminal_dropdown = None;
        self.general_dropdown = None;
        self.new_tab_branch_dropdown = None;
        self.font_dropdown_open = false;
        self.font_search.clear();
        self.theme_dropdown_open = false;
        // Clear any stale nav search so the landing row is always visible (a
        // leftover query could filter the nav to a section that doesn't match
        // the displayed page).
        self.clear_settings_search(cx);
        if section == SettingsSection::Shortcuts {
            // The page is virtualized, so its rows have to exist before the
            // first frame renders. `select_settings_section` does the same for
            // the nav path; this is the deep-link one.
            self.rebuild_shortcut_rows(cx);
        }
        self.settings_focus.focus(window, cx);
        cx.notify();
    }

    /// Arm or disarm the Shortcuts page's key-capture mode.
    ///
    /// Arming clears the field so the next chord lands in an empty box; the
    /// captured chord is then just the field's value, which keeps a single
    /// visible filter state instead of a hidden second one.
    ///
    /// Disarming deliberately leaves the field alone. Clicking a row to rebind
    /// disarms capture, and wiping the query there would drop the filter that
    /// put the row on screen - re-collapsing its section and scrolling the
    /// now-armed row out of sight.
    pub(crate) fn set_shortcut_capture(&mut self, active: bool, cx: &mut Context<Self>) {
        let changed = self.shortcut_capture_active != active;
        self.shortcut_capture_active = active;
        if active {
            // Clearing the field notifies, and the observer on it rebuilds the
            // filtered rows - no explicit rebuild needed on this arm.
            self.shortcut_search_input.update(cx, |input, cx| {
                input.clear(cx);
            });
            self.recording_shortcut_idx = None;
        } else if changed {
            // Leaving capture flips the match rule from "this exact chord" back
            // to substring, so the visible rows change while the query does not
            // - nothing else would tell the page to re-filter. Guarded on
            // `changed` because every row click disarms capture on the way to
            // recording, and an unconditional rebuild would re-seed the list
            // (and scroll it) under the row the user just clicked.
            self.rebuild_shortcut_rows(cx);
        }
    }

    /// Clear the Shortcuts-page filter and leave capture mode. The explicit
    /// "start over" action, unlike [`Self::set_shortcut_capture`].
    pub(crate) fn clear_shortcut_filters(&mut self, cx: &mut Context<Self>) {
        self.shortcut_capture_active = false;
        self.shortcut_search_input.update(cx, |input, cx| {
            input.clear(cx);
        });
        // Both halves of the filter moved at once, and the field's observer
        // only knows about one of them.
        self.rebuild_shortcut_rows(cx);
    }

    /// The single way out of Settings: Escape, the Back control, the search
    /// fields' Escape and the sidebar toggle all land here.
    pub(crate) fn close_settings(&mut self, cx: &mut Context<Self>) {
        // Issue #1096: Settings may hold the focus, and its node unmounts on
        // the next frame. This path has no `Window`, so it only owes the
        // drain a return (`return_focus_from_settings`), which lands before
        // that frame paints. A call while Settings is already closed owes
        // nothing: queuing then would pull the focus out of whatever holds it.
        if self.settings_section.is_some() {
            self.pending_settings_return = true;
            cx.notify();
        }
        self.settings_section = None;
        // Shortcuts-page ephemeral state. The armed "Reset" confirmation is the
        // one that matters: left standing across a close, it would turn a
        // stray click on reopen into "every binding erased, no undo".
        self.clear_shortcut_filters(cx);
        self.collapsed_shortcut_groups.clear();
        self.set_shortcut_reset_arm(None, cx);
        // A thumb drag released off the list never reached the list's
        // `on_mouse_up`. Ending it here, through the handle so the list's
        // lazy measurement unfreezes, is what keeps a reopened page from
        // scrolling under a bare hover.
        scrollbar::end_drag(&self.shortcut_list, self.shortcut_drag.take());
        self.font_dropdown_open = false;
        self.font_search.clear();
        self.theme_dropdown_open = false;
        self.terminal_dropdown = None;
        self.general_dropdown = None;
        self.new_tab_branch_dropdown = None;
        self.clear_settings_search(cx);
        if self.recording_shortcut_idx.is_some() {
            self.recording_shortcut_idx = None;
            let config = paneflow_config::loader::load_config();
            keybindings::apply_keybindings(cx, &config.shortcuts);
        }
    }

    /// Drain half of [`Self::close_settings`] (issue #1096), run by
    /// `drain_pending_window_actions` before the frame that unmounts Settings.
    ///
    /// The focus goes back to the pane Settings was opened from, then to the
    /// pane the focus would return to anyway (the active Review pane in
    /// Review, else the active tab's first pane), then to the empty-workspace
    /// placeholder. Only while Settings still holds the focus (read from the
    /// last frame, which still contains it), the placeholder holds it, or
    /// nothing does: a surface that took the focus over Settings (Pane
    /// Overview, About, the sessions rail) keeps it. The origin is taken either way so it never outlives the
    /// close.
    pub(crate) fn return_focus_from_settings(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.settings_section.is_some() {
            // Re-opened before the drain ran: Settings keeps the focus and
            // its origin.
            return;
        }
        let target = self.take_overlay_return_pane(OverlayKind::Settings);
        // The app root tracks the placeholder handle, so a click on the
        // Settings rail (the Back control, the title-bar toggle) lands the
        // focus there before the close runs: that is Settings' own focus too.
        let settings_holds_focus = self.settings_focus.contains_focused(window, cx)
            || self.empty_workspace_focus.is_focused(window)
            || window.focused(cx).is_none();
        if !settings_holds_focus {
            return;
        }
        match target {
            Some(pane) => pane.read(cx).focus_handle(cx).focus(window, cx),
            None => window.focus(&self.empty_workspace_focus, cx),
        }
    }

    /// Drop stale scroll geometry when the settings surface is remounted,
    /// changes page, or the window is resized. GPUI repopulates the handle from
    /// the next `track_scroll` layout pass.
    pub(crate) fn reset_settings_scroll(&mut self) {
        self.settings_scroll = ScrollHandle::new();
        self.settings_drag = None;
        // The Shortcuts list keeps its `ListState` across remounts, so its
        // drag has to be ended, not just dropped (see `close_settings`).
        scrollbar::end_drag(&self.shortcut_list, self.shortcut_drag.take());
    }

    /// Reset the nav search box. Shared by open/close so a reopened settings
    /// page always shows the full, unfiltered section list.
    fn clear_settings_search(&mut self, cx: &mut Context<Self>) {
        self.settings_search_input.update(cx, |inp, cx| {
            inp.clear(cx);
        });
    }

    /// Apply a settings-control change. Mutates the render cache in memory for
    /// instant feedback, repaints, then persists the field to disk off the GPUI
    /// main thread (`smol::unblock`). `nested` routes into the `terminal` block;
    /// a `Null` value clears the field.
    ///
    /// Bumps the persist generation before spawn so a ConfigWatcher reload of
    /// write N cannot replace in-memory write N+1. Failed writes keep the
    /// in-memory mutate and toast.
    pub(crate) fn persist_setting(
        &mut self,
        nested: bool,
        key: &'static str,
        value: serde_json::Value,
        cx: &mut Context<Self>,
    ) {
        let default_shell_changed = !nested
            && key == "default_shell"
            && normalized_shell_setting(self.cached_config.default_shell.as_deref())
                != normalized_shell_setting(value.as_str());
        // Issue #300: a value the typed config cannot hold must not reach
        // disk either, or the render cache and `paneflow.json` diverge.
        match config_writer::with_field(&self.cached_config, nested, key, value.clone()) {
            Ok(next) => self.cached_config = next,
            Err(_) => {
                self.show_toast(format!("Could not apply setting: {key}"), cx);
                return;
            }
        }
        config_writer::publish_config_snapshot(cx, &self.cached_config);
        if !nested && matches!(key, "macos_chrome_material") {
            for ws in &self.workspaces {
                ws.propagate_config(&self.cached_config, cx);
            }
        }
        if nested
            && matches!(
                key,
                "integrated_glyphs" | "color_emoji" | "cursor_color" | "minimum_contrast"
            )
        {
            for ws in &self.workspaces {
                ws.propagate_config(&self.cached_config, cx);
            }
        }
        if is_font_block_key(nested, key) {
            // The font block is read by the render thread from a 500 ms
            // disk-backed cache, and every terminal is hosted behind
            // `Entity::cached` (#429): resolve the new block from the
            // in-memory config now, then notify every view so idle panes
            // re-shape instead of replaying their old glyphs.
            crate::terminal::element::refresh_font_config(&self.cached_config);
            for ws in &self.workspaces {
                ws.propagate_config(&self.cached_config, cx);
            }
        }
        if !nested && key == "reduce_motion" {
            crate::ui_primitives::set_reduce_motion(self.cached_config.reduce_motion_enabled());
        }
        if !nested && key == "ai_unrestricted" {
            // Issue #283: `system.capabilities` reads this mirror on the
            // socket thread; flip it with the toggle, not at the next reload.
            crate::ipc::set_ai_unrestricted(self.cached_config.ai_unrestricted_enabled());
        }
        if default_shell_changed {
            self.handle_default_shell_changed(cx);
        }
        cx.notify();
        // Issue #242: `value` is captured at spawn time, so gate the write on
        // this field's generation under the config-write lock; an older task
        // that acquires the lock last must not publish its stale value.
        let scope = if nested {
            config_writer::FieldScope::Terminal
        } else {
            config_writer::FieldScope::TopLevel
        };
        let seq = self.config_field_persist_seq.bump(scope, key);
        let seqs = Arc::clone(&self.config_field_persist_seq);
        let flight = self.begin_config_persist();
        cx.spawn(async move |this, cx| {
            let ok = smol::unblock(move || {
                if nested {
                    config_writer::save_terminal_field_checked(key, value, &seqs, seq)
                } else {
                    config_writer::save_config_value_checked(key, value, &seqs, seq)
                }
            })
            .await;
            drop(flight);
            if !ok {
                log::warn!(
                    "settings: failed to persist {key}; choice is in-memory only this session"
                );
                let _ = this.update(cx, |this, cx| {
                    this.show_toast(format!("Could not save setting: {key}"), cx);
                });
            }
        })
        .detach();
    }

    /// Mark a settings persist in-flight and assign its generation. Call
    /// immediately before spawning the off-thread write.
    pub(crate) fn begin_config_persist(&self) -> ConfigPersistInFlight {
        self.config_persist_in_flight.fetch_add(1, Ordering::SeqCst);
        let persist_gen = self.config_persist_seq.fetch_add(1, Ordering::SeqCst) + 1;
        ConfigPersistInFlight {
            persist_gen,
            last_persist_gen: Arc::clone(&self.config_last_persist_gen),
            in_flight: Arc::clone(&self.config_persist_in_flight),
        }
    }

    /// The shell only binds when a PTY spawns, so a live terminal keeps the
    /// one it was started with. Say so rather than restarting anything under
    /// the user: a running session is work in progress.
    pub(crate) fn handle_default_shell_changed(&mut self, cx: &mut Context<Self>) {
        self.show_toast("Shell updated. New terminals will use it.", cx);
    }

    /// Apply an Agents-panel-scoped settings change. This keeps
    /// `agent_panel` writes as narrow read-modify-writes so profile settings
    /// and future sibling fields survive notification toggles.
    pub(crate) fn persist_agent_panel_setting(
        &mut self,
        key: &'static str,
        value: serde_json::Value,
        cx: &mut Context<Self>,
    ) {
        match config_writer::with_agent_panel_field(&self.cached_config, key, value.clone()) {
            Ok(next) => self.cached_config = next,
            Err(_) => {
                self.show_toast(format!("Could not apply agent panel setting: {key}"), cx);
                return;
            }
        }
        config_writer::publish_config_snapshot(cx, &self.cached_config);
        cx.notify();
        let seq = self
            .config_field_persist_seq
            .bump(config_writer::FieldScope::AgentPanel, key);
        let seqs = Arc::clone(&self.config_field_persist_seq);
        let flight = self.begin_config_persist();
        cx.spawn(async move |this, cx| {
            let ok = smol::unblock(move || {
                config_writer::save_agent_panel_field_checked(key, value, &seqs, seq)
            })
            .await;
            drop(flight);
            if !ok {
                log::warn!(
                    "settings: failed to persist agent_panel.{key}; choice is in-memory only this session"
                );
                let _ = this.update(cx, |this, cx| {
                    this.show_toast(format!("Could not save agent panel setting: {key}"), cx);
                });
            }
        })
        .detach();
    }

    pub(crate) fn handle_settings_key_down(
        &mut self,
        event: &KeyDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Font dropdown typeahead (Terminal page). Enter commits the first
        // matching row; control characters (Enter's "\n", Tab's "\t") are
        // never inserted into the query.
        if self.font_dropdown_open {
            let default_font = crate::terminal::element::resolve_font_family(None);
            let effect = apply_font_typeahead_key(
                &mut self.font_search,
                &mut self.font_dropdown_open,
                &event.keystroke,
                &default_font,
                &self.mono_font_names,
            );
            match effect {
                FontKeyEffect::Ignored => {}
                FontKeyEffect::Redraw => cx.notify(),
                FontKeyEffect::Select(commit) => {
                    self.persist_setting(false, "font_family", commit.config_value(), cx);
                }
            }
            return;
        }

        // Escape folds an open settings select first. The Appearance Preset
        // menu is one of those selects; with none open, Escape closes
        // Settings. Shortcut recording and key capture consume Escape
        // upstream, before GPUI matches any binding.
        if event.keystroke.key == "escape" && self.recording_shortcut_idx.is_none() {
            let mut menus = SettingsEscapeState::from_app(self);
            menus.dispatch(&event.keystroke);
            menus.write_back(self, cx);
            cx.notify();
        }
    }

    /// App-wide keystroke interceptor for the Shortcuts settings page.
    ///
    /// Registered through `App::intercept_keystrokes` (in
    /// `main.rs::mount_paneflow_app`), which is the *only* hook that runs
    /// before GPUI matches a key binding. An `on_key_down` (or even
    /// `capture_key_down`) listener is too late: when a binding matches and its
    /// action is handled, `dispatch_key_event` returns without ever calling
    /// `finish_dispatch_key_event`, so no key listener fires at all. That is
    /// why pressing Cmd+Q to search for - or rebind - the Quit shortcut used to
    /// quit the app instead, and why recording Cmd+Shift+D split the pane.
    ///
    /// Returns `true` when the chord was consumed, in which case the caller
    /// must call `cx.stop_propagation()` to suppress the action.
    pub(crate) fn intercept_shortcut_keystroke(
        &mut self,
        keystroke: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.settings_section != Some(SettingsSection::Shortcuts) {
            return false;
        }
        // Modifiers held on the way to a real chord are not a chord.
        if keybindings::is_bare_modifier(keystroke) {
            return false;
        }

        let search_focused = self
            .shortcut_search_input
            .read(cx)
            .focus_handle
            .is_focused(window);
        match route_shortcut_keystroke(
            search_focused,
            self.recording_shortcut_idx.is_some(),
            self.shortcut_capture_active,
        ) {
            ShortcutKeyRoute::Pass => {
                if search_focused {
                    // The field took focus under an armed row or live
                    // capture: the user clicked into it to type, so both
                    // stand down instead of eating the letters. Capture
                    // leaves through `set_shortcut_capture` so the match
                    // rule flips back to substring for what is about to be
                    // typed.
                    self.disarm_shortcut_recording(cx);
                    self.set_shortcut_capture(false, cx);
                }
                false
            }
            // Rebind recording takes precedence: the row is already armed.
            ShortcutKeyRoute::Record => {
                self.handle_shortcut_recording(keystroke, window, cx);
                cx.notify();
                true
            }
            ShortcutKeyRoute::Capture => {
                if keystroke.key == "escape" {
                    self.set_shortcut_capture(false, cx);
                    cx.notify();
                    return true;
                }

                // The chord goes straight into the search field: seeing what
                // was pressed is the whole point, and it keeps one visible
                // filter state rather than a hidden second one.
                // `format_keystroke` expects the `-`-separated spelling,
                // which is what `recorded_shortcut_key` gives.
                let formatted = keybindings::format_keystroke(&recorded_shortcut_key(keystroke));
                self.shortcut_search_input.update(cx, |input, cx| {
                    input.set_value(formatted, cx);
                });
                cx.notify();
                true
            }
        }
    }

    /// Record `keystroke` as the new binding of the armed row.
    ///
    /// Reached only through [`Self::intercept_shortcut_keystroke`], so the
    /// chord arrives before GPUI could have dispatched it as an action.
    pub(crate) fn handle_shortcut_recording(
        &mut self,
        keystroke: &Keystroke,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(idx) = self.recording_shortcut_idx else {
            return;
        };

        // Ignore bare modifier presses (Shift alone, Ctrl alone, etc.)
        if keybindings::is_bare_modifier(keystroke) {
            return;
        }

        // Escape cancels recording.
        if keystroke.key == "escape" {
            self.recording_shortcut_idx = None;
            cx.notify();
            return;
        }

        // Resolve the action by the row's stable identity, NOT by indexing
        // `DEFAULTS` (the displayed list chains macOS-only defaults, skips
        // unbound rows, and appends user-only actions, so a positional index
        // would rebind the wrong action and corrupt `paneflow.json`).
        let Some(action_name) = self
            .effective_shortcuts
            .get(idx)
            .filter(|entry| !entry.fixed)
            .map(|e| e.action_name)
        else {
            self.recording_shortcut_idx = None;
            cx.notify();
            return;
        };

        // Format keystroke to a GPUI string (e.g. "ctrl-shift-d") and save it.
        // The write is synchronous but still goes through the persist guard,
        // so a ConfigWatcher deposit stamped before it cannot be applied over
        // the config reloaded just below.
        let new_key = recorded_shortcut_key(keystroke);
        // Issue #196: name the chord's current owner BEFORE the save erases
        // the evidence - `merge_shortcut` evicts a user entry on the same
        // physical chord and `apply_keybindings` drops the matching default,
        // both silently. Warn-and-proceed: the rebind still lands.
        let displaced = keybindings::displaced_action_description(
            &paneflow_config::loader::load_config().shortcuts,
            &new_key,
            action_name,
        );
        let flight = self.begin_config_persist();
        let saved = config_writer::save_shortcut_checked(&new_key, action_name);
        drop(flight);
        if !saved {
            self.recording_shortcut_idx = None;
            self.show_toast("Could not save shortcut", cx);
            cx.notify();
            return;
        }

        // Re-apply keybindings from the updated config.
        let config = paneflow_config::loader::load_config();
        keybindings::apply_keybindings(cx, &config.shortcuts);
        self.effective_shortcuts = keybindings::settings_shortcuts(&config.shortcuts);
        self.recording_shortcut_idx = None;
        // The rows carry indices into `effective_shortcuts` and render its key
        // text, so they are stale the moment it is replaced.
        self.rebuild_shortcut_rows(cx);
        if let Some(displaced) = displaced {
            self.show_toast(
                format!(
                    "{} taken from {displaced}",
                    keybindings::format_keystroke(&new_key)
                ),
                cx,
            );
        }
        cx.notify();
    }
}

impl PaneFlowApp {
    /// Stand an armed rebind row down without recording anything. The row's
    /// own Escape, a click that lands anywhere else, and the search field
    /// taking focus all end here.
    pub(crate) fn disarm_shortcut_recording(&mut self, cx: &mut Context<Self>) {
        if self.recording_shortcut_idx.take().is_some() {
            cx.notify();
        }
    }
}

/// Where an intercepted chord goes on the Shortcuts page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ShortcutKeyRoute {
    /// Not the page's: let GPUI dispatch it to the focused field or as an
    /// action.
    Pass,
    /// The armed row records it as its new binding.
    Record,
    /// Capture mode writes it into the search field as the filter.
    Capture,
}

/// Decide what [`PaneFlowApp::intercept_shortcut_keystroke`] does with a
/// chord, pure so the rule is testable without a `Window`.
///
/// Recording wins over capture (arming a row disarms capture, so both being
/// set is transient). A focused search field wins over both: the interceptor
/// runs before GPUI can deliver the key to the field, so consuming there
/// meant a user who clicked into the field under an armed row typed a letter
/// and rebound the row to it instead.
pub(crate) fn route_shortcut_keystroke(
    search_focused: bool,
    recording: bool,
    capture_active: bool,
) -> ShortcutKeyRoute {
    if search_focused {
        ShortcutKeyRoute::Pass
    } else if recording {
        ShortcutKeyRoute::Record
    } else if capture_active {
        ShortcutKeyRoute::Capture
    } else {
        ShortcutKeyRoute::Pass
    }
}

fn normalized_shell_setting(shell: Option<&str>) -> &str {
    shell.map(str::trim).filter(|s| !s.is_empty()).unwrap_or("")
}

/// Serialize a captured keystroke into the chord syntax that `paneflow.json` and
/// [`crate::keybindings::apply`] expect.
///
/// MUST be `unparse()`, never `to_string()`: GPUI's `Display` renders macOS HIG
/// glyphs (`^`, `⌥`, `⌘`), so `to_string()` recorded Cmd+Shift+D as the literal
/// `"⌘⇧D"`. Nothing validates this string on the way to disk, and `apply.rs`
/// suppresses the matching default by ACTION NAME - so the override registered a
/// chord no event can ever produce, the real default was dropped, and the action
/// went permanently dead while Settings still rendered the row as bound.
///
/// Extracted from [`PaneFlowApp::handle_shortcut_recording`] so the round trip is
/// testable without a `Window`.
pub(crate) fn recorded_shortcut_key(keystroke: &Keystroke) -> String {
    keystroke.unparse()
}

#[cfg(test)]
mod tests {
    #[test]
    fn every_font_cache_key_is_a_font_block_key() {
        for key in [
            "font_family",
            "font_size",
            "font_weight",
            "font_fallbacks",
            "line_height",
            "cell_width",
        ] {
            assert!(super::is_font_block_key(false, key), "{key}");
        }
        assert!(super::is_font_block_key(true, "ligatures"));
        assert!(!super::is_font_block_key(false, "ligatures"));
        assert!(!super::is_font_block_key(true, "font_size"));
        assert!(!super::is_font_block_key(false, "theme"));
    }

    use super::{
        SettingsEscapeState, ShortcutKeyRoute, recorded_shortcut_key, route_shortcut_keystroke,
    };
    use crate::SettingsSection;
    use gpui::Keystroke;

    /// The text of `name`'s body: from its `fn` line to `end`.
    fn body<'a>(src: &'a str, name: &str, end: &str) -> &'a str {
        let start = src
            .find(name)
            .unwrap_or_else(|| panic!("{name} must exist"));
        let rest = &src[start..];
        let stop = rest
            .find(end)
            .unwrap_or_else(|| panic!("{end} must follow {name}"));
        &rest[..stop]
    }

    /// Item 2 of the Phase 4 audit. The interceptor runs before GPUI can hand
    /// a key to the focused field, so an armed row (or live capture) ate the
    /// letters a user typed after clicking into the search box. A focused
    /// search field is the field's, whatever else is armed.
    #[test]
    fn interceptor_leaves_every_chord_to_a_focused_search_field() {
        for (recording, capture) in [(true, false), (false, true), (true, true)] {
            assert_eq!(
                route_shortcut_keystroke(true, recording, capture),
                ShortcutKeyRoute::Pass,
                "recording={recording} capture={capture}: a focused search field must get the key"
            );
        }
        // Away from the field the page's modes apply, recording first.
        assert_eq!(
            route_shortcut_keystroke(false, true, false),
            ShortcutKeyRoute::Record
        );
        assert_eq!(
            route_shortcut_keystroke(false, true, true),
            ShortcutKeyRoute::Record,
            "an armed row outranks capture; arming one disarms the other anyway"
        );
        assert_eq!(
            route_shortcut_keystroke(false, false, true),
            ShortcutKeyRoute::Capture
        );
        assert_eq!(
            route_shortcut_keystroke(false, false, false),
            ShortcutKeyRoute::Pass
        );
    }

    /// The route is what the interceptor actually consults - a second `if`
    /// chain next to it would be the drift this test exists to catch.
    #[test]
    fn interceptor_consults_the_route_and_disarms_for_a_focused_field() {
        let src = include_str!("settings.rs");
        let interceptor = body(
            src,
            "fn intercept_shortcut_keystroke(",
            "/// Record `keystroke` as the new binding",
        );
        assert!(
            interceptor.contains(".is_focused(window)"),
            "the interceptor must ask whether the search field holds focus: {interceptor}"
        );
        assert!(
            interceptor.contains("route_shortcut_keystroke("),
            "the interceptor must route through the tested decision: {interceptor}"
        );
        assert!(
            interceptor.contains("self.disarm_shortcut_recording(cx)"),
            "a chord passed to the focused field must also stand the armed row down: {interceptor}"
        );
    }

    /// Item 3 of the Phase 4 audit: a scrollbar-thumb drag released off the
    /// list never reached the list's `on_mouse_up`, so `shortcut_drag` stayed
    /// set across a close and a reopen. Both settle points end it, the way
    /// `reset_settings_scroll` already ends the sibling `settings_drag`.
    #[test]
    fn closing_or_remounting_settings_ends_a_stale_shortcut_thumb_drag() {
        let src = include_str!("settings.rs");
        let close = body(src, "fn close_settings(", "/// Drop stale scroll geometry");
        assert!(
            close.contains("shortcut_drag"),
            "close_settings must end a shortcut-list thumb drag: {close}"
        );
        let remount = body(
            src,
            "fn reset_settings_scroll(",
            "/// Reset the nav search box",
        );
        assert!(
            remount.contains("shortcut_drag"),
            "reset_settings_scroll must end a shortcut-list thumb drag: {remount}"
        );
        for site in [close, remount] {
            assert!(
                site.contains(
                    "scrollbar::end_drag(&self.shortcut_list, self.shortcut_drag.take())"
                ),
                "the drag must be ended through the handle so the list's lazy \
                 measurement unfreezes, not merely cleared: {site}"
            );
        }
    }

    #[test]
    fn recorded_shortcut_key_round_trips_through_keystroke_parse() {
        for chord in [
            "cmd-shift-d",
            "ctrl-shift-f",
            "alt-left",
            "cmd-1",
            "cmd-alt-t",
            "f2",
        ] {
            let original = Keystroke::parse(chord).expect("chord parses");
            let recorded = recorded_shortcut_key(&original);

            // `to_string()` would emit HIG glyphs (`⌘`, `⌥`, `^`) here, which are
            // non-ASCII and which `Keystroke::parse` cannot read back.
            assert!(
                recorded.is_ascii(),
                "`{chord}` was recorded as `{recorded}`, which is not ASCII chord \
                 syntax - `paneflow.json` would receive an unparseable key"
            );

            let reparsed = Keystroke::parse(&recorded)
                .unwrap_or_else(|_| panic!("recorded chord `{recorded}` must re-parse"));
            assert_eq!(
                reparsed.modifiers, original.modifiers,
                "`{chord}` lost modifiers through the record -> parse round trip (`{recorded}`)"
            );
            assert_eq!(
                reparsed.key, original.key,
                "`{chord}` lost its key through the record -> parse round trip (`{recorded}`)"
            );
        }
    }

    /// Issue #915: Escape with the Appearance Preset menu open folds that
    /// menu and leaves Settings on the same page. A second Escape, with no
    /// menu open, closes Settings.
    #[test]
    fn escape_closes_the_theme_preset_menu_before_settings() {
        let mut settings = SettingsEscapeState {
            section: Some(SettingsSection::Appearance),
            theme_dropdown_open: true,
            terminal_dropdown_open: false,
            general_dropdown_open: false,
            new_tab_branch_dropdown_open: false,
        };
        let escape = Keystroke::parse("escape").expect("escape parses");

        settings.dispatch(&escape);

        assert!(
            !settings.theme_dropdown_open,
            "escape must fold the open Preset menu"
        );
        assert!(
            settings.section == Some(SettingsSection::Appearance),
            "settings stays open"
        );

        settings.dispatch(&escape);
        assert!(
            settings.section.is_none(),
            "escape with no menu open closes settings"
        );
    }

    /// Issue #1096: the return-focus restore hangs off `close_settings`, so
    /// it covers every way out only while `close_settings` is the one place
    /// that clears `settings_section`. A second writer would close Settings
    /// with the focus stranded inside it. Every source file under `src/` is
    /// scanned whole (several have early test modules followed by production
    /// code), and the needles are built with `concat!` so this test does not
    /// match its own text.
    #[test]
    fn close_settings_is_the_only_settings_exit() {
        fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for entry in std::fs::read_dir(dir).expect("readable source dir") {
                let path = entry.expect("dir entry").path();
                if path.is_dir() {
                    walk(&path, out);
                } else if path.extension().is_some_and(|ext| ext == "rs") {
                    out.push(path);
                }
            }
        }
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        assert!(root.is_dir(), "no source root at {}", root.display());
        let mut files = Vec::new();
        walk(&root, &mut files);
        assert!(
            files.len() > 100
                && files.iter().any(|f| f.ends_with("app/settings.rs"))
                && files.iter().any(|f| f.ends_with("main.rs")),
            "the walk must read the app sources under {} (read {})",
            root.display(),
            files.len()
        );

        let clear = concat!("settings_section", " = None");
        let needles = [
            clear,
            concat!("settings_section", " = Default::default()"),
            concat!("settings_section", ".take()"),
            concat!("settings_section", ".replace("),
            // `std::mem::take`, `mem::replace` and `Option::take` all borrow it.
            concat!("&mut self.", "settings_section"),
            concat!("&mut app.", "settings_section"),
        ];
        let mut writers = Vec::new();
        for file in &files {
            let src = std::fs::read_to_string(file).expect("readable source");
            for needle in needles {
                let count = src.matches(needle).count();
                if count > 0 {
                    let rel = file.strip_prefix(&root).unwrap_or(file).to_owned();
                    writers.push((rel, needle, count));
                }
            }
        }
        assert_eq!(
            writers,
            vec![(std::path::PathBuf::from("app/settings.rs"), clear, 1)],
            "Settings must close only through close_settings"
        );

        let settings = include_str!("settings.rs");
        let close = body(
            settings,
            "fn close_settings(",
            "/// Drain half of [`Self::close_settings`]",
        );
        assert!(close.contains(clear));
        assert!(
            close.contains("if self.settings_section.is_some() {")
                && close.contains("self.pending_settings_return = true;"),
            "close_settings must owe the drain a return only when Settings was open: {close}"
        );
        let open = body(
            settings,
            "fn open_settings_at(",
            "/// Arm or disarm the Shortcuts page",
        );
        assert!(
            open.contains("self.remember_overlay_origin(OverlayKind::Settings, window, cx);"),
            "open_settings_at must record the pane Settings opens from: {open}"
        );
        let main = include_str!("../main.rs");
        let drain = body(
            main,
            "fn drain_pending_window_actions(",
            "self.prune_stale_split_palette(cx);",
        );
        let panes = drain
            .find("self.pending_pane_focus.take()")
            .expect("the drain focuses a queued pane");
        let settings_return = drain
            .find("self.return_focus_from_settings(window, cx);")
            .expect("the drain runs the Settings return");
        assert!(
            panes < settings_return,
            "a queued pane must take the focus before the Settings return checks it"
        );
    }

    type PaneEntity = gpui::Entity<crate::pane::Pane>;
    type App = gpui::Entity<crate::PaneFlowApp>;

    fn make_pane(cx: &mut gpui::VisualTestContext) -> PaneEntity {
        use gpui::AppContext as _;
        let terminal = cx.new(|cx| crate::terminal::TerminalView::display_only_for_test(1, cx));
        cx.new(|cx| crate::pane::Pane::new(terminal, 1, cx))
    }

    /// Paint one frame. The focus-lost fallback only runs at the end of a
    /// draw, so every Settings transition has to be drawn to be real.
    fn draw(cx: &mut gpui::VisualTestContext) {
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.run_until_parked();
    }

    fn is_focused(pane: &PaneEntity, cx: &mut gpui::VisualTestContext) -> bool {
        use gpui::Focusable as _;
        let pane = pane.clone();
        cx.update(|window, cx| pane.read(cx).focus_handle(cx).is_focused(window))
    }

    fn handle_focused(
        app: &App,
        cx: &mut gpui::VisualTestContext,
        handle: impl Fn(&crate::PaneFlowApp) -> &gpui::FocusHandle,
    ) -> bool {
        let handle = app.read_with(cx, |app, _| handle(app).clone());
        cx.update(|window, _| handle.is_focused(window))
    }

    /// A staged restore makes `save_session` return before it resolves the
    /// session path, so the sidebar toggle below cannot write the developer's
    /// `session-dev.json` (or leave a debounced write running off-thread).
    fn hold_session_saves(app: &mut crate::PaneFlowApp) {
        app.session_restore = crate::app::session::PendingSessionRestore::from_session(
            paneflow_config::schema::SessionState {
                version: paneflow_config::schema::SESSION_SCHEMA_VERSION,
                active_workspace: 0,
                workspaces: vec![paneflow_config::schema::WorkspaceSession {
                    title: "hold".into(),
                    cwd: "/tmp/paneflow-session-hold".into(),
                    tabs: vec![paneflow_config::schema::TabSession::empty()],
                    active_tab: 0,
                    legacy_layout: None,
                    legacy_empty: false,
                    pinned: false,
                    sidebar_collapsed: false,
                    muted: false,
                }],
                mode: paneflow_config::schema::AppMode::Cli,
                review_layout: None,
                review_collapsed: Vec::new(),
                primary_sidebar_collapsed: false,
            },
        );
        assert!(
            app.session_restore.is_some(),
            "session saves must stay skipped"
        );
    }

    /// The whole app in a window, with the same focus-lost fallback and
    /// pending-focus drain `mount_paneflow_app` registers, over one
    /// workspace split into panes A and B. B holds the focus.
    fn split_app(
        cx: &mut gpui::TestAppContext,
    ) -> (App, PaneEntity, PaneEntity, &mut gpui::VisualTestContext) {
        use gpui::Focusable as _;
        let (app, cx) = cx.add_window_view(|_window, cx| {
            let mut app = crate::app::test_support::blank_paneflow_app(cx);
            hold_session_saves(&mut app);
            app
        });
        let a = make_pane(cx);
        let b = make_pane(cx);
        let root = crate::layout::LayoutTree::from_panes_equal(
            crate::layout::SplitDirection::Vertical,
            vec![a.clone(), b.clone()],
        )
        .expect("two panes make a split");
        cx.update(|window, cx| {
            app.update(cx, |app, cx| {
                crate::register_focus_lost_fallback(window, cx, |app| &app.empty_workspace_focus);
                let entity = cx.entity();
                cx.observe_in(&entity, window, |this, _app, window, cx| {
                    this.drain_pending_window_actions(window, cx);
                })
                .detach();
                app.workspaces = vec![crate::workspace::Workspace::with_layout_and_id(
                    1,
                    "split",
                    std::path::PathBuf::new(),
                    root,
                )];
                app.active_idx = 0;
            });
            b.read(cx).focus_handle(cx).focus(window, cx);
        });
        draw(cx);
        assert!(is_focused(&b, cx), "the second pane starts focused");
        (app, a, b, cx)
    }

    fn open_settings(app: &App, cx: &mut gpui::VisualTestContext) {
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_settings_window(window, cx));
        });
        draw(cx);
        assert!(
            handle_focused(app, cx, |app| &app.settings_focus),
            "Settings takes the focus while it is open"
        );
    }

    fn assert_closed_onto(
        app: &App,
        pane: &PaneEntity,
        how: &str,
        cx: &mut gpui::VisualTestContext,
    ) {
        assert!(
            app.read_with(cx, |app, _| app.settings_section.is_none()),
            "{how} closes Settings"
        );
        assert!(
            !handle_focused(app, cx, |app| &app.empty_workspace_focus),
            "{how}: the focus must not be parked on the empty-workspace placeholder"
        );
        assert!(
            is_focused(pane, cx),
            "{how}: the terminal pane regains focus"
        );
    }

    /// Issue #1096: opening Settings from the second pane of a split moves
    /// the focus onto `settings_focus`; every way out (Escape, the Back
    /// control, after a menu re-open) must hand it back to that pane, or to a
    /// live pane of the active workspace once that pane is gone. Before the
    /// fix the Settings node unmounted with the focus inside it and the
    /// focus-lost fallback parked the window on `empty_workspace_focus`,
    /// which takes no terminal input.
    #[gpui::test]
    fn closing_settings_restores_originating_pane_focus(cx: &mut gpui::TestAppContext) {
        use gpui::Focusable as _;
        let (app, a, b, cx) = split_app(cx);

        // Escape, through the Settings root's key handler.
        open_settings(&app, cx);
        cx.simulate_keystrokes("escape");
        draw(cx);
        assert_closed_onto(&app, &b, "Escape", cx);

        // A real click on the Back control at the top of the Settings rail.
        open_settings(&app, cx);
        let back = cx
            .debug_bounds("settings-back")
            .expect("the Settings rail paints its Back control");
        cx.simulate_click(back.center(), gpui::Modifiers::none());
        draw(cx);
        assert_closed_onto(&app, &b, "Back", cx);

        // PaneFlow ▸ Settings… again while Settings is open keeps the pane it
        // was first opened from, even when a pane handle holds the focus at
        // the re-open (a workspace chord focuses one under Settings).
        open_settings(&app, cx);
        cx.update(|window, cx| {
            a.read(cx).focus_handle(cx).focus(window, cx);
            app.update(cx, |app, cx| app.open_settings_window(window, cx));
        });
        draw(cx);
        cx.simulate_keystrokes("escape");
        draw(cx);
        assert_closed_onto(&app, &b, "Escape after a menu re-open", cx);

        // Closing Settings while it is already closed owes no return, so it
        // cannot pull the focus out of whatever holds it.
        cx.update(|_window, cx| {
            app.update(cx, |app, cx| {
                app.close_settings(cx);
                assert!(
                    !app.pending_settings_return,
                    "a close of a closed Settings must not queue a return"
                );
            });
        });
        draw(cx);
        assert!(is_focused(&b, cx));

        // The originating pane closes while Settings is open: the focus goes
        // to a live pane of the active workspace instead.
        open_settings(&app, cx);
        cx.update(|_window, cx| {
            app.update(cx, |app, cx| {
                app.workspaces[0].active_tab_mut().root =
                    Some(crate::layout::LayoutTree::Leaf(a.clone()));
                cx.notify();
            });
        });
        drop(b);
        cx.simulate_keystrokes("escape");
        draw(cx);
        assert_closed_onto(&app, &a, "Escape after the origin closed", cx);
    }

    /// Issue #1096 follow-up: a surface that took the focus over Settings
    /// keeps it when Settings closes underneath it (the sidebar toggle,
    /// Cmd+Alt+B, closes Settings from anywhere), and Pane Overview closing
    /// over a still-open Settings hands the focus to Settings, not to the
    /// pane Settings hides.
    #[gpui::test]
    fn settings_return_leaves_surfaces_opened_over_settings(cx: &mut gpui::TestAppContext) {
        let (app, _a, b, cx) = split_app(cx);
        let open_overview = |cx: &mut gpui::VisualTestContext| {
            cx.update(|window, cx| {
                app.update(cx, |app, cx| {
                    app.handle_open_pane_overview(&crate::OpenPaneOverview, window, cx);
                });
            });
            draw(cx);
            assert!(
                handle_focused(&app, cx, |app| &app.pane_overview_focus),
                "Pane Overview takes the focus over Settings"
            );
        };

        // Pane Overview closes over an open Settings: back to Settings, which
        // still answers Escape and still returns to B.
        open_settings(&app, cx);
        open_overview(cx);
        cx.update(|window, cx| {
            app.update(cx, |app, cx| {
                app.close_pane_overview_and_restore_focus(window, cx)
            });
        });
        draw(cx);
        assert!(
            app.read_with(cx, |app, _| app.settings_section.is_some()),
            "Settings stays open under the overview"
        );
        assert!(
            handle_focused(&app, cx, |app| &app.settings_focus),
            "an overlay closing over Settings returns the focus to Settings"
        );
        cx.simulate_keystrokes("escape");
        draw(cx);
        assert_closed_onto(
            &app,
            &b,
            "Escape after Pane Overview closed over Settings",
            cx,
        );

        // The sidebar toggle closes Settings under Pane Overview: the
        // overview keeps the focus, and its own close then lands on B.
        open_settings(&app, cx);
        open_overview(cx);
        cx.update(|_window, cx| {
            app.update(cx, |app, cx| app.toggle_primary_sidebar_with_chrome(cx));
        });
        draw(cx);
        assert!(app.read_with(cx, |app, _| app.settings_section.is_none()));
        assert!(
            handle_focused(&app, cx, |app| &app.pane_overview_focus),
            "closing Settings must not pull the focus out of Pane Overview"
        );
        assert!(!is_focused(&b, cx));
        cx.update(|window, cx| {
            app.update(cx, |app, cx| {
                app.close_pane_overview_and_restore_focus(window, cx)
            });
        });
        draw(cx);
        assert!(is_focused(&b, cx), "the overview's own close lands on B");

        // The same for a modal: About opened from the menu over Settings.
        open_settings(&app, cx);
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_about_dialog(window, cx));
        });
        draw(cx);
        cx.update(|_window, cx| {
            app.update(cx, |app, cx| app.toggle_primary_sidebar_with_chrome(cx));
        });
        draw(cx);
        assert!(
            handle_focused(&app, cx, |app| &app.about_dialog_focus),
            "closing Settings must not pull the focus out of About"
        );
        assert!(!is_focused(&b, cx));
    }

    /// Issue #1096 follow-up: in Review, with no origin pane, Settings falls
    /// back to the active Review pane, not the grid's first leaf, which
    /// `review_track_focus` would turn into a pane switch that drops the
    /// selected file.
    #[gpui::test]
    fn settings_falls_back_to_the_active_review_pane(cx: &mut gpui::TestAppContext) {
        use gpui::Focusable as _;
        let (app, cx) = cx.add_window_view(|_window, cx| {
            let mut app = crate::app::test_support::blank_paneflow_app(cx);
            hold_session_saves(&mut app);
            app
        });
        let a = make_pane(cx);
        let b = make_pane(cx);
        let root = crate::layout::LayoutTree::from_panes_equal(
            crate::layout::SplitDirection::Vertical,
            vec![a.clone(), b.clone()],
        )
        .expect("two panes make a split");
        cx.update(|window, cx| {
            app.update(cx, |app, cx| {
                app.mode = paneflow_config::schema::AppMode::Diff;
                app.review.layout = Some(root);
                app.review.active_pane = Some(b.downgrade());
                // Opened with no pane holding the focus: no origin recorded.
                window.focus(&app.empty_workspace_focus, cx);
                app.open_settings_window(window, cx);
                app.close_settings(cx);
                app.drain_pending_window_actions(window, cx);
            });
            assert!(
                b.read(cx).focus_handle(cx).is_focused(window),
                "the active Review pane regains the focus"
            );
            assert!(!a.read(cx).focus_handle(cx).is_focused(window));
        });
    }
}
