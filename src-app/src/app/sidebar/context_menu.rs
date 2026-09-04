//! Context-menu row helpers shared between the sidebar workspace menu and the
//! title-bar burger menu. Includes the action-name shortcut lookup used
//! to render the keyboard-shortcut label next to each action.
//!
//! Part of the US-025 sidebar decomposition.

use std::path::PathBuf;

use gpui::{
    AnyElement, App, ClickEvent, ClipboardItem, Context, CursorStyle, InteractiveElement,
    IntoElement, MouseButton, ParentElement, Pixels, SharedString, Styled, Window, deferred, div,
    point, prelude::*, px,
};

use crate::app::files_tree;
use crate::editor::WorkspaceEditor;
use crate::pane::PaneSurface;
use crate::settings::components::{menu_divider_color, select_item, select_menu, with_alpha};
use crate::ui_primitives::AnimatedHoverExt;
use crate::{PaneContextMenu, PaneFlowApp, TabContextMenu, WorkspaceContextMenu};

fn context_menu_divider(ui: crate::theme::UiColors) -> gpui::Div {
    div()
        .mx(px(6.))
        .my(px(4.))
        .h(px(1.))
        .bg(menu_divider_color(ui))
}

/// Fixed rows are pin/unpin, reveal, copy path, manage custom buttons, and
/// close. The divider before Reveal only exists when an editor section does;
/// otherwise the earlier workflow/service divider already separates groups.
fn workspace_context_menu_counts(
    visible_editor_rows: usize,
    workflow_rows: usize,
    service_rows: usize,
) -> (usize, usize) {
    let menu_rows = visible_editor_rows + 5 + workflow_rows + service_rows;
    let separator_rows = 2 + usize::from(service_rows > 0) + usize::from(visible_editor_rows > 0);
    (menu_rows, separator_rows)
}

/// The tallest a select menu is drawn: `select_menu` caps its surface at this
/// and scrolls the rows inside it (`settings/components.rs`).
const SELECT_MENU_MAX_HEIGHT: f32 = 320.;

/// Height of the tab context menu as it is actually drawn: the menu chrome,
/// its two fixed rows, when `branch_rows` is non-zero the Branch section
/// (header plus one row per branch), and when `remove_row` the "Remove
/// worktree" row a bound tab carries (issue #348; its divider is folded into
/// the row it introduces, like the Branch header), all capped at the
/// surface's own ceiling. The on-screen clamp has to measure what is painted:
/// sizing a forty-branch list at its uncapped height pinned the menu at
/// `y = 0` (issue #347).
pub(crate) fn tab_context_menu_height(branch_rows: usize, remove_row: bool) -> Pixels {
    let branch_rows = if branch_rows > 0 {
        1. + branch_rows as f32
    } else {
        0.
    };
    let remove_rows = if remove_row { 1. } else { 0. };
    px((8. + (2. + branch_rows + remove_rows) * 28.).min(SELECT_MENU_MAX_HEIGHT))
}

pub(crate) fn clamped_context_menu_position(
    position: gpui::Point<Pixels>,
    width: Pixels,
    height: Pixels,
    window: &Window,
) -> gpui::Point<Pixels> {
    let win_size = window.window_bounds().get_bounds().size;
    let x = if position.x + width > win_size.width {
        (position.x - width).max(px(0.))
    } else {
        position.x
    };
    let y = if position.y + height > win_size.height {
        (position.y - height).max(px(0.))
    } else {
        position.y
    };
    point(x, y)
}

impl PaneFlowApp {
    pub(crate) fn shortcut_for_action(&self, action_name: &str) -> Option<&str> {
        self.effective_shortcuts
            .iter()
            .find(|entry| entry.action_name == action_name && entry.key != "Unassigned")
            .map(|entry| entry.key.as_str())
    }

