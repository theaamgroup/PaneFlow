//! The dock's "Agent setup" tab (issue #331): the rulebook the agents running
//! in this workspace load - instruction files, skills, rules, hooks and MCP
//! entries - listed project-first, then global, and openable as sibling dock
//! tabs.
//!
//! PaneFlow's version of Blume's Setup tab. The Files sidebar hides dotfiles
//! and gitignored entries on purpose, so `.claude/`, `.codex/`, `.cursor/` and
//! the `SKILL.md` trees are invisible there; this tab reads a fixed catalog
//! ([`paneflow_agent_setup::catalog`]) instead of un-hiding the tree.
//!
//! Read-only: the scan never writes, and clicking a row opens the file through
//! [`PaneFlowApp::open_diff_file_tab`] - the same editor every other dock tab
//! gets, undo stack and save included. Nothing here renders an MCP server's
//! `command` / `args` / `env` or a hook's command string.
//!
//! Lifecycle: one scan on open, one whenever the dock's folder changes under
//! the tab, and one per manual Refresh - off the GPUI thread through
//! `smol::unblock`, the way the session scans run. No watcher thread, and no
//! work on the 50 ms automation tick.

use std::path::PathBuf;

use gpui::{
    AnyElement, App, AppContext, ClickEvent, Context, CursorStyle, EventEmitter, FontWeight,
    InteractiveElement, IntoElement, MouseButton, ParentElement, Render, SharedString,
    StatefulInteractiveElement, Styled, Window, div, prelude::FluentBuilder, px, svg, uniform_list,
};
use paneflow_agent_setup::{ArtifactType, Inventory, Roots, Scope, SetupRow, scan};

use super::model::DiffDockTab;
use super::render::{diff_panel_centered, render_diff_header_icon_button};
use crate::PaneFlowApp;
use crate::agent_launcher::TerminalAgent;
use crate::settings::components::with_alpha;
use crate::ui_primitives::{BODY, LABEL_SM, LABEL_XS};

/// Height of one list line - a row (path over title) or a scope header. The
/// list is a `uniform_list`, so every line shares it: at the 200-row cap a
/// plain child list rebuilt every frame is the kind of node count that made
/// the shortcuts page lag.
const LINE_HEIGHT: f32 = 44.0;

/// Accessible name and tooltip of the header's Refresh control (issue #340:
/// one string feeds both, through `render_diff_header_icon_button`).
const REFRESH_LABEL: &str = "Refresh agent setup";

/// What a row click asks the dock to do.
pub(crate) enum SetupViewEvent {
    /// Open this file as a sibling tab of the same dock.
    OpenFile(PathBuf),
}

/// The tab's state: the folder it scanned, what came back, and the filter.
pub(crate) struct SetupView {
    /// The Tab's canonical cwd the inventory describes - not the focused
    /// pane's live cwd, which would rescan the home directory on every focus
    /// change. A pane that `cd`'d elsewhere shows the Tab's rulebook.
    cwd: String,
    inventory: Option<Inventory>,
    loading: bool,
    /// Monotonic scan token: a scan that finishes after a newer one started
    /// is dropped rather than overwriting it.
    generation: u64,
    /// `None` is "All".
    filter: Option<ArtifactType>,
}

impl EventEmitter<SetupViewEvent> for SetupView {}

/// One line of the rendered list.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Line {
    Header { scope: Scope, count: usize },
    Row(usize),
}

impl SetupView {
    pub(crate) fn new(cwd: String, cx: &mut Context<Self>) -> Self {
        let mut view = Self {
            cwd,
            inventory: None,
            loading: false,
            generation: 0,
            filter: None,
        };
        view.rescan(cx);
        view
    }

    /// Follow the dock's folder: a changed cwd is a different rulebook.
    pub(crate) fn sync_cwd(&mut self, cwd: String, cx: &mut Context<Self>) {
        if self.cwd != cwd {
            self.cwd = cwd;
            self.rescan(cx);
        }
    }

