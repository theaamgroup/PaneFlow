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
    /// Times each tab's checkout binding has changed, keyed by tab id
    /// (issue #937). Not dropped by [`Self::retain_live`]: tab ids are never
    /// reused, and each entry is one integer.
    binding_generations: HashMap<u64, u64>,
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

    /// Times `tab_id`'s checkout binding has changed. Zero until the first
    /// change. A slow checkout snapshots this before awaiting git (issue #937).
    pub(crate) fn binding_generation(&self, tab_id: u64) -> u64 {
        self.binding_generations.get(&tab_id).copied().unwrap_or(0)
    }

    /// Record that `tab_id`'s binding changed. `bind_tab_to_checkout` and
    /// `set_tab_worktree` are the callers; a landing that snapshotted the
    /// previous value then leaves the tab alone.
    pub(crate) fn bump_binding_generation(&mut self, tab_id: u64) {
        let next = self.binding_generation(tab_id).wrapping_add(1);
        self.binding_generations.insert(tab_id, next);
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

impl PaneFlowApp {
    /// Bind a tab to a checkout the user picked, or say why not.
    ///
    /// The one door for a path chosen from a list: the picker's rows come
    /// from a listing read when it opened, and between then and the click the
    /// directory can have been removed, or the path is not valid UTF-8. A
    /// refusal reaches the user as a toast and leaves the tab as it was.
    /// Returns whether it bound.
    pub(crate) fn bind_tab_to_checkout(
        &mut self,
        ws_idx: usize,
        tab_idx: usize,
        path: std::path::PathBuf,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(path) = crate::workspace::existing_worktree_dir(Some(path)) else {
            self.show_toast(
                "That checkout no longer exists; run `git worktree prune`",
                cx,
            );
            return false;
        };
        // A same-path rebind returns inside `set_tab_worktree` without
        // writing. It still has to retire a checkout that is in flight:
        // the user just chose this directory again (issue #937). A path
        // that is not valid UTF-8 is not a rebind: `set_tab_worktree`
        // refuses it, and bumping here would retire the in-flight checkout
        // the user did not replace (issue #1025).
        if path.to_str().is_some() {
            self.bump_tab_binding(ws_idx, tab_idx);
        }
        match self.set_tab_worktree(ws_idx, tab_idx, Some(path), cx) {
            Ok(()) => true,
            Err(message) => {
                self.show_toast(message, cx);
                false
            }
        }
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

    fn bump_tab_binding(&mut self, ws_idx: usize, tab_idx: usize) {
        let Some(tab_id) = self
            .workspaces
            .get(ws_idx)
            .and_then(|ws| ws.tabs().get(tab_idx))
            .map(|tab| tab.id)
        else {
            return;
        };
        self.worktree_states.bump_binding_generation(tab_id);
    }

    /// Bind `tab_idx` to `worktree`, or unbind it with `None`.
    ///
    /// The binding takes effect for panes opened *after* it: an existing pane
    /// keeps the shell it already has, because moving a live process between
    /// checkouts is not something PaneFlow can do behind the user's back. What
    /// changes immediately is the row's identity and where the next pane lands.
    ///
    /// A path that is not valid UTF-8 is refused before the tab, the binding
    /// generation, the session, or a git probe changes. The session field is a
    /// `String`, so a lossy path would persist U+FFFD and could not be
    /// restored (issue #1025). Callers show the error with the worktree toast.
    pub(crate) fn set_tab_worktree(
        &mut self,
        ws_idx: usize,
        tab_idx: usize,
        worktree: Option<std::path::PathBuf>,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        if worktree
            .as_ref()
            .is_some_and(|path| path.to_str().is_none())
        {
            return Err("That worktree path is not valid UTF-8".to_string());
        }
        let active_idx = self.active_idx;
        let (is_active_tab, ws_id, tab_id) = {
            let Some(ws) = self.workspaces.get_mut(ws_idx) else {
                return Ok(());
            };
            let is_active_tab = ws_idx == active_idx && ws.active_tab_idx() == tab_idx;
            let ws_id = ws.id;
            let Some(tab) = ws.tab_mut(tab_idx) else {
                return Ok(());
            };
            if tab.worktree == worktree {
                return Ok(());
            }
            let tab_id = tab.id;
            tab.worktree = worktree.clone();
            (is_active_tab, ws_id, tab_id)
        };
        // Issue #937: a slow checkout snapshotted the previous generation and
        // must not replace this choice when git returns.
        self.worktree_states.bump_binding_generation(tab_id);
        // Probe the new checkout now rather than waiting up to 30 s for the
        // poll: a row that names a branch only after half a minute reads as
        // broken. `to_str` is `Some`: a non-UTF-8 path already returned.
        if let Some(path) = worktree.as_ref()
            && let Some(cwd) = path.to_str()
            && !checkout_probes_suppressed()
        {
            Self::spawn_initial_git_stats(ws_id, cwd.to_owned(), cx);
        }
        // The git surfaces follow the tab's checkout: Diff mode is rebuilt
        // when the tab is the one on screen - switching tab already does
        // that, and binding changes the same fact without a switch.
        if is_active_tab {
            self.reconcile_diff_after_workspace_change(cx);
        }
        self.save_session(cx);
        cx.notify();
        Ok(())
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
                    let branches =
                        crate::workspace::worktree::list_branches(&probe.to_string_lossy());
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
    /// indices do not survive an await. A binding chosen while git runs is
    /// left in place (issue #937); the checkout on disk stays. Nothing
    /// removes it but the tab menu's "Remove worktree"
    /// ([`Self::remove_tab_worktree`]).
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
                let _ = self.set_tab_worktree(ws_idx, tab_idx, None, cx);
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

        // Snapshot before the await. Fast binds return above this check and
        // still bump the generation, so they win when this checkout lands.
        let binding_at_start = self.begin_slow_branch_checkout(tab_id, &branch, cx);
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
                            Ok(path) => app.land_branch_checkout(
                                ws_id,
                                tab_id,
                                &repo_root,
                                path,
                                binding_at_start,
                                cx,
                            ),
                            Err(message) => app.show_toast(message, cx),
                        }
                        cx.notify();
                    })
                });
            },
        )
        .detach();
    }

    /// Mark `branch` as the checkout in flight and return the tab's binding
    /// generation at this moment (issue #937). The landing binds only while
    /// that generation is still current.
    fn begin_slow_branch_checkout(
        &mut self,
        tab_id: u64,
        branch: &str,
        cx: &mut Context<Self>,
    ) -> u64 {
        let binding_at_start = self.worktree_states.binding_generation(tab_id);
        self.branch_checkout_pending = Some(branch.to_string());
        cx.notify();
        binding_at_start
    }

    /// Bind the checkout a slow `git worktree add` produced, unless the tab's
    /// binding changed while git ran (issue #937).
    ///
    /// The directory is left on disk either way. Only "Remove worktree"
    /// deletes a checkout this picker created.
    fn land_branch_checkout(
        &mut self,
        ws_id: u64,
        tab_id: u64,
        repo_root: &std::path::Path,
        path: std::path::PathBuf,
        binding_at_start: u64,
        cx: &mut Context<Self>,
    ) {
        // Direct callers (tests) did not pass through the spawn's clear.
        self.branch_checkout_pending = None;
        let Some((ws_idx, tab_idx)) = self.tab_position(ws_id, tab_id) else {
            cx.notify();
            return;
        };
        if self.worktree_states.binding_generation(tab_id) != binding_at_start {
            // The user already chose another binding. Refresh what the picker
            // offers; the new directory is still there. Do not touch the tab.
            if !checkout_probes_suppressed() {
                self.spawn_worktree_listing(ws_idx, cx);
            }
            return;
        }
        if path.as_path() == repo_root {
            let _ = self.set_tab_worktree(ws_idx, tab_idx, None, cx);
        } else {
            // Through the same gate as the fast path, which re-checks that
            // the directory git resolved the branch to still exists.
            self.bind_tab_to_checkout(ws_idx, tab_idx, path, cx);
        }
        if !checkout_probes_suppressed() {
            self.spawn_worktree_listing(ws_idx, cx);
        }
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
    /// `<repo>.worktrees/` no longer grows for the life of the project:
    /// nothing else ever removes a checkout the picker created
    /// ([`crate::workspace::worktree::prepare_branch_checkout`]).
    ///
    /// The invariants: the BRANCH IS NEVER DELETED, a checkout holding
    /// uncommitted work is never removed, and a directory PaneFlow did not
    /// create belongs to somebody else. This is a gesture the user made, so a
    /// refusal is a toast rather than a log line nobody reads. The rules are
    /// [`removal_refusal`]; the git
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
        // A shell or agent still working in the checkout (a tab restored from
        // a session spawned its panes there) must not have its cwd deleted
        // from under it.
        let protected = self.live_terminal_session_ids(cx);
        cx.spawn(
            async move |this: gpui::WeakEntity<Self>, cx: &mut gpui::AsyncApp| {
                let (probe_root, probe_path) = (repo_root.clone(), path.clone());
                let checked = smol::unblock(move || {
                    check_checkout_removable(&probe_root, &probe_path, &open_roots, &protected)
                })
                .await;
                // Re-validate against the workspaces open *now*, on the main
                // thread, and only then delete. In the remaining window git
                // itself still refuses a checkout that turned dirty.
                let revalidated = cx.update(|cx| {
                    this.update(cx, |app: &mut Self, cx: &mut Context<Self>| {
                        let refusal = checked
                            .err()
                            .or_else(|| open_workspace_refusal(&path, &app.open_workspace_roots()));
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

    /// Drop every trace of a checkout that is gone: unbind the tabs that
    /// worked in it, forget its cached git state, refresh what the picker
    /// offers, and close any Review pane still showing that checkout.
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
            let _ = self.set_tab_worktree(ws_idx, tab_idx, None, cx);
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
        self.review_forget_worktree(repo_root, path, cx);
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
/// warning.
fn removal_refusal(
    repo_root: &std::path::Path,
    path: &std::path::Path,
    branch: Option<&str>,
    open_roots: &[std::path::PathBuf],
) -> Option<String> {
    if let Some(reason) = open_workspace_refusal(path, open_roots) {
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
/// open, not ours, or not clean, then remove the directory and drop
/// the administrative entry that named it.
///
/// The branch is read from a fresh `git worktree list` rather than the
/// picker's cached listing, so the ownership test runs against what git holds
/// now; a path git no longer lists is refused too, there being nothing to
/// remove. Cleanliness is [`worktree::is_clean_for_removal`]: tracked
/// modifications and untracked files refuse, ignored files (the `.env*`
/// copies the picker makes, build output) do not, the same gate
/// `git worktree remove` applies. A live process whose cwd is inside the
/// checkout refuses too ([`worktree::worktree_has_live_process_cwd`], with
/// the open terminals' sessions protected): a
/// shell or agent must not have its directory deleted from under it. Both
/// report an error rather than "clean" when they cannot prove it, and that
/// error propagates: never delete what cannot be read. The BRANCH IS NEVER
/// DELETED.
#[cfg(test)]
fn remove_checkout(
    repo_root: &std::path::Path,
    path: &std::path::Path,
    open_roots: &[std::path::PathBuf],
    protected_session_ids: &[u32],
) -> Result<(), String> {
    check_checkout_removable(repo_root, path, open_roots, protected_session_ids)?;
    remove_validated_checkout(repo_root, path)
}

/// The workspace-state refusal of [`removal_refusal`], on its own so
/// [`PaneFlowApp::remove_tab_worktree`] can apply it a second time on the
/// main thread, against the workspaces open at that moment, after the git
/// probes and before the delete (issue #348). `open_roots` are matched at
/// or under the path.
fn open_workspace_refusal(
    path: &std::path::Path,
    open_roots: &[std::path::PathBuf],
) -> Option<String> {
    if open_roots.iter().any(|root| root.starts_with(path)) {
        return Some(format!(
            "{} is open as a workspace - close it first",
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
    if let Some(reason) = removal_refusal(repo_root, path, entry.branch.as_deref(), open_roots) {
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

/// The deleting half of [`remove_checkout`]: `git worktree remove`, which
/// refuses by itself a checkout that turned dirty since the check and deletes
/// that checkout's administrative entry. A repository-wide prune does not
/// follow. A sibling whose directory is only temporarily missing would lose
/// its HEAD, index, and reflog (issue #938). The BRANCH IS NEVER DELETED.
fn remove_validated_checkout(
    repo_root: &std::path::Path,
    path: &std::path::Path,
) -> Result<(), String> {
    crate::workspace::worktree::remove_worktree(repo_root, path)
}

// A probe still running on smol's pool when a test returns is dropped off
// the GPUI test thread, and the scheduler aborts the suite.
#[cfg(test)]
thread_local! {
    static SUPPRESS_CHECKOUT_PROBES: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn checkout_probes_suppressed() -> bool {
    #[cfg(test)]
    {
        SUPPRESS_CHECKOUT_PROBES.with(|flag| flag.get())
    }
    #[cfg(not(test))]
    {
        false
    }
}

#[cfg(test)]
struct SuppressCheckoutProbes;

#[cfg(test)]
impl SuppressCheckoutProbes {
    fn arm() -> Self {
        SUPPRESS_CHECKOUT_PROBES.with(|flag| flag.set(true));
        Self
    }
}

#[cfg(test)]
impl Drop for SuppressCheckoutProbes {
    fn drop(&mut self) {
        SUPPRESS_CHECKOUT_PROBES.with(|flag| flag.set(false));
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CheckoutGit, SuppressCheckoutProbes, WorktreeStates, removal_refusal, remove_checkout,
        remove_validated_checkout,
    };
    use crate::workspace::GitDiffStats;
    use gpui::AppContext;
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
    fn every_picked_checkout_passes_through_the_binding_gate() {
        // Findings 3 and 4 at the source level: the fast path and the async
        // landing of `bind_tab_to_branch` bind through `bind_tab_to_checkout`
        // ("still a directory"), never through the raw setter,
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
        let exists_at = gate
            .find("crate::workspace::existing_worktree_dir(Some(path))")
            .expect("the gate checks the directory still exists");
        let utf8_guard = gate
            .find("path.to_str().is_some()")
            .expect("a non-UTF-8 path must not bump the binding generation");
        let bump_at = gate
            .find("self.bump_tab_binding(ws_idx, tab_idx)")
            .expect("a same-path rebind still bumps");
        let set_at = gate
            .find("self.set_tab_worktree(ws_idx, tab_idx, Some(path), cx)")
            .expect("the gate is what binds");
        assert!(
            exists_at < utf8_guard && utf8_guard < bump_at && bump_at < set_at,
            "refuse a non-UTF-8 path before bumping, and bump before the bind: {gate}"
        );
        assert!(
            gate.contains("self.show_toast(message, cx)"),
            "the bind error is shown with the worktree toast: {gate}"
        );
        assert_eq!(
            gate.matches("self.show_toast(").count(),
            2,
            "each refusal reaches the user as a toast: {gate}"
        );
    }

    #[test]
    fn binding_the_active_tab_retargets_the_git_surfaces() {
        // Issue #347 review, finding 11: the bind only saved and repainted,
        // so Diff mode kept showing the checkout the tab had just left.
        let src = include_str!("tab_worktree.rs");
        let set = crate::source_probe::source_slice(
            src,
            "pub(crate) fn set_tab_worktree(",
            "/// Refresh what the branch picker offers",
        );
        let active_at = set
            .find("if is_active_tab {")
            .expect("the rebuild is gated on the tab being the visible one");
        assert!(
            set[active_at..].contains("self.reconcile_diff_after_workspace_change(cx);"),
            "Diff mode must rebuild when the visible tab rebinds: {set}"
        );
    }

    /// Issue #1025: a checkout whose name is not valid UTF-8 cannot be stored
    /// on the session (`TabSession::worktree` is a `String`) or passed to git.
    /// The bind returns that error and leaves the tab alone.
    #[gpui::test]
    fn a_non_utf8_worktree_bind_is_refused_and_the_tab_is_unchanged(cx: &mut gpui::TestAppContext) {
        use std::os::unix::ffi::OsStrExt;

        // APFS rejects a directory whose name is not valid UTF-8 (EILSEQ).
        // The bind API takes a `PathBuf`, and the refusal is `to_str()`, so
        // the fixture is that path. A real directory is not required.
        let parent = tempfile::tempdir().expect("tempdir");
        let path = parent
            .path()
            .join(std::ffi::OsStr::from_bytes(b"wt-\xFF\xFE"));
        assert!(
            path.to_str().is_none(),
            "the fixture name must not be valid UTF-8"
        );

        let root = tempfile::tempdir().expect("workspace root");
        let _probes = SuppressCheckoutProbes::arm();
        let window = cx.add_empty_window();
        let app = window.new(blank_paneflow_app);
        app.update(window, |app, cx| {
            let ws = crate::workspace::Workspace::empty_with_cwd_and_id(
                7,
                "repo",
                root.path().to_path_buf(),
            );
            let tab_id = ws.tabs()[0].id;
            app.workspaces.push(ws);
            app.active_idx = 0;
            // `save_session` writes session-dev.json. Holding a restore makes
            // it return before `session_path()`, so a regression that still
            // saves cannot latch `PANEFLOW_HOME`.
            app.session_restore = hold_session_save();

            let before = app.workspaces[0].tabs()[0].worktree.clone();
            let generation = app.worktree_states.binding_generation(tab_id);
            let err = app
                .set_tab_worktree(0, 0, Some(path.clone()), cx)
                .expect_err("a non-UTF-8 worktree must be refused");
            assert!(
                err.contains("not valid UTF-8"),
                "the error must say the path is not valid UTF-8: {err}"
            );
            assert_eq!(
                app.workspaces[0].tabs()[0].worktree,
                before,
                "a refused bind must not change the tab"
            );
            assert_eq!(
                app.worktree_states.binding_generation(tab_id),
                generation,
                "a refused bind must not retire an in-flight checkout"
            );

            // A path that is already in memory must not be probed or saved
            // as U+FFFD. The bind above refuses to put one there.
            app.workspaces[0].tab_mut(0).expect("tab").worktree = Some(path);
            assert!(
                app.workspaces[0].bound_tab_worktrees().is_empty(),
                "git probes must skip a non-UTF-8 worktree"
            );
            let saved = app.workspaces[0].serialize_tabs_without_scrollback(cx);
            assert_eq!(
                saved[0].worktree, None,
                "the session must omit a non-UTF-8 worktree"
            );
        });
        cx.run_until_parked();
    }

    fn state(branch: &str, insertions: usize) -> CheckoutGit {
        CheckoutGit {
            branch: branch.to_string(),
            is_repo: true,
            stats: GitDiffStats {
                files_changed: 1,
                insertions,
                deletions: 0,
                ..GitDiffStats::default()
            },
        }
    }

    #[test]
    fn removal_is_refused_for_what_is_not_ours_or_open() {
        // Issue #348: the tab menu's "Remove worktree" row takes a
        // picker-created checkout back down, and every refusal is a toast.
        use crate::workspace::worktree::{worktree_dir, worktree_dir_hashed};
        let repo = PathBuf::from("/repo");
        let ours = worktree_dir(&repo, "feat/x");
        let none: Vec<PathBuf> = Vec::new();
        assert_eq!(
            removal_refusal(&repo, &ours, Some("feat/x"), &none),
            None,
            "a clean, owned, not-open checkout may go"
        );
        assert_eq!(
            removal_refusal(
                &repo,
                &worktree_dir_hashed(&repo, "feat/x"),
                Some("feat/x"),
                &none
            ),
            None,
            "the collision-resistant directory is ours too"
        );
        let not_ours = |path: &Path, branch: Option<&str>| {
            removal_refusal(&repo, path, branch, &none)
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
            removal_refusal(&repo, &ours, Some("feat/x"), std::slice::from_ref(&ours))
                .unwrap_or_default()
                .contains("open as a workspace"),
            "a checkout that is itself an open workspace is somebody's cwd"
        );
        assert!(
            removal_refusal(&repo, &ours, Some("feat/x"), &[ours.join("src")])
                .unwrap_or_default()
                .contains("open as a workspace"),
            "a workspace standing under the checkout is open in it"
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
        let repo_root = tmp.path().join("repo");
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
        // The foreign checkout sits outside every PaneFlow root, so the
        // listing keeps it as git reports it (symlinks resolved).
        let foreign = tmp.path().join("foreign");
        let foreign_s = foreign.to_string_lossy().into_owned();
        git(
            &repo_root,
            &["worktree", "add", "-q", &foreign_s, "-b", "feat/foreign"],
        );
        let foreign = std::fs::canonicalize(&foreign).expect("canonical foreign checkout");
        let before = list_worktrees(&repo_root).expect("listing");
        assert_eq!(before.len(), 4, "root, two picker checkouts, one foreign");

        let refused = remove_checkout(&repo_root, &dirty, &[], &[]).expect_err("dirty is refused");
        assert!(refused.contains("uncommitted"), "{refused}");
        assert!(dirty.is_dir());
        let refused =
            remove_checkout(&repo_root, &foreign, &[], &[]).expect_err("foreign is refused");
        assert!(refused.contains("not created by PaneFlow"), "{refused}");
        assert!(foreign.is_dir());
        let refused = remove_checkout(&repo_root, &clean, std::slice::from_ref(&clean), &[])
            .expect_err("an open workspace is refused");
        assert!(refused.contains("open as a workspace"), "{refused}");

        // A shell still working in the checkout keeps it: its cwd is never
        // deleted from under it.
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
        let refused = remove_checkout(&repo_root, &clean, &[], &[])
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
        remove_checkout(&repo_root, &clean, &[], &[]).expect("a clean owned checkout is removed");
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

    /// Issue #938: `git worktree remove` already drops the entry it removed.
    /// A repository-wide `git worktree prune` must not follow, or a sibling
    /// whose directory is only temporarily missing loses its registration.
    #[test]
    fn removing_a_checkout_keeps_a_missing_sibling_worktree_entry() {
        let git = |cwd: &Path, args: &[&str]| -> String {
            let mut cmd = crate::workspace::worktree::git_command();
            cmd.env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .arg("-C")
                .arg(cwd)
                .args(args);
            let out = cmd.output().expect("git runs");
            assert!(
                out.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };

        let tmp = tempfile::tempdir().expect("tempdir");
        let repo_root = tmp.path().join("repo");
        std::fs::create_dir_all(&repo_root).expect("repo root");
        git(&repo_root, &["init", "-q"]);
        git(
            &repo_root,
            &["config", "user.email", "paneflow-tests@example.invalid"],
        );
        git(&repo_root, &["config", "user.name", "PaneFlow Tests"]);
        std::fs::write(repo_root.join("README.md"), "test\n").expect("tracked file");
        git(&repo_root, &["add", "."]);
        git(
            &repo_root,
            &[
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-q",
                "-m",
                "fixture",
            ],
        );

        let kept = tmp.path().join("kept");
        let missing = tmp.path().join("missing");
        let kept_arg = kept.to_string_lossy().into_owned();
        let missing_arg = missing.to_string_lossy().into_owned();
        git(
            &repo_root,
            &["worktree", "add", "-q", &kept_arg, "-b", "feat/kept"],
        );
        git(
            &repo_root,
            &["worktree", "add", "-q", &missing_arg, "-b", "feat/missing"],
        );

        let gitfile = std::fs::read_to_string(missing.join(".git")).expect("sibling gitfile");
        let gitdir = gitfile
            .lines()
            .find_map(|line| line.trim().strip_prefix("gitdir:"))
            .expect("gitdir line")
            .trim();
        let admin = {
            let raw = PathBuf::from(gitdir);
            if raw.is_absolute() {
                raw
            } else {
                missing.join(raw)
            }
        };
        assert!(
            admin.is_dir(),
            "the sibling is registered at {}",
            admin.display()
        );
        let worktrees = repo_root.join(".git").join("worktrees");
        assert!(
            std::fs::canonicalize(&admin)
                .expect("admin dir")
                .starts_with(std::fs::canonicalize(&worktrees).expect("worktrees dir")),
            "registration must be .git/worktrees/<id>, got {}",
            admin.display()
        );

        let aside = tmp.path().join("missing-aside");
        std::fs::rename(&missing, &aside).expect("move the sibling aside");
        assert!(
            !missing.exists(),
            "the sibling directory is missing while it stays registered"
        );
        assert!(
            admin.is_dir(),
            "moving the directory must not drop {}",
            admin.display()
        );

        remove_validated_checkout(&repo_root, &kept).expect("remove the other checkout");
        assert!(
            !kept.exists(),
            "git worktree remove deletes the checkout it was given"
        );
        assert!(
            admin.is_dir(),
            "removing {} must keep the missing sibling's administrative entry at {}",
            kept.display(),
            admin.display()
        );
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

    /// Issue #937: `git worktree add` for branch A must not put the tab back
    /// on A when the user bound that tab somewhere else while git was running.
    #[gpui::test]
    fn a_late_branch_checkout_does_not_override_a_newer_binding(cx: &mut gpui::TestAppContext) {
        let newer = tempfile::tempdir().expect("newer checkout");
        let late = tempfile::tempdir().expect("late checkout");
        let root = tempfile::tempdir().expect("workspace root");
        // Binding starts git probes on smol threads. The test scheduler
        // panics if one of those tasks is still running when this returns.
        let _probes = SuppressCheckoutProbes::arm();
        let window = cx.add_empty_window();
        let app = window.new(blank_paneflow_app);
        app.update(window, |app, cx| {
            let ws = crate::workspace::Workspace::empty_with_cwd_and_id(
                7,
                "repo",
                root.path().to_path_buf(),
            );
            let tab_id = ws.tabs()[0].id;
            let ws_id = ws.id;
            app.workspaces.push(ws);
            app.active_idx = 0;
            // `save_session` writes session-dev.json. Holding a restore makes
            // it return before `session_path()`, so this test never latches
            // `PANEFLOW_HOME`.
            app.session_restore = hold_session_save();
            let started = app.begin_slow_branch_checkout(tab_id, "feat/a", cx);
            assert!(
                app.bind_tab_to_checkout(0, 0, newer.path().to_path_buf(), cx),
                "the binding chosen while git runs must take"
            );
            app.land_branch_checkout(
                ws_id,
                tab_id,
                root.path(),
                late.path().to_path_buf(),
                started,
                cx,
            );
            assert_eq!(
                app.workspaces[0].tabs()[0].worktree.as_deref(),
                Some(newer.path()),
                "a checkout that finishes late must not replace the newer binding"
            );
            assert!(
                late.path().is_dir(),
                "the late checkout stays on disk; only Remove worktree deletes one"
            );
            assert!(
                app.branch_checkout_pending.is_none(),
                "landing releases the in-flight slot"
            );
        });
        // Binding and the skipped landing start git probes. Finish them on
        // this scheduler. Dropping the window while a probe is still on the
        // background thread panics the harness.
        cx.run_until_parked();

        // Production must snapshot before the await and land through the same
        // helper this test calls. The behavioral half above cannot see a spawn
        // that went back to binding unconditionally.
        let src = include_str!("tab_worktree.rs");
        let bind = crate::source_probe::source_slice(
            src,
            "pub(crate) fn bind_tab_to_branch(",
            "/// Locate a tab by the ids that survive an await",
        );
        let armed = crate::source_probe::source_slice(bind, "let binding_at_start", "cx.spawn(");
        assert!(
            !armed.contains(".await"),
            "the binding generation must be snapshotted before git runs: {armed}"
        );
        let spawn = crate::source_probe::source_slice(bind, "cx.spawn(", ".detach();");
        assert!(
            spawn.contains("land_branch_checkout(") && spawn.contains("binding_at_start"),
            "the spawn must land through the snapshotted generation: {spawn}"
        );
    }

    fn hold_session_save() -> Option<crate::app::session::PendingSessionRestore> {
        crate::app::session::PendingSessionRestore::from_session(
            paneflow_config::schema::SessionState {
                version: paneflow_config::schema::SESSION_SCHEMA_VERSION,
                active_workspace: 0,
                workspaces: vec![paneflow_config::schema::WorkspaceSession {
                    title: "hold".into(),
                    cwd: "/tmp/paneflow-session-hold".into(),
                    tabs: vec![paneflow_config::schema::TabSession::empty()],
                    active_tab: 0,
                    legacy_layout: None,
                    legacy_empty: false,
                    pinned: false,
                    sidebar_collapsed: false,
                    muted: false,
                }],
                mode: paneflow_config::schema::AppMode::Cli,
                review_layout: None,
                review_collapsed: Vec::new(),
                primary_sidebar_collapsed: false,
            },
        )
    }

    fn blank_paneflow_app(cx: &mut gpui::Context<crate::PaneFlowApp>) -> crate::PaneFlowApp {
        use std::sync::atomic::{AtomicU64, AtomicUsize};
        use std::sync::{Arc, Mutex};

        use gpui::AppContext;

        let settings_search_input =
            cx.new(|cx| crate::widgets::text_input::TextInput::new("", "Search settings…", cx));
        let shortcut_search_input = cx.new(|cx| {
            crate::widgets::text_input::TextInput::new("", "Search actions or keys…", cx)
        });
        let sessions_filter_input =
            cx.new(|cx| crate::widgets::text_input::TextInput::new("", "Filter sessions", cx));
        let (_ipc_tx, ipc_rx) = std::sync::mpsc::channel();
        let (_git_tx, git_event_rx) = std::sync::mpsc::channel();
        crate::PaneFlowApp {
            workspaces: Vec::new(),
            active_idx: 0,
            renaming_idx: None,
            renaming_tab: None,
            rename_text: String::new(),
            rename_seeded: false,
            pending_config: Arc::new(Mutex::new(None)),
            save_seq: Arc::new(AtomicU64::new(0)),
            session_corruption: None,
            session_restore: None,
            config_persist_seq: Arc::new(AtomicU64::new(0)),
            config_field_persist_seq: Arc::new(crate::config_writer::FieldPersistSeq::default()),
            config_persist_in_flight: Arc::new(AtomicUsize::new(0)),
            config_last_persist_gen: Arc::new(AtomicU64::new(0)),
            cached_config: paneflow_config::schema::PaneFlowConfig::default(),
            ipc_rx,
            ipc_status: crate::ipc::IpcStatus::disabled_for_test(),
            title_bar: cx.new(crate::window_chrome::title_bar::TitleBar::new),
            primary_sidebar_visible: true,
            primary_sidebar_animation: None,
            git_watcher: None,
            git_event_rx,
            git_watch_counts: std::collections::HashMap::new(),
            terminal_branches: std::collections::HashMap::new(),
            settings_section: None,
            settings_scroll: gpui::ScrollHandle::new(),
            settings_drag: None,
            settings_search_input,
            terminal_dropdown: None,
            general_dropdown: None,
            new_tab_branch_dropdown: None,
            sidebar_scroll: gpui::ScrollHandle::new(),
            effective_shortcuts: Vec::new(),
            recording_shortcut_idx: None,
            shortcut_search_input,
            shortcut_capture_active: false,
            shortcut_reset_pending: false,
            collapsed_shortcut_groups: std::collections::HashSet::new(),
            shortcut_rows: Vec::new(),
            shortcut_list: crate::settings::tabs::shortcuts::new_shortcut_list_state(),
            shortcut_drag: None,
            settings_focus: cx.focus_handle(),
            mono_font_names: Vec::new(),
            font_dropdown_open: false,
            font_search: String::new(),
            theme_dropdown_open: false,
            theme_mode: crate::ThemeMode::Dark,
            workspace_menu_open: None,
            worktree_states: crate::app::tab_worktree::WorktreeStates::default(),
            branch_checkout_pending: None,
            sidebar_customize_menu_open: false,
            sidebar_show_submenu_open: false,
            tab_menu_open: None,
            pane_menu_open: None,
            pending_pane_focus: None,
            agent_sessions: crate::AgentSessionsState {
                sessions_sidebar_open: false,
                sessions_sidebar_animation: None,
                sessions_by_agent: std::array::from_fn(|_| Vec::new()),
                sessions_omitted: [0; crate::agent_sessions::SESSION_AGENT_COUNT],
                sessions_cwd: None,
                sessions_surface_id: None,
                sessions_bound_palette: None,
                sessions_scroll: gpui::ScrollHandle::new(),
                sessions_scan_generation: 0,
                sessions_selected: 0,
                sessions_focus: cx.focus_handle(),
                sessions_group_collapsed: [false; crate::agent_sessions::SESSION_AGENT_COUNT],
                sessions_group_show_all: [false; crate::agent_sessions::SESSION_AGENT_COUNT],
                sessions_scanning: [false; crate::agent_sessions::SESSION_AGENT_COUNT],
                sessions_filter_input,
            },
            toast: None,
            toast_queue: std::collections::VecDeque::new(),
            _toast_task: None,
            toast_serial: 0,
            jump_cursor: None,
            closed_items: Vec::new(),
            show_about_dialog: false,
            about_dialog_focus: cx.focus_handle(),
            system_info_dialog: None,
            system_info_dialog_focus: cx.focus_handle(),
            overlay_origins: Default::default(),
            pane_overview: None,
            pane_overview_focus: cx.focus_handle(),
            work_review: None,
            work_review_focus: cx.focus_handle(),
            pane_palette: None,
            pane_palette_focus: cx.focus_handle(),
            pending_palette_focus: false,
            pending_palette_launch: None,
            pending_close: None,
            claude_registry_seen: Default::default(),
            claude_registry_sweep_pending: false,
            pending_close_focus: cx.focus_handle(),
            pending_close_focus_claim: false,
            review: crate::app::review::ReviewState::new(cx),
            mode: paneflow_config::schema::AppMode::Cli,
            sidebar_order_cache: std::cell::RefCell::new(Default::default()),
            empty_workspace_focus: cx.focus_handle(),
            sidebar_rename_focus: cx.focus_handle(),
        }
    }
}
