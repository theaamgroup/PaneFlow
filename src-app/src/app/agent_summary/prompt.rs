//! Pure text preparation for the Agent Summary overlay (issue #576).
//!
//! No GPUI and no process spawning: what a pane's transcript becomes before
//! it reaches the on-device model, and what the sidecar's reply becomes
//! before it reaches the screen. Both directions are untrusted - the
//! transcript because it is terminal output, the reply because the model
//! read that output - so both are clamped here and unit-tested here.

use crate::limits::{MAX_AGENT_SUMMARY_CHARS, MAX_AGENT_SUMMARY_TAIL_BYTES};

/// Rows of history plus live screen read from a pane per summary. Roughly
/// 80 rows of an 80-100 column pane is ~2k tokens after stripping, which
/// leaves the ~4k-token context window room for the instructions and the
/// reply.
pub(crate) const TAIL_LINES: usize = 80;

/// The sidecar's answer for one pane.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SidecarReply {
    Summary(String),
    /// Already tidied: single line, no control characters, capped.
    Error(String),
}

/// What `paneflow-agent-summary --probe` reported.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ModelAvailability {
    Available,
    /// A one-line reason the overlay shows verbatim.
    Unavailable(String),
}

/// Strip control characters and escape sequences, keep the last
/// [`TAIL_LINES`] lines, and cap the byte size from the front so the live
/// end of the pane - the part that says what the agent is doing now -
/// survives.
///
/// The transcript is grid text (cells, not the byte stream), so escape
/// sequences are rare; a pane can still print a raw `ESC` into a log line,
/// and a stripped tail is cheaper for the model than a noisy one.
pub(crate) fn prepare_tail(text: &str) -> String {
    let clean = strip_controls(text);
    let lines: Vec<&str> = clean.lines().collect();
    let start = lines.len().saturating_sub(TAIL_LINES);
    let tail = lines[start..].join("\n");
    cap_from_front(&tail, MAX_AGENT_SUMMARY_TAIL_BYTES)
}

/// Drop C0/C1 controls except newline and tab, and skip CSI (`ESC [ … final`)
/// and OSC (`ESC ] … BEL|ST`) sequences whole. Any other `ESC` swallows one
/// following character, the way a terminal treats an unknown two-byte
/// escape.
fn strip_controls(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => match chars.next() {
                Some('[') => {
                    for c in chars.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&c) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    while let Some(c) = chars.next() {
                        if c == '\u{07}' {
                            break;
                        }
                        if c == '\u{1b}' {
                            if chars.peek() == Some(&'\\') {
                                chars.next();
                            }
                            break;
                        }
                    }
                }
                _ => {}
            },
            '\n' | '\t' => out.push(c),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

/// Keep at most `max_bytes` from the END of `text`, on a char boundary and,
/// when one is available, on a line boundary.
fn cap_from_front(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_owned();
    }
    let mut boundary = text.len() - max_bytes;
    while !text.is_char_boundary(boundary) {
        boundary += 1;
    }
    if let Some(newline) = text[boundary..].find('\n')
        && boundary + newline + 1 < text.len()
    {
        boundary += newline + 1;
    }
    text[boundary..].to_owned()
}

/// The one request line the sidecar reads from stdin. `fenced` is the tail
/// already wrapped by `ipc_handler::wrap_untrusted`; the sidecar's
/// instructions name that fence as data, never as a message.
pub(crate) fn request_json(agent: &str, state: &str, fenced: &str) -> String {
    serde_json::json!({
        "agent": agent,
        "state": state,
        "text": fenced,
    })
    .to_string()
}

