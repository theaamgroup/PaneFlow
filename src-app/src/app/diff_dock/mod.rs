//! Codex-style git diff side panel for the CLI cockpit.
//!
//! A right-docked panel (toggled from a pane header, see
//! [`crate::app::cli_diff_dock`]) that shows what the agent changed
//! in a workspace folder: the working-tree diff against `HEAD` (staged +
//! unstaged tracked changes) plus untracked files.
//!
//! EP-001 (review redesign, US-001/US-002): the dock no longer
//! has its own diff renderer or unified-diff parser. It renders through the
//! exact same path as the full-screen Review view ([`crate::diff`]): the shared
//! git pipeline ([`crate::diff::compute_head_diff`]), the shared row model
//! ([`crate::diff::build_display_rows`] / `build_split_rows`) and the shared
//! direct-paint [`crate::diff::DiffElement`] hosted in an `overflow_y_scroll`
//! div. The dock keeps the cheap HEAD-relative semantic (the right "what did the
//! agent just touch" base, vs the Review view's `merge-base(HEAD, base)`), but
//! shares everything else - so a visual change to the diff body is made once.
//!
//! Split (US-004) into seams: [`git`] (off-thread build), [`model`]
//! ([`DiffDockData`] + layout constants) and [`render`] (chrome render
//! helpers). This module owns the [`PaneFlowApp`] panel orchestration: the
//! open/refresh/collapse lifecycle, the panel + body render, and the body click.

mod branch;
// prd-file-editor-2026-Q3, EP-001: the editable-document layer behind the
// dock's `File` tabs (US-017).
pub(crate) mod code;
mod git;
mod model;
mod new_tab_menu;
mod options_menu;
mod render;
mod revert;
mod setup;
mod surface_picker;
mod tabs;

pub(crate) use branch::{DiffBranchMenuState, list_branches};
pub(crate) use model::{
    DIFF_DOCK_PANEL_MAX_WIDTH, DIFF_DOCK_PANEL_MIN_WIDTH, DIFF_DOCK_PANEL_WIDTH, DiffDockData,
    DiffDockHScrollDrag, DiffDockTab, DiffHover,
};

use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, ClickEvent, Context, InteractiveElement, IntoElement, MouseButton, MouseDownEvent,
    MouseMoveEvent, ParentElement, Pixels, Point, ScrollHandle, ScrollWheelEvent,
    StatefulInteractiveElement, Styled, Window, div, px,
};

use self::branch::render_diff_branch_chip;
use self::git::build_diff_dock;
use self::model::DiffChrome;
use self::render::{
    diff_file_header_path, diff_panel_centered, render_diff_file_header, render_diff_files_toolbar,
    render_diff_resize_handle, render_diff_tab_strip,
};
use self::surface_picker::{render_diff_picker_header, render_diff_surface_picker};
use crate::PaneFlowApp;
use crate::diff::{
    DiffBody, DiffElement, H_SCROLLBAR_TRACK_HEIGHT, HScrollbarSegment, RowKind, SplitRow,
    discard_expanded_folds_for_path, file_at_row, h_offset_index, h_offset_len,
    h_scrollbar_click_offset, h_scrollbar_segments, palette, row_at_offset, set_file_side_offset,
    split_right_side_at_x,
};
use crate::ui_primitives::squircle::{squircle_border, squircle_fill};

/// What a resize drag commits to the stored width preference. `dragged` is
/// the width the cursor asks for, anchored on the *rendered* width, and
/// `ceiling` is what the panel can show this frame.
///
/// Issue #184: the render fit never writes its clamped value back, and the
/// drag must not do so by proxy. A result strictly below the ceiling is a
/// width the user chose, so it is stored. A result pinned at the ceiling only
/// says "at least this wide": a stored preference that already satisfies that
/// (a wide dock clamped under a rail) is left alone, so closing the rail
/// restores it; a narrower one is raised to the ceiling. Either way the stored
/// value stays within `[DIFF_DOCK_PANEL_MIN_WIDTH, DIFF_DOCK_PANEL_MAX_WIDTH]`.
pub(crate) fn diff_dock_drag_preference(stored: f32, dragged: f32, ceiling: f32) -> f32 {
    let ceiling = ceiling.clamp(DIFF_DOCK_PANEL_MIN_WIDTH, DIFF_DOCK_PANEL_MAX_WIDTH);
    let dragged = dragged.clamp(DIFF_DOCK_PANEL_MIN_WIDTH, ceiling);
    if dragged < ceiling {
        dragged
    } else {
        stored.max(ceiling)
    }
}

impl PaneFlowApp {
    /// Open the Codex-style diff dock on `cwd`, computing the diff off-thread.
    /// Closing (see [`Self::close_diff_dock_panel`]) drops the retained
    /// snapshot so a large hidden dock cannot keep old rows alive.
    pub(crate) fn open_diff_dock_panel(&mut self, cwd: String, cx: &mut Context<Self>) {
        let cwd = cwd.trim().to_string();
        // The single door to an open dock, so the single place that records
        // which session owns it - `sync_diff_dock_session` would otherwise read
        // the open as a drift and park it on the next frame.
        self.diff_dock.owner = self.active_session_id();
        let split = self.diff_dock.split;
        let has_current_snapshot = self.diff_dock.data.as_ref().is_some_and(|data| {
            data.cwd == cwd
                && !data.loading
                && data.error.is_none()
                && data.has_mode(split)
                && data.theme_generation == crate::theme::theme_generation()
        });
        if !self.diff_dock.open {
            self.diff_dock.reveal_animation =
                (!crate::ui_primitives::reduce_motion()).then(|| crate::SidebarWidthAnimation {
                    from_width: 0.,
                    to_width: 1.,
                    started_at: std::time::Instant::now(),
                });
        }
        self.diff_dock.open = true;
        if has_current_snapshot {
            cx.notify();
        } else {
            self.refresh_diff_dock(cwd, cx);
        }
    }

