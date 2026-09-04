//! Shared last-N-line window diff for `wait` / `flow` ready polls.

/// New output in `current` relative to a previous `surface.read` window.
///
/// When both snapshots carry `total_lines`, overlap is positional: the
/// last `current_total - baseline_total` lines of `current` are new. That
/// keeps a newly printed sentinel that happens to match the last baseline
/// line from being skipped after the 500-line window slides.
///
/// Without totals, fall back to content: `current` still starts with
/// `baseline` (the window has only grown), else the longest suffix of
/// `baseline` that is a prefix of `current`.
pub(crate) fn new_text_since_baseline(
    baseline: &str,
    current: &str,
    baseline_total_lines: Option<u64>,
    current_total_lines: Option<u64>,
) -> String {
    if current == baseline {
        return String::new();
    }
    if let (Some(old_total), Some(new_total)) = (baseline_total_lines, current_total_lines) {
        if new_total < old_total {
            return current.to_string();
        }
        if new_total > old_total {
            let added = (new_total - old_total) as usize;
            return suffix_line_window(current, added);
        }
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

fn suffix_line_window(text: &str, n: usize) -> String {
    if n == 0 {
        return String::new();
    }
    let line_count = text.lines().count();
    if n >= line_count {
        return text.to_string();
    }
    skip_lines(text, line_count - n).to_string()
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

    fn content_only(baseline: &str, current: &str) -> String {
        new_text_since_baseline(baseline, current, None, None)
    }

    #[test]
    fn unchanged_window_is_empty() {
        assert_eq!(content_only("a\nb\n", "a\nb\n"), "");
    }

    #[test]
    fn prefix_growth_keeps_the_suffix() {
        let base = "please print RENDER_AUDIT_DONE when complete\n";
        let current = "please print RENDER_AUDIT_DONE when complete\nactual work\n";
        assert_eq!(content_only(base, current), "actual work\n");
    }

    #[test]
    fn overlapping_suffix_is_not_new() {
        let base = "old header\nplease print RENDER_AUDIT_DONE when complete\nstill working\n";
        let shifted = "please print RENDER_AUDIT_DONE when complete\nstill working\nnew DONE\n";
        assert_eq!(content_only(base, shifted), "new DONE\n");
    }

    #[test]
    fn repeated_sentinel_after_slide_is_new() {
        let base = "noise-1\ntests passed\nkeep-me\n";
        let shifted = "keep-me\nnew output\ntests passed\n";
        assert_eq!(content_only(base, shifted), "new output\ntests passed\n");
    }

    #[test]
    fn identical_sentinel_at_window_head_is_new_when_total_lines_advance() {
        let base = "noise\nSENTINEL\n";
        let current = "SENTINEL\nmore work\n";
        assert_eq!(
            new_text_since_baseline(base, current, None, None),
            "more work\n",
            "content-only overlap would skip the reprinted sentinel"
        );
        assert_eq!(
            new_text_since_baseline(base, current, Some(2), Some(502)),
            "SENTINEL\nmore work\n"
        );
    }

    #[test]
    fn equal_totals_with_different_text_are_empty() {
        assert_eq!(
            new_text_since_baseline("old\n", "new\n", Some(10), Some(10)),
            ""
        );
    }

    #[test]
    fn decreased_totals_return_the_whole_current_window() {
        assert_eq!(
            new_text_since_baseline("aaaa\nbbbb\n", "reset\n", Some(50), Some(1)),
            "reset\n"
        );
    }
}
