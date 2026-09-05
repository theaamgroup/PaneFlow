//! "Shortcuts" settings tab - grouped, searchable, *virtualized* list of every
//! rebindable action with click-to-record key capture.
//!
//! The page used to be one flat card of ~80 rows in registry order, which made
//! finding a binding a scrolling exercise and answering "what already owns this
//! chord?" impossible. Three things fix that:
//!
//! - **Sections.** Rows are filed under [`ShortcutGroup`], declared on the
//!   action in `keybindings::registry` rather than implied by table order.
//!   Each section collapses, and a header control folds or unfolds all of them.
//! - **Text filter.** One field matching the action description *and* the
//!   rendered keystroke, so "workspace" and "cmd+shift" both narrow the list.
//! - **Key capture.** A toggle that turns the next pressed chord into the
//!   filter (the VS Code / KDE recipe). Text search cannot answer "who owns
//!   this key?" unless you already know how the chord is spelled; capture can.
//!
//! Filtering auto-expands: a collapsed section that contains a match opens for
//! the duration of the query, so a hit is never hidden behind a closed header.
//! Rebind capture itself is still driven by
//! `PaneFlowApp::handle_shortcut_recording` (in `app::settings`), which
//! serialises the chord through `recorded_shortcut_key` (`unparse()`, never
//! `to_string()`), and every row carries its index into the *unfiltered*
//! `effective_shortcuts`, because that is what the rebind keys off.
//!
//! "Reset to defaults" rewrites `paneflow.json` with no undo, so it is a
//! two-step inline confirm ([`step_reset_confirm`]) behind a settle delay
//! ([`RESET_ARM_SETTLE`]) so a double-click cannot confirm through its own
//! arm; the confirming click copies the file to `paneflow.json.before-reset`
//! first and then goes through `config_writer::reset_shortcuts_checked`, the
//! same checked writer the flat page used.
//!
//! ## Why this page is virtualized and the other six are not
//!
//! Fully expanded, the page is ~80 rows of ~8 elements each. GPUI rebuilds its
//! taffy tree from scratch every frame (`TaffyLayoutEngine::clear`), so a flat
//! tree of ~700 nodes was laid out and prepainted on *every* repaint, offscreen
//! rows included - and because each row carries a hover style, simply moving
//! the pointer across the list repaints the whole page. Upstream measured
//! 6.2 ms/frame expanded vs 1.2 ms folded in release, 70 ms vs 14 ms in a
//! debug build.
//!
//! So the rows live in a [`gpui::list`], which materializes only what the
//! viewport shows. `list` rather than `uniform_list` because section headers
//! and binding rows are different heights. The item stream is flat - header,
//! its rows, next header - exactly the shape Zed's settings window uses
//! (`crates/settings_ui/src/settings_ui.rs`, `render_current_page_items`).
//!
//! Two consequences the code has to carry:
//!
//! - **The card is drawn per row.** A virtualized item cannot span its
//!   neighbours, so each row paints its own slice of the section card and the
//!   first/last row of a section round the slice's top/bottom. That also
//!   retires the per-section `squircle_fill`: a filled path forces GPUI to end
//!   the main render pass, rasterize into an intermediate texture and reopen
//!   the pass, so nine open sections cost nine render-pass splits per frame.
//!   See [`SHORTCUT_CARD_RADIUS`] for the corner that replaces it.
//! - **The filter runs outside `render`.** [`PaneFlowApp::shortcut_rows`] is
//!   rebuilt when the query, the fold state or the bindings change, not once
//!   per frame.

use gpui::{
    AnyElement, App, ClickEvent, Context, Div, InteractiveElement, IntoElement, KeyDownEvent,
    ListAlignment, ListState, MouseButton, MouseDownEvent, MouseMoveEvent, ParentElement, Pixels,
    Point, Role, Styled, div, list, prelude::*, px, svg,
};

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::keybindings::ShortcutGroup;
use crate::settings::components::{
    SETTINGS_CONTROL_CORNER_RADIUS, card_color, destructive_button, hairline, secondary_button,
    section_header_with_action, setting_card,
};
use crate::terminal::element::{MIN_APCA_CONTRAST, ensure_minimum_contrast};
use crate::ui_primitives::{ROW_RADIUS, squircle_skin};
use crate::widgets::scrollbar::{self, ScrollableHandle as _};
use crate::{PaneFlowApp, config_writer, keybindings};

/// Corner of a section card on this page.
///
/// Elsewhere a settings card is a superellipse at
/// `constants::PANE_CARD_RADIUS` (20). Here the card is sliced across list
/// items, so it is painted as plain rounded quads instead - a filled path per
/// section would cost a render-pass split per section, every frame (see the
/// module docs). GPUI resolves `rounded()` with a circular arc, and
/// `ui_primitives::ROW_RADIUS` documents the conversion: a superellipse needs
/// roughly 1.5x the circular radius to read as equally round, so 20 / 1.5
/// lands here.
const SHORTCUT_CARD_RADIUS: Pixels = px(13.);

/// Inset between a section card's edge and its rows, matching the `p(4.)` the
/// card used to carry as a single element.
const SHORTCUT_CARD_INSET: Pixels = px(4.);

/// Vertical air above a section header, matching the `gap(16.)` the page used
/// between sections before the list flattened them.
const SHORTCUT_SECTION_GAP: Pixels = px(16.);

/// Air between a section header and its card, matching the old `gap(6.)`.
const SHORTCUT_HEADER_GAP: Pixels = px(6.);

/// One entry in the flattened, virtualized Shortcuts list.
///
/// Flat rather than nested because that is what a virtualized list can index:
/// a section is a header item followed by its binding items, and folding a
/// section simply drops that run.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ShortcutListRow {
    /// A collapsible section header. `count` is how many bindings survived the
    /// filter under it, which is what the header's badge shows.
    Header { group: ShortcutGroup, count: usize },
    /// One binding. `idx` indexes the *unfiltered* `effective_shortcuts`,
    /// because that is what a rebind keys off. `first` / `last` place the row
    /// in its section's card so the slab's corners land on the right rows.
    Binding { idx: usize, first: bool, last: bool },
}

/// Range of `rows` holding the binding items of `group`, or `None` when the
/// group is not on the page at all. The range is empty when it is folded.
fn shortcut_group_span(rows: &[ShortcutListRow], group: ShortcutGroup) -> Option<Range<usize>> {
    let header = rows
        .iter()
        .position(|row| matches!(row, ShortcutListRow::Header { group: g, .. } if *g == group))?;
    let start = header + 1;
    let len = rows[start..]
        .iter()
        .take_while(|row| matches!(row, ShortcutListRow::Binding { .. }))
        .count();
    Some(start..start + len)
}

