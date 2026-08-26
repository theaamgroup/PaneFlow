//! Safe-write primitives (EP-002 US-006).
//!
//! Every agent-config writer goes through [`write_if_changed`], which is:
//! - **idempotent** - a write only happens when the bytes actually differ,
//!   so a re-run of `paneflow mcp install` produces zero disk churn (no
//!   mtime bump, no backup spam);
//! - **backed up** - the previous contents are copied to `<file>.bak`
//!   *before* the new bytes land, and a backup failure aborts the write
//!   (we never modify the original if we could not preserve it first);
//! - **atomic** - bytes are written to a temp file in the same directory
//!   and `rename`d into place, mirroring `session.rs`'s tmp+rename pattern.
//!   A crash mid-write leaves the temp file, never a half-written config.

use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

const LOCK_TIMEOUT: Duration = Duration::from_secs(5);
const LOCK_RETRY: Duration = Duration::from_millis(50);
/// Match the shim's stale-lock steal window (`STALE_CONFIG_LOCK_AFTER`).
/// Caps PID-reuse: a live PID in an old lockfile is not trusted forever.
const STALE_LOCK_AFTER: Duration = Duration::from_secs(60);

/// Best-effort inter-process lock for one config file. Paneflow writers honor
/// it before any read-modify-write pass, so two Paneflow invocations cannot
/// race each other and lose an edit between read and atomic persist.
pub struct ConfigLock {
    path: PathBuf,
    _file: File,
}

impl Drop for ConfigLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn lock_path(path: &Path) -> PathBuf {
    let mut lock = path.as_os_str().to_owned();
    lock.push(".lock");
    PathBuf::from(lock)
}

/// Acquire the Paneflow lock for `path`, retrying briefly if another
/// Paneflow process is already editing the same config.
pub fn lock_config(path: &Path) -> Result<ConfigLock> {
    lock_config_with_timeout(path, LOCK_TIMEOUT)
}

fn lock_config_with_timeout(path: &Path, timeout: Duration) -> Result<ConfigLock> {
    lock_config_with(path, timeout, STALE_LOCK_AFTER)
}

fn lock_config_with(path: &Path, timeout: Duration, stale_after: Duration) -> Result<ConfigLock> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    std::fs::create_dir_all(&parent)
        .with_context(|| format!("create parent dir {} failed", parent.display()))?;

    let lock = lock_path(path);
    let deadline = Instant::now() + timeout;
    loop {
        match OpenOptions::new().write(true).create_new(true).open(&lock) {
            Ok(mut file) => match write_holder_pid(&mut file) {
                Ok(()) => {
                    return Ok(ConfigLock {
                        path: lock,
                        _file: file,
                    });
                }
                Err(e) => {
                    let _ = std::fs::remove_file(&lock);
                    return Err(e).with_context(|| {
                        format!("write holder pid into {} failed", lock.display())
                    });
                }
            },
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                if try_steal_lock(&lock, stale_after) {
                    continue;
                }
                if Instant::now() < deadline {
                    std::thread::sleep(LOCK_RETRY);
                } else {
                    anyhow::bail!("timed out waiting for config lock {}", lock.display());
                }
            }
            Err(e) => return Err(e).with_context(|| format!("lock {} failed", lock.display())),
        }
    }
}

fn write_holder_pid(file: &mut File) -> std::io::Result<()> {
    file.write_all(std::process::id().to_string().as_bytes())
}

fn read_lock_pid(path: &Path) -> Option<u32> {
    let raw = std::fs::read_to_string(path).ok()?;
    let pid: u32 = raw.trim().parse().ok()?;
    (pid > 0).then_some(pid)
}

fn pid_is_alive(pid: u32) -> bool {
    if pid == 0 || pid > i32::MAX as u32 {
        return false;
    }
    // SAFETY: `libc::kill` with sig=0 performs error-checking only and
    // does not deliver a signal. The call takes an i32 pid by value and
    // has no memory aliasing requirements.
    let ret = unsafe { libc::kill(pid as i32, 0) };
    if ret == -1 {
        let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
        // ESRCH = no such process. EPERM/etc. => process exists but we
        // can't signal it; treat as alive so we do not steal.
        return errno != libc::ESRCH;
    }
    true
}

fn lock_mtime_age(path: &Path) -> Option<Duration> {
    path.metadata().ok()?.modified().ok()?.elapsed().ok()
}

