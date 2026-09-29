//! Session discovery for agent CLIs that expose a documented list command.
//!
//! These readers intentionally stay conservative: they never parse private
//! storage, they run the vendor CLI in the scanned cwd when the command is
//! project-scoped, and they drop output that cannot be reduced to a safe
//! session id. Commands are scoped to the scanned cwd.

use std::io;
use std::path::Path;
use std::process::Command;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use regex::Regex;

use crate::agent_sessions::{SessionAgent, SessionMeta, clean_session_label};

/// Per-invocation ceiling for one vendor list command. A caller-owned budget
/// (`budget_until`) can shorten it but never extend it (issue #401).
pub(crate) const COMMAND_DEADLINE: Duration = Duration::from_secs(15);
const COMMAND_STDOUT_CAP: u64 = 4 * 1024 * 1024;
const STDERR_LOG_CAP: usize = 200;

struct CommandSessionConfig {
    agent: SessionAgent,
    program: &'static str,
    args: &'static [&'static str],
    allow_numeric_ids: bool,
}

pub(crate) fn read_gemini_sessions_for_cwd(
    cwd: &str,
    budget_until: Instant,
) -> (Vec<SessionMeta>, usize) {
    read_command_sessions(
        CommandSessionConfig {
            agent: SessionAgent::Gemini,
            program: list_program("gemini"),
            args: &["--list-sessions"],
            allow_numeric_ids: true,
        },
        cwd,
        budget_until,
    )
}

pub(crate) fn read_cursor_sessions_for_cwd(
    cwd: &str,
    budget_until: Instant,
) -> (Vec<SessionMeta>, usize) {
    read_command_sessions(
        CommandSessionConfig {
            agent: SessionAgent::Cursor,
            program: list_program("cursor-agent"),
            args: &["ls"],
            allow_numeric_ids: false,
        },
        cwd,
        budget_until,
    )
}

pub(crate) fn read_grok_sessions_for_cwd(
    cwd: &str,
    budget_until: Instant,
) -> (Vec<SessionMeta>, usize) {
    read_command_sessions(
        CommandSessionConfig {
            agent: SessionAgent::Grok,
            program: list_program("grok"),
            args: &["sessions", "list", "--limit", "100"],
            allow_numeric_ids: false,
        },
        cwd,
        budget_until,
    )
}

/// Run one agent's list command against a caller-owned wall-clock budget
/// (issue #401). The subprocess deadline is whatever is left of `budget_until`,
/// capped at [`COMMAND_DEADLINE`], so N enabled command agents on one blocking
/// worker cannot stack N independent deadlines; an exhausted budget spawns
/// nothing at all.
fn read_command_sessions(
    config: CommandSessionConfig,
    cwd: &str,
    budget_until: Instant,
) -> (Vec<SessionMeta>, usize) {
    if !Path::new(cwd).is_dir() {
        return (Vec::new(), 0);
    }
    let Some(stdout) = run_list_command(&config, cwd, budget_until) else {
        return (Vec::new(), 0);
    };
    parse_command_sessions(&stdout, config.agent, cwd, config.allow_numeric_ids)
}

fn run_list_command(
    config: &CommandSessionConfig,
    cwd: &str,
    budget_until: Instant,
) -> Option<Vec<u8>> {
    let deadline = budget_until
        .saturating_duration_since(Instant::now())
        .min(COMMAND_DEADLINE);
    if deadline.is_zero() {
        log::warn!(
            "session list budget exhausted before {} ran; {:?} sessions will be empty",
            config.program,
            config.agent
        );
        return None;
    }
    let mut cmd = Command::new(config.program);
    cmd.args(config.args);
    cmd.current_dir(cwd);

    let output = match paneflow_process::run_with_timeout(cmd, deadline, COMMAND_STDOUT_CAP) {
        Ok(out) => out,
        Err(paneflow_process::ProcError::Spawn(err)) if err.kind() == io::ErrorKind::NotFound => {
            log::info!(
                "{} binary not found on PATH; {:?} sessions will be empty",
                config.program,
                config.agent
            );
            return None;
        }
        Err(paneflow_process::ProcError::Timeout) => {
            log::warn!(
                "{} session list timed out; {:?} sessions will be empty",
                config.program,
                config.agent
            );
            return None;
        }
        Err(err) => {
            log::warn!(
                "failed to spawn {} for {:?} sessions: {err}",
                config.program,
                config.agent
            );
            return None;
        }
    };

    if !output.status.success() {
        let stderr = sanitized_stderr(&output.stderr);
        log::warn!(
            "{} session list exited with {}: {}",
            config.program,
            output.status,
            stderr
        );
        return None;
    }

    Some(output.stdout)
}

