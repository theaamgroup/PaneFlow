use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;

use gpui::{App, Context};
use paneflow_config::schema::{LayoutNode, SurfaceDefinition};

use super::MAX_REVIEW_PANES;
use crate::PaneFlowApp;
use crate::app::session::{PersistedDirStatus, RESTORED_CWD_PROBE_TIMEOUT, persisted_dir_status};
use crate::diff::{DiffWorktree, ReviewSubject};
use crate::layout::LayoutTree;

pub(crate) fn surface_for_subject(subject: &ReviewSubject) -> SurfaceDefinition {
    SurfaceDefinition {
        surface_type: Some("diff".to_string()),
        name: Some(subject.worktree.branch.clone()),
        custom_name: None,
        cwd: Some(subject.worktree.path.to_string_lossy().into_owned()),
        path: Some(subject.repo_root.to_string_lossy().into_owned()),
        focus: Some(true),
        scrollback: None,
        agent: None,
        font_size: None,
        // Fork-only: a diff pane has no agent identity or task to carry.
        agent_context: None,
    }
}

fn subject_from_surface(surface: &SurfaceDefinition) -> Option<ReviewSubject> {
    if surface.surface_type.as_deref() != Some("diff") {
        return None;
    }
    let path = PathBuf::from(surface.cwd.as_deref()?);
    let repo_root = PathBuf::from(surface.path.as_deref()?);
    Some(ReviewSubject {
        repo_root,
        worktree: DiffWorktree {
            path,
            branch: surface.name.clone().unwrap_or_default(),
            workspace_id: None,
        },
    })
}

fn prune_layout(
    node: &LayoutNode,
    keep: &impl Fn(&SurfaceDefinition) -> bool,
) -> Option<LayoutNode> {
    match node {
        LayoutNode::Pane { surfaces } => {
            surfaces
                .first()
                .filter(|surface| keep(surface))
                .map(|surface| LayoutNode::Pane {
                    surfaces: vec![surface.clone()],
                })
        }
        LayoutNode::Split {
            direction,
            children,
            ..
        } => {
            let resolved = node.resolved_ratios();
            let mut kept: Vec<(LayoutNode, f64)> = Vec::new();
            for (index, child) in children.iter().enumerate() {
                if let Some(pruned) = prune_layout(child, keep) {
                    let ratio = resolved
                        .get(index)
                        .copied()
                        .unwrap_or(1.0 / children.len() as f64);
                    kept.push((pruned, ratio));
                }
            }
            match kept.len() {
                0 => None,
                1 => kept.pop().map(|(child, _)| child),
                _ => {
                    let total: f64 = kept.iter().map(|(_, ratio)| ratio).sum();
                    let ratios = kept
                        .iter()
                        .map(|(_, ratio)| if total > 0.0 { ratio / total } else { 1.0 })
                        .collect();
                    Some(LayoutNode::Split {
                        direction: direction.clone(),
                        ratio: None,
                        ratios: Some(ratios),
                        children: kept.into_iter().map(|(child, _)| child).collect(),
                    })
                }
            }
        }
    }
}

impl PaneFlowApp {
    pub(crate) fn serialize_review_layout(&self, cx: &App) -> Option<LayoutNode> {
        if let Some(root) = self.review.full_layout() {
            return Some(root.serialize_without_scrollback(cx));
        }
        // Issue #932: Review was off at restore, so nothing was rebuilt.
        // Write the raw node back. `skip_serializing_if = Option::is_none`
        // would otherwise drop the key on the save that finishes restore.
        self.review.retained_layout.clone()
    }

    pub(crate) fn serialize_review_collapsed(&self) -> Vec<String> {
        let mut roots: Vec<String> = self
            .review
            .collapsed
            .iter()
            .map(|root| root.to_string_lossy().into_owned())
            .collect();
        roots.sort();
        roots
    }

    pub(crate) fn restore_review_collapsed(&mut self, roots: &[String]) {
        let open_roots: HashSet<PathBuf> = self
            .workspaces
            .iter()
            .filter_map(|ws| ws.repo_root.clone())
            .collect();
        self.review.collapsed = roots
            .iter()
            .map(PathBuf::from)
            .filter(|root| open_roots.contains(root))
            .collect();
    }