    fn rescan(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        let generation = self.generation;
        if self.cwd.is_empty() {
            self.inventory = Some(Inventory::default());
            self.loading = false;
            cx.notify();
            return;
        }
        self.loading = true;
        let project = PathBuf::from(&self.cwd);
        cx.spawn(async move |this, cx| {
            let inventory = smol::unblock(move || scan(&Roots::resolve(project))).await;
            let _ = this.update(cx, |view, cx| {
                if view.generation == generation {
                    view.inventory = Some(inventory);
                    view.loading = false;
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
    }

    fn set_filter(&mut self, filter: Option<ArtifactType>, cx: &mut Context<Self>) {
        if self.filter != filter {
            self.filter = filter;
            cx.notify();
        }
    }

    fn lines(&self) -> Vec<Line> {
        let Some(inventory) = self.inventory.as_ref() else {
            return Vec::new();
        };
        let visible = inventory
            .rows
            .iter()
            .enumerate()
            .filter(|(_, row)| self.filter.is_none_or(|kind| row.artifact_type == kind))
            .map(|(index, row)| (index, row.scope))
            .collect::<Vec<_>>();
        group_lines(&visible)
    }

    fn render_header(&self, ui: crate::theme::UiColors, cx: &mut Context<Self>) -> AnyElement {
        let mut filters = div().flex().flex_row().flex_wrap().gap(px(4.));
        filters = filters.child(self.filter_chip("setup-filter-all", "All", None, ui, cx));
        for kind in ArtifactType::ALL {
            let id: &'static str = match kind {
                ArtifactType::Rule => "setup-filter-rule",
                ArtifactType::Skill => "setup-filter-skill",
                ArtifactType::Hook => "setup-filter-hook",
                ArtifactType::Doc => "setup-filter-doc",
                ArtifactType::Mcp => "setup-filter-mcp",
            };
            filters = filters.child(self.filter_chip(id, kind.plural(), Some(kind), ui, cx));
        }

        div()
            .flex_none()
            .flex()
            .flex_col()
            .gap(px(6.))
            .px(px(12.))
            .py(px(8.))
            .border_b_1()
            .border_color(ui.border)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .whitespace_nowrap()
                            .overflow_hidden()
                            .text_ellipsis()
                            .text_size(BODY)
                            .text_color(ui.muted)
                            .child(if self.cwd.is_empty() {
                                "No workspace folder".to_string()
                            } else {
                                self.cwd.clone()
                            }),
                    )
                    // Issue #340's recipe: one label feeds the button role,
                    // its accessible name and its tooltip, inside the
                    // primitive.
                    .child(render_diff_header_icon_button(
                        "setup-refresh",
                        "icons/refresh.svg",
                        REFRESH_LABEL,
                        cx.listener(|this, _: &ClickEvent, _w, cx| this.rescan(cx)),
                        ui.muted,
                    )),
            )
            .child(filters)
            .into_any_element()
    }

    fn filter_chip(
        &self,
        id: &'static str,
        label: &'static str,
        kind: Option<ArtifactType>,
        ui: crate::theme::UiColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let active = self.filter == kind;
        div()
            .id(id)
            .flex_none()
            .h(px(22.))
            .px(px(8.))
            .flex()
            .items_center()
            .rounded(px(6.))
            .cursor(CursorStyle::PointingHand)
            .bg(if active {
                with_alpha(ui.text, 0.10)
            } else {
                with_alpha(ui.text, 0.0)
            })
            .when(!active, |chip| {
                chip.hover(|style| style.bg(with_alpha(ui.text, 0.05)))
            })
            .text_size(LABEL_SM)
            .text_color(if active { ui.text } else { ui.muted })
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(move |this, _: &ClickEvent, _w, cx| {
                this.set_filter(kind, cx);
            }))
            .child(label)
            .into_any_element()
    }

    fn render_footer(&self, ui: crate::theme::UiColors) -> Option<AnyElement> {
        let omitted = self.inventory.as_ref().map(|inv| inv.omitted).unwrap_or(0);
        let unmapped = unmapped_launchers();
        if omitted == 0 && unmapped.is_empty() {
            return None;
        }
        let mut footer = div()
            .flex_none()
            .flex()
            .flex_col()
            .gap(px(2.))
            .px(px(12.))
            .py(px(8.))
            .border_t_1()
            .border_color(ui.border)
            .text_size(LABEL_SM)
            .text_color(ui.muted);
        if omitted > 0 {
            footer = footer.child(format!("{omitted} more not shown"));
        }
        if !unmapped.is_empty() {
            footer = footer.child(
                div()
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .text_ellipsis()
                    .child(format!("Not inspected: {}", unmapped.join(", "))),
            );
        }
        Some(footer.into_any_element())
    }

    fn render_body(&self, ui: crate::theme::UiColors, cx: &mut Context<Self>) -> AnyElement {
        if self.loading && self.inventory.is_none() {
            return diff_panel_centered("icons/loader-circle.svg", "Scanning agent setup…", ui);
        }
        let lines = self.lines();
        if lines.is_empty() {
            let message = if self.filter.is_some() {
                "No artifacts of this type"
            } else {
                "No agent setup found. Instruction files are read at the workspace root only."
            };
            return crate::ui_primitives::panel_empty_state(
                ui,
                Some("icons/list.svg"),
                Some("Nothing to show".into()),
                message,
                false,
            )
            .into_any_element();
        }
        let entity = cx.entity();
        uniform_list("setup-lines", lines.len(), move |range, _window, cx| {
            let view = entity.read(cx);
            let lines = view.lines();
            range
                .filter_map(|index| lines.get(index).cloned())
                .map(|line| match line {
                    Line::Header { scope, count } => render_header_line(scope, count, ui),
                    Line::Row(index) => {
                        match view.inventory.as_ref().and_then(|inv| inv.rows.get(index)) {
                            Some(row) => render_row(index, row, &entity, ui),
                            None => div().h(px(LINE_HEIGHT)).into_any_element(),
                        }
                    }
                })
                .collect::<Vec<_>>()
        })
        .flex_1()
        .min_h_0()
        .into_any_element()
    }
}

/// Group filtered `(row index, scope)` pairs under one header per scope, in
/// inventory order (`Project` first). A scope with no visible row gets no
/// header: an empty "Global (0)" would read as a scan that failed.
fn group_lines(visible: &[(usize, Scope)]) -> Vec<Line> {
    let mut lines = Vec::with_capacity(visible.len() + 2);
    for scope in [Scope::Project, Scope::Global] {
        let count = visible.iter().filter(|(_, s)| *s == scope).count();
        if count == 0 {
            continue;
        }
        lines.push(Line::Header { scope, count });
        lines.extend(
            visible
                .iter()
                .filter(|(_, s)| *s == scope)
                .map(|(index, _)| Line::Row(*index)),
        );
    }
    lines
}

fn render_header_line(scope: Scope, count: usize, ui: crate::theme::UiColors) -> AnyElement {
    div()
        .h(px(LINE_HEIGHT))
        .px(px(12.))
        .flex()
        .items_end()
        .pb(px(6.))
        .text_size(LABEL_SM)
        .font_weight(FontWeight::MEDIUM)
        .text_color(ui.muted)
        .child(format!("{} ({count})", scope.label()))
        .into_any_element()
}

/// One row: type chip · harness · display path, over the title line. A row
/// that cannot open renders muted with its reason and takes no click.
fn render_row(
    index: usize,
    row: &SetupRow,
    entity: &gpui::Entity<SetupView>,
    ui: crate::theme::UiColors,
) -> AnyElement {
    let openable = row.openable;
    let path = row.path.clone();
    let entity = entity.clone();
    let text = if openable { ui.text } else { ui.muted };
    let second_line = if openable {
        row.title.clone().unwrap_or_default()
    } else {
        "Too large to open (over 10 MB)".to_string()
    };
    div()
        .id(SharedString::from(format!("setup-row-{index}")))
        .h(px(LINE_HEIGHT))
        .px(px(12.))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(10.))
        .when(openable, |row| {
            row.cursor(CursorStyle::PointingHand)
                .hover(|style| style.bg(with_alpha(ui.text, 0.05)))
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_click(move |_: &ClickEvent, _window: &mut Window, cx: &mut App| {
                    let path = path.clone();
                    entity.update(cx, |_, cx| cx.emit(SetupViewEvent::OpenFile(path)));
                })
        })
        .child(
            div()
                .flex_none()
                .w(px(40.))
                .h(px(18.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(4.))
                .bg(with_alpha(ui.accent, if openable { 0.15 } else { 0.06 }))
                .text_size(LABEL_XS)
                .font_weight(FontWeight::MEDIUM)
                .text_color(if openable { ui.accent } else { ui.muted })
                .child(row.artifact_type.label()),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_baseline()
                        .gap(px(6.))
                        .min_w_0()
                        .child(
                            div()
                                .flex_none()
                                .text_size(LABEL_SM)
                                .text_color(ui.muted)
                                .child(row.harness.label()),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .whitespace_nowrap()
                                .overflow_hidden()
                                .text_ellipsis()
                                .text_size(BODY)
                                .text_color(text)
                                .child(row.display_path.clone()),
                        ),
                )
                .child(
                    div()
                        .whitespace_nowrap()
                        .overflow_hidden()
                        .text_ellipsis()
                        .text_size(LABEL_SM)
                        .text_color(ui.muted)
                        .child(second_line),
                ),
        )
        .when(openable, |row| {
            row.child(
                svg()
                    .size(px(12.))
                    .flex_none()
                    .path("icons/chevron-right.svg")
                    .text_color(ui.muted),
            )
        })
        .into_any_element()
}