fn parse_command_sessions(
    stdout: &[u8],
    agent: SessionAgent,
    cwd: &str,
    allow_numeric_ids: bool,
) -> (Vec<SessionMeta>, usize) {
    let text = String::from_utf8_lossy(stdout);
    let sessions = text
        .lines()
        .filter_map(|line| parse_session_line(line, agent, cwd, allow_numeric_ids));
    crate::agent_sessions::collect_recent_sessions(
        sessions,
        crate::agent_sessions::SIDEBAR_SESSION_RETAINED_PER_SOURCE,
    )
}

fn parse_session_line(
    line: &str,
    agent: SessionAgent,
    cwd: &str,
    allow_numeric_ids: bool,
) -> Option<SessionMeta> {
    let line = line.trim();
    if line.is_empty() || is_header_or_separator(line) {
        return None;
    }

    let (session_id, summary, timestamp) = if agent == SessionAgent::Gemini {
        let (session_id, summary, index) = parse_gemini_list_line(line)?;
        // Gemini prints oldest-first with a relative time only. The list
        // index is the order key, so the collector's timestamp sort keeps
        // the highest index (the newest row).
        (session_id, summary, gemini_index_timestamp(&index))
    } else {
        let session_id = extract_session_id(line, allow_numeric_ids)?;
        let summary = line_summary(line, &session_id);
        let timestamp = extract_iso8601(line).unwrap_or_default();
        (session_id, summary, timestamp)
    };

    Some(SessionMeta {
        agent,
        session_id,
        timestamp,
        cwd: cwd.to_string(),
        git_branch: String::new(),
        summary,
    })
}

/// Gemini CLI `packages/cli/src/utils/sessions.ts` prints
/// `{index}. {title} ({time}[, current]) [{session.id}]`.
///
/// Resume accepts `{index|uuid|latest}`. Prefer the bracket id so a relative
/// time like `(2 days ago)` is never treated as session id `2`.
fn parse_gemini_list_line(line: &str) -> Option<(String, Option<String>, String)> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"^\s*(\d+)\.\s+(.*)\s+\[([^\]]+)\]\s*$")
            .expect("gemini list-sessions line regex")
    });
    let caps = re.captures(line)?;
    let index = caps.get(1)?.as_str();
    let middle = caps.get(2)?.as_str();
    let bracket_id = caps.get(3)?.as_str().trim();

    let session_id = if crate::agent_sessions::is_valid_session_id(bracket_id) {
        bracket_id.to_string()
    } else if crate::agent_sessions::is_valid_session_id(index) {
        index.to_string()
    } else {
        return None;
    };

    Some((
        session_id,
        gemini_summary_from_middle(middle),
        index.to_string(),
    ))
}

/// Lexical order key for a Gemini list index. A higher index is newer.
///
/// The length prefix keeps numeric order (`9` before `10`). `" T"` sorts
/// before any real ISO-8601 year, so mixed-agent attribution still prefers
/// dated rows, and [`crate::agent_sessions::format_relative_time`] falls
/// back to one space instead of a fabricated age.
fn gemini_index_timestamp(index: &str) -> String {
    format!(" T{:010}{index}", index.len())
}

fn gemini_summary_from_middle(middle: &str) -> Option<String> {
    let trimmed = middle.trim();
    let title = match trimmed.rsplit_once(" (") {
        Some((head, tail)) if tail.ends_with(')') => head.trim(),
        _ => trimmed,
    };
    if title.is_empty() {
        None
    } else {
        clean_session_label(title, 120)
    }
}

