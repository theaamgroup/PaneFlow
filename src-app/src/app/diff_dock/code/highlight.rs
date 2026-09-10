//! Incremental syntax highlighting for the file editor.
//!
//! prd-file-editor-2026-Q3, US-004. The diff colors a file by parsing it whole
//! ([`crate::diff::highlight_lines`]); that is right for a diff, which is built
//! once and never mutated, and wrong for an editor, where a full parse per
//! keystroke is the entire latency budget. This module keeps the tree alive and
//! feeds it edits instead - but it deliberately owns **no** grammar table, no
//! query and no color map of its own. It consumes
//! [`crate::diff::grammar_for_ext`], [`crate::diff::markdown_inline_grammar`],
//! [`crate::diff::resolve_runs`], [`crate::diff::highlight_cap`] and
//! [`DiffSyntax`], which is why `parity_matches_diff_highlighter_on_all_grammars`
//! below can assert the two surfaces produce byte-identical runs.
//!
//! **Text before tree** (#427). [`CodeHighlighter::new`] parses nothing: the
//! file's first parse is a [`DeferredParse`] handed out by
//! [`CodeHighlighter::initial_parse`], run off the render thread and landed
//! through [`CodeHighlighter::apply_parsed`], so a 2 MB file shows its text
//! before its tree exists. An initial parse that runs past
//! [`INITIAL_PARSE_TIMEOUT`] gives up: the file stays plain and
//! [`CodeHighlighter::is_too_complex`] tells the view to say so.
//!
//! **Viewport-bounded coloring** (#427). Rows are never queried eagerly.
//! Every row starts [`RowState::Stale`]; an edit or a landed tree only marks
//! rows stale, and [`CodeHighlighter::fill_stale_rows`] colors the rows a
//! frame asks for under [`HIGHLIGHT_FRAME_BUDGET`]. Runs are stored as
//! 12-byte `(start, end, capture)` triples resolved against a per-grammar
//! color table, so a theme switch rebuilds the table and never re-queries
//! tree-sitter.
//!
//! **The per-keystroke sequence**, in the order Zed established and this module
//! mirrors:
//!
//! 1. [`CodeHighlighter::edit`] first *interpolates* the cached runs
//!    (`zed:crates/language/src/syntax_map.rs::interpolate`): rows are spliced
//!    and columns shifted so the frame that renders the keystroke already shows
//!    plausible colors, with zero parsing.
//! 2. Each live tree gets the same edit through `Tree::edit`, so the next parse
//!    can reuse every subtree the edit did not touch.
//! 3. A reparse is attempted **synchronously under a 1 ms budget** - Zed's
//!    production value (`zed:crates/language/src/buffer.rs`, `sync_parse_timeout`),
//!    not a number picked here. The budget is enforced through tree-sitter's
//!    `ParseOptions::progress_callback`, which is the modern spelling of the
//!    `reparse_with_timeout` mechanism.
//! 4. If the budget blows, the aborted parser is [`Parser::reset`] (mandatory:
//!    a cancelled parse leaves it mid-document) and the work becomes a
//!    [`DeferredParse`] the caller runs off-thread. The text stays fully
//!    editable meanwhile, colored by the interpolated runs. A deferred parse
//!    carries a cancel token: the next edit, or dropping the highlighter,
//!    stops it at its next progress callback instead of letting a burst of
//!    keystrokes queue a full parse each.
//! 5. The deferred result carries a generation. [`CodeHighlighter::apply_parsed`]
//!    drops it if any edit happened in between, so a slow parse can never
//!    repaint stale colors over newer text.
//!
//! After a successful parse, only the rows tree-sitter reports as changed
//! (`Tree::changed_ranges`, unioned with the edit itself) are marked stale. A
//! keystroke therefore costs an incremental parse plus, at the next frame, a
//! query over the stale rows in view, never a full parse and never a full-file
//! query.

use std::ops::{ControlFlow, Range};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

#[cfg(test)]
use gpui::AppContext;
use gpui::{AsyncApp, Context, Hsla, WeakEntity};
use ropey::Rope;
use streaming_iterator::StreamingIterator;
use tree_sitter::{
    InputEdit, Node, ParseOptions, ParseState, Parser, Point as TsPoint, QueryCursor, TextProvider,
    Tree,
};

use crate::diff::{
    DiffSyntax, Grammar, MAX_CAPTURES_PER_ROW, grammar_for_ext, highlight_cap, is_markdown,
    markdown_inline_grammar, resolve_runs,
};

use super::document::{CodeDocument, CodeEdit};

/// Synchronous reparse budget. 1 ms, matching Zed's `sync_parse_timeout`: long
/// enough that ordinary edits in ordinary files never leave the main thread,
/// short enough to stay invisible inside a 16 ms frame.
pub(crate) const SYNC_PARSE_BUDGET: Duration = Duration::from_millis(1);

/// How long the deferred initial parse may run before the file is declared
/// too complex to color. Off the render thread, so this bounds memory and CPU
/// spent on a pathological file, not a stall.
pub(crate) const INITIAL_PARSE_TIMEOUT: Duration = Duration::from_secs(5);

/// Render-thread budget one frame may spend coloring stale rows in view.
pub(crate) const HIGHLIGHT_FRAME_BUDGET: Duration = Duration::from_millis(2);

/// One line's foreground runs, in line-relative byte ranges - the exact shape
/// [`crate::diff::highlight_lines`] returns per line, so the renderer treats a
/// diff row and an editor row identically.
pub(crate) type LineRuns = Vec<(Range<usize>, Hsla)>;

/// Which grammar pass and which of its query captures produced a run. The
/// color is looked up at read time through [`GrammarPass::colors`], so a theme
/// change never rewrites the rows.
#[derive(Clone, Copy)]
struct IndexedCapture {
    pass: u16,
    capture: u16,
}

/// A stored run: line-relative `(start, end)` plus the capture that owns it.
/// Twelve bytes, pinned by `indexed_runs_keep_the_twelve_byte_storage_contract`.
type IndexedRun = (u32, u32, IndexedCapture);
type IndexedLineRuns = Vec<IndexedRun>;

#[derive(Clone, Copy, PartialEq, Eq)]
enum RowState {
    /// The stored runs match the live tree.
    Fresh,
    /// The row must be re-queried before its runs are shown; until then it
    /// renders plain.
    Stale,
}

/// A grammar kept live across edits: its interned grammar, a parser bound to
/// it, the tree from the last successful parse, and the color of each of its
/// query captures under the current theme.
struct GrammarPass {
    grammar: &'static Grammar,
    parser: Parser,
    tree: Option<Tree>,
    colors: Vec<Option<Hsla>>,
}

/// How many captures [`CodeHighlighter::requery_rows`] pulls between two
/// looks at the frame deadline while a row is being queried.
const DEADLINE_CHECK_STRIDE: usize = 64;

/// What a [`CodeHighlighter::fill_stale_rows`] call left behind: the rows of
/// the requested range still uncolored when the budget ran out.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct StaleFill {
    pub(crate) stale_rows: usize,
}

impl StaleFill {
    pub(crate) fn any_stale(self) -> bool {
        self.stale_rows > 0
    }
}

/// What [`CodeHighlighter::edit`] managed to do within the budget.
pub(crate) enum HighlightOutcome {
    /// Reparsed inside the budget: the trees are exact, and the rows the edit
    /// touched are stale until the next fill.
    Synced,
    /// The budget blew. Runs are interpolated (plausible, not exact); run the
    /// payload off-thread and feed it back to [`CodeHighlighter::apply_parsed`].
    Deferred(DeferredParse),
}

/// A batch handed to [`CodeHighlighter::edit_batch`] whose hunks do not
/// descend by row without overlapping. Nothing was applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct UnorderedBatch;

