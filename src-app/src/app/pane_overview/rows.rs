//! Pure row model for the Pane Overview (issue #339).
//!
//! No GPUI: the overlay's grouping, filtering, ordering, and selection rules
//! are plain data transforms so they can be unit-tested without building a
//! window. `PaneFlowApp` walks the workspace tree, produces `CardMeta` values,
//! and hands them here.

use std::collections::HashSet;

use crate::agent_launcher::TerminalAgent;
use crate::ai_types::AgentState;

/// Cards past this many, in visible order, render a static shell instead of a
/// live thumbnail.
///
/// The eight-pane perf gate (`layout/render.rs`) already spends a whole 60 Hz
/// frame on eight painted terminals, and the theoretical worst case here is
/// 20 workspaces x 32 tabs x 32 panes = 20,480 panes. Off-screen cards cull
/// themselves in prepaint; this cap bounds what is left when a very large
/// grid IS on screen.
pub(crate) const MAX_LIVE_THUMBNAILS: usize = 24;

/// One terminal pane, flattened. Carries only plain data - no entities - so
/// the transforms below stay testable.
#[derive(Clone, Debug)]
pub(crate) struct CardMeta {
    pub surface_id: u64,
    pub ws_idx: usize,
    pub ws_title: String,
    pub tab_idx: usize,
    pub tab_title: String,
    /// Position among terminal panes in the full split layout, independent of zoom.
    pub tab_pane_index: usize,
    /// Terminal panes only; metadata filtering does not change this count.
    pub tab_pane_count: usize,
    /// Display name, already clamped through `limits::clamp_untrusted_label`.
    pub name: String,
    pub cwd_label: Option<String>,
    pub agent: Option<TerminalAgent>,
    /// `None` == idle: a session absent from `agent_sessions` has no state.
    pub state: Option<AgentState>,
    pub cols: usize,
    pub rows: usize,
    pub exited: bool,
    /// The focused pane of the active tab of the active workspace - the one
    /// card the overlay opens on and marks "current". True for at most one
    /// card. NOT "any pane of the active tab".
    pub is_active: bool,
    /// The card's workspace is the active workspace. Lifted onto
    /// `WorkspaceGroup::is_active` by `group_cards`; kept separate from
    /// `is_active` so the header still marks the active workspace when its
    /// focused pane is a markdown or diff pane.
    pub ws_is_active: bool,
    /// `Workspace::git_branch`, empty when the workspace is not a checkout.
    pub ws_branch: String,
}

impl CardMeta {
    /// Everything the filter matches on, lowercased once per card per render.
    fn search_key(&self) -> String {
        let mut key = String::with_capacity(64);
        key.push_str(&self.name.to_lowercase());
        key.push(' ');
        key.push_str(&self.ws_title.to_lowercase());
        key.push(' ');
        key.push_str(&self.tab_title.to_lowercase());
        if let Some(agent) = self.agent {
            key.push(' ');
            key.push_str(&agent.display_name().to_lowercase());
        }
        if let Some(cwd) = &self.cwd_label {
            key.push(' ');
            key.push_str(&cwd.to_lowercase());
        }
        key
    }
}

#[derive(Clone, Debug)]
pub(crate) struct TabGroup {
    pub tab_idx: usize,
    pub cards: Vec<CardMeta>,
}

#[derive(Clone, Debug)]
pub(crate) struct WorkspaceGroup {
    pub ws_idx: usize,
    pub title: String,
    /// Shown in the section header beside the title; empty when none.
    pub branch: String,
    /// The header's active marker.
    pub is_active: bool,
    pub tabs: Vec<TabGroup>,
}

