use std::collections::HashSet;
use std::path::PathBuf;

use gpui::{AppContext, Context, Entity, Pixels, Point, WeakEntity};

use crate::diff::ReviewSubject;
use crate::layout::LayoutTree;
use crate::pane::Pane;
use crate::widgets::text_input::TextInput;

mod agent;
mod events;
mod grid;
mod menu;
mod mode;
mod rail;
mod session;

pub(crate) use session::surface_for_subject;

pub(crate) const MAX_REVIEW_PANES: usize = 6;
pub(crate) const REVIEW_WORKSPACES_RAIL_WIDTH: f32 = 220.;
pub(crate) const REVIEW_CHANGES_RAIL_WIDTH: f32 = crate::app::constants::SIDEBAR_WIDTH;

#[derive(Clone)]
pub(crate) struct ReviewRailMenu {
    pub(crate) subject: ReviewSubject,
    pub(crate) position: Point<Pixels>,
}

pub(crate) struct ReviewState {
    pub(crate) layout: Option<LayoutTree>,
    pub(crate) saved_layout: Option<LayoutTree>,
    /// Issue #475: weak, like every other transient pane reference in the
    /// app (`PendingClose`, the pane palette, the composer, the launch pad).
    /// The read chokepoint `review_active_pane` already treats this as a
    /// reference - it filters by `review_contains_pane` before handing the
    /// pane out - so nothing here should keep a `DiffView` and its watchers
    /// alive after the layout that held it went away.
    pub(crate) active_pane: Option<WeakEntity<Pane>>,
    pub(crate) collapsed: HashSet<PathBuf>,
    pub(crate) rail_menu: Option<ReviewRailMenu>,
    pub(crate) base_picker_open: bool,
    pub(crate) base_filter: Entity<TextInput>,
    pub(crate) selected_file: Option<String>,
    pub(crate) files_tree: bool,
    pub(crate) collapsed_dirs: HashSet<String>,
    pub(crate) file_filter: Entity<TextInput>,
}

impl ReviewState {
    pub(crate) fn new<T: 'static>(cx: &mut Context<T>) -> Self {
        let file_filter = cx.new(|cx| TextInput::new("", "Filter files…", cx));
        cx.observe(&file_filter, |_, _, cx| cx.notify()).detach();
        let base_filter = cx.new(|cx| TextInput::new("", "Base branch or ref", cx));
        cx.observe(&base_filter, |_, _, cx| cx.notify()).detach();
        Self {
            layout: None,
            saved_layout: None,
            active_pane: None,
            collapsed: HashSet::new(),
            rail_menu: None,
            base_picker_open: false,
            base_filter,
            selected_file: None,
            files_tree: false,
            collapsed_dirs: HashSet::new(),
            file_filter,
        }
    }

    pub(crate) fn full_layout(&self) -> Option<&LayoutTree> {
        self.saved_layout.as_ref().or(self.layout.as_ref())
    }

    /// A rail selection must reveal an already-open pane even when another
    /// pane is zoomed. Restore the parked grid before asking the app to focus.
    pub(crate) fn reveal_pane(&mut self, pane: &Entity<Pane>, cx: &mut gpui::App) {
        if self
            .layout
            .as_ref()
            .is_some_and(|root| root.contains_leaf(pane))
        {
            return;
        }
        if self
            .saved_layout
            .as_ref()
            .is_some_and(|root| root.contains_leaf(pane))
        {
            if let Some(zoomed) = self.layout.as_ref().and_then(LayoutTree::first_leaf) {
                zoomed.update(cx, |pane, cx| {
                    pane.zoomed = false;
                    cx.notify();
                });
            }
            self.layout = self.saved_layout.take();
        }
    }

    pub(crate) fn can_add_pane(&self) -> bool {
        self.full_layout()
            .is_none_or(|root| root.leaf_count() < MAX_REVIEW_PANES)
    }

    pub(crate) fn is_zoomed(&self) -> bool {
        self.saved_layout.is_some()
    }

    pub(crate) fn dismiss_popovers(&mut self) {
        self.rail_menu = None;
        self.base_picker_open = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::{DiffView, DiffWorktree};
    use crate::layout::SplitDirection;
    use crate::pane::PaneSurface;

    #[gpui::test]
    fn review_cap_counts_saved_grid_while_zoomed(cx: &mut gpui::TestAppContext) {
        let state = cx.new(ReviewState::new);
        state.update(cx, |state, cx| {
            assert!(state.can_add_pane());
            let mut panes = Vec::new();
            for index in 0..MAX_REVIEW_PANES {
                let view = cx.new(|cx| {
                    DiffView::for_test(
                        ReviewSubject {
                            repo_root: PathBuf::from("/repo"),
                            worktree: DiffWorktree {
                                path: PathBuf::from(format!("/checkout-{index}")),
                                branch: "feature".into(),
                                workspace_id: Some(11),
                            },
                        },
                        cx,
                    )
                });
                panes.push(cx.new(|cx| Pane::new_with_surface(PaneSurface::Diff(view), 11, cx)));
                state.layout =
                    LayoutTree::from_panes_equal(SplitDirection::Vertical, panes.clone());
                assert_eq!(state.can_add_pane(), index + 1 < MAX_REVIEW_PANES);
            }
            state.saved_layout = state.layout.take();
            state.layout = Some(LayoutTree::Leaf(panes[0].clone()));
            assert!(
                !state.can_add_pane(),
                "zoom must not bypass the six-pane cap"
            );
            assert_eq!(
                state
                    .full_layout()
                    .unwrap()
                    .serialize_without_scrollback(cx)
                    .leaf_count(),
                6
            );
            panes[0].update(cx, |pane, _| pane.zoomed = true);
            state.reveal_pane(&panes[1], cx);
            assert!(state.saved_layout.is_none());
            assert_eq!(state.layout.as_ref().unwrap().leaf_count(), 6);
            assert!(state.layout.as_ref().unwrap().contains_leaf(&panes[1]));
            assert!(!panes[0].read(cx).zoomed);
        });
    }
}