/// Steal if the recorded PID is dead (or missing/unparseable) **or** the
/// lockfile is older than `stale_after`. Never steal a live PID whose
/// mtime is still within the stale window.
fn try_steal_lock(path: &Path, stale_after: Duration) -> bool {
    let aged_out = lock_mtime_age(path).is_some_and(|age| age > stale_after);
    let pid_dead = match read_lock_pid(path) {
        Some(pid) => !pid_is_alive(pid),
        None => true,
    };
    (aged_out || pid_dead) && std::fs::remove_file(path).is_ok()
}

/// Run a closure while holding the Paneflow config lock for `path`.
pub fn with_config_lock<T>(path: &Path, f: impl FnOnce() -> Result<T>) -> Result<T> {
    let _lock = lock_config(path)?;
    f()
}

/// Copy `path` to `path` + `.bak` when it exists. Returns the backup path
/// (or `None` if the original did not exist - nothing to preserve).
///
/// A copy failure is an error: callers MUST abort the write rather than
/// risk clobbering a config they could not back up first (US-006 AC).
pub fn backup(path: &Path) -> Result<Option<PathBuf>> {
    if !path.exists() {
        return Ok(None);
    }
    let mut bak = path.as_os_str().to_owned();
    bak.push(".bak");
    let bak = PathBuf::from(bak);
    std::fs::copy(path, &bak)
        .with_context(|| format!("backup {} -> {} failed", path.display(), bak.display()))?;
    Ok(Some(bak))
}

/// Atomically write `contents` to `path`: temp file in the same directory,
/// flush + fsync, then `rename`. The rename is atomic on POSIX and on
/// Windows NTFS (`MoveFileEx` semantics inside `persist`).
pub fn write_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    std::fs::create_dir_all(&parent)
        .with_context(|| format!("create parent dir {} failed", parent.display()))?;

    let mut tmp = tempfile::NamedTempFile::new_in(&parent)
        .with_context(|| format!("tempfile in {} failed", parent.display()))?;
    std::io::Write::write_all(&mut tmp, contents).context("write_all to tempfile failed")?;
    tmp.as_file_mut()
        .sync_all()
        .context("sync_all on tempfile failed")?;
    tmp.persist(path).map_err(|e| {
        anyhow::anyhow!("atomic rename into {} failed: {}", path.display(), e.error)
    })?;
    Ok(())
}

/// Backup-then-atomic-write `contents` to `path`, **only if** the bytes
/// differ from what is already on disk.
///
/// Returns `true` when a write happened, `false` when the on-disk bytes
/// already matched (a no-op - no backup, no rename, no mtime change). This
/// is the idempotency knob every writer relies on.
pub fn write_if_changed(path: &Path, contents: &[u8]) -> Result<bool> {
    with_config_lock(path, || write_if_changed_unlocked(path, contents))
}