/// Group a flat card list into workspace sections, preserving tab adjacency.
///
/// Input order is the caller's traversal order (workspace index, then tab
/// index, then layout traversal); grouping is stable and never re-sorts, so
/// the on-screen order matches `flat_order`.
///
/// A workspace with no card - empty, or holding only markdown / diff panes -
/// never gets a group: a group is created by the first card that lands in it.
/// That is the product decision pinned by
/// `group_cards_omits_workspaces_with_no_terminal_cards`, not an accident of
/// this loop, so do not "fix" it by pre-seeding one group per workspace.
pub(crate) fn group_cards(cards: Vec<CardMeta>) -> Vec<WorkspaceGroup> {
    let mut groups: Vec<WorkspaceGroup> = Vec::new();
    for card in cards {
        let ws_slot = match groups.iter().position(|g| g.ws_idx == card.ws_idx) {
            Some(at) => at,
            None => {
                groups.push(WorkspaceGroup {
                    ws_idx: card.ws_idx,
                    title: card.ws_title.clone(),
                    branch: card.ws_branch.clone(),
                    is_active: card.ws_is_active,
                    tabs: Vec::new(),
                });
                groups.len() - 1
            }
        };
        let tabs = &mut groups[ws_slot].tabs;
        let tab_slot = match tabs.iter().position(|t| t.tab_idx == card.tab_idx) {
            Some(at) => at,
            None => {
                tabs.push(TabGroup {
                    tab_idx: card.tab_idx,
                    cards: Vec::new(),
                });
                tabs.len() - 1
            }
        };
        tabs[tab_slot].cards.push(card);
    }
    groups
}

/// Metadata-only filter: name, workspace, tab, agent, cwd basename.
///
/// Deliberately NOT a content search. Fleet Search already owns that, and
/// re-running it across every pane on each keystroke is the
/// extract-scrollback-on-the-tick anti-pattern guarded in `ipc_handler.rs`
/// (issue #29).
pub(crate) fn filter_cards(cards: &[CardMeta], query: &str) -> Vec<CardMeta> {
    let needle = query.trim().to_lowercase();
    if needle.is_empty() {
        return cards.to_vec();
    }
    cards
        .iter()
        .filter(|card| card.search_key().contains(&needle))
        .cloned()
        .collect()
}

/// Surface ids in grid order: workspace, tab, then full-layout traversal.
pub(crate) fn flat_order(groups: &[WorkspaceGroup]) -> Vec<u64> {
    groups
        .iter()
        .flat_map(|ws| ws.tabs.iter())
        .flat_map(|tab| tab.cards.iter())
        .map(|card| card.surface_id)
        .collect()
}

/// Direct children of the scroll container. Tabs share rows within a workspace.
pub(crate) enum GridRow<'a> {
    Workspace(&'a WorkspaceGroup),
    Cards(Vec<&'a CardMeta>),
}

pub(crate) fn grid_rows(groups: &[WorkspaceGroup], columns: usize) -> Vec<GridRow<'_>> {
    let mut rows = Vec::new();
    for group in groups {
        rows.push(GridRow::Workspace(group));
        let cards: Vec<_> = group.tabs.iter().flat_map(|tab| &tab.cards).collect();
        rows.extend(
            cards
                .chunks(columns.max(1))
                .map(|cards| GridRow::Cards(cards.to_vec())),
        );
    }
    rows
}

