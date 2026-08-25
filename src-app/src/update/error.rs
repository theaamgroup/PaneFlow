//! Structured classification of update failures (US-013) plus the `is_disk_full`
//! predicate. The main thread calls [`UpdateError::classify`] at the boundary to
//! bucket `anyhow::Error`s into renderable variants; the toast renderer picks
//! its copy per variant (`Network`, `IntegrityMismatch`, `DiskFull`, `Other`).
//!
//! Extracted from `self_update/mod.rs` per US-031. The `libc::ENOSPC` arm of
//! `is_disk_full` is gated `#[cfg(unix)]` per US-034, alongside the typed
//! `std::io::ErrorKind::StorageFull` branch.

use std::path::PathBuf;

/// Structured, user-facing classification of an update failure (US-013).
///
/// Lower layers return `anyhow::Error`; the main thread calls
/// [`UpdateError::classify`] at the boundary to bucket the failure into one
/// of the variants the title bar knows how to render. Keep variants
/// *exhaustive from the renderer's perspective*: adding a new failure mode
/// without a matching toast string is a UX regression.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UpdateError {
    /// DNS/TCP/TLS/socket-level failure. The wrapped string is preserved for
    /// logs; the toast uses a fixed human message.
    Network(#[allow(dead_code)] String),
    /// SHA-256 of the downloaded asset did not match its `.sha256` sibling.
    IntegrityMismatch { expected: String, got: String },
    /// ENOSPC on a write inside the update flow. `path` is best-effort - we
    /// don't always know which write failed, in which case this is an empty
    /// `PathBuf` and the toast renders without the "at {path}" clause.
    DiskFull { path: PathBuf },
    /// Install step was declined by the OS or user. Distinct from
    /// `DiskFull` / `Other`: the update was downloaded and verified
    /// cleanly, but the actual installation could not proceed. Typical
    /// causes on the DMG path are `/Applications/` (or
    /// `$HOME/Applications`) being non-writable, SIP blocking the
    /// replace, the user dismissing the macOS authorization prompt,
    /// App Translocation of a quarantined bundle, or the running
    /// process holding a lock on the install path. The wrapped
    /// `message` is user-visible verbatim so the toast can be specific
    /// ("reinstall manually", "move PaneFlow.app into /Applications").
    InstallDeclined { message: String },
    /// The install step failed after the update was downloaded and
    /// verified. Distinct from `InstallDeclined`: the replace was
    /// attempted, not cancelled. On the DMG path this is the
    /// catastrophic case where promoting the new bundle into place
    /// failed and rolling the old bundle back also failed, so no live
    /// install remains. `log_path` is empty there (the DMG installer
    /// does not write a verbose log); `user_message` then points the
    /// user at a manual reinstall from the DMG.
    InstallFailed { log_path: PathBuf },
    /// A bounded external subprocess in the update flow (`hdiutil`, or
    /// any other installer tool) exceeded its wall-clock deadline and
    /// was killed (EP-002, U-002/U-015), or the whole `Downloading`
    /// state was reset by the worker watchdog. Distinct from
    /// `Network`: the transport may be fine but the tool hung
    /// (half-open TCP, a mirror that accepts then stalls, a wedged
    /// future). The download/install never completed; the user can
    /// retry.
    Timeout,
    /// Classifier couldn't bucket the error. The wrapped message is shown
    /// verbatim so the user sees *something* actionable instead of a
    /// generic "update failed".
    Other(String),
}

