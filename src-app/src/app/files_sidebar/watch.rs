//! Files sidebar live filesystem watch + per-workspace expansion persistence
//! (PRD `prd-files-tree-sidebar-2026-Q3`, EP-002).
//!
//! `spawn_files_hydration` reads the tree + registers non-recursive `notify`
//! watches for the root and expanded dirs off the render thread (US-018; US-005
//! wiring, degrading gracefully on failure per US-006); the background drain loop in `bootstrap` debounces +
//! coalesces events and calls `refresh_files_dirs` for the targeted
//! per-directory re-read. `sync_files_expansion` mirrors the live expansion into
//! the active `Workspace` so it persists to `session.json` (US-007). Split out
//! of `mod.rs` to keep each file under the 250-line budget.

use std::path::{Path, PathBuf};

use gpui::Context;

use crate::PaneFlowApp;
use crate::app::files_tree;

fn should_apply_files_hydration(
    sidebar_open: bool,
    current_root: &Path,
    expected_root: &Path,
    current_generation: u64,
    expected_generation: u64,
) -> bool {
    sidebar_open && current_root == expected_root && current_generation == expected_generation
}

fn should_apply_files_dir_refresh(current_seq: u64, expected_seq: u64) -> bool {
    current_seq == expected_seq
}

/// Blocking re-read of already-cached directories. Call from `smol::unblock`.
fn reread_cached_dirs(
    root: &Path,
    dirs: Vec<PathBuf>,
) -> Vec<(PathBuf, Vec<files_tree::FileNode>)> {
    dirs.into_iter()
        .map(|dir| {
            let listing = files_tree::read_dir_sorted(root, &dir);
            (dir, listing)
        })
        .collect()
}

impl PaneFlowApp {
    /// Mirror the live tree's expansion into the active workspace (excluding
    /// the implicit root) so it survives close/reopen and persists to
    /// `session.json` (US-007).
    pub(super) fn sync_files_expansion(&mut self) {
        let root = self.files_tree.root.clone();
        let mut expanded: Vec<PathBuf> = self
            .files_tree
            .expanded
            .iter()
            .filter(|p| **p != root)
            .cloned()
            .collect();
        expanded.sort();
        if let Some(ws) = self.workspaces.get_mut(self.active_idx) {
            ws.files_expanded = expanded;
        }
    }

    /// US-018: hydrate the Files tree and install non-recursive watches off the
    /// GPUI main thread.
    ///
    /// A recursive `notify` watch walks the entire subtree at registration
    /// (inotify adds one watch per directory), so large repos can freeze the UI
    /// and exhaust OS watcher budgets. We instead watch only the root and
    /// currently-expanded directories. Collapsed subtrees are read and watched
    /// lazily on expand.
    pub(crate) fn spawn_files_hydration(
        &mut self,
        root: PathBuf,
        persisted: Vec<PathBuf>,
        cx: &mut Context<Self>,
    ) {
        self.files_hydrate_generation = self.files_hydrate_generation.wrapping_add(1);
        self.files_dir_refresh_seq.clear();
        let generation = self.files_hydrate_generation;
        // Drop the previous watch + channel immediately (cheap), and show a
        // root shell so the panel paints this frame while the reads run.
        self.files_watcher = None;
        self.files_event_rx = None;
        self.files_tree = files_tree::FilesTreeState::root_shell(root.clone());

        cx.spawn(
            async move |this: gpui::WeakEntity<Self>, cx: &mut gpui::AsyncApp| {
                // Stage 1: directory reads. Inject the populated tree first so
                // content appears before watch registration completes.
                let tree = smol::unblock({
                    let root = root.clone();
                    let persisted = persisted.clone();
                    move || files_tree::FilesTreeState::hydrated(root, &persisted)
                })
                .await;
                let watch_dirs = tree.expanded.iter().cloned().collect::<Vec<_>>();
                let still_current = this
                    .update(cx, |app, cx| {
                        if should_apply_files_hydration(
                            app.files_sidebar_open,
                            &app.files_tree.root,
                            &root,
                            app.files_hydrate_generation,
                            generation,
                        ) {
                            app.files_tree = tree;
                            app.sync_files_expansion();
                            app.clamp_files_selection();
                            cx.notify();
                            true
                        } else {
                            false
                        }
                    })
                    .unwrap_or(false);
                if !still_current {
                    return;
                }

                // Stage 2: non-recursive watch registration for the visible
                // tree frontier. The watcher is still built off-thread because
                // some platforms do synchronous filesystem work at registration.
                let built = smol::unblock({
                    let root = root.clone();
                    move || build_files_watcher(&root, &watch_dirs)
                })
                .await;
                let _ = this.update(cx, |app, _cx| {
                    if should_apply_files_hydration(
                        app.files_sidebar_open,
                        &app.files_tree.root,
                        &root,
                        app.files_hydrate_generation,
                        generation,
                    ) && let Some((watcher, rx)) = built
                    {
                        app.files_watcher = Some(watcher);
                        app.files_event_rx = Some(rx);
                    }
                });
            },
        )
        .detach();
    }

