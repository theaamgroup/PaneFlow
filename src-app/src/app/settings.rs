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

use gpui::{Context, KeyDownEvent, Keystroke, ScrollHandle, Window};

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

impl PaneFlowApp {
    /// Open the embedded settings (Codex-style). The Settings button and the
    /// title-bar / macOS menu route here; it sets `settings_section`, and
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
        self.profile_menu_open = None;
        self.settings_section = Some(section);
        self.reset_settings_scroll();
        self.terminal_dropdown = None;
        self.general_dropdown = None;
        self.workspace_template_dropdown = None;
        self.workspace_template_detail_open = false;
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
        // Warm the MCP bridge status off-thread so the MCP page can render its
        // button label without ever doing config I/O during a frame.
        self.refresh_mcp_status(cx);
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

    pub(crate) fn close_settings(&mut self, cx: &mut Context<Self>) {
        self.settings_section = None;
        self.profile_menu_open = None;
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
        self.workspace_template_dropdown = None;
        self.workspace_template_detail_open = false;
        self.clear_settings_search(cx);
        if self.recording_shortcut_idx.is_some() {
            self.recording_shortcut_idx = None;
            let config = paneflow_config::loader::load_config();
            keybindings::apply_keybindings(cx, &config.shortcuts);
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
        self.cached_config =
            config_writer::with_field(&self.cached_config, nested, key, value.clone());
        config_writer::publish_config_snapshot(cx, &self.cached_config);
        if !nested && matches!(key, "macos_chrome_material") {
            for ws in &self.workspaces {
                ws.propagate_config(&self.cached_config, cx);
            }
        }
        if nested && matches!(key, "integrated_glyphs" | "color_emoji" | "cursor_color") {
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
        self.cached_config =
            config_writer::with_agent_panel_field(&self.cached_config, key, value.clone());
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
        // Font dropdown typeahead (Terminal page).
        if self.font_dropdown_open {
            let key = event.keystroke.key.as_str();
            match key {
                "escape" => {
                    self.font_dropdown_open = false;
                    self.font_search.clear();
                    cx.notify();
                }
                "backspace" => {
                    self.font_search.pop();
                    cx.notify();
                }
                _ => {
                    if let Some(ch) = &event.keystroke.key_char
                        && !ch.is_empty()
                        && !event.keystroke.modifiers.control
                        && !event.keystroke.modifiers.platform
                    {
                        self.font_search.push_str(ch);
                        cx.notify();
                    }
                }
            }
            return;
        }

        // Escape: close an open Terminal-page dropdown first, otherwise leave
        // settings. Escape during shortcut recording or key capture never gets
        // here - `intercept_shortcut_keystroke` consumes it upstream, before
        // GPUI matches any binding.
        if event.keystroke.key == "escape" && self.recording_shortcut_idx.is_none() {
            if self.terminal_dropdown.is_some() {
                self.terminal_dropdown = None;
            } else if self.general_dropdown.is_some() {
                self.general_dropdown = None;
            } else if self.workspace_template_dropdown.is_some() {
                self.workspace_template_dropdown = None;
            } else {
                self.close_settings(cx);
            }
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
        let Some(action_name) = self.effective_shortcuts.get(idx).map(|e| e.action_name) else {
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
        self.effective_shortcuts = keybindings::effective_shortcuts(&config.shortcuts);
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
    use super::{ShortcutKeyRoute, recorded_shortcut_key, route_shortcut_keystroke};
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
}
