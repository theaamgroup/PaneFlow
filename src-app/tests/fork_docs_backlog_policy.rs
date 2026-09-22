#![allow(
    clippy::panic,
    reason = "integration test setup failures need contextual diagnostics"
)]

//! GitHub issues remain the work tracker; the living handoff describes landed work.
//! Historical audit documents have been removed from the current tree.

use std::path::{Path, PathBuf};

const STATE_DOC: &str = "docs/fork/STATE.md";

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
    let names_archive = state.contains("archived");
    let names_tracker = state.contains("gh issue list");
    assert!(
        names_archive && names_tracker,
        "{STATE_DOC} must distinguish archived history and `gh issue list` as the tracker \
         (says archived: {names_archive}, names tracker: {names_tracker})"
    );
}
