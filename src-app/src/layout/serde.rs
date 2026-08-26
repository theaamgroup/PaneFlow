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
    /// Full history extract. Kept for [`LayoutTree::serialize`]; IPC
    /// `workspace.current` and session persistence use [`Self::Omit`] so the
    /// GPUI tick does not walk 4000 lines per pane (issue #29).
    #[allow(dead_code)]
    Inline,
    Omit,
}

impl LayoutTree {
    /// Serialize the layout tree to a `LayoutNode` (config schema type).
    ///
    /// Each leaf produces a `LayoutNode::Pane` with one `SurfaceDefinition` per
    /// tab, capturing the terminal's CWD and OSC title. The active tab is marked
    /// with `focus: true`. Each container produces a `LayoutNode::Split` with
    /// per-child `ratios` and recursive children.
    ///
    /// Not used by `workspace.current` (issue #29); the inline extract is the
    /// snapshot-with-scrollback path.
    #[allow(dead_code)]
    pub fn serialize(&self, cx: &App) -> LayoutNode {
        self.serialize_with(cx, ScrollbackCapture::Inline)
    }

    /// Serialize session metadata without carrying terminal output into the
    /// next process. The schema field remains present as `None` for backward
    /// compatibility with existing session files.
    pub fn serialize_without_scrollback(&self, cx: &App) -> LayoutNode {
        self.serialize_with(cx, ScrollbackCapture::Omit)
    }

    /// Inner serializer parametrised by the [`ScrollbackCapture`] strategy.
    fn serialize_with(&self, cx: &App, capture: ScrollbackCapture) -> LayoutNode {
        match self {
            LayoutTree::Leaf(pane) => {
                let pane_ref = pane.read(cx);
                let surfaces: Vec<SurfaceDefinition> = pane_ref
                    .tabs
                    .iter()
                    .enumerate()
                    .map(|(i, tab)| match tab {
                        crate::pane::TabContent::Terminal(tv) => {
                            let tv_ref = tv.read(cx);
                            let name = if tv_ref.terminal.title.is_empty() {
                                None
                            } else {
                                Some(tv_ref.terminal.title.clone())
                            };
                            let cwd = tv_ref.terminal.current_cwd.clone().or_else(|| {
                                tv_ref.terminal.cwd_now().map(|p| p.display().to_string())
                            });
                            let scrollback = match capture {
                                ScrollbackCapture::Inline => tv_ref.terminal.extract_scrollback(),
                                ScrollbackCapture::Omit => None,
                            };
                            SurfaceDefinition {
                                surface_type: Some("terminal".to_string()),
                                name,
                                custom_name: tv_ref.terminal.custom_name.clone(),
                                command: None,
                                prompt: None,
                                cwd,
                                path: None,
                                env: None,
                                focus: (i == pane_ref.selected_idx).then_some(true),
                                scrollback,
                                agent: tv_ref.terminal.detected_agent.map(|a| a.tag().to_string()),
                                font_size: tv_ref.terminal.font_size_override,
                            }
                        }
                        crate::pane::TabContent::Markdown(markdown) => {
                            let path = markdown.read(cx).path.display().to_string();
                            SurfaceDefinition {
                                surface_type: Some("markdown".to_string()),
                                name: None,
                                custom_name: None,
                                command: None,
                                prompt: None,
                                cwd: None,
                                path: Some(path),
                                env: None,
                                focus: (i == pane_ref.selected_idx).then_some(true),
                                scrollback: None,
                                agent: None,
                                font_size: None,
                            }
                        }
                        crate::pane::TabContent::Diff(_) => SurfaceDefinition {
                            surface_type: Some("diff".to_string()),
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
                    child_nodes.push(c.node.serialize_with(cx, capture));
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

    use super::*;

    fn test_pane(cx: &mut impl AppContext, workspace_id: u64) -> Entity<Pane> {
        let terminal = cx.new(|cx| TerminalView::display_only_for_test(workspace_id, cx));
        cx.new(|cx| Pane::new(terminal, workspace_id, cx))
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
            test_pane(cx, 1)
        });

        assert_eq!(tree.leaf_count(), 2);
        assert_eq!(
            spawned.len(),
            2,
            "both leaves must spawn when nothing is reused"
        );
        match &spawned[0] {
            LayoutNode::Pane { surfaces } => {
                assert_eq!(surfaces.len(), 2, "leaf 0 keeps both tabs");
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
        let existing = test_pane(cx, 1);
        let layout = two_pane_layout_with_first_pane_metadata();
        let mut panes = VecDeque::from([existing.clone()]);
        let mut spawned: Vec<LayoutNode> = Vec::new();
        let tree = LayoutTree::from_layout_node(&layout, &mut panes, &mut |node| {
            spawned.push(node.clone());
            test_pane(cx, 1)
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
}
