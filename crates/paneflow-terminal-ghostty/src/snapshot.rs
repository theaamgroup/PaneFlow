use std::sync::Arc;

use paneflow_libghostty_sys as sys;

use crate::engine::DisplayTerminal;
use crate::handles::check;
use crate::limits::MAX_SNAPSHOT_CELLS;
use crate::snapshot_ffi::{
    RenderDirty, render_get, render_row_data, render_row_iterator, terminal_scrollbar,
};
use crate::{Cell, Content, GhosttyError, Point, Result, Scroll, SelectionRange};

#[derive(Default)]
pub(crate) struct SnapshotCache {
    cells: Arc<[Cell]>,
    /// Rows the last refresh rewrote, cleared to all-false by a clean frame.
    dirty_rows: Vec<bool>,
    selection: Option<SelectionRange>,
    cols: usize,
    rows: usize,
    valid: bool,
}

impl SnapshotCache {
    pub(crate) fn invalidate(&mut self) {
        self.valid = false;
    }

    fn matches(&self, cols: usize, rows: usize, cell_count: usize) -> bool {
        self.valid && self.cols == cols && self.rows == rows && self.cells.len() == cell_count
    }
}

impl DisplayTerminal {
    pub fn snapshot(&mut self) -> Result<Content> {
        // Split into its two phases rather than calling
        // `ghostty_render_state_update`: only the begin phase touches the
        // terminal, so a future caller that shares the terminal with an IO
        // thread can hold its lock across that call alone.
        // SAFETY: both handles are owned by `self`.
        let result = unsafe {
            sys::ghostty_render_state_begin_update(self.render_state.raw(), self.terminal.raw())
        };
        check("render_state_begin_update", result)?;
        // SAFETY: the render state is owned by `self` and a begin phase just
        // completed.
        let result = unsafe { sys::ghostty_render_state_end_update(self.render_state.raw()) };
        check("render_state_end_update", result)?;
        let (history_size, display_offset) = self.scrollbar_position()?;
        let display_offset_i32 = i32::try_from(display_offset)
            .map_err(|_| crate::GhosttyError::AbiMismatch("display offset overflow".into()))?;
        let (cols, rows) = self.render_dimensions()?;
        let cell_count = cols.checked_mul(rows).ok_or_else(|| {
            crate::GhosttyError::AbiMismatch("snapshot cell count overflow".into())
        })?;
        if cell_count > MAX_SNAPSHOT_CELLS {
            return Err(crate::GhosttyError::LimitExceeded {
                resource: "snapshot cells",
                limit: MAX_SNAPSHOT_CELLS,
            });
        }

        let dirty = render_get::<RenderDirty>(self.render_state.raw())?;
        let full_refresh = match dirty {
            sys::GhosttyRenderStateDirty_GHOSTTY_RENDER_STATE_DIRTY_FALSE
            | sys::GhosttyRenderStateDirty_GHOSTTY_RENDER_STATE_DIRTY_PARTIAL => {
                !self.snapshot_cache.matches(cols, rows, cell_count)
            }
            sys::GhosttyRenderStateDirty_GHOSTTY_RENDER_STATE_DIRTY_FULL => true,
            value => {
                return Err(GhosttyError::AbiMismatch(format!(
                    "render state reported unknown dirty value {value}"
                )));
            }
        };
        if full_refresh || dirty == sys::GhosttyRenderStateDirty_GHOSTTY_RENDER_STATE_DIRTY_PARTIAL
        {
            self.refresh_snapshot_cache(cols, rows, cell_count, full_refresh)?;
        } else {
            self.snapshot_cache.dirty_rows.fill(false);
        }
        if !self.snapshot_cache.matches(cols, rows, cell_count) {
            return Err(GhosttyError::AbiMismatch(
                "render state was clean before the snapshot cache was initialized".into(),
            ));
        }
        if full_refresh {
            // The whole frame was rebuilt, so consume every dirty bit at
            // once. A partial pass cleared its rows as it went and only the
            // global flag is left.
            // SAFETY: the render state is owned by `self`.
            let result = unsafe { sys::ghostty_render_state_clean(self.render_state.raw()) };
            check("render_state_clean", result)?;
        } else {
            self.clear_render_dirty()?;
        }

        let cells = self.snapshot_cache.cells.clone();
        let dirty_rows: Arc<[bool]> = self.snapshot_cache.dirty_rows.as_slice().into();
        let selection = self
            .snapshot_cache
            .selection
            .as_ref()
            .map(|selection| {
                let start_line = selection
                    .start
                    .line
                    .checked_sub(display_offset_i32)
                    .ok_or_else(|| GhosttyError::AbiMismatch("selection start overflow".into()))?;
                let end_line = selection
                    .end
                    .line
                    .checked_sub(display_offset_i32)
                    .ok_or_else(|| GhosttyError::AbiMismatch("selection end overflow".into()))?;
                Ok(SelectionRange {
                    start: Point::new(start_line, selection.start.column),
                    end: Point::new(end_line, selection.end.column),
                    rectangle: selection.rectangle,
                })
            })
            .transpose()?;
        Ok(Content {
            cells,
            dirty_rows,
            cursor: self.cursor(display_offset)?,
            selection,
            cols,
            rows,
            display_offset,
            history_size,
        })
    }

