//! Bounded reads and safe-write primitives for the bridge cleanup.
//!
//! Every removal goes through [`replace_unchanged_unlocked`] under the
//! config lock ([`with_config_lock`]), which is:
//! - **guarded** - the file is re-read right before the atomic rename, and
//!   the write is refused when its bytes differ from what the caller
//!   parsed; the next launch retries. Claude Code rewrites `~/.claude.json`
//!   itself without PaneFlow's lock, and replacing a file it changed
//!   mid-pass would drop its changes. The re-read makes that unlikely but
//!   does not prevent it: `rename(2)` replaces whatever is there, so a write
//!   that lands between the re-read and the rename is lost, and the backup
//!   holds the parsed bytes, not that write.
//! - **backed up** - the parsed bytes are copied to a backup *before* the new
//!   bytes land, and a backup failure aborts the write (we never modify the
//!   original if we could not preserve it first). The backup never replaces
//!   a file: `<file>.bak` when that name is free, else `<file>.paneflow-bak`,
//!   then `<file>.paneflow-bak.1`, `.2`, …
//! - **atomic** - bytes are written to a temp file in the same directory
//!   and `rename`d into place. A crash mid-write leaves the temp file,
//!   never a half-written config.
//!
//! Reads are this crate's own: [`read_config`] allows 64 MiB, because
//! `~/.claude.json` grows past the 1 MiB cap `paneflow-agent-config` keeps
//! for the shim (the cleanup runs on a background thread, so the larger
//! buffer never touches the UI), and [`may_contain`] scans raw bytes in
//! constant memory so a config that never names the bridge is not parsed at
//! all, however large it is.

use std::io::{ErrorKind, Read};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

/// Largest config the cleanup parses. Real `~/.claude.json` files reach a
/// few MiB (per-project history); a larger one is refused, not read.
const MAX_CONFIG_BYTES: u64 = 64 << 20;

/// Chunk size for [`may_contain`].
const SCAN_CHUNK_BYTES: usize = 64 << 10;

/// Most backups tried beside one config before the write is refused.
const MAX_BACKUP_NAMES: usize = 1000;

/// Run a closure while holding the PaneFlow config lock for `path`,
/// retrying briefly if another PaneFlow process is already editing it.
/// Abandoned leases are recovered by the shared dependency-light layer.
pub(crate) fn with_config_lock<T>(path: &Path, f: impl FnOnce() -> Result<T>) -> Result<T> {
    let _lock = lock_config(path)?;
    f()
}

fn lock_config(path: &Path) -> Result<paneflow_agent_config::ConfigLock> {
    paneflow_agent_config::lock_config(path)
        .with_context(|| format!("lock {} failed", path.display()))
}

/// Open `path` for reading without blocking on a FIFO that has no writer.
/// `O_NONBLOCK` as macOS `open(2)` expects it (`<sys/fcntl.h>`); it has no
/// effect on the regular files read here. Spelled out because this crate
/// carries no `libc` dependency, as `paneflow-agent-config` does.
fn open_nonblocking(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    const O_NONBLOCK: i32 = 0x4;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(O_NONBLOCK)
        .open(path)
}

/// Read a UTF-8 config, `None` when it does not exist.
///
/// Same guards as `paneflow_agent_config::read_optional_text` (a regular
/// file only, checked on the open descriptor; a bounded read), with a
/// 64 MiB ceiling instead of that crate's 1 MiB so a large `~/.claude.json`
/// that holds the bridge entry can still be cleaned. Refusals surface as
/// `InvalidData`.
pub(crate) fn read_config(path: &Path) -> Result<Option<String>> {
    read_config_capped(path, MAX_CONFIG_BYTES)
        .with_context(|| format!("read {} failed", path.display()))
}

