//! Agent context is attached to the terminal entity, never the focused pane.
//! The inherited IDs are routing metadata under the existing same-UID IPC
//! boundary, not credentials or proof of an agent's process identity.

use gpui::{Context, Entity};
use paneflow_config::schema::{AgentContext, AgentTask, TaskAssignment, TaskReport};
use serde::Deserialize;
use serde_json::{Value, json};

use super::ipc_handler::JsonRpcError;
use crate::{PaneFlowApp, terminal::TerminalView};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ContextRequest {
    surface_id: u64,
    workspace_id: u64,
    #[serde(default)]
    assignment: Option<TaskAssignment>,
    #[serde(default)]
    task_id: Option<String>,
    #[serde(default)]
    revision: Option<u64>,
    #[serde(default)]
    report: Option<TaskReport>,
}

pub(super) fn valid_context(context: &AgentContext) -> bool {
    uuid::Uuid::parse_str(&context.pane_id).is_ok()
        && context.task.as_ref().is_none_or(|task| {
            uuid::Uuid::parse_str(&task.task_id).is_ok()
                && task.revision > 0
                && task.assignment.validate().is_ok()
                && task
                    .report
                    .as_ref()
                    .is_none_or(|report| report.validate().is_ok())
        })
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
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
        if !matches!(
            method,
            "agent.whoami" | "task.get" | "task.assign" | "task.report"
        ) {
            return Err(JsonRpcError::method_not_found(format!(
                "Method not found: {method}"
            )));
        }
        let request: ContextRequest = serde_json::from_value(params.clone())
            .map_err(|error| JsonRpcError::invalid_params(error.to_string()))?;
        // Unlike surface.read, omission must never fall back to the active pane.
        let terminal = self.resolve_readable_surface(
            &json!({"surface_id": request.surface_id, "workspace_id": request.workspace_id}),
            cx,
        )?;
        match method {
            "agent.whoami" | "task.get" => {
                if request.assignment.is_some()
                    || request.report.is_some()
                    || request.task_id.is_some()
                    || request.revision.is_some()
                {
                    return Err(JsonRpcError::invalid_params(
                        "read operations accept identity only",
                    ));
                }
            }
            "task.assign" => {
                if request.report.is_some()
                    || request.task_id.is_some()
                    || request.revision.is_some()
                {
                    return Err(JsonRpcError::invalid_params(
                        "task.assign accepts assignment only",
                    ));
                }
                let assignment = request
                    .assignment
                    .ok_or_else(|| JsonRpcError::invalid_params("assignment required"))?;
                assignment
                    .validate()
                    .map_err(JsonRpcError::invalid_params)?;
                terminal.update(cx, |view, _| {
                    view.agent_context.task = Some(AgentTask {
                        task_id: uuid::Uuid::new_v4().to_string(),
                        revision: 1,
                        assignment,
                        report: None,
                        updated_at_ms: now_ms(),
                    });
                });
                self.save_session(cx);
            }
            "task.report" => {
                if request.assignment.is_some() {
                    return Err(JsonRpcError::invalid_params(
                        "a report cannot change the assignment",
                    ));
                }
                let task_id = request
                    .task_id
                    .ok_or_else(|| JsonRpcError::invalid_params("task_id required"))?;
                let revision = request
                    .revision
                    .ok_or_else(|| JsonRpcError::invalid_params("revision required"))?;
                let report = request
                    .report
                    .ok_or_else(|| JsonRpcError::invalid_params("report required"))?;
                terminal
                    .update(cx, |view, _| {
                        view.agent_context
                            .task
                            .as_mut()
                            .ok_or("no task assigned")?
                            .apply_report(&task_id, revision, report, now_ms())
                    })
                    .map_err(JsonRpcError::invalid_params)?;
                self.save_session(cx);
            }
            _ => unreachable!(),
        }
        if method == "agent.whoami" {
            self.agent_identity(&terminal, request.workspace_id, cx)
        } else {
            Ok(json!({"pane_id": terminal.read(cx).agent_context.pane_id,
                "task": terminal.read(cx).agent_context.task}))
        }
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
        let sessions: Vec<_> = ws
            .agent_sessions
            .iter()
            .filter(|(_, session)| session.surface_id == Some(sid))
            .map(|(pid, session)| {
                json!({"process_key": pid, "tool": session.tool.tag(),
                "state": session.state.wire_str(), "source": match session.source {
                    crate::ai_types::AgentStateSource::Terminal => "terminal",
                    crate::ai_types::AgentStateSource::SessionRegistry => "session_registry",
                    crate::ai_types::AgentStateSource::Hook => "hook",
                }, "last_activity_age_ms": session.last_activity.elapsed().as_millis()})
            })
            .collect();
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
            "task_id": view.agent_context.task.as_ref().map(|task| &task.task_id),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::AppContext;

    #[test]
    fn context_requests_require_both_ids_and_reject_target_overrides() {
        for value in [
            json!({}),
            json!({"surface_id": 1}),
            json!({"surface_id": 1, "workspace_id": 2, "target": 3}),
            json!({"surface_id": "1", "workspace_id": 2}),
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
        let context = terminal.update(cx, |view, _| {
            view.agent_context.task = Some(AgentTask {
                task_id: uuid::Uuid::new_v4().to_string(),
                revision: 1,
                assignment: TaskAssignment {
                    objective: "Keep context".into(),
                    acceptance_criteria: vec![],
                    owned_files: vec![],
                },
                report: None,
                updated_at_ms: 1,
            });
            view.agent_context.clone()
        });
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
}
