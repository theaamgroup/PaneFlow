//! The entity that hosts a [`CodeElement`]: one open file, its scroll state,
//! its caret, and the mouse plumbing the element cannot own.
//!
//! `CodeElement` paints; everything that has to survive between frames lives
//! here. The split follows Zed's `Editor` / `EditorElement` pair and Paneflow's
//! own diff dock: state on the entity, geometry on the element, handed back
//! through a single `Rc<Cell<CodeGeometry>>` the element writes during
//! `prepaint` and the wheel / scrollbar handlers read.
//!
//! ## The editor owns its scroll (EP-008)
//!
//! This view does **not** use the two-axis recipe (`overflow_y_scroll` plus
//! `track_scroll` plus `restrict_scroll_to_axis`) the diff dock documents in
//! `CLAUDE.md` ("GPUI scroll & wheel"). It did until EP-008, and dropping the recipe
//! without replacing every part of it yields a frozen viewport, so the parts
//! are listed here with what replaced them:
//!
//! - The host div is `overflow_hidden()`: GPUI translates nothing. The
//!   position lives in [`CodeScroll`] as a fractional **row** (`f64`), shared
//!   with the element by `Rc`; the element fills the viewport and places every
//!   row at `origin.y + (row - scroll_rows) * CODE_ROW_HEIGHT`, rounded to the
//!   device pixel. A pixel offset would stop resolving single pixels somewhere
//!   past 200 000 lines; a row never does.
//! - The scrollbar reads the same number through [`ScrollableHandle`], so the
//!   painted thumb, the dragged thumb, [`CodeView::reveal_cursor`],
//!   [`CodeView::page_rows`] and the drag autoscroll cannot diverge.
//! - The wheel is converted here, by [`wheel_pixels`]: a `Lines` notch is
//!   exactly three `CODE_ROW_HEIGHT` rows vertically (never the host's
//!   inherited line height) and whole columns horizontally; a trackpad's
//!   `Pixels` delta passes through unrounded. A document shorter than the
//!   viewport absorbs a notch without a repaint.
//! - `CodeScroll::set_rows` refuses to move while the viewport is 0 px tall,
//!   so a frame laid out at zero height (a collapsed dock, a tab mid-switch)
//!   cannot rewind the file to row 0.
//!
//! Horizontal always reads `delta.x`: macOS delivers horizontal natively, and
//! Shift+wheel arrives already swapped onto the X axis with `delta.y` zeroed,
//! so branching on `modifiers.shift` would read a zero.
//!
//! ## Caret and selection (EP-003)
//!
//! The caret is a byte offset carried by a [`CodeSelection`], the same shape
//! `widgets/text_area.rs:441` uses. Every "where does it land" rule lives in
//! [`super::cursor`], which knows nothing about GPUI; this file only turns an
//! event into one call and one repaint. Hit-testing goes through the element's
//! [`CodeHitMap`], i.e. the real `ShapedLine`s of the frame that was painted,
//! so a click lands on a glyph boundary even with tabs or wide characters.
//!
//! ## Editing (EP-004)
//!
//! Every mutation of the rope goes through [`CodeView::splice_all`], including
//! the platform's own text input. That single door is what makes the read-only
//! refusal, the undo history and the dirty mark impossible to bypass: an action
//! handler that spliced directly would silently skip all three.
//!
//! Three decisions are deliberate and worth stating rather than rediscovering:
//!
//! - **The IME composition lives in the document.** US-012 asks that the
//!   document be "mutated only on commit", but GPUI's `EntityInputHandler`
//!   protocol reads the marked text back out of the buffer
//!   (`text_for_range`, `bounds_for_range`), so a preedit held on the side
//!   would render nothing and place the candidate window nowhere. The preedit
//!   is spliced in as a single typing transaction, tracked in `marked`, and
//!   painted underlined so it reads as uncommitted - the same shape as Zed's
//!   `Editor` and `widgets/text_area.rs`. A commit replaces it in place; an
//!   abandoned composition is removed, never left pending.
//! - **This view owns its own conflict watcher.** `diff/view/watcher.rs`
//!   watches a worktree, on another entity, only while the diff is open. What
//!   is reused is its shape (parent directory, non-recursive, debounced), not
//!   its instance.
//! - **Disk work runs on GPUI's background executor**, not `smol::unblock`.
//!   Both keep the render thread free; only the former is driven by the test
//!   scheduler, which is what lets `Ctrl+S` and the conflict refusal be proven
//!   from the action rather than from `super::save` alone.
//!
//! ## Reloads (EP-004, EP-009)
//!
//! An external write is folded in as a batch of line hunks, not a whole-file
//! replacement: [`reload_from_disk`] snapshots the rope and its revision on
//! the main thread, runs [`edit::disk_splices`] on the background executor,
//! and delivers `(revision, splices)` back. A delivery whose revision is stale
//! (the user typed meanwhile) recomputes once ([`RELOAD_DIFF_ATTEMPTS`]); a
//! second miss, or a document that is dirty when the diff settles, becomes a
//! conflict and the user's text stays - unless the reload was forced by the
//! banner's "Reload from disk". The hunks reach [`CodeView::splice_all`] as
//! one batch, so the highlighter sees one generation and the history one
//! transaction: a 10-line agent rewrite of a 9 000-line file is a 10-line
//! undo step, the caret keeps its place, and the undo stack is bounded by
//! bytes ([`edit::MAX_UNDO_BYTES`]) as well as by count. `disk_generation`
//! still orders probes against saves; `revision` orders the diff against
//! edits.

use std::cell::{Cell, RefCell};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::StreamExt;
use futures::channel::mpsc;
use futures::future::Either;
use gpui::{
    Anchor, AnyElement, App, AppContext, AsyncApp, Bounds, ClickEvent, ClipboardItem, Context,
    CursorStyle, EntityInputHandler, FocusHandle, Focusable, FontWeight, Hsla, InteractiveElement,
    IntoElement, KeyBinding, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    ParentElement, Pixels, Point, Render, Role, ScrollDelta, ScrollWheelEvent, SharedString,
    StatefulInteractiveElement, Styled, StyledText, UTF16Selection, WeakEntity, Window, actions,
    anchored, deferred, div, point, px, size,
};
use notify::{RecursiveMode, Watcher};
use paneflow_textdiff::{Block, BlockKind, BlockTracker, ComparisonPolicy, split_lines};
use ropey::Rope;

/// The one link between the notify backend thread and the reload task.
///
/// See [`CodeView::_watch_bridge`] for why the sender lives behind a lock
/// rather than inside the watcher callback.
type WatchBridge = Arc<Mutex<Option<mpsc::UnboundedSender<notify::Result<notify::Event>>>>>;
/// The receiving end of that bridge, polled by the reload task.
type WatchEvents = mpsc::UnboundedReceiver<notify::Result<notify::Event>>;

/// OS watcher in production; `NullWatcher` under `cfg(test)` so GPUI's
/// scheduler never sees the notify-rs fsevents thread.
#[cfg(not(test))]
type ConflictWatcher = notify::RecommendedWatcher;
#[cfg(test)]
type ConflictWatcher = notify::NullWatcher;

use super::base::{Base, spawn_base_load};
use super::controls::EditorControls;
use super::cursor::{self, CodeSelection};
use super::document::{CodeDocument, CodeEdit, LineEnding, ReadOnlyReason, normalize_newlines};
use super::edit::{self, DocChange, EditGroup, IndentUnit};
use super::element::{
    CODE_FONT_SIZE, CODE_ROW_HEIGHT, CodeCaret, CodeColors, CodeElement, CodeGeometry, CodeHitMap,
    CodeScroll, GutterMemo, autoscroll_step, code_font, reveal_h_offset, reveal_rows,
    syntax_text_runs,
};
use super::highlight::{
    CodeHighlighter, DeferredParse, HighlightOutcome, SYNC_PARSE_BUDGET, spawn_deferred_parse,
};
use super::load::{CodeLoadError, CodeLoadSlot, CodeLoadState, CodeOpen, spawn_code_load};
use super::markers::MARKER_COLUMN_W;
use super::navigation::NavigationState;
use super::save::{self, FileStamp};
use crate::diff::{DiffSyntax, highlight_lines, palette};
use crate::settings::components::menu_surface;
use crate::terminal::blink::{BlinkPhaseGlobal, CURSOR_BLINK_INTERVAL};

/// Key context the editor's bindings are scoped to (US-009).
pub(crate) const CODE_KEY_CONTEXT: &str = "CodeEditor";

/// Whether a batch of splices can reach the highlighter as one call: every
/// hunk strictly above the one before it by row, in the document the batch is
/// about to be applied to. The order [`edit::disk_splices`] and the indent
/// commands emit.
fn ops_descend_by_row(doc: &CodeDocument, ops: &[(Range<usize>, String)]) -> bool {
    ops.windows(2)
        .all(|pair| doc.byte_to_line(pair[1].0.end) < doc.byte_to_line(pair[0].0.start))
}

/// A wheel delta in editor pixels (US-023). A `Lines` notch is one editor row
/// per line vertically - never the host div's inherited line height - and one
/// column per line horizontally; a trackpad's `Pixels` delta passes through
/// unrounded.
fn wheel_pixels(delta: &ScrollDelta, char_w: f32) -> Point<f32> {
    match delta {
        ScrollDelta::Pixels(pixels) => Point::new(f32::from(pixels.x), f32::from(pixels.y)),
        ScrollDelta::Lines(lines) => Point::new(lines.x * char_w, lines.y * CODE_ROW_HEIGHT),
    }
}

/// Two presses closer together than this, and within [`MULTI_CLICK_RADIUS`],
/// chain into a double then a triple click. Same values as
/// `widgets/text_area.rs`, so the two editors feel identical.
const MULTI_CLICK_INTERVAL: Duration = Duration::from_millis(400);
const MULTI_CLICK_RADIUS: f32 = 2.0;

/// Rows an out-of-viewport drag scrolls per mouse-move event (US-010).
const DRAG_SCROLL_ROWS: f32 = 1.0;
/// Columns the same drag scrolls horizontally. Three columns is close to one
/// row height on the editor's mono font, so both axes move at a similar visual
/// speed.
const DRAG_SCROLL_COLUMNS: f32 = 3.0;

/// How long a refused keystroke lights the read-only banner up (US-012). Long
/// enough to be noticed, short enough not to linger after a burst of typing.
const READ_ONLY_FLASH: Duration = Duration::from_millis(600);

/// Quiet period a burst of filesystem events has to end with before the file is
/// re-read (US-016). Same value, same reasoning as `markdown/view.rs`: an
/// editor writing through a temp file emits several events per save.
const RELOAD_DEBOUNCE: Duration = Duration::from_millis(200);

/// How many times a reload's background diff may be computed before a document
/// that keeps changing underneath it is left to the user as a conflict
/// (EP-009): the first attempt plus one recomputation.
const RELOAD_DIFF_ATTEMPTS: usize = 2;

pub(crate) const TRACKER_DEBOUNCE: Duration = Duration::from_millis(150);
const TRACKER_POLICY: ComparisonPolicy = ComparisonPolicy::Default;

pub(crate) const POPUP_SHOWN_LINES: usize = 200;
const POPUP_VISIBLE_ROWS: f32 = 12.0;
const POPUP_MIN_W: f32 = 280.0;
const POPUP_MAX_W: f32 = 520.0;
const POPUP_MARGIN: f32 = 12.0;
const POPUP_HEADER_H: f32 = 28.0;
const POPUP_ACTIONS_H: f32 = 34.0;
const POPUP_FOOTER_H: f32 = 20.0;
const POPUP_PADDING: f32 = 6.0;

actions!(
    paneflow_code_editor,
    [
        /// Move the caret one grapheme left.
        CeLeft,
        /// Move the caret one grapheme right.
        CeRight,
        /// Move the caret one row up, keeping the goal column.
        CeUp,
        /// Move the caret one row down, keeping the goal column.
        CeDown,
        /// Extend the selection one grapheme left.
        CeSelectLeft,
        /// Extend the selection one grapheme right.
        CeSelectRight,
        /// Extend the selection one row up.
        CeSelectUp,
        /// Extend the selection one row down.
        CeSelectDown,
        /// Move the caret to the start of the previous word.
        CeWordLeft,
        /// Move the caret to the end of the next word.
        CeWordRight,
        /// Extend the selection to the start of the previous word.
        CeSelectWordLeft,
        /// Extend the selection to the end of the next word.
        CeSelectWordRight,
        /// Move the caret to the first column of its row.
        CeHome,
        /// Move the caret past the last column of its row.
        CeEnd,
        /// Extend the selection to the first column of its row.
        CeSelectHome,
        /// Extend the selection past the last column of its row.
        CeSelectEnd,
        /// Move the caret one viewport up.
        CePageUp,
        /// Move the caret one viewport down.
        CePageDown,
        /// Extend the selection one viewport up.
        CeSelectPageUp,
        /// Extend the selection one viewport down.
        CeSelectPageDown,
        /// Move the caret to the first byte of the document.
        CeDocStart,
        /// Move the caret to the last byte of the document.
        CeDocEnd,
        /// Extend the selection to the first byte of the document.
        CeSelectDocStart,
        /// Extend the selection to the last byte of the document.
        CeSelectDocEnd,
        /// Select the whole document.
        CeSelectAll,
        /// Delete the selection, or the grapheme before the caret.
        CeBackspace,
        /// Delete the selection, or the grapheme after the caret.
        CeDelete,
        /// Insert a newline, repeating the current row's indentation.
        CeNewline,
        /// Undo the newest transaction.
        CeUndo,
        /// Redo the newest undone transaction.
        CeRedo,
        /// Copy the selection, or the whole current row.
        CeCopy,
        /// Cut the selection, or the whole current row.
        CeCut,
        /// Paste the clipboard, sanitized.
        CePaste,
        /// Indent the selected rows by one level.
        CeIndent,
        /// Outdent the selected rows by one level.
        CeOutdent,
        /// Write the document back to disk.
        CeSave,
        /// Dismiss the marker popup.
        CeEscape,
    ]
);

/// Register the code editor's key bindings (US-011).
///
/// Called from [`crate::keybindings::apply_keybindings`], which clears every
/// binding before rebuilding them, so this has to run on every apply and not
/// only at startup.
///
/// Each shortcut is declared once, with its platform variants adjacent: the
/// shared half is unconditional, and the two `cfg` blocks carry only the chords
/// that genuinely differ (word motion and document ends follow the macOS
/// Option / Command conventions, everything else is identical). Nothing here
/// installs a catch-all key handler, so a key with no binding bubbles to the
/// parent dispatch instead of dying on the editor.
pub(crate) fn register_keybindings(cx: &mut App) {
    let ctx = Some(CODE_KEY_CONTEXT);
    cx.bind_keys([
        KeyBinding::new("left", CeLeft, ctx),
        KeyBinding::new("right", CeRight, ctx),
        KeyBinding::new("up", CeUp, ctx),
        KeyBinding::new("down", CeDown, ctx),
        KeyBinding::new("shift-left", CeSelectLeft, ctx),
        KeyBinding::new("shift-right", CeSelectRight, ctx),
        KeyBinding::new("shift-up", CeSelectUp, ctx),
        KeyBinding::new("shift-down", CeSelectDown, ctx),
        KeyBinding::new("home", CeHome, ctx),
        KeyBinding::new("end", CeEnd, ctx),
        KeyBinding::new("shift-home", CeSelectHome, ctx),
        KeyBinding::new("shift-end", CeSelectEnd, ctx),
        KeyBinding::new("pageup", CePageUp, ctx),
        KeyBinding::new("pagedown", CePageDown, ctx),
        KeyBinding::new("shift-pageup", CeSelectPageUp, ctx),
        KeyBinding::new("shift-pagedown", CeSelectPageDown, ctx),
        // `secondary` is Cmd on macOS and Ctrl elsewhere, so Select All needs
        // no platform split.
        KeyBinding::new("secondary-a", CeSelectAll, ctx),
        // Editing (EP-004). `secondary` is Cmd on this macOS-only fork, so
        // redo is `secondary-shift-z` (Cmd+Shift+Z); there is no extra redo chord.
        KeyBinding::new("backspace", CeBackspace, ctx),
        KeyBinding::new("delete", CeDelete, ctx),
        KeyBinding::new("enter", CeNewline, ctx),
        KeyBinding::new("tab", CeIndent, ctx),
        KeyBinding::new("shift-tab", CeOutdent, ctx),
        KeyBinding::new("secondary-z", CeUndo, ctx),
        KeyBinding::new("secondary-shift-z", CeRedo, ctx),
        KeyBinding::new("secondary-c", CeCopy, ctx),
        KeyBinding::new("secondary-x", CeCut, ctx),
        KeyBinding::new("secondary-v", CePaste, ctx),
        KeyBinding::new("secondary-s", CeSave, ctx),
        KeyBinding::new("escape", CeEscape, ctx),
    ]);
    #[cfg(target_os = "macos")]
    cx.bind_keys([
        KeyBinding::new("alt-left", CeWordLeft, ctx),
        KeyBinding::new("alt-right", CeWordRight, ctx),
        KeyBinding::new("alt-shift-left", CeSelectWordLeft, ctx),
        KeyBinding::new("alt-shift-right", CeSelectWordRight, ctx),
        KeyBinding::new("cmd-up", CeDocStart, ctx),
        KeyBinding::new("cmd-down", CeDocEnd, ctx),
        KeyBinding::new("cmd-shift-up", CeSelectDocStart, ctx),
        KeyBinding::new("cmd-shift-down", CeSelectDocEnd, ctx),
    ]);
}

/// What a mouse drag selects by (US-010): the granularity the opening press
/// established, kept for the whole drag.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DragGrain {
    Grapheme,
    Word,
    Line,
}

/// A live text drag: its granularity plus the range the opening press selected,
/// which a word or line drag always keeps covered.
#[derive(Clone, Debug)]
struct TextDrag {
    grain: DragGrain,
    anchor: Range<usize>,
}

/// Multi-click accumulator: when and where the last press landed, and how many
/// presses have chained so far.
#[derive(Clone, Copy)]
struct ClickChain {
    at: Instant,
    position: Point<Pixels>,
    count: u8,
}

/// How the in-memory document stands against the file on disk (US-016).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
enum DiskState {
    /// The last stamp the editor took still describes the file.
    #[default]
    InSync,
    /// Someone else wrote the file. Nothing is overwritten until the user
    /// picks a side.
    Conflict,
    /// The file is gone. Saving recreates it.
    Deleted,
}

type PopupLine = (SharedString, Vec<(Range<usize>, Hsla)>);

struct MarkerPopup {
    block: Block,
    title: String,
    shown: Vec<PopupLine>,
    hidden: usize,
    base_text: String,
}

/// What a reload's background diff runs against: a snapshot of the rope
/// (cheap, ropey shares chunks) and the revision it was taken at, so the
/// splices can be refused if an edit landed while the diff ran.
struct DiskDiff {
    rope: Rope,
    revision: u64,
}

impl DiskDiff {
    fn of(doc: &CodeDocument) -> Self {
        Self {
            rope: doc.text().clone(),
            revision: doc.revision(),
        }
    }
}

/// What the background diff delivers: the hunks that turn the snapshot into
/// the incoming text, stamped with the revision the snapshot was taken at.
struct DiskSplices {
    revision: u64,
    splices: Vec<(Range<usize>, String)>,
}

/// What a probe read off disk: the document built from the bytes and the
/// stamp of those bytes, or the written refusal.
type DiskLoad = Result<(CodeDocument, Option<FileStamp>), CodeLoadError>;

/// Fold a disk probe into the view: the guards and the rope snapshot on the
/// main thread, the line diff on the background executor, the splices back
/// on the main thread (EP-009). Returns `false` once the view is gone, which
/// is the watcher loop's exit condition.
///
/// `generation` orders this probe against saves and newer probes exactly as
/// before; the document revision inside [`DiskDiff`] orders the diff against
/// edits typed while it ran. A stale revision buys one recomputation
/// ([`RELOAD_DIFF_ATTEMPTS`]); after that the document is a conflict.
async fn reload_from_disk(
    this: &WeakEntity<CodeView>,
    cx: &mut AsyncApp,
    generation: u64,
    loaded: DiskLoad,
    force: bool,
) -> bool {
    let begun = cx.update(|cx| {
        this.update(cx, |view: &mut CodeView, cx: &mut Context<CodeView>| {
            view.begin_disk_reload(generation, loaded, force, cx)
        })
    });
    let Ok(begun) = begun else {
        return false;
    };
    let Some((mut diff, incoming)) = begun else {
        return true;
    };
    let incoming = Arc::new(incoming);
    for attempt in 0..RELOAD_DIFF_ATTEMPTS {
        let DiskDiff { rope, revision } = diff;
        let source = Arc::clone(&incoming);
        let splices = cx
            .background_spawn(async move { edit::disk_splices(&rope, &source.text().to_string()) })
            .await;
        let retry = attempt + 1 < RELOAD_DIFF_ATTEMPTS;
        let finished = cx.update(|cx| {
            this.update(cx, |view: &mut CodeView, cx: &mut Context<CodeView>| {
                view.finish_disk_reload(
                    generation,
                    DiskSplices { revision, splices },
                    &incoming,
                    retry,
                    force,
                    cx,
                )
            })
        });
        let Ok(next) = finished else {
            return false;
        };
        match next {
            Some(again) => diff = again,
            None => return true,
        }
    }
    true
}

/// One open file inside the diff dock.
pub(crate) struct CodeView {
    pub(crate) controls: gpui::Entity<EditorControls>,
    pub(super) navigation: NavigationState,
    path: PathBuf,
    state: CodeLoadState,
    /// Generation guard: a load that lands after the tab moved on is dropped
    /// without repainting (US-002).
    slot: CodeLoadSlot,
    /// Keyboard focus (US-009). Owning it here is what scopes the
    /// [`CODE_KEY_CONTEXT`] bindings to this widget, and what tells the element
    /// whether to paint a caret at all.
    focus: FocusHandle,
    /// Vertical scroll position, owned here as a fractional row and shared
    /// with the element by `Rc` (US-024). See the module docs.
    scroll: CodeScroll,
    /// Live horizontal offset in pixels, always `>= 0` (US-008).
    h_offset: f32,
    /// Caret plus selection anchor, in document bytes (US-009, US-010).
    selection: CodeSelection,
    /// Char column vertical motion aims at, so Up/Down across a short row come
    /// back to where the caret started (US-011).
    goal_column: usize,
    /// Live text selection drag (US-010).
    text_drag: Option<TextDrag>,
    /// Double / triple click tracking (US-010).
    click_chain: Option<ClickChain>,
    /// When the caret last moved. It stops blinking for one interval after
    /// that, which is the "solid while you work" behavior US-009 asks for.
    last_motion: Instant,
    /// Blink phase, mirrored from the app-wide [`BlinkPhaseGlobal`].
    blink_visible: bool,
    /// Theme snapshot the highlighter's colors were resolved against (US-005).
    theme_generation: u64,
    /// Geometry the element resolves each `prepaint` and the handlers read back.
    geometry: Rc<Cell<CodeGeometry>>,
    /// Gutter width memo, keyed on the line-number digit count (US-006).
    gutter_memo: Rc<Cell<GutterMemo>>,
    /// The frame's shaped lines, published by the element for hit-testing.
    hits: Rc<RefCell<CodeHitMap>>,
    /// Stable element id, built once so the render hot path never formats a
    /// string per frame.
    element_id: SharedString,
    /// Undo / redo stack (US-013).
    history: edit::UndoHistory,
    /// Where the history stood when the file last agreed with disk. The dirty
    /// mark is `history.mark() != saved_mark`, which is what makes undoing back
    /// to the saved state clear the dot instead of stacking a second change.
    saved_mark: edit::HistoryMark,
    /// Indentation Tab inserts, detected from the file at load (US-014).
    indent: IndentUnit,
    /// Byte range of the live IME composition (US-012).
    marked: Option<Range<usize>>,
    /// When a keystroke was last refused because the document is read-only.
    /// Drives the banner flash (US-012).
    read_only_flash: Option<Instant>,
    /// What the file looked like on disk when it was last read or written
    /// (US-016). `None` means it is not there.
    stamp: Option<FileStamp>,
    /// Incoming stamp from [`Self::begin_disk_reload`], adopted in
    /// [`Self::finish_disk_reload`] only after splices land. Held here so
    /// `finish_disk_reload` stays inside the clippy argument cap; a save in
    /// the window still copies `stamp`, not this.
    pending_stamp: Option<FileStamp>,
    /// Whether an agent got to the file first (US-016).
    disk: DiskState,
    /// Written explanation of the last failed save (US-015).
    save_error: Option<String>,
    /// Ordering guard for watcher probes and saves. Every disk read claims a
    /// generation before it starts; a save advances it before writing and the
    /// post-save re-stat claims another, so older bytes cannot land afterward.
    disk_generation: u64,
    /// A save is in flight; a second Ctrl+S is ignored rather than racing it.
    saving: bool,
    /// A background longest-line scan is running. Further shrinks set
    /// [`Self::longest_line_rescan_needed`] instead of launching another walk.
    longest_line_scan_in_flight: bool,
    /// Another shrink landed while a scan ran; refresh once more at the
    /// latest revision when that scan completes.
    longest_line_rescan_needed: bool,
    /// Parent-directory watcher (US-016). Held only to keep it alive: dropping
    /// it unregisters the watch. Tests hold a `NullWatcher` so the seed write
    /// cannot land on a live FSEvents thread and trip the GPUI scheduler.
    _watcher: Option<ConflictWatcher>,
    /// The sender the watcher callback writes into, owned here rather than by
    /// the callback (US-016).
    ///
    /// Every wake of the reload task has to happen on the thread that owns
    /// this view, or GPUI's test scheduler rightly calls the test
    /// non-deterministic. The callback owning the sender breaks that twice:
    /// `INotifyWatcher::drop` only posts a shutdown message, so the backend
    /// thread drops the callback - and with it the last sender, closing the
    /// channel - after the drop has already returned, and until it gets there
    /// it can still deliver one last event. Both wakes land on the notify
    /// thread.
    ///
    /// Holding the sender behind a lock fixes both: dropping the watcher
    /// closes nothing, and clearing the option severs the callback before the
    /// watcher goes away, from whichever thread does the clearing.
    ///
    /// Declared after `_watcher` on purpose: fields drop in declaration order,
    /// so the watch is unregistered first and the channel closes second.
    _watch_bridge: Option<WatchBridge>,
    base: Base,
    tracker: BlockTracker,
    tracker_generation: u64,
    /// Independent of [`Self::slot`]: overlapping HEAD reloads must not share
    /// the document-load generation, or a slower older probe can install last.
    base_generation: u64,
    hovered_marker: Option<usize>,
    popup: Option<MarkerPopup>,
}

