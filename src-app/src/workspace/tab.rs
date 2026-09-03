//! Workspace tab - the level that owns a pane layout tree.
//!
//! US-001 (prd-cli-tab-hierarchy): a workspace no longer owns a single
//! `LayoutTree`; it owns a list of [`Tab`], each carrying the tree the
//! workspace used to carry, plus the zoom `saved_layout` that used to live at
//! the workspace level. Split, zoom and focus mechanics are unchanged - they
//! now operate one level down.

use gpui::{App, Entity, Window};
use paneflow_config::schema::LayoutNode;

use crate::layout::LayoutTree;
use crate::pane::Pane;

/// Monotonic tab ID counter. Process-local and never persisted: the IPC
/// surface addresses surfaces by `surface_id`, never by tab index or tab id
/// (FR-07), so a stable in-memory identity is all the UI needs.
static NEXT_TAB_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

pub fn next_tab_id() -> u64 {
    NEXT_TAB_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// One working composition inside a workspace: a title plus the pane layout
/// tree, with the zoom bookkeeping that belongs to it.
pub struct Tab {
    /// Unique tab identifier, assigned at construction.
    pub id: u64,
    /// User-facing title. Empty means "unnamed" - the sidebar derives a
    /// fallback label (US-009).
    pub title: String,
    /// Pane layout tree. `None` for an empty tab (every pane closed).
    pub root: Option<LayoutTree>,
    /// Saved layout tree while zoomed. `Some(tree)` means this tab is zoomed
    /// and `root` holds only the zoomed pane as a single Leaf.
    pub saved_layout: Option<LayoutTree>,
    /// The git worktree this tab works in, or `None` for an unbound tab
    /// (issue #347, upstream discussion #41).
    ///
    /// This is the tab's git identity: bound, every pane opened in the tab
    /// starts in this checkout, and the sidebar row names the branch it sits
    /// on. Unbound, the tab behaves exactly as it always has and its row says
    /// nothing extra - the line only appears where it tells two tabs apart.
    ///
    /// A path, not a branch name: a branch can only be checked out in one
    /// worktree at a time, the checkout directory is what a pane actually
    /// needs to spawn in, and a detached-HEAD worktree has no branch at all.
    /// The branch is derived for display. Persisted as `TabSession::worktree`;
    /// a path that no longer exists at restore is dropped.
    pub worktree: Option<std::path::PathBuf>,
    /// Whether this tab wants the docked Files rail on screen.
    ///
    /// The rail itself is a single app-level surface (one tree, one watcher),
    /// but *wanting* it is a property of the session that asked for it: opening
    /// the tree in one tab must not put it in front of a sibling tab. The app
    /// mirrors the visible tab's flag and reconciles on every session change
    /// ([`crate::PaneFlowApp::sync_files_sidebar_session`]). Never persisted -
    /// like the app-level mirror, a restart starts every tab closed.
    pub files_sidebar_open: bool,
}

impl Tab {
    /// Create a tab holding `root`, with a freshly allocated id.
    pub fn new(title: impl Into<String>, root: Option<LayoutTree>) -> Self {
        Self {
            id: next_tab_id(),
            title: title.into(),
            root,
            saved_layout: None,
            worktree: None,
            files_sidebar_open: false,
        }
    }

    /// Rebuild a tab from a session record or an `up` batch: like
    /// [`Tab::new`], plus the worktree it was bound to.
    pub fn restored(
        title: impl Into<String>,
        root: Option<LayoutTree>,
        worktree: Option<std::path::PathBuf>,
    ) -> Self {
        Self {
            worktree,
            ..Self::new(title, root)
        }
    }

    /// The cwd a pane opened in this tab must start in, given the cwd it would
    /// otherwise have inherited.
    ///
    /// An unbound tab keeps `inherited` untouched. A bound tab keeps it only
    /// while it stays inside its own checkout - splitting from a pane sitting
    /// in `src/` must land in `src/`, not back at the root - and pulls anything
    /// outside back to the worktree. That last clause is the whole safety
    /// property: without it a `cd` in one pane leaks into every pane opened
    /// after it, and the binding is decoration.
    pub fn confine_cwd(&self, inherited: Option<std::path::PathBuf>) -> Option<std::path::PathBuf> {
        let Some(worktree) = self.worktree.as_ref() else {
            return inherited;
        };
        match inherited {
            Some(cwd) if cwd.starts_with(worktree) => Some(cwd),
            _ => Some(worktree.clone()),
        }
    }

    /// Create an untitled tab with no pane. Used to honour the "a workspace
    /// always has at least one tab" invariant (FR-01) when the last tab is
    /// closed.
    pub fn empty() -> Self {
        Self::new(String::new(), None)
    }

    pub fn is_zoomed(&self) -> bool {
        self.saved_layout.is_some()
    }

    /// Leave zoom, restoring the saved tree. Returns the pane that was zoomed.
    pub fn exit_zoom(&mut self, cx: &mut App) -> Option<Entity<Pane>> {
        let zoomed_pane = self.root.as_ref().and_then(|root| root.first_leaf());
        let saved = self.saved_layout.take()?;
        self.root = Some(saved);
        if let Some(pane) = &zoomed_pane {
            pane.update(cx, |pane, _| {
                pane.zoomed = false;
            });
        }
        zoomed_pane
    }

    pub fn pane_count(&self) -> usize {
        // Mirror `serialize`'s precedence, and `contains_pane` / `any_pane`, which
        // both already consult BOTH trees. Under zoom the real layout lives in
        // `saved_layout` and `root` holds only the zoomed pane, so reading `root`
        // alone reported 1 - making `can_add_pane()` effectively unconditional.
        // Every pane added while zoomed was then silently discarded by
        // `exit_zoom`'s `self.root = Some(saved)`, taking its PTY and any running
        // agent with it.
        self.saved_layout
            .as_ref()
            .or(self.root.as_ref())
            .map_or(0, |r| r.leaf_count())
    }

    /// Whether this tab can take one more pane.
    ///
    /// US-003 (prd-cli-tab-hierarchy): `MAX_PANES` bounds a *tab*, not a
    /// workspace. Every create site - keyboard split, drop-to-split, launch
    /// pad, IPC `surface.split` - gates on this single predicate so the cap
    /// cannot drift between them.
    pub fn can_add_pane(&self) -> bool {
        self.pane_count() < crate::layout::MAX_PANES
    }

    pub fn contains_pane(&self, pane: &Entity<Pane>) -> bool {
        self.root
            .as_ref()
            .is_some_and(|root| root.contains_leaf(pane))
            || self
                .saved_layout
                .as_ref()
                .is_some_and(|saved| saved.contains_leaf(pane))
    }

    pub fn any_pane(&self, f: &mut impl FnMut(&Entity<Pane>) -> bool) -> bool {
        if let Some(root) = &self.root
            && root.any_leaf(f)
        {
            return true;
        }
        if let Some(saved) = &self.saved_layout
            && saved.any_leaf(f)
        {
            return true;
        }
        false
    }

    /// Every pane of this tab, the zoom-saved tree included, in traversal
    /// order and without duplicates.
    /// Every terminal surface under this tab, by entity id (#184 Phase 3.8:
    /// the agent-status path asks whether an observed pane is on screen).
    pub fn surface_ids(&self, cx: &gpui::App) -> std::collections::HashSet<u64> {
        let mut ids = std::collections::HashSet::new();
        for pane in self.collect_panes() {
            for terminal in pane.read(cx).terminals() {
                ids.insert(terminal.entity_id().as_u64());
            }
        }
        ids
    }

    pub fn collect_panes(&self) -> Vec<Entity<Pane>> {
        let mut panes = Vec::new();
        if let Some(root) = &self.root {
            panes.extend(root.collect_leaves());
        }
        if let Some(saved) = &self.saved_layout {
            for pane in saved.collect_leaves() {
                if !panes.contains(&pane) {
                    panes.push(pane);
                }
            }
        }
        panes
    }

    /// Focus this tab's first pane. Returns `true` when focus actually landed
    /// on a pane; `false` for an empty tab (no root), which has nothing to
    /// focus and leaves the caller to park focus somewhere else.
    ///
    /// `#[must_use]`: dropping the report is the issue #108 bug - the window
    /// then keeps naming an unmounted element and every global binding goes
    /// inert. Callers must park focus themselves on `false`.
    #[must_use]
    pub fn focus_first(&self, window: &mut Window, cx: &mut App) -> bool {
        match &self.root {
            Some(root) => {
                root.focus_first(window, cx);
                true
            }
            None => false,
        }
    }

    /// Serialize this tab's layout to a `LayoutNode`.
    ///
    /// When zoomed, serializes the saved (un-zoomed) layout so the full pane
    /// arrangement is captured rather than just the single zoomed pane.
    pub fn serialize(&self, cx: &App) -> Option<LayoutNode> {
        let tree = self.saved_layout.as_ref().or(self.root.as_ref())?;
        Some(tree.serialize(cx))
    }

    /// Serialize this tab for session persistence without terminal output,
    /// which must remain local to the current process.
    pub fn serialize_without_scrollback(&self, cx: &App) -> Option<LayoutNode> {
        let tree = self.saved_layout.as_ref().or(self.root.as_ref())?;
        Some(tree.serialize_without_scrollback(cx))
    }
}

/// Copy a CLI pane rename onto its tab so the sidebar row matches the pane
/// header. `None` (or whitespace) clears the stored title so the row goes
/// back to deriving from the pane.
pub(crate) fn apply_pane_rename_to_tab(tab: &mut Tab, new_name: Option<&str>) {
    tab.title = new_name.unwrap_or("").trim().to_string();
}

/// A tab binding that can still be honoured: the path, when it is a directory
/// now, and `None` otherwise (issue #347).
///
/// Every path that rebuilds a bound tab - session restore, undo-close-tab,
/// undo-close-workspace, a pick from a cached worktree listing - runs its
/// binding through here, so a checkout removed in the meantime (by
/// `git worktree remove`, by another PaneFlow session's teardown, by hand)
/// restores the tab unbound instead of pinning every pane it opens to a
/// missing directory. A plain `is_dir`, no canonicalization: a stat is
/// bounded where resolving symlinks across a dead mount is not.
pub fn existing_worktree_dir(worktree: Option<std::path::PathBuf>) -> Option<std::path::PathBuf> {
    worktree.filter(|path| path.is_dir())
}

/// Where the panes of a tab rebuilt with binding `worktree` spawn: the bound
/// checkout, or the workspace root for an unbound tab (issue #347).
///
/// Persisted surfaces carry no cwd of their own, so this fallback IS the
/// spawn directory. Handing the workspace root to a bound tab restored a row
/// that named `feat/x` above a shell sitting in the repository on `main`,
/// with `confine_cwd` then pinning only the panes opened after it.
pub fn tab_spawn_root(
    worktree: Option<&std::path::Path>,
    workspace_cwd: &std::path::Path,
) -> std::path::PathBuf {
    worktree.map_or_else(|| workspace_cwd.to_path_buf(), std::path::Path::to_path_buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renaming_a_cli_pane_overwrites_the_tab_title() {
        let mut tab = Tab::new("Claude", None);
        apply_pane_rename_to_tab(&mut tab, Some("logs"));
        assert_eq!(
            tab.title, "logs",
            "a pane rename must replace the palette/preset title the tab was created with"
        );
        apply_pane_rename_to_tab(&mut tab, None);
        assert!(
            tab.title.is_empty(),
            "clearing the pane name must un-freeze the tab so derivation can resume"
        );
        apply_pane_rename_to_tab(&mut tab, Some("  "));
        assert!(
            tab.title.is_empty(),
            "whitespace is not a name: {title:?}",
            title = tab.title
        );
    }

    /// #184 Phase 4: the Files rail is wanted per tab, and never persisted, so
    /// every construction path starts closed.
    #[test]
    fn a_new_tab_starts_with_the_files_sidebar_closed() {
        assert!(!Tab::new("Claude", None).files_sidebar_open);
        assert!(!Tab::empty().files_sidebar_open);
    }

    #[test]
    fn a_rebuilt_tab_spawns_its_panes_in_its_worktree() {
        // Issue #347 review, finding 1: session restore and both undo paths
        // rebuild a bound tab's panes from the tab's own checkout, not the
        // workspace root - the row said `feat/x` while the shell sat on main.
        let root = std::path::Path::new("/repo");
        let worktree = std::path::Path::new("/repo.worktrees/feat-x");
        assert_eq!(tab_spawn_root(Some(worktree), root), worktree);
        assert_eq!(tab_spawn_root(None, root), root);
    }

    #[test]
    fn a_binding_to_a_missing_checkout_is_dropped_before_it_is_honoured() {
        // Issue #347 review, finding 4: the fast bind path and
        // undo-close-workspace skipped the "path still exists" rule the
        // session restore and undo-close-tab paths apply.
        let live = tempfile::tempdir().expect("tempdir");
        assert_eq!(
            existing_worktree_dir(Some(live.path().to_path_buf())),
            Some(live.path().to_path_buf())
        );
        let gone = live.path().join("removed-checkout");
        assert_eq!(existing_worktree_dir(Some(gone)), None);
        let file = live.path().join("not-a-dir");
        std::fs::write(&file, b"x").expect("write");
        assert_eq!(existing_worktree_dir(Some(file)), None);
        assert_eq!(existing_worktree_dir(None), None);
    }

    #[test]
    fn a_freshly_built_tab_is_unbound() {
        assert!(Tab::new("Claude", None).worktree.is_none());
        assert!(Tab::empty().worktree.is_none());
        let bound = std::path::PathBuf::from("/repo.worktrees/feat-x");
        assert_eq!(
            Tab::restored("feat", None, Some(bound.clone())).worktree,
            Some(bound)
        );
    }

    #[test]
    fn an_unbound_tab_inherits_whatever_it_was_given() {
        let tab = Tab::new("free", None);
        let inherited = std::path::PathBuf::from("/anywhere/at/all");
        assert_eq!(tab.confine_cwd(Some(inherited.clone())), Some(inherited));
        assert_eq!(tab.confine_cwd(None), None);
    }

    #[test]
    fn a_bound_tab_confines_every_pane_to_its_worktree() {
        let worktree = std::path::PathBuf::from("/repo.worktrees/feat-login");
        let tab = Tab::restored("login", None, Some(worktree.clone()));

        // Splitting from a pane deeper inside the same checkout keeps that
        // directory: the boundary is the worktree, not its root.
        let inside = worktree.join("src/auth");
        assert_eq!(tab.confine_cwd(Some(inside.clone())), Some(inside));

        // A pane that wandered out - a `cd` to the main checkout, to a sibling
        // worktree, to anywhere - does not drag the next pane out with it.
        assert_eq!(
            tab.confine_cwd(Some(std::path::PathBuf::from("/repo"))),
            Some(worktree.clone())
        );
        assert_eq!(
            tab.confine_cwd(Some(std::path::PathBuf::from(
                "/repo.worktrees/feat-billing"
            ))),
            Some(worktree.clone()),
            "a sibling worktree is outside, however similar its path looks"
        );

        // Nothing inherited at all still lands in the worktree, never at the
        // process cwd.
        assert_eq!(tab.confine_cwd(None), Some(worktree));
    }
}
