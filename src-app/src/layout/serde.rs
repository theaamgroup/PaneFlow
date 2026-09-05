//! Conversion between `LayoutTree` and `LayoutNode` (the session-persistence
//! schema). `serialize` captures each leaf's tabs + CWD + scrollback, while
//! `serialize_without_scrollback` keeps terminal output process-local. The
//! reverse `from_layout_node` consumes a pane deque and calls `spawn` for any
//! leaves beyond what was handed in.

use std::cell::Cell;
use std::collections::VecDeque;
use std::rc::Rc;

use gpui::{App, Entity};
use paneflow_config::schema::{LayoutNode, SurfaceDefinition};

use crate::pane::Pane;

use super::tree::{LayoutChild, LayoutTree, SplitDirection};

#[derive(Clone, Copy)]
enum ScrollbackCapture {
    /// History extract, bounded to the newest `max_lines` grid rows. IPC
    /// `workspace.current` and session persistence use [`Self::Omit`] so the
    /// GPUI tick does not walk 4000 lines per pane (issue #29); the undo-close
    /// record cannot omit it, so it lowers the bound instead.
    Inline {
        max_lines: usize,
    },
    Omit,
}

impl LayoutTree {
    /// Serialize the layout tree to a `LayoutNode` (config schema type).
    ///
    /// Each leaf produces a `LayoutNode::Pane` with one `SurfaceDefinition` for
    /// its single surface, capturing the terminal's CWD and OSC title. That
    /// surface is marked with `focus: true`. Legacy multi-surface leaves are
    /// demoted to one surface per pane by the v1→v2 session migration. Each
    /// container produces a `LayoutNode::Split` with per-child `ratios` and
    /// recursive children.
    ///
    /// Not used by `workspace.current` (issue #29); the inline extract is the
    /// snapshot-with-scrollback path, taken by the undo-close-tab record.
    pub fn serialize(&self, cx: &App) -> LayoutNode {
        self.serialize_with(
            cx,
            ScrollbackCapture::Inline {
                max_lines: crate::limits::MAX_SCROLLBACK_EXTRACT_LINES,
            },
            None,
        )
    }

    /// Serialize undo metadata while sharing one byte budget across every leaf
    /// and, when the caller reuses `remaining_bytes`, across multiple tabs.
    /// Once exhausted, later terminal leaves skip the history lock entirely.
    pub fn serialize_with_scrollback_budget(
        &self,
        cx: &App,
        max_lines: usize,
        remaining_bytes: &Cell<usize>,
    ) -> LayoutNode {
        self.serialize_with(
            cx,
            ScrollbackCapture::Inline { max_lines },
            Some(remaining_bytes),
        )
    }

    /// Serialize session metadata without carrying terminal output into the
    /// next process. The schema field remains present as `None` for backward
    /// compatibility with existing session files.
    pub fn serialize_without_scrollback(&self, cx: &App) -> LayoutNode {
        self.serialize_with(cx, ScrollbackCapture::Omit, None)
    }

