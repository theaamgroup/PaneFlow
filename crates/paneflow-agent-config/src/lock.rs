use std::fs::{File, OpenOptions};
use std::io::{Error, ErrorKind, Result};
use std::path::{Path, PathBuf};
use std::time::Duration;

const LOCK_TIMEOUT: Duration = Duration::from_secs(5);
/// Cross-process lock for Paneflow's agent-configuration mutations.
///
/// The file is intentionally persistent. The operating system owns the
/// actual lock and releases it when its last descriptor closes, including
/// after a crash. Dropping the guard explicitly unlocks it even if a forked
/// child still holds an inherited descriptor. Keeping one global lock outside
/// agent-owned directories also lets ephemeral `.claude` and `.codex`
/// directories be removed safely.
pub struct ConfigLock {
    file: File,
}

impl Drop for ConfigLock {
    fn drop(&mut self) {
        // flock is shared by duplicated/inherited descriptors. Closing only our
        // File can leave the lock held until a forked child closes its copy.
        // Drop cannot return an error; File's close remains the fallback.
        let _ = self.file.unlock();
    }
}

fn lock_path() -> Result<PathBuf> {
    let config_dir = dirs::config_dir().ok_or_else(|| {
        Error::new(
            ErrorKind::NotFound,
            "could not resolve the user configuration directory",
        )
    })?;
    let paneflow_dir = config_dir.join("paneflow");
    std::fs::create_dir_all(&paneflow_dir)?;
    Ok(paneflow_dir.join("agent-config.lock"))
}

/// Acquire the shared Paneflow lock for `path`.
///
/// All agent configurations share one lock because their read-modify-write
/// sections are short. This avoids per-project lockfile debris while retaining
/// correct crash recovery on macOS (kernel-released flock).
pub fn lock_config(path: &Path) -> Result<ConfigLock> {
    acquire_lock(&lock_path()?, path, LOCK_TIMEOUT)
}

fn acquire_lock(lock_path: &Path, target: &Path, timeout: Duration) -> Result<ConfigLock> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)?;
    let file = lock_within(file, LockKind::Exclusive, timeout, || {
        format!(
            "timed out waiting for the PaneFlow config lock while editing {}",
            target.display()
        )
    })?;
    Ok(ConfigLock { file })
}

#[derive(Clone, Copy)]
pub(crate) enum LockKind {
    Exclusive,
    Shared,
}

/// Take an OS file lock, waiting at most `timeout`.
///
/// The wait is a blocking `flock` on a helper thread, so contenders sit in
/// the kernel's wait queue and are served in turn. A polled `try_lock` loop
/// (the previous shape) is not fair: under disk load, where every locked
/// section ends in an `F_FULLFSYNC`, a waiter could be starved past the
/// deadline while later arrivals kept winning the race, which surfaced as
/// spurious `TimedOut` errors in the shim's test suite. If the deadline
/// passes first, the helper keeps waiting and simply closes the descriptor
/// once it gets the lock, so nothing leaks past the holder's own lifetime.
pub(crate) fn lock_within(
    file: File,
    kind: LockKind,
    timeout: Duration,
    timed_out: impl FnOnce() -> String,
) -> Result<File> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let outcome = match kind {
            LockKind::Exclusive => file.lock(),
            LockKind::Shared => file.lock_shared(),
        };
        // A receiver that gave up has dropped `rx`; the file drops here and
        // its close releases the lock we just took.
        let _ = tx.send(outcome.map(|()| file));
    });
    match rx.recv_timeout(timeout) {
        Ok(result) => result,
        Err(_) => Err(Error::new(ErrorKind::TimedOut, timed_out())),
    }
}