    /// Move to a viewport row measured from the top of the scrollable area.
    /// This matches Ghostty's scrollbar offset space, so output that extends
    /// history cannot move an in-progress scrollbar gesture to newer content.
    pub fn scroll_to_viewport_row(&mut self, row: usize) -> Result<()> {
        let (history_size, current) = self.scrollbar_position()?;
        let target = history_size.saturating_sub(row.min(history_size));
        let current = i32::try_from(current)
            .map_err(|_| GhosttyError::AbiMismatch("display offset overflow".into()))?;
        let target = i32::try_from(target)
            .map_err(|_| GhosttyError::AbiMismatch("display offset target overflow".into()))?;
        let delta = target - current;
        if delta != 0 {
            self.scroll(Scroll::Delta(delta));
        }
        Ok(())
    }

    fn refresh_snapshot_cache(
        &mut self,
        cols: usize,
        rows: usize,
        cell_count: usize,
        full_refresh: bool,
    ) -> Result<()> {
        // A full refresh of an unchanged grid size writes over the cached
        // cells when nothing else holds them, the same way a partial refresh
        // does, instead of allocating a fresh array for every scrolled frame.
        // The consumer drops its previous snapshot before asking for the
        // next one, so this is the common case.
        let in_place = full_refresh
            && self.snapshot_cache.cols == cols
            && self.snapshot_cache.rows == rows
            && self.snapshot_cache.cells.len() == cell_count
            && Arc::get_mut(&mut self.snapshot_cache.cells).is_some();
        let mut rebuilt_cells = (full_refresh && !in_place).then(|| {
            self.snapshot_cache.valid = false;
            Vec::with_capacity(cell_count)
        });
        self.snapshot_cache.dirty_rows.clear();
        self.snapshot_cache.dirty_rows.resize(rows, false);

        let iterator = render_row_iterator(self.render_state.raw(), self.row_iterator.raw())?;

        let mut row_index = 0usize;
        let mut selection_start = None;
        let mut selection_end = None;
        while unsafe { sys::ghostty_render_state_row_iterator_next(iterator) } {
            if row_index >= rows {
                return Err(GhosttyError::AbiMismatch(
                    "render iterator returned too many rows".into(),
                ));
            }

            let row = render_row_data(iterator, self.row_cells.raw())?;
            let row_selection = if let Some(selection) = row.selection {
                let start = usize::from(selection.start_x.min(selection.end_x));
                let end = usize::from(selection.start_x.max(selection.end_x));
                if end >= cols {
                    return Err(GhosttyError::AbiMismatch(
                        "render selection exceeded snapshot columns".into(),
                    ));
                }
                Some((start, end))
            } else {
                None
            };
            if let Some((start, end)) = row_selection {
                let line = i32::try_from(row_index)
                    .map_err(|_| GhosttyError::AbiMismatch("selection row overflow".into()))?;
                selection_start.get_or_insert(Point::new(line, start));
                selection_end = Some(Point::new(line, end));
            }
            if full_refresh || row.dirty {
                self.snapshot_cache.dirty_rows[row_index] = true;
                let mut column = 0usize;
                while unsafe { sys::ghostty_render_state_row_cells_next(row.cells) } {
                    if column >= cols {
                        return Err(GhosttyError::AbiMismatch(
                            "render iterator returned too many columns".into(),
                        ));
                    }
                    let selected =
                        row_selection.is_some_and(|(start, end)| (start..=end).contains(&column));
                    let cell = self.copy_cell(row.cells, row_index, column, selected)?;
                    if let Some(cells) = rebuilt_cells.as_mut() {
                        cells.push(cell);
                    } else {
                        let cell_index = row_index * cols + column;
                        let cached = Arc::make_mut(&mut self.snapshot_cache.cells)
                            .get_mut(cell_index)
                            .ok_or_else(|| {
                                GhosttyError::AbiMismatch(
                                    "partial render update exceeded the snapshot cache".into(),
                                )
                            })?;
                        *cached = cell;
                    }
                    column += 1;
                }
                if column != cols {
                    return Err(GhosttyError::AbiMismatch(format!(
                        "render row returned {column} columns, expected {cols}"
                    )));
                }
            }

            // A full refresh clears everything in one call after the loop
            // through `ghostty_render_state_clean`; a partial pass has to
            // clear the rows it actually consumed, one at a time.
            if !full_refresh && row.dirty {
                let clean = false;
                // SAFETY: the option takes a `bool*` that outlives the call
                // and `iterator` is positioned on a live row.
                let result = unsafe {
                    sys::ghostty_render_state_row_set(
                        iterator,
                        sys::GhosttyRenderStateRowOption_GHOSTTY_RENDER_STATE_ROW_OPTION_DIRTY,
                        (&raw const clean).cast(),
                    )
                };
                check("render_state_row_set", result)?;
            }
            row_index += 1;
        }
        if row_index != rows {
            return Err(GhosttyError::AbiMismatch(format!(
                "render iterator returned {row_index} rows, expected {rows}"
            )));
        }

        let selection = match selection_start.zip(selection_end) {
            Some((start, end)) => Some(SelectionRange {
                start,
                end,
                rectangle: self.selection_rectangle()?.unwrap_or(false),
            }),
            None => None,
        };

        if let Some(cells) = rebuilt_cells {
            if cells.len() != cell_count {
                return Err(GhosttyError::AbiMismatch(format!(
                    "render iterator returned {} cells, expected {cell_count}",
                    cells.len()
                )));
            }
            self.snapshot_cache = SnapshotCache {
                cells: cells.into(),
                dirty_rows: std::mem::take(&mut self.snapshot_cache.dirty_rows),
                selection,
                cols,
                rows,
                valid: true,
            };
        } else {
            self.snapshot_cache.selection = selection;
            self.snapshot_cache.valid = true;
        }
        Ok(())
    }

