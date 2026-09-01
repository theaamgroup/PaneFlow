#![allow(
    clippy::panic,
    reason = "integration test setup failures need contextual diagnostics"
)]

//! Policy pin for `.github/workflows/*.yml` (issue #208).
//!
//! Every third-party action referenced by a `uses:` line must be pinned to
//! an immutable 40-hex commit SHA, not a mutable version tag. A tag like
//! `@v4` can be force-moved by the action's owner (or an attacker who
//! compromises the owner's account) to point at arbitrary code that then
//! runs with this repository's CI credentials. This is the same
//! immutability policy `src-app/tests/dependency_source_policy.rs` enforces
//! for git dependencies in `Cargo.lock`.
//!
//! Convention: `owner/repo@<40-hex-sha> # <tag>` so the human-readable
//! version survives as a comment. Locally-defined actions (`./path`) are
//! exempt; none exist today. cargo-deny is installed via
//! `cargo install --version '^0.19'` (a deliberate, documented band in
//! run_tests.yml), not via a `uses:` line, so it is outside this policy.

use std::path::{Path, PathBuf};

fn workflows_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../.github/workflows")
}

fn workflow_files() -> Vec<PathBuf> {
    let dir = workflows_dir();
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", dir.display()))
        .map(|entry| {
            entry
                .unwrap_or_else(|error| panic!("failed to enumerate {}: {error}", dir.display()))
                .path()
        })
        .filter(|path| {
            matches!(
                path.extension().and_then(|ext| ext.to_str()),
                Some("yml" | "yaml")
            )
        })
        .collect();
    files.sort();
    files
}

fn is_forty_hex(reference: &str) -> bool {
    reference.len() == 40 && reference.chars().all(|c| c.is_ascii_hexdigit())
}

#[test]
fn every_third_party_action_is_pinned_to_a_commit_sha() {
    let files = workflow_files();
    assert!(
        !files.is_empty(),
        "{} contains no workflow files; update this policy test",
        workflows_dir().display()
    );

    let mut scanned_uses = 0usize;
    let mut violations: Vec<String> = Vec::new();

    for path in &files {
        let contents = std::fs::read_to_string(path)
            .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()));
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_else(|| panic!("non-UTF-8 workflow file name under {}", path.display()));

        for (index, line) in contents.lines().enumerate() {
            let trimmed = line.trim_start();
            if trimmed.starts_with('#') {
                continue;
            }
            let Some(reference) = trimmed
                .strip_prefix("- uses:")
                .or_else(|| trimmed.strip_prefix("uses:"))
            else {
                continue;
            };
            // Drop a trailing `# comment` and surrounding quotes/whitespace.
            let reference = reference
                .split('#')
                .next()
                .expect("split always yields at least one element")
                .trim()
                .trim_matches(|c| c == '"' || c == '\'');
            if reference.is_empty() {
                continue;
            }
            scanned_uses += 1;

            // Locally-defined actions are not fetched from a remote and
            // carry no ref to pin.
            if reference.starts_with("./") {
                continue;
            }

            let site = format!("{file_name}:{}: uses: {reference}", index + 1);
            match reference.split_once('@') {
                Some((_, git_ref)) if is_forty_hex(git_ref) => {}
                Some(_) | None => violations.push(site),
            }
        }
    }

    assert!(
        scanned_uses >= 1,
        "no `uses:` entries matched under {}; the scan is broken, update this policy test",
        workflows_dir().display()
    );
    assert!(
        violations.is_empty(),
        "every third-party GitHub Action must be pinned to a 40-hex commit SHA \
         (`owner/repo@<sha> # <tag>`), because version tags are mutable and run \
         with this repository's CI credentials (issue #208). Unpinned sites:\n{}",
        violations.join("\n")
    );
}
