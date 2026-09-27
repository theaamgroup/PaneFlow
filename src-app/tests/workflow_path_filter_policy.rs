#![allow(
    clippy::panic,
    reason = "integration test setup failures need contextual diagnostics"
)]

//! Every file the test suite compiles in or reads off disk must select the
//! `cargo test` lane in `.github/workflows/run_tests.yml` (issue #1055).
//!
//! `macos_check` runs only when one of the `orchestrate` path filters it is
//! gated on matches, and `tests_pass` accepts a skipped lane. A pull request
//! that touches only a file outside those filters therefore lands green even
//! when a test that reads the file would fail. #700 and #873 fixed this by
//! hand for other inputs; `ARCHITECTURE.md`, which `app/actions.rs`
//! compiles in with `include_str!`, was still missing.
//!
//! This test derives the compile-time inputs from the source: every
//! `include_str!`/`include_bytes!` literal under `src-app/` and `crates/`,
//! resolved against its file. It also pins the runtime inputs that tests
//! read off disk. Each one must match a glob of a filter that gates
//! `macos_check`. The filters and the gate are both parsed from the
//! workflow, so dropping a filter entry or narrowing the gate fails here.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

const WORKFLOW: &str = ".github/workflows/run_tests.yml";
const GATED_JOB: &str = "macos_check";

/// Files tests open at run time (`std::fs::read_to_string` and friends),
/// which no `include_str!` scan can see. Keep in step with the comments on
/// the `rust` filter in run_tests.yml.
const RUNTIME_TEST_INPUTS: &[(&str, &str)] = &[
    ("Cargo.lock", "src-app/tests/dependency_source_policy.rs"),
    ("deny.toml", "src-app/tests/dependency_policy_drift.rs"),
    (
        "docs/fork/STATE.md",
        "src-app/tests/fork_docs_backlog_policy.rs",
    ),
    (
        "schemas/paneflow.schema.json",
        "crates/paneflow-config/src/schema.rs",
    ),
    (
        "docs/user/configuration/schema.md",
        "crates/paneflow-config/src/schema.rs",
    ),
    (
        ".github/workflows/release.yml",
        "src-app/tests/release_workflow_policy.rs",
    ),
    (
        "scripts/bundle-macos.sh",
        "src-app/tests/release_workflow_policy.rs",
    ),
    (
        "scripts/sign-macos.sh",
        "src-app/tests/release_workflow_policy.rs",
    ),
    (WORKFLOW, "src-app/tests/workflow_path_filter_policy.rs"),
];

fn repo_root() -> PathBuf {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    root.canonicalize()
        .unwrap_or_else(|error| panic!("failed to resolve {}: {error}", root.display()))
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()))
}

