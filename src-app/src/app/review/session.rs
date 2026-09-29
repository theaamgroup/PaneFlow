use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;

use gpui::{App, Context};
use paneflow_config::schema::{LayoutNode, SurfaceDefinition};

use super::MAX_REVIEW_PANES;
use crate::PaneFlowApp;
use crate::app::session::{RESTORED_CWD_PROBE_TIMEOUT, persisted_dir_is_live_within};
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

    pub(crate) fn restore_review_layout(&mut self, node: &LayoutNode, cx: &mut Context<Self>) {
        let open_roots: HashSet<PathBuf> = self
            .workspaces
            .iter()
            .filter_map(|ws| ws.repo_root.clone())
            .collect();
        let keep = |surface: &SurfaceDefinition| {
            subject_from_surface(surface).is_some_and(|subject| {
                open_roots.contains(&subject.repo_root)
                    && persisted_dir_is_live_within(
                        &subject.worktree.path,
                        RESTORED_CWD_PROBE_TIMEOUT,
                    )
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
