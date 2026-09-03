# Pane Overview Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A Mission-Control-style overlay showing every open terminal pane across every workspace and tab, each bottom-cropped to its last 12 rows at a legible size, with its agent status, where clicking a card jumps to that pane.

**Architecture:** A `deferred(...).with_priority(6)` overlay owned by `PaneFlowApp`, following the Attention Queue pattern exactly. Pane previews come from a new read-only GPUI `Element` that reuses the existing `layout_from_snapshot` pure-layout seam rather than `TerminalElement` — reusing `TerminalElement` would resize every child process, because its layout pass calls `notify_window_size`. Enumeration, grouping, filtering, and selection movement live in a GPUI-free module so they are unit-testable without a window.

**Tech Stack:** Rust 1.98.0, GPUI (git dependency, `zed-industries/zed@fecc3273`), libghostty-vt for terminal state, macOS/Apple Silicon only.

**Spec:** `docs/superpowers/specs/2026-09-03-pane-overview-design.md` — read it before Task 1. **Issue:** #339.

## Global Constraints

- **macOS only.** No `#[cfg(target_os = "linux")]`, no `#[cfg(windows)]`, no `not(unix)`, no `not(target_os = "macos")`. `./scripts/linux-census.sh` fails the build if the STAGE 2c total goes non-zero.
- **Rust 1.98.0**, pinned by `rust-toolchain.toml`.
- **Run `cargo fmt --check` before EVERY commit.** A single mis-formatted line fails CI and burns a ~25 min release run.
- **Six verification gates**, before and after the whole pass, quoting actual output:
  ```bash
  cargo build                                # exit 0
  cargo test --workspace                     # diff test NAMES, never trust the integer
  cargo clippy --workspace --all-targets     # exit 0, WARNING COUNT 1 (block v0.1.6)
  cargo fmt --check                          # exit 0
  ./target/debug/paneflow --version          # paneflow 0.2.1
  cargo deny check advisories licenses sources   # exit 0
  ```
