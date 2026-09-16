//! Nested-embed profile selection for `src-app/build.rs` (issue #554).
//!
//! Release (and the dedicated `release-min` profile) keep fat-LTO
//! `release-min` so `EMBED_SIZE_LIMIT_BYTES` still measures shipped Mach-O
//! sizes. Every other outer profile uses `dev` so a debug `cargo build`
//! does not fat-LTO the shim, hook, and MCP binaries.
//!
//! Staged bytes are isolated per rust-embed ingest slot (`debug` vs
//! `release`) so a debug restage cannot overwrite the helpers a later
//! `--release` compile bakes in. `assets::Bins` reads
//! `target/embed/<slot>/bin` under the matching `cfg(debug_assertions)`.

use std::path::{Path, PathBuf};

/// Nested cargo profile used to stage embedded helpers for an outer `PROFILE`.
pub fn embed_profile_for_outer(profile: &str) -> &'static str {
    match profile {
        "release" | "release-min" => "release-min",
        _ => "dev",
    }
}

/// On-disk slot under `src-app/target/embed/<slot>/bin`.
///
/// `debug` matches `cfg(debug_assertions)` in `assets.rs`; `release`
/// matches `cfg(not(debug_assertions))`. Outer `release` / `release-min`
/// share the `release` slot because both stage `release-min` helpers.
pub fn embed_slot_for_outer(profile: &str) -> &'static str {
    match embed_profile_for_outer(profile) {
        "release-min" => "release",
        _ => "debug",
    }
}

/// `CARGO_MANIFEST_DIR/target/embed/<slot>/bin/<target>`.
///
/// rust-embed's `#[folder]` is `target/embed/<slot>/bin`; keys stay
/// `bin/<target>/<binary>` via `#[prefix = "bin/"]`.
pub fn embed_ingest_dir(manifest_dir: &Path, target: &str, outer_profile: &str) -> PathBuf {
    manifest_dir
        .join("target")
        .join("embed")
        .join(embed_slot_for_outer(outer_profile))
        .join("bin")
        .join(target)
}

/// Cargo's on-disk artifact directory for a profile.
///
/// The built-in `dev` / `test` profiles write under `debug/`; `bench` under
/// `release/`; custom profiles (including `release-min`) use the profile name.
pub fn cargo_profile_dir(profile: &str) -> &str {
    match profile {
        "dev" | "test" => "debug",
        "bench" => "release",
        other => other,
    }
}

/// Size cap applies only to `release-min` artifacts, never to debug helpers.
pub fn should_enforce_embed_size_limit(embed_profile: &str) -> bool {
    embed_profile == "release-min"
}
