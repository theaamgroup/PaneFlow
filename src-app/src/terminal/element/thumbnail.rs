//! Read-only miniature render of a pane's terminal grid (issue #339).
//!
//! # Why this is not a small `TerminalElement`
//!
//! `TerminalElement::build_layout` calls `self.backend.notify_window_size(..)`
//! as a side effect of layout, which SIGWINCHes the child process to the
//! element's bounds. There is no flag to suppress it. A card-sized
//! `TerminalElement` would therefore resize the PTY to the card every frame
//! while the real pane resized it back.
//!
//! This module hangs off `layout_from_snapshot` instead - the `Window`-free,
//! `App`-free pure layout pass built so the golden-frame tests could assert
//! layout with no GPU. It takes cell dimensions and the base font as plain
//! values, so it lays out at any scale.
//!
//! It lives under `element/` because `LayoutState`'s fields, `mod paint`, and
//! `CellGeometry` are all private to `element`, and a child module can see its
//! parent's private items.

use std::sync::Arc;

use gpui::{
    App, Bounds, ContentMask, Element, ElementId, Font, GlobalElementId, InspectorElementId,
    IntoElement, LayoutId, Pixels, Point, Style, Window, px, relative,
};

use super::font::CellMetrics;
use super::geometry::CellGeometry;
use super::{
    CellDimensions, LayoutInputs, LayoutState, cursor_from_content, layout_from_snapshot, paint,
};
use crate::terminal::TerminalSessionBackend;
use crate::terminal::types::{Content, CursorShape, TerminalWindowSize, terminal_metric_to_u16};
use crate::theme::TerminalTheme;

/// Rows of the pane's viewport a card shows at the DEFAULT cell metrics,
/// counted from the bottom.
///
/// Bottom-crop, always - one rule, one test. It loses a full-screen TUI's
/// header row; that was weighed against centring on the cursor (which makes
/// the card jitter between refreshes) and against pinning row 0 (two layout
/// passes per card), and the fixed rule won on determinism. The live count
/// is [`thumbnail_rows_for`]: the band is fixed, so a taller `line_height`
/// fits fewer rows, and cropping fewer keeps the LAST rows - the prompt and
/// the cursor - inside the band rather than clipping them off its bottom
/// (PR #354 review). Test-only since then: production reads the derived
/// count, and the tests pin that the derivation lands on this figure at the
/// defaults.
#[cfg(test)]
pub(super) const THUMBNAIL_ROWS: usize = 12;

/// Preview content is 25% smaller than the original 25/3 px face.
/// Card labels keep their full UI font size.
pub(crate) const THUMBNAIL_FONT_PX: f32 = (25.0 / 3.0) * 0.75;

/// A 312.5 by 192.5 card (25% larger than 250 by 154), less chrome.
/// The extra status row stays outside this read-only terminal band.
pub(crate) const THUMBNAIL_BAND_W: f32 = 302.5;
pub(crate) const THUMBNAIL_BAND_H: f32 = 98.5;

/// Cell metrics for a thumbnail under `settings`: the same two multipliers
/// the pane uses, applied to the thumbnail font size through the same
/// face measurement and rounding as the pane
/// (`font::cell_metrics_without_window`, at scale 1.0: a logical-pixel grid).
pub(super) fn thumbnail_cell_metrics_for(settings: &super::font::FontSettings) -> CellMetrics {
    super::font::cell_metrics_without_window(settings, px(THUMBNAIL_FONT_PX))
}

/// Cell metrics for a thumbnail under the live config.
pub(super) fn thumbnail_cell_metrics() -> CellMetrics {
    thumbnail_cell_metrics_for(&super::font::cached_font_config())
}

/// The logical-pixel strides of [`thumbnail_cell_metrics_for`].
pub(super) fn thumbnail_cell_dimensions_for(
    settings: &super::font::FontSettings,
) -> CellDimensions {
    let metrics = thumbnail_cell_metrics_for(settings);
    CellDimensions {
        cell_width: metrics.cell_width_px(),
        line_height: metrics.cell_height_px(),
    }
}

/// The logical-pixel strides of a thumbnail under the live config.
pub(super) fn thumbnail_cell_dimensions() -> CellDimensions {
    thumbnail_cell_dimensions_for(&super::font::cached_font_config())
}

