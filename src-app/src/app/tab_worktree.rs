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
                                // A checkout made behind a tab is invisible
                                // to the Worktree-scope cache key (issue
                                // #348): force the miss so the new lane
                                // appears without a scope toggle.
                                app.invalidate_worktree_diff_cache(&repo_root, cx);
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

    /// Remove the checkout a tab is bound to, unbinding every tab that works
    /// in it (issue #348).
    ///
    /// The counterpart of [`Self::bind_tab_to_branch`], and the reason
    /// `<repo>.worktrees/` no longer grows for the life of the project: a
    /// checkout the picker created is deliberately NOT a
    /// [`crate::workspace::worktree::ManagedWorktree`]
    /// ([`crate::workspace::worktree::prepare_branch_checkout`]), so nothing
    /// else ever tears it down.
    ///
    /// It keeps the invariants workspace teardown holds for orchestration's
    /// own worktrees, for the same reasons: the BRANCH IS NEVER DELETED, a
    /// checkout holding uncommitted work is never removed, and a directory
    /// PaneFlow did not create belongs to somebody else. Unlike teardown,
    /// this is a gesture the user made, so a refusal is a toast rather than
    /// a log line nobody reads. The rules are [`removal_refusal`]; the git
    /// work is [`remove_checkout`], through `smol::unblock` (four
    /// subprocesses, one of them deleting a tree), and the workspace is
    /// re-resolved by id afterwards, because indices do not survive an await.
    pub(crate) fn remove_tab_worktree(
        &mut self,
        ws_idx: usize,
        tab_idx: usize,
        cx: &mut Context<Self>,
    ) {
        let Some(ws) = self.workspaces.get(ws_idx) else {
            return;
        };
        let Some(repo_root) = ws.repo_root.clone() else {
            return;
        };
        let Some(path) = ws.tabs().get(tab_idx).and_then(|tab| tab.worktree.clone()) else {
            return;
        };
        // Snapshotted at the click for the off-thread checks, then taken
        // again on the main thread right before the delete (below): a
        // workspace opened at the checkout while the git probes ran has no
        // terminal yet for the cwd scan to find, so only a re-read of the
        // live workspace list can refuse it.
        let open_roots = self.open_workspace_roots();
        let reserved = self.teardown_owned_worktrees();
        // The same live-process gate managed teardown applies: a shell or
        // agent still working in the checkout (a tab restored from a session
        // spawned its panes there) must not have its cwd deleted from under it.
        let protected = self.live_terminal_session_ids(cx);
        cx.spawn(
            async move |this: gpui::WeakEntity<Self>, cx: &mut gpui::AsyncApp| {
                let (probe_root, probe_path) = (repo_root.clone(), path.clone());
                let checked = smol::unblock(move || {
                    check_checkout_removable(
                        &probe_root,
                        &probe_path,
                        &open_roots,
                        &reserved,
                        &protected,
                    )
                })
                .await;
                // Re-validate against the workspaces open *now*, on the main
                // thread, and only then delete. The remaining window is the
                // one managed teardown has too: git itself still refuses a
                // checkout that turned dirty in between.
                let revalidated = cx.update(|cx| {
                    this.update(cx, |app: &mut Self, cx: &mut Context<Self>| {
                        let refusal = checked.err().or_else(|| {
                            open_or_reserved_refusal(
                                &path,
                                &app.open_workspace_roots(),
                                &app.teardown_owned_worktrees(),
                            )
                        });
                        match refusal {
                            Some(message) => {
                                app.show_toast(message, cx);
                                cx.notify();
                                false
                            }
                            None => true,
                        }
                    })
                });
                if !matches!(revalidated, Ok(true)) {
                    return;
                }
                let (remove_root, remove_path) = (repo_root.clone(), path.clone());
                let removed =
                    smol::unblock(move || remove_validated_checkout(&remove_root, &remove_path))
                        .await;
                let _ = cx.update(|cx| {
                    this.update(cx, |app: &mut Self, cx: &mut Context<Self>| {
                        match removed {
                            Ok(()) => app.forget_removed_worktree(&repo_root, &path, cx),
                            Err(message) => app.show_toast(message, cx),
                        }
                        cx.notify();
                    })
                });
            },
        )
        .detach();
    }

    /// The checkout every open workspace stands in, for the open-workspace
    /// refusal (issue #348).
    fn open_workspace_roots(&self) -> Vec<std::path::PathBuf> {
        self.workspaces
            .iter()
            .map(|ws| ws.worktree_root.clone())
            .collect()
    }

    /// Every checkout the teardown path owns and removes on its own schedule:
    /// each workspace's managed worktrees, the ones an undo record still
    /// holds, and the pending teardown journal (issue #348). The tab menu's
    /// removal must not take one of these from under it.
    fn teardown_owned_worktrees(&self) -> Vec<std::path::PathBuf> {
        self.workspaces
            .iter()
            .flat_map(|ws| ws.managed_worktrees.iter())
            .chain(self.closed_items.iter().flat_map(|record| {
                let held: &[crate::workspace::worktree::ManagedWorktree] = match record {
                    crate::ClosedRecord::Workspace(ws) => &ws.managed_worktrees,
                    crate::ClosedRecord::Pane(_) | crate::ClosedRecord::Tab(_) => &[],
                };
                held.iter()
            }))
            .chain(self.pending_worktree_teardowns.iter())
            .map(|worktree| worktree.path.clone())
            .collect()
    }

    /// Drop every trace of a checkout that is gone: unbind the tabs that
    /// worked in it, forget its cached git state, refresh what the picker
    /// offers, and make the Worktree-scope diff recount its columns.
    ///
    /// Every workspace is walked, not only the one whose menu was clicked: a
    /// picker checkout is marker-less, so two workspaces on the same
    /// repository may both have tabs bound to it, and the one that clicked
    /// may have closed during the await. Tabs are collected by index first:
    /// [`Self::set_tab_worktree`] takes `&mut self`, and it is the one place
    /// a binding is allowed to change, so the unbind goes through it rather
    /// than writing the field here.
    fn forget_removed_worktree(
        &mut self,
        repo_root: &std::path::Path,
        path: &std::path::Path,
        cx: &mut Context<Self>,
    ) {
        let orphaned: Vec<(usize, usize)> = self
            .workspaces
            .iter()
            .enumerate()
            .flat_map(|(ws_idx, ws)| {
                ws.tabs()
                    .iter()
                    .enumerate()
                    .filter(|(_, tab)| tab.worktree.as_deref() == Some(path))
                    .map(move |(tab_idx, _)| (ws_idx, tab_idx))
            })
            .collect();
        for (ws_idx, tab_idx) in orphaned {
            self.set_tab_worktree(ws_idx, tab_idx, None, cx);
        }
        self.prune_worktree_states();
        let on_repo: Vec<usize> = self
            .workspaces
            .iter()
            .enumerate()
            .filter(|(_, ws)| ws.repo_root.as_deref() == Some(repo_root))
            .map(|(ws_idx, _)| ws_idx)
            .collect();
        for ws_idx in on_repo {
            self.spawn_worktree_listing(ws_idx, cx);
        }
        self.invalidate_worktree_diff_cache(repo_root, cx);
    }
}

