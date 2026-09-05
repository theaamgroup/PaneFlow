//! Right-click menu on a sessions-sidebar row (issue #334): Resume, Copy
//! summary, and "Continue in" one row per launcher the session can be handed
//! to.
//!
//! The hook lives on the row (`sessions_sidebar.rs`); the paint mirrors the
//! workspace and Files menus (`deferred().priority(3)`, `occlude()`,
//! `on_mouse_down_out` dismiss, right-button propagation stopped). The text
//! itself comes from the pure layer (`sessions_handoff.rs`). A handoff opens a
//! new workspace tab in the session's directory running the target's launch
//! command and prefills the block once the CLI settles; it never submits.

use std::path::{Path, PathBuf};

use gpui::{
    AnyElement, ClickEvent, ClipboardItem, Context, CursorStyle, InteractiveElement, IntoElement,
    MouseButton, ParentElement, Pixels, Point, SharedString, Styled, deferred, div, prelude::*, px,
};

use crate::PaneFlowApp;
use crate::agent_launcher::TerminalAgent;
use crate::agent_sessions::{SessionAgent, SessionMeta};
use crate::app::ipc_handler::find_pane_by_surface_id;
use crate::app::sessions_handoff::{copy_text, handoff_prompt, handoff_targets};
use crate::app::sidebar::context_menu::clamped_context_menu_position;
use crate::settings::components::{menu_divider_color, select_item, select_menu};

/// The open row menu: which row, where the pointer was, and whether the
/// session's directory still existed when the menu opened (the one
/// filesystem read the menu performs).
#[derive(Debug, Clone)]
pub(crate) struct SessionContextMenu {
    pub(crate) agent: SessionAgent,
    pub(crate) session_id: String,
    pub(crate) position: Point<Pixels>,
    pub(crate) cwd_missing: bool,
}

/// Caption under "Continue in" when no other launcher is enabled.
const NO_TARGETS_CAPTION: &str = "Enable another agent in Settings ▸ AI Agent";
/// Caption when the session's directory is gone; Copy summary still works.
const CWD_MISSING_CAPTION: &str = "Directory no longer exists";
/// Suffix on a target without a session reader of its own.
const NO_READER_HINT: &str = "no session history";

const MENU_WIDTH: f32 = 248.;
const ROW_HEIGHT: f32 = 28.;

/// Height of the menu as drawn, for the on-screen clamp: chrome, two fixed
/// rows, the divider and section header, then one row per target (or the
/// single caption row), capped at the select-menu surface ceiling.
fn sessions_context_menu_height(target_rows: usize) -> Pixels {
    let rows = 2. + target_rows.max(1) as f32;
    px((8. + rows * ROW_HEIGHT + 9. + 18.).min(320.))
}

impl PaneFlowApp {
    /// Open the row menu at `position`. Called from the row's right-click
    /// hook; the row is also selected so keyboard focus follows the pointer.
    pub(crate) fn open_sessions_context_menu(
        &mut self,
        agent: SessionAgent,
        session_id: &str,
        cwd: &str,
        position: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        self.dismiss_transient_surfaces();
        self.select_session_row(agent, session_id, cx);
        self.agent_sessions.sessions_menu_open = Some(SessionContextMenu {
            agent,
            session_id: session_id.to_string(),
            position,
            cwd_missing: cwd.is_empty() || !Path::new(cwd).is_dir(),
        });
        cx.notify();
    }

    /// The row's metadata, if the scan still holds it.
    fn session_meta(&self, agent: SessionAgent, session_id: &str) -> Option<SessionMeta> {
        self.agent_sessions.sessions_by_agent[agent.index()]
            .iter()
            .find(|meta| meta.session_id == session_id)
            .cloned()
    }

