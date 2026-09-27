// US-018: Hot-reload via file watcher

use crate::loader::{
    config_path, load_config_from_path, read_config_string, try_parse_and_validate,
};
use crate::schema::PaneFlowConfig;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use tracing::{info, warn};

/// Debounce window: accumulate file events for this duration before reloading.
const DEBOUNCE_DURATION: Duration = Duration::from_millis(300);

/// US-029: hard ceiling on how long the debounce may keep postponing a reload.
/// Each event pushes the 300ms deadline forward; a source touching the watched
/// directory faster than 300ms (FSEvents batches on macOS, multi-event saves on
/// Windows) would otherwise starve the reload indefinitely. Once events have
/// been arriving for this long, the reload fires regardless (leading+trailing
/// debounce with a max-wait).
const MAX_DEBOUNCE: Duration = Duration::from_secs(1);

/// Watches the PaneFlow config file for changes and triggers hot-reload.
///
/// The watcher monitors the parent directory (not the file directly) so that
/// editor save patterns involving delete+recreate (atomic saves) are captured.
/// A symlink whose target lives in another directory is watched there too,
/// including one that replaces a real file after the watcher has started: the
/// write lands in the target's directory, often under the target's own name.
/// File events are debounced at 300ms to coalesce rapid sequences of writes.
pub struct ConfigWatcher {
    callback: Arc<dyn Fn(PaneFlowConfig) + Send + Sync>,
    config_path: PathBuf,
}

impl ConfigWatcher {
    /// Creates a new `ConfigWatcher` that will invoke `callback` with the new
    /// configuration whenever the config file is successfully reloaded.
    ///
    /// Uses `config_path()` to determine which file to watch. Returns `None`
    /// when the platform config directory cannot be resolved, letting the app
    /// keep running with cold-loaded defaults and hot reload disabled.
    pub fn new(callback: Arc<dyn Fn(PaneFlowConfig) + Send + Sync>) -> Option<Self> {
        let Some(config_path) = config_path() else {
            warn!("could not determine config path; config hot-reload disabled");
            return None;
        };
        Some(Self {
            callback,
            config_path,
        })
    }

    /// Creates a `ConfigWatcher` targeting a specific path - useful for testing.
    #[cfg(test)]
    fn new_with_path(path: PathBuf, callback: Arc<dyn Fn(PaneFlowConfig) + Send + Sync>) -> Self {
        Self {
            callback,
            config_path: path,
        }
    }

    /// Starts watching the config file's parent directory for changes.
    ///
    /// Spawns a background thread that:
    /// 1. Receives raw file-system events from `notify::RecommendedWatcher`
    /// 2. Debounces them over a 300ms window
    /// 3. Reloads and validates the config file
    /// 4. Calls the callback on success, or logs a warning on failure
    ///
    /// Returns `Ok(())` once the watcher is installed, or an error if the
    /// underlying OS watcher could not be created.
    pub fn start(&self) -> Result<(), notify::Error> {
        // Invariant: `self.config_path` is always a file path built from
        // `config_path()` (e.g., `/home/u/.config/paneflow/paneflow.json`),
        // so `.parent()` is guaranteed to be `Some`. `expect` is correct
        // here - documented invariant per CLAUDE.md.
        #[allow(clippy::expect_used)]
        let watch_dir = self
            .config_path
            .parent()
            .expect("config path has no parent directory")
            .to_path_buf();

        // Canonicalize follows the symlink. It fails when the file is not
        // there yet; the configured parent is still watched below.
        let canonical_path = std::fs::canonicalize(&self.config_path).ok();
        // Issue #875: a write to the target does not show up in the link's
        // directory. Watch the canonical parent when it is a different one.
        let extra_watch_dir = canonical_path.as_ref().and_then(|path| {
            path.parent()
                .filter(|dir| *dir != watch_dir.as_path())
                .map(Path::to_path_buf)
        });

        let mut watch_dirs = vec![watch_dir];
        if let Some(dir) = extra_watch_dir {
            watch_dirs.push(dir);
        }

        // notify can't watch a directory that doesn't exist yet - create it
        // on first run so hot-reload works even before the user writes a config.
        for dir in &watch_dirs {
            if !dir.exists() {
                std::fs::create_dir_all(dir).map_err(notify::Error::io)?;
            }
        }

        let config_path = self.config_path.clone();
        let mut match_path = canonical_path.unwrap_or_else(|| config_path.clone());
        let callback = Arc::clone(&self.callback);

        // Channel for notify -> processing thread.
        let (tx, rx) = mpsc::channel::<notify::Result<Event>>();

        // Create the OS file watcher. It sends events through `tx`.
        let mut watcher = RecommendedWatcher::new(
            move |res| {
                // Best-effort send; if the receiver is gone the watcher is being dropped.
                let _ = tx.send(res);
            },
            notify::Config::default(),
        )?;

        // Watch each parent (non-recursive) to catch delete+recreate.
        for dir in &watch_dirs {
            watcher.watch(dir, RecursiveMode::NonRecursive)?;
        }

        // Spawn the event-processing loop in a background thread.
        // The thread owns `watcher` so it can outlive `start` and so the loop
        // can add a watch if the configured path later becomes a symlink.
        thread::spawn(move || {
            event_loop(
                rx,
                &config_path,
                &mut match_path,
                &callback,
                &mut watcher,
                &mut watch_dirs,
            );
        });

        info!(
            path = %self.config_path.display(),
            "config watcher started"
        );

        Ok(())
    }
}