    pub(crate) fn close_diff_dock_panel(&mut self, cx: &mut Context<Self>) {
        self.diff_dock.open = false;
        self.diff_dock.data = None;
        self.clear_diff_dock_snapshot_state();
        self.diff_dock.maximized = None;
        self.diff_dock.maximize_animation = None;
        self.diff_dock.reveal_animation = None;
        self.diff_dock.resize = None;
        self.diff_dock.h_scroll_drag = None;
        self.diff_dock.vertical_scrollbar.cancel_drag();
        cx.notify();
    }

    /// Recompute the diff for `cwd`, parking a loading state first. Shared by the
    /// open path and the panel's refresh button. The async result is dropped if
    /// the cached slot has since rebound to a different cwd.
    pub(crate) fn refresh_diff_dock(&mut self, cwd: String, cx: &mut Context<Self>) {
        let cwd = cwd.trim().to_string();
        let generation = self.diff_dock.generation.wrapping_add(1);
        self.diff_dock.generation = generation;
        let previous = self.diff_dock.data.as_ref().filter(|data| data.cwd == cwd);
        let previous_fingerprint = previous.map(|data| data.fingerprint).unwrap_or(0);
        // The loading stub has no rows. Carry the last HEAD so the build
        // callback can still tell a commit/checkout from a same-SHA refresh;
        // `has_rows()` would make `reload_file_tab_bases` a no-op.
        let previous_head_sha = previous.and_then(|data| data.head_sha.clone());
        let cwd_changed = self
            .diff_dock
            .data
            .as_ref()
            .is_some_and(|data| data.cwd != cwd);
        if cwd_changed {
            self.clear_diff_dock_snapshot_state();
        }
        if cwd.is_empty() {
            self.clear_diff_dock_snapshot_state();
            self.diff_dock.data = Some(DiffDockData::message(
                cwd,
                "No folder is linked to this thread.".to_string(),
            ));
            cx.notify();
            return;
        }
        let mut loading = DiffDockData::loading(cwd.clone());
        loading.fingerprint = previous_fingerprint;
        loading.head_sha = previous_head_sha;
        self.diff_dock.data = Some(loading);
        cx.notify();

        self.spawn_diff_dock_build(cwd, generation, cx);
    }

    fn reload_file_tab_bases(&mut self, cx: &mut Context<Self>) {
        for tab in self.diff_dock.diff_tabs.clone() {
            if let DiffDockTab::File(view) = tab {
                view.update(cx, |view, cx| view.reload_base(cx));
            }
        }
    }

    fn clear_diff_dock_snapshot_state(&mut self) {
        self.diff_dock.collapsed.clear();
        self.diff_dock.expanded_folds.clear();
        self.diff_dock.scroll = ScrollHandle::new();
        // The vertical scrollbar's drag anchor and units-per-pixel were
        // measured against the handle just replaced; a thumb still held
        // would otherwise scroll the new document from the old anchor.
        self.diff_dock.vertical_scrollbar.cancel_drag();
        self.diff_dock.h_offsets = std::rc::Rc::new(Vec::new());
    }

    pub(crate) fn refresh_diff_dock_if_open_for_cwd(
        &mut self,
        cwd: &str,
        cx: &mut Context<Self>,
    ) -> bool {
        let should_refresh = self.diff_dock.open
            && self
                .diff_dock
                .data
                .as_ref()
                .is_some_and(|data| data.cwd == cwd && !data.loading);
        if should_refresh {
            self.refresh_diff_dock(cwd.to_string(), cx);
        }
        should_refresh
    }

    fn spawn_diff_dock_build(&mut self, cwd: String, generation: u64, cx: &mut Context<Self>) {
        // Capture the theme on the main thread (the syntax pass needs it) and
        // move it into the worker, exactly as the Review view does.
        let theme = crate::theme::active_theme();
        let theme_generation = crate::theme::theme_generation();
        cx.spawn(
            async move |this: gpui::WeakEntity<Self>, cx: &mut gpui::AsyncApp| {
                let result = smol::unblock({
                    let cwd = cwd.clone();
                    move || build_diff_dock(&cwd, theme, theme_generation)
                })
                .await;
                let _ = cx.update(|cx| {
                    this.update(cx, |app, cx| {
                        // Apply even while the dock is hidden so the next reopen can
                        // render from the warm snapshot instead of flashing a loader.
                        let still_current = app
                            .diff_dock
                            .data
                            .as_ref()
                            .is_some_and(|data| data.cwd == cwd)
                            && app.diff_dock.generation == generation;
                        if !still_current {
                            return;
                        }
                        // Read the live collapse set (it may have changed during
                        // the async build) so the first paint honors it.
                        let collapsed = app.diff_dock.collapsed.clone();
                        let expanded = app.diff_dock.expanded_folds.clone();
                        match result {
                            Ok(built) => {
                                let stats = crate::workspace::GitDiffStats {
                                    files_changed: built.file_count,
                                    insertions: built.added as usize,
                                    deletions: built.removed as usize,
                                };
                                app.apply_git_stats_for_cwd(&cwd, stats);
                                let reset_snapshot_state =
                                    app.diff_dock.data.as_ref().is_some_and(|data| {
                                        data.fingerprint != 0
                                            && data.fingerprint != built.fingerprint
                                    });
                                if reset_snapshot_state {
                                    app.clear_diff_dock_snapshot_state();
                                } else {
                                    app.diff_dock.h_offsets = std::rc::Rc::new(Vec::new());
                                }
                                let collapsed = if reset_snapshot_state {
                                    std::collections::HashSet::new()
                                } else {
                                    collapsed
                                };
                                let expanded = if reset_snapshot_state {
                                    std::collections::HashSet::new()
                                } else {
                                    expanded
                                };
                                let head_changed = app
                                    .diff_dock
                                    .data
                                    .as_ref()
                                    .is_some_and(|data| data.head_sha != built.head_sha);
                                if let Some(data) = app.diff_dock.data.as_mut() {
                                    data.apply_built(built, &collapsed, &expanded);
                                }
                                app.diff_dock.hover = None;
                                if head_changed {
                                    app.reload_file_tab_bases(cx);
                                }
                            }
                            Err(err) => {
                                app.diff_dock.data = Some(DiffDockData::message(cwd.clone(), err));
                            }
                        }
                        cx.notify();
                    })
                });
            },
        )
        .detach();
    }