    /// The workspace a handoff opens its tab in: the picker's workspace for
    /// a palette-bound sidebar, else the workspace owning the bound pane,
    /// else the active one.
    fn handoff_workspace_index(&self, cx: &gpui::App) -> usize {
        if let Some((ws_id, _)) = self.agent_sessions.sessions_bound_palette
            && let Some(idx) = self.workspaces.iter().position(|ws| ws.id == ws_id)
        {
            return idx;
        }
        if let Some(surface_id) = self.agent_sessions.sessions_surface_id
            && let Some(loc) = find_pane_by_surface_id(&self.workspaces, surface_id, cx)
        {
            return loc.workspace_idx;
        }
        self.active_idx
    }

    /// "Copy summary": the payload chain, never the transcript.
    fn copy_session_summary(
        &mut self,
        agent: SessionAgent,
        session_id: &str,
        cx: &mut Context<Self>,
    ) {
        let Some(meta) = self.session_meta(agent, session_id) else {
            self.show_toast("Could not copy summary - that session is gone", cx);
            return;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(copy_text(&meta)));
        self.show_toast("Summary copied", cx);
    }

    /// "Continue in ▸ `target`": a new workspace tab at the session's cwd
    /// running the target's launch command, with the handoff block prefilled
    /// once the CLI settles. Nothing is submitted.
    pub(crate) fn continue_session_in(
        &mut self,
        agent: SessionAgent,
        session_id: &str,
        target: TerminalAgent,
        cx: &mut Context<Self>,
    ) {
        let Some(meta) = self.session_meta(agent, session_id) else {
            self.show_toast("Could not continue session - that session is gone", cx);
            return;
        };
        let cwd = PathBuf::from(&meta.cwd);
        if meta.cwd.is_empty() || !cwd.is_dir() {
            self.show_toast(CWD_MISSING_CAPTION, cx);
            return;
        }
        let ws_idx = self.handoff_workspace_index(cx);
        let ws_id = self.workspaces[ws_idx].id;
        self.show_toast("Preparing repository handoff…", cx);
        cx.spawn(async move |this, cx| {
            let inspection_cwd = cwd.clone();
            let evidence = cx.background_spawn(async move {
                super::work_review::model::inspect(&inspection_cwd)
                    .map(|c| super::work_review::model::handoff_context(&c))
                    .unwrap_or_else(|_| "\n\nRepository snapshot unavailable. Inspect the current checkout and verification results before continuing.".into())
            }).await;
            let _ = this.update(cx, |app, cx| {
                let Some(ws_idx) = app.workspaces.iter().position(|w| w.id == ws_id) else {
                    app.show_toast("Workspace closed while preparing handoff", cx);
                    return;
                };
                let command = target.launch_command(&app.cached_config);
                let Some(terminal) = app.open_agent_tab_at_cwd(ws_idx, cwd, Some(command), Some(target), cx) else { return; };
                if ws_idx != app.active_idx { app.activate_workspace_without_window(ws_idx, cx); }
                let block = format!("{}{}", handoff_prompt(&meta, target), evidence);
                let (prompt, _) = crate::app::composer::normalize_composer_text(&block);
                Self::schedule_prompt_prefill(&terminal, prompt, usize::MAX, cx);
            });
        }).detach();
    }

