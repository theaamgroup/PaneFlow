//! Layout presets: equal-distribution, main-vertical (60/40 split with stack),
//! and tmux-style tiled grid. Each builder returns `None` for empty, `Leaf`
//! for single pane, or a multi-child `Container` otherwise.

use std::cell::Cell;
use std::rc::Rc;

use gpui::Entity;

use crate::pane::Pane;

use super::tree::{LayoutChild, LayoutTree, SplitDirection};

impl LayoutTree {
    /// Build a flat container with all panes at equal ratios in the given direction.
    /// Returns `None` for empty, `Leaf` for single pane, `Container` for 2+.
    pub fn from_panes_equal(direction: SplitDirection, panes: Vec<Entity<Pane>>) -> Option<Self> {
        match panes.len() {
            0 => None,
            1 => Some(LayoutTree::Leaf(panes.into_iter().next().unwrap())),
            n => {
                let ratio = 1.0 / n as f32;
                let children = panes
                    .into_iter()
                    .map(|pane| LayoutChild {
                        node: LayoutTree::Leaf(pane),
                        ratio: Rc::new(Cell::new(ratio)),
                    })
                    .collect();
                Some(LayoutTree::Container {
                    direction,
                    children,
                    drag: Rc::new(Cell::new(None)),
                    container_size: Rc::new(Cell::new(0.0)),
                })
            }
        }
    }

    /// Build a "main-vertical" layout: one main pane on the left,
    /// remaining panes stacked vertically on the right.
    /// `main_pane` is placed first. Returns `None` for empty, `Leaf` for single.
    pub fn main_vertical(main_pane: Entity<Pane>, others: Vec<Entity<Pane>>) -> Option<Self> {
        if others.is_empty() {
            return Some(LayoutTree::Leaf(main_pane));
        }

        // Right side: stack remaining panes with equal ratios (Horizontal = top/bottom)
        let right = LayoutTree::from_panes_equal(SplitDirection::Horizontal, others)
            .expect("others is non-empty");

        // Outer: Vertical (side by side) - centered split between main and side panel.
        Some(LayoutTree::Container {
            direction: SplitDirection::Vertical,
            children: vec![
                LayoutChild {
                    node: LayoutTree::Leaf(main_pane),
                    ratio: Rc::new(Cell::new(0.5)),
                },
                LayoutChild {
                    node: right,
                    ratio: Rc::new(Cell::new(0.5)),
                },
            ],
            drag: Rc::new(Cell::new(None)),
            container_size: Rc::new(Cell::new(0.0)),
        })
    }

    /// Build a tiled grid layout. Uses tmux's algorithm: increment rows and
    /// columns alternately until `rows * cols >= N`. Each row is a Vertical
    /// container; rows are stacked in a Horizontal container.
    /// Returns `None` for empty, `Leaf` for single.
    pub fn tiled(panes: Vec<Entity<Pane>>) -> Option<Self> {
        match panes.len() {
            0 => return None,
            1 => return Some(LayoutTree::Leaf(panes.into_iter().next().unwrap())),
            _ => {}
        }

        let n = panes.len();
        // tmux algorithm: increment rows and cols alternately until rows*cols >= n
        let mut rows = 1usize;
        let mut cols = 1usize;
        while rows * cols < n {
            if cols <= rows {
                cols += 1;
            } else {
                rows += 1;
            }
        }

        // Distribute panes across rows
        let row_ratio = 1.0 / rows as f32;
        let mut pane_iter = panes.into_iter();
        let mut row_children: Vec<LayoutChild> = Vec::with_capacity(rows);

        for r in 0..rows {
            // Last row may have fewer panes
            let panes_in_row = if r < rows - 1 {
                cols
            } else {
                n - cols * (rows - 1)
            };

            let row_panes: Vec<Entity<Pane>> = pane_iter.by_ref().take(panes_in_row).collect();
            let row_tree = LayoutTree::from_panes_equal(SplitDirection::Vertical, row_panes)
                .expect("row is non-empty");

            row_children.push(LayoutChild {
                node: row_tree,
                ratio: Rc::new(Cell::new(row_ratio)),
            });
        }

        Some(LayoutTree::Container {
            direction: SplitDirection::Horizontal,
            children: row_children,
            drag: Rc::new(Cell::new(None)),
            container_size: Rc::new(Cell::new(0.0)),
        })
    }
}

#[cfg(test)]
mod tests {
    use gpui::{AppContext, Entity, TestAppContext};