/// Run `operation` while holding the shared lock for `path`.
pub fn with_config_lock<T>(path: &Path, operation: impl FnOnce() -> Result<T>) -> Result<T> {
    let _lock = lock_config(path)?;
    operation()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::TryLockError;

    #[test]
    fn lock_is_exclusive_and_released_on_drop() {
        let dir = tempfile_path();
        let config = dir.join("settings.json");
        std::fs::create_dir_all(&dir).unwrap();
        let lock_path = dir.join("agent-config.lock");
        let first = acquire_lock(&lock_path, &config, Duration::from_secs(1)).unwrap();
        let contender = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lock_path)
            .unwrap();
        assert!(matches!(
            contender.try_lock(),
            Err(TryLockError::WouldBlock)
        ));
        drop(first);
        contender.try_lock().unwrap();
        drop(contender);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn drop_releases_lock_while_duplicate_descriptor_remains_open() {
        let dir = tempfile_path();
        let config = dir.join("settings.json");
        std::fs::create_dir_all(&dir).unwrap();
        let lock_path = dir.join("agent-config.lock");
        let first = acquire_lock(&lock_path, &config, Duration::from_secs(1)).unwrap();
        // Model a descriptor inherited across fork without racing process startup.
        let duplicate = first.file.try_clone().unwrap();
        let contender = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lock_path)
            .unwrap();
        assert!(matches!(
            contender.try_lock(),
            Err(TryLockError::WouldBlock)
        ));
        drop(first);
        contender.try_lock().unwrap();

        // Closing the old description must not release the next owner's lock.
        drop(duplicate);
        let observer = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lock_path)
            .unwrap();
        assert!(matches!(observer.try_lock(), Err(TryLockError::WouldBlock)));
        contender.unlock().unwrap();
        drop(contender);
        observer.try_lock().unwrap();
        drop(observer);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn unlocked_persistent_lockfile_is_recoverable() {
        let dir = tempfile_path();
        let config = dir.join("settings.json");
        std::fs::create_dir_all(&dir).unwrap();
        let lock_path = dir.join("agent-config.lock");
        std::fs::write(&lock_path, b"left by a terminated process").unwrap();
        let first = acquire_lock(&lock_path, &config, Duration::from_secs(1)).unwrap();
        drop(first);
        let second = acquire_lock(&lock_path, &config, Duration::from_secs(1)).unwrap();
        drop(second);
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn tempfile_path() -> PathBuf {
        std::env::temp_dir().join(format!(
            "paneflow-agent-config-lock-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ))
    }

    #[test]
    fn waiting_contender_is_served_when_the_holder_releases() {
        let dir = tempfile_path();
        let config = dir.join("settings.json");
        std::fs::create_dir_all(&dir).unwrap();
        let lock_path = dir.join("agent-config.lock");
        let first = acquire_lock(&lock_path, &config, Duration::from_secs(1)).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let contender = {
            let lock_path = lock_path.clone();
            let config = config.clone();
            std::thread::spawn(move || {
                tx.send(acquire_lock(&lock_path, &config, Duration::from_secs(5)).map(|_| ()))
                    .unwrap();
            })
        };
        std::thread::sleep(Duration::from_millis(100));
        assert!(rx.try_recv().is_err(), "contender must block while held");
        drop(first);
        rx.recv_timeout(Duration::from_secs(2))
            .expect("contender must be served once the holder releases")
            .unwrap();
        contender.join().unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn acquire_times_out_and_the_late_lock_is_released() {
        let dir = tempfile_path();
        let config = dir.join("settings.json");
        std::fs::create_dir_all(&dir).unwrap();
        let lock_path = dir.join("agent-config.lock");
        let first = acquire_lock(&lock_path, &config, Duration::from_secs(1)).unwrap();
        let kind = acquire_lock(&lock_path, &config, Duration::from_millis(200))
            .map(|_| None)
            .unwrap_or_else(|error| Some(error.kind()));
        assert_eq!(kind, Some(ErrorKind::TimedOut));
        drop(first);
        // The abandoned helper thread takes and immediately releases the
        // lock, so a fresh acquisition succeeds promptly.
        let second = acquire_lock(&lock_path, &config, Duration::from_secs(2)).unwrap();
        drop(second);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