    /// Inner serializer parametrised by the [`ScrollbackCapture`] strategy.
    fn serialize_with(
        &self,
        cx: &App,
        capture: ScrollbackCapture,
        remaining_bytes: Option<&Cell<usize>>,
    ) -> LayoutNode {
        match self {
            LayoutTree::Leaf(pane) => {
                let pane_ref = pane.read(cx);
                // EP-002 US-004: a pane holds exactly one surface, so the
                // serialized list has at most one entry and it is always the
                // focused one.
                let surfaces: Vec<SurfaceDefinition> = std::iter::once(&pane_ref.surface)
                    .map(|tab| match tab {
                        crate::pane::PaneSurface::Terminal(tv) => {
                            let tv_ref = tv.read(cx);
                            let name =
                                crate::sidebar_title::clean_sidebar_title(&tv_ref.terminal.title);
                            let cwd = tv_ref.terminal.current_cwd.clone().or_else(|| {
                                tv_ref.terminal.cwd_now().map(|p| p.display().to_string())
                            });
                            let scrollback = match capture {
                                ScrollbackCapture::Inline { max_lines } => {
                                    if remaining_bytes.is_some_and(|remaining| remaining.get() == 0)
                                    {
                                        None
                                    } else {
                                        let mut scrollback =
                                            tv_ref.terminal.extract_scrollback_capped(max_lines);
                                        if let (Some(remaining), Some(text)) =
                                            (remaining_bytes, scrollback.as_mut())
                                        {
                                            let budget = remaining.get();
                                            if text.len() > budget {
                                                let mut boundary = budget.min(text.len());
                                                while boundary > 0
                                                    && !text.is_char_boundary(boundary)
                                                {
                                                    boundary -= 1;
                                                }
                                                text.truncate(boundary);
                                            }
                                            remaining.set(budget.saturating_sub(text.len()));
                                            if text.is_empty() {
                                                scrollback = None;
                                            }
                                        }
                                        scrollback
                                    }
                                }
                                ScrollbackCapture::Omit => None,
                            };
                            SurfaceDefinition {
                                surface_type: Some("terminal".to_string()),
                                agent_context: Some(tv_ref.agent_context.clone()),
                                name,
                                custom_name: tv_ref.terminal.custom_name.clone(),
                                command: None,
                                prompt: None,
                                cwd,
                                path: None,
                                env: None,
                                focus: Some(true),
                                scrollback,
                                agent: tv_ref.terminal.detected_agent.map(|a| a.tag().to_string()),
                                font_size: tv_ref.terminal.font_size_override,
                            }
                        }
                        crate::pane::PaneSurface::Markdown(markdown) => {
                            let path = markdown.read(cx).path.display().to_string();
                            SurfaceDefinition {
                                surface_type: Some("markdown".to_string()),
                                agent_context: None,
                                name: None,
                                custom_name: None,
                                command: None,
                                prompt: None,
                                cwd: None,
                                path: Some(path),
                                env: None,
                                focus: Some(true),
                                scrollback: None,
                                agent: None,
                                font_size: None,
                            }
                        }
                        crate::pane::PaneSurface::Diff(_) => SurfaceDefinition {
                            surface_type: Some("diff".to_string()),
                            agent_context: None,
                            name: None,
                            custom_name: None,
                            command: None,
                            prompt: None,
                            cwd: None,
                            path: None,
                            env: None,
                            focus: None,
                            scrollback: None,
                            agent: None,
                            font_size: None,
                        },
                    })
                    .filter(|surface| surface.surface_type.as_deref() != Some("diff"))
                    .collect();
                LayoutNode::Pane { surfaces }
            }
            LayoutTree::Container {
                direction,
                children,
                ..
            } => {
                let dir_str = match direction {
                    SplitDirection::Horizontal => "horizontal",
                    SplitDirection::Vertical => "vertical",
                };
                let ratios: Vec<f64> = children.iter().map(|c| c.ratio.get() as f64).collect();
                let mut child_nodes: Vec<LayoutNode> = Vec::with_capacity(children.len());
                for c in children.iter() {
                    child_nodes.push(c.node.serialize_with(cx, capture, remaining_bytes));
                }
                LayoutNode::Split {
                    direction: dir_str.to_string(),
                    ratio: None,
                    ratios: Some(ratios),
                    children: child_nodes,
                }
            }
        }
    }

    /// A zero-leaf tree used as a stand-in until [`Self::from_layout_node`]
    /// spawns every pane from a supplied layout.
    ///
    /// `workspace.create` with a layout must not pre-spawn a default terminal:
    /// this function reuses existing leaves left-to-right, and a dummy leaf 0
    /// would swallow that pane's cwd/env/tabs/`custom_name`.
    pub(crate) fn empty() -> Self {
        LayoutTree::Container {
            direction: SplitDirection::Vertical,
            children: Vec::new(),
            drag: Rc::new(Cell::new(None)),
            container_size: Rc::new(Cell::new(0.0)),
        }
    }

