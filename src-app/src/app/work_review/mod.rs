//! On-demand review evidence across open checkouts. No permanent poller.
pub(crate) mod model;
use gpui::prelude::*;

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Instant;

use crate::PaneFlowApp;
use gpui::{
    AnyElement, AppContext, ClickEvent, ClipboardItem, Context, InteractiveElement, IntoElement,
    KeyDownEvent, MouseButton, ParentElement, Styled, Window, deferred, div, px,
};
use model::{Checkout, PullRequest};

pub(crate) struct ReviewState {
    rows: Vec<ReviewRow>,
    cancel: Arc<AtomicBool>,
    running: bool,
    selected: usize,
    opened: Instant,
    scroll: gpui::ScrollHandle,
}

impl Drop for ReviewState {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

struct ReviewRow {
    tab_id: u64,
    surface_id: Option<u64>,
    title: String,
    checkout: Result<Checkout, String>,
    pr: Result<Option<PullRequest>, String>,
}

impl PaneFlowApp {
    pub(crate) fn open_work_review(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.work_review.as_ref().is_some_and(|s| s.running) {
            return;
        }
        let mut targets = Vec::new();
        let mut seen = HashSet::new();
        for ws in &self.workspaces {
            for (index, tab) in ws.tabs().iter().enumerate() {
                let title = format!(
                    "{} / {}",
                    ws.title,
                    crate::app::sidebar::tab_row_title(tab, index, cx)
                );
                let mut paths = Vec::new();
                for pane in tab.collect_panes() {
                    for terminal in pane.read(cx).terminals() {
                        if let Some(cwd) = &terminal.read(cx).terminal.current_cwd {
                            paths.push((PathBuf::from(cwd), Some(terminal.entity_id().as_u64())));
                        }
                    }
                }
                if paths.is_empty() {
                    paths.push((
                        tab.worktree
                            .clone()
                            .unwrap_or_else(|| PathBuf::from(&ws.cwd)),
                        None,
                    ));
                }
                for (cwd, surface_id) in paths {
                    if cwd.is_absolute() && seen.insert(cwd.clone()) && targets.len() < 64 {
                        targets.push((tab.id, surface_id, title.clone(), cwd));
                    }
                }
            }
        }
        let cancel = Arc::new(AtomicBool::new(false));
        self.work_review = Some(ReviewState {
            rows: Vec::new(),
            cancel: cancel.clone(),
            running: true,
            selected: 0,
            opened: Instant::now(),
            scroll: gpui::ScrollHandle::new(),
        });
        self.work_review_focus.focus(window, cx);
        cx.notify();
        cx.spawn(async move |this, cx| {
            let mut roots = HashSet::new();
            for (tab_id, surface_id, title, cwd) in targets {
                if cancel.load(Ordering::Relaxed) {
                    break;
                }
                let checkout = cx
                    .background_spawn(async move { model::inspect(&cwd) })
                    .await;
                if let Ok(c) = &checkout
                    && !roots.insert(c.root.clone())
                {
                    continue;
                }
                if cancel.load(Ordering::Relaxed) {
                    break;
                }
                let evidence = checkout.clone();
                let (checkout, pr) = cx
                    .background_spawn(async move {
                        match evidence {
                            Ok(c) => {
                                let pr = model::pull_request(&c);
                                // Reinspect after the network wait: an agent may
                                // have edited or committed while GitHub answered.
                                (model::inspect(&c.root), pr)
                            }
                            Err(error) => (Err(error), Ok(None)),
                        }
                    })
                    .await;
                let _ = this.update(cx, |app, cx| {
                    if let Some(state) = &mut app.work_review
                        && Arc::ptr_eq(&state.cancel, &cancel)
                    {
                        state.rows.push(ReviewRow {
                            tab_id,
                            surface_id,
                            title,
                            checkout,
                            pr,
                        });
                        cx.notify();
                    }
                });
            }
            let _ = this.update(cx, |app, cx| {
                if let Some(state) = &mut app.work_review
                    && Arc::ptr_eq(&state.cancel, &cancel)
                {
                    state.running = false;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn close_work_review(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.work_review = None;
        self.restore_focus_after_close_confirm(window, cx);
        cx.notify();
    }

    fn visit_review_row(
        &mut self,
        index: usize,
        review: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((tab_id, surface_id, checkout)) = self
            .work_review
            .as_ref()
            .and_then(|s| s.rows.get(index))
            .map(|r| (r.tab_id, r.surface_id, r.checkout.clone()))
        else {
            return;
        };
        let location = self.workspaces.iter().enumerate().find_map(|(w, ws)| {
            ws.tabs()
                .iter()
                .position(|t| t.id == tab_id)
                .map(|t| (w, t))
        });
        self.close_work_review(window, cx);
        if let Some((w, t)) = location {
            self.select_workspace_tab(w, t, window, cx);
            if let Some(surface_id) = surface_id {
                self.teleport_to_surface(surface_id, window, cx);
            }
            if review
                && self.cached_config.review_view_enabled()
                && let Ok(checkout) = checkout
            {
                let ws_id = self.workspaces[w].id;
                let worktree = crate::diff::DiffWorktree {
                    path: checkout.root.clone(),
                    branch: checkout.branch,
                    workspace_id: Some(ws_id),
                };
                let diff =
                    cx.new(|cx| crate::diff::DiffView::new(checkout.root, vec![worktree], cx));
                let pane = self.create_pane_with_existing_surface(
                    crate::pane::PaneSurface::Diff(diff),
                    ws_id,
                    cx,
                );
                if self.open_pane_in_new_workspace_tab(w, pane.clone(), cx) {
                    self.pending_pane_focus = Some(pane);
                }
                cx.notify();
            }
        } else {
            self.show_toast("That task was closed", cx);
        }
    }

    fn review_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let Some(state) = &mut self.work_review else {
            return;
        };
        let selected = state.selected;
        match event.keystroke.key.as_str() {
            "escape" => self.close_work_review(window, cx),
            "up" => {
                state.selected = selected.saturating_sub(1);
                state.scroll.scroll_to_item(state.selected);
                cx.notify();
            }
            "down" => {
                state.selected = (selected + 1).min(state.rows.len().saturating_sub(1));
                state.scroll.scroll_to_item(state.selected);
                cx.notify();
            }
            "enter" => self.visit_review_row(selected, false, window, cx),
            "r" => self.visit_review_row(selected, true, window, cx),
            _ => {}
        }
        cx.stop_propagation();
    }

    pub(crate) fn render_work_review(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(state) = &self.work_review else {
            return div().into_any_element();
        };
        let ui = crate::theme::ui_colors();
        let mut rows = div()
            .id("work-review-rows")
            .track_scroll(&state.scroll)
            .overflow_y_scroll()
            .max_h(px(500.))
            .flex()
            .flex_col()
            .gap(px(8.));
        if state.rows.is_empty() {
            rows = rows.child(if state.running {
                "Inspecting open checkouts…"
            } else {
                "Open a project to review its work."
            });
        }
        for (index, row) in state.rows.iter().enumerate() {
            let mut card = div()
                .id(("work-review-row", index))
                .flex_none()
                .p(px(12.))
                .rounded(px(6.))
                .border_1()
                .border_color(if state.selected == index {
                    ui.text
                } else {
                    ui.border
                })
                .flex()
                .flex_col()
                .gap(px(5.))
                .on_click(cx.listener(move |app, _: &ClickEvent, _, cx| {
                    if let Some(s) = &mut app.work_review {
                        s.selected = index;
                        cx.notify();
                    }
                }))
                .child(crate::limits::clamp_untrusted_label(&row.title));
            if let Some(session) = self
                .workspaces
                .iter()
                .flat_map(|ws| ws.agent_sessions.values())
                .find(|session| row.surface_id.is_some() && session.surface_id == row.surface_id)
            {
                let label = match session.state {
                    crate::ai_types::AgentState::Thinking => "Working",
                    crate::ai_types::AgentState::WaitingForInput => "Needs your input",
                    crate::ai_types::AgentState::Finished => "Agent finished",
                    crate::ai_types::AgentState::Errored => "Agent failed",
                    crate::ai_types::AgentState::Stalled => "Agent stalled",
                };
                card = card.child(format!("{} · {label}", session.tool.display_name()));
            }
            match &row.checkout {
                Err(error) => {
                    card = card.child(
                        div()
                            .text_color(ui.muted)
                            .child(format!("Repository unavailable: {error}")),
                    );
                }
                Ok(c) => {
                    card = card
                        .child(div().text_color(ui.muted).child(format!(
                            "{} · {} files changed · {}",
                            crate::limits::clamp_untrusted_label(&c.branch),
                            c.files.len(),
                            &c.head[..c.head.len().min(8)]
                        )))
                        .child(match &row.pr {
                            Ok(pr) => model::readiness(c, pr.as_ref()).to_string(),
                            Err(e) => e.clone(),
                        });
                    if c.base.is_none() {
                        card =
                            card.child("Branch base unavailable; showing working-tree files only");
                    }
                    for other in &state.rows {
                        if let Ok(other_c) = &other.checkout {
                            let overlap = model::overlap(c, other_c);
                            if !overlap.is_empty() {
                                card = card.child(div().text_color(ui.muted).child(format!(
                                        "Overlaps {}: {}{}",
                                        crate::limits::clamp_untrusted_label(&other_c.branch),
                                        overlap
                                            .iter()
                                            .take(4)
                                            .map(|s| crate::limits::clamp_untrusted_label(s))
                                            .collect::<Vec<_>>()
                                            .join(", "),
                                        if overlap.len() > 4 { "…" } else { "" }
                                    )));
                            }
                        }
                    }
                    let context = model::handoff_context(c);
                    let mut actions = div()
                        .flex()
                        .gap(px(14.))
                        .child(
                            div()
                                .id(("review-open", index))
                                .cursor_pointer()
                                .child("Open task")
                                .on_click(cx.listener(move |app, _: &ClickEvent, window, cx| {
                                    app.visit_review_row(index, false, window, cx);
                                    cx.stop_propagation();
                                })),
                        )
                        .child(
                            div()
                                .id(("review-diff", index))
                                .cursor_pointer()
                                .child("Review diff")
                                .on_click(cx.listener(move |app, _: &ClickEvent, window, cx| {
                                    app.visit_review_row(index, true, window, cx);
                                    cx.stop_propagation();
                                })),
                        )
                        .child(
                            div()
                                .id(("review-context", index))
                                .cursor_pointer()
                                .child("Copy handoff")
                                .on_click(cx.listener(move |app, _: &ClickEvent, _, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(
                                        context.clone(),
                                    ));
                                    app.show_toast("Repository handoff copied", cx);
                                    cx.stop_propagation();
                                })),
                        );
                    if let Ok(Some(pr)) = &row.pr {
                        let url = pr.url.clone();
                        actions = actions.child(
                            div()
                                .id(("review-pr", index))
                                .cursor_pointer()
                                .child(format!("PR #{} / checks", pr.number))
                                .on_click(cx.listener(move |app, _: &ClickEvent, _, cx| {
                                    if let Err(e) = crate::external_open::open_http_url(&url) {
                                        app.show_toast(format!("Could not open PR: {e}"), cx);
                                    }
                                    cx.stop_propagation();
                                })),
                        );
                    }
                    card = card.child(actions);
                }
            }
            rows = rows.child(card);
        }
        let card = div()
            .id("work-review-dialog")
            .occlude()
            .track_focus(&self.work_review_focus)
            .on_key_down(cx.listener(Self::review_key))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .w(px(780.))
            .max_w_full()
            .p(px(20.))
            .rounded(px(12.))
            .bg(ui.overlay)
            .text_color(ui.text)
            .text_size(px(13.))
            .flex()
            .flex_col()
            .gap(px(14.))
            .child(div().text_size(px(20.)).child("Work review"))
            .child(div().text_color(ui.muted).child(format!(
                "{} · snapshot started {}s ago · ↑↓ select · Enter open · R review",
                if state.running {
                    "Checking repositories and GitHub"
                } else {
                    "Refresh to update evidence"
                },
                state.opened.elapsed().as_secs()
            )))
            .child(rows)
            .child(
                div()
                    .flex()
                    .gap(px(20.))
                    .child(
                        div()
                            .id("work-review-refresh")
                            .cursor_pointer()
                            .child(if state.running {
                                "Checking…"
                            } else {
                                "Refresh"
                            })
                            .on_click(cx.listener(|app, _: &ClickEvent, window, cx| {
                                app.open_work_review(window, cx)
                            })),
                    )
                    .child(
                        div()
                            .id("work-review-close")
                            .cursor_pointer()
                            .child("Close (Esc)")
                            .on_click(cx.listener(|app, _: &ClickEvent, window, cx| {
                                app.close_work_review(window, cx)
                            })),
                    ),
            );
        deferred(
            div()
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .bg(gpui::black().opacity(0.45))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|app, _, window, cx| app.close_work_review(window, cx)),
                )
                .child(card),
        )
        .into_any_element()
    }
}