/// Whether one binding survives the page's filter.
///
/// Matching is case-insensitive and substring-based over the action
/// description, the displayed keystroke, and the ASCII spellings of the chord
/// (`ShortcutEntry::search_key`), so "workspace", "⌘⇧" and "cmd+shift" all
/// narrow the list. `query` is expected lowercased and trimmed.
///
/// A captured chord (`capture` set) is compared against the whole displayed
/// keystroke instead, since a chord is an exact thing: macOS glyphs
/// concatenate with no separator, so a substring test would answer ⌘⇧D with
/// every row that merely contains it, ⌃⌘⇧D included.
fn entry_matches(entry: &keybindings::ShortcutEntry, query: &str, capture: bool) -> bool {
    if query.is_empty() {
        return true;
    }
    if capture {
        return entry.key.to_lowercase() == query;
    }
    entry.description.to_lowercase().contains(query)
        || entry.key.to_lowercase().contains(query)
        || entry.search_key.contains(query)
}

/// Rows surviving the filter, flattened for the virtualized list.
///
/// Pure so the filter, the grouping and the fold policy can be tested without
/// a window. `collapsed` is the user's fold state; a non-empty `query`
/// overrides it (hiding a match behind a closed header would make the search
/// look broken) without clearing it, so the folds come back when the query
/// goes.
fn shortcut_rows_from(
    entries: &[keybindings::ShortcutEntry],
    query: &str,
    capture: bool,
    collapsed: &HashSet<ShortcutGroup>,
) -> Vec<ShortcutListRow> {
    let filtering = !query.is_empty();

    // One pass over the entries rather than one per section.
    let mut by_group: HashMap<ShortcutGroup, Vec<usize>> = HashMap::new();
    for (idx, entry) in entries.iter().enumerate() {
        if entry_matches(entry, query, capture) {
            by_group.entry(entry.group).or_default().push(idx);
        }
    }

    let mut rows = Vec::with_capacity(entries.len() + ShortcutGroup::ALL.len());
    for group in ShortcutGroup::ALL {
        let Some(indices) = by_group.remove(group) else {
            continue;
        };
        let count = indices.len();
        rows.push(ShortcutListRow::Header {
            group: *group,
            count,
        });
        if !filtering && collapsed.contains(group) {
            continue;
        }
        for (position, idx) in indices.into_iter().enumerate() {
            rows.push(ShortcutListRow::Binding {
                idx,
                first: position == 0,
                last: position + 1 == count,
            });
        }
    }
    rows
}

/// What one click on the page's reset control did.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ResetOutcome {
    /// The first click: the confirm was armed and nothing was written.
    Armed,
    /// The confirming click: the reset ran; carries whether the write
    /// succeeded.
    Reset(bool),
}

/// How long the armed "Reset" confirm has to have been on screen before a
/// click on it counts as the decision to erase every binding.
///
/// Same number, same reason as [`crate::app::close_guard::ARM_SETTLE`]: a
/// double-click delivers BOTH clicks to whatever is under the pointer, and
/// the "Reset" confirm is painted on the same row as the "Reset to defaults"
/// button that arms it. Without the delay the second click of a double-click
/// confirmed through an armed state that had existed for one frame. That
/// guard protects an agent; this one protects a file with no undo.
pub(crate) const RESET_ARM_SETTLE: Duration = crate::app::close_guard::ARM_SETTLE;

/// When the "Reset" confirm was armed, or `None` while it is not.
///
/// The instant is what the settle rule needs and what the `bool` the toolbar
/// renders from (`shortcut_reset_pending`) cannot carry. It lives in a GPUI
/// global, the same mechanism as `terminal::blink::BlinkPhaseGlobal`, and is
/// written only through [`PaneFlowApp::set_shortcut_reset_arm`], which keeps
/// the flag and the instant in step. The two failure modes are not symmetric:
/// a flag that says "armed" over a missing instant merely arms again, but a
/// stale instant under a cleared flag would let a "first" click confirm.
#[derive(Default)]
struct ShortcutResetArm(Option<Instant>);

impl gpui::Global for ShortcutResetArm {}

/// The two-step "Reset to defaults" confirm as a pure state machine.
///
/// `pending` is when the confirm was armed, or `None`. `reset` - the checked
/// writer - is only ever called on the confirming click, which is the whole
/// point: resetting rewrites every binding in `paneflow.json` with no undo, so
/// a single stray click must not be able to do it. Returns the next `pending`
/// value and what happened.
///
/// A click inside `settle` of the arm re-arms rather than confirming, the
/// same rule as `close_guard::click_outcome`: a double-click delivers both of
/// its clicks, so the settle delay is what makes the armed state a perception
/// gate instead of a single unperceivable frame. Re-arming restarts the
/// window, so a burst of clicks never accumulates into a reset either.
pub(crate) fn step_reset_confirm(
    pending: Option<Instant>,
    now: Instant,
    settle: Duration,
    reset: impl FnOnce() -> bool,
) -> (Option<Instant>, ResetOutcome) {
    match pending {
        Some(armed_at) if now.duration_since(armed_at) >= settle => {
            (None, ResetOutcome::Reset(reset()))
        }
        _ => (Some(now), ResetOutcome::Armed),
    }
}

/// Copy `paneflow.json` to `paneflow.json.before-reset` beside it, so the
/// bindings a reset erases can be put back by hand.
///
/// Returns the backup's path, `None` when there is no config file to save,
/// and the error when the copy failed - in which case the reset must not run:
/// a file that cannot be read for a copy cannot be rewritten either, and one
/// that can should not be erased without its backup. `paneflow.json` is the
/// user's own text (comments and all in the worst case), so this is a byte
/// copy rather than a re-serialisation.
fn backup_config_before_reset(path: &Path) -> std::io::Result<Option<PathBuf>> {
    if !path.exists() {
        return Ok(None);
    }
    let mut name = path
        .file_name()
        .map(std::ffi::OsString::from)
        .unwrap_or_default();
    name.push(".before-reset");
    let backup = path.with_file_name(name);
    std::fs::copy(path, &backup)?;
    Ok(Some(backup))
}

/// The confirming click's writer: back the config up, then erase the
/// bindings through the checked writer. `config_writer` has no backup helper
/// of its own (the `.bak` one in `paneflow-mcp-install` is for agent configs),
/// so the copy lives here, next to the only caller that needs it.
fn reset_shortcuts_with_backup() -> bool {
    if let Some(path) = paneflow_config::loader::config_path() {
        match backup_config_before_reset(&path) {
            Ok(Some(backup)) => {
                log::info!(
                    "shortcuts: backed up {} to {}",
                    path.display(),
                    backup.display()
                );
            }
            Ok(None) => {}
            Err(error) => {
                log::warn!(
                    "shortcuts: could not back up {} before reset: {error}; not resetting",
                    path.display()
                );
                return false;
            }
        }
    }
    config_writer::reset_shortcuts_checked()
}

