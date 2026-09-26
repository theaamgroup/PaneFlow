//! Unit tests for nested-embed profile selection (issue #554).
//!
//! The helper lives next to `build.rs` so the build script can `#[path]` it
//! without wiring a module through `src/main.rs`.

#[path = "../build/embed_staging.rs"]
mod embed_staging;

use std::path::Path;

use embed_staging::{
    cargo_profile_dir, embed_ingest_dir, embed_profile_for_cfg, embed_size_limit_for,
    embed_slot_for_cfg, prune_unlisted_helpers,
};

#[test]
fn a_build_without_debug_assertions_stages_release_min() {
    assert_eq!(embed_profile_for_cfg(false), "release-min");
}

#[test]
fn a_build_with_debug_assertions_stages_dev() {
    assert_eq!(embed_profile_for_cfg(true), "dev");
}

#[test]
fn size_cap_follows_the_staged_profile() {
    assert_eq!(
        embed_size_limit_for("release-min", 975_000, 7_000_000),
        975_000
    );
    assert_eq!(embed_size_limit_for("dev", 975_000, 7_000_000), 7_000_000);
    assert!(
        embed_size_limit_for("dev", 975_000, 7_000_000)
            > embed_size_limit_for("release-min", 975_000, 7_000_000)
    );
}

#[test]
fn dev_profile_artifacts_live_under_debug() {
    assert_eq!(cargo_profile_dir("dev"), "debug");
    assert_eq!(cargo_profile_dir("test"), "debug");
    assert_eq!(cargo_profile_dir("release-min"), "release-min");
    assert_eq!(cargo_profile_dir("release"), "release");
    assert_eq!(cargo_profile_dir("bench"), "release");
}

#[test]
fn embed_slot_follows_the_debug_assertions_cfg() {
    assert_eq!(embed_slot_for_cfg(true), "debug");
    assert_eq!(embed_slot_for_cfg(false), "release");
    assert_ne!(embed_slot_for_cfg(true), embed_slot_for_cfg(false));
}

#[test]
fn profile_and_slot_are_decided_by_one_signal() {
    // Whatever PROFILE says, a release slot only ever receives release-min
    // bytes and a debug slot only ever receives dev bytes, so neither
    // CARGO_PROFILE_RELEASE_DEBUG_ASSERTIONS=true nor
    // CARGO_PROFILE_DEV_DEBUG_ASSERTIONS=false can leave unmeasured helpers
    // in the slot a later build embeds.
    for debug_assertions in [true, false] {
        let pair = (
            embed_profile_for_cfg(debug_assertions),
            embed_slot_for_cfg(debug_assertions),
        );
        assert!(
            matches!(pair, ("dev", "debug") | ("release-min", "release")),
            "{pair:?}"
        );
    }
}

#[test]
fn debug_restage_cannot_clobber_release_ingest_dir() {
    let manifest = Path::new("/app");
    let target = "aarch64-apple-darwin";
    let debug_dir = embed_ingest_dir(manifest, target, "debug");
    let release_dir = embed_ingest_dir(manifest, target, "release");
    assert_ne!(debug_dir, release_dir);
    assert!(debug_dir.ends_with("target/embed/debug/bin/aarch64-apple-darwin"));
    assert!(release_dir.ends_with("target/embed/release/bin/aarch64-apple-darwin"));
}

#[test]
fn prune_removes_every_staged_entry_that_is_not_a_listed_helper() {
    // Issue #857: an older build staged the retired `paneflow-mcp` here;
    // rust-embed would keep embedding it and the budget would keep counting it.
    let dir = tempfile::tempdir().unwrap();
    let staged = dir.path().join("bin").join("aarch64-apple-darwin");
    std::fs::create_dir_all(staged.join("stray-dir")).unwrap();
    for name in [
        "paneflow-shim",
        "paneflow-ai-hook",
        "paneflow-mcp",
        ".DS_Store",
    ] {
        std::fs::write(staged.join(name), b"x").unwrap();
    }
    let keep = ["paneflow-shim", "paneflow-ai-hook"];

    let removed = prune_unlisted_helpers(&staged, &keep).unwrap();

    assert_eq!(removed, [".DS_Store", "paneflow-mcp", "stray-dir"]);
    let mut left: Vec<String> = std::fs::read_dir(&staged)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    left.sort();
    assert_eq!(left, ["paneflow-ai-hook", "paneflow-shim"]);
    assert!(prune_unlisted_helpers(&staged, &keep).unwrap().is_empty());
    assert!(
        prune_unlisted_helpers(&dir.path().join("missing"), &keep)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn prune_removes_a_symlink_as_a_link_and_leaves_its_target_alone() {
    // A stray link in the staging dir (to a file or a directory outside it)
    // is removed as a link: its target is never followed or deleted.
    let dir = tempfile::tempdir().unwrap();
    let staged = dir.path().join("staged");
    std::fs::create_dir_all(&staged).unwrap();
    std::fs::write(staged.join("paneflow-shim"), b"x").unwrap();
    let outside_file = dir.path().join("outside-paneflow-mcp");
    std::fs::write(&outside_file, b"keep me").unwrap();
    let outside_dir = dir.path().join("outside-dir");
    std::fs::create_dir_all(&outside_dir).unwrap();
    std::fs::write(outside_dir.join("inner"), b"keep me too").unwrap();
    std::os::unix::fs::symlink(&outside_file, staged.join("paneflow-mcp")).unwrap();
    std::os::unix::fs::symlink(&outside_dir, staged.join("linked-dir")).unwrap();

    let removed = prune_unlisted_helpers(&staged, &["paneflow-shim"]).unwrap();

    assert_eq!(removed, ["linked-dir", "paneflow-mcp"]);
    assert!(std::fs::symlink_metadata(staged.join("paneflow-mcp")).is_err());
    assert!(std::fs::symlink_metadata(staged.join("linked-dir")).is_err());
    assert_eq!(std::fs::read(&outside_file).unwrap(), b"keep me");
    assert_eq!(
        std::fs::read(outside_dir.join("inner")).unwrap(),
        b"keep me too"
    );
}