impl UpdateError {
    /// Render the PRD-mandated toast copy for this variant. The strings are
    /// intentionally frozen - changing them in one place but not the other
    /// would break US-013 acceptance tests.
    pub fn user_message(&self) -> String {
        match self {
            UpdateError::Network(_) => {
                "Update failed: no connection. Retry when online.".to_string()
            }
            UpdateError::IntegrityMismatch { .. } => {
                "Update failed: downloaded file is corrupt or tampered. Retry or download manually."
                    .to_string()
            }
            UpdateError::DiskFull { path } => {
                if path.as_os_str().is_empty() {
                    "Update failed: disk full. Free space and retry.".to_string()
                } else {
                    format!(
                        "Update failed: disk full at `{}`. Free space and retry.",
                        path.display()
                    )
                }
            }
            UpdateError::InstallDeclined { message } => message.clone(),
            UpdateError::InstallFailed { log_path } => {
                // Empty path is the DMG installer: it does not write a
                // standalone verbose log. A non-empty path is shown so
                // the user can attach it to a bug report.
                if log_path.as_os_str().is_empty() {
                    "Update install failed. Retry later, or reinstall PaneFlow from the DMG.".to_string()
                } else {
                    format!(
                        "Update install failed. Verbose log saved to `{}` - attach it to a bug report.",
                        log_path.display()
                    )
                }
            }
            UpdateError::Timeout => {
                "Update timed out. The download or install stalled - retry when your connection is stable."
                    .to_string()
            }
            UpdateError::Other(msg) => msg.clone(),
        }
    }

    /// Bucket an `anyhow::Error` into a variant.
    ///
    /// Preference order (most specific first):
    ///   1. Downcast to `UpdateError` - lower layers can bail with a
    ///      pre-classified error for free.
    ///   2. Downcast to [`IntegrityMismatch`] - carries `expected`/`got`.
    ///   3. Walk the chain looking for `std::io::Error` with ENOSPC.
    ///   4. Substring-match on the formatted error chain for
    ///      network / integrity / disk-full keywords.
    ///   5. Fall back to `Other` with the raw formatted message.
    pub fn classify(err: &anyhow::Error) -> Self {
        // Walk the full cause chain. `Error::downcast_ref` only inspects the
        // outermost error, which would miss a pre-classified tag wrapped by
        // `.context(...)` - probe every cause so callers are free to add
        // context without losing classification.
        for cause in err.chain() {
            if let Some(tag) = cause.downcast_ref::<UpdateError>() {
                return tag.clone();
            }
            if let Some(mm) = cause.downcast_ref::<IntegrityMismatch>() {
                return UpdateError::IntegrityMismatch {
                    expected: mm.expected.clone(),
                    got: mm.got.clone(),
                };
            }
            // US-001: the update flow bounds every ureq call with a 30 s
            // global timeout. When it fires at the request/response layer,
            // the error surfaces as `ureq::Error::Timeout(_)` - treat it as
            // a network failure so the title bar renders the "no connection"
            // toast instead of the generic `Other` catch-all.
            if let Some(ureq::Error::Timeout(_)) = cause.downcast_ref::<ureq::Error>() {
                return UpdateError::Network(format!("{err:#}"));
            }
            if let Some(io) = cause.downcast_ref::<std::io::Error>()
                && is_disk_full(io)
            {
                return UpdateError::DiskFull {
                    path: PathBuf::new(),
                };
            }
        }
        let full = format!("{err:#}");
        let lower = full.to_ascii_lowercase();
        if lower.contains("no space left") || lower.contains("disk full") {
            return UpdateError::DiskFull {
                path: PathBuf::new(),
            };
        }
        // Integrity keywords are checked BEFORE network keywords so a
        // crafted error like "checksum timed out" cannot silently route a
        // genuine integrity failure into the Network toast. Typed
        // `IntegrityMismatch` downcasts happen earlier in the cause-chain
        // walk, so real SHA mismatches are unaffected - this ordering only
        // matters for stringly-typed errors coming from external tools.
        if lower.contains("failed integrity check")
            || lower.contains("integrity check")
            || lower.contains("checksum")
            || lower.contains("hash mismatch")
        {
            return UpdateError::IntegrityMismatch {
                expected: String::new(),
                got: String::new(),
            };
        }
        if lower.contains("exceeded its deadline") {
            return UpdateError::Timeout;
        }
        if lower.contains("could not fetch integrity checksum")
            || lower.contains("could not download update")
            || lower.contains("could not download update tool")
            || lower.contains("try again when online")
            || lower.contains("could not resolve host")
            || lower.contains("could not connect")
            || lower.contains("failed to connect")
            || lower.contains("network is unreachable")
            || lower.contains("no such host")
            // US-001: body-stream timeouts arrive as io::Error wrapped by
            // std::io::copy - the typed downcast above misses them because
            // the chain no longer carries the original ureq::Error.
            || lower.contains("timed out")
            || lower.contains("timeout")
        {
            return UpdateError::Network(full);
        }
        UpdateError::Other(full)
    }
}