impl Render for SetupView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let ui = crate::theme::ui_colors();
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(self.render_header(ui, cx))
            .child(self.render_body(ui, cx))
            .children(self.render_footer(ui))
    }
}

/// Whether the inventory's catalog maps this launcher's files. The five with
/// a verified layout; every other launcher is named in the footer so a user
/// running one sees that it is not inspected rather than that it has no rules.
fn inventory_covers(agent: TerminalAgent) -> bool {
    matches!(
        agent,
        TerminalAgent::ClaudeCode
            | TerminalAgent::Codex
            | TerminalAgent::OpenCode
            | TerminalAgent::Gemini
            | TerminalAgent::Cursor
    )
}

fn unmapped_launchers() -> Vec<&'static str> {
    TerminalAgent::ALL
        .iter()
        .copied()
        .filter(|agent| !inventory_covers(*agent))
        .map(TerminalAgent::display_name)
        .collect()
}

/// Where the dock's Setup tab sits, if it has one. Backs the singleton rule:
/// invoking Agent setup again activates this tab instead of stacking a second
/// scan of the same folder.
pub(super) fn setup_tab_index(tabs: &[DiffDockTab]) -> Option<usize> {
    tabs.iter()
        .position(|tab| matches!(tab, DiffDockTab::Setup(_)))
}