fn is_header_or_separator(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    let headerish = lower.contains("session")
        && lower.contains("id")
        && (lower.contains("title") || lower.contains("summary"))
        && extract_session_id(line, false).is_none();
    headerish
        || line
            .chars()
            .all(|c| matches!(c, '-' | '=' | '+' | '|' | ' '))
}

fn extract_session_id(line: &str, allow_numeric_ids: bool) -> Option<String> {
    let tokens: Vec<String> = line
        .split_whitespace()
        .map(clean_token)
        .filter(|token| !token.is_empty())
        .collect();

    extract_labeled_session_id(&tokens, allow_numeric_ids).or_else(|| {
        tokens
            .iter()
            .find(|token| is_candidate_session_id(token, allow_numeric_ids, false))
            .cloned()
    })
}

fn clean_token(token: &str) -> String {
    token
        .trim_matches(|c: char| {
            matches!(
                c,
                '"' | '\'' | '`' | ',' | ';' | '(' | ')' | '[' | ']' | '{' | '}' | '<' | '>' | ':'
            )
        })
        .to_string()
}

fn extract_labeled_session_id(tokens: &[String], allow_numeric_ids: bool) -> Option<String> {
    for (idx, token) in tokens.iter().enumerate() {
        if let Some((label, value)) = split_labeled_token(token) {
            let label = label.to_ascii_lowercase();
            if is_id_label(&label) && is_candidate_session_id(value, allow_numeric_ids, true) {
                return Some(value.to_string());
            }
        }

        let lower = token.to_ascii_lowercase();
        let candidate_index = if lower == "session" {
            tokens
                .get(idx + 1)
                .is_some_and(|next| next.eq_ignore_ascii_case("id"))
                .then_some(idx + 2)
        } else if is_id_label(&lower) {
            Some(idx + 1)
        } else {
            None
        };
        if let Some(candidate_index) = candidate_index
            && let Some(candidate) = tokens.get(candidate_index)
            && is_candidate_session_id(candidate, allow_numeric_ids, true)
        {
            return Some(candidate.clone());
        }
    }
    None
}

fn split_labeled_token(token: &str) -> Option<(&str, &str)> {
    token
        .split_once('=')
        .or_else(|| token.split_once(':'))
        .filter(|(_, value)| !value.is_empty())
}

fn is_id_label(label: &str) -> bool {
    matches!(label, "id" | "session_id" | "sessionid")
}

fn is_candidate_session_id(token: &str, allow_numeric_ids: bool, explicit_id_label: bool) -> bool {
    if token.is_empty() || !crate::agent_sessions::is_valid_session_id(token) {
        return false;
    }
    if token.chars().all(|c| c.is_ascii_digit()) {
        return allow_numeric_ids;
    }
    if looks_like_iso_date(token) {
        return false;
    }
    if explicit_id_label && token.len() >= 3 {
        let lower = token.to_ascii_lowercase();
        if matches!(lower.as_str(), "title" | "summary" | "created" | "updated") {
            return false;
        }
        return true;
    }
    token.starts_with("ses_")
        || token.starts_with("sess_")
        || token.starts_with("T-")
        || (token.len() >= 8 && (token.contains('-') || token.contains('_')))
}

fn looks_like_iso_date(token: &str) -> bool {
    let bytes = token.as_bytes();
    token.len() >= 10
        && bytes.get(4) == Some(&b'-')
        && bytes.get(7) == Some(&b'-')
        && bytes
            .iter()
            .take(10)
            .enumerate()
            .all(|(i, b)| i == 4 || i == 7 || b.is_ascii_digit())
}

fn extract_iso8601(line: &str) -> Option<String> {
    line.split_whitespace()
        .map(|token| token.trim_matches(|c: char| matches!(c, ',' | ';' | ')' | '(' | '[' | ']')))
        .find(|token| looks_like_iso_date(token))
        .map(|token| {
            if token.contains('T') {
                token.trim_end_matches('Z').to_string() + "Z"
            } else {
                format!("{}T00:00:00Z", &token[..10])
            }
        })
}