/// A fresh, empty list state. The page seeds it on every rebuild, so only the
/// three policy knobs are fixed here.
///
/// - **Top-aligned**, like any settings page.
/// - **No overdraw**: a settings list has no streaming tail to keep ahead of,
///   and every overdrawn row is a row rendered for nothing.
/// - **`measure_all`**, which is the one that is not obvious. A `ListState`
///   measures lazily, and `max_offset_for_scrollbar` only counts what it has
///   measured, so a lazy list reports a scroll range covering roughly the
///   visible rows: the thumb starts near full height and grows as you scroll,
///   and the wheel can only advance in bites of that range. The height *hints*
///   `reset_with_uniform_height` seeds do not save it either - the first
///   prepaint after any width change wipes every hint
///   (`gpui::List::prepaint`, "invalidate all cached item heights"), and the
///   first layout is always a width change. So the list measures everything
///   once per re-seed instead. That pass is O(rows) - the cost the page used
///   to pay on *every* frame - and it now happens only when the row set
///   changes. Zed's settings window makes the same call
///   (`crates/settings_ui/src/settings_ui.rs`: `ListState::new(..).measure_all()`).
pub(crate) fn new_shortcut_list_state() -> ListState {
    ListState::new(0, ListAlignment::Top, px(0.)).measure_all()
}

impl PaneFlowApp {
    /// The page's current filter query, lowercased and trimmed.
    fn shortcut_query(&self, cx: &App) -> String {
        self.shortcut_search_input
            .read(cx)
            .value()
            .trim()
            .to_lowercase()
    }

    /// Whether a filter query is set: the answer without the lowercased
    /// copy, for the callers that only need the answer - `render` asks every
    /// frame. (`TextInput::value` still hands out an owned `String`; the
    /// widget has no borrowed accessor.)
    fn shortcut_filtering(&self, cx: &App) -> bool {
        !self
            .shortcut_search_input
            .read(cx)
            .value()
            .trim()
            .is_empty()
    }

    /// Rows surviving the active filter, flattened for the virtualized list.
    /// See [`shortcut_rows_from`].
    fn shortcut_rows_for(&self, cx: &App) -> Vec<ShortcutListRow> {
        shortcut_rows_from(
            &self.effective_shortcuts,
            &self.shortcut_query(cx),
            self.shortcut_capture_active,
            &self.collapsed_shortcut_groups,
        )
    }

    /// Recompute the flattened rows and re-seed the list.
    ///
    /// Used whenever the *whole* list can move: the query changed, capture mode
    /// flipped, or the bindings themselves were rewritten. A fold, which moves
    /// only one run of rows, goes through [`Self::toggle_shortcut_group`]
    /// instead so it can splice.
    ///
    /// Re-seeding drops the scroll position, which is what a new result set
    /// wants - the same thing Zed does after a keymap query
    /// (`scroll_to_item(0)`). But a rebind also comes through here, and the row
    /// count is the tell: an unchanged count means the same rows in the same
    /// order with one key relabelled, and throwing the user back to the top of
    /// an 80-row list after every rebind would be its own bug.
    pub(crate) fn rebuild_shortcut_rows(&mut self, cx: &mut Context<Self>) {
        let previous_len = self.shortcut_rows.len();
        let previous_top = self.shortcut_list.logical_scroll_top();

        self.shortcut_rows = self.shortcut_rows_for(cx);
        let len = self.shortcut_rows.len();
        self.shortcut_list.reset(len);

        if len > 0 && len == previous_len {
            self.shortcut_list.scroll_to(previous_top);
        }
    }

    /// Fold or unfold one section, splicing just its run of rows so the list
    /// keeps its scroll position.
    fn toggle_shortcut_group(&mut self, group: ShortcutGroup, cx: &mut Context<Self>) {
        if !self.collapsed_shortcut_groups.remove(&group) {
            self.collapsed_shortcut_groups.insert(group);
        }

        let before = shortcut_group_span(&self.shortcut_rows, group);
        self.shortcut_rows = self.shortcut_rows_for(cx);
        let after = shortcut_group_span(&self.shortcut_rows, group);

        match (before, after) {
            // The header did not move, so only its own rows changed.
            (Some(before), Some(after)) if before.start == after.start => {
                self.shortcut_list.splice(before, after.len());
            }
            // The section moved or left the page (the filter changed underneath
            // us). Splicing the wrong range would corrupt the item heights, so
            // re-seed instead.
            _ => self.shortcut_list.reset(self.shortcut_rows.len()),
        }
        cx.notify();
    }

    /// The instant the "Reset" confirm was armed, if it is. See
    /// [`ShortcutResetArm`].
    fn shortcut_reset_armed_at(&self, cx: &App) -> Option<Instant> {
        cx.try_global::<ShortcutResetArm>().and_then(|arm| arm.0)
    }

    /// Arm (`Some(now)`) or stand down (`None`) the "Reset" confirm. The only
    /// writer of `shortcut_reset_pending`: see [`ShortcutResetArm`] for why
    /// the flag and the instant must never be set separately.
    pub(crate) fn set_shortcut_reset_arm(&mut self, armed_at: Option<Instant>, cx: &mut App) {
        self.shortcut_reset_pending = armed_at.is_some();
        cx.set_global(ShortcutResetArm(armed_at));
    }

    /// One click on the reset control - either the "Reset to defaults" button
    /// (arms) or the "Reset" confirm (fires). Both route through
    /// [`step_reset_confirm`] so the arm-then-settle-then-fire order is the
    /// one thing that decides whether [`reset_shortcuts_with_backup`] runs.
    fn shortcut_reset_clicked(&mut self, cx: &mut Context<Self>) {
        // The write is synchronous, but it still goes through the persist
        // guard: a ConfigWatcher deposit stamped before this write must not
        // be applied over the config we are about to reload.
        let flight = self.begin_config_persist();
        let (armed_at, outcome) = step_reset_confirm(
            self.shortcut_reset_armed_at(cx),
            Instant::now(),
            RESET_ARM_SETTLE,
            reset_shortcuts_with_backup,
        );
        drop(flight);
        self.set_shortcut_reset_arm(armed_at, cx);
        match outcome {
            ResetOutcome::Armed => {}
            ResetOutcome::Reset(false) => {
                self.show_toast("Could not reset shortcuts", cx);
            }
            ResetOutcome::Reset(true) => {
                let config = paneflow_config::loader::load_config();
                keybindings::apply_keybindings(cx, &config.shortcuts);
                self.effective_shortcuts = keybindings::effective_shortcuts(&config.shortcuts);
                self.recording_shortcut_idx = None;
                self.rebuild_shortcut_rows(cx);
            }
        }
        cx.notify();
    }