    /// Rebuild the Review grid from a saved node.
    ///
    /// Issue #1095: only a checkout whose `stat` finished and found no
    /// directory is pruned. When any open checkout's probe is unconfirmed
    /// (timed out, or its `/Volumes` drive is not mounted), the whole node,
    /// ratios included, is held in `retained_layout` and no pane opens.
    /// Opening the confirmed part would save that smaller grid over the
    /// original. A later call (the next Review open, a branch pick, or the
    /// next launch) probes again. Any confirmed verdict clears the held node.
    pub(crate) fn restore_review_layout(&mut self, node: &LayoutNode, cx: &mut Context<Self>) {
        let open_roots: HashSet<PathBuf> = self
            .workspaces
            .iter()
            .filter_map(|ws| ws.repo_root.clone())
            .collect();
        let mut checkouts: HashMap<PathBuf, PersistedDirStatus> = HashMap::new();
        for subject in collect_pane_surfaces(node)
            .into_iter()
            .filter_map(subject_from_surface)
            .filter(|subject| open_roots.contains(&subject.repo_root))
        {
            let path = subject.worktree.path;
            if checkouts.contains_key(&path) {
                continue;
            }
            let status = persisted_dir_status(&path, RESTORED_CWD_PROBE_TIMEOUT);
            if status == PersistedDirStatus::Unknown {
                log::warn!(
                    "review restore: checkout {} did not answer in time or is not mounted; keeping the saved grid unopened",
                    path.display()
                );
                self.review.retained_layout = Some(node.clone());
                self.review.unconfirmed_checkout = Some(path);
                return;
            }
            checkouts.insert(path, status);
        }
        self.review.retained_layout = None;
        self.review.unconfirmed_checkout = None;
        let keep = |surface: &SurfaceDefinition| {
            subject_from_surface(surface).is_some_and(|subject| {
                open_roots.contains(&subject.repo_root)
                    && checkouts.get(&subject.worktree.path) == Some(&PersistedDirStatus::Live)
            })
        };
        let Some(mut pruned) = prune_layout(node, &keep) else {
            return;
        };
        paneflow_config::schema::validate_layout(&mut pruned);
        if pruned.leaf_count() > MAX_REVIEW_PANES {
            log::warn!(
                "review restore: layout has {} panes, cap is {MAX_REVIEW_PANES}; skipped",
                pruned.leaf_count()
            );
            return;
        }
        let workspace_for_path = |path: &PathBuf| {
            // A root hit wins. Issue #933: a tab-bound checkout never equals
            // the workspace root, so the root miss falls through to the first
            // workspace whose tab is bound to that path.
            if let Some(ws) = self.workspaces.iter().find(|ws| ws.worktree_root == *path) {
                return Some(ws.id);
            }
            self.workspaces
                .iter()
                .find(|ws| {
                    ws.tabs()
                        .iter()
                        .any(|tab| tab.worktree.as_deref() == Some(path.as_path()))
                })
                .map(|ws| ws.id)
        };
        let subjects: Vec<Option<ReviewSubject>> = collect_pane_surfaces(&pruned)
            .into_iter()
            .map(|surface| {
                subject_from_surface(surface).map(|mut subject| {
                    subject.worktree.workspace_id = workspace_for_path(&subject.worktree.path);
                    subject
                })
            })
            .collect();
        let mut subjects: VecDeque<Option<ReviewSubject>> = subjects.into();
        let mut fallback: Vec<ReviewSubject> = Vec::new();
        let tree = LayoutTree::from_layout_node(&pruned, &mut VecDeque::new(), &mut |_| {
            let subject = subjects
                .pop_front()
                .flatten()
                .or_else(|| fallback.pop())
                .or_else(|| self.review_default_subject())
                .unwrap_or_else(|| ReviewSubject {
                    repo_root: PathBuf::new(),
                    worktree: DiffWorktree {
                        path: PathBuf::new(),
                        branch: String::new(),
                        workspace_id: None,
                    },
                });
            fallback.push(subject.clone());
            self.review_new_pane(subject, cx)
        });
        self.review.active_pane = tree.first_leaf().as_ref().map(gpui::Entity::downgrade);
        self.review.layout = Some(tree);
    }
}