    /// Re-derive the cached collapse-filtered display rows after a collapse /
    /// split change (no git work - just re-filters the retained full rows).
    fn recompute_diff_dock_display(&mut self) {
        let collapsed = self.diff_dock.collapsed.clone();
        let expanded = self.diff_dock.expanded_folds.clone();
        if let Some(data) = self.diff_dock.data.as_mut() {
            data.recompute(&collapsed, &expanded);
        }
    }

    fn refresh_diff_dock_if_theme_changed(&mut self, cx: &mut Context<Self>) {
        let current_theme_generation = crate::theme::theme_generation();
        let Some(data) = self.diff_dock.data.as_ref() else {
            return;
        };
        if data.loading
            || data.error.is_some()
            || data.theme_generation == current_theme_generation
            || data.cwd.trim().is_empty()
        {
            return;
        }
        self.refresh_diff_dock(data.cwd.clone(), cx);
    }

    /// Fold / unfold a single file in the diff dock (click on its header row).
    pub(crate) fn toggle_diff_file_collapsed(&mut self, path: String, cx: &mut Context<Self>) {
        if !self.diff_dock.collapsed.remove(&path) {
            discard_expanded_folds_for_path(&mut self.diff_dock.expanded_folds, &path);
            self.diff_dock.collapsed.insert(path);
        }
        self.recompute_diff_dock_display();
        cx.notify();
    }

    /// "Collapse all" / "expand all" for the diff dock. `collapse == true` folds
    /// every file in `paths`; `false` clears the whole collapse set.
    pub(crate) fn set_all_diff_collapsed(
        &mut self,
        paths: &[String],
        collapse: bool,
        cx: &mut Context<Self>,
    ) {
        if collapse {
            self.diff_dock.collapsed.extend(paths.iter().cloned());
            self.diff_dock.expanded_folds.clear();
        } else {
            self.diff_dock.collapsed.clear();
        }
        self.recompute_diff_dock_display();
        cx.notify();
    }

    /// Switch the diff dock between unified and split views. Both row models are
    /// warmed by the off-thread load, so this is a paint-only toggle.
    pub(crate) fn set_diff_dock_split(&mut self, split: bool, cx: &mut Context<Self>) {
        if self.diff_dock.split == split {
            return;
        }
        self.diff_dock.split = split;
        self.diff_dock.h_scroll_drag = None;
        self.diff_dock.vertical_scrollbar.cancel_drag();
        cx.notify();
    }

    /// The docked diff panel: a header over the body. Reads the live snapshot
    /// from state (cloned cheaply) so the caller keeps its `self` borrow short.
    ///
    /// `width` is the width the dock renders at (see
    /// `cli_diff_dock::diff_dock_fit`) - not `self.diff_dock.width`, which is
    /// only the user's preference. The ceiling stays with the dock host, which
    /// feeds it to the resize drag on every move.
    pub(crate) fn render_diff_dock_panel(
        &mut self,
        width: f32,
        ui: crate::theme::UiColors,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let files_width = crate::app::files_sidebar::dock_tree_width(
            self.files_tree_in_dock()
                && self.files_sidebar_open
                && self.files_sidebar_host_visible(),
            self.diff_file_tab_active(),
            width,
        );
        // A tree hidden by the active tab or the width clamp cannot keep the
        // keyboard. The panel and watcher remain alive for the next mount.
        if self.files_tree_in_dock() && files_width == 0. {
            self.files_menu_open = None;
            use gpui::Focusable;
            if self
                .files_sidebar
                .read(cx)
                .focus_handle(cx)
                .contains_focused(window, cx)
            {
                self.focus_diff_tab(self.diff_dock.diff_active_tab, window, cx);
            }
        }
        // First open of the session: the dock asks what to show instead of
        // dropping into the diff. Gated on `Cli` because the flag is set by the
        // pane-header toggle - the Agents dock is opened from its own chrome,
        // already on a chosen surface, and must never inherit the question.
        // An empty strip is the same question: nothing was chosen yet, or the
        // last tab was closed.
        if (self.diff_dock.picker || self.diff_dock.diff_tabs.is_empty())
            && matches!(self.mode, paneflow_config::schema::AppMode::Cli)
        {
            return self.render_diff_dock_picker(width, ui, cx);
        }
        self.refresh_diff_dock_if_theme_changed(cx);
        let data = self.diff_dock.data.clone();
        let cwd = data.as_ref().map(|d| d.cwd.clone()).unwrap_or_default();
        let active = self
            .diff_dock
            .diff_active_tab
            .min(self.diff_dock.diff_tabs.len().saturating_sub(1));
        let tabs = self.diff_dock.diff_tabs.clone();
        let maximized = self.diff_dock.maximized.is_some();
        let fills_panel = self.diff_dock_fills_panel();
        let header = render_diff_tab_strip(
            &tabs,
            active,
            self.diff_dock.diff_tab_close_armed,
            self.diff_dock.diff_new_tab_menu_open,
            maximized,
            ui,
            cx,
        );
        // A terminal tab owns the whole body: the files toolbar describes the
        // diff (scope, +/- totals, branch, layout menu) and has nothing to say
        // about a shell.
        let (toolbar, body) = match tabs.get(active) {
            Some(DiffDockTab::Terminal(terminal)) => (None, terminal.clone().into_any_element()),
            // No toolbar either: the file header describes an open document,
            // and this tab is the one that has none yet.
            Some(DiffDockTab::PendingFile) => (
                self.files_tree_in_dock().then(|| {
                    render_dock_file_toolbar(&cwd, None, self.render_files_tree_toggle(ui, cx), ui)
                }),
                render::render_pending_file_body(ui),
            ),
            // The inventory follows the dock's folder: a dock retargeted onto
            // another checkout rescans, a repaint does not.
            Some(DiffDockTab::Setup(view)) => {
                let folder = if cwd.is_empty() {
                    self.diff_setup_cwd()
                } else {
                    cwd.clone()
                };
                view.update(cx, |view, cx| view.sync_cwd(folder, cx));
                (None, view.clone().into_any_element())
            }
            // A file tab swaps the diff's files toolbar for its own header
            // (US-018): same 36 px band, describing the open document instead
            // of the working tree.
            Some(DiffDockTab::File(view)) => {
                let (icon, path, line, column, controls) = {
                    let view = view.read(cx);
                    let path = view.path().to_path_buf();
                    let (line, column) = view.cursor_line_column();
                    let name = path
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    (
                        render::file_tab_icon(&name),
                        diff_file_header_path(&cwd, &path),
                        line,
                        column,
                        view.controls.clone().into_any_element(),
                    )
                };
                (
                    Some(if self.files_tree_in_dock() {
                        render_dock_file_toolbar(
                            &cwd,
                            Some((icon, path, line, column, controls)),
                            self.render_files_tree_toggle(ui, cx),
                            ui,
                        )
                    } else {
                        render_diff_file_header(icon, path, line, column, controls, ui)
                    }),
                    view.clone().into_any_element(),
                )
            }
            Some(DiffDockTab::Changes) => (
                Some(self.render_diff_toolbar(&cwd, &data, ui, cx)),
                self.render_diff_dock_body(&data, ui, cx),
            ),
            None => (None, render_diff_surface_picker(ui, cx)),
        };

        let body = if files_width > 0. {
            render_dock_file_split(body, self.render_files_sidebar(window, cx), files_width, ui)
        } else {
            body
        };

        // The dock is a floating card beside the pane grid, drawn with the same
        // silhouette the panes use (see `crate::pane`): a superellipse fill
        // under the subtree and a hairline over it. Nothing between the two may
        // paint its own background, or it repaints the corners square - GPUI
        // clips no child to a parent radius.
        let radius = crate::app::constants::PANE_CARD_RADIUS;
        div()
            .relative()
            .map(|panel| {
                if fills_panel {
                    panel.w_full()
                } else {
                    panel.w(px(width))
                }
            })
            .h_full()
            .flex_none()
            .flex()
            .flex_col()
            .child(squircle_fill(radius, ui.base))
            .when(!fills_panel, |panel| {
                panel.child(render_diff_resize_handle(width, ui, cx))
            })
            .child(header)
            .children(toolbar)
            .child(body)
            .child(squircle_border(radius, px(1.), ui.border))
            .into_any_element()
    }

