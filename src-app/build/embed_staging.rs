//! Nested-embed profile selection for `src-app/build.rs` (issue #554).
//!
//! A build without `cfg(debug_assertions)` (release, `release-min`) keeps
//! fat-LTO `release-min` so `EMBED_SIZE_LIMIT_BYTES` still measures shipped
//! Mach-O sizes. A build with it (dev, test) uses `dev` so a debug
//! `cargo build` does not fat-LTO the shim and hook binaries.
//!
//! Staged bytes are isolated per rust-embed ingest slot (`debug` vs
//! `release`) so a debug restage cannot overwrite the helpers a later
//! `--release` compile bakes in. `assets::Bins` reads
//! `target/embed/<slot>/bin` under the matching `cfg(debug_assertions)`,
//! and the build script picks the slot from that same cfg.

use std::io;
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
/// stripped or LTO'd (see `EMBED_SIZE_LIMIT_DEBUG_BYTES` in `build.rs` for
/// the measured sizes) and every launch writes one shim copy per agent, so
/// they get a looser but still bounded cap instead of none.
pub fn embed_size_limit_for(embed_profile: &str, release_limit: u64, debug_limit: u64) -> u64 {
    if embed_profile == "release-min" {
        release_limit
    } else {
        debug_limit
    }
}

/// Remove every top-level entry of `embed_dir` whose name is not in `keep`,
/// returning the removed names, sorted.
///
/// rust-embed ingests every file in the staging folder and the size budget
/// sums every top-level file, but a restage only overwrites the helpers it
/// builds. Without this, a helper an older build staged (the retired
/// `paneflow-mcp`, issue #857) stays embedded forever and counts against the
/// cap. A missing folder is nothing to prune.
pub fn prune_unlisted_helpers(embed_dir: &Path, keep: &[&str]) -> io::Result<Vec<String>> {
    let entries = match std::fs::read_dir(embed_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut removed = Vec::new();
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if keep.contains(&name.as_str()) {
            continue;
        }
        if entry.file_type()?.is_dir() {
            std::fs::remove_dir_all(entry.path())?;
        } else {
            std::fs::remove_file(entry.path())?;
        }
        removed.push(name);
    }
    removed.sort();
    Ok(removed)
}
