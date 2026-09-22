//! Lifecycle of the diff dock's tabs: opening a terminal tab from the `+`
//! menu, selecting a tab, and closing one.
//!
//! No tab is permanent (upstream `f587f7fc`): `Changes` is opened from the
//! surface picker or the `+` menu like any other surface, every tab closes,
//! and a strip that empties hands the dock back to the picker.

use gpui::{AppContext, Context, Entity, Focusable, Window};

use super::model::DiffDockTab;
use crate::PaneFlowApp;
use crate::terminal::{TerminalEvent, TerminalView};

impl PaneFlowApp {
    /// The picker's `Changes` card and the `+` menu's "Changes" row. Singleton
    /// per dock: a second invocation selects the existing tab.
    pub(crate) fn open_diff_changes_tab(&mut self, cx: &mut Context<Self>) {
        let index = self
            .diff_dock
            .diff_tabs
            .iter()
            .position(|tab| matches!(tab, DiffDockTab::Changes));
        let index = index.unwrap_or_else(|| {
            self.diff_dock.diff_tabs.push(DiffDockTab::Changes);
            self.diff_dock.diff_tabs.len() - 1
        });
        self.select_diff_tab(index, cx);
    }

    /// Open a terminal tab in the dock and focus it. The shell lands in the
    /// folder the dock is diffing, falling back to the active workspace root
    /// (the same chain `new_terminal_cwd` uses for a split).
    pub(crate) fn open_diff_terminal_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ws) = self.active_workspace() else {
            return;
        };
        let ws_id = ws.id;
        let cwd = self
            .diff_dock
            .data
            .as_ref()
            .map(|data| data.cwd.clone())
            .filter(|cwd| !cwd.is_empty())
            .map(std::path::PathBuf::from);
        let cwd = self.new_terminal_cwd(cwd);
        let effective_cwd = cwd
            .clone()
            .unwrap_or_else(crate::launch_cwd::implicit_launch_cwd);
        if self.pending_worktree_teardown_conflicts(&effective_cwd) {
            self.show_toast("Worktree is still being retired", cx);
            return;
        }

        let terminal = cx.new(|cx| TerminalView::with_cwd(ws_id, Some(effective_cwd), None, cx));
        // Only the exit and the program's own notifications are wired: the
        // dock terminal has no pane in the layout tree, so the app-level CWD
        // / port-scan / open-path handlers have nothing to act on for it, and
        // `handle_terminal_event` (which delivers OSC 9/777 for hosted panes,
        // #422) would never find it under a workspace.
        cx.subscribe(
            &terminal,
            |this, terminal: Entity<TerminalView>, event: &TerminalEvent, cx| match event {
                TerminalEvent::ChildExited => this.close_diff_terminal_tab(&terminal, cx),
                TerminalEvent::ProgramNotification { title, body } => {
                    // Resolve current ownership: parked docks follow their tab
                    // when it moves to another workspace.
                    let muted = this.workspaces.iter().any(|ws| {
                        this.workspace_is_muted(ws.id)
                            && this
                                .diff_dock_terminals_for_workspace(ws.id)
                                .contains(&terminal)
                    });
                    let seen = this.dock_terminal_is_seen(&terminal) || muted;
                    let pane_title = terminal.read(cx).terminal.title.clone();
                    crate::agents::notifications::fire_program_notification(
                        crate::agents::notifications::program_notification(
                            title.clone(),
                            body.clone(),
                            &pane_title,
                        ),
                        seen,
                        cx.background_executor().clone(),
                    );
                }
                _ => {}
            },
        )
        .detach();

        let focus = terminal.read(cx).focus_handle(cx);
        window.focus(&focus, cx);
        self.diff_dock
            .diff_tabs
            .push(DiffDockTab::Terminal(terminal));
        self.select_diff_tab(self.diff_dock.diff_tabs.len() - 1, cx);
    }

    /// Move keyboard focus onto whatever the tab at `index` hosts. The
    /// `Changes` and Agent setup tabs own no focus handle, so this is a no-op
    /// there. A terminal tab does.
    pub(crate) fn dock_tab_focus_handle(
        &self,
        index: usize,
        cx: &Context<Self>,
    ) -> Option<gpui::FocusHandle> {
        match self.diff_dock.diff_tabs.get(index) {
            Some(DiffDockTab::Terminal(terminal)) => Some(terminal.read(cx).focus_handle(cx)),
            _ => None,
        }
    }

    /// Ctrl+J: the chord the `+` menu advertises on its "Terminal" row.
    /// Inert unless the diff dock is actually on screen
    /// ([`Self::diff_dock_visible`], not the `open` flag alone, which survives
    /// a trip through Settings or a mode switch).
    pub(crate) fn handle_diff_new_terminal_tab(
        &mut self,
        _: &crate::DiffNewTerminalTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.diff_dock_visible() {
            return;
        }
        self.open_diff_terminal_tab(window, cx);
    }

    /// Select the tab at `index`. Owns the picker state: landing on a tab is
    /// the answer to the dock's surface question, both now and the next time
    /// this session toggles the dock from a pane header.
    pub(crate) fn select_diff_tab(&mut self, index: usize, cx: &mut Context<Self>) {
        if index < self.diff_dock.diff_tabs.len() {
            self.diff_dock.picker = false;
            self.diff_dock.picked = true;
            // Re-selecting the active tab is not a move: the chip's own click
            // fires right after its close control's, and clearing the arm here
            // would undo the confirmation that click just set.
            if self.diff_dock.diff_active_tab != index {
                self.diff_dock.diff_active_tab = index;
            }
            cx.notify();
        }
    }

    /// The close affordance's entry point. Every tab closes on the first press.
    pub(crate) fn request_close_diff_tab(&mut self, index: usize, cx: &mut Context<Self>) {
        self.close_diff_tab(index, cx);
    }

    /// Close the tab at `index`. The selection falls back to the previous
    /// tab; closing the last one returns the dock to its surface picker,
    /// re-armed so the session is asked again.
    pub(crate) fn close_diff_tab(&mut self, index: usize, cx: &mut Context<Self>) {
        if index >= self.diff_dock.diff_tabs.len() {
            return;
        }
        let closed = self.diff_dock.diff_tabs.remove(index);
        self.diff_dock.diff_active_tab =
            active_tab_after_close(self.diff_dock.diff_active_tab, index);
        if self.diff_dock.diff_tabs.is_empty() {
            self.diff_dock.picker = true;
            self.diff_dock.picked = false;
        }
        // The menus describe a strip that just changed under them.
        self.close_diff_options_menu(cx);
        self.close_diff_new_tab_menu(cx);
        // The branch picker belongs to the Changes surface, and this path has
        // no `Window` to hand its focus back through `close_diff_branch_menu`
        // (a terminal exiting under another tab lands here). Drop it only when
        // the surface it was open over is the tab that just went away.
        if matches!(closed, DiffDockTab::Changes) {
            self.diff_dock.diff_branch_menu = None;
        }
        cx.notify();
    }

    /// Close whichever tab hosts `terminal` (the shell exited under it).
    fn close_diff_terminal_tab(&mut self, terminal: &Entity<TerminalView>, cx: &mut Context<Self>) {
        let found = self
            .diff_dock
            .diff_tabs
            .iter()
            .position(|tab| matches!(tab, DiffDockTab::Terminal(t) if t == terminal));
        if let Some(index) = found {
            self.close_diff_tab(index, cx);
        }
    }
}

