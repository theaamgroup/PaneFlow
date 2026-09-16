//! Recently opened workspace folders (issue #521, upstream `9aa03d09` part 2).
//!
//! The list lives in `recents.json` beside `session.json`, under the same
//! `APP_SUBDIR` rule as `window_state.rs`, so a debug build writes to
//! `paneflow-dev` and never touches the installed app's file. It is capped at
//! [`MAX_RECENT_WORKSPACES`] entries, newest first; a folder that no longer
//! exists is dropped on the next load. The list is process-wide state rather
//! than a `PaneFlowApp` field: the sidebar's empty state and the `Cmd+1..5`
//! fallback read it, `open_workspace_folders` and the session restore write
//! it, and none of them needs more than a snapshot.
//!
//! Reads are bounded by [`crate::limits::MAX_RECENTS_SIZE_BYTES`] and
//! non-fatal: a corrupt, oversized, or non-regular file is ignored with a log
//! line. Writes go through `smol::unblock` off the render thread, the same
//! shape as `save_session`, and land through a temporary file plus rename.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

use gpui::{App, AppContext};
use serde::{Deserialize, Serialize};

use crate::limits::MAX_RECENTS_SIZE_BYTES;

/// How many folders the list remembers.
pub(crate) const MAX_RECENT_WORKSPACES: usize = 8;

/// How many of them answer `Cmd+1..5` while no workspace is open.
pub(crate) const MAX_RECENT_SHORTCUTS: usize = 5;

/// Per-process sequence for temp-file names in `write_to_disk`.
static RECENTS_TMP_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RecentWorkspace {
    pub(crate) path: PathBuf,
    pub(crate) title: String,
}

#[derive(Default, Serialize, Deserialize)]
struct RecentsFile {
    #[serde(default)]
    workspaces: Vec<RecentWorkspace>,
}

/// `None` until the first read; a load that finds nothing caches `Some(vec![])`
/// so the sidebar does not re-read the file every frame.
static RECENTS: Mutex<Option<Vec<RecentWorkspace>>> = Mutex::new(None);

fn cache() -> MutexGuard<'static, Option<Vec<RecentWorkspace>>> {
    RECENTS.lock().unwrap_or_else(PoisonError::into_inner)
}

/// `recents.json` beside `session.json`: `dirs::config_dir()/APP_SUBDIR/`.
pub(crate) fn recents_path() -> Option<PathBuf> {
    dirs::config_dir().map(|directory| {
        directory
            .join(paneflow_config::loader::APP_SUBDIR)
            .join("recents.json")
    })
}

