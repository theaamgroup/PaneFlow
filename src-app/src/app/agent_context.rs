//! Agent context is attached to the terminal entity, never the focused pane.
//! The inherited IDs are routing metadata under the existing same-UID IPC
//! boundary, not credentials or proof of an agent's process identity.

use gpui::{App, Context, Entity};
use paneflow_config::schema::AgentContext;
use serde::Deserialize;
use serde_json::{Value, json};

use super::ipc_handler::JsonRpcError;
use crate::{PaneFlowApp, terminal::TerminalView, workspace::Workspace};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ContextRequest {
    surface_id: u64,
    /// Required so a caller outside a pane cannot omit it, but never used for
    /// routing: the surface's live workspace wins (a dragged pane keeps the
    /// inherited ID in its environment).
    #[serde(rename = "workspace_id")]
    _workspace_id: u64,
}

/// The surface's live workspace. A drag keeps the PTY and its inherited
/// workspace ID alive, so identity follows the surface, not the inherited ID.
fn context_workspace(
    workspaces: &[Workspace],
    request: &ContextRequest,
    cx: &App,
) -> Result<u64, JsonRpcError> {
    let location = super::ipc_handler::find_pane_by_surface_id(workspaces, request.surface_id, cx)
        .ok_or_else(|| JsonRpcError::invalid_params("surface not found"))?;
    Ok(workspaces[location.workspace_idx].id)
}

/// Only the pane identity is validated. A legacy `task` key (issue #810) is
/// ignored at parse time and can never discard the context.
pub(super) fn valid_context(context: &AgentContext) -> bool {
    uuid::Uuid::parse_str(&context.pane_id).is_ok()
}

fn mapped_agent_sessions(workspaces: &[Workspace], sid: u64, cx: &App) -> Vec<Value> {
    if super::ipc_handler::find_pane_by_surface_id(workspaces, sid, cx).is_none() {
        return Vec::new();
    }
    // Hook/session registries can still belong to the source workspace after
    // a move. The mapped surface, not the registry's owner, identifies these rows.
    workspaces
        .iter()
        .flat_map(|workspace| workspace.agent_sessions.iter())
        .filter(|(_, session)| session.surface_id == Some(sid))
        .map(|(pid, session)| {
            json!({"process_key": pid, "tool": session.tool.tag(),
                "state": session.state.wire_str(), "source": match session.source {
                    crate::ai_types::AgentStateSource::Terminal => "terminal",
                    crate::ai_types::AgentStateSource::SessionRegistry => "session_registry",
                    crate::ai_types::AgentStateSource::Hook => "hook",
                }, "last_activity_age_ms": session.last_activity.elapsed().as_millis()})
        })
        .collect()
}

/// Session and undo reconstruction share the same validated restore path.
pub(super) fn restore_context(view: &mut TerminalView, context: Option<&AgentContext>) {
    if let Some(context) = context.filter(|context| valid_context(context)) {
        view.agent_context = context.clone();
    }
}

impl PaneFlowApp {
    pub(super) fn handle_agent_context_method(
        &mut self,
        method: &str,
        params: &Value,
        cx: &mut Context<Self>,
    ) -> Value {
        match self.agent_context_request(method, params, cx) {
            Ok(value) => value,
            Err(error) => error.into_value(),
        }
    }

    fn agent_context_request(
        &mut self,
        method: &str,
        params: &Value,
        cx: &mut Context<Self>,
    ) -> Result<Value, JsonRpcError> {
        if method != "agent.whoami" {
            return Err(JsonRpcError::method_not_found(format!(
                "Method not found: {method}"
            )));
        }
        let request: ContextRequest = serde_json::from_value(params.clone())
            .map_err(|error| JsonRpcError::invalid_params(error.to_string()))?;
        let workspace_id = context_workspace(&self.workspaces, &request, cx)?;
        // Unlike surface.read, omission must never fall back to the active pane.
        let terminal = self.resolve_readable_surface(
            &json!({"surface_id": request.surface_id, "workspace_id": workspace_id}),
            cx,
        )?;
        self.agent_identity(&terminal, workspace_id, cx)
    }