    pub(super) fn watch_files_dir(&mut self, dir: &Path) {
        let Some(watcher) = self.files_watcher.as_mut() else {
            return;
        };
        use notify::Watcher;
        if let Err(e) = watcher.watch(dir, notify::RecursiveMode::NonRecursive) {
            log::warn!(
                "files watcher: failed to watch expanded dir {} ({e}); falling back to on-expand reads for it",
                dir.display()
            );
        }
    }

    pub(super) fn unwatch_files_dir(&mut self, dir: &Path) {
        if dir == self.files_tree.root {
            return;
        }
        let Some(watcher) = self.files_watcher.as_mut() else {
            return;
        };
        use notify::Watcher;
        if let Err(e) = watcher.unwatch(dir) {
            tracing::debug!(
                target: "paneflow_app::files_sidebar",
                "files watcher: unwatch {} failed: {e}",
                dir.display()
            );
        }
    }

    /// Apply a debounced, prefix-coalesced batch of changed directories
    /// (US-005), called from the background drain loop in `bootstrap`. Re-reads
    /// only the cached (expanded) directories among the affected parents - a
    /// change under a collapsed/uncached dir is ignored until it's expanded
    /// (then read fresh by `toggle_dir`). `rescan` (a notify overflow/Rescan
    /// signal, US-006 AC3) forces a root re-read. Never walks the whole tree.
    ///
    /// Directory reads run on a background executor keyed by
    /// `files_hydrate_generation`. Listings apply on the GPUI thread only if
    /// the sidebar is still open and the root and generation still match.
    pub(crate) fn refresh_files_dirs(
        &mut self,
        mut dirs: Vec<PathBuf>,
        rescan: bool,
        cx: &mut Context<Self>,
    ) {
        if !self.files_sidebar_open {
            return;
        }
        let root = self.files_tree.root.clone();
        if rescan {
            dirs.push(root.clone());
        }
        // AC4: only re-read directories we've already cached (expanded).
        let to_reread: Vec<PathBuf> = files_tree::coalesce_by_prefix(dirs)
            .into_iter()
            .filter(|dir| self.files_tree.children.contains_key(dir))
            .collect();
        if to_reread.is_empty() {
            return;
        }
        let generation = self.files_hydrate_generation;
        let sequenced: Vec<(PathBuf, u64)> = to_reread
            .iter()
            .map(|dir| {
                let seq = self
                    .files_dir_refresh_seq
                    .entry(dir.clone())
                    .and_modify(|seq| *seq = seq.wrapping_add(1))
                    .or_insert(1);
                (dir.clone(), *seq)
            })
            .collect();
        cx.spawn(
            async move |this: gpui::WeakEntity<Self>, cx: &mut gpui::AsyncApp| {
                let listings = smol::unblock({
                    let root = root.clone();
                    let dirs = sequenced
                        .iter()
                        .map(|(dir, _)| dir.clone())
                        .collect::<Vec<_>>();
                    move || reread_cached_dirs(&root, dirs)
                })
                .await;
                let _ = this.update(cx, |app, cx| {
                    if !should_apply_files_hydration(
                        app.files_sidebar_open,
                        &app.files_tree.root,
                        &root,
                        app.files_hydrate_generation,
                        generation,
                    ) {
                        return;
                    }
                    let mut changed = false;
                    for ((dir, seq), listing) in sequenced
                        .into_iter()
                        .zip(listings.into_iter().map(|(_, listing)| listing))
                    {
                        let current_seq = app.files_dir_refresh_seq.get(&dir).copied().unwrap_or(0);
                        if !should_apply_files_dir_refresh(current_seq, seq) {
                            continue;
                        }
                        if let std::collections::hash_map::Entry::Occupied(mut e) =
                            app.files_tree.children.entry(dir)
                        {
                            e.insert(listing);
                            changed = true;
                        }
                    }
                    if changed {
                        app.clamp_files_selection();
                        cx.notify();
                    }
                });
            },
        )
        .detach();
    }
}