/// How many viewport rows fit the fixed band at these cell metrics: at the
/// 1.0 / 1.0 defaults twelve (`THUMBNAIL_ROWS`); at `line_height = 2.5`
/// (21 px rows) four. Never zero, so a card always shows the prompt row.
pub(super) fn thumbnail_rows_for(dims: &CellDimensions) -> usize {
    ((THUMBNAIL_BAND_H / f32::from(dims.line_height)).floor() as usize).max(1)
}

/// A grid snapshot plus the row window a thumbnail paints.
pub(super) struct ThumbnailSnapshot {
    pub content: Content,
    /// Cull range `[first, last)`. Cells keep their absolute line numbers
    /// through `layout_from_snapshot`, which culls by line index rather than
    /// renumbering, so the painter shifts its origin up by `first` rows.
    pub first_visible_row: i32,
    pub last_visible_row: i32,
}

/// Read one pane's current grid without touching it.
///
/// `clear_on_resize: false` is load-bearing: the `true` branch mutates
/// `ResizeState` and calls `submit_requested_resize`. On the `false` path the
/// window size is consumed only by `normalized_window_size` and discarded, and
/// the two visible-row arguments are ignored outright (culling happens later,
/// in `layout_from_snapshot`) - but the size is still derived honestly from
/// the pane's own metrics rather than relying on arguments being inert.
///
/// The snapshot is the VIEWPORT, not scrollback, with `display_offset` already
/// applied, so a pane the user has scrolled up shows in its thumbnail exactly
/// what the pane itself is showing.
pub(super) fn thumbnail_snapshot(backend: &TerminalSessionBackend) -> ThumbnailSnapshot {
    let metrics = backend.grid_metrics();
    let dims = thumbnail_cell_dimensions();
    let window_size = TerminalWindowSize::new(
        metrics.columns,
        metrics.screen_lines,
        terminal_metric_to_u16(f32::from(dims.cell_width)),
        terminal_metric_to_u16(f32::from(dims.line_height)),
    );
    let (content, _initial_clear_consumed) = backend.render_content(window_size, 0, 0, false);

    let last_visible_row = content.rows as i32;
    let first_visible_row = content.rows.saturating_sub(thumbnail_rows_for(&dims)) as i32;
    ThumbnailSnapshot {
        content,
        first_visible_row,
        last_visible_row,
    }
}

/// Base font for a thumbnail: the same family the pane uses, at thumbnail size.
///
/// Resolved here from the cached font config rather than threaded in from the
/// overlay. It does NOT go through `resolve_frame_metrics`, whose
/// `size_override` is clamped to [8.0, 32.0] pt and so could not produce a
/// 6.25 px thumbnail face.
pub(super) fn thumbnail_font() -> (Font, Pixels) {
    (
        super::font::cached_font_config().font,
        px(THUMBNAIL_FONT_PX),
    )
}

/// A pane's last [`thumbnail_rows_for`] rows, painted read-only into a card.
pub(crate) struct TerminalThumbnail {
    backend: TerminalSessionBackend,
    theme: Arc<TerminalTheme>,
    /// Row offset the paint pass shifts the origin by, carried from prepaint.
    first_visible_row: i32,
}

impl TerminalThumbnail {
    pub(crate) fn new(backend: TerminalSessionBackend, theme: Arc<TerminalTheme>) -> Self {
        Self {
            backend,
            theme,
            first_visible_row: 0,
        }
    }
}

