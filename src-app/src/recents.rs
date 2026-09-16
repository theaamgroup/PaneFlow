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
//! Neither the read nor the `is_dir` prune ever runs on the GPUI thread: the
//! one load runs on the background pool (`warm` at boot, or the first reader)
//! and publishes into the cache, which reads as empty until then.
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

/// The process-wide list and where its first load stands. The file is read
/// and pruned (`Path::is_dir` per entry) on the background pool, never on the
/// GPUI thread: a recent folder on a stalled network mount must not block
/// startup or freeze the empty-state sidebar. Until that load lands the list
/// reads as empty and promotions queue in `Loading::pending`, replayed on
/// publish so nothing recorded during the window is lost.
enum Cache {
    Unloaded,
    /// Each queued `record` call is its own batch: `promote` gives one
    /// slice multi-folder-open semantics (the first path wins the head), so
    /// two sequential records must not be flattened into one slice.
    Loading {
        pending: Vec<Vec<PathBuf>>,
    },
    Loaded(Vec<RecentWorkspace>),
}

/// What a cache transition asks its caller to do next.
#[derive(Debug, PartialEq, Eq)]
enum CacheAction {
    Nothing,
    /// Start the one background load; the request was queued behind it.
    SpawnLoad,
    /// Write this snapshot to disk (off-thread).
    Persist(Vec<RecentWorkspace>),
}

static RECENTS: Mutex<Cache> = Mutex::new(Cache::Unloaded);

fn cache() -> MutexGuard<'static, Cache> {
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

/// Pure: what a reader sees (empty until the load has published) and
/// whether that reader has to start the load.
fn cache_read(state: &mut Cache) -> (Vec<RecentWorkspace>, CacheAction) {
    match state {
        Cache::Loaded(list) => (list.clone(), CacheAction::Nothing),
        Cache::Loading { .. } => (Vec::new(), CacheAction::Nothing),
        Cache::Unloaded => {
            *state = Cache::Loading {
                pending: Vec::new(),
            };
            (Vec::new(), CacheAction::SpawnLoad)
        }
    }
}

/// Pure: promote `paths` in memory when the list is loaded, otherwise queue
/// them behind the load (starting it if nothing has yet).
fn cache_record(state: &mut Cache, paths: &[PathBuf]) -> CacheAction {
    match state {
        Cache::Loaded(list) => {
            if promote(list, paths) {
                CacheAction::Persist(list.clone())
            } else {
                CacheAction::Nothing
            }
        }
        Cache::Loading { pending } => {
            pending.push(paths.to_vec());
            CacheAction::Nothing
        }
        Cache::Unloaded => {
            *state = Cache::Loading {
                pending: vec![paths.to_vec()],
            };
            CacheAction::SpawnLoad
        }
    }
}

/// Pure: drop `path` from a loaded list; a list still loading has nothing
/// the user could have clicked.
fn cache_forget(state: &mut Cache, path: &Path) -> CacheAction {
    let Cache::Loaded(list) = state else {
        return CacheAction::Nothing;
    };
    let before = list.len();
    list.retain(|entry| entry.path != path);
    if list.len() == before {
        CacheAction::Nothing
    } else {
        CacheAction::Persist(list.clone())
    }
}

/// Pure: install the loaded list and replay the promotions queued while it
/// was in flight, in the order they were recorded. Returns the snapshot to
/// persist when the replay changed the list. A publish onto a cache that is
/// already loaded is ignored, so a second load can never roll back a
/// promotion.
fn cache_publish(state: &mut Cache, loaded: Vec<RecentWorkspace>) -> CacheAction {
    let pending = match std::mem::replace(state, Cache::Unloaded) {
        Cache::Loading { pending } => pending,
        Cache::Unloaded => Vec::new(),
        Cache::Loaded(existing) => {
            *state = Cache::Loaded(existing);
            return CacheAction::Nothing;
        }
    };
    let mut list = loaded;
    let mut changed = false;
    for batch in &pending {
        changed |= promote(&mut list, batch);
    }
    *state = Cache::Loaded(list.clone());
    if changed {
        CacheAction::Persist(list)
    } else {
        CacheAction::Nothing
    }
}

/// Run the one background load: read and prune `recents.json` on the pool,
/// then publish on the GPUI thread and repaint so the empty-state rows show.
fn spawn_load(cx: &App) {
    let path = recents_path();
    cx.spawn(async move |cx: &mut gpui::AsyncApp| {
        let loaded = smol::unblock(move || path.map(|p| load_pruned(&p)).unwrap_or_default()).await;
        cx.update(|cx| {
            let action = cache_publish(&mut cache(), loaded);
            let has_rows = matches!(&*cache(), Cache::Loaded(list) if !list.is_empty());
            if let CacheAction::Persist(snapshot) = action {
                persist(snapshot, cx);
            }
            if has_rows {
                cx.refresh_windows();
            }
        });
    })
    .detach();
}

