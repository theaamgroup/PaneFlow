//! Pure diff-to-text serialization (US-001, prd-ai-in-diff-2026-Q3.md).
//!
//! Turns the read-only diff value types (`FileDiff` / `DiffHunk` from `git.rs` /
//! `engine.rs`) back into a byte-correct unified diff string, with no GPUI, no
//! I/O, no `self`. This is the load-bearing primitive under every "copy hunk",
//! "send to agent", and "review" gesture (EP-001..003): by generating the diff
//! deterministically here, the agent never sees a hallucinated `@@` header -
//! the #1 failure mode of LLM-*generated* patches is designed out because the
//! model only ever *consumes* a diff we produced.
//!
//! Row indices in `DiffHunk` are 0-based half-open ranges into the line
//! sequence produced by `imara_diff::sources::lines_with_terminator` (see
//! `engine::compute_hunks`); [`lines_inclusive`] reproduces exactly that
//! segmentation so the `@@` math stays aligned with the rendered rows.

use std::fmt::Write as _;
use std::ops::Range;

use super::engine::DiffHunk;
use super::git::{FileChange, FileDiff};

/// Context lines emitted on each side of a changed region, matching the
/// `diff -U3` / git default. Hunks whose context windows touch are merged into a
/// single `@@` block so the output is always a valid unified diff.
const CONTEXT: u32 = 3;

/// Serialize a whole [`FileDiff`] into a git-style unified diff (raw, no fence).
/// Suitable for "copy file diff" and for an agent review payload.
pub(crate) fn file_to_unified(file: &FileDiff) -> String {
    let mut out = String::new();
    let old_disp = if file.change == FileChange::Renamed {
        file.old_path.as_deref().unwrap_or(&file.path)
    } else {
        &file.path
    };
    let _ = writeln!(out, "diff --git a/{old_disp} b/{}", file.path);

    if file.change == FileChange::Renamed
        && let Some(old) = &file.old_path
    {
        let _ = writeln!(out, "rename from {old}");
        let _ = writeln!(out, "rename to {}", file.path);
    }

    if file.is_binary {
        let _ = writeln!(out, "Binary files a/{old_disp} b/{} differ", file.path);
        return out;
    }

    let (a_label, b_label) = dev_null_labels(file);
    let _ = writeln!(out, "--- {a_label}");
    let _ = writeln!(out, "+++ {b_label}");

    let base_lines = lines_inclusive(&file.base_text);
    let new_lines = lines_inclusive(&file.new_text);
    for group in group_hunks(&file.hunks, base_lines.len() as u32, new_lines.len() as u32) {
        emit_group(&mut out, &group, &base_lines, &new_lines);
    }
    out
}

/// Serialize a single [`DiffHunk`] into a fenced ```` ```diff ```` block,
/// prefixed by a `path:Lstart-Lend` tag so an agent knows exactly which lines
/// the change touches. Suitable for "copy hunk" and `@diff`-style handoff.
///
/// Context stops at neighbouring hunks ([`isolated_hunk_group`]). Widening this
/// hunk alone would run into a close change, and the `@@` new-count would no
/// longer match the body (#731).
pub(crate) fn hunk_to_unified(file: &FileDiff, hunk: &DiffHunk) -> String {
    let tag = hunk_tag(file, hunk);
    if file.is_binary {
        return format!(
            "{tag}\n```diff\nBinary files a/{p} b/{p} differ\n```\n",
            p = file.path
        );
    }
    let base_lines = lines_inclusive(&file.base_text);
    let new_lines = lines_inclusive(&file.new_text);
    let mut body = String::new();
    let group = isolated_hunk_group(
        &file.hunks,
        hunk,
        base_lines.len() as u32,
        new_lines.len() as u32,
    );
    emit_group(&mut body, &group, &base_lines, &new_lines);
    format!("{tag}\n```diff\n{body}```\n")
}

