//! In-buffer scrollback search for terminal panes.
//!
//! Searches the alacritty_terminal grid (scrollback + visible area) for
//! plain text or regex matches, returning grid-coordinate spans
//! that TerminalElement can highlight.

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column as GridCol, Point as AlacPoint};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::Term;
use alacritty_terminal::term::cell::Flags;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::search_engine::SearchEngine;
use crate::terminal::ZedListener;
use crate::terminal::types::Point;

/// Maximum query length (bytes). Owned by the matching engine; re-exported
/// here because `terminal/search.rs` and `app/ipc_handler.rs` clamp against it.
pub const MAX_QUERY_LEN: usize = crate::search_engine::MAX_QUERY_LEN;

/// A single search match: start and end points in the terminal grid.
#[derive(Clone, Debug)]
pub struct SearchMatch {
    pub start: Point,
    pub end: Point,
}

/// Result of a search operation.
pub struct SearchResult {
    pub matches: Vec<SearchMatch>,
    /// If regex mode and the pattern is invalid, contains the error message.
    pub regex_error: Option<String>,
}

fn extract_line_text_and_columns(
    term: &Term<ZedListener>,
    line: alacritty_terminal::index::Line,
    cols: usize,
    line_text: &mut String,
    char_to_col: &mut Vec<usize>,
) {
    line_text.clear();
    char_to_col.clear();
    line_text.reserve(cols);
    char_to_col.reserve(cols);
    for col in 0..cols {
        let cell = &term.grid()[AlacPoint::new(line, GridCol(col))];
        if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
            continue;
        }
        char_to_col.push(col);
        if cell.c == '\0' {
            line_text.push(' ');
        } else {
            line_text.push(cell.c);
        }
        // Combining marks live in the cell's zero-width extra, not in `c`.
        // Emitting them at the BASE cell's column keeps `char_to_col` a
        // char->column map, so a query containing a combining mark matches
        // and still reports the base column.
        if let Some(zero_width) = cell.zerowidth() {
            for &character in zero_width {
                char_to_col.push(col);
                line_text.push(character);
            }
        }
    }
}

/// Search the terminal's full grid (scrollback + visible) for matches.
/// In plain text mode, performs case-insensitive substring matching.
/// In regex mode, compiles the query as a regex pattern.
#[cfg(test)]
pub fn search_term(
    term: &Arc<FairMutex<Term<ZedListener>>>,
    query: &str,
    regex_mode: bool,
) -> SearchResult {
    search_term_inner(term, query, regex_mode, None, &AtomicBool::new(false))
}

/// Like [`search_term`], but abandons the scan at the next row boundary once
/// `cancelled` is set. A cancelled scan returns the matches found so far; the
/// caller is responsible for discarding them (the callers here are all
/// generation-guarded, so a cancelled result is stale by construction).
pub fn search_term_with_cancel(
    term: &Arc<FairMutex<Term<ZedListener>>>,
    query: &str,
    regex_mode: bool,
    cancelled: &AtomicBool,
) -> SearchResult {
    search_term_inner(term, query, regex_mode, None, cancelled)
}

/// Like [`search_term`], optionally limited to the most recent `max_lines`
/// grid rows (history + viewport). `None` walks the full grid. Used by
/// `surface.search` so the GPUI tick does not scan 10k history rows.
pub(crate) fn search_term_windowed(
    term: &Arc<FairMutex<Term<ZedListener>>>,
    query: &str,
    regex_mode: bool,
    max_lines: Option<usize>,
) -> SearchResult {
    search_term_inner(term, query, regex_mode, max_lines, &AtomicBool::new(false))
}

