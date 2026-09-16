// Build scripts idiomatically `panic!` on fatal errors - that is how
// Cargo surfaces build-time failures to the user. The workspace-wide
// `clippy::panic = "deny"` policy targets production runtime code, not
// build tooling; a `?`-returning `main() -> Result<…>` here would only
// produce worse error messages via `Termination`. Allow-listed at file
// level with this justification.
#![allow(clippy::panic)]

//! Build script for `paneflow-app`.
//!
//! Responsibilities:
//! 1. **US-008 / EP-001 - embedded binary staging.** Build the
//!    `paneflow-shim`, `paneflow-ai-hook` and `paneflow-mcp` workspace
//!    binaries for the current target triple and stage them into
//!    `src-app/target/embed/{debug,release}/bin/<target>/` so the `Bins`
//!    `RustEmbed` struct in `src-app/src/assets.rs` picks them up at
//!    compile time. A nested
//!    `cargo build` is used rather than relying on workspace build ordering
//!    because `paneflow-app` does not directly depend on any of those
//!    crates - without this step they would not be guaranteed to exist when
//!    `rust-embed` expands. `paneflow-mcp` (the MCP pane-context bridge) is
//!    embedded here so every package ships it with zero new CI step; it is
//!    extracted at launch to a stable path by
//!    `ai_hooks::extract::ensure_bridge_extracted` (see EP-001 US-003).
//!
//!    The nested build uses a **separate `--target-dir`**
//!    (`<workspace>/target/embed-build`) so it does not fight the outer
//!    cargo for the same target-dir lock. The cost is duplicated
//!    compilation of the shim + hook + bridge dependency closure; all three
//!    closures are tiny (serde_json, tempfile, interprocess) so the overhead
//!    is acceptable and far cheaper than designing a shared build graph.
//!
//!    Nested staging uses `--profile release-min` when the outer PROFILE
//!    is `release` or `release-min`, and a cheap `dev` profile otherwise,
//!    so an ordinary `cargo build` does not fat-LTO the helpers. Staged
//!    bytes land in `src-app/target/embed/{debug,release}/bin/<target>/`
//!    so a debug restage cannot overwrite the files a later `--release`
//!    rust-embed expansion bakes in. The size budget is enforced only
//!    against release-min artifacts: a debug outer build that embeds
//!    debug-profile helpers must not change what `--release` ships.
//!
//!    Size budget: total embedded bytes per target triple must stay
//!    ≤ the documented cap on `EMBED_SIZE_LIMIT_BYTES`. The check fails the
//!    outer build when exceeded rather than silently shipping a bloated
//!    `paneflow` binary.
//!
//!    Escape hatch: setting `PANEFLOW_SKIP_EMBED_BUILD=1` skips the nested
//!    build - useful in CI pre-stages that build the nested crates
//!    separately and pre-populate
//!    `target/embed/{debug,release}/bin/<target>/`, and for fast iteration
//!    on the main crate when the nested binaries have not changed. The
//!    staging dir must still be populated when the `Bins` `RustEmbed`
//!    macro expands - rust-embed 8.x panics on missing folders.

#[path = "build/embed_staging.rs"]
mod embed_staging;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use embed_staging::{
    cargo_profile_dir, embed_ingest_dir, embed_profile_for_cfg, embed_slot_for_cfg,
    should_enforce_embed_size_limit,
};

/// Hard cap on the total bytes staged under
/// `target/embed/{debug,release}/bin/<target>/`.
/// Enforced to keep the main PaneFlow binary slim.
///
/// Measured `release-min` sizes (aarch64-apple-darwin Mach-O, 2026-08-27):
///
/// ```text
///   paneflow-shim      472_368 B
///   paneflow-ai-hook   336_464 B
///   paneflow-mcp       403_008 B
///   ----------------------------
///   total            1_211_840 B
/// ```
///
/// Cap 1_400_000 B = total + 15.5% (headroom relative to the total);
/// slack 188_160 B = 13.4% of the cap. Two denominators, two readings:
/// name the one you mean when you re-baseline. The cap exists so a real
/// bloat regression fails the build, while strip/LTO jitter does not.
/// Release builds print the measured total as a `cargo:warning` so the
/// figures above can be checked against build output, not trusted.
/// Nested staging uses `--profile release-min` only for a release (or
/// release-min) outer build; the cap is enforced against those artifacts
/// and is skipped when the staged profile is `dev`.
const EMBED_SIZE_LIMIT_BYTES: u64 = 1_400_000;
const EMBED_BINARIES: [&str; 3] = ["paneflow-shim", "paneflow-ai-hook", "paneflow-mcp"];