/// `--- a/x` / `+++ b/x` labels, substituting `/dev/null` for the absent side of
/// an Added or Deleted file (git's convention).
fn dev_null_labels(file: &FileDiff) -> (String, String) {
    match file.change {
        FileChange::Added => ("/dev/null".to_string(), format!("b/{}", file.path)),
        FileChange::Deleted => (format!("a/{}", file.path), "/dev/null".to_string()),
        FileChange::Renamed => (
            format!("a/{}", file.old_path.as_deref().unwrap_or(&file.path)),
            format!("b/{}", file.path),
        ),
        FileChange::Modified => (format!("a/{}", file.path), format!("b/{}", file.path)),
    }
}

/// `path:Lstart-Lend` tag (1-based, inclusive), anchored on the new side when it
/// has content, else on the base side (a pure deletion). `pub(crate)` so the
/// "copy hunk" confirmation toast (US-003) can label exactly which lines landed.
pub(crate) fn hunk_tag(file: &FileDiff, hunk: &DiffHunk) -> String {
    let (start, end) = if hunk.new_row_range.start != hunk.new_row_range.end {
        (hunk.new_row_range.start + 1, hunk.new_row_range.end)
    } else {
        (hunk.base_row_range.start + 1, hunk.base_row_range.end)
    };
    format!("{}:L{start}-L{end}", file.path)
}

/// One merged hunk group: the union row span (already context-expanded) on each
/// side plus the changed hunks it covers, in order.
struct HunkGroup {
    base: Range<u32>,
    new: Range<u32>,
    hunks: Vec<DiffHunk>,
}

/// Expand each hunk by [`CONTEXT`] lines (clamped to file bounds) and merge hunks
/// whose windows touch, so every emitted `@@` block is a valid, non-overlapping
/// unified-diff hunk. Hunks arrive in row order from `compute_hunks`.
///
/// Whole-file output ([`file_to_unified`]) uses this merge. A one-hunk copy does
/// not: see [`isolated_hunk_group`].
fn group_hunks(hunks: &[DiffHunk], base_lines: u32, new_lines: u32) -> Vec<HunkGroup> {
    let mut groups: Vec<HunkGroup> = Vec::new();
    for h in hunks {
        let bs = h.base_row_range.start.saturating_sub(CONTEXT);
        let be = (h.base_row_range.end + CONTEXT).min(base_lines);
        let ns = h.new_row_range.start.saturating_sub(CONTEXT);
        let ne = (h.new_row_range.end + CONTEXT).min(new_lines);
        if let Some(last) = groups.last_mut()
            && bs <= last.base.end
            && ns <= last.new.end
        {
            last.base.end = last.base.end.max(be);
            last.new.end = last.new.end.max(ne);
            last.hunks.push(h.clone());
            continue;
        }
        groups.push(HunkGroup {
            base: bs..be,
            new: ns..ne,
            hunks: vec![h.clone()],
        });
    }
    groups
}

/// Context window for copying `hunk` by itself.
///
/// [`group_hunks`] widens each side by [`CONTEXT`] on its own, but [`emit_group`]
/// prints every context line from the base side. That stays aligned when every
/// nearby change is in the group. For one hunk it does not: a neighbour inside
/// the window changes that side's length, so the `@@` new-count disagrees with
/// the body and the neighbour's lines are emitted as context (#731).
///
/// Clip at the previous and next hunk (or the file bounds) and keep the same
/// lead and trail on both sides, so the new window is the base window shifted
/// onto the new rows.
fn isolated_hunk_group(
    hunks: &[DiffHunk],
    hunk: &DiffHunk,
    base_len: u32,
    new_len: u32,
) -> HunkGroup {
    let idx = hunks
        .iter()
        .position(|candidate| std::ptr::eq(candidate, hunk))
        .or_else(|| hunks.iter().position(|candidate| candidate == hunk));

    let (base_lo, new_lo) = match idx {
        Some(i) if i > 0 => {
            let prev = &hunks[i - 1];
            (prev.base_row_range.end, prev.new_row_range.end)
        }
        _ => (0, 0),
    };
    let (base_hi, new_hi) = match idx {
        Some(i) if i + 1 < hunks.len() => {
            let next = &hunks[i + 1];
            (next.base_row_range.start, next.new_row_range.start)
        }
        _ => (base_len, new_len),
    };

    let lead = CONTEXT
        .min(hunk.base_row_range.start.saturating_sub(base_lo))
        .min(hunk.new_row_range.start.saturating_sub(new_lo));
    let trail = CONTEXT
        .min(base_hi.saturating_sub(hunk.base_row_range.end))
        .min(new_hi.saturating_sub(hunk.new_row_range.end));

    HunkGroup {
        base: (hunk.base_row_range.start - lead)..(hunk.base_row_range.end + trail),
        new: (hunk.new_row_range.start - lead)..(hunk.new_row_range.end + trail),
        hunks: vec![hunk.clone()],
    }
}