    fn clear_render_dirty(&self) -> Result<()> {
        let clean = sys::GhosttyRenderStateDirty_GHOSTTY_RENDER_STATE_DIRTY_FALSE;
        let result = unsafe {
            sys::ghostty_render_state_set(
                self.render_state.raw(),
                sys::GhosttyRenderStateOption_GHOSTTY_RENDER_STATE_OPTION_DIRTY,
                (&clean as *const sys::GhosttyRenderStateDirty).cast(),
            )
        };
        check("render_state_set", result)
    }

    fn scrollbar(&self) -> Result<sys::GhosttyTerminalScrollbar> {
        terminal_scrollbar(self.terminal.raw())
    }

    fn scrollbar_position(&self) -> Result<(usize, usize)> {
        let scrollbar = self.scrollbar()?;
        let history_size = scrollbar
            .total
            .checked_sub(scrollbar.len)
            .and_then(|value| usize::try_from(value).ok())
            .ok_or_else(|| GhosttyError::AbiMismatch("invalid scrollbar length".into()))?;
        let scrollbar_offset = usize::try_from(scrollbar.offset)
            .map_err(|_| GhosttyError::AbiMismatch("scrollbar offset overflow".into()))?;
        let display_offset = history_size
            .checked_sub(scrollbar_offset)
            .ok_or_else(|| GhosttyError::AbiMismatch("scrollbar offset exceeds history".into()))?;
        Ok((history_size, display_offset))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{TerminalAppearance, WindowSize};

    fn terminal(cols: usize, rows: usize, scrollback: usize) -> DisplayTerminal {
        let size = WindowSize::new(cols, rows, 8, 16).expect("valid terminal size");
        DisplayTerminal::new(size, scrollback, TerminalAppearance::default())
            .expect("terminal must initialize")
    }

    /// [`DisplayTerminal::snapshot`] remaps `set_selection`'s screen-space
    /// selection into viewport space by subtracting the current
    /// `display_offset` (snapshot.rs:100-121). The round trip
    /// `viewport_line = screen_line + display_offset` must hold both at
    /// `display_offset == 0` and after scrolling into history, or a wrong
    /// mapping mis-paints the selection highlight after a scroll while every
    /// other selection test (which never scrolls) stays green.
    #[test]
    fn snapshot_selection_matches_set_selection_including_scrolled_viewport() {
        let mut terminal = terminal(20, 3, 200);

        // display_offset == 0: select a range on the live, unscrolled top
        // row and confirm the snapshot reports it back unchanged.
        terminal.feed(b"hello world").expect("output must parse");
        terminal
            .set_selection(SelectionRange {
                start: Point::new(0, 0),
                end: Point::new(0, 4),
                rectangle: false,
            })
            .expect("selection must install");
        let content = terminal.snapshot().expect("snapshot");
        assert_eq!(content.display_offset, 0);
        let selection = content
            .selection
            .expect("selection must be present at display_offset 0");
        assert_eq!(selection.start, Point::new(0, 0));
        assert_eq!(selection.end, Point::new(0, 4));

        // Push enough output through a 3-row viewport that real scrollback
        // accumulates above it.
        for index in 0..10 {
            terminal
                .feed(format!("line {index}\r\n").as_bytes())
                .expect("output must parse");
        }

        // Scroll all the way to the top of the scrollback and read back the
        // resulting display_offset, rather than assuming it.
        terminal
            .scroll_to_viewport_row(0)
            .expect("scroll to top of scrollback");
        let scrolled = terminal.snapshot().expect("snapshot after scroll");
        let display_offset = scrolled.display_offset;
        assert!(
            display_offset > 0,
            "expected scrolling to the top to produce a non-zero display_offset"
        );

        // Select the row that sits at the current top of the scrollback in
        // screen space (screen space is anchored to the live area's top, so
        // that row is `-display_offset`); it is the top row of the viewport
        // once rendered at this exact scroll position.
        let history_line = -i32::try_from(display_offset).expect("display_offset fits i32");
        terminal
            .set_selection(SelectionRange {
                start: Point::new(history_line, 0),
                end: Point::new(history_line, 4),
                rectangle: false,
            })
            .expect("selection must install");

        let content = terminal
            .snapshot()
            .expect("snapshot with scrolled selection");
        assert_eq!(content.display_offset, display_offset);
        let selection = content
            .selection
            .expect("selection must be present after scrolling");
        // `snapshot` must hand the selection back in the same screen-space
        // coordinates `set_selection` was given, not the viewport-local row
        // the render iterator used internally: display_offset is applied and
        // then undone (`viewport_line = screen_line + display_offset`,
        // `screen_line = viewport_line - display_offset`), so the round trip
        // is the identity for any row that stays inside the viewport.
        assert_eq!(selection.start, Point::new(history_line, 0));
        assert_eq!(selection.end, Point::new(history_line, 4));

        // A row just below the current viewport has no selection to report
        // at all, confirming the mapping is viewport-relative rather than a
        // blanket pass-through of whatever was last installed.
        let out_of_view = history_line + i32::try_from(content.rows).expect("rows fits i32");
        terminal
            .set_selection(SelectionRange {
                start: Point::new(out_of_view, 0),
                end: Point::new(out_of_view, 4),
                rectangle: false,
            })
            .expect("selection must install");
        let content = terminal
            .snapshot()
            .expect("snapshot with off-screen selection");
        assert_eq!(content.selection, None);
    }

    /// `CellMirror` (`src-app/src/terminal/ghostty_session.rs:4249`) trusts
    /// `Content::dirty_rows` completely: on the incremental path it only
    /// re-converts rows this crate flags dirty, and keeps the rest of its
    /// cached grid untouched. `refresh_snapshot_cache`'s in-place partial
    /// path (above) is the sole place that promise is kept, and a
    /// self-referential comparison (deriving the "expected" value from the
    /// very same snapshot under test) can't catch it slipping: a stale cell
    /// with a matching dirty flag still agrees with itself. This test
    /// instead reads the live grid independently through
    /// [`DisplayTerminal::render_cell`], which jumps straight to a cell
    /// through the FFI row iterator rather than going through
    /// `snapshot_cache` at all.
    #[test]
    fn snapshot_partial_refresh_cells_and_dirty_rows_match_the_live_grid() {
        let mut terminal = terminal(20, 4, 100);

        // Write four distinct rows, then park the cursor back on the row
        // that is about to be rewritten so the second write does not force
        // any other row dirty just because the cursor vacated it.
        terminal
            .feed(b"\x1b[1;1Hrow0\x1b[2;1Hrow1\x1b[3;1Hrow2\x1b[4;1Hrow3\x1b[3;1H")
            .expect("output must parse");
        let first = terminal
            .snapshot()
            .expect("first snapshot primes the cache");
        assert_eq!(first.dirty_rows.len(), 4);

        // Overwrite only row 2, in place, with different text.
        terminal.feed(b"XXXX").expect("output must parse");
        let second = terminal
            .snapshot()
            .expect("second snapshot takes the incremental path");

        // Exactly the rewritten row comes back dirty.
        assert_eq!(
            second.dirty_rows.as_ref(),
            &[false, false, true, false],
            "expected only row 2 dirty, got {:?}",
            second.dirty_rows
        );

        // The row the cache marked dirty must actually hold the new
        // content, verified against a read that bypasses the snapshot
        // cache entirely. A regression that flags a row dirty without
        // refreshing its cells (or that refreshes the wrong cells) leaves
        // `second.cells` holding stale data while `dirty_rows` still
        // (correctly) reports the row as dirty.
        let expected_row2 = "XXXX                "; // 4 written + 16 blanks = 20 cols
        assert_eq!(expected_row2.len(), second.cols);
        for (column, expected_char) in expected_row2.chars().enumerate() {
            let live = terminal
                .render_cell(2, u16::try_from(column).expect("column fits u16"))
                .expect("independent point read of the live grid");
            let cached = &second.cells[2 * second.cols + column];
            assert_eq!(
                live.character, expected_char,
                "column {column}: live grid does not hold the new text"
            );
            assert_eq!(
                cached.character, expected_char,
                "column {column}: cached snapshot does not hold the new text"
            );
            assert_eq!(
                live, *cached,
                "column {column}: cached snapshot cell drifted from the live grid"
            );
        }

        // Untouched rows keep their original content in the cache.
        for (row, expected) in [(0usize, "row0"), (1, "row1"), (3, "row3")] {
            for (column, expected_char) in expected.chars().enumerate() {
                let cached = &second.cells[row * second.cols + column];
                assert_eq!(
                    cached.character, expected_char,
                    "row {row} column {column} changed but was never written to"
                );
            }
        }
    }
}