impl CodeView {
    /// Open `path`. The read, the rope and the first parse all happen off the
    /// render thread; the view renders a spinner until they land.
    pub(crate) fn new(path: PathBuf, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        let controls = EditorControls::attach(focus.clone(), cx);
        let mut view = Self {
            controls,
            navigation: NavigationState::default(),
            element_id: format!("code-view:{}", path.display()).into(),
            path,
            state: CodeLoadState::Loading,
            slot: CodeLoadSlot::new(),
            focus,
            scroll: CodeScroll::new(),
            h_offset: 0.0,
            selection: CodeSelection::default(),
            goal_column: 0,
            text_drag: None,
            click_chain: None,
            last_motion: Instant::now(),
            blink_visible: true,
            theme_generation: crate::theme::theme_generation(),
            geometry: Rc::new(Cell::new(CodeGeometry::default())),
            gutter_memo: Rc::new(Cell::new(GutterMemo::default())),
            hits: Rc::new(RefCell::new(CodeHitMap::default())),
            history: edit::UndoHistory::default(),
            saved_mark: edit::HistoryMark::default(),
            indent: IndentUnit::Spaces(4),
            marked: None,
            read_only_flash: None,
            stamp: None,
            pending_stamp: None,
            disk: DiskState::default(),
            save_error: None,
            disk_generation: 0,
            saving: false,
            longest_line_scan_in_flight: false,
            longest_line_rescan_needed: false,
            _watcher: None,
            _watch_bridge: None,
            base: Base::None,
            tracker: BlockTracker::inactive(),
            tracker_generation: 0,
            base_generation: 0,
            hovered_marker: None,
            popup: None,
        };
        view.observe_blink(cx);
        view.start_load(cx);
        view
    }

    /// A view that is already `Ready` on `text`, built on the caller's thread
    /// with no load task, no blink observer and no watcher: what the
    /// scroll-frame measurement in `layout/render.rs` docks beside its
    /// terminal panes (#425). Lists every field so a new one has to be
    /// placed here deliberately.
    #[cfg(test)]
    pub(crate) fn ready_for_test(path: PathBuf, text: &str, cx: &mut Context<Self>) -> Self {
        let document = super::load::build_document(path.clone(), text, false);
        let highlighter = CodeHighlighter::new(
            &document,
            DiffSyntax::from_theme(&crate::theme::active_theme()),
        );
        let focus = cx.focus_handle();
        let controls = EditorControls::attach(focus.clone(), cx);
        Self {
            controls,
            navigation: NavigationState::default(),
            element_id: format!("code-view:{}", path.display()).into(),
            path,
            state: CodeLoadState::Ready(Box::new(super::load::LoadedCode {
                document,
                highlighter,
                indent: IndentUnit::Spaces(4),
                stamp: None,
            })),
            slot: CodeLoadSlot::new(),
            focus,
            scroll: CodeScroll::new(),
            h_offset: 0.0,
            selection: CodeSelection::default(),
            goal_column: 0,
            text_drag: None,
            click_chain: None,
            last_motion: Instant::now(),
            blink_visible: true,
            theme_generation: crate::theme::theme_generation(),
            geometry: Rc::new(Cell::new(CodeGeometry::default())),
            gutter_memo: Rc::new(Cell::new(GutterMemo::default())),
            hits: Rc::new(RefCell::new(CodeHitMap::default())),
            history: edit::UndoHistory::default(),
            saved_mark: edit::HistoryMark::default(),
            indent: IndentUnit::Spaces(4),
            marked: None,
            read_only_flash: None,
            stamp: None,
            pending_stamp: None,
            disk: DiskState::default(),
            save_error: None,
            disk_generation: 0,
            saving: false,
            longest_line_scan_in_flight: false,
            longest_line_rescan_needed: false,
            _watcher: None,
            _watch_bridge: None,
            base: Base::None,
            tracker: BlockTracker::inactive(),
            tracker_generation: 0,
            base_generation: 0,
            hovered_marker: None,
            popup: None,
        }
    }

    /// Mirror the app-wide cursor blink (US-009). `try_global` rather than
    /// `global` so a headless or test-built view degrades to a solid caret
    /// instead of panicking, the same fallback `terminal/view.rs` takes.
    fn observe_blink(&mut self, cx: &mut Context<Self>) {
        let Some(global) = cx.try_global::<BlinkPhaseGlobal>() else {
            log::warn!("BlinkPhaseGlobal not installed - the code caret will not blink");
            return;
        };
        let phase = global.0.clone();
        cx.observe(&phase, |view: &mut Self, phase, cx: &mut Context<Self>| {
            // A caret that just moved stays solid for a full interval: blinking
            // through a burst of navigation is what makes a caret hard to
            // follow.
            let visible =
                view.last_motion.elapsed() < CURSOR_BLINK_INTERVAL || phase.read(cx).visible;
            if visible != view.blink_visible {
                view.blink_visible = visible;
                cx.notify();
            }
        })
        .detach();
    }

    /// Point the view at a different file, cancelling whatever load is in
    /// flight.
    pub(crate) fn open(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.element_id = format!("code-view:{}", path.display()).into();
        self.path = path;
        self.state = CodeLoadState::Loading;
        self.h_offset = 0.0;
        self.selection = CodeSelection::default();
        self.goal_column = 0;
        self.text_drag = None;
        self.click_chain = None;
        self.gutter_memo.set(GutterMemo::default());
        self.geometry.set(CodeGeometry::default());
        *self.hits.borrow_mut() = CodeHitMap::default();
        self.scroll.reset_rows();
        self.history.clear();
        self.saved_mark = edit::HistoryMark::default();
        self.marked = None;
        self.read_only_flash = None;
        self.stamp = None;
        self.pending_stamp = None;
        self.disk = DiskState::default();
        self.save_error = None;
        self.disk_generation = self.disk_generation.wrapping_add(1);
        self.saving = false;
        self.longest_line_scan_in_flight = false;
        self.longest_line_rescan_needed = false;
        self._watcher = None;
        self._watch_bridge = None;
        self.base = Base::None;
        self.tracker = BlockTracker::inactive();
        self.tracker_generation = self.tracker_generation.wrapping_add(1);
        self.hovered_marker = None;
        self.popup = None;
        self.start_load(cx);
        cx.notify();
    }

    fn start_load(&mut self, cx: &mut Context<Self>) {
        let generation = self.slot.begin();
        let syntax = DiffSyntax::from_theme(&crate::theme::active_theme());
        self.theme_generation = crate::theme::theme_generation();
        spawn_code_load(
            self.path.clone(),
            generation,
            syntax,
            cx,
            |view: &mut Self, generation, outcome: CodeOpen, cx| {
                view.apply_load(generation, outcome, cx);
            },
        );
    }

