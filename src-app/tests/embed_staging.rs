//! Unit tests for nested-embed profile selection (issue #554).
//!
//! The helper lives next to `build.rs` so the build script can `#[path]` it
//! without wiring a module through `src/main.rs`.

#[path = "../build/embed_staging.rs"]
mod embed_staging;

use embed_staging::{cargo_profile_dir, embed_profile_for_outer, should_enforce_embed_size_limit};

#[test]
fn release_outer_stages_release_min() {
    assert_eq!(embed_profile_for_outer("release"), "release-min");
    assert_eq!(embed_profile_for_outer("release-min"), "release-min");
}

#[test]
fn debug_outer_stages_dev() {
    assert_eq!(embed_profile_for_outer("debug"), "dev");
    assert_eq!(embed_profile_for_outer("dev"), "dev");
    assert_eq!(embed_profile_for_outer(""), "dev");
}

#[test]
fn size_cap_only_on_release_min() {
    assert!(should_enforce_embed_size_limit("release-min"));
    assert!(!should_enforce_embed_size_limit("dev"));
    assert!(!should_enforce_embed_size_limit("debug"));
    assert!(!should_enforce_embed_size_limit("release"));
}

#[test]
fn dev_profile_artifacts_live_under_debug() {
    assert_eq!(cargo_profile_dir("dev"), "debug");
    assert_eq!(cargo_profile_dir("test"), "debug");
    assert_eq!(cargo_profile_dir("release-min"), "release-min");
    assert_eq!(cargo_profile_dir("release"), "release");
    assert_eq!(cargo_profile_dir("bench"), "release");
}
