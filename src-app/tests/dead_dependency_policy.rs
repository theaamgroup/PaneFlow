#![allow(
    clippy::panic,
    reason = "integration test setup failures need contextual diagnostics"
)]

//! Guards against dead direct dependencies quietly returning (issue #212).
//!
//! `rfd`, `syntect`, and `tree-sitter-language` were declared in
//! `src-app/Cargo.toml` with zero Rust call sites. `syntect` alone pulled
//! `plist` (and, through it, an ignored-vulnerable `quick-xml`) into the
//! graph. The app highlights with Tree-sitter and opens files through GPUI's
//! `cx.prompt_for_paths`, so none of the three has a job. This test reads the
//! lockfile and the app manifest off disk, the way
//! `dependency_source_policy.rs` does, so a reintroduction fails the suite
//! instead of silently inflating the binary.

use std::path::Path;

fn read_repo_file(relative: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(relative);
    let contents = std::fs::read_to_string(&path);
    assert!(
        contents.is_ok(),
        "failed to read {}: {:?}",
        path.display(),
        contents.as_ref().err()
    );
    contents.unwrap_or_default()
}

/// Package names resolved in `Cargo.lock`, one per `name = "..."` line.
fn locked_package_names(lock: &str) -> Vec<&str> {
    lock.lines()
        .filter_map(|line| line.strip_prefix("name = \""))
        .filter_map(|rest| rest.strip_suffix('"'))
        .collect()
}

/// Bare dependency keys declared in a manifest: the identifier before the
/// first `=` on a non-comment line, so `rfd = "0.17"` and
/// `syntect = { version = "5", ... }` both yield their crate name, while a
/// `# rfd ...` comment or a `[dependencies]` header yields nothing.
fn declared_dependency_keys(manifest: &str) -> Vec<&str> {
    manifest
        .lines()
        .map(str::trim_start)
        .filter(|line| !line.starts_with('#') && !line.starts_with('['))
        .filter_map(|line| line.split_once('=').map(|(key, _)| key.trim()))
        .filter(|key| !key.is_empty())
        .collect()
}

#[test]
fn dead_dependencies_stay_out_of_the_lockfile() {
    let lock = read_repo_file("../Cargo.lock");
    let names = locked_package_names(&lock);
    assert!(
        names.len() > 100,
        "Cargo.lock parse produced only {} package names; the probe is broken",
        names.len()
    );

    for banned in ["rfd", "syntect", "plist"] {
        assert!(
            !names.contains(&banned),
            "`{banned}` is back in Cargo.lock; it had no call sites when it was \
             removed (issue #212) and `syntect -> plist -> quick-xml` carried an \
             ignored RustSec path. Remove the dependency that pulls it in."
        );
    }
}

#[test]
fn dead_dependencies_are_not_declared_by_the_app_manifest() {
    let manifest = read_repo_file("Cargo.toml");
    let keys = declared_dependency_keys(&manifest);
    assert!(
        keys.contains(&"gpui") && keys.contains(&"tree-sitter"),
        "manifest parse missed known dependencies; the probe is broken: {keys:?}"
    );

    for banned in ["rfd", "syntect", "tree-sitter-language"] {
        assert!(
            !keys.contains(&banned),
            "`{banned}` is declared directly in src-app/Cargo.toml again with no \
             Rust call site (issue #212). `tree-sitter-language` is reached \
             transitively through the grammar crates; `rfd` and `syntect` have \
             no job at all."
        );
    }
}