/// A reparse that has to happen off the render thread. Owns everything it
/// needs: a snapshot of the rope (cheap - ropey clones share their chunks) and
/// the already-edited trees to reuse.
pub(crate) struct DeferredParse {
    generation: u64,
    rope: Rope,
    passes: Vec<(&'static Grammar, Option<Tree>)>,
    cancel: Arc<AtomicBool>,
    /// Set for the initial parse only: past it the parse gives up and the
    /// result reports `timed_out`.
    timeout: Option<Duration>,
}

/// The result of a [`DeferredParse`], still stamped with the generation it was
/// started for.
pub(crate) struct ParsedTrees {
    generation: u64,
    len_bytes: usize,
    trees: Vec<Option<Tree>>,
    timed_out: bool,
    cancelled: bool,
}

#[cfg(test)]
impl ParsedTrees {
    pub(crate) fn was_cancelled(&self) -> bool {
        self.cancelled
    }
}

impl DeferredParse {
    #[allow(dead_code)] // EP-001 accessor: generations are compared inside the highlighter; no caller reads them out yet.
    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    #[cfg(test)]
    pub(crate) fn with_timeout_for_test(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// **Blocking**, no budget beyond the optional timeout. Runs inside
    /// `smol::unblock` (see [`spawn_deferred_parse`]). A cancelled parse stops
    /// at its next progress callback and returns a result
    /// [`CodeHighlighter::apply_parsed`] rejects.
    pub(crate) fn run(self) -> ParsedTrees {
        let DeferredParse {
            generation,
            rope,
            passes,
            cancel,
            timeout,
        } = self;
        let deadline = timeout.map(|timeout| Instant::now() + timeout);
        let len_bytes = rope.len_bytes();
        let mut timed_out = false;
        let trees = passes
            .into_iter()
            .map(|(grammar, old)| {
                if timed_out || cancel.load(Ordering::Relaxed) {
                    return None;
                }
                let mut parser = Parser::new();
                if parser.set_language(&grammar.language).is_err() {
                    return None;
                }
                let tree = parse_rope(
                    &mut parser,
                    &rope,
                    old.as_ref(),
                    deadline,
                    Some(cancel.as_ref()),
                );
                if tree.is_none() {
                    timed_out = deadline.is_some_and(|deadline| Instant::now() >= deadline);
                }
                tree
            })
            .collect();
        ParsedTrees {
            generation,
            len_bytes,
            trees,
            timed_out,
            cancelled: cancel.load(Ordering::Relaxed),
        }
    }
}

/// Per-file highlighting state: the live trees plus the per-row runs the
/// renderer reads.
pub(crate) struct CodeHighlighter {
    syntax: DiffSyntax,
    passes: Vec<GrammarPass>,
    rows: Vec<IndexedLineRuns>,
    row_states: Vec<RowState>,
    /// `false` for an unknown extension, a file past
    /// [`crate::diff::highlight_cap`], or a file whose initial parse gave up.
    /// The document stays fully editable; every row simply renders in the
    /// default foreground.
    enabled: bool,
    /// The initial parse ran past [`INITIAL_PARSE_TIMEOUT`]; the view shows a
    /// banner for it.
    too_complex: bool,
    generation: u64,
    /// Cancel token of the deferred parse in flight, if any.
    deferred_cancel: Option<Arc<AtomicBool>>,
    /// Captures pulled from the query cursors so far: the enumeration
    /// [`Self::requery_rows`] bounds, not the runs it stores.
    #[cfg(test)]
    captures_pulled: usize,
}

impl CodeHighlighter {
    /// Build the highlighter for `doc` **without parsing it**: the first tree
    /// comes from [`Self::initial_parse`], run off the render thread. `syntax`
    /// is a snapshot of the active theme, rebuilt by the caller on theme
    /// change exactly as the diff does.
    pub(crate) fn new(doc: &CodeDocument, syntax: DiffSyntax) -> Self {
        let mut passes = Vec::new();
        if doc.len_bytes() <= highlight_cap(doc.ext())
            && let Some(grammar) = grammar_for_ext(doc.ext())
        {
            passes.push(grammar);
            // Markdown is colored by two grammars, block then inline, merged by
            // `resolve_runs` - the same two passes `highlight_lines` runs.
            if is_markdown(doc.ext())
                && let Some(inline) = markdown_inline_grammar()
            {
                passes.push(inline);
            }
        }
        let passes = passes
            .into_iter()
            .filter_map(|grammar| {
                let mut parser = Parser::new();
                parser.set_language(&grammar.language).ok()?;
                Some(GrammarPass {
                    grammar,
                    parser,
                    tree: None,
                    colors: capture_colors(grammar, &syntax),
                })
            })
            .collect::<Vec<_>>();
        let enabled = !passes.is_empty();

        Self {
            syntax,
            passes,
            rows: vec![Vec::new(); doc.line_count()],
            row_states: vec![
                if enabled {
                    RowState::Stale
                } else {
                    RowState::Fresh
                };
                doc.line_count()
            ],
            enabled,
            too_complex: false,
            generation: 0,
            deferred_cancel: None,
            #[cfg(test)]
            captures_pulled: 0,
        }
    }

    /// The file's first parse, to run off the render thread and land through
    /// [`Self::apply_parsed`]. `None` for a plain file or one that already
    /// has its tree.
    pub(crate) fn initial_parse(&mut self, doc: &CodeDocument) -> Option<DeferredParse> {
        if !self.enabled || self.has_tree() {
            return None;
        }
        let cancel = Arc::new(AtomicBool::new(false));
        self.deferred_cancel = Some(cancel.clone());
        Some(DeferredParse {
            generation: self.generation,
            rope: doc.text().clone(),
            passes: self
                .passes
                .iter()
                .map(|pass| (pass.grammar, None))
                .collect(),
            cancel,
            timeout: Some(INITIAL_PARSE_TIMEOUT),
        })
    }

    /// Whether any grammar pass holds a tree yet. `false` between the open and
    /// the landing of the initial parse, when the text is shown plain.
    pub(crate) fn has_tree(&self) -> bool {
        self.passes.iter().any(|pass| pass.tree.is_some())
    }

    /// Whether the initial parse gave up past [`INITIAL_PARSE_TIMEOUT`].
    pub(crate) fn is_too_complex(&self) -> bool {
        self.too_complex
    }

    /// Whether this file is colored at all. `false` means plain text, which is
    /// a rendering outcome, never an editing restriction.
    #[allow(dead_code)] // EP-001 accessor: the view branches on the highlight result, not on the flag, so far.
    pub(crate) fn is_enabled(&self) -> bool {
        self.enabled
    }