impl std::fmt::Display for UpdateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.user_message())
    }
}

impl std::error::Error for UpdateError {}

/// Tag error for SHA-256 verification failures. Attached via `anyhow::bail!`
/// so the outer classifier can [`downcast_ref`](anyhow::Error::downcast_ref)
/// to recover the exact `expected`/`got` digests - substring-parsing a
/// human message would lose them.
#[derive(Debug, Clone)]
pub struct IntegrityMismatch {
    pub expected: String,
    pub got: String,
}

impl std::fmt::Display for IntegrityMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Downloaded file failed integrity check. Retry or download manually. (expected {}, got {})",
            self.expected, self.got
        )
    }
}

impl std::error::Error for IntegrityMismatch {}

/// True if `err` represents ENOSPC. Covers both the typed `StorageFull`
/// variant (stable since Rust 1.83) and the raw-errno fallback for
/// syscalls that surface `28` via `raw_os_error()` without setting the
/// typed kind. The `libc::ENOSPC` arm is gated `#[cfg(unix)]` per US-034.
pub fn is_disk_full(err: &std::io::Error) -> bool {
    if matches!(err.kind(), std::io::ErrorKind::StorageFull) {
        return true;
    }
    #[cfg(unix)]
    {
        if err.raw_os_error() == Some(libc::ENOSPC) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn io_err(kind: std::io::ErrorKind) -> std::io::Error {
        std::io::Error::new(kind, "synthetic")
    }

    #[test]
    fn classify_direct_update_error_roundtrips() {
        let tagged = UpdateError::InstallDeclined {
            message: "Unable to replace PaneFlow.app".into(),
        };
        let err = anyhow::Error::new(tagged.clone());
        assert_eq!(UpdateError::classify(&err), tagged);
    }

    #[test]
    fn timeout_variant_roundtrips_and_has_user_copy() {
        // EP-002 U-002: invoke_tool and the watchdog both surface
        // `UpdateError::Timeout` through `anyhow::Error::new(..)`; classify must
        // recover it (so record_update_failure tags it, not the `Other`
        // catch-all) and it must carry a non-empty user-facing message.
        let err = anyhow::Error::new(UpdateError::Timeout);
        assert_eq!(UpdateError::classify(&err), UpdateError::Timeout);
        assert!(!UpdateError::Timeout.user_message().is_empty());
    }

    #[test]
    fn classify_recovers_tag_through_context_wrapping() {
        // If a caller wraps `bail!(UpdateError::…)` with `.context(…)`, the
        // root error becomes the context string. The classifier must still
        // recover the tag by walking the chain instead of stopping at the
        // outermost layer.
        let err = anyhow::Error::new(UpdateError::Network("ureq hit EOF".into()))
            .context("fetch release asset")
            .context("self-update/dmg");
        assert!(matches!(
            UpdateError::classify(&err),
            UpdateError::Network(_)
        ));
    }

    #[test]
    fn classify_extracts_integrity_mismatch_digests() {
        let mm = IntegrityMismatch {
            expected: "a".repeat(64),
            got: "b".repeat(64),
        };
        let err = anyhow::Error::new(mm)
            .context("download asset")
            .context("self-update/dmg");
        match UpdateError::classify(&err) {
            UpdateError::IntegrityMismatch { expected, got } => {
                assert_eq!(expected, "a".repeat(64));
                assert_eq!(got, "b".repeat(64));
            }
            other => panic!("expected IntegrityMismatch, got {other:?}"),
        }
    }

    #[test]
    fn classify_disk_full_via_storage_full_kind() {
        let err = anyhow::Error::new(io_err(std::io::ErrorKind::StorageFull))
            .context("write chunk to disk");
        assert!(matches!(
            UpdateError::classify(&err),
            UpdateError::DiskFull { .. }
        ));
    }

    #[cfg(unix)]
    #[test]
    fn classify_disk_full_via_raw_errno() {
        // On some platforms the typed kind isn't set but errno is 28.
        let err = anyhow::Error::new(std::io::Error::from_raw_os_error(libc::ENOSPC))
            .context("create cache dir");
        assert!(matches!(
            UpdateError::classify(&err),
            UpdateError::DiskFull { .. }
        ));
    }

    #[test]
    fn classify_disk_full_via_substring_fallback() {
        // When the io::Error is already stringified (e.g., came out of a
        // subprocess stderr), we fall back to text matching.
        let err = anyhow::anyhow!("extract dmg into scratch dir: No space left on device");
        assert!(matches!(
            UpdateError::classify(&err),
            UpdateError::DiskFull { .. }
        ));
    }

    #[test]
    fn classify_network_via_context_message() {
        let err = anyhow::anyhow!("Could not download update. Try again when online.");
        assert!(matches!(
            UpdateError::classify(&err),
            UpdateError::Network(_)
        ));
    }

    #[test]
    fn classify_network_via_resolve_host() {
        let err = anyhow::anyhow!("curl: (6) Could not resolve host: github.com");
        assert!(matches!(
            UpdateError::classify(&err),
            UpdateError::Network(_)
        ));
    }

    #[test]
    fn classify_integrity_via_keyword_fallback() {
        // No downcast available (e.g., message came from an external tool).
        let err = anyhow::anyhow!("zsync2: checksum verification failed");
        assert!(matches!(
            UpdateError::classify(&err),
            UpdateError::IntegrityMismatch { .. }
        ));
    }

    #[test]
    fn classify_ureq_timeout_variant_as_network() {
        // US-001 AC4: a direct `ureq::Error::Timeout(_)` in the cause chain
        // is classified as `Network`, not `Other`. The context is
        // intentionally neutral - it contains no pre-existing network
        // keyword - so this test actually exercises the typed-downcast arm
        // and would fail if that arm were removed.
        let err = anyhow::Error::new(ureq::Error::Timeout(ureq::Timeout::Global))
            .context("update checker main loop");
        assert!(matches!(
            UpdateError::classify(&err),
            UpdateError::Network(_)
        ));
    }

    #[test]
    fn classify_integrity_keyword_shadowed_by_timeout_substring() {
        // Regression: a crafted error message that mixes an integrity
        // keyword with a timeout keyword must classify as IntegrityMismatch,
        // not Network. Before the substring reordering, "checksum timed out"
        // hit the Network arm first and suppressed the corruption toast.
        let err = anyhow::anyhow!("zsync2: checksum timed out waiting for block");
        assert!(matches!(
            UpdateError::classify(&err),
            UpdateError::IntegrityMismatch { .. }
        ));
    }

    #[test]
    fn classify_ureq_timeout_via_substring_fallback() {
        // US-001: when the timeout surfaces mid-body (std::io::copy wraps
        // it as io::Error, so the typed downcast above misses it), the
        // "timed out" / "timeout" substring fallback must still route it
        // to `Network` instead of `Other`.
        let err = anyhow::anyhow!("stream dmg to disk: request timed out");
        assert!(matches!(
            UpdateError::classify(&err),
            UpdateError::Network(_)
        ));
        let err = anyhow::anyhow!("ureq: timeout");
        assert!(matches!(
            UpdateError::classify(&err),
            UpdateError::Network(_)
        ));
    }

    #[test]
    fn classify_process_deadline_as_timeout() {
        let err = anyhow::anyhow!("process exceeded its deadline and was killed");
        assert!(matches!(UpdateError::classify(&err), UpdateError::Timeout));
    }

    #[test]
    fn classify_other_for_unclassifiable_error() {
        let err = anyhow::anyhow!("some totally unexpected garbage");
        match UpdateError::classify(&err) {
            UpdateError::Other(msg) => assert!(msg.contains("unexpected garbage")),
            other => panic!("expected Other, got {other:?}"),
        }
    }

    #[test]
    fn user_message_matches_prd_copy_network() {
        assert_eq!(
            UpdateError::Network("any".into()).user_message(),
            "Update failed: no connection. Retry when online."
        );
    }

    #[test]
    fn user_message_matches_prd_copy_integrity() {
        assert_eq!(
            UpdateError::IntegrityMismatch {
                expected: "a".into(),
                got: "b".into(),
            }
            .user_message(),
            "Update failed: downloaded file is corrupt or tampered. Retry or download manually."
        );
    }

    #[test]
    fn user_message_disk_full_includes_path_when_set() {
        let err = UpdateError::DiskFull {
            path: PathBuf::from("/Users/u/Library/Caches/paneflow"),
        };
        let msg = err.user_message();
        assert!(msg.contains("disk full"));
        assert!(msg.contains("/Users/u/Library/Caches/paneflow"));
        assert!(msg.contains("Free space and retry"));
    }

    #[test]
    fn user_message_disk_full_omits_path_when_empty() {
        let err = UpdateError::DiskFull {
            path: PathBuf::new(),
        };
        let msg = err.user_message();
        assert!(msg.contains("disk full"));
        assert!(!msg.contains("at `"));
    }

    #[test]
    fn user_message_other_passes_through_raw() {
        let err = UpdateError::Other("raw detail".into());
        assert_eq!(err.user_message(), "raw detail");
    }

    #[test]
    fn classify_install_declined_roundtrips() {
        // US-009 AC8: InstallDeclined survives a downcast through the
        // cause chain so its user-visible message isn't flattened into
        // the generic Other bucket.
        let tagged = UpdateError::InstallDeclined {
            message: "Unable to replace /Applications/PaneFlow.app - reinstall manually".into(),
        };
        let err = anyhow::Error::new(tagged.clone()).context("self-update/dmg");
        assert_eq!(UpdateError::classify(&err), tagged);
    }

    #[test]
    fn user_message_install_declined_passes_through() {
        let err = UpdateError::InstallDeclined {
            message: "Unable to replace /Applications/PaneFlow.app - reinstall manually".into(),
        };
        assert_eq!(
            err.user_message(),
            "Unable to replace /Applications/PaneFlow.app - reinstall manually"
        );
    }

    #[test]
    fn classify_install_failed_roundtrips() {
        // InstallFailed carries its log path through the cause chain so
        // the toast can name it - flattening to Other would lose the
        // path. The DMG installer currently passes an empty path.
        let tagged = UpdateError::InstallFailed {
            log_path: PathBuf::new(),
        };
        let err = anyhow::Error::new(tagged.clone()).context("self-update/dmg");
        assert_eq!(UpdateError::classify(&err), tagged);
    }

    #[test]
    fn user_message_install_failed_includes_log_path() {
        let err = UpdateError::InstallFailed {
            log_path: PathBuf::from("/tmp/paneflow-update.log"),
        };
        let msg = err.user_message();
        assert!(msg.contains("/tmp/paneflow-update.log"), "got: {msg}");
        assert!(msg.contains("Update install failed"), "got: {msg}");
    }

    #[test]
    fn user_message_install_failed_handles_empty_path() {
        // The DMG installer passes `PathBuf::new()` because it does not
        // write a standalone verbose log. The renderer must not emit
        // empty backticks, and must point the user at a manual DMG
        // reinstall rather than a package manager.
        let err = UpdateError::InstallFailed {
            log_path: PathBuf::new(),
        };
        let msg = err.user_message();
        assert!(msg.contains("Update install failed"), "got: {msg}");
        assert!(
            !msg.contains("``"),
            "empty-backticks leaked into user-visible copy: {msg}"
        );
        assert!(
            msg.to_ascii_lowercase().contains("dmg"),
            "empty-path branch should point the user at a DMG reinstall: {msg}"
        );
        assert!(
            !msg.contains("Verbose log saved"),
            "empty-path branch must not advertise a log file that does not exist: {msg}"
        );
        assert!(
            !msg.to_ascii_lowercase().contains("package manager"),
            "empty-path branch must not mention a package manager: {msg}"
        );
    }

    #[test]
    fn classify_recovers_other_through_context_wrapping() {
        // Chain walk must recover an inner `Other` so the toast shows
        // the wrapped message rather than the context string.
        let tagged = UpdateError::Other("hdiutil attach failed".into());
        let err = anyhow::Error::new(tagged.clone()).context("self-update/dmg");
        assert_eq!(UpdateError::classify(&err), tagged);
    }
}