/// Returns `true` if this event kind is relevant for config reload.
fn is_relevant_event(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
    )
}

/// Returns `true` if any path in the event matches the config file.
///
/// Matches by file name rather than full path: platforms rewrite watched
/// paths before emitting events (macOS FSEvents canonicalizes
/// `/var/folders/...` to `/private/var/folders/...`) so a full-path
/// comparison is inherently fragile. The watcher is installed
/// `NonRecursive` on the configured path's parent, on a launch-time
/// canonical parent when that directory differs, and on any directory the
/// configured path later canonicalizes into. Basename equality against
/// either file name is sufficient.
fn event_targets_config(event: &Event, config_path: &Path, match_path: &Path) -> bool {
    let configured_name = config_path.file_name();
    let canonical_name = match_path.file_name();
    event.paths.iter().any(|path| {
        let name = path.file_name();
        (configured_name.is_some() && name == configured_name)
            || (canonical_name.is_some() && name == canonical_name)
    })
}

/// Returns `true` when an event path uses the configured file's basename.
///
/// Issue #1026 re-resolves the symlink only for that name. An edit of a
/// target that has its own name must not be what arms the new directory
/// watch; the replacement of the configured path does.
fn event_names_configured_file(event: &Event, config_path: &Path) -> bool {
    let Some(configured_name) = config_path.file_name() else {
        return false;
    };
    event
        .paths
        .iter()
        .any(|path| path.file_name() == Some(configured_name))
}

/// Returns `true` when `candidate` is a directory the watcher is already
/// observing. Comparison follows canonical paths so `/var/folders/...` and
/// `/private/var/folders/...` count as one watch: calling `watch` again
/// restarts the macOS FSEvents stream.
fn directory_already_watched(watched_dirs: &[PathBuf], candidate: &Path) -> bool {
    watched_dirs
        .iter()
        .any(|dir| same_directory(dir, candidate))
}

fn same_directory(left: &Path, right: &Path) -> bool {
    if left == right {
        return true;
    }
    let Ok(left_canonical) = std::fs::canonicalize(left) else {
        return false;
    };
    if left_canonical == right {
        return true;
    }
    std::fs::canonicalize(right).is_ok_and(|right_canonical| right_canonical == left_canonical)
}

/// Issue #1026: follow `config_path` again after it is created, modified, or
/// removed. A symlink that appears after start points at a directory that
/// was not watched at launch. Add that directory without dropping the
/// original parent watch, and aim basename matching at the current target.
fn refresh_symlink_target_watch(
    config_path: &Path,
    watcher: &mut RecommendedWatcher,
    watched_dirs: &mut Vec<PathBuf>,
    match_path: &mut PathBuf,
) {
    let Ok(canonical) = std::fs::canonicalize(config_path) else {
        // Missing or dangling. Keep the previous match path so a transient
        // delete (atomic save) does not forget a target watched for #875.
        return;
    };
    if let Some(parent) = canonical.parent().map(Path::to_path_buf) {
        let parent = std::fs::canonicalize(&parent).unwrap_or(parent);
        if !directory_already_watched(watched_dirs, &parent) {
            match watcher.watch(&parent, RecursiveMode::NonRecursive) {
                Ok(()) => {
                    info!(
                        path = %parent.display(),
                        "watching config symlink target directory"
                    );
                    watched_dirs.push(parent);
                }
                Err(error) => {
                    warn!(
                        path = %parent.display(),
                        %error,
                        "failed to watch config symlink target directory"
                    );
                }
            }
        }
    }
    *match_path = canonical;
}