    use crate::pane::Pane;
    use crate::terminal::TerminalView;

    use super::*;

    fn test_pane(cx: &mut impl AppContext, workspace_id: u64) -> Entity<Pane> {
        let terminal = cx.new(|cx| TerminalView::display_only_for_test(workspace_id, cx));
        cx.new(|cx| Pane::new(terminal, workspace_id, cx))
    }

    fn leaf_ids(tree: &LayoutTree) -> Vec<gpui::EntityId> {
        tree.collect_leaves()
            .into_iter()
            .map(|pane| pane.entity_id())
            .collect()
    }

    fn container_parts(tree: &LayoutTree) -> (SplitDirection, &Vec<LayoutChild>) {
        match tree {
            LayoutTree::Container {
                direction,
                children,
                ..
            } => (*direction, children),
            LayoutTree::Leaf(_) => panic!("expected a container"),
        }
    }

    fn assert_ratio(actual: f32, expected: f32) {
        assert!(
            (actual - expected).abs() < f32::EPSILON,
            "ratio {actual} != {expected}"
        );
    }

    #[gpui::test]
    fn tiled_returns_none_for_no_panes(_cx: &mut TestAppContext) {
        assert!(LayoutTree::tiled(Vec::new()).is_none());
    }

    #[gpui::test]
    fn tiled_returns_bare_leaf_for_one_pane(cx: &mut TestAppContext) {
        let cx = cx.add_empty_window();
        let a = test_pane(cx, 1);

        let tree = LayoutTree::tiled(vec![a.clone()]).expect("one pane builds a tree");

        match tree {
            LayoutTree::Leaf(pane) => assert_eq!(pane.entity_id(), a.entity_id()),
            LayoutTree::Container { .. } => panic!("one pane must not be wrapped"),
        }
    }

    #[gpui::test]
    fn tiled_two_panes_is_one_row_of_two_columns(cx: &mut TestAppContext) {
        let cx = cx.add_empty_window();
        let a = test_pane(cx, 2);
        let b = test_pane(cx, 2);

        let tree = LayoutTree::tiled(vec![a.clone(), b.clone()]).expect("two panes build a tree");

        let (direction, rows) = container_parts(&tree);
        assert!(
            direction == SplitDirection::Horizontal,
            "wrong split direction"
        );
        assert_eq!(rows.len(), 1);
        assert_ratio(rows[0].ratio.get(), 1.0);

        let (row_direction, cells) = container_parts(&rows[0].node);
        assert!(
            row_direction == SplitDirection::Vertical,
            "wrong split direction"
        );
        assert_eq!(cells.len(), 2);
        assert_ratio(cells[0].ratio.get(), 0.5);
        assert_ratio(cells[1].ratio.get(), 0.5);
        assert_eq!(leaf_ids(&tree), vec![a.entity_id(), b.entity_id()]);
    }

    #[gpui::test]
    fn tiled_seven_panes_fills_three_rows_with_remainder_last(cx: &mut TestAppContext) {
        let cx = cx.add_empty_window();
        let panes: Vec<Entity<Pane>> = (0..7).map(|_| test_pane(cx, 3)).collect();
        let expected_ids: Vec<gpui::EntityId> = panes.iter().map(|p| p.entity_id()).collect();

        let tree = LayoutTree::tiled(panes).expect("seven panes build a tree");

        // tmux grid for N=7 is 3x3 with the last row holding the remainder.
        let (direction, rows) = container_parts(&tree);
        assert!(
            direction == SplitDirection::Horizontal,
            "wrong split direction"
        );
        assert_eq!(rows.len(), 3);
        for row in rows {
            assert_ratio(row.ratio.get(), 1.0 / 3.0);
        }
        assert_eq!(
            rows.iter()
                .map(|row| row.node.leaf_count())
                .collect::<Vec<_>>(),
            vec![3, 3, 1]
        );

        for full_row in &rows[..2] {
            let (row_direction, cells) = container_parts(&full_row.node);
            assert!(
                row_direction == SplitDirection::Vertical,
                "wrong split direction"
            );
            assert_eq!(cells.len(), 3);
            for cell in cells {
                assert_ratio(cell.ratio.get(), 1.0 / 3.0);
            }
        }
        // A one-pane remainder row is a bare leaf, not a single-child container.
        assert!(matches!(rows[2].node, LayoutTree::Leaf(_)));

        assert_eq!(tree.leaf_count(), 7);
        assert_eq!(leaf_ids(&tree), expected_ids);
    }
}