impl Element for TerminalThumbnail {
    type RequestLayoutState = ();
    type PrepaintState = Option<LayoutState>;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = relative(1.).into();
        let layout_id = window.request_layout(style, [], cx);
        (layout_id, ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        // Cull off-screen cards. This is the culling mechanism the frame
        // budget depends on: a card scrolled out of the overlay's viewport
        // does no terminal work at all, not even the snapshot lock read.
        let visible = window.content_mask().bounds;
        if !bounds.intersects(&visible) {
            return None;
        }

        let snap = thumbnail_snapshot(&self.backend);
        self.first_visible_row = snap.first_visible_row;

        let dims = thumbnail_cell_dimensions();
        let (base_font, _size) = thumbnail_font();
        // Same `terminal.minimum_contrast` floor the live pane applies, so a
        // preview never shows colours its pane does not.
        let minimum_contrast = crate::config_writer::current_config(cx).terminal.map_or(
            paneflow_config::schema::TerminalConfig::DEFAULT_MINIMUM_CONTRAST,
            |terminal| terminal.resolved_minimum_contrast(),
        );
        // A dim, non-blinking block cursor (spec §4.3): a useful "parked at a
        // prompt" signal for one quad. `cursor_from_content` is the private
        // helper `build_layout` uses; `focused: true` because the helper
        // returns `None` for an unfocused pane and a thumbnail is never
        // "unfocused". It filters `CursorShape::Hidden` itself. The shape is
        // then forced to `Block` regardless of the pane's own beam/underline/
        // vintage mode - a 5 px beam is invisible - and `text` is cleared so
        // the block does not try to re-shape the glyph under it. Blink is not
        // a layout input: it only gates whether `paint` draws the cursor, and
        // this element always draws it.
        let cursor = cursor_from_content(
            snap.content.cursor,
            true,
            self.theme.cursor.opacity(0.5),
            CursorShape::Block,
            &self.theme,
        )
        .map(|mut c| {
            c.shape = CursorShape::Block;
            c.text = None;
            c
        });
        Some(layout_from_snapshot(LayoutInputs {
            cells: snap.content.cells.clone(),
            cursor,
            // Selection, copy mode, and search belong to the live pane.
            selection_range: None,
            copy_mode_cursor: None,
            search_highlights: &[],
            display_offset: snap.content.display_offset,
            history_size: snap.content.history_size,
            desired_cols: snap.content.cols.max(1),
            desired_rows: snap.content.rows.max(1),
            first_visible_row: snap.first_visible_row,
            last_visible_row: snap.last_visible_row,
            dims,
            base_font,
            theme: &self.theme,
            exited: None,
            exit_signal: None,
            integrated_glyphs_enabled: true,
            color_emoji_enabled: false,
            minimum_contrast,
        }))
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let Some(layout) = prepaint.take() else {
            return;
        };
        let metrics = thumbnail_cell_metrics();
        // Cells keep their absolute line numbers, so lift the origin by the
        // cropped-away rows to land the band at the top of the card.
        let origin = Point {
            x: bounds.origin.x,
            y: bounds.origin.y - metrics.cell_height_px() * (self.first_visible_row as f32),
        };
        let geom = CellGeometry::new(origin, metrics);
        let (cell_x_bounds, cell_y_bounds) = if layout.desired_cols == 0 || layout.desired_rows == 0
        {
            (Vec::new(), Vec::new())
        } else {
            (
                geom.x_boundaries(layout.desired_cols),
                geom.y_boundaries(layout.desired_rows),
            )
        };
        let (base_font, font_size) = thumbnail_font();

        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            paint::background::paint_base_fill(&layout, bounds, window);
            paint::background::paint_cell_backgrounds(
                &layout,
                bounds,
                &cell_x_bounds,
                &cell_y_bounds,
                window,
            );
            paint::background::paint_block_quads(&layout, &cell_x_bounds, &cell_y_bounds, window);
            paint::sprites::paint_sprites(&layout, &geom, window);
            // Underlines and strikethroughs come from the cell metrics now
            // (#418), not from the text runs, so a card needs the pass too.
            paint::decorations::paint_decorations(&layout, &geom, window);
            paint::text::paint_text_runs(&layout, &geom, &base_font, font_size, window, cx);
            // Nerd Font icons leave the text runs for `layout.symbols` (#420)
            // and are constrained to their cells at paint time; without this
            // pass every icon cell on a card is an empty hole.
            paint::text::paint_symbols(&layout, &geom, font_size, window, cx);
            // The dim block cursor built in prepaint. Unconditional: there is
            // no blink phase here.
            paint::cursor::paint_cursor(&layout, &geom, &base_font, font_size, window, cx);
        });
        // Deliberately not painted: selection, search highlights, hyperlink
        // underlines, IME preedit, the scrollbar and its match rail, and Kitty
        // graphics. Kitty in particular: each pane carries a 32 MiB image cap,
        // and decoding placements into a 320px card is not a trade worth
        // making.
    }
}

impl IntoElement for TerminalThumbnail {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::TerminalState;