/// Same as [`write_if_changed`], but assumes the caller already holds
/// [`ConfigLock`] for `path`.
pub(crate) fn write_if_changed_unlocked(path: &Path, contents: &[u8]) -> Result<bool> {
    // Edition 2021 (workspace default) - no let-chains, so nest the guard.
    if let Ok(existing) = std::fs::read(path) {
        if existing == contents {
            return Ok(false);
        }
    }
    // Bytes differ (or the file is absent / unreadable): back up the old
    // contents first, then publish the new bytes atomically.
    backup(path)?;
    write_atomic(path, contents)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backup_noop_when_absent() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("missing.json");
        assert_eq!(backup(&p).unwrap(), None);
    }

    #[test]
    fn backup_copies_existing() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("config.json");
        std::fs::write(&p, b"original").unwrap();
        let bak = backup(&p).unwrap().unwrap();
        assert_eq!(std::fs::read(&bak).unwrap(), b"original");
        // Original untouched.
        assert_eq!(std::fs::read(&p).unwrap(), b"original");
    }

    #[test]
    fn write_atomic_creates_file_and_parents() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("nested").join("deep").join("config.json");
        write_atomic(&p, b"hello").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"hello");
    }

    #[test]
    fn write_if_changed_is_noop_when_identical() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("config.json");
        std::fs::write(&p, b"same").unwrap();
        let mtime_before = std::fs::metadata(&p).unwrap().modified().unwrap();

        std::thread::sleep(std::time::Duration::from_millis(50));
        let wrote = write_if_changed(&p, b"same").unwrap();

        assert!(!wrote, "identical bytes must not be rewritten");
        let mtime_after = std::fs::metadata(&p).unwrap().modified().unwrap();
        assert_eq!(mtime_before, mtime_after, "no-op must not bump mtime");
        // No backup was created on the no-op path.
        let mut bak = p.as_os_str().to_owned();
        bak.push(".bak");
        assert!(!PathBuf::from(bak).exists(), "no-op must not write a .bak");
    }

    #[test]
    fn write_if_changed_writes_and_backs_up_on_diff() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("config.json");
        std::fs::write(&p, b"old").unwrap();

        let wrote = write_if_changed(&p, b"new").unwrap();
        assert!(wrote);
        assert_eq!(std::fs::read(&p).unwrap(), b"new");

        let mut bak = p.as_os_str().to_owned();
        bak.push(".bak");
        assert_eq!(
            std::fs::read(PathBuf::from(bak)).unwrap(),
            b"old",
            "backup must hold the pre-write contents"
        );
    }

    #[test]
    fn write_if_changed_creates_new_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("fresh.json");
        let wrote = write_if_changed(&p, b"data").unwrap();
        assert!(wrote);
        assert_eq!(std::fs::read(&p).unwrap(), b"data");
    }

    fn dead_child_pid() -> u32 {
        let mut child = std::process::Command::new("/usr/bin/true")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id();
        let _ = child.wait();
        assert!(!pid_is_alive(pid), "reaped child must not still be alive");
        pid
    }

    #[test]
    fn lock_config_steals_dead_pid() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("config.json");
        let lock = lock_path(&p);
        std::fs::write(&lock, format!("{}\n", dead_child_pid())).unwrap();

        let acquired = lock_config(&p).unwrap();
        assert_eq!(
            std::fs::read_to_string(&lock).unwrap(),
            std::process::id().to_string()
        );
        drop(acquired);
        assert!(
            !lock.exists(),
            "Drop must unlink the lock after a stolen acquire"
        );
    }

    #[test]
    fn lock_config_times_out_on_live_pid() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("config.json");
        let lock = lock_path(&p);
        let pid = std::process::id();
        std::fs::write(&lock, pid.to_string()).unwrap();

        let timeout = Duration::from_millis(150);
        let err = lock_config_with_timeout(&p, timeout)
            .err()
            .expect("live holder must not be stolen");
        assert!(
            err.to_string()
                .contains("timed out waiting for config lock"),
            "live holder must not be stolen: {err}"
        );
        assert!(lock.exists(), "must not unlink a live lock on timeout");
        assert_eq!(std::fs::read_to_string(&lock).unwrap(), pid.to_string());
    }

    #[test]
    fn lock_config_drop_unlinks_and_reacquire_works() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("config.json");
        let lock = lock_path(&p);

        {
            let _held = lock_config(&p).unwrap();
            assert!(lock.exists());
            assert_eq!(
                std::fs::read_to_string(&lock).unwrap(),
                std::process::id().to_string()
            );
        }
        assert!(!lock.exists(), "Drop must unlink the lockfile");

        let _held_again = lock_config(&p).unwrap();
        assert!(lock.exists());
        assert_eq!(
            std::fs::read_to_string(&lock).unwrap(),
            std::process::id().to_string()
        );
    }

    #[test]
    fn lock_config_steals_aged_lock_with_live_pid() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("config.json");
        let lock = lock_path(&p);
        std::fs::write(&lock, std::process::id().to_string()).unwrap();
        std::thread::sleep(Duration::from_millis(5));

        let acquired =
            lock_config_with(&p, Duration::from_secs(1), Duration::from_millis(1)).unwrap();
        assert_eq!(
            std::fs::read_to_string(&lock).unwrap(),
            std::process::id().to_string()
        );
        drop(acquired);
        assert!(!lock.exists());
    }

    #[test]
    fn lock_config_steals_empty_lockfile() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("config.json");
        let lock = lock_path(&p);
        std::fs::write(&lock, b"").unwrap();

        let acquired = lock_config(&p).unwrap();
        assert_eq!(
            std::fs::read_to_string(&lock).unwrap(),
            std::process::id().to_string()
        );
        drop(acquired);
    }
}
