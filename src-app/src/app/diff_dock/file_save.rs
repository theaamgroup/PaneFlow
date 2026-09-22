//! Atomic write of a regular file, and the stamp the Changes tab uses to
//! refuse a revert over a file that changed after the diff was read.
//!
//! The bytes land in a sibling temp file in the target's own directory and
//! are renamed into place, so a reader sees the old file or the new one.
//! A cross-filesystem rename is neither atomic nor always permitted, so the
//! temp file never goes in `$TMPDIR`.

use std::io::Write;
use std::path::Path;
use std::time::SystemTime;

use tempfile::NamedTempFile;

/// Modification time plus length of a regular file the Changes tab agreed with.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct FileStamp {
    mtime: Option<SystemTime>,
    len: u64,
}

impl FileStamp {
    /// Stat `path`. `None` means the file is not there or cannot be stat'd.
    pub(crate) fn read(path: &Path) -> Option<Self> {
        Self::from_metadata(&std::fs::metadata(path).ok()?)
    }

    /// A stamp discarded after restoring a symlink whose target cannot be stat'd.
    pub(crate) fn discarded() -> Self {
        Self {
            mtime: None,
            len: 0,
        }
    }

    /// The stamp of metadata already in hand. `None` for anything that is not
    /// a regular file, so a symlink or fifo is never treated as the snapshot.
    pub(crate) fn from_metadata(meta: &std::fs::Metadata) -> Option<Self> {
        if !meta.is_file() {
            return None;
        }
        Some(Self {
            mtime: meta.modified().ok(),
            len: meta.len(),
        })
    }

    /// Whether a save that last agreed with `expected` may land over a file
    /// that currently stats as `current`.
    ///
    /// `expected` is `None` for a file that was not on disk when it was last
    /// stamped: anything present now is someone else's file.
    fn conflicts(expected: Option<Self>, current: Option<Self>) -> bool {
        match (expected, current) {
            (Some(expected), Some(current)) => expected.differs(&current),
            (None, Some(_)) => true,
            _ => false,
        }
    }

    /// Whether `other` describes a different file state than `self`.
    ///
    /// A missing mtime on either side falls back to the length alone.
    pub(crate) fn differs(&self, other: &Self) -> bool {
        if self.len != other.len {
            return true;
        }
        match (self.mtime, other.mtime) {
            (Some(a), Some(b)) => a != b,
            _ => false,
        }
    }
}

/// Why a save did not land.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SaveError {
    /// The file on disk no longer matches the stamp. Nothing was written.
    Conflict,
    /// The write itself failed. A written sentence, never a debug-formatted
    /// `io::Error`.
    Write(String),
}

impl From<String> for SaveError {
    fn from(message: String) -> Self {
        Self::Write(message)
    }
}

/// Write `contents` to a regular file at `path`, preserving its permissions,
/// and return the stamp of what landed.
///
/// Refuses a symlink, including one substituted while the temp file is being
/// written, so a revert cannot follow a link onto a different target. The
/// file is compared against `expected` before the temp file is written and
/// again immediately before the rename.
///
/// **Blocking.**
pub(crate) fn save_regular_blocking(
    path: &Path,
    contents: &str,
    expected: Option<FileStamp>,
) -> Result<FileStamp, SaveError> {
    persist_blocking_with(path, contents, expected, || {})
}

fn persist_blocking_with(
    path: &Path,
    contents: &str,
    expected: Option<FileStamp>,
    before_persist: impl FnOnce(),
) -> Result<FileStamp, SaveError> {
    let is_regular = || std::fs::symlink_metadata(path).is_ok_and(|meta| meta.is_file());
    if !is_regular() {
        return Err(SaveError::Conflict);
    }
    let parent = parent_dir(path);
    let existing = std::fs::metadata(path).ok();
    if FileStamp::conflicts(
        expected,
        existing.as_ref().and_then(FileStamp::from_metadata),
    ) {
        return Err(SaveError::Conflict);
    }

    let mut temp = NamedTempFile::new_in(&parent).map_err(|err| write_error(&err))?;
    temp.write_all(contents.as_bytes())
        .map_err(|err| write_error(&err))?;
    temp.as_file_mut()
        .flush()
        .map_err(|err| write_error(&err))?;
    temp.as_file().sync_all().map_err(|err| write_error(&err))?;

    if let Some(meta) = &existing {
        let permissions = meta.permissions();
        if let Err(err) = temp.as_file().set_permissions(permissions) {
            log::warn!(
                "could not carry the original permissions onto {}: {err}",
                path.display()
            );
        }
    }

    before_persist();
    if !is_regular() || FileStamp::conflicts(expected, FileStamp::read(path)) {
        return Err(SaveError::Conflict);
    }
    temp.persist(path).map_err(|err| write_error(&err.error))?;
    FileStamp::read(path).ok_or_else(|| {
        SaveError::Write("The file was written but could not be read back.".to_string())
    })
}

fn parent_dir(path: &Path) -> std::path::PathBuf {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => std::path::PathBuf::from("."),
    }
}

fn write_error(err: &std::io::Error) -> String {
    use std::io::ErrorKind;
    match err.kind() {
        ErrorKind::PermissionDenied => "Permission denied - this file could not be written.",
        ErrorKind::NotFound => "The folder holding this file no longer exists.",
        ErrorKind::StorageFull => "The disk is full - nothing was written.",
        ErrorKind::ReadOnlyFilesystem => "This file is on a read-only filesystem.",
        _ => "This file could not be written.",
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_revert_preserves_a_read_only_files_permissions() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file");
        std::fs::write(&path, "modified\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444)).unwrap();
        save_regular_blocking(&path, "base\n", FileStamp::read(&path)).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "base\n");
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o444
        );
    }

    #[test]
    fn a_regular_revert_never_follows_a_substituted_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        let path = dir.path().join("file");
        std::fs::write(&target, "secret\n").unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();
        assert_eq!(
            save_regular_blocking(&path, "revert\n", FileStamp::read(&path)),
            Err(SaveError::Conflict)
        );
        assert_eq!(std::fs::read_to_string(target).unwrap(), "secret\n");
    }

    #[test]
    fn a_regular_revert_refuses_a_symlink_substituted_during_the_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file");
        let target = dir.path().join("target");
        std::fs::write(&path, "before\n").unwrap();
        std::fs::write(&target, "secret\n").unwrap();
        let result = persist_blocking_with(&path, "reverted\n", FileStamp::read(&path), || {
            std::fs::remove_file(&path).unwrap();
            std::os::unix::fs::symlink(&target, &path).unwrap();
        });
        assert_eq!(result, Err(SaveError::Conflict));
        assert_eq!(std::fs::read_to_string(target).unwrap(), "secret\n");
        assert!(
            std::fs::symlink_metadata(&path)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn a_save_is_refused_when_the_stamp_no_longer_matches() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("main.rs");
        std::fs::write(&path, "one\n").unwrap();
        let expected = FileStamp::read(&path);
        std::fs::write(&path, "written by someone else\n").unwrap();

        assert_eq!(
            save_regular_blocking(&path, "mine\n", expected),
            Err(SaveError::Conflict)
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "written by someone else\n"
        );
    }

    #[test]
    fn the_stamp_detects_an_external_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("watched.rs");
        std::fs::write(&path, "aaaa").unwrap();
        let first = FileStamp::read(&path).unwrap();

        std::fs::write(&path, "aaaaaa").unwrap();
        let second = FileStamp::read(&path).unwrap();
        assert!(first.differs(&second), "a length change is a change");
        assert!(!second.differs(&second));
    }
}