    /// The whole Shortcuts page: a fixed header block, the virtualized list,
    /// and the hint line.
    ///
    /// The toolbar stays *outside* the list on purpose. It owns a focused text
    /// field, and a virtualized item that scrolls out of range is unmounted -
    /// which would drop the focus mid-query. Zed keeps its keymap search bar
    /// outside its table for the same reason.
    pub(crate) fn render_shortcuts_page(
        &self,
        heading: AnyElement,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let ui = crate::theme::ui_colors();
        let filtering = self.shortcut_filtering(cx);

        let hint = if self.shortcut_capture_active {
            "Press a chord to find what owns it. Escape to leave capture mode."
        } else {
            "Click a row to record a new shortcut. Escape to cancel."
        };

        let body = if self.shortcut_rows.is_empty() {
            // No result count anywhere else on this page: the matching rows are
            // on screen, and counting what the user can already see answers no
            // question they have. An empty result is the one case that does.
            div()
                .flex_none()
                .pt(SHORTCUT_SECTION_GAP)
                .child(
                    setting_card(ui).p(SHORTCUT_CARD_INSET).child(
                        div()
                            .px(px(8.))
                            .py(px(14.))
                            .text_size(px(12.))
                            .text_color(ui.muted)
                            .child("No shortcut matches this filter"),
                    ),
                )
                .into_any_element()
        } else {
            self.render_shortcut_list(ui, cx)
        };

        div()
            .absolute()
            .top_0()
            .right_0()
            .bottom_0()
            .left_0()
            .min_h_0()
            .pr(scrollbar::SCROLLBAR_GUTTER)
            .bg(crate::settings::chrome::settings_chrome_bg())
            .flex()
            .flex_col()
            .items_start()
            .child(
                self.settings_reading_column()
                    .flex_1()
                    .min_h_0()
                    .pb(px(20.))
                    .child(heading)
                    .child(
                        div()
                            .flex_none()
                            .flex()
                            .flex_col()
                            .gap(SHORTCUT_SECTION_GAP)
                            .child(self.render_shortcut_toolbar(ui, filtering, cx))
                            .child(self.render_shortcut_group_controls(ui, filtering, cx)),
                    )
                    .child(body)
                    .child(
                        div()
                            .flex_none()
                            .pt(SHORTCUT_SECTION_GAP)
                            .text_size(px(11.))
                            .text_color(ui.muted)
                            .child(hint.to_string()),
                    ),
            )
            .into_any_element()
    }

    /// The virtualized list plus its scrollbar overlay.
    fn render_shortcut_list(
        &self,
        ui: crate::theme::UiColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let card_bg = card_color();
        let rows = list(
            self.shortcut_list.clone(),
            cx.processor(move |this, index: usize, _window, cx| {
                let Some(row) = this.shortcut_rows.get(index).copied() else {
                    return gpui::Empty.into_any_element();
                };
                match row {
                    ShortcutListRow::Header { group, count } => {
                        this.render_shortcut_section_header(ui, group, count, index == 0, cx)
                    }
                    ShortcutListRow::Binding { idx, first, last } => {
                        this.render_shortcut_row(ui, card_bg, idx, first, last, cx)
                    }
                }
            }),
        )
        .size_full();

        let bar = scrollbar::render(
            &self.shortcut_list,
            ui,
            None,
            "shortcut-scrollbar-track",
            "shortcut-scrollbar-thumb",
            cx.listener(|this, ev: &MouseDownEvent, _, cx| {
                if let Some(off) = scrollbar::track_click_offset(&this.shortcut_list, ev.position.y)
                {
                    this.shortcut_list.set_offset(Point::new(px(0.), px(off)));
                    cx.notify();
                }
            }),
            cx.listener(|this, ev: &MouseDownEvent, _, cx| {
                this.shortcut_drag =
                    Some(scrollbar::begin_drag(&this.shortcut_list, ev.position.y));
                cx.stop_propagation();
            }),
        );

        // Two boxes, not one. The outer carries the spacing above the list; the
        // inner is the scrollbar's coordinate frame and must coincide *exactly*
        // with the list's viewport, because `scrollbar::render` anchors the
        // track at `top_0`. The gutter is the inner's padding rather than the
        // list's: `List` applies its own padding vertically only - it hands
        // items the full `bounds.size.width` and offsets them by `padding.top`
        // alone - so `pr` on the list would reserve nothing.
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            // The eyebrow above already carries 8px of its own; together they
            // reproduce the 16px the page put between every section back when
            // the whole column was one flex stack.
            .pt(SHORTCUT_SECTION_GAP)
            .child(self.shortcut_list_region(rows, bar, cx))
            .into_any_element()
    }

