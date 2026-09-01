use std::ops::Range;

use crate::Result;
use crate::engine::DisplayTerminal;

const MAX_SCROLLBACK_LINES: usize = 4_000;
const MAX_SCROLLBACK_CHARS: usize = 400_000;
/// Rows per grid read while [`DisplayTerminal::transcript_window`] walks a
/// blank screen back into history looking for the newest painted row.
const BLANK_WALK_CHUNK_ROWS: usize = 64;
/// Rows per grid read when materializing a transcript window, so a 4000-row
/// page is several bounded reads rather than one grid-wide allocation.
const WINDOW_READ_CHUNK_ROWS: usize = 256;

/// One page of a pane's transcript, cut the way `surface.read` asks for it:
/// the retained history followed by the screen being painted, trailing blank
/// rows trimmed, windowed from the newest end.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TranscriptWindow {
    /// The rows in the window, each `trim_end()`ed, joined with `\n`.
    pub text: String,
    /// Rows in `text`.
    pub returned: usize,
    /// Rows the whole transcript holds: the retained history (at most
    /// 4000 rows) plus the screen, with trailing blank rows trimmed.
    pub total: usize,
    /// The window reaches the oldest retained row.
    pub eof: bool,
}

impl DisplayTerminal {
    /// Read one page of the transcript without materializing the rest.
    ///
    /// `offset` skips rows from the newest end and `lines` is the page size,
    /// so the page covers transcript rows `[total - offset - lines, total -
    /// offset)`. Only those rows and the screen are read from the grid: the
    /// screen because it is the newest end and decides where trailing blank
    /// rows stop, the page because it is what the caller asked for. A blank
    /// screen over live history walks history backwards in chunks until it
    /// meets a painted row, so a `clear` does not turn every later read into
    /// a full-history read either (issue #29).
    pub fn transcript_window(&self, lines: usize, offset: usize) -> Result<TranscriptWindow> {
        let geometry = self.grid_geometry()?;
        let history_rows = usize::try_from(geometry.scrollback).unwrap_or(0);
        let start = history_rows.saturating_sub(MAX_SCROLLBACK_LINES);

        let screen = self.trimmed_rows(history_rows..geometry.total_rows)?;
        let end_row = match screen.iter().rposition(|row| !row.is_empty()) {
            Some(index) => history_rows + index + 1,
            None => self.newest_painted_history_row_end(start, history_rows)?,
        };
        let total = end_row.saturating_sub(start);
        let end = total.saturating_sub(offset);
        if end == 0 {
            return Ok(TranscriptWindow {
                text: String::new(),
                returned: 0,
                total,
                eof: true,
            });
        }
        let window_start = end.saturating_sub(lines);
        let first_row = start + window_start;
        let last_row = start + end;

        let mut rows = Vec::with_capacity(last_row - first_row);
        if first_row < history_rows {
            rows.extend(self.trimmed_rows(first_row..last_row.min(history_rows))?);
        }
        if last_row > history_rows {
            let from = first_row.max(history_rows) - history_rows;
            rows.extend(screen[from..last_row - history_rows].iter().cloned());
        }
        Ok(TranscriptWindow {
            text: rows.join("\n"),
            returned: rows.len(),
            total,
            eof: window_start == 0,
        })
    }

    /// Grid rows in `range`, each `trim_end()`ed, read in bounded chunks.
    fn trimmed_rows(&self, range: Range<usize>) -> Result<Vec<String>> {
        let mut rows = Vec::with_capacity(range.len());
        let mut chunk_start = range.start;
        while chunk_start < range.end {
            let chunk_end = chunk_start
                .saturating_add(WINDOW_READ_CHUNK_ROWS)
                .min(range.end);
            rows.extend(
                self.grid_lines(Some(chunk_start..chunk_end))?
                    .into_iter()
                    .map(|line| line.text.trim_end().to_owned()),
            );
            chunk_start = chunk_end;
        }
        Ok(rows)
    }

    /// One past the newest history row in `[start, history_rows)` that holds
    /// text, or `start` when every retained history row is blank. Walks
    /// backwards in chunks so a blank screen does not cost the whole history.
    fn newest_painted_history_row_end(&self, start: usize, history_rows: usize) -> Result<usize> {
        let mut chunk_end = history_rows;
        while chunk_end > start {
            let chunk_start = chunk_end.saturating_sub(BLANK_WALK_CHUNK_ROWS).max(start);
            let chunk = self.grid_lines(Some(chunk_start..chunk_end))?;
            if let Some(index) = chunk
                .iter()
                .rposition(|line| !line.text.trim_end().is_empty())
            {
                return Ok(chunk_start + index + 1);
            }
            chunk_end = chunk_start;
        }
        Ok(start)
    }

