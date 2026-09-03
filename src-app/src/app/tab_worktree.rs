//! Git state for a checkout that is not a workspace root (issue #347).
//!
//! A workspace carries the branch and diffstat of its own cwd
//! (`Workspace::git_branch` / `git_stats`). A tab bound to a worktree needs
//! the same values for a *different* directory, and the sidebar reads them
//! every frame - so they are cached per checkout here and refreshed by the
//! same off-thread probes that already feed the workspace fields. Nothing in
//! this module runs git on the render thread: it holds what the bootstrap
//! watcher and the 30 s poll bring back, and every subprocess it starts goes
//! through `smol::unblock`.

use std::collections::HashMap;

use crate::PaneFlowApp;
use crate::workspace::{GitDiffStats, worktree::WorktreeEntry};
use gpui::Context;

/// The three values a bound tab's row needs about its checkout.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct CheckoutGit {
    /// Current branch, empty for a detached HEAD.
    pub branch: String,
    /// Whether the directory is inside a git repository at all.
    pub is_repo: bool,
    pub stats: GitDiffStats,
}

/// Per-checkout git state plus the worktree list and local branches of each
/// repository, all keyed by absolute path.
#[derive(Default)]
pub(crate) struct WorktreeStates {
    checkouts: HashMap<String, CheckoutGit>,
    /// `git worktree list` per repository root, for the pickers.
    listings: HashMap<String, Vec<WorktreeEntry>>,
    /// Local branches per repository root. What the picker actually offers -
    /// the listing only says which of them already has a directory.
    branches: HashMap<String, Vec<String>>,
}

impl WorktreeStates {
    /// Store a probe result. Returns `true` when it changed something, so the
    /// caller only repaints on a real delta - the same contract
    /// [`PaneFlowApp::apply_git_state_for_cwd`] already honors.
    pub(crate) fn set_checkout(&mut self, cwd: &str, state: CheckoutGit) -> bool {
        match self.checkouts.get(cwd) {
            Some(current) if *current == state => false,
            _ => {
                self.checkouts.insert(cwd.to_string(), state);
                true
            }
        }
    }

    /// [`Self::set_checkout`] for a probe taken at `probed`: stored only when
    /// that is the directory `cwd` names. The workspace fields follow a pane's
    /// shell wherever it goes, but this cache answers for a *checkout*, and a
    /// branch read in a foreign directory filed under the workspace-root key
    /// is what a tab bound there would show.
    pub(crate) fn set_checkout_probed_at(
        &mut self,
        cwd: &str,
        probed: &str,
        state: CheckoutGit,
    ) -> bool {
        if probed != cwd {
            return false;
        }
        self.set_checkout(cwd, state)
    }

    pub(crate) fn checkout(&self, cwd: &str) -> Option<&CheckoutGit> {
        self.checkouts.get(cwd)
    }

    pub(crate) fn set_listing(&mut self, repo_root: &str, entries: Vec<WorktreeEntry>) -> bool {
        match self.listings.get(repo_root) {
            Some(current) if *current == entries => false,
            _ => {
                self.listings.insert(repo_root.to_string(), entries);
                true
            }
        }
    }

    pub(crate) fn listing(&self, repo_root: &str) -> &[WorktreeEntry] {
        self.listings.get(repo_root).map_or(&[], Vec::as_slice)
    }

    pub(crate) fn set_branches(&mut self, repo_root: &str, branches: Vec<String>) -> bool {
        match self.branches.get(repo_root) {
            Some(current) if *current == branches => false,
            _ => {
                self.branches.insert(repo_root.to_string(), branches);
                true
            }
        }
    }

    pub(crate) fn branches(&self, repo_root: &str) -> &[String] {
        self.branches.get(repo_root).map_or(&[], Vec::as_slice)
    }

    /// Drop every entry no longer named by `live`. Called after a workspace or
    /// tab closes so a torn-down worktree does not keep a row's worth of state
    /// alive for the rest of the session.
    pub(crate) fn retain_live(&mut self, live: &std::collections::HashSet<String>) {
        self.checkouts.retain(|cwd, _| live.contains(cwd));
        self.listings.retain(|root, _| live.contains(root));
        self.branches.retain(|root, _| live.contains(root));
    }
}

