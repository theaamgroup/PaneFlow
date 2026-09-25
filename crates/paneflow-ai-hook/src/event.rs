use std::fmt;
use std::str::FromStr;

use paneflow_ipc_client::ai_hook::{
    AiHookFrame, AiHookMethod, AiHookParams, AiToolName, LifecycleEventSource, SessionPid,
    SurfaceId,
};
use serde_json::Value;

pub(crate) const MAX_HOOK_TEXT_BYTES: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HookEvent {
    SessionStart,
    SessionEnd,
    UserPromptSubmit,
    Notification,
    Stop,
    SubagentStart,
    SubagentStop,
    PreToolUse,
    PostToolUse,
    PermissionRequest,
    Exit,
}

impl HookEvent {
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::SessionStart => "SessionStart",
            Self::SessionEnd => "SessionEnd",
            Self::UserPromptSubmit => "UserPromptSubmit",
            Self::Notification => "Notification",
            Self::Stop => "Stop",
            Self::SubagentStart => "SubagentStart",
            Self::SubagentStop => "SubagentStop",
            Self::PreToolUse => "PreToolUse",
            Self::PostToolUse => "PostToolUse",
            Self::PermissionRequest => "PermissionRequest",
            Self::Exit => "Exit",
        }
    }

    pub(crate) const fn input_source(self) -> InputSource {
        match self {
            Self::SessionEnd => InputSource::Empty,
            Self::Exit => InputSource::ExitCodeEnvironment,
            _ => InputSource::Stdin,
        }
    }

    const fn carries_interrupt_source(self) -> bool {
        matches!(self, Self::Stop | Self::Exit | Self::SessionEnd)
    }
}