    /// The trap this whole module exists to avoid.
    ///
    /// `TerminalElement::build_layout` calls `notify_window_size`, which
    /// SIGWINCHes the child process to the element's bounds. A card-sized
    /// `TerminalElement` would resize every displayed pane to thumbnail-sized content worth
    /// of cells while the real pane resized it back, corrupting the layout of
    /// every pane the overlay showed. The thumbnail path must never touch the
    /// grid.
    #[test]
    fn thumbnail_never_resizes_the_pty() {
        let state = TerminalState::new_display_only(24, 80);
        let backend = state.session_backend();

        let before = backend.grid_metrics();
        for _ in 0..5 {
            let _ = thumbnail_snapshot(&backend);
        }
        let after = backend.grid_metrics();

        assert_eq!(
            (before.columns, before.screen_lines),
            (after.columns, after.screen_lines),
            "a thumbnail snapshot resized the grid"
        );
        assert_eq!((after.columns, after.screen_lines), (80, 24));
    }

    #[test]
    fn the_crop_is_the_last_rows_of_the_viewport() {
        let state = TerminalState::new_display_only(24, 80);
        let snap = thumbnail_snapshot(&state.session_backend());

        assert_eq!(snap.content.rows, 24);
        assert_eq!(snap.last_visible_row, snap.content.rows as i32);
        assert_eq!(
            snap.first_visible_row,
            snap.content.rows as i32 - THUMBNAIL_ROWS as i32
        );
    }

    /// A grid shorter than the crop depth must render all of it, not underflow.
    #[test]
    fn a_short_grid_crops_to_itself() {
        let state = TerminalState::new_display_only(5, 40);
        let snap = thumbnail_snapshot(&state.session_backend());

        assert!(snap.content.rows <= THUMBNAIL_ROWS);
        assert_eq!(snap.first_visible_row, 0);
        assert_eq!(snap.last_visible_row, snap.content.rows as i32);
    }

    /// The larger band at the smaller preview scale fits more terminal content.
    /// Use explicit defaults so the developer's config cannot change this check.
    #[test]
    fn the_thumbnail_band_fits_the_expected_cells() {
        use super::super::font::{DEFAULT_CELL_WIDTH, DEFAULT_LINE_HEIGHT, FontSettings};

        let defaults = FontSettings {
            font: gpui::font("JetBrainsMono Nerd Font"),
            size: 13.0,
            line_height: DEFAULT_LINE_HEIGHT,
            cell_width: DEFAULT_CELL_WIDTH,
        };
        let dims = thumbnail_cell_dimensions_for(&defaults);
        assert_eq!(f32::from(dims.cell_width), 4.0);
        assert_eq!(f32::from(dims.line_height), 8.0);
        assert_eq!(
            (THUMBNAIL_BAND_W / f32::from(dims.cell_width)).floor(),
            75.0
        );
        assert_eq!(
            (THUMBNAIL_BAND_H / f32::from(dims.line_height)).floor(),
            THUMBNAIL_ROWS as f32
        );
    }

    #[test]
    fn a_taller_line_height_crops_fewer_rows_so_the_prompt_stays_in_the_band() {
        // A larger line-height still crops from the bottom, preserving the prompt.
        use super::super::font::{DEFAULT_CELL_WIDTH, DEFAULT_LINE_HEIGHT, FontSettings};

        let mut settings = FontSettings {
            font: gpui::font("JetBrainsMono Nerd Font"),
            size: 13.0,
            line_height: DEFAULT_LINE_HEIGHT,
            cell_width: DEFAULT_CELL_WIDTH,
        };
        assert_eq!(
            thumbnail_rows_for(&thumbnail_cell_dimensions_for(&settings)),
            THUMBNAIL_ROWS
        );
        settings.line_height = 2.5;
        let dims = thumbnail_cell_dimensions_for(&settings);
        let rows = thumbnail_rows_for(&dims);
        assert_eq!(rows, 4);
        assert!(
            rows as f32 * f32::from(dims.line_height) <= THUMBNAIL_BAND_H,
            "every cropped row fits the band"
        );
        assert!(
            (rows + 1) as f32 * f32::from(dims.line_height) > THUMBNAIL_BAND_H,
            "one more row would not"
        );
    }