impl PaneFlowApp {
    /// The `+` menu's "Agent setup" row and the picker's card. Singleton per
    /// dock (so per `Tab`, which the dock is keyed by): a second invocation
    /// selects the existing tab.
    pub(crate) fn open_diff_setup_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(index) = setup_tab_index(&self.diff_dock.diff_tabs) {
            self.select_diff_tab(index, cx);
            return;
        }
        let cwd = self.diff_setup_cwd();
        let view = cx.new(|cx| SetupView::new(cwd, cx));
        cx.subscribe_in(
            &view,
            window,
            |this, _view, event: &SetupViewEvent, window, cx| match event {
                // Into the same dock, beside the inventory rather than over it.
                SetupViewEvent::OpenFile(path) => this.open_diff_file_tab(path.clone(), window, cx),
            },
        )
        .detach();
        self.diff_dock.diff_tabs.push(DiffDockTab::Setup(view));
        self.diff_dock.diff_active_tab = self.diff_dock.diff_tabs.len() - 1;
        self.diff_dock.diff_tab_close_armed = None;
        cx.notify();
    }

    /// The folder the Setup tab describes: the dock's own folder, falling
    /// back to the active tab's checkout.
    pub(crate) fn diff_setup_cwd(&self) -> String {
        self.diff_dock
            .data
            .as_ref()
            .map(|data| data.cwd.clone())
            .filter(|cwd| !cwd.is_empty())
            .or_else(|| self.active_checkout())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two size ceilings cannot drift: a row the inventory calls openable
    /// is one the dock editor will actually open.
    #[test]
    fn the_artifact_size_cap_is_the_editors_file_cap() {
        assert_eq!(
            paneflow_agent_setup::MAX_ARTIFACT_BYTES,
            crate::app::diff_dock::code::load::MAX_FILE_BYTES
        );
    }

    /// Every launcher is either inspected or named as not inspected, so a
    /// partial inventory never looks complete.
    #[test]
    fn every_launcher_is_covered_or_named_as_not_inspected() {
        let covered = TerminalAgent::ALL
            .iter()
            .filter(|agent| inventory_covers(**agent))
            .count();
        let unmapped = unmapped_launchers();
        assert_eq!(covered, 5);
        assert_eq!(covered + unmapped.len(), TerminalAgent::ALL.len());
        assert_eq!(
            unmapped,
            [
                "Pi",
                "Hermes Agent",
                "Grok",
                "Amp",
                "Kiro",
                "Antigravity",
                "Copilot",
                "CodeBuddy",
                "Factory",
                "Qoder",
                "Openclaw",
            ]
        );
    }

    #[test]
    fn lines_group_by_scope_and_skip_an_empty_scope() {
        let visible = [(0, Scope::Project), (3, Scope::Global), (4, Scope::Global)];
        assert_eq!(
            group_lines(&visible),
            vec![
                Line::Header {
                    scope: Scope::Project,
                    count: 1
                },
                Line::Row(0),
                Line::Header {
                    scope: Scope::Global,
                    count: 2
                },
                Line::Row(3),
                Line::Row(4),
            ]
        );
        assert_eq!(
            group_lines(&[(7, Scope::Global)]),
            vec![
                Line::Header {
                    scope: Scope::Global,
                    count: 1
                },
                Line::Row(7)
            ]
        );
        assert!(group_lines(&[]).is_empty());
    }

    /// Invoking Agent setup twice on one dock finds the one Setup tab.
    #[gpui::test]
    fn a_dock_holds_at_most_one_setup_tab(cx: &mut gpui::TestAppContext) {
        let cx = cx.add_empty_window();
        let setup = cx.new(|cx| SetupView::new(String::new(), cx));
        let tabs = vec![
            DiffDockTab::Changes,
            DiffDockTab::PendingFile,
            DiffDockTab::Setup(setup),
        ];
        assert_eq!(setup_tab_index(&tabs), Some(2));
        assert_eq!(
            setup_tab_index(&[DiffDockTab::Changes, DiffDockTab::PendingFile]),
            None
        );
        // An empty cwd settles synchronously into an empty inventory.
        let DiffDockTab::Setup(view) = &tabs[2] else {
            unreachable!("built above")
        };
        view.read_with(cx, |view, _| {
            assert!(!view.loading);
            assert_eq!(view.inventory, Some(Inventory::default()));
            assert!(view.lines().is_empty());
        });
    }
}
