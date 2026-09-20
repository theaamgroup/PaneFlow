use std::fs::{File, OpenOptions, TryLockError};
use std::io::{Error, ErrorKind, Result};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const LOCK_TIMEOUT: Duration = Duration::from_secs(5);

/// Crash-safe lifetime lease for an agent configuration resource.
///
/// Each live session holds a shared OS lock. Cleanup upgrades to an exclusive
/// lock only after the final shared holder exits. The kernel releases locks on
/// process termination, so a killed shim cannot strand a stale lease marker.
///
/// The lock file carries no payload. Windows shared locks forbid writes to the
/// locked range for *every* process, the lock holder included, so writing the
/// ownership bit into the locked file fails with `ERROR_LOCK_VIOLATION` there.
/// The bit is therefore a sibling file: its presence is the ownership
/// state, and it may carry a small PaneFlow-written note
/// (`mark_created_with`) for state that must never live in a
/// user-editable file.
pub struct ConfigLease {
    file: Option<File>,
    marker: PathBuf,
    path: PathBuf,
}

pub struct LastConfigLease {
    /// Held for its exclusive lock, never read: dropping it releases the lock.
    _file: File,
    marker: PathBuf,
    path: PathBuf,
}

impl ConfigLease {
    pub fn acquire(resource: &Path) -> Result<Self> {
        let path = lease_path(resource)?;
        let marker = path.with_extension("created");
        let started = Instant::now();
        let file = loop {
            let remaining = LOCK_TIMEOUT.checked_sub(started.elapsed()).ok_or_else(|| {
                Error::new(
                    ErrorKind::TimedOut,
                    "timed out acquiring a current config lease",
                )
            })?;
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&path)?;
            if let Some(file) = lock_current_file(&path, file, remaining)? {
                break file;
            }
        };
        Ok(Self {
            file: Some(file),
            marker,
            path,
        })
    }

    /// Release this session's shared lock and become the exclusive last owner.
    /// `None` means another live session still owns the resource.
    pub fn try_take_last(&mut self) -> Result<Option<LastConfigLease>> {
        let Some(file) = self.file.take() else {
            return Ok(None);
        };
        file.unlock()?;
        match file.try_lock() {
            Ok(()) if file_is_current(&file, &self.path)? => Ok(Some(LastConfigLease {
                _file: file,
                marker: self.marker.clone(),
                path: self.path.clone(),
            })),
            Ok(()) => Ok(None),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(error)) => Err(error),
        }
    }

    /// Whether the durable ownership bit is set for the leased resource:
    /// a non-consuming peek for callers that must read a managed file
    /// only when PaneFlow created it, leaving the bit for the eventual last
    /// owner to consume.
    pub fn is_created(&self) -> bool {
        self.marker.exists()
    }

    /// Persist that the leased resource was created by PaneFlow.
    ///
    /// Callers serialize this update with their configuration lock. The bit
    /// survives process crashes and is consumed by the eventual last owner.
    pub fn mark_created(&mut self) -> Result<()> {
        self.mark_created_with("")
    }

    /// [`Self::mark_created`] with a note stored in the marker itself: a
    /// small record only PaneFlow writes (the marker lives under
    /// PaneFlow's own configuration directory, keyed by the resource
    /// path), for state that must survive the session that recorded it
    /// and must never be read from a user-editable file.
    pub fn mark_created_with(&mut self, note: &str) -> Result<()> {
        use std::io::Write;
        if self.file.is_none() {
            return Err(Error::new(
                ErrorKind::BrokenPipe,
                "configuration lease was already released",
            ));
        }
        let mut marker = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&self.marker)?;
        marker.write_all(note.as_bytes())?;
        marker.sync_all()
    }

    /// The note stored by [`Self::mark_created_with`], `None` when the
    /// ownership bit is not set. A non-consuming read; the eventual last
    /// owner still clears the bit with [`LastConfigLease::take_created`].
    pub fn created_note(&self) -> Option<String> {
        std::fs::read_to_string(&self.marker).ok()
    }
}

impl LastConfigLease {
    /// Consume and clear the durable resource-ownership bit.
    ///
    /// Clearing before cleanup makes a crash conservative: it may leave a
    /// managed file behind, but it cannot later delete a user-created file.
    pub fn take_created(&mut self) -> Result<bool> {
        match std::fs::remove_file(&self.marker) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }
}