fn main() {
    println!("cargo:rerun-if-env-changed=PANEFLOW_SKIP_EMBED_BUILD");
    // PROFILE only gates the release size warning; the staging profile and
    // ingest slot both follow cfg(debug_assertions), the signal
    // `assets::Bins` compiles against, so an assertion override in either
    // direction restages the slot rust-embed will actually read.
    println!("cargo:rerun-if-env-changed=PROFILE");
    println!("cargo:rerun-if-env-changed=CARGO_CFG_DEBUG_ASSERTIONS");

    // 1. One engine, one archive (#184): libghostty-vt is vendored for
    //    aarch64-apple-darwin only, so any other target has nothing to link.
    assert_ghostty_target_is_supported();

    // 2. US-008 - stage the AI-hook binaries into a dir that
    //    `assets::Bins` (rust-embed) will ingest.
    let target = std::env::var("TARGET").expect("cargo always sets TARGET for build scripts");
    // Expose the triple to source code via `env!("PANEFLOW_TARGET_TRIPLE")`
    // so `ai_hooks::extract` can locate the correct sub-folder under
    // `bin/<triple>/` at runtime without re-deriving it from `std::env::consts`.
    println!("cargo:rustc-env=PANEFLOW_TARGET_TRIPLE={target}");

    let manifest_dir = PathBuf::from(
        std::env::var("CARGO_MANIFEST_DIR")
            .expect("cargo always sets CARGO_MANIFEST_DIR for build scripts"),
    );
    let workspace_root = manifest_dir
        .parent()
        .expect("src-app manifest dir has a parent (the workspace root)")
        .to_path_buf();

    // Both the nested helper profile and the ingest slot follow
    // cfg(debug_assertions), the signal `assets::Bins` compiles against, so
    // the release slot only ever holds release-min bytes and the debug slot
    // only ever holds dev bytes, whatever PROFILE says.
    let debug_assertions = std::env::var_os("CARGO_CFG_DEBUG_ASSERTIONS").is_some();
    let embed_profile = embed_profile_for_cfg(debug_assertions);
    let embed_slot = embed_slot_for_cfg(debug_assertions);

    // Create both ingest slots so rust-analyzer / cfg-checking of the
    // unused `assets::Bins` arm does not panic on a missing folder.
    // Only the current slot is populated below.
    for slot in ["debug", "release"] {
        let dir = embed_ingest_dir(&manifest_dir, &target, slot);
        fs::create_dir_all(&dir).unwrap_or_else(|e| {
            panic!(
                "US-008: cannot create embed staging dir {}: {e}",
                dir.display()
            )
        });
    }

    // The folder `RustEmbed` points at, relative to CARGO_MANIFEST_DIR.
    // Keep the in-memory/on-disk folder layout aligned with the macro.
    let embed_dir = embed_ingest_dir(&manifest_dir, &target, embed_slot);

    // Rerun when a helper crate's sources or manifest change. Watching
    // `src/` + `Cargo.toml` (not the crate directory) avoids a fat-LTO
    // restage on test-only or asset-dir mtime noise; the two plugin
    // assets below keep their explicit per-file watches.
    for helper in [
        "crates/paneflow-shim",
        "crates/paneflow-ai-hook",
        "crates/paneflow-mcp",
    ] {
        println!(
            "cargo:rerun-if-changed={}",
            workspace_root.join(helper).join("src").display()
        );
        println!(
            "cargo:rerun-if-changed={}",
            workspace_root.join(helper).join("Cargo.toml").display()
        );
    }
    // Also rerun if the root manifest changes (workspace-wide lint policy,
    // dep version bumps, etc., affect the staged binaries).
    println!(
        "cargo:rerun-if-changed={}",
        workspace_root.join("Cargo.toml").display()
    );
    // Explicit per-FILE watches for the shim's `include_str!`'d plugin assets.
    // A directory `rerun-if-changed` only catches add/remove/rename (the dir
    // mtime), NOT a content edit of a nested file on Windows - so without these
    // an edited `*-paneflow-status.ts` would silently not be re-embedded.
    for asset in [
        "crates/paneflow-shim/assets/opencode-paneflow-status.ts",
        "crates/paneflow-shim/assets/pi-paneflow-status.ts",
    ] {
        println!(
            "cargo:rerun-if-changed={}",
            workspace_root.join(asset).display()
        );
    }

    let skip_nested_build = matches!(
        std::env::var("PANEFLOW_SKIP_EMBED_BUILD").ok().as_deref(),
        Some("1")
    );
    if !skip_nested_build {
        stage_ai_hook_binaries(&workspace_root, &target, &embed_dir, embed_profile);
    } else {
        println!(
            "cargo:warning=PANEFLOW_SKIP_EMBED_BUILD=1 - validating pre-populated helpers in {}",
            embed_dir.display()
        );
    }

    // Required helpers must exist so rust-embed 8.x does not panic on an
    // empty folder. The byte cap is release-min only: debug-profile
    // helpers are larger and must not fail a debug outer build or rewrite
    // the shipped `--release` budget.
    enforce_embed_size_budget(&embed_dir, should_enforce_embed_size_limit(embed_profile));
}