/// The main event-processing loop running on the background thread.
///
/// `watcher` stays alive for the loop and stays mutable so a config path
/// replaced by a symlink can gain a watch on the target directory.
/// `match_path` starts as the launch-time canonical path, or the configured
/// path when that file is not there yet, and is updated when the target moves.
fn event_loop(
    rx: mpsc::Receiver<notify::Result<Event>>,
    config_path: &Path,
    match_path: &mut PathBuf,
    callback: &Arc<dyn Fn(PaneFlowConfig) + Send + Sync>,
    watcher: &mut RecommendedWatcher,
    watched_dirs: &mut Vec<PathBuf>,
) {
    // The last config that was successfully loaded (starts as the current one).
    let mut current_config = load_config_from_path(config_path);
    let mut pending_reload: Option<Instant> = None;
    // US-029: timestamp of the first event in the current debounce burst, used
    // to cap the trailing debounce so a continuous event stream can't starve
    // the reload forever.
    let mut first_event_at: Option<Instant> = None;

    loop {
        // If we have a pending reload, wait only until the debounce window expires.
        // Otherwise block indefinitely for the next event.
        let event_result = if let Some(deadline) = pending_reload {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                // Debounce window expired - do the reload.
                pending_reload = None;
                first_event_at = None;
                attempt_reload(config_path, &mut current_config, callback);
                continue;
            }
            rx.recv_timeout(remaining)
        } else {
            // No pending reload - block for the next event.
            match rx.recv() {
                Ok(ev) => Ok(ev),
                Err(_) => break, // Channel closed - watcher was dropped.
            }
        };

        match event_result {
            Ok(Ok(event)) => {
                if is_relevant_event(&event.kind) {
                    // Arm a newly linked target before this event is allowed
                    // to schedule a reload, so the reload and any later edit
                    // both see the updated match path.
                    if event_names_configured_file(&event, config_path) {
                        refresh_symlink_target_watch(
                            config_path,
                            watcher,
                            watched_dirs,
                            match_path,
                        );
                    }
                    if event_targets_config(&event, config_path, match_path) {
                        let now = Instant::now();
                        let burst_start = *first_event_at.get_or_insert(now);
                        // Trailing debounce, but never pushed past the max-wait cap
                        // measured from the first event of the burst.
                        let deadline = (now + DEBOUNCE_DURATION).min(burst_start + MAX_DEBOUNCE);
                        pending_reload = Some(deadline);
                    }
                }
            }
            Ok(Err(e)) => {
                warn!("file watcher error: {e}");
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Debounce window expired.
                pending_reload = None;
                first_event_at = None;
                attempt_reload(config_path, &mut current_config, callback);
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                break; // Channel closed.
            }
        }
    }
}

/// Attempt to reload the config file. On success, call the callback and update
/// `current_config`. On failure (file deleted or invalid), log a warning and
/// keep the old config.
fn attempt_reload(
    config_path: &Path,
    current_config: &mut PaneFlowConfig,
    callback: &Arc<dyn Fn(PaneFlowConfig) + Send + Sync>,
) {
    // US-029: read through the shared helper so the oversize guard (cheap stat
    // before allocating) applies on this hot path too - it previously read
    // with no cap, the only path a hostile/runaway file could freeze.
    let contents = match read_config_string(config_path) {
        Ok(Some(contents)) => contents,
        Ok(None) => {
            warn!(
                path = %config_path.display(),
                "config file was deleted; keeping previous config and continuing to watch"
            );
            return;
        }
        Err(error) => {
            warn!(%error, "config reload rejected; keeping previous config");
            return;
        }
    };

    // US-029: parse exactly once. A syntax error keeps the previous config
    // (never broadcast defaults on a malformed save); the old code parsed the
    // JSON twice - a syntax-guard `from_str` plus a second parse inside
    // `parse_and_validate_with_path`.
    let new_config = match try_parse_and_validate(&contents) {
        Ok(c) => c,
        Err(e) => {
            warn!(
                error = %e,
                "config file has validation errors; keeping previous config"
            );
            return;
        }
    };

    // US-029: a save that didn't actually change the parsed config (whitespace,
    // a `touch`, an unrelated key) shouldn't fire the callback and re-apply on
    // the GPUI thread.
    if new_config == *current_config {
        return;
    }

    info!("config reloaded successfully");
    *current_config = new_config.clone();
    callback(new_config);
}

#[cfg(test)]
#[path = "watcher_tests.rs"]
mod tests;