// An opener may have obtained the old inode just before the final holder
// unlinked it. Validate *after* locking, otherwise two holders can believe
// they own the same resource while locking different files.
fn lock_current_file(path: &Path, file: File, timeout: Duration) -> Result<Option<File>> {
    let file = crate::lock::lock_within(file, crate::lock::LockKind::Shared, timeout, || {
        format!(
            "timed out waiting for the PaneFlow config lease {}",
            path.display()
        )
    })?;
    if file_is_current(&file, path)? {
        Ok(Some(file))
    } else {
        Ok(None)
    }
}

fn file_is_current(file: &File, path: &Path) -> Result<bool> {
    let opened = file.metadata()?;
    match std::fs::metadata(path) {
        Ok(current) => Ok((opened.dev(), opened.ino()) == (current.dev(), current.ino())),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn remove_current_lock(file: &File, path: &Path) {
    if file_is_current(file, path).unwrap_or(false) {
        let _ = std::fs::remove_file(path);
    }
}

impl Drop for ConfigLease {
    fn drop(&mut self) {
        if let Some(file) = self.file.take() {
            let _ = file.unlock();
            if file.try_lock().is_ok() {
                remove_current_lock(&file, &self.path);
            }
        }
    }
}

impl Drop for LastConfigLease {
    fn drop(&mut self) {
        // Durable .created markers survive until take_created consumes them.
        remove_current_lock(&self._file, &self.path);
    }
}

fn lease_directory(dirs: paneflow_config::loader::UserDirs) -> PathBuf {
    dirs.config
        .join(paneflow_config::loader::APP_SUBDIR)
        .join("agent-config-leases")
}

fn lease_path(resource: &Path) -> Result<PathBuf> {
    let dirs = paneflow_config::loader::user_dirs().ok_or_else(|| {
        Error::new(
            ErrorKind::NotFound,
            "could not resolve the user configuration directory",
        )
    })?;
    let directory = lease_directory(dirs);
    std::fs::create_dir_all(&directory)?;
    Ok(directory.join(format!("{:016x}.lock", resource_hash(resource))))
}

fn resource_hash(path: &Path) -> u64 {
    use std::os::unix::ffi::OsStrExt;
    fnv1a(path.as_os_str().as_bytes().iter().copied())
}

fn fnv1a(bytes: impl IntoIterator<Item = u8>) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_final_live_lease_can_clean_up() {
        let resource = unique_resource("last");
        let mut first = ConfigLease::acquire(&resource).unwrap();
        let mut second = ConfigLease::acquire(&resource).unwrap();
        first.mark_created().unwrap();

        assert!(first.try_take_last().unwrap().is_none());
        let mut last = second.try_take_last().unwrap().unwrap();
        assert!(last.take_created().unwrap());
        drop(last);

        let mut later = ConfigLease::acquire(&resource).unwrap();
        let mut last = later.try_take_last().unwrap().unwrap();
        assert!(!last.take_created().unwrap());
    }

    #[test]
    fn created_note_survives_the_recording_lease_and_is_cleared_by_the_last() {
        let resource = unique_resource("note");
        let mut recorder = ConfigLease::acquire(&resource).unwrap();
        assert!(!recorder.is_created());
        assert_eq!(recorder.created_note(), None);
        recorder.mark_created_with("[\"A\"]").unwrap();
        drop(recorder);

        // File drop releases the shared flock synchronously, but a loaded
        // runner can still observe WouldBlock on the immediate exclusive
        // upgrade. Same retry as dropped_lease_does_not_strand_the_resource.
        let mut last = None;
        for attempt in 0..10 {
            let mut later = ConfigLease::acquire(&resource).unwrap();
            assert!(later.is_created());
            assert_eq!(later.created_note().as_deref(), Some("[\"A\"]"));
            match later.try_take_last().unwrap() {
                Some(taken) => {
                    last = Some(taken);
                    break;
                }
                None => {
                    if attempt + 1 < 10 {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                }
            }
        }
        let mut last = last.expect(
            "dropped lease stranded the resource: try_take_last stayed WouldBlock after Drop",
        );
        assert!(last.take_created().unwrap());
        drop(last);
        assert_eq!(
            ConfigLease::acquire(&resource).unwrap().created_note(),
            None
        );
    }

    #[test]
    fn dropped_lease_does_not_strand_the_resource() {
        let resource = unique_resource("crash");
        let mut abandoned = ConfigLease::acquire(&resource).unwrap();
        abandoned.mark_created().unwrap();
        drop(abandoned);

        // File drop releases the shared flock synchronously, but a loaded
        // runner can still observe WouldBlock on the immediate exclusive
        // upgrade. Retry acquire + try_take_last; a real strand stays None.
        let mut last = None;
        for attempt in 0..10 {
            let mut survivor = ConfigLease::acquire(&resource).unwrap();
            match survivor.try_take_last().unwrap() {
                Some(taken) => {
                    last = Some(taken);
                    break;
                }
                None => {
                    if attempt + 1 < 10 {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                }
            }
        }
        let mut last = last.expect(
            "dropped lease stranded the resource: try_take_last stayed WouldBlock after Drop",
        );
        assert!(last.take_created().unwrap());
    }

    #[test]
    fn acquire_times_out_when_exclusive_lock_held() {
        let resource = unique_resource("timeout");
        let mut lease = ConfigLease::acquire(&resource).unwrap();
        let last = lease.try_take_last().unwrap().unwrap();
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        let contender = std::thread::spawn(move || {
            result_tx
                .send(ConfigLease::acquire(&resource).map(|_| ()))
                .unwrap();
        });

        let result = result_rx
            .recv_timeout(LOCK_TIMEOUT + Duration::from_millis(500))
            .expect("lease acquisition did not respect its timeout");
        drop(last);
        contender.join().unwrap();

        let error = result.unwrap_err();
        assert_eq!(error.kind(), ErrorKind::TimedOut);
    }

    #[test]
    fn final_drop_removes_unused_lock_file() {
        let resource = unique_resource("removal");
        let path = lease_path(&resource).unwrap();
        drop(ConfigLease::acquire(&resource).unwrap());
        assert!(!path.exists(), "unused lease lock must be removed");
    }

    #[test]
    fn final_drop_removes_lock_but_preserves_durable_marker() {
        let resource = unique_resource("cleanup");
        let path = lease_path(&resource).unwrap();
        let mut first = ConfigLease::acquire(&resource).unwrap();
        let second = ConfigLease::acquire(&resource).unwrap();
        first.mark_created_with("owned").unwrap();
        drop(first);
        assert!(path.exists(), "a live holder keeps its lock inode");
        drop(second);
        assert!(!path.exists(), "the final holder removes the lock");
        let mut later = ConfigLease::acquire(&resource).unwrap();
        assert_eq!(later.created_note().as_deref(), Some("owned"));
        let mut last = later.try_take_last().unwrap().unwrap();
        assert!(last.take_created().unwrap());
        drop(last);
        assert!(!path.exists());
        assert!(!path.with_extension("created").exists());
    }

    #[test]
    fn opener_of_unlinked_inode_cannot_become_a_second_holder() {
        let resource = unique_resource("inode");
        let path = lease_path(&resource).unwrap();
        let holder = ConfigLease::acquire(&resource).unwrap();
        let stale = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        drop(holder);
        let mut replacement = ConfigLease::acquire(&resource).unwrap();
        assert!(lock_current_file(&path, stale, Duration::from_secs(1))
            .unwrap()
            .is_none());
        let second = ConfigLease::acquire(&resource).unwrap();
        assert!(replacement.try_take_last().unwrap().is_none());
        drop(second);
        assert!(!path.exists());
    }

    #[test]
    fn leases_use_the_shared_home_and_build_namespace() {
        let home = tempfile::tempdir().unwrap();
        let directory = lease_directory(paneflow_config::loader::user_dirs_under(home.path()));
        assert_eq!(
            directory,
            home.path()
                .join("config")
                .join(paneflow_config::loader::APP_SUBDIR)
                .join("agent-config-leases")
        );
        if cfg!(debug_assertions) {
            assert!(!directory.starts_with(home.path().join("config/paneflow")));
        }
    }

    fn unique_resource(label: &str) -> PathBuf {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!(
            "paneflow-agent-config-lease-{label}-{}-{:?}-{seq}-{nanos}",
            std::process::id(),
            std::thread::current().id()
        ))
    }
}