/// Invoke a child `cargo build` against the workspace to produce the
/// `paneflow-shim`, `paneflow-ai-hook` and `paneflow-mcp` binaries for
/// `target`, then copy them into `embed_dir`. Panics (fails the outer
/// build) on any non-success exit, non-existent artifact, or IO error.
fn stage_ai_hook_binaries(workspace_root: &Path, target: &str, embed_dir: &Path, profile: &str) {
    // Use a dedicated `--target-dir` so we do not fight the outer cargo
    // for `target/debug/.cargo-lock` or `target/release/.cargo-lock`.
    // `embed-build` is a sibling of the outer `target/<profile>/` tree.
    let nested_target_dir = workspace_root.join("target").join("embed-build");

    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());

    // Run the nested cargo from the workspace root so `-p <crate>` is
    // resolved unambiguously and the workspace's `[patch.crates-io]`
    // block is honored.
    let mut cmd = Command::new(&cargo);
    cmd.current_dir(workspace_root)
        .arg("build")
        .arg("--profile")
        .arg(profile)
        .arg("--target")
        .arg(target)
        .arg("--target-dir")
        .arg(&nested_target_dir)
        .arg("-p")
        .arg("paneflow-shim")
        .arg("-p")
        .arg("paneflow-ai-hook")
        .arg("-p")
        .arg("paneflow-mcp")
        // Prevent the nested cargo from inheriting the outer cargo's
        // target-dir via `CARGO_TARGET_DIR` - the explicit `--target-dir`
        // above already pins it, but removing the env avoids confusion if
        // the parent environment sets it.
        .env_remove("CARGO_TARGET_DIR")
        // `RUSTFLAGS` changes (e.g. `-C link-arg=...` from sccache setups)
        // would invalidate the nested cache on every outer build. Leave
        // them alone; Cargo deals with that via its own fingerprinting.
        ;

    let status = cmd
        .status()
        .unwrap_or_else(|e| panic!("US-008: failed to spawn nested cargo build: {e}"));
    if !status.success() {
        panic!(
            "US-008: nested `cargo build --profile {profile} -p paneflow-shim -p paneflow-ai-hook -p paneflow-mcp --target {target}` \
             failed with {status}. Re-run the outer build with verbose logging to see the child cargo output."
        );
    }

    // Cargo lays artifacts out at
    // `<target-dir>/<triple>/<profile-dir>/<binary>`.
    // `dev` writes under `debug/`; custom profiles use the profile name
    // (`release-min` → `release-min`).
    let artifact_dir = nested_target_dir
        .join(target)
        .join(cargo_profile_dir(profile));

    // Copy only the three binaries we need; anything else in
    // `artifact_dir` is a transitive build product we don't want to embed.
    for bin in EMBED_BINARIES {
        let src = artifact_dir.join(bin);
        let dst = embed_dir.join(bin);

        if !src.exists() {
            panic!(
                "US-008: expected nested build artifact {} is missing - \
                 did the child cargo build silently skip this binary?",
                src.display()
            );
        }
        // `fs::copy` preserves mode on Unix; embedded bytes don't need
        // the executable bit (the extractor sets it), but a 0o755 here
        // keeps `ls -l target/embed/<slot>/bin/<triple>/` self-documenting.
        fs::copy(&src, &dst).unwrap_or_else(|e| {
            panic!(
                "US-008: copy {} → {} failed: {e}",
                src.display(),
                dst.display()
            )
        });
    }
}