/// Why a tab of workspace `ws_idx` cannot be bound to `path`, or `None` when
/// it can.
///
/// A checkout that another workspace owns as a `ManagedWorktree` - live, or
/// held by an undo record - is removed when that ownership ends, and a tab
/// bound to it would then spawn every pane into a missing directory. One the
/// retirement journal already names is going now. A workspace's own managed
/// checkouts are fine: that is exactly what `workspace.up` binds its tabs to.
/// Prefix matches both ways, like the ownership check `surface.split` runs,
/// so a binding cannot sit under or over an owned path either.
fn binding_refusal(
    path: &std::path::Path,
    ws_idx: usize,
    owned: &[(usize, std::path::PathBuf)],
    closed: &[std::path::PathBuf],
    pending: &[std::path::PathBuf],
) -> Option<&'static str> {
    let overlaps = |owned: &std::path::Path| owned.starts_with(path) || path.starts_with(owned);
    if pending.iter().any(|p| overlaps(p)) {
        return Some("That worktree is being retired");
    }
    if owned
        .iter()
        .any(|(owner, p)| *owner != ws_idx && overlaps(p))
    {
        return Some("That worktree belongs to another workspace");
    }
    if closed.iter().any(|p| overlaps(p)) {
        return Some("That worktree belongs to a closed workspace");
    }
    None
}

