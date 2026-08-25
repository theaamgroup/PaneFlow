//! Runtime detection of how PaneFlow was installed.
//!
//! The in-app updater ships a single strategy on this macOS-only fork: replace
//! the `.app` bundle from a signed `.dmg`. Detection exists to tell that case
//! apart from a binary running outside a bundle (an ad-hoc copy, or a dev
//! build under `target/`), where in-app updates cannot work and must be
//! disabled with an accurate explanation rather than a never-firing prompt.
//!
//! Detection runs at startup. The caller canonicalises `current_exe()` before
//! classifying, so a symlinked launcher resolves to the real path inside the
//! bundle.
//!
//! Every public API in this module is consumed by the updater work in
//! US-009/010/011/012. Until those stories land, much of it is only
//! reachable through the unit tests - hence the crate-level dead-code
//! suppression.

#![allow(dead_code)]

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// How the running binary was installed on the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallMethod {
    /// macOS `.app` bundle layout (US-007) - the running binary lives at
    /// `<bundle_path>/Contents/MacOS/paneflow`, whether under
    /// `/Applications`, `$HOME/Applications`, or anywhere the user dragged
    /// the bundle. The updater pairs this with `AssetFormat::Dmg`
    /// (US-008) to download a matching `.dmg`.
    AppBundle { bundle_path: PathBuf },

    /// In-app updates are disabled by the host environment, signalled by
    /// `PANEFLOW_UPDATE_EXPLANATION` at build or run time. The pill renders
    /// a system-managed hint and clicking copies the explanation rather than
    /// attempting any download.
    ///
    /// This mirrors Zed's `ZED_UPDATE_EXPLANATION` convention: a packager
    /// bakes the variable into its wrapper or formula, and the in-app updater
    /// stays out of the way. It is driven purely by that variable, never by
    /// platform, so it remains reachable here - a Homebrew cask is exactly
    /// the case it exists for.
    ExternallyManaged { explanation: String },

    /// Binary location doesn't match any known layout (legacy `.run` install,
    /// manual copy, dev build). Updater disables in-app updates.
    Unknown,
}

/// Probe the filesystem and environment to classify the running binary.
pub fn detect() -> InstallMethod {
    // An explicit packager opt-out takes priority over any path-based
    // heuristic: a cask or wrapper that sets `PANEFLOW_UPDATE_EXPLANATION`
    // owns updates for this install, so the pill copies its instruction
    // instead of attempting a download it cannot complete.
    if let Some(externally_managed) = detect_externally_managed(
        std::env::var_os("PANEFLOW_UPDATE_EXPLANATION"),
        option_env!("PANEFLOW_UPDATE_EXPLANATION"),
    ) {
        return externally_managed;
    }

    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(_) => return InstallMethod::Unknown,
    };
    // Canonicalise resolves symlinks and `..` segments. If it fails (unlikely),
    // fall back to the raw exe path.
    //
    // On Windows, `canonicalize` returns the extended-length `\\?\C:\…` form
    // (rust-lang/rust#42869). `windows_msi_install_path` compares this against
    // the non-verbatim `%ProgramFiles%` env value, and
    // `Path::starts_with`'s leading component (`Prefix(VerbatimDisk)` vs
    // `Prefix(Disk)`) never matches - so every MSI install would fall through
    // to `Unknown` and the updater would wrongly take the Linux `$HOME` tar.gz
    // path. Strip the verbatim prefix so the comparison lines up.
    let canonical = std::fs::canonicalize(&exe).unwrap_or(exe);

    let result = classify(&canonical);

    // US-007 AC3 - on macOS, a binary that is NOT inside a .app bundle means
    // someone extracted paneflow ad-hoc (e.g. copied to ~/bin/). In-app
    // updates can't target such installs, so surface the reason once at
    // startup instead of silently showing a never-firing update prompt.
    #[cfg(target_os = "macos")]
    if matches!(result, InstallMethod::Unknown) {
        // In a debug build the binary lives under `target/debug/` and is never
        // inside a .app - this is the expected dev path, so log it at debug
        // level to avoid spamming a warning on every `cargo run`. A release
        // binary running outside a bundle is a genuine ad-hoc extraction worth
        // surfacing at warn level.
        let msg = format!(
            "paneflow: running binary at {} is not inside a .app bundle - in-app updates disabled",
            canonical.display()
        );
        if cfg!(debug_assertions) {
            log::debug!("{msg}");
        } else {
            log::warn!("{msg}");
        }
    }

    result
}

/// Pure detector for sandboxed / packager-managed environments. Returns
/// `Some(InstallMethod::ExternallyManaged)` when a packager has claimed this
/// install:
///
/// - `runtime_explanation` - `PANEFLOW_UPDATE_EXPLANATION` env var read at
///   startup. Highest priority because it's the explicit opt-out a packager
///   set in the host wrapper / launcher.
/// - `build_explanation` - same env var captured at build time via
///   `option_env!("PANEFLOW_UPDATE_EXPLANATION")`, baked in by the formula
///   or cask that produced the binary.
///
/// Pure (no I/O, no FS reads) so the unit tests can mock both signals.
fn detect_externally_managed(
    runtime_explanation: Option<OsString>,
    build_explanation: Option<&str>,
) -> Option<InstallMethod> {
    if let Some(value) = runtime_explanation
        && let Some(text) = value.to_str()
        && !text.trim().is_empty()
    {
        return Some(InstallMethod::ExternallyManaged {
            explanation: text.trim().to_string(),
        });
    }
    if let Some(text) = build_explanation
        && !text.trim().is_empty()
    {
        return Some(InstallMethod::ExternallyManaged {
            explanation: text.trim().to_string(),
        });
    }
    None
}