fn read_config_capped(path: &Path, cap: u64) -> std::io::Result<Option<String>> {
    let file = match open_nonblocking(path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() {
        return Err(std::io::Error::new(
            ErrorKind::InvalidData,
            format!("{} is not a regular file", path.display()),
        ));
    }
    if metadata.len() > cap {
        return Err(too_large(path, metadata.len(), cap));
    }
    let mut content = String::new();
    file.take(cap + 1).read_to_string(&mut content)?;
    if content.len() as u64 > cap {
        return Err(too_large(path, content.len() as u64, cap));
    }
    Ok(Some(content))
}

fn too_large(path: &Path, len: u64, cap: u64) -> std::io::Error {
    std::io::Error::new(
        ErrorKind::InvalidData,
        format!(
            "{} is too large ({len} bytes; maximum {cap})",
            path.display()
        ),
    )
}

/// Could the raw bytes of `path` contain `needle`? `false` only when the
/// whole file was scanned and `needle` never appears, or the file does not
/// exist. The scan streams in chunks, so any size costs constant memory.
/// Anything it cannot read - not a regular file, an open or read error -
/// answers `true`, so the caller goes on to parse (and refuses loudly)
/// instead of treating an unreadable config as free of the bridge.
pub(crate) fn may_contain(path: &Path, needle: &[u8]) -> bool {
    may_contain_in_chunks(path, needle, SCAN_CHUNK_BYTES)
}

fn may_contain_in_chunks(path: &Path, needle: &[u8], chunk: usize) -> bool {
    let mut file = match open_nonblocking(path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return false,
        Err(_) => return true,
    };
    if !file.metadata().is_ok_and(|m| m.file_type().is_file()) {
        return true;
    }
    if needle.is_empty() {
        return true;
    }
    // Keep the last `needle.len() - 1` bytes of each chunk in front of the
    // next one, so a match split across a chunk boundary is still found.
    let keep = needle.len() - 1;
    let mut window: Vec<u8> = Vec::with_capacity(chunk + keep);
    let mut buf = vec![0u8; chunk.max(1)];
    loop {
        let n = match file.read(&mut buf) {
            Ok(0) => return false,
            Ok(n) => n,
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(_) => return true,
        };
        window.extend_from_slice(&buf[..n]);
        if window.windows(needle.len()).any(|w| w == needle) {
            return true;
        }
        let drop = window.len().saturating_sub(keep);
        window.drain(..drop);
    }
}

/// Write `bytes` beside `path` as its backup without replacing any existing
/// file: `<path>.bak` when that name is free (a user's own `.bak` is never
/// overwritten), else `<path>.paneflow-bak`, then `<path>.paneflow-bak.1`,
/// `.2`, … Each name is claimed with an exclusive rename, so a file that
/// appears between the check and the write is not clobbered either.
/// Returns the path written.
fn write_backup(path: &Path, bytes: &[u8]) -> Result<PathBuf> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    let mut tmp = tempfile::NamedTempFile::new_in(&parent)
        .with_context(|| format!("tempfile in {} failed", parent.display()))?;
    std::io::Write::write_all(&mut tmp, bytes).context("write_all to backup tempfile failed")?;
    tmp.as_file_mut()
        .sync_all()
        .context("sync_all on backup tempfile failed")?;

    for candidate in backup_candidates(path).take(MAX_BACKUP_NAMES) {
        match tmp.persist_noclobber(&candidate) {
            Ok(_) => return Ok(candidate),
            Err(error) if error.error.kind() == ErrorKind::AlreadyExists => tmp = error.file,
            Err(error) => {
                return Err(anyhow::anyhow!(
                    "back up {} to {} failed: {}",
                    path.display(),
                    candidate.display(),
                    error.error
                ));
            }
        }
    }
    bail!(
        "back up {} failed: {MAX_BACKUP_NAMES} backup names beside it are taken",
        path.display()
    )
}

/// `<path>.bak`, `<path>.paneflow-bak`, `<path>.paneflow-bak.1`, …
fn backup_candidates(path: &Path) -> impl Iterator<Item = PathBuf> + '_ {
    let with_suffix = move |suffix: String| {
        let mut name = path.as_os_str().to_owned();
        name.push(suffix);
        PathBuf::from(name)
    };
    std::iter::once(".bak".to_string())
        .chain(std::iter::once(".paneflow-bak".to_string()))
        .chain((1..).map(|n| format!(".paneflow-bak.{n}")))
        .map(with_suffix)
}