impl PaneFlowApp {
    /// Why the tab cannot be bound to `path` right now, as the toast to show,
    /// or `None` when it can: see [`binding_refusal`].
    pub(crate) fn tab_binding_refusal(
        &self,
        path: &std::path::Path,
        ws_idx: usize,
    ) -> Option<&'static str> {
        let owned: Vec<(usize, std::path::PathBuf)> = self
            .workspaces
            .iter()
            .enumerate()
            .flat_map(|(index, ws)| {
                ws.managed_worktrees
                    .iter()
                    .map(move |worktree| (index, worktree.path.clone()))
            })
            .collect();
        let closed: Vec<std::path::PathBuf> = self
            .closed_items
            .iter()
            .flat_map(|record| match record {
                crate::ClosedRecord::Workspace(ws) => ws
                    .managed_worktrees
                    .iter()
                    .map(|worktree| worktree.path.clone())
                    .collect::<Vec<_>>(),
                crate::ClosedRecord::Pane(_) | crate::ClosedRecord::Tab(_) => Vec::new(),
            })
            .collect();
        let pending: Vec<std::path::PathBuf> = self
            .pending_worktree_teardowns
            .iter()
            .map(|worktree| worktree.path.clone())
            .collect();
        binding_refusal(path, ws_idx, &owned, &closed, &pending)
    }

    /// Whether a picker may offer `path` to a tab of workspace `ws_idx`:
    /// the row is left out when binding to it would be refused.
    pub(crate) fn checkout_is_bindable(&self, path: &std::path::Path, ws_idx: usize) -> bool {
        self.tab_binding_refusal(path, ws_idx).is_none()
    }

    /// Bind a tab to a checkout the user picked, or say why not.
    ///
    /// The one door for a path chosen from a list: the picker's rows come
    /// from a listing read when it opened, and between then and the click the
    /// directory can have been removed or claimed. A refusal reaches the user
    /// as a toast and leaves the tab as it was. Returns whether it bound.
    pub(crate) fn bind_tab_to_checkout(
        &mut self,
        ws_idx: usize,
        tab_idx: usize,
        path: std::path::PathBuf,
        cx: &mut Context<Self>,
    ) -> bool {
        if let Some(reason) = self.tab_binding_refusal(&path, ws_idx) {
            self.show_toast(reason, cx);
            return false;
        }
        let Some(path) = crate::workspace::existing_worktree_dir(Some(path)) else {
            self.show_toast(
                "That checkout no longer exists; run `git worktree prune`",
                cx,
            );
            return false;
        };
        self.set_tab_worktree(ws_idx, tab_idx, Some(path), cx);
        true
    }

    /// Every checkout worth probing: each workspace root, plus the worktree of
    /// every bound tab. Deduplicated, so two tabs on one worktree cost one
    /// subprocess per tick rather than two.
    pub(crate) fn git_probe_cwds(&self) -> Vec<String> {
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for ws in &self.workspaces {
            if !ws.cwd.is_empty() && seen.insert(ws.cwd.clone()) {
                out.push(ws.cwd.clone());
            }
            for cwd in ws.bound_tab_worktrees() {
                if seen.insert(cwd.clone()) {
                    out.push(cwd);
                }
            }
        }
        out
    }

    /// The git state a tab's row should show, or `None` for an unbound tab
    /// (which has no identity of its own to report) or one whose first probe
    /// has not landed yet.
    pub(crate) fn tab_checkout_git(&self, tab: &crate::workspace::Tab) -> Option<&CheckoutGit> {
        let path = tab.worktree.as_ref()?;
        self.worktree_states.checkout(&path.to_string_lossy())
    }

    /// The checkout the active tab works in: its worktree when bound, the
    /// workspace root otherwise.
    ///
    /// The git surfaces read this rather than `ws.cwd` so they follow the tab
    /// the user is looking at.
    pub(crate) fn active_checkout(&self) -> Option<String> {
        let ws = self.active_workspace()?;
        ws.active_tab()
            .worktree
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned())
            .or_else(|| (!ws.cwd.is_empty()).then(|| ws.cwd.clone()))
    }

    /// The checkout a pane belongs to: the worktree of the tab holding it when
    /// that tab is bound, otherwise its workspace's root.
    pub(crate) fn checkout_for_pane(
        &self,
        pane: &gpui::Entity<crate::pane::Pane>,
    ) -> Option<String> {
        let ws = self
            .workspaces
            .iter()
            .find(|ws| ws.tab_for_pane(pane).is_some())?;
        ws.tab_for_pane(pane)
            .and_then(|tab| tab.worktree.as_ref())
            .map(|path| path.to_string_lossy().into_owned())
            .or_else(|| (!ws.cwd.is_empty()).then(|| ws.cwd.clone()))
    }

    /// What to call a workspace's own checkout in a branch picker.
    ///
    /// Its branch, taken from the worktree listing when it has arrived and
    /// from the workspace's own git state otherwise - the two agree, the
    /// listing is only more precise about a detached HEAD. "Project root" is
    /// the last resort, for a workspace that is not in a repository at all.
    pub(crate) fn workspace_checkout_label(&self, ws_idx: usize) -> String {
        let Some(ws) = self.workspaces.get(ws_idx) else {
            return "Project root".to_string();
        };
        let root = &ws.worktree_root;
        self.workspace_worktree_listing(ws_idx)
            .iter()
            .find(|entry| entry.path == *root)
            .map(|entry| {
                crate::workspace::worktree::checkout_label(entry.branch.as_deref(), root, root)
            })
            .filter(|label| !label.is_empty())
            .or_else(|| (!ws.git_branch.is_empty()).then(|| ws.git_branch.clone()))
            .unwrap_or_else(|| "Project root".to_string())
    }

    /// Bind `tab_idx` to `worktree`, or unbind it with `None`.
    ///
    /// The binding takes effect for panes opened *after* it: an existing pane
    /// keeps the shell it already has, because moving a live process between
    /// checkouts is not something PaneFlow can do behind the user's back. What
    /// changes immediately is the row's identity and where the next pane lands.
    pub(crate) fn set_tab_worktree(
        &mut self,
        ws_idx: usize,
        tab_idx: usize,
        worktree: Option<std::path::PathBuf>,
        cx: &mut Context<Self>,
    ) {
        let active_idx = self.active_idx;
        let Some(ws) = self.workspaces.get_mut(ws_idx) else {
            return;
        };
        let is_active_tab = ws_idx == active_idx && ws.active_tab_idx() == tab_idx;
        let ws_id = ws.id;
        let ws_cwd = ws.cwd.clone();
        let Some(tab) = ws.tab_mut(tab_idx) else {
            return;
        };
        if tab.worktree == worktree {
            return;
        }
        let tab_id = tab.id;
        let checkout = |bound: Option<&std::path::PathBuf>| {
            bound.map_or_else(
                || ws_cwd.clone(),
                |path| path.to_string_lossy().into_owned(),
            )
        };
        let from = checkout(tab.worktree.as_ref());
        let to = checkout(worktree.as_ref());
        tab.worktree = worktree.clone();
        // Probe the new checkout now rather than waiting up to 30 s for the
        // poll: a row that names a branch only after half a minute reads as
        // broken.
        if let Some(path) = worktree {
            Self::spawn_initial_git_stats(ws_id, path.to_string_lossy().into_owned(), cx);
        }
        // The git surfaces follow the tab's checkout: the diff dock this tab
        // owns moves with it, and Diff mode is rebuilt when the tab is the one
        // on screen - switching tab already does both, and binding changes
        // the same fact without a switch.
        self.retarget_diff_dock_for_tab(tab_id, &from, &to, cx);
        if is_active_tab {
            self.reconcile_diff_after_workspace_change(cx);
        }
        self.save_session(cx);
        cx.notify();
    }

    /// Refresh what the branch picker offers for a workspace's repository -
    /// its local branches, and which of them already has a worktree - off the
    /// render thread. Called when a picker opens: both are plumbing reads, but
    /// they are still subprocesses (issue #161: never on the UI thread).
    pub(crate) fn spawn_worktree_listing(&mut self, ws_idx: usize, cx: &mut Context<Self>) {
        let Some(repo_root) = self
            .workspaces
            .get(ws_idx)
            .and_then(|ws| ws.repo_root.clone())
        else {
            return;
        };
        let key = repo_root.to_string_lossy().into_owned();
        cx.spawn(
            async move |this: gpui::WeakEntity<Self>, cx: &mut gpui::AsyncApp| {
                let probe = repo_root.clone();
                let read = smol::unblock(move || {
                    let listing = crate::workspace::worktree::list_worktrees(&probe);
                    // The diff dock's reader, not a second one: one branch
                    // list for the whole app.
                    let branches = crate::app::diff_dock::list_branches(&probe.to_string_lossy());
                    (listing, branches)
                })
                .await;
                let _ = cx.update(|cx| {
                    this.update(cx, |app: &mut Self, cx: &mut Context<Self>| {
                        let mut changed = false;
                        if let Ok(entries) = read.0 {
                            changed |= app.worktree_states.set_listing(&key, entries);
                        }
                        if let Ok(branches) = read.1 {
                            changed |= app.worktree_states.set_branches(&key, branches);
                        }
                        if changed {
                            cx.notify();
                        }
                    })
                });
            },
        )
        .detach();
    }

    /// The branches offered for a workspace's tabs, as last read.
    pub(crate) fn workspace_branches(&self, ws_idx: usize) -> &[String] {
        self.workspaces
            .get(ws_idx)
            .and_then(|ws| ws.repo_root.as_ref())
            .map_or(&[], |root| {
                self.worktree_states.branches(&root.to_string_lossy())
            })
    }

    /// Point a tab at a branch, making its worktree if the branch has none.
    ///
    /// This is the whole point of the picker: the user picks a branch, and
    /// whether that branch already has a directory is git's problem, not
    /// theirs. Selecting the branch the repository itself is on unbinds the
    /// tab instead of duplicating that checkout - git would refuse the second
    /// worktree anyway.
    ///
    /// The work runs off the render thread (a checkout can take seconds on a
    /// large repository) and re-resolves the tab by id when it lands, because
    /// indices do not survive an await. A checkout made here is deliberately
    /// not a `ManagedWorktree`: see `prepare_branch_checkout`.
    pub(crate) fn bind_tab_to_branch(
        &mut self,
        ws_idx: usize,
        tab_idx: usize,
        branch: String,
        cx: &mut Context<Self>,
    ) {
        let Some(ws) = self.workspaces.get(ws_idx) else {
            return;
        };
        let Some(repo_root) = ws.repo_root.clone() else {
            return;
        };
        let Some(tab_id) = ws.tabs().get(tab_idx).map(|tab| tab.id) else {
            return;
        };
        let ws_id = ws.id;
        // A branch the listing already places needs no subprocess: bind now,
        // so the common case stays a single frame. The listing is as old as
        // the picker, so the path is checked the way every other binding is
        // (`bind_tab_to_checkout`): a checkout removed since the listing was
        // read falls through to git, which re-lists and re-creates it.
        let placed = self
            .workspace_worktree_listing(ws_idx)
            .iter()
            .find(|entry| entry.branch.as_deref() == Some(branch.as_str()))
            .map(|entry| entry.path.clone());
        match placed {
            Some(path) if path == repo_root => {
                self.set_tab_worktree(ws_idx, tab_idx, None, cx);
                return;
            }
            Some(path) if path.is_dir() => {
                self.bind_tab_to_checkout(ws_idx, tab_idx, path, cx);
                return;
            }
            _ => {}
        }
        // One checkout at a time: a second click while git works would race
        // the first for the same directory.
        if let Some(pending) = self.branch_checkout_pending.clone() {
            self.show_toast(format!("Still checking out {pending}"), cx);
            return;
        }

        self.branch_checkout_pending = Some(branch.clone());
        cx.notify();
        cx.spawn(
            async move |this: gpui::WeakEntity<Self>, cx: &mut gpui::AsyncApp| {
                let probe = repo_root.clone();
                let name = branch.clone();
                let prepared = smol::unblock(move || {
                    crate::workspace::worktree::prepare_branch_checkout(&probe, &name)
                })
                .await;
                let _ = cx.update(|cx| {
                    this.update(cx, |app: &mut Self, cx: &mut Context<Self>| {
                        // Cleared first, on every outcome: a refusal below
                        // must not leave the picker reading "Checking out"
                        // for the rest of the session.
                        app.branch_checkout_pending = None;
                        match prepared {
                            Ok(path) => {
                                let Some((ws_idx, tab_idx)) = app.tab_position(ws_id, tab_id)
                                else {
                                    cx.notify();
                                    return;
                                };
                                if path == repo_root {
                                    app.set_tab_worktree(ws_idx, tab_idx, None, cx);
                                } else {
                                    // The same refusals as the fast path: git
                                    // resolved the branch to a checkout, but
                                    // whether this tab may stand on it is
                                    // PaneFlow's question, and the answer can
                                    // have changed while git worked.
                                    app.bind_tab_to_checkout(ws_idx, tab_idx, path, cx);
                                }
                                app.spawn_worktree_listing(ws_idx, cx);
                            }
                            Err(message) => app.show_toast(message, cx),
                        }
                        cx.notify();
                    })
                });
            },
        )
        .detach();
    }

    /// Locate a tab by the ids that survive an await, unlike its indices.
    pub(crate) fn tab_position(&self, ws_id: u64, tab_id: u64) -> Option<(usize, usize)> {
        let ws_idx = self.workspaces.iter().position(|ws| ws.id == ws_id)?;
        let tab_idx = self.workspaces[ws_idx]
            .tabs()
            .iter()
            .position(|tab| tab.id == tab_id)?;
        Some((ws_idx, tab_idx))
    }

    /// The worktrees of a workspace's repository: what `git worktree list`
    /// last reported for it.
    pub(crate) fn workspace_worktree_listing(&self, ws_idx: usize) -> &[WorktreeEntry] {
        self.workspaces
            .get(ws_idx)
            .and_then(|ws| ws.repo_root.as_ref())
            .map_or(&[], |root| {
                self.worktree_states.listing(&root.to_string_lossy())
            })
    }

    /// Forget the state of every checkout no longer open.
    pub(crate) fn prune_worktree_states(&mut self) {
        let live: std::collections::HashSet<String> = self
            .git_probe_cwds()
            .into_iter()
            .chain(
                self.workspaces
                    .iter()
                    .filter_map(|ws| ws.worktree_root.to_str().map(str::to_string)),
            )
            .chain(
                self.workspaces
                    .iter()
                    .filter_map(|ws| ws.repo_root.as_ref())
                    .filter_map(|root| root.to_str().map(str::to_string)),
            )
            .collect();
        self.worktree_states.retain_live(&live);
    }
}