    fn render_files_tree_toggle(
        &self,
        ui: crate::theme::UiColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        use crate::ui_primitives::{ROW_RADIUS, TooltipDelayExt, squircle_skin, text_tooltip};
        let label = if self.files_sidebar_open {
            "Hide files tree"
        } else {
            "Show files tree"
        };
        squircle_skin(
            div()
                .id("diff-files-tree-toggle")
                .role(gpui::Role::Button)
                .aria_label(label)
                .aria_toggled(crate::settings::components::switch_toggled(
                    self.files_sidebar_open,
                ))
                .flex_none()
                .size(px(28.))
                .flex()
                .items_center()
                .justify_center(),
            "diff-files-tree-toggle-group",
            ROW_RADIUS,
            None,
            Some(crate::app::constants::sidebar_tab_hover_background()),
        )
        .delayed_tooltip(text_tooltip(label))
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
            this.handle_toggle_files_sidebar(&crate::ToggleFilesSidebar, window, cx);
        }))
        .child(
            gpui::svg()
                .path("icons/folder-open.svg")
                .size(px(14.))
                .text_color(if self.files_sidebar_open {
                    ui.text
                } else {
                    ui.muted
                }),
        )
        .into_any_element()
    }

    /// The dock drawn on its surface picker: same card silhouette, same resize
    /// handle, but the tab strip and the body are replaced by the question. The
    /// panel is rebuilt here rather than branched inside the main renderer so
    /// the picker never pays for the diff snapshot it is not showing.
    fn render_diff_dock_picker(
        &mut self,
        width: f32,
        ui: crate::theme::UiColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let radius = crate::app::constants::PANE_CARD_RADIUS;
        let fills_panel = self.diff_dock_fills_panel();
        div()
            .relative()
            .map(|panel| {
                if fills_panel {
                    panel.w_full()
                } else {
                    panel.w(px(width))
                }
            })
            .h_full()
            .flex_none()
            .flex()
            .flex_col()
            .child(squircle_fill(radius, ui.base))
            .when(!fills_panel, |panel| {
                panel.child(render_diff_resize_handle(width, ui, cx))
            })
            .child(render_diff_picker_header(ui, cx))
            .child(render_diff_surface_picker(ui, cx))
            .child(squircle_border(radius, px(1.), ui.border))
            .into_any_element()
    }

    /// Apply a live resize drag: set the dock width so its left edge tracks the
    /// cursor. Driven by the CLI dock wrapper's `on_mouse_move` (a full-height
    /// capture surface, so the drag survives the cursor leaving the dock for the
    /// pane grid beside it), which passes `ceiling` - the width the panel can
    /// show *this frame* (`cli_diff_dock::diff_dock_fit`), re-read on every
    /// move rather than frozen at mouse-down. No-op when no drag is in progress.
    pub(crate) fn drag_diff_dock_resize(
        &mut self,
        cursor_x: f32,
        ceiling: f32,
        cx: &mut Context<Self>,
    ) {
        if let Some((anchor_x, anchor_w)) = self.diff_dock.resize {
            // The panel docks right and the handle is on its left edge, so
            // dragging left (cursor_x shrinks) widens the dock. The stored
            // width never passes the ceiling the panel can render: letting it
            // run past what is on screen would leave the handle unresponsive
            // until the cursor came back over the overshoot. This is the one
            // writer of the preference, and it records a user gesture - the
            // render-time fit never writes, and a drag pinned at the ceiling
            // does not write the clamp back either (`diff_dock_drag_preference`).
            let delta = anchor_x - cursor_x;
            self.diff_dock.width =
                diff_dock_drag_preference(self.diff_dock.width, anchor_w + delta, ceiling);
            cx.notify();
        }
    }

    /// End a diff-dock resize drag (mouse up / button released mid-move). Returns
    /// whether a drag was actually in progress, so the caller can skip a
    /// redundant notify.
    pub(crate) fn end_diff_dock_resize(&mut self, cx: &mut Context<Self>) -> bool {
        if self.diff_dock.resize.take().is_some() {
            cx.notify();
            true
        } else {
            false
        }
    }

    /// The always-present summary row under the title strip. Owns the branch
    /// chip (which needs `self` for the picker state) and hands the rest of the
    /// dock state to the row renderer as a [`DiffChrome`].
    fn render_diff_toolbar(
        &mut self,
        cwd: &str,
        data: &Option<DiffDockData>,
        ui: crate::theme::UiColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let chip = self
            .diff_branch_for_cwd(cwd)
            .map(|(branch, files_changed)| {
                render_diff_branch_chip(
                    cwd.to_string(),
                    branch,
                    files_changed,
                    self.diff_dock.diff_branch_menu.as_ref(),
                    ui,
                    cx,
                )
            });
        let chrome = DiffChrome {
            data,
            cwd: cwd.to_string(),
            split: self.diff_dock.split,
            options_open: self.diff_dock.diff_options_menu_open,
            layout_submenu_open: self.diff_dock.diff_layout_submenu_open,
            collapsed: &self.diff_dock.collapsed,
        };
        render_diff_files_toolbar(&chrome, chip, ui, cx)
    }

    /// The diff body: the shared [`DiffElement`] in an `overflow_y_scroll` host
    /// (the same render path as the Review view). Empty diffs leave the body
    /// blank; loading and error states render a centered placeholder.
    fn render_diff_dock_body(
        &mut self,
        data: &Option<DiffDockData>,
        ui: crate::theme::UiColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(data) = data else {
            return diff_panel_centered(
                "icons/file-text.svg",
                "Open the panel to see changes.",
                ui,
            );
        };
        if data.loading {
            return diff_panel_centered("icons/loader-circle.svg", "Loading changes…", ui);
        }
        if let Some(error) = &data.error {
            return diff_panel_centered("icons/triangle-alert.svg", error, ui);
        }
        if data.file_count == 0 {
            return div().size_full().into_any_element();
        }

        let split = self.diff_dock.split;

        // Horizontal offsets, lazily resized to the current mode's slot count.
        // Unified uses one slot per file; split keeps detached left/right slots.
        // Cloned into the element each frame.
        let file_count = if split {
            data.disp_split_spans.len()
        } else {
            data.disp_unified_spans.len()
        };
        let needed_offsets = h_offset_len(file_count, split);
        if split {
            if self.diff_dock.h_offsets.len() != needed_offsets {
                std::rc::Rc::make_mut(&mut self.diff_dock.h_offsets).resize(needed_offsets, 0.0);
            }
        } else if self.diff_dock.h_offsets.len() < needed_offsets {
            std::rc::Rc::make_mut(&mut self.diff_dock.h_offsets).resize(needed_offsets, 0.0);
        }
        let h_offsets = self.diff_dock.h_offsets.clone();

        // Collapse-filtered rows + cached layout inputs (recomputed only on a
        // collapse / split change), handed to the direct-paint element.
        let body = if split {
            DiffBody::Split {
                rows: data.disp_split.clone(),
                offsets: data.disp_split_offsets.clone(),
                max_line_no: data.disp_split_max_no,
                spans: data.disp_split_spans.clone(),
                h_offsets: h_offsets.clone(),
            }
        } else {
            DiffBody::Unified {
                rows: data.disp_unified.clone(),
                offsets: data.disp_unified_offsets.clone(),
                max_line_no: data.disp_unified_max_no,
                spans: data.disp_unified_spans.clone(),
                h_offsets,
            }
        };
        let pal = palette(ui);
        let scroll = self.diff_dock.scroll.clone();
        let chip_row = self
            .diff_dock
            .hover
            .as_ref()
            .filter(|hover| hover.split == split)
            .map(|hover| hover.chip_row);

        // Custom direct-paint element hosted in a scroll-tracked div, exactly
        // like the Review view (`diff/view/render.rs`): `overflow_y_scroll` so
        // GPUI's native handler owns VERTICAL - it translates the child's origin
        // by the scroll offset, which is the ONLY thing that moves `DiffElement`
        // (it positions every row off its prepainted `bounds.origin`, never off
        // `window.element_offset()`; under `overflow_hidden` that origin never
        // moves and the body looks frozen). `track_scroll` keeps the handle's
        // `offset()`/`bounds()`/`max_offset()` live for the click→row mapping.
        //
        // `restrict_scroll_to_axis = Some(true)` is the Zed opt-in (style.rs doc;
        // used by markdown.rs / thread_view.rs / data_table.rs) that stops a
        // vertical wheel from bleeding into a horizontally-scrollable child - and,
        // crucially here, stops the native Y handler back-filling `delta_y` from
        // `delta.x` under Shift+wheel (div.rs: the `else if !restrict_scroll_to_axis
        // && overflow.x != Scroll` fallback). On Linux/Windows the platform layer
        // already swaps Shift+wheel onto the X axis (delta.x set, delta.y zeroed),
        // so without this flag a Shift gesture would scroll the list vertically.
        // With it: Shift → native does nothing, our handler scrolls horizontal.
        //
        // HORIZONTAL stays per-file and fully custom: `overflow.x` is Hidden, so
        // the native handler never touches X; `apply_diff_dock_wheel` reads
        // `delta.x` (the platform-swapped Shift value, or a trackpad swipe) and
        // shifts the file under the cursor. A body click toggles a file's collapse.
        let mut element = div()
            .id("diff-dock-scroll")
            .min_w_0()
            .flex_1()
            .min_h_0()
            .w_full()
            .overflow_y_scroll()
            .track_scroll(&scroll)
            .on_click(cx.listener(|this, ev: &ClickEvent, _w, cx| {
                this.handle_diff_dock_body_click(ev, cx);
            }))
            .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _w, cx| {
                this.update_diff_dock_hover(ev.position, cx);
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, ev: &MouseDownEvent, _w, cx| {
                    if this.handle_diff_dock_revert_click(ev.position, cx) {
                        cx.stop_propagation();
                        return;
                    }
                    let split = this.diff_dock.split;
                    if this.handle_diff_dock_h_scrollbar_mouse_down(ev.position, split, cx) {
                        cx.stop_propagation();
                    }
                }),
            )
            .on_scroll_wheel(cx.listener(|this, ev: &ScrollWheelEvent, window, cx| {
                this.apply_diff_dock_wheel(ev, window, cx);
            }))
            .child(DiffElement::new(body, pal).with_revert_chip(chip_row));
        // Not exposed as a builder method on the pinned fork - set on the style
        // refinement directly, the same raw mutation Zed uses.
        element.style().restrict_scroll_to_axis = Some(true);

        // The permanent editor-style scrollbar (#434) sits in its own 15 px
        // gutter beside the scroll host, not over it: `min_w_0` on the host
        // lets the flex row shrink it so the gutter is never squeezed out.
        div()
            .id("diff-dock-body")
            .flex_1()
            .min_h_0()
            .w_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .child(element)
                    .child(self.diff_dock.vertical_scrollbar.render(&scroll, cx)),
            )
            .into_any_element()
    }

    /// Wheel gesture over the diff body. Handles ONLY the per-file HORIZONTAL
    /// axis; the host's `overflow_y_scroll` native handler owns vertical (it both
    /// translates the `DiffElement` and updates the scroll handle - duplicating
    /// it here would double-scroll the list). `restrict_scroll_to_axis` on the
    /// host keeps the native handler off the X axis and stops it bleeding a Shift
    /// gesture into vertical - see the host comment in `render_diff_dock_body`.
    ///
    /// The horizontal delta always arrives on `delta.x`: the platform layer swaps
    /// Shift+wheel onto X (Linux X11/Wayland + Windows; `delta.y` is zeroed under
    /// Shift), and a trackpad horizontal swipe is natively on X. So read `delta.x`
    /// unconditionally - no `modifiers.shift` branch. A bare `delta.y` (plain
    /// vertical wheel) is ignored here and handled natively.
    fn apply_diff_dock_wheel(
        &mut self,
        ev: &ScrollWheelEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let delta = ev.delta.pixel_delta(window.line_height());
        let dx = f32::from(delta.x);
        if dx != 0.0 {
            let split = self.diff_dock.split;
            let bounds = self.diff_dock.scroll.bounds();
            if ev.position.x < bounds.left()
                || ev.position.x > bounds.right()
                || ev.position.y < bounds.top()
                || ev.position.y > bounds.bottom()
            {
                return;
            }
            let width = f32::from(bounds.size.width);
            let content_y =
                f32::from(ev.position.y - bounds.top() - self.diff_dock.scroll.offset().y).max(0.0);
            if let Some(file_idx) = self.diff_dock_file_at_content_y(content_y, split) {
                let spans = self.diff_dock_spans(split);
                let local_x = f32::from(ev.position.x - bounds.left());
                let right = split && split_right_side_at_x(local_x, width);
                let offset_idx = h_offset_index(spans.len(), file_idx, split, right);
                let cur = self
                    .diff_dock
                    .h_offsets
                    .get(offset_idx)
                    .copied()
                    .unwrap_or(0.0);
                // GPUI scroll deltas go negative toward the end; subtract to grow
                // our positive offset and reveal the right of the line.
                set_file_side_offset(
                    std::rc::Rc::make_mut(&mut self.diff_dock.h_offsets),
                    &spans,
                    file_idx,
                    right,
                    cur - dx,
                    split,
                    width,
                );
            }
            // Only horizontal scroll mutates our state here; vertical is native
            // (it notifies on its own). Notifying unconditionally would double-
            // render every plain vertical wheel tick.
            cx.notify();
        }
    }

    fn diff_dock_scrollbar_segments(&self, split: bool) -> Option<Vec<HScrollbarSegment>> {
        let data = self.diff_dock.data.as_ref()?;
        let bounds = self.diff_dock.scroll.bounds();
        let viewport_h = f32::from(bounds.size.height);
        let panel_width = f32::from(bounds.size.width);
        if viewport_h <= 0.0 || panel_width <= 0.0 {
            return None;
        }

        let visible_top = f32::from(-self.diff_dock.scroll.offset().y).max(0.0);
        let visible_bottom = visible_top + viewport_h;
        let (offsets, spans) = if split {
            (&data.disp_split_offsets, &data.disp_split_spans)
        } else {
            (&data.disp_unified_offsets, &data.disp_unified_spans)
        };
        Some(h_scrollbar_segments(
            spans,
            offsets,
            self.diff_dock.h_offsets.as_ref(),
            split,
            panel_width,
            visible_top,
            visible_bottom,
        ))
    }

    fn diff_dock_h_scrollbar_local_point(&self, point: Point<Pixels>) -> Option<(f32, f32)> {
        let bounds = self.diff_dock.scroll.bounds();
        if point.x < bounds.left()
            || point.x > bounds.right()
            || point.y < bounds.top()
            || point.y > bounds.bottom()
        {
            return None;
        }
        Some((
            f32::from(point.x - bounds.left()),
            f32::from(point.y - bounds.top() - self.diff_dock.scroll.offset().y).max(0.0),
        ))
    }

    fn handle_diff_dock_h_scrollbar_mouse_down(
        &mut self,
        point: Point<Pixels>,
        split: bool,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some((x, y)) = self.diff_dock_h_scrollbar_local_point(point) else {
            return false;
        };
        let Some(segments) = self.diff_dock_scrollbar_segments(split) else {
            return false;
        };
        let Some(segment) = segments.iter().find(|segment| {
            x >= segment.x
                && x <= segment.x + segment.width
                && y >= segment.y
                && y <= segment.y + H_SCROLLBAR_TRACK_HEIGHT
        }) else {
            return false;
        };

        let thumb_left = segment.x + segment.thumb_x;
        let thumb_right = thumb_left + segment.thumb_width;
        let target = if x >= thumb_left && x <= thumb_right {
            segment.offset
        } else {
            h_scrollbar_click_offset(&segments, x, y)
                .map(|(_, offset)| offset)
                .unwrap_or(segment.offset)
        }
        .clamp(0.0, segment.max_scroll);

        self.set_diff_dock_h_offset(segment.offset_idx, target);
        self.diff_dock.h_scroll_drag = Some(DiffDockHScrollDrag {
            offset_idx: segment.offset_idx,
            start_mouse_x: point.x,
            start_offset: target,
            max_scroll: segment.max_scroll,
            track_width: segment.width,
            thumb_width: segment.thumb_width,
        });
        cx.notify();
        true
    }

    fn handle_diff_dock_h_scrollbar_click(
        &mut self,
        point: Point<Pixels>,
        split: bool,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some((x, y)) = self.diff_dock_h_scrollbar_local_point(point) else {
            return false;
        };
        let Some(segments) = self.diff_dock_scrollbar_segments(split) else {
            return false;
        };
        let Some(segment) = segments.iter().find(|segment| {
            x >= segment.x
                && x <= segment.x + segment.width
                && y >= segment.y
                && y <= segment.y + H_SCROLLBAR_TRACK_HEIGHT
        }) else {
            return false;
        };

        let thumb_left = segment.x + segment.thumb_x;
        let thumb_right = thumb_left + segment.thumb_width;
        if x >= thumb_left && x <= thumb_right {
            return true;
        }

        let target = h_scrollbar_click_offset(&segments, x, y)
            .map(|(_, offset)| offset)
            .unwrap_or(segment.offset)
            .clamp(0.0, segment.max_scroll);
        self.set_diff_dock_h_offset(segment.offset_idx, target);
        cx.notify();
        true
    }

    fn set_diff_dock_h_offset(&mut self, offset_idx: usize, value: f32) {
        let offsets = std::rc::Rc::make_mut(&mut self.diff_dock.h_offsets);
        if offsets.len() <= offset_idx {
            offsets.resize(offset_idx + 1, 0.0);
        }
        if let Some(slot) = offsets.get_mut(offset_idx) {
            *slot = value;
        }
    }

    pub(crate) fn drag_diff_dock_h_scrollbar(&mut self, mouse_x: Pixels, cx: &mut Context<Self>) {
        let Some(drag) = self.diff_dock.h_scroll_drag else {
            return;
        };

        let track_range = (drag.track_width - drag.thumb_width).max(1.0);
        let delta = f32::from(mouse_x - drag.start_mouse_x);
        let next =
            (drag.start_offset + delta * drag.max_scroll / track_range).clamp(0.0, drag.max_scroll);
        if self
            .diff_dock
            .h_offsets
            .get(drag.offset_idx)
            .is_none_or(|current| (*current - next).abs() > 0.1)
        {
            self.set_diff_dock_h_offset(drag.offset_idx, next);
            cx.notify();
        }
    }

    pub(crate) fn end_diff_dock_h_scrollbar_drag(&mut self, cx: &mut Context<Self>) {
        if self.diff_dock.h_scroll_drag.take().is_some() {
            cx.notify();
        }
    }

    /// File owning the display row at content pixel `content_y` (0 = first row),
    /// in the current view mode. `None` when there is no diff or `content_y`
    /// lands past the last row.
    fn diff_dock_file_at_content_y(&self, content_y: f32, split: bool) -> Option<usize> {
        let data = self.diff_dock.data.as_ref()?;
        let (offsets, spans) = if split {
            (&data.disp_split_offsets, &data.disp_split_spans)
        } else {
            (&data.disp_unified_offsets, &data.disp_unified_spans)
        };
        let row = row_at_offset(offsets, content_y)?;
        file_at_row(spans, row)
    }

    /// The per-file scroll spans for the current view mode (empty `Rc` when no
    /// diff is loaded), cloned for the wheel handler that mutates offsets.
    fn diff_dock_spans(&self, split: bool) -> std::rc::Rc<Vec<crate::diff::FileSpan>> {
        self.diff_dock
            .data
            .as_ref()
            .map(|d| {
                if split {
                    d.disp_split_spans.clone()
                } else {
                    d.disp_unified_spans.clone()
                }
            })
            .unwrap_or_default()
    }

    /// Map a body click to a row and, if it landed on a file header, toggle that
    /// file's collapse. Mirrors the Review view's header-collapse path (the dock
    /// has no click-to-ask, so a non-header click is a no-op).
    fn handle_diff_dock_body_click(&mut self, ev: &ClickEvent, cx: &mut Context<Self>) {
        // Revert is handled on mouse-down. `stop_propagation` there does not
        // cancel this `on_click`, and a second revert would write twice then
        // toast STALE_FILE_MESSAGE against the stamp the first write just
        // replaced.
        let split = self.diff_dock.split;
        if self.handle_diff_dock_h_scrollbar_click(ev.position(), split, cx) {
            return;
        }

        let row = {
            let Some(data) = self.diff_dock.data.as_ref() else {
                return;
            };
            let bounds = self.diff_dock.scroll.bounds();
            let y = ev.position().y;
            if y < bounds.top() || y > bounds.bottom() {
                return;
            }
            let target = f32::from(y - bounds.top() - self.diff_dock.scroll.offset().y).max(0.0);
            let offsets = if split {
                &data.disp_split_offsets
            } else {
                &data.disp_unified_offsets
            };
            let Some(row) = row_at_offset(offsets, target) else {
                return; // click past the last row
            };
            row
        };

        let fold_key = {
            let Some(data) = self.diff_dock.data.as_ref() else {
                return;
            };
            if split {
                match data.disp_split.get(row) {
                    Some(SplitRow::Fold(fold)) => Some(fold.key.to_string()),
                    _ => None,
                }
            } else {
                data.disp_unified
                    .get(row)
                    .filter(|r| r.kind == RowKind::Fold)
                    .and_then(|r| r.fold_key.as_ref())
                    .map(|key| key.to_string())
            }
        };
        if let Some(key) = fold_key {
            if !self.diff_dock.expanded_folds.remove(&key) {
                self.diff_dock.expanded_folds.insert(key);
            }
            self.recompute_diff_dock_display();
            cx.notify();
            return;
        }

        let path = {
            let Some(data) = self.diff_dock.data.as_ref() else {
                return;
            };
            let anchors = if split {
                &data.disp_anchors_split
            } else {
                &data.disp_anchors_unified
            };
            anchors
                .iter()
                .find(|(_, i)| *i == row)
                .map(|(p, _)| p.clone())
        };
        let Some(path) = path else {
            return; // not a file header - nothing to collapse
        };
        self.toggle_diff_file_collapsed(path, cx);
    }
}