/// Enforce the `EMBED_SIZE_LIMIT_BYTES` total embedded-bytes cap.
/// Inspects only top-level files in `embed_dir` - there are no subdirs
/// in the per-target staging layout so a recursive walk is not warranted.
fn enforce_embed_size_budget(embed_dir: &Path, enforce_size_limit: bool) {
    let mut total: u64 = 0;
    let mut per_file: BTreeMap<String, u64> = BTreeMap::new();
    let iter = match fs::read_dir(embed_dir) {
        Ok(iter) => iter,
        Err(e) => panic!(
            "US-008: cannot read embed staging dir {}: {e}",
            embed_dir.display()
        ),
    };
    for entry in iter {
        let entry = entry
            .unwrap_or_else(|e| panic!("US-008: broken embed dir entry in {embed_dir:?}: {e}"));
        let metadata = entry
            .metadata()
            .unwrap_or_else(|e| panic!("US-008: cannot stat {}: {e}", entry.path().display()));
        if metadata.is_file() {
            let size = metadata.len();
            total = total.saturating_add(size);
            per_file.insert(entry.file_name().to_string_lossy().into_owned(), size);
        }
    }

    for binary in EMBED_BINARIES {
        match per_file.get(binary) {
            Some(size) if *size > 0 => {}
            Some(_) => panic!(
                "US-008/EP-001: required embedded helper {} is empty",
                embed_dir.join(binary).display()
            ),
            None => panic!(
                "US-008/EP-001: required embedded helper {} is missing",
                embed_dir.join(binary).display()
            ),
        }
    }

    // Release builds only: surface the measured total so the figures in the
    // `EMBED_SIZE_LIMIT_BYTES` doc comment can be checked against build
    // output. Debug builds and clippy stay at their known warning count.
    if std::env::var("PROFILE").as_deref() == Ok("release") {
        println!(
            "cargo:warning=embedded helpers total {total} B of the {EMBED_SIZE_LIMIT_BYTES} B cap"
        );
    }

    if enforce_size_limit && total > EMBED_SIZE_LIMIT_BYTES {
        let mut details = String::new();
        for (name, size) in &per_file {
            details.push_str(&format!("  {name}: {size} bytes\n"));
        }
        panic!(
            "US-008/EP-001: embedded binaries exceed the {EMBED_SIZE_LIMIT_BYTES}-byte cap ({total} bytes).\n\
             Staging dir: {}\n\
             Per-file:\n{details}\
             Shrink shim/ai-hook/paneflow-mcp via smaller deps or a tighter release-min profile, \
             or raise EMBED_SIZE_LIMIT_BYTES with a fresh measurement note.",
            embed_dir.display()
        );
    }
}

/// libghostty-vt is the only terminal engine and its archive is vendored for
/// exactly one target (`native/libghostty/prebuilt/aarch64-apple-darwin`).
/// Fail the build outright anywhere else instead of producing a binary whose
/// every pane is dead. (#184)
fn assert_ghostty_target_is_supported() {
    let cfg = |key: &str| std::env::var(key).unwrap_or_default();
    let supported =
        cfg("CARGO_CFG_TARGET_OS") == "macos" && cfg("CARGO_CFG_TARGET_ARCH") == "aarch64";
    assert!(
        supported,
        "paneflow-app has no libghostty archive for target {}: Ghostty is the only terminal engine, so this target cannot be built (see native/libghostty/README.md)",
        std::env::var("TARGET").unwrap_or_default()
    );
}
