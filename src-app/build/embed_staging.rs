//! Nested-embed profile selection for `src-app/build.rs` (issue #554).
//!
//! A build without `cfg(debug_assertions)` (release, `release-min`) keeps
//! fat-LTO `release-min` so `EMBED_SIZE_LIMIT_BYTES` still measures shipped
//! Mach-O sizes. A build with it (dev, test) uses `dev` so a debug
//! `cargo build` does not fat-LTO the shim, hook, and MCP binaries.
//!
//! Staged bytes are isolated per rust-embed ingest slot (`debug` vs
//! `release`) so a debug restage cannot overwrite the helpers a later
//! `--release` compile bakes in. `assets::Bins` reads
//! `target/embed/<slot>/bin` under the matching `cfg(debug_assertions)`,
//! and the build script picks the slot from that same cfg.

use std::path::{Path, PathBuf};

/// Nested cargo profile used to stage embedded helpers.
///
/// Decided by the same `cfg(debug_assertions)` signal as the ingest slot,
/// never by the outer `PROFILE`: a release slot only ever holds fat-LTO
/// `release-min` bytes that the shipped-size cap measured, and a debug
/// slot only ever holds `dev` bytes. Keying the profile off `PROFILE`
/// while the slot followed the cfg let `CARGO_PROFILE_DEV_DEBUG_ASSERTIONS=false`
/// write unoptimized helpers into the release slot for a later `--release`
/// to embed without restaging.
pub fn embed_profile_for_cfg(debug_assertions: bool) -> &'static str {
    if debug_assertions {
        "dev"
    } else {
        "release-min"
    }
}

/// On-disk slot under `src-app/target/embed/<slot>/bin`.
///
/// The slot is chosen from the same signal `assets.rs` compiles against:
/// `cfg(debug_assertions)` (`CARGO_CFG_DEBUG_ASSERTIONS` in the build
/// script) selects `debug`, its absence selects `release`. Keying off the
/// outer `PROFILE` instead would stage the `release` slot for
/// `CARGO_PROFILE_RELEASE_DEBUG_ASSERTIONS=true` while rust-embed ingests
/// the (empty) `debug` folder, shipping a binary with no helpers.
pub fn embed_slot_for_cfg(debug_assertions: bool) -> &'static str {
    if debug_assertions { "debug" } else { "release" }
}

/// `CARGO_MANIFEST_DIR/target/embed/<slot>/bin/<target>`.
///
/// rust-embed's `#[folder]` is `target/embed/<slot>/bin`; keys stay
/// `bin/<target>/<binary>` via `#[prefix = "bin/"]`.
pub fn embed_ingest_dir(manifest_dir: &Path, target: &str, slot: &str) -> PathBuf {
    manifest_dir
        .join("target")
        .join("embed")
        .join(slot)
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

/// Byte cap for the staged helpers of a nested profile.
///
/// `release-min` keeps the shipped-size budget. `dev` helpers are not
/// stripped or LTO'd (measured 2026-09-15 on aarch64-apple-darwin:
/// shim 2_973_616 B + ai-hook 1_353_680 B + mcp 2_494_224 B = 6_821_520 B)
/// and every launch writes one shim copy per agent, so they get a looser
/// but still bounded cap instead of none.
pub fn embed_size_limit_for(embed_profile: &str, release_limit: u64, debug_limit: u64) -> u64 {
    if embed_profile == "release-min" {
        release_limit
    } else {
        debug_limit
    }
}