/// Parse the sidecar's stdout: the first non-empty line is the reply, and
/// anything that is not `{"summary":…}` or `{"error":…}` is an error the
/// overlay can show. A replaced or misbehaving helper cannot put more than
/// [`MAX_AGENT_SUMMARY_CHARS`] of single-line text on screen.
pub(crate) fn parse_reply(stdout: &[u8]) -> SidecarReply {
    let Some(line) = first_line(stdout) else {
        return SidecarReply::Error("the summary helper returned nothing".to_owned());
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
        return SidecarReply::Error("the summary helper returned malformed output".to_owned());
    };
    if let Some(summary) = value.get("summary").and_then(|v| v.as_str()) {
        let summary = tidy_summary(summary);
        return if summary.is_empty() {
            SidecarReply::Error("the model returned an empty summary".to_owned())
        } else {
            SidecarReply::Summary(summary)
        };
    }
    if let Some(error) = value.get("error").and_then(|v| v.as_str()) {
        let error = tidy_summary(error);
        return SidecarReply::Error(if error.is_empty() {
            "the summary helper failed".to_owned()
        } else {
            error
        });
    }
    SidecarReply::Error("the summary helper returned an unexpected reply".to_owned())
}

/// Parse `--probe` output. Anything but a well-formed `{"available":true}`
/// is unavailable: the overlay must never assume a model it cannot prove.
pub(crate) fn parse_probe(stdout: &[u8]) -> ModelAvailability {
    let Some(line) = first_line(stdout) else {
        return ModelAvailability::Unavailable(
            "the summary helper did not answer the availability probe".to_owned(),
        );
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
        return ModelAvailability::Unavailable(
            "the summary helper returned a malformed availability probe".to_owned(),
        );
    };
    if value.get("available").and_then(|v| v.as_bool()) == Some(true) {
        return ModelAvailability::Available;
    }
    let reason = value
        .get("reason")
        .and_then(|v| v.as_str())
        .map(tidy_summary)
        .filter(|r| !r.is_empty())
        .unwrap_or_else(|| "the on-device model is unavailable".to_owned());
    ModelAvailability::Unavailable(reason)
}

fn first_line(stdout: &[u8]) -> Option<&str> {
    std::str::from_utf8(stdout)
        .ok()?
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
}