- **Never pipe a command whose exit status matters.** `cargo test | tail` reports `tail`'s status. Redirect as `cmd > file 2>&1`, never `cmd 2>&1 > file`.
- **A green `cargo build` is not a green tree.** This repo has already shipped a change that built clean and failed `cargo test`.
- **Commit convention:** `feat(module): description`, citing `#339`. Atomic commits per logical change.
- **Untrusted text:** any OSC-derived title rendered into chrome goes through `crate::limits::clamp_untrusted_label` (cap 64 chars, bidi/zero-width stripped).
- **Source-read tests must use `crate::source_probe::source_slice(src, start, end)`** with BOTH anchors. An unbounded `split().nth(1)` runs to end-of-file and fails open. This is repo policy (issue #219).
- **No new `paneflow.json` key.** The feature is entirely constant-driven.
- **`AgentState` is `Clone`, not `Copy`** — use `.cloned()`. `TerminalAgent` **is** `Copy`.

---

## File Structure

| File | Responsibility |
|---|---|
| `src-app/src/app/pane_overview/rows.rs` | **CREATE.** GPUI-free: `CardMeta`, grouping, filtering, flat order, selection movement, live-thumbnail partition. All unit-tested. |
| `src-app/src/app/pane_overview/mod.rs` | **CREATE.** Overlay state machine on `PaneFlowApp`: open/close, key handling, activate, `collect_cards`, render. |
| `src-app/src/terminal/element/thumbnail.rs` | **CREATE.** `thumbnail_snapshot` (pure, no GPUI) + `TerminalThumbnail` (the `Element`). Must live under `element/` — `LayoutState`'s fields, `mod paint`, and `CellGeometry` are all private to `element`, and a child module can see its parent's private items. |
| `src-app/src/app/workspace_ops/focus.rs` | **MODIFY.** Extract `teleport_to_surface`. |
| `src-app/src/app/attention_queue.rs` | **MODIFY.** Call the extracted helper. |
| `src-app/src/app/fleet_search.rs` | **MODIFY.** Call the extracted helper. |
| `src-app/src/app/actions.rs` | **MODIFY.** Add `OpenPaneOverview`. |
| `src-app/src/keybindings/{registry,defaults,apply}.rs` | **MODIFY.** Register, bind `secondary-shift-p`, guard test. |
| `src-app/src/app/mod.rs` | **MODIFY.** Declare `pub mod pane_overview;`. |
| `src-app/src/terminal/element/mod.rs` | **MODIFY.** Declare `mod thumbnail;`. |
| `src-app/src/main.rs` | **MODIFY.** Struct fields, `.on_action`, render composition. |
| `src-app/src/app/bootstrap.rs` | **MODIFY.** Field init, 250 ms refresh loop, Window menu item, menu fallback, guard test. |
| `src-app/src/app/sidebar/mod.rs` | **MODIFY.** Button beside the "Workspaces" header. |
| `CLAUDE.md`, `docs/user/features.md` | **MODIFY.** Action count, chord table, architecture tree, corrected header sentence. |

---

## Task 1: Pure row model (`pane_overview/rows.rs`)

**Files:**
- Create: `src-app/src/app/pane_overview/rows.rs`
- Create: `src-app/src/app/pane_overview/mod.rs` (stub only, so the module tree compiles)
- Modify: `src-app/src/app/mod.rs`

**Interfaces:**
- Consumes: `crate::ai_types::AgentState`, `crate::agent_launcher::TerminalAgent`.
- Produces: `CardMeta`, `TabGroup`, `WorkspaceGroup`, `group_cards`, `filter_cards`, `flat_order`, `initial_selection`, `move_selection`, `live_thumbnail_ids`, `MAX_LIVE_THUMBNAILS`. Tasks 5-7 depend on every one of these names.
- `WorkspaceGroup` keeps `is_active` and `branch` (the spec's §6 definition). `CardMeta.is_active` means **the focused pane** of the active tab of the active workspace - one card at most - not "any pane in the active tab"; `ws_is_active` is the separate workspace-level flag the header uses.

- [ ] **Step 1: Declare the module**

In `src-app/src/app/mod.rs`, the list is alphabetical. Insert between `pub mod pane_palette;` and `pub mod profile_menu;`:

```rust
pub mod pane_overview;
```

Note the existing list has `pane_palette` before `profile_menu`; `pane_overview` sorts before `pane_palette`, so insert it immediately **before** `pub mod pane_palette;`.

Create `src-app/src/app/pane_overview/mod.rs` with only:

```rust
//! Pane Overview (issue #339): a cross-workspace overlay showing every open
//! terminal pane, grouped by workspace and tab, each bottom-cropped to its
//! last rows so a pane can be identified and jumped to.
//!
//! The overlay is a pure read. Every pane's grid snapshot is published
//! unconditionally by its runtime thread with no visibility gate, so panes in
//! background tabs of inactive workspaces already have live content sitting in
//! `SharedState` - nothing has to be pulled or woken.

pub(crate) mod rows;
```

- [ ] **Step 2: Write the failing tests**

Create `src-app/src/app/pane_overview/rows.rs` containing ONLY this test module for now (the `use super::*;` will not resolve yet — that is the point):

```rust
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
        let cards = vec![
            card(1, 0, 0, "a"),
            card(2, 0, 1, "b"),
            card(3, 1, 0, "c"),
        ];
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

    /// Product decision (2026-09-03): a workspace with no terminal pane -
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
        assert!(groups.iter().all(|g| g.tabs.iter().all(|t| !t.cards.is_empty())));
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
        assert_eq!(filter_cards(&[c], "claude code").len(), 1);
    }

    #[test]
    fn flat_order_is_workspace_then_tab_then_traversal() {
        let groups = group_cards(vec![
            card(3, 1, 0, "c"),
            card(1, 0, 0, "a"),
            card(2, 0, 1, "b"),
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
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p paneflow-app pane_overview::rows 2>&1 | tail -30`
Expected: FAIL to compile — `cannot find type CardMeta in this scope` and similar for every symbol.

- [ ] **Step 4: Write the implementation**

Prepend to `src-app/src/app/pane_overview/rows.rs`, above the test module:

```rust
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
/// The `#[ignore]`d eight-pane benchmark (`layout/render.rs:464-469`) already
/// spends a whole 60 Hz frame on eight painted terminals, and the theoretical worst case here is
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
    pub title: String,
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

/// Group a flat card list into workspace sections and tab rows.
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
                    title: card.tab_title.clone(),
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

/// Surface ids in visible order. This is the same stable order
/// `jump_next_session_where` uses, so the overview and Cmd+Shift+J agree
/// about what "next" means.
pub(crate) fn flat_order(groups: &[WorkspaceGroup]) -> Vec<u64> {
    groups
        .iter()
        .flat_map(|ws| ws.tabs.iter())
        .flat_map(|tab| tab.cards.iter())
        .map(|card| card.surface_id)
        .collect()
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
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p paneflow-app pane_overview::rows 2>&1 | tail -20`
Expected: PASS — 15 tests.

- [ ] **Step 6: Format, lint, commit**

```bash
cargo fmt
cargo clippy -p paneflow-app --all-targets 2>&1 | tail -5
git add src-app/src/app/mod.rs src-app/src/app/pane_overview/
git commit -m "feat(pane_overview): pure row model for the pane overview (#339)"
```

---

## Task 2: Extract `teleport_to_surface`

The click-to-jump sequence exists twice already and this feature would make it three copies. Its *ordering* is a correctness contract — focus can only land on a rendered pane, so the owning tab must become visible first — and a contract that lives in three places drifts.

**Files:**
- Modify: `src-app/src/app/workspace_ops/focus.rs`
- Modify: `src-app/src/app/attention_queue.rs:157-178`
- Modify: `src-app/src/app/fleet_search.rs` (`fleet_search_activate`)

**Interfaces:**
- Produces: `PaneFlowApp::teleport_to_surface(&mut self, surface_id: u64, window: &mut Window, cx: &mut Context<Self>) -> bool`. Task 5 calls it.

- [ ] **Step 1: Add the helper**

Append inside the existing `impl PaneFlowApp` block in `src-app/src/app/workspace_ops/focus.rs`:

```rust
    /// Focus the pane hosting `surface_id`, wherever it lives.
    ///
    /// Returns `false` when the surface is gone (a pane closed between render
    /// and activation), which every caller treats as a clean no-op.
    ///
    /// The ORDER is load-bearing and is why this is one function rather than
    /// three copies: focus can only land on a *rendered* pane, so the owning
    /// tab has to become visible before `activate_workspace_at` runs. Indices
    /// are re-resolved from `surface_id` here rather than captured by the
    /// caller, so a workspace or tab reorder between render and click cannot
    /// teleport the user to the wrong pane.
    pub(crate) fn teleport_to_surface(
        &mut self,
        surface_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(loc) = crate::app::ipc_handler::find_pane_by_surface_id(
            &self.workspaces,
            surface_id,
            cx,
        ) else {
            cx.notify();
            return false;
        };
        let (ws_idx, pane) = (loc.workspace_idx, loc.pane);
        if let Some(ws) = self.workspaces.get_mut(ws_idx) {
            ws.set_active_tab(loc.tab_idx);
        }
        self.activate_workspace_at(
            ws_idx,
            crate::app::workspace_ops::WorkspaceFocusTarget::Pane { pane },
            window,
            cx,
        );
        // Keep the jump cycle coherent: a teleport counts as visiting that
        // surface, so the next Cmd+Shift+J continues from here.
        self.jump_cursor = Some(surface_id);
        true
    }
```

- [ ] **Step 2: Point the attention queue at it**

In `src-app/src/app/attention_queue.rs`, replace the body of `attention_queue_activate` (keep its doc comment) with:

```rust
    pub(crate) fn attention_queue_activate(
        &mut self,
        surface_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.teleport_to_surface(surface_id, window, cx) {
            self.close_attention_queue(cx);
        }
    }
```

Then delete the now-unused imports at the top of the file if `cargo clippy` flags them — specifically `use crate::app::ipc_handler::find_pane_by_surface_id;` and `use crate::app::workspace_ops::WorkspaceFocusTarget;`.

- [ ] **Step 3: Point fleet search at it**

In `src-app/src/app/fleet_search.rs`, rewrite `fleet_search_activate` the same way: replace its lookup/`set_active_tab`/`activate_workspace_at`/`jump_cursor` block with a single `if self.teleport_to_surface(surface_id, window, cx) { ... }`, keeping whatever local-search arming and close logic follows it inside the `if`. Remove imports clippy then reports as unused.

- [ ] **Step 4: Verify nothing regressed**

Run: `cargo test -p paneflow-app 2>&1 > /tmp/pf-t2.log; tail -25 /tmp/pf-t2.log`
Expected: same test names passing as before this task. Compare against a baseline captured with
`grep -oE '^test [a-zA-Z0-9_:]+ \.\.\.' /tmp/pf-t2.log | sed 's/^test //; s/ \.\.\.$//' | sort`.

- [ ] **Step 5: Format, lint, commit**

```bash
cargo fmt
cargo clippy -p paneflow-app --all-targets 2>&1 | tail -5
git add src-app/src/app/workspace_ops/focus.rs src-app/src/app/attention_queue.rs src-app/src/app/fleet_search.rs
git commit -m "refactor(workspace_ops): one teleport_to_surface for all three overlays (#339)"
```

---

## Task 3: Thumbnail snapshot helper (pure) — and the no-resize guard

This is the highest-risk part of the feature, isolated into a GPUI-free function so it can be tested against a real backend with no window.

**Files:**
- Create: `src-app/src/terminal/element/thumbnail.rs`
- Modify: `src-app/src/terminal/element/mod.rs` (add `mod thumbnail;`)

**Interfaces:**
- Consumes: `TerminalSessionBackend::{render_content, grid_metrics}`, `TerminalWindowSize`, `Content`.
- Produces: `THUMBNAIL_ROWS`, `THUMBNAIL_FONT_PX`, `THUMBNAIL_BAND_W/H`, `thumbnail_cell_dimensions`, `thumbnail_font`, `thumbnail_snapshot(&TerminalSessionBackend) -> ThumbnailSnapshot`, `struct ThumbnailSnapshot { content, first_visible_row, last_visible_row }`. Task 4 uses all of them.
- Font and cell metrics come from `font::cached_font_config()` (`font.rs:290`, `pub(super)`, reachable from `element::thumbnail` because a child module sees its parent's private items). **Do not use `font::base_font()`**: it is `#[cfg(test)]`-gated (`font.rs:449-451`, and its re-export at `element/mod.rs:30-32` is `cfg(test)` too), so a production call does not compile. Do not propose un-gating it.

- [ ] **Step 1: Declare the module**

In `src-app/src/terminal/element/mod.rs`, find the existing module declarations near the top (`mod paint;` and siblings) and add, keeping alphabetical order among the private ones:

```rust
mod thumbnail;
```

- [ ] **Step 2: Write the failing tests**

Create `src-app/src/terminal/element/thumbnail.rs` with only this test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::TerminalState;

    /// The trap this whole module exists to avoid.
    ///
    /// `TerminalElement::build_layout` calls `notify_window_size`, which
    /// SIGWINCHes the child process to the element's bounds. A card-sized
    /// `TerminalElement` would resize every displayed pane to 320x132 px worth
    /// of cells while the real pane resized it back, corrupting the layout of
    /// every pane the overlay showed. The thumbnail path must never touch the
    /// grid.
    #[test]
    fn thumbnail_never_resizes_the_pty() {
        let state = TerminalState::new_display_only(24, 80);
        let backend = state.session_backend();

        let before = backend.grid_metrics();
        for _ in 0..5 {
            let _ = thumbnail_snapshot(&backend);
        }
        let after = backend.grid_metrics();

        assert_eq!(
            (before.columns, before.screen_lines),
            (after.columns, after.screen_lines),
            "a thumbnail snapshot resized the grid"
        );
    }

    #[test]
    fn the_crop_is_the_last_rows_of_the_viewport() {
        let state = TerminalState::new_display_only(24, 80);
        let snap = thumbnail_snapshot(&state.session_backend());

        assert_eq!(snap.last_visible_row, snap.content.rows as i32);
        assert_eq!(
            snap.first_visible_row,
            snap.content.rows as i32 - THUMBNAIL_ROWS as i32
        );
    }

    /// A grid shorter than the crop depth must render all of it, not underflow.
    #[test]
    fn a_short_grid_crops_to_itself() {
        let state = TerminalState::new_display_only(5, 40);
        let snap = thumbnail_snapshot(&state.session_backend());

        assert!(snap.content.rows <= THUMBNAIL_ROWS);
        assert_eq!(snap.first_visible_row, 0);
        assert_eq!(snap.last_visible_row, snap.content.rows as i32);
    }

    /// 9 px is above the quantization floor at the DEFAULT multipliers:
    /// `round(9 * 0.6) = 5` and `round(9 * 1.2) = 11`, so a 320x132 band is
    /// exactly 64 columns by 12 rows. Below ~4 px cell width the rounding
    /// dominates and columns drift, which is why the design crops rather than
    /// scaling the whole grid.
    ///
    /// The multipliers are config-driven (`settings.cell_width` /
    /// `settings.line_height`), so this test pins the band against the
    /// defaults explicitly rather than reading the developer's own config
    /// through `cached_font_config()`.
    #[test]
    fn the_thumbnail_band_is_a_whole_number_of_cells() {
        use super::super::font::{DEFAULT_CELL_WIDTH, DEFAULT_LINE_HEIGHT, FontSettings};

        let defaults = FontSettings {
            font: gpui::font("JetBrainsMono Nerd Font Mono"),
            size: 13.0,
            line_height: DEFAULT_LINE_HEIGHT,
            cell_width: DEFAULT_CELL_WIDTH,
        };
        let dims = thumbnail_cell_dimensions_for(&defaults);
        assert_eq!(f32::from(dims.cell_width), 5.0);
        assert_eq!(f32::from(dims.line_height), 11.0);
        assert_eq!(THUMBNAIL_BAND_W / f32::from(dims.cell_width), 64.0);
        assert_eq!(
            THUMBNAIL_BAND_H / f32::from(dims.line_height),
            THUMBNAIL_ROWS as f32
        );
    }
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p paneflow-app terminal::element::thumbnail 2>&1 | tail -25`
Expected: FAIL to compile — `cannot find function thumbnail_snapshot in this scope`, etc.

- [ ] **Step 4: Write the implementation**

Prepend to `src-app/src/terminal/element/thumbnail.rs`:

```rust
//! Read-only miniature render of a pane's terminal grid (issue #339).
//!
//! # Why this is not a small `TerminalElement`
//!
//! `TerminalElement::build_layout` calls `self.backend.notify_window_size(..)`
//! as a side effect of layout, which SIGWINCHes the child process to the
//! element's bounds. There is no flag to suppress it. A card-sized
//! `TerminalElement` would therefore resize the PTY to the card every frame
//! while the real pane resized it back.
//!
//! This module hangs off `layout_from_snapshot` instead - the `Window`-free,
//! `App`-free pure layout pass built so the golden-frame tests could assert
//! layout with no GPU. It takes cell dimensions and the base font as plain
//! values, so it lays out at any scale.
//!
//! It lives under `element/` because `LayoutState`'s fields, `mod paint`, and
//! `CellGeometry` are all private to `element`, and a child module can see its
//! parent's private items.

use gpui::{Font, Pixels, px};

use super::CellDimensions;
use crate::terminal::TerminalSessionBackend;
use crate::terminal::types::{Content, TerminalWindowSize, terminal_metric_to_u16};

/// Rows of the pane's viewport a card shows, counted from the bottom.
///
/// Bottom-crop, always - one rule, one test. It loses a full-screen TUI's
/// header row; that was weighed against centring on the cursor (which makes
/// the card jitter between refreshes) and against pinning row 0 (two layout
/// passes per card), and the fixed rule won on determinism.
pub(super) const THUMBNAIL_ROWS: usize = 12;

/// Font size for a thumbnail, in pixels.
///
/// Cell geometry is a pure function of this scalar and the two configured
/// multipliers - `font::cell_dimensions` (`font.rs:579-584`) computes
/// `cell_width = round(size * settings.cell_width)` and
/// `line_height = round(size * settings.line_height)`, with no glyph
/// measurement. At the 0.6 / 1.2 defaults (`font.rs:26-27`) 9 px yields
/// exactly 5x11 px cells.
pub(super) const THUMBNAIL_FONT_PX: f32 = 9.0;

/// Thumbnail band size, in pixels. These are the DEFAULT-derived figure:
/// 64 columns x 12 rows at 5x11 px cells. They stay hardcoded on purpose -
/// the card box is sized to them - and a user with non-default
/// `cell_width` / `line_height` gets a different number of cells in the same
/// band, not a different band. `the_thumbnail_band_is_a_whole_number_of_cells`
/// pins them against the defaults.
pub(super) const THUMBNAIL_BAND_W: f32 = 320.0;
pub(super) const THUMBNAIL_BAND_H: f32 = 132.0;

/// Cell metrics for a thumbnail under `settings`: the same two multipliers
/// the pane uses, applied to the thumbnail font size through the same
/// rounding as the pane (`font::cell_dimensions`).
pub(super) fn thumbnail_cell_dimensions_for(settings: &super::font::FontSettings) -> CellDimensions {
    super::font::cell_dimensions(settings, px(THUMBNAIL_FONT_PX))
}

/// Cell metrics for a thumbnail under the live config.
pub(super) fn thumbnail_cell_dimensions() -> CellDimensions {
    thumbnail_cell_dimensions_for(&super::font::cached_font_config())
}

/// A grid snapshot plus the row window a thumbnail paints.
pub(super) struct ThumbnailSnapshot {
    pub content: Content,
    /// Cull range `[first, last)`. Cells keep their absolute line numbers
    /// through `layout_from_snapshot`, which culls by line index rather than
    /// renumbering, so the painter shifts its origin up by `first` rows.
    pub first_visible_row: i32,
    pub last_visible_row: i32,
}

/// Read one pane's current grid without touching it.
///
/// `clear_on_resize: false` is load-bearing: the `true` branch mutates
/// `ResizeState` and calls `submit_requested_resize`. On the `false` path the
/// window size is consumed only by `normalized_window_size` and discarded, and
/// the two visible-row arguments are ignored outright (culling happens later,
/// in `layout_from_snapshot`) - but the size is still derived honestly from
/// the pane's own metrics rather than relying on arguments being inert.
///
/// The snapshot is the VIEWPORT, not scrollback, with `display_offset` already
/// applied, so a pane the user has scrolled up shows in its thumbnail exactly
/// what the pane itself is showing.
pub(super) fn thumbnail_snapshot(backend: &TerminalSessionBackend) -> ThumbnailSnapshot {
    let metrics = backend.grid_metrics();
    let dims = thumbnail_cell_dimensions();
    let window_size = TerminalWindowSize::new(
        metrics.columns,
        metrics.screen_lines,
        terminal_metric_to_u16(dims.cell_width.as_f32()),
        terminal_metric_to_u16(dims.line_height.as_f32()),
    );
    let (content, _initial_clear_consumed) = backend.render_content(window_size, 0, 0, false);

    let last_visible_row = content.rows as i32;
    let first_visible_row = (content.rows.saturating_sub(THUMBNAIL_ROWS)) as i32;
    ThumbnailSnapshot {
        content,
        first_visible_row,
        last_visible_row,
    }
}

/// Base font for a thumbnail: the same family the pane uses, at thumbnail size.
///
/// `font::cached_font_config()` (`font.rs:290`) is `pub(super)` in
/// `element/font.rs` and takes no `&mut Window`, so the thumbnail resolves its
/// own font rather than having one threaded in from the overlay. It is the
/// same 500 ms mtime-cached read the renderer's `resolve_frame_metrics`
/// makes, so a font change reaches thumbnails and panes together. Note it
/// does NOT go through `resolve_frame_metrics` itself, whose `size_override`
/// is clamped to [8.0, 32.0] pt and so could not produce a 9 px thumbnail
/// face. (`font::base_font()` would read the same way but is
/// `#[cfg(test)]`-gated at `font.rs:449-451`: a production call does not
/// compile.)
pub(super) fn thumbnail_font() -> (Font, Pixels) {
    (super::font::cached_font_config().font, px(THUMBNAIL_FONT_PX))
}
```

`mod font;` is private in `element/mod.rs`; `element::thumbnail` still reaches
`super::font::cached_font_config()`, `super::font::cell_dimensions` and
`super::font::FontSettings` (all `pub(super)`) because a child module sees its parent's private
items. `DEFAULT_CELL_WIDTH` / `DEFAULT_LINE_HEIGHT` are `pub(crate)`.

If `terminal_metric_to_u16` is not re-exported from `crate::terminal::types`, import it from wherever `element/mod.rs` imports it — it is defined at `src-app/src/terminal/types.rs:71`.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p paneflow-app terminal::element::thumbnail 2>&1 | tail -20`
Expected: PASS — 4 tests, including `thumbnail_never_resizes_the_pty`.

- [ ] **Step 6: Format, lint, commit**

```bash
cargo fmt
cargo clippy -p paneflow-app --all-targets 2>&1 | tail -5
git add src-app/src/terminal/element/thumbnail.rs src-app/src/terminal/element/mod.rs
git commit -m "feat(terminal): read-only thumbnail snapshot that never resizes the pty (#339)"
```

---

## Task 4: The `TerminalThumbnail` element

**Files:**
- Modify: `src-app/src/terminal/element/thumbnail.rs`

**Interfaces:**
- Consumes: everything Task 3 produced, plus `super::{layout_from_snapshot, LayoutInputs, CellGeometry}` and `super::paint::*`.
- Produces: `pub(crate) struct TerminalThumbnail` with `TerminalThumbnail::new(backend, theme)` — two arguments; the font is resolved inside via `thumbnail_font()`, not passed in. Task 7 constructs one per live card.
- **Snapshot lifetime rule (spec §4.2e / §5).** `CellMirror::publish` (`ghostty_session.rs:4241-4245`) reuses its recycled back buffer only when `Arc::get_mut` succeeds; a thumbnail holding a `Content.cells` clone across a publish forces a full-grid conversion for that pane that frame. So: the `ThumbnailSnapshot` is a prepaint local, the `LayoutState` is `take()`n and dropped in paint, and `TerminalThumbnail` stores no `Content`, no `Arc<[Cell]>`, and no `LayoutState` field. Never cache any of them across frames.

- [ ] **Step 1: Write the element**

Append to `src-app/src/terminal/element/thumbnail.rs`, above the test module:

```rust
use std::sync::Arc;

use gpui::{
    App, Bounds, ContentMask, Element, ElementId, GlobalElementId, InspectorElementId, LayoutId,
    Point, Style, Window, relative,
};

use super::geometry::CellGeometry;
use super::{LayoutInputs, LayoutState, cursor_from_content, layout_from_snapshot, paint};
use crate::terminal::types::CursorShape;
use crate::theme::TerminalTheme;

/// A pane's last [`THUMBNAIL_ROWS`] rows, painted read-only into a card.
///
/// Holds no snapshot: `ThumbnailSnapshot` lives only inside `prepaint`, and
/// the `LayoutState` it produces is consumed and dropped by `paint`. Keeping
/// either across frames would hold the pane's `Content.cells` `Arc` across
/// the runtime thread's next `CellMirror::publish`, defeating its
/// `Arc::get_mut` buffer reuse and forcing a full-grid conversion.
pub(crate) struct TerminalThumbnail {
    backend: TerminalSessionBackend,
    theme: Arc<TerminalTheme>,
    /// Row offset the paint pass shifts the origin by, carried from prepaint.
    first_visible_row: i32,
}

impl TerminalThumbnail {
    pub(crate) fn new(backend: TerminalSessionBackend, theme: Arc<TerminalTheme>) -> Self {
        Self {
            backend,
            theme,
            first_visible_row: 0,
        }
    }
}

impl Element for TerminalThumbnail {
    type RequestLayoutState = ();
    type PrepaintState = Option<LayoutState>;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = relative(1.).into();
        let layout_id = window.request_layout(style, [], cx);
        (layout_id, ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        _cx: &mut App,
    ) -> Self::PrepaintState {
        // Cull off-screen cards. This is the culling mechanism the frame
        // budget depends on: a card scrolled out of the overlay's viewport
        // does no terminal work at all, not even the snapshot lock read.
        let visible = window.content_mask().bounds;
        if bounds.intersect(&visible).is_empty() {
            return None;
        }

        let snap = thumbnail_snapshot(&self.backend);
        self.first_visible_row = snap.first_visible_row;

        let dims = thumbnail_cell_dimensions();
        let (base_font, _size) = thumbnail_font();
        // A dim, non-blinking block cursor (spec §4.3): a useful "parked at a
        // prompt" signal for one quad. `cursor_from_content` is the private
        // helper `build_layout` uses (`element/mod.rs:533`); `focused: true`
        // because the helper returns `None` for an unfocused pane and a
        // thumbnail is never "unfocused". It filters `CursorShape::Hidden`
        // itself. The shape is then forced to `Block` regardless of the pane's
        // own beam/underline/vintage mode - a 5 px beam is invisible - and
        // `text` is cleared so the block does not try to re-shape the glyph
        // under it. Blink is not a layout input: it only gates whether
        // `paint` draws the cursor, and this element always draws it.
        // `CursorInfo`'s fields are private to `element`, which this child
        // module can see.
        let cursor = cursor_from_content(
            snap.content.cursor,
            true,
            self.theme.cursor.opacity(0.5),
            CursorShape::Block,
            &self.theme,
        )
        .map(|mut c| {
            c.shape = CursorShape::Block;
            c.text = None;
            c
        });
        Some(layout_from_snapshot(LayoutInputs {
            // The only `Arc<[Cell]>` clone this element makes; `snap` goes
            // out of scope at the end of prepaint and the `LayoutState` is
            // dropped in `paint`.
            cells: snap.content.cells.clone(),
            cursor,
            // Selection, copy mode, and search belong to the live pane.
            selection_range: None,
            copy_mode_cursor: None,
            search_highlights: &[],
            display_offset: snap.content.display_offset,
            history_size: snap.content.history_size,
            desired_cols: snap.content.cols.max(1),
            desired_rows: snap.content.rows.max(1),
            first_visible_row: snap.first_visible_row,
            last_visible_row: snap.last_visible_row,
            dims,
            base_font,
            theme: &self.theme,
            exited: None,
            exit_signal: None,
            integrated_glyphs_enabled: true,
            color_emoji_enabled: false,
        }))
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let Some(layout) = prepaint.take() else {
            return;
        };
        let dims = thumbnail_cell_dimensions();
        // Cells keep their absolute line numbers, so lift the origin by the
        // cropped-away rows to land the band at the top of the card.
        let origin = Point {
            x: bounds.origin.x,
            y: bounds.origin.y - dims.line_height * (self.first_visible_row as f32),
        };
        let geom = CellGeometry {
            origin,
            cell_width: dims.cell_width,
            line_height: dims.line_height,
        };
        let (cell_x_bounds, cell_y_bounds) = if layout.desired_cols == 0 || layout.desired_rows == 0
        {
            (Vec::new(), Vec::new())
        } else {
            (
                geom.x_boundaries(layout.desired_cols),
                geom.y_boundaries(layout.desired_rows),
            )
        };
        let (base_font, font_size) = thumbnail_font();

        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            // No-op here: the band's background is transparent (the card
            // paints `theme.background` under it) and `paint_base_fill` only
            // emits a quad when `background_color.a > 0.0`
            // (`paint/background.rs:23`). Kept so the sequence mirrors
            // `TerminalElement::paint` and a reader does not go looking for
            // the missing fill.
            paint::background::paint_base_fill(&layout, bounds, window);
            paint::background::paint_cell_backgrounds(
                &layout,
                bounds,
                &cell_x_bounds,
                &cell_y_bounds,
                window,
            );
            paint::background::paint_block_quads(&layout, &cell_x_bounds, &cell_y_bounds, window);
            paint::box_drawing::paint_box_drawing_glyphs(
                &layout,
                &cell_x_bounds,
                &cell_y_bounds,
                window,
            );
            paint::text::paint_text_runs(&layout, &geom, &base_font, font_size, window, cx);
            // The dim block cursor built in prepaint. Unconditional: there is
            // no blink phase here.
            paint::cursor::paint_cursor(&layout, &geom, &base_font, font_size, window, cx);
        });
        // `layout` (and with it the pane's `Arc<[Cell]>`) drops here. Do not
        // stash it on `self` for the next frame - see the struct doc.
        drop(layout);
        // Deliberately not painted: selection, search highlights, hyperlink
        // underlines, IME preedit, the scrollbar and its match rail, and Kitty
        // graphics. Kitty in particular: each pane carries a 32 MiB image cap,
        // and decoding placements into a 320px card is not a trade worth
        // making.
    }
}

impl gpui::IntoElement for TerminalThumbnail {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}
```

- [ ] **Step 2: Build and fix visibility**

Run: `cargo build -p paneflow-app 2>&1 | tail -30`

Expected: it may fail on module privacy. Fix by widening in `src-app/src/terminal/element/mod.rs` **only as far as `pub(super)`**, never `pub(crate)`:
- if `mod paint;` is not visible, change the `mod geometry;`/`mod paint;` declarations the compiler names to `pub(super) mod`;
- if `LayoutState`'s fields are unreachable, they are private to `element` and a child module CAN read them — a failure here means the file was created outside `element/`;
- the same holds for `cursor_from_content` (a private fn at `element/mod.rs:533`) and `CursorInfo`'s private fields (`element/mod.rs:457`): reachable from `element::thumbnail`, and a failure means the file is in the wrong place;
- `CellGeometry`'s fields are `pub(super)` in `geometry.rs`, which is `pub(in element)` and therefore visible.

Re-run until it builds clean.

- [ ] **Step 3: Run the module's tests**

Run: `cargo test -p paneflow-app terminal::element::thumbnail 2>&1 | tail -20`
Expected: PASS — the same 4 tests from Task 3, still green.

- [ ] **Step 4: Format, lint, commit**

```bash
cargo fmt
cargo clippy -p paneflow-app --all-targets 2>&1 | tail -5
git add src-app/src/terminal/element/
git commit -m "feat(terminal): TerminalThumbnail element paints a cropped pane preview (#339)"
```

---

## Task 5: Action, keybinding, registry, and the CLAUDE.md count

**Files:**
- Modify: `src-app/src/app/actions.rs:175`
- Modify: `src-app/src/keybindings/registry.rs:673`
- Modify: `src-app/src/keybindings/defaults.rs:472`
- Modify: `src-app/src/keybindings/apply.rs` (new guard test)
- Modify: `CLAUDE.md:192`, `CLAUDE.md:382`

**Interfaces:**
- Produces: `crate::OpenPaneOverview` (a zero-sized action struct in the `paneflow` namespace) and the registry name `"open_pane_overview"`. Tasks 6, 9, and 10 dispatch it.

- [ ] **Step 1: Add the action**

In `src-app/src/app/actions.rs`, the last entry (`TogglePrimarySidebar`, line 175) has **no trailing comma** and the block must literally end with `]\n);`. Add a comma to that entry and append:

```rust
        TogglePrimarySidebar,
        // Issue #339: Pane Overview - every open terminal pane across every
        // workspace and tab, grouped, with a cropped live preview each.
        // Global context: a terminal holds focus nearly all the time, so a
        // scoped binding would be dead exactly when it is wanted.
        OpenPaneOverview
    ]
);
```

- [ ] **Step 2: Run the count test to verify it fails**

Run: `cargo test -p paneflow-app claude_md_action_count 2>&1 | tail -12`
Expected: FAIL — `CLAUDE.md says '92 GPUI action types' but 'actions!' declares 93`.

- [ ] **Step 3: Update both CLAUDE.md numbers**

Both sites must move together — the test checks each phrase.

`CLAUDE.md:192`:
```
│   ├── actions.rs                     ← 93 GPUI action types (paneflow namespace)
```

`CLAUDE.md:382`: change `via \`cx.bind_keys()\`. 92 actions total` to `93 actions total`.

- [ ] **Step 4: Run the count test to verify it passes**

Run: `cargo test -p paneflow-app claude_md_action_count 2>&1 | tail -8`
Expected: PASS.

- [ ] **Step 5: Register the action**

In `src-app/src/keybindings/registry.rs`, append inside `ACTIONS` immediately before the closing `];` (line 674):

```rust
    // Issue #339: Pane Overview. `ShortcutGroup::Agents` alongside the other
    // cockpit overlays, so Settings > Keyboard Shortcuts files it with them.
    // No `display.rs` edit is needed - that page is generated from this table,
    // and two of its tests fail if an action is missing from it.
    ActionMeta {
        name: "open_pane_overview",
        factory: || Box::new(crate::OpenPaneOverview),
        context: "",
        description: "Pane overview",
        group: ShortcutGroup::Agents,
    },
```

- [ ] **Step 6: Bind the chord**

In `src-app/src/keybindings/defaults.rs`, after the `open_launch_pad` entry (line 477), append:

```rust
    // Issue #339: Pane Overview. `p` for panes; `secondary-shift-p` is free on
    // this table today and stays clear of `secondary-shift-a` (attention
    // queue) and `secondary-shift-l` (launch pad), the two chords a user
    // reaches for in the same breath. Global for the same reason those are:
    // a terminal holds focus nearly always. Pinned by
    // `pane_overview_chord_is_bindable_and_does_not_collide` in `apply.rs`.
    DefaultBinding {
        key: "secondary-shift-p",
        action_name: "open_pane_overview",
        context: None,
    },
```

- [ ] **Step 7: Write the failing guard test**

In `src-app/src/keybindings/apply.rs`, inside the existing `mod tests`, after `primary_sidebar_chord_is_bindable_and_does_not_collide` (which ends at line 534), add:

```rust
    /// Issue #339: the Pane Overview chord is bindable, global, and claimed by
    /// exactly one default. `keystrokes_conflict` normalizes modifier order
    /// and the `secondary` shorthand, so a chord picked by eye rather than by
    /// this assertion could silently shadow another cockpit overlay instead of
    /// failing loudly.
    #[test]
    fn pane_overview_chord_is_bindable_and_does_not_collide() {
        use super::super::defaults::DEFAULTS;

        let key = "secondary-shift-p";
        let action_name = "open_pane_overview";

        let context = context_for_action(action_name);
        assert_eq!(
            context, None,
            "the overview is global: scoping it would make it dead while a \
             terminal holds focus, which is nearly always"
        );
        let action = action_from_name(action_name).expect("registered action");
        assert!(
            make_binding(key, action, context).is_some(),
            "{key} must parse into a valid KeyBinding"
        );

        let claimants: Vec<&str> = DEFAULTS
            .iter()
            .chain(MACOS_ONLY_DEFAULTS.iter())
            .filter(|d| keystrokes_conflict(d.key, key))
            .map(|d| d.action_name)
            .collect();
        assert_eq!(
            claimants,
            vec![action_name],
            "{key} must be claimed by exactly one default on this platform"
        );

        for neighbour in ["secondary-shift-a", "secondary-shift-l"] {
            assert!(
                !keystrokes_conflict(key, neighbour),
                "{key} must stay distinct from {neighbour}"
            );
        }
    }
```

- [ ] **Step 8: Run the keybinding tests**

Run: `cargo test -p paneflow-app keybindings 2>&1 | tail -20`
Expected: PASS, including the new test and the table-wide
`no_two_default_actions_claim_the_same_chord_in_the_same_context`.

- [ ] **Step 9: Format, lint, commit**

```bash
cargo fmt
cargo clippy -p paneflow-app --all-targets 2>&1 | tail -5
git add src-app/src/app/actions.rs src-app/src/keybindings/ CLAUDE.md
git commit -m "feat(keybindings): OpenPaneOverview on Cmd+Shift+P (#339)"
```

---

## Task 6: Overlay state machine

**Files:**
- Modify: `src-app/src/app/pane_overview/mod.rs`
- Modify: `src-app/src/main.rs` (struct fields ~1509, `.on_action` ~2224)
- Modify: `src-app/src/app/bootstrap.rs` (field init ~862)

**Interfaces:**
- Consumes: Task 1's `rows` module, Task 2's `teleport_to_surface`, Task 5's `crate::OpenPaneOverview`.
- Produces: `PaneOverviewState`, `PaneFlowApp::{handle_open_pane_overview, close_pane_overview, close_pane_overview_and_restore_focus, pane_overview_activate, handle_pane_overview_key_down, collect_pane_overview_cards}`. Task 7 renders from these.

- [ ] **Step 1: Add the state fields**

In `src-app/src/main.rs`, after the Launch Pad fields (`launch_pad_focus: FocusHandle,` around line 1527), add:

```rust
    /// Issue #339: Pane Overview overlay, `None` = closed. Cards are derived
    /// from the live workspace tree on every render, never stored - a pane
    /// that closes while the overlay is open disappears at the next repaint.
    pane_overview: Option<app::pane_overview::PaneOverviewState>,
    pane_overview_focus: FocusHandle,
```

In `src-app/src/app/bootstrap.rs`, after `launch_pad_focus: cx.focus_handle(),` (around line 872), add:

```rust
            // Issue #339: Pane Overview closed.
            pane_overview: None,
            pane_overview_focus: cx.focus_handle(),
```

- [ ] **Step 2: Register the action handler**

In `src-app/src/main.rs`, after `.on_action(cx.listener(Self::handle_open_launch_pad))` (line 2225), add:

```rust
            .on_action(cx.listener(Self::handle_open_pane_overview))
```

- [ ] **Step 3: Write the state machine**

Append to `src-app/src/app/pane_overview/mod.rs`:

```rust
use gpui::{Context, FocusableView, KeyDownEvent, Window};

use crate::PaneFlowApp;
use crate::limits::clamp_untrusted_label;
use rows::{
    CardMeta, MAX_LIVE_THUMBNAILS, filter_cards, flat_order, group_cards, initial_selection,
    move_selection,
};

/// Open-overlay state. Cards are never cached here - only the query and the
/// cursor - so live agent state and live terminal content cannot go stale.
pub(crate) struct PaneOverviewState {
    pub query: String,
    pub selected: usize,
    /// The pane that held focus when the overlay opened - the card that
    /// carries the "current" marker. Captured once at open: while the overlay
    /// is up its own focus handle holds focus, so it cannot be re-derived on
    /// render. `None` when no terminal pane was focused.
    pub current: Option<u64>,
    /// Cards per wrapped row, captured from the last render's real pixel
    /// width. Up/Down move by this. Captured rather than estimated for the
    /// same reason `Container::container_size` is: the old hardcoded 800px
    /// container guess in `split.rs` was wrong at every other window size.
    pub cards_per_row: usize,
}

impl Default for PaneOverviewState {
    fn default() -> Self {
        Self {
            query: String::new(),
            selected: 0,
            current: None,
            cards_per_row: 1,
        }
    }
}

impl PaneFlowApp {
    pub(crate) fn handle_open_pane_overview(
        &mut self,
        _: &crate::OpenPaneOverview,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Same mode gate as the other cockpit overlays: a mode switch must
        // not leave cockpit chrome painted over Agents/Review.
        if !matches!(self.mode, paneflow_config::schema::AppMode::Cli) {
            return;
        }
        if self.pane_overview.is_some() {
            self.close_pane_overview_and_restore_focus(window, cx);
            return;
        }
        // Product decision (2026-09-03): open on the current pane's card, so
        // Esc then Enter is a no-op round trip. `is_active` is the focused
        // pane, resolved while the terminal still holds focus - i.e. BEFORE
        // the overlay takes it below.
        let cards = self.collect_pane_overview_cards(window, cx);
        let current = cards.iter().find(|c| c.is_active).map(|c| c.surface_id);
        let order = flat_order(&group_cards(cards));
        self.pane_overview = Some(PaneOverviewState {
            selected: initial_selection(&order, current),
            current,
            ..PaneOverviewState::default()
        });
        self.pane_overview_focus.focus(window, cx);
        cx.notify();
    }

    pub(crate) fn close_pane_overview(&mut self, cx: &mut Context<Self>) {
        self.pane_overview = None;
        cx.notify();
    }

    pub(crate) fn close_pane_overview_and_restore_focus(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_pane_overview(cx);
        // Issue #108: fall back to the empty-workspace placeholder when the
        // workspace we are restoring focus to has no pane.
        let focused = match self.workspaces.get(self.active_idx) {
            Some(ws) => ws.focus_first(window, cx),
            None => false,
        };
        if !focused {
            window.focus(&self.empty_workspace_focus, cx);
        }
    }

    /// Click / Enter on a card. The surface is re-resolved at activation time,
    /// so a pane closed between render and click is a clean no-op.
    pub(crate) fn pane_overview_activate(
        &mut self,
        surface_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.teleport_to_surface(surface_id, window, cx) {
            self.close_pane_overview(cx);
        }
    }

    /// Every terminal pane, in workspace -> tab -> traversal order.
    ///
    /// Walks `ws.tabs()` and NOT `ws.active_tab()`: the Attention Queue and
    /// Fleet Search both visit only the active tab, and inheriting that here
    /// would hide most of what the overview exists to show. It also avoids
    /// `Workspace::collect_panes`, which dedupes with a linear `contains` per
    /// pane - `Tab::collect_panes` already dedupes `root` against
    /// `saved_layout`, so a zoomed tab yields each pane exactly once.
    ///
    /// Takes a `&Window` because `is_active` is the FOCUSED pane, and focus
    /// is a window property: `LayoutTree::focused_pane(window, cx)`
    /// (`layout/queries.rs:12`) is the accessor - the same one
    /// `workspace_ops/focus.rs:29`, `workspace_ops/layout.rs:67` and
    /// `workspace_ops/mod.rs:1671` use - and it walks the tree testing each
    /// leaf's `focus_handle(cx).is_focused(window)`. There is no
    /// `Workspace::focused_pane`; go through `ws.active_tab().root`.
    pub(crate) fn collect_pane_overview_cards(
        &self,
        window: &Window,
        cx: &Context<Self>,
    ) -> Vec<CardMeta> {
        let mut cards = Vec::new();
        for (ws_idx, ws) in self.workspaces.iter().enumerate() {
            let ws_is_active = ws_idx == self.active_idx;
            let active_tab_idx = ws.active_tab_idx();
            // The one pane that is "current": only the active workspace's
            // active tab can hold focus, so resolve it once per workspace and
            // only there. `None` when focus sits in chrome, in a markdown or
            // diff pane, or on the empty-workspace placeholder.
            let focused_pane = if ws_is_active {
                ws.active_tab()
                    .root
                    .as_ref()
                    .and_then(|root| root.focused_pane(window, cx))
            } else {
                None
            };
            for (tab_idx, tab) in ws.tabs().iter().enumerate() {
                let tab_title = crate::app::sidebar::tab_row_title(tab, tab_idx, cx);
                for pane in tab.collect_panes() {
                    let pane_ref = pane.read(cx);
                    // Terminals only: markdown and diff panes are omitted.
                    let Some(terminal) = pane_ref.active_terminal_opt() else {
                        continue;
                    };
                    let surface_id = terminal.entity_id().as_u64();
                    let state = ws
                        .agent_sessions
                        .values()
                        .find(|s| s.surface_id == Some(surface_id))
                        .map(|s| s.state.clone());
                    let view = terminal.read(cx);
                    let metrics = view.terminal.session_backend().grid_metrics();
                    cards.push(CardMeta {
                        surface_id,
                        ws_idx,
                        ws_title: clamp_untrusted_label(&ws.title),
                        tab_idx,
                        tab_title: clamp_untrusted_label(&tab_title),
                        name: clamp_untrusted_label(&crate::pane::Pane::surface_title(
                            &pane_ref.surface,
                            cx,
                        )),
                        cwd_label: view
                            .terminal
                            .current_cwd
                            .as_ref()
                            .and_then(|p| p.file_name())
                            .map(|n| n.to_string_lossy().to_string()),
                        agent: view.terminal.detected_agent,
                        state,
                        cols: metrics.columns,
                        rows: metrics.screen_lines,
                        exited: view.terminal.exit_status().is_some(),
                        // The focused pane, not merely a pane of the active
                        // tab: `Entity<Pane>` compares by entity id, so this
                        // is true for exactly one card at most.
                        is_active: tab_idx == active_tab_idx
                            && focused_pane.as_ref() == Some(&pane),
                        ws_is_active,
                        ws_branch: ws.git_branch.clone(),
                    });
                }
            }
        }
        cards
    }

    pub(crate) fn handle_pane_overview_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = event.keystroke.key.as_str();
        let Some(state) = self.pane_overview.as_ref() else {
            return;
        };
        let per_row = state.cards_per_row.max(1) as isize;
        let cards = filter_cards(&self.collect_pane_overview_cards(window, cx), &state.query);
        let order = flat_order(&group_cards(cards));
        let len = order.len();
        let selected = state.selected.min(len.saturating_sub(1));

        let delta = match key {
            "escape" => {
                self.close_pane_overview_and_restore_focus(window, cx);
                return;
            }
            "enter" if len > 0 => {
                if let Some(sid) = order.get(selected).copied() {
                    self.pane_overview_activate(sid, window, cx);
                }
                return;
            }
            "left" => -1,
            "right" => 1,
            "up" => -per_row,
            "down" => per_row,
            _ => return,
        };
        if let Some(state) = self.pane_overview.as_mut() {
            state.selected = move_selection(selected, len, delta);
        }
        cx.notify();
    }
}
```

`ws.git_branch` is a `pub String` on `Workspace` (`workspace/mod.rs:124`), empty when the
workspace is not a checkout; the sidebar reads it the same way (`sidebar/mod.rs:2110`).

If `TerminalState` exposes the exit status under a different name than `exit_status()`, find it with
`grep -n "exit" src-app/src/terminal/pty_session.rs | grep -i "pub"` and use that accessor; the field
is only used to dim an exited card. Likewise confirm `current_cwd`'s type with
`grep -n "current_cwd" src-app/src/terminal/pty_session.rs`.

- [ ] **Step 4: Build**

Run: `cargo build -p paneflow-app 2>&1 | tail -30`
Expected: exit 0. Fix any accessor-name mismatches the compiler reports.

- [ ] **Step 5: Format, lint, commit**

```bash
cargo fmt
cargo clippy -p paneflow-app --all-targets 2>&1 | tail -5
git add src-app/src/app/pane_overview/mod.rs src-app/src/main.rs src-app/src/app/bootstrap.rs
git commit -m "feat(pane_overview): overlay state, enumeration and key handling (#339)"
```

---

## Task 7: Overlay render + refresh tick

**Files:**
- Modify: `src-app/src/app/pane_overview/mod.rs`
- Modify: `src-app/src/main.rs` (render composition ~2542)
- Modify: `src-app/src/app/bootstrap.rs` (refresh loop, after the 50 ms automation loop at ~487)

**Interfaces:**
- Consumes: Task 4's `TerminalThumbnail`, Task 6's state machine, Task 1's `live_thumbnail_ids`.
- Produces: `PaneFlowApp::render_pane_overview(&mut self, window, cx) -> AnyElement`. It takes the `&Window` because `collect_pane_overview_cards` needs it for the focused pane; `PaneFlowApp::render` (`main.rs:1841`) already has one in scope.
- Renders three things the first draft of this plan left out: the workspace header's **branch** and **active marker** (`WorkspaceGroup::{branch, is_active}`), and a **"current" marker** on the focused pane's card (`CardMeta::is_active`), distinct from the moving selection fill.

- [ ] **Step 1: Write the render**

Append to `src-app/src/app/pane_overview/mod.rs`. Card and grid constants first:

```rust
use gpui::{
    AnyElement, ClickEvent, InteractiveElement, IntoElement, MouseButton, ParentElement,
    SharedString, Styled, deferred, div, prelude::*, px, svg,
};

use rows::live_thumbnail_ids;

/// Card box. The thumbnail band is 320x132 (Task 3); the rest is 4px padding,
/// a 30px header (name + status) and a 26px footer (breadcrumb + grid size).
const CARD_W: f32 = 328.0;
const CARD_H: f32 = 196.0;
const CARD_GAP: f32 = 12.0;
const CARD_RADIUS: f32 = 10.0;
const GRID_PADDING: f32 = 16.0;

impl PaneFlowApp {
    pub(crate) fn render_pane_overview(
        &mut self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let ui = crate::theme::ui_colors();
        let theme = crate::theme::active_theme();
        let query = self
            .pane_overview
            .as_ref()
            .map(|s| s.query.clone())
            .unwrap_or_default();
        // Note: while the overlay is open its own focus handle holds focus,
        // so `is_active` is false for every card on re-render. That is fine -
        // the "current" marker is decided once, at open, and carried below.
        let all = self.collect_pane_overview_cards(window, cx);
        let groups = group_cards(filter_cards(&all, &query));
        let order = flat_order(&groups);
        let live = live_thumbnail_ids(&order, MAX_LIVE_THUMBNAILS);
        let selected_id = self
            .pane_overview
            .as_ref()
            .and_then(|s| order.get(s.selected.min(order.len().saturating_sub(1))).copied());
        let current_id = self.pane_overview.as_ref().and_then(|s| s.current);

        let mut body = div().flex().flex_col().gap(px(18.)).p(px(GRID_PADDING));

        if order.is_empty() {
            body = body.child(
                div()
                    .py(px(48.))
                    .flex()
                    .justify_center()
                    .text_size(px(12.))
                    .text_color(ui.muted)
                    .child(if all.is_empty() {
                        SharedString::from("No terminal panes are open")
                    } else {
                        SharedString::from(format!("No panes match “{query}”"))
                    }),
            );
        } else {
            for group in &groups {
                // Section header: title, branch, active marker (spec §7.1).
                let mut header = div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        div()
                            .text_size(px(13.))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(ui.text)
                            .child(SharedString::from(group.title.clone())),
                    );
                if !group.branch.is_empty() {
                    header = header.child(
                        div()
                            .text_size(px(11.))
                            .text_color(ui.muted)
                            .child(SharedString::from(group.branch.clone())),
                    );
                }
                if group.is_active {
                    header = header.child(
                        div()
                            .px(px(6.))
                            .py(px(1.))
                            .rounded(px(4.))
                            .bg(ui.subtle)
                            .text_size(px(10.))
                            .text_color(ui.text)
                            .child("active"),
                    );
                }
                let mut section = div().flex().flex_col().gap(px(10.)).child(header);
                for tab in &group.tabs {
                    let mut row = div()
                        .flex()
                        .flex_col()
                        .gap(px(6.))
                        .child(
                            div()
                                .text_size(px(11.))
                                .text_color(ui.muted)
                                .child(SharedString::from(tab.title.clone())),
                        );
                    let mut grid = div().flex().flex_row().flex_wrap().gap(px(CARD_GAP));
                    for card in &tab.cards {
                        grid = grid.child(self.render_pane_overview_card(
                            card,
                            live.contains(&card.surface_id),
                            selected_id == Some(card.surface_id),
                            current_id == Some(card.surface_id),
                            ui,
                            &theme,
                            cx,
                        ));
                    }
                    row = row.child(grid);
                    section = section.child(row);
                }
                body = body.child(section);
            }
        }

        let card = div()
            .id("pane-overview")
            .occlude()
            .track_focus(&self.pane_overview_focus)
            .on_key_down(cx.listener(Self::handle_pane_overview_key_down))
            .on_mouse_down_out(cx.listener(|this, _, window, cx| {
                this.close_pane_overview_and_restore_focus(window, cx);
            }))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
            .w(px(1080.))
            .max_h(px(720.))
            .flex()
            .flex_col()
            .bg(ui.overlay)
            .border_1()
            .border_color(ui.border)
            .rounded(px(12.))
            .shadow_lg()
            .overflow_hidden()
            .child(
                div()
                    .px(px(16.))
                    .py(px(10.))
                    .border_b_1()
                    .border_color(ui.border)
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_size(px(13.))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(ui.text)
                            .child("All panes"),
                    )
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(ui.muted)
                            .child(SharedString::from(format!("{} panes", order.len()))),
                    ),
            )
            .child(
                div()
                    .id("pane-overview-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(body),
            )
            .child(
                div()
                    .px(px(16.))
                    .py(px(8.))
                    .border_t_1()
                    .border_color(ui.border)
                    .text_size(px(10.))
                    .text_color(ui.muted)
                    .child("Arrows select · Enter focuses the pane · Esc closes · search by name, workspace or tab (use fleet search for pane contents)"),
            );

        deferred(
            div()
                .id("pane-overview-backdrop")
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .flex()
                .items_start()
                .justify_center()
                .pt(px(64.))
                .bg(gpui::hsla(0., 0., 0., 0.4))
                .child(card),
        )
        .with_priority(6)
        .into_any_element()
    }
```

Then the card renderer, in the same `impl` block:

```rust
    fn render_pane_overview_card(
        &self,
        card: &CardMeta,
        live: bool,
        selected: bool,
        current: bool,
        ui: crate::theme::UiColors,
        theme: &std::sync::Arc<crate::theme::TerminalTheme>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let sid = card.surface_id;
        let (dot, dot_color, status) = pane_overview_status_visual(card.state.as_ref(), ui);
        // The "current" marker (spec §7.1): the pane that held focus when the
        // overlay opened. It does not move with the selection.
        let current_chip = current.then(|| {
            div()
                .flex_none()
                .px(px(5.))
                .py(px(1.))
                .rounded(px(4.))
                .bg(ui.subtle)
                .text_size(px(9.))
                .text_color(ui.text)
                .child("current")
        });
        let mut shell = div()
            .id(SharedString::from(format!("pane-overview-card-{sid}")))
            .flex_none()
            .w(px(CARD_W))
            .h(px(CARD_H))
            .flex()
            .flex_col()
            .p(px(4.))
            .rounded(px(CARD_RADIUS))
            .border_1()
            .border_color(if selected { ui.text } else { ui.border })
            .bg(if selected { ui.subtle } else { ui.overlay })
            .cursor(gpui::CursorStyle::PointingHand)
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.pane_overview_activate(sid, window, cx);
                cx.stop_propagation();
            }))
            .child(
                div()
                    .h(px(30.))
                    .flex_none()
                    .px(px(6.))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.))
                    .child(dot)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(12.))
                            .text_color(ui.text)
                            .child(SharedString::from(card.name.clone())),
                    )
                    .children(current_chip)
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(10.))
                            .text_color(dot_color)
                            .child(status),
                    ),
            );

        let band = div()
            .flex_none()
            .w(px(crate::terminal::element::THUMBNAIL_BAND_W))
            .h(px(crate::terminal::element::THUMBNAIL_BAND_H))
            .overflow_hidden()
            .rounded(px(6.))
            .bg(theme.background);
        let band = if live {
            match self.pane_overview_backend(sid, cx) {
                Some(backend) => band.child(
                    crate::terminal::element::TerminalThumbnail::new(backend, theme.clone()),
                ),
                None => band,
            }
        } else {
            band.child(
                div()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(10.))
                    .text_color(ui.muted)
                    .child("Preview paused"),
            )
        };
        shell = shell.child(if card.exited { band.opacity(0.5) } else { band });

        shell
            .child(
                div()
                    .h(px(26.))
                    .flex_none()
                    .px(px(6.))
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .text_size(px(10.))
                    .text_color(ui.muted)
                    .child(SharedString::from(format!(
                        "{} › {}",
                        card.ws_title, card.tab_title
                    )))
                    .child(SharedString::from(format!("{}×{}", card.cols, card.rows))),
            )
            .into_any_element()
    }

    /// The backend for one card's thumbnail, resolved by surface id so a pane
    /// closed since `collect_pane_overview_cards` yields `None` rather than a
    /// stale handle. The thumbnail resolves its own font internally.
    fn pane_overview_backend(
        &self,
        surface_id: u64,
        cx: &Context<Self>,
    ) -> Option<crate::terminal::TerminalSessionBackend> {
        let loc =
            crate::app::ipc_handler::find_pane_by_surface_id(&self.workspaces, surface_id, cx)?;
        let terminal = loc.pane.read(cx).active_terminal_opt()?.clone();
        Some(terminal.read(cx).terminal.session_backend())
    }
}

/// Status dot, colour and label for one card.
///
/// Colours come from the sidebar's grammar so the two surfaces cannot fork:
/// amber = needs input, `agent_error` = errored, `agent_stalled` = stalled,
/// muted = thinking, blue = finished, nothing = idle.
fn pane_overview_status_visual(
    state: Option<&crate::ai_types::AgentState>,
    ui: crate::theme::UiColors,
) -> (AnyElement, gpui::Hsla, SharedString) {
    use crate::ai_types::AgentState;
    let (color, label) = match state {
        Some(AgentState::WaitingForInput) => (gpui::rgb(0xFBBF24).into(), "Input"),
        Some(AgentState::Errored) => (ui.agent_error, "Error"),
        Some(AgentState::Stalled) => (ui.agent_stalled, "Stalled"),
        Some(AgentState::Thinking) => (ui.muted, "Working"),
        Some(AgentState::Finished) => (gpui::rgb(0x83C3FF).into(), "Done"),
        None => (ui.muted.opacity(0.0), ""),
    };
    let dot = div()
        .flex_none()
        .w(px(6.))
        .h(px(6.))
        .rounded_full()
        .bg(color)
        .into_any_element();
    (dot, color, SharedString::from(label))
}
```

Re-export the three thumbnail items the render needs. In `src-app/src/terminal/element/mod.rs`, add near the other re-exports:

```rust
pub(crate) use thumbnail::{THUMBNAIL_BAND_H, THUMBNAIL_BAND_W, TerminalThumbnail};
```

and change those three items in `thumbnail.rs` from `pub(super)` to `pub(crate)`. Nothing else
needs widening: the thumbnail resolves its own font and cell metrics from
`font::cached_font_config()` / `font::cell_dimensions`, which are `pub(super)` in `element/font.rs`
and therefore already reachable from `element::thumbnail` (`font::base_font()` is `#[cfg(test)]`
only and is not used).

- [ ] **Step 2: Compose it into the render root**

In `src-app/src/main.rs`, after the fleet-search block (which ends at line 2543) and before the
`custom_buttons_modal` block, add:

```rust
        // Issue #339: Pane Overview (same mode gate).
        if self.pane_overview.is_some() && in_cli_mode {
            app_content = app_content.child(self.render_pane_overview(window, cx));
        }
```

- [ ] **Step 3: Add the refresh tick**

In `src-app/src/app/bootstrap.rs`, after the 50 ms automation-channel loop (which ends around
line 490), add a sibling loop:

```rust
        // Issue #339: repaint the Pane Overview while it is open, at ~4 fps.
        //
        // The overlay deliberately does NOT subscribe to terminal wakeups: a
        // chatty pane fires at the 4 ms coalescing floor, ~250 times a second,
        // and a grid of them driving repaints would blow the frame budget the
        // eight-pane gate in `layout/render.rs` already spends. This loop does
        // nothing at all while the overlay is closed.
        cx.spawn(
            async |this: gpui::WeakEntity<Self>, cx: &mut gpui::AsyncApp| {
                loop {
                    smol::Timer::after(std::time::Duration::from_millis(250)).await;
                    let alive = cx.update(|cx| {
                        this.update(cx, |app: &mut Self, cx: &mut Context<Self>| {
                            if app.pane_overview.is_some() {
                                cx.notify();
                            }
                        })
                    });
                    if alive.is_err() {
                        break;
                    }
                }
            },
        )
        .detach();
```

- [ ] **Step 4: Build and run the full suite**

```bash
cargo build 2>&1 | tail -20
cargo test --workspace > /tmp/pf-t7.log 2>&1; tail -25 /tmp/pf-t7.log
```
Expected: build exit 0; test names a superset of the pre-task baseline, nothing lost.

- [ ] **Step 5: Verify it in the running app**

```bash
PANEFLOW_ALLOW_MULTIPLE=1 PANEFLOW_SOCKET_PATH=/tmp/paneflow-overview.sock cargo run -p paneflow-app
```
Open two workspaces, put a pane in a **non-active tab**, press `Cmd+Shift+P`. Confirm: every pane
appears including the background-tab one; the selection opens **on the pane you were in** and that
card carries the "current" chip; the active workspace's header shows its branch and the "active"
marker; thumbnails show readable text, a dim block cursor, and update; arrows move the ring; Enter
and click both land on the right pane; `Esc` then `Cmd+Shift+P` then `Enter` puts you back where you
started; Esc closes. Add a third workspace with only a markdown pane (or none) and confirm it has no
section at all. Then confirm each pane's content is undisturbed — the no-resize guard is a unit test,
but see it hold live.

- [ ] **Step 6: Format, lint, commit**

```bash
cargo fmt
cargo clippy --workspace --all-targets 2>&1 | tail -5
git add src-app/src/app/pane_overview/mod.rs src-app/src/main.rs src-app/src/app/bootstrap.rs src-app/src/terminal/element/
git commit -m "feat(pane_overview): grouped card grid with live cropped previews (#339)"
```

---

## Task 8: Sidebar entry point

**Files:**
- Modify: `src-app/src/app/sidebar/mod.rs:1019-1032`

- [ ] **Step 1: Add the button**

The "Workspaces" header is already a `justify_between()` flex row with a single child, so it is laid
out to take a trailing control. Replace the header block at `src-app/src/app/sidebar/mod.rs:1018-1032`
with:

```rust
        sidebar = sidebar.child(
            div()
                // Tight enough that the header reads as the list's first line
                // rather than a floating band: at 48 the header label sat 43 px
                // from the first row's label while consecutive rows sit 34
                // apart.
                .h(px(36.))
                .flex_none()
                .px(px(8.))
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .child(
                    div()
                        .pl(px(8.))
                        .text_size(px(13.))
                        .text_color(ui.muted)
                        .child("Workspaces"),
                )
                // Issue #339: Pane Overview. Issue #105 stripped this header's
                // `+` as a redundant fifth route to New Workspace; the overview
                // is the opposite case - it has three entry points and no other
                // discoverable one. Dispatches the action rather than calling
                // the handler so the button and Cmd+Shift+P cannot drift.
                .child(
                    sidebar_action_button(
                        SharedString::from("sidebar-pane-overview"),
                        "icons/layout-grid.svg",
                        12.,
                        ui,
                    )
                    .role(gpui::Role::Button)
                    .aria_label(pane_overview_label.clone())
                    .delayed_tooltip(crate::ui_primitives::text_tooltip(
                        pane_overview_label.clone(),
                    ))
                    .on_click(cx.listener(|_this, _: &ClickEvent, window, cx| {
                        window.dispatch_action(Box::new(crate::OpenPaneOverview), cx);
                        cx.stop_propagation();
                    })),
                ),
        );
```

Bind the label once, above the `sidebar = sidebar.child(` statement:

```rust
        let pane_overview_label = SharedString::from("Show all panes · \u{21E7}\u{2318}P");
```

**The `role` + `aria_label` pair is not optional.** Issue #321 established the repo recipe for an
icon button — `.role(Role::Button)` + `.aria_label(..)` bound to the same string as the tooltip +
`.on_click` (never `on_mouse_down`, which AccessKit does not expose as activation) — and pinned it
with the source-read guard `sidebar_toggle_is_an_accessible_named_button`
(`window_chrome/title_bar.rs:390`). Without the pair, an icon-only control is invisible to a screen
reader. Note the sidebar module currently has **zero** `aria_label` call sites, so this button is
the first there; issue #340 covers retrofitting the rest.

If `window.dispatch_action` is not the right call for this GPUI revision, use the form already used
elsewhere in the file — find it with
`grep -rn "dispatch_action" src-app/src/app/ src-app/src/main.rs | head -5` — and match it.

- [ ] **Step 2: Verify the #105 guard test still passes**

That test forbids only the id `sidebar-new-workspace`, so a different id does not trip it.

Run: `cargo test -p paneflow-app the_workspaces_header_carries_no_new_workspace_button 2>&1 | tail -8`
Expected: PASS.

- [ ] **Step 3: Build and see it**

```bash
cargo build 2>&1 | tail -10
PANEFLOW_ALLOW_MULTIPLE=1 PANEFLOW_SOCKET_PATH=/tmp/paneflow-overview.sock cargo run -p paneflow-app
```
Confirm the icon sits at the right edge of the "Workspaces" row, shows its tooltip on hover, and
opens the overlay.

- [ ] **Step 4: Format, lint, commit**

```bash
cargo fmt
cargo clippy -p paneflow-app --all-targets 2>&1 | tail -5
git add src-app/src/app/sidebar/mod.rs
git commit -m "feat(sidebar): pane overview button on the Workspaces header (#339)"
```

---

## Task 9: Window menu entry point

**Files:**
- Modify: `src-app/src/app/bootstrap.rs:1026-1033` (menu), `:1225` (fallback), `:1479-1520` (guard test)

- [ ] **Step 1: Extend the guard test first**

In `src-app/src/app/bootstrap.rs`, inside `the_macos_menu_bar_routes_settings_and_has_no_view_menu`,
replace the Window-menu ordering assertion (lines 1479-1505) with one that also pins the new item,
and add the new fallback to the loop at line 1511:

```rust
        let window_menu = source_slice(menus, "Menu::new(\"Window\")", "Menu::new(\"Help\")");
        let minimize_at = window_menu
            .find("MenuItem::action(\"Minimize\", MinimizeWindow)")
            .expect("Window > Minimize must exist and dispatch MinimizeWindow");
        let zoom_at = window_menu
            .find("MenuItem::action(\"Zoom\", ZoomWindow)")
            .expect("Window > Zoom must exist and dispatch ZoomWindow");
        let window_separator_at = window_menu
            .find("MenuItem::separator()")
            .expect("the Window menu keeps the separator above the workspace group");
        // Issue #339: the overview sits between the chrome group and the
        // workspace group, fenced by its own separator.
        let overview_at = window_menu
            .find("MenuItem::action(\"Show All Panes\", OpenPaneOverview)")
            .expect("Window > Show All Panes must exist and dispatch OpenPaneOverview");
        let next_at = window_menu
            .find("MenuItem::action(\"Next Workspace\", NextWorkspace)")
            .expect("Window > Next Workspace");
        let close_at = window_menu
            .find("MenuItem::action(\"Close Workspace\", CloseWorkspace)")
            .expect("Window > Close Workspace");
        let new_at = window_menu
            .find("MenuItem::action(\"New Workspace\", NewWorkspace)")
            .expect("Window > New Workspace");
        assert!(
            minimize_at < zoom_at
                && zoom_at < window_separator_at
                && window_separator_at < overview_at
                && overview_at < next_at
                && next_at < close_at
                && close_at < new_at,
            "Window is Minimize, Zoom, separator, Show All Panes, separator, Next, Close, New Workspace"
        );
        assert!(
            window_menu[overview_at..next_at].contains("MenuItem::separator()"),
            "a second separator isolates the workspace group below Show All Panes"
        );

        // AppKit validates a menu item through `is_action_available`, which
        // can miss the render-root listeners while focus sits in a terminal.
        // Without an app-global fallback the item paints permanently greyed,
        // which is why every other menu action has one.
        for fallback in [
            "cx.on_action(|_: &OpenSettings, cx|",
            "cx.on_action(|_: &MinimizeWindow, cx|",
            "cx.on_action(|_: &ZoomWindow, cx|",
            "cx.on_action(|_: &OpenPaneOverview, cx|",
        ] {
            assert!(
                production.contains(fallback),
                "missing app-global menu fallback `{fallback}`; the item would grey out"
            );
        }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p paneflow-app the_macos_menu_bar_routes_settings 2>&1 | tail -12`
Expected: FAIL — `Window > Show All Panes must exist and dispatch OpenPaneOverview`.

- [ ] **Step 3: Add the menu item**

In `install_macos_menu_bar`, add `OpenPaneOverview` to the `use crate::{...}` list at line 1000, and
replace the Window menu (lines 1026-1033) with:

```rust
        Menu::new("Window").items(vec![
            MenuItem::action("Minimize", MinimizeWindow),
            MenuItem::action("Zoom", ZoomWindow),
            MenuItem::separator(),
            // Issue #339: above the workspace group, fenced by its own
            // separator so the chrome / overview / workspaces split reads.
            MenuItem::action("Show All Panes", OpenPaneOverview),
            MenuItem::separator(),
            MenuItem::action("Next Workspace", NextWorkspace),
            MenuItem::action("Close Workspace", CloseWorkspace),
            MenuItem::action("New Workspace", NewWorkspace),
        ]),
```

- [ ] **Step 4: Add the app-global fallback**

In `install_macos_menu_action_fallbacks`, add `OpenPaneOverview` to its `use crate::{...}` list at
line 1226, and add after the `NextWorkspace` fallback (line 1290):

```rust
    // Issue #339: without this the menu item paints permanently greyed while
    // focus sits in a terminal, which is nearly always.
    cx.on_action(|_: &OpenPaneOverview, cx| {
        with_active_paneflow_window(cx, |app, window, cx| {
            app.handle_open_pane_overview(&OpenPaneOverview, window, cx);
        });
    });
```

- [ ] **Step 5: Run the test to verify it passes**

Run: `cargo test -p paneflow-app the_macos_menu_bar_routes_settings 2>&1 | tail -8`
Expected: PASS.

- [ ] **Step 6: Verify the menu item is not greyed**

```bash
cargo build 2>&1 | tail -5
PANEFLOW_ALLOW_MULTIPLE=1 PANEFLOW_SOCKET_PATH=/tmp/paneflow-overview.sock cargo run -p paneflow-app
```
Click into a terminal pane so it holds focus, then open the **Window** menu. "Show All Panes" must be
black, not grey, and must open the overlay.

- [ ] **Step 7: Format, lint, commit**

```bash
cargo fmt
cargo clippy -p paneflow-app --all-targets 2>&1 | tail -5
git add src-app/src/app/bootstrap.rs
git commit -m "feat(menu): Window > Show All Panes with its app-global fallback (#339)"
```

---

## Task 10: Perf gate and documentation

**Files:**
- Modify: `src-app/src/app/pane_overview/mod.rs` (perf gate)
- Modify: `CLAUDE.md` (architecture tree, chord table, corrected header sentence)
- Modify: `docs/user/features.md`

- [ ] **Step 1: Add the ignored perf gate**

Append a test module to `src-app/src/app/pane_overview/mod.rs`:

```rust
#[cfg(test)]
mod perf {
    /// The overlay's frame budget, mirroring the `#[ignore]`d eight-pane
    /// benchmark in `layout/render.rs:464-469`
    /// (`INPUT_TO_FRAME_P95_LIMIT_US = 16_700`, one 60 Hz frame). Eight
    /// painted terminals already spend that whole frame, which is why the
    /// overview caps live thumbnails and culls off-screen cards.
    ///
    /// `#[ignore]` like its sibling: run with
    /// `cargo test --release -p paneflow-app -- --ignored pane_overview`.
    #[test]
    #[ignore = "perf baseline; run explicitly in release"]
    fn overview_with_max_live_thumbnails_stays_inside_one_frame() {
        // Build MAX_LIVE_THUMBNAILS display-only terminals in a render
        // harness, open the overlay, and assert the dirty-to-draw p95 against
        // INPUT_TO_FRAME_P95_LIMIT_US. Follow the harness in
        // `src-app/src/layout/render.rs` (`add_window_view` + `simulate_resize`
        // + `run_until_parked` + the traced-frame probe); it is the only
        // in-repo pattern that measures a real GPUI frame.
        unimplemented!("port the layout/render.rs frame harness to the overlay");
    }
}
```

If porting that harness proves larger than this task, delete the test rather than leaving an
`unimplemented!()` in the tree, and note in the commit message that the gate is deferred — an
`#[ignore]`d test that panics is worse than no test.

- [ ] **Step 2: Update CLAUDE.md — architecture tree**

Under `app/` in the tree (near `├── launch_pad.rs`), add:

```
│   ├── pane_overview/                 ← Cmd+Shift+P expose: every terminal pane
│                                         across every workspace/tab, grouped, each
│                                         bottom-cropped to its last 12 rows
```

Under `terminal/element/`, add:

```
│       ├── thumbnail.rs               ← read-only cropped pane preview; NEVER routes
│                                         through TerminalElement (its build_layout
│                                         resizes the PTY)
```

- [ ] **Step 3: Update CLAUDE.md — chord table**

In the keybinding table, after the `Cmd+Shift+J` / `Cmd+Shift+A` row, add:

```
| `Cmd+Shift+P` | Pane overview (all panes, all workspaces) | Global |
```

- [ ] **Step 4: Correct the CLAUDE.md sidebar sentence**

Find the sentence reading `The sidebar's "Workspaces" header carries no `+`.` and replace it with:

```
The sidebar's "Workspaces" header carries no `+` (issue #105); it does carry the
Pane Overview button (issue #339, id `sidebar-pane-overview`), which the #105
guard test permits because it forbids only the `sidebar-new-workspace` id.
```

Also update the macOS menu-bar paragraph's Window listing to `Minimize / Zoom / separator /
Show All Panes / separator / Next Workspace / Close Workspace / New Workspace`.

- [ ] **Step 5: Add a user-facing section**

In `docs/user/features.md`, after `## Agent-first panes`, add:

```markdown
## Pane overview

`Cmd+Shift+P`, the button beside **Workspaces** in the sidebar, or **Window ▸ Show
All Panes** opens a grid of every terminal pane you have open, grouped by workspace
and tab. Each card shows the pane's last twelve lines, live, plus its name and
whether its agent is working, waiting for you, or finished.

Arrow keys move the selection, `Enter` jumps to the selected pane, clicking a card
does the same, and `Esc` closes. Typing filters by pane name, workspace, tab, or
agent — to search what panes have *printed*, use fleet search instead.
```

- [ ] **Step 6: Run every gate**

```bash
cargo build 2>&1 | tail -5
cargo test --workspace > /tmp/pf-final.log 2>&1; tail -30 /tmp/pf-final.log
grep -oE '^test [a-zA-Z0-9_:]+ \.\.\.' /tmp/pf-final.log | sed 's/^test //; s/ \.\.\.$//' | sort > /tmp/pf-final-names.txt
cargo clippy --workspace --all-targets 2>&1 | tail -5
cargo fmt --check
./target/debug/paneflow --version
cargo deny check advisories licenses sources 2>&1 | tail -5
./scripts/linux-census.sh
```

Expected: build exit 0; test names a strict superset of the baseline; clippy WARNING COUNT 1
(`block v0.1.6`); fmt exit 0; version `paneflow 0.2.1`; deny exit 0; census prints no `FAIL:` line.

- [ ] **Step 7: Commit**

```bash
cargo fmt
git add CLAUDE.md docs/user/features.md src-app/src/app/pane_overview/
git commit -m "docs: pane overview in CLAUDE.md and the user guide (#339)"
```

---

## Self-Review Notes

Checked against the spec, section by section:

- §3 naming/placement → Tasks 1, 6, 7
- §4 thumbnail painter (incl. the `notify_window_size` trap and the crop rule) → Tasks 3, 4
- §5 frame budget (culling, cap, own tick) → Tasks 4 (culling), 1 (cap), 7 (tick)
- §6 data model, all-tabs walk, name clamping, status grammar → Tasks 1, 6, 7
- §7 layout, filter, keys, teleport, empty states → Tasks 6, 7
- §8 `teleport_to_surface` extraction → Task 2
- §9 all three entry points → Tasks 5, 8, 9
- §10 every named test → Tasks 1, 3, 5, 9, 10
- §11 documentation → Tasks 5 (counts) and 10 (the rest)
- §12 out-of-scope items → nothing in this plan touches them
- §13 verification → Task 10 Step 6

**One known soft spot**, flagged rather than hidden:

- **Task 10 Step 1** is the only step that does not carry finished code — porting the
  `layout/render.rs` frame harness to the overlay is genuinely open-ended. The step says to delete
  the test rather than leave a panicking `#[ignore]` in the tree if it overruns.

**One claim worth recording**, because a research pass got it backwards and the whole design rests
on it: `TerminalSessionBackend::render_content`'s doc comment reads *"Resize and snapshot in one
runtime round-trip"*, which describes only the `clear_on_resize: true` path. Verified against
`ghostty_session.rs:1527` (trait fn at `pty_session.rs:183`): every mutation (`resize.requested`,
`clear_initial_requested`, `submit_requested_resize`) sits inside `if clear_on_resize`, and
`normalized_window_size` is a pure clamp. On the `false` path the call is one `RwLock` read plus an
`Arc` refcount bump (`ghostty_session.rs:1535-1536`), with no consumer-side bookkeeping in the
`0185ee4f` frame-publication gate, so a second reader cannot starve the pane. Do not "fix"
`thumbnail_snapshot` to avoid `render_content` on the strength of that doc comment.

**Citations were read on `main` at `7642b564`** (contains `origin/main` `6a47fea4`); the spec header
records the same tree.