fn search_term_inner(
    term: &Arc<FairMutex<Term<ZedListener>>>,
    query: &str,
    regex_mode: bool,
    max_lines: Option<usize>,
    cancelled: &AtomicBool,
) -> SearchResult {
    let mut search = match SearchEngine::new(query, regex_mode) {
        Ok(search) => search,
        Err(error) => {
            return SearchResult {
                matches: Vec::new(),
                regex_error: Some(error.to_string()),
            };
        }
    };
    if search.is_done() {
        return search.finish();
    }

    let (top, bottom, initial_cols) = {
        let term = term.lock();
        (term.topmost_line(), term.bottommost_line(), term.columns())
    };

    // Keep the `Term` lock only while copying one row. Regex and lowercase
    // matching can be expensive on large scrollback, and holding the FairMutex
    // for the whole scan blocks PTY output processing.
    let mut line_text = String::with_capacity(initial_cols);
    let mut char_to_col = Vec::with_capacity(initial_cols);
    let mut line = top;
    if let Some(max_lines) = max_lines {
        let oldest = bottom.0.saturating_sub(max_lines.saturating_sub(1) as i32);
        if oldest > line.0 {
            line = alacritty_terminal::index::Line(oldest);
        }
    }
    while line <= bottom {
        // Cooperative cancellation: checked once per row, outside the lock.
        if cancelled.load(Ordering::Acquire) {
            break;
        }
        let Some(()) = ({
            let term = term.lock();
            if line < term.topmost_line() || line > term.bottommost_line() {
                None
            } else {
                let cols = term.columns();
                extract_line_text_and_columns(&term, line, cols, &mut line_text, &mut char_to_col);
                Some(())
            }
        }) else {
            line += 1;
            continue;
        };

        if !search.push_line(line.0, &line_text, &char_to_col) {
            break;
        }
        line += 1;
    }

    search.finish()
}

/// Compute the display offset for scrolling to a match, and apply the scroll
/// in a single lock acquisition. Returns the applied display_offset.
pub fn scroll_to_match(term: &Arc<FairMutex<Term<ZedListener>>>, m: &SearchMatch) -> usize {
    use alacritty_terminal::grid::Scroll as AlacScroll;

    let mut term = term.lock();
    let bottom = term.bottommost_line();
    let screen_lines = term.screen_lines();

    // lines_from_bottom is always >= 0 because matches come from topmost..=bottommost
    let lines_from_bottom = bottom.0.saturating_sub(m.start.line.0);
    let half_screen = screen_lines / 2;

    let target_offset = if lines_from_bottom <= half_screen as i32 {
        0
    } else {
        (lines_from_bottom - half_screen as i32).max(0) as usize
    };

    let current = term.grid().display_offset();
    let delta = target_offset as i32 - current as i32;
    if delta != 0 {
        term.scroll_display(AlacScroll::Delta(delta));
    }

    target_offset
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::TerminalState;

    fn restored_search(text: &str, query: &str, regex_mode: bool) -> SearchResult {
        let state = TerminalState::new_display_only(5, 20);
        state.restore_scrollback(text);
        state.session_backend().search(query, regex_mode)
    }

    #[test]
    fn plain_search_matches_across_wide_char_spacer() {
        let result = restored_search("中abc", "中a", false);

        assert!(!result.matches.is_empty());
        assert_eq!(result.matches[0].start.column.0, 0);
        assert_eq!(result.matches[0].end.column.0, 2);
    }

    #[test]
    fn plain_search_column_mapping_survives_lowercase_expansion() {
        let result = restored_search("İabc", "abc", false);

        assert!(!result.matches.is_empty());
        assert_eq!(result.matches[0].start.column.0, 1);
        assert_eq!(result.matches[0].end.column.0, 3);
    }

    #[test]
    fn regex_search_matches_across_wide_char_spacer() {
        let result = restored_search("中abc", "中a", true);

        assert!(!result.matches.is_empty());
        assert_eq!(result.matches[0].start.column.0, 0);
        assert_eq!(result.matches[0].end.column.0, 2);
    }

    #[test]
    fn search_includes_combining_characters_at_their_base_column() {
        let result = restored_search("e\u{301}abc", "e\u{301}", false);

        assert_eq!(result.matches.len(), 1);
        assert_eq!(result.matches[0].start.column.0, 0);
        assert_eq!(result.matches[0].end.column.0, 0);
    }

    #[test]
    fn plain_query_without_the_mark_still_matches_the_base_cell() {
        let result = restored_search("e\u{301}abc", "e", false);

        assert_eq!(result.matches.len(), 1);
        assert_eq!(result.matches[0].start.column.0, 0);
        assert_eq!(result.matches[0].end.column.0, 0);
    }

    #[test]
    fn cancelled_search_stops_before_scanning() {
        let state = TerminalState::new_display_only(5, 20);
        state.restore_scrollback("needle");
        let cancelled = AtomicBool::new(true);
        let result = state
            .session_backend()
            .search_with_cancel("needle", false, &cancelled);
        assert!(result.matches.is_empty());
    }
}