/// Why a tab's checkout may not be removed right now, as the toast to show,
/// or `None` when it may (issue #348). Pure, so the rules are testable
/// without a repository; [`remove_checkout`] applies them off the render
/// thread, with the cleanliness check that needs git.
///
/// Ownership is the deterministic-path test
/// [`crate::workspace::worktree::is_paneflow_worktree_dir`]: the picker
/// writes no owner marker (`prepare_branch_checkout`, issue #347), so a
/// checkout is ours exactly when it sits where PaneFlow would have put its
/// branch. A detached checkout has no branch to test and is never what the
/// picker made, and the repository root never passes. `open_roots` are the
/// open workspaces' checkouts: one standing in or under the path would be
/// left in a directory that no longer exists, so that is a refusal, not a
/// warning. `reserved` are the paths the teardown path owns (every
/// workspace's managed worktrees, the ones held by undo records, and the
/// pending teardown journal), matched both ways like the binding gate, since
/// it removes them on its own schedule and must not lose one from under it.
fn removal_refusal(
    repo_root: &std::path::Path,
    path: &std::path::Path,
    branch: Option<&str>,
    open_roots: &[std::path::PathBuf],
    reserved: &[std::path::PathBuf],
) -> Option<String> {
    if let Some(reason) = open_or_reserved_refusal(path, open_roots, reserved) {
        return Some(reason);
    }
    let ours = branch.is_some_and(|branch| {
        crate::workspace::worktree::is_paneflow_worktree_dir(repo_root, branch, path)
    });
    if !ours {
        return Some(format!(
            "{} was not created by PaneFlow - remove it with git worktree remove",
            path.display()
        ));
    }
    None
}