/// US-018: build non-recursive `notify` watches for the root and currently
/// expanded directories, returning the watcher + its event channel, or `None`
/// on failure. The caller falls back to on-expand reads (US-006).
///
/// Runs on a background thread; the caller re-injects the returned handles.
#[allow(clippy::type_complexity)]
fn build_files_watcher(
    root: &Path,
    dirs: &[PathBuf],
) -> Option<(
    notify::RecommendedWatcher,
    std::sync::mpsc::Receiver<notify::Result<notify::Event>>,
)> {
    use notify::Watcher;
    let (tx, rx) = std::sync::mpsc::channel();
    let mut watcher = match notify::recommended_watcher(tx) {
        Ok(w) => w,
        Err(e) => {
            log::warn!("files watcher unavailable: {e}; falling back to on-expand reads");
            return None;
        }
    };
    let mut watched = std::collections::HashSet::new();
    for dir in std::iter::once(root.to_path_buf()).chain(dirs.iter().cloned()) {
        if !watched.insert(dir.clone()) {
            continue;
        }
        if let Err(e) = watcher.watch(&dir, notify::RecursiveMode::NonRecursive) {
            log::warn!(
                "files watcher: failed to watch {} ({e}); falling back to on-expand reads",
                dir.display()
            );
            if dir.as_path() == root {
                return None;
            }
        }
    }
    Some((watcher, rx))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{should_apply_files_dir_refresh, should_apply_files_hydration};
    use notify::Watcher;

    #[test]
    fn files_hydration_ignores_stale_generation() {
        let root = Path::new("/repo");
        let other = Path::new("/other");
        let current_generation = 2;
        let stale_generation = 1;
        let mut files_tree_generation = current_generation;
        let mut files_event_rx_generation = Some(current_generation);

        if should_apply_files_hydration(true, root, root, current_generation, stale_generation) {
            files_tree_generation = stale_generation;
        }
        if should_apply_files_hydration(true, root, root, current_generation, stale_generation) {
            files_event_rx_generation = Some(stale_generation);
        }

        assert_eq!(files_tree_generation, current_generation);
        assert_eq!(files_event_rx_generation, Some(current_generation));

        assert!(should_apply_files_hydration(
            true,
            root,
            root,
            current_generation,
            current_generation,
        ));
        assert!(!should_apply_files_hydration(
            false,
            root,
            root,
            current_generation,
            current_generation,
        ));
        assert!(!should_apply_files_hydration(
            true,
            other,
            root,
            current_generation,
            current_generation,
        ));
        assert!(!should_apply_files_hydration(
            true,
            root,
            root,
            current_generation,
            stale_generation,
        ));
    }

    #[test]
    fn files_refresh_apply_uses_hydration_generation_guard() {
        let src = include_str!("watch.rs");
        let body = src
            .split("pub(crate) fn refresh_files_dirs(")
            .nth(1)
            .and_then(|rest| rest.split("/// US-018: build non-recursive").next())
            .expect("refresh_files_dirs body");
        assert!(
            !body.contains("read_dir_sorted"),
            "refresh_files_dirs must not re-read directories on the GPUI thread: {body}"
        );
        assert!(
            body.contains("should_apply_files_hydration"),
            "refresh apply must use the same generation guard as hydration: {body}"
        );
        assert!(
            body.contains("files_hydrate_generation"),
            "refresh apply must key listings by files_hydrate_generation: {body}"
        );
        assert!(
            body.contains("smol::unblock"),
            "refresh must re-read on a background executor: {body}"
        );
        assert!(
            body.contains("files_dir_refresh_seq"),
            "refresh apply must key listings by per-directory sequence: {body}"
        );
        assert!(
            body.contains("should_apply_files_dir_refresh"),
            "refresh apply must drop superseded per-directory listings: {body}"
        );

        let root = Path::new("/repo");
        let current_generation = 2;
        let stale_generation = 1;
        let mut listing_generation = current_generation;
        if should_apply_files_hydration(true, root, root, current_generation, stale_generation) {
            listing_generation = stale_generation;
        }
        assert_eq!(listing_generation, current_generation);
        assert!(should_apply_files_hydration(
            true,
            root,
            root,
            current_generation,
            current_generation,
        ));
    }

    #[test]
    fn files_dir_refresh_seq_is_independent_per_directory() {
        assert!(should_apply_files_dir_refresh(2, 2));
        assert!(!should_apply_files_dir_refresh(2, 1));
        assert!(
            should_apply_files_dir_refresh(1, 1),
            "a later refresh of A must not drop an in-flight listing for B"
        );
    }

    #[test]
    fn files_watcher_keeps_root_watch_when_one_expanded_dir_fails() {
        let root = tempfile::tempdir().expect("tempdir");
        let missing = root.path().join("does-not-exist");
        assert!(!missing.exists(), "expanded dir must fail to watch");

        let built = super::build_files_watcher(root.path(), &[missing]);
        let (mut watcher, _rx) = built.expect(
            "a failed expanded-dir watch must keep the watcher when the root watch succeeded",
        );
        assert!(
            watcher.unwatch(root.path()).is_ok(),
            "root watch must remain after a later expanded-dir watch error"
        );
    }
}
