//! Unit tests for nested-embed profile selection (issue #554).
//!
//! The helper lives next to `build.rs` so the build script can `#[path]` it
//! without wiring a module through `src/main.rs`.

#[path = "../build/embed_staging.rs"]
mod embed_staging;

use std::path::Path;

use embed_staging::{
    cargo_profile_dir, embed_ingest_dir, embed_profile_for_outer, embed_slot_for_cfg,
    should_enforce_embed_size_limit,
};

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

#[test]
fn embed_slot_follows_the_debug_assertions_cfg() {
    assert_eq!(embed_slot_for_cfg(true), "debug");
    assert_eq!(embed_slot_for_cfg(false), "release");
    assert_ne!(embed_slot_for_cfg(true), embed_slot_for_cfg(false));
}

#[test]
fn release_outer_with_debug_assertions_stages_release_min_into_the_debug_slot() {
    // CARGO_PROFILE_RELEASE_DEBUG_ASSERTIONS=true keeps PROFILE=release but
    // compiles assets.rs with cfg(debug_assertions): the nested profile is
    // still release-min while the slot is the one rust-embed will read.
    assert_eq!(embed_profile_for_outer("release"), "release-min");
    assert_eq!(embed_slot_for_cfg(true), "debug");
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