/// The blocking half of [`PaneFlowApp::remove_tab_worktree`]: refuse what is
/// open, reserved, not ours, or not clean, then remove the directory and drop
/// the administrative entry that named it.
///
/// The branch is read from a fresh `git worktree list` rather than the
/// picker's cached listing, so the ownership test runs against what git holds
/// now; a path git no longer lists is refused too, there being nothing to
/// remove. Cleanliness is [`worktree::is_clean_for_removal`]: tracked
/// modifications and untracked files refuse, ignored files (the `.env*`
/// copies the picker makes, build output) do not, the same gate
/// `git worktree remove` applies. A live process whose cwd is inside the
/// checkout refuses too ([`worktree::worktree_has_live_process_cwd`], the
/// managed-teardown gate, with the open terminals' sessions protected): a
/// shell or agent must not have its directory deleted from under it. Both
/// report an error rather than "clean" when they cannot prove it, and that
/// error propagates: never delete what cannot be read. The BRANCH IS NEVER
/// DELETED.
#[cfg(test)]
fn remove_checkout(
    repo_root: &std::path::Path,
    path: &std::path::Path,
    open_roots: &[std::path::PathBuf],
    reserved: &[std::path::PathBuf],
    protected_session_ids: &[u32],
) -> Result<(), String> {
    check_checkout_removable(repo_root, path, open_roots, reserved, protected_session_ids)?;
    remove_validated_checkout(repo_root, path)
}

/// The two workspace-state refusals of [`removal_refusal`], on their own so
/// [`PaneFlowApp::remove_tab_worktree`] can apply them a second time on the
/// main thread, against the workspaces open at that moment, after the git
/// probes and before the delete (issue #348). `open_roots` are matched at
/// or under the path; `reserved` paths both ways, like the binding gate.
fn open_or_reserved_refusal(
    path: &std::path::Path,
    open_roots: &[std::path::PathBuf],
    reserved: &[std::path::PathBuf],
) -> Option<String> {
    if open_roots.iter().any(|root| root.starts_with(path)) {
        return Some(format!(
            "{} is open as a workspace - close it first",
            path.display()
        ));
    }
    let overlaps = |owned: &std::path::Path| owned.starts_with(path) || path.starts_with(owned);
    if reserved.iter().any(|owned| overlaps(owned)) {
        return Some(format!(
            "{} is managed by workspace teardown - it is removed when its workspace closes",
            path.display()
        ));
    }
    None
}

/// The read-only half of [`remove_checkout`]: every refusal, nothing
/// deleted. Runs off the render thread; a passing result is re-validated on
/// the main thread before [`remove_validated_checkout`] runs.
fn check_checkout_removable(
    repo_root: &std::path::Path,
    path: &std::path::Path,
    open_roots: &[std::path::PathBuf],
    reserved: &[std::path::PathBuf],
    protected_session_ids: &[u32],
) -> Result<(), String> {
    use crate::workspace::worktree;
    let entries = worktree::list_worktrees(repo_root)?;
    let Some(entry) = entries.iter().find(|entry| entry.path == path) else {
        return Err(format!(
            "{} is not a worktree of this repository",
            path.display()
        ));
    };
    if let Some(reason) = removal_refusal(
        repo_root,
        path,
        entry.branch.as_deref(),
        open_roots,
        reserved,
    ) {
        return Err(reason);
    }
    if !worktree::is_clean_for_removal(path)? {
        return Err(format!(
            "{} has uncommitted changes - commit or discard them first",
            path.display()
        ));
    }
    if worktree::worktree_has_live_process_cwd(path, protected_session_ids)? {
        return Err(format!(
            "{} is in use by a running process - close the shell or agent working there first",
            path.display()
        ));
    }
    Ok(())
}

