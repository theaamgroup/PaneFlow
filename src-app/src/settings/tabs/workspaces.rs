//! Workspaces settings: sidebar order and the default branch for new tabs.
//!
//! Repeatable layouts come from session restore. The old template builder
//! that edited `commands` is gone; that key is accepted and ignored.

use gpui::{AnyElement, Context, IntoElement, ParentElement, div, prelude::*, px};

use crate::PaneFlowApp;
use crate::settings::components::{section_header, setting_card, toggle_row};

impl PaneFlowApp {
    pub(crate) fn render_workspaces_content(&self, cx: &mut Context<Self>) -> AnyElement {
        let ui = crate::theme::ui_colors();
        div()
            .flex()
            .flex_col()
            .gap(px(20.))
            .child(section_header(ui, "Sidebar"))
            .child(setting_card(ui).child(toggle_row(
                "row-workspace-auto-sort",
                "Sort workspaces automatically",
                "Order the sidebar by pinned first, then workspaces with something \
                 running, then idle ones, alphabetically within each group. \
                 Drag-to-reorder is disabled while this is on.",
                None,
                self.cached_config.workspace_auto_sort_enabled(),
                "workspace_auto_sort",
                ui,
                cx,
            )))
            .child(section_header(ui, "New tabs"))
            .child(self.render_new_tab_branch_settings(ui, cx))
            .into_any_element()
    }
}