    /// The layout pass over the cropped range keeps only the bottom rows:
    /// text written above the crop line never reaches the painter.
    #[test]
    fn layout_over_the_crop_range_drops_rows_above_it() {
        let state = TerminalState::new_display_only(24, 80);
        for line in 0..24 {
            state.write_output(format!("row{line}\r\n").as_bytes());
        }
        let backend = state.session_backend();
        let snap = thumbnail_snapshot(&backend);
        let theme = crate::theme::active_theme();
        let (base_font, _) = thumbnail_font();
        let layout = layout_from_snapshot(LayoutInputs {
            cells: snap.content.cells.clone(),
            cursor: None,
            selection_range: None,
            copy_mode_cursor: None,
            search_highlights: &[],
            display_offset: snap.content.display_offset,
            history_size: snap.content.history_size,
            desired_cols: snap.content.cols.max(1),
            desired_rows: snap.content.rows.max(1),
            first_visible_row: snap.first_visible_row,
            last_visible_row: snap.last_visible_row,
            dims: thumbnail_cell_dimensions(),
            base_font,
            theme: &theme,
            exited: None,
            exit_signal: None,
            integrated_glyphs_enabled: true,
            color_emoji_enabled: false,
            minimum_contrast: 0.0,
        });
        let lines: std::collections::BTreeSet<i32> =
            layout.batched_runs.iter().map(|run| run.line).collect();
        assert!(!lines.is_empty(), "the bottom rows carry text");
        assert!(
            lines.iter().all(|line| *line >= snap.first_visible_row),
            "a run above the crop line reached the layout: {lines:?}"
        );
        assert!(
            lines.iter().all(|line| *line < snap.last_visible_row),
            "a run past the viewport reached the layout: {lines:?}"
        );
    }

    /// A Nerd Font icon on a card lives in `layout.symbols`, not in the text
    /// runs (#420), so the card's paint pass has to draw that collection too.
    /// The first half proves the layout puts the glyph there under the
    /// thumbnail's own inputs; the second half is a source-slice probe on
    /// this file's `paint` body, because a GPUI paint pass is not observable
    /// in a headless test and every earlier miss of this kind (decorations
    /// after #418, sprites after #419, symbols after #420) was exactly a
    /// pass the live element gained and the card did not.
    #[test]
    fn a_card_paints_every_glyph_collection_the_layout_produces() {
        let state = TerminalState::new_display_only(24, 80);
        // U+F07B (Nerd Font folder) is a Private Use Area icon that only
        // `paint_symbols` draws. The card crops to the bottom rows of the
        // viewport, so push the icon down to the last row first.
        state.write_output(&b"\r\n".repeat(23));
        state.write_output("\u{f07b} icons".as_bytes());
        let backend = state.session_backend();
        let snap = thumbnail_snapshot(&backend);
        let theme = crate::theme::active_theme();
        let (base_font, _) = thumbnail_font();
        let layout = layout_from_snapshot(LayoutInputs {
            cells: snap.content.cells.clone(),
            cursor: None,
            selection_range: None,
            copy_mode_cursor: None,
            search_highlights: &[],
            display_offset: snap.content.display_offset,
            history_size: snap.content.history_size,
            desired_cols: snap.content.cols.max(1),
            desired_rows: snap.content.rows.max(1),
            first_visible_row: snap.first_visible_row,
            last_visible_row: snap.last_visible_row,
            dims: thumbnail_cell_dimensions(),
            base_font,
            theme: &theme,
            exited: None,
            exit_signal: None,
            integrated_glyphs_enabled: true,
            color_emoji_enabled: false,
            minimum_contrast: 0.0,
        });
        assert_eq!(
            layout.symbols.len(),
            1,
            "the icon must reach layout.symbols under the card's inputs"
        );
        assert!(
            !layout
                .batched_runs
                .iter()
                .any(|run| run.text.contains('\u{f07b}')),
            "the icon must not also be in a text run"
        );

        let source = include_str!("thumbnail.rs");
        let start = source
            .find("fn paint(")
            .expect("thumbnail.rs defines the card paint");
        let end = source[start..]
            .find("// Deliberately not painted:")
            .map(|i| start + i)
            .expect("the paint body ends at the not-painted note");
        let body = &source[start..end];
        for pass in [
            "paint::sprites::paint_sprites(",
            "paint::decorations::paint_decorations(",
            "paint::text::paint_text_runs(",
            "paint::text::paint_symbols(",
        ] {
            assert!(body.contains(pass), "the card paint body must call {pass}");
        }
    }
}
