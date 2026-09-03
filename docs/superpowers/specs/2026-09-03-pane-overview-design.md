# Pane Overview — design

**Date:** 2026-09-03
**Status:** approved design, not yet implemented
**Scope:** one new overlay surface, one new terminal painter, three entry points
**Issue:** #339 (tooltip audit split out as #340)

A Mission-Control-style overlay that shows every open terminal pane across
every workspace and tab at once, each rendered legibly enough to identify,
with its agent status, and clicking one jumps to it.

---

## 1. Problem

PaneFlow runs coding agents in parallel across up to 20 workspaces × 32 tabs ×
32 panes. Today, finding a specific pane means either knowing where you put it
or using one of two narrow tools:

- **Attention Queue** (`Cmd+Shift+A`) lists only panes whose agent reported a
  state, and only in each workspace's *active* tab.
- **Fleet Search** (`Alt+F` from search) finds panes by their *content*, and
  again only in active tabs.

Neither answers "show me everything I have open so I can pick the right one".
There is no surface anywhere in the app that renders a pane's content outside
its own live pane — no thumbnail, minimap, or preview exists today.

## 2. Product decisions

Settled with the user before design:

| Question | Decision |
|---|---|
| Scope | **All workspaces**, all tabs, grouped |
| Liveness | **Live, throttled** — previews keep updating while open |
| Interaction | **Both** — arrows + type-to-filter, plus click |
| Pane kinds | **Terminals only**; markdown and diff panes are omitted with no placeholder |
| Layout | **Workspace sections, tab sub-rows** of wrapping cards |
| Card content | **Bottom-crop at readable size**, not scaled-to-fit |
| Crop rule | **Always the bottom 12 rows** — one rule, one test |
| Chord | **`Cmd+Shift+P`** |
| Entry points | Sidebar "Workspaces" header button + Window menu item above Next Workspace |
| Closing panes from the overlay | **Out of scope** |
| App-wide tooltip audit | **Separate issue**, not part of this design |

## 3. Naming and placement

**Pane Overview.** Action `OpenPaneOverview`, registry name `open_pane_overview`.

It is a full-window `deferred(...).with_priority(6)` overlay — a peer of the
Attention Queue (`attention_queue.rs:380`) and Fleet Search
(`fleet_search.rs:548`), which occupy the same priority band. It is deliberately
**not** title-bar chrome: `title_bar.rs:378` carries a guard test asserting that
removed title-bar popover surfaces stay removed.

It renders from `PaneFlowApp::render` between the fleet-search block and the
`custom_buttons_modal` block, inside the same `in_cli_mode` gate the other
cockpit overlays use. That gate is deliberate (see the comment above the
attention-queue block in `main.rs`): cockpit chrome must not paint over the
Agents or Review surfaces after a mode switch.

### New files

| File | Purpose |
|---|---|
| `src-app/src/app/pane_overview/mod.rs` | overlay state, open/close, key handling, activate, render |
| `src-app/src/app/pane_overview/rows.rs` | **pure** grouping / filter / flatten / selection movement — no GPUI, unit-testable |
| `src-app/src/terminal/element/thumbnail.rs` | the miniature read-only painter |

The painter *must* live under `terminal/element/`. `LayoutState`'s fields are
all private (`element/mod.rs:606`), `mod paint` is a private module, and
`CellGeometry` is `pub(super)`. A painter anywhere else in the crate could call
`layout_from_snapshot` (it is `pub(crate)`) but could not paint the
`LayoutState` it returns.

---

## 4. The thumbnail painter

### 4.1 The trap this design exists to avoid

`TerminalElement` **cannot** be reused at a smaller size. Its `build_layout`
resizes the child process as a side effect of layout:

```rust
// src-app/src/terminal/element/mod.rs:867
if notify_resize {
    self.backend.notify_window_size(window_size);   // → SIGWINCH to the child
}
```