#[cfg(test)]
mod tests {
    use super::{CheckoutGit, WorktreeStates, binding_refusal};
    use crate::workspace::GitDiffStats;
    use std::path::{Path, PathBuf};

    #[test]
    fn a_probe_taken_elsewhere_is_not_filed_under_the_checkout_key() {
        // Issue #347 review, finding 12: `handle_cwd_change`'s fall-through
        // applied a probe taken at the pane's new cwd under the workspace-root
        // key, so a pane that walked into another repository wrote that
        // repository's branch where a tab bound to the root would read it.
        let mut states = WorktreeStates::default();
        assert!(
            !states.set_checkout_probed_at("/w/a", "/elsewhere/b", state("other", 7)),
            "a foreign probe must neither store nor repaint"
        );
        assert!(states.checkout("/w/a").is_none());
        assert!(
            !states.set_checkout_probed_at("/w/a", "/w/a/src", state("main", 1)),
            "a subdirectory is not the checkout key either"
        );
        assert!(states.set_checkout_probed_at("/w/a", "/w/a", state("main", 1)));
        assert_eq!(
            states.checkout("/w/a").map(|s| s.branch.as_str()),
            Some("main")
        );
        assert!(
            !states.set_checkout_probed_at("/w/a", "/elsewhere/b", state("other", 7)),
            "a later foreign probe must not overwrite a real one"
        );
        assert_eq!(
            states.checkout("/w/a").map(|s| s.branch.as_str()),
            Some("main")
        );
    }

