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

// The Settings appearance preview paints sample diff rows with the Review
// palette and row height, so those are exposed crate-internally. Everything
// else in the engine / git / rows pipeline stays behind `super::` paths.
#[cfg(test)]
pub(crate) use git::load_column;
#[cfg(test)]
pub(crate) use highlighter::{grammar_for_ext, markdown_inline_grammar};
pub(crate) use rows::{ROW_HEIGHT, RowPalette, palette};