    /// Land a guarded open outcome. A stale generation is dropped without
    /// repainting (US-002).
    fn apply_load(&mut self, generation: u64, outcome: CodeOpen, cx: &mut Context<Self>) {
        if !self.slot.accept(generation) {
            return;
        }
        // The indent unit and the disk stamp are both properties of the file
        // that just landed, so they are adopted here rather than derived at
        // the first Tab or the first save, when the file may already have
        // moved on. Both come off the loader: the indent was detected over
        // the lines off-thread, and the stamp is the one taken from the
        // handle the bytes were read through, never a fresh stat of the path
        // - by now an agent may have rewritten the file, and a stamp of that
        // rewrite would let the next save clobber it without a conflict.
        match outcome {
            Ok(loaded) => {
                self.indent = loaded.indent;
                self.stamp = loaded.stamp;
                self.state = CodeLoadState::Ready(Box::new(loaded));
                self.start_base_load(cx);
            }
            Err(err) => {
                self.stamp = None;
                self.state = CodeLoadState::Failed(err);
            }
        }
        self.start_watcher(cx);
        cx.notify();
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn document(&self) -> Option<&CodeDocument> {
        self.state.document()
    }

    pub(crate) fn highlighter(&self) -> Option<&CodeHighlighter> {
        self.state.highlighter()
    }

    /// The rows the element shapes for the current position and viewport.
    #[cfg(test)]
    pub(crate) fn visible_row_range(&self) -> Range<usize> {
        let Some(line_count) = self.state.document().map(CodeDocument::line_count) else {
            return 0..0;
        };
        super::element::visible_rows_at(
            self.scroll.rows(),
            self.scroll.viewport_height(),
            line_count,
        )
    }

    #[cfg(test)]
    pub(crate) fn scroll_rows(&self) -> f64 {
        self.scroll.rows()
    }

    #[cfg(test)]
    pub(crate) fn scroll_offset_y(&self) -> f32 {
        self.scroll.content_top()
    }

    #[cfg(test)]
    pub(crate) fn materialized_lines(&self) -> usize {
        self.hits.borrow().materialized_lines
    }

    #[cfg(test)]
    pub(crate) fn materialized_numbers(&self) -> usize {
        self.hits.borrow().materialized_numbers
    }

    #[cfg(test)]
    pub(crate) fn row_width(&self, row: usize) -> Option<f32> {
        let hits = self.hits.borrow();
        let index = row.checked_sub(hits.first_row)?;
        Some(f32::from(hits.lines.get(index)?.as_ref()?.width()))
    }

    #[cfg(test)]
    pub(crate) fn row_top(&self, row: usize) -> f32 {
        let hits = self.hits.borrow();
        hits.top_y + row.saturating_sub(hits.first_row) as f32 * CODE_ROW_HEIGHT
    }

    /// The caret's byte offset (US-009).
    #[allow(dead_code)] // EP-003 accessor: no caller outside the view reads the cursor yet.
    pub(crate) fn cursor(&self) -> usize {
        self.selection.cursor()
    }

    /// The caret's row. Derived from the selection rather than stored, so the
    /// gutter highlight and the current-line wash can never drift from the byte
    /// offset that actually moved.
    #[allow(dead_code)] // EP-003 accessor: no caller outside the view reads the cursor row yet.
    pub(crate) fn cursor_row(&self) -> usize {
        self.document()
            .map(|doc| doc.byte_to_line(self.selection.cursor()))
            .unwrap_or(0)
    }

    /// The caret's 1-based `(line, column)`, for the dock's file header
    /// (US-018). The column counts characters, not bytes, so a line of accented
    /// text reports the position the user can actually count to. An unloaded
    /// document reports the top of an empty file rather than nothing, which is
    /// what the header shows while the spinner is up.
    pub(crate) fn cursor_line_column(&self) -> (usize, usize) {
        let Some(doc) = self.document() else {
            return (1, 1);
        };
        let offset = self.selection.cursor();
        (
            doc.byte_to_line(offset) + 1,
            cursor::goal_column(doc, offset) + 1,
        )
    }

    /// The refusal panel (US-003) plus, when a retry could actually clear the
    /// error, the reload button US-018 asks for. The written sentence and the
    /// icon still come from `diff_panel_centered`, so every dock state is drawn
    /// by one component; the button is the only thing layered on top.
    fn render_load_error(
        &self,
        message: String,
        ui: crate::theme::UiColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let panel =
            super::super::render::diff_panel_centered("icons/triangle-alert.svg", message, ui);
        if !self.state.is_retriable() {
            return panel;
        }
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .items_center()
            .pb(px(20.))
            .child(panel)
            .child(
                div()
                    .id("code-reload")
                    .flex_none()
                    .h(px(26.))
                    .px(px(10.))
                    .flex()
                    .items_center()
                    .rounded(px(6.))
                    .border_1()
                    .border_color(ui.border)
                    .cursor(CursorStyle::PointingHand)
                    .hover(|style| style.bg(ui.subtle))
                    .text_size(crate::ui_primitives::BODY)
                    .text_color(ui.text)
                    .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                        let path = this.path.clone();
                        this.open(path, cx);
                    }))
                    .child("Reload"),
            )
            .into_any_element()
    }

    /// The current selection, empty when the caret carries none.
    #[allow(dead_code)] // EP-003 accessor: no caller outside the view reads the selection yet.
    pub(crate) fn selection(&self) -> Range<usize> {
        self.selection.range()
    }

    /// Put the caret on `row`, column 0, and scroll it into view (US-007). The
    /// entry point the outline and the diff dock call to jump to a line.
    #[allow(dead_code)] // EP-003 setter: reserved for a jump-to-line entry point that has no gesture yet.
    pub(crate) fn set_cursor_row(&mut self, row: usize, cx: &mut Context<Self>) {
        let Some(doc) = self.state.document() else {
            return;
        };
        let row = row.min(doc.line_count().saturating_sub(1));
        let offset = doc.line_to_byte(row);
        self.place_caret(offset, false, cx);
    }

    /// Apply a resolved caret offset: clamp it to a legal slot, refresh the
    /// goal column, keep the caret solid, reveal it, repaint.
    ///
    /// Every keyboard motion and every mouse gesture funnels through here,
    /// which is what keeps the caret, the goal column, the blink and the scroll
    /// in step.
    fn place_caret(&mut self, offset: usize, extend: bool, cx: &mut Context<Self>) {
        self.end_typing_group();
        let Some(doc) = self.state.document() else {
            return;
        };
        let offset = cursor::clamp(doc, offset);
        let goal = cursor::goal_column(doc, offset);
        self.goal_column = goal;
        self.selection.apply(offset, extend);
        self.after_motion(cx);
    }

    /// Vertical motion, which is the one case that must *not* refresh the goal
    /// column: preserving it across a shorter row is the whole point (US-011).
    fn move_rows(&mut self, delta: isize, extend: bool, cx: &mut Context<Self>) {
        self.end_typing_group();
        let goal = self.goal_column;
        let Some(doc) = self.state.document() else {
            return;
        };
        let offset = cursor::vertical(doc, self.selection.cursor(), goal, delta);
        self.selection.apply(offset, extend);
        self.after_motion(cx);
    }

    fn after_motion(&mut self, cx: &mut Context<Self>) {
        self.last_motion = Instant::now();
        self.blink_visible = true;
        self.reveal_cursor();
        cx.notify();
    }

    /// Rows a Page key travels, derived from the live viewport.
    fn page_rows(&self) -> usize {
        cursor::page_rows(self.scroll.viewport_height(), CODE_ROW_HEIGHT)
    }

    /// Hand the document's current line count to the scroll state, so a move
    /// made between two frames clamps against the document as it stands now
    /// rather than as the last `prepaint` saw it.
    fn sync_scroll_line_count(&self) {
        if let Some(doc) = self.state.document() {
            self.scroll.set_line_count(doc.line_count());
        }
    }

    /// Scroll so the caret sits inside the viewport with the mandated margin,
    /// on both axes. A no-op when it is already comfortably visible.
    pub(crate) fn reveal_cursor(&mut self) {
        let viewport_h = self.scroll.viewport_height();
        let geometry = self.geometry.get();
        let h_offset = self.h_offset;
        let Some(doc) = self.state.document() else {
            return;
        };
        let offset = self.selection.cursor();
        let row = doc.byte_to_line(offset);
        let column = cursor::goal_column(doc, offset);
        self.scroll.set_line_count(doc.line_count());

        let target = reveal_rows(row, viewport_h, self.scroll.max_rows(), self.scroll.rows());
        self.scroll.set_rows(target);
        // The horizontal reveal uses the monospace advance rather than a shaped
        // x: the caret's row may not have been shaped this frame (it can be off
        // screen entirely), and the editor's font is mono by construction.
        let caret_x = column as f32 * geometry.char_w;
        self.h_offset = reveal_h_offset(
            caret_x,
            geometry.text_viewport_w,
            geometry.max_h_offset,
            h_offset,
        );
    }

    /// Recolor after a theme hot-reload (US-005).
    ///
    /// `set_syntax` re-derives every row's colors from the already-parsed trees,
    /// so this costs one requery and no reparse. GPUI's shaped-line cache keys
    /// on the `TextRun`s, colors included, so the new colors invalidate the
    /// cached glyphs by themselves.
    fn sync_theme(&mut self) {
        let generation = crate::theme::theme_generation();
        if generation == self.theme_generation {
            return;
        }
        self.theme_generation = generation;
        let syntax = DiffSyntax::from_theme(&crate::theme::active_theme());
        if let Some((doc, hl)) = self.state.editable() {
            hl.set_syntax(doc, syntax);
        }
    }

    /// Both wheel axes (US-008, US-023). The delta is converted here through
    /// [`wheel_pixels`], so a notch is three editor rows and a trackpad
    /// gesture is its exact pixels; nothing else scrolls the host.
    fn apply_wheel(&mut self, ev: &ScrollWheelEvent, cx: &mut Context<Self>) {
        let bounds = self.scroll.bounds();
        if !bounds.contains(&ev.position) {
            return;
        }
        let geometry = self.geometry.get();
        let delta = wheel_pixels(&ev.delta, geometry.char_w);
        self.sync_scroll_line_count();
        // GPUI deltas go negative toward the end of the axis; subtract so the
        // position grows toward the end of the file and the right of the line.
        let mut moved = self.scroll.scroll_by_pixels(-delta.y);
        if delta.x != 0.0 {
            let next = (self.h_offset - delta.x).clamp(0.0, geometry.max_h_offset);
            if next != self.h_offset {
                self.h_offset = next;
                moved = true;
            }
        }
        // A notch the document absorbs (shorter than the viewport, or already
        // at the end) must not repaint.
        if moved {
            cx.notify();
        }
    }

    fn on_scrollbar_down(&mut self, ev: &MouseDownEvent, cx: &mut Context<Self>) -> bool {
        let handled = self.navigation.mouse_down(
            ev.position,
            &self.scroll,
            &mut self.h_offset,
            self.geometry.get().max_h_offset,
        );
        if handled {
            cx.notify();
        }
        handled
    }

    pub(super) fn on_scrollbar_move(&mut self, ev: &MouseMoveEvent, cx: &mut Context<Self>) {
        if self.navigation.mouse_move(
            ev.position,
            ev.pressed_button == Some(MouseButton::Left),
            &self.scroll,
            &mut self.h_offset,
            self.geometry.get().max_h_offset,
        ) {
            cx.notify();
        }
    }

    pub(super) fn on_scrollbar_up(&mut self, _ev: &MouseUpEvent, cx: &mut Context<Self>) {
        if self.navigation.drag.take().is_some() {
            cx.notify();
        }
    }

    /// Resolve a window position to a caret slot through the frame's shaped
    /// lines (US-010).
    fn offset_at(&self, position: Point<Pixels>) -> Option<usize> {
        let doc = self.state.document()?;
        Some(self.hits.borrow().offset_at(doc, position))
    }

    /// Advance the double / triple click chain and return how many presses have
    /// landed in a row (1, 2 or 3, then back to 1).
    fn chain_click(&mut self, position: Point<Pixels>, now: Instant) -> u8 {
        let count = match self.click_chain {
            Some(prev)
                if now.duration_since(prev.at) <= MULTI_CLICK_INTERVAL
                    && f32::from(position.x - prev.position.x).abs() <= MULTI_CLICK_RADIUS
                    && f32::from(position.y - prev.position.y).abs() <= MULTI_CLICK_RADIUS =>
            {
                prev.count % 3 + 1
            }
            _ => 1,
        };
        self.click_chain = Some(ClickChain {
            at: now,
            position,
            count,
        });
        count
    }

    /// Take focus, place the caret, open a selection drag (US-009, US-010).
    /// Returns `true` when the press was consumed.
    fn on_text_down(
        &mut self,
        ev: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.scroll.bounds().contains(&ev.position) {
            return false;
        }
        window.focus(&self.focus, cx);
        self.end_typing_group();
        let Some(offset) = self.offset_at(ev.position) else {
            return false;
        };
        let count = self.chain_click(ev.position, Instant::now());
        let Some(doc) = self.state.document() else {
            return false;
        };
        let (grain, range) = match count {
            2 => (DragGrain::Word, cursor::word_range_at(doc, offset)),
            3 => (DragGrain::Line, cursor::line_range_at(doc, offset)),
            _ => (DragGrain::Grapheme, offset..offset),
        };
        let goal = cursor::goal_column(doc, range.end);
        self.goal_column = goal;
        self.selection = CodeSelection {
            anchor: range.start,
            head: range.end,
        };
        self.text_drag = Some(TextDrag {
            grain,
            anchor: range,
        });
        self.after_motion(cx);
        true
    }

    /// Extend the live selection, auto-scrolling when the pointer has left the
    /// viewport (US-010).
    ///
    /// The scroll step is applied per mouse-move event rather than on a timer:
    /// a drag that has left the viewport is a moving pointer by definition, and
    /// a timer would be a second source of truth for the scroll offset.
    fn on_text_move(&mut self, ev: &MouseMoveEvent, cx: &mut Context<Self>) {
        if self.text_drag.is_none() {
            return;
        }
        if ev.pressed_button != Some(MouseButton::Left) {
            // Same hitbox reasoning as the scrollbar drag: a release outside
            // the view never reaches us, so an unpressed move ends the drag.
            self.text_drag = None;
            cx.notify();
            return;
        }
        let scrolled = self.drag_autoscroll(ev.position);
        let Some(offset) = self.offset_at(ev.position) else {
            // A pointer outside the shaped rows still owes the scroll a frame.
            if scrolled {
                cx.notify();
            }
            return;
        };
        self.extend_drag_to(offset, cx);
    }

    /// Grow the live selection so it reaches `offset` at the drag's own
    /// granularity (US-010).
    ///
    /// A word or line drag always keeps the unit the opening press selected
    /// covered, and puts the head on whichever end the pointer is chasing, so
    /// dragging back over the anchor flips the direction instead of collapsing
    /// the selection.
    fn extend_drag_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        let Some(drag) = self.text_drag.clone() else {
            return;
        };
        let Some(doc) = self.state.document() else {
            return;
        };
        let reach = match drag.grain {
            DragGrain::Grapheme => offset..offset,
            DragGrain::Word => cursor::word_range_at(doc, offset),
            DragGrain::Line => cursor::line_range_at(doc, offset),
        };
        let selection = if reach.start < drag.anchor.start {
            CodeSelection {
                anchor: drag.anchor.end,
                head: reach.start,
            }
        } else {
            CodeSelection {
                anchor: drag.anchor.start,
                head: reach.end.max(drag.anchor.end),
            }
        };
        let goal = cursor::goal_column(doc, selection.cursor());
        self.selection = selection;
        self.goal_column = goal;
        self.last_motion = Instant::now();
        self.blink_visible = true;
        cx.notify();
    }

    /// Scroll toward a pointer that has left the viewport, on either axis
    /// (US-010). Returns `true` when an offset actually moved.
    ///
    /// Both axes matter: the editor scrolls horizontally too (US-008), so a
    /// selection dragged off the right edge of a long line has to follow the
    /// pointer the same way one dragged off the bottom does.
    fn drag_autoscroll(&mut self, position: Point<Pixels>) -> bool {
        let bounds = self.scroll.bounds();
        let geometry = self.geometry.get();
        let mut moved = false;

        let dy = autoscroll_step(
            f32::from(position.y),
            f32::from(bounds.origin.y),
            f32::from(bounds.bottom()),
            DRAG_SCROLL_ROWS * CODE_ROW_HEIGHT,
        );
        if dy != 0.0 {
            self.sync_scroll_line_count();
            moved = self.scroll.scroll_by_pixels(dy);
        }

        let dx = autoscroll_step(
            f32::from(position.x),
            f32::from(bounds.origin.x),
            f32::from(bounds.right()),
            DRAG_SCROLL_COLUMNS * geometry.char_w,
        );
        if dx != 0.0 {
            let next = (self.h_offset + dx).clamp(0.0, geometry.max_h_offset);
            if next != self.h_offset {
                self.h_offset = next;
                moved = true;
            }
        }

        moved
    }

    fn on_text_up(&mut self, _ev: &MouseUpEvent, cx: &mut Context<Self>) {
        if self.text_drag.take().is_some() {
            cx.notify();
        }
    }

    fn left(&mut self, _: &CeLeft, _w: &mut Window, cx: &mut Context<Self>) {
        self.horizontal(-1, false, cx);
    }

    fn right(&mut self, _: &CeRight, _w: &mut Window, cx: &mut Context<Self>) {
        self.horizontal(1, false, cx);
    }

    fn select_left(&mut self, _: &CeSelectLeft, _w: &mut Window, cx: &mut Context<Self>) {
        self.horizontal(-1, true, cx);
    }

    fn select_right(&mut self, _: &CeSelectRight, _w: &mut Window, cx: &mut Context<Self>) {
        self.horizontal(1, true, cx);
    }

    fn up(&mut self, _: &CeUp, _w: &mut Window, cx: &mut Context<Self>) {
        self.move_rows(-1, false, cx);
    }

    fn down(&mut self, _: &CeDown, _w: &mut Window, cx: &mut Context<Self>) {
        self.move_rows(1, false, cx);
    }

    fn select_up(&mut self, _: &CeSelectUp, _w: &mut Window, cx: &mut Context<Self>) {
        self.move_rows(-1, true, cx);
    }

    fn select_down(&mut self, _: &CeSelectDown, _w: &mut Window, cx: &mut Context<Self>) {
        self.move_rows(1, true, cx);
    }

    fn word_left(&mut self, _: &CeWordLeft, _w: &mut Window, cx: &mut Context<Self>) {
        self.word(-1, false, cx);
    }

    fn word_right(&mut self, _: &CeWordRight, _w: &mut Window, cx: &mut Context<Self>) {
        self.word(1, false, cx);
    }

    fn select_word_left(&mut self, _: &CeSelectWordLeft, _w: &mut Window, cx: &mut Context<Self>) {
        self.word(-1, true, cx);
    }

    fn select_word_right(
        &mut self,
        _: &CeSelectWordRight,
        _w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.word(1, true, cx);
    }

    fn home(&mut self, _: &CeHome, _w: &mut Window, cx: &mut Context<Self>) {
        self.line_edge(false, false, cx);
    }

    fn end(&mut self, _: &CeEnd, _w: &mut Window, cx: &mut Context<Self>) {
        self.line_edge(true, false, cx);
    }

    fn select_home(&mut self, _: &CeSelectHome, _w: &mut Window, cx: &mut Context<Self>) {
        self.line_edge(false, true, cx);
    }

    fn select_end(&mut self, _: &CeSelectEnd, _w: &mut Window, cx: &mut Context<Self>) {
        self.line_edge(true, true, cx);
    }

    fn page_up(&mut self, _: &CePageUp, _w: &mut Window, cx: &mut Context<Self>) {
        self.page(-1, false, cx);
    }

    fn page_down(&mut self, _: &CePageDown, _w: &mut Window, cx: &mut Context<Self>) {
        self.page(1, false, cx);
    }

    fn select_page_up(&mut self, _: &CeSelectPageUp, _w: &mut Window, cx: &mut Context<Self>) {
        self.page(-1, true, cx);
    }

    fn select_page_down(&mut self, _: &CeSelectPageDown, _w: &mut Window, cx: &mut Context<Self>) {
        self.page(1, true, cx);
    }

    fn doc_start(&mut self, _: &CeDocStart, _w: &mut Window, cx: &mut Context<Self>) {
        self.doc_edge(false, false, cx);
    }

    fn doc_end(&mut self, _: &CeDocEnd, _w: &mut Window, cx: &mut Context<Self>) {
        self.doc_edge(true, false, cx);
    }

    fn select_doc_start(&mut self, _: &CeSelectDocStart, _w: &mut Window, cx: &mut Context<Self>) {
        self.doc_edge(false, true, cx);
    }

    fn select_doc_end(&mut self, _: &CeSelectDocEnd, _w: &mut Window, cx: &mut Context<Self>) {
        self.doc_edge(true, true, cx);
    }

    fn select_all(&mut self, _: &CeSelectAll, _w: &mut Window, cx: &mut Context<Self>) {
        self.take_whole_document(cx);
    }

    fn horizontal(&mut self, direction: isize, extend: bool, cx: &mut Context<Self>) {
        // A bare arrow on a live selection collapses onto its edge rather than
        // stepping off the head, which is what every editor does.
        let from = match (extend, self.selection.is_empty(), direction < 0) {
            (false, false, true) => self.selection.range().start,
            (false, false, false) => self.selection.range().end,
            _ => self.selection.cursor(),
        };
        let collapsing = !extend && !self.selection.is_empty();
        let Some(doc) = self.state.document() else {
            return;
        };
        let offset = if collapsing {
            from
        } else if direction < 0 {
            cursor::grapheme_left(doc, from)
        } else {
            cursor::grapheme_right(doc, from)
        };
        self.place_caret(offset, extend, cx);
    }

    fn word(&mut self, direction: isize, extend: bool, cx: &mut Context<Self>) {
        let from = self.selection.cursor();
        let Some(doc) = self.state.document() else {
            return;
        };
        let offset = if direction < 0 {
            cursor::word_left(doc, from)
        } else {
            cursor::word_right(doc, from)
        };
        self.place_caret(offset, extend, cx);
    }

    fn line_edge(&mut self, end: bool, extend: bool, cx: &mut Context<Self>) {
        let from = self.selection.cursor();
        let Some(doc) = self.state.document() else {
            return;
        };
        let offset = if end {
            cursor::line_end(doc, from)
        } else {
            cursor::line_home(doc, from)
        };
        self.place_caret(offset, extend, cx);
    }

    fn page(&mut self, direction: isize, extend: bool, cx: &mut Context<Self>) {
        let rows = self.page_rows() as isize;
        self.move_rows(direction * rows, extend, cx);
    }

    fn doc_edge(&mut self, end: bool, extend: bool, cx: &mut Context<Self>) {
        let Some(doc) = self.state.document() else {
            return;
        };
        let offset = if end { cursor::doc_end(doc) } else { 0 };
        self.place_caret(offset, extend, cx);
    }

    /// Select All (US-010). Anchored at the start so a following Shift+arrow
    /// shrinks from the end, the way a dragged selection would.
    fn take_whole_document(&mut self, cx: &mut Context<Self>) {
        self.end_typing_group();
        let Some(doc) = self.state.document() else {
            return;
        };
        let end = cursor::doc_end(doc);
        let goal = cursor::goal_column(doc, end);
        self.selection = CodeSelection {
            anchor: 0,
            head: end,
        };
        self.goal_column = goal;
        self.after_motion(cx);
    }

    // ----------------------------------------------------------------- EP-004

    /// Close the open undo group and drop any IME mark.
    ///
    /// Every deliberate caret move calls this: it is what stops a keystroke
    /// after a click or an arrow from being folded into the transaction that
    /// preceded it (US-013), and what guarantees a composition interrupted by a
    /// click is committed rather than left pending (US-012).
    fn end_typing_group(&mut self) {
        self.history.close_group();
        self.marked = None;
    }

    /// Whether the document differs from what is on disk (US-015).
    ///
    /// Compared by transaction identity, not by a counter: undoing back to the
    /// saved state has to clear the dot, and a counter can only ever grow.
    pub(crate) fn is_dirty(&self) -> bool {
        self.history.mark() != self.saved_mark
    }

    /// Whether a conflict banner is showing, i.e. the user still owes the file
    /// a decision (US-016).
    #[allow(dead_code)] // EP-004 accessor: the conflict banner is rendered from the state enum inside the view.
    pub(crate) fn has_conflict(&self) -> bool {
        self.disk == DiskState::Conflict
    }

    /// The one door into the rope.
    ///
    /// `ops` are applied in the order given, so a caller touching several
    /// places must order them back to front for its own offsets to stay valid.
    /// Every `CodeEdit` the splices produce is handed to the highlighter, which
    /// is what keeps the reparse incremental, and the whole batch lands as one
    /// undo transaction. A batch that descends by row (a reload's hunks, an
    /// indent) reaches the highlighter as **one** call (EP-009): one
    /// generation, one interpolation, at most one deferred parse.
    ///
    /// Returns `false` when nothing changed - a read-only document (refused
    /// visibly), or a batch that turned out to be a no-op.
    fn splice_all(
        &mut self,
        ops: &[(Range<usize>, String)],
        after: CodeSelection,
        group: EditGroup,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.state.document().is_none_or(CodeDocument::is_read_only) {
            self.flash_read_only(cx);
            return false;
        }
        let before = self.selection;
        let now = Instant::now();
        // Decided against the document the ops were computed for, before any
        // of them lands.
        let batched = self
            .state
            .document()
            .is_some_and(|doc| ops_descend_by_row(doc, ops));
        let mut records = Vec::with_capacity(ops.len());
        let mut edits = Vec::with_capacity(ops.len());
        let mut deferred: Option<DeferredParse> = None;
        let mut changes = Vec::with_capacity(ops.len());
        // The highlighter and the document come out of one borrow, and
        // `spawn_deferred_parse` needs `&mut self`, so the deferred parse is
        // collected here and started once the borrow is over.
        if let Some((doc, hl)) = self.state.editable() {
            for (range, text) in ops {
                let Some(applied) = edit::splice(doc, range.clone(), text) else {
                    continue;
                };
                if batched {
                    edits.push(applied.edit.clone());
                } else if let HighlightOutcome::Deferred(parse) = hl.edit(doc, &applied.edit) {
                    deferred = Some(parse);
                }
                changes.push(DocChange {
                    edit: applied.edit,
                    window: applied.window,
                });
                records.push(applied.record);
            }
            if batched
                && let Ok(HighlightOutcome::Deferred(parse)) =
                    hl.edit_batch(doc, &edits, SYNC_PARSE_BUDGET)
            {
                deferred = Some(parse);
            }
        }
        if records.is_empty() {
            return false;
        }
        self.history.push(records, before, after, group, now);
        self.note_changes(&changes, cx);
        self.finish_edit(after, deferred, cx);
        true
    }

    /// Land a mutation: clamp the caret to the new text, refresh the goal
    /// column, start whatever reparse was deferred, repaint.
    fn finish_edit(
        &mut self,
        after: CodeSelection,
        deferred: Option<DeferredParse>,
        cx: &mut Context<Self>,
    ) {
        if let Some(doc) = self.state.document() {
            self.selection = CodeSelection {
                anchor: cursor::clamp(doc, after.anchor),
                head: cursor::clamp(doc, after.head),
            };
            self.goal_column = cursor::goal_column(doc, self.selection.cursor());
        }
        if let Some(parse) = deferred {
            spawn_deferred_parse(parse, cx, |view: &mut Self, parsed, cx| {
                if let Some((doc, hl)) = view.state.editable()
                    && hl.apply_parsed(doc, parsed)
                {
                    cx.notify();
                }
            });
        }
        self.refresh_longest_line(cx);
        self.after_motion(cx);
    }

    /// Hand a stale longest-line maximum to a background rescan, so the
    /// horizontal extent shrinks after the widest line is cut without an
    /// O(lines) walk on the render thread. Guarded twice on the way back: by
    /// the load generation, so a tab that moved on to another file drops the
    /// result, and by the document revision inside
    /// `apply_longest_line_measurement`, so an edit that landed during the
    /// scan wins and the next keystroke's snapshot carries it.
    fn refresh_longest_line(&mut self, cx: &mut Context<Self>) {
        if self.longest_line_scan_in_flight {
            self.longest_line_rescan_needed = true;
            return;
        }
        let Some((text, revision)) = self
            .state
            .document()
            .and_then(CodeDocument::longest_line_snapshot)
        else {
            return;
        };
        self.longest_line_scan_in_flight = true;
        self.longest_line_rescan_needed = false;
        let load_generation = self.slot.current();
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let longest = cx
                .background_spawn(async move { CodeDocument::measure_longest_line(&text) })
                .await;
            cx.update(|cx| {
                let _ = this.update(cx, |view: &mut Self, cx: &mut Context<Self>| {
                    if !view.slot.accept(load_generation) {
                        view.longest_line_scan_in_flight = false;
                        view.longest_line_rescan_needed = false;
                        return;
                    }
                    if let Some(doc) = view.state.document_mut()
                        && doc.apply_longest_line_measurement(revision, longest)
                    {
                        cx.notify();
                    }
                    view.longest_line_scan_in_flight = false;
                    if view.longest_line_rescan_needed {
                        view.refresh_longest_line(cx);
                    }
                });
            });
        })
        .detach();
    }

    /// Light the read-only banner up for [`READ_ONLY_FLASH`] (US-012).
    ///
    /// The refusal has to be *seen*: `accepts_text_input` deliberately stays at
    /// its permissive default so the keystroke still reaches
    /// [`Self::replace_text_in_range`], where it can be turned down loudly
    /// rather than swallowed by the platform.
    fn flash_read_only(&mut self, cx: &mut Context<Self>) {
        self.read_only_flash = Some(Instant::now());
        cx.notify();
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            cx.background_executor().timer(READ_ONLY_FLASH).await;
            cx.update(|cx| {
                let _ = this.update(cx, |view: &mut Self, cx: &mut Context<Self>| {
                    if view
                        .read_only_flash
                        .is_some_and(|at| at.elapsed() >= READ_ONLY_FLASH)
                    {
                        view.read_only_flash = None;
                        cx.notify();
                    }
                });
            });
        })
        .detach();
    }

    /// Replace the selection (or the caret slot) with `text`.
    ///
    /// The text is normalized here as well as inside [`edit::splice`], because
    /// the caret has to land past what the rope really received: a pasted
    /// `\r\n` is one byte shorter once it is in.
    fn insert_text(&mut self, text: &str, group: EditGroup, cx: &mut Context<Self>) -> bool {
        let range = self.replacement_range();
        let inserted = normalize_newlines(text).into_owned();
        let caret = CodeSelection::at(range.start + inserted.len());
        self.splice_all(&[(range, inserted)], caret, group, cx)
    }

    /// Range the next insertion replaces: the live composition if there is one,
    /// otherwise the selection.
    fn replacement_range(&self) -> Range<usize> {
        match &self.marked {
            Some(marked) => marked.clone(),
            None => self.selection.range(),
        }
    }

    /// Turn the platform's optional UTF-16 range into document bytes.
    ///
    /// `None` means "wherever the editor thinks it is", which is the marked
    /// range during a composition and the selection otherwise - the same
    /// resolution `widgets/text_area.rs` performs.
    fn resolve_replacement(&self, range_utf16: Option<Range<usize>>) -> Option<Range<usize>> {
        let doc = self.state.document()?;
        Some(match range_utf16 {
            Some(range) => {
                let start = doc.utf16_to_byte(range.start);
                start..doc.utf16_to_byte(range.end).max(start)
            }
            None => self.replacement_range(),
        })
    }

    /// Backspace and Delete (US-012). A live selection is what gets removed;
    /// otherwise one whole grapheme goes, which is why a composed emoji
    /// disappears in one press instead of losing a modifier at a time.
    fn delete_grapheme(&mut self, forward: bool, cx: &mut Context<Self>) {
        let selection = self.selection.range();
        let range = if !selection.is_empty() {
            selection
        } else {
            let Some(doc) = self.state.document() else {
                return;
            };
            let at = self.selection.cursor();
            if forward {
                at..cursor::grapheme_right(doc, at)
            } else {
                cursor::grapheme_left(doc, at)..at
            }
        };
        if range.is_empty() {
            return;
        }
        let caret = CodeSelection::at(range.start);
        self.splice_all(&[(range, String::new())], caret, EditGroup::Typing, cx);
    }

    /// Enter (US-012): a newline plus whatever indentation the row already had,
    /// truncated at the caret so splitting a line mid-indent cannot invent
    /// leading whitespace that was never typed.
    fn insert_newline(&mut self, cx: &mut Context<Self>) {
        let mut text = String::from("\n");
        if let Some(doc) = self.state.document() {
            let at = self.selection.range().start;
            let row = doc.byte_to_line(at);
            let start = doc.line_to_byte(row);
            if let Some(line) = doc.line_string(row) {
                let indent = edit::leading_indent(&line);
                let column = at.saturating_sub(start);
                text.push_str(&indent[..indent.len().min(column)]);
            }
        }
        self.insert_text(&text, EditGroup::Atomic, cx);
    }

    /// Rows the current selection touches, as an inclusive row range.
    fn selected_rows(&self) -> Option<(usize, usize)> {
        let doc = self.state.document()?;
        let range = self.selection.range();
        let first = doc.byte_to_line(range.start);
        // A selection ending exactly at a row start stops on the row before:
        // Tab on a full-line selection must not indent the row after it.
        let last_byte = if range.end > range.start {
            range.end - 1
        } else {
            range.end
        };
        Some((first, doc.byte_to_line(last_byte).max(first)))
    }

    /// Tab and Shift+Tab (US-014).
    ///
    /// A bare Tab with no selection inserts one unit at the caret; anything
    /// else shifts every touched row. The rows are rewritten back to front so
    /// each splice's offsets are still valid when it runs, and the caret and
    /// anchor are carried across with [`shift_offset`] rather than re-derived,
    /// so a selection survives the operation intact.
    fn shift_lines(&mut self, outdent: bool, cx: &mut Context<Self>) {
        let Some((first, last)) = self.selected_rows() else {
            return;
        };
        if !outdent && self.selection.is_empty() {
            let unit = self.indent.as_str().into_owned();
            self.insert_text(&unit, EditGroup::Atomic, cx);
            return;
        }
        let unit = self.indent;
        let mut ops: Vec<(Range<usize>, String)> = Vec::new();
        let mut deltas: Vec<(usize, isize)> = Vec::new();
        {
            let Some(doc) = self.state.document() else {
                return;
            };
            for row in (first..=last).rev() {
                let start = doc.line_to_byte(row);
                let Some(line) = doc.line_string(row) else {
                    continue;
                };
                if outdent {
                    let width = edit::dedent_width(&line, unit);
                    if width == 0 {
                        continue;
                    }
                    ops.push((start..start + width, String::new()));
                    deltas.push((start, -(width as isize)));
                } else {
                    // A blank row gains nothing: indenting whitespace-only
                    // lines is churn the diff would show and the user did not
                    // ask for.
                    if line.trim_end_matches('\n').is_empty() {
                        continue;
                    }
                    let text = unit.as_str().into_owned();
                    let width = text.len() as isize;
                    ops.push((start..start, text));
                    deltas.push((start, width));
                }
            }
        }
        if ops.is_empty() {
            return;
        }
        let after = CodeSelection {
            anchor: shift_offset(self.selection.anchor, &deltas),
            head: shift_offset(self.selection.head, &deltas),
        };
        self.splice_all(&ops, after, EditGroup::Atomic, cx);
    }

    /// The text Copy and Cut act on, and the range Cut removes.
    ///
    /// With no selection that is the whole row, newline included (US-014), so
    /// pasting it back lands a complete line rather than gluing it onto the
    /// current one.
    fn clip_range(&self) -> Option<Range<usize>> {
        let doc = self.state.document()?;
        let selection = self.selection.range();
        if selection.is_empty() {
            Some(cursor::line_range_at(doc, selection.start))
        } else {
            Some(selection)
        }
    }

    fn copy_selection(&mut self, cut: bool, cx: &mut Context<Self>) {
        let Some(range) = self.clip_range() else {
            return;
        };
        if range.is_empty() {
            return;
        }
        let Some(doc) = self.state.document() else {
            return;
        };
        let text = doc.slice_string(range.clone());
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        if cut {
            let caret = CodeSelection::at(range.start);
            self.splice_all(&[(range, String::new())], caret, EditGroup::Atomic, cx);
        }
    }

    /// Paste (US-014). One transaction whatever the clipboard holds, so a
    /// multi-line paste is a single Ctrl+Z, and the text goes through
    /// [`edit::sanitize_paste`] first: control characters and bidi overrides
    /// from a web page must not end up in a source file.
    fn paste(&mut self, cx: &mut Context<Self>) {
        let Some(item) = cx.read_from_clipboard() else {
            return;
        };
        let Some(text) = item.text() else {
            return;
        };
        let text = edit::sanitize_paste(&text);
        if text.is_empty() {
            return;
        }
        self.end_typing_group();
        self.insert_text(&text, EditGroup::Atomic, cx);
    }

    /// Undo / redo (US-013). The document and the highlighter move together:
    /// every edit the replay produces is fed to `hl.edit`, so the tree stays in
    /// step with the rope in both directions.
    fn time_travel(&mut self, redo: bool, cx: &mut Context<Self>) {
        // A read-only document can still hold history: a silent reload
        // (US-016) records its transaction with the flag lifted for the
        // duration of the splice. Replaying that here would move the caret and
        // the history mark while `CodeDocument` refuses the rope mutation,
        // leaving a file that matches disk exactly looking modified. The
        // refusal is the same one a keystroke gets (US-012).
        if self.state.document().is_none_or(CodeDocument::is_read_only) {
            self.flash_read_only(cx);
            return;
        }
        self.marked = None;
        let mut deferred: Option<DeferredParse> = None;
        let mut restored = None;
        let mut changes = Vec::new();
        if let Some((doc, hl)) = self.state.editable() {
            let step = if redo {
                self.history.redo(doc)
            } else {
                self.history.undo(doc)
            };
            if let Some(step) = step {
                for change in &step.edits {
                    if let HighlightOutcome::Deferred(parse) = hl.edit(doc, change) {
                        deferred = Some(parse);
                    }
                }
                changes = step
                    .windows
                    .into_iter()
                    .map(|window| DocChange {
                        edit: CodeEdit {
                            start_byte: 0,
                            old_end_byte: 0,
                            new_end_byte: 0,
                            start_point: Default::default(),
                            old_end_point: Default::default(),
                            new_end_point: Default::default(),
                        },
                        window,
                    })
                    .collect();
                restored = Some(step.selection);
            }
        }
        let Some(selection) = restored else {
            return;
        };
        self.close_marker_popup(cx);
        self.note_changes(&changes, cx);
        self.finish_edit(selection, deferred, cx);
    }

    // ------------------------------------------------------------ disk (EP-004)

    /// Ctrl+S (US-015). No autosave anywhere in this file.
    ///
    /// The stamp check happens on the worker thread, immediately before the
    /// write, so a file an agent touched between the last watcher tick and this
    /// keystroke is still caught. A conflict is reported *without writing*.
    fn save(&mut self, cx: &mut Context<Self>) {
        if self.saving || self.disk == DiskState::Conflict {
            return;
        }
        let Some(doc) = self.state.document() else {
            return;
        };
        if doc.is_read_only() {
            self.flash_read_only(cx);
            return;
        }
        if !self.is_dirty() && self.disk == DiskState::InSync {
            return;
        }
        // Closing the group first is what makes the saved mark stable: a
        // keystroke after the save must open a new transaction, or typing would
        // silently extend the one the save just blessed.
        self.history.close_group();
        let contents = doc.to_disk_string();
        let path = self.path.clone();
        let expected = self.stamp;
        let mark = self.history.mark();
        self.disk_generation = self.disk_generation.wrapping_add(1);
        self.saving = true;
        self.save_error = None;
        cx.notify();
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let outcome = cx
                .background_spawn(async move {
                    // The stamp comparison lives in `save_blocking`, which
                    // checks it before the temp write and again right before
                    // the rename (issue #402).
                    save::save_blocking(&path, &contents, expected).map_err(|err| match err {
                        save::SaveError::Conflict => None,
                        save::SaveError::Write(message) => Some(message),
                    })
                })
                .await;
            cx.update(|cx| {
                let _ = this.update(cx, |view: &mut Self, cx: &mut Context<Self>| {
                    view.finish_save(outcome, mark, cx);
                });
            });
        })
        .detach();
    }

    /// Land a save's result. `Err(None)` is the refused-before-writing case.
    fn finish_save(
        &mut self,
        outcome: Result<FileStamp, Option<String>>,
        mark: edit::HistoryMark,
        cx: &mut Context<Self>,
    ) {
        self.saving = false;
        match outcome {
            Ok(stamp) => {
                self.stamp = Some(stamp);
                self.saved_mark = mark;
                self.disk = DiskState::InSync;
                self.save_error = None;
            }
            Err(Some(message)) => {
                // The in-memory edits are untouched: a failed write must never
                // be able to cost the user their work (US-015).
                self.save_error = Some(message);
            }
            Err(None) => {
                self.disk = DiskState::Conflict;
            }
        }
        self.recheck_disk(cx);
        cx.notify();
    }

    /// Re-stat after a save so events ignored while the write was in flight do
    /// not hide an external change that landed immediately afterward.
    fn recheck_disk(&mut self, cx: &mut Context<Self>) {
        let path = self.path.clone();
        let generation = self.begin_disk_probe();
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let probe = path.clone();
            let loaded = cx
                .background_spawn(async move { super::load::load_stamped(&probe) })
                .await;
            reload_from_disk(&this, cx, generation, loaded, false).await;
        })
        .detach();
    }

    fn begin_disk_probe(&mut self) -> u64 {
        self.disk_generation = self.disk_generation.wrapping_add(1);
        self.disk_generation
    }

    /// Watch the file's parent directory for someone else's write (US-016).
    ///
    /// The parent rather than the file: an atomic save renames a sibling over
    /// the target, which arrives as a directory event and would never reach a
    /// watch registered on the old inode. Non-recursive, so a deep tree costs
    /// one watch descriptor - the inotify-exhaustion lesson from
    /// `reference_gpui_recursive_watcher_main_thread_hang`.
    ///
    /// Registration (a stat of the parent plus the OS watch call) runs on the
    /// background executor and is adopted under the load generation, so ten
    /// files opened in quick succession leave exactly one watcher behind -
    /// the newest one's - and the others are dropped, unregistered, as they
    /// arrive.
    fn start_watcher(&mut self, cx: &mut Context<Self>) {
        self._watcher = None;
        self._watch_bridge = None;
        let Some(parent) = self.path.parent().map(Path::to_path_buf) else {
            return;
        };
        let Some(name) = self.path.file_name().map(|name| name.to_os_string()) else {
            return;
        };
        let generation = self.slot.current();
        let path = self.path.clone();
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let watched_parent = parent.clone();
            let probe = path.clone();
            let outcome = cx
                .background_spawn(async move {
                    let watched = create_file_watcher(parent);
                    let stamp = FileStamp::read(&probe);
                    (watched, stamp)
                })
                .await;
            let (watcher, bridge, rx, disk_stamp) = match outcome {
                (Ok((watcher, bridge, rx)), stamp) => (watcher, bridge, rx, stamp),
                (Err(err), _) => {
                    log::warn!(
                        "could not watch {} for changes: {err}",
                        watched_parent.display()
                    );
                    return;
                }
            };
            cx.update(|cx| {
                let _ = this.update(cx, |view: &mut Self, cx: &mut Context<Self>| {
                    if !view.slot.accept(generation) {
                        return;
                    }
                    view._watcher = Some(watcher);
                    view._watch_bridge = Some(bridge);
                    view.spawn_reload_loop(path, name, rx, cx);
                    // Events between load and this registration never reach
                    // the loop. One re-stat after the watch is live closes
                    // that window; a later event still folds in through it.
                    if view.stamp != disk_stamp {
                        view.recheck_disk(cx);
                    }
                });
            });
        })
        .detach();
    }

    /// The reload task behind a registered watcher: debounce a burst of
    /// directory events, then re-read the file off-thread and fold it in
    /// through `disk_loaded`.
    fn spawn_reload_loop(
        &mut self,
        path: PathBuf,
        name: std::ffi::OsString,
        mut rx: WatchEvents,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            while let Some(first) = rx.next().await {
                if !event_is_relevant(&first, &name) {
                    continue;
                }
                // One save is several events. Wait for the burst to go quiet
                // rather than re-reading the file three times.
                let deadline = Instant::now() + RELOAD_DEBOUNCE;
                loop {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        break;
                    }
                    let timer = cx.background_executor().timer(remaining);
                    match futures::future::select(rx.next(), timer).await {
                        Either::Left((Some(_), _)) => continue,
                        Either::Left((None, _)) => return,
                        Either::Right(_) => break,
                    }
                }
                let generation = cx.update(|cx| {
                    this.update(cx, |view: &mut Self, _| {
                        (view.path == path).then(|| view.begin_disk_probe())
                    })
                    .unwrap_or(None)
                });
                let Some(generation) = generation else {
                    break;
                };
                let probe = path.clone();
                let loaded = cx
                    .background_spawn(async move { super::load::load_stamped(&probe) })
                    .await;
                // A closed tab is the loop's exit condition, not an error.
                if !reload_from_disk(&this, cx, generation, loaded, false).await {
                    break;
                }
            }
        })
        .detach();
    }

    /// The main-thread half of a reload that runs before the diff: the
    /// ordering guards, the conflict decision, and the rope snapshot the
    /// background diff runs against. The incoming stamp is stashed on
    /// `pending_stamp`, not adopted: a Cmd+S in this window must still carry
    /// the pre-agent stamp so [`save::save_blocking`] refuses rather than
    /// overwriting the rewrite (#402, #428). `None` means there is nothing
    /// to diff - the probe was stale, the file is gone, the bytes are the
    /// ones already adopted, or the document is dirty and now conflicted.
    fn begin_disk_reload(
        &mut self,
        generation: u64,
        loaded: DiskLoad,
        force: bool,
        cx: &mut Context<Self>,
    ) -> Option<(DiskDiff, CodeDocument)> {
        if self.saving || generation != self.disk_generation {
            return None;
        }
        let (document, stamp) = match loaded {
            Ok(loaded) => loaded,
            Err(error) => {
                self.disk = if error == CodeLoadError::NotFound {
                    DiskState::Deleted
                } else {
                    DiskState::Conflict
                };
                self.save_error = Some(error.message());
                cx.notify();
                return None;
            }
        };
        if !force {
            if self.stamp == stamp && self.disk == DiskState::InSync {
                return None;
            }
            if self.is_dirty() || self.disk == DiskState::Conflict {
                self.disk = DiskState::Conflict;
                cx.notify();
                return None;
            }
        }
        let doc = self.state.document()?;
        self.pending_stamp = stamp;
        Some((DiskDiff::of(doc), document))
    }

    /// The main-thread half after the diff. `Some` hands back a fresh
    /// snapshot to diff again: the document's revision moved while the diff
    /// ran and `retry` still allows one more attempt. Otherwise the splices
    /// land as one transaction and the incoming stamp is adopted - or the
    /// document is left to the user as a conflict, when it moved once too
    /// often or is dirty by now and the reload was not forced. A save that
    /// ran in the window still holds the pre-agent stamp.
    fn finish_disk_reload(
        &mut self,
        generation: u64,
        batch: DiskSplices,
        incoming: &CodeDocument,
        retry: bool,
        force: bool,
        cx: &mut Context<Self>,
    ) -> Option<DiskDiff> {
        if self.saving || generation != self.disk_generation {
            return None;
        }
        let DiskSplices { revision, splices } = batch;
        let doc = self.state.document()?;
        if doc.revision() != revision {
            if retry {
                return Some(DiskDiff::of(doc));
            }
            self.disk = DiskState::Conflict;
            cx.notify();
            return None;
        }
        if !force && self.is_dirty() {
            self.disk = DiskState::Conflict;
            cx.notify();
            return None;
        }
        self.apply_disk_splices(
            &splices,
            incoming.line_ending(),
            incoming.read_only_reason(),
            cx,
        );
        self.stamp = self.pending_stamp;
        self.disk = DiskState::InSync;
        self.save_error = None;
        self.saved_mark = self.history.mark();
        cx.notify();
        None
    }

    /// Land a reload's hunks, keeping the viewport where the user left it and
    /// carrying the caret across the hunks.
    ///
    /// Applied as a normal transaction rather than a reload, so Ctrl+Z brings
    /// the previous state back - the recovery US-016 asks for when the reload
    /// was not what the user wanted. The disk's own rules are re-applied
    /// afterwards whatever the hunks were: `read_only` is what the loader
    /// decided for the bytes now on disk (the giant-line guard, the mode
    /// bits), and `line_ending` is the terminator those bytes used, which the
    /// diff normalized away - without it a rewrite that only changed LF to
    /// CRLF would be reverted on the next save (#271).
    fn apply_disk_splices(
        &mut self,
        ops: &[(Range<usize>, String)],
        line_ending: LineEnding,
        read_only: Option<ReadOnlyReason>,
        cx: &mut Context<Self>,
    ) {
        let Some(doc) = self.state.document() else {
            return;
        };
        let scroll_rows = self.scroll.rows();
        let after = edit::shift_selection_for_splices(self.selection, ops);
        // `edit::splice` refuses a read-only document, and a file can be
        // read-only on disk and still change underneath us. The flag is lifted
        // for the duration of the reload and put straight back.
        let reason = doc.read_only_reason();
        let replaced = if ops.is_empty() {
            false
        } else {
            if reason.is_some()
                && let Some(doc) = self.state.document_mut()
            {
                doc.set_read_only(None);
            }
            self.splice_all(ops, after, EditGroup::Atomic, cx)
        };
        if let Some(doc) = self.state.document_mut() {
            doc.set_read_only(read_only);
            doc.set_line_ending(line_ending);
        }
        if !replaced {
            return;
        }
        self.popup = None;
        self.hovered_marker = None;
        self.reset_tracker(cx);
        // A symlink may now point at a different tracked file. Regular-file
        // reloads must not spawn `start_base_load`: tests drain the document
        // reload, and an extra `smol::unblock` wakes the GPUI local task from
        // a blocking thread (`a_forced_reload_still_overwrites_the_document_the_user_edited`).
        if std::fs::symlink_metadata(&self.path).is_ok_and(|meta| meta.file_type().is_symlink()) {
            self.start_base_load(cx);
        }
        self.sync_scroll_line_count();
        self.scroll.set_rows(scroll_rows);
        cx.notify();
    }

    /// "Keep mine" (US-016): the in-memory text wins, and the on-disk stamp is
    /// adopted so the next Ctrl+S goes through instead of being refused again.
    fn resolve_keep_mine(&mut self, cx: &mut Context<Self>) {
        let generation = self.begin_disk_probe();
        let path = self.path.clone();
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let stamp = cx
                .background_spawn(async move { FileStamp::read(&path) })
                .await;
            cx.update(|cx| {
                let _ = this.update(cx, |view: &mut Self, cx: &mut Context<Self>| {
                    if view.saving || generation != view.disk_generation {
                        return;
                    }
                    view.stamp = stamp;
                    view.disk = DiskState::InSync;
                    view.save_error = None;
                    cx.notify();
                });
            });
        })
        .detach();
        cx.notify();
    }

    /// "Reload from disk" (US-016). Re-reads rather than trusting a snapshot
    /// taken when the banner appeared, which may already be stale.
    fn resolve_reload(&mut self, cx: &mut Context<Self>) {
        let generation = self.begin_disk_probe();
        let mark = self.history.mark();
        let path = self.path.clone();
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let probe = path.clone();
            let loaded = cx
                .background_spawn(async move { super::load::load_stamped(&probe) })
                .await;
            // Do not discard edits typed while the read was in flight. Edits
            // typed while the diff runs are caught by the revision check.
            let moved_on = cx.update(|cx| {
                this.update(cx, |view: &mut Self, _| view.history.mark() != mark)
                    .unwrap_or(true)
            });
            if moved_on {
                return;
            }
            reload_from_disk(&this, cx, generation, loaded, true).await;
        })
        .detach();
    }

    // ----------------------------------------------------- EP-004 action glue

    fn backspace(&mut self, _: &CeBackspace, _w: &mut Window, cx: &mut Context<Self>) {
        self.delete_grapheme(false, cx);
    }

    fn delete(&mut self, _: &CeDelete, _w: &mut Window, cx: &mut Context<Self>) {
        self.delete_grapheme(true, cx);
    }

    fn newline(&mut self, _: &CeNewline, _w: &mut Window, cx: &mut Context<Self>) {
        self.insert_newline(cx);
    }

    fn undo(&mut self, _: &CeUndo, _w: &mut Window, cx: &mut Context<Self>) {
        self.time_travel(false, cx);
    }

    fn redo(&mut self, _: &CeRedo, _w: &mut Window, cx: &mut Context<Self>) {
        self.time_travel(true, cx);
    }

    fn copy(&mut self, _: &CeCopy, _w: &mut Window, cx: &mut Context<Self>) {
        self.copy_selection(false, cx);
    }

    fn cut(&mut self, _: &CeCut, _w: &mut Window, cx: &mut Context<Self>) {
        self.copy_selection(true, cx);
    }

    fn paste_action(&mut self, _: &CePaste, _w: &mut Window, cx: &mut Context<Self>) {
        self.paste(cx);
    }

    fn indent(&mut self, _: &CeIndent, _w: &mut Window, cx: &mut Context<Self>) {
        self.shift_lines(false, cx);
    }

    fn outdent(&mut self, _: &CeOutdent, _w: &mut Window, cx: &mut Context<Self>) {
        self.shift_lines(true, cx);
    }

    fn save_action(&mut self, _: &CeSave, _w: &mut Window, cx: &mut Context<Self>) {
        self.save(cx);
    }

    fn escape(&mut self, _: &CeEscape, _w: &mut Window, cx: &mut Context<Self>) {
        self.close_marker_popup(cx);
    }

    /// The banners stacked above the file: read-only, conflict, deletion, and
    /// the last failed write.
    fn banners(&self, ui: crate::theme::UiColors, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut out: Vec<AnyElement> = Vec::new();
        let row = || {
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .px_3()
                .py_1p5()
                .text_xs()
                .border_b_1()
                .border_color(ui.border)
        };
        if let Some(reason) = self
            .state
            .document()
            .and_then(CodeDocument::read_only_reason)
        {
            let flashing = self.read_only_flash.is_some();
            out.push(
                row()
                    .bg(if flashing {
                        ui.vc_conflict.opacity(0.22)
                    } else {
                        ui.overlay
                    })
                    .text_color(if flashing { ui.text } else { ui.muted })
                    .child(read_only_text(reason))
                    .into_any_element(),
            );
        }
        match self.disk {
            DiskState::Conflict => out.push(
                row()
                    .bg(ui.vc_conflict.opacity(0.16))
                    .text_color(ui.text)
                    .child(div().flex_1().child(
                        "This file changed on disk while you were editing it. Nothing has been \
                         overwritten.",
                    ))
                    .child(conflict_button(
                        "code-conflict-keep",
                        "Keep mine",
                        ui,
                        cx.listener(|this, _: &ClickEvent, _w, cx| this.resolve_keep_mine(cx)),
                    ))
                    .child(conflict_button(
                        "code-conflict-reload",
                        "Reload from disk",
                        ui,
                        cx.listener(|this, _: &ClickEvent, _w, cx| this.resolve_reload(cx)),
                    ))
                    .into_any_element(),
            ),
            DiskState::Deleted => out.push(
                row()
                    .bg(ui.vc_conflict.opacity(0.16))
                    .text_color(ui.text)
                    .child("This file was deleted on disk. Saving recreates it.")
                    .into_any_element(),
            ),
            DiskState::InSync => {}
        }
        if let Some(message) = &self.save_error {
            out.push(
                row()
                    .bg(ui.vc_deleted.opacity(0.16))
                    .text_color(ui.text)
                    .child(format!("{message} Your edits are still here."))
                    .into_any_element(),
            );
        }
        out
    }
}