fn line_summary(line: &str, session_id: &str) -> Option<String> {
    let without_id = line.replace(session_id, " ");
    let mut summary = without_id.trim();
    summary = trim_leading_id_label(summary);
    summary = trim_leading_table_metadata(summary);
    summary = summary.trim_start_matches(|c: char| {
        c.is_ascii_digit() || matches!(c, '.' | ')' | '#' | '[' | ']' | '-' | '|' | ' ')
    });
    summary = trim_leading_id_label(summary);
    summary = summary.trim_matches(|c: char| matches!(c, '|' | '-' | ' '));
    if summary.is_empty()
        || summary.eq_ignore_ascii_case("session id")
        || summary.eq_ignore_ascii_case("(no summary)")
    {
        None
    } else {
        clean_session_label(summary, 120)
    }
}

fn trim_leading_id_label(summary: &str) -> &str {
    let trimmed = summary.trim_start();
    let lower = trimmed.to_ascii_lowercase();
    for prefix in [
        "session id:",
        "session_id:",
        "session_id=",
        "sessionid:",
        "sessionid=",
        "id:",
        "id=",
    ] {
        if lower.starts_with(prefix) {
            return trimmed[prefix.len()..].trim_start();
        }
    }
    trimmed
}

fn trim_leading_table_metadata(mut summary: &str) -> &str {
    loop {
        let trimmed = summary.trim_start();
        let Some((first_token, rest)) = trimmed.split_once(char::is_whitespace) else {
            return trimmed;
        };
        if looks_like_iso_date(first_token) {
            summary = rest;
            continue;
        }
        if matches!(
            first_token,
            "local" | "remote" | "archived" | "running" | "done"
        ) {
            summary = rest;
            continue;
        }
        return trimmed;
    }
}

// Test seam so a budget test can point every command-backed reader at one
// hanging program. Absent on the production path, which keeps the vendor name.
#[cfg(test)]
thread_local! {
    static LIST_PROGRAM_OVERRIDE: std::cell::Cell<Option<&'static str>> =
        const { std::cell::Cell::new(None) };
}

#[cfg(test)]
pub(crate) fn set_list_program_override(program: Option<&'static str>) {
    LIST_PROGRAM_OVERRIDE.with(|slot| slot.set(program));
}