/// Emit one `@@` header + interleaved context/removed/added body for a group.
fn emit_group(out: &mut String, group: &HunkGroup, base_lines: &[&str], new_lines: &[&str]) {
    let bc = group.base.end - group.base.start;
    let nc = group.new.end - group.new.start;
    let _ = writeln!(
        out,
        "@@ -{} +{} @@",
        fmt_range(group.base.start, bc),
        fmt_range(group.new.start, nc),
    );

    let mut bcur = group.base.start;
    for h in &group.hunks {
        // Leading context: unchanged lines (identical + equal-length on both
        // sides) between the cursor and this hunk; emit from the base side.
        for r in bcur..h.base_row_range.start {
            push_line(out, ' ', base_lines[r as usize]);
        }
        for r in h.base_row_range.clone() {
            push_line(out, '-', base_lines[r as usize]);
        }
        for r in h.new_row_range.clone() {
            push_line(out, '+', new_lines[r as usize]);
        }
        bcur = h.base_row_range.end;
    }
    // Trailing context.
    for r in bcur..group.base.end {
        push_line(out, ' ', base_lines[r as usize]);
    }
}

/// Unified-diff range field: `start+1,count`, or `start,0` for an empty side
/// (git anchors an empty hunk on the line *before* which content is inserted).
fn fmt_range(start: u32, count: u32) -> String {
    if count == 0 {
        format!("{start},0")
    } else {
        format!("{},{count}", start + 1)
    }
}

/// Push one diff line `<prefix><content>`, appending a `\ No newline at end of
/// file` marker when the source line lacks its terminator (only the final line
/// of a side can).
fn push_line(out: &mut String, prefix: char, line: &str) {
    out.push(prefix);
    out.push_str(line);
    if !line.ends_with('\n') {
        out.push('\n');
        out.push_str("\\ No newline at end of file\n");
    }
}

