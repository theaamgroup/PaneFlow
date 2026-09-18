//! First-launch-after-upgrade detection for the release-notes toast (#526).
//!
//! A one-line version marker under the cache dir is compared with the running
//! version on every launch and then rewritten. It fires on the first run of a
//! newer bundle whatever installed it: Sparkle's install-on-quit, a DMG drag,
//! or a local build. A first install, a downgrade, and an unreadable marker
//! all stay quiet.

use std::path::{Path, PathBuf};

const MARKER_FILENAME: &str = "last-launched-version";
const RELEASES_TAG_BASE_URL: &str = "https://github.com/theaamgroup/paneflow/releases/tag";

/// The fork's GitHub release page for `version`.
pub(crate) fn release_notes_url(version: &str) -> String {
    format!("{RELEASES_TAG_BASE_URL}/v{version}")
}

/// `~/Library/Caches/paneflow[-dev]/last-launched-version`, relocated by
/// `PANEFLOW_HOME` with the rest of the cache root.
fn marker_path() -> Option<PathBuf> {
    Some(
        crate::runtime_paths::cache_dir()?
            .join(crate::runtime_paths::APP_SUBDIR)
            .join(MARKER_FILENAME),
    )
}

/// Record this launch and return the running version when it is newer than
/// the one recorded by the previous launch. Call once per process: the second
/// call always reads the marker the first one wrote.
pub(crate) fn upgraded_version() -> Option<String> {
    record_launch(&marker_path()?, env!("CARGO_PKG_VERSION"))
}

fn record_launch(marker_path: &Path, current: &str) -> Option<String> {
    let previous = std::fs::read_to_string(marker_path).ok();
    if previous.as_deref().map(str::trim) != Some(current) {
        write_marker(marker_path, current);
    }
    is_upgrade(previous.as_deref(), current).then(|| current.to_string())
}

fn write_marker(marker_path: &Path, current: &str) {
    if let Some(parent) = marker_path.parent()
        && let Err(err) = std::fs::create_dir_all(parent)
    {
        log::warn!(
            "paneflow: cannot create cache dir {} ({err}); the release toast may repeat next launch",
            parent.display()
        );
        return;
    }
    if let Err(err) = std::fs::write(marker_path, current.as_bytes()) {
        log::warn!(
            "paneflow: cannot write version marker {} ({err}); the release toast may repeat next launch",
            marker_path.display()
        );
    }
}

fn is_upgrade(previous: Option<&str>, current: &str) -> bool {
    let (Some(previous), Some(current)) = (
        previous.and_then(|previous| parse_version(previous.trim())),
        parse_version(current),
    ) else {
        return false;
    };
    current > previous
}

/// Exactly `x.y.z`, three decimal `u64` fields. Anything else (a pre-release
/// suffix, a leading `v`, a sign, a fourth field) is unparsable, and an
/// unparsable side never announces.
fn parse_version(text: &str) -> Option<(u64, u64, u64)> {
    let mut fields = text.split('.');
    let mut next = || -> Option<u64> {
        let field = fields.next()?;
        if field.is_empty() || !field.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        field.parse().ok()
    };
    let version = (next()?, next()?, next()?);
    fields.next().is_none().then_some(version)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_notes_url_points_at_the_forks_tag_page() {
        let url = release_notes_url("0.6.1");
        assert_eq!(
            url,
            "https://github.com/theaamgroup/paneflow/releases/tag/v0.6.1"
        );
        // The toast opens it through `open_http_url`, which refuses anything
        // this gate refuses.
        assert!(crate::external_open::require_http_url(&url).is_ok());
    }

    #[test]
    fn a_first_launch_records_the_version_without_announcing_it() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let marker = tmp.path().join("cache").join(MARKER_FILENAME);

        assert_eq!(record_launch(&marker, "0.14.2"), None);
        assert_eq!(
            std::fs::read_to_string(&marker).expect("marker written"),
            "0.14.2"
        );
    }

    #[test]
    fn a_newer_version_announces_once() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let marker = tmp.path().join(MARKER_FILENAME);
        std::fs::write(&marker, b"0.14.1").expect("seed marker");

        assert_eq!(record_launch(&marker, "0.14.2"), Some("0.14.2".to_string()));
        assert_eq!(record_launch(&marker, "0.14.2"), None);
    }

    #[test]
    fn a_trailing_newline_in_the_marker_still_compares() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let marker = tmp.path().join(MARKER_FILENAME);
        std::fs::write(&marker, b"0.14.1\n").expect("seed marker");

        assert_eq!(record_launch(&marker, "0.14.2"), Some("0.14.2".to_string()));
    }

    #[test]
    fn a_downgrade_does_not_announce() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let marker = tmp.path().join(MARKER_FILENAME);
        std::fs::write(&marker, b"0.15.0").expect("seed marker");

        assert_eq!(record_launch(&marker, "0.14.2"), None);
        assert_eq!(
            std::fs::read_to_string(&marker).expect("marker rewritten"),
            "0.14.2"
        );
    }

    #[test]
    fn an_unparsable_marker_does_not_announce() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let marker = tmp.path().join(MARKER_FILENAME);
        std::fs::write(&marker, b"nightly").expect("seed marker");

        assert_eq!(record_launch(&marker, "0.14.2"), None);
        assert_eq!(
            std::fs::read_to_string(&marker).expect("marker rewritten"),
            "0.14.2"
        );
    }

    #[test]
    fn versions_compare_by_number_not_by_text() {
        assert!(is_upgrade(Some("0.9.9"), "0.10.0"));
        assert!(is_upgrade(Some("0.6.1"), "1.0.0"));
        assert!(!is_upgrade(Some("0.10.0"), "0.9.9"));
        assert!(!is_upgrade(Some("0.6.1"), "0.6.1"));
        assert!(!is_upgrade(None, "0.6.1"));
    }

    #[test]
    fn only_three_plain_decimal_fields_parse() {
        assert_eq!(parse_version("0.6.1"), Some((0, 6, 1)));
        for bad in [
            "",
            "0.6",
            "0.6.1.2",
            "v0.6.1",
            "0.6.1-rc1",
            "0.+6.1",
            "0..1",
            "0.6.x",
        ] {
            assert_eq!(parse_version(bad), None, "{bad:?} must not parse");
        }
    }

    #[test]
    fn the_running_version_parses() {
        // A version bump to a shape the parser refuses would silence the
        // toast forever; fail here instead.
        assert!(parse_version(env!("CARGO_PKG_VERSION")).is_some());
    }
}
