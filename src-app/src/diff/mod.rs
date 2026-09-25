//! Single-worktree diff viewer.
//!
//! Issue #438 (upstream a8d55f74): Review stopped being one host with N
//! columns over a scope and became a grid of panes that each hold one
//! [`DiffView`] pointed at one [`ReviewSubject`]. The multi-column arranger,
//! the multi-repo host and the scope model went with that change.
//!
//! `DiffView` is an `Entity` implementing `Render + Focusable`, hosted in a
//! pane through the `PaneSurface::Diff` variant. A Review pane round-trips
//! through `SessionState::review_layout` as its subject
//! (`app/review/session.rs`), so the grid survives a relaunch.

mod align;
mod element;
mod engine;
mod extract;
mod git;
mod highlighter;
mod hit_test;
mod hscroll;
#[cfg(test)]
pub(crate) mod parity_tests;
pub(crate) mod review_terminal;
mod rows;
mod syntax;
mod view;

#[cfg(test)]
pub(crate) use git::tests::{capture_logs, captured_logs_contain};

// Only the host view and its subject/seed types are consumed outside this
// module (`pane::PaneSurface::Diff`, `app/review/`). The engine / git / rows
// types stay crate-internal, reached via `super::` paths.
pub use git::FileChange;
pub use view::{DiffView, DiffWorktree, FileEntry, FileListState, ReviewSubject};

// EP-001 (review redesign, US-001/US-002): the diff dock
// (`crate::app::diff_dock`) renders through the SAME `DiffElement` + git
// pipeline + row model as the Review view, so these are exposed crate-internally
// rather than re-implemented. Kept `pub(crate)` (not `pub`) so the unification
// surface stays inside the binary.
pub(crate) use align::CellKind;
pub(crate) use element::{DiffBody, DiffElement, revert_chip_bounds};
#[cfg(test)]
pub(crate) use engine::compute_hunks;
pub(crate) use engine::{DiffHunk, hunk_for_base_line, hunk_for_new_line};
pub(crate) use git::FileDiff;
#[cfg(test)]
pub(crate) use git::head_sha;
pub(crate) use git::{
    HeadFile, MAX_FILE_BYTES as MAX_DIFF_FILE_BYTES, compute_head_diff, is_git_worktree,
    show_revision_file,
};
#[cfg(test)]
pub(crate) use highlighter::{grammar_for_ext, markdown_inline_grammar};
pub(crate) use hit_test::row_at_offset;
pub(crate) use hscroll::{
    H_SCROLLBAR_TRACK_HEIGHT, HScrollbarSegment, file_at_row, h_offset_index, h_offset_len,
    h_scrollbar_click_offset, h_scrollbar_segments, set_file_side_offset, split_right_side_at_x,
};
pub(crate) use rows::{
    DisplayRow, FileRowCache, FileSpan, ROW_HEIGHT, RowKind, RowPalette, SplitRow,
    apply_collapse_split, apply_collapse_unified, apply_expanded_split_with_sources,
    apply_expanded_unified_with_sources, build_display_rows_with_caches, build_file_row_caches,
    build_split_rows_with_caches, discard_expanded_folds_for_path, palette, split_file_spans,
    split_max_line_no, split_offsets, unified_file_spans, unified_max_line_no, unified_offsets,
};
pub(crate) use syntax::DiffSyntax;
