//! Unit tests for nested-embed profile selection (issue #554).
//!
//! The helper lives next to `build.rs` so the build script can `#[path]` it
//! without wiring a module through `src/main.rs`.

#[path = "../build/embed_staging.rs"]
mod embed_staging;

use std::path::Path;

use embed_staging::{
    cargo_profile_dir, embed_ingest_dir, embed_profile_for_cfg, embed_size_limit_for,
    embed_slot_for_cfg,
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
        embed_size_limit_for("release-min", 1_400_000, 10_000_000),
        1_400_000
    );
    assert_eq!(
        embed_size_limit_for("dev", 1_400_000, 10_000_000),
        10_000_000
    );
    assert!(
        embed_size_limit_for("dev", 1_400_000, 10_000_000)
            > embed_size_limit_for("release-min", 1_400_000, 10_000_000)
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