    #[test]
    fn a_tab_cannot_bind_to_a_checkout_another_workspace_owns_or_is_retiring() {
        // Issue #347 review, finding 3: the picker bound to any listing entry
        // holding the branch, including a checkout `workspace.up` created for
        // another workspace - which is removed when that workspace closes.
        let feat = PathBuf::from("/repo.worktrees/feat-x");
        let owned = vec![(0usize, feat.clone())];
        let none: Vec<PathBuf> = Vec::new();
        assert_eq!(
            binding_refusal(&feat, 1, &owned, &none, &none),
            Some("That worktree belongs to another workspace")
        );
        assert_eq!(
            binding_refusal(&feat.join("src"), 1, &owned, &none, &none),
            Some("That worktree belongs to another workspace"),
            "a path under an owned checkout is owned with it"
        );
        assert_eq!(
            binding_refusal(&feat, 0, &owned, &none, &none),
            None,
            "a workspace may bind its own managed checkout - that is what `up` does"
        );
        assert_eq!(
            binding_refusal(Path::new("/repo.worktrees/feat-y"), 1, &owned, &none, &none),
            None,
            "a sibling checkout nobody owns is free"
        );
        assert_eq!(
            binding_refusal(&feat, 0, &[], std::slice::from_ref(&feat), &none),
            Some("That worktree belongs to a closed workspace"),
            "an undo record still owns its checkouts"
        );
        assert_eq!(
            binding_refusal(&feat, 0, &owned, &none, std::slice::from_ref(&feat)),
            Some("That worktree is being retired"),
            "retirement outranks ownership: the directory is going now"
        );
    }