/// The deleting half of [`remove_checkout`]: `git worktree remove` (which
/// refuses by itself a checkout that turned dirty since the check) and the
/// prune that drops the administrative entry. The BRANCH IS NEVER DELETED.
fn remove_validated_checkout(
    repo_root: &std::path::Path,
    path: &std::path::Path,
) -> Result<(), String> {
    use crate::workspace::worktree;
    worktree::remove_worktree(repo_root, path)?;
    // The directory is gone; drop the administrative entry with it, so a
    // later `worktree add` for the same branch is not refused by a stale
    // record.
    let _ = worktree::prune(repo_root);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{CheckoutGit, WorktreeStates, binding_refusal, removal_refusal, remove_checkout};
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
    fn removal_is_refused_for_what_is_not_ours_open_or_reserved() {
        // Issue #348: the tab menu's "Remove worktree" row takes a
        // picker-created checkout back down, and every refusal is a toast.
        use crate::workspace::worktree::{worktree_dir, worktree_dir_hashed};
        let repo = PathBuf::from("/repo");
        let ours = worktree_dir(&repo, "feat/x");
        let none: Vec<PathBuf> = Vec::new();
        assert_eq!(
            removal_refusal(&repo, &ours, Some("feat/x"), &none, &none),
            None,
            "a clean, owned, not-open checkout may go"
        );
        assert_eq!(
            removal_refusal(
                &repo,
                &worktree_dir_hashed(&repo, "feat/x"),
                Some("feat/x"),
                &none,
                &none
            ),
            None,
            "the collision-resistant directory is ours too"
        );
        let not_ours = |path: &Path, branch: Option<&str>| {
            removal_refusal(&repo, path, branch, &none, &none)
                .unwrap_or_default()
                .contains("not created by PaneFlow")
        };
        assert!(
            not_ours(Path::new("/elsewhere/feat-x"), Some("feat/x")),
            "a checkout somewhere else belongs to somebody else"
        );
        assert!(
            not_ours(&ours, Some("feat/y")),
            "our directory holding another branch is not the one we made"
        );
        assert!(
            not_ours(&ours, None),
            "a detached checkout is never what the picker made"
        );
        assert!(
            not_ours(&repo, Some("main")),
            "the repository root is never a worktree to remove"
        );
        assert!(
            removal_refusal(
                &repo,
                &ours,
                Some("feat/x"),
                std::slice::from_ref(&ours),
                &none
            )
            .unwrap_or_default()
            .contains("open as a workspace"),
            "a checkout that is itself an open workspace is somebody's cwd"
        );
        assert!(
            removal_refusal(&repo, &ours, Some("feat/x"), &[ours.join("src")], &none)
                .unwrap_or_default()
                .contains("open as a workspace"),
            "a workspace standing under the checkout is open in it"
        );
        assert!(
            removal_refusal(
                &repo,
                &ours,
                Some("feat/x"),
                &none,
                std::slice::from_ref(&ours)
            )
            .unwrap_or_default()
            .contains("teardown"),
            "a managed or retiring checkout is the teardown path's to remove"
        );
        assert!(
            removal_refusal(&repo, &ours, Some("feat/x"), &none, &[ours.join("nested")])
                .unwrap_or_default()
                .contains("teardown"),
            "a managed checkout under the path is reserved with it"
        );
    }

    fn git(cwd: &Path, args: &[&str]) -> String {
        let mut cmd = crate::workspace::worktree::git_command();
        cmd.arg("-C").arg(cwd).args(args);
        let out = cmd.output().expect("git runs");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    #[test]
    fn a_clean_owned_checkout_is_removed_keeping_its_branch_and_a_dirty_one_is_refused() {
        // Issue #348, against a real repository: the removal deletes the
        // checkout and only the checkout, and a refusal changes nothing.
        use crate::workspace::worktree::{list_worktrees, prepare_branch_checkout};
        let tmp = tempfile::tempdir().expect("tempdir");
        // Git records worktree paths resolved, and a macOS tempdir is a
        // symlink: compare like with like.
        let sandbox = std::fs::canonicalize(tmp.path()).expect("canonical tempdir");
        let repo_root = sandbox.join("repo");
        std::fs::create_dir_all(&repo_root).expect("repo root");
        git(&repo_root, &["init", "-q"]);
        git(
            &repo_root,
            &["config", "user.email", "paneflow-tests@example.invalid"],
        );
        git(&repo_root, &["config", "user.name", "PaneFlow Tests"]);
        std::fs::write(repo_root.join("README.md"), "test\n").expect("tracked file");
        std::fs::write(repo_root.join(".gitignore"), ".env\n").expect("ignore rule");
        git(&repo_root, &["add", "."]);
        git(&repo_root, &["commit", "-q", "-m", "fixture"]);
        git(&repo_root, &["branch", "feat/clean"]);
        git(&repo_root, &["branch", "feat/dirty"]);
        // Through the same door as the picker, so what is removed is exactly
        // what the picker makes: `prepare_branch_checkout` copies the
        // project's `.env*` into the new checkout, and that copy is ignored.
        std::fs::write(repo_root.join(".env"), "SECRET=1\n").expect("ignored env file");
        let clean = prepare_branch_checkout(&repo_root, "feat/clean").expect("clean checkout");
        let dirty = prepare_branch_checkout(&repo_root, "feat/dirty").expect("dirty checkout");
        assert!(
            clean.join(".env").is_file(),
            "the picker copies .env into its checkout; the test must exercise that"
        );
        std::fs::write(dirty.join("scratch.txt"), "wip\n").expect("dirty file");
        let foreign = sandbox.join("foreign");
        let foreign_s = foreign.to_string_lossy().into_owned();
        git(
            &repo_root,
            &["worktree", "add", "-q", &foreign_s, "-b", "feat/foreign"],
        );
        let before = list_worktrees(&repo_root).expect("listing");
        assert_eq!(before.len(), 4, "root, two picker checkouts, one foreign");

        let refused =
            remove_checkout(&repo_root, &dirty, &[], &[], &[]).expect_err("dirty is refused");
        assert!(refused.contains("uncommitted"), "{refused}");
        assert!(dirty.is_dir());
        let refused =
            remove_checkout(&repo_root, &foreign, &[], &[], &[]).expect_err("foreign is refused");
        assert!(refused.contains("not created by PaneFlow"), "{refused}");
        assert!(foreign.is_dir());
        let refused = remove_checkout(&repo_root, &clean, std::slice::from_ref(&clean), &[], &[])
            .expect_err("an open workspace is refused");
        assert!(refused.contains("open as a workspace"), "{refused}");
        let refused = remove_checkout(&repo_root, &clean, &[], std::slice::from_ref(&clean), &[])
            .expect_err("a managed checkout is refused");
        assert!(refused.contains("teardown"), "{refused}");

        // A shell still working in the checkout keeps it: the same gate
        // managed teardown applies, so its cwd is never deleted from under it.
        struct ChildCleanup(std::process::Child);
        impl Drop for ChildCleanup {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let child = std::process::Command::new("/bin/sh")
            .args(["-c", "echo ready; exec sleep 30"])
            .current_dir(&clean)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn a process in the checkout");
        let mut child = ChildCleanup(child);
        let mut ready = String::new();
        std::io::BufRead::read_line(
            &mut std::io::BufReader::new(child.0.stdout.take().expect("child stdout")),
            &mut ready,
        )
        .expect("read child readiness");
        assert_eq!(ready.trim_end(), "ready");
        let refused = remove_checkout(&repo_root, &clean, &[], &[], &[])
            .expect_err("a checkout with a live process inside is refused");
        assert!(refused.contains("in use by a running process"), "{refused}");
        assert!(clean.is_dir());
        child.0.kill().expect("stop the live-cwd fixture");
        child.0.wait().expect("reap the live-cwd fixture");
        drop(child);
        assert_eq!(
            list_worktrees(&repo_root).expect("listing"),
            before,
            "a refusal leaves `git worktree list` exactly as it was"
        );

        // The ignored `.env` copy is still there, and it is not "uncommitted
        // work": the checkout the picker made is removable as it stands.
        assert!(clean.join(".env").is_file());
        remove_checkout(&repo_root, &clean, &[], &[], &[])
            .expect("a clean owned checkout is removed");
        assert!(!clean.exists(), "the directory is gone");
        let after = list_worktrees(&repo_root).expect("listing");
        assert_eq!(after.len(), 3);
        assert!(
            after.iter().all(|entry| entry.path != clean),
            "the administrative entry went with the directory: {after:?}"
        );
        let branches = git(&repo_root, &["branch", "--format=%(refname:short)"]);
        assert!(
            branches.lines().any(|line| line == "feat/clean"),
            "the branch is never deleted: {branches}"
        );
        assert!(git(&repo_root, &["status", "--porcelain"]).is_empty());
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