    #[allow(dead_code)] // EP-001 accessor: generations are compared inside the highlighter; no caller reads them out yet.
    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    /// Foreground runs for `row`, line-relative and non-overlapping. Empty for
    /// an uncolored row, a stale row, or an out-of-range index. Built on the
    /// fly from the stored captures and the current color table, so the
    /// result is owned.
    pub(crate) fn runs(&self, row: usize) -> LineRuns {
        if self.row_states.get(row) != Some(&RowState::Fresh) {
            return Vec::new();
        }
        self.rows
            .get(row)
            .map(|runs| {
                runs.iter()
                    .filter_map(|&(start, end, indexed)| {
                        let pass = self.passes.get(indexed.pass as usize)?;
                        let color = pass
                            .colors
                            .get(indexed.capture as usize)
                            .copied()
                            .flatten()?;
                        Some((start as usize..end as usize, color))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Rebuild the color tables against a new theme snapshot without reparsing
    /// or re-querying: the rows store captures, not colors.
    pub(crate) fn set_syntax(&mut self, _doc: &CodeDocument, syntax: DiffSyntax) {
        self.syntax = syntax;
        for pass in &mut self.passes {
            pass.colors = capture_colors(pass.grammar, &self.syntax);
        }
    }

    /// Fold one applied edit in. `doc` must already reflect `edit`.
    ///
    /// Always interpolates first, so the caller can paint immediately whatever
    /// this returns.
    pub(crate) fn edit(&mut self, doc: &CodeDocument, edit: &CodeEdit) -> HighlightOutcome {
        self.edit_with_budget(doc, edit, SYNC_PARSE_BUDGET)
    }

    /// [`Self::edit`] with an explicit budget. Exists so a test can pin the
    /// budget instead of racing the 1 ms timer: zero to exercise the
    /// off-thread path, or a wall-clock eternity when the subject is what the
    /// parse produces rather than how long it may take.
    pub(crate) fn edit_with_budget(
        &mut self,
        doc: &CodeDocument,
        edit: &CodeEdit,
        budget: Duration,
    ) -> HighlightOutcome {
        // A single edit is trivially ordered, so the refusal arm is unreachable.
        self.edit_batch(doc, std::slice::from_ref(edit), budget)
            .unwrap_or(HighlightOutcome::Synced)
    }

    /// Fold a batch of applied edits in as **one** step (EP-009): one
    /// generation, one interpolation pass, one budgeted parse and at most one
    /// deferred parse, however many hunks an external reload produced. `doc`
    /// must already reflect every edit.
    ///
    /// The batch has to descend - each hunk strictly above the previous one by
    /// row, with no byte overlap - which is the order [`super::edit::disk_splices`]
    /// emits and the order in which the hunks were spliced. Interpolating them
    /// in that sequence is exact: a hunk never moves the rows of the hunks that
    /// follow it. A batch that does not descend is refused untouched.
    pub(crate) fn edit_batch(
        &mut self,
        doc: &CodeDocument,
        edits: &[CodeEdit],
        budget: Duration,
    ) -> Result<HighlightOutcome, UnorderedBatch> {
        if !descends_without_overlap(edits) {
            return Err(UnorderedBatch);
        }
        if edits.is_empty() {
            return Ok(HighlightOutcome::Synced);
        }
        // Whatever parse was in flight is for text that no longer exists.
        self.cancel_deferred();
        self.generation = self.generation.wrapping_add(1);
        for (edit, line_count) in edits.iter().zip(intermediate_line_counts(doc, edits)) {
            self.interpolate(line_count, edit);
        }
        if !self.enabled {
            self.row_states.fill(RowState::Fresh);
            return Ok(HighlightOutcome::Synced);
        }

        for edit in edits {
            let input = InputEdit {
                start_byte: edit.start_byte,
                old_end_byte: edit.old_end_byte,
                new_end_byte: edit.new_end_byte,
                start_position: point(edit.start_point.row, edit.start_point.column),
                old_end_position: point(edit.old_end_point.row, edit.old_end_point.column),
                new_end_position: point(edit.new_end_point.row, edit.new_end_point.column),
            };
            for pass in &mut self.passes {
                if let Some(tree) = pass.tree.as_mut() {
                    tree.edit(&input);
                }
            }
        }

        // One budget for the whole batch, not one per pass: Markdown must not
        // get twice the stall budget of every other file type.
        let deadline = Instant::now() + budget;
        // Issue #450: a hunk's bytes are recorded in the coordinate space it
        // was applied in, and the batch descends, so every later hunk sits
        // above it and moved it. `changed_ranges` below is already in final
        // coordinates; these have to be translated into it, the same way
        // `edit::shift_selection_for_splices` carries the caret.
        let mut dirty: Vec<Range<usize>> = Vec::with_capacity(edits.len());
        let mut shift = 0isize;
        for edit in edits.iter().rev() {
            let start = (edit.start_byte as isize + shift).max(0) as usize;
            let end = (edit.new_end_byte.max(edit.start_byte) as isize + shift).max(0) as usize;
            dirty.push(start..end);
            shift += edit.new_end_byte as isize - edit.old_end_byte as isize;
        }
        let mut without_old_tree = false;
        let mut deferred = false;
        for pass in &mut self.passes {
            let old = pass.tree.clone();
            match parse_rope(
                &mut pass.parser,
                doc.text(),
                old.as_ref(),
                Some(deadline),
                None,
            ) {
                Some(new_tree) => {
                    match old.as_ref() {
                        Some(old) => dirty.extend(
                            old.changed_ranges(&new_tree)
                                .map(|range| range.start_byte..range.end_byte),
                        ),
                        None => without_old_tree = true,
                    }
                    pass.tree = Some(new_tree);
                }
                None => {
                    // An aborted parse leaves the parser mid-document; it must
                    // be reset before it is usable again. The edited tree is
                    // kept as the base for the off-thread retry.
                    pass.parser.reset();
                    deferred = true;
                }
            }
        }

        if without_old_tree {
            self.mark_stale(0..doc.line_count());
        } else {
            for range in &dirty {
                let rows = self.dirty_rows(doc, range);
                self.mark_stale(rows);
            }
        }

        if deferred {
            let cancel = Arc::new(AtomicBool::new(false));
            self.deferred_cancel = Some(cancel.clone());
            return Ok(HighlightOutcome::Deferred(DeferredParse {
                generation: self.generation,
                rope: doc.text().clone(),
                passes: self
                    .passes
                    .iter()
                    .map(|p| (p.grammar, p.tree.clone()))
                    .collect(),
                cancel,
                timeout: None,
            }));
        }

        Ok(HighlightOutcome::Synced)
    }

    /// Install an off-thread parse. Returns `false` - changing nothing, so
    /// the caller must not repaint - when the parse was cancelled, when
    /// another edit landed in the meantime, or when the document no longer
    /// matches the text that was parsed. An initial parse that timed out is
    /// applied as a give-up: the file turns plain and
    /// [`Self::is_too_complex`] goes up.
    pub(crate) fn apply_parsed(&mut self, doc: &CodeDocument, parsed: ParsedTrees) -> bool {
        if parsed.cancelled
            || parsed.generation != self.generation
            || parsed.len_bytes != doc.len_bytes()
        {
            return false;
        }
        if parsed.trees.len() != self.passes.len() {
            return false;
        }
        if parsed.timed_out {
            log::warn!(
                "coloring gave up on {}: its parse ran past {:?}",
                doc.path().display(),
                INITIAL_PARSE_TIMEOUT
            );
            self.deferred_cancel = None;
            self.enabled = false;
            self.too_complex = true;
            self.passes = Vec::new();
            self.rows = Vec::new();
            self.row_states = Vec::new();
            return true;
        }
        for (pass, tree) in self.passes.iter_mut().zip(parsed.trees) {
            if tree.is_some() {
                pass.tree = tree;
            }
        }
        self.deferred_cancel = None;
        self.mark_stale(0..doc.line_count());
        true
    }

    /// Shift the cached runs to match the edited text without parsing anything
    /// (`zed:crates/language/src/syntax_map.rs::interpolate`). Runs left of the
    /// edit keep their columns, runs right of it move with the text, and rows
    /// the edit added or removed are spliced in or out. The rows the edit
    /// touched are marked stale; any run the edit straddles is truncated
    /// rather than guessed at - a missing color for one frame reads better
    /// than a wrong one.
    ///
    /// `line_count` is the document's line count *right after this edit*: for
    /// a batch that is an intermediate count, not the final one, or the tail
    /// rows would be truncated or padded before the lower hunks shift them.
    fn interpolate(&mut self, line_count: usize, edit: &CodeEdit) {
        let start_row = edit.start_point.row;
        let old_end_row = edit.old_end_point.row;
        let new_end_row = edit.new_end_point.row;
        interpolate_rows(&mut self.rows, line_count, edit);
        if start_row >= self.row_states.len() {
            self.row_states.resize(line_count, RowState::Stale);
            return;
        }
        let removed_end = (old_end_row + 1).min(self.row_states.len());
        let replacement = vec![RowState::Stale; new_end_row - start_row + 1];
        self.row_states.splice(start_row..removed_end, replacement);
        self.row_states.resize(line_count, RowState::Stale);
    }

    /// Rows overlapping `bytes`, clamped to the document.
    fn dirty_rows(&self, doc: &CodeDocument, bytes: &Range<usize>) -> Range<usize> {
        let lines = doc.line_count();
        let first = doc.byte_to_line(bytes.start);
        let last = doc.byte_to_line(bytes.end.max(bytes.start));
        first..(last + 1).min(lines)
    }

    /// Re-run every grammar's query over `rows` only and rebuild their runs.
    /// The query is bounded with `QueryCursor::set_byte_range`, so its cost
    /// follows the requested rows, not the file. A treeless highlighter leaves
    /// the rows stale. Production goes through [`Self::fill_stale_rows`],
    /// which carries the frame deadline; the tests and the bench query
    /// whole ranges without one.
    #[cfg(test)]
    pub(crate) fn requery_rows(&mut self, doc: &CodeDocument, rows: Range<usize>) {
        self.requery_rows_until(doc, rows, None);
    }

    /// [`Self::requery_rows`] with two bounds on the enumeration itself, not
    /// only on what is stored: the cursor stops once every requested row has
    /// reached [`MAX_CAPTURES_PER_ROW`] (captures arrive in document order,
    /// so nothing past that point could be kept), and once `deadline` has
    /// passed, checked every [`DEADLINE_CHECK_STRIDE`] captures. A row cut
    /// short by the deadline keeps what was gathered and is still marked
    /// fresh: a single 2 MB minified row must not be retried every frame.
    fn requery_rows_until(
        &mut self,
        doc: &CodeDocument,
        rows: Range<usize>,
        deadline: Option<Instant>,
    ) {
        let lines = doc.line_count();
        if self.rows.len() != lines {
            self.rows.resize(lines, Vec::new());
        }
        if self.row_states.len() != lines {
            self.row_states.resize(lines, RowState::Stale);
        }
        if !self.has_tree() {
            return;
        }
        let rows = rows.start.min(lines)..rows.end.min(lines);
        if rows.is_empty() {
            return;
        }
        let start_byte = doc.line_to_byte(rows.start);
        let end_byte = if rows.end < lines {
            doc.line_to_byte(rows.end)
        } else {
            doc.len_bytes()
        };
        let line_ranges = rows
            .clone()
            .filter_map(|row| doc.line_byte_range(row))
            .collect::<Vec<_>>();
        let mut capture_counts = vec![0usize; line_ranges.len()];
        let mut saturated_rows = 0usize;
        let mut pulled = 0usize;
        for row in rows.clone() {
            self.rows[row].clear();
        }

        'passes: for (pass_index, pass) in self.passes.iter().enumerate() {
            if saturated_rows == capture_counts.len() {
                break;
            }
            let Some(tree) = pass.tree.as_ref() else {
                continue;
            };
            let Ok(pass_index) = u16::try_from(pass_index) else {
                continue;
            };
            let mut cursor = QueryCursor::new();
            cursor.set_byte_range(start_byte..end_byte);
            let mut caps =
                cursor.captures(&pass.grammar.query, tree.root_node(), RopeText(doc.text()));
            while let Some((mat, idx)) = caps.next() {
                pulled += 1;
                #[cfg(test)]
                {
                    self.captures_pulled += 1;
                }
                if pulled.is_multiple_of(DEADLINE_CHECK_STRIDE)
                    && deadline.is_some_and(|deadline| Instant::now() >= deadline)
                {
                    break 'passes;
                }
                let cap = mat.captures()[*idx];
                let Ok(capture) = u16::try_from(cap.index) else {
                    continue;
                };
                if pass
                    .colors
                    .get(capture as usize)
                    .copied()
                    .flatten()
                    .is_none()
                {
                    continue;
                }
                saturated_rows += bucket_capture(
                    cap.node.start_byte(),
                    cap.node.end_byte(),
                    IndexedCapture {
                        pass: pass_index,
                        capture,
                    },
                    &rows,
                    &line_ranges,
                    &mut capture_counts,
                    &mut self.rows,
                );
                if saturated_rows == capture_counts.len() {
                    break 'passes;
                }
            }
        }

        for row in rows {
            resolve_indexed_runs(&mut self.rows[row]);
            self.row_states[row] = RowState::Fresh;
        }
    }

    /// Color the stale rows of `rows`, one at a time, until `budget` runs out.
    /// The frame calls this for its viewport; whatever is left stale renders
    /// plain and is reported so the caller can ask for another frame. A
    /// plain file, or one whose tree has not landed, fills nothing and
    /// reports nothing stale.
    pub(crate) fn fill_stale_rows(
        &mut self,
        doc: &CodeDocument,
        rows: Range<usize>,
        budget: Duration,
    ) -> StaleFill {
        if !self.enabled || !self.has_tree() {
            return StaleFill::default();
        }
        let lines = doc.line_count();
        let rows = rows.start.min(lines)..rows.end.min(lines);
        let deadline = Instant::now() + budget;
        for row in rows.clone() {
            if self.row_states.get(row) != Some(&RowState::Stale) {
                continue;
            }
            self.requery_rows_until(doc, row..row + 1, Some(deadline));
            if Instant::now() >= deadline {
                break;
            }
        }
        let mut stale_rows = 0usize;
        for row in rows {
            if self.row_states.get(row) == Some(&RowState::Stale) {
                stale_rows += 1;
            }
        }
        StaleFill { stale_rows }
    }

    fn mark_stale(&mut self, rows: Range<usize>) {
        let end = rows.end.min(self.row_states.len());
        for state in &mut self.row_states[rows.start.min(end)..end] {
            *state = RowState::Stale;
        }
    }

    fn cancel_deferred(&mut self) {
        if let Some(cancel) = self.deferred_cancel.take() {
            cancel.store(true, Ordering::Relaxed);
        }
    }
}

impl Drop for CodeHighlighter {
    fn drop(&mut self) {
        self.cancel_deferred();
    }
}

#[cfg(test)]
impl CodeHighlighter {
    /// The deferred initial parse, run and applied in line: what a test wants
    /// when the subject is anything but the deferral itself.
    pub(crate) fn parse_initial_blocking(&mut self, doc: &CodeDocument) -> bool {
        let Some(parse) = self.initial_parse(doc) else {
            return false;
        };
        let parsed = parse.run();
        self.apply_parsed(doc, parsed)
    }

    /// Identities of the root node's children in the block-grammar tree.
    /// tree-sitter reuses untouched subtrees verbatim across an incremental
    /// parse, so a stable id is direct evidence the subtree was not re-parsed.
    fn root_child_ids(&self) -> Vec<usize> {
        self.passes
            .first()
            .and_then(|p| p.tree.as_ref())
            .map(|tree| {
                let root = tree.root_node();
                let mut cursor = root.walk();
                root.children(&mut cursor).map(|n| n.id()).collect()
            })
            .unwrap_or_default()
    }

    fn all_rows_stale(&self) -> bool {
        self.row_states
            .iter()
            .all(|state| *state == RowState::Stale)
    }

    fn has_stale_rows(&self) -> bool {
        self.row_states.contains(&RowState::Stale)
    }
}

/// The color of every capture of `grammar`'s query under `syntax`, indexed by
/// capture index.
fn capture_colors(grammar: &Grammar, syntax: &DiffSyntax) -> Vec<Option<Hsla>> {
    grammar
        .query
        .capture_names()
        .iter()
        .map(|name| syntax.color_for_capture(name))
        .collect()
}

/// Whether a batch is applicable in sequence: every hunk strictly above the
/// one before it by row, and never overlapping it by byte. Two hunks on one
/// row are refused too, since the second would read the first's shifted
/// columns.
fn descends_without_overlap(edits: &[CodeEdit]) -> bool {
    edits.windows(2).all(|pair| {
        pair[1].old_end_point.row < pair[0].start_point.row
            && pair[1].old_end_byte <= pair[0].start_byte
    })
}

/// The document's line count after each edit of a descending batch, given
/// that `doc` already holds the result of all of them: the count after
/// `edits[i]` is the final count minus the rows every later hunk added.
fn intermediate_line_counts(doc: &CodeDocument, edits: &[CodeEdit]) -> Vec<usize> {
    let mut counts = vec![0usize; edits.len()];
    let mut count = doc.line_count() as isize;
    for (index, edit) in edits.iter().enumerate().rev() {
        counts[index] = count.max(0) as usize;
        count -= edit.new_end_point.row as isize - edit.old_end_point.row as isize;
    }
    counts
}

/// The row-splice half of interpolation: runs left of the edit keep their
/// columns, runs right of it move with the text, rows the edit added or
/// removed are spliced in or out.
fn interpolate_rows(rows: &mut Vec<IndexedLineRuns>, line_count: usize, edit: &CodeEdit) {
    let start_row = edit.start_point.row;
    let old_end_row = edit.old_end_point.row;
    let new_end_row = edit.new_end_point.row;
    if start_row >= rows.len() {
        rows.resize(line_count, Vec::new());
        return;
    }

    let start_col = edit.start_point.column;
    let old_end_col = edit.old_end_point.column;
    let new_end_col = edit.new_end_point.column;
    let prefix = rows[start_row]
        .iter()
        .filter_map(|&(start, end, capture)| {
            let start = start as usize;
            let end = (end as usize).min(start_col);
            (start < start_col && start < end).then_some((start as u32, end as u32, capture))
        })
        .collect::<IndexedLineRuns>();
    let suffix = rows
        .get(old_end_row)
        .map(|runs| {
            runs.iter()
                .filter_map(|&(start, end, capture)| {
                    let start = start as usize;
                    let end = end as usize;
                    if end <= old_end_col {
                        return None;
                    }
                    let shifted_start = start.max(old_end_col) - old_end_col + new_end_col;
                    let shifted_end = end - old_end_col + new_end_col;
                    Some((
                        u32::try_from(shifted_start).ok()?,
                        u32::try_from(shifted_end).ok()?,
                        capture,
                    ))
                })
                .collect::<IndexedLineRuns>()
        })
        .unwrap_or_default();

    let mut replacement = Vec::with_capacity(new_end_row - start_row + 1);
    if new_end_row == start_row {
        let mut merged = prefix;
        merged.extend(suffix);
        replacement.push(merged);
    } else {
        replacement.push(prefix);
        replacement.resize(new_end_row - start_row, Vec::new());
        replacement.push(suffix);
    }

    let removed_end = (old_end_row + 1).min(rows.len());
    rows.splice(start_row..removed_end, replacement);
    rows.resize(line_count, Vec::new());
}

/// Split one capture across the rows it covers, pushing line-relative runs.
/// Same contract as `highlighter.rs::bucket_capture`, resolved against the
/// byte ranges of the queried rows alone (a `partition_point` over a
/// viewport-sized slice), never a materialized `Vec` of every line in the
/// file. A row stops accepting captures at [`MAX_CAPTURES_PER_ROW`]; returns
/// how many rows this capture brought to that cap, so the caller can stop
/// enumerating once every row it asked for is full.
fn bucket_capture(
    cstart: usize,
    cend: usize,
    capture: IndexedCapture,
    rows: &Range<usize>,
    line_ranges: &[Range<usize>],
    capture_counts: &mut [usize],
    out: &mut [IndexedLineRuns],
) -> usize {
    let mut newly_saturated = 0usize;
    if cend <= cstart {
        return newly_saturated;
    }
    let mut local_row = line_ranges.partition_point(|range| range.end <= cstart);
    while let Some(lr) = line_ranges.get(local_row) {
        if lr.start >= cend {
            break;
        }
        let s = cstart.max(lr.start).saturating_sub(lr.start);
        let e = cend.min(lr.end).saturating_sub(lr.start);
        if e > s
            && capture_counts[local_row] < MAX_CAPTURES_PER_ROW
            && let (Ok(s), Ok(e)) = (u32::try_from(s), u32::try_from(e))
        {
            out[rows.start + local_row].push((s, e, capture));
            capture_counts[local_row] += 1;
            if capture_counts[local_row] == MAX_CAPTURES_PER_ROW {
                newly_saturated += 1;
            }
        }
        local_row += 1;
    }
    newly_saturated
}

/// [`resolve_runs`] over the indexed storage: widen to `usize` ranges, resolve
/// the overlaps exactly as the diff does, narrow back.
fn resolve_indexed_runs(runs: &mut IndexedLineRuns) {
    let mut expanded = runs
        .drain(..)
        .map(|(start, end, capture)| (start as usize..end as usize, capture))
        .collect::<Vec<_>>();
    resolve_runs(&mut expanded);
    runs.extend(expanded.into_iter().filter_map(|(range, capture)| {
        Some((
            u32::try_from(range.start).ok()?,
            u32::try_from(range.end).ok()?,
            capture,
        ))
    }));
}

/// Feeds tree-sitter's query engine straight from the rope's chunks - no
/// full-text copy is ever materialized for a query.
struct RopeText<'a>(&'a Rope);

type ChunkBytes<'a> = std::iter::Map<ropey::iter::Chunks<'a>, fn(&'a str) -> &'a [u8]>;

impl<'a> TextProvider<&'a [u8]> for RopeText<'a> {
    type I = ChunkBytes<'a>;

    fn text(&mut self, node: Node) -> Self::I {
        let len = self.0.len_bytes();
        let range = node.byte_range();
        let end = range.end.min(len);
        let start = range.start.min(end);
        self.0
            .byte_slice(start..end)
            .chunks()
            .map(str::as_bytes as fn(&str) -> &[u8])
    }
}

/// Parse `rope` incrementally, reading it chunk by chunk, and abort past
/// `deadline` or once `cancel` is set. Returns `None` when the parse was
/// aborted (the caller must then [`Parser::reset`]) or when tree-sitter failed
/// outright.
fn parse_rope(
    parser: &mut Parser,
    rope: &Rope,
    old: Option<&Tree>,
    deadline: Option<Instant>,
    cancel: Option<&AtomicBool>,
) -> Option<Tree> {
    // A budget already spent means the parse never starts. Without this, a
    // zero budget would still run to completion on a small file, because the
    // progress callback only fires every few thousand nodes.
    if deadline.is_some_and(|deadline| Instant::now() >= deadline)
        || cancel.is_some_and(|cancel| cancel.load(Ordering::Relaxed))
    {
        return None;
    }
    let len = rope.len_bytes();
    let mut read = |byte: usize, _pos: TsPoint| -> &[u8] {
        if byte >= len {
            return &[];
        }
        let (chunk, chunk_start, _, _) = rope.chunk_at_byte(byte);
        &chunk.as_bytes()[byte - chunk_start..]
    };
    let mut progress = |_state: &ParseState| -> ControlFlow<()> {
        if deadline.is_some_and(|deadline| Instant::now() >= deadline)
            || cancel.is_some_and(|cancel| cancel.load(Ordering::Relaxed))
        {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    };
    let options = ParseOptions::new().progress_callback(&mut progress);
    parser.parse_with_options(&mut read, old, Some(options))
}

const fn point(row: usize, column: usize) -> TsPoint {
    TsPoint { row, column }
}

/// Run `deferred` off the render thread and hand the trees back to `view` on
/// the main thread. Mirrors `load::spawn_code_load`: a closed tab makes the
/// `WeakEntity` update fail silently, and `apply` is responsible for its own
/// `cx.notify()` - it is simply never reached for a dead entity. Under test
/// the parse rides GPUI's background executor, so `run_until_parked` drives
/// it deterministically.
pub(crate) fn spawn_deferred_parse<V, F>(deferred: DeferredParse, cx: &mut Context<V>, apply: F)
where
    V: 'static,
    F: FnOnce(&mut V, ParsedTrees, &mut Context<V>) + 'static,
{
    cx.spawn(async move |this: WeakEntity<V>, cx: &mut AsyncApp| {
        #[cfg(not(test))]
        let parsed = smol::unblock(move || deferred.run()).await;
        #[cfg(test)]
        let parsed = cx.background_spawn(async move { deferred.run() }).await;
        cx.update(|cx| {
            let _ = this.update(cx, |view: &mut V, cx: &mut Context<V>| {
                apply(view, parsed, cx);
            });
        });
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::diff::{MAX_HIGHLIGHT_BYTES, MAX_MARKDOWN_HIGHLIGHT_BYTES, highlight_lines};
    use crate::theme::paneflow_dark;

    fn syntax() -> DiffSyntax {
        DiffSyntax::from_theme(&paneflow_dark())
    }

    fn doc(name: &str, text: &str) -> CodeDocument {
        CodeDocument::new(PathBuf::from(format!("/tmp/{name}")), text)
    }

    /// A highlighter whose initial parse has already landed: what every test
    /// wants unless the deferral itself is the subject.
    fn parsed(doc: &CodeDocument) -> CodeHighlighter {
        let mut highlighter = CodeHighlighter::new(doc, syntax());
        highlighter.parse_initial_blocking(doc);
        highlighter
    }

    fn fill_all(highlighter: &mut CodeHighlighter, document: &CodeDocument) {
        highlighter.requery_rows(document, 0..document.line_count());
    }

    /// One sample per arm of `diff/highlighter.rs::grammar_for_ext`, so the
    /// parity assertion below covers every grammar the diff can select. The
    /// samples are the diff's own regression corpus (issue #433), so the
    /// editor and the diff are measured on the same bytes.
    fn corpus() -> Vec<(&'static str, &'static str)> {
        crate::diff::parity_tests::CORPUS.to_vec()
    }

    /// The runs the diff would produce for `text`, padded to the document's
    /// line count. `str::lines()` drops the empty line a trailing `\n` implies,
    /// while the editor keeps it (a cursor can sit there), so the editor has at
    /// most one extra row and it must be empty.
    fn expected_rows(text: &str, ext: &str, lines: usize) -> Vec<LineRuns> {
        let mut rows = highlight_lines(text, ext, &syntax());
        assert!(
            rows.len() <= lines,
            "diff produced more rows ({}) than the document has lines ({lines})",
            rows.len()
        );
        rows.resize(lines, Vec::new());
        rows
    }

    fn assert_parity(name: &str, text: &str) {
        let d = doc(name, text);
        let mut h = parsed(&d);
        assert!(h.is_enabled(), "{name} resolved no grammar");
        assert!(h.all_rows_stale(), "{name} queried during construction");
        fill_all(&mut h, &d);
        let expected = expected_rows(text, d.ext(), d.line_count());
        for (row, want) in expected.iter().enumerate() {
            assert_eq!(
                h.runs(row),
                want.as_slice(),
                "{name} row {row} diverges from the diff: {:?}",
                d.line_string(row)
            );
        }
    }

    #[test]
    fn parity_matches_the_diff_highlighter_on_every_grammar() {
        for (name, text) in corpus() {
            assert_parity(name, text);
        }
    }

    #[test]
    fn parity_holds_after_an_incremental_edit_on_every_grammar() {
        for (name, text) in corpus() {
            let mut d = doc(name, text);
            let h = &mut parsed(&d);
            fill_all(h, &d);
            // Insert a newline plus a space at the start of the second line:
            // it shifts every byte after it and adds a row, so a stale tree or
            // a bad `InputEdit` shows up immediately.
            let at = d.line_to_byte(1);
            let edit = d.insert(at, "\n ").expect("insert");
            // A wall-clock eternity rather than the production budget: the
            // subject is the runs the reparse produces, and a loaded runner
            // can blow past 1 ms on any grammar here.
            let outcome = h.edit_with_budget(&d, &edit, Duration::from_secs(5));
            assert!(
                matches!(outcome, HighlightOutcome::Synced),
                "{name} deferred"
            );

            let after = d.to_disk_string();
            fill_all(h, &d);
            let expected = expected_rows(&after, d.ext(), d.line_count());
            for (row, want) in expected.iter().enumerate() {
                assert_eq!(
                    h.runs(row),
                    want.as_slice(),
                    "{name} row {row} diverges after an edit: {:?}",
                    d.line_string(row)
                );
            }
        }
    }

    #[test]
    fn a_keystroke_reuses_the_existing_tree_instead_of_reparsing_the_file() {
        let mut text = String::new();
        for i in 0..400 {
            text.push_str(&format!(
                "pub fn f{i}(a: i32) -> i32 {{\n    a + {i}\n}}\n\n"
            ));
        }
        let mut d = doc("big.rs", &text);
        let mut h = parsed(&d);
        let before = h.root_child_ids();
        assert!(before.len() >= 400);

        // Type one character inside the very first function.
        let at = d.line_to_byte(1) + 4;
        let edit = d.insert(at, "1").expect("insert");
        // An explicit, generous budget instead of the production 1 ms one: the
        // subject here is subtree reuse, not the deadline, and a wall-clock
        // budget makes the assertion depend on how loaded the machine running
        // the suite happens to be (it defers, and the test fails, roughly one
        // run in five under a fully parallel `cargo test`).
        assert!(matches!(
            h.edit_with_budget(&d, &edit, Duration::from_secs(5)),
            HighlightOutcome::Synced
        ));

        let after = h.root_child_ids();
        assert_eq!(after.len(), before.len());
        let reused = before.iter().zip(&after).filter(|(a, b)| a == b).count();
        // A tree-sitter node id is the address of its subtree, so an id that
        // survives an edit is a subtree that was reused verbatim rather than
        // re-parsed. A from-scratch parse allocates a fresh tree and shares
        // almost nothing, which is the control this asserts against.
        let fresh = parsed(&d).root_child_ids();
        let coincidental = after.iter().zip(&fresh).filter(|(a, b)| a == b).count();
        assert!(
            reused * 10 >= before.len() * 9,
            "only {reused}/{} subtrees were reused - the parse was not incremental",
            before.len()
        );
        assert!(
            coincidental * 10 < before.len(),
            "the from-scratch control shared {coincidental}/{} subtrees, so id identity \
             proves nothing here",
            before.len()
        );
    }

    #[test]
    fn a_blown_budget_defers_the_parse_until_visible_rows_are_refilled() {
        let text = "fn main() {\n    let s = \"hello\";\n    println!(\"{s}\");\n}\n";
        let mut d = doc("deferred.rs", text);
        let mut h = parsed(&d);
        fill_all(&mut h, &d);
        let colored_before = h.runs(1);
        assert!(!colored_before.is_empty());

        let at = d.line_to_byte(1);
        let edit = d.insert(at, "    // note\n").expect("insert");
        let HighlightOutcome::Deferred(deferred) = h.edit_with_budget(&d, &edit, Duration::ZERO)
        else {
            panic!("a zero budget must defer");
        };
        assert_eq!(deferred.generation(), h.generation());
        // The rows the edit touched are stale, so they render plain until a
        // frame refills them.
        assert!(h.runs(1).is_empty());
        assert!(h.runs(2).is_empty());

        // The off-thread result restores exact parity with the diff once the
        // rows are refilled.
        assert!(h.apply_parsed(&d, deferred.run()));
        assert!(h.all_rows_stale());
        fill_all(&mut h, &d);
        let after = d.to_disk_string();
        let expected = expected_rows(&after, d.ext(), d.line_count());
        for (row, want) in expected.iter().enumerate() {
            assert_eq!(h.runs(row), want.as_slice(), "row {row}");
        }
    }

    #[test]
    fn a_deferred_parse_from_a_superseded_generation_is_dropped() {
        let text = "fn main() {\n    let s = \"hello\";\n}\n";
        let mut d = doc("stale.rs", text);
        let mut h = parsed(&d);
        fill_all(&mut h, &d);

        let first = d.insert(0, "//x\n").expect("insert");
        let HighlightOutcome::Deferred(stale) = h.edit_with_budget(&d, &first, Duration::ZERO)
        else {
            panic!("a zero budget must defer");
        };

        // A second keystroke lands while the first parse is still running.
        let second = d.insert(0, "//y\n").expect("insert");
        let _ = h.edit(&d, &second);
        let snapshot: Vec<_> = (0..d.line_count()).map(|r| h.runs(r)).collect();

        // The older result is cancelled and rejected, and nothing changes -
        // so the caller never repaints.
        let parsed = stale.run();
        assert!(parsed.cancelled);
        assert!(!h.apply_parsed(&d, parsed));
        let after: Vec<_> = (0..d.line_count()).map(|r| h.runs(r)).collect();
        assert_eq!(snapshot, after);
    }

    #[test]
    fn only_the_latest_deferred_parse_survives_an_edit_burst() {
        let mut d = doc("burst.rs", "fn main() { let value = 1; }\n");
        let mut h = parsed(&d);
        let first_edit = d.insert(0, "x").expect("first insert");
        let HighlightOutcome::Deferred(first) = h.edit_with_budget(&d, &first_edit, Duration::ZERO)
        else {
            panic!("first parse must defer");
        };
        let second_edit = d.insert(0, "y").expect("second insert");
        let HighlightOutcome::Deferred(second) =
            h.edit_with_budget(&d, &second_edit, Duration::ZERO)
        else {
            panic!("second parse must defer");
        };

        assert!(first.run().was_cancelled());
        let latest = second.run();
        assert!(!latest.was_cancelled());
        assert!(h.apply_parsed(&d, latest));
    }

    #[test]
    fn dropping_the_highlighter_cancels_its_deferred_parse() {
        let mut d = doc("drop.rs", "fn main() {}\n");
        let mut h = parsed(&d);
        let edit = d.insert(0, "x").expect("insert");
        let HighlightOutcome::Deferred(parse) = h.edit_with_budget(&d, &edit, Duration::ZERO)
        else {
            panic!("parse must defer");
        };
        drop(h);
        assert!(parse.run().was_cancelled());
    }

    #[test]
    fn dropping_the_highlighter_cancels_its_initial_parse() {
        let d = doc("closed.rs", "fn main() {}\n");
        let mut h = CodeHighlighter::new(&d, syntax());
        let parse = h.initial_parse(&d).expect("the initial parse is deferred");
        drop(h);
        assert!(parse.run().was_cancelled());
    }

    #[test]
    fn a_keystroke_before_the_first_tree_cancels_it_and_reparses_from_nothing() {
        let mut d = doc("typed.rs", &rows_of_code(400));
        let mut h = CodeHighlighter::new(&d, syntax());
        let initial = h.initial_parse(&d).expect("the initial parse is deferred");

        let edit = d.insert(0, "//\n").expect("insert");
        let HighlightOutcome::Deferred(after_edit) = h.edit_with_budget(&d, &edit, Duration::ZERO)
        else {
            panic!("a treeless reparse must defer");
        };
        assert_eq!(h.generation(), 1, "the keystroke advanced the generation");
        assert!(
            initial.run().was_cancelled(),
            "the initial parse was cancelled by the keystroke"
        );
        assert_eq!(
            after_edit.generation(),
            1,
            "the replacement parse carries the new generation"
        );

        assert!(h.apply_parsed(&d, after_edit.run()));
        assert!(h.has_tree(), "the replacement parse installed the tree");
        assert!(h.all_rows_stale(), "a first tree leaves every row stale");
        fill_all(&mut h, &d);
        let expected = expected_rows(&d.to_disk_string(), d.ext(), d.line_count());
        for (row, want) in expected.iter().enumerate() {
            assert_eq!(h.runs(row), want.as_slice(), "row {row} diverges");
        }
    }

    #[test]
    fn an_initial_parse_past_its_timeout_gives_up_and_greys_the_file() {
        let d = doc("slow.rs", &rows_of_code(4_000));
        let mut h = CodeHighlighter::new(&d, syntax());
        let parse = h
            .initial_parse(&d)
            .expect("the initial parse is deferred")
            .with_timeout_for_test(Duration::ZERO);

        let parsed = parse.run();
        assert!(!parsed.was_cancelled(), "a timeout is not a cancellation");
        assert!(h.apply_parsed(&d, parsed), "the timeout is applied");

        assert!(h.is_too_complex(), "the tab reports the give-up");
        assert!(!h.is_enabled(), "the file stays grey");
        assert!(!h.has_tree());
        assert!(h.rows.is_empty(), "no per-row storage is kept");
        assert!(h.runs(0).is_empty());
        assert!(
            !h.fill_stale_rows(&d, 0..d.line_count(), Duration::from_secs(1))
                .any_stale()
        );
    }

    #[test]
    fn unfinished_visible_rows_are_left_stale_for_the_next_frame() {
        let d = doc(
            "progressive.rs",
            "fn one() {}\nfn two() {}\nfn three() {}\n",
        );
        let mut h = parsed(&d);
        assert_eq!(h.fill_stale_rows(&d, 0..3, Duration::ZERO).stale_rows, 2);
        assert!(!h.runs(0).is_empty());
        assert!(h.runs(1).is_empty());
        assert!(
            !h.fill_stale_rows(&d, 0..3, Duration::from_secs(1))
                .any_stale()
        );
        assert!(!h.runs(1).is_empty());
        assert!(!h.runs(2).is_empty());
    }

    /// One row holding more colored captures than [`MAX_CAPTURES_PER_ROW`]:
    /// the shape of a minified file. Captures arrive in document order, so
    /// doubling the row cannot move the point where the cap is reached: the
    /// pull count is the same for both, which is what proves the enumeration
    /// stopped at the cap instead of walking the whole row.
    #[test]
    fn a_saturated_row_stops_the_query_at_the_capture_cap() {
        let statement = "let a = 1; ";
        let short = format!("fn f() {{ {} }}\n", statement.repeat(MAX_CAPTURES_PER_ROW));
        let long = format!(
            "fn f() {{ {} }}\n",
            statement.repeat(MAX_CAPTURES_PER_ROW * 2)
        );

        let d = doc("short.rs", &short);
        let mut h = parsed(&d);
        h.requery_rows(&d, 0..1);
        let pulled_short = h.captures_pulled;
        let stored = h.runs(0).len();
        assert!(
            stored > 0 && stored <= MAX_CAPTURES_PER_ROW,
            "the row stores at most the cap (overlaps resolve down), got {stored}"
        );
        assert!(!h.all_rows_stale() && h.row_states[0] == RowState::Fresh);

        let d = doc("long.rs", &long);
        let mut h = parsed(&d);
        h.requery_rows(&d, 0..1);
        assert!(h.runs(0).len() <= MAX_CAPTURES_PER_ROW);
        assert!(pulled_short > 0);
        assert_eq!(
            h.captures_pulled, pulled_short,
            "a row twice as long pulls no more captures once the cap is reached"
        );
        assert!(
            h.captures_pulled < MAX_CAPTURES_PER_ROW * 2,
            "well under the {} statements the long row holds",
            MAX_CAPTURES_PER_ROW * 2
        );
    }

    /// A deadline that has already passed cuts the row's query short but
    /// still marks the row fresh with what it gathered, so a huge single row
    /// is never re-queried frame after frame.
    #[test]
    fn a_row_cut_short_by_the_deadline_keeps_its_partial_runs_and_stays_fresh() {
        let text = format!(
            "fn f() {{ {} }}\n",
            "let a = 1; ".repeat(MAX_CAPTURES_PER_ROW)
        );
        let d = doc("deadline.rs", &text);
        let mut h = parsed(&d);
        let fill = h.fill_stale_rows(&d, 0..1, Duration::ZERO);
        assert_eq!(fill.stale_rows, 0, "the row is fresh");
        assert!(h.row_states[0] == RowState::Fresh);
        assert!(
            h.captures_pulled <= DEADLINE_CHECK_STRIDE,
            "the expired deadline stops the enumeration at the first check, pulled {}",
            h.captures_pulled
        );
        assert!(
            !h.runs(0).is_empty() && h.runs(0).len() < MAX_CAPTURES_PER_ROW,
            "the captures gathered before the check are kept"
        );
        let before = h.captures_pulled;
        assert_eq!(h.fill_stale_rows(&d, 0..1, Duration::ZERO).stale_rows, 0);
        assert_eq!(h.captures_pulled, before, "a fresh row is not re-queried");
    }

    #[test]
    fn a_viewport_past_the_end_of_the_document_is_clamped_and_counts_only_real_rows() {
        let d = doc("clamped.rs", "fn one() {}\nfn two() {}\nfn three() {}\n");
        let mut h = parsed(&d);
        assert_eq!(d.line_count(), 4);

        let starved = h.fill_stale_rows(&d, 0..10_000, Duration::ZERO);
        assert!(starved.any_stale());
        assert_eq!(starved.stale_rows, d.line_count() - 1);

        let filled = h.fill_stale_rows(&d, 0..10_000, Duration::from_secs(1));
        assert!(!filled.any_stale());
        assert_eq!(filled.stale_rows, 0);
    }

    #[test]
    fn a_markdown_file_past_its_own_cap_stays_editable_and_plain() {
        let mut text = String::with_capacity(MAX_MARKDOWN_HIGHLIGHT_BYTES + 64);
        while text.len() <= MAX_MARKDOWN_HIGHLIGHT_BYTES {
            text.push_str("# Heading with `code`, *emphasis* and [a link](https://paneflow.dev)\n");
        }
        assert!(
            text.len() < MAX_HIGHLIGHT_BYTES,
            "the markdown cap must be the lower of the two, or this test proves nothing"
        );

        let d = doc("huge.md", &text);
        let mut h = CodeHighlighter::new(&d, syntax());
        assert!(
            !h.is_enabled(),
            "markdown past its two-pass cap is left plain"
        );
        assert!(h.initial_parse(&d).is_none(), "and asks for no parse");

        let small = doc("small.md", "# Title\n\nSome `code` here.\n");
        let mut colored = parsed(&small);
        assert!(colored.is_enabled(), "markdown under the cap still colors");
        fill_all(&mut colored, &small);
        assert!(!colored.runs(0).is_empty());
    }

    #[test]
    fn a_file_past_the_highlight_cap_stays_editable_and_plain() {
        let mut text = String::with_capacity(MAX_HIGHLIGHT_BYTES + 64);
        while text.len() <= MAX_HIGHLIGHT_BYTES {
            text.push_str("pub fn f() -> i32 { 1 }\n");
        }
        let mut d = doc("huge.rs", &text);
        let mut h = parsed(&d);
        assert!(!h.is_enabled());
        assert!(h.runs(0).is_empty());

        // Still editable, and the edit is folded in without a parse.
        let edit = d.insert(0, "// still editable\n").expect("insert");
        assert!(matches!(h.edit(&d, &edit), HighlightOutcome::Synced));
        assert!(!h.has_stale_rows());
        assert!(
            !h.fill_stale_rows(&d, 0..d.line_count(), Duration::ZERO)
                .any_stale()
        );
        assert!(h.runs(0).is_empty());
        assert_eq!(d.line_string(0).as_deref(), Some("// still editable"));
    }

    #[test]
    fn an_unknown_extension_stays_editable_and_plain() {
        let mut d = doc("notes.unknownext", "anything at all\nsecond line\n");
        let mut h = parsed(&d);
        assert!(!h.is_enabled());
        let edit = d.insert(0, "x").expect("insert");
        assert!(matches!(h.edit(&d, &edit), HighlightOutcome::Synced));
        assert!(h.runs(0).is_empty());
        // The diff renders this file exactly as plainly.
        assert!(highlight_lines("anything at all\n", "unknownext", &syntax())[0].is_empty());
    }

    #[test]
    fn deleting_a_row_keeps_the_row_map_aligned_with_the_document() {
        let text = "fn a() {}\nfn b() {}\nfn c() {}\n";
        let mut d = doc("del.rs", text);
        let mut h = parsed(&d);
        fill_all(&mut h, &d);

        let start = d.line_to_byte(1);
        let end = d.line_to_byte(2);
        let edit = d.remove(start..end).expect("remove");
        // Budget pinned for the same reason as the parity tests: the subject
        // is the row map, not the 1 ms timer.
        assert!(matches!(
            h.edit_with_budget(&d, &edit, Duration::from_secs(5)),
            HighlightOutcome::Synced
        ));

        let after = d.to_disk_string();
        fill_all(&mut h, &d);
        assert_eq!(after, "fn a() {}\nfn c() {}\n");
        let expected = expected_rows(&after, "rs", d.line_count());
        for (row, want) in expected.iter().enumerate() {
            assert_eq!(h.runs(row), want.as_slice(), "row {row}");
        }
    }

    fn rows_of_code(rows: usize) -> String {
        (0..rows).map(|row| format!("fn f{row}() {{}}\n")).collect()
    }

    /// EP-009: ten hunks reach the highlighter as one batch - one generation,
    /// one deferred parse - and the deferred result restores exact parity.
    #[test]
    fn a_batch_of_hunks_advances_one_generation_and_defers_one_parse() {
        let mut d = doc("batch.rs", &rows_of_code(400));
        let mut h = parsed(&d);
        fill_all(&mut h, &d);
        let before = h.generation();

        let mut edits = Vec::with_capacity(10);
        for hunk in (0..10).rev() {
            let at = d.line_to_byte(hunk * 30 + 5);
            edits.push(d.insert(at, "// agent\n").expect("insert"));
        }

        let HighlightOutcome::Deferred(deferred) = h
            .edit_batch(&d, &edits, Duration::ZERO)
            .expect("descending hunks are a valid batch")
        else {
            panic!("a zero budget must defer");
        };
        assert_eq!(
            h.generation(),
            before + 1,
            "ten hunks are one batch, so one generation"
        );
        assert_eq!(deferred.generation(), h.generation());
        assert!(h.apply_parsed(&d, deferred.run()));
        fill_all(&mut h, &d);
        let expected = expected_rows(&d.to_disk_string(), d.ext(), d.line_count());
        for (row, want) in expected.iter().enumerate() {
            assert_eq!(h.runs(row), want.as_slice(), "row {row} after a batch");
        }
    }

    /// Issue #450: the lower hunk of a batch moves the bytes of the hunk
    /// above it, so the earlier hunk's dirty range has to be translated into
    /// the final document before it is marked stale. A rename preserves the
    /// syntax-tree shape, so `changed_ranges` contributes nothing and the
    /// translated range is the only thing that can invalidate that row.
    #[test]
    fn a_batch_requeries_the_upper_hunk_through_the_lower_hunks_shift() {
        let mut d = doc("batch.rs", &rows_of_code(200));
        let mut h = parsed(&d);
        fill_all(&mut h, &d);

        // Applied first, at the bottom: rename the identifier in place.
        let row = d.line_to_byte(150);
        let name = row + "fn ".len();
        let high = super::super::edit::splice(&mut d, name..name + "f150".len(), "renamed_symbol")
            .expect("rename")
            .edit;
        // Applied second, above it, and it changes both the row and the byte
        // count, so every byte of the rename moved.
        // Ten inserted rows, so the stale range lands clear of the renamed
        // row instead of merely straddling it.
        let low = d
            .insert(d.line_to_byte(50), &"// filler line\n".repeat(10))
            .expect("insert");

        assert!(
            matches!(
                h.edit_batch(&d, &[high, low], Duration::from_secs(5))
                    .expect("descending hunks are a valid batch"),
                HighlightOutcome::Synced
            ),
            "a real budget must parse in line"
        );

        let after = d.to_disk_string();
        assert!(
            after.contains("fn renamed_symbol() {}"),
            "the rename landed: {:?}",
            &after[d.line_to_byte(160)..d.line_to_byte(161)]
        );
        assert!(
            h.runs(160).is_empty(),
            "the renamed row must be marked stale at its shifted offset, not left interpolated"
        );
        let expected = expected_rows(&after, d.ext(), d.line_count());
        assert!(
            !expected[160].is_empty(),
            "the renamed row is colored from scratch"
        );
        assert!(
            !h.fill_stale_rows(&d, 160..161, Duration::from_secs(1))
                .any_stale()
        );
        assert_eq!(
            h.runs(160),
            expected[160].as_slice(),
            "the renamed row is requeried at its shifted offset"
        );
    }

    #[test]
    fn edit_batch_refuses_hunks_that_do_not_descend() {
        let mut d = doc("batch.rs", &rows_of_code(40));
        let mut h = parsed(&d);
        fill_all(&mut h, &d);
        let generation = h.generation();
        let low = d.insert(d.line_to_byte(5), "// a\n").expect("insert");
        let high = d.insert(d.line_to_byte(20), "// b\n").expect("insert");
        let snapshot: Vec<_> = (0..d.line_count()).map(|r| h.runs(r)).collect();

        assert!(
            h.edit_batch(&d, &[low, high], Duration::from_secs(1))
                .is_err(),
            "an ascending batch is refused"
        );
        assert_eq!(
            h.generation(),
            generation,
            "a refused batch changes nothing"
        );
        let after: Vec<_> = (0..d.line_count()).map(|r| h.runs(r)).collect();
        assert_eq!(snapshot, after);
    }

    #[test]
    fn edit_batch_refuses_hunks_that_share_a_row() {
        let mut d = doc("batch.rs", &rows_of_code(40));
        let mut h = parsed(&d);
        fill_all(&mut h, &d);
        let generation = h.generation();
        let second = d.insert(d.line_to_byte(10), "// b\n").expect("insert");
        let first = d.insert(d.line_to_byte(10), "// a\n").expect("insert");

        assert!(
            h.edit_batch(&d, &[second, first], Duration::from_secs(1))
                .is_err(),
            "two hunks on one row overlap and are refused"
        );
        assert_eq!(
            h.generation(),
            generation,
            "a refused batch changes nothing"
        );
    }

    /// The interpolation of a batch shifts the rows between and after its
    /// hunks intact: a row two hunks straddle keeps its runs, and so does the
    /// last row of the file even though the lower hunk moved it.
    #[test]
    fn a_batch_leaves_the_rows_between_its_hunks_alone() {
        let mut d = doc("batch.rs", &rows_of_code(200));
        let mut h = parsed(&d);
        fill_all(&mut h, &d);
        let quiet = h.runs(100);
        let last = h.runs(199);
        assert!(!quiet.is_empty(), "the untouched row starts colored");
        assert!(!last.is_empty(), "the last row starts colored");

        let high = d.insert(d.line_to_byte(150), "// b\n").expect("insert");
        let low = d.insert(d.line_to_byte(50), "// a\n").expect("insert");
        let HighlightOutcome::Deferred(deferred) = h
            .edit_batch(&d, &[high, low], Duration::ZERO)
            .expect("descending hunks are a valid batch")
        else {
            panic!("a zero budget must defer");
        };

        assert!(h.runs(50).is_empty(), "the lower inserted row is plain");
        assert!(h.runs(151).is_empty(), "the upper inserted row is plain");
        assert_eq!(
            h.runs(101),
            quiet.as_slice(),
            "a row between two hunks kept the runs it had, one row down"
        );
        assert_eq!(
            h.runs(201),
            last.as_slice(),
            "the last row kept its runs across both shifts"
        );
        assert_eq!(
            h.runs(199),
            quiet.as_slice(),
            "rows past the upper hunk shifted twice"
        );

        assert!(h.apply_parsed(&d, deferred.run()));
        fill_all(&mut h, &d);
        let expected = expected_rows(&d.to_disk_string(), d.ext(), d.line_count());
        for (row, want) in expected.iter().enumerate() {
            assert_eq!(h.runs(row), want.as_slice(), "row {row} after the parse");
        }
    }

    #[test]
    fn a_theme_change_recolors_without_touching_the_trees() {
        let text = "fn main() { let s = \"x\"; }\n";
        let d = doc("theme.rs", text);
        let mut h = parsed(&d);
        fill_all(&mut h, &d);
        let before = h.root_child_ids();
        let colors_before: Vec<_> = h.runs(0).iter().map(|(_, c)| *c).collect();

        let other = crate::theme::THEMES
            .iter()
            .find(|(name, _)| *name != crate::theme::DEFAULT_THEME)
            .map(|(_, build)| build())
            .expect("a second bundled theme");
        h.set_syntax(&d, DiffSyntax::from_theme(&other));

        assert_eq!(h.root_child_ids(), before, "the trees were rebuilt");
        assert!(!h.has_stale_rows(), "a theme switch queries nothing");
        let colors_after: Vec<_> = h.runs(0).iter().map(|(_, c)| *c).collect();
        assert_eq!(colors_after.len(), colors_before.len());
        assert_ne!(colors_after, colors_before);
    }

    #[test]
    fn indexed_runs_keep_the_twelve_byte_storage_contract() {
        assert_eq!(std::mem::size_of::<IndexedRun>(), 12);
    }

    #[test]
    fn an_edit_marks_changed_rows_stale_without_querying_them() {
        let mut d = doc("stale.rs", "fn one() {}\nfn two() {}\n");
        let mut h = parsed(&d);
        fill_all(&mut h, &d);
        let edit = d.insert(3, "x").expect("insert");
        assert!(matches!(
            h.edit_with_budget(&d, &edit, Duration::from_secs(5)),
            HighlightOutcome::Synced
        ));
        assert!(h.runs(0).is_empty());
        assert!(!h.runs(1).is_empty());
        assert!(
            !h.fill_stale_rows(&d, 0..1, Duration::from_secs(1))
                .any_stale()
        );
        assert!(!h.runs(0).is_empty());
    }

    #[test]
    fn opening_builds_trees_but_leaves_every_row_stale() {
        let d = doc("lazy.rs", "fn main() {}\n");
        let h = parsed(&d);
        assert!(h.has_tree());
        assert!(h.all_rows_stale());
        assert!(h.runs(0).is_empty());
    }

    #[test]
    fn opening_a_file_leaves_its_first_parse_to_the_deferred_pass() {
        let d = doc("lazy.rs", "fn main() {}\n");
        let mut h = CodeHighlighter::new(&d, syntax());
        assert!(h.is_enabled(), "the grammar resolved");
        assert!(!h.has_tree(), "the initial parse did not run inside new");
        assert!(h.all_rows_stale());
        assert!(h.runs(0).is_empty());
        assert_eq!(
            h.fill_stale_rows(&d, 0..d.line_count(), Duration::from_secs(1)),
            StaleFill::default(),
            "a treeless fill asks for no follow-up frame"
        );

        let parse = h.initial_parse(&d).expect("the initial parse is deferred");
        assert_eq!(parse.generation(), 0, "the initial parse is generation 0");
        assert!(h.apply_parsed(&d, parse.run()));

        assert!(h.has_tree(), "the deferred parse installed the tree");
        assert!(h.all_rows_stale());
        fill_all(&mut h, &d);
        assert!(!h.runs(0).is_empty(), "and the rows color from it");
        assert!(
            h.initial_parse(&d).is_none(),
            "a parsed highlighter asks for no second initial parse"
        );
    }
}