    #[test]
    fn every_picked_checkout_passes_through_the_binding_gate() {
        // Findings 3 and 4 at the source level: the fast path and the async
        // landing of `bind_tab_to_branch` bind through `bind_tab_to_checkout`
        // (refusals + "still a directory"), never through the raw setter,
        // and a stale listing entry whose directory is gone is not bound.
        let src = include_str!("tab_worktree.rs");
        let bind = crate::source_probe::source_slice(
            src,
            "pub(crate) fn bind_tab_to_branch(",
            "/// Locate a tab by the ids that survive an await",
        );
        assert!(
            bind.contains("Some(path) if path.is_dir() => {"),
            "the fast path must check the listing's path still exists: {bind}"
        );
        assert_eq!(
            bind.matches("bind_tab_to_checkout(ws_idx, tab_idx, path, cx)")
                .count(),
            2,
            "both the fast path and the landing must bind through the gate: {bind}"
        );
        assert!(
            !bind.contains("set_tab_worktree(ws_idx, tab_idx, Some("),
            "no bind path may skip the gate: {bind}"
        );
        let landing = crate::source_probe::source_slice(
            bind,
            "this.update(cx, |app: &mut Self, cx: &mut Context<Self>| {",
            "match prepared {",
        );
        assert!(
            landing.contains("app.branch_checkout_pending = None;"),
            "the pending slot must clear before any outcome is judged: {landing}"
        );

        let gate = crate::source_probe::source_slice(
            src,
            "pub(crate) fn bind_tab_to_checkout(",
            "/// Every checkout worth probing",
        );
        let refusal_at = gate
            .find("self.tab_binding_refusal(&path, ws_idx)")
            .expect("the gate consults the refusal rules");
        let exists_at = gate
            .find("crate::workspace::existing_worktree_dir(Some(path))")
            .expect("the gate checks the directory still exists");
        let set_at = gate
            .find("self.set_tab_worktree(ws_idx, tab_idx, Some(path), cx)")
            .expect("the gate is what binds");
        assert!(refusal_at < exists_at && exists_at < set_at, "{gate}");
        assert_eq!(
            gate.matches("self.show_toast(").count(),
            2,
            "each refusal reaches the user as a toast: {gate}"
        );
    }