/// One line, no control characters, whitespace collapsed, capped at
/// [`MAX_AGENT_SUMMARY_CHARS`] characters. Applied to summaries, error
/// lines and probe reasons alike: every string that came back from the
/// helper is rendered as inert text and nothing else.
pub(crate) fn tidy_summary(text: &str) -> String {
    let mut out = String::with_capacity(text.len().min(MAX_AGENT_SUMMARY_CHARS));
    let mut pending_space = false;
    for c in text.chars() {
        if c.is_whitespace() {
            pending_space = !out.is_empty();
            continue;
        }
        if c.is_control() {
            continue;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        out.push(c);
    }
    if out.chars().count() > MAX_AGENT_SUMMARY_CHARS {
        let mut cut: String = out.chars().take(MAX_AGENT_SUMMARY_CHARS - 1).collect();
        cut.push('\u{2026}');
        return cut;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepare_tail_keeps_only_the_last_lines() {
        let text: String = (0..200).map(|i| format!("line {i}\n")).collect();
        let tail = prepare_tail(&text);
        let lines: Vec<&str> = tail.lines().collect();
        assert_eq!(lines.len(), TAIL_LINES);
        assert_eq!(lines[0], "line 120");
        assert_eq!(lines[TAIL_LINES - 1], "line 199");
    }

    #[test]
    fn prepare_tail_strips_escape_sequences_and_controls() {
        let text = "\u{1b}[32mgreen\u{1b}[0m text\u{1b}]7;file://h/dir\u{07} more\u{1b}]0;t\u{1b}\\ end\u{07}\u{08}\u{7f}\ttab\r\nnext\u{1b}x";
        assert_eq!(
            prepare_tail(text),
            "green text more end\ttab\nnext",
            "CSI, OSC (BEL and ST terminated), a lone two-byte ESC, BEL, BS, DEL and CR go; \\n and \\t stay"
        );
    }

    #[test]
    fn prepare_tail_caps_bytes_from_the_front_on_a_char_boundary() {
        // Multi-byte characters straddle the cut so a byte-index slice would
        // panic; a line boundary is preferred when one exists after the cut.
        let line = "é".repeat(100); // 200 bytes per line
        let text: String = (0..60).map(|_| format!("{line}\n")).collect();
        let tail = prepare_tail(&text);
        assert!(tail.len() <= MAX_AGENT_SUMMARY_TAIL_BYTES, "{}", tail.len());
        assert!(tail.starts_with('é'), "must start on a whole character");
        assert!(
            tail.lines().all(|l| l == line),
            "must start on a whole line: {:?}",
            tail.lines().next()
        );
        assert!(tail.ends_with(&line), "the live end of the pane survives");
    }

    #[test]
    fn prepare_tail_falls_back_to_a_partial_line_when_the_tail_is_one_line() {
        let text = "x".repeat(MAX_AGENT_SUMMARY_TAIL_BYTES * 2);
        let tail = prepare_tail(&text);
        assert_eq!(tail.len(), MAX_AGENT_SUMMARY_TAIL_BYTES);
    }

    #[test]
    fn request_json_carries_the_three_fields_verbatim() {
        let fenced = "<untrusted_terminal_output id=\"1\">\nhi \"there\"\n</untrusted_terminal_output id=\"1\">";
        let json = request_json("Claude Code", "Needs input", fenced);
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["agent"], "Claude Code");
        assert_eq!(value["state"], "Needs input");
        assert_eq!(value["text"], fenced);
        assert!(!json.contains('\n'), "one request line: {json}");
    }

    #[test]
    fn parse_reply_reads_summary_error_and_garbage() {
        assert_eq!(
            parse_reply(b"{\"summary\":\"Running tests.\"}\n"),
            SidecarReply::Summary("Running tests.".to_owned())
        );
        assert_eq!(
            parse_reply(b"\n  {\"error\":\"the model declined\"}\n"),
            SidecarReply::Error("the model declined".to_owned())
        );
        assert!(matches!(parse_reply(b""), SidecarReply::Error(_)));
        assert!(matches!(parse_reply(b"not json"), SidecarReply::Error(_)));
        assert!(matches!(
            parse_reply(b"{\"other\":1}"),
            SidecarReply::Error(e) if e.contains("unexpected")
        ));
        assert!(matches!(
            parse_reply(b"{\"summary\":\"  \\n \"}"),
            SidecarReply::Error(e) if e.contains("empty")
        ));
        assert!(matches!(parse_reply(&[0xff, 0xfe]), SidecarReply::Error(_)));
    }

    #[test]
    fn replies_are_one_line_control_free_and_capped() {
        let long = "word ".repeat(200);
        let reply = parse_reply(
            format!("{{\"summary\":\"first\\nline\\t\\u0007 two   {long}\"}}").as_bytes(),
        );
        let SidecarReply::Summary(summary) = reply else {
            panic!("expected a summary");
        };
        assert!(summary.starts_with("first line two word"), "{summary}");
        assert!(!summary.contains('\n'));
        assert!(!summary.contains('\u{07}'));
        assert!(!summary.contains("  "));
        assert_eq!(summary.chars().count(), MAX_AGENT_SUMMARY_CHARS);
        assert!(summary.ends_with('\u{2026}'));
    }

    #[test]
    fn parse_probe_trusts_only_an_explicit_true() {
        assert_eq!(
            parse_probe(b"{\"available\":true}\n"),
            ModelAvailability::Available
        );
        assert_eq!(
            parse_probe(b"{\"available\":false,\"reason\":\"Apple Intelligence is off\"}"),
            ModelAvailability::Unavailable("Apple Intelligence is off".to_owned())
        );
        assert!(matches!(
            parse_probe(b"{\"available\":false}"),
            ModelAvailability::Unavailable(r) if r.contains("unavailable")
        ));
        assert!(matches!(
            parse_probe(b"{\"available\":\"true\"}"),
            ModelAvailability::Unavailable(_)
        ));
        assert!(matches!(
            parse_probe(b""),
            ModelAvailability::Unavailable(_)
        ));
        assert!(matches!(
            parse_probe(b"segfault"),
            ModelAvailability::Unavailable(_)
        ));
    }
}
