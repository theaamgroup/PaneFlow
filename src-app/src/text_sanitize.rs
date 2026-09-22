//! Strip bidi-control and zero-width code points from untrusted text.
//!
//! Shared by every sink that echoes a terminal title, hook payload, or other
//! caller-controlled label back into chrome. No length cap: callers bound the
//! string first.

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