/// The active tab index after the tab at `closed` is removed. Selection falls
/// back to the previous tab, and can never point past the shortened strip.
pub(super) fn active_tab_after_close(active: usize, closed: usize) -> usize {
    if active >= closed {
        active.saturating_sub(1)
    } else {
        active
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The close control's click is followed by the chip's own click. The
    /// close chip must own the gesture, or the chip underneath re-selects the
    /// tab that was just closed.
    #[test]
    fn the_close_chip_owns_its_click() {
        use crate::source_probe::source_slice;
        let close = source_slice(
            include_str!("render.rs"),
            "this.request_close_diff_tab(index, cx);",
            "}))",
        );
        assert!(
            close.contains("cx.stop_propagation();"),
            "the dock tab close chip must stop its click from reaching the chip"
        );
    }

    /// Closing a tab leaves `diff_active_tab` inside the strip.
    #[test]
    fn the_active_index_stays_in_bounds_after_a_close() {
        // Closing the active tab falls back to the previous one.
        assert_eq!(active_tab_after_close(3, 3), 2);
        // Closing before the active tab shifts it up.
        assert_eq!(active_tab_after_close(3, 1), 2);
        // Closing after it leaves it alone.
        assert_eq!(active_tab_after_close(1, 2), 1);
        // The last remaining tab after the previous one lands on its neighbor.
        assert_eq!(active_tab_after_close(1, 1), 0);
        // Index 0 is closable too (upstream f587f7fc): closing the first tab
        // keeps the selection at the front of the shortened strip.
        assert_eq!(active_tab_after_close(0, 0), 0);
        assert_eq!(active_tab_after_close(1, 0), 0);

        // Exhaustive: for any strip up to 12 tabs, closing any index leaves
        // an index inside the shortened strip.
        for len in 2..=12usize {
            for closed in 0..len {
                for active in 0..len {
                    let next = active_tab_after_close(active, closed);
                    assert!(
                        next < len - 1,
                        "len={len} closed={closed} active={active} -> {next} is out of bounds"
                    );
                }
            }
        }
    }
}