fn title_for(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// The current list, newest first. Loads and prunes the file on first use.
pub(crate) fn current() -> Vec<RecentWorkspace> {
    let mut guard = cache();
    guard
        .get_or_insert_with(|| recents_path().map(|p| load_pruned(&p)).unwrap_or_default())
        .clone()
}

/// Promote `paths` (left to right, so the first ends up at the head) and
/// persist the list off the render thread when it changed.
pub(crate) fn record(paths: &[PathBuf], cx: &App) {
    if paths.is_empty() {
        return;
    }
    let snapshot = {
        let mut guard = cache();
        let list = guard
            .get_or_insert_with(|| recents_path().map(|p| load_pruned(&p)).unwrap_or_default());
        if !promote(list, paths) {
            return;
        }
        list.clone()
    };
    persist(snapshot, cx);
}

/// Drop one folder (the user clicked a row whose directory vanished).
pub(crate) fn forget(path: &Path, cx: &App) {
    let snapshot = {
        let mut guard = cache();
        let Some(list) = guard.as_mut() else {
            return;
        };
        let before = list.len();
        list.retain(|entry| entry.path != path);
        if list.len() == before {
            return;
        }
        list.clone()
    };
    persist(snapshot, cx);
}

fn persist(workspaces: Vec<RecentWorkspace>, cx: &App) {
    let Some(path) = recents_path() else {
        return;
    };
    cx.background_spawn(async move {
        smol::unblock(move || write_to_disk(&path, &workspaces)).await;
    })
    .detach();
}

/// Pure: move each of `paths` to the head, dropping duplicates and anything
/// past the cap. Returns whether the list changed.
pub(crate) fn promote(workspaces: &mut Vec<RecentWorkspace>, paths: &[PathBuf]) -> bool {
    let before = workspaces.clone();
    for path in paths.iter().rev() {
        if path.as_os_str().is_empty() {
            continue;
        }
        let entry = RecentWorkspace {
            path: path.clone(),
            title: title_for(path),
        };
        workspaces.retain(|kept| kept.path != entry.path);
        workspaces.insert(0, entry);
    }
    workspaces.truncate(MAX_RECENT_WORKSPACES);
    *workspaces != before
}

/// Pure: keep only entries whose directory still exists, deduplicated, capped.
pub(crate) fn prune(stored: Vec<RecentWorkspace>) -> Vec<RecentWorkspace> {
    let mut kept: Vec<RecentWorkspace> =
        Vec::with_capacity(stored.len().min(MAX_RECENT_WORKSPACES));
    for entry in stored {
        if kept.len() >= MAX_RECENT_WORKSPACES {
            break;
        }
        if entry.path.as_os_str().is_empty() || !entry.path.is_dir() {
            continue;
        }
        if kept.iter().any(|k| k.path == entry.path) {
            continue;
        }
        kept.push(entry);
    }
    kept
}

/// Read `path` (bounded, non-fatal) and prune it.
pub(crate) fn load_pruned(path: &Path) -> Vec<RecentWorkspace> {
    prune(read_from_disk(path))
}

/// The restored session's folders, active workspace first, for `record`.
pub(crate) fn restored_session_paths(
    workspaces: &[crate::workspace::Workspace],
    active_idx: usize,
) -> Vec<PathBuf> {
    let mut ordered: Vec<PathBuf> = Vec::with_capacity(workspaces.len());
    if let Some(active) = workspaces.get(active_idx).filter(|ws| !ws.cwd.is_empty()) {
        ordered.push(PathBuf::from(&active.cwd));
    }
    for (idx, ws) in workspaces.iter().enumerate() {
        if idx == active_idx || ws.cwd.is_empty() {
            continue;
        }
        ordered.push(PathBuf::from(&ws.cwd));
    }
    ordered
}

/// Which recent row a `SelectWorkspaceN` chord falls through to: only while
/// no workspace is open, and only for the first [`MAX_RECENT_SHORTCUTS`].
pub(crate) fn shortcut_fallback(workspace_count: usize, idx: usize) -> Option<usize> {
    (workspace_count == 0 && idx < MAX_RECENT_SHORTCUTS).then_some(idx)
}

fn read_from_disk(path: &Path) -> Vec<RecentWorkspace> {
    use std::os::unix::fs::OpenOptionsExt;
    // O_NONBLOCK so a FIFO swapped in at this path cannot hang the open
    // (the window-state reader's issue #407 lesson).
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(error) => {
            log::warn!("recents: failed to open {}: {error}", path.display());
            return Vec::new();
        }
    };
    let metadata = match file.metadata() {
        Ok(metadata) => metadata,
        Err(error) => {
            log::warn!("recents: failed to inspect {}: {error}", path.display());
            return Vec::new();
        }
    };
    if !metadata.is_file() || metadata.len() > MAX_RECENTS_SIZE_BYTES {
        log::warn!(
            "recents: ignoring {} (not a regular file or over {} bytes)",
            path.display(),
            MAX_RECENTS_SIZE_BYTES
        );
        return Vec::new();
    }
    let mut raw = String::new();
    match file
        .take(MAX_RECENTS_SIZE_BYTES + 1)
        .read_to_string(&mut raw)
    {
        Ok(_) if raw.len() as u64 <= MAX_RECENTS_SIZE_BYTES => {}
        Ok(_) => {
            log::warn!(
                "recents: ignoring {} (over {} bytes)",
                path.display(),
                MAX_RECENTS_SIZE_BYTES
            );
            return Vec::new();
        }
        Err(error) => {
            log::warn!("recents: failed to read {}: {error}", path.display());
            return Vec::new();
        }
    }
    match serde_json::from_str::<RecentsFile>(&raw) {
        Ok(parsed) => parsed.workspaces,
        Err(error) => {
            log::warn!(
                "recents: {} is not valid JSON ({error}), starting empty",
                path.display()
            );
            Vec::new()
        }
    }
}