/// The scroll child containing a card, including intervening workspace headers.
pub(crate) fn selected_row(rows: &[GridRow<'_>], surface_id: u64) -> Option<usize> {
    rows.iter().position(|row| {
        matches!(row, GridRow::Cards(cards)
        if cards.iter().any(|card| card.surface_id == surface_id))
    })
}

/// Keep the visual column across workspace boundaries and partial rows.
pub(crate) fn move_vertical(rows: &[GridRow<'_>], selected: usize, down: bool) -> usize {
    let lengths: Vec<_> = rows
        .iter()
        .filter_map(|row| match row {
            GridRow::Cards(cards) => Some(cards.len()),
            GridRow::Workspace(_) => None,
        })
        .collect();
    let mut start = 0;
    for (row, &len) in lengths.iter().enumerate() {
        if selected < start + len {
            let column = selected - start;
            return if down {
                lengths
                    .get(row + 1)
                    .map_or(selected, |&next_len| start + len + column.min(next_len - 1))
            } else if row > 0 {
                let previous_len = lengths[row - 1];
                start - previous_len + column.min(previous_len - 1)
            } else {
                selected
            };
        }
        start += len;
    }
    start.saturating_sub(1)
}

/// Where the selection cursor starts when the overlay opens: on the current
/// pane's card (the focused pane of the active tab of the active workspace),
/// so Esc then Enter is a no-op round trip. Falls back to the first card when
/// `current` is `None` or not in `order` - no focused terminal, or the focused
/// pane is a markdown / diff pane the overlay does not list.
pub(crate) fn initial_selection(order: &[u64], current: Option<u64>) -> usize {
    current
        .and_then(|sid| order.iter().position(|id| *id == sid))
        .unwrap_or(0)
}

/// Move the selection cursor by `delta`, clamped. Never wraps: wrapping from
/// the last card of the last workspace to the first is disorienting in a
/// grouped grid where the two are visually far apart.
pub(crate) fn move_selection(selected: usize, len: usize, delta: isize) -> usize {
    if len == 0 {
        return 0;
    }
    let last = len as isize - 1;
    (selected as isize + delta).clamp(0, last) as usize
}

/// The prefix of `order` allowed to paint a live thumbnail.
pub(crate) fn live_thumbnail_ids(order: &[u64], cap: usize) -> HashSet<u64> {
    order.iter().take(cap).copied().collect()
}

/// How many cards fit on one wrapped row of a grid `width` px wide, given the
/// card width and the gap between cards. Never below one: a grid narrower
/// than a card still shows one card per row.
pub(crate) fn cards_per_row(width: f32, card_w: f32, gap: f32) -> usize {
    if width <= 0.0 || card_w <= 0.0 {
        return 1;
    }
    (((width + gap) / (card_w + gap)).floor() as usize).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(sid: u64, ws: usize, tab: usize, name: &str) -> CardMeta {
        CardMeta {
            surface_id: sid,
            ws_idx: ws,
            ws_title: format!("ws{ws}"),
            tab_idx: tab,
            tab_title: format!("tab{tab}"),
            tab_pane_index: 0,
            tab_pane_count: 1,
            name: name.to_string(),
            cwd_label: None,
            agent: None,
            state: None,
            cols: 80,
            rows: 24,
            exited: false,
            is_active: false,
            ws_is_active: false,
            ws_branch: String::new(),
        }
    }

    #[test]
    fn grouping_preserves_workspace_then_tab_order() {
        let cards = vec![card(1, 0, 0, "a"), card(2, 0, 1, "b"), card(3, 1, 0, "c")];
        let groups = group_cards(cards);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].ws_idx, 0);
        assert_eq!(groups[0].tabs.len(), 2);
        assert_eq!(groups[0].tabs[0].tab_idx, 0);
        assert_eq!(groups[0].tabs[1].tab_idx, 1);
        assert_eq!(groups[1].ws_idx, 1);
        assert_eq!(groups[1].tabs.len(), 1);
    }

    #[test]
    fn grouping_keeps_two_panes_of_one_tab_together() {
        let groups = group_cards(vec![card(1, 0, 0, "a"), card(2, 0, 0, "b")]);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].tabs.len(), 1);
        assert_eq!(groups[0].tabs[0].cards.len(), 2);
    }

    fn packed_ids(rows: &[GridRow<'_>]) -> Vec<Vec<u64>> {
        rows.iter()
            .filter_map(|row| match row {
                GridRow::Workspace(_) => None,
                GridRow::Cards(cards) => Some(cards.iter().map(|card| card.surface_id).collect()),
            })
            .collect()
    }

    #[test]
    fn single_pane_tabs_share_rows_and_split_panes_stay_adjacent() {
        let groups = group_cards(vec![
            card(1, 0, 0, "single"),
            card(2, 0, 1, "single"),
            card(3, 0, 2, "split a"),
            card(4, 0, 2, "split b"),
            card(5, 0, 3, "single"),
            card(6, 1, 0, "other workspace"),
        ]);
        let rows = grid_rows(&groups, 4);
        assert_eq!(packed_ids(&rows), vec![vec![1, 2, 3, 4], vec![5], vec![6]]);
        assert_eq!(selected_row(&rows, 4), Some(1));
        assert_eq!(selected_row(&rows, 6), Some(4));
        assert_eq!(selected_row(&rows, 99), None);
    }

    #[test]
    fn vertical_navigation_follows_partial_workspace_rows() {
        let groups = group_cards(vec![
            card(1, 0, 0, "a"),
            card(2, 0, 1, "b"),
            card(3, 0, 2, "c"),
            card(4, 1, 0, "d"),
            card(5, 1, 1, "e"),
            card(6, 1, 2, "f"),
            card(7, 1, 3, "g"),
            card(8, 1, 4, "h"),
        ]);
        let rows = grid_rows(&groups, 4);
        // Visual rows: [1,2,3], [4,5,6,7], [8]. Headers consume no column.
        assert_eq!(move_vertical(&rows, 1, true), 4);
        assert_eq!(move_vertical(&rows, 5, false), 2);
        assert_eq!(move_vertical(&rows, 6, false), 2);
        assert_eq!(move_vertical(&rows, 6, true), 7);
        assert_eq!(move_vertical(&rows, 1, false), 1);
        assert_eq!(move_vertical(&rows, 7, true), 7);
        assert_eq!(move_vertical(&[], 0, true), 0);
    }

    #[test]
    fn filtering_and_resizing_repack_without_empty_tab_rows() {
        let groups = group_cards(filter_cards(
            &[
                card(1, 0, 0, "keep"),
                card(2, 0, 1, "hide"),
                card(3, 0, 2, "keep"),
                card(4, 0, 3, "keep"),
            ],
            "keep",
        ));
        assert_eq!(packed_ids(&grid_rows(&groups, 3)), vec![vec![1, 3, 4]]);
        assert_eq!(
            packed_ids(&grid_rows(&groups, 2)),
            vec![vec![1, 3], vec![4]]
        );
        assert_eq!(
            packed_ids(&grid_rows(&groups, 0)),
            vec![vec![1], vec![3], vec![4]]
        );
        assert!(grid_rows(&[], 3).is_empty());
    }

    /// Product decision (2026-09-03, spec §7.5): a workspace with no card -
    /// empty, or holding only markdown / diff panes - gets no section and no
    /// header. Grouping does this as a side effect (a group only exists once
    /// a card lands in it); this test makes it a decision, not an accident.
    #[test]
    fn group_cards_omits_workspaces_with_no_terminal_cards() {
        // Workspace 1 has no cards at all: nothing was enumerated for it.
        let groups = group_cards(vec![card(1, 0, 0, "a"), card(3, 2, 0, "c")]);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].ws_idx, 0);
        assert_eq!(groups[1].ws_idx, 2);
        assert!(groups.iter().all(|g| !g.tabs.is_empty()));
        assert!(
            groups
                .iter()
                .all(|g| g.tabs.iter().all(|t| !t.cards.is_empty()))
        );
    }

    /// The section header marks the active workspace and shows its branch,
    /// lifted from the cards rather than looked up again.
    #[test]
    fn grouping_lifts_the_active_workspace_and_branch_onto_the_header() {
        let mut a = card(1, 0, 0, "a");
        a.ws_is_active = true;
        a.ws_branch = "main".to_string();
        let b = card(2, 1, 0, "b");
        let groups = group_cards(vec![a, b]);
        assert!(groups[0].is_active);
        assert_eq!(groups[0].branch, "main");
        assert!(!groups[1].is_active);
        assert_eq!(groups[1].branch, "");
    }

    #[test]
    fn filter_matches_name_workspace_and_tab_case_insensitively() {
        let cards = vec![card(1, 0, 0, "Claude"), card(2, 1, 0, "vite")];
        assert_eq!(filter_cards(&cards, "cla").len(), 1);
        assert_eq!(filter_cards(&cards, "CLA").len(), 1);
        assert_eq!(filter_cards(&cards, "ws1").len(), 1);
        assert_eq!(filter_cards(&cards, "tab0").len(), 2);
        assert_eq!(filter_cards(&cards, "nothing").len(), 0);
    }

    #[test]
    fn an_empty_filter_keeps_every_card() {
        let cards = vec![card(1, 0, 0, "a"), card(2, 1, 0, "b")];
        assert_eq!(filter_cards(&cards, "").len(), 2);
        assert_eq!(filter_cards(&cards, "   ").len(), 2);
    }

    #[test]
    fn filter_matches_the_agent_display_name() {
        let mut c = card(1, 0, 0, "shell");
        c.agent = Some(crate::agent_launcher::TerminalAgent::ClaudeCode);
        let needle = crate::agent_launcher::TerminalAgent::ClaudeCode
            .display_name()
            .to_lowercase();
        assert_eq!(filter_cards(&[c], &needle).len(), 1);
    }

    #[test]
    fn filter_matches_the_cwd_basename() {
        let mut c = card(1, 0, 0, "shell");
        c.cwd_label = Some("paneflow".to_string());
        assert_eq!(filter_cards(&[c], "PANEflow").len(), 1);
    }

    #[test]
    fn flat_order_is_workspace_then_tab_then_traversal() {
        let groups = group_cards(vec![
            card(3, 1, 0, "c"),
            card(1, 0, 0, "a"),
            card(2, 0, 1, "b"),
        ]);
        assert_eq!(flat_order(&groups), vec![3, 1, 2]);
        // The caller supplies traversal order; grouping never re-sorts.
        let groups = group_cards(vec![
            card(1, 0, 0, "a"),
            card(2, 0, 1, "b"),
            card(3, 1, 0, "c"),
        ]);
        assert_eq!(flat_order(&groups), vec![1, 2, 3]);
    }

    /// Product decision (2026-09-03): the overlay opens on the current pane's
    /// card, so Esc then Enter is a no-op round trip.
    #[test]
    fn initial_selection_starts_on_the_current_card() {
        let order = vec![10, 20, 30];
        assert_eq!(initial_selection(&order, Some(30)), 2);
        assert_eq!(initial_selection(&order, Some(10)), 0);
    }

    /// No focused terminal (or the focused pane is a markdown / diff pane):
    /// fall back to the first card rather than guessing.
    #[test]
    fn initial_selection_falls_back_to_the_first_card() {
        let order = vec![10, 20, 30];
        assert_eq!(initial_selection(&order, Some(99)), 0);
        assert_eq!(initial_selection(&order, None), 0);
        assert_eq!(initial_selection(&[], Some(10)), 0);
    }

    #[test]
    fn move_selection_clamps_at_both_ends() {
        assert_eq!(move_selection(0, 5, -1), 0);
        assert_eq!(move_selection(4, 5, 1), 4);
        assert_eq!(move_selection(2, 5, 1), 3);
        assert_eq!(move_selection(2, 5, -1), 1);
    }

    #[test]
    fn move_selection_by_a_row_clamps_rather_than_wrapping() {
        // Down from the last partial row lands on the last card, not past it.
        assert_eq!(move_selection(3, 5, 4), 4);
        // Up from the first row stays put.
        assert_eq!(move_selection(1, 5, -4), 0);
    }

    #[test]
    fn move_selection_on_an_empty_list_is_zero() {
        assert_eq!(move_selection(0, 0, 1), 0);
        assert_eq!(move_selection(3, 0, -1), 0);
    }

    #[test]
    fn only_the_first_cards_render_live_thumbnails() {
        let order: Vec<u64> = (0..30).collect();
        let live = live_thumbnail_ids(&order, 24);
        assert_eq!(live.len(), 24);
        assert!(live.contains(&0));
        assert!(live.contains(&23));
        assert!(!live.contains(&24));
    }

    #[test]
    fn a_short_list_renders_every_thumbnail_live() {
        let order: Vec<u64> = (0..3).collect();
        assert_eq!(live_thumbnail_ids(&order, 24).len(), 3);
    }

    #[test]
    fn cards_per_row_follows_the_measured_width() {
        // Three 328px cards with 12px gaps need 1008px; a fourth needs 1348.
        assert_eq!(cards_per_row(1008.0, 328.0, 12.0), 3);
        assert_eq!(cards_per_row(1347.0, 328.0, 12.0), 3);
        assert_eq!(cards_per_row(1348.0, 328.0, 12.0), 4);
        // Narrower than one card still yields one card per row.
        assert_eq!(cards_per_row(100.0, 328.0, 12.0), 1);
        assert_eq!(cards_per_row(0.0, 328.0, 12.0), 1);
    }
}