/// Dock placement shares one breadcrumb band across editor and tree. Rail
/// placement continues to use the existing 36 px file header above.
fn render_dock_file_toolbar(
    root: &str,
    file: Option<(&'static str, String, usize, usize, AnyElement)>,
    toggle: AnyElement,
    ui: crate::theme::UiColors,
) -> AnyElement {
    let project = std::path::Path::new(root)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| root.to_string());
    let mut row = div()
        .id("diff-file-breadcrumb-toolbar")
        .flex_none()
        .h(px(40.))
        .w_full()
        .flex()
        .items_center()
        .gap(px(6.))
        .px(px(10.))
        .border_b_1()
        .border_color(ui.border);
    let (path, position, controls) = match file {
        Some((icon, path, line, column, controls)) => {
            row = row.child(if icon.starts_with("icons/languages/") {
                gpui::img(icon).size(px(14.)).flex_none().into_any_element()
            } else {
                gpui::svg()
                    .path(icon)
                    .size(px(14.))
                    .flex_none()
                    .text_color(ui.muted)
                    .into_any_element()
            });
            (
                format!("{project} › {}", path.replace('/', " › ")),
                Some(format!("Ln {line}, Col {column}")),
                Some(controls),
            )
        }
        None => (project, None, None),
    };
    row.child(
        div()
            .flex_1()
            .min_w_0()
            .overflow_hidden()
            .whitespace_nowrap()
            .text_ellipsis()
            .text_size(crate::ui_primitives::BODY)
            .text_color(ui.text)
            .child(path),
    )
    .children(position.map(|position| {
        div()
            .flex_none()
            .text_size(crate::ui_primitives::BODY)
            .text_color(ui.muted)
            .child(position)
    }))
    .children(controls)
    .child(toggle)
    .into_any_element()
}