impl FromStr for HookEvent {
    type Err = ();

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        match raw {
            "SessionStart" => Ok(Self::SessionStart),
            "SessionEnd" => Ok(Self::SessionEnd),
            "UserPromptSubmit" => Ok(Self::UserPromptSubmit),
            "Notification" => Ok(Self::Notification),
            "Stop" => Ok(Self::Stop),
            "SubagentStart" => Ok(Self::SubagentStart),
            "SubagentStop" => Ok(Self::SubagentStop),
            "PreToolUse" => Ok(Self::PreToolUse),
            "PostToolUse" => Ok(Self::PostToolUse),
            "PermissionRequest" => Ok(Self::PermissionRequest),
            "Exit" => Ok(Self::Exit),
            _ => Err(()),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InputSource {
    Stdin,
    Empty,
    ExitCodeEnvironment,
}

pub(crate) struct FrameContext {
    pub(crate) workspace_id: u64,
    pub(crate) tool: AiToolName,
    pub(crate) pid: Option<SessionPid>,
    pub(crate) surface_id: Option<SurfaceId>,
    pub(crate) event_source: Option<LifecycleEventSource>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum DropReason {
    InformationalNotification(Option<String>),
    LlmCallContinuesWithToolCalls(u64),
    MissingSubagentId,
}

impl fmt::Display for DropReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InformationalNotification(kind) => {
                write!(formatter, "dropping notification_type={kind:?}")
            }
            Self::LlmCallContinuesWithToolCalls(count) => {
                write!(
                    formatter,
                    "dropping PostLLMCall with tool_call_count={count}"
                )
            }
            Self::MissingSubagentId => {
                formatter.write_str("dropping subagent event without an agent id")
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum BuildError {
    MissingSessionPid,
    MissingOrInvalidExitCode,
}

impl fmt::Display for BuildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingSessionPid => {
                formatter.write_str("missing pid (set PANEFLOW_AI_PID or include pid in hook JSON)")
            }
            Self::MissingOrInvalidExitCode => formatter.write_str("missing or invalid exit_code"),
        }
    }
}

#[derive(Debug)]
pub(crate) enum BuildOutcome {
    Send(AiHookFrame),
    Drop(DropReason),
}

pub(crate) fn build_frame(
    event: HookEvent,
    context: FrameContext,
    hook_payload: Value,
) -> Result<BuildOutcome, BuildError> {
    let session_pid = context.pid.or_else(|| {
        hook_payload
            .get("pid")
            .and_then(Value::as_u64)
            .and_then(SessionPid::from_u64)
    });

    let subagent_heartbeat = matches!(event, HookEvent::PreToolUse | HookEvent::PostToolUse)
        && context.tool.as_str() == paneflow_ipc_client::ai_hook::DEFAULT_TOOL
        && hook_payload
            .get("agent_id")
            .and_then(Value::as_str)
            .is_some_and(|id| !id.trim().is_empty());

    let method = match event {
        HookEvent::SessionStart => {
            if session_pid.is_none() {
                return Err(BuildError::MissingSessionPid);
            }
            AiHookMethod::SessionStart
        }
        HookEvent::SessionEnd => AiHookMethod::SessionEnd,
        HookEvent::UserPromptSubmit => AiHookMethod::PromptSubmit,
        HookEvent::Notification => {
            let notification_type = hook_payload
                .get("notification_type")
                .and_then(Value::as_str);
            if !matches!(
                notification_type,
                Some("permission_prompt" | "elicitation_dialog")
            ) {
                return Ok(BuildOutcome::Drop(DropReason::InformationalNotification(
                    notification_type.map(|kind| truncate_utf8(kind, 128)),
                )));
            }
            AiHookMethod::Notification
        }
        HookEvent::Stop => {
            if let Some(count) = pending_tool_calls_after_llm_call(&hook_payload) {
                return Ok(BuildOutcome::Drop(
                    DropReason::LlmCallContinuesWithToolCalls(count),
                ));
            }
            AiHookMethod::Stop
        }
        // A subagent ending is not the parent's turn ending: mapping it to
        // `ai.stop` marked the whole session finished while it still worked.
        // An event with no id cannot be paired with its other half, and an
        // unpaired start would hold the count up for the life of the agent.
        HookEvent::SubagentStart | HookEvent::SubagentStop => {
            if subagent_id(&hook_payload).is_none() {
                return Ok(BuildOutcome::Drop(DropReason::MissingSubagentId));
            }
            if event == HookEvent::SubagentStart {
                AiHookMethod::SubagentStart
            } else {
                AiHookMethod::SubagentStop
            }
        }
        // Claude Code puts `agent_id` on a hook only when it fires inside a
        // subagent. That tool call is the subagent's: sent as the parent's,
        // it would mark a parent whose turn already ended as working again,
        // and no later frame would finish it, since a subagent's stop no
        // longer stops the parent. It goes out as an idempotent subagent
        // start instead, a heartbeat that keeps a working parent's activity
        // clock fresh and restores a lost start. Other agents' tool payloads
        // are not documented to carry the id only then, so theirs go through.
        HookEvent::PreToolUse | HookEvent::PostToolUse if subagent_heartbeat => {
            if subagent_id(&hook_payload).is_none() {
                return Ok(BuildOutcome::Drop(DropReason::MissingSubagentId));
            }
            AiHookMethod::SubagentStart
        }
        HookEvent::PreToolUse | HookEvent::PostToolUse => AiHookMethod::ToolUse,
        HookEvent::PermissionRequest => AiHookMethod::Notification,
        HookEvent::Exit => AiHookMethod::Exit,
    };

    let compact_payload = if subagent_heartbeat {
        let mut compact = serde_json::Map::new();
        if let Some(id) = subagent_id(&hook_payload) {
            compact.insert("subagent_id".to_owned(), Value::String(id));
        }
        Value::Object(compact)
    } else {
        compact_hook_payload(event, &hook_payload)
    };
    let mut params = AiHookParams::new(context.workspace_id, context.tool, compact_payload);
    params.pid = session_pid;
    // Stamped here, in the producing process, not on arrival: the server keeps
    // a per-session watermark and drops a frame that lost its race with a
    // later one (an `ai.stop` overtaken by the shim's `ai.exit`, say).
    params.emitted_at_ms = paneflow_ipc_client::ai_hook::epoch_millis();
    params.surface_id = context.surface_id;

    if matches!(event, HookEvent::PreToolUse | HookEvent::PostToolUse) && !subagent_heartbeat {
        params.tool_name = hook_payload
            .get("tool_name")
            .and_then(Value::as_str)
            .map(|name| truncate_utf8(name, 128));
    }
    if event == HookEvent::Exit {
        params.exit_code = hook_payload
            .get("exit_code")
            .and_then(Value::as_i64)
            .and_then(|code| i32::try_from(code).ok());
        if params.exit_code.is_none() {
            return Err(BuildError::MissingOrInvalidExitCode);
        }
    }
    if event.carries_interrupt_source() {
        params.event_source = context.event_source;
    }

    Ok(BuildOutcome::Send(AiHookFrame::new(method, params)))
}

/// Payload keys that carry a subagent's pairing id, one per agent family:
/// Claude Code and Codex (`agent_id`), Cursor (`subagent_id`), and Grok's
/// camelCase spellings. The same id arrives on both halves of the pair.
const SUBAGENT_ID_KEYS: &[&str] = &["agent_id", "subagent_id", "subagentId", "agentId"];

fn subagent_id(payload: &Value) -> Option<String> {
    SUBAGENT_ID_KEYS.iter().find_map(|key| {
        payload
            .get(*key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|id| {
                !id.is_empty() && id.len() <= paneflow_ipc_client::ai_hook::MAX_SUBAGENT_ID_BYTES
            })
            .map(str::to_owned)
    })
}

fn pending_tool_calls_after_llm_call(payload: &Value) -> Option<u64> {
    if payload.get("hook_event_name").and_then(Value::as_str) != Some("PostLLMCall") {
        return None;
    }
    payload
        .get("tool_call_count")
        .and_then(Value::as_u64)
        .filter(|count| *count > 0)
}

fn compact_hook_payload(event: HookEvent, payload: &Value) -> Value {
    let mut compact = serde_json::Map::new();
    copy_string_field(payload, &mut compact, "session_id", 256);
    copy_u64_field(payload, &mut compact, "pid");

    match event {
        HookEvent::SessionStart => copy_string_field(payload, &mut compact, "cwd", 2048),
        HookEvent::UserPromptSubmit => {}
        HookEvent::Notification => {
            copy_string_field(payload, &mut compact, "notification_type", 128);
            copy_string_field(payload, &mut compact, "message", MAX_HOOK_TEXT_BYTES);
        }
        HookEvent::PermissionRequest => {
            copy_string_field(payload, &mut compact, "message", MAX_HOOK_TEXT_BYTES);
        }
        HookEvent::PreToolUse | HookEvent::PostToolUse => {
            copy_string_field(payload, &mut compact, "tool_name", 128);
        }
        HookEvent::SubagentStart | HookEvent::SubagentStop => {
            if let Some(id) = subagent_id(payload) {
                compact.insert("subagent_id".to_owned(), Value::String(id));
            }
        }
        HookEvent::Stop | HookEvent::SessionEnd => {
            copy_string_field(payload, &mut compact, "summary", MAX_HOOK_TEXT_BYTES);
            copy_string_field(payload, &mut compact, "last_result", MAX_HOOK_TEXT_BYTES);
            copy_string_field(payload, &mut compact, "transcript_path", 2048);
        }
        HookEvent::Exit => {
            copy_i64_field(payload, &mut compact, "exit_code");
            copy_string_field(payload, &mut compact, "summary", MAX_HOOK_TEXT_BYTES);
        }
    }

    Value::Object(compact)
}

fn copy_string_field(
    source: &Value,
    target: &mut serde_json::Map<String, Value>,
    key: &str,
    max_bytes: usize,
) {
    if let Some(value) = source.get(key).and_then(Value::as_str) {
        target.insert(
            key.to_owned(),
            Value::String(truncate_utf8(value, max_bytes)),
        );
    }
}

fn copy_u64_field(source: &Value, target: &mut serde_json::Map<String, Value>, key: &str) {
    if let Some(value) = source.get(key).and_then(Value::as_u64) {
        target.insert(key.to_owned(), Value::from(value));
    }
}

fn copy_i64_field(source: &Value, target: &mut serde_json::Map<String, Value>, key: &str) {
    if let Some(value) = source.get(key).and_then(Value::as_i64) {
        target.insert(key.to_owned(), Value::from(value));
    }
}

fn truncate_utf8(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_owned();
    }
    const MARKER: &str = "...[truncated]";
    if max_bytes <= MARKER.len() {
        return MARKER[..max_bytes].to_owned();
    }
    let keep = max_bytes - MARKER.len();
    let mut boundary = keep.min(value.len());
    while boundary > 0 && !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    format!("{}{}", &value[..boundary], MARKER)
}