There is no flag to suppress it. A card-sized `TerminalElement` would resize the
PTY to the card's bounds every frame, and the real pane would resize it back —
each open of the overlay would corrupt the layout of every pane it displayed.

### 4.2 The seam it uses instead

`layout_from_snapshot` (`element/mod.rs:921`) is an explicitly `Window`-free,
`App`-free pure function. Its `LayoutInputs` (`element/mod.rs:578`) takes cell
dimensions and the base font as plain values, so it lays out at any scale. It
exists so the golden-frame tests can assert layout with no GPU or display; a
thumbnail is the same kind of consumer.

```
TerminalThumbnail::prepaint(bounds, window, cx):
    if bounds does not intersect window.content_mask() { return None }   // (a)
    let m = backend.grid_metrics();                                      // (b)
    let size = TerminalWindowSize { cols: m.columns, rows: m.screen_lines, .. };
    let (content, _) = backend.render_content(size, 0, 0, /* clear_on_resize */ false)
    let last  = content.rows
    let first = last.saturating_sub(THUMBNAIL_ROWS)                      // (c)
    layout_from_snapshot(LayoutInputs {
        cells: content.cells, first_visible_row: first, last_visible_row: last,
        dims: THUMBNAIL_CELL_DIMS, base_font, theme, ..
    })

TerminalThumbnail::paint(bounds, layout, window, cx):
    with_content_mask(bounds):
        origin = bounds.origin - point(0, first * line_height)           // (d)
        backgrounds → block quads → box drawing → text runs → dim block cursor
```

- **(a)** is the culling mechanism. A card scrolled out of view does no terminal
  work at all — not even the lock read.
- **(b)** `clear_on_resize: false` is load-bearing: the `true` branch mutates
  `ResizeState` and calls `submit_requested_resize`. Verified against
  `ghostty_session.rs:1433` — on the `false` path `window_size` is consumed only
  by `normalized_window_size` and then discarded, and the two visible-row
  arguments are `_`-prefixed and ignored outright (culling happens later, in
  `layout_from_snapshot`). Deriving the size from `grid_metrics()` anyway keeps
  the call honest rather than relying on arguments being inert.
- The snapshot is the **viewport**, not scrollback, with `display_offset`
  already applied. So a pane the user has scrolled up shows, in its thumbnail,
  exactly what the pane itself is showing. That is the correct behaviour and
  needs no special handling.
- **(c)** is the whole crop rule. `first_visible_row = rows - 12`, always.
- **(d)** shifts the origin up so the cropped band lands at the top of the card.
  Cells keep their absolute line numbers through `layout_from_snapshot`, which
  culls by line index rather than renumbering.

`render_content` for a read-only consumer is an `RwLock::read()` plus an
`Arc<[Cell]>` refcount bump — `Content::cells` is an `Arc`, so the clone is not
a copy.

### 4.3 What is not painted

Selection, search highlights, hyperlink underlines, IME preedit, the scrollbar,
and **Kitty graphics placements**. Kitty in particular: each pane has a 32 MiB
image cap, and decoding placements into a 320 px card is not a trade worth
making. The cursor is painted as a dim, non-blinking block — it is a useful
"parked at a prompt" signal and costs one quad.

### 4.4 Geometry

Cell geometry is a pure function of a scalar. `element/font.rs` computes
`cell_width = round(font_size × 0.6)` and `line_height = round(font_size × 1.2)`;
there is no glyph measurement to redo. At a 9 px thumbnail font:

```
cell_width  = round(9 × 0.6) = 5 px
line_height = round(9 × 1.2) = 11 px
```

so a **320 × 132** band is exactly **12 rows × 64 columns**. Lines wider than 64
columns are truncated at the right edge; that is accepted.

The 8.0–32.0 pt clamp in `font.rs` applies to the config and zoom path, not to
`layout_from_snapshot`, so 9 px is reachable. Below roughly 4 px cell width the
`.round()` quantization would dominate and columns would drift — 5 px is above
that floor, which is why "scale the whole grid to fit" was rejected.