    #[test]
    fn binding_the_active_tab_retargets_the_git_surfaces() {
        // Issue #347 review, finding 11: the bind only saved and repainted,
        // so Diff mode and an open dock kept showing the checkout the tab
        // had just left.
        let src = include_str!("tab_worktree.rs");
        let set = crate::source_probe::source_slice(
            src,
            "pub(crate) fn set_tab_worktree(",
            "/// Refresh what the branch picker offers",
        );
        assert!(
            set.contains("self.retarget_diff_dock_for_tab(tab_id, &from, &to, cx);"),
            "the tab's dock must follow its checkout: {set}"
        );
        let active_at = set
            .find("if is_active_tab {")
            .expect("the rebuild is gated on the tab being the visible one");
        assert!(
            set[active_at..].contains("self.reconcile_diff_after_workspace_change(cx);"),
            "Diff mode must rebuild when the visible tab rebinds: {set}"
        );
    }

    fn state(branch: &str, insertions: usize) -> CheckoutGit {
        CheckoutGit {
            branch: branch.to_string(),
            is_repo: true,
            stats: GitDiffStats {
                files_changed: 1,
                insertions,
                deletions: 0,
            },
        }
    }

    #[test]
    fn a_repeated_probe_reports_no_change() {
        let mut states = WorktreeStates::default();
        assert!(states.set_checkout("/w/a", state("main", 3)));
        assert!(
            !states.set_checkout("/w/a", state("main", 3)),
            "an identical probe must not ask the rail to repaint"
        );
        assert!(states.set_checkout("/w/a", state("main", 4)));
        assert!(states.set_checkout("/w/a", state("feat/x", 4)));
    }

    #[test]
    fn checkouts_are_independent_and_prunable() {
        let mut states = WorktreeStates::default();
        states.set_checkout("/w/a", state("main", 1));
        states.set_checkout("/w/b", state("feat/x", 9));
        assert_eq!(
            states.checkout("/w/b").map(|s| s.branch.as_str()),
            Some("feat/x")
        );
        assert!(
            states.checkout("/w/missing").is_none(),
            "an unprobed checkout reports nothing rather than a stale neighbor"
        );

        let live = std::collections::HashSet::from(["/w/a".to_string()]);
        states.retain_live(&live);
        assert!(states.checkout("/w/a").is_some());
        assert!(
            states.checkout("/w/b").is_none(),
            "closing a tab must not leave its worktree state alive for the session"
        );
    }

    #[test]
    fn listings_and_branches_report_change_only_on_a_real_delta() {
        let mut states = WorktreeStates::default();
        assert!(states.set_branches("/r", vec!["main".into(), "feat/x".into()]));
        assert!(!states.set_branches("/r", vec!["main".into(), "feat/x".into()]));
        assert_eq!(states.branches("/r"), ["main", "feat/x"]);
        assert!(states.branches("/other").is_empty());
        assert!(states.set_listing("/r", vec![]));
        assert!(!states.set_listing("/r", vec![]));
        let live = std::collections::HashSet::new();
        states.retain_live(&live);
        assert!(states.branches("/r").is_empty());
        assert!(states.listing("/r").is_empty());
    }
}