    pub fn extract_scrollback(&self) -> Result<Option<String>> {
        // Ghostty stores history before the active screen in its page list.
        // `scrollback_rows` is therefore the exclusive viewport boundary.
        let history_rows = self.scrollback_rows()?;
        if history_rows == 0 {
            return Ok(None);
        }
        let start = history_rows.saturating_sub(MAX_SCROLLBACK_LINES);
        let mut lines: Vec<String> = self
            .grid_lines(Some(start..history_rows))?
            .into_iter()
            .map(|line| line.text.trim_end().to_owned())
            .collect();
        while lines.last().is_some_and(String::is_empty) {
            lines.pop();
        }
        if lines.is_empty() {
            return Ok(None);
        }
        let mut text = lines.join("\n");
        cap_complete_lines(&mut text, MAX_SCROLLBACK_CHARS);
        Ok((!text.is_empty()).then_some(text))
    }

    /// The rows the program is painting, history excluded: the alternate
    /// screen while it is active, the viewport otherwise. This is the half
    /// of a pane [`Self::extract_scrollback`] leaves out, and for a
    /// full-screen TUI it is the only half there is. Trailing blank rows are
    /// dropped; `None` when the screen is blank.
    pub fn extract_screen(&self) -> Result<Option<String>> {
        let geometry = self.grid_geometry()?;
        let history_rows = usize::try_from(geometry.scrollback).unwrap_or(0);
        let mut lines: Vec<String> = self
            .grid_lines(Some(history_rows..geometry.total_rows))?
            .into_iter()
            .map(|line| line.text.trim_end().to_owned())
            .collect();
        while lines.last().is_some_and(String::is_empty) {
            lines.pop();
        }
        Ok((!lines.is_empty()).then(|| lines.join("\n")))
    }

    pub fn restore_scrollback(&mut self, text: &str) -> Result<()> {
        let mut lines: Vec<String> = bounded_recent_text(text, MAX_SCROLLBACK_CHARS)
            .split('\n')
            .rev()
            .take(MAX_SCROLLBACK_LINES)
            .map(sanitize_scrollback_line)
            .collect();
        lines.reverse();
        let mut sanitized = lines.join("\n");
        cap_complete_lines(&mut sanitized, MAX_SCROLLBACK_CHARS);
        self.feed(b"\x1b[0m")?;
        for line in sanitized.split('\n') {
            self.feed(line.as_bytes())?;
            self.feed(b"\r\n")?;
        }
        Ok(())
    }
}

fn sanitize_scrollback_line(line: &str) -> String {
    line.chars()
        .filter(|&character| {
            character == '\t'
                || (!character.is_control() && !('\u{80}'..='\u{9f}').contains(&character))
        })
        .collect()
}

fn cap_complete_lines(text: &mut String, cap: usize) {
    if text.len() <= cap {
        return;
    }
    let mut start = text.len() - cap;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    if start > 0 && text.as_bytes()[start - 1] == b'\n' {
        text.drain(..start);
        return;
    }
    if let Some(newline) = text[start..].find('\n') {
        start += newline + 1;
    } else {
        text.clear();
        return;
    }
    text.drain(..start);
}

fn bounded_recent_text(text: &str, cap: usize) -> &str {
    if text.len() <= cap {
        return text;
    }
    let mut start = text.len() - cap;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    if start > 0 && text.as_bytes()[start - 1] == b'\n' {
        return &text[start..];
    }
    text[start..]
        .find('\n')
        .map_or("", |newline| &text[start + newline + 1..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restore_sanitizer_removes_escape_and_c1_controls() {
        assert_eq!(
            sanitize_scrollback_line("safe\u{1b}]0;spoof\u{7}\u{9b}31m\tend"),
            "safe]0;spoof31m\tend"
        );
    }

    #[test]
    fn cap_keeps_valid_utf8_and_complete_recent_lines() {
        let mut text = "old\n中中\nrecent".to_owned();
        cap_complete_lines(&mut text, 11);
        assert_eq!(text, "recent");
    }

    #[test]
    fn oversized_single_line_is_not_restored_partially() {
        assert_eq!(bounded_recent_text("0123456789", 5), "");
        assert_eq!(bounded_recent_text("old\nrecent", 6), "recent");
    }
}
