#![allow(
    clippy::panic,
    reason = "integration test setup failures need contextual diagnostics"
)]

//! Policy pins for `.github/workflows/release.yml` (issue #207).
//!
//! The release build job runs Cargo build scripts and the full test suite
//! on the same runner that later receives Apple signing/notarization and
//! Sparkle secrets. Two invariants keep the credential surface minimal and
//! must not silently regress:
//!
//! 1. No checkout in the release workflow persists the `GITHUB_TOKEN` into
//!    `.git/config` (`persist-credentials: false` on every
//!    `actions/checkout` step). Nothing in the build job performs an
//!    authenticated git operation; the one token consumer (the signed
//!    appcast step's `gh api` call) reads the token from a step-scoped
//!    `env:` instead.
//! 2. Every `permissions:` block in the workflow grants `contents` and
//!    nothing else, and the workflow-level default is `contents: read`.
//!    The build job keeps `contents: write` only because the
//!    `releases/generate-notes` endpoint is classified under "Contents"
//!    write for the `GITHUB_TOKEN`.
//!
//! A third pin covers the summariser sidecar (issue #586): the Xcode 16.4
//! pin stays, the sidecar alone compiles with a macOS 26 SDK, and the run
//! fails before bundling when the built sidecar cannot reach the model.

use std::path::{Path, PathBuf};

fn release_workflow_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../.github/workflows/release.yml")
}

fn release_workflow() -> String {
    let path = release_workflow_path();
    std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()))
}

fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

#[test]
fn every_release_checkout_disables_credential_persistence() {
    let workflow = release_workflow();
    let lines: Vec<&str> = workflow.lines().collect();
    let mut checkouts = 0usize;

    for (index, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        if !trimmed.starts_with("- uses: actions/checkout")
            && !trimmed.starts_with("uses: actions/checkout")
        {
            continue;
        }
        checkouts += 1;

        // The indentation of the `- ` marker that opens this step. When the
        // `uses:` key is not itself the marker line, walk back to the
        // enclosing marker.
        let marker_indent = if trimmed.starts_with("- ") {
            indent_of(line)
        } else {
            lines[..index]
                .iter()
                .rev()
                .find(|previous| {
                    previous.trim_start().starts_with("- ") && indent_of(previous) < indent_of(line)
                })
                .map(|previous| indent_of(previous))
                .unwrap_or_else(|| {
                    panic!(
                        "release.yml line {}: checkout `uses:` has no enclosing `- ` step marker",
                        index + 1
                    )
                })
        };

        // The step block runs until the next sibling step or a dedent.
        let has_persist_credentials_false = lines[index + 1..]
            .iter()
            .take_while(|next| next.trim().is_empty() || indent_of(next) > marker_indent)
            .any(|next| next.trim() == "persist-credentials: false");

        assert!(
            has_persist_credentials_false,
            "release.yml line {}: this actions/checkout step must set \
             `persist-credentials: false` so the GITHUB_TOKEN is not written into \
             .git/config while Cargo build scripts and tests run on the release \
             runner (issue #207)",
            index + 1
        );
    }

    assert!(
        checkouts >= 1,
        "release.yml no longer contains an actions/checkout step; update this policy test"
    );
}

#[test]
fn release_permissions_grant_contents_only_with_read_default() {
    let workflow = release_workflow();
    let lines: Vec<&str> = workflow.lines().collect();
    let mut top_level_blocks = 0usize;
    let mut blocks = 0usize;

    for (index, line) in lines.iter().enumerate() {
        if line.trim() != "permissions:" || line.trim_start().starts_with('#') {
            continue;
        }
        blocks += 1;
        let block_indent = indent_of(line);

        let entries: Vec<&str> = lines[index + 1..]
            .iter()
            .take_while(|next| {
                next.trim().is_empty()
                    || next.trim_start().starts_with('#')
                    || indent_of(next) > block_indent
            })
            .map(|next| next.trim())
            .filter(|entry| !entry.is_empty() && !entry.starts_with('#'))
            .collect();

        assert!(
            !entries.is_empty(),
            "release.yml line {}: empty permissions block",
            index + 1
        );
        for entry in &entries {
            assert!(
                *entry == "contents: read" || *entry == "contents: write",
                "release.yml line {}: permissions block grants `{entry}`; only \
                 `contents: read` or `contents: write` is allowed in the release \
                 workflow (issue #207)",
                index + 1
            );
        }

        if block_indent == 0 {
            top_level_blocks += 1;
            assert_eq!(
                entries,
                ["contents: read"],
                "release.yml line {}: the workflow-level permissions default must \
                 be exactly `contents: read` (issue #207)",
                index + 1
            );
        }
    }

    assert!(blocks >= 1, "release.yml declares no permissions blocks");
    assert_eq!(
        top_level_blocks, 1,
        "release.yml must declare exactly one workflow-level \
         `permissions: contents: read` default"
    );
}

/// Byte offset of `needle` in `workflow`, which must occur exactly once
/// outside a comment line.
fn offset_of_single(workflow: &str, needle: &str) -> usize {
    let hits: Vec<usize> = workflow
        .match_indices(needle)
        .map(|(offset, _)| offset)
        .filter(|offset| {
            let line_start = workflow[..*offset].rfind('\n').map_or(0, |at| at + 1);
            !workflow[line_start..].trim_start().starts_with('#')
        })
        .collect();
    assert_eq!(
        hits.len(),
        1,
        "release.yml must contain `{needle}` exactly once outside comments, found {}",
        hits.len()
    );
    hits[0]
}

#[test]
fn the_sidecar_gets_a_macos_26_sdk_and_is_verified_before_bundling() {
    let workflow = release_workflow();

    // The pin the Metal / GPUI build depends on is not what #586 changes.
    offset_of_single(
        &workflow,
        "DEVELOPER_DIR: /Applications/Xcode_16.4.app/Contents/Developer",
    );

    let select = offset_of_single(&workflow, "bash scripts/select-summarizer-sdk.sh");
    let export = offset_of_single(
        &workflow,
        "echo \"PANEFLOW_SUMMARIZER_DEVELOPER_DIR=$sidecar_developer_dir\" >> \"$GITHUB_ENV\"",
    );
    let build = offset_of_single(&workflow, "cargo build --release -p paneflow-app");
    let verify = offset_of_single(&workflow, "bash scripts/verify-summarizer-sidecar.sh");
    let bundle = offset_of_single(&workflow, "bash scripts/bundle-macos.sh");

    assert!(
        select < export && export < build,
        "release.yml must export PANEFLOW_SUMMARIZER_DEVELOPER_DIR before the release \
         build, or src-app/build.rs compiles the sidecar against the Xcode 16.4 SDK, \
         which has no FoundationModels (issue #586)"
    );
    assert!(
        build < verify && verify < bundle,
        "release.yml must run scripts/verify-summarizer-sidecar.sh between the release \
         build and bundle-macos.sh, so a sidecar without FoundationModels is never \
         signed and shipped (issue #586)"
    );

    // The override name is a contract between the workflow and the build
    // script; a rename on one side silently restores the old SDK.
    let build_script = Path::new(env!("CARGO_MANIFEST_DIR")).join("build.rs");
    let build_script = std::fs::read_to_string(&build_script)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", build_script.display()));
    assert!(
        build_script.contains("\"PANEFLOW_SUMMARIZER_DEVELOPER_DIR\""),
        "src-app/build.rs no longer reads PANEFLOW_SUMMARIZER_DEVELOPER_DIR (issue #586)"
    );
}