    pub(crate) fn render_context_menu_item(
        &self,
        id: SharedString,
        label: &str,
        shortcut: Option<SharedString>,
        ui: crate::theme::UiColors,
        on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> impl IntoElement {
        div()
            .id(id)
            .flex()
            .items_center()
            .justify_between()
            .gap(px(10.))
            .px(px(8.))
            .py(px(5.))
            .rounded(px(4.))
            .text_size(px(11.))
            .text_color(ui.text)
            .animated_hover_bg(ui.subtle.opacity(0.0), ui.subtle)
            .on_click(on_click)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_x_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .child(label.to_string()),
            )
            .when_some(shortcut, |d, shortcut| {
                d.child(
                    div()
                        .flex_none()
                        .text_size(px(10.))
                        .text_color(ui.muted)
                        .child(shortcut),
                )
            })
    }

    /// One workspace-menu row in the shared Settings "Shell" select look
    /// (`components::select_item`): 28px tall, 7px radius, 12px label flex-filled
    /// with the optional shortcut pinned right, and the whisper hover highlight
    /// (`text @ 0.05`) instead of the older flat `ui.subtle`. Keeps every app
    /// menu reading as one consistent menu language.
    pub(crate) fn render_select_menu_item(
        &self,
        id: SharedString,
        label: &str,
        shortcut: Option<SharedString>,
        ui: crate::theme::UiColors,
        on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> impl IntoElement {
        select_item(id, false, ui)
            .cursor(CursorStyle::Arrow)
            .on_click(on_click)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_x_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_color(ui.text)
                    .child(label.to_string()),
            )
            .when_some(shortcut, |d, shortcut| {
                d.child(
                    div()
                        .flex_none()
                        .text_size(px(10.))
                        .text_color(ui.muted)
                        .child(shortcut),
                )
            })
    }

    pub(crate) fn open_workspace_service_url(&mut self, url: &str, cx: &mut Context<Self>) {
        if let Err(err) = crate::external_open::open_url(url) {
            let message = if err.kind() == std::io::ErrorKind::NotFound {
                "Could not open URL - check that /usr/bin/open exists, or set a default browser"
                    .to_string()
            } else {
                format!("Could not open URL: {err}")
            };
            log::warn!("sidebar: open URL failed: {err}");
            self.show_toast(message, cx);
        }
    }