### 4.5 Constants

```rust
const THUMBNAIL_ROWS: usize = 12;
const THUMBNAIL_FONT_PX: f32 = 9.0;
const CARD_W: f32 = 328.0;      // 320 band + 4 px padding each side
const CARD_H: f32 = 196.0;      // 4 + 30 header + 132 band + 26 footer + 4
const CARD_GAP: f32 = 12.0;
const MAX_LIVE_THUMBNAILS: usize = 24;
const OVERVIEW_REFRESH_MS: u64 = 250;
```

---

## 5. Staying inside the frame budget

This is the constraint that shapes the feature. `layout/render.rs:470` holds an
`#[ignore]`d perf gate asserting that **eight live panes** stay inside a
`16_700 µs` input-to-paint P95 — one 60 Hz frame. Eight already consumes the
qualified budget. Three bounds keep the overview under it:

1. **Element self-culling** (§4.2a). Off-screen cards cost a small div tree and
   nothing else.
2. **`MAX_LIVE_THUMBNAILS = 24`.** Cards past the 24th in visible order render
   the static shell — name, status badge, breadcrumb — with no thumbnail band.
   The worst case is 20 × 32 × 32 = 20,480 panes; this cap is what makes that
   survivable rather than theoretical.
3. **Its own tick.** The overlay does **not** subscribe to terminal wakeups. A
   chatty pane can fire at the 4 ms coalescing floor (`terminal/view.rs`),
   ~250 times a second, and eight of them driving thumbnail repaints would be a
   disaster. Instead `process_automation_tick` — already running at 50 ms —
   calls `cx.notify()` every fifth tick while the overlay is open. ~4 fps, which
   is what "live, throttled" means here.

Change detection, if it is ever needed, polls `TerminalState::output_generation`
rather than `dirty`: `dirty` is cleared on every repaint, and the comment at
`pty_session.rs` is explicit that `output_generation` is the durable signal.

**Why no wakeup subscription is needed at all:** every pane's snapshot is
published unconditionally by its runtime thread with no visibility gate
(`update_shared_state`, `ghostty_session.rs`). Panes in background tabs of
inactive workspaces — which are never rendered, since `main.rs` renders only the
active workspace — already have live grids sitting in `SharedState`. The
overview is a pure read; nothing has to be pulled or woken.

---

## 6. Data model and enumeration

```rust
// pane_overview/rows.rs — GPUI-free plain data
pub(crate) struct CardMeta {
    pub surface_id: u64,
    pub ws_idx: usize, pub ws_id: u64, pub ws_title: String,
    pub tab_idx: usize, pub tab_id: u64, pub tab_title: String,
    pub name: String,
    pub cwd_label: Option<String>,
    pub agent: Option<TerminalAgent>,
    pub state: Option<AgentState>,
    pub cols: usize, pub rows: usize,
    pub exited: bool,
    pub is_active: bool,
}

pub(crate) struct TabGroup { pub tab_idx: usize, pub title: String, pub cards: Vec<CardMeta> }
pub(crate) struct WorkspaceGroup { pub ws_idx: usize, pub title: String, pub is_active: bool, pub tabs: Vec<TabGroup> }
```

The GPUI side builds a parallel `Vec<(u64, Entity<TerminalView>)>` for the
painter; `rows.rs` never sees an entity, which is what keeps it testable.

**The walk** is modelled on `workspace_surface_entries`
(`app/ipc_handler.rs`), the authoritative full enumeration:

```rust
for (ws_idx, ws) in self.workspaces.iter().enumerate() {
    for (tab_idx, tab) in ws.tabs().iter().enumerate() {
        for pane in tab.collect_panes() {
            let Some(terminal) = pane.read(cx).active_terminal_opt() else { continue };
            ...
        }
    }
}
```