fn indent(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// The `filters: |` block of the paths-filter step, as filter name to globs.
fn path_filters(workflow: &str) -> BTreeMap<String, Vec<String>> {
    let lines: Vec<&str> = workflow.lines().collect();
    let start = lines
        .iter()
        .position(|line| line.trim() == "filters: |")
        .unwrap_or_else(|| panic!("{WORKFLOW} has no `filters: |` block; update this test"));
    let block_indent = indent(lines[start]);
    let block: Vec<&str> = lines[start + 1..]
        .iter()
        .copied()
        .take_while(|line| line.trim().is_empty() || indent(line) > block_indent)
        .filter(|line| !line.trim().is_empty() && !line.trim().starts_with('#'))
        .collect();
    let name_indent = block.iter().map(|line| indent(line)).min().unwrap_or(0);

    let mut filters: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut current: Option<String> = None;
    for line in block {
        let trimmed = line.trim();
        if indent(line) == name_indent {
            let name = trimmed
                .strip_suffix(':')
                .unwrap_or_else(|| panic!("unexpected filter line in {WORKFLOW}: {line:?}"));
            current = Some(name.to_owned());
            filters.entry(name.to_owned()).or_default();
        } else if let Some(entry) = trimmed.strip_prefix("- ") {
            let glob = entry.trim().trim_matches('\'').trim_matches('"').to_owned();
            let name = current
                .as_ref()
                .unwrap_or_else(|| panic!("filter entry before any filter name: {line:?}"));
            filters.entry(name.clone()).or_default().push(glob);
        } else {
            panic!("unexpected line in {WORKFLOW} filters: {line:?}; update this test");
        }
    }
    filters
}

/// The `orchestrate` outputs named in `macos_check`'s `if:` condition.
fn gating_outputs(workflow: &str) -> Vec<String> {
    let lines: Vec<&str> = workflow.lines().collect();
    let header = format!("  {GATED_JOB}:");
    let job_start = lines
        .iter()
        .position(|line| line.trim_end() == header)
        .unwrap_or_else(|| panic!("{WORKFLOW} has no `{GATED_JOB}` job; update this test"));
    let job: Vec<&str> = lines[job_start + 1..]
        .iter()
        .copied()
        .take_while(|line| line.trim().is_empty() || indent(line) > 2)
        .collect();
    let if_line = job
        .iter()
        .position(|line| line.trim_start().starts_with("if:"))
        .unwrap_or_else(|| panic!("`{GATED_JOB}` has no `if:` gate; update this test"));
    let if_indent = indent(job[if_line]);
    let condition: String = std::iter::once(job[if_line])
        .chain(
            job[if_line + 1..]
                .iter()
                .copied()
                .take_while(|line| indent(line) > if_indent),
        )
        .collect::<Vec<_>>()
        .join(" ");

    const PREFIX: &str = "needs.orchestrate.outputs.";
    let mut outputs = Vec::new();
    for (at, _) in condition.match_indices(PREFIX) {
        let rest = &condition[at + PREFIX.len()..];
        let name: String = rest
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        assert!(
            rest[name.len()..].trim_start().starts_with("== 'true'"),
            "`{GATED_JOB}` gate uses `{PREFIX}{name}` in a form this test cannot read: {condition}"
        );
        outputs.push(name);
    }
    outputs
}

/// dorny/paths-filter glob semantics for the forms run_tests.yml uses: `**`
/// spans any number of path segments, `*` any run within one segment.
fn glob_matches(pattern: &str, path: &str) -> bool {
    assert!(
        !pattern.contains(['?', '[', '{', '!']),
        "glob {pattern:?} uses syntax this test does not model; extend glob_matches"
    );
    let pattern: Vec<&str> = pattern.split('/').collect();
    let path: Vec<&str> = path.split('/').collect();
    segments_match(&pattern, &path)
}

fn segments_match(pattern: &[&str], path: &[&str]) -> bool {
    match pattern.split_first() {
        None => path.is_empty(),
        Some((&"**", rest)) => (0..=path.len()).any(|skip| segments_match(rest, &path[skip..])),
        Some((first, rest)) => path.split_first().is_some_and(|(segment, tail)| {
            segment_matches(first, segment) && segments_match(rest, tail)
        }),
    }
}

fn segment_matches(pattern: &str, segment: &str) -> bool {
    match pattern.split_once('*') {
        None => pattern == segment,
        Some((head, tail)) => {
            segment.starts_with(head)
                && (head.len()..=segment.len()).any(|at| segment_matches(tail, &segment[at..]))
        }
    }
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = std::fs::read_dir(dir)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", dir.display()));
    for entry in entries {
        let path = entry
            .unwrap_or_else(|error| panic!("failed to enumerate {}: {error}", dir.display()))
            .path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if path.is_dir() {
            if name != "target" && !name.starts_with('.') {
                rust_files(&path, out);
            }
        } else if name.ends_with(".rs") {
            out.push(path);
        }
    }
}

/// Lexically resolve `relative` against `base` and express it from `root`.
fn repo_relative(root: &Path, base: &Path, relative: &str) -> Option<String> {
    let mut resolved = PathBuf::new();
    for component in base.join(relative).components() {
        match component {
            Component::ParentDir => {
                resolved.pop();
            }
            Component::CurDir => {}
            other => resolved.push(other),
        }
    }
    let inside = resolved.strip_prefix(root).ok()?;
    Some(
        inside
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/"),
    )
}

/// Every file compiled in by `include_str!` / `include_bytes!`, as a
/// repo-relative path mapped to the first site that includes it.
fn compiled_inputs(root: &Path) -> BTreeMap<String, String> {
    let mut files = Vec::new();
    for dir in ["src-app", "crates"] {
        rust_files(&root.join(dir), &mut files);
    }
    files.sort();
    assert!(
        files.len() > 100,
        "found only {} Rust files; the walk is broken",
        files.len()
    );

    // Split so this file's own source does not match the scan.
    let macros = [concat!("include_", "str!("), concat!("include_", "bytes!(")];
    let mut inputs = BTreeMap::new();
    for file in files {
        let source = read(&file);
        let base = file.parent().unwrap_or(root);
        let site = file
            .strip_prefix(root)
            .unwrap_or(&file)
            .display()
            .to_string();
        for needle in macros {
            for (at, _) in source.match_indices(needle) {
                let line = source[..at].matches('\n').count() + 1;
                let line_start = source[..at].rfind('\n').map_or(0, |i| i + 1);
                if source[line_start..at].trim_start().starts_with("//") {
                    continue;
                }
                let argument = source[at + needle.len()..].trim_start();
                let literal = argument
                    .strip_prefix('"')
                    .and_then(|rest| rest.split_once('"'))
                    .map(|(literal, _)| literal)
                    .unwrap_or_else(|| {
                        panic!(
                            "{site}:{line}: non-literal {needle}…) argument; teach this test \
                             to resolve it so its input is still checked against the CI filters"
                        )
                    });
                let resolved = repo_relative(root, base, literal).unwrap_or_else(|| {
                    panic!("{site}:{line}: {literal:?} resolves outside the repository")
                });
                inputs
                    .entry(resolved)
                    .or_insert_with(|| format!("{site}:{line}"));
            }
        }
    }
    inputs
}

#[test]
fn every_test_input_selects_the_cargo_test_lane() {
    let root = repo_root();
    let workflow = read(&root.join(WORKFLOW));
    let filters = path_filters(&workflow);
    let gates = gating_outputs(&workflow);
    assert!(
        gates.iter().any(|gate| gate == "rust"),
        "`{GATED_JOB}` must be gated on the `rust` filter, got {gates:?}"
    );
    let globs: Vec<(&str, &str)> = gates
        .iter()
        .map(|gate| {
            let globs = filters.get(gate).unwrap_or_else(|| {
                panic!("`{GATED_JOB}` is gated on `{gate}`, which is not a path filter")
            });
            (gate.as_str(), globs)
        })
        .flat_map(|(gate, globs)| globs.iter().map(move |glob| (gate, glob.as_str())))
        .collect();

    let compiled = compiled_inputs(&root);
    // Negative control: a scan that finds nothing passes vacuously.
    for known in ["ARCHITECTURE.md", "CLAUDE.md", "docs/user/keybindings.md"] {
        assert!(
            compiled.contains_key(known),
            "the include scan no longer finds {known}; it is broken or the include moved"
        );
    }

    let mut inputs = compiled;
    for (path, reader) in RUNTIME_TEST_INPUTS {
        assert!(
            root.join(path).is_file(),
            "{path} (read by {reader}) no longer exists; update RUNTIME_TEST_INPUTS"
        );
        inputs
            .entry((*path).to_owned())
            .or_insert_with(|| (*reader).to_owned());
    }

    let unselected: Vec<String> = inputs
        .iter()
        .filter(|(path, _)| !globs.iter().any(|(_, glob)| glob_matches(glob, path)))
        .map(|(path, site)| format!("{path} (read by {site})"))
        .collect();
    assert!(
        unselected.is_empty(),
        "these test inputs select no filter that gates `{GATED_JOB}` ({gates:?}) in \
         {WORKFLOW}, so a PR touching only them skips cargo test; add them to the \
         `rust` filter:\n  {}",
        unselected.join("\n  ")
    );
}

#[test]
fn glob_model_matches_paths_filter_semantics() {
    assert!(glob_matches("src-app/**", "src-app/src/main.rs"));
    assert!(glob_matches("**/Cargo.toml", "Cargo.toml"));
    assert!(glob_matches("**/Cargo.toml", "crates/x/Cargo.toml"));
    assert!(glob_matches("CLAUDE.md", "CLAUDE.md"));
    assert!(glob_matches("docs/*.md", "docs/a.md"));
    assert!(!glob_matches("docs/*.md", "docs/sub/a.md"));
    assert!(!glob_matches("CLAUDE.md", "ARCHITECTURE.md"));
    assert!(!glob_matches("src-app/**", "src-application/x.rs"));
}