    /// The list and its scrollbar, sharing one coordinate frame.
    fn shortcut_list_region(
        &self,
        rows: gpui::List,
        bar: Option<AnyElement>,
        cx: &mut Context<Self>,
    ) -> Div {
        div()
            .relative()
            .flex_1()
            .min_h_0()
            .pr(scrollbar::SCROLLBAR_GUTTER)
            .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _, cx| {
                // A move with no button held is not a drag, whatever
                // `shortcut_drag` says. GPUI fires the `on_mouse_up` below
                // only over this region, so a thumb released off the panel
                // left the drag set: bare hover then scrolled the list, and
                // the `ListState`'s lazy measurement stayed frozen. End it
                // here, through the handle, rather than scroll on it.
                if ev.pressed_button != Some(MouseButton::Left) {
                    if scrollbar::end_drag(&this.shortcut_list, this.shortcut_drag.take()) {
                        cx.notify();
                    }
                    return;
                }
                if let Some(drag) = this.shortcut_drag
                    && let Some(off) =
                        scrollbar::drag_offset(&this.shortcut_list, &drag, ev.position.y)
                {
                    this.shortcut_list.set_offset(Point::new(px(0.), px(off)));
                    cx.notify();
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    let drag = this.shortcut_drag.take();
                    if scrollbar::end_drag(&this.shortcut_list, drag) {
                        cx.notify();
                    }
                }),
            )
            .child(rows)
            .when_some(bar, |d, sb| d.child(sb))
    }

    /// Search field + key-capture toggle + "Reset to defaults".
    fn render_shortcut_toolbar(
        &self,
        ui: crate::theme::UiColors,
        filtering: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let capture_active = self.shortcut_capture_active;
        // `accent` and `text` are independent theme tokens: on Vercel Dark they
        // are #ffffff and #ededed, so a plain `text` label on an `accent` fill
        // is invisible. Lift the label off the fill with the same APCA pass the
        // terminal uses for selected text.
        let on_accent = ensure_minimum_contrast(ui.text, ui.accent, MIN_APCA_CONTRAST);

        // One field in both modes. In capture mode the interceptor writes the
        // pressed chord straight into it, so the user always reads back exactly
        // what was captured instead of trusting an invisible filter.
        let field = crate::ui_primitives::filter_pill(
            "shortcut-search",
            "shortcut-search-clear",
            ui,
            self.shortcut_search_input.clone(),
            filtering,
            cx.listener(|this, _: &ClickEvent, _window, cx| {
                this.clear_shortcut_filters(cx);
                cx.notify();
            }),
        )
        .flex_1()
        .min_w_0()
        // Two-stage Escape, the same recipe as the nav search: clear the
        // query if there is one, otherwise leave settings. Only reached while
        // the field holds focus; during capture or recording the interceptor
        // has already consumed the key.
        .on_key_down(cx.listener(|this, ev: &KeyDownEvent, _window, cx| {
            if ev.keystroke.key == "escape" {
                if !this.shortcut_filtering(cx) {
                    this.close_settings(cx);
                } else {
                    this.clear_shortcut_filters(cx);
                }
                cx.notify();
                cx.stop_propagation();
            }
        }))
        // Clicking outside drops focus so the caret disappears.
        .on_mouse_down_out(cx.listener(|this, _, window, cx| {
            if this
                .shortcut_search_input
                .read(cx)
                .focus_handle
                .is_focused(window)
            {
                window.blur();
                cx.notify();
            }
        }));

        let capture_toggle = squircle_skin(
            div()
                .id("shortcut-capture-toggle")
                .role(Role::Switch)
                .aria_label("Find by key")
                .aria_toggled(crate::settings::components::switch_toggled(capture_active))
                .tab_index(0)
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.))
                .px(px(10.))
                .py(px(5.)),
            "shortcut-capture-skin",
            ROW_RADIUS,
            // Armed state is a resting fill, not just a hover: the mode
            // swallows keystrokes, so it must be visible without pointing at it.
            capture_active.then_some(ui.accent),
            Some(ui.subtle),
        )
        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
            let next = !this.shortcut_capture_active;
            this.set_shortcut_capture(next, cx);
            if next {
                // The chord has to land on the settings surface, not on
                // whatever held focus before.
                this.settings_focus.focus(window, cx);
            }
            cx.notify();
        }))
        .child(
            svg()
                .size(px(13.))
                .flex_none()
                .path("icons/keyboard.svg")
                .text_color(if capture_active { on_accent } else { ui.muted }),
        )
        .child(
            div()
                .text_size(px(11.))
                .text_color(if capture_active { on_accent } else { ui.muted })
                .child(if capture_active {
                    "Capturing"
                } else {
                    "Find by key"
                }),
        );

        // Resetting rewrites every binding in paneflow.json with no undo, so it
        // asks first. A two-step inline confirm rather than a dialog: settings
        // already live in a modal, and stacking a second one over it to ask a
        // one-line question reads as heavier than the action deserves.
        let mut row = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.))
            .child(field)
            .child(capture_toggle);

        row = if self.shortcut_reset_pending {
            row.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.))
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(ui.muted)
                            .child("Reset all?"),
                    )
                    .child(secondary_button(
                        "reset-shortcuts-cancel",
                        "Cancel",
                        ui,
                        cx.listener(|this, _: &ClickEvent, _w, cx| {
                            this.set_shortcut_reset_arm(None, cx);
                            cx.notify();
                        }),
                    ))
                    .child(
                        destructive_button("reset-shortcuts-confirm", "Reset").on_click(
                            cx.listener(|this, _: &ClickEvent, _w, cx| {
                                this.shortcut_reset_clicked(cx);
                            }),
                        ),
                    ),
            )
        } else {
            row.child(secondary_button(
                "reset-shortcuts",
                "Reset to defaults",
                ui,
                cx.listener(|this, _: &ClickEvent, _w, cx| {
                    this.shortcut_reset_clicked(cx);
                }),
            ))
        };

        row.into_any_element()
    }

    /// The "Bindings" eyebrow, with an "Expand all" / "Collapse all" action.
    ///
    /// The action is dropped while a filter is active: a query forces every
    /// matching section open for its duration, so the button could only ever be
    /// a no-op there - it would set the fold state and change nothing on screen.
    fn render_shortcut_group_controls(
        &self,
        ui: crate::theme::UiColors,
        filtering: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if filtering {
            return section_header_with_action(ui, "Bindings", div()).into_any_element();
        }

        // Offer whichever action actually changes something: if every visible
        // section is already folded, the only useful verb is "expand". Every
        // section on the page opens with its header, so a non-empty row set
        // has at least one header to ask about - no need to collect them.
        let all_collapsed = !self.shortcut_rows.is_empty()
            && self
                .shortcut_rows
                .iter()
                .filter_map(|row| match row {
                    ShortcutListRow::Header { group, .. } => Some(*group),
                    ShortcutListRow::Binding { .. } => None,
                })
                .all(|group| self.collapsed_shortcut_groups.contains(&group));
        let (label, collapse) = if all_collapsed {
            ("Expand all", false)
        } else {
            ("Collapse all", true)
        };

        section_header_with_action(
            ui,
            "Bindings",
            secondary_button(
                "shortcut-toggle-all",
                label,
                ui,
                cx.listener(move |this, _: &ClickEvent, _w, cx| {
                    this.collapsed_shortcut_groups.clear();
                    if collapse {
                        this.collapsed_shortcut_groups
                            .extend(ShortcutGroup::ALL.iter().copied());
                    }
                    // Every section moved, so re-seed rather than splice.
                    this.rebuild_shortcut_rows(cx);
                    cx.notify();
                }),
            ),
        )
        .into_any_element()
    }

    /// One section header, as a list item. `first` drops the leading air so the
    /// list does not open on a gap.
    fn render_shortcut_section_header(
        &self,
        ui: crate::theme::UiColors,
        group: ShortcutGroup,
        count: usize,
        first: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // The chevron follows the rows actually on the page, not
        // `collapsed_shortcut_groups`: a query opens every matching section for
        // its duration (see `shortcut_rows_from`) while leaving the user's fold
        // state untouched, and the header has to say what is on screen. A
        // section with no rows under it is folded - one with no *matches* has
        // no header at all.
        let collapsed =
            shortcut_group_span(&self.shortcut_rows, group).is_some_and(|span| span.is_empty());

        let header = squircle_skin(
            div()
                .id(("shortcut-group", group as usize))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.))
                .px(px(8.))
                .py(px(6.)),
            format!("shortcut-group-skin-{}", group as usize),
            ROW_RADIUS,
            None,
            Some(ui.subtle),
        )
        .on_click(cx.listener(move |this, _: &ClickEvent, _w, cx| {
            this.toggle_shortcut_group(group, cx);
        }))
        .child(
            svg()
                .size(px(11.))
                .flex_none()
                .path(if collapsed {
                    "icons/chevron-right.svg"
                } else {
                    "icons/chevron-down.svg"
                })
                .text_color(ui.muted),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_size(px(12.))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(ui.text)
                .truncate()
                .child(group.label()),
        )
        .child(
            div()
                .text_size(px(11.))
                .text_color(ui.muted)
                .child(count.to_string()),
        );

        div()
            // See `shortcut_card_slice`: a list item is its own layout root.
            // A block container already fills, but stating it keeps the two
            // item kinds reading the same way.
            .w_full()
            .when(!first, |d| d.pt(SHORTCUT_SECTION_GAP))
            .pb(SHORTCUT_HEADER_GAP)
            .child(header)
            .into_any_element()
    }

    /// One binding row, wrapped in its slice of the section card.
    fn render_shortcut_row(
        &self,
        ui: crate::theme::UiColors,
        card_bg: gpui::Hsla,
        idx: usize,
        first: bool,
        last: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(entry) = self.effective_shortcuts.get(idx) else {
            return gpui::Empty.into_any_element();
        };
        let is_recording = self.recording_shortcut_idx == Some(idx);
        let unassigned = entry.key == "Unassigned";

        let key_badge = if is_recording {
            div()
                .px(px(10.))
                .py(px(3.))
                .rounded(SETTINGS_CONTROL_CORNER_RADIUS)
                .bg(ui.accent)
                .text_size(px(11.))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                // Same APCA lift as the capture toggle: on themes where accent
                // and text nearly coincide, a plain `text` label would leave
                // the armed row looking like an empty pill.
                .text_color(ensure_minimum_contrast(
                    ui.text,
                    ui.accent,
                    MIN_APCA_CONTRAST,
                ))
                .child("Press a key…")
        } else {
            div()
                .px(px(10.))
                .py(px(3.))
                .rounded(SETTINGS_CONTROL_CORNER_RADIUS)
                .bg(ui.subtle)
                .text_size(px(11.))
                .font_weight(gpui::FontWeight::MEDIUM)
                // An unassigned row is an absence, not a binding, so it reads
                // muted instead of sitting at the same weight as a real chord.
                .text_color(if unassigned { ui.muted } else { ui.text })
                .child(entry.key.clone())
        };

        let row = squircle_skin(
            div()
                .id(("shortcut", idx))
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .gap(px(12.))
                .px(px(8.))
                .py(px(10.)),
            format!("shortcut-squircle-{idx}"),
            ROW_RADIUS,
            None,
            Some(ui.subtle),
        )
        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
            // Recording a rebind and capturing a search chord both want the
            // keyboard; arming one disarms the other.
            this.set_shortcut_capture(false, cx);
            this.recording_shortcut_idx = Some(idx);
            this.settings_focus.focus(window, cx);
            cx.notify();
        }))
        .when(is_recording, |row| {
            // A click anywhere else is the user moving on - to the search
            // field, another row, the reset control - and a row left armed
            // behind them would swallow the next chord they typed there.
            // Only a mounted row can listen, so an armed row scrolled out
            // of the viewport relies on the interceptor's focused-field
            // rule instead (`route_shortcut_keystroke`).
            row.on_mouse_down_out(cx.listener(|this, _: &MouseDownEvent, _window, cx| {
                this.disarm_shortcut_recording(cx);
            }))
        })
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_size(px(13.))
                .text_color(ui.text)
                .truncate()
                .child(entry.description.clone()),
        )
        .child(key_badge);

        shortcut_card_slice(card_bg, first, last)
            .child(row)
            // The separator lives inside the row above it, not between two
            // items: a list item is the smallest thing the viewport can clip,
            // and a 1px item of its own would be one more thing to measure.
            .when(!last, |d| d.child(hairline(ui)))
            .into_any_element()
    }
}