This is deliberately **not** the walk used by Fleet Search
(`fleet_search.rs`) or the Attention Queue's liveness set
(`attention_queue.rs`), both of which visit only `ws.active_tab()`. That
restriction would hide most of what the overview exists to show. Walk
`ws.tabs()` directly rather than `Workspace::collect_panes()`, which dedupes
with a linear `contains` per pane — the same reason `is_idle` and
`workspace_surface_entries` already avoid it.

`Tab::collect_panes` dedupes `root` against `saved_layout`, so a zoomed tab
yields each pane once.

**Names** come from `Pane::surface_title` — the human label the pane header and
sidebar already show, which resolves user renames first, then agent identity,
then cwd. It is passed through `crate::limits::clamp_untrusted_label` before
rendering: OSC titles are attacker-controlled.

**Status** is resolved per card with the codebase's canonical pattern — there is
no `Pane::state()`, and `Pane::attention` / `Pane::errored` are private mirrors
with no getters:

```rust
let sid = terminal.entity_id().as_u64();
let state = ws.agent_sessions.values()
    .find(|s| s.surface_id == Some(sid))
    .map(|s| s.state);   // None == idle
```

Badges reuse `sidebar_agent_summary` and `agent_summary_visual`
(`app/sidebar/mod.rs`) verbatim so the overview cannot fork from the sidebar's
colour grammar: amber bell = Input, `ui.agent_error` = Errored,
`ui.agent_stalled` = Stalled, the comet-trail loader = Thinking, a blue dot =
Finished, nothing = idle.

---

## 7. Interaction

### 7.1 Layout

A vertically scrolling column. Per workspace: a section header (title, branch,
active marker). Per tab within it: a labelled row of cards that wraps
(`flex_wrap`). Card visual grammar follows `diff_dock/surface_picker.rs` —
`rounded(px(10.))`, `border_1`, hover fill at 5% ink, `CursorStyle::PointingHand`
— with the selection ring drawn as a fill, matching the app-wide selection
grammar documented in `custom_buttons_modal.rs`.

### 7.2 Filter

A `TextInput` in the header. It matches **metadata only**: pane name, workspace
title, tab title, detected agent name, and cwd basename.

