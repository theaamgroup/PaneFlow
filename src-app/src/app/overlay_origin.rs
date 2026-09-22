//! Per-overlay focus origin (issue #584).
//!
//! Every overlay that takes the focus (broadcast picker, Pane Overview, the
//! pane palette) records the pane it was opened from,
//! keyed by the overlay, so that:
//!
//! - its own close hands focus back to that pane, not to the first leaf;
//! - an inner overlay closed over an outer one (broadcast picker from pane B,
//!   then Pane Overview, then Escape) takes only its own entry, so the outer
//!   overlay's origin survives for its close or for a command palette fold;
//! - the command palette, which folds every open overlay before it captures
//!   its own return pane (#523), reads the outermost origin: the pane the
//!   user was in before any overlay opened.
//!
//! `PaneFlowApp` cannot be built in a unit test, so the stack itself and the
//! focus step are free of it and tested here; the wiring is pinned by source
//! assertions like the #523 guard in `command_palette.rs`.

use gpui::{App, Entity, Focusable as _, WeakEntity, Window};

use crate::PaneFlowApp;
use crate::layout::LayoutTree;
use crate::pane::Pane;

/// The overlays that take the focus and remember where it came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OverlayKind {
    BroadcastPicker,
    PaneOverview,
    PanePalette,
}

/// Insertion-ordered origins, outermost first. One entry per overlay kind:
/// re-opening an overlay replaces its own entry and nothing else. Weak, so
/// an overlay never keeps a pane closed underneath it alive; every read
/// upgrades, and the callers re-check tree membership.
#[derive(Default)]
pub(crate) struct OverlayOrigins {
    stack: Vec<(OverlayKind, WeakEntity<Pane>)>,
}

impl OverlayOrigins {
    /// Record `pane` as the origin of `kind`. `None` records nothing (the
    /// overlay was opened from the sidebar, say) but still drops a previous
    /// entry for the same kind, so a stale pane is never reused.
    pub(crate) fn remember(&mut self, kind: OverlayKind, pane: Option<WeakEntity<Pane>>) {
        self.stack.retain(|(k, _)| *k != kind);
        if let Some(pane) = pane {
            self.stack.push((kind, pane));
        }
    }

    /// Remove and return `kind`'s origin, upgraded. `None` when `kind` never
    /// recorded one or the pane entity is gone.
    pub(crate) fn take(&mut self, kind: OverlayKind) -> Option<Entity<Pane>> {
        let at = self.stack.iter().position(|(k, _)| *k == kind)?;
        self.stack.remove(at).1.upgrade()
    }

    /// The most recently recorded origin, for an overlay opening over
    /// another: the inner overlay inherits the pane the outer one came from,
    /// because the outer overlay, not a pane, owns the focus at that moment.
    pub(crate) fn innermost(&self) -> Option<WeakEntity<Pane>> {
        self.stack.last().map(|(_, pane)| pane.clone())
    }

    /// The first recorded origin among the kinds `open` admits, upgraded:
    /// the pane the user was in before any of those overlays opened.
    pub(crate) fn outermost(
        &self,
        mut open: impl FnMut(OverlayKind) -> bool,
    ) -> Option<Entity<Pane>> {
        self.stack
            .iter()
            .find(|(kind, _)| open(*kind))
            .and_then(|(_, pane)| pane.upgrade())
    }

    /// Whether `kind` recorded an origin at all: a pane owned the focus when
    /// it opened. `false` when the focus sat outside every pane (the sidebar,
    /// the dock editor, the placeholder), which records nothing.
    pub(crate) fn recorded(&self, kind: OverlayKind) -> bool {
        self.stack.iter().any(|(k, _)| *k == kind)
    }

    /// Drop every entry whose overlay is no longer open, so a close path that
    /// bypassed `take` cannot leave a stale origin at the bottom.
    pub(crate) fn retain_open(&mut self, mut open: impl FnMut(OverlayKind) -> bool) {
        self.stack.retain(|(kind, _)| open(*kind));
    }

    #[cfg(test)]
    fn kinds(&self) -> Vec<OverlayKind> {
        self.stack.iter().map(|(kind, _)| *kind).collect()
    }
}

/// Focus `origin` when it is still a leaf of `root`. `false` when there is
/// no origin, it left the tree, or the tree is gone, so the caller falls
/// back to the first leaf.
pub(crate) fn focus_origin_leaf(
    origin: Option<Entity<Pane>>,
    root: Option<&LayoutTree>,
    window: &mut Window,
    cx: &mut App,
) -> bool {
    match (origin, root) {
        (Some(pane), Some(root)) if root.contains_leaf(&pane) => {
            pane.read(cx).focus_handle(cx).focus(window, cx);
            true
        }
        _ => false,
    }
}

