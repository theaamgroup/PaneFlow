//! Shared last-N-line window diff for `wait` / `flow` ready polls.

/// New output in `current` relative to a previous `surface.read` window.
///
/// Fast path: `current` still starts with `baseline` (the window has only
/// grown). Fallback: longest suffix of `baseline` that is a prefix of
/// `current` (the last-N-line window slid). The remainder is new, so a
/// still-present overlapping line does not rematch and a later identical
/// sentinel printed after the slide does.
pub(crate) fn new_text_since_baseline(baseline: &str, current: &str) -> String {
    if current == baseline {
        return String::new();
    }
    if let Some(rest) = current.strip_prefix(baseline) {
        return rest.to_string();
    }

    let old: Vec<&str> = baseline.lines().collect();
    let new_lines: Vec<&str> = current.lines().collect();
    let max_k = old.len().min(new_lines.len());
    let mut overlap = 0usize;
    for k in (1..=max_k).rev() {
        if old[old.len() - k..] == new_lines[..k] {
            overlap = k;
            break;
        }
    }
    skip_lines(current, overlap).to_string()
}

fn skip_lines(text: &str, n: usize) -> &str {
    let mut rest = text;
    for _ in 0..n {
        rest = match rest.find('\n') {
            Some(i) => &rest[i + 1..],
            None => return "",
        };
    }
    rest
}

#[cfg(test)]
mod tests {
    use super::new_text_since_baseline;

    #[test]
    fn unchanged_window_is_empty() {
        assert_eq!(new_text_since_baseline("a\nb\n", "a\nb\n"), "");
    }

    #[test]
    fn prefix_growth_keeps_the_suffix() {
        let base = "please print RENDER_AUDIT_DONE when complete\n";
        let current = "please print RENDER_AUDIT_DONE when complete\nactual work\n";
        assert_eq!(new_text_since_baseline(base, current), "actual work\n");
    }

    #[test]
    fn overlapping_suffix_is_not_new() {
        let base = "old header\nplease print RENDER_AUDIT_DONE when complete\nstill working\n";
        let shifted = "please print RENDER_AUDIT_DONE when complete\nstill working\nnew DONE\n";
        assert_eq!(new_text_since_baseline(base, shifted), "new DONE\n");
    }

    #[test]
    fn repeated_sentinel_after_slide_is_new() {
        let base = "noise-1\ntests passed\nkeep-me\n";
        let shifted = "keep-me\nnew output\ntests passed\n";
        assert_eq!(
            new_text_since_baseline(base, shifted),
            "new output\ntests passed\n"
        );
    }
}