It deliberately does **not** search visible text. Fleet Search already owns
content search, and re-running it across every pane on each keystroke is exactly
the anti-pattern `ipc_handler.rs` warns about ("Metadata only: do not
extract_scrollback every pane on the GPUI tick", issue #29), guarded by a
regression test. The overlay's footer hint points at Fleet Search for content.

### 7.3 Keys

| Key | Effect |
|---|---|
| `←` / `→` | ±1 in flat order |
| `↑` / `↓` | ±`cards_per_row`, clamped |
| `Enter` | jump to the selected pane |
| `Esc` | close, restoring focus |
| printable | goes to the filter input |

`selected: usize` indexes the **flattened, filtered** card order: workspace index
→ tab index → layout traversal — the same stable order `jump_next_session_where`
uses, so the overview and `Cmd+Shift+J` agree about what "next" means.

`cards_per_row` is a render-time value, captured each frame the way
`Container::container_size` captures its real main-axis pixel width via a
`canvas()` prepaint. Do not hardcode an estimate; the old `split.rs` 800 px guess
is the cautionary precedent.

The input/keys split follows Fleet Search: the card is
`.occlude().track_focus(&self.pane_overview_focus).on_key_down(...)` with
`.on_mouse_down_out(...)` to dismiss and `stop_propagation` on L/R mouse-down;
the header input owns printable keys.

### 7.4 Click-to-jump

```rust
let Some(loc) = find_pane_by_surface_id(&self.workspaces, surface_id, cx) else { ... };
if let Some(ws) = self.workspaces.get_mut(loc.workspace_idx) { ws.set_active_tab(loc.tab_idx); }
self.activate_workspace_at(loc.workspace_idx, WorkspaceFocusTarget::Pane { pane: loc.pane }, window, cx);
self.jump_cursor = Some(surface_id);
self.close_pane_overview(cx);
```

The ordering is load-bearing: focus can only land on a *rendered* pane, so the
owning tab must become visible before the focus call.

Because indices are re-resolved from `surface_id` at click time rather than
captured at open time, a workspace or tab reorder while the overlay is open
cannot teleport the user to the wrong pane.

Closing restores focus via the Attention Queue's idiom
(`close_attention_queue_and_restore_focus`): `ws.focus_first(window, cx)`,
falling back to `window.focus(&self.empty_workspace_focus, cx)` when the
workspace has no pane (issue #108).

### 7.5 Empty and degenerate states

- **No terminal panes anywhere** — a centred empty state naming the two ways to
  make one.
- **Filter matches nothing** — "No panes match" with the query echoed.
- **Exited pane** — `Content` still holds the last frame; the card renders it
  dimmed with an "exited" chip.
- **Never-mounted pane** — keeps the bootstrap 120×40 grid, so its thumbnail
  aspect may not match its on-screen pane. The content is real and current
  regardless; the footer shows the true `cols×rows`.

---

## 8. Targeted refactor

The teleport body in §7.4 already exists twice — `attention_queue_activate`
(`app/attention_queue.rs`) and `fleet_search_activate`
(`app/fleet_search.rs`) — and this feature would make it three copies of a
sequence whose *ordering* is a correctness contract. Duplicating it a third time
is how that contract eventually drifts.

Extract:

```rust
// app/workspace_ops/focus.rs
pub(crate) fn teleport_to_surface(
    &mut self, surface_id: u64, window: &mut Window, cx: &mut Context<Self>,
) -> bool
```

performing lookup → `set_active_tab` → `activate_workspace_at` →
`jump_cursor`, returning `false` when the surface is gone. All three call sites
use it; each keeps its own overlay-specific tail (closing itself, arming local
search).

This is in scope because it is the code this feature is writing into. No other
refactoring is proposed.

---

## 9. Entry points

### 9.1 Sidebar

An icon button (`icons/layout-grid.svg`, already shipped and embedded — dropping
an SVG into `src-app/assets/icons/` is sufficient, there is no list to maintain)
beside the "Workspaces" header at `app/sidebar/mod.rs:1031`. That header is
already a `justify_between()` flex row with a single child, so it is laid out to
accept a trailing control.

Built with `sidebar_action_button` + `.delayed_tooltip(SidebarTooltip { label:
"Show all panes · ⇧⌘P" })`, id `sidebar-pane-overview`, and it dispatches
`OpenPaneOverview` rather than calling a handler directly, so button and chord
cannot drift.

**Issue #105 note.** That header was deliberately stripped of its `+`. The guard
test `the_workspaces_header_carries_no_new_workspace_button`
(`sidebar/mod.rs:3602`) forbids only the id `sidebar-new-workspace`, so a
different affordance does not trip it. But the reasoning behind #105 was
"redundant fifth entry point"; the overview button is not redundant — the
overlay has exactly three entry points and no other discoverable one. The
CLAUDE.md sentence describing the header as bare must be corrected in the same
change.

### 9.2 Window menu

`install_macos_menu_bar` (`app/bootstrap.rs`), currently:

```
Minimize / Zoom / ─── / Next Workspace / Close Workspace / New Workspace
```

becomes:

```
Minimize / Zoom / ─── / Show All Panes / ─── / Next Workspace / Close Workspace / New Workspace
```

The added separator keeps the existing guard test satisfied — it finds the
*first* `MenuItem::separator()` and asserts it precedes `Next Workspace`, which
still holds.

A menu item needs **both** the entry above **and** a fallback in
`install_macos_menu_action_fallbacks`, or AppKit's `is_action_available` check
paints the item permanently greyed while focus sits in a terminal — which it
almost always does. This is documented in CLAUDE.md and is the single most
common way a new menu item ships broken.

### 9.3 Chord

`Cmd+Shift+P`, global context. Verified free: nothing in `keybindings/` claims
`secondary-shift-p` or `cmd-shift-p` today.

Registry entry in `ShortcutGroup::Agents` (where the other cockpit overlays
live), so the Settings → Keyboard Shortcuts page picks it up automatically —
`display.rs` needs no edit, and its two coverage tests will fail if the registry
entry is missing.

---

## 10. Testing

### Pure, no GPUI (`pane_overview/rows.rs`)
- grouping produces one `WorkspaceGroup` per workspace and one `TabGroup` per
  tab, in index order
- filter matches name / workspace / tab / agent / cwd, case-insensitively, and
  drops empty groups
- flat order equals workspace idx → tab idx → traversal order
- selection movement clamps at both ends and at grid-row boundaries
- `MAX_LIVE_THUMBNAILS` partitions the visible order into live and shell cards

### The two trap tests
- **`thumbnail_never_resizes_the_pty`** — capture `grid_metrics()` before and
  after a thumbnail prepaint against a live backend and assert they are
  identical. This is the highest-value test in the feature: it pins §4.1, the
  one mistake that would silently corrupt every pane the overlay displays.
- **`overview_walks_every_tab_not_just_the_active_one`** — build a workspace
  with a pane in a non-active tab and assert it appears. Pins §6 against
  regression toward the Fleet Search walk.

### Crop
- `layout_from_snapshot` over a fixture grid with `first_visible_row = rows - 12`
  yields exactly the last 12 rows and nothing above them
- a grid shorter than 12 rows renders all of it without underflow
  (`saturating_sub`)

### Keybinding
- `pane_overview_chord_is_bindable_and_does_not_collide` in
  `keybindings/apply.rs`, following the `TogglePrimarySidebar` precedent
  (`apply.rs:499`): asserts the context, that `make_binding` parses, and that
  exactly one action claims the chord.

### Perf
- an `#[ignore]`d gate mirroring `layout/render.rs`: overlay open with 24 live
  thumbnails, P95 under `INPUT_TO_FRAME_P95_LIMIT_US`.

### Source-read guards
- the Window menu ordering assertion extended for the new item and its separator

---

## 11. Documentation changes

- **CLAUDE.md action count `92` → `93` at both sites** — `CLAUDE.md:192` and
  `CLAUDE.md:382`. `claude_md_action_count_matches_the_actions_macro` fails
  otherwise.
- CLAUDE.md keybinding table: a `Cmd+Shift+P` row.
- CLAUDE.md architecture tree: `pane_overview/` under `app/`, and
  `element/thumbnail.rs` under `terminal/element/`.
- CLAUDE.md: correct the sentence describing the sidebar "Workspaces" header as
  carrying no affordance, and the macOS menu-bar paragraph's Window menu listing.

## 12. Explicitly out of scope

- Closing panes from the overlay (drags in the live-agent confirmation gate)
- Drag-to-rearrange panes between tabs or workspaces
- Previews for markdown and diff panes
- Content search inside the overlay (Fleet Search owns it)
- Any new `paneflow.json` key — the feature is fully constant-driven
- The app-wide tooltip audit — filed as its own issue

## 13. Verification

The six gates from CLAUDE.md, before and after, with output quoted:

```bash
cargo build
cargo test --workspace          # diff test names, do not trust the integer
cargo clippy --workspace --all-targets   # WARNING COUNT 1 (block v0.1.6)
cargo fmt --check
./target/debug/paneflow --version
cargo deny check advisories licenses sources
```

Plus manual verification, which UI changes always need: open the overlay with
panes spread across at least two workspaces and a non-active tab, confirm
thumbnails render legibly and update, confirm clicking jumps to the right pane,
and confirm the panes' grid dimensions are unchanged afterward.