impl PaneFlowApp {
    /// Whether `kind`'s overlay is open right now.
    pub(crate) fn overlay_is_open(&self, kind: OverlayKind) -> bool {
        match kind {
            OverlayKind::BroadcastPicker => self.broadcast_picker_open,
            OverlayKind::PaneOverview => self.pane_overview.is_some(),
            OverlayKind::PanePalette => self.pane_palette.is_some(),
        }
    }

    /// The origins whose overlays are still open. `PaneFlowApp` stays
    /// borrowed immutably here; the callers apply the result to the stack.
    fn open_overlay_kinds(&self) -> Vec<OverlayKind> {
        [
            OverlayKind::BroadcastPicker,
            OverlayKind::PaneOverview,
            OverlayKind::PanePalette,
        ]
        .into_iter()
        .filter(|kind| self.overlay_is_open(*kind))
        .collect()
    }

    /// Record the pane `kind` is being opened from. Called BEFORE the overlay
    /// takes the focus, while the pane still owns it. When no pane owns the
    /// focus because another overlay does, the new entry inherits that
    /// overlay's origin, so each open overlay carries the pane the user was
    /// in before the first of them opened.
    pub(crate) fn remember_overlay_origin(&mut self, kind: OverlayKind, window: &Window, cx: &App) {
        let open = self.open_overlay_kinds();
        self.overlay_origins.retain_open(|k| open.contains(&k));
        let pane = self
            .pane_owning_focus(window, cx)
            .map(|pane| pane.downgrade())
            .or_else(|| self.overlay_origins.innermost());
        self.overlay_origins.remember(kind, pane);
    }

    /// Record `pane` itself as `kind`'s origin, for an open path that has no
    /// `Window` to resolve the focused pane with (the split pane palette,
    /// whose target pane is the origin by construction).
    pub(crate) fn remember_overlay_origin_pane(&mut self, kind: OverlayKind, pane: &Entity<Pane>) {
        self.overlay_origins.remember(kind, Some(pane.downgrade()));
    }

    /// Drop `kind`'s origin without touching the focus: the close is landing
    /// the focus somewhere else on purpose (a teleport, a freshly split pane,
    /// a command palette fold).
    pub(crate) fn forget_overlay_origin(&mut self, kind: OverlayKind) {
        let _ = self.overlay_origins.take(kind);
    }

    /// Whether a pane owned the focus when `kind` opened (see
    /// [`OverlayOrigins::recorded`]).
    pub(crate) fn overlay_origin_recorded(&self, kind: OverlayKind) -> bool {
        self.overlay_origins.recorded(kind)
    }

    /// Take `kind`'s origin and return it only while it is still a leaf of
    /// the tree the focus would return to (the active workspace's visible tab,
    /// or the Review grid), so a pane closed underneath the overlay is never
    /// re-focused.
    pub(crate) fn take_live_overlay_origin(&mut self, kind: OverlayKind) -> Option<Entity<Pane>> {
        self.overlay_origins
            .take(kind)
            .filter(|pane| self.command_palette_pane_is_live(pane))
    }

    /// The outermost origin among the overlays that are open: what the
    /// command palette returns a dispatched action to after folding them.
    pub(crate) fn outermost_open_overlay_origin(&self) -> Option<Entity<Pane>> {
        self.overlay_origins
            .outermost(|kind| self.overlay_is_open(kind))
    }

    /// Close path for an overlay closing on its own (Escape, an outside
    /// click, a committed choice): hand the focus back to the pane `kind`
    /// was opened from, or to the first pane when that pane is gone, or to
    /// the empty-workspace placeholder when the workspace has none (issue
    /// #108: an overlay that closes with nothing focused leaves every global
    /// chord without a handler). The caller has already cleared the
    /// overlay's own state.
    pub(crate) fn restore_overlay_origin_focus(
        &mut self,
        kind: OverlayKind,
        window: &mut Window,
        cx: &mut App,
    ) {
        let origin = self.take_live_overlay_origin(kind);
        let root = self.focus_return_root();
        if !focus_origin_leaf(origin, root, window, cx) {
            self.focus_first_leaf_or_placeholder(window, cx);
        }
    }

    /// The tree a closing overlay returns the focus to.
    fn focus_return_root(&self) -> Option<&LayoutTree> {
        if self.mode == paneflow_config::schema::AppMode::Diff {
            self.review.layout.as_ref()
        } else {
            self.workspaces
                .get(self.active_idx)?
                .active_tab()
                .root
                .as_ref()
        }
    }

