//! Session handoff to another harness (issue #334): the pure layer.
//!
//! Everything the tests pin lives here and nothing touches GPUI. The
//! sessions-sidebar row menu (`sessions_context_menu.rs`) turns a
//! [`SessionMeta`] into the block a *different* agent is started with, and
//! into the clipboard text of "Copy summary"; both read the same payload
//! chain so they can never disagree. Never the raw transcript, never the
//! session file path.

use std::borrow::Cow;

use paneflow_config::schema::PaneFlowConfig;

use crate::agent_launcher::TerminalAgent;
use crate::agent_sessions::{SessionAgent, SessionMeta};

/// Ceiling on the summary section of the handoff block, in bytes. The cut
/// lands on a char boundary and is marked with [`TRUNCATION_MARKER`].
pub(crate) const HANDOFF_SUMMARY_CAP: usize = 4 * 1024;

/// Appended to a summary that was cut at [`HANDOFF_SUMMARY_CAP`].
const TRUNCATION_MARKER: &str = " […]";

/// Printed in place of a session id that fails the resume allow-list
/// (`agent_sessions::is_valid_session_id`): the block still reads, but an
/// unvalidated token is never pasted into another agent's prompt.
const WITHHELD_ID: &str = "(id withheld)";

/// Where the handoff text came from. Recorded so the block can say when only
/// the identifier is known.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PayloadKind {
    /// `SessionMeta::summary`: an LLM title, or the first user message every
    /// reader already folds into that field.
    Summary,
    /// A first user message a reader records *separately* from its title.
    /// No reader does today ([`first_user_message`] is the seam one would
    /// fill), so [`payload_for`] never returns it; a test pins that.
    FirstUserMessage,
    /// Nothing usable was recorded: the text is the identifier line.
    Identifier,
}

/// The text "Copy summary" puts on the clipboard and the block's summary
/// section carries: already capped, already trimmed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HandoffPayload {
    pub kind: PayloadKind,
    pub text: String,
}

/// The payload chain: `summary` when present and not identifier-shaped,
/// otherwise the identifier line. There is no second field to fall to
/// (every reader folds the first user message into `summary`), so an
/// identifier-shaped summary is treated as absent.
pub(crate) fn payload_for(meta: &SessionMeta) -> HandoffPayload {
    let summary = meta
        .summary
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty() && !is_identifier_shaped(s));
    if let Some(summary) = summary {
        return HandoffPayload {
            kind: PayloadKind::Summary,
            text: cap_summary(summary),
        };
    }
    if let Some(first) = first_user_message(meta) {
        return HandoffPayload {
            kind: PayloadKind::FirstUserMessage,
            text: cap_summary(&first),
        };
    }
    HandoffPayload {
        kind: PayloadKind::Identifier,
        text: identifier_line(meta),
    }
}

/// The seam for a reader that records the first user message apart from the
/// title. `SessionMeta` has no such field: Claude prefers an AI title and
/// falls back to the first user message inside `summary`
/// (`claude_sessions.rs`), Codex and Pi derive `summary` from the first user
/// record, OpenCode stores a title, Gemini and Cursor scrape a line of CLI
/// output. So this is `None` for every reader until one grows the field.
fn first_user_message(_meta: &SessionMeta) -> Option<String> {
    None
}

/// Clipboard text of "Copy summary": the same chain as the block, not the
/// block itself, so the two gestures cannot disagree. Never the raw JSONL,
/// never the file path.
pub(crate) fn copy_text(meta: &SessionMeta) -> String {
    payload_for(meta).text
}

/// The block a different harness is started with. `target` is carried for a
/// future per-target preamble; today every target gets the same text.
pub(crate) fn handoff_prompt(meta: &SessionMeta, _target: TerminalAgent) -> String {
    let payload = payload_for(meta);
    let mut block = format!(
        "Continue this work from a prior {} session.\nSession: {}\nCwd: {}\n",
        meta.agent.label(),
        display_id(meta),
        meta.cwd
    );
    if !meta.git_branch.is_empty() {
        block.push_str("Branch: ");
        block.push_str(&meta.git_branch);
        block.push('\n');
    }
    match payload.kind {
        PayloadKind::Summary | PayloadKind::FirstUserMessage => block.push_str("Summary:\n"),
        PayloadKind::Identifier => {
            block.push_str("Summary: (none recorded; only the session identifier is known)\n")
        }
    }
    block.push_str(&payload.text);
    block
}