/// The section card, sliced for one row.
///
/// `first` / `last` round the slab where the card's own corners used to be, so
/// the run of rows still reads as one card even though no element spans them.
fn shortcut_card_slice(card_bg: gpui::Hsla, first: bool, last: bool) -> Div {
    div()
        // `w_full` is load-bearing. `List` lays every item out as its own
        // layout root (`layout_items` -> `layout_as_root`), and taffy sizes an
        // `auto` *flex container* root from its content, not from the available
        // space the way a block one fills it. Without this the slabs
        // shrink-wrap their text - measured at 191px inside a 640px list - and
        // the page reads as a ragged staircase. `ListState::bounds_for_item`
        // cannot see it either: it reports the list's own width for every item,
        // which is why the guard test below asserts on painted bounds.
        .w_full()
        .flex()
        .flex_col()
        .bg(card_bg)
        .px(SHORTCUT_CARD_INSET)
        .when(first, |d| {
            d.rounded_t(SHORTCUT_CARD_RADIUS).pt(SHORTCUT_CARD_INSET)
        })
        .when(last, |d| {
            d.rounded_b(SHORTCUT_CARD_RADIUS).pb(SHORTCUT_CARD_INSET)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn header(group: ShortcutGroup, count: usize) -> ShortcutListRow {
        ShortcutListRow::Header { group, count }
    }

    fn binding(idx: usize) -> ShortcutListRow {
        ShortcutListRow::Binding {
            idx,
            first: false,
            last: false,
        }
    }

    /// The default rows, exactly as the page sees them at startup.
    fn default_entries() -> Vec<keybindings::ShortcutEntry> {
        keybindings::effective_shortcuts(&HashMap::new())
    }

    /// Action names of the binding rows in `rows`, in page order.
    fn listed<'a>(
        rows: &[ShortcutListRow],
        entries: &'a [keybindings::ShortcutEntry],
    ) -> Vec<&'a str> {
        rows.iter()
            .filter_map(|row| match row {
                ShortcutListRow::Binding { idx, .. } => Some(entries[*idx].action_name),
                ShortcutListRow::Header { .. } => None,
            })
            .collect()
    }

    #[test]
    fn group_span_covers_only_its_own_bindings() {
        let rows = vec![
            header(ShortcutGroup::Panes, 2),
            binding(0),
            binding(1),
            header(ShortcutGroup::Tabs, 1),
            binding(2),
        ];
        assert_eq!(
            shortcut_group_span(&rows, ShortcutGroup::Panes),
            Some(1..3),
            "the first section must stop at the next header"
        );
        assert_eq!(
            shortcut_group_span(&rows, ShortcutGroup::Tabs),
            Some(4..5),
            "the last section must run to the end"
        );
    }

    #[test]
    fn group_span_of_a_folded_section_is_empty_not_missing() {
        // A folded section still has a header, and the splice that unfolds it
        // needs its insertion point - `None` would force a full re-seed and
        // throw the scroll position away.
        let rows = vec![
            header(ShortcutGroup::Panes, 2),
            header(ShortcutGroup::Tabs, 1),
            binding(2),
        ];
        let span = shortcut_group_span(&rows, ShortcutGroup::Panes).expect("header is present");
        assert!(span.is_empty(), "a folded section owns no rows");
        assert_eq!(
            span.start, 1,
            "unfolding must insert right after the header"
        );
    }

    #[test]
    fn group_span_is_none_when_the_filter_removed_the_section() {
        let rows = vec![header(ShortcutGroup::Tabs, 1), binding(0)];
        assert_eq!(shortcut_group_span(&rows, ShortcutGroup::Panes), None);
    }

    #[test]
    fn search_matches_an_action_by_its_chord_text() {
        // The attention queue is Cmd+Shift+A (issue #184). A user who knows the
        // chord but not the action's name has to be able to find it by typing
        // the chord - in either of the spellings the config and the docs use,
        // the dashed GPUI one and the plus-joined one.
        let entries = default_entries();
        let none = HashSet::new();
        for query in ["shift-a", "shift+a", "cmd-shift-a", "cmd+shift+a"] {
            let rows = shortcut_rows_from(&entries, query, false, &none);
            assert!(
                listed(&rows, &entries).contains(&"open_attention_queue"),
                "searching `{query}` must find open_attention_queue; got {:?}",
                listed(&rows, &entries)
            );
        }
        // And the name still works, so the chord match is an addition.
        let rows = shortcut_rows_from(&entries, "attention", false, &none);
        assert_eq!(listed(&rows, &entries), vec!["open_attention_queue"]);
    }

    #[test]
    fn search_leaves_no_empty_section_and_reports_the_match_count() {
        // A section with no matches has no header at all; one with matches
        // carries the surviving count on its header.
        let entries = default_entries();
        let rows = shortcut_rows_from(&entries, "attention", false, &HashSet::new());
        assert_eq!(
            rows,
            vec![
                header(ShortcutGroup::Agents, 1),
                ShortcutListRow::Binding {
                    idx: entries
                        .iter()
                        .position(|e| e.action_name == "open_attention_queue")
                        .expect("listed"),
                    first: true,
                    last: true,
                },
            ]
        );
    }

    #[test]
    fn capture_query_matches_the_whole_chord_only() {
        // Capture mode asks "what owns exactly this?". macOS glyphs concatenate
        // with no separator, so a substring test would answer ⌘⇧A with every
        // row that merely contains it.
        let entries = default_entries();
        let none = HashSet::new();
        let chord = keybindings::format_keystroke("cmd-shift-a").to_lowercase();
        let rows = shortcut_rows_from(&entries, &chord, true, &none);
        assert_eq!(listed(&rows, &entries), vec!["open_attention_queue"]);

        // A partial chord matches nothing in capture mode, and the name never
        // takes part.
        let partial = keybindings::format_keystroke("shift-a").to_lowercase();
        assert!(shortcut_rows_from(&entries, &partial, true, &none).is_empty());
        assert!(shortcut_rows_from(&entries, "attention", true, &none).is_empty());
    }

    #[test]
    fn folded_section_keeps_its_header_and_drops_its_rows() {
        let entries = default_entries();
        let mut collapsed = HashSet::new();
        collapsed.insert(ShortcutGroup::Agents);
        let rows = shortcut_rows_from(&entries, "", false, &collapsed);

        let agents = shortcut_group_span(&rows, ShortcutGroup::Agents).expect("header stays");
        assert!(agents.is_empty(), "a folded section shows no bindings");
        let panes = shortcut_group_span(&rows, ShortcutGroup::Panes).expect("header present");
        assert!(!panes.is_empty(), "other sections are unaffected");

        // Every section is on the page, in `ALL` order, with its full count.
        let headers: Vec<ShortcutGroup> = rows
            .iter()
            .filter_map(|row| match row {
                ShortcutListRow::Header { group, .. } => Some(*group),
                ShortcutListRow::Binding { .. } => None,
            })
            .collect();
        assert_eq!(headers, ShortcutGroup::ALL.to_vec());
    }

    #[test]
    fn filter_opens_a_collapsed_section_that_contains_a_match() {
        // Hiding a hit behind a closed header would make the search look
        // broken, so a query overrides the fold - and the fold state itself is
        // the caller's to keep, so it comes back when the query goes.
        let entries = default_entries();
        let mut collapsed = HashSet::new();
        collapsed.insert(ShortcutGroup::Agents);
        let rows = shortcut_rows_from(&entries, "attention", false, &collapsed);
        assert_eq!(listed(&rows, &entries), vec!["open_attention_queue"]);
        assert!(
            collapsed.contains(&ShortcutGroup::Agents),
            "the fold state is not consumed by filtering"
        );
    }

    /// The text of `name`'s body: from its `fn` line to `end`.
    fn body<'a>(src: &'a str, name: &str, end: &str) -> &'a str {
        let start = src
            .find(name)
            .unwrap_or_else(|| panic!("{name} must exist"));
        let rest = &src[start..];
        let stop = rest
            .find(end)
            .unwrap_or_else(|| panic!("{end} must follow {name}"));
        &rest[..stop]
    }

    #[test]
    fn first_reset_click_arms_and_second_fires() {
        let calls = Cell::new(0);
        let reset = || {
            calls.set(calls.get() + 1);
            true
        };

        let armed_at = Instant::now();
        let (pending, outcome) = step_reset_confirm(None, armed_at, RESET_ARM_SETTLE, reset);
        assert_eq!(outcome, ResetOutcome::Armed);
        assert_eq!(pending, Some(armed_at), "the first click arms the confirm");
        assert_eq!(calls.get(), 0, "the first click must not reset anything");

        // A deliberate second click: the armed state has been on screen for
        // the whole settle window.
        let (pending, outcome) = step_reset_confirm(
            pending,
            armed_at + RESET_ARM_SETTLE,
            RESET_ARM_SETTLE,
            reset,
        );
        assert_eq!(outcome, ResetOutcome::Reset(true));
        assert_eq!(pending, None, "the confirm disarms once it has fired");
        assert_eq!(calls.get(), 1, "the second click resets exactly once");
    }

    /// The reason [`RESET_ARM_SETTLE`] exists, mirroring
    /// `close_guard::a_second_click_inside_the_settle_delay_re_arms_instead_of_confirming`.
    /// "Reset to defaults" and its "Reset" confirm are painted on the same
    /// row, so a double-click on the control delivers its second click to the
    /// confirm - and without the delay that erased every binding in
    /// `paneflow.json`, no undo, behind an armed state painted for one frame.
    #[test]
    fn a_second_reset_click_inside_the_settle_delay_re_arms_instead_of_confirming() {
        let calls = Cell::new(0);
        let reset = || {
            calls.set(calls.get() + 1);
            true
        };

        let armed_at = Instant::now();
        for early in [
            Duration::from_millis(0),
            Duration::from_millis(100),
            RESET_ARM_SETTLE - Duration::from_millis(1),
        ] {
            let now = armed_at + early;
            let (pending, outcome) =
                step_reset_confirm(Some(armed_at), now, RESET_ARM_SETTLE, reset);
            assert_eq!(
                outcome,
                ResetOutcome::Armed,
                "a click {early:?} after the arm is the tail of a double-click, not a decision"
            );
            assert_eq!(
                pending,
                Some(now),
                "the re-arm restarts the window, so a burst of clicks never adds up to a reset"
            );
        }
        assert_eq!(calls.get(), 0, "nothing inside the settle window may write");

        // Past the delay the armed state has been on screen long enough to
        // read, so the second click means what it says.
        for late in [RESET_ARM_SETTLE, Duration::from_millis(500)] {
            let (pending, outcome) =
                step_reset_confirm(Some(armed_at), armed_at + late, RESET_ARM_SETTLE, reset);
            assert_eq!(
                outcome,
                ResetOutcome::Reset(true),
                "a deliberate second click {late:?} after the arm must still confirm"
            );
            assert_eq!(pending, None);
        }
        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn reset_confirm_reports_a_failed_write() {
        // The outcome carries the writer's verdict so the page can toast
        // instead of silently claiming a reset that never reached disk.
        let armed_at = Instant::now();
        let (pending, outcome) = step_reset_confirm(
            Some(armed_at),
            armed_at + RESET_ARM_SETTLE,
            RESET_ARM_SETTLE,
            || false,
        );
        assert_eq!(outcome, ResetOutcome::Reset(false));
        assert_eq!(pending, None);
    }

    /// A reset is unrecoverable from inside the app, so the bindings it erases
    /// are copied beside the config first. The copy is the state before *this*
    /// reset, so a later one overwrites it; and a missing file has nothing to
    /// save.
    #[test]
    fn reset_backs_the_config_up_beside_it_before_erasing_the_bindings() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("paneflow.json");
        let first = r#"{"shortcuts":{"cmd-k":"none"}}"#;
        std::fs::write(&path, first).unwrap();

        let backup = backup_config_before_reset(&path)
            .expect("copy succeeds")
            .expect("there is a file to save");
        assert_eq!(backup, dir.path().join("paneflow.json.before-reset"));
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), first);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            first,
            "the backup is a copy; the config itself is the writer's to change"
        );

        let second = r#"{"shortcuts":{"cmd-j":"none"}}"#;
        std::fs::write(&path, second).unwrap();
        backup_config_before_reset(&path).unwrap();
        assert_eq!(
            std::fs::read_to_string(&backup).unwrap(),
            second,
            "each reset saves the bindings it is about to erase"
        );

        assert_eq!(
            backup_config_before_reset(&dir.path().join("missing.json")).unwrap(),
            None,
            "no config file means nothing to back up and nothing to refuse"
        );
    }

    /// The confirming click goes through the backup on its way to the checked
    /// writer, and the click handler feeds the state machine the real clock
    /// and the shared settle constant - a hand-rolled `bool` here would be the
    /// original bug back.
    #[test]
    fn the_confirming_click_backs_up_before_it_resets() {
        let src = include_str!("shortcuts.rs");

        let writer = body(src, "fn reset_shortcuts_with_backup(", "\n}\n");
        let backup_at = writer
            .find("backup_config_before_reset(")
            .expect("the writer must back the config up");
        let reset_at = writer
            .find("config_writer::reset_shortcuts_checked()")
            .expect("the writer must then reset through the checked writer");
        assert!(
            backup_at < reset_at,
            "the backup must be taken before the reset, not after: {writer}"
        );

        let click = body(
            src,
            "fn shortcut_reset_clicked(",
            "/// The whole Shortcuts page",
        );
        for needle in [
            "reset_shortcuts_with_backup",
            "RESET_ARM_SETTLE",
            "Instant::now()",
            "self.set_shortcut_reset_arm(",
        ] {
            assert!(
                click.contains(needle),
                "shortcut_reset_clicked must use `{needle}`: {click}"
            );
        }
    }

    /// Item 2 of the Phase 4 audit: an armed row swallowed every chord, so a
    /// user who clicked into the search field and typed rebound the row to
    /// the first letter. A click anywhere but the armed row now disarms it.
    #[test]
    fn an_armed_row_disarms_when_a_click_lands_elsewhere() {
        let src = include_str!("shortcuts.rs");
        let row = body(
            src,
            "fn render_shortcut_row(",
            "/// The section card, sliced",
        );
        let armed = body(row, ".when(is_recording,", "})");
        assert!(
            armed.contains("on_mouse_down_out("),
            "the armed row must listen for a click outside itself: {armed}"
        );
        assert!(
            armed.contains("disarm_shortcut_recording("),
            "and that click must disarm the row: {armed}"
        );
    }

    /// Item 3 of the Phase 4 audit: GPUI fires the list's `on_mouse_up` only
    /// over the list, so a thumb drag released off the panel left
    /// `shortcut_drag` set - bare hover then scrolled the list, and the
    /// `ListState`'s lazy measurement stayed frozen. A move with no button
    /// held ends the drag instead of continuing it.
    #[test]
    fn a_move_with_no_button_held_ends_a_thumb_drag() {
        let src = include_str!("shortcuts.rs");
        let region = body(
            src,
            "fn shortcut_list_region(",
            "/// Search field + key-capture toggle",
        );
        let on_move = body(region, ".on_mouse_move(", ".on_mouse_up(");
        assert!(
            on_move.contains("pressed_button != Some(MouseButton::Left)"),
            "the move listener must check that a button is held: {on_move}"
        );
        let heal = body(
            on_move,
            "pressed_button != Some(MouseButton::Left)",
            "return;",
        );
        assert!(
            heal.contains("scrollbar::end_drag(&this.shortcut_list, this.shortcut_drag.take())"),
            "a stale drag must be ended, not merely ignored: {heal}"
        );
    }

    /// A `List` lays every item out as its own layout root, so an `auto` width
    /// resolves to the item's *content* width instead of stretching the way a
    /// flex child does. Dropping `w_full` from a row therefore does not fail to
    /// compile and does not fail any logic test - it just renders the page as a
    /// ragged staircase of shrink-wrapped cards, which is exactly how this
    /// shipped once upstream. Pin the invariant on the real element.
    #[gpui::test]
    fn list_items_span_the_full_list_width(cx: &mut gpui::TestAppContext) {
        struct Probe {
            state: ListState,
        }

        impl gpui::Render for Probe {
            fn render(
                &mut self,
                _window: &mut gpui::Window,
                _cx: &mut Context<Self>,
            ) -> impl IntoElement {
                let card = card_color();
                let ui = crate::theme::ui_colors();
                list(self.state.clone(), move |index, _window, _cx| {
                    // The real row shape: a squircle-skinned flex row with a
                    // truncating label and a trailing badge.
                    let row = squircle_skin(
                        div()
                            .id(("probe", index))
                            .flex()
                            .flex_row()
                            .items_center()
                            .justify_between()
                            .gap(px(12.))
                            .px(px(8.))
                            .py(px(10.)),
                        format!("probe-skin-{index}"),
                        ROW_RADIUS,
                        None,
                        Some(ui.subtle),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .child(format!("action {index}")),
                    )
                    .child(div().px(px(10.)).py(px(3.)).child("Ctrl+X"));
                    shortcut_card_slice(card, index == 0, index + 1 == PROBE_ITEMS)
                        .debug_selector(move || format!("probe-card-{index}"))
                        .child(row)
                        .into_any_element()
                })
                .size_full()
            }
        }

        const PROBE_ITEMS: usize = 6;
        const WIDTH: f32 = 640.0;

        let (view, cx) = cx.add_window_view(|_, _| {
            let state = new_shortcut_list_state();
            state.reset(PROBE_ITEMS);
            Probe { state }
        });
        cx.simulate_resize(gpui::size(px(WIDTH), px(400.0)));
        cx.run_until_parked();

        // `ListState::bounds_for_item` reports the *list's* width, not the
        // item's, so it cannot see this bug. Read what was actually painted.
        let painted = cx
            .debug_bounds("probe-card-1")
            .expect("item 1 must be painted");
        let viewport = view.read_with(cx, |probe, _| probe.state.viewport_bounds().size.width);
        assert_eq!(
            painted.size.width, viewport,
            "a list item that does not span the list is shrink-wrapping its content"
        );
    }
}