    /// The first pane of the tree the focus returns to, then the
    /// empty-workspace placeholder. Review mode has its own grid: its first
    /// live diff pane is the fallback there, never the CLI workspace's pane
    /// behind it.
    pub(crate) fn focus_first_leaf_or_placeholder(&mut self, window: &mut Window, cx: &mut App) {
        let focused = if self.mode == paneflow_config::schema::AppMode::Diff {
            match self
                .review
                .layout
                .as_ref()
                .and_then(|root| root.first_leaf())
            {
                Some(pane) => {
                    pane.read(cx).focus_handle(cx).focus(window, cx);
                    true
                }
                None => false,
            }
        } else {
            match self.workspaces.get(self.active_idx) {
                Some(ws) => ws.focus_first(window, cx),
                None => false,
            }
        };
        if !focused {
            window.focus(&self.empty_workspace_focus, cx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::AppContext as _;

    fn make_pane(cx: &mut gpui::VisualTestContext) -> Entity<Pane> {
        let terminal = cx.new(|cx| crate::terminal::TerminalView::display_only_for_test(1, cx));
        cx.new(|cx| Pane::new(terminal, 1, cx))
    }

    /// Issue #584 case 1: an outer overlay from pane B, then Pane Overview
    /// over it, then Escape on Pane Overview. The inner close takes only its
    /// own entry; the outer overlay's origin, and the palette's outermost
    /// read, still name B.
    #[gpui::test]
    fn an_inner_close_leaves_the_outer_origin_in_place(cx: &mut gpui::TestAppContext) {
        let cx = cx.add_empty_window();
        let b = make_pane(cx);
        let mut origins = OverlayOrigins::default();

        origins.remember(OverlayKind::BroadcastPicker, Some(b.downgrade()));
        // Pane Overview opens while the broadcast picker owns the focus: no
        // pane does, so it inherits the innermost origin.
        let inherited = origins.innermost();
        origins.remember(OverlayKind::PaneOverview, inherited);
        assert_eq!(
            origins.kinds(),
            vec![OverlayKind::BroadcastPicker, OverlayKind::PaneOverview]
        );

        assert_eq!(origins.take(OverlayKind::PaneOverview), Some(b.clone()));
        assert_eq!(
            origins.kinds(),
            vec![OverlayKind::BroadcastPicker],
            "Escape on Pane Overview must not clear the broadcast picker's origin"
        );
        assert_eq!(
            origins.outermost(|_| true),
            Some(b.clone()),
            "a palette command after that still targets pane B"
        );
        assert_eq!(origins.take(OverlayKind::BroadcastPicker), Some(b));
        assert!(origins.kinds().is_empty());
        assert_eq!(origins.take(OverlayKind::BroadcastPicker), None);
    }

    #[gpui::test]
    fn the_outermost_origin_is_the_pane_before_any_overlay(cx: &mut gpui::TestAppContext) {
        let cx = cx.add_empty_window();
        let a = make_pane(cx);
        let b = make_pane(cx);
        let mut origins = OverlayOrigins::default();
        origins.remember(OverlayKind::BroadcastPicker, Some(b.downgrade()));
        origins.remember(OverlayKind::PanePalette, Some(a.downgrade()));
        assert_eq!(origins.outermost(|_| true), Some(b.clone()));
        // Only open overlays count: a stale bottom entry never wins.
        assert_eq!(
            origins.outermost(|kind| kind == OverlayKind::PanePalette),
            Some(a.clone())
        );
        origins.retain_open(|kind| kind == OverlayKind::PanePalette);
        assert_eq!(origins.kinds(), vec![OverlayKind::PanePalette]);
        // Re-opening replaces the kind's own entry and nothing else.
        origins.remember(OverlayKind::PanePalette, Some(b.downgrade()));
        assert_eq!(origins.kinds(), vec![OverlayKind::PanePalette]);
        assert_eq!(origins.take(OverlayKind::PanePalette), Some(b));
        // `None` records nothing but still forgets the previous entry.
        origins.remember(OverlayKind::PanePalette, Some(a.downgrade()));
        origins.remember(OverlayKind::PanePalette, None);
        assert!(origins.kinds().is_empty());
    }

    #[gpui::test]
    fn a_dropped_origin_pane_upgrades_to_none(cx: &mut gpui::TestAppContext) {
        let cx = cx.add_empty_window();
        let mut origins = OverlayOrigins::default();
        {
            let gone = make_pane(cx);
            origins.remember(OverlayKind::PaneOverview, Some(gone.downgrade()));
        }
        // The only strong handle is out of scope; the weak one is dead.
        assert_eq!(origins.take(OverlayKind::PaneOverview), None);
        assert!(origins.kinds().is_empty());
    }

    /// The focus step: the origin gets the focus while it is a leaf of the
    /// tree; a pane that left the tree, or no origin at all, reports `false`
    /// so the caller falls back to the first leaf.
    #[gpui::test]
    fn focus_origin_leaf_focuses_only_a_live_leaf(cx: &mut gpui::TestAppContext) {
        let cx = cx.add_empty_window();
        let a = make_pane(cx);
        let b = make_pane(cx);
        let closed = make_pane(cx);
        let root = LayoutTree::from_panes_equal(
            crate::layout::SplitDirection::Vertical,
            vec![a.clone(), b.clone()],
        )
        .expect("two panes make a container");

        cx.update(|window, cx| {
            assert!(focus_origin_leaf(Some(b.clone()), Some(&root), window, cx));
            assert!(b.read(cx).focus_handle(cx).is_focused(window));
            assert!(!a.read(cx).focus_handle(cx).is_focused(window));

            assert!(
                !focus_origin_leaf(Some(closed.clone()), Some(&root), window, cx),
                "a pane that is not a leaf of the tree is never re-focused"
            );
            assert!(!focus_origin_leaf(None, Some(&root), window, cx));
            assert!(!focus_origin_leaf(Some(b.clone()), None, window, cx));
            assert!(
                b.read(cx).focus_handle(cx).is_focused(window),
                "a refused restore leaves the focus where it was"
            );
        });
    }

    /// The wiring: every overlay records its origin under its own kind
    /// before taking the focus, and its Escape path restores through that
    /// kind. Source-text assertions, the #523 precedent.
    #[test]
    fn every_overlay_remembers_and_restores_its_own_origin() {
        // Split at the trailing test module, not the first `#[cfg(test)]`:
        let production = |src: &'static str| -> &'static str {
            src.split("\n#[cfg(test)]\nmod ")
                .next()
                .expect("production half of the module")
        };
        // `main.rs` carries several test modules; the render root sits
        // between them, so it is read whole.
        let main = include_str!("../main.rs");
        for (module, src, kind, escape) in [
            (
                "broadcast.rs",
                production(include_str!("broadcast.rs")),
                "OverlayKind::BroadcastPicker",
                "self.close_broadcast_picker_and_restore_focus(window, cx);",
            ),
            (
                "pane_overview/mod.rs",
                production(include_str!("pane_overview/mod.rs")),
                "OverlayKind::PaneOverview",
                "self.close_pane_overview_and_restore_focus(window, cx);",
            ),
        ] {
            let remember = format!("self.remember_overlay_origin({kind}, window, cx);");
            assert!(
                src.contains(&remember) || main.contains(&remember),
                "{module} must record its origin under {kind} before taking the focus"
            );
            assert!(
                src.contains(&format!(
                    "self.restore_overlay_origin_focus({kind}, window, cx);"
                )),
                "{module} must restore the focus through its own origin"
            );
            assert!(
                src.contains(escape),
                "{module}: Escape must restore: `{escape}`"
            );
        }
        // The pane palette records both open paths and forgets on every take.
        let pane_palette = production(include_str!("pane_palette.rs"));
        assert!(
            pane_palette
                .contains("self.remember_overlay_origin(OverlayKind::PanePalette, window, cx);")
        );
        assert!(
            pane_palette
                .contains("self.remember_overlay_origin_pane(OverlayKind::PanePalette, &target);")
        );
        assert_eq!(
            pane_palette
                .matches("self.forget_overlay_origin(OverlayKind::PanePalette);")
                .count()
                + pane_palette
                    .matches(".take_live_overlay_origin(OverlayKind::PanePalette)")
                    .count(),
            pane_palette.matches("self.pane_palette = None;").count()
                + pane_palette.matches("self.pane_palette.take()").count(),
            "every path that drops the pane palette must forget or take its origin"
        );
        // `close_pane_palette` validates the saved handle through the origin:
        // recorded before the tab close, taken live after it.
        let close = &pane_palette[pane_palette
            .find("fn close_pane_palette(")
            .expect("close_pane_palette exists")..];
        let recorded = close
            .find("let origin_recorded = self.overlay_origin_recorded(OverlayKind::PanePalette);")
            .expect("close_pane_palette reads whether a pane origin was recorded");
        let closed = close
            .find("self.close_workspace_tab(ws_idx, tab_idx, window, cx);")
            .expect("close_pane_palette closes the picker tab");
        let live = close
            .find(".take_live_overlay_origin(OverlayKind::PanePalette)")
            .expect("close_pane_palette takes the origin live");
        assert!(recorded < closed && closed < live);
        assert!(
            close.contains("&& (origin_live || !origin_recorded)"),
            "a saved handle inside a pane that left the tree must not be re-focused"
        );
    }
}