#[cfg(test)]
mod tests {
    use super::*;
    use paneflow_ipc_client::ai_hook::{AiToolName, LifecycleEventSource, SessionPid};
    use serde_json::json;

    fn test_context() -> FrameContext {
        FrameContext {
            workspace_id: 7,
            tool: AiToolName::parse("claude").expect("valid test tool"),
            pid: None,
            surface_id: None,
            event_source: None,
        }
    }

    fn sent_frame(outcome: BuildOutcome) -> Value {
        match outcome {
            BuildOutcome::Send(frame) => frame.to_value(),
            BuildOutcome::Drop(reason) => panic!("unexpected drop: {reason}"),
        }
    }

    #[test]
    fn supported_events_map_to_methods() {
        let cases = [
            (HookEvent::UserPromptSubmit, json!({}), "ai.prompt_submit"),
            (
                HookEvent::Notification,
                json!({"notification_type": "permission_prompt"}),
                "ai.notification",
            ),
            (HookEvent::Stop, json!({}), "ai.stop"),
            (
                HookEvent::Stop,
                json!({"hook_event_name": "PostLLMCall", "tool_call_count": 0}),
                "ai.stop",
            ),
            (
                HookEvent::SubagentStart,
                json!({"agent_id": "a68ad35317fb486f6"}),
                "ai.subagent_start",
            ),
            (
                HookEvent::SubagentStop,
                json!({"agent_id": "a68ad35317fb486f6"}),
                "ai.subagent_stop",
            ),
            (
                HookEvent::PreToolUse,
                json!({"tool_name": "Bash"}),
                "ai.tool_use",
            ),
            (
                HookEvent::PostToolUse,
                json!({"tool_name": "Edit"}),
                "ai.tool_use",
            ),
            (HookEvent::PermissionRequest, json!({}), "ai.notification"),
            (HookEvent::SessionEnd, json!({}), "ai.session_end"),
        ];

        for (event, payload, expected_method) in cases {
            let frame =
                sent_frame(build_frame(event, test_context(), payload).expect("valid frame"));
            assert_eq!(frame["method"], expected_method, "event={}", event.name());
        }
    }