/// The editor and shared Files panel below their common toolbar.
fn render_dock_file_split(
    body: AnyElement,
    tree: AnyElement,
    files_width: f32,
    ui: crate::theme::UiColors,
) -> AnyElement {
    div()
        .flex()
        .flex_row()
        .flex_1()
        .min_h_0()
        .min_w_0()
        .child(
            div()
                // CodeView fills a flex parent; a block host collapses its
                // percentage-height viewport even when this box is full height.
                .flex()
                .flex_col()
                .flex_1()
                .min_w(px(200.))
                .h_full()
                .overflow_hidden()
                .child(body),
        )
        .child(
            div()
                .w(px(files_width))
                .flex_shrink_0()
                .h_full()
                .relative()
                .child(tree)
                .child(
                    div()
                        .absolute()
                        .left_0()
                        .top_0()
                        .bottom_0()
                        .w(px(1.))
                        .bg(ui.border),
                ),
        )
        .into_any_element()
}

#[cfg(test)]
mod dock_file_layout_tests {
    use super::code::{element::visible_rows_at, view::CodeView};
    use super::*;
    use crate::app::files_sidebar::FilesSidebar;
    use gpui::{AppContext, Entity, Render, TestAppContext, size};

    struct FileDock {
        editor: Entity<CodeView>,
        tree: Entity<FilesSidebar>,
    }