    /// Build the deferred row menu. Caller guards on
    /// `self.agent_sessions.sessions_menu_open`.
    pub(crate) fn render_sessions_context_menu(
        &self,
        menu: SessionContextMenu,
        ui: crate::theme::UiColors,
        window: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let SessionContextMenu {
            agent,
            session_id,
            position,
            cwd_missing,
        } = menu;
        let targets = handoff_targets(&self.cached_config, agent);
        let menu_height = sessions_context_menu_height(targets.len());
        let menu_pos = clamped_context_menu_position(position, px(MENU_WIDTH), menu_height, window);

        let resume_id = session_id.clone();
        let copy_id = session_id.clone();
        let mut context_menu = select_menu("sessions-context-menu", ui)
            .occlude()
            .absolute()
            .left(menu_pos.x)
            .top(menu_pos.y)
            .w(px(MENU_WIDTH))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.agent_sessions.sessions_menu_open = None;
                cx.notify();
            }))
            .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
            .child(self.render_select_menu_item(
                "sessions-context-resume".into(),
                "Resume",
                None,
                ui,
                cx.listener(move |this, _: &ClickEvent, window, cx| {
                    this.agent_sessions.sessions_menu_open = None;
                    this.resume_session_from_sidebar(agent, &resume_id, window, cx);
                    cx.stop_propagation();
                }),
            ))
            .child(self.render_select_menu_item(
                "sessions-context-copy-summary".into(),
                "Copy summary",
                None,
                ui,
                cx.listener(move |this, _: &ClickEvent, _window, cx| {
                    this.agent_sessions.sessions_menu_open = None;
                    this.copy_session_summary(agent, &copy_id, cx);
                    cx.stop_propagation();
                }),
            ))
            .child(
                div()
                    .mx(px(6.))
                    .my(px(4.))
                    .h(px(1.))
                    .bg(menu_divider_color(ui)),
            )
            .child(
                div()
                    .px(px(8.))
                    .pb(px(4.))
                    .text_size(px(10.))
                    .text_color(ui.muted)
                    .child("Continue in"),
            );

        if targets.is_empty() {
            context_menu = context_menu.child(disabled_row(
                "sessions-context-no-targets",
                NO_TARGETS_CAPTION,
                ui,
            ));
        } else if cwd_missing {
            context_menu = context_menu.child(disabled_row(
                "sessions-context-no-cwd",
                CWD_MISSING_CAPTION,
                ui,
            ));
        } else {
            for (target, has_reader) in targets {
                let target_id = session_id.clone();
                context_menu = context_menu.child(
                    select_item(
                        SharedString::from(format!("sessions-context-continue-{}", target.tag())),
                        false,
                        ui,
                    )
                    .cursor(CursorStyle::Arrow)
                    .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                        this.agent_sessions.sessions_menu_open = None;
                        this.continue_session_in(agent, &target_id, target, cx);
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
                            .child(target.display_name()),
                    )
                    .when(!has_reader, |row| {
                        row.child(
                            div()
                                .flex_none()
                                .text_size(px(10.))
                                .text_color(ui.muted)
                                .child(NO_READER_HINT),
                        )
                    }),
                );
            }
        }

        deferred(context_menu).priority(3).into_any_element()
    }
}

/// A caption row: same silhouette as an item, muted, no handler.
fn disabled_row(
    id: &'static str,
    label: &'static str,
    ui: crate::theme::UiColors,
) -> impl IntoElement {
    div()
        .id(id)
        .h(px(ROW_HEIGHT))
        .px(px(8.))
        .rounded(px(7.))
        .flex()
        .flex_row()
        .items_center()
        .text_size(px(12.))
        .text_color(ui.muted)
        .child(
            div()
                .flex_1()
                .min_w_0()
                .overflow_x_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .child(label),
        )
}

#[cfg(test)]
mod tests {
    use super::sessions_context_menu_height;
    use gpui::px;

    #[test]
    fn menu_height_counts_the_fixed_rows_and_caps_at_the_surface() {
        // Two fixed rows plus the caption row when there is no target.
        assert_eq!(
            sessions_context_menu_height(0),
            px(8. + 3. * 28. + 9. + 18.)
        );
        assert_eq!(
            sessions_context_menu_height(2),
            px(8. + 4. * 28. + 9. + 18.)
        );
        assert_eq!(sessions_context_menu_height(40), px(320.));
    }

    #[test]
    fn the_menu_never_submits_the_handoff() {
        // The prefill is the settle-poll path shared with Launch Pad, and no
        // carriage return or deferred submit is scheduled after it.
        let src = include_str!("sessions_context_menu.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("production source");
        assert!(src.contains("Self::schedule_prompt_prefill(&terminal, prompt, usize::MAX, cx)"));
        assert!(!src.contains("schedule_deferred_submit"));
        assert!(!src.contains("send_command(&prompt"));
        assert!(!src.contains("\\r"));
    }
}