/// Carry `offset` across a batch of line-start insertions and removals.
///
/// The batch is what Tab and Shift+Tab produce: one delta per touched row, at
/// that row's first byte. An insertion pushes everything at or after it along;
/// a removal only takes back what actually sat between the row start and the
/// offset, which is what keeps a caret parked inside the indentation from
/// jumping into the previous line.
fn shift_offset(offset: usize, deltas: &[(usize, isize)]) -> usize {
    let mut out = offset as isize;
    for (start, delta) in deltas {
        if *delta > 0 {
            if *start <= offset {
                out += delta;
            }
        } else if *start < offset {
            let removed = delta.unsigned_abs();
            out -= removed.min(offset - start) as isize;
        }
    }
    out.max(0) as usize
}

/// Register a non-recursive watch on `parent`. Blocking (a stat plus the OS
/// registration), so it runs on the background executor. The sender side of
/// the bridge is handed back to the view, which is what lets the callback be
/// severed from the owning thread before the watcher goes away.
fn create_file_watcher(
    parent: PathBuf,
) -> Result<(ConflictWatcher, WatchBridge, WatchEvents), String> {
    if !parent.is_dir() {
        return Err("the parent directory no longer exists".to_string());
    }
    // Unbounded on purpose: events fired between registration and the first
    // poll of the reload loop have to queue, not be dropped.
    let (tx, rx) = mpsc::unbounded::<notify::Result<notify::Event>>();
    let bridge: WatchBridge = Arc::new(Mutex::new(Some(tx)));
    let notify_side = Arc::clone(&bridge);
    let mut watcher = ConflictWatcher::new(
        move |result| {
            if let Ok(guard) = notify_side.lock()
                && let Some(tx) = guard.as_ref()
            {
                let _ = tx.unbounded_send(result);
            }
        },
        notify::Config::default(),
    )
    .map_err(|err| err.to_string())?;
    watcher
        .watch(&parent, RecursiveMode::NonRecursive)
        .map_err(|err| err.to_string())?;
    Ok((watcher, bridge, rx))
}

/// Whether a filesystem event concerns the open file.
///
/// An `Err` is not a change: a watcher that lost an event should not be able to
/// present the user with a conflict that never happened.
fn event_is_relevant(result: &notify::Result<notify::Event>, target: &std::ffi::OsStr) -> bool {
    match result {
        Ok(event) => event
            .paths
            .iter()
            .any(|path| path.file_name() == Some(target)),
        Err(_) => false,
    }
}

/// The banner sentence for a read-only document, plus what a refused keystroke
/// adds to it.
fn read_only_text(reason: ReadOnlyReason) -> String {
    format!(
        "{} Nothing you type is discarded - it simply is not applied.",
        reason.banner()
    )
}

/// One button inside the conflict banner.
fn conflict_button(
    id: &'static str,
    label: &'static str,
    ui: crate::theme::UiColors,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    div()
        .id(id)
        .role(Role::Button)
        .aria_label(label)
        .px_2()
        .py_0p5()
        .rounded_sm()
        .border_1()
        .border_color(ui.border)
        .bg(ui.surface)
        .text_color(ui.text)
        .cursor_pointer()
        .hover(|style| style.bg(ui.overlay))
        .on_click(on_click)
        .child(label)
        .into_any_element()
}

fn count_lines(text: &str) -> u32 {
    (text.bytes().filter(|byte| *byte == b'\n').count() + 1) as u32
}

pub(crate) fn base_block_text(base_lines: &[&str], range: &Range<u32>) -> String {
    let start = (range.start as usize).min(base_lines.len());
    let end = (range.end as usize).min(base_lines.len()).max(start);
    let mut text = base_lines[start..end].join("\n");
    if end > start && end < base_lines.len() {
        text.push('\n');
    } else if end > start
        && end == base_lines.len()
        && base_lines.len() > 1
        && start + 1 == end
        && base_lines.last() == Some(&"")
    {
        // `split_lines("a\n")` is `["a", ""]`. The trailing empty slot is the
        // final newline; joining it alone yields "" and must still emit `\n`.
        text.push('\n');
    }
    text
}

pub(crate) fn doc_line_range(doc: &CodeDocument, lines: &Range<u32>) -> Range<usize> {
    let line_count = doc.line_count();
    let start = (lines.start as usize).min(line_count);
    let end = (lines.end as usize).min(line_count).max(start);
    let byte_at = |line: usize| {
        if line < line_count {
            doc.line_to_byte(line)
        } else {
            doc.len_bytes()
        }
    };
    byte_at(start)..byte_at(end)
}

/// Splice line tokens, including the separator before an EOF suffix. A
/// trailing empty token represents a final newline but occupies zero bytes
/// at line_to_byte, so ordinary row-start offsets cannot delete it.
fn block_replacement(
    doc: &CodeDocument,
    base_lines: &[&str],
    block: &Block,
) -> (Range<usize>, String) {
    let mut range = doc_line_range(doc, &block.lines);
    let start = (block.base_lines.start as usize).min(base_lines.len());
    let end = (block.base_lines.end as usize).clamp(start, base_lines.len());
    let mut replacement = base_lines[start..end].join("\n");
    if block.lines.end as usize >= doc.line_count() {
        if block.lines.start > 0 {
            if (block.lines.start as usize) < doc.line_count() {
                range.start = range.start.saturating_sub(1);
            }
            if start < end {
                replacement.insert(0, '\n');
            }
        }
    } else if start < end {
        replacement.push('\n');
    }
    (range, replacement)
}

fn plural(count: usize, word: &str) -> String {
    if count == 1 {
        format!("{count} {word}")
    } else {
        format!("{count} {word}s")
    }
}

pub(crate) fn popup_title(block: &Block) -> String {
    match block.kind() {
        BlockKind::Added => format!("Added {}", plural(block.lines.len(), "line")),
        BlockKind::Deleted => {
            let lines = plural(block.base_lines.len(), "line");
            if block.lines.start == 0 {
                format!("Deleted {lines} at the top")
            } else {
                format!("Deleted {lines} after {}", block.lines.start)
            }
        }
        BlockKind::Modified => {
            let first = block.lines.start + 1;
            let last = block.lines.end;
            if first == last {
                format!("Modified line {first}")
            } else {
                format!("Modified lines {first}-{last}")
            }
        }
    }
}

pub(crate) fn popup_width(editor_w: f32) -> f32 {
    let available = (editor_w - 2.0 * POPUP_MARGIN).max(0.0);
    available.min(POPUP_MAX_W).max(POPUP_MIN_W.min(available))
}

fn popup_height_estimate(kind: BlockKind, shown: usize, hidden: usize) -> f32 {
    let code = if kind == BlockKind::Added {
        0.0
    } else {
        (shown as f32).min(POPUP_VISIBLE_ROWS) * CODE_ROW_HEIGHT + 2.0 * POPUP_PADDING
    };
    let footer = if hidden > 0 { POPUP_FOOTER_H } else { 0.0 };
    POPUP_HEADER_H + code + footer + POPUP_ACTIONS_H + 2.0 * POPUP_PADDING
}

pub(crate) fn popup_anchor(
    row_top: f32,
    row_bottom: f32,
    popup_h: f32,
    viewport_bottom: f32,
) -> (Anchor, f32) {
    if row_bottom + popup_h > viewport_bottom && row_top - popup_h >= 0.0 {
        (Anchor::BottomLeft, row_top)
    } else {
        (Anchor::TopLeft, row_bottom)
    }
}

impl CodeView {
    pub(crate) fn reload_base(&mut self, cx: &mut Context<Self>) {
        if self.state.document().is_none() {
            return;
        }
        self.start_base_load(cx);
    }

    fn start_base_load(&mut self, cx: &mut Context<Self>) {
        self.base_generation = self.base_generation.wrapping_add(1);
        let generation = self.base_generation;
        spawn_base_load(
            self.path.clone(),
            generation,
            cx,
            |view: &mut Self, generation, base: Base, cx| {
                if view.base_generation != generation {
                    return;
                }
                view.install_base(base, cx);
            },
        );
    }

    fn install_base(&mut self, base: Base, cx: &mut Context<Self>) {
        // Same commit SHA is not the same blob after a symlink retarget
        // (`latest` → `a.rs` then `b.rs`): HEAD did not move, the canonical
        // file did. Compare the loaded text, not only the SHA.
        let same_base = self.base == base;
        self.base = base;
        if same_base && self.tracker.is_active() {
            return;
        }
        self.popup = None;
        self.hovered_marker = None;
        self.reset_tracker(cx);
        cx.notify();
    }

    fn reset_tracker(&mut self, cx: &mut Context<Self>) {
        let doc_lines = self.state.document().map(CodeDocument::line_count);
        match (doc_lines, self.base.text()) {
            (Some(doc_lines), Some(text)) => {
                self.tracker = BlockTracker::fresh(doc_lines as u32, count_lines(text));
                self.schedule_tracker_refresh(cx);
            }
            _ => {
                self.tracker = BlockTracker::inactive();
                self.tracker_generation = self.tracker_generation.wrapping_add(1);
            }
        }
    }

    fn note_changes(&mut self, changes: &[DocChange], cx: &mut Context<Self>) {
        if !self.tracker.is_active() {
            return;
        }
        for change in changes {
            let window = change.window;
            self.tracker
                .range_changed(window.start_line, window.before_len, window.after_len);
        }
        self.schedule_tracker_refresh(cx);
    }

    fn schedule_tracker_refresh(&mut self, cx: &mut Context<Self>) {
        if !self.tracker.is_active() || !self.tracker.is_dirty() {
            return;
        }
        self.tracker_generation = self.tracker_generation.wrapping_add(1);
        let generation = self.tracker_generation;
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            cx.background_executor().timer(TRACKER_DEBOUNCE).await;
            cx.update(|cx| {
                let _ = this.update(cx, |view: &mut Self, cx: &mut Context<Self>| {
                    if view.tracker_generation == generation {
                        view.refresh_tracker_now(cx);
                    }
                });
            });
        })
        .detach();
    }

    fn refresh_tracker_now(&mut self, cx: &mut Context<Self>) {
        if !self.tracker.is_active() || !self.tracker.is_dirty() {
            return;
        }
        let Some(doc) = self.state.document() else {
            return;
        };
        let Some(base) = self.base.text().cloned() else {
            return;
        };
        let base_sha = self.base.head_sha().map(str::to_string);
        let revision = doc.revision();
        let rope = doc.text().clone();
        let tracker = self.tracker.clone();
        let load_generation = self.slot.current();
        self.tracker_generation = self.tracker_generation.wrapping_add(1);
        let tracker_generation = self.tracker_generation;
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let compute = move || {
                let mut tracker = tracker;
                let text = rope.to_string();
                let doc_lines = split_lines(&text);
                let base_lines = split_lines(&base);
                tracker.refresh_dirty(&doc_lines, &base_lines, TRACKER_POLICY);
                tracker
            };
            #[cfg(not(test))]
            let tracker = smol::unblock(compute).await;
            #[cfg(test)]
            let tracker = cx.background_spawn(async move { compute() }).await;
            cx.update(|cx| {
                let _ = this.update(cx, |view: &mut Self, cx: &mut Context<Self>| {
                    if !view.slot.accept(load_generation)
                        || !view.tracker.is_active()
                        || view.tracker_generation != tracker_generation
                    {
                        return;
                    }
                    let current = view.state.document().map(CodeDocument::revision);
                    let same_base = view.base.head_sha() == base_sha.as_deref();
                    if same_base && current == Some(revision) {
                        view.tracker = tracker;
                        cx.notify();
                    } else {
                        view.schedule_tracker_refresh(cx);
                    }
                });
            });
        })
        .detach();
    }

    pub(crate) fn marker_blocks(&self) -> &[Block] {
        self.tracker.blocks()
    }

    pub(crate) fn hovered_marker(&self) -> Option<usize> {
        self.hovered_marker
            .filter(|index| *index < self.tracker.blocks().len())
    }

    fn on_marker_move(&mut self, ev: &MouseMoveEvent, cx: &mut Context<Self>) {
        if self.text_drag.is_some() || self.navigation.drag.is_some() {
            return;
        }
        let hit = self.hits.borrow().marker_at(ev.position);
        if hit != self.hovered_marker {
            self.hovered_marker = hit;
            cx.notify();
        }
    }

    fn on_marker_down(
        &mut self,
        ev: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.scroll.bounds().contains(&ev.position) {
            return false;
        }
        let Some(index) = self.hits.borrow().marker_at(ev.position) else {
            return false;
        };
        window.focus(&self.focus, cx);
        self.end_typing_group();
        self.open_marker_popup(index, cx);
        true
    }

    fn open_marker_popup(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(block) = self.tracker.blocks().get(index).cloned() else {
            return;
        };
        let Some(doc) = self.state.document() else {
            return;
        };
        let Some(base) = self.base.text() else {
            return;
        };
        let base_lines = split_lines(base);
        let start = (block.base_lines.start as usize).min(base_lines.len());
        let end = (block.base_lines.end as usize)
            .min(base_lines.len())
            .max(start);
        let block_lines = &base_lines[start..end];
        let shown_count = block_lines.len().min(POPUP_SHOWN_LINES);
        let shown_text = block_lines[..shown_count].join("\n");
        let syntax = DiffSyntax::from_theme(&crate::theme::active_theme());
        let runs = highlight_lines(&shown_text, doc.ext(), &syntax);
        let shown = block_lines[..shown_count]
            .iter()
            .zip(runs.into_iter().chain(std::iter::repeat_with(Vec::new)))
            .map(|(line, runs)| (SharedString::from(line.to_string()), runs))
            .collect();
        let popup = MarkerPopup {
            title: popup_title(&block),
            shown,
            hidden: block_lines.len() - shown_count,
            base_text: base_block_text(&base_lines, &block.base_lines),
            block,
        };
        self.popup = Some(popup);
        cx.notify();
    }

    fn close_marker_popup(&mut self, cx: &mut Context<Self>) {
        if self.popup.take().is_some() {
            cx.notify();
        }
    }

    fn copy_popup_base(&mut self, cx: &mut Context<Self>) {
        if let Some(popup) = &self.popup {
            cx.write_to_clipboard(ClipboardItem::new_string(popup.base_text.clone()));
        }
    }

    fn revert_from_popup(&mut self, cx: &mut Context<Self>) {
        let Some(popup) = self.popup.take() else {
            return;
        };
        cx.notify();
        // `popup.block.lines` is the range at open time. Keystrokes shift the
        // live tracker; `base_lines` is the stable HEAD identity, so look the
        // block up there instead of restoring whatever now sits at the old row.
        let Some(line) = self
            .tracker
            .blocks()
            .iter()
            .find(|block| block.base_lines == popup.block.base_lines)
            .map(|block| block.lines.start as usize)
        else {
            // The original block may have disappeared or merged during an
            // edit. Its former row can now name an unrelated change.
            return;
        };
        self.revert_block(line, cx);
    }

    pub(crate) fn revert_block(&mut self, line: usize, cx: &mut Context<Self>) -> bool {
        if self.state.document().is_none_or(CodeDocument::is_read_only) {
            self.flash_read_only(cx);
            return false;
        }
        let Some((_, block)) = self.tracker.block_at(line as u32) else {
            log::debug!(
                "revert: no changed block at line {} of {}",
                line + 1,
                self.path.display()
            );
            return false;
        };
        let block = block.clone();
        let (range, replacement) = {
            let Some(doc) = self.state.document() else {
                return false;
            };
            let Some(base) = self.base.text() else {
                return false;
            };
            let base_lines = split_lines(base);
            block_replacement(doc, &base_lines, &block)
        };
        let caret = CodeSelection::at(range.start);
        self.end_typing_group();
        if !self.splice_all(&[(range, replacement)], caret, EditGroup::Atomic, cx) {
            return false;
        }
        self.refresh_tracker_now(cx);
        true
    }

    fn render_marker_popup(
        &self,
        ui: crate::theme::UiColors,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let popup = self.popup.as_ref()?;
        let bounds = self.scroll.bounds();
        let (row_top, anchor_x) = {
            let hits = self.hits.borrow();
            (
                hits.row_top(popup.block.lines.start as usize),
                hits.marker_x + MARKER_COLUMN_W,
            )
        };
        let kind = popup.block.kind();
        let row_bottom = if kind == BlockKind::Deleted {
            row_top
        } else {
            row_top + CODE_ROW_HEIGHT
        };
        let width = popup_width(f32::from(bounds.size.width));
        let height = popup_height_estimate(kind, popup.shown.len(), popup.hidden);
        let (anchor, anchor_y) = popup_anchor(
            row_top,
            row_bottom,
            height,
            f32::from(window.viewport_size().height),
        );
        let font = code_font();

        let mut panel = menu_surface(div().id("code-marker-popup"), ui)
            .flex()
            .flex_col()
            .w(px(width))
            .p(px(POPUP_PADDING))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_down_out(
                cx.listener(|this, _: &MouseDownEvent, _w, cx| this.close_marker_popup(cx)),
            )
            .child(
                div()
                    .flex_none()
                    .h(px(POPUP_HEADER_H))
                    .px(px(8.))
                    .flex()
                    .items_center()
                    .text_size(crate::ui_primitives::BODY)
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(ui.text)
                    .child(SharedString::from(popup.title.clone())),
            );
        if kind != BlockKind::Added {
            let lines = popup.shown.iter().map(|(text, syntax)| {
                let runs = syntax_text_runs(text, syntax, &font, ui.text);
                div()
                    .flex_none()
                    .h(px(CODE_ROW_HEIGHT))
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .font_family(font.family.clone())
                    .text_size(px(CODE_FONT_SIZE))
                    .child(StyledText::new(text.clone()).with_runs(runs))
                    .into_any_element()
            });
            panel = panel.child(
                div()
                    .id("code-marker-popup-lines")
                    .flex_none()
                    .max_h(px(
                        POPUP_VISIBLE_ROWS * CODE_ROW_HEIGHT + 2.0 * POPUP_PADDING
                    ))
                    .overflow_y_scroll()
                    .mx(px(2.))
                    .py(px(POPUP_PADDING))
                    .px(px(8.))
                    .rounded(px(6.))
                    .bg(ui.vc_deleted_background)
                    .flex()
                    .flex_col()
                    .children(lines),
            );
            if popup.hidden > 0 {
                panel = panel.child(
                    div()
                        .flex_none()
                        .h(px(POPUP_FOOTER_H))
                        .px(px(8.))
                        .flex()
                        .items_center()
                        .text_size(crate::ui_primitives::LABEL_SM)
                        .text_color(ui.muted)
                        .child(SharedString::from(format!(
                            "and {} more lines",
                            popup.hidden
                        ))),
                );
            }
        }
        let mut actions = div()
            .flex_none()
            .h(px(POPUP_ACTIONS_H))
            .px(px(4.))
            .flex()
            .flex_row()
            .items_center()
            .justify_end()
            .gap(px(6.))
            .text_size(crate::ui_primitives::BODY);
        if kind != BlockKind::Added {
            actions = actions.child(conflict_button(
                "code-marker-copy",
                "Copy",
                ui,
                cx.listener(|this, _: &ClickEvent, _w, cx| this.copy_popup_base(cx)),
            ));
        }
        actions = actions.child(conflict_button(
            "code-marker-revert",
            "Revert",
            ui,
            cx.listener(|this, _: &ClickEvent, _w, cx| this.revert_from_popup(cx)),
        ));
        panel = panel.child(actions);

        Some(
            deferred(
                anchored()
                    .anchor(anchor)
                    .position(point(px(anchor_x), px(anchor_y)))
                    .child(panel),
            )
            .with_priority(3)
            .into_any_element(),
        )
    }
}