    impl Render for FileDock {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let ui = crate::theme::ui_colors();
            div()
                .flex()
                .flex_col()
                .size_full()
                .child(render_dock_file_toolbar(
                    "/workspace",
                    None,
                    div().into_any_element(),
                    ui,
                ))
                .child(render_dock_file_split(
                    self.editor.clone().into_any_element(),
                    self.tree.clone().into_any_element(),
                    250.,
                    ui,
                ))
        }
    }

    #[gpui::test]
    fn dock_tree_split_keeps_loaded_editor_viewport_at_full_body_height(cx: &mut TestAppContext) {
        let text = "let value = 1;\n".repeat(100);
        let (host, cx) = cx.add_window_view(move |_, cx| FileDock {
            editor: cx.new(|cx| CodeView::ready_for_test("/workspace/main.rs".into(), &text, cx)),
            tree: cx.new(|cx| {
                let mut panel = FilesSidebar::new(cx);
                panel.set_chrome(false, true, true, cx);
                panel
            }),
        });
        let editor = host.read_with(cx, |host, _| host.editor.clone());
        // The exact tree/editor width boundary and a wider, taller dock both
        // render the actual loaded editor through the production split builder.
        for (width, height) in [(450., 400.), (800., 640.)] {
            cx.simulate_resize(size(px(width), px(height)));
            cx.update(|window, cx| {
                window.refresh();
                window.draw(cx).clear(cx);
            });
            editor.read_with(cx, |editor, _| {
                let lines = editor.document().expect("loaded source document").line_count();
                assert_eq!(editor.visible_row_range(), visible_rows_at(0., height - 40., lines),
                    "the loaded editor must fill the body below the 40 px toolbar at {width} by {height}");
                // A resize can warm shaping before this explicit draw, so inspect
                // retained row geometry rather than cache misses in this frame.
                assert!(editor.row_width(10).is_some_and(|width| width > 0.),
                    "the viewport must retain shaped source rows");
            });
        }
    }
}
