//! Text sanitizers.
//!
//! [`strip_bidi_zero_width`] strips bidi-control and zero-width code points
//! from untrusted text. It is shared by every sink that echoes a terminal
//! title, hook payload, or other caller-controlled label back into chrome. No
//! length cap: callers bound the string first. [`normalize_prompt_text`]
//! prepares a prompt for a PTY prefill.

/// Remove bidi-control and zero-width code points from `text`.
///
/// Reallocates only when such a character is present, so a clean string is
/// returned as-is.
pub(crate) fn strip_bidi_zero_width(text: String) -> String {
    if text.chars().any(is_bidi_or_zero_width) {
        text.chars()
            .filter(|&c| !is_bidi_or_zero_width(c))
            .collect()
    } else {
        text
    }
}

/// True for Unicode bidi-control and zero-width code points.
fn is_bidi_or_zero_width(c: char) -> bool {
    matches!(
        c,
        // Zero-width: ZWSP, ZWNJ, ZWJ, word joiner, BOM/ZWNBSP.
        '\u{200B}' | '\u{200C}' | '\u{200D}' | '\u{2060}' | '\u{FEFF}'
        // Directional marks: LRM, RLM, ALM.
        | '\u{200E}' | '\u{200F}' | '\u{061C}'
        // Embeddings/overrides: LRE, RLE, PDF, LRO, RLO.
        | '\u{202A}'..='\u{202E}'
        // Isolates: LRI, RLI, FSI, PDI.
        | '\u{2066}'..='\u{2069}'
        // Deprecated bidi/shaping format chars.
        | '\u{206A}'..='\u{206F}'
        // Interlinear annotation anchor/separator/terminator.
        | '\u{FFF9}'..='\u{FFFB}'
    )
}

/// Maximum prefilled prompt size - parity with the IPC `surface.send_text`
/// 64 KiB cap (`MAX_TEXT_LEN`, ipc_handler.rs).
const MAX_PROMPT_TEXT: usize = 64 * 1024;

/// Normalize a prompt before it is prefilled into an agent's PTY (the
/// session handoff). The delivery profile is LF-only: CR/CRLF become LF and
/// trailing newlines are trimmed, so a compliant TUI treats in-envelope
/// newlines as literal input and a target without bracketed-paste awareness
/// never sees a trailing CR it could read as a submit. Oversized text is
/// truncated at a char boundary (64 KiB, IPC parity). Returns
/// `(normalized, was_truncated)`.
pub(crate) fn normalize_prompt_text(text: &str) -> (String, bool) {
    let mut t = text.replace("\r\n", "\n").replace('\r', "\n");
    while t.ends_with('\n') {
        t.pop();
    }
    let truncated = t.len() > MAX_PROMPT_TEXT;
    if truncated {
        let mut cut = MAX_PROMPT_TEXT;
        while cut > 0 && !t.is_char_boundary(cut) {
            cut -= 1;
        }
        t.truncate(cut);
    }
    (t, truncated)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_converts_cr_and_crlf_to_lf() {
        let (out, truncated) = normalize_prompt_text("a\r\nb\rc\nd");
        assert_eq!(out, "a\nb\nc\nd");
        assert!(!truncated);
    }

    #[test]
    fn normalize_trims_trailing_newlines_only() {
        // Trailing CR/LF would ride right behind the paste envelope and read
        // as a submit on a non-bracketed-paste-aware target; interior
        // newlines are part of a multi-line prompt.
        let (out, _) = normalize_prompt_text("line1\nline2\r\n\n\r");
        assert_eq!(out, "line1\nline2");
    }

    #[test]
    fn normalize_truncates_at_char_boundary() {
        // 64 KiB cap, never splitting a multibyte char.
        let big = "é".repeat(MAX_PROMPT_TEXT); // 2 bytes per char
        let (out, truncated) = normalize_prompt_text(&big);
        assert!(truncated);
        assert!(out.len() <= MAX_PROMPT_TEXT);
        assert!(out.is_char_boundary(out.len()));
        let (ok, truncated) = normalize_prompt_text("short prompt");
        assert_eq!(ok, "short prompt");
        assert!(!truncated);
    }
}