/// Atomically write `contents` to `path`: temp file in the same directory,
/// flush + fsync, run `before_rename`, then `rename`. An `Err` from
/// `before_rename` abandons the write (the temp file is removed) and leaves
/// `path` untouched.
///
/// A symlinked `path` (stow, chezmoi, yadm) is resolved to its target first so
/// the rename updates the managed file instead of replacing the link with a
/// regular file; a dangling link is refused.
fn write_atomic_checked(
    path: &Path,
    contents: &[u8],
    before_rename: impl FnOnce() -> Result<()>,
) -> Result<()> {
    let target = paneflow_agent_config::io::write_target(path)
        .with_context(|| format!("resolve write target for {} failed", path.display()))?;
    let path = target.as_path();
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    std::fs::create_dir_all(&parent)
        .with_context(|| format!("create parent dir {} failed", parent.display()))?;

    let mut tmp = tempfile::NamedTempFile::new_in(&parent)
        .with_context(|| format!("tempfile in {} failed", parent.display()))?;
    std::io::Write::write_all(&mut tmp, contents).context("write_all to tempfile failed")?;
    // Preserve the existing file's mode: persist replaces the inode, which
    // would otherwise silently reset the user's permissions to the temp
    // file's 0600. A missing target keeps the temp file's 0600 default.
    match std::fs::metadata(path) {
        Ok(metadata) => tmp
            .as_file()
            .set_permissions(metadata.permissions())
            .context("preserve existing file mode failed")?,
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).context(format!("stat {} failed", path.display()));
        }
    }
    tmp.as_file_mut()
        .sync_all()
        .context("sync_all on tempfile failed")?;
    before_rename()?;
    tmp.persist(path).map_err(|e| {
        anyhow::anyhow!("atomic rename into {} failed: {}", path.display(), e.error)
    })?;
    Ok(())
}

/// Refuse unless `path` still holds exactly `parsed`. A missing file counts
/// as changed. Any other read failure is reported as such.
fn ensure_unchanged(path: &Path, parsed: &str) -> Result<()> {
    let current = read_config(path)?;
    if current.as_deref() != Some(parsed) {
        bail!(
            "{} changed while it was being edited; left it as it is (the next launch retries)",
            path.display()
        );
    }
    Ok(())
}

/// Replace `path`, whose current bytes the caller parsed as `parsed`, with
/// `contents`. Assumes the caller already holds the config lock for `path`.
///
/// Refuses, writing nothing, when a check finds that the file no longer
/// holds `parsed`: once before the backup and again immediately before the
/// rename (a backup this call already wrote is removed again). The checks
/// narrow the race with a writer that skips PaneFlow's lock, such as Claude
/// Code, but do not close it: a write that lands after the second check and
/// before the rename is replaced, and the backup holds `parsed`, not that
/// write. On success the returned path is the backup that holds `parsed`
/// (see [`write_backup`] for its name).
pub(crate) fn replace_unchanged_unlocked(
    path: &Path,
    parsed: &str,
    contents: &str,
) -> Result<PathBuf> {
    ensure_unchanged(path, parsed)?;
    let backup = write_backup(path, parsed.as_bytes())?;
    let written = write_atomic_checked(path, contents.as_bytes(), || {
        #[cfg(test)]
        run_before_rename_hook();
        ensure_unchanged(path, parsed)
    });
    if let Err(error) = written {
        // Nothing was replaced, so the backup would only be clutter.
        if let Err(remove) = std::fs::remove_file(&backup) {
            log::warn!(
                "mcp bridge cleanup: could not remove the unused backup {} ({remove})",
                backup.display()
            );
        }
        return Err(error);
    }
    Ok(backup)
}

#[cfg(test)]
thread_local! {
    static BEFORE_RENAME_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        std::cell::RefCell::new(None);
}

/// Test seam: run `hook` inside the next [`replace_unchanged_unlocked`] on
/// this thread, after the backup and temp file are written and right before
/// the final re-read, to simulate another process rewriting the config.
#[cfg(test)]
pub(crate) fn set_before_rename_hook(hook: impl FnOnce() + 'static) {
    BEFORE_RENAME_HOOK.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
}