fn collect_pane_surfaces(node: &LayoutNode) -> Vec<&SurfaceDefinition> {
    match node {
        LayoutNode::Pane { surfaces } => surfaces.first().into_iter().collect(),
        LayoutNode::Split { children, .. } => {
            children.iter().flat_map(collect_pane_surfaces).collect()
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::app::test_support::blank_paneflow_app;
    use gpui::AppContext;

    use super::*;

    fn diff_pane(repo: &str, path: &str) -> LayoutNode {
        LayoutNode::Pane {
            surfaces: vec![surface_for_subject(&ReviewSubject {
                repo_root: PathBuf::from(repo),
                worktree: DiffWorktree {
                    path: PathBuf::from(path),
                    branch: "main".into(),
                    workspace_id: None,
                },
            })],
        }
    }

    fn split(children: Vec<LayoutNode>, ratios: Vec<f64>) -> LayoutNode {
        LayoutNode::Split {
            direction: "vertical".into(),
            ratio: None,
            ratios: Some(ratios),
            children,
        }
    }

    #[test]
    fn diff_surface_round_trips_its_subject() {
        let subject = ReviewSubject {
            repo_root: PathBuf::from("/repo"),
            worktree: DiffWorktree {
                path: PathBuf::from("/repo/.worktrees/feature"),
                branch: "feature".into(),
                workspace_id: Some(3),
            },
        };
        let restored = subject_from_surface(&surface_for_subject(&subject)).unwrap();
        assert_eq!(restored.repo_root, subject.repo_root);
        assert_eq!(restored.worktree.path, subject.worktree.path);
        assert_eq!(restored.worktree.branch, "feature");
        assert_eq!(restored.worktree.workspace_id, None);
    }

    #[test]
    fn prune_drops_unknown_subjects_and_collapses_single_children() {
        let node = split(
            vec![
                diff_pane("/a", "/a"),
                split(
                    vec![diff_pane("/b", "/b"), diff_pane("/c", "/c")],
                    vec![0.5, 0.5],
                ),
            ],
            vec![0.3, 0.7],
        );
        let keep = |surface: &SurfaceDefinition| surface.path.as_deref() != Some("/b");
        let pruned = prune_layout(&node, &keep).unwrap();
        match pruned {
            LayoutNode::Split {
                children, ratios, ..
            } => {
                assert_eq!(children.len(), 2);
                assert!(matches!(&children[1], LayoutNode::Pane { .. }));
                let ratios = ratios.unwrap();
                assert!((ratios[0] - 0.3).abs() < 1e-9);
                assert!((ratios[1] - 0.7).abs() < 1e-9);
            }
            LayoutNode::Pane { .. } => unreachable!("expected a split"),
        }
    }

    #[test]
    fn prune_renormalizes_ratios_of_kept_children() {
        let node = split(
            vec![
                diff_pane("/a", "/a"),
                diff_pane("/b", "/b"),
                diff_pane("/c", "/c"),
            ],
            vec![0.5, 0.25, 0.25],
        );
        let keep = |surface: &SurfaceDefinition| surface.path.as_deref() != Some("/a");
        let pruned = prune_layout(&node, &keep).unwrap();
        let LayoutNode::Split { ratios, .. } = pruned else {
            unreachable!("expected a split");
        };
        let ratios = ratios.unwrap();
        assert!((ratios[0] - 0.5).abs() < 1e-9);
        assert!((ratios[1] - 0.5).abs() < 1e-9);
    }

    #[test]
    fn prune_returns_none_when_nothing_survives() {
        let node = split(vec![diff_pane("/a", "/a")], vec![1.0]);
        assert!(prune_layout(&node, &|_| false).is_none());
    }

    /// Issue #932: quitting with a Review grid, then relaunching with Review
    /// off, used to drop the node. The save at the end of restore writes
    /// `build_session_state`, and a `None` layout is omitted from session.json.
    #[gpui::test]
    fn review_layout_survives_a_restart_with_review_disabled(cx: &mut gpui::TestAppContext) {
        let saved = split(
            vec![
                diff_pane("/no/such/review-a", "/no/such/review-a/.worktrees/a"),
                diff_pane("/no/such/review-b", "/no/such/review-b/.worktrees/b"),
            ],
            vec![0.25, 0.75],
        );
        let cx = cx.add_empty_window();
        let app = cx.new(|cx| {
            let mut app = blank_paneflow_app(cx);
            app.cached_config.review_enabled = Some(false);
            app
        });
        app.update(cx, |app, cx| {
            app.apply_restored_diff_mode(
                paneflow_config::schema::AppMode::Diff,
                Some(saved.clone()),
                &[],
                cx,
            );
            assert!(
                app.review.layout.is_none(),
                "Review stays closed while the setting is off"
            );
            assert_eq!(app.mode, paneflow_config::schema::AppMode::Cli);
            let state = app.build_session_state(cx);
            assert_eq!(state.review_layout, Some(saved));
        });
    }

    /// Registers checkouts whose `stat` stalls past the restore probe budget,
    /// and removes them again on drop.
    struct StalledCheckouts(Vec<PathBuf>);

    impl StalledCheckouts {
        fn stall(paths: &[&std::path::Path]) -> Self {
            let paths: Vec<PathBuf> = paths.iter().map(|path| path.to_path_buf()).collect();
            crate::app::session::STALLED_STAT_PATHS
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .extend(paths.iter().cloned());
            Self(paths)
        }
    }

    impl Drop for StalledCheckouts {
        fn drop(&mut self) {
            let ours = &self.0;
            crate::app::session::STALLED_STAT_PATHS
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .retain(|path| !ours.contains(path));
        }
    }

    /// One workspace whose repository root is `repo`, so its Review subjects
    /// count as open.
    fn app_with_repo(repo: &str, cx: &mut Context<PaneFlowApp>) -> PaneFlowApp {
        let mut app = blank_paneflow_app(cx);
        let mut workspace =
            crate::workspace::Workspace::empty_with_cwd_and_id(1, "repo", PathBuf::from(repo));
        workspace.repo_root = Some(PathBuf::from(repo));
        workspace.worktree_root = PathBuf::from(repo);
        app.workspaces = vec![workspace];
        app
    }

    /// Issue #1095: a checkout whose `stat` outlives the restore budget is
    /// not missing. Restore keeps the whole saved grid, ratios included,
    /// opens no Review pane for it, and the next save writes it back. Once
    /// the filesystem answers, the retained node rebuilds the grid.
    #[gpui::test]
    fn review_layout_survives_an_unconfirmed_worktree_probe(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        let live = repo.join(".worktrees").join("live");
        let slow = repo.join(".worktrees").join("slow");
        std::fs::create_dir_all(&live).unwrap();
        std::fs::create_dir_all(&slow).unwrap();
        let stalled = StalledCheckouts::stall(&[&slow]);
        let repo = repo.to_str().expect("utf8 temp path").to_string();
        let live = live.to_str().expect("utf8 temp path").to_string();
        let slow = slow.to_str().expect("utf8 temp path").to_string();
        let saved = split(
            vec![diff_pane(&repo, &live), diff_pane(&repo, &slow)],
            vec![0.3, 0.7],
        );

        let cx = cx.add_empty_window();
        let app = cx.new(|cx| app_with_repo(&repo, cx));
        app.update(cx, |app, cx| {
            let _bootstrap = crate::diff::SuppressDiffBootstrap::arm();
            app.apply_restored_diff_mode(
                paneflow_config::schema::AppMode::Diff,
                Some(saved.clone()),
                &[],
                cx,
            );
            assert!(
                app.review.layout.is_none(),
                "no Review pane opens while a checkout is unconfirmed"
            );
            let state = app.build_session_state(cx);
            assert_eq!(
                state.review_layout,
                Some(saved.clone()),
                "the save keeps the original grid and its ratios"
            );

            drop(stalled);
            let retained = app
                .review
                .retained_layout
                .clone()
                .expect("the unconfirmed grid is retained");
            app.restore_review_layout(&retained, cx);
            assert!(app.review.retained_layout.is_none());
            let subjects = app.review_grid_subjects(cx);
            assert_eq!(subjects.len(), 2, "a confirmed probe rebuilds the grid");
            // The live grid stores ratios as f32, so compare them loosely.
            let Some(LayoutNode::Split { ratios, .. }) = app.build_session_state(cx).review_layout
            else {
                unreachable!("expected the rebuilt split");
            };
            let ratios = ratios.expect("the split keeps its ratios");
            assert!((ratios[0] - 0.3).abs() < 1e-6 && (ratios[1] - 0.7).abs() < 1e-6);
        });
    }

    /// `save_session` writes the real `session-dev.json`. A staged restore
    /// makes it return first, so these tests cannot clobber it.
    fn hold_session_saves(app: &mut PaneFlowApp) {
        app.session_restore = crate::app::session::PendingSessionRestore::from_session(
            paneflow_config::schema::SessionState {
                version: paneflow_config::schema::SESSION_SCHEMA_VERSION,
                active_workspace: 0,
                workspaces: vec![paneflow_config::schema::WorkspaceSession {
                    title: String::new(),
                    cwd: String::new(),
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
        );
        assert!(
            app.session_restore.is_some(),
            "session saves must stay skipped"
        );
    }

    /// A checkout directory that exists, as a UTF-8 string.
    fn checkout(root: &std::path::Path, name: &str) -> String {
        let path = root.join(".worktrees").join(name);
        std::fs::create_dir_all(&path).unwrap();
        path.to_str().expect("utf8 temp path").to_string()
    }

    fn picked_subject(repo: &str, path: &str) -> ReviewSubject {
        ReviewSubject {
            repo_root: PathBuf::from(repo),
            worktree: DiffWorktree {
                path: PathBuf::from(path),
                branch: "picked".into(),
                workspace_id: Some(1),
            },
        }
    }

    fn subject_paths(app: &PaneFlowApp, cx: &App) -> Vec<PathBuf> {
        app.review_grid_subjects(cx)
            .into_iter()
            .map(|subject| subject.worktree.path)
            .collect()
    }

    /// Issue #1095: a confirmed-missing checkout does not settle the grid
    /// while a later one is unconfirmed. Nothing is pruned or opened.
    #[gpui::test]
    fn review_restore_holds_a_missing_checkout_when_a_later_one_is_unconfirmed(
        cx: &mut gpui::TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        let slow = checkout(&root, "slow");
        let gone = root.join(".worktrees").join("gone");
        let gone = gone.to_str().expect("utf8 temp path").to_string();
        let repo = root.to_str().expect("utf8 temp path").to_string();
        let _stalled = StalledCheckouts::stall(&[std::path::Path::new(&slow)]);
        let saved = split(
            vec![diff_pane(&repo, &gone), diff_pane(&repo, &slow)],
            vec![0.4, 0.6],
        );

        let cx = cx.add_empty_window();
        let app = cx.new(|cx| app_with_repo(&repo, cx));
        app.update(cx, |app, cx| {
            let _bootstrap = crate::diff::SuppressDiffBootstrap::arm();
            app.apply_restored_diff_mode(
                paneflow_config::schema::AppMode::Diff,
                Some(saved.clone()),
                &[],
                cx,
            );
            assert!(app.review.layout.is_none());
            assert_eq!(app.review.unconfirmed_checkout, Some(PathBuf::from(&slow)));
            assert_eq!(app.build_session_state(cx).review_layout, Some(saved));
        });
    }

    /// Issue #1095: entering Review while the grid is still unconfirmed
    /// opens no default grid, which the next save would write over it.
    #[gpui::test]
    fn review_entry_opens_no_default_grid_while_a_checkout_is_unconfirmed(
        cx: &mut gpui::TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        let live = checkout(&root, "live");
        let slow = checkout(&root, "slow");
        let repo = root.to_str().expect("utf8 temp path").to_string();
        let _stalled = StalledCheckouts::stall(&[std::path::Path::new(&slow)]);
        let saved = split(
            vec![diff_pane(&repo, &live), diff_pane(&repo, &slow)],
            vec![0.3, 0.7],
        );

        let cx = cx.add_empty_window();
        let app = cx.new(|cx| app_with_repo(&repo, cx));
        let _bootstrap = crate::diff::SuppressDiffBootstrap::arm();
        // Entering Review lists worktrees on smol's pool, which aborts the
        // GPUI test scheduler.
        let _listing = crate::app::tab_worktree::SuppressCheckoutProbes::arm();
        app.update(cx, |app, cx| {
            hold_session_saves(app);
            app.apply_restored_diff_mode(
                paneflow_config::schema::AppMode::Cli,
                Some(saved.clone()),
                &[],
                cx,
            );
            assert!(app.review_default_subject().is_some());
        });
        cx.update(|window, cx| app.update(cx, |app, cx| app.enter_diff_mode(window, cx)));
        app.update(cx, |app, cx| {
            assert_eq!(app.mode, paneflow_config::schema::AppMode::Diff);
            assert!(
                app.review.layout.is_none(),
                "no default grid while the saved one is held"
            );
            assert_eq!(app.build_session_state(cx).review_layout, Some(saved));
        });
    }

    /// Issue #1095: a branch pick is the way out of a hold that never
    /// resolves, such as a drive that stays unmounted. It replaces the held
    /// grid and says so.
    #[gpui::test]
    fn a_branch_pick_replaces_a_grid_that_stays_unconfirmed(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        let live = checkout(&root, "live");
        let slow = checkout(&root, "slow");
        let picked = checkout(&root, "picked");
        let repo = root.to_str().expect("utf8 temp path").to_string();
        let _stalled = StalledCheckouts::stall(&[std::path::Path::new(&slow)]);
        let saved = split(
            vec![diff_pane(&repo, &live), diff_pane(&repo, &slow)],
            vec![0.3, 0.7],
        );

        let cx = cx.add_empty_window();
        let app = cx.new(|cx| app_with_repo(&repo, cx));
        app.update(cx, |app, cx| {
            let _bootstrap = crate::diff::SuppressDiffBootstrap::arm();
            hold_session_saves(app);
            app.apply_restored_diff_mode(
                paneflow_config::schema::AppMode::Diff,
                Some(saved),
                &[],
                cx,
            );
            assert!(app.review.retained_layout.is_some());

            app.review_show_subject(picked_subject(&repo, &picked), cx);
            assert!(
                app.review.retained_layout.is_none(),
                "no stale node is left"
            );
            assert!(app.review.unconfirmed_checkout.is_none());
            assert_eq!(subject_paths(app, cx), vec![PathBuf::from(&picked)]);
            let toast = app.toast.as_ref().map(|toast| toast.message.clone());
            assert_eq!(
                toast,
                Some(format!("Saved Review grid replaced; {slow} did not answer"))
            );
            let Some(LayoutNode::Pane { surfaces }) = app.build_session_state(cx).review_layout
            else {
                unreachable!("the pick is the saved grid");
            };
            assert_eq!(surfaces[0].cwd.as_deref(), Some(picked.as_str()));
        });
    }

    /// Issue #1095: a pick retries the held grid first. A grid that now
    /// answers opens as saved, and the pick does not retarget one of its panes.
    #[gpui::test]
    fn a_branch_pick_restores_a_held_grid_that_now_answers(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        let live = checkout(&root, "live");
        let slow = checkout(&root, "slow");
        let picked = checkout(&root, "picked");
        let repo = root.to_str().expect("utf8 temp path").to_string();
        let stalled = StalledCheckouts::stall(&[std::path::Path::new(&slow)]);
        let saved = split(
            vec![diff_pane(&repo, &live), diff_pane(&repo, &slow)],
            vec![0.3, 0.7],
        );

        let cx = cx.add_empty_window();
        let app = cx.new(|cx| app_with_repo(&repo, cx));
        app.update(cx, |app, cx| {
            let _bootstrap = crate::diff::SuppressDiffBootstrap::arm();
            hold_session_saves(app);
            app.apply_restored_diff_mode(
                paneflow_config::schema::AppMode::Diff,
                Some(saved),
                &[],
                cx,
            );
            assert!(app.review.retained_layout.is_some());
            drop(stalled);

            app.review_show_subject(picked_subject(&repo, &picked), cx);
            assert!(app.review.retained_layout.is_none());
            assert!(app.toast.is_none(), "a restored grid replaces nothing");
            assert_eq!(
                subject_paths(app, cx),
                vec![PathBuf::from(&live), PathBuf::from(&slow)]
            );
        });
    }

    /// Only a `stat` that finished and found no directory prunes a pane.
    #[gpui::test]
    fn review_restore_prunes_a_confirmed_missing_checkout(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        let live = repo.join(".worktrees").join("live");
        let gone = repo.join(".worktrees").join("gone");
        std::fs::create_dir_all(&live).unwrap();
        let repo = repo.to_str().expect("utf8 temp path").to_string();
        let live = live.to_str().expect("utf8 temp path").to_string();
        let gone = gone.to_str().expect("utf8 temp path").to_string();
        let saved = split(
            vec![diff_pane(&repo, &live), diff_pane(&repo, &gone)],
            vec![0.3, 0.7],
        );

        let cx = cx.add_empty_window();
        let app = cx.new(|cx| app_with_repo(&repo, cx));
        app.update(cx, |app, cx| {
            let _bootstrap = crate::diff::SuppressDiffBootstrap::arm();
            app.apply_restored_diff_mode(
                paneflow_config::schema::AppMode::Diff,
                Some(saved),
                &[],
                cx,
            );
            assert!(app.review.retained_layout.is_none());
            let subjects = app.review_grid_subjects(cx);
            assert_eq!(subjects.len(), 1, "the missing checkout is pruned");
            assert_eq!(subjects[0].worktree.path, PathBuf::from(&live));
            assert_eq!(
                app.build_session_state(cx).review_layout,
                Some(diff_pane(&repo, &live))
            );
        });
    }

    /// Issue #933: a Review subject on a checkout bound to a tab, not to
    /// either workspace root, must come back with that tab's workspace.
    /// Two workspaces share the repository; only the second binds the path.
    #[gpui::test]
    fn restored_review_subject_on_a_bound_tab_keeps_its_workspace(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        let checkout = repo.join(".worktrees").join("feature");
        std::fs::create_dir_all(&checkout).unwrap();
        let repo = repo.to_str().expect("utf8 temp path").to_string();
        let checkout = checkout.to_str().expect("utf8 temp path").to_string();

        let cx = cx.add_empty_window();
        let app = cx.new(blank_paneflow_app);
        app.update(cx, |app, cx| {
            let mut first =
                crate::workspace::Workspace::empty_with_cwd_and_id(1, "one", PathBuf::from(&repo));
            let mut second =
                crate::workspace::Workspace::empty_with_cwd_and_id(2, "two", PathBuf::from(&repo));
            let shared_root = PathBuf::from(&repo);
            first.repo_root = Some(shared_root.clone());
            first.worktree_root = shared_root.clone();
            second.repo_root = Some(shared_root.clone());
            second.worktree_root = shared_root;
            assert!(
                first.tabs().iter().all(|tab| tab.worktree.is_none()),
                "only the second workspace binds the checkout"
            );
            second
                .tabs_mut()
                .next()
                .expect("a workspace keeps one tab")
                .worktree = Some(PathBuf::from(&checkout));
            let second_id = second.id;
            app.workspaces = vec![first, second];

            let _bootstrap = crate::diff::SuppressDiffBootstrap::arm();
            app.restore_review_layout(&diff_pane(&repo, &checkout), cx);

            let subjects = app.review_grid_subjects(cx);
            assert_eq!(subjects.len(), 1, "the bound checkout must survive restore");
            assert_eq!(subjects[0].worktree.path, PathBuf::from(&checkout));
            assert_eq!(subjects[0].worktree.workspace_id, Some(second_id));
        });
    }
}