    /// Rebuild a `LayoutTree` from a `LayoutNode` (config schema).
    ///
    /// Panes are consumed from `panes` in left-to-right order for each leaf.
    /// When `panes` is exhausted, `spawn` is called with the current `LayoutNode`
    /// so the caller can extract per-surface metadata (e.g. CWD) for new panes.
    ///
    /// Reused leaves are returned as-is: `spawn` is not called, so cwd/env/tabs/
    /// `custom_name` on that node are ignored. Callers that need every leaf to
    /// honor surface metadata (notably `workspace.create` with a layout) must
    /// pass an empty deque.
    pub fn from_layout_node(
        node: &LayoutNode,
        panes: &mut VecDeque<Entity<Pane>>,
        spawn: &mut impl FnMut(&LayoutNode) -> Entity<Pane>,
    ) -> Self {
        match node {
            LayoutNode::Pane { .. } => {
                let pane = panes.pop_front().unwrap_or_else(|| spawn(node));
                LayoutTree::Leaf(pane)
            }
            LayoutNode::Split {
                direction,
                children,
                ..
            } => {
                let dir = match direction.as_str() {
                    "vertical" => SplitDirection::Vertical,
                    _ => SplitDirection::Horizontal,
                };
                let resolved = node.resolved_ratios();
                let child_trees: Vec<LayoutChild> = children
                    .iter()
                    .enumerate()
                    .map(|(i, child_node)| {
                        let ratio = resolved
                            .get(i)
                            .copied()
                            .unwrap_or(1.0 / children.len() as f64);
                        LayoutChild {
                            node: LayoutTree::from_layout_node(child_node, panes, spawn),
                            ratio: Rc::new(Cell::new(ratio as f32)),
                        }
                    })
                    .collect();
                LayoutTree::Container {
                    direction: dir,
                    children: child_trees,
                    drag: Rc::new(Cell::new(None)),
                    container_size: Rc::new(Cell::new(0.0)),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, VecDeque};

    use gpui::{AppContext, Entity, TestAppContext};
    use paneflow_config::schema::{LayoutNode, SurfaceDefinition};

    use crate::pane::Pane;
    use crate::terminal::TerminalView;
    use crate::workspace::{Tab, Workspace};

    use super::super::tree::{LayoutTree, SplitDirection};

    fn test_pane(cx: &mut impl AppContext) -> Entity<Pane> {
        let terminal = cx.new(|cx| TerminalView::display_only_for_test(1, cx));
        cx.new(|cx| Pane::new(terminal, 1, cx))
    }

    fn surface(custom_name: &str, cwd: &str) -> SurfaceDefinition {
        SurfaceDefinition {
            custom_name: Some(custom_name.to_string()),
            cwd: Some(cwd.to_string()),
            env: Some(HashMap::from([("LEAF".into(), custom_name.into())])),
            ..Default::default()
        }
    }

    fn two_pane_layout_with_first_pane_metadata() -> LayoutNode {
        LayoutNode::Split {
            direction: "vertical".to_string(),
            ratio: None,
            ratios: Some(vec![0.5, 0.5]),
            children: vec![
                LayoutNode::Pane {
                    surfaces: vec![surface("agent", "/tmp/agent"), surface("logs", "/tmp/logs")],
                },
                LayoutNode::Pane {
                    surfaces: vec![surface("right", "/tmp/right")],
                },
            ],
        }
    }

    fn leaf_ids(tree: &LayoutTree) -> Vec<gpui::EntityId> {
        tree.collect_leaves()
            .into_iter()
            .map(|pane| pane.entity_id())
            .collect()
    }

    #[test]
    fn empty_tree_has_zero_leaves() {
        assert_eq!(LayoutTree::empty().leaf_count(), 0);
        assert!(LayoutTree::empty().collect_leaves().is_empty());
    }

    #[gpui::test]
    fn from_layout_node_spawns_first_pane_surfaces_when_deque_is_empty(cx: &mut TestAppContext) {
        let cx = cx.add_empty_window();
        let layout = two_pane_layout_with_first_pane_metadata();
        let mut panes = VecDeque::new();
        let mut spawned: Vec<LayoutNode> = Vec::new();
        let tree = LayoutTree::from_layout_node(&layout, &mut panes, &mut |node| {
            spawned.push(node.clone());
            test_pane(cx)
        });

        assert_eq!(tree.leaf_count(), 2);
        assert_eq!(
            spawned.len(),
            2,
            "both leaves must spawn when nothing is reused"
        );
        match &spawned[0] {
            LayoutNode::Pane { surfaces } => {
                assert_eq!(surfaces.len(), 2, "leaf 0 keeps both surfaces");
                assert_eq!(surfaces[0].custom_name.as_deref(), Some("agent"));
                assert_eq!(surfaces[0].cwd.as_deref(), Some("/tmp/agent"));
                assert_eq!(
                    surfaces[0]
                        .env
                        .as_ref()
                        .and_then(|env| env.get("LEAF"))
                        .map(String::as_str),
                    Some("agent")
                );
                assert_eq!(surfaces[1].custom_name.as_deref(), Some("logs"));
            }
            LayoutNode::Split { .. } => panic!("leaf 0 spawn must receive the pane node"),
        }
        match &spawned[1] {
            LayoutNode::Pane { surfaces } => {
                assert_eq!(surfaces[0].custom_name.as_deref(), Some("right"));
            }
            LayoutNode::Split { .. } => panic!("leaf 1 spawn must receive the pane node"),
        }
    }

    #[gpui::test]
    fn from_layout_node_reuses_leftmost_leaf_without_spawn(cx: &mut TestAppContext) {
        let cx = cx.add_empty_window();
        let existing = test_pane(cx);
        let layout = two_pane_layout_with_first_pane_metadata();
        let mut panes = VecDeque::from([existing.clone()]);
        let mut spawned: Vec<LayoutNode> = Vec::new();
        let tree = LayoutTree::from_layout_node(&layout, &mut panes, &mut |node| {
            spawned.push(node.clone());
            test_pane(cx)
        });

        assert_eq!(tree.leaf_count(), 2);
        assert_eq!(
            leaf_ids(&tree)[0],
            existing.entity_id(),
            "a pre-spawned leaf 0 is reused; spawn never sees its surfaces"
        );
        assert_eq!(spawned.len(), 1, "only the extra leaf is spawned");
        match &spawned[0] {
            LayoutNode::Pane { surfaces } => {
                assert_eq!(surfaces[0].custom_name.as_deref(), Some("right"));
            }
            LayoutNode::Split { .. } => panic!("the leftover spawn is the second pane"),
        }
    }

    #[gpui::test]
    fn serialized_terminal_name_is_scrubbed_and_bounded(cx: &mut TestAppContext) {
        let cx = cx.add_empty_window();
        let terminal = cx.new(|cx| TerminalView::display_only_for_test(1, cx));
        let hostile = format!("\u{202e}\u{200b}{}", "x".repeat(400));
        terminal.update(cx, |view, _cx| view.terminal.title = hostile);
        let pane = cx.new(|cx| Pane::new(terminal, 1, cx));
        let tree = LayoutTree::Leaf(pane);

        let serialized = cx.update(|_, cx| tree.serialize_without_scrollback(cx));
        let LayoutNode::Pane { surfaces } = serialized else {
            panic!("a leaf must serialize as a pane")
        };
        let name = surfaces[0]
            .name
            .as_deref()
            .expect("meaningful title survives scrubbing");
        assert!(!name.contains('\u{202e}'));
        assert!(!name.contains('\u{200b}'));
        assert!(name.chars().count() <= 241, "{name}");
        assert!(name.ends_with('…'), "{name}");
    }

    fn pane_leaf_count(node: &LayoutNode) -> usize {
        match node {
            LayoutNode::Pane { .. } => 1,
            LayoutNode::Split { children, .. } => children.iter().map(pane_leaf_count).sum(),
        }
    }

    /// US-002 (prd-cli-tab-hierarchy): serialization emits one `LayoutNode` per
    /// tab, the zoomed tab contributes its *saved* (un-zoomed) arrangement, and
    /// the tree survives a round-trip through `from_layout_node`. Switching the
    /// visible tab must not disturb either tab's zoom state.
    #[gpui::test]
    fn two_tab_workspace_with_one_zoomed_round_trips(cx: &mut TestAppContext) {
        let cx = cx.add_empty_window();
        let (a, b, c, d) = (test_pane(cx), test_pane(cx), test_pane(cx), test_pane(cx));

        let flat = LayoutTree::new_split(
            SplitDirection::Vertical,
            LayoutTree::Leaf(a.clone()),
            LayoutTree::Leaf(b.clone()),
        );
        let mut ws = Workspace::with_layout_and_id(1, "ws", std::path::PathBuf::new(), flat);

        // Second tab, zoomed on `c`: root holds the single zoomed leaf while
        // `saved_layout` keeps the full arrangement.
        let mut zoomed = Tab::new("zoomed", Some(LayoutTree::Leaf(c.clone())));
        zoomed.saved_layout = Some(LayoutTree::new_split(
            SplitDirection::Horizontal,
            LayoutTree::Leaf(c.clone()),
            LayoutTree::Leaf(d.clone()),
        ));
        assert!(ws.open_tab(zoomed), "opening the second tab must succeed");
        assert_eq!(ws.active_tab_idx(), 1);
        assert!(ws.is_zoomed(), "the active tab is the zoomed one");

        // Per-tab serialization: the flat tab yields its two leaves, the zoomed
        // one yields the saved arrangement (two leaves), not the single leaf
        // `root` currently displays.
        let nodes: Vec<LayoutNode> = cx.update(|_, cx| {
            ws.tabs()
                .iter()
                .map(|tab| tab.serialize(cx).expect("every tab has a layout"))
                .collect()
        });
        assert_eq!(nodes.len(), 2, "one LayoutNode per tab");
        assert_eq!(pane_leaf_count(&nodes[0]), 2);
        assert_eq!(
            pane_leaf_count(&nodes[1]),
            2,
            "zoomed tab serializes its saved layout, not the zoomed leaf"
        );

        // Round-trip the zoomed tab's node back into a tree with the same panes.
        let mut panes: VecDeque<Entity<Pane>> = VecDeque::from(vec![c.clone(), d.clone()]);
        let restored = LayoutTree::from_layout_node(&nodes[1], &mut panes, &mut |_| {
            panic!("round-trip must not need to spawn a pane")
        });
        assert_eq!(
            leaf_ids(&restored),
            vec![c.entity_id(), d.entity_id()],
            "round-trip preserves the saved arrangement"
        );

        // Zoom is per tab and survives switching the visible tab.
        ws.set_active_tab(0);
        assert!(!ws.is_zoomed(), "the flat tab is not zoomed");
        ws.set_active_tab(1);
        assert!(ws.is_zoomed(), "the zoomed tab is still zoomed");
        assert_eq!(
            cx.update(|_, cx| ws.serialize_layout(cx).map(|n| pane_leaf_count(&n))),
            Some(2)
        );
    }
}