/// The native text-input and IME target (US-012).
///
/// GPUI dispatches actions before it dispatches text (`gpui/src/window.rs:4525`
/// only reaches `dispatch_input` when the action pass left propagation alive
/// and the keystroke carries a `key_char`), so Enter, Tab and Backspace land on
/// their bindings and only genuinely printable input arrives here. All of it
/// converts between UTF-16 - the unit every platform IME speaks - and the
/// rope's bytes through [`CodeDocument::byte_to_utf16`] and its inverse.
impl EntityInputHandler for CodeView {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        adjusted_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let doc = self.state.document()?;
        let start = doc.utf16_to_byte(range_utf16.start);
        let end = doc.utf16_to_byte(range_utf16.end).max(start);
        *adjusted_range = Some(doc.byte_to_utf16(start)..doc.byte_to_utf16(end));
        Some(doc.slice_string(start..end))
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let doc = self.state.document()?;
        let range = self.selection.range();
        Some(UTF16Selection {
            range: doc.byte_to_utf16(range.start)..doc.byte_to_utf16(range.end),
            reversed: self.selection.head < self.selection.anchor,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        let doc = self.state.document()?;
        let marked = self.marked.clone()?;
        Some(doc.byte_to_utf16(marked.start)..doc.byte_to_utf16(marked.end))
    }

    /// The platform abandoning a composition. The text stays: it is already in
    /// the rope and in one undo transaction, so Ctrl+Z is what removes it.
    fn unmark_text(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.marked.take().is_some() {
            self.history.close_group();
            cx.notify();
        }
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(range) = self.resolve_replacement(range_utf16) else {
            return;
        };
        self.marked = None;
        let inserted = normalize_newlines(text).into_owned();
        let caret = CodeSelection::at(range.start + inserted.len());
        self.splice_all(&[(range, inserted)], caret, EditGroup::Typing, cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(range) = self.resolve_replacement(range_utf16) else {
            return;
        };
        let inserted = normalize_newlines(new_text).into_owned();
        let start = range.start;
        let end = start + inserted.len();
        let caret = CodeSelection::at(end);
        // Typing, so the whole composition - every intermediate state the IME
        // pushed - collapses into one undo transaction.
        if !self.splice_all(&[(range, inserted)], caret, EditGroup::Typing, cx) {
            return;
        }
        self.marked = if start == end { None } else { Some(start..end) };
        // The IME's own caret inside the composition, expressed relative to it.
        if let Some(selected) = new_selected_range_utf16
            && let Some(doc) = self.state.document()
        {
            let base = doc.byte_to_utf16(start);
            let head = doc.utf16_to_byte(base + selected.end);
            let anchor = doc.utf16_to_byte(base + selected.start);
            self.selection = CodeSelection { anchor, head };
        }
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        element_bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let geometry = self.geometry.get();
        let doc = self.state.document()?;
        let start = doc.utf16_to_byte(range_utf16.start);
        let row = doc.byte_to_line(start);
        let column = cursor::goal_column(doc, start);
        let hits = self.hits.borrow();
        // The hit map is the frame that was actually painted, so the candidate
        // window lands on the composition even when the gutter is wide or the
        // line is scrolled sideways. With no painted frame yet, the element's
        // own origin is the honest fallback.
        let (x, y) = if hits.lines.is_empty() {
            (
                f32::from(element_bounds.origin.x),
                f32::from(element_bounds.origin.y),
            )
        } else {
            (
                hits.text_x + column as f32 * geometry.char_w,
                hits.top_y + row.saturating_sub(hits.first_row) as f32 * CODE_ROW_HEIGHT,
            )
        };
        Some(Bounds {
            origin: Point::new(px(x), px(y)),
            size: size(px(geometry.char_w.max(1.0)), px(CODE_ROW_HEIGHT)),
        })
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        let doc = self.state.document()?;
        let offset = self.hits.borrow().offset_at(doc, point);
        Some(doc.byte_to_utf16(offset))
    }
}

impl Focusable for CodeView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for CodeView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_theme();
        let ui = crate::theme::ui_colors();

        let Some(doc) = self.state.document() else {
            return match self.state.error_message() {
                Some(message) => self.render_load_error(message, ui, cx),
                None => super::super::render::diff_panel_centered(
                    "icons/loader-circle.svg",
                    "Loading file…",
                    ui,
                ),
            };
        };

        self.scroll.set_line_count(doc.line_count());
        let banners = self.banners(ui, cx);
        let theme = crate::theme::active_theme();
        let focused = self.focus.is_focused(window);
        let element = CodeElement::new(
            cx.entity(),
            palette(ui),
            CodeColors {
                scrollbar_thumb: theme.scrollbar_thumb,
                cursor: theme.cursor,
                selection: theme.selection,
                selection_fg: theme.selection_foreground,
                marker_added: ui.vc_added,
                marker_modified: ui.vc_modified,
                marker_deleted: ui.vc_deleted,
            },
            self.scroll.clone(),
            self.h_offset,
            CodeCaret {
                cursor: self.selection.cursor(),
                selection: self.selection.range(),
                focused,
                visible: self.blink_visible,
                marked: self.marked.clone().unwrap_or(0..0),
            },
            self.geometry.clone(),
            self.gutter_memo.clone(),
            self.hits.clone(),
        );

        // `overflow_hidden`, not `overflow_y_scroll`: the element owns the
        // position and places its rows itself (see the module docs). The row
        // height is pinned on the host so nothing inherited can leak into a
        // text metric the element reads.
        let popup = self.render_marker_popup(ui, window, cx);
        let host = div()
            .id(self.element_id.clone())
            .flex_1()
            .min_h_0()
            .w_full()
            .overflow_hidden()
            .line_height(px(CODE_ROW_HEIGHT))
            .on_hover(cx.listener(|this, hovered: &bool, _window, cx| {
                if !*hovered && this.navigation.hovered.take().is_some() {
                    cx.notify();
                }
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, ev: &MouseDownEvent, window, cx| {
                    if this.on_scrollbar_down(ev, cx)
                        || this.on_marker_down(ev, window, cx)
                        || this.on_text_down(ev, window, cx)
                    {
                        cx.stop_propagation();
                    }
                }),
            )
            .on_scroll_wheel(cx.listener(|this, ev: &ScrollWheelEvent, _window, cx| {
                this.apply_wheel(ev, cx);
            }))
            .child(element);

        div()
            .id("code-view-body")
            .key_context(CODE_KEY_CONTEXT)
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::left))
            .on_action(cx.listener(Self::right))
            .on_action(cx.listener(Self::up))
            .on_action(cx.listener(Self::down))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::select_up))
            .on_action(cx.listener(Self::select_down))
            .on_action(cx.listener(Self::word_left))
            .on_action(cx.listener(Self::word_right))
            .on_action(cx.listener(Self::select_word_left))
            .on_action(cx.listener(Self::select_word_right))
            .on_action(cx.listener(Self::home))
            .on_action(cx.listener(Self::end))
            .on_action(cx.listener(Self::select_home))
            .on_action(cx.listener(Self::select_end))
            .on_action(cx.listener(Self::page_up))
            .on_action(cx.listener(Self::page_down))
            .on_action(cx.listener(Self::select_page_up))
            .on_action(cx.listener(Self::select_page_down))
            .on_action(cx.listener(Self::doc_start))
            .on_action(cx.listener(Self::doc_end))
            .on_action(cx.listener(Self::select_doc_start))
            .on_action(cx.listener(Self::select_doc_end))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::newline))
            .on_action(cx.listener(Self::undo))
            .on_action(cx.listener(Self::redo))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::paste_action))
            .on_action(cx.listener(Self::indent))
            .on_action(cx.listener(Self::outdent))
            .on_action(cx.listener(Self::save_action))
            .on_action(cx.listener(Self::escape))
            .flex_1()
            .min_h_0()
            .w_full()
            .flex()
            .flex_col()
            // The drag continuation lives on the root, not on the scroll host:
            // mouse listeners only fire over their own hitbox, so keeping them
            // on the host would drop the release the moment the pointer leaves
            // it. Same placement as the markdown view and the settings pane.
            .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _w, cx| {
                this.on_scrollbar_move(ev, cx);
                this.on_marker_move(ev, cx);
                this.on_text_move(ev, cx);
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, ev: &MouseUpEvent, _w, cx| {
                    this.on_scrollbar_up(ev, cx);
                    this.on_text_up(ev, cx);
                }),
            )
            .children(banners)
            .child(host)
            .children(popup)
            .into_any_element()
    }
}

/// Synchronous spellings of the reload pipeline, for tests that want the
/// outcome of a probe without driving the background diff.
#[cfg(test)]
impl CodeView {
    /// A probe's outcome folded in end to end on the calling thread: what
    /// [`reload_from_disk`] does, minus the executor hop and the retry.
    fn disk_loaded(
        &mut self,
        generation: u64,
        loaded: DiskLoad,
        force: bool,
        cx: &mut Context<Self>,
    ) {
        let Some((diff, incoming)) = self.begin_disk_reload(generation, loaded, force, cx) else {
            return;
        };
        let splices = edit::disk_splices(&diff.rope, &incoming.text().to_string());
        self.finish_disk_reload(
            generation,
            DiskSplices {
                revision: diff.revision,
                splices,
            },
            &incoming,
            false,
            force,
            cx,
        );
    }

    /// A probe that read `text` with `stamp` (`None` for a missing file).
    fn disk_changed(
        &mut self,
        generation: u64,
        stamp: Option<FileStamp>,
        text: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let loaded = text
            .filter(|_| stamp.is_some())
            .map(|text| {
                (
                    super::load::build_document(self.path.clone(), &text, false),
                    stamp,
                )
            })
            .ok_or(CodeLoadError::NotFound);
        self.disk_loaded(generation, loaded, false, cx);
    }

    /// Replace the buffer with `text` as a reload would, guards aside: the
    /// document's own read-only rule stays, the line ending is the text's.
    fn adopt_disk_text(&mut self, text: &str, cx: &mut Context<Self>) {
        let Some(doc) = self.state.document() else {
            return;
        };
        let splices = edit::disk_splices(doc.text(), text);
        let read_only = doc.read_only_reason();
        self.apply_disk_splices(&splices, LineEnding::detect(text), read_only, cx);
    }
}

#[cfg(test)]
mod tests {
    use gpui::{Entity, Modifiers, TestAppContext, TouchPhase, VisualTestContext, point};

    use super::super::highlight::CodeHighlighter;
    use super::super::load::{LoadedCode, build_document, open_blocking};
    use super::*;
    use crate::widgets::scrollbar;

