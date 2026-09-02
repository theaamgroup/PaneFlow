use std::io::{Error, ErrorKind, Result, Write};
use std::path::Path;

use serde_json::Value;

pub fn home_dir() -> Option<std::path::PathBuf> {
    dirs::home_dir()
}

pub fn config_dir() -> Option<std::path::PathBuf> {
    dirs::config_dir()
}

/// Hard cap on the size of any agent configuration file this crate reads
/// into memory. Real settings files are kilobytes; the cap matches the app
/// config loader's `MAX_CONFIG_SIZE_BYTES` (1 MiB) so a runaway or hostile
/// file cannot OOM the shim on every wrapped-agent launch.
const MAX_OPTIONAL_TEXT_BYTES: u64 = 1 << 20;

/// Read a UTF-8 configuration file without conflating absence with failure.
///
/// Mirrors `read_config_string` in `crates/paneflow-config/src/loader.rs`:
/// the file type and size are checked on the already-open descriptor (so a
/// path swap between stat and read cannot bypass the guard), anything that is
/// not a regular file is refused, and the read itself is bounded by
/// `MAX_OPTIONAL_TEXT_BYTES`. Both refusals surface as `InvalidData` so every
/// caller fails closed through its existing `?` instead of hanging on a FIFO
/// or allocating an unbounded blob.
pub fn read_optional_text(path: &Path) -> Result<Option<String>> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;

    // `O_NONBLOCK` makes `open(2)` on a writer-less FIFO return at once
    // instead of blocking until a writer appears; it has no effect on the
    // regular files this helper is meant to read.
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc_o_nonblock())
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };

    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() {
        return Err(Error::new(
            ErrorKind::InvalidData,
            format!("{} is not a regular file", path.display()),
        ));
    }
    if metadata.len() > MAX_OPTIONAL_TEXT_BYTES {
        return Err(Error::new(
            ErrorKind::InvalidData,
            format!(
                "{} is too large ({} bytes; maximum {MAX_OPTIONAL_TEXT_BYTES})",
                path.display(),
                metadata.len()
            ),
        ));
    }

    let mut content = String::new();
    file.take(MAX_OPTIONAL_TEXT_BYTES + 1)
        .read_to_string(&mut content)?;
    if content.len() as u64 > MAX_OPTIONAL_TEXT_BYTES {
        return Err(Error::new(
            ErrorKind::InvalidData,
            format!(
                "{} is too large ({} bytes; maximum {MAX_OPTIONAL_TEXT_BYTES})",
                path.display(),
                content.len()
            ),
        ));
    }
    Ok(Some(content))
}

/// `O_NONBLOCK` as macOS `open(2)` expects it (`<sys/fcntl.h>`). Spelled out
/// here because this crate deliberately carries no `libc` dependency, and the
/// fork builds for `aarch64-apple-darwin` only.
const fn libc_o_nonblock() -> i32 {
    0x4
}

pub fn write_json_atomic(path: &Path, value: &Value) -> Result<()> {
    write_atomic(path, |file| {
        serde_json::to_writer_pretty(&mut *file, value).map_err(Error::other)?;
        file.write_all(b"\n")
    })
}

pub fn write_text_atomic(path: &Path, content: &str) -> Result<()> {
    write_atomic(path, |file| file.write_all(content.as_bytes()))
}

/// Resolve an existing symlink to its managed target so an atomic replacement
/// updates the target without silently breaking a dotfile-manager (stow,
/// chezmoi, yadm) link. A dangling link is refused: replacing it would change
/// the user's path policy. Mirrors `config_write_target` in
/// `src-app/src/config_writer.rs`; duplicated here because this crate stays
/// GPU-free and `src-app` does not depend on it.
pub fn write_target(path: &Path) -> Result<std::path::PathBuf> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => std::fs::canonicalize(path),
        Ok(_) => Ok(path.to_path_buf()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(path.to_path_buf()),
        Err(error) => Err(error),
    }
}

