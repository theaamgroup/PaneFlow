use super::*;

impl DiffView {
    /// Tooltip lines for the Review pane header: a count line, then one
    /// `Agent · <when>` row per attributed session. Empty when nothing is
    /// attributed.
    pub fn attribution_lines(&self) -> Vec<SharedString> {
        attribution_lines_for(&self.column.attribution)
    }
}

fn attribution_lines_for(sessions: &[SessionMeta]) -> Vec<SharedString> {
    if sessions.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<SharedString> = Vec::with_capacity(sessions.len() + 1);
    lines.push(
        format!(
            "Attributed to {} session{}",
            sessions.len(),
            if sessions.len() == 1 { "" } else { "s" }
        )
        .into(),
    );
    for s in sessions {
        let when = crate::agent_sessions::format_relative_time(&s.timestamp);
        lines.push(format!("{} · {when}", s.agent.label()).into());
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_sessions::SessionAgent;

    fn session(agent: SessionAgent, ts: &str) -> SessionMeta {
        SessionMeta {
            agent,
            session_id: "s".into(),
            timestamp: ts.into(),
            cwd: "/repo".into(),
            git_branch: "main".into(),
            summary: None,
        }
    }

    #[test]
    fn attribution_lines_are_a_count_line_plus_agent_and_when_rows() {
        assert!(attribution_lines_for(&[]).is_empty());

        let sessions = [
            session(SessionAgent::Claude, "2026-06-01T10:00:00Z"),
            session(SessionAgent::Codex, "2026-05-01T10:00:00Z"),
        ];
        let lines: Vec<String> = attribution_lines_for(&sessions)
            .iter()
            .map(|l| l.to_string())
            .collect();
        let when = |ts: &str| crate::agent_sessions::format_relative_time(ts);
        assert_eq!(
            lines,
            vec![
                "Attributed to 2 sessions".to_string(),
                format!(
                    "{} · {}",
                    SessionAgent::Claude.label(),
                    when("2026-06-01T10:00:00Z")
                ),
                format!(
                    "{} · {}",
                    SessionAgent::Codex.label(),
                    when("2026-05-01T10:00:00Z")
                ),
            ]
        );
        for line in &lines {
            assert!(!line.contains('$'), "no cost figure: {line}");
            assert!(!line.contains("tokens:"), "no token total: {line}");
            assert!(!line.contains("prices v"), "no price-table footer: {line}");
        }

        let single = attribution_lines_for(&sessions[..1]);
        assert_eq!(single.len(), 2);
        assert_eq!(single[0].as_ref(), "Attributed to 1 session");
    }
}