    /// Build a view around `text` inside a real window: the action handlers
    /// take a `&mut Window`, so the tests need one, and `update_in` is the only
    /// way to get a genuine one.
    ///
    /// The state is assembled by hand rather than through `CodeView::new`,
    /// whose constructor kicks off an off-thread read the deterministic test
    /// scheduler refuses. An empty `text` leaves the view loading, which is
    /// what the scrollbar guard needs.
    fn view<'a>(
        cx: &'a mut TestAppContext,
        text: &str,
    ) -> (Entity<CodeView>, &'a mut VisualTestContext) {
        view_named(cx, "/nonexistent/paneflow-code.rs", text)
    }

    /// [`view`] over a path of the caller's choosing: the extension decides
    /// whether the file is syntax-colored, which the shaping tests care about.
    fn view_named<'a>(
        cx: &'a mut TestAppContext,
        name: &str,
        text: &str,
    ) -> (Entity<CodeView>, &'a mut VisualTestContext) {
        let path = PathBuf::from(name);
        let state = if text.is_empty() {
            CodeLoadState::Loading
        } else {
            let document = build_document(path.clone(), text, false);
            let highlighter = CodeHighlighter::new(
                &document,
                DiffSyntax::from_theme(&crate::theme::paneflow_dark()),
            );
            CodeLoadState::Ready(Box::new(LoadedCode {
                document,
                highlighter,
                indent: IndentUnit::Spaces(4),
                stamp: None,
            }))
        };
        cx.add_window_view(move |_window, cx| {
            let focus = cx.focus_handle();
            let controls = EditorControls::attach(focus.clone(), cx);
            CodeView {
                controls,
                navigation: NavigationState::default(),
                element_id: "code-view:test".into(),
                path,
                state,
                slot: CodeLoadSlot::new(),
                focus,
                scroll: CodeScroll::new(),
                h_offset: 0.0,
                selection: CodeSelection::default(),
                goal_column: 0,
                text_drag: None,
                click_chain: None,
                last_motion: Instant::now(),
                blink_visible: true,
                theme_generation: 0,
                geometry: Rc::new(Cell::new(CodeGeometry::default())),
                gutter_memo: Rc::new(Cell::new(GutterMemo::default())),
                hits: Rc::new(RefCell::new(CodeHitMap::default())),
                history: edit::UndoHistory::default(),
                saved_mark: edit::HistoryMark::default(),
                indent: IndentUnit::Spaces(4),
                marked: None,
                read_only_flash: None,
                stamp: None,
                pending_stamp: None,
                disk: DiskState::default(),
                save_error: None,
                disk_generation: 0,
                saving: false,
                longest_line_scan_in_flight: false,
                longest_line_rescan_needed: false,
                _watcher: None,
                _watch_bridge: None,
                base: Base::None,
                tracker: BlockTracker::inactive(),
                tracker_generation: 0,
                base_generation: 0,
                hovered_marker: None,
                popup: None,
            }
        })
    }

    /// A release outside the view never reaches the mouse-up listener, so the
    /// drag has to end on the first move that arrives with no button held.
    /// Without the guard, re-entering the view after such a release scrolled
    /// the file off the stale anchor with nothing pressed (US-007).
    #[gpui::test]
    fn a_move_without_the_left_button_ends_the_drag(cx: &mut TestAppContext) {
        let (view, cx) = view(cx, "");

        view.update(cx, |view, cx| {
            use super::super::navigation::{NavigationLayout, NavigationPart, Track};
            view.scroll.set_metrics(
                gpui::Bounds::new(point(px(0.), px(0.)), size(px(200.), px(400.))),
                100,
            );
            view.navigation.layout.set(NavigationLayout {
                vertical: Some(Track {
                    bounds: gpui::Bounds::new(point(px(0.), px(0.)), size(px(15.), px(400.))),
                    thumb: Some(gpui::Bounds::new(
                        point(px(0.), px(30.)),
                        size(px(15.), px(40.)),
                    )),
                }),
                ..Default::default()
            });
            assert!(view.navigation.mouse_down(
                point(px(7.), px(40.)),
                &view.scroll,
                &mut view.h_offset,
                0.0
            ));

            view.on_scrollbar_move(
                &MouseMoveEvent {
                    position: point(px(10.), px(90.)),
                    pressed_button: None,
                    modifiers: Modifiers::default(),
                },
                cx,
            );
            assert!(view.navigation.drag.is_none());

            assert!(view.navigation.mouse_down(
                point(px(7.), px(40.)),
                &view.scroll,
                &mut view.h_offset,
                0.0
            ));
            view.on_scrollbar_move(
                &MouseMoveEvent {
                    position: point(px(10.), px(90.)),
                    pressed_button: Some(MouseButton::Left),
                    modifiers: Modifiers::default(),
                },
                cx,
            );
            assert!(view.navigation.dragging(NavigationPart::Vertical));
        });
    }

    /// EP-005 US-018: the file header reads the caret as 1-based line and
    /// column, the way every editor's status bar states it, and a document
    /// that has not loaded yet still answers with a coherent position.
    #[gpui::test]
    fn the_header_reads_the_caret_as_one_based_line_and_column(cx: &mut TestAppContext) {
        let (editor, cx) = view(cx, "let foo = 1;\nbb\nlast line");

        editor.update_in(cx, |view, window, cx| {
            // Start of the document: line 1, column 1 - never 0.
            assert_eq!(view.cursor_line_column(), (1, 1));

            view.right(&CeRight, window, cx);
            view.right(&CeRight, window, cx);
            view.right(&CeRight, window, cx);
            assert_eq!(view.cursor_line_column(), (1, 4));

            view.down(&CeDown, window, cx);
            assert_eq!(
                view.cursor_line_column().0,
                2,
                "the caret moved a line down"
            );

            view.doc_end(&CeDocEnd, window, cx);
            assert_eq!(view.cursor_line_column(), (3, 10), "end of `last line`");
        });

        // A tab whose load is still in flight reports the origin rather than
        // panicking on the absent document.
        let (loading, cx) = view(cx, "");
        loading.update(cx, |view, _cx| {
            assert!(view.document().is_none());
            assert_eq!(view.cursor_line_column(), (1, 1));
        });
    }

    /// US-011: the actions the key bindings dispatch to walk the document by
    /// grapheme, by word and to both edges, and plain motion never selects.
    #[gpui::test]
    fn the_navigation_actions_walk_the_document(cx: &mut TestAppContext) {
        let (view, cx) = view(cx, "let foo = 1;\nbb\nlast line");

        view.update_in(cx, |view, window, cx| {
            view.right(&CeRight, window, cx);
            assert_eq!(view.cursor(), 1);
            view.word_right(&CeWordRight, window, cx);
            assert_eq!(view.cursor(), 3, "end of `let`");
            view.end(&CeEnd, window, cx);
            assert_eq!(view.cursor(), 12);
            view.right(&CeRight, window, cx);
            assert_eq!(
                view.cursor(),
                13,
                "right at a row end steps to the next row"
            );
            view.home(&CeHome, window, cx);
            assert_eq!(view.cursor(), 13);
            view.doc_end(&CeDocEnd, window, cx);
            assert_eq!(view.cursor(), view.document().unwrap().len_bytes());
            view.doc_start(&CeDocStart, window, cx);
            assert_eq!(view.cursor(), 0);
            assert!(view.selection().is_empty(), "plain motion never selects");
        });
    }

    /// US-010 / US-011: Shift extends instead of replacing, a bare arrow
    /// collapses onto the selection's edge, and Select All takes the document.
    #[gpui::test]
    fn shift_extends_and_select_all_takes_the_document(cx: &mut TestAppContext) {
        let (view, cx) = view(cx, "abc\ndef");

        view.update_in(cx, |view, window, cx| {
            view.select_right(&CeSelectRight, window, cx);
            view.select_right(&CeSelectRight, window, cx);
            assert_eq!(view.selection(), 0..2);
            assert_eq!(view.cursor(), 2);

            view.left(&CeLeft, window, cx);
            assert_eq!(view.cursor(), 0, "collapses onto the near edge");
            assert!(view.selection().is_empty());

            view.select_all(&CeSelectAll, window, cx);
            assert_eq!(view.selection(), 0..7);
        });
    }

    /// US-011: Up/Down keep the column they started from, even after crossing a
    /// shorter row.
    #[gpui::test]
    fn vertical_motion_restores_the_goal_column(cx: &mut TestAppContext) {
        let (view, cx) = view(cx, "aaaaaaa\nbb\ncccccccc");

        view.update_in(cx, |view, window, cx| {
            view.place_caret(5, false, cx);
            view.down(&CeDown, window, cx);
            assert_eq!(view.cursor(), 10, "clamped to the short row");
            view.down(&CeDown, window, cx);
            assert_eq!(view.cursor(), 16, "the goal column comes back");
        });
    }

    /// US-009: a caret pushed past the end of the file lands on the last legal
    /// slot rather than panicking, and US-010: a new caret drops the selection
    /// without touching the content.
    #[gpui::test]
    fn the_caret_clamps_and_a_new_caret_clears_the_selection(cx: &mut TestAppContext) {
        let (view, cx) = view(cx, "one\ntwo");

        view.update(cx, |view, cx| {
            view.place_caret(9_999, false, cx);
            assert_eq!(view.cursor(), 7);

            view.take_whole_document(cx);
            assert_eq!(view.selection(), 0..7);
            let before = view.document().unwrap().len_bytes();
            view.place_caret(2, false, cx);
            assert!(view.selection().is_empty(), "the selection is gone");
            assert_eq!(
                view.document().unwrap().len_bytes(),
                before,
                "and the content is untouched"
            );
            assert_eq!(view.cursor_row(), 0, "the row follows the byte offset");
        });
    }

    /// US-010: presses inside the interval chain into double then triple, and a
    /// press that is too late or too far restarts the chain.
    #[gpui::test]
    fn multi_click_chains_then_resets(cx: &mut TestAppContext) {
        let (view, cx) = view(cx, "let foo = 1;\nnext");

        view.update(cx, |view, _cx| {
            let at = point(px(4.), px(4.));
            let now = Instant::now();
            assert_eq!(view.chain_click(at, now), 1);
            assert_eq!(view.chain_click(at, now), 2);
            assert_eq!(view.chain_click(at, now), 3);
            assert_eq!(view.chain_click(at, now), 1, "the chain wraps at three");

            assert_eq!(view.chain_click(at, now), 2);
            assert_eq!(
                view.chain_click(point(px(80.), px(4.)), now),
                1,
                "too far restarts it"
            );
            assert_eq!(
                view.chain_click(
                    point(px(80.), px(4.)),
                    now + MULTI_CLICK_INTERVAL + Duration::from_millis(1)
                ),
                1,
                "too late restarts it"
            );
        });
    }

    /// US-010: a drag started on a word keeps whole words selected, and one
    /// started on a row keeps whole rows, whichever direction the pointer goes.
    #[gpui::test]
    fn a_word_drag_extends_by_whole_words(cx: &mut TestAppContext) {
        let (view, cx) = view(cx, "alpha beta gamma");

        view.update(cx, |view, cx| {
            // Stand in for the press: a double click on `beta`.
            view.selection = CodeSelection {
                anchor: 6,
                head: 10,
            };
            view.text_drag = Some(TextDrag {
                grain: DragGrain::Word,
                anchor: 6..10,
            });
            view.extend_drag_to(2, cx);
            assert_eq!(view.selection(), 0..10, "backward, whole words");
            view.extend_drag_to(13, cx);
            assert_eq!(view.selection(), 6..16, "forward, whole words");
        });
    }

    fn rows_of_code(rows: usize) -> String {
        (0..rows).map(|row| format!("fn f{row}() {{}}\n")).collect()
    }

    // --------------------------------------------------------------- EP-008

    const VIEWPORT: Point<Pixels> = Point {
        x: px(800.),
        y: px(360.),
    };

    /// A view laid out in an 800 x 360 window with the pointer over it, so a
    /// wheel event lands on the host and the element has a real viewport.
    fn scrolled<'a>(
        cx: &'a mut TestAppContext,
        name: &str,
        text: &str,
    ) -> (Entity<CodeView>, &'a mut VisualTestContext) {
        let (view, cx) = view_named(cx, name, text);
        cx.simulate_resize(size(VIEWPORT.x, VIEWPORT.y));
        cx.run_until_parked();
        let centre = point(VIEWPORT.x / 2., VIEWPORT.y / 2.);
        cx.simulate_mouse_move(centre, None, Modifiers::default());
        cx.run_until_parked();
        (view, cx)
    }

    fn wheel(delta: ScrollDelta) -> ScrollWheelEvent {
        ScrollWheelEvent {
            position: point(VIEWPORT.x / 2., VIEWPORT.y / 2.),
            delta,
            modifiers: Modifiers::default(),
            touch_phase: TouchPhase::Moved,
        }
    }

    fn notification_counter(
        view: &Entity<CodeView>,
        cx: &mut VisualTestContext,
    ) -> (Rc<Cell<usize>>, gpui::Subscription) {
        let count = Rc::new(Cell::new(0usize));
        let seen = count.clone();
        let subscription =
            cx.update(|_, cx| cx.observe(view, move |_, _| seen.set(seen.get() + 1)));
        (count, subscription)
    }

    /// US-023: a wheel notch moves exactly three editor rows, not three of the
    /// host div's inherited line height.
    #[gpui::test]
    fn a_wheel_notch_scrolls_three_rows(cx: &mut TestAppContext) {
        let (view, cx) = scrolled(cx, "/nonexistent/wheel.rs", &rows_of_code(500));

        cx.simulate_event(wheel(ScrollDelta::Lines(point(0.0, -3.0))));
        cx.run_until_parked();

        assert_eq!(
            view.read_with(cx, |view, _| view.scroll_offset_y()),
            3.0 * CODE_ROW_HEIGHT,
            "a notch must move exactly three rows"
        );
        assert_eq!(view.read_with(cx, |view, _| view.scroll_rows()), 3.0);
    }

    /// US-023: a macOS trackpad delivers pixels, and they pass through
    /// unrounded.
    #[gpui::test]
    fn a_trackpad_delta_scrolls_its_exact_pixels(cx: &mut TestAppContext) {
        let (view, cx) = scrolled(cx, "/nonexistent/trackpad.rs", &rows_of_code(500));

        cx.simulate_event(wheel(ScrollDelta::Pixels(point(px(0.), px(-7.5)))));
        cx.run_until_parked();

        assert_eq!(view.read_with(cx, |view, _| view.scroll_offset_y()), 7.5);
    }

    #[gpui::test]
    fn a_horizontal_notch_moves_whole_columns(cx: &mut TestAppContext) {
        let mut text = "x".repeat(400);
        text.push('\n');
        text.push_str(&rows_of_code(200));
        let (view, cx) = scrolled(cx, "/nonexistent/wide.rs", &text);

        let char_w = view.read_with(cx, |view, _| view.geometry.get().char_w);
        assert!(char_w > 0.0, "the test text system must measure a column");

        cx.simulate_event(wheel(ScrollDelta::Lines(point(-1.0, 0.0))));
        cx.run_until_parked();

        let (h_offset, rows) = view.read_with(cx, |view, _| (view.h_offset, view.scroll_rows()));
        assert_eq!(h_offset, char_w, "one notch is one column, not one line");
        assert_eq!(rows, 0.0, "a horizontal notch must not scroll vertically");

        cx.simulate_event(wheel(ScrollDelta::Lines(point(-2.0, 0.0))));
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |view, _| view.h_offset), 3.0 * char_w);
    }

    #[gpui::test]
    fn a_document_shorter_than_the_viewport_absorbs_the_notch(cx: &mut TestAppContext) {
        let (view, cx) = scrolled(cx, "/nonexistent/short.rs", &rows_of_code(3));
        let (notifications, _subscription) = notification_counter(&view, cx);

        cx.simulate_event(wheel(ScrollDelta::Lines(point(0.0, -3.0))));
        cx.run_until_parked();

        assert_eq!(view.read_with(cx, |view, _| view.scroll_offset_y()), 0.0);
        assert_eq!(
            notifications.get(),
            0,
            "an absorbed notch must not repaint the editor"
        );
    }

    #[gpui::test]
    fn notches_in_one_frame_coalesce_into_a_single_notification(cx: &mut TestAppContext) {
        let (view, cx) = scrolled(cx, "/nonexistent/coalesce.rs", &rows_of_code(500));
        let (notifications, _subscription) = notification_counter(&view, cx);

        cx.update(|window, cx| {
            for _ in 0..3 {
                window.dispatch_event(
                    gpui::PlatformInput::ScrollWheel(wheel(ScrollDelta::Lines(point(0.0, -3.0)))),
                    cx,
                );
            }
        });
        cx.run_until_parked();

        assert_eq!(
            view.read_with(cx, |view, _| view.scroll_rows()),
            9.0,
            "the three deltas must all land"
        );
        assert_eq!(
            notifications.get(),
            1,
            "three notches inside one frame are one repaint"
        );
    }

    /// US-024: at 300 000 lines the last row sits on the viewport floor,
    /// stays there across two identical frames, and moves by exactly one row
    /// height per scrolled row.
    #[gpui::test]
    fn the_last_row_of_a_huge_file_lands_on_the_viewport_floor(cx: &mut TestAppContext) {
        let line_count = 300_000usize;
        let text = rows_of_code(line_count);
        let (view, cx) = scrolled(cx, "/nonexistent/huge.rs", &text);

        view.update(cx, |view, cx| {
            view.scroll.set_rows(view.scroll.max_rows());
            cx.notify();
        });
        cx.run_until_parked();

        let (last, first, floor) = view.read_with(cx, |view, _| {
            let last = view.document().expect("a loaded document").line_count() - 1;
            (
                last,
                view.row_top(last),
                f32::from(view.scroll.bounds().bottom()),
            )
        });
        assert!(last >= line_count, "{last} must reach past {line_count}");
        assert!(
            (first + CODE_ROW_HEIGHT - floor).abs() < 1.0,
            "the last row must sit on the viewport floor, got {first} for a floor at {floor}"
        );

        view.update(cx, |_, cx| cx.notify());
        cx.run_until_parked();
        assert_eq!(
            view.read_with(cx, |view, _| view.row_top(last)),
            first,
            "two identical frames must place the last row identically"
        );

        view.update(cx, |view, cx| {
            view.scroll.set_rows(view.scroll.rows() - 1.0);
            cx.notify();
        });
        cx.run_until_parked();
        assert_eq!(
            view.read_with(cx, |view, _| view.row_top(last)),
            first + CODE_ROW_HEIGHT,
            "one scrolled row must move the last row by exactly one row height"
        );
    }

    #[gpui::test]
    fn the_scrollbar_drives_the_owned_position(cx: &mut TestAppContext) {
        let (view, cx) = scrolled(cx, "/nonexistent/bar.rs", &rows_of_code(2_000));

        let thumb_h = view.update(cx, |view, cx| {
            let metrics = scrollbar::metrics(&view.scroll).expect("an overflowing document");
            let bar_x = view.scroll.bounds().right() - px(3.);
            let below_thumb = view.scroll.bounds().origin.y + px(metrics.thumb_h + 40.0);
            assert!(view.on_scrollbar_down(
                &MouseDownEvent {
                    button: MouseButton::Left,
                    position: point(bar_x, below_thumb),
                    modifiers: Modifiers::default(),
                    click_count: 1,
                    first_mouse: false,
                },
                cx,
            ));
            metrics.thumb_h
        });
        let after_click = view.read_with(cx, |view, _| view.scroll_rows());
        assert!(after_click > 0.0, "a track click must move the position");

        view.update(cx, |view, cx| {
            let metrics = scrollbar::metrics(&view.scroll).expect("an overflowing document");
            let bar_x = view.scroll.bounds().right() - px(3.);
            let thumb_y = view.scroll.bounds().origin.y + px(metrics.thumb_top + thumb_h / 2.0);
            view.on_scrollbar_down(
                &MouseDownEvent {
                    button: MouseButton::Left,
                    position: point(bar_x, thumb_y),
                    modifiers: Modifiers::default(),
                    click_count: 1,
                    first_mouse: false,
                },
                cx,
            );
            view.on_scrollbar_move(
                &MouseMoveEvent {
                    position: point(bar_x, thumb_y + px(60.)),
                    pressed_button: Some(MouseButton::Left),
                    modifiers: Modifiers::default(),
                },
                cx,
            );
        });
        let after_drag = view.read_with(cx, |view, _| view.scroll_rows());
        assert!(
            after_drag > after_click,
            "dragging the thumb down must advance the position, {after_click} -> {after_drag}"
        );
    }

    #[gpui::test]
    fn an_external_reload_that_drops_lines_rebinds_the_position(cx: &mut TestAppContext) {
        let (view, cx) = scrolled(cx, "/nonexistent/reload.rs", &rows_of_code(500));

        view.update(cx, |view, cx| {
            view.scroll.set_rows(view.scroll.max_rows());
            cx.notify();
        });
        cx.run_until_parked();
        assert!(view.read_with(cx, |view, _| view.scroll_rows()) > 400.0);

        view.update(cx, |view, cx| {
            view.adopt_disk_text(&rows_of_code(25), cx);
        });
        cx.run_until_parked();

        let (rows, max_rows, viewport_h) = view.read_with(cx, |view, _| {
            (
                view.scroll_rows(),
                view.scroll.max_rows(),
                view.scroll.viewport_height(),
            )
        });
        let line_count = view.read_with(cx, |view, _| {
            view.document().expect("a loaded document").line_count()
        });
        assert_eq!(
            max_rows,
            line_count as f64 - f64::from(viewport_h) / f64::from(CODE_ROW_HEIGHT)
        );
        assert_eq!(rows, max_rows, "the position must stop at the new end");
    }

    fn frame(view: &Entity<CodeView>, cx: &mut VisualTestContext) {
        view.update(cx, |_, cx| cx.notify());
        cx.run_until_parked();
    }

    fn scroll_to(view: &Entity<CodeView>, cx: &mut VisualTestContext, rows: f64) {
        view.update(cx, |view, cx| {
            view.scroll.set_rows(rows);
            cx.notify();
        });
        cx.run_until_parked();
    }

    /// US-025: a frame whose rows are all in the layout cache builds no
    /// `String` at all, for the code or for the gutter.
    #[gpui::test]
    fn a_warm_frame_shapes_nothing_it_already_shaped(cx: &mut TestAppContext) {
        let text: String = (0..400)
            .map(|row| format!("row {row} of plain text\n"))
            .collect();
        let (view, cx) = scrolled(cx, "/nonexistent/warm.txt", &text);

        frame(&view, cx);
        assert_eq!(
            view.read_with(cx, |view, _| view.materialized_lines()),
            0,
            "a warm frame must not build a single line string"
        );
        assert_eq!(
            view.read_with(cx, |view, _| view.materialized_numbers()),
            0,
            "a warm frame must not build a single number string"
        );

        scroll_to(&view, cx, 200.0);
        frame(&view, cx);
        scroll_to(&view, cx, 0.0);
        assert!(
            view.read_with(cx, |view, _| view.materialized_lines()) > 0,
            "rows the layout cache dropped must be shaped again"
        );
    }

    #[gpui::test]
    fn an_edit_only_reshapes_the_row_it_touched(cx: &mut TestAppContext) {
        let rows = 100;
        let text: String = (0..rows)
            .map(|row| format!("row {row} of plain text\n"))
            .collect();
        let (view, cx) = scrolled(cx, "/nonexistent/edit.txt", &text);

        view.update(cx, |_, cx| cx.notify());
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |view, _| view.materialized_lines()), 0);

        view.update_in(cx, |view, window, cx| {
            view.selection = CodeSelection { anchor: 0, head: 0 };
            view.replace_text_in_range(None, "z", window, cx);
        });
        cx.run_until_parked();

        assert_eq!(
            view.read_with(cx, |view, _| view.materialized_lines()),
            1,
            "only the edited row may miss the layout cache"
        );
    }

    #[gpui::test]
    fn identical_rows_share_one_shaped_line(cx: &mut TestAppContext) {
        let text: String = (0..400)
            .map(|row| {
                if row % 2 == 0 {
                    "same line\n"
                } else {
                    "other line\n"
                }
            })
            .collect();
        let (view, cx) = scrolled(cx, "/nonexistent/twins.txt", &text);

        let (even, odd, twin) = view.read_with(cx, |view, _| {
            (view.row_width(0), view.row_width(1), view.row_width(2))
        });
        assert_eq!(even, twin, "identical rows must carry identical layouts");
        assert_ne!(even, odd, "the probe needs two measurably different texts");

        frame(&view, cx);
        scroll_to(&view, cx, 200.0);

        let visible = view.read_with(cx, |view, _| view.visible_row_range().len());
        assert!(visible > 4, "the probe needs more rows than distinct texts");
        assert_eq!(
            view.read_with(cx, |view, _| view.materialized_lines()),
            0,
            "{visible} rows of already shaped texts must all hit at their new indices"
        );
    }

    #[gpui::test]
    fn the_ime_reads_the_caret_from_the_painted_rows(cx: &mut TestAppContext) {
        let (view, cx) = scrolled(cx, "/nonexistent/ime.rs", "let alpha = 1;\nlet beta = 2;\n");

        let (caret, index) = view.update_in(cx, |view, window, cx| {
            let element_bounds = view.scroll.bounds();
            let caret = view
                .bounds_for_range(4..9, element_bounds, window, cx)
                .expect("a laid out row");
            let index = view
                .character_index_for_point(caret.origin, window, cx)
                .expect("a laid out row");
            (caret, index)
        });
        assert_eq!(index, 4, "the IME round trips the caret it was given");
        assert_eq!(f32::from(caret.size.height), CODE_ROW_HEIGHT);
        assert_eq!(
            f32::from(caret.origin.y),
            view.read_with(cx, |view, _| view.row_top(0)),
            "the IME caret sits on the painted row"
        );
    }

    // --------------------------------------------------------------- EP-004

    /// Build a view over a file that really exists, so the save and conflict
    /// paths have something to stat. Returns the temp dir, which has to outlive
    /// the view.
    ///
    /// `watch` stays off for every test that writes into the directory: a live
    /// OS watcher wakes the reload task from the notify thread, which the
    /// deterministic test scheduler rightly calls non-determinism. Under test
    /// the handle is a `NullWatcher`, so the registration test never starts
    /// FSEvents either.
    fn file_view<'a>(
        cx: &'a mut TestAppContext,
        text: &str,
        watch: bool,
    ) -> (
        tempfile::TempDir,
        Entity<CodeView>,
        &'a mut VisualTestContext,
    ) {
        file_view_named(cx, "main.rs", text, watch)
    }

    fn file_view_named<'a>(
        cx: &'a mut TestAppContext,
        name: &str,
        text: &str,
        watch: bool,
    ) -> (
        tempfile::TempDir,
        Entity<CodeView>,
        &'a mut VisualTestContext,
    ) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(name);
        std::fs::write(&path, text).expect("seed");
        let seeded = seeded_view(path, text);
        let (view, cx) = cx.add_window_view(move |_window, cx| {
            let mut view = seeded(cx);
            if watch {
                view.start_watcher(cx);
            }
            view
        });
        (dir, view, cx)
    }

    /// The constructor `file_view_named` hands to the window: a view over the
    /// file at `path`, already loaded with `text` and stamped.
    fn seeded_view(
        path: PathBuf,
        text: &str,
    ) -> impl FnOnce(&mut Context<CodeView>) -> CodeView + use<> {
        let document = build_document(path.clone(), text, false);
        let highlighter = CodeHighlighter::new(
            &document,
            DiffSyntax::from_theme(&crate::theme::paneflow_dark()),
        );
        let stamp = FileStamp::read(&path);
        let state = CodeLoadState::Ready(Box::new(LoadedCode {
            document,
            highlighter,
            indent: IndentUnit::Spaces(4),
            stamp,
        }));
        move |cx: &mut Context<CodeView>| {
            let focus = cx.focus_handle();
            let controls = EditorControls::attach(focus.clone(), cx);
            CodeView {
                controls,
                navigation: NavigationState::default(),
                element_id: "code-view:test".into(),
                path,
                state,
                slot: CodeLoadSlot::new(),
                focus,
                scroll: CodeScroll::new(),
                h_offset: 0.0,
                selection: CodeSelection::default(),
                goal_column: 0,
                text_drag: None,
                click_chain: None,
                last_motion: Instant::now(),
                blink_visible: true,
                theme_generation: 0,
                geometry: Rc::new(Cell::new(CodeGeometry::default())),
                gutter_memo: Rc::new(Cell::new(GutterMemo::default())),
                hits: Rc::new(RefCell::new(CodeHitMap::default())),
                history: edit::UndoHistory::default(),
                saved_mark: edit::HistoryMark::default(),
                indent: IndentUnit::Spaces(4),
                marked: None,
                read_only_flash: None,
                stamp,
                pending_stamp: None,
                disk: DiskState::default(),
                save_error: None,
                disk_generation: 0,
                saving: false,
                longest_line_scan_in_flight: false,
                longest_line_rescan_needed: false,
                _watcher: None,
                _watch_bridge: None,
                base: Base::None,
                tracker: BlockTracker::inactive(),
                tracker_generation: 0,
                base_generation: 0,
                hovered_marker: None,
                popup: None,
            }
        }
    }

    /// Current buffer text.
    fn text_of(view: &CodeView) -> String {
        view.document()
            .map(|doc| doc.slice_string(0..doc.len_bytes()))
            .unwrap_or_default()
    }

    /// US-012 AC: typing with a live selection replaces it, through the real
    /// platform text-input entry point rather than a helper.
    #[gpui::test]
    fn typing_replaces_the_live_selection(cx: &mut TestAppContext) {
        let (view, cx) = view(cx, "hello world\n");

        view.update_in(cx, |view, window, cx| {
            view.selection = CodeSelection { anchor: 0, head: 5 };
            view.replace_text_in_range(None, "bye", window, cx);
        });

        view.update(cx, |view, _cx| {
            assert_eq!(text_of(view), "bye world\n");
            assert_eq!(view.cursor(), 3, "the caret lands past what was inserted");
            assert!(view.is_dirty(), "an edit marks the document dirty");
        });
    }

    /// Issue #396: quitting must see a dock file tab's unsaved edits.
    /// `any_file_tab_dirty` is what `quit_after_session_save` consults before
    /// `cx.quit()`, so this exercises it against a real, edited `CodeView`
    /// rather than a stand-in boolean.
    #[gpui::test]
    fn a_dirty_code_view_is_reported_by_the_dock_file_dirty_check(cx: &mut TestAppContext) {
        use crate::app::cli_diff_dock::any_file_tab_dirty;
        use crate::app::diff_dock::DiffDockTab;

        let (view, cx) = view(cx, "hello world\n");
        let tab = DiffDockTab::File(view.clone());

        cx.cx.update(|cx| {
            assert!(
                !any_file_tab_dirty([&tab], cx),
                "a freshly opened file tab is clean"
            );
        });

        view.update_in(cx, |view, window, cx| {
            view.selection = CodeSelection { anchor: 0, head: 5 };
            view.replace_text_in_range(None, "bye", window, cx);
        });

        cx.cx.update(|cx| {
            assert!(
                any_file_tab_dirty([&tab], cx),
                "an edited buffer must not be silently discarded"
            );
        });
    }

    /// US-012 AC: Backspace removes a full grapheme, so a composed emoji goes in
    /// one press instead of shedding its skin-tone modifier first.
    #[gpui::test]
    fn backspace_removes_a_whole_composed_emoji(cx: &mut TestAppContext) {
        let emoji = "\u{1F44D}\u{1F3FD}";
        let (view, cx) = view(cx, &format!("ok{emoji}\n"));
        let end = 2 + emoji.len();

        view.update_in(cx, |view, window, cx| {
            view.selection = CodeSelection::at(end);
            view.backspace(&CeBackspace, window, cx);
        });

        view.update(cx, |view, _cx| {
            assert_eq!(
                text_of(view),
                "ok\n",
                "the whole grapheme went in one press"
            );
        });
    }

    /// US-012 AC: Enter repeats the row's indentation.
    #[gpui::test]
    fn enter_repeats_the_row_indentation(cx: &mut TestAppContext) {
        let (view, cx) = view(cx, "fn main() {\n    let x = 1;\n}\n");
        // End of the indented row.
        let at = "fn main() {\n    let x = 1;".len();

        view.update_in(cx, |view, window, cx| {
            view.selection = CodeSelection::at(at);
            view.newline(&CeNewline, window, cx);
        });

        view.update(cx, |view, _cx| {
            assert_eq!(text_of(view), "fn main() {\n    let x = 1;\n    \n}\n");
            assert_eq!(view.cursor(), at + 5, "the caret sits past the new indent");
        });
    }

    /// US-012 AC: a keystroke on a read-only document mutates nothing and says
    /// so. The refusal has to be visible, which is why the input handler is left
    /// enabled and the keystroke is turned down here rather than by the
    /// platform.
    #[gpui::test]
    fn a_keystroke_on_a_read_only_document_is_refused_visibly(cx: &mut TestAppContext) {
        let path = PathBuf::from("/nonexistent/paneflow-code.rs");
        let document = build_document(path.clone(), "locked\n", true);
        let highlighter = CodeHighlighter::new(
            &document,
            DiffSyntax::from_theme(&crate::theme::paneflow_dark()),
        );
        let state = CodeLoadState::Ready(Box::new(LoadedCode {
            document,
            highlighter,
            indent: IndentUnit::Spaces(4),
            stamp: None,
        }));
        let (view, cx) = cx.add_window_view(move |_window, cx| {
            let focus = cx.focus_handle();
            let controls = EditorControls::attach(focus.clone(), cx);
            CodeView {
                controls,
                navigation: NavigationState::default(),
                element_id: "code-view:test".into(),
                path,
                state,
                slot: CodeLoadSlot::new(),
                focus,
                scroll: CodeScroll::new(),
                h_offset: 0.0,
                selection: CodeSelection::default(),
                goal_column: 0,
                text_drag: None,
                click_chain: None,
                last_motion: Instant::now(),
                blink_visible: true,
                theme_generation: 0,
                geometry: Rc::new(Cell::new(CodeGeometry::default())),
                gutter_memo: Rc::new(Cell::new(GutterMemo::default())),
                hits: Rc::new(RefCell::new(CodeHitMap::default())),
                history: edit::UndoHistory::default(),
                saved_mark: edit::HistoryMark::default(),
                indent: IndentUnit::Spaces(4),
                marked: None,
                read_only_flash: None,
                stamp: None,
                pending_stamp: None,
                disk: DiskState::default(),
                save_error: None,
                disk_generation: 0,
                saving: false,
                longest_line_scan_in_flight: false,
                longest_line_rescan_needed: false,
                _watcher: None,
                _watch_bridge: None,
                base: Base::None,
                tracker: BlockTracker::inactive(),
                tracker_generation: 0,
                base_generation: 0,
                hovered_marker: None,
                popup: None,
            }
        });

        view.update_in(cx, |view, window, cx| {
            view.replace_text_in_range(None, "x", window, cx);
        });

        view.update(cx, |view, _cx| {
            assert_eq!(text_of(view), "locked\n", "nothing was written");
            assert!(
                !view.is_dirty(),
                "a refused keystroke leaves no transaction"
            );
            assert!(
                view.read_only_flash.is_some(),
                "the refusal lights the banner up"
            );
        });
    }

    /// US-012 and US-016 AC: a read-only file an agent rewrote reloads
    /// silently, which leaves a transaction in the history. `Ctrl+Z` must be
    /// refused like any other edit on that document - replaying it moves the
    /// caret and the dirty mark while the rope stays put, so a file that
    /// matches disk exactly starts claiming it is modified.
    #[gpui::test]
    fn undo_on_a_read_only_document_is_refused_visibly(cx: &mut TestAppContext) {
        let (dir, view, cx) = file_view(cx, "one\ntwo\n", false);
        let path = dir.path().join("main.rs");

        view.update(cx, |view, _cx| {
            view.state
                .document_mut()
                .expect("document")
                .set_read_only(Some(ReadOnlyReason::Permissions));
        });

        std::fs::write(&path, "one\ntwo\nthree\n").expect("external write");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444)).unwrap();
        let loaded = super::super::load::load_stamped(&path);
        view.update(cx, |view, cx| {
            let generation = view.begin_disk_probe();
            view.disk_loaded(generation, loaded, false, cx);
            assert_eq!(text_of(view), "one\ntwo\nthree\n", "the reload landed");
            assert!(
                !view.is_dirty(),
                "a silent reload leaves the document clean"
            );
        });

        view.update_in(cx, |view, window, cx| {
            view.undo(&CeUndo, window, cx);
        });

        view.update(cx, |view, _cx| {
            assert_eq!(text_of(view), "one\ntwo\nthree\n", "nothing was replayed");
            assert!(!view.is_dirty(), "and the document is still clean");
            assert!(view.read_only_flash.is_some(), "the refusal is visible");
        });
    }

    /// US-013 AC: an undo feeds `Tree::edit` in reverse, so the coloring that
    /// survives a `Ctrl+Z` is the coloring a fresh parse of the same text
    /// produces. The oracle is that fresh parse, compared row by row against
    /// the tree the view kept editing incrementally.
    #[gpui::test]
    fn undo_keeps_the_highlighting_a_fresh_parse_would_give(cx: &mut TestAppContext) {
        let (view, cx) = view(cx, "fn main() {\n    let value = 1;\n}\n");

        view.update_in(cx, |view, window, cx| {
            view.selection = CodeSelection::at(16);
            view.replace_text_in_range(None, "xyz", window, cx);
            view.undo(&CeUndo, window, cx);
        });
        // An edit whose reparse overran the 1 ms budget finishes off-thread,
        // and on a loaded runner even three lines of Rust can. Park first so
        // the comparison below reads a settled highlighter either way.
        cx.run_until_parked();

        view.update(cx, |view, _cx| {
            let doc = view.document().expect("document");
            let oracle =
                CodeHighlighter::new(doc, DiffSyntax::from_theme(&crate::theme::paneflow_dark()));
            let live = view.highlighter().expect("highlighter");
            assert!(live.is_enabled(), "the grammar is loaded");
            assert!(
                !oracle.runs(1).is_empty(),
                "the oracle colors something, so the comparison means something"
            );
            for row in 0..doc.line_count() {
                assert_eq!(
                    live.runs(row),
                    oracle.runs(row),
                    "row {row} kept its coloring across the undo"
                );
            }
        });
    }

    /// US-013 AC: consecutive keystrokes undo as one transaction, and a caret
    /// move closes the group so what follows undoes on its own.
    #[gpui::test]
    fn keystrokes_group_until_the_caret_moves(cx: &mut TestAppContext) {
        let (view, cx) = view(cx, "\n");

        view.update_in(cx, |view, window, cx| {
            for letter in ["a", "b", "c"] {
                view.replace_text_in_range(None, letter, window, cx);
            }
            view.left(&CeLeft, window, cx);
            view.right(&CeRight, window, cx);
            view.replace_text_in_range(None, "d", window, cx);
            assert_eq!(text_of(view), "abcd\n");

            view.undo(&CeUndo, window, cx);
            assert_eq!(
                text_of(view),
                "abc\n",
                "the post-move keystroke undid alone"
            );
            view.undo(&CeUndo, window, cx);
            assert_eq!(
                text_of(view),
                "\n",
                "the three grouped keystrokes undid together"
            );

            view.redo(&CeRedo, window, cx);
            assert_eq!(text_of(view), "abc\n", "redo replays the same grouping");
        });
    }

    /// US-013 AC: undo restores the caret and the selection the edit started
    /// from, not merely the text.
    #[gpui::test]
    fn undo_restores_the_selection_the_edit_replaced(cx: &mut TestAppContext) {
        let (view, cx) = view(cx, "hello world\n");

        view.update_in(cx, |view, window, cx| {
            view.selection = CodeSelection {
                anchor: 6,
                head: 11,
            };
            view.replace_text_in_range(None, "there", window, cx);
            view.undo(&CeUndo, window, cx);
        });

        view.update(cx, |view, _cx| {
            assert_eq!(text_of(view), "hello world\n");
            assert_eq!(view.selection(), 6..11, "the replaced selection came back");
        });
    }

    /// US-013 and US-014 AC: a multi-line paste is one transaction and one
    /// Ctrl+Z, and the caret ends at the end of what was inserted.
    #[gpui::test]
    fn a_multi_line_paste_is_one_undo_step(cx: &mut TestAppContext) {
        let (view, cx) = view(cx, "start\n");

        view.update_in(cx, |view, window, cx| {
            view.selection = CodeSelection::at(6);
            cx.write_to_clipboard(ClipboardItem::new_string("one\r\ntwo\r\nthree".to_string()));
            view.paste_action(&CePaste, window, cx);
            assert_eq!(text_of(view), "start\none\ntwo\nthree");
            assert_eq!(
                view.cursor(),
                text_of(view).len(),
                "the caret is at the end"
            );

            view.undo(&CeUndo, window, cx);
            assert_eq!(text_of(view), "start\n", "the whole paste undid at once");
        });
    }

    /// US-014 AC: a paste carrying control characters and a bidi override is
    /// neutralized before it reaches the rope.
    #[gpui::test]
    fn a_paste_is_sanitized_before_insertion(cx: &mut TestAppContext) {
        let (view, cx) = view(cx, "\n");

        view.update_in(cx, |view, window, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string(
                "let x = 1;\u{202E}\u{0007}\u{200B}".to_string(),
            ));
            view.paste_action(&CePaste, window, cx);
        });

        view.update(cx, |view, _cx| {
            assert_eq!(text_of(view), "let x = 1;\n");
        });
    }

    /// US-014 AC: Copy with no selection takes the whole row, newline included,
    /// so pasting it back lands a complete line.
    #[gpui::test]
    fn copy_with_no_selection_takes_the_whole_row(cx: &mut TestAppContext) {
        let (view, cx) = view(cx, "first\nsecond\n");

        view.update_in(cx, |view, window, cx| {
            view.selection = CodeSelection::at(8);
            view.copy(&CeCopy, window, cx);
            let clipped = cx
                .read_from_clipboard()
                .and_then(|item| item.text())
                .unwrap_or_default();
            assert_eq!(clipped, "second\n");
            assert_eq!(text_of(view), "first\nsecond\n", "copy never mutates");
        });
    }

    /// US-014 AC: Tab indents every row a multi-line selection touches, and
    /// Shift+Tab takes exactly one level back off without ever eating a
    /// non-blank character.
    #[gpui::test]
    fn tab_and_shift_tab_shift_every_touched_row(cx: &mut TestAppContext) {
        let (view, cx) = view(cx, "one\ntwo\nthree\n");

        view.update_in(cx, |view, window, cx| {
            view.selection = CodeSelection {
                anchor: 0,
                head: 8, // into row two
            };
            view.indent(&CeIndent, window, cx);
            assert_eq!(text_of(view), "    one\n    two\nthree\n");

            view.outdent(&CeOutdent, window, cx);
            assert_eq!(text_of(view), "one\ntwo\nthree\n");

            // Nothing left to take: the row's own characters are safe.
            view.outdent(&CeOutdent, window, cx);
            assert_eq!(text_of(view), "one\ntwo\nthree\n");
        });
    }

    /// US-015 AC: Ctrl+S writes the file and clears the dirty mark, and undoing
    /// back to the saved state leaves it clear.
    #[gpui::test]
    fn saving_writes_the_file_and_settles_the_dirty_mark(cx: &mut TestAppContext) {
        let (dir, view, cx) = file_view(cx, "one\n", false);
        let path = dir.path().join("main.rs");

        view.update_in(cx, |view, window, cx| {
            view.selection = CodeSelection::at(4);
            view.replace_text_in_range(None, "two\n", window, cx);
            assert!(view.is_dirty());
            view.save_action(&CeSave, window, cx);
        });
        cx.executor().allow_parking();
        cx.run_until_parked();

        assert_eq!(std::fs::read_to_string(&path).expect("read"), "one\ntwo\n");
        view.update_in(cx, |view, window, cx| {
            assert!(!view.is_dirty(), "a landed save clears the dot");
            assert!(view.save_error.is_none());

            view.replace_text_in_range(None, "x", window, cx);
            assert!(view.is_dirty());
            view.undo(&CeUndo, window, cx);
            assert!(
                !view.is_dirty(),
                "undoing back to the saved state clears the dot again"
            );
        });
    }

    #[gpui::test]
    fn disk_changed_ignores_stale_pre_save_probe(cx: &mut TestAppContext) {
        let (dir, view, cx) = file_view(cx, "v1\n", false);
        let path = dir.path().join("main.rs");
        let stale_stamp = FileStamp::read(&path);
        let stale_generation = view.update(cx, |view, _cx| view.begin_disk_probe());

        view.update_in(cx, |view, window, cx| {
            view.select_all(&CeSelectAll, window, cx);
            view.replace_text_in_range(None, "v2\n", window, cx);
            view.save_action(&CeSave, window, cx);
        });
        cx.executor().allow_parking();
        cx.run_until_parked();

        view.update(cx, |view, cx| {
            view.disk_changed(stale_generation, stale_stamp, Some("v1\n".to_string()), cx);
            assert_eq!(text_of(view), "v2\n", "a pre-save probe is stale");
            assert_eq!(view.disk, DiskState::InSync);
            assert_eq!(view.stamp, FileStamp::read(&path));
        });
    }

    /// US-016 AC: a save is refused *before* writing when the file changed in
    /// the meantime, and the on-disk bytes are untouched.
    #[gpui::test]
    fn a_save_is_refused_when_the_file_changed_underneath(cx: &mut TestAppContext) {
        let (dir, view, cx) = file_view(cx, "one\n", false);
        let path = dir.path().join("main.rs");

        view.update_in(cx, |view, window, cx| {
            view.selection = CodeSelection::at(4);
            view.replace_text_in_range(None, "mine\n", window, cx);
        });
        // An agent gets there first. The stamp carries a length change, so this
        // does not depend on the filesystem's timestamp granularity.
        std::fs::write(&path, "written by someone else\n").expect("agent write");

        view.update_in(cx, |view, window, cx| {
            view.save_action(&CeSave, window, cx);
        });
        cx.executor().allow_parking();
        cx.run_until_parked();

        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "written by someone else\n",
            "the refusal happened before the write"
        );
        view.update(cx, |view, _cx| {
            assert!(view.has_conflict(), "the user is asked to choose");
            assert!(view.is_dirty(), "the in-memory edits survived");
            assert_eq!(text_of(view), "one\nmine\n");
        });
    }

    /// A rewrite that lands after the off-thread read returned but before the
    /// outcome is applied on the main thread must still be caught: the stamp
    /// the view adopts is the loader's, of the bytes it read, so the next save
    /// refuses and the rewrite stays on disk.
    #[gpui::test]
    fn a_rewrite_between_the_read_and_its_landing_still_conflicts(cx: &mut TestAppContext) {
        let (dir, view, cx) = file_view(cx, "one\n", false);
        let path = dir.path().join("main.rs");
        let outcome = open_blocking(
            &path,
            DiffSyntax::from_theme(&crate::theme::paneflow_dark()),
        );
        // An agent gets there between the read and the apply callback. The
        // length changes, so this does not depend on timestamp granularity.
        std::fs::write(&path, "written by someone else\n").expect("agent write");

        view.update(cx, |view, cx| {
            let generation = view.slot.begin();
            view.apply_load(generation, outcome, cx);
            assert_eq!(text_of(view), "one\n", "the buffer holds what was read");
        });

        view.update_in(cx, |view, window, cx| {
            view.selection = CodeSelection::at(4);
            view.replace_text_in_range(None, "mine\n", window, cx);
            view.save_action(&CeSave, window, cx);
        });
        cx.executor().allow_parking();
        cx.run_until_parked();

        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "written by someone else\n",
            "the agent's rewrite was not clobbered"
        );
        view.update(cx, |view, _cx| {
            assert_eq!(view.disk, DiskState::Conflict);
            assert!(view.is_dirty(), "the in-memory edits survived");
            assert_eq!(text_of(view), "one\nmine\n");
        });
    }

    /// US-016 AC: an external write to a clean document reloads silently,
    /// keeping the scroll and the caret, and the reload is undoable.
    #[gpui::test]
    fn an_external_write_reloads_a_clean_document(cx: &mut TestAppContext) {
        let (dir, view, cx) = file_view(cx, "one\ntwo\n", false);
        let path = dir.path().join("main.rs");
        // The replacement changes the length on purpose. Windows stamps a
        // file's last-write time on the system timer tick (~15 ms), so a
        // same-length rewrite this soon after the load can carry a stamp
        // identical to the one the load recorded and read as "no change".
        std::fs::write(&path, "ONE!\nTWO!\n").expect("agent write");
        let stamp = FileStamp::read(&path);

        view.update(cx, |view, cx| {
            view.selection = CodeSelection::at(4);
            let generation = view.begin_disk_probe();
            view.disk_changed(generation, stamp, Some("ONE!\nTWO!\n".to_string()), cx);
        });

        view.update_in(cx, |view, window, cx| {
            assert_eq!(text_of(view), "ONE!\nTWO!\n");
            assert!(!view.has_conflict(), "a clean document reloads silently");
            assert!(!view.is_dirty(), "the reload is the new saved state");
            assert_eq!(
                view.cursor(),
                4,
                "the caret held: the line count is unchanged"
            );

            view.undo(&CeUndo, window, cx);
            assert_eq!(
                text_of(view),
                "one\ntwo\n",
                "Ctrl+Z recovers what was replaced"
            );
        });
    }

    /// EP-004: an agent's rewrite of a few lines is a few-line transaction. A
    /// caret far from every hunk keeps its place, and one Ctrl+Z reverts the
    /// whole reload.
    #[gpui::test]
    fn an_external_write_keeps_a_distant_caret_and_undoes_all_hunks_once(cx: &mut TestAppContext) {
        let original = (0..30)
            .map(|row| format!("line {row:03}\n"))
            .collect::<String>();
        let mut incoming_lines = original.lines().map(str::to_string).collect::<Vec<_>>();
        for (row, line) in incoming_lines.iter_mut().enumerate().take(8).skip(5) {
            *line = format!("agent changed line {row:03}");
        }
        incoming_lines[15] = "second distant hunk".to_string();
        let incoming = incoming_lines.join("\n") + "\n";
        let (dir, view, cx) = file_view_named(cx, "main.txt", &original, false);
        let path = dir.path().join("main.txt");
        std::fs::write(&path, &incoming).expect("agent write");
        let stamp = FileStamp::read(&path);
        let caret = original.find("line 025").expect("caret line") + 5;
        let expected = incoming.find("line 025").expect("shifted caret line") + 5;

        view.update(cx, |view, cx| {
            view.selection = CodeSelection::at(caret);
            let generation = view.begin_disk_probe();
            view.disk_changed(generation, stamp, Some(incoming.clone()), cx);
            assert_eq!(view.cursor(), expected);
            assert_eq!(text_of(view), incoming);
        });

        view.update_in(cx, |view, window, cx| {
            view.undo(&CeUndo, window, cx);
            assert_eq!(text_of(view), original, "every hunk shares one transaction");
            assert_eq!(view.cursor(), caret);
        });
    }

    #[gpui::test]
    fn an_identical_external_reload_pushes_no_transaction(cx: &mut TestAppContext) {
        let (_dir, view, cx) = file_view(cx, "one\ntwo\n", false);
        view.update(cx, |view, cx| {
            let before = view.history.mark();
            view.adopt_disk_text("one\r\ntwo\r\n", cx);
            assert_eq!(view.history.mark(), before);
            assert_eq!(text_of(view), "one\ntwo\n");
        });
    }

    #[gpui::test]
    fn a_crlf_reload_preserves_the_document_line_ending(cx: &mut TestAppContext) {
        let (_dir, view, cx) = file_view(cx, "one\r\ntwo\r\n", false);
        view.update(cx, |view, cx| {
            view.adopt_disk_text("one\r\nTWO\r\n", cx);
            let doc = view.document().expect("document");
            assert_eq!(doc.to_disk_string(), "one\r\nTWO\r\n");
        });
    }

    #[gpui::test]
    fn a_read_only_reload_temporarily_unlocks_and_restores_the_document(cx: &mut TestAppContext) {
        let (_dir, view, cx) = file_view(cx, "old\n", false);
        view.update(cx, |view, cx| {
            view.state
                .document_mut()
                .expect("document")
                .set_read_only(Some(ReadOnlyReason::Permissions));
            view.adopt_disk_text("new content\n", cx);
            assert_eq!(text_of(view), "new content\n");
            assert_eq!(
                view.document().and_then(CodeDocument::read_only_reason),
                Some(ReadOnlyReason::Permissions)
            );
        });
    }

    #[gpui::test]
    fn a_whole_document_reload_remeasures_the_longest_line(cx: &mut TestAppContext) {
        let (_dir, view, cx) = file_view(cx, "this line starts longest\nx\n", false);
        view.update(cx, |view, cx| {
            view.adopt_disk_text("a\na much longer replacement line\n", cx);
        });
        cx.executor().allow_parking();
        cx.run_until_parked();
        view.update(cx, |view, _cx| {
            assert_eq!(
                view.document().expect("document").longest_line_chars(),
                "a much longer replacement line".len()
            );
        });
    }

    // --------------------------------------------------------------- EP-009

    /// A probe of `text` for the view's own file, built the way the loader
    /// builds one.
    fn probe(view: &CodeView, text: &str, stamp: Option<FileStamp>) -> DiskLoad {
        Ok((
            build_document(view.path().to_path_buf(), text, false),
            stamp,
        ))
    }

    #[gpui::test]
    fn a_reload_that_races_an_edit_recomputes_once_then_conflicts(cx: &mut TestAppContext) {
        let (dir, view, cx) = file_view(cx, "one\ntwo\n", false);
        let path = dir.path().join("main.rs");
        std::fs::write(&path, "ONE!\nTWO!\n").expect("agent write");
        let stamp = FileStamp::read(&path);

        view.update_in(cx, |view, window, cx| {
            let generation = view.begin_disk_probe();
            let loaded = probe(view, "ONE!\nTWO!\n", stamp);
            let (diff, incoming) = view
                .begin_disk_reload(generation, loaded, false, cx)
                .expect("a clean document starts a diff");
            let splices = edit::disk_splices(&diff.rope, "ONE!\nTWO!\n");
            view.selection = CodeSelection::at(0);
            view.replace_text_in_range(None, "x", window, cx);

            let again = view
                .finish_disk_reload(
                    generation,
                    DiskSplices {
                        revision: diff.revision,
                        splices,
                    },
                    &incoming,
                    true,
                    false,
                    cx,
                )
                .expect("a stale revision buys exactly one recomputation");
            assert_eq!(
                text_of(view),
                "xone\ntwo\n",
                "splices computed against an older revision are refused"
            );
            assert!(!view.has_conflict(), "the first miss is not a conflict yet");

            let splices = edit::disk_splices(&again.rope, "ONE!\nTWO!\n");
            view.replace_text_in_range(None, "y", window, cx);
            assert!(
                view.finish_disk_reload(
                    generation,
                    DiskSplices {
                        revision: again.revision,
                        splices
                    },
                    &incoming,
                    false,
                    false,
                    cx
                )
                .is_none(),
                "the second stale delivery gives up"
            );
            assert!(
                view.has_conflict(),
                "a document that keeps moving is left to the user"
            );
            assert_eq!(text_of(view), "xyone\ntwo\n", "the user edits survived");
        });
    }

    #[gpui::test]
    fn a_reload_that_settles_on_a_dirty_document_keeps_the_user_text(cx: &mut TestAppContext) {
        let (dir, view, cx) = file_view(cx, "one\ntwo\n", false);
        let path = dir.path().join("main.rs");
        std::fs::write(&path, "ONE!\nTWO!\n").expect("agent write");
        let stamp = FileStamp::read(&path);

        view.update_in(cx, |view, window, cx| {
            let generation = view.begin_disk_probe();
            let loaded = probe(view, "ONE!\nTWO!\n", stamp);
            let (diff, incoming) = view
                .begin_disk_reload(generation, loaded, false, cx)
                .expect("a clean document starts a diff");
            let splices = edit::disk_splices(&diff.rope, "ONE!\nTWO!\n");
            view.selection = CodeSelection::at(0);
            view.replace_text_in_range(None, "x", window, cx);

            let again = view
                .finish_disk_reload(
                    generation,
                    DiskSplices {
                        revision: diff.revision,
                        splices,
                    },
                    &incoming,
                    true,
                    false,
                    cx,
                )
                .expect("a stale revision buys exactly one recomputation");
            let splices = edit::disk_splices(&again.rope, "ONE!\nTWO!\n");
            assert!(
                view.finish_disk_reload(
                    generation,
                    DiskSplices {
                        revision: again.revision,
                        splices
                    },
                    &incoming,
                    false,
                    false,
                    cx
                )
                .is_none(),
                "the recomputed diff reaches a document that stopped moving"
            );
            assert_eq!(
                text_of(view),
                "xone\ntwo\n",
                "a document the user touched during the diff is never overwritten"
            );
            assert!(view.is_dirty(), "the unsaved edit is still unsaved");
            assert!(
                view.has_conflict(),
                "the user resolves it like any other conflict"
            );
        });
    }

    #[gpui::test]
    fn a_forced_reload_still_overwrites_the_document_the_user_edited(cx: &mut TestAppContext) {
        let (dir, view, cx) = file_view(cx, "one\ntwo\n", false);
        let path = dir.path().join("main.rs");
        std::fs::write(&path, "ONE!\nTWO!\n").expect("agent write");
        let stamp = FileStamp::read(&path);

        view.update_in(cx, |view, window, cx| {
            view.selection = CodeSelection::at(0);
            view.replace_text_in_range(None, "x", window, cx);
            assert!(view.is_dirty(), "the fixture starts dirty");

            let generation = view.begin_disk_probe();
            let loaded = probe(view, "ONE!\nTWO!\n", stamp);
            let (diff, incoming) = view
                .begin_disk_reload(generation, loaded, true, cx)
                .expect("a forced reload ignores the dirty mark");
            let splices = edit::disk_splices(&diff.rope, "ONE!\nTWO!\n");
            assert!(
                view.finish_disk_reload(
                    generation,
                    DiskSplices {
                        revision: diff.revision,
                        splices
                    },
                    &incoming,
                    false,
                    true,
                    cx
                )
                .is_none()
            );
            assert_eq!(
                text_of(view),
                "ONE!\nTWO!\n",
                "discarding my changes is what the user asked for"
            );
            assert!(!view.is_dirty(), "and the reload is the new saved state");
            assert!(!view.has_conflict());
        });
    }

    /// A probe that lands after a save began is stale even once the diff has
    /// run: the save's generation wins, and the splices are dropped.
    #[gpui::test]
    fn a_diff_that_outlives_its_probe_generation_is_dropped(cx: &mut TestAppContext) {
        let (dir, view, cx) = file_view(cx, "one\ntwo\n", false);
        let path = dir.path().join("main.rs");
        std::fs::write(&path, "ONE!\nTWO!\n").expect("agent write");
        let stamp = FileStamp::read(&path);

        view.update(cx, |view, cx| {
            let generation = view.begin_disk_probe();
            let loaded = probe(view, "ONE!\nTWO!\n", stamp);
            let (diff, incoming) = view
                .begin_disk_reload(generation, loaded, false, cx)
                .expect("a clean document starts a diff");
            let splices = edit::disk_splices(&diff.rope, "ONE!\nTWO!\n");
            let _newer = view.begin_disk_probe();
            assert!(
                view.finish_disk_reload(
                    generation,
                    DiskSplices {
                        revision: diff.revision,
                        splices
                    },
                    &incoming,
                    true,
                    false,
                    cx
                )
                .is_none(),
                "a superseded probe never asks for a recomputation"
            );
            assert_eq!(text_of(view), "one\ntwo\n", "its splices never land");
            assert!(!view.has_conflict());
        });
    }

    /// Adopting the agent's stamp in `begin_disk_reload` lets Cmd+S copy it as
    /// `expected`, so `save_blocking` sees a match and overwrites the rewrite
    /// with the old buffer. The stamp must stay the pre-agent one until splices
    /// land (#402, #428).
    #[gpui::test]
    fn a_save_during_an_in_flight_reload_does_not_overwrite_the_agent_bytes(
        cx: &mut TestAppContext,
    ) {
        let (dir, view, cx) = file_view(cx, "one\ntwo\n", false);
        let path = dir.path().join("main.rs");
        let original_stamp = view.update(cx, |view, _cx| view.stamp);

        std::fs::write(&path, "ONE!\nTWO!\n").expect("agent write");
        let stamp = FileStamp::read(&path);
        assert_ne!(
            stamp, original_stamp,
            "the agent rewrite must change the stamp"
        );

        view.update_in(cx, |view, window, cx| {
            let generation = view.begin_disk_probe();
            let loaded = probe(view, "ONE!\nTWO!\n", stamp);
            let (_diff, _incoming) = view
                .begin_disk_reload(generation, loaded, false, cx)
                .expect("a clean document starts a diff");
            assert_eq!(view.pending_stamp, stamp);
            assert_eq!(
                view.stamp, original_stamp,
                "the incoming stamp is not adopted until splices land"
            );
            assert_eq!(
                text_of(view),
                "one\ntwo\n",
                "the buffer is still the old text"
            );

            view.selection = CodeSelection::at(0);
            view.replace_text_in_range(None, "x", window, cx);
            view.save_action(&CeSave, window, cx);
        });
        cx.executor().allow_parking();
        cx.run_until_parked();

        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "ONE!\nTWO!\n",
            "the agent's rewrite must still be on disk"
        );
        view.update(cx, |view, _cx| {
            assert!(
                view.has_conflict(),
                "the save must refuse rather than clobber"
            );
            assert_eq!(text_of(view), "xone\ntwo\n");
            assert_eq!(
                view.stamp, original_stamp,
                "a refused save must not adopt the agent's stamp"
            );
        });
    }

    /// A rewrite between load and watcher registration never produces an
    /// event. The post-registration re-stat must still fold it in.
    #[gpui::test]
    fn a_rewrite_before_the_watcher_registers_is_still_folded_in(cx: &mut TestAppContext) {
        let (dir, view, cx) = file_view(cx, "one\ntwo\n", false);
        let path = dir.path().join("main.rs");
        std::fs::write(&path, "ONE!\nTWO!\n").expect("agent write");

        view.update(cx, |view, cx| {
            view.start_watcher(cx);
        });
        cx.executor().allow_parking();
        cx.run_until_parked();

        view.update(cx, |view, _cx| {
            assert_eq!(
                text_of(view),
                "ONE!\nTWO!\n",
                "the gap between load and watch must not leave stale bytes"
            );
            assert!(!view.has_conflict());
        });
    }

    #[gpui::test]
    async fn a_reload_whose_tab_closed_ends_without_a_panic(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("main.rs");
        std::fs::write(&path, "one\ntwo\n").expect("seed");
        let stamp = FileStamp::read(&path);
        let loaded: DiskLoad = Ok((build_document(path.clone(), "one\ntwo\n", false), stamp));
        let seeded = seeded_view(path, "one\n");
        let weak = cx.update(|cx| {
            let view = cx.new(seeded);
            let weak = view.downgrade();
            drop(view);
            weak
        });
        cx.run_until_parked();
        assert!(weak.upgrade().is_none(), "the tab is gone");

        let carried = cx
            .spawn(|mut cx| async move { reload_from_disk(&weak, &mut cx, 1, loaded, false).await })
            .await;
        assert!(
            !carried,
            "a reload delivered to a closed tab stops its loop instead of panicking"
        );
    }

    #[gpui::test]
    fn a_multi_hunk_reload_reaches_the_highlighter_as_one_batch(cx: &mut TestAppContext) {
        let original = (0..40)
            .map(|row| format!("fn f{row:03}() {{}}\n"))
            .collect::<String>();
        let mut lines = original.lines().map(str::to_string).collect::<Vec<_>>();
        lines[5] = "fn agent_a() {}".to_string();
        lines[25] = "fn agent_b() {}".to_string();
        let incoming = lines.join("\n") + "\n";
        let (dir, view, cx) = file_view(cx, &original, false);
        let path = dir.path().join("main.rs");
        std::fs::write(&path, &incoming).expect("agent write");
        let stamp = FileStamp::read(&path);

        let before = view.update(cx, |view, _cx| {
            let highlighter = view.highlighter().expect("highlighter");
            assert!(highlighter.is_enabled(), "the fixture must be colored");
            highlighter.generation()
        });

        view.update(cx, |view, cx| {
            let generation = view.begin_disk_probe();
            view.disk_changed(generation, stamp, Some(incoming.clone()), cx);
            assert_eq!(text_of(view), incoming, "both hunks landed");
            assert_eq!(
                view.highlighter().expect("highlighter").generation(),
                before + 1,
                "two hunks reach the highlighter as a single batched edit"
            );
        });
    }

    /// A rewrite that only changes the line endings (a git checkout or a
    /// formatter) must be adopted as the file's encoding too, or the next
    /// Ctrl+S silently puts the old terminators back.
    #[gpui::test]
    fn a_clean_reload_adopts_the_new_line_ending(cx: &mut TestAppContext) {
        let (dir, view, cx) = file_view(cx, "one\ntwo\n", false);
        let path = dir.path().join("main.rs");
        std::fs::write(&path, "one\r\ntwo\r\n").expect("agent write");
        let stamp = FileStamp::read(&path);

        view.update(cx, |view, cx| {
            let generation = view.begin_disk_probe();
            view.disk_changed(generation, stamp, Some("one\r\ntwo\r\n".to_string()), cx);
            assert_eq!(text_of(view), "one\ntwo\n", "the rope stays LF");
            let doc = view.document().expect("document");
            assert_eq!(doc.line_ending(), LineEnding::Crlf);
            assert_eq!(
                doc.to_disk_string(),
                "one\r\ntwo\r\n",
                "a save writes the terminators the disk now uses"
            );
        });
    }

    /// US-016 AC: the same write against a dirty document raises the banner and
    /// overwrites nothing.
    #[gpui::test]
    fn an_external_write_on_a_dirty_document_raises_a_conflict(cx: &mut TestAppContext) {
        let (dir, view, cx) = file_view(cx, "one\n", false);
        let path = dir.path().join("main.rs");

        view.update_in(cx, |view, window, cx| {
            view.selection = CodeSelection::at(4);
            view.replace_text_in_range(None, "mine\n", window, cx);
        });
        std::fs::write(&path, "theirs\n").expect("agent write");
        let stamp = FileStamp::read(&path);

        view.update(cx, |view, cx| {
            let generation = view.begin_disk_probe();
            view.disk_changed(generation, stamp, Some("theirs\n".to_string()), cx);
            assert!(view.has_conflict());
            assert_eq!(text_of(view), "one\nmine\n", "the buffer was not touched");
        });

        // "Keep mine" adopts the on-disk stamp, so the next save deliberately
        // wins instead of looping on the same refusal.
        view.update(cx, |view, cx| view.resolve_keep_mine(cx));
        cx.executor().allow_parking();
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            assert!(!view.has_conflict());
            view.save_action(&CeSave, window, cx);
        });
        cx.run_until_parked();
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "one\nmine\n");
    }

    /// US-016 AC: a file deleted on disk is flagged, and saving recreates it.
    #[gpui::test]
    fn a_deleted_file_is_flagged_and_saving_recreates_it(cx: &mut TestAppContext) {
        let (dir, view, cx) = file_view(cx, "one\n", false);
        let path = dir.path().join("main.rs");
        std::fs::remove_file(&path).expect("delete");

        view.update(cx, |view, cx| {
            let generation = view.begin_disk_probe();
            view.disk_changed(generation, None, None, cx);
            view.stamp = None;
            assert_eq!(view.disk, DiskState::Deleted);
        });

        view.update_in(cx, |view, window, cx| {
            view.save_action(&CeSave, window, cx);
        });
        cx.executor().allow_parking();
        cx.run_until_parked();

        assert_eq!(std::fs::read_to_string(&path).expect("read"), "one\n");
        view.update(cx, |view, _cx| assert_eq!(view.disk, DiskState::InSync));
    }

    /// US-016 AC: detection is wired at load time, on the file's parent
    /// directory. A rename-based save never reaches a watch on the old inode,
    /// which is why the watch is registered one level up.
    #[gpui::test]
    fn opening_a_real_file_registers_the_conflict_watcher(cx: &mut TestAppContext) {
        let (_dir, view, cx) = file_view(cx, "one\n", true);
        // Registration runs on the background executor now; drive it.
        cx.executor().allow_parking();
        cx.run_until_parked();
        view.update(cx, |view, _cx| {
            assert!(view._watcher.is_some(), "the parent directory is watched");
            let bridge = view
                ._watch_bridge
                .take()
                .expect("the reload task is bridged to the watcher");
            // Sever the bridge from this thread, then stop watching, both
            // before the temp dir is removed. Either order of the last two
            // would otherwise let the notify thread wake the reload task,
            // which the test scheduler reads as non-determinism.
            *bridge.lock().expect("bridge lock") = None;
            view._watcher = None;
        });
    }

    #[gpui::test]
    fn repeated_saves_without_a_watcher_cannot_bypass_conflict(cx: &mut TestAppContext) {
        let (dir, view, cx) = file_view(cx, "one\n", false);
        let path = dir.path().join("main.rs");
        view.update_in(cx, |view, window, cx| {
            view.replace_text_in_range(None, "mine", window, cx)
        });
        std::fs::write(&path, "external version\n").unwrap();
        cx.executor().allow_parking();
        for _ in 0..3 {
            view.update_in(cx, |view, window, cx| view.save_action(&CeSave, window, cx));
            cx.run_until_parked();
            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                "external version\n"
            );
        }
        view.update(cx, |view, cx| view.resolve_keep_mine(cx));
        cx.run_until_parked();
        std::fs::write(&path, "another external version\n").unwrap();
        view.update_in(cx, |view, window, cx| view.save_action(&CeSave, window, cx));
        cx.run_until_parked();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "another external version\n"
        );
    }

    #[gpui::test]
    fn reload_refusals_preserve_the_buffer_and_long_lines_are_read_only(cx: &mut TestAppContext) {
        let (dir, view, cx) = file_view(cx, "one\n", false);
        let path = dir.path().join("main.rs");
        cx.executor().allow_parking();
        for bytes in [
            b"binary\0data".to_vec(),
            vec![0xff],
            vec![b'x'; super::super::load::MAX_FILE_BYTES + 1],
        ] {
            std::fs::write(&path, bytes).unwrap();
            view.update(cx, |view, cx| view.resolve_reload(cx));
            cx.run_until_parked();
            view.update(cx, |view, _| {
                assert_eq!(text_of(view), "one\n");
                assert!(view.save_error.is_some());
            });
        }
        std::fs::write(&path, "x".repeat(super::super::load::MAX_LINE_CHARS + 1)).unwrap();
        view.update(cx, |view, cx| view.resolve_reload(cx));
        cx.run_until_parked();
        view.update(cx, |view, _| {
            assert!(view.document().unwrap().is_read_only())
        });
        std::fs::write(&path, "short\n").unwrap();
        view.update(cx, |view, cx| view.resolve_reload(cx));
        cx.run_until_parked();
        view.update(cx, |view, _| {
            assert!(!view.document().unwrap().is_read_only())
        });
    }

    #[gpui::test]
    fn regression_save_without_conflict_resolution_preserves_external_edit(
        cx: &mut TestAppContext,
    ) {
        let (dir, view, cx) = file_view(cx, "one\n", false);
        let path = dir.path().join("main.rs");
        view.update_in(cx, |view, window, cx| {
            view.selection = CodeSelection::at(4);
            view.replace_text_in_range(None, "mine\n", window, cx);
        });
        std::fs::write(&path, "theirs\n").unwrap();
        let stamp = FileStamp::read(&path);
        view.update(cx, |view, cx| {
            let generation = view.begin_disk_probe();
            view.disk_changed(generation, stamp, Some("theirs\n".into()), cx);
            assert!(view.has_conflict());
        });
        view.update_in(cx, |view, window, cx| view.save_action(&CeSave, window, cx));
        cx.executor().allow_parking();
        cx.run_until_parked();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "theirs\n",
            "Save must require Keep mine before replacing the external edit"
        );
    }

    #[gpui::test]
    fn regression_reload_reapplies_long_line_guard(cx: &mut TestAppContext) {
        let (dir, view, cx) = file_view(cx, "one\n", false);
        let path = dir.path().join("main.rs");
        let long = "x".repeat(super::super::load::MAX_LINE_CHARS + 1);
        std::fs::write(&path, &long).unwrap();
        let stamp = FileStamp::read(&path);
        view.update(cx, |view, cx| {
            let generation = view.begin_disk_probe();
            view.disk_changed(generation, stamp, Some(long), cx);
            assert!(
                view.document().unwrap().is_read_only(),
                "reload bypassed the initial-load long-line guard"
            );
        });
    }

    /// US-005: cutting the widest line marks the maximum stale; the rescan
    /// runs on the background executor and the exact value lands afterwards,
    /// so the horizontal extent shrinks instead of staying grow-only.
    #[gpui::test]
    fn shortening_the_longest_line_refreshes_horizontal_extent_off_thread(cx: &mut TestAppContext) {
        let (view, cx) = view(cx, "the longest line\nshort\n");

        view.update(cx, |view, cx| {
            assert!(view.splice_all(
                &[(0..16, "tiny".to_string())],
                CodeSelection::at(4),
                EditGroup::Atomic,
                cx,
            ));
            // On the render thread the value is the over-estimate...
            assert_eq!(view.document().expect("document").longest_line_chars(), 16);
        });
        cx.run_until_parked();

        // ...and the background measurement brings it down to the truth.
        view.update(cx, |view, _cx| {
            assert_eq!(view.document().expect("document").longest_line_chars(), 5);
        });
    }

    #[gpui::test]
    fn longest_line_rescans_coalesce_while_one_is_in_flight(cx: &mut TestAppContext) {
        let (view, cx) = view(cx, "the longest line\nshort\n");
        view.update(cx, |view, cx| {
            assert!(view.splice_all(
                &[(0..16, "tiny".to_string())],
                CodeSelection::at(4),
                EditGroup::Atomic,
                cx,
            ));
            assert!(
                view.longest_line_scan_in_flight,
                "the first shrink launches one scan"
            );
            assert!(!view.longest_line_rescan_needed);
            assert!(view.splice_all(
                &[(0..4, "x".to_string())],
                CodeSelection::at(1),
                EditGroup::Atomic,
                cx,
            ));
            assert!(
                view.longest_line_scan_in_flight,
                "a second shrink must not launch another walk"
            );
            assert!(
                view.longest_line_rescan_needed,
                "the in-flight scan is asked to run once more at the latest revision"
            );
        });
        cx.run_until_parked();
        view.update(cx, |view, _cx| {
            assert!(!view.longest_line_scan_in_flight);
            assert!(!view.longest_line_rescan_needed);
            assert_eq!(
                view.document().expect("document").longest_line_chars(),
                5,
                "the coalesced rescan lands the true maximum"
            );
        });
    }

    /// US-006: watcher registration is guarded by the load generation, so of
    /// two files opened back to back only the newest keeps a watcher.
    #[gpui::test]
    async fn only_the_latest_rapid_open_keeps_its_watcher(cx: &mut TestAppContext) {
        let first = tempfile::tempdir().expect("first tempdir");
        let second = tempfile::tempdir().expect("second tempdir");
        let first_path = first.path().join("first.rs");
        let second_path = second.path().join("second.rs");
        std::fs::write(&first_path, "first\n").expect("first fixture");
        std::fs::write(&second_path, "second\n").expect("second fixture");
        let (view, cx) = view(cx, "seed\n");
        // `open` reads through `smol::unblock`, outside the test scheduler.
        cx.executor().allow_parking();

        view.update(cx, |view, cx| {
            view.open(first_path, cx);
            view.open(second_path.clone(), cx);
        });
        for _ in 0..100 {
            cx.run_until_parked();
            if view.update(cx, |view, _cx| {
                view.document()
                    .and_then(|doc| doc.line_string(0))
                    .as_deref()
                    == Some("second")
                    && view._watcher.is_some()
            }) {
                break;
            }
            smol::Timer::after(Duration::from_millis(1)).await;
        }

        view.update(cx, |view, _cx| {
            assert_eq!(view.path(), second_path);
            assert_eq!(
                view.document()
                    .and_then(|doc| doc.line_string(0))
                    .as_deref(),
                Some("second")
            );
            assert!(view._watcher.is_some());
            if let Some(bridge) = view._watch_bridge.take() {
                *bridge.lock().expect("bridge lock") = None;
            }
            view._watcher = None;
        });
    }

    #[test]
    fn a_removed_parent_refuses_watcher_creation() {
        let dir = tempfile::tempdir().expect("tempdir");
        let parent = dir.path().to_path_buf();
        drop(dir);
        assert!(create_file_watcher(parent).is_err());
    }

    fn tracked_view<'a>(
        cx: &'a mut TestAppContext,
        base: &str,
        text: &str,
    ) -> (Entity<CodeView>, &'a mut VisualTestContext) {
        let (view, cx) = view(cx, text);
        view.update(cx, |view, cx| {
            view.install_base(
                Base::Text {
                    text: base.into(),
                    head_sha: "0123456789abcdef0123456789abcdef01234567".to_string(),
                },
                cx,
            );
        });
        settle_tracker(cx);
        (view, cx)
    }

    fn settle_tracker(cx: &mut VisualTestContext) {
        for _ in 0..4 {
            cx.executor().advance_clock(TRACKER_DEBOUNCE);
            cx.run_until_parked();
        }
    }

    #[gpui::test]
    fn a_pending_tracker_cannot_replace_a_new_base_at_the_same_commit(cx: &mut TestAppContext) {
        let (view, cx) = view(cx, "a\nB\nc\n");
        view.update(cx, |view, cx| {
            view.install_base(
                Base::Text {
                    text: "a\nb\nc\n".into(),
                    head_sha: "same-commit".into(),
                },
                cx,
            );
            view.refresh_tracker_now(cx);
            view.install_base(
                Base::Text {
                    text: "a\nB\nc\n".into(),
                    head_sha: "same-commit".into(),
                },
                cx,
            );
        });
        cx.run_until_parked();
        view.update(cx, |view, _| {
            assert!(
                view.tracker.is_dirty(),
                "the old worker must not mark the new base clean"
            );
        });
        settle_tracker(cx);
        view.update(cx, |view, _| assert!(view.marker_blocks().is_empty()));
    }

    fn blocks_of(view: &CodeView) -> Vec<(BlockKind, Range<u32>, Range<u32>)> {
        view.marker_blocks()
            .iter()
            .map(|block| (block.kind(), block.lines.clone(), block.base_lines.clone()))
            .collect()
    }

    #[gpui::test]
    fn a_tracked_base_yields_blocks_after_the_debounce(cx: &mut TestAppContext) {
        let (view, cx) = view(cx, "a\nX\nc\nd\ne\n");
        view.update(cx, |view, cx| {
            assert!(view.marker_blocks().is_empty());
            view.install_base(
                Base::Text {
                    text: "a\nb\nc\ne\n".into(),
                    head_sha: "deadbeef".to_string(),
                },
                cx,
            );
            assert!(view.tracker.is_dirty(), "the base installs one dirty block");
            assert_eq!(
                view.marker_blocks().len(),
                1,
                "nothing is diffed on the render thread"
            );
        });
        cx.run_until_parked();
        view.update(cx, |view, _cx| {
            assert!(view.tracker.is_dirty(), "the debounce has not elapsed yet");
        });
        settle_tracker(cx);
        view.update(cx, |view, _cx| {
            assert_eq!(
                blocks_of(view),
                vec![
                    (BlockKind::Modified, 1..2, 1..2),
                    (BlockKind::Added, 3..4, 3..3),
                ]
            );
            assert!(!view.tracker.is_dirty());
        });
    }

    #[gpui::test]
    fn an_untracked_or_absent_base_keeps_the_tracker_inactive(cx: &mut TestAppContext) {
        let (view, cx) = view(cx, "a\nb\n");
        view.update_in(cx, |view, window, cx| {
            view.install_base(Base::Untracked, cx);
            view.replace_text_in_range(None, "x", window, cx);
            assert!(!view.tracker.is_active());
            assert!(view.marker_blocks().is_empty());
            view.install_base(Base::None, cx);
            assert!(!view.tracker.is_active());
        });
        settle_tracker(cx);
        view.update(cx, |view, _cx| assert!(view.marker_blocks().is_empty()));
    }

    #[gpui::test]
    fn keystrokes_move_the_blocks_without_a_full_rediff(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let base: String = (0..200).map(|row| format!("line {row}\n")).collect();
        let (view, cx) = tracked_view(cx, &base, &base);
        view.update(cx, |view, _cx| {
            assert!(view.marker_blocks().is_empty());
            assert_eq!(
                view.tracker.stats().rediffs,
                0,
                "an equal document short-circuits"
            );
        });

        let at = base.find("line 100").expect("line 100") + 8;
        view.update_in(cx, |view, window, cx| {
            view.selection = CodeSelection::at(at);
            for letter in ["x", "y", "z"] {
                view.replace_text_in_range(None, letter, window, cx);
            }
            assert_eq!(
                blocks_of(view),
                vec![(BlockKind::Modified, 100..101, 100..101)],
                "range_changed marks the typed line synchronously"
            );
            assert!(view.tracker.is_dirty());
            assert_eq!(view.tracker.stats().rediffs, 0);
        });
        settle_tracker(cx);
        view.update_in(cx, |view, window, cx| {
            assert_eq!(
                blocks_of(view),
                vec![(BlockKind::Modified, 100..101, 100..101)]
            );
            assert_eq!(
                view.tracker.stats().rediffs,
                1,
                "three grouped keystrokes, one diff"
            );
            assert_eq!(view.tracker.stats().full_rediffs, 0);

            let top = view.document().expect("document").line_to_byte(10);
            view.selection = CodeSelection::at(top);
            view.replace_text_in_range(None, "inserted\n", window, cx);
            assert_eq!(
                blocks_of(view),
                vec![
                    (BlockKind::Added, 10..11, 10..10),
                    (BlockKind::Modified, 101..102, 100..101),
                ],
                "blocks after the edit shift without a diff"
            );
            assert!(view.marker_blocks()[0].dirty);
            assert!(!view.marker_blocks()[1].dirty);
        });
        settle_tracker(cx);
        view.update(cx, |view, _cx| {
            assert_eq!(
                blocks_of(view),
                vec![
                    (BlockKind::Added, 10..11, 10..10),
                    (BlockKind::Modified, 101..102, 100..101),
                ]
            );
            assert_eq!(view.tracker.stats().full_rediffs, 0);
        });
    }

    #[gpui::test]
    fn a_stale_refresh_result_is_discarded_and_rescheduled(cx: &mut TestAppContext) {
        let (view, cx) = tracked_view(cx, "a\nb\nc\n", "a\nB\nc\n");
        view.update_in(cx, |view, window, cx| {
            assert_eq!(blocks_of(view), vec![(BlockKind::Modified, 1..2, 1..2)]);
            view.selection = CodeSelection::at(4);
            view.replace_text_in_range(None, "C", window, cx);
            view.refresh_tracker_now(cx);
            view.replace_text_in_range(None, "C", window, cx);
        });
        cx.run_until_parked();
        view.update(cx, |view, _cx| {
            assert!(
                view.tracker.is_dirty(),
                "the result computed before the second keystroke was discarded"
            );
        });
        settle_tracker(cx);
        view.update(cx, |view, _cx| {
            assert_eq!(blocks_of(view), vec![(BlockKind::Modified, 1..3, 1..3)]);
            assert_eq!(text_of(view), "a\nB\nCCc\n");
        });
    }

    #[gpui::test]
    fn a_refresh_in_flight_when_the_base_changes_is_discarded(cx: &mut TestAppContext) {
        let (view, cx) = tracked_view(cx, "a\nb\nc\n", "a\nB\nc\n");
        view.update(cx, |view, cx| {
            assert_eq!(blocks_of(view), vec![(BlockKind::Modified, 1..2, 1..2)]);
            let doc_lines = view.document().expect("document").line_count() as u32;
            view.tracker.reset(doc_lines, count_lines("a\nb\nc\n"));
            view.refresh_tracker_now(cx);
            view.install_base(
                Base::Text {
                    text: "a\nB\nc\n".into(),
                    head_sha: "cafebabe".to_string(),
                },
                cx,
            );
            assert!(
                view.tracker.is_dirty(),
                "the new base installs one dirty block"
            );
        });
        cx.run_until_parked();
        view.update(cx, |view, _cx| {
            assert!(
                view.tracker.is_dirty(),
                "the result computed against the previous base is discarded"
            );
        });
        settle_tracker(cx);
        view.update(cx, |view, _cx| {
            assert!(
                view.marker_blocks().is_empty(),
                "the document equals the new base, got {:?}",
                blocks_of(view)
            );
        });
    }

    #[gpui::test]
    fn an_external_reload_resets_the_tracker_and_closes_the_popup(cx: &mut TestAppContext) {
        let (view, cx) = tracked_view(cx, "a\nb\nc\n", "a\nB\nc\n");
        view.update(cx, |view, cx| {
            view.open_marker_popup(0, cx);
            assert!(view.popup.is_some());
            view.adopt_disk_text("a\nb\nc\nd\n", cx);
            assert!(view.popup.is_none(), "an agent write closes the popup");
            assert_eq!(view.marker_blocks().len(), 1);
            assert!(
                view.marker_blocks()[0].dirty,
                "one dirty block covers everything"
            );
            assert_eq!(view.marker_blocks()[0].lines, 0..5);
        });
        settle_tracker(cx);
        view.update(cx, |view, _cx| {
            assert_eq!(blocks_of(view), vec![(BlockKind::Added, 3..4, 3..3)]);
        });
    }

    #[gpui::test]
    fn reverting_added_deleted_and_modified_blocks_restores_the_base(cx: &mut TestAppContext) {
        let base = "a\nb\nc\nd\n";
        let (view, cx) = tracked_view(cx, base, "a\nB\nc\nd\n");
        view.update(cx, |view, cx| {
            assert!(view.revert_block(1, cx), "a modified block is replaced");
            assert_eq!(text_of(view), base);
            assert!(view.is_dirty(), "the document is dirty after a revert");
        });
        settle_tracker(cx);
        view.update(cx, |view, _cx| assert!(view.marker_blocks().is_empty()));

        let (view, cx) = tracked_view(cx, base, "a\nb\nnew\nc\nd\n");
        view.update(cx, |view, cx| {
            assert_eq!(blocks_of(view), vec![(BlockKind::Added, 2..3, 2..2)]);
            assert!(view.revert_block(2, cx), "an added block loses its lines");
            assert_eq!(text_of(view), base);
        });

        let (view, cx) = tracked_view(cx, base, "a\nd\n");
        view.update(cx, |view, cx| {
            assert_eq!(blocks_of(view), vec![(BlockKind::Deleted, 1..1, 1..3)]);
            assert!(
                view.revert_block(1, cx),
                "a deleted block is reinserted at its boundary"
            );
            assert_eq!(text_of(view), base);
        });

        let (view, cx) = tracked_view(cx, base, "a\nB\nC\nd\n");
        view.update(cx, |view, cx| {
            assert_eq!(blocks_of(view), vec![(BlockKind::Modified, 1..3, 1..3)]);
            assert!(view.revert_block(2, cx));
            assert_eq!(text_of(view), base);
        });
        settle_tracker(cx);
        view.update(cx, |view, _cx| assert!(view.marker_blocks().is_empty()));
    }

    #[gpui::test]
    fn two_reverts_on_adjacent_blocks_give_the_base_and_undo_brings_each_back(
        cx: &mut TestAppContext,
    ) {
        let base = "a\nb\nc\nd\ne\n";
        let edited = "a\nB\nc\nD\ne\n";
        let (view, cx) = tracked_view(cx, base, edited);
        let before = view.update(cx, |view, cx| {
            assert_eq!(
                blocks_of(view),
                vec![
                    (BlockKind::Modified, 1..2, 1..2),
                    (BlockKind::Modified, 3..4, 3..4),
                ]
            );
            let before = view.tracker.stats();
            assert!(view.revert_block(1, cx));
            assert_eq!(
                blocks_of(view),
                vec![
                    (BlockKind::Modified, 1..2, 1..2),
                    (BlockKind::Modified, 3..4, 3..4),
                ],
                "the next block is untouched by the revert"
            );
            assert!(
                view.marker_blocks()[0].dirty,
                "the reverted block awaits its diff"
            );
            assert!(!view.marker_blocks()[1].dirty);
            assert_eq!(view.tracker.stats().rediffs, before.rediffs);
            before
        });
        cx.run_until_parked();
        view.update(cx, |view, cx| {
            assert_eq!(
                blocks_of(view),
                vec![(BlockKind::Modified, 3..4, 3..4)],
                "the revert is diffed at once, without the keystroke debounce"
            );
            assert_eq!(view.tracker.stats().rediffs, before.rediffs + 1);
            assert_eq!(
                view.tracker.stats().full_rediffs,
                before.full_rediffs,
                "only the reverted window is diffed"
            );
            assert!(view.revert_block(3, cx));
            assert_eq!(text_of(view), base);
        });
        settle_tracker(cx);
        view.update_in(cx, |view, window, cx| {
            assert!(view.marker_blocks().is_empty());
            view.undo(&CeUndo, window, cx);
            assert_eq!(text_of(view), "a\nb\nc\nD\ne\n", "one Ctrl+Z, one revert");
            view.undo(&CeUndo, window, cx);
            assert_eq!(
                text_of(view),
                edited,
                "the second Ctrl+Z restores the rest exactly"
            );
        });
        settle_tracker(cx);
        view.update(cx, |view, _cx| {
            assert_eq!(
                blocks_of(view),
                vec![
                    (BlockKind::Modified, 1..2, 1..2),
                    (BlockKind::Modified, 3..4, 3..4),
                ],
                "the tracker finds both blocks again"
            );
        });
    }

    #[gpui::test]
    fn a_revert_on_a_read_only_document_or_a_plain_line_does_nothing(cx: &mut TestAppContext) {
        let (view, cx) = tracked_view(cx, "a\nb\nc\n", "a\nB\nc\n");
        view.update(cx, |view, cx| {
            assert!(!view.revert_block(0, cx), "line 0 has no block");
            assert_eq!(text_of(view), "a\nB\nc\n");
            assert!(view.read_only_flash.is_none());

            view.state
                .document_mut()
                .expect("document")
                .set_read_only(Some(ReadOnlyReason::Permissions));
            assert!(!view.revert_block(1, cx));
            assert_eq!(text_of(view), "a\nB\nc\n");
            assert!(view.read_only_flash.is_some(), "refused like a keystroke");
        });
    }

    #[test]
    fn gutter_revert_restores_eof_separators_and_empty_files() {
        for (base, edited) in [
            ("a\nb", "a\nb\n"),
            ("a\nb\n", "a\nb"),
            ("a", "a\nextra"),
            ("a\nextra", "a"),
            ("", "\nx"),
            ("\nx", ""),
            ("", "x"),
            ("x", ""),
        ] {
            let doc = build_document(PathBuf::from("/nonexistent/eof.txt"), edited, false);
            let base_lines = split_lines(base);
            let mut tracker = BlockTracker::fresh(doc.line_count() as u32, base_lines.len() as u32);
            tracker.refresh_dirty(&split_lines(edited), &base_lines, TRACKER_POLICY);
            assert_eq!(tracker.blocks().len(), 1, "{base:?} -> {edited:?}");
            let (range, replacement) = block_replacement(&doc, &base_lines, &tracker.blocks()[0]);
            let mut restored = edited.to_string();
            restored.replace_range(range, &replacement);
            assert_eq!(restored, base, "{base:?} -> {edited:?}");
        }
    }

    #[gpui::test]
    fn removing_a_final_newline_through_gutter_revert_is_undoable(cx: &mut TestAppContext) {
        let (view, cx) = tracked_view(cx, "a\nb", "a\nb\n");
        view.update_in(cx, |view, window, cx| {
            assert!(view.revert_block(2, cx));
            assert_eq!(text_of(view), "a\nb");
            view.undo(&CeUndo, window, cx);
            assert_eq!(text_of(view), "a\nb\n");
        });
    }

    #[gpui::test]
    fn the_popup_shows_the_base_text_copies_all_of_it_and_reverts(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let base: String = (0..250).map(|row| format!("base {row}\n")).collect();
        let doc = "top\n".to_string() + &base[base.find("base 245").expect("tail")..];
        let (view, cx) = tracked_view(cx, &base, &doc);
        view.update(cx, |view, cx| {
            assert_eq!(blocks_of(view), vec![(BlockKind::Modified, 0..1, 0..245)]);
            view.open_marker_popup(0, cx);
            let popup = view.popup.as_ref().expect("popup");
            assert_eq!(popup.title, "Modified line 1");
            assert_eq!(popup.shown.len(), POPUP_SHOWN_LINES);
            assert_eq!(popup.hidden, 45);
            assert_eq!(popup.shown[0].0.as_ref(), "base 0");
            assert!(popup.base_text.ends_with("base 244\n"));
            assert_eq!(popup.base_text.lines().count(), 245);

            view.copy_popup_base(cx);
            let clipped = cx
                .read_from_clipboard()
                .and_then(|item| item.text())
                .unwrap_or_default();
            assert_eq!(clipped.lines().count(), 245, "Copy takes the whole block");

            view.revert_from_popup(cx);
            assert!(view.popup.is_none(), "Revert closes the popup");
            assert_eq!(text_of(view), base);
        });
    }

    #[gpui::test]
    fn revert_from_popup_follows_the_block_after_an_insert_above(cx: &mut TestAppContext) {
        let (view, cx) = tracked_view(cx, "a\nb\nc\n", "a\nB\nc\n");
        view.update(cx, |view, cx| {
            view.open_marker_popup(0, cx);
            assert!(
                view.splice_all(
                    &[(0..0, "HEAD\n".into())],
                    CodeSelection::at(0),
                    EditGroup::Atomic,
                    cx,
                ),
                "insert above the open popup"
            );
            view.revert_from_popup(cx);
            assert_eq!(
                text_of(view),
                "HEAD\na\nb\nc\n",
                "Revert must restore the HEAD block at its shifted row, not the new top line"
            );
        });
    }

    #[gpui::test]
    fn a_popup_whose_block_disappeared_does_not_revert_another_block(cx: &mut TestAppContext) {
        let (view, cx) = tracked_view(cx, "a\nb\nc\nd\ne\n", "a\nB\nc\nD\ne\n");
        view.update(cx, |view, cx| {
            view.open_marker_popup(0, cx);
            assert!(view.splice_all(
                &[(0..4, "".into())],
                CodeSelection::at(0),
                EditGroup::Atomic,
                cx
            ));
        });
        settle_tracker(cx);
        view.update(cx, |view, cx| {
            assert!(view.popup.is_some());
            view.revert_from_popup(cx);
            assert_eq!(
                text_of(view),
                "c\nD\ne\n",
                "the second change must remain intact"
            );
        });
    }

    #[gpui::test]
    fn escape_closes_the_popup_and_hover_alone_never_opens_it(cx: &mut TestAppContext) {
        let (view, cx) = tracked_view(cx, "a\nb\nc\n", "a\nB\nc\n");
        view.update_in(cx, |view, window, cx| {
            *view.hits.borrow_mut() = CodeHitMap {
                marker_x: 10.0,
                markers: vec![super::super::element::MarkerHit {
                    index: 0,
                    y0: 100.0,
                    y1: 118.0,
                }],
                ..CodeHitMap::default()
            };
            let over = MouseMoveEvent {
                position: point(px(12.), px(105.)),
                pressed_button: None,
                modifiers: Modifiers::default(),
            };
            view.on_marker_move(&over, cx);
            assert_eq!(view.hovered_marker(), Some(0), "hover widens the bar");
            assert!(view.popup.is_none(), "hover never opens the popup");

            let away = MouseMoveEvent {
                position: point(px(200.), px(105.)),
                ..over
            };
            view.on_marker_move(&away, cx);
            assert_eq!(view.hovered_marker(), None);

            view.open_marker_popup(0, cx);
            assert!(view.popup.is_some());
            view.escape(&CeEscape, window, cx);
            assert!(view.popup.is_none(), "Escape closes it without acting");
            assert_eq!(text_of(view), "a\nB\nc\n");
        });
    }

    #[test]
    fn popup_titles_name_the_block() {
        let block = |lines: Range<u32>, base_lines: Range<u32>| Block {
            lines,
            base_lines,
            dirty: false,
            too_big: false,
        };
        assert_eq!(popup_title(&block(11..15, 11..13)), "Modified lines 12-15");
        assert_eq!(popup_title(&block(4..5, 4..5)), "Modified line 5");
        assert_eq!(
            popup_title(&block(20..20, 19..22)),
            "Deleted 3 lines after 20"
        );
        assert_eq!(popup_title(&block(0..0, 0..1)), "Deleted 1 line at the top");
        assert_eq!(popup_title(&block(3..7, 3..3)), "Added 4 lines");
    }

    #[test]
    fn the_popup_width_is_bounded_by_the_editor_and_flips_when_short_of_room() {
        assert_eq!(popup_width(880.0), POPUP_MAX_W);
        assert_eq!(popup_width(400.0), 400.0 - 2.0 * POPUP_MARGIN);
        assert_eq!(popup_width(360.0), 336.0);
        assert_eq!(popup_width(200.0), 200.0 - 2.0 * POPUP_MARGIN);
        assert_eq!(
            popup_anchor(100.0, 118.0, 200.0, 600.0),
            (Anchor::TopLeft, 118.0)
        );
        assert_eq!(
            popup_anchor(500.0, 518.0, 200.0, 600.0),
            (Anchor::BottomLeft, 500.0)
        );
        assert_eq!(
            popup_anchor(50.0, 68.0, 200.0, 100.0),
            (Anchor::TopLeft, 68.0),
            "no room either side keeps the default below"
        );
    }

    #[test]
    fn base_block_text_and_doc_line_range_agree_on_terminators() {
        let base = ["a", "b", "c", ""];
        assert_eq!(base_block_text(&base, &(1..3)), "b\nc\n");
        assert_eq!(base_block_text(&base, &(2..4)), "c\n");
        assert_eq!(base_block_text(&base, &(1..1)), "");
        assert_eq!(
            base_block_text(&base, &(3..4)),
            "\n",
            "the trailing empty slot of a terminated file is a newline"
        );
        assert_eq!(base_block_text(&[""], &(0..1)), "");
        let unterminated = ["a", "b"];
        assert_eq!(base_block_text(&unterminated, &(1..2)), "b");

        let doc = build_document(PathBuf::from("/nonexistent/x.txt"), "a\nb\nc\n", false);
        assert_eq!(doc_line_range(&doc, &(1..3)), 2..6);
        assert_eq!(doc_line_range(&doc, &(3..4)), 6..6);
        assert_eq!(doc_line_range(&doc, &(1..1)), 2..2);
        let short = build_document(PathBuf::from("/nonexistent/y.txt"), "a\nb", false);
        assert_eq!(doc_line_range(&short, &(1..2)), 2..3);
    }
}