/// Pure classifier - no I/O at all. Every input is a parameter so callers
/// (and tests) control them.
///
/// On this fork there is exactly one updatable layout: the macOS `.app`
/// bundle. The check is structural on the path components
/// (`<bundle>/Contents/MacOS/<binary>`), so a drag-install to an arbitrary
/// directory is detected, not just `/Applications`. Anything else is
/// `Unknown`, and `detect()` logs why.
fn classify(canonical: &Path) -> InstallMethod {
    if let Some(bundle_path) = app_bundle_path(canonical) {
        return InstallMethod::AppBundle { bundle_path };
    }
    InstallMethod::Unknown
}

/// Return the enclosing `.app` bundle path if `path` points at a binary
/// inside a macOS app bundle, else `None`. We check structurally - parent
/// must be `MacOS`, grandparent `Contents`, great-grandparent ends with
/// `.app` - so drag-installs to arbitrary locations (e.g. `~/Downloads/`)
/// are still detected, not just the canonical `/Applications` path.
fn app_bundle_path(path: &Path) -> Option<PathBuf> {
    let macos_dir = path.parent()?;
    if macos_dir.file_name()?.to_str()? != "MacOS" {
        return None;
    }
    let contents_dir = macos_dir.parent()?;
    if contents_dir.file_name()?.to_str()? != "Contents" {
        return None;
    }
    let bundle = contents_dir.parent()?;
    let bundle_name = bundle.file_name()?.to_str()?;
    // `.app` is an extension; a directory literally named `.app` with no
    // prefix isn't a real bundle.
    if !bundle_name.ends_with(".app") || bundle_name == ".app" {
        return None;
    }
    Some(bundle.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_for_legacy_run_install() {
        let r = classify(Path::new("/home/u/.local/bin/paneflow"));
        assert_eq!(r, InstallMethod::Unknown);
    }

    #[test]
    fn unknown_for_random_path() {
        let r = classify(Path::new("/opt/random/paneflow"));
        assert_eq!(r, InstallMethod::Unknown);
    }

    // ---- US-007 tests ----

    #[test]
    fn app_bundle_in_slash_applications() {
        let r = classify(Path::new(
            "/Applications/PaneFlow.app/Contents/MacOS/paneflow",
        ));
        match r {
            InstallMethod::AppBundle { bundle_path } => {
                assert_eq!(bundle_path, Path::new("/Applications/PaneFlow.app"));
            }
            other => panic!("expected AppBundle, got {other:?}"),
        }
    }

    #[test]
    fn app_bundle_in_home_applications() {
        let r = classify(Path::new(
            "/Users/alice/Applications/PaneFlow.app/Contents/MacOS/paneflow",
        ));
        match r {
            InstallMethod::AppBundle { bundle_path } => {
                assert_eq!(
                    bundle_path,
                    Path::new("/Users/alice/Applications/PaneFlow.app")
                );
            }
            other => panic!("expected AppBundle, got {other:?}"),
        }
    }

    #[test]
    fn app_bundle_at_arbitrary_drag_install_location() {
        // Structural check matches any location, not just /Applications.
        let r = classify(Path::new(
            "/opt/third-party/PaneFlow.app/Contents/MacOS/paneflow",
        ));
        assert!(matches!(r, InstallMethod::AppBundle { .. }));
    }

    #[test]
    fn macos_binary_outside_bundle_is_unknown() {
        // A user who extracted paneflow to ~/bin/ gets Unknown (AC3).
        let r = classify(Path::new("/Users/alice/bin/paneflow"));
        assert_eq!(r, InstallMethod::Unknown);
    }

    #[test]
    fn app_bundle_parser_rejects_wrong_layout() {
        // Wrong MacOS directory name
        assert!(
            app_bundle_path(Path::new(
                "/Applications/PaneFlow.app/Contents/bin/paneflow"
            ))
            .is_none()
        );
        // Wrong Contents directory name
        assert!(
            app_bundle_path(Path::new(
                "/Applications/PaneFlow.app/Payload/MacOS/paneflow"
            ))
            .is_none()
        );
        // Bundle dir not ending in .app
        assert!(
            app_bundle_path(Path::new("/Applications/PaneFlow/Contents/MacOS/paneflow")).is_none()
        );
        // Bundle dir named literally `.app` (edge case)
        assert!(app_bundle_path(Path::new("/Applications/.app/Contents/MacOS/paneflow")).is_none());
        // Missing parent entirely (root-level binary)
        assert!(app_bundle_path(Path::new("/paneflow")).is_none());
    }

    // ─── US-004: rpm-ostree (Silverblue / Kinoite) detection precedence ───
}