    fn agent_identity(
        &self,
        terminal: &Entity<TerminalView>,
        workspace_id: u64,
        cx: &Context<Self>,
    ) -> Result<Value, JsonRpcError> {
        let sid = terminal.entity_id().as_u64();
        let ws = self
            .workspaces
            .iter()
            .find(|ws| ws.id == workspace_id)
            .ok_or_else(|| JsonRpcError::invalid_params("workspace vanished"))?;
        let meta = self
            .collect_surface_meta(cx)
            .into_iter()
            .find(|meta| meta.surface_id == sid)
            .ok_or_else(|| JsonRpcError::invalid_params("surface vanished"))?;
        let tab = ws.tabs().iter().find(|tab| Some(tab.id) == meta.tab_id);
        let sessions = mapped_agent_sessions(&self.workspaces, sid, cx);
        let view = terminal.read(cx);
        Ok(json!({
            "identity_source": "inherited_environment",
            "pane_id": view.agent_context.pane_id,
            "surface_id": sid,
            "terminal_session_id": view.terminal_session_id,
            "workspace_id": ws.id, "workspace": ws.title,
            "workspace_cwd": ws.cwd, "cwd": meta.cwd,
            "tab_id": meta.tab_id,
            "worktree": tab.and_then(|tab| tab.worktree.as_ref()),
            "agent_sessions": sessions,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::AppContext;

    #[gpui::test]
    fn moved_pane_keeps_agent_evidence_held_in_the_source_workspace(cx: &mut gpui::TestAppContext) {
        let cx = cx.add_empty_window();
        let terminal = cx.new(|cx| TerminalView::display_only_for_test(1, cx));
        let sid = terminal.entity_id().as_u64();
        let pane = cx.new(|cx| crate::pane::Pane::new(terminal, 1, cx));
        let mut source = Workspace::with_layout_and_id(
            1,
            "source",
            std::path::PathBuf::new(),
            crate::layout::LayoutTree::Leaf(pane.clone()),
        );
        let mut session = crate::ai_types::AgentSession::new(
            crate::agent_launcher::TerminalAgent::Codex,
            crate::ai_types::AgentState::Thinking,
        );
        session.surface_id = Some(sid);
        source.agent_sessions.insert(42, session.clone());
        // An unrelated mapped agent and an unresolved row must not leak in.
        session.surface_id = Some(sid + 1000);
        source.agent_sessions.insert(43, session.clone());
        session.surface_id = None;
        source.agent_sessions.insert(44, session);
        let mut destination =
            Workspace::empty_with_cwd_and_id(2, "destination", std::path::PathBuf::new());
        let tab = source.close_tab(0).expect("detach");
        pane.update(cx, |pane, _| pane.workspace_id = 2);
        assert!(destination.open_tab(tab));
        let workspaces = vec![source, destination];
        let rows = cx.update(|_, cx| mapped_agent_sessions(&workspaces, sid, cx));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["process_key"], 42);
        assert_eq!(rows[0]["tool"], "codex");
        assert_eq!(
            rows[0]["state"],
            crate::ai_types::AgentState::Thinking.wire_str()
        );
    }

    #[gpui::test]
    fn hook_frames_follow_the_surface_to_its_live_workspace(cx: &mut gpui::TestAppContext) {
        let cx = cx.add_empty_window();
        let terminal = cx.new(|cx| TerminalView::display_only_for_test(1, cx));
        let sid = terminal.entity_id().as_u64();
        let pane = cx.new(|cx| crate::pane::Pane::new(terminal, 1, cx));
        let mut source = Workspace::with_layout_and_id(
            1,
            "source",
            std::path::PathBuf::new(),
            crate::layout::LayoutTree::Leaf(pane.clone()),
        );
        let mut destination =
            Workspace::empty_with_cwd_and_id(2, "destination", std::path::PathBuf::new());
        let tab = source.close_tab(0).expect("detach");
        pane.update(cx, |pane, _| pane.workspace_id = 2);
        assert!(destination.open_tab(tab));
        let workspaces = vec![source, destination];
        let mut frame = |params: serde_json::Value| {
            cx.update(|_, cx| {
                super::super::ipc_handler::frame_workspace_id(&workspaces, &params, cx)
            })
        };
        // The inherited id is stale after the move; the surface's workspace wins.
        assert_eq!(
            frame(json!({"workspace_id": 1, "surface_id": sid})),
            Some(2)
        );
        // No surface, or one that no longer exists: the inherited id as before.
        assert_eq!(frame(json!({"workspace_id": 1})), Some(1));
        assert_eq!(
            frame(json!({"workspace_id": 1, "surface_id": sid + 1000})),
            Some(1)
        );
        assert_eq!(frame(json!({"surface_id": sid})), None);
    }

    #[gpui::test]
    fn agent_context_follows_a_live_tab_move_with_unchanged_inherited_ids(
        cx: &mut gpui::TestAppContext,
    ) {
        let cx = cx.add_empty_window();
        let terminal = cx.new(|cx| TerminalView::display_only_for_test(1, cx));
        let pane = cx.new(|cx| crate::pane::Pane::new(terminal.clone(), 1, cx));
        let mut workspaces = vec![
            Workspace::with_layout_and_id(
                1,
                "source",
                std::path::PathBuf::new(),
                crate::layout::LayoutTree::Leaf(pane.clone()),
            ),
            Workspace::empty_with_cwd_and_id(2, "destination", std::path::PathBuf::new()),
        ];
        let request: ContextRequest = serde_json::from_value(json!({
            "surface_id": terminal.entity_id().as_u64(), "workspace_id": 1,
        }))
        .expect("inherited identity");
        cx.update(|_, cx| {
            assert_eq!(
                context_workspace(&workspaces, &request, cx).expect("before move"),
                1
            )
        });
        // The production drag path transfers this tab and retains its PTY.
        let tab = workspaces[0].close_tab(0).expect("detach tab");
        pane.update(cx, |pane, _| pane.workspace_id = 2);
        assert!(workspaces[1].open_tab(tab));
        // Also cover a source workspace being closed after the move.
        workspaces.remove(0);
        cx.update(|_, cx| {
            assert_eq!(
                context_workspace(&workspaces, &request, cx).expect("moved context"),
                2
            );
            assert_eq!(
                super::super::ipc_handler::find_terminal_by_surface_id(
                    &workspaces,
                    request.surface_id,
                    cx
                ),
                Some(terminal.clone())
            );
        });
        workspaces[0].close_tab(0).expect("close moved tab");
        cx.update(|_, cx| assert!(context_workspace(&workspaces, &request, cx).is_err()));
    }

    #[test]
    fn context_requests_require_both_ids_and_reject_target_overrides() {
        for value in [
            json!({}),
            json!({"surface_id": 1}),
            json!({"surface_id": 1, "workspace_id": 2, "target": 3}),
            json!({"surface_id": "1", "workspace_id": 2}),
            json!({"surface_id": 1, "workspace_id": 2, "task_id": "t", "revision": 1}),
        ] {
            assert!(serde_json::from_value::<ContextRequest>(value).is_err());
        }
    }

    #[gpui::test]
    fn persisted_context_survives_layout_capture_and_terminal_reconstruction(
        cx: &mut gpui::TestAppContext,
    ) {
        let cx = cx.add_empty_window();
        let terminal = cx.new(|cx| TerminalView::display_only_for_test(1, cx));
        let context = cx.update(|_, cx| terminal.read(cx).agent_context.clone());
        let old_session = cx.update(|_, cx| terminal.read(cx).terminal_session_id.clone());
        let pane = cx.new(|cx| crate::pane::Pane::new(terminal.clone(), 1, cx));
        let layout = cx
            .update(|_, cx| crate::layout::LayoutTree::Leaf(pane).serialize_without_scrollback(cx));
        let encoded = serde_json::to_string(&layout).expect("save layout");
        let layout: paneflow_config::schema::LayoutNode =
            serde_json::from_str(&encoded).expect("load layout");
        let paneflow_config::schema::LayoutNode::Pane { surfaces } = layout else {
            panic!("expected pane")
        };
        assert_eq!(surfaces[0].agent_context.as_ref(), Some(&context));
        let restored = cx.new(|cx| TerminalView::display_only_for_test(1, cx));
        restored.update(cx, |view, _| {
            restore_context(view, surfaces[0].agent_context.as_ref())
        });
        cx.update(|_, cx| {
            assert_eq!(restored.read(cx).agent_context, context);
            assert_ne!(restored.read(cx).terminal_session_id, old_session);
        });
    }

    /// Issue #810: every surface saved before task assignment was removed
    /// carries `"task": null`, and an assigned pane carries a full task
    /// object, even one the old validator would have rejected. Both must
    /// restore with their `pane_id`.
    #[gpui::test]
    fn legacy_task_keys_restore_with_their_pane_id(cx: &mut gpui::TestAppContext) {
        let cx = cx.add_empty_window();
        let null_task = json!({"pane_id": "5d0f4b8a-9c3e-4f21-8a6b-1e2d3c4b5a69", "task": null});
        let full_task = json!({
            "pane_id": "7a1c9e2e-5b1f-4a37-9c43-0f2b8f0e7d11",
            "task": {
                "task_id": "not-a-uuid",
                "revision": 0,
                "updated_at_ms": 1,
                "assignment": {"objective": "", "acceptance_criteria": [], "owned_files": []},
                "report": {"status": "completed", "summary": "Fixed", "changed_files": [],
                    "commits": [], "tests": [], "unresolved_questions": []}
            }
        });
        for legacy in [null_task, full_task] {
            let pane_id = legacy["pane_id"].as_str().expect("pane_id").to_string();
            let context: AgentContext =
                serde_json::from_value(legacy).expect("legacy agent_context loads");
            assert_eq!(context.pane_id, pane_id);
            assert!(
                valid_context(&context),
                "a legacy task must not discard the context"
            );
            let restored = cx.new(|cx| TerminalView::display_only_for_test(1, cx));
            restored.update(cx, |view, _| restore_context(view, Some(&context)));
            cx.update(|_, cx| assert_eq!(restored.read(cx).agent_context.pane_id, pane_id));
        }
    }
}
