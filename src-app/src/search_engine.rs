//! Pure text-matching engine for find-in-buffer.
//!
//! Lifted from upstream `crates/paneflow-terminal-ghostty/src/search.rs`
//! (`13b8fdb`) when this fork deleted that crate. Deliberately backend-free:
//! no `alacritty_terminal`, no `cfg` predicate, no platform string. The grid
//! walk lives in `crate::search`, which feeds this one extracted row at a
//! time via `push_line`.
//!
//! Deliberate divergences from upstream (see
//! `docs/fork/2026-08-25-mac-only-fork-design.md`):
//! - `MAX_SEARCH_CELLS` (12M) and the `SearchChunk` / `SearchLine` chunk
//!   driver are NOT ported. They existed for Ghostty's blocking mailbox
//!   round-trip; the budget never fires at our 10 000-line scrollback
//!   (~2M cells), so porting it would be dead code implying a limit we do
//!   not have. `SearchResult` therefore has no `truncated` field.
//! - Matches are emitted as `crate::terminal::types::Point` directly, so
//!   upstream's `from_shared_result` translation layer is unnecessary.
//! - `GhosttyError` collapses to the one variant the engine can raise.

use regex::Regex;

use crate::search::{SearchMatch, SearchResult};
use crate::terminal::types::Point;

/// Maximum query length (bytes). Re-exported by `crate::search`.
pub const MAX_QUERY_LEN: usize = 512;

/// Maximum number of matches to collect before stopping the scan.
const MAX_MATCHES: usize = 10_000;

/// The only way [`SearchEngine::new`] can fail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchEngineError {
    QueryTooLong { limit: usize },
}

impl std::fmt::Display for SearchEngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::QueryTooLong { limit } => {
                write!(f, "search query exceeds the {limit}-byte safety cap")
            }
        }
    }
}

pub struct SearchEngine {
    regex: Option<Regex>,
    plain_query: Vec<char>,
    result: SearchResult,
    done: bool,
}

impl SearchEngine {
    pub fn new(query: &str, regex_mode: bool) -> Result<Self, SearchEngineError> {
        if query.len() > MAX_QUERY_LEN {
            return Err(SearchEngineError::QueryTooLong {
                limit: MAX_QUERY_LEN,
            });
        }
        if query.is_empty() {
            return Ok(Self {
                regex: None,
                plain_query: Vec::new(),
                result: SearchResult {
                    matches: Vec::new(),
                    regex_error: None,
                },
                done: true,
            });
        }
        let (regex, regex_error) = if regex_mode {
            match regex::RegexBuilder::new(query)
                .case_insensitive(true)
                .build()
            {
                Ok(regex) => (Some(regex), None),
                Err(error) => (None, Some(error.to_string())),
            }
        } else {
            (None, None)
        };
        let done = regex_error.is_some();
        Ok(Self {
            regex,
            plain_query: if regex_mode {
                Vec::new()
            } else {
                query.chars().collect()
            },
            result: SearchResult {
                matches: Vec::new(),
                regex_error,
            },
            done,
        })
    }

    /// Feed one extracted grid row. `char_to_column` maps each char of `text`
    /// to its grid column (combining marks share their base cell's column).
    /// Returns `false` once the engine is finished and further rows are moot.
    pub fn push_line(&mut self, line: i32, text: &str, char_to_column: &[usize]) -> bool {
        if self.done {
            return false;
        }
        if let Some(regex) = &self.regex {
            for found in regex.find_iter(text) {
                let start = text[..found.start()].chars().count();
                let count = text[found.start()..found.end()].chars().count();
                push_match(line, char_to_column, start, count, &mut self.result.matches);
                if self.result.matches.len() == MAX_MATCHES {
                    self.done = true;
                    return false;
                }
            }
        } else {
            let line_chars: Vec<char> = text.chars().collect();
            if line_chars.len() >= self.plain_query.len() {
                for start in 0..=line_chars.len() - self.plain_query.len() {
                    if line_chars[start..start + self.plain_query.len()]
                        .iter()
                        .zip(&self.plain_query)
                        .all(|(&actual, &expected)| {
                            actual.to_lowercase().eq(expected.to_lowercase())
                        })
                    {
                        push_match(
                            line,
                            char_to_column,
                            start,
                            self.plain_query.len(),
                            &mut self.result.matches,
                        );
                        if self.result.matches.len() == MAX_MATCHES {
                            self.done = true;
                            return false;
                        }
                    }
                }
            }
        }
        true
    }

    pub fn is_done(&self) -> bool {
        self.done
    }

    pub fn finish(self) -> SearchResult {
        self.result
    }
}

fn push_match(
    line: i32,
    char_to_column: &[usize],
    start: usize,
    count: usize,
    matches: &mut Vec<SearchMatch>,
) {
    if count == 0 {
        return;
    }
    if let (Some(&start_column), Some(&end_column)) = (
        char_to_column.get(start),
        char_to_column.get(start + count - 1),
    ) {
        matches.push(SearchMatch {
            start: Point::new(line, start_column),
            end: Point::new(line, end_column),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_maps_wide_and_lowercase_expansion_columns() {
        let mut search = SearchEngine::new("abc", false).unwrap();
        assert!(search.push_line(7, "中İabc", &[0, 2, 3, 4, 5]));
        let result = search.finish();
        assert_eq!(result.matches.len(), 1);
        assert_eq!(result.matches[0].start, Point::new(7, 3));
        assert_eq!(result.matches[0].end, Point::new(7, 5));
    }

    #[test]
    fn invalid_regex_is_a_result_not_a_scan_failure() {
        let search = SearchEngine::new("(", true).unwrap();
        assert!(search.is_done());
        assert!(search.finish().regex_error.is_some());
    }

    #[test]
    fn over_long_query_is_rejected_by_the_engine() {
        let result = SearchEngine::new(&"a".repeat(MAX_QUERY_LEN + 1), false);
        assert_eq!(
            result.err(),
            Some(SearchEngineError::QueryTooLong {
                limit: MAX_QUERY_LEN,
            })
        );
    }
}