fn list_program(program: &'static str) -> &'static str {
    #[cfg(test)]
    {
        if let Some(program) = LIST_PROGRAM_OVERRIDE.with(|slot| slot.get()) {
            return program;
        }
    }
    program
}

fn sanitized_stderr(stderr: &[u8]) -> String {
    String::from_utf8_lossy(stderr)
        .chars()
        .take(STDERR_LOG_CAP)
        .map(|c| if c.is_control() && c != '\n' { '?' } else { c })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #401: `attribution_for_column` runs every enabled command agent on one
    /// blocking worker. Each list command used to take a fresh 15 s
    /// COMMAND_DEADLINE, so N hung CLIs pinned that worker for N x 15 s. With a
    /// shared budget the first hung command is cut at the budget and the next
    /// command agent spawns nothing at all.
    #[test]
    fn second_hanging_command_agent_is_not_waited_out_once_the_shared_budget_is_spent() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("hang-list");
        std::fs::write(&script, "#!/bin/sh\nsleep 60\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let program: &'static str =
            Box::leak(script.to_string_lossy().into_owned().into_boxed_str());
        let cwd = dir.path().to_string_lossy().into_owned();
        let config = |agent| CommandSessionConfig {
            agent,
            program,
            args: &[],
            allow_numeric_ids: false,
        };

        let budget = Duration::from_millis(500);
        let budget_until = Instant::now() + budget;
        let started = Instant::now();
        let first = read_command_sessions(config(SessionAgent::Gemini), &cwd, budget_until);
        let first_elapsed = started.elapsed();
        assert!(first.0.is_empty(), "a hung list command yields no rows");
        assert!(
            first_elapsed >= budget && first_elapsed < COMMAND_DEADLINE,
            "first hung command must stop at the shared budget, took {first_elapsed:?}"
        );

        let second_started = Instant::now();
        let second = read_command_sessions(config(SessionAgent::Cursor), &cwd, budget_until);
        let second_elapsed = second_started.elapsed();
        assert!(second.0.is_empty());
        assert!(
            second_elapsed < Duration::from_secs(1),
            "second command agent must not be waited out once the budget is spent, took {second_elapsed:?}"
        );
        assert!(
            started.elapsed() < COMMAND_DEADLINE,
            "two hung list CLIs must not stack, took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn parse_command_sessions_extracts_uuid_from_cursorish_line() {
        let out = b"550e8400-e29b-41d4-a716-446655440000 2026-06-29T09:10:11Z Refactor auth flow\n";
        let (sessions, omitted) = parse_command_sessions(out, SessionAgent::Cursor, "/repo", false);
        assert_eq!(omitted, 0);
        assert_eq!(sessions.len(), 1);
        assert_eq!(
            sessions[0].session_id,
            "550e8400-e29b-41d4-a716-446655440000"
        );
        assert_eq!(sessions[0].agent, SessionAgent::Cursor);
    }

    #[test]
    fn parse_gemini_published_list_sessions_prefers_bracket_id() {
        // google-gemini/gemini-cli docs/cli/session-management.md:
        //   1. Fix bug in auth (2 days ago) [a1b2c3d4]
        let out = b"\
Available sessions for this project (3):\n\
\n\
1. Fix bug in auth (2 days ago) [a1b2c3d4]\n\
2. Refactor database schema (5 hours ago) [e5f67890]\n\
3. Update documentation (Just now) [abcd1234]\n";
        let (sessions, omitted) = parse_command_sessions(out, SessionAgent::Gemini, "/repo", true);
        assert_eq!(omitted, 0);
        assert_eq!(sessions.len(), 3);
        // Highest list index is newest, so the sidebar order is the reverse
        // of Gemini's oldest-first printout.
        assert_eq!(
            sessions
                .iter()
                .map(|s| s.session_id.as_str())
                .collect::<Vec<_>>(),
            ["abcd1234", "e5f67890", "a1b2c3d4"]
        );
        assert!(sessions[0].timestamp > sessions[1].timestamp);
        assert!(sessions[1].timestamp > sessions[2].timestamp);
        assert!(sessions.iter().all(|s| s.session_id != "2"));
        assert_eq!(sessions[0].summary.as_deref(), Some("Update documentation"));
        assert_eq!(
            sessions[1].summary.as_deref(),
            Some("Refactor database schema")
        );
        assert_eq!(sessions[2].summary.as_deref(), Some("Fix bug in auth"));
    }

    #[test]
    fn gemini_sessions_keep_the_newest_under_the_cap() {
        let cap = crate::agent_sessions::SIDEBAR_SESSION_RETAINED_PER_SOURCE;
        let mut out = String::new();
        for i in 1..=cap + 1 {
            out.push_str(&format!("{i}. Session {i} (1 day ago) [id{i:04}]\n"));
        }
        let (sessions, omitted) =
            parse_command_sessions(out.as_bytes(), SessionAgent::Gemini, "/repo", true);
        let expected: Vec<String> = (2..=cap + 1).rev().map(|i| format!("id{i:04}")).collect();
        assert_eq!(omitted, 1);
        assert_eq!(sessions.len(), cap);
        assert_eq!(
            sessions
                .iter()
                .map(|s| s.session_id.as_str())
                .collect::<Vec<_>>(),
            expected.iter().map(String::as_str).collect::<Vec<_>>()
        );
        assert_eq!(sessions[0].session_id, format!("id{:04}", cap + 1));
        assert!(sessions.iter().all(|s| s.session_id != "id0001"));
    }

    #[test]
    fn parse_gemini_relative_time_digit_is_not_session_id() {
        let line = "1. Fix bug in auth (2 days ago) [a1b2c3d4]";
        assert_eq!(
            extract_session_id(line, true).as_deref(),
            Some("2"),
            "generic tokenizer still misreads '(2 days ago)' as id 2"
        );
        let parsed = parse_gemini_list_line(line).expect("published Gemini line");
        assert_eq!(parsed.0, "a1b2c3d4");
        assert_ne!(parsed.0, "2");
    }

    #[test]
    fn parse_gemini_uuid_line_ignores_digits_in_title() {
        let out =
            b"  1. List 3 functions defined (Just now, current) [875c2ac1-4eec-42dd-a7b0-cccc97bcbd53]\n";
        let (sessions, omitted) = parse_command_sessions(out, SessionAgent::Gemini, "/repo", true);
        assert_eq!(omitted, 0);
        assert_eq!(sessions.len(), 1);
        assert_eq!(
            sessions[0].session_id,
            "875c2ac1-4eec-42dd-a7b0-cccc97bcbd53"
        );
        assert_ne!(sessions[0].session_id, "3");
        assert_eq!(
            sessions[0].summary.as_deref(),
            Some("List 3 functions defined")
        );
    }

    #[test]
    fn parse_gemini_falls_back_to_list_index_when_bracket_id_invalid() {
        let out = b"4. Something (Just now) [not a valid id!]\n";
        let (sessions, omitted) = parse_command_sessions(out, SessionAgent::Gemini, "/repo", true);
        assert_eq!(omitted, 0);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, "4");
        assert_eq!(sessions[0].summary.as_deref(), Some("Something"));
    }

    #[test]
    fn parse_cursor_does_not_use_gemini_list_parser() {
        let out = b"1. Fix bug in auth (2 days ago) [a1b2c3d4]\n";
        let (sessions, _) = parse_command_sessions(out, SessionAgent::Cursor, "/repo", false);
        assert_eq!(
            sessions.len(),
            0,
            "Cursor parser must not accept Gemini list output: {sessions:?}"
        );
        assert!(sessions.iter().all(|s| s.session_id != "2"));
        assert!(sessions.iter().all(|s| s.session_id != "a1b2c3d4"));
    }

    #[test]
    fn parse_command_sessions_accepts_short_explicit_session_id() {
        let out = b"Session ID: abc123\n";
        let (sessions, _) = parse_command_sessions(out, SessionAgent::Cursor, "/repo", false);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, "abc123");
        assert_eq!(sessions[0].summary, None);
    }

    #[test]
    fn parse_command_sessions_does_not_pick_long_summary_word_as_id() {
        let out =
            b"550e8400-e29b-41d4-a716-446655440000 2026-06-29T09:10:11Z Refactor authentication\n";
        let (sessions, omitted) = parse_command_sessions(out, SessionAgent::Cursor, "/repo", false);
        assert_eq!(omitted, 0);
        assert_eq!(sessions.len(), 1);
        assert_eq!(
            sessions[0].session_id,
            "550e8400-e29b-41d4-a716-446655440000"
        );
        assert_eq!(
            sessions[0].summary.as_deref(),
            Some("Refactor authentication")
        );
    }

    #[test]
    fn parse_command_sessions_accepts_labeled_token_id() {
        let out = b"id=abc123 label from command\n";
        let (sessions, _) = parse_command_sessions(out, SessionAgent::Cursor, "/repo", false);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, "abc123");
        assert_eq!(sessions[0].summary.as_deref(), Some("label from command"));
    }

    #[test]
    fn line_summary_collapses_whitespace_and_controls() {
        let summary = line_summary(
            "ses_current_123456   first\n\tsecond\u{1b}   third",
            "ses_current_123456",
        );
        assert_eq!(summary.as_deref(), Some("first second third"));
    }

    #[test]
    fn parse_grok_sessions_table_output() {
        let out = br#"
(no label)
SESSION ID                            CREATED     UPDATED     STATUS      SUMMARY
019f1501-50e7-76d0-bb9e-4a72ede6b35d  2026-06-29  2026-06-29  local  List Sessions Command in Software Codebase
019f1501-69f1-7800-bc1e-cb269e1d985b  2026-06-29  2026-06-29  local  (no summary)
"#;
        let (sessions, omitted) = parse_command_sessions(out, SessionAgent::Grok, "/repo", false);
        assert_eq!(omitted, 0);
        assert_eq!(sessions.len(), 2);
        assert_eq!(
            sessions[0].session_id,
            "019f1501-50e7-76d0-bb9e-4a72ede6b35d"
        );
        assert_eq!(sessions[0].timestamp, "2026-06-29T00:00:00Z");
        assert_eq!(
            sessions[0].summary.as_deref(),
            Some("List Sessions Command in Software Codebase")
        );
        assert_eq!(sessions[1].summary, None);
    }
}
