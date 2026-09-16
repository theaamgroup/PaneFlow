//! Nested-embed profile selection for `src-app/build.rs` (issue #554).
//!
//! Release (and the dedicated `release-min` profile) keep fat-LTO
//! `release-min` so `EMBED_SIZE_LIMIT_BYTES` still measures shipped Mach-O
//! sizes. Every other outer profile uses `dev` so a debug `cargo build`
//! does not fat-LTO the shim, hook, and MCP binaries.

/// Nested cargo profile used to stage embedded helpers for an outer `PROFILE`.
pub fn embed_profile_for_outer(profile: &str) -> &'static str {
    match profile {
        "release" | "release-min" => "release-min",
        _ => "dev",
    }
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