/// The launchers a session can be continued in: every visible launcher
/// except the row's own agent, each flagged with whether a session reader
/// exists for it (a reader-less target stays selectable, with a hint).
pub(crate) fn handoff_targets(
    config: &PaneFlowConfig,
    source: SessionAgent,
) -> Vec<(TerminalAgent, bool)> {
    let own = source.terminal_agent();
    TerminalAgent::visible(config)
        .into_iter()
        .filter(|agent| *agent != own)
        .map(|agent| (agent, agent.session_agent().is_some()))
        .collect()
}

/// True when a "summary" is really a path, a file name, a bare id, or a
/// `<word> session <token>` line - something a reader emitted in place of a
/// title. A real title with spaces is never identifier-shaped, even when it
/// contains a slash; multi-line text never is.
pub(crate) fn is_identifier_shaped(s: &str) -> bool {
    let s = s.trim();
    if s.is_empty() || s.contains('\n') {
        return false;
    }
    let no_whitespace = !s.chars().any(char::is_whitespace);
    if no_whitespace && s.contains(['/', '\\']) {
        return true;
    }
    if no_whitespace && has_file_extension(s) {
        return true;
    }
    let tokens: Vec<&str> = s.split_whitespace().collect();
    if tokens.len() == 3 && tokens[1] == "session" {
        return true;
    }
    if no_whitespace && s.len() >= 16 && s.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
        return true;
    }
    // ULID: 26 Crockford base32 characters.
    no_whitespace
        && s.len() == 26
        && s.chars()
            .all(|c| c.is_ascii_digit() || c.is_ascii_uppercase())
        && s.chars().any(|c| c.is_ascii_digit())
}

/// `<agent> session <id> from <cwd>`: the identifier fallback.
fn identifier_line(meta: &SessionMeta) -> String {
    format!(
        "{} session {} from {}",
        meta.agent.label(),
        display_id(meta),
        meta.cwd
    )
}

/// The session id the block may show: the id itself when it passes the
/// resume allow-list, [`WITHHELD_ID`] otherwise.
fn display_id(meta: &SessionMeta) -> Cow<'_, str> {
    if crate::agent_sessions::is_valid_session_id(&meta.session_id) {
        Cow::Borrowed(meta.session_id.as_str())
    } else {
        Cow::Borrowed(WITHHELD_ID)
    }
}

/// Ends in `.<one to five ASCII alphanumerics>` with something before the
/// dot.
fn has_file_extension(s: &str) -> bool {
    let Some((stem, ext)) = s.rsplit_once('.') else {
        return false;
    };
    !stem.is_empty()
        && (1..=5).contains(&ext.len())
        && ext.chars().all(|c| c.is_ascii_alphanumeric())
}