    #[test]
    fn stop_from_a_post_llm_call_with_tool_calls_is_dropped() {
        let outcome = build_frame(
            HookEvent::Stop,
            test_context(),
            json!({"hook_event_name": "PostLLMCall", "tool_call_count": 2}),
        )
        .expect("valid payload");
        match outcome {
            BuildOutcome::Drop(reason) => {
                assert_eq!(reason, DropReason::LlmCallContinuesWithToolCalls(2));
            }
            BuildOutcome::Send(frame) => panic!("unexpected frame: {:?}", frame.to_value()),
        }
    }

    #[test]
    fn subagent_events_carry_the_pairing_id_from_each_agent_family() {
        let cases = [
            (json!({"agent_id": "claude-or-codex"}), "claude-or-codex"),
            (json!({"subagent_id": "cursor-id"}), "cursor-id"),
            (json!({"subagentId": "grok-id"}), "grok-id"),
            (json!({"agentId": " grok-alt "}), "grok-alt"),
        ];
        for (payload, expected) in cases {
            for event in [HookEvent::SubagentStart, HookEvent::SubagentStop] {
                let frame = sent_frame(
                    build_frame(event, test_context(), payload.clone()).expect("valid frame"),
                );
                assert_eq!(frame["params"]["hook_payload"]["subagent_id"], expected);
                assert!(frame["params"]["hook_payload"].get("summary").is_none());
            }
        }
    }

    #[test]
    fn a_claude_subagents_tool_use_is_a_subagent_heartbeat_not_the_parents() {
        for event in [HookEvent::PreToolUse, HookEvent::PostToolUse] {
            let payload = json!({"tool_name": "Bash", "agent_id": "a68ad35317fb486f6"});
            let frame =
                sent_frame(build_frame(event, test_context(), payload.clone()).expect("frame"));
            assert_eq!(frame["method"], "ai.subagent_start");
            assert_eq!(
                frame["params"]["hook_payload"],
                json!({"subagent_id": "a68ad35317fb486f6"})
            );
            assert!(frame["params"].get("tool_name").is_none());
            // The parent's own tool use, and other agents', still go through.
            let parent = sent_frame(
                build_frame(event, test_context(), json!({"tool_name": "Bash"})).expect("frame"),
            );
            assert_eq!(parent["method"], "ai.tool_use");
            let codex = FrameContext {
                tool: AiToolName::parse("codex").expect("valid tool"),
                ..test_context()
            };
            let frame = sent_frame(build_frame(event, codex, payload).expect("frame"));
            assert_eq!(frame["method"], "ai.tool_use");
        }
    }

    #[test]
    fn subagent_event_without_an_id_is_dropped_not_sent_as_a_stop() {
        for payload in [
            json!({}),
            json!({"agent_id": ""}),
            json!({"agent_id": "x".repeat(129)}),
        ] {
            for event in [HookEvent::SubagentStart, HookEvent::SubagentStop] {
                match build_frame(event, test_context(), payload.clone()).expect("not an error") {
                    BuildOutcome::Drop(reason) => {
                        assert_eq!(reason, DropReason::MissingSubagentId);
                    }
                    BuildOutcome::Send(frame) => {
                        panic!("unexpected frame: {:?}", frame.to_value())
                    }
                }
            }
        }
    }

