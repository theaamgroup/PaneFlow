//! Layout presets, zoom, and split-equalize.
//!
//! Part of the US-023 workspace_ops decomposition.

use gpui::{Context, Entity, Focusable, Window};

use crate::layout::{LayoutTree, SplitDirection};
use crate::pane::Pane;
use crate::{
    LayoutEvenHorizontal, LayoutEvenVertical, LayoutMainVertical, LayoutTiled, PaneFlowApp,
    SplitEqualize, ToggleZoom,
};

impl PaneFlowApp {
    pub(crate) fn handle_toggle_zoom(
        &mut self,
        _: &ToggleZoom,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if matches!(self.mode, paneflow_config::schema::AppMode::Diff) {
            self.review_toggle_zoom(window, cx);
            return;
        }
        let Some(ws) = self.active_workspace_mut() else {
            return;
        };

        if ws.is_zoomed() {
            if let Some(pane) = ws.exit_zoom(cx) {
                pane.read(cx).focus_handle(cx).focus(window, cx);
            }
        } else {
            // Zoom: save the full tree, replace root with the focused pane
            let Some(root) = &ws.active_tab().root else {
                return;
            };

            if root.leaf_count() <= 1 {
                return;
            }

            let Some(focused) = root.focused_pane(window, cx) else {
                return;
            };

            focused.update(cx, |p, _| p.zoomed = true);
            let full_tree = ws.active_tab_mut().root.take().unwrap();
            ws.active_tab_mut().saved_layout = Some(full_tree);
            ws.active_tab_mut().root = Some(LayoutTree::Leaf(focused.clone()));
            focused.read(cx).focus_handle(cx).focus(window, cx);
        }
        self.save_session(cx);
        cx.notify();
    }

    /// Apply a layout preset: collect all panes, rebuild tree with the given factory.
    fn apply_layout_preset(
        &mut self,
        build: impl FnOnce(Vec<Entity<Pane>>) -> Option<LayoutTree>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.exit_nav_zoom(cx);

        let Some(root) = self.take_nav_root() else {
            return;
        };
        let panes = root.collect_leaves();

        // No-op for single pane
        if panes.len() <= 1 {
            self.put_nav_root(Some(root));
            return;
        }

        // Rebuild tree and focus first pane
        // (root is consumed by collect_leaves moving entities out - but collect_leaves
        //  clones Entity refs, so root is still valid. We drop it explicitly.)
        drop(root);
        self.put_nav_root(build(panes));
        if let Some(r) = self.nav_root() {
            r.focus_first(window, cx);
        }
        self.save_session(cx);
        cx.notify();
    }

    pub(crate) fn handle_layout_even_h(
        &mut self,
        _: &LayoutEvenHorizontal,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.apply_layout_preset(
            |panes| LayoutTree::from_panes_equal(SplitDirection::Vertical, panes),
            window,
            cx,
        );
    }

    pub(crate) fn handle_layout_even_v(
        &mut self,
        _: &LayoutEvenVertical,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.apply_layout_preset(
            |panes| LayoutTree::from_panes_equal(SplitDirection::Horizontal, panes),
            window,
            cx,
        );
    }

    pub(crate) fn handle_layout_main_v(
        &mut self,
        _: &LayoutMainVertical,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.exit_nav_zoom(cx);

        let Some(root) = self.nav_root() else {
            return;
        };

        if root.leaf_count() <= 1 {
            return;
        }

        // The main pane is the focused one, or the first leaf
        let Some(main_pane) = root.focused_pane(window, cx).or_else(|| root.first_leaf()) else {
            return;
        };

        let panes = root.collect_leaves();
        let others: Vec<_> = panes.into_iter().filter(|p| *p != main_pane).collect();

        drop(self.take_nav_root());
        self.put_nav_root(LayoutTree::main_vertical(main_pane.clone(), others));
        main_pane.read(cx).focus_handle(cx).focus(window, cx);
        self.save_session(cx);
        cx.notify();
    }

    pub(crate) fn handle_layout_tiled(
        &mut self,
        _: &LayoutTiled,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.apply_layout_preset(LayoutTree::tiled, window, cx);
    }

    pub(crate) fn handle_split_equalize(
        &mut self,
        _: &SplitEqualize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(root) = self.nav_root() {
            root.equalize_ratios();
            self.save_session(cx);
            cx.notify();
        }
    }
}