/// Start the background load at boot so the empty state fills within
/// milliseconds on a local disk. Idempotent: only the first call loads.
pub(crate) fn warm(cx: &App) {
    if cache_read(&mut cache()).1 == CacheAction::SpawnLoad {
        spawn_load(cx);
    }
}

/// The current list, newest first. Never touches the disk on the caller's
/// thread: empty until the background load has published, and the first
/// reader starts that load if `warm` has not already.
pub(crate) fn current(cx: &App) -> Vec<RecentWorkspace> {
    let (list, action) = cache_read(&mut cache());
    if action == CacheAction::SpawnLoad {
        spawn_load(cx);
    }
    list
}

/// Promote `paths` (left to right, so the first ends up at the head) and
/// persist the list off the render thread when it changed. Before the load
/// has published the promotion is queued and replayed on publish.
pub(crate) fn record(paths: &[PathBuf], cx: &App) {
    if paths.is_empty() {
        return;
    }
    apply(cache_record(&mut cache(), paths), cx);
}

/// Drop one folder (the user clicked a row whose directory vanished).
pub(crate) fn forget(path: &Path, cx: &App) {
    apply(cache_forget(&mut cache(), path), cx);
}

fn apply(action: CacheAction, cx: &App) {
    match action {
        CacheAction::Nothing => {}
        CacheAction::SpawnLoad => spawn_load(cx),
        CacheAction::Persist(snapshot) => persist(snapshot, cx),
    }
}

/// Snapshot order. Every `persist` call runs on the GPUI thread (record,
/// forget, and the load's publish all do), so a generation taken here is
/// the order the snapshots were produced in, whatever order the pool
/// writes them.
static PERSIST_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Serializes the writers and remembers the newest generation on disk, so
/// an older snapshot whose pool task ran late can never rename over a
/// newer one (a stale ordering or a resurrected forgotten row on the next
/// launch).
static WRITE_GATE: Mutex<u64> = Mutex::new(0);

fn persist(workspaces: Vec<RecentWorkspace>, cx: &App) {
    let Some(path) = recents_path() else {
        return;
    };
    let generation = PERSIST_GENERATION.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
    cx.background_spawn(async move {
        smol::unblock(move || write_ordered(&WRITE_GATE, &path, generation, &workspaces)).await;
    })
    .detach();
}

