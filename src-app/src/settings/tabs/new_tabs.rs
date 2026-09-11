//! Default branch selection for new workspace tabs.

use std::collections::BTreeSet;

use gpui::{
    AnyElement, ClickEvent, Context, FontWeight, IntoElement, MouseButton, ParentElement,
    SharedString, Styled, Window, div, prelude::*, px,
};
use serde_json::Value;

use crate::settings::components::{
    deferred_select_menu, hairline, select_chevron, select_listbox, select_option, select_trigger,
    setting_card,
};
use crate::{PaneFlowApp, WorkspaceTemplateDropdown};

fn branch_label(branch: &str) -> String {
    if branch.is_empty() {
        "Workspace checkout".to_string()
    } else {
        branch.to_string()
    }
}

impl PaneFlowApp {
    pub(crate) fn render_new_tab_branch_settings(
        &self,
        ui: crate::theme::UiColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut card = setting_card(ui).child(self.new_tab_branch_row(None, ui, cx));
        for (index, ws) in self.workspaces.iter().enumerate() {
            if ws.repo_root.is_some() {
                card = card
                    .child(hairline(ui))
                    .child(self.new_tab_branch_row(Some(index), ui, cx));
            }
        }
        card.into_any_element()
    }

    fn new_tab_branch_row(
        &self,
        ws_idx: Option<usize>,
        ui: crate::theme::UiColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let workspace = ws_idx.and_then(|index| self.workspaces.get(index));
        let ws_id = workspace.map(|ws| ws.id);
        let which = WorkspaceTemplateDropdown::NewTabBranch(ws_id);
        let is_open = self.workspace_template_dropdown == Some(which);
        let default = self.cached_config.default_new_tab_branch();
        let current = workspace.map_or_else(
            || Some(default.to_string()),
            |ws| {
                self.cached_config
                    .workspace_new_tab_branches
                    .get(&ws.cwd)
                    .cloned()
            },
        );
        let inherited_label = format!("Use default ({})", branch_label(default));
        let selected_label = current
            .as_deref()
            .map_or_else(|| inherited_label.clone(), branch_label);
        let title = workspace
            .map_or("Default branch", |ws| ws.title.as_str())
            .to_string();
        let description = workspace
            .map_or(
                "New tabs start on this branch unless a workspace chooses another.",
                |ws| ws.cwd.as_str(),
            )
            .to_string();
        let id = ws_id.map_or_else(|| "default".to_string(), |id| id.to_string());
        let mut trigger = select_trigger(
            SharedString::from(format!("new-tab-branch-{id}")),
            ui,
            is_open,
        )
        .aria_label(format!("{title}: new tab branch, {selected_label}"))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _, window, cx| {
                cx.stop_propagation();
                this.toggle_new_tab_branch_menu(ws_id, is_open, window, cx);
            }),
        )
        .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
            if matches!(event, ClickEvent::Keyboard(_)) {
                this.toggle_new_tab_branch_menu(ws_id, is_open, window, cx);
            }
        }))
        .child(div().flex_1().min_w_0().truncate().child(selected_label))
        .child(select_chevron(ui));

        if is_open {
            let mut branches = BTreeSet::from(["main".to_string()]);
            for (index, ws) in self.workspaces.iter().enumerate() {
                if ws_id.is_none_or(|id| id == ws.id) {
                    branches.extend(self.workspace_branches(index).iter().cloned());
                }
            }
            for branch in [current.as_deref(), Some(default)].into_iter().flatten() {
                if !branch.is_empty() {
                    branches.insert(branch.to_string());
                }
            }
            let mut options = Vec::new();
            if workspace.is_some() {
                options.push((None, inherited_label));
            }
            options.push((Some(String::new()), "Workspace checkout".to_string()));
            options.extend(
                branches
                    .into_iter()
                    .map(|branch| (Some(branch.clone()), branch)),
            );
            let mut menu =
                select_listbox(SharedString::from(format!("new-tab-branch-menu-{id}")), ui)
                    .on_mouse_down_out(cx.listener(move |this, _, _, cx| {
                        if this.workspace_template_dropdown == Some(which) {
                            this.workspace_template_dropdown = None;
                            cx.notify();
                        }
                    }));
            for (index, (branch, label)) in options.into_iter().enumerate() {
                menu = menu.child(
                    select_option(
                        (
                            SharedString::from(format!("new-tab-branch-option-{id}")),
                            index,
                        ),
                        branch == current,
                        ui,
                    )
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.workspace_template_dropdown = None;
                        if let Some(ws_id) = ws_id {
                            let Some(ws) = this.workspaces.iter().find(|ws| ws.id == ws_id) else {
                                return;
                            };
                            let mut overrides =
                                this.cached_config.workspace_new_tab_branches.clone();
                            if let Some(branch) = &branch {
                                overrides.insert(ws.cwd.clone(), branch.clone());
                            } else {
                                overrides.remove(&ws.cwd);
                            }
                            this.persist_setting(
                                false,
                                "workspace_new_tab_branches",
                                serde_json::json!(overrides),
                                cx,
                            );
                        } else if let Some(branch) = &branch {
                            this.persist_setting(
                                false,
                                "new_tab_branch",
                                Value::String(branch.clone()),
                                cx,
                            );
                        }
                    }))
                    .child(div().flex_1().min_w_0().truncate().child(label)),
                );
            }
            trigger = trigger.child(deferred_select_menu(menu));
        }

        div()
            .flex()
            .items_center()
            .gap(px(16.))
            .px(px(12.))
            .py(px(10.))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .child(
                        div()
                            .text_size(px(12.))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(ui.text)
                            .truncate()
                            .child(title),
                    )
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(ui.muted)
                            .truncate()
                            .child(description),
                    ),
            )
            .child(div().flex_shrink_0().w(px(200.)).child(trigger))
            .into_any_element()
    }

    fn toggle_new_tab_branch_menu(
        &mut self,
        ws_id: Option<u64>,
        was_open: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.workspace_template_dropdown = if was_open {
            None
        } else {
            Some(WorkspaceTemplateDropdown::NewTabBranch(ws_id))
        };
        if !was_open {
            for index in 0..self.workspaces.len() {
                if ws_id.is_none_or(|id| id == self.workspaces[index].id) {
                    self.spawn_worktree_listing(index, cx);
                }
            }
        }
        self.settings_focus.focus(window, cx);
        cx.notify();
    }
}