fn write_to_disk(path: &Path, workspaces: &[RecentWorkspace]) {
    use std::os::unix::fs::OpenOptionsExt;
    let Some(parent) = path.parent() else {
        return;
    };
    if let Err(error) = std::fs::create_dir_all(parent) {
        log::warn!("recents: could not create {}: {error}", parent.display());
        return;
    }
    let file = RecentsFile {
        workspaces: workspaces.to_vec(),
    };
    let json = match serde_json::to_string_pretty(&file) {
        Ok(json) => json,
        Err(error) => {
            log::warn!("recents: could not serialize: {error}");
            return;
        }
    };
    if json.len() as u64 > MAX_RECENTS_SIZE_BYTES {
        log::warn!(
            "recents: not writing {} ({} bytes exceeds the {} byte cap)",
            path.display(),
            json.len(),
            MAX_RECENTS_SIZE_BYTES
        );
        return;
    }
    // Owner-only: the file stores absolute folder paths. Temp + rename so a
    // reader never sees a half-written list.
    // Per-write sequence (like `session_tmp_path`): two persists in flight
    // on the unblock pool must not share a temp path, or one could rename
    // the other's half-written file into place.
    let seq = RECENTS_TMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = parent.join(format!(".recents.json.tmp-{}-{seq}", std::process::id()));
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .and_then(|mut f| f.write_all(json.as_bytes()).and_then(|()| f.sync_all()))
        .and_then(|()| std::fs::rename(&tmp, path));
    if let Err(error) = written {
        log::warn!("recents: could not write {}: {error}", path.display());
        let _ = std::fs::remove_file(&tmp);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str) -> RecentWorkspace {
        RecentWorkspace {
            path: PathBuf::from(path),
            title: title_for(Path::new(path)),
        }
    }

    #[test]
    fn promote_moves_a_known_path_back_to_the_front() {
        let mut recents = vec![entry("/a"), entry("/b")];
        assert!(promote(&mut recents, &[PathBuf::from("/b")]));
        assert_eq!(recents[0].path, PathBuf::from("/b"));
        assert_eq!(recents.len(), 2, "promotion dedupes, it never duplicates");
    }

    #[test]
    fn promote_reports_no_change_when_the_front_entry_is_reopened() {
        let mut recents = vec![entry("/a")];
        assert!(!promote(&mut recents, &[PathBuf::from("/a")]));
        assert!(
            !promote(&mut recents, &[PathBuf::new()]),
            "an empty path is skipped"
        );
    }

    #[test]
    fn promote_keeps_the_newest_entries_within_the_cap() {
        let mut recents = Vec::new();
        let paths: Vec<PathBuf> = (0..MAX_RECENT_WORKSPACES + 3)
            .map(|i| PathBuf::from(format!("/ws{i}")))
            .collect();
        for path in &paths {
            promote(&mut recents, std::slice::from_ref(path));
        }
        assert_eq!(recents.len(), MAX_RECENT_WORKSPACES);
        assert_eq!(recents[0].path, paths[paths.len() - 1]);
        assert_eq!(recents[0].title, "ws10");
    }

    #[test]
    fn promote_orders_a_multi_folder_open_left_to_right() {
        let mut recents = Vec::new();
        promote(
            &mut recents,
            &[PathBuf::from("/first"), PathBuf::from("/second")],
        );
        assert_eq!(recents[0].path, PathBuf::from("/first"));
        assert_eq!(recents[1].path, PathBuf::from("/second"));
    }

    #[test]
    fn prune_drops_missing_directories_duplicates_and_the_overflow() {
        let dir = tempfile::tempdir().expect("tempdir");
        let live = dir.path().join("live");
        std::fs::create_dir(&live).expect("mkdir");
        let gone = dir.path().join("gone");
        let mut stored = vec![
            RecentWorkspace {
                path: gone.clone(),
                title: "gone".into(),
            },
            RecentWorkspace {
                path: live.clone(),
                title: "live".into(),
            },
            RecentWorkspace {
                path: live.clone(),
                title: "live again".into(),
            },
        ];
        for i in 0..MAX_RECENT_WORKSPACES + 2 {
            let extra = dir.path().join(format!("extra{i}"));
            std::fs::create_dir(&extra).expect("mkdir");
            stored.push(RecentWorkspace {
                path: extra,
                title: format!("extra{i}"),
            });
        }
        let kept = prune(stored);
        assert_eq!(kept[0].path, live);
        assert_eq!(kept.len(), MAX_RECENT_WORKSPACES);
        assert!(kept.iter().all(|e| e.path != gone));
        assert_eq!(kept.iter().filter(|e| e.path == live).count(), 1);
    }

    #[test]
    fn load_ignores_a_corrupt_file_and_a_missing_one() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("recents.json");
        assert!(load_pruned(&path).is_empty(), "missing file is empty");
        std::fs::write(&path, "{ this is not json").expect("write");
        assert!(load_pruned(&path).is_empty(), "corrupt file is ignored");
        std::fs::write(&path, "{}").expect("write");
        assert!(
            load_pruned(&path).is_empty(),
            "a file with no list is empty"
        );
    }

    #[test]
    fn load_ignores_an_oversized_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("recents.json");
        let mut json = format!(
            "{{\"workspaces\":[{{\"path\":{:?},\"title\":\"t\"}}]",
            dir.path().to_string_lossy()
        );
        while (json.len() as u64) <= MAX_RECENTS_SIZE_BYTES {
            json.push(' ');
        }
        json.push('}');
        std::fs::write(&path, json).expect("write");
        assert!(load_pruned(&path).is_empty(), "oversized file is ignored");
    }

    #[test]
    fn write_then_load_round_trips_and_a_deleted_folder_vanishes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nested").join("recents.json");
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        std::fs::create_dir(&a).expect("mkdir");
        std::fs::create_dir(&b).expect("mkdir");
        let mut list = Vec::new();
        promote(&mut list, &[a.clone(), b.clone()]);
        write_to_disk(&path, &list);
        assert_eq!(load_pruned(&path), list);
        std::fs::remove_dir(&b).expect("rmdir");
        let reloaded = load_pruned(&path);
        assert_eq!(reloaded.len(), 1);
        assert_eq!(reloaded[0].path, a);
        assert_eq!(reloaded[0].title, "a");
    }

    #[test]
    fn recents_path_sits_beside_session_json_under_app_subdir() {
        let path = recents_path().expect("config dir must resolve on macOS");
        let session = paneflow_config::loader::session_path().expect("session path");
        assert_eq!(path.parent(), session.parent());
        assert_eq!(
            path.parent()
                .and_then(|parent| parent.file_name())
                .and_then(|name| name.to_str()),
            Some(paneflow_config::loader::APP_SUBDIR)
        );
        if cfg!(debug_assertions) {
            assert!(path.to_string_lossy().contains("paneflow-dev"));
        }
    }

    #[test]
    fn shortcut_fallback_only_fires_with_no_workspace_and_only_for_the_first_five() {
        assert_eq!(shortcut_fallback(0, 0), Some(0));
        assert_eq!(shortcut_fallback(0, 4), Some(4));
        assert_eq!(shortcut_fallback(0, 5), None, "Cmd+6..9 stay inert");
        assert_eq!(
            shortcut_fallback(1, 0),
            None,
            "an open workspace owns the chord"
        );
    }

    #[test]
    fn restored_session_paths_lead_with_the_active_workspace() {
        use crate::workspace::Workspace;
        let first = Workspace::empty_with_cwd_and_id(1, "one", PathBuf::from("/one"));
        let second = Workspace::empty_with_cwd_and_id(2, "two", PathBuf::from("/two"));
        let mut blank = Workspace::empty_with_cwd_and_id(3, "three", PathBuf::from("/x"));
        blank.cwd = String::new();
        let ordered = restored_session_paths(&[first, second, blank], 1);
        assert_eq!(
            ordered,
            vec![PathBuf::from("/two"), PathBuf::from("/one")],
            "active first, blank cwd skipped"
        );
    }
}
