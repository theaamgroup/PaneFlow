//! Bounded JSONL reads shared by native session readers.

use std::io::{BufRead, Read};
use std::path::Path;

use crate::limits::MAX_LINE_BYTES;

pub(crate) enum CappedLine {
    Eof,
    Oversized,
    Line(String),
}

/// One capped line, shared with the Claude and Codex session readers.
/// Bytes are decoded lossily: `read_line` returns `InvalidData` when
/// [`MAX_LINE_BYTES`] splits a multibyte character, and that error used to
/// abort the file after a header was already parsed.
pub(crate) fn read_capped_line<R: BufRead>(
    reader: &mut R,
    path: &Path,
    budget: &mut u64,
) -> Option<CappedLine> {
    let mut bytes = Vec::new();
    let read = reader
        .by_ref()
        .take(MAX_LINE_BYTES)
        .read_until(b'\n', &mut bytes)
        .ok()?;
    if read == 0 {
        return Some(CappedLine::Eof);
    }
    *budget = budget.saturating_sub(read as u64);

    if read as u64 == MAX_LINE_BYTES && !bytes.ends_with(b"\n") {
        let more_follows = match reader.fill_buf() {
            Ok(buf) => !buf.is_empty(),
            Err(_) => return None,
        };
        if more_follows {
            log::debug!(
                target: "paneflow_app::session_lines",
                "skipped an oversized (>{} B) line in {}; continuing scan for the session header",
                MAX_LINE_BYTES,
                path.display(),
            );
            drain_oversized_line(reader, budget)?;
            return Some(CappedLine::Oversized);
        }
    }

    Some(CappedLine::Line(
        String::from_utf8_lossy(&bytes).into_owned(),
    ))
}

/// Discard the rest of an oversized line in bounded chunks, charging every
/// byte against `budget`. Returns `None` when the budget runs out before the
/// newline, so one huge line costs at most the remaining scan budget.
fn drain_oversized_line<R: BufRead>(reader: &mut R, budget: &mut u64) -> Option<()> {
    loop {
        if *budget == 0 {
            return None;
        }
        let chunk = match reader.fill_buf() {
            Ok(buf) => buf,
            Err(_) => return None,
        };
        if chunk.is_empty() {
            return Some(());
        }
        if let Some(nl) = chunk.iter().position(|&b| b == b'\n') {
            reader.consume(nl + 1);
            *budget = budget.saturating_sub(nl as u64 + 1);
            return Some(());
        }
        let consumed = chunk.len();
        reader.consume(consumed);
        *budget = budget.saturating_sub(consumed as u64);
    }
}