fn write_atomic(path: &Path, write: impl FnOnce(&mut std::fs::File) -> Result<()>) -> Result<()> {
    let target = write_target(path)?;
    let parent = target
        .parent()
        .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "configuration path has no parent"))?;
    // The temp file lives in the target's own directory so the rename stays on
    // one filesystem. `NamedTempFile` creates a random, exclusive 0600 file.
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    write(temporary.as_file_mut())?;
    temporary.flush()?;
    // Preserve the existing file's mode: the rename replaces the inode, which
    // would otherwise silently reset the user's permissions to the temp
    // file's 0600. A missing target keeps the temp file's 0600 default.
    match std::fs::metadata(&target) {
        Ok(metadata) => temporary
            .as_file()
            .set_permissions(metadata.permissions())?,
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    // `flush()` alone leaves the bytes in the OS cache; a crash after the
    // rename could then publish an empty or truncated config.
    temporary.as_file().sync_all()?;
    temporary.persist(&target).map_err(|error| error.error)?;
    // Best-effort: push the rename's directory entry to disk too. The data is
    // already published atomically, so a directory sync failure is not worth
    // failing the write over.
    if let Ok(directory) = std::fs::File::open(parent) {
        let _ = directory.sync_all();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_write_updates_a_symlinked_config_through_the_link() {
        let directory = tempfile::TempDir::new().unwrap();
        let target = directory.path().join("managed.json");
        let link = directory.path().join("config.json");
        std::fs::write(&target, "{}\n").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        write_json_atomic(&link, &serde_json::json!({"updated": true})).unwrap();

        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink(),
            "the write must update the managed target, not replace the symlink"
        );
        let written: Value =
            serde_json::from_str(&std::fs::read_to_string(&target).unwrap()).unwrap();
        assert_eq!(written, serde_json::json!({"updated": true}));
    }

    #[test]
    fn atomic_write_refuses_a_dangling_symlink() {
        let directory = tempfile::TempDir::new().unwrap();
        let missing = directory.path().join("missing.json");
        let link = directory.path().join("config.json");
        std::os::unix::fs::symlink(&missing, &link).unwrap();

        let error = write_text_atomic(&link, "content").unwrap_err();

        assert_eq!(error.kind(), ErrorKind::NotFound);
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink(),
            "a dangling link must be refused, not replaced with a regular file"
        );
        assert!(!missing.exists(), "the missing target must not be created");
    }

    #[test]
    fn atomic_write_preserves_the_existing_file_mode() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::TempDir::new().unwrap();
        let path = directory.path().join("config.json");
        std::fs::write(&path, "before").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();

        write_text_atomic(&path, "after").unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "after");
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o640
        );
    }

    #[test]
    fn atomic_write_still_creates_a_missing_file() {
        let directory = tempfile::TempDir::new().unwrap();
        let path = directory.path().join("config.json");

        write_text_atomic(&path, "fresh").unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "fresh");
    }

    #[test]
    fn atomic_write_through_a_symlinked_parent_updates_in_place() {
        let directory = tempfile::TempDir::new().unwrap();
        let real_dir = directory.path().join("real");
        std::fs::create_dir(&real_dir).unwrap();
        let linked_dir = directory.path().join("linked");
        std::os::unix::fs::symlink(&real_dir, &linked_dir).unwrap();
        std::fs::write(real_dir.join("config.json"), "before").unwrap();

        write_text_atomic(&linked_dir.join("config.json"), "after").unwrap();

        assert_eq!(
            std::fs::read_to_string(real_dir.join("config.json")).unwrap(),
            "after"
        );
        assert!(std::fs::symlink_metadata(&linked_dir)
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[test]
    fn optional_read_distinguishes_absence_from_invalid_utf8() {
        let directory = tempfile::TempDir::new().unwrap();
        let missing = directory.path().join("missing.json");
        assert_eq!(read_optional_text(&missing).unwrap(), None);

        let invalid = directory.path().join("invalid.json");
        std::fs::write(&invalid, [0xff]).unwrap();
        assert_eq!(
            read_optional_text(&invalid).unwrap_err().kind(),
            ErrorKind::InvalidData
        );
        assert_eq!(std::fs::read(&invalid).unwrap(), [0xff]);
    }

    #[test]
    fn optional_read_refuses_an_oversize_file() {
        let directory = tempfile::TempDir::new().unwrap();
        let path = directory.path().join("huge.json");
        let oversize = usize::try_from(MAX_OPTIONAL_TEXT_BYTES).unwrap() + 1;
        std::fs::write(&path, vec![b' '; oversize]).unwrap();

        let error = read_optional_text(&path).unwrap_err();

        assert_eq!(error.kind(), ErrorKind::InvalidData);
        assert!(
            error.to_string().contains("too large"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn optional_read_still_accepts_a_file_at_the_cap() {
        let directory = tempfile::TempDir::new().unwrap();
        let path = directory.path().join("exact.json");
        let exact = usize::try_from(MAX_OPTIONAL_TEXT_BYTES).unwrap();
        std::fs::write(&path, vec![b' '; exact]).unwrap();

        assert_eq!(read_optional_text(&path).unwrap().unwrap().len(), exact);
    }

    #[test]
    fn optional_read_refuses_a_fifo_without_blocking() {
        let directory = tempfile::TempDir::new().unwrap();
        let path = directory.path().join("settings.json");
        let status = std::process::Command::new("mkfifo")
            .arg(&path)
            .status()
            .expect("mkfifo must be runnable");
        assert!(status.success(), "mkfifo failed: {status}");

        // No writer ever opens the pipe: an unguarded read would block here
        // forever instead of returning.
        let error = read_optional_text(&path).unwrap_err();

        assert_eq!(error.kind(), ErrorKind::InvalidData);
        assert!(
            error.to_string().contains("not a regular file"),
            "unexpected error: {error}"
        );
    }
}