/// Fold line endings to `\n`, strip trailing whitespace, and cut at
/// [`HANDOFF_SUMMARY_CAP`] on a char boundary with the marker appended.
fn cap_summary(summary: &str) -> String {
    let folded = summary.replace("\r\n", "\n").replace('\r', "\n");
    let trimmed = folded.trim_end();
    if trimmed.len() <= HANDOFF_SUMMARY_CAP {
        return trimmed.to_string();
    }
    let mut cut = HANDOFF_SUMMARY_CAP;
    while cut > 0 && !trimmed.is_char_boundary(cut) {
        cut -= 1;
    }
    let mut out = trimmed[..cut].trim_end().to_string();
    out.push_str(TRUNCATION_MARKER);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_launcher::TerminalAgent;
    use crate::agent_sessions::{SessionAgent, SessionMeta};
    use paneflow_config::schema::PaneFlowConfig;

    const ID: &str = "019dc9ea-38d7-7372-9cc4-253ce944d41b";

    fn meta(agent: SessionAgent, summary: Option<&str>, branch: &str) -> SessionMeta {
        SessionMeta {
            agent,
            session_id: ID.to_string(),
            timestamp: "2026-09-03T00:00:00Z".to_string(),
            cwd: "/Users/x/proj".to_string(),
            git_branch: branch.to_string(),
            summary: summary.map(str::to_string),
            model: None,
            usage: None,
        }
    }

    #[test]
    fn handoff_prompt_summary_kind_matches_the_locked_shape() {
        let m = meta(
            SessionAgent::Claude,
            Some("Fix the worktree teardown race"),
            "",
        );
        assert_eq!(
            handoff_prompt(&m, TerminalAgent::Codex),
            format!(
                "Continue this work from a prior Claude Code session.\n\
                 Session: {ID}\n\
                 Cwd: /Users/x/proj\n\
                 Summary:\n\
                 Fix the worktree teardown race"
            )
        );
    }

    #[test]
    fn handoff_prompt_adds_the_branch_line_only_when_known() {
        let m = meta(SessionAgent::Codex, Some("Ship the release"), "feat/x");
        assert_eq!(
            handoff_prompt(&m, TerminalAgent::ClaudeCode),
            format!(
                "Continue this work from a prior Codex session.\n\
                 Session: {ID}\n\
                 Cwd: /Users/x/proj\n\
                 Branch: feat/x\n\
                 Summary:\n\
                 Ship the release"
            )
        );
        let unbranched = meta(SessionAgent::Codex, Some("Ship the release"), "");
        assert!(!handoff_prompt(&unbranched, TerminalAgent::ClaudeCode).contains("Branch:"));
    }

    #[test]
    fn handoff_prompt_identifier_kind_marks_the_missing_summary() {
        let m = meta(SessionAgent::Gemini, None, "main");
        assert_eq!(
            handoff_prompt(&m, TerminalAgent::Grok),
            format!(
                "Continue this work from a prior Gemini session.\n\
                 Session: {ID}\n\
                 Cwd: /Users/x/proj\n\
                 Branch: main\n\
                 Summary: (none recorded; only the session identifier is known)\n\
                 Gemini session {ID} from /Users/x/proj"
            )
        );
    }

    #[test]
    fn handoff_prompt_withholds_an_id_that_fails_the_resume_allow_list() {
        let mut m = meta(SessionAgent::Claude, Some("Real title"), "");
        m.session_id = "--dangerously-skip-permissions".to_string();
        let block = handoff_prompt(&m, TerminalAgent::Codex);
        assert!(block.contains("Session: (id withheld)\n"), "{block}");
        assert!(!block.contains("dangerously"), "{block}");
    }

    #[test]
    fn handoff_prompt_is_the_same_for_every_target() {
        let m = meta(SessionAgent::Claude, Some("Same for all"), "main");
        let first = handoff_prompt(&m, TerminalAgent::Codex);
        for target in TerminalAgent::ALL {
            assert_eq!(handoff_prompt(&m, target), first, "{target:?}");
        }
    }

    #[test]
    fn handoff_prompt_never_submits_and_never_carries_a_carriage_return() {
        let m = meta(SessionAgent::Claude, Some("line one\r\nline two\n"), "");
        let block = handoff_prompt(&m, TerminalAgent::Codex);
        assert!(!block.contains('\r'), "{block:?}");
        assert!(!block.ends_with('\n'), "{block:?}");
        assert!(block.ends_with("line one\nline two"), "{block:?}");
    }

    #[test]
    fn handoff_prompt_caps_the_summary_on_a_char_boundary() {
        // 10 KiB of ASCII: the summary section is exactly the cap plus the
        // marker. (`z`, not a hex digit: a run of `a` is a bare hex token
        // and is identifier-shaped.)
        let long = "z".repeat(10 * 1024);
        let m = meta(SessionAgent::Claude, Some(&long), "");
        let block = handoff_prompt(&m, TerminalAgent::Codex);
        let summary = block.split("Summary:\n").nth(1).expect("summary section");
        assert!(
            summary.ends_with(" […]"),
            "{}",
            &summary[summary.len() - 16..]
        );
        let body = summary.strip_suffix(" […]").expect("marker");
        assert_eq!(body.len(), HANDOFF_SUMMARY_CAP);
        assert!(body.bytes().all(|b| b == b'z'));

        // Multi-byte text: the cut never splits a char and never exceeds the
        // cap.
        let wide = "é".repeat(6 * 1024); // 12 KiB, two bytes per char
        let m = meta(SessionAgent::Claude, Some(&wide), "");
        let block = handoff_prompt(&m, TerminalAgent::Codex);
        let summary = block.split("Summary:\n").nth(1).expect("summary section");
        let body = summary.strip_suffix(" […]").expect("marker");
        assert!(body.len() <= HANDOFF_SUMMARY_CAP);
        assert!(body.len() >= HANDOFF_SUMMARY_CAP - 1);
        assert!(body.chars().all(|c| c == 'é'));

        // Under the cap nothing is touched.
        let short = meta(SessionAgent::Claude, Some("short"), "");
        assert!(!handoff_prompt(&short, TerminalAgent::Codex).contains("[…]"));
    }

    #[test]
    fn is_identifier_shaped_table() {
        for shaped in [
            "/Users/x/proj",
            "~/.gemini/tmp/abc",
            "C:\\dev\\proj",
            "rollout-2026.jsonl",
            "notes.md",
            "gemini session abc",
            "019dc9ea-38d7-7372-9cc4-253ce944d41b",
            "01ARZ3NDEKTSV4RRFFQ69G5FAV",
            "deadbeefdeadbeef",
            "  /trimmed/path  ",
        ] {
            assert!(
                is_identifier_shaped(shaped),
                "{shaped:?} should be identifier-shaped"
            );
        }
        for title in [
            "Fix the worktree teardown race",
            "Add tests for src/app/foo.rs",
            "Write docs for the MCP bridge",
            "Investigate the 5 hour cap",
            "Refactor the sessions sidebar filter",
            "two\nlines",
            "/path with space",
            "",
        ] {
            assert!(!is_identifier_shaped(title), "{title:?} is a real title");
        }
    }

    #[test]
    fn payload_for_with_no_summary_is_identifier_and_non_empty() {
        let m = meta(SessionAgent::Codex, None, "");
        let payload = payload_for(&m);
        assert_eq!(payload.kind, PayloadKind::Identifier);
        assert!(!payload.text.is_empty());
        assert_eq!(
            payload.text,
            format!("Codex session {ID} from /Users/x/proj")
        );

        // An empty or identifier-shaped summary is treated as absent.
        for absent in ["", "   ", "/Users/x/proj/.claude/abc.jsonl"] {
            let m = meta(SessionAgent::Codex, Some(absent), "");
            assert_eq!(payload_for(&m).kind, PayloadKind::Identifier, "{absent:?}");
        }
        let m = meta(SessionAgent::Codex, Some("A real summary"), "");
        assert_eq!(payload_for(&m).kind, PayloadKind::Summary);
    }

    #[test]
    fn payload_for_never_returns_first_user_message_today() {
        // Reserved for a reader that grows a separate first-user-message
        // field; every reader folds it into `summary` for now.
        for summary in [None, Some(""), Some("title"), Some("a/b.rs")] {
            for agent in SessionAgent::ALL {
                let m = meta(agent, summary, "");
                assert_ne!(payload_for(&m).kind, PayloadKind::FirstUserMessage);
            }
        }
    }

    #[test]
    fn copy_text_equals_the_blocks_payload() {
        for summary in [None, Some("Fix the race"), Some("abc.jsonl")] {
            let m = meta(SessionAgent::Claude, summary, "");
            let block = handoff_prompt(&m, TerminalAgent::Codex);
            let payload = copy_text(&m);
            assert!(!payload.is_empty());
            assert!(block.ends_with(&payload), "{block:?} vs {payload:?}");
            assert_eq!(payload, payload_for(&m).text);
        }
    }

    #[test]
    fn handoff_prompt_never_leaks_a_session_file_path() {
        let m = meta(
            SessionAgent::Claude,
            Some("/Users/x/.claude/projects/-Users-x-proj/019dc9ea.jsonl"),
            "",
        );
        let block = handoff_prompt(&m, TerminalAgent::Codex);
        assert!(!block.contains(".jsonl"), "{block}");
        assert!(!block.contains(".claude/projects"), "{block}");
        assert!(!copy_text(&m).contains(".jsonl"));
    }

    fn all_visible() -> PaneFlowConfig {
        PaneFlowConfig {
            claude_code_button_visible: Some(true),
            codex_button_visible: Some(true),
            opencode_button_visible: Some(true),
            pi_button_visible: Some(true),
            hermes_agent_button_visible: Some(true),
            grok_button_visible: Some(true),
            amp_button_visible: Some(true),
            cursor_button_visible: Some(true),
            gemini_button_visible: Some(true),
            kiro_button_visible: Some(true),
            antigravity_button_visible: Some(true),
            copilot_button_visible: Some(true),
            codebuddy_button_visible: Some(true),
            factory_button_visible: Some(true),
            qoder_button_visible: Some(true),
            openclaw_button_visible: Some(true),
            ..Default::default()
        }
    }

    #[test]
    fn handoff_targets_excludes_the_source_and_flags_reader_less_launchers() {
        let targets = handoff_targets(&all_visible(), SessionAgent::Claude);
        assert_eq!(targets.len(), TerminalAgent::ALL.len() - 1);
        assert!(
            !targets
                .iter()
                .any(|(agent, _)| *agent == TerminalAgent::ClaudeCode)
        );
        let reader_less: Vec<TerminalAgent> = targets
            .iter()
            .filter(|(_, has_reader)| !has_reader)
            .map(|(agent, _)| *agent)
            .collect();
        assert_eq!(
            reader_less,
            vec![
                TerminalAgent::Amp,
                TerminalAgent::Antigravity,
                TerminalAgent::Copilot,
                TerminalAgent::CodeBuddy,
                TerminalAgent::Factory,
                TerminalAgent::Qoder,
                TerminalAgent::Openclaw,
            ]
        );
        assert!(
            targets
                .iter()
                .any(|(agent, has_reader)| *agent == TerminalAgent::Codex && *has_reader)
        );
    }

    #[test]
    fn handoff_targets_respects_visibility() {
        let mut config = all_visible();
        config.codex_button_visible = Some(false);
        config.grok_button_visible = Some(false);
        let targets = handoff_targets(&config, SessionAgent::Claude);
        assert!(!targets.iter().any(|(a, _)| *a == TerminalAgent::Codex));
        assert!(!targets.iter().any(|(a, _)| *a == TerminalAgent::Grok));
        assert!(targets.iter().any(|(a, _)| *a == TerminalAgent::OpenCode));

        // Only the source visible: nothing to continue in.
        let mut lone = PaneFlowConfig::default();
        for agent in TerminalAgent::ALL {
            set_visible(&mut lone, agent, false);
        }
        lone.claude_code_button_visible = Some(true);
        assert!(handoff_targets(&lone, SessionAgent::Claude).is_empty());
    }

    fn set_visible(config: &mut PaneFlowConfig, agent: TerminalAgent, visible: bool) {
        let slot = match agent {
            TerminalAgent::ClaudeCode => &mut config.claude_code_button_visible,
            TerminalAgent::Codex => &mut config.codex_button_visible,
            TerminalAgent::OpenCode => &mut config.opencode_button_visible,
            TerminalAgent::Pi => &mut config.pi_button_visible,
            TerminalAgent::Hermes => &mut config.hermes_agent_button_visible,
            TerminalAgent::Grok => &mut config.grok_button_visible,
            TerminalAgent::Amp => &mut config.amp_button_visible,
            TerminalAgent::Cursor => &mut config.cursor_button_visible,
            TerminalAgent::Gemini => &mut config.gemini_button_visible,
            TerminalAgent::Kiro => &mut config.kiro_button_visible,
            TerminalAgent::Antigravity => &mut config.antigravity_button_visible,
            TerminalAgent::Copilot => &mut config.copilot_button_visible,
            TerminalAgent::CodeBuddy => &mut config.codebuddy_button_visible,
            TerminalAgent::Factory => &mut config.factory_button_visible,
            TerminalAgent::Qoder => &mut config.qoder_button_visible,
            TerminalAgent::Openclaw => &mut config.openclaw_button_visible,
        };
        *slot = Some(visible);
    }
}