/// Split `text` into lines *including* their `\n` terminator, reproducing
/// `imara_diff::sources::lines_with_terminator` so `DiffHunk` row indices slice
/// correctly: the empty string yields zero lines, and a missing final terminator
/// yields a final line without `\n`.
fn lines_inclusive(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0usize;
    for (i, b) in text.bytes().enumerate() {
        if b == b'\n' {
            out.push(&text[start..=i]);
            start = i + 1;
        }
    }
    if start < text.len() {
        out.push(&text[start..]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::engine::compute_hunks;

    fn modified(path: &str, base: &str, new: &str) -> FileDiff {
        FileDiff {
            path: path.to_string(),
            change: FileChange::Modified,
            old_path: None,
            base_text: base.to_string(),
            new_text: new.to_string(),
            hunks: compute_hunks(base, new),
            is_binary: false,
        }
    }

    #[test]
    fn lines_inclusive_matches_terminator_semantics() {
        assert_eq!(lines_inclusive("a\nb\n"), vec!["a\n", "b\n"]);
        assert_eq!(lines_inclusive("a\nb"), vec!["a\n", "b"]);
        assert_eq!(lines_inclusive(""), Vec::<&str>::new());
        assert_eq!(lines_inclusive("a\n"), vec!["a\n"]);
    }

    #[test]
    fn new_file_uses_dev_null_and_zero_base_count() {
        let mut f = modified("src/new.rs", "", "a\nb\n");
        f.change = FileChange::Added;
        let out = file_to_unified(&f);
        assert!(out.contains("diff --git a/src/new.rs b/src/new.rs\n"));
        assert!(out.contains("--- /dev/null\n"));
        assert!(out.contains("+++ b/src/new.rs\n"));
        // Empty base side -> `0,0`; two added lines -> `1,2`.
        assert!(out.contains("@@ -0,0 +1,2 @@\n"), "got:\n{out}");
        assert!(out.contains("+a\n"));
        assert!(out.contains("+b\n"));
        assert!(!out.contains("-a"));
    }

    #[test]
    fn pure_deletion_keeps_context_and_counts() {
        let f = modified("x.rs", "a\nb\nc\n", "a\nc\n");
        let out = file_to_unified(&f);
        // 3 base lines shown, 2 new lines shown.
        assert!(out.contains("@@ -1,3 +1,2 @@\n"), "got:\n{out}");
        assert!(out.contains(" a\n"));
        assert!(out.contains("-b\n"));
        assert!(out.contains(" c\n"));
        assert!(!out.contains("+b"));
    }

    #[test]
    fn modification_emits_minus_then_plus() {
        let f = modified("x.rs", "a\nb\nc\n", "a\nB\nc\n");
        let out = file_to_unified(&f);
        assert!(out.contains("@@ -1,3 +1,3 @@\n"), "got:\n{out}");
        let minus = out.find("-b\n").expect("minus line");
        let plus = out.find("+B\n").expect("plus line");
        assert!(minus < plus, "removed line must precede added line:\n{out}");
    }

    #[test]
    fn rename_emits_rename_headers() {
        let mut f = modified("new.rs", "a\nb\n", "a\nB\n");
        f.change = FileChange::Renamed;
        f.old_path = Some("old.rs".to_string());
        let out = file_to_unified(&f);
        assert!(
            out.contains("diff --git a/old.rs b/new.rs\n"),
            "got:\n{out}"
        );
        assert!(out.contains("rename from old.rs\n"));
        assert!(out.contains("rename to new.rs\n"));
        assert!(out.contains("--- a/old.rs\n"));
        assert!(out.contains("+++ b/new.rs\n"));
    }

    #[test]
    fn binary_emits_stub_no_body() {
        let mut f = modified("logo.png", "", "");
        f.is_binary = true;
        f.change = FileChange::Modified;
        let out = file_to_unified(&f);
        assert!(
            out.contains("Binary files a/logo.png b/logo.png differ\n"),
            "got:\n{out}"
        );
        assert!(!out.contains("@@"));
    }

    #[test]
    fn missing_final_newline_marker() {
        // new_text's last line lacks a terminator.
        let f = modified("x.rs", "a\nb\n", "a\nb");
        let out = file_to_unified(&f);
        assert!(
            out.contains("\\ No newline at end of file\n"),
            "got:\n{out}"
        );
    }

    #[test]
    fn far_hunks_split_near_hunks_merge() {
        // Two changes far apart -> two @@ blocks.
        let base = "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11\n12\n13\n14\n15\n16\n";
        let new = "X\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11\n12\n13\n14\n15\nY\n";
        let f = modified("x.rs", base, new);
        let out = file_to_unified(&f);
        assert_eq!(
            out.matches("@@ ").count(),
            2,
            "far hunks must not merge:\n{out}"
        );

        // Two changes one line apart -> a single merged @@ block.
        let base2 = "a\nb\nc\nd\ne\n";
        let new2 = "A\nb\nc\nd\nE\n";
        let f2 = modified("y.rs", base2, new2);
        let out2 = file_to_unified(&f2);
        assert_eq!(
            out2.matches("@@ ").count(),
            1,
            "near hunks must merge:\n{out2}"
        );
    }

    #[test]
    fn hunk_to_unified_has_tag_and_fence() {
        let f = modified("src/foo.rs", "a\nb\nc\n", "a\nB\nc\n");
        let hunk = &f.hunks[0];
        let out = hunk_to_unified(&f, hunk);
        assert!(out.starts_with("src/foo.rs:L2-L2\n"), "got:\n{out}");
        assert!(out.contains("```diff\n"));
        assert!(out.trim_end().ends_with("```"));
        assert!(out.contains("-b\n"));
        assert!(out.contains("+B\n"));
    }

    #[test]
    fn hunk_to_unified_counts_match_body_near_eof_neighbor() {
        // Lines 1..=9, with 6 replaced by X and 8..=9 deleted. One unchanged
        // line ("7") sits between the hunks, inside the ±3 context window.
        let base = "1\n2\n3\n4\n5\n6\n7\n8\n9\n";
        let new = "1\n2\n3\n4\n5\nX\n7\n";
        let near_eof = modified("x.rs", base, new);
        assert_eq!(
            near_eof.hunks.len(),
            2,
            "neighbour must stay its own hunk: {:?}",
            near_eof.hunks
        );
        let copied = hunk_to_unified(&near_eof, &near_eof.hunks[0]);
        assert!(
            copied.contains("-6\n") && copied.contains("+X\n"),
            "got:\n{copied}"
        );
        assert_header_counts_match_body(&copied);
        let leaked = unified_hunks(&copied).iter().flatten().any(|line| {
            let text = line
                .strip_prefix(' ')
                .or_else(|| line.strip_prefix('-'))
                .or_else(|| line.strip_prefix('+'))
                .unwrap_or(line);
            text == "8" || text == "9"
        });
        assert!(
            !leaked,
            "deleted neighbour lines leaked into the copied hunk:\n{copied}"
        );
        assert_git_apply_accepts("x.rs", base, &copied);

        // The whole-file diff merges both hunks and must still be applicable.
        let whole = file_to_unified(&near_eof);
        assert_header_counts_match_body(&whole);
        assert!(
            whole.contains("-8\n") && whole.contains("-9\n"),
            "file diff dropped the deletion:\n{whole}"
        );

        // Insertion two lines above the copied edit. Not an end-of-file clip:
        // the new side is longer, and the windows meet at the start of the file.
        let above_base = "a\nb\nc\nd\ne\nf\n";
        let above_new = "X\na\nb\nC\nd\ne\nf\n";
        let inserted_above = modified("y.rs", above_base, above_new);
        assert_eq!(inserted_above.hunks.len(), 2, "{:?}", inserted_above.hunks);
        assert!(
            inserted_above.hunks[0].base_row_range.is_empty(),
            "first hunk should be the insertion: {:?}",
            inserted_above.hunks
        );
        let above_gap = inserted_above.hunks[1]
            .base_row_range
            .start
            .saturating_sub(inserted_above.hunks[0].base_row_range.end);
        assert!(
            above_gap < 3,
            "insertion must sit within 3 lines, gap {above_gap}: {:?}",
            inserted_above.hunks
        );
        let above = hunk_to_unified(&inserted_above, &inserted_above.hunks[1]);
        assert_header_counts_match_body(&above);
        assert_git_apply_accepts("y.rs", above_base, &above);

        // Insertion one line below the copied edit, so the clip is not only at
        // the start of the file either.
        let below_base = "a\nb\nc\nd\n";
        let below_new = "a\nB\nc\nX\nd\n";
        let inserted_below = modified("z.rs", below_base, below_new);
        assert_eq!(inserted_below.hunks.len(), 2, "{:?}", inserted_below.hunks);
        assert!(
            inserted_below.hunks[1].base_row_range.is_empty(),
            "second hunk should be the insertion: {:?}",
            inserted_below.hunks
        );
        let below_gap = inserted_below.hunks[1]
            .base_row_range
            .start
            .saturating_sub(inserted_below.hunks[0].base_row_range.end);
        assert!(
            below_gap < 3,
            "insertion must sit within 3 lines, gap {below_gap}: {:?}",
            inserted_below.hunks
        );
        let below = hunk_to_unified(&inserted_below, &inserted_below.hunks[0]);
        assert_header_counts_match_body(&below);
        assert_git_apply_accepts("z.rs", below_base, &below);
    }

    /// Old-side lines are ` ` and `-`; new-side lines are ` ` and `+`. Counts
    /// come from the `@@` header, not from a fixture that restates the body.
    fn assert_header_counts_match_body(text: &str) {
        let hunks = unified_hunks(text);
        assert!(!hunks.is_empty(), "no @@ header in:\n{text}");
        for hunk in hunks {
            let (old_count, new_count) = header_counts(hunk[0]);
            let mut old_lines = 0usize;
            let mut new_lines = 0usize;
            for line in &hunk[1..] {
                if line.starts_with('\\') {
                    continue;
                }
                match line.as_bytes().first().copied() {
                    Some(b' ') => {
                        old_lines += 1;
                        new_lines += 1;
                    }
                    Some(b'-') => old_lines += 1,
                    Some(b'+') => new_lines += 1,
                    _ => {}
                }
            }
            assert_eq!(old_count, old_lines, "old @@ count != body:\n{text}");
            assert_eq!(new_count, new_lines, "new @@ count != body:\n{text}");
        }
    }

    fn header_counts(header: &str) -> (usize, usize) {
        let mut parts = header.split_whitespace();
        assert_eq!(parts.next(), Some("@@"), "{header}");
        let old = parts.next().expect("old range");
        let new = parts.next().expect("new range");
        assert_eq!(parts.next(), Some("@@"), "{header}");
        (range_count(old), range_count(new))
    }

    fn range_count(field: &str) -> usize {
        let digits = field
            .strip_prefix('-')
            .or_else(|| field.strip_prefix('+'))
            .expect("range sign");
        let (_, count) = digits.split_once(',').expect("explicit range count");
        count.parse().expect("range count")
    }

    fn unified_hunks(text: &str) -> Vec<Vec<&str>> {
        let mut hunks: Vec<Vec<&str>> = Vec::new();
        let mut current: Option<Vec<&str>> = None;
        for line in text.lines() {
            if line.starts_with("@@ ") {
                if let Some(done) = current.take() {
                    hunks.push(done);
                }
                current = Some(vec![line]);
            } else if let Some(body) = current.as_mut()
                && matches!(line.as_bytes().first(), Some(b' ' | b'+' | b'-' | b'\\'))
            {
                body.push(line);
            }
        }
        if let Some(done) = current {
            hunks.push(done);
        }
        hunks
    }

    fn assert_git_apply_accepts(file_name: &str, base: &str, copied: &str) {
        let hunks = unified_hunks(copied);
        assert_eq!(hunks.len(), 1, "expected one hunk:\n{copied}");
        let mut patch = format!("--- a/{file_name}\n+++ b/{file_name}\n");
        for line in &hunks[0] {
            patch.push_str(line);
            patch.push('\n');
        }
        let dir = std::env::temp_dir().join(format!(
            "paneflow-731-{}-{}",
            std::process::id(),
            file_name.replace(['/', '.'], "_")
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(dir.join(file_name), base).expect("base file");
        let patch_path = dir.join("hunk.diff");
        std::fs::write(&patch_path, &patch).expect("patch");
        let output = std::process::Command::new("git")
            .args(["apply", "--check", "--whitespace=nowarn"])
            .arg(&patch_path)
            .current_dir(&dir)
            .output()
            .expect("git apply");
        assert!(
            output.status.success(),
            "git apply rejected the hunk ({}):\n{patch}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