    #[test]
    fn session_start_requires_a_canonical_pid() {
        assert_eq!(
            build_frame(HookEvent::SessionStart, test_context(), json!({})).unwrap_err(),
            BuildError::MissingSessionPid
        );

        let frame = sent_frame(
            build_frame(HookEvent::SessionStart, test_context(), json!({"pid": 42}))
                .expect("payload pid is valid"),
        );
        assert_eq!(frame["params"]["pid"], 42);
        assert!(frame["params"].get("session_id").is_none());
    }

    #[test]
    fn environment_pid_wins_over_payload_pid() {
        let mut context = test_context();
        context.pid = SessionPid::new(4242);
        let frame = sent_frame(
            build_frame(HookEvent::Stop, context, json!({"pid": 7777})).expect("valid stop"),
        );
        assert_eq!(frame["params"]["pid"], 4242);
        assert_eq!(frame["params"]["hook_payload"]["pid"], 7777);
    }

    #[test]
    fn informational_notification_has_an_explicit_drop_outcome() {
        let outcome = build_frame(
            HookEvent::Notification,
            test_context(),
            json!({"notification_type": "idle_prompt"}),
        )
        .expect("drop is not an error");
        match outcome {
            BuildOutcome::Drop(DropReason::InformationalNotification(Some(kind))) => {
                assert_eq!(kind, "idle_prompt");
            }
            BuildOutcome::Drop(other) => panic!("unexpected drop: {other}"),
            BuildOutcome::Send(_) => panic!("informational notification was sent"),
        }
    }

    #[test]
    fn permission_request_does_not_emit_speculative_metadata() {
        let frame = sent_frame(
            build_frame(
                HookEvent::PermissionRequest,
                test_context(),
                json!({"message": "Allow?"}),
            )
            .expect("valid permission request"),
        );
        assert!(frame["params"].get("notification_type").is_none());
        assert_eq!(frame["params"]["hook_payload"]["message"], "Allow?");
    }

    #[test]
    fn exit_code_is_typed_at_the_boundary() {
        assert_eq!(
            build_frame(
                HookEvent::Exit,
                test_context(),
                json!({"exit_code": i64::MAX}),
            )
            .unwrap_err(),
            BuildError::MissingOrInvalidExitCode
        );
        let frame = sent_frame(
            build_frame(
                HookEvent::Exit,
                test_context(),
                json!({"exit_code": -1_073_741_510_i32}),
            )
            .expect("signed i32 exit code is accepted"),
        );
        assert_eq!(frame["params"]["exit_code"], -1_073_741_510_i64);
    }

    #[test]
    fn interrupt_source_is_limited_to_lifecycle_events() {
        let mut context = test_context();
        context.event_source = Some(LifecycleEventSource::Interrupt);
        let stop =
            sent_frame(build_frame(HookEvent::Stop, context, json!({})).expect("valid stop frame"));
        assert_eq!(stop["params"]["event_source"], "interrupt");

        // Exit and SessionEnd are the shim-synthesized interrupt frames
        // (Ctrl+C / pane close); the server suppresses stop summaries on
        // `event_source=interrupt`, so both must forward it.
        let mut context = test_context();
        context.event_source = Some(LifecycleEventSource::Interrupt);
        let exit = sent_frame(
            build_frame(HookEvent::Exit, context, json!({"exit_code": 130}))
                .expect("valid exit frame"),
        );
        assert_eq!(exit["params"]["event_source"], "interrupt");
        assert_eq!(exit["params"]["exit_code"], 130);

        let mut context = test_context();
        context.event_source = Some(LifecycleEventSource::Interrupt);
        let session_end = sent_frame(
            build_frame(HookEvent::SessionEnd, context, json!({}))
                .expect("valid session end frame"),
        );
        assert_eq!(session_end["params"]["event_source"], "interrupt");

        let mut context = test_context();
        context.event_source = Some(LifecycleEventSource::Interrupt);
        let prompt = sent_frame(
            build_frame(HookEvent::UserPromptSubmit, context, json!({})).expect("valid prompt"),
        );
        assert!(prompt["params"].get("event_source").is_none());
    }

    #[test]
    fn payload_compaction_drops_prompts_and_caps_text() {
        let payload = json!({
            "session_id": "s1",
            "prompt": "x".repeat(10_000),
            "message": "é".repeat(10_000),
            "notification_type": "permission_prompt",
        });
        let prompt = compact_hook_payload(HookEvent::UserPromptSubmit, &payload);
        assert_eq!(prompt, json!({"session_id": "s1"}));

        let notification = compact_hook_payload(HookEvent::Notification, &payload);
        let message = notification["message"].as_str().expect("message string");
        assert!(message.len() <= MAX_HOOK_TEXT_BYTES);
        assert!(message.is_char_boundary(message.len()));
    }
}