#[cfg(test)]
fn run_before_rename_hook() {
    if let Some(hook) = BEFORE_RENAME_HOOK.with(|slot| slot.borrow_mut().take()) {
        hook();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::time::{Duration, Instant};

    /// Mirror of `paneflow_agent_config::lock::LOCK_TIMEOUT` (5s). That
    /// constant is private, so this value can drift if the agent-config
    /// timeout changes.
    const LOCK_TIMEOUT: Duration = Duration::from_secs(5);

    fn write_atomic(path: &Path, contents: &[u8]) -> Result<()> {
        write_atomic_checked(path, contents, || Ok(()))
    }

    fn names_in(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn write_atomic_creates_file_and_parents() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("nested").join("deep").join("config.json");
        write_atomic(&p, b"hello").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"hello");
    }

    #[test]
    fn write_atomic_updates_a_symlinked_config_through_the_link() {
        let dir = tempfile::TempDir::new().unwrap();
        let target = dir.path().join("managed.json");
        let link = dir.path().join("config.json");
        std::fs::write(&target, b"{}\n").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        write_atomic(&link, b"updated").unwrap();

        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink(),
            "the write must update the managed target, not replace the symlink"
        );
        assert_eq!(std::fs::read(&target).unwrap(), b"updated");
    }

    #[test]
    fn write_atomic_preserves_the_existing_mode_through_a_symlink() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::TempDir::new().unwrap();
        let target = dir.path().join("managed.json");
        let link = dir.path().join("config.json");
        std::fs::write(&target, b"{}\n").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o640)).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        write_atomic(&link, b"updated").unwrap();

        assert_eq!(std::fs::read(&target).unwrap(), b"updated");
        assert_eq!(
            std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o640
        );
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink(),
            "the write must update the managed target, not replace the symlink"
        );
    }

    #[test]
    fn write_atomic_refuses_a_dangling_symlink() {
        let dir = tempfile::TempDir::new().unwrap();
        let missing = dir.path().join("missing.json");
        let link = dir.path().join("config.json");
        std::os::unix::fs::symlink(&missing, &link).unwrap();

        write_atomic(&link, b"content").unwrap_err();

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
    fn read_config_allows_files_past_the_shim_cap_and_refuses_past_its_own() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("big.json");
        let two_mib = " ".repeat(2 << 20);
        std::fs::write(&p, &two_mib).unwrap();
        assert!(
            paneflow_agent_config::read_optional_text(&p).is_err(),
            "the shim's 1 MiB cap stays in place"
        );
        assert_eq!(read_config(&p).unwrap().unwrap().len(), two_mib.len());

        assert_eq!(MAX_CONFIG_BYTES, 64 << 20);
        let err = read_config_capped(&p, 1 << 20).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidData);
        assert_eq!(
            err.to_string(),
            format!(
                "{} is too large ({} bytes; maximum 1048576)",
                p.display(),
                two_mib.len()
            )
        );
        assert!(read_config(&dir.path().join("missing.json"))
            .unwrap()
            .is_none());
    }

    #[test]
    fn read_config_refuses_a_fifo_without_blocking() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blocked");
        assert!(std::process::Command::new("/usr/bin/mkfifo")
            .arg(&path)
            .status()
            .unwrap()
            .success());
        let (tx, rx) = std::sync::mpsc::channel();
        let probe = path.clone();
        let worker = std::thread::spawn(move || {
            tx.send(read_config(&probe).is_err() && may_contain(&probe, b"x"))
                .unwrap()
        });
        let early = rx.recv_timeout(Duration::from_secs(1));
        if early.is_err() {
            use std::os::unix::fs::OpenOptionsExt;
            // Unblock a regressed reader so a failed assertion cannot strand it.
            drop(
                std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .custom_flags(4)
                    .open(&path)
                    .unwrap(),
            );
        }
        assert!(early.unwrap(), "a FIFO is refused and counts as a maybe");
        worker.join().unwrap();
    }

    #[test]
    fn may_contain_finds_a_needle_split_across_chunks_and_rules_out_a_clean_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("config.json");
        let needle = b"paneflow-mcp";
        // The needle starts 5 bytes before the end of the first 16-byte chunk.
        let mut bytes = vec![b'x'; 11];
        bytes.extend_from_slice(needle);
        bytes.extend(std::iter::repeat_n(b'y', 40));
        std::fs::write(&p, &bytes).unwrap();
        assert!(may_contain_in_chunks(&p, needle, 16));
        for chunk in [1, 7, 12, 13, 64] {
            assert!(may_contain_in_chunks(&p, needle, chunk), "{chunk}");
        }

        std::fs::write(
            &p,
            "{\"mcpServers\":{\"paneflow\":{\"command\":\"/x/paneflow-mc\"}}}",
        )
        .unwrap();
        assert!(!may_contain_in_chunks(&p, needle, 16));
        assert!(!may_contain(&dir.path().join("missing.json"), needle));
    }

    #[test]
    fn may_contain_answers_maybe_only_for_what_it_cannot_read() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::TempDir::new().unwrap();
        assert!(may_contain(dir.path(), b"paneflow-mcp"), "a directory");

        // A file larger than any read cap is still scanned to the end.
        let p = dir.path().join("config.json");
        let mut big = vec![b'x'; (65 << 20) + 3];
        assert!(!may_contain(&p, b"paneflow-mcp"), "missing");
        std::fs::write(&p, &big).unwrap();
        assert!(!may_contain(&p, b"paneflow-mcp"));
        big.extend_from_slice(b"paneflow-mcp");
        std::fs::write(&p, &big).unwrap();
        assert!(may_contain(&p, b"paneflow-mcp"));

        std::fs::write(&p, b"nothing here").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read(&p).is_err() {
            assert!(may_contain(&p, b"paneflow-mcp"), "an unreadable file");
        }
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
    }

    #[test]
    fn replace_unchanged_writes_and_backs_up_the_parsed_bytes() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("config.json");
        std::fs::write(&p, b"old").unwrap();

        let backup = replace_unchanged_unlocked(&p, "old", "new").unwrap();

        assert_eq!(backup, dir.path().join("config.json.bak"));
        assert_eq!(std::fs::read(&p).unwrap(), b"new");
        assert_eq!(
            std::fs::read(&backup).unwrap(),
            b"old",
            "backup must hold the pre-write contents"
        );
    }

    #[test]
    fn replace_unchanged_never_overwrites_an_existing_backup() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("config.json");
        std::fs::write(dir.path().join("config.json.bak"), b"the user's backup").unwrap();

        std::fs::write(&p, b"first").unwrap();
        let backup = replace_unchanged_unlocked(&p, "first", "second").unwrap();
        assert_eq!(backup, dir.path().join("config.json.paneflow-bak"));
        assert_eq!(std::fs::read(&backup).unwrap(), b"first");

        let backup = replace_unchanged_unlocked(&p, "second", "third").unwrap();
        assert_eq!(backup, dir.path().join("config.json.paneflow-bak.1"));
        assert_eq!(std::fs::read(&backup).unwrap(), b"second");

        assert_eq!(
            std::fs::read(dir.path().join("config.json.bak")).unwrap(),
            b"the user's backup"
        );
        assert_eq!(
            std::fs::read(dir.path().join("config.json.paneflow-bak")).unwrap(),
            b"first"
        );
        assert_eq!(std::fs::read(&p).unwrap(), b"third");
    }

    #[test]
    fn a_dangling_bak_symlink_is_not_followed_or_replaced() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("config.json");
        let elsewhere = dir.path().join("elsewhere");
        std::os::unix::fs::symlink(&elsewhere, dir.path().join("config.json.bak")).unwrap();
        std::fs::write(&p, b"old").unwrap();

        let backup = replace_unchanged_unlocked(&p, "old", "new").unwrap();

        assert_eq!(backup, dir.path().join("config.json.paneflow-bak"));
        assert!(!elsewhere.exists(), "the link target must not be created");
    }

    #[test]
    fn replace_unchanged_refuses_a_file_that_differs_from_the_parse() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("config.json");
        std::fs::write(&p, b"rewritten by the agent").unwrap();

        let err = replace_unchanged_unlocked(&p, "what we parsed", "new").unwrap_err();

        assert!(format!("{err:#}").contains("changed"), "{err:#}");
        assert_eq!(std::fs::read(&p).unwrap(), b"rewritten by the agent");
        assert_eq!(names_in(dir.path()), ["config.json"]);
    }

    #[test]
    fn a_file_changed_right_before_the_rename_is_left_and_its_new_backup_removed() {
        // The first check passes; another process rewrites the file after
        // the backup and temp file are written. The final re-read must catch
        // it, leave the other process's bytes in place, and remove the
        // backup this call made - while a user's existing `.bak` stays.
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("config.json");
        std::fs::write(dir.path().join("config.json.bak"), b"the user's backup").unwrap();
        std::fs::write(&p, b"old").unwrap();
        let racer = p.clone();
        set_before_rename_hook(move || std::fs::write(&racer, b"claude wrote this").unwrap());

        let err = replace_unchanged_unlocked(&p, "old", "new").unwrap_err();

        assert!(format!("{err:#}").contains("changed"), "{err:#}");
        assert_eq!(std::fs::read(&p).unwrap(), b"claude wrote this");
        assert_eq!(names_in(dir.path()), ["config.json", "config.json.bak"]);
        assert_eq!(
            std::fs::read(dir.path().join("config.json.bak")).unwrap(),
            b"the user's backup"
        );
    }

    #[test]
    fn replace_unchanged_refuses_a_deleted_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("config.json");

        replace_unchanged_unlocked(&p, "old", "new").unwrap_err();

        assert!(!p.exists(), "a vanished config must not be recreated");
    }

    #[test]
    fn replace_unchanged_refuses_an_unreadable_file_before_backup() {
        use std::os::unix::fs::PermissionsExt;

        // Root reads a 0o000 file fine, so the probe below is meaningless.
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("config.json");
        std::fs::write(&p, b"old").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read(&p).is_ok() {
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
            eprintln!("skipping: process can read a 0o000 file (running as root?)");
            return;
        }

        let err = replace_unchanged_unlocked(&p, "old", "new").unwrap_err();

        let msg = err.to_string();
        assert!(
            msg.starts_with("read ") && !msg.contains("back up"),
            "a read failure must be reported as such, not fall through to backup: {err:#}"
        );
        let io_kind = err
            .chain()
            .find_map(|e| e.downcast_ref::<std::io::Error>().map(std::io::Error::kind));
        assert_eq!(io_kind, Some(ErrorKind::PermissionDenied));

        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            std::fs::read(&p).unwrap(),
            b"old",
            "original must be untouched"
        );
        assert_eq!(
            names_in(dir.path()),
            ["config.json"],
            "no backup may be attempted for a file that could not be read"
        );
    }

    /// OS advisory lock is released on process death, including SIGKILL.
    ///
    /// `paneflow_agent_config::lock_config` uses one process-global lockfile
    /// (`<config_dir>/paneflow/agent-config.lock`), so this test contends
    /// with every other installer/shim test that acquires the same lock.
    /// Keep the hold window tiny: the child prints `LOCKED` and self-exits
    /// after 500 ms if the parent has not SIGKILL'd it yet.
    #[test]
    fn lock_survives_a_crashed_holder() {
        const CHILD_TARGET: &str = "PANE_FLOW_LOCK_CHILD_TARGET";
        if let Some(target) = std::env::var_os(CHILD_TARGET) {
            let _lock = lock_config(Path::new(&target)).expect("child acquire");
            eprintln!("LOCKED");
            let _ = std::io::stderr().flush();
            std::thread::sleep(Duration::from_millis(500));
            std::process::exit(0);
        }

        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("config.json");
        std::fs::write(&p, b"{}").unwrap();

        let exe = std::env::current_exe().unwrap();
        let mut child = std::process::Command::new(&exe)
            .env(CHILD_TARGET, &p)
            .args([
                "--exact",
                "io::tests::lock_survives_a_crashed_holder",
                "--nocapture",
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn lock-holder child");

        let mut stderr = child.stderr.take().expect("piped stderr");
        let mut buf = String::new();
        let wait_start = Instant::now();
        loop {
            if wait_start.elapsed() > LOCK_TIMEOUT {
                let _ = child.kill();
            }
            assert!(
                wait_start.elapsed() <= LOCK_TIMEOUT,
                "child did not acquire lock within LOCK_TIMEOUT: {buf}"
            );
            let mut tmp = [0u8; 64];
            let n = match stderr.read(&mut tmp) {
                Ok(n) => n,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => {
                    assert_eq!(e.kind(), ErrorKind::Interrupted, "read child stderr: {e}");
                    continue;
                }
            };
            assert!(n > 0, "child exited before acquiring lock: {buf}");
            buf.push_str(&String::from_utf8_lossy(&tmp[..n]));
            if buf.contains("LOCKED") {
                break;
            }
        }

        child.kill().expect("SIGKILL child");
        let _ = child.wait();

        let acquire_start = Instant::now();
        lock_config(&p).expect("a SIGKILLed holder must not strand the lock");
        assert!(
            acquire_start.elapsed() < LOCK_TIMEOUT,
            "parent must acquire within LOCK_TIMEOUT after SIGKILL"
        );
    }
}