    /// Build the deferred element that paints the right-click workspace
    /// context menu. Caller is responsible for the
    /// `if let Some(menu) = self.workspace_menu_open && menu.idx < self.workspaces.len()`
    /// guard. Extracted from `main.rs` per US-002.
    pub(crate) fn render_workspace_context_menu(
        &self,
        menu: WorkspaceContextMenu,
        ui: crate::theme::UiColors,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let idx = menu.idx;
        let can_close = !self.workspaces.is_empty();
        let workflow_template = self.workspace_template_for_workspace(idx);
        let services: Vec<_> = self
            .workspaces
            .get(idx)
            .map(|workspace| {
                workspace
                    .active_ports
                    .iter()
                    .filter_map(|port| {
                        workspace
                            .service_labels
                            .get(port)
                            .cloned()
                            .map(|info| (*port, info))
                    })
                    .collect()
            })
            .unwrap_or_default();

        let is_pinned = self.workspaces.get(idx).is_some_and(|ws| ws.pinned);
        let workflow_rows = usize::from(workflow_template.is_some());
        let service_rows = services.len();
        let visible_editors: Vec<_> = WorkspaceEditor::ALL
            .into_iter()
            .filter(|editor| editor.is_visible(&self.cached_config))
            .collect();
        let (menu_rows, separator_rows) =
            workspace_context_menu_counts(visible_editors.len(), workflow_rows, service_rows);
        let menu_height = px(8. + menu_rows as f32 * 28. + separator_rows as f32 * 9.);
        let menu_pos = clamped_context_menu_position(menu.position, px(248.), menu_height, window);

        let mut context_menu = select_menu("workspace-context-menu", ui)
            .occlude()
            .absolute()
            .left(menu_pos.x)
            .top(menu_pos.y)
            .w(px(248.))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.workspace_menu_open = None;
                cx.notify();
            }))
            .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation());

        // Issue #107: first row, so the pin is the cheapest thing to reach on a
        // row the user just right-clicked. Toggling persists (`save_session`);
        // the star on the row is the state it reports back.
        context_menu = context_menu.child(self.render_select_menu_item(
            "workspace-context-pin".into(),
            if is_pinned { "Unpin" } else { "Pin" },
            None,
            ui,
            cx.listener(move |this, _: &ClickEvent, _window, cx| {
                this.workspace_menu_open = None;
                this.toggle_pin_workspace(idx, cx);
                cx.stop_propagation();
            }),
        ));

        if let Some(template_idx) = workflow_template {
            context_menu = context_menu.child(self.render_select_menu_item(
                "workspace-context-run-workflow".into(),
                "Run Workflow",
                None,
                ui,
                cx.listener(move |this, _: &ClickEvent, _window, cx| {
                    this.workspace_menu_open = None;
                    this.run_saved_workspace_template_for_workspace(idx, template_idx, cx);
                    cx.stop_propagation();
                }),
            ));
        }

        // Unconditional since issue #107: the pin row always sits above this
        // rule, so it can no longer open the menu on a leading divider - which
        // is the only reason it used to be gated on "Run Workflow" existing.
        context_menu = context_menu.child(context_menu_divider(ui));

        for (port, info) in services {
            let service_name = info
                .label
                .clone()
                .unwrap_or_else(|| "Local service".to_string());
            if info.is_frontend {
                let label = format!("Open {service_name} :{port}");
                let url = info
                    .url
                    .clone()
                    .unwrap_or_else(|| format!("http://localhost:{port}"));
                context_menu = context_menu.child(self.render_select_menu_item(
                    SharedString::from(format!("workspace-context-service-{port}")),
                    &label,
                    None,
                    ui,
                    cx.listener(move |this, _: &ClickEvent, _window, cx| {
                        this.workspace_menu_open = None;
                        this.open_workspace_service_url(&url, cx);
                        cx.stop_propagation();
                    }),
                ));
            } else {
                context_menu = context_menu.child(Self::render_disabled_select_menu_item(
                    SharedString::from(format!("workspace-context-service-{port}-info")),
                    &format!("{service_name} :{port}"),
                    ui,
                ));
            }
        }

        if service_rows > 0 {
            context_menu = context_menu.child(context_menu_divider(ui));
        }

        for editor in &visible_editors {
            let shortcut = self
                .shortcut_for_action(editor.shortcut_action())
                .map(|s| SharedString::from(s.to_string()));
            let command = editor.command().to_string();
            let label_owned = editor.menu_label().to_string();
            context_menu = context_menu.child(self.render_select_menu_item(
                SharedString::from(format!("workspace-context-{}", editor.id())),
                editor.menu_label(),
                shortcut,
                ui,
                cx.listener(move |this, _: &ClickEvent, _window, cx| {
                    this.open_workspace_in_editor(idx, &command, &label_owned, cx);
                    cx.stop_propagation();
                }),
            ));
        }

        if !visible_editors.is_empty() {
            context_menu = context_menu.child(context_menu_divider(ui));
        }

        // Reveal in file manager
        let reveal_shortcut = self
            .shortcut_for_action("reveal_workspace_in_file_manager")
            .map(|s| SharedString::from(s.to_string()));
        context_menu = context_menu.child(self.render_select_menu_item(
            "workspace-context-reveal".into(),
            "Reveal in File Manager",
            reveal_shortcut,
            ui,
            cx.listener(move |this, _: &ClickEvent, _window, cx| {
                this.reveal_workspace_in_file_manager(idx, cx);
                cx.stop_propagation();
            }),
        ));

        // Copy path
        let copy_shortcut = self
            .shortcut_for_action("copy_workspace_path")
            .map(|s| SharedString::from(s.to_string()));
        context_menu = context_menu.child(self.render_select_menu_item(
            "workspace-context-copy".into(),
            "Copy Path",
            copy_shortcut,
            ui,
            cx.listener(move |this, _: &ClickEvent, _window, cx| {
                this.copy_workspace_path(idx, cx);
                cx.stop_propagation();
            }),
        ));

        // Manage Custom Buttons - opens the per-workspace button editor modal.
        context_menu = context_menu.child(self.render_select_menu_item(
            "workspace-context-custom-buttons".into(),
            "Manage Custom Buttons…",
            None,
            ui,
            cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.open_custom_buttons_modal(idx, window, cx);
                cx.stop_propagation();
            }),
        ));

        context_menu = context_menu.child(context_menu_divider(ui));

        // Close workspace (conditionally disabled)
        let close_shortcut = self
            .shortcut_for_action("close_workspace")
            .map(|s| SharedString::from(s.to_string()));
        context_menu = context_menu.child({
            let hover_bg = with_alpha(ui.text, 0.05);
            let target_bg = if can_close {
                hover_bg
            } else {
                hover_bg.opacity(0.0)
            };
            div()
                .id("workspace-context-close")
                .h(px(28.))
                .px(px(8.))
                .rounded(px(7.))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.))
                .text_size(px(12.))
                .text_color(ui.muted)
                .when(can_close, |d| d.text_color(ui.text))
                .animated_hover_bg(hover_bg.opacity(0.0), target_bg)
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    cx.stop_propagation();
                    if can_close {
                        this.request_close_workspace(
                            idx,
                            crate::app::close_guard::ConfirmStyle::Modal,
                            window,
                            cx,
                        );
                    } else {
                        this.workspace_menu_open = None;
                        cx.notify();
                    }
                }))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_x_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .child("Close Workspace"),
                )
                .when_some(close_shortcut, |d, shortcut| {
                    d.child(
                        div()
                            .flex_none()
                            .text_size(px(10.))
                            .text_color(ui.muted)
                            .child(shortcut),
                    )
                })
        });

        deferred(context_menu).priority(3).into_any_element()
    }

    /// Build the deferred pane context menu, anchored on the pane header
    /// (EP-002 US-007). A pane is mono-surface, so the former "Move to pane…"
    /// entry is gone along with the tab strip that anchored it: what remains
    /// are the surface actions (copy path, cancel a queued prompt, close the
    /// pane). No dead or disabled move entry is left behind.
    /// US-010: right-click menu on a sidebar tab row. Two entries - Rename and
    /// Close - in the shared select-menu language of the workspace menu. Close
    /// keeps FR-01: the last tab of a workspace is replaced by an empty one,
    /// the workspace itself is never closed from here.
    pub(crate) fn render_tab_context_menu(
        &self,
        menu: TabContextMenu,
        ui: crate::theme::UiColors,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let TabContextMenu {
            ws_idx,
            tab_idx,
            position,
        } = menu;
        // Issue #347: the branches this tab can be moved to, one row each,
        // exactly what the New pane picker offers - a branch with no worktree
        // yet gets one when it is chosen. The repository's own branch is in
        // there like any other, and picking it unbinds the tab.
        let branches = self.tab_menu_branch_rows(ws_idx, tab_idx);
        // A single branch is where the tab already works: the section would
        // only restate it.
        let show_branches = branches.len() > 1;
        // Issue #348: only a bound tab has a checkout to remove. Built here
        // rather than inside the `when_some` below because
        // `render_select_menu_item` needs `self`, like every other item.
        let is_bound = self
            .workspaces
            .get(ws_idx)
            .and_then(|ws| ws.tabs().get(tab_idx))
            .is_some_and(|tab| tab.worktree.is_some());
        let menu_height =
            tab_context_menu_height(if show_branches { branches.len() } else { 0 }, is_bound);
        let menu_pos = clamped_context_menu_position(position, px(248.), menu_height, window);
        let close_shortcut = self
            .shortcut_for_action("close_tab")
            .map(|key| SharedString::from(key.to_string()));
        let remove_worktree_item = is_bound.then(|| {
            self.render_select_menu_item(
                "tab-context-remove-worktree".into(),
                "Remove worktree",
                None,
                ui,
                cx.listener(move |this, _: &ClickEvent, _window, cx| {
                    this.tab_menu_open = None;
                    this.remove_tab_worktree(ws_idx, tab_idx, cx);
                    cx.stop_propagation();
                }),
            )
        });

        select_menu("tab-context-menu", ui)
            .occlude()
            .absolute()
            .left(menu_pos.x)
            .top(menu_pos.y)
            .w(px(248.))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.tab_menu_open = None;
                cx.notify();
            }))
            .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
            .child(self.render_select_menu_item(
                "tab-context-rename".into(),
                "Rename",
                None,
                ui,
                cx.listener(move |this, _: &ClickEvent, window, cx| {
                    this.tab_menu_open = None;
                    this.begin_tab_rename(ws_idx, tab_idx, window, cx);
                    cx.stop_propagation();
                }),
            ))
            .child(self.render_select_menu_item(
                "tab-context-close".into(),
                "Close",
                close_shortcut,
                ui,
                cx.listener(move |this, _: &ClickEvent, window, cx| {
                    this.tab_menu_open = None;
                    // Issue #83: ask first when this tab holds a live agent.
                    this.request_close_workspace_tab(
                        ws_idx,
                        tab_idx,
                        crate::app::close_guard::ConfirmStyle::Modal,
                        window,
                        cx,
                    );
                    cx.stop_propagation();
                }),
            ))
            .when(show_branches, |menu| {
                let mut menu = menu.child(context_menu_divider(ui)).child(
                    div()
                        .px(px(8.))
                        .pb(px(4.))
                        .text_size(px(10.))
                        .text_color(ui.muted)
                        .child("Branch"),
                );
                for (detached, label, selected) in branches {
                    let branch = label.clone();
                    menu = menu.child(
                        select_item(
                            SharedString::from(format!("tab-branch-{label}")),
                            selected,
                            ui,
                        )
                        .cursor(CursorStyle::Arrow)
                        .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                            this.tab_menu_open = None;
                            match detached.clone() {
                                Some(path) => {
                                    this.bind_tab_to_checkout(ws_idx, tab_idx, path, cx);
                                }
                                None => {
                                    this.bind_tab_to_branch(ws_idx, tab_idx, branch.clone(), cx)
                                }
                            }
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
                                .child(label),
                        ),
                    );
                }
                menu
            })
            .when_some(remove_worktree_item, |menu, item| {
                menu.child(context_menu_divider(ui)).child(item)
            })
            .into_any_element()
    }

    /// The rows of the tab menu's Branch section: `(detached checkout path,
    /// label, selected)`. A branch row carries `None` and is bound through
    /// [`PaneFlowApp::bind_tab_to_branch`]; a detached checkout has no branch
    /// to resolve and is bound to its path directly. Empty outside a
    /// repository.
    fn tab_menu_branch_rows(
        &self,
        ws_idx: usize,
        tab_idx: usize,
    ) -> Vec<(Option<PathBuf>, String, bool)> {
        let Some(ws) = self.workspaces.get(ws_idx) else {
            return Vec::new();
        };
        let Some(root) = ws.repo_root.clone() else {
            return Vec::new();
        };
        let bound = ws.tabs().get(tab_idx).and_then(|tab| tab.worktree.clone());
        let listing = self.workspace_worktree_listing(ws_idx);
        let on_branch = match bound.as_ref() {
            Some(path) => listing
                .iter()
                .find(|entry| entry.path == *path)
                .and_then(|entry| entry.branch.clone()),
            None => Some(self.workspace_checkout_label(ws_idx)),
        };
        // A branch whose checkout this tab may not stand on - one another
        // workspace owns, or one being retired - is not offered at all: a row
        // that can only refuse is a row that should not be there.
        let bindable = |entry: &crate::workspace::worktree::WorktreeEntry| {
            entry.path == root || self.checkout_is_bindable(&entry.path, ws_idx)
        };
        let mut rows: Vec<(Option<PathBuf>, String, bool)> = self
            .workspace_branches(ws_idx)
            .iter()
            .filter(|branch| {
                listing
                    .iter()
                    .find(|entry| entry.branch.as_deref() == Some(branch.as_str()))
                    .is_none_or(bindable)
            })
            .map(|branch| {
                let selected = on_branch.as_deref() == Some(branch.as_str());
                (None, branch.clone(), selected)
            })
            .collect();
        // A detached checkout is under no branch, and dropping it would strand
        // a tab already bound to one.
        rows.extend(
            listing
                .iter()
                .filter(|entry| entry.branch.is_none() && !entry.is_bare && entry.path != root)
                .filter(|entry| bindable(entry))
                .map(|entry| {
                    let label =
                        crate::workspace::worktree::checkout_label(None, &entry.path, &root);
                    let selected = bound.as_deref() == Some(entry.path.as_path());
                    (Some(entry.path.clone()), label, selected)
                }),
        );
        rows
    }

    pub(crate) fn render_pane_context_menu(
        &self,
        menu: PaneContextMenu,
        ui: crate::theme::UiColors,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let source = menu.pane.clone();

        // Workspace root of the pane, for the relative-path entry. The pane
        // carries its owning workspace id, so this resolves without walking
        // every tab's layout tree.
        let owner_id = source.read(cx).workspace_id;
        let workspace_cwd: Option<PathBuf> = self
            .workspaces
            .iter()
            .find(|ws| ws.id == owner_id)
            .map(|ws| PathBuf::from(&ws.cwd));

        let surface_path = Self::surface_context_path(&source.read(cx).surface, cx);
        let full_path = surface_path
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned());
        let relative_path = surface_path.as_ref().map(|path| {
            workspace_cwd
                .as_ref()
                .map(|root| files_tree::workspace_relative_path(root, path))
                .unwrap_or_else(|| path.to_string_lossy().into_owned())
        });

        // EP-001 US-003 (cli-cockpit): cancel this surface's queued prompt -
        // the non-Composer cancel path. Only shown when a buffer exists.
        let pending_sid = source
            .read(cx)
            .surface
            .as_terminal()
            .map(|t| t.entity_id().as_u64())
            .filter(|sid| self.broadcast.pending.contains_key(sid));

        let rows = 3 + usize::from(pending_sid.is_some()) + 1;
        let menu_height = px(8. + rows as f32 * 29. + 18.);
        let menu_pos = clamped_context_menu_position(menu.position, px(248.), menu_height, window);

        let source_for_rename = source.clone();
        let mut context_menu = select_menu("pane-context-menu", ui)
            .occlude()
            .absolute()
            .left(menu_pos.x)
            .top(menu_pos.y)
            .w(px(248.))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.pane_menu_open = None;
                cx.notify();
            }))
            .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
            .child(self.render_select_menu_item(
                "pane-context-rename".into(),
                "Rename",
                None,
                ui,
                cx.listener(move |this, _: &ClickEvent, window, cx| {
                    this.pane_menu_open = None;
                    this.commit_inline_rename(window, cx);
                    source_for_rename.update(cx, |pane, cx| pane.begin_rename(window, cx));
                    cx.stop_propagation();
                    cx.notify();
                }),
            ));

        if let Some(value) = full_path {
            context_menu = context_menu.child(self.render_select_menu_item(
                "pane-context-copy-path".into(),
                "Copy Path",
                None,
                ui,
                cx.listener(move |this, _: &ClickEvent, _window, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(value.clone()));
                    this.pane_menu_open = None;
                    this.show_toast("Copied path", cx);
                    cx.stop_propagation();
                }),
            ));
        } else {
            context_menu = context_menu.child(Self::render_disabled_select_menu_item(
                "pane-context-copy-path-disabled".into(),
                "Copy Path unavailable",
                ui,
            ));
        }

        if let Some(value) = relative_path {
            context_menu = context_menu.child(self.render_select_menu_item(
                "pane-context-copy-relative-path".into(),
                "Copy Relative Path",
                None,
                ui,
                cx.listener(move |this, _: &ClickEvent, _window, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(value.clone()));
                    this.pane_menu_open = None;
                    this.show_toast("Copied relative path", cx);
                    cx.stop_propagation();
                }),
            ));
        } else {
            context_menu = context_menu.child(Self::render_disabled_select_menu_item(
                "pane-context-copy-relative-path-disabled".into(),
                "Copy Relative Path unavailable",
                ui,
            ));
        }

        if let Some(sid) = pending_sid {
            context_menu = context_menu.child(self.render_select_menu_item(
                SharedString::from("pane-cancel-queued"),
                "Cancel queued prompt",
                None,
                ui,
                cx.listener(move |this, _: &ClickEvent, _window, cx| {
                    this.pane_menu_open = None;
                    this.cancel_pending_for(sid, cx);
                    cx.stop_propagation();
                    cx.notify();
                }),
            ));
        }

        context_menu = context_menu.child(
            div()
                .mx(px(6.))
                .my(px(4.))
                .h(px(1.))
                .bg(menu_divider_color(ui)),
        );

        let source_for_close = source.clone();
        context_menu = context_menu.child(self.render_select_menu_item(
            "pane-context-close".into(),
            "Close Pane",
            None,
            ui,
            cx.listener(move |this, _: &ClickEvent, _window, cx| {
                this.pane_menu_open = None;
                // Issue #83: a MODAL confirmation, not the inline
                // arm-then-confirm - the menu has already dismissed itself, so
                // an inline affordance here would be a dead menu item. The
                // session save moved into the close path itself so a pending
                // close never persists the pre-close tree.
                this.request_close_pane(
                    source_for_close.clone(),
                    crate::app::close_guard::ConfirmStyle::Modal,
                    cx,
                );
                cx.stop_propagation();
                cx.notify();
            }),
        ));

        deferred(context_menu).priority(3).into_any_element()
    }

    fn render_disabled_select_menu_item(
        id: SharedString,
        label: &str,
        ui: crate::theme::UiColors,
    ) -> impl IntoElement {
        div()
            .id(id)
            .h(px(28.))
            .px(px(8.))
            .rounded(px(7.))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.))
            .text_size(px(12.))
            .text_color(ui.muted)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_x_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .child(label.to_string()),
            )
    }

    /// Path a pane surface can advertise in its context menu: the terminal's
    /// live CWD, the markdown file, or the diff's first column.
    fn surface_context_path(surface: &PaneSurface, cx: &App) -> Option<PathBuf> {
        match surface {
            PaneSurface::Terminal(terminal) => terminal
                .read(cx)
                .terminal
                .current_cwd
                .as_ref()
                .filter(|cwd| !cwd.is_empty())
                .map(PathBuf::from),
            PaneSurface::Markdown(markdown) => Some(markdown.read(cx).path.clone()),
            PaneSurface::Diff(diff) => diff.read(cx).column_paths().into_iter().next(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{tab_context_menu_height, workspace_context_menu_counts};
    use gpui::px;

    #[test]
    fn tab_menu_height_never_exceeds_the_surface_cap() {
        // Issue #347 review, finding 7: the height fed to the on-screen
        // clamp counted every branch row, while `select_menu` caps the
        // surface at 320 px, so a long branch list pinned the menu at y = 0.
        assert_eq!(
            tab_context_menu_height(0, false),
            px(64.),
            "chrome + two rows"
        );
        assert_eq!(
            tab_context_menu_height(2, false),
            px(8. + (2. + 3.) * 28.),
            "the section header and one row per branch, under the cap"
        );
        assert_eq!(tab_context_menu_height(40, false), px(320.));
        assert_eq!(tab_context_menu_height(400, false), px(320.));
    }

    #[test]
    fn a_bound_tab_menu_is_one_row_taller_for_remove_worktree() {
        // Issue #348: the "Remove worktree" row exists only for a bound tab,
        // and the clamp must measure it or the menu overshoots the window
        // bottom by exactly one row.
        assert_eq!(
            tab_context_menu_height(0, true),
            px(8. + 3. * 28.),
            "chrome, two rows, and the removal row"
        );
        assert_eq!(
            tab_context_menu_height(2, true),
            px(8. + (2. + 3. + 1.) * 28.),
            "the removal row sits under the Branch section"
        );
        assert_eq!(tab_context_menu_height(40, true), px(320.));
    }

    #[test]
    fn workspace_menu_geometry_uses_filtered_editor_rows() {
        assert_eq!(workspace_context_menu_counts(4, 0, 0), (9, 3));
        assert_eq!(workspace_context_menu_counts(2, 1, 0), (8, 3));
        assert_eq!(workspace_context_menu_counts(0, 0, 0), (5, 2));
    }

    #[test]
    fn workspace_menu_geometry_counts_service_and_editor_dividers_independently() {
        assert_eq!(workspace_context_menu_counts(4, 0, 2), (11, 4));
        assert_eq!(workspace_context_menu_counts(0, 0, 2), (7, 3));
    }
}