/// Write `workspaces` unless a newer generation already reached the disk.
/// Holding `gate` across the write is what orders publication; returns
/// whether this snapshot was the one written. A failed write still
/// advances the gate: the newer snapshot must not be replaced by an older
/// one that happens to succeed afterwards.
fn write_ordered(
    gate: &Mutex<u64>,
    path: &Path,
    generation: u64,
    workspaces: &[RecentWorkspace],
) -> bool {
    let mut newest = gate.lock().unwrap_or_else(PoisonError::into_inner);
    if generation <= *newest {
        log::debug!(
            "recents: skipping stale snapshot {generation} (generation {} already written)",
            *newest
        );
        return false;
    }
    write_to_disk(path, workspaces);
    *newest = generation;
    true
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
    fn an_older_snapshot_that_writes_late_never_replaces_a_newer_one() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("recents.json");
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        std::fs::create_dir(&a).expect("mkdir");
        std::fs::create_dir(&b).expect("mkdir");
        let gate = Mutex::new(0);
        let newer = vec![RecentWorkspace {
            path: b.clone(),
            title: "b".into(),
        }];
        let older = vec![
            RecentWorkspace {
                path: a.clone(),
                title: "a".into(),
            },
            RecentWorkspace {
                path: b.clone(),
                title: "b".into(),
            },
        ];
        assert!(
            write_ordered(&gate, &path, 2, &newer),
            "generation 2 writes"
        );
        assert!(
            !write_ordered(&gate, &path, 1, &older),
            "generation 1 arriving late is refused"
        );
        assert_eq!(
            load_pruned(&path),
            newer,
            "the forgotten row must not be resurrected"
        );
        assert!(
            write_ordered(&gate, &path, 3, &older),
            "a newer generation writes"
        );
        assert_eq!(load_pruned(&path), older);
    }

    #[test]
    fn concurrent_writers_leave_the_highest_generation_on_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("recents.json");
        let folders: Vec<PathBuf> = (0..8).map(|i| dir.path().join(format!("f{i}"))).collect();
        for folder in &folders {
            std::fs::create_dir(folder).expect("mkdir");
        }
        let gate = std::sync::Arc::new(Mutex::new(0));
        let start = std::sync::Arc::new(std::sync::Barrier::new(folders.len()));
        let handles: Vec<_> = folders
            .iter()
            .enumerate()
            .rev()
            .map(|(generation, folder)| {
                let gate = gate.clone();
                let start = start.clone();
                let path = path.clone();
                let snapshot = vec![RecentWorkspace {
                    path: folder.clone(),
                    title: format!("f{generation}"),
                }];
                std::thread::spawn(move || {
                    start.wait();
                    write_ordered(&gate, &path, generation as u64 + 1, &snapshot)
                })
            })
            .collect();
        let written = handles
            .into_iter()
            .map(|h| h.join().expect("writer"))
            .filter(|wrote| *wrote)
            .count();
        assert!(written >= 1);
        let on_disk = load_pruned(&path);
        assert_eq!(on_disk.len(), 1);
        assert_eq!(
            on_disk[0].path,
            folders[folders.len() - 1],
            "whatever the scheduling, the newest snapshot is what the next launch reads"
        );
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
    fn a_cold_cache_reads_empty_and_starts_exactly_one_load() {
        let mut state = Cache::Unloaded;
        let (list, action) = cache_read(&mut state);
        assert!(
            list.is_empty(),
            "nothing is shown before the load publishes"
        );
        assert_eq!(action, CacheAction::SpawnLoad);
        let (list, action) = cache_read(&mut state);
        assert!(list.is_empty());
        assert_eq!(
            action,
            CacheAction::Nothing,
            "a second reader must not load again"
        );
        assert_eq!(
            cache_publish(&mut state, vec![entry("/a")]),
            CacheAction::Nothing
        );
        assert_eq!(cache_read(&mut state).0, vec![entry("/a")]);
    }

    #[test]
    fn promotions_recorded_during_the_load_are_replayed_in_order_on_publish() {
        let mut state = Cache::Unloaded;
        assert_eq!(
            cache_record(&mut state, &[PathBuf::from("/first")]),
            CacheAction::SpawnLoad,
            "the first record on a cold cache starts the load"
        );
        assert_eq!(
            cache_record(&mut state, &[PathBuf::from("/second")]),
            CacheAction::Nothing,
            "a record while loading queues"
        );
        let action = cache_publish(&mut state, vec![entry("/old")]);
        let CacheAction::Persist(list) = action else {
            panic!("replaying queued promotions must persist, got {action:?}");
        };
        let paths: Vec<PathBuf> = list.into_iter().map(|e| e.path).collect();
        assert_eq!(
            paths,
            vec![
                PathBuf::from("/second"),
                PathBuf::from("/first"),
                PathBuf::from("/old")
            ],
            "later records win the head, the loaded file trails"
        );
        assert_eq!(
            cache_publish(&mut state, vec![entry("/stale")]),
            CacheAction::Nothing,
            "a publish onto a loaded cache never rolls back a promotion"
        );
        assert_eq!(cache_read(&mut state).0[0].path, PathBuf::from("/second"));
    }

    #[test]
    fn record_and_forget_on_a_loaded_cache_persist_only_real_changes() {
        let mut state = Cache::Loaded(vec![entry("/a"), entry("/b")]);
        assert_eq!(
            cache_record(&mut state, &[PathBuf::from("/a")]),
            CacheAction::Nothing
        );
        assert!(matches!(
            cache_record(&mut state, &[PathBuf::from("/b")]),
            CacheAction::Persist(_)
        ));
        assert_eq!(
            cache_forget(&mut state, Path::new("/nope")),
            CacheAction::Nothing
        );
        assert!(matches!(
            cache_forget(&mut state, Path::new("/a")),
            CacheAction::Persist(_)
        ));
        assert_eq!(cache_read(&mut state).0, vec![entry("/b")]);
        let mut loading = Cache::Loading {
            pending: Vec::new(),
        };
        assert_eq!(
            cache_forget(&mut loading, Path::new("/a")),
            CacheAction::Nothing,
            "nothing to forget before the load publishes"
        );
    }

    /// The load, and the `is_dir` prune inside it, run only on the
    /// background pool: the sole production call to `load_pruned` sits in
    /// the `smol::unblock` closure of `spawn_load`.
    #[test]
    fn the_file_is_only_ever_read_inside_the_background_load() {
        let production = include_str!("recents.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("production half");
        let calls: Vec<&str> = production
            .lines()
            .filter(|line| line.contains("load_pruned(&"))
            .collect();
        assert_eq!(calls.len(), 1, "exactly one load call site: {calls:?}");
        assert!(
            calls[0].contains("smol::unblock("),
            "the load must run inside smol::unblock, got {:?}",
            calls[0]
        );
        let is_dir_sites = production
            .lines()
            .filter(|line| line.contains(".is_dir()"))
            .count();
        assert_eq!(is_dir_sites, 1, "is_dir belongs to prune alone");
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
