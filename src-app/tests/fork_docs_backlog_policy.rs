#![allow(
    clippy::panic,
    reason = "integration test setup failures need contextual diagnostics"
)]

//! Policy pins for the fork docs under `docs/fork/` (issue #224).
//!
//! `CLAUDE.md` makes GitHub issues the only tracker: no Markdown file may
//! function as a backlog. The 2026-08-28 deep-review findings document once
//! did - it told readers "Everything else below is open" and "Work through
//! the fixes in fixes.md in order", and `STATE.md` called it authoritative
//! ("read it before planning a pass"). Both roles are retired: the document
//! is an archived historical record, and `STATE.md` points at `gh issue
//! list`. These pins fail if either role silently returns. Like the schema
//! drift tests, they read the documents off disk.

use std::path::{Path, PathBuf};

const FINDINGS_DOC: &str = "docs/fork/2026-08-28-deep-review-findings.md";
const STATE_DOC: &str = "docs/fork/STATE.md";
const ARCHIVED_MARKER: &str = "> **Archived (historical record)";

fn fork_doc_path(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join(relative)
}

fn fork_doc(relative: &str) -> String {
    let path = fork_doc_path(relative);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()))
}

#[test]
fn deep_review_findings_doc_is_archived_not_a_work_queue() {
    let doc = fork_doc(FINDINGS_DOC);
    let first_line = doc
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or_else(|| panic!("{FINDINGS_DOC} is empty"));
    assert!(
        first_line.starts_with(ARCHIVED_MARKER),
        "{FINDINGS_DOC} must open with the `{ARCHIVED_MARKER}` block; first line is {first_line:?}"
    );
    for backlog_phrase in [
        "Everything else below is open",
        "How to use this file",
        "fixes.md",
    ] {
        assert!(
            !doc.contains(backlog_phrase),
            "{FINDINGS_DOC} still carries the backlog instruction {backlog_phrase:?}"
        );
    }
}

#[test]
fn state_doc_points_at_github_issues_not_the_findings_doc() {
    let state = fork_doc(STATE_DOC);
    assert!(
        !state.contains("read it before planning a"),
        "{STATE_DOC} still tells readers to plan a pass from the findings document"
    );
    assert!(
        !state.contains("remain open are in"),
        "{STATE_DOC} still quotes an open-item count for the findings document"
    );
    let mentions_findings = state.contains("2026-08-28-deep-review-findings.md");
    let names_archive = state.contains("archived");
    let names_tracker = state.contains("gh issue list");
    assert!(
        mentions_findings && names_archive && names_tracker,
        "{STATE_DOC} must cite the findings document as archived history and `gh issue list` as the tracker \
         (mentions findings: {mentions_findings}, says archived: {names_archive}, names tracker: {names_tracker})"
    );
}
