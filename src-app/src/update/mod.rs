//! In-app self-update flow + install-method detection + GitHub release polling.
//!
//! There is exactly one on-the-wire update strategy on this fork:
//!
//! - **macOS DMG** - minisign-verified, Gatekeeper-checked, copied and swapped
//!   in place inside `/Applications` or `~/Applications`.
//!
//! Any install that is not a `.app` bundle (an ad-hoc copy, a dev build under
//! `target/`, or an install a packager has claimed via
//! `PANEFLOW_UPDATE_EXPLANATION`) disables in-app updates and says why,
//! rather than showing a prompt that can never fire.
//!
//! The strategy eventually calls GPUI's `cx.set_restart_path(path) +
//! cx.restart()` - the "launcher pattern" where GPUI spawns a detached
//! `bash` script that waits for our PID to exit (via `kill -0` polling) and
//! then execs the new binary. Safe for Wayland/GPU apps because the current
//! process runs its Drops cleanly before the new one opens a fresh
//! compositor/GPU connection.
//!
//! State lives in `PaneFlowApp::self_update_status`. The title bar reads it
//! each render to flip the pill label between `available / Downloading… /
//! Installing…`. Errors are reported via a toast.
//!
//! Module layout (US-031):
//! - [`error`] - `UpdateError`, `IntegrityMismatch`, `classify`, `is_disk_full`
//! - [`checker`] - GitHub release polling + asset picking
//! - [`install_method`] - install source detection (AppBundle / ExternallyManaged / Unknown)
//! - [`macos`] - DMG update runner

pub mod checker;
pub mod error;
pub mod install_method;
pub mod macos;
pub(crate) mod redirect;
// US-001 (prd-audit-remediation): minisign detached-signature verification -
// the independent root of trust shared by every installer path.
pub mod signature;
pub(crate) mod verified_download;

// Ergonomic re-export: callers use `crate::update::UpdateError` without
// reaching into `update::error::UpdateError`.
pub use error::UpdateError;

use std::path::PathBuf;

use anyhow::Result;

/// Rendering-facing state of the self-update flow.
#[derive(Clone, Debug, Default)]
pub enum SelfUpdateStatus {
    /// No update operation in flight - the title bar shows `v{x} available`.
    #[default]
    Idle,
    Downloading,
    /// The new binary has been downloaded, verified and swapped into place;
    /// `cx.set_restart_path()` has already been called. The pill switches
    /// to a "Restart for vX" affordance whose click handler only invokes
    /// `cx.restart()` - no I/O, no waiting, no analytics flush. This
    /// mirrors Zed's auto-update split (download/install in background →
    /// "Restart to Update" CTA) so the click-to-restart latency is ~0
    /// regardless of network speed or disk throughput.
    ReadyToRestart,
    /// Structured classification of the last failure (US-013). The toast
    /// renderer picks its copy per variant; the pill shows "Update failed"
    /// and remains clickable so the user can retry.
    Errored(#[allow(dead_code)] UpdateError),
}

impl SelfUpdateStatus {
    pub fn is_busy(&self) -> bool {
        matches!(self, SelfUpdateStatus::Downloading)
    }
}

/// Resolve the expected install location of the paneflow binary. The
/// installer writes here; callers pass this path to `cx.set_restart_path()` so
/// GPUI's relaunch script execs the freshly installed binary.
///
/// US-009 AC3: unused by the DMG updater itself. The dispatcher passes
/// `InstallMethod::AppBundle { bundle_path }` directly to the macOS install
/// flow, which returns the promoted `.app` bundle path for GPUI's `open`
/// based restart. Retained for callers that want the canonical location.
#[allow(dead_code)]
pub fn installed_binary_path() -> Result<PathBuf> {
    Ok(PathBuf::from(
        "/Applications/PaneFlow.app/Contents/MacOS/paneflow",
    ))
}
