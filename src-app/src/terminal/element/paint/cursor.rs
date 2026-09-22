//! Cursor paint pass - primary text cursor
//! (Vintage/Block/Beam/Underline/DoubleUnderline/HollowBlock) plus the
//! copy-mode selection anchor cursor, drawn on the device-pixel grid with
//! the cell metrics' cursor thickness (Ghostty `special.zig`).

use gpui::{
    App, BorderStyle, Font, FontStyle, FontWeight, Pixels, SharedString, TextRun, Window, fill,
    outline,
};

use super::super::geometry::CellGeometry;
use super::super::{CursorInfo, LayoutState, MIN_APCA_CONTRAST, ensure_minimum_contrast};
use crate::terminal::types::CursorShape;

/// Glyph colour for a block cursor.
///
/// An opaque cell or layout background is the inverted colour and is used
/// as-is. Both are transparent when the pane card paints the ground, so the
/// glyph takes that opaque theme background and must clear
/// `MIN_APCA_CONTRAST` against the cursor (#677).
fn cursor_text_color(
    cell_bg: gpui::Hsla,
    layout_bg: gpui::Hsla,
    pane_bg: gpui::Hsla,
    cursor_color: gpui::Hsla,
) -> gpui::Hsla {
    if cell_bg.a > 0.01 {
        cell_bg
    } else if layout_bg.a > 0.01 {
        layout_bg
    } else {
        ensure_minimum_contrast(pane_bg, cursor_color, MIN_APCA_CONTRAST)
    }
}

fn paint_cursor_info(
    cursor: &CursorInfo,
    layout: &LayoutState,
    geom: &CellGeometry,
    base_font: &Font,
    font_size: Pixels,
    window: &mut Window,
    cx: &mut App,
) {
    let cols = if cursor.wide { 2 } else { 1 };
    let m = geom.metrics;
    let x0 = geom.x_device(cursor.col);
    let y0 = geom.y_device(cursor.line);
    let w = geom.x_device(cursor.col + cols) - x0;
    let h = geom.y_device(cursor.line + 1) - y0;
    if w <= 0 || h <= 0 {
        return;
    }
    let t = m.cursor_thickness.max(1);
    let color = cursor.color;

    match cursor.shape {
        CursorShape::Vintage => {
            let height = ((h as f32 * 0.28).round() as i32).max(t * 2).min(h);
            window.paint_quad(fill(
                geom.device_rect(x0, y0 + h - height, w, height),
                color,
            ));
        }
        CursorShape::Block => {
            window.paint_quad(fill(geom.device_rect(x0, y0, w, h), color));
            // Paint the character on top of the cursor quad, in the cell's
            // background color, on the same baseline as the text pass.
            if let Some(ch) = cursor.text {
                let mut cursor_font = base_font.clone();
                if cursor.bold {
                    cursor_font.weight = FontWeight::BOLD;
                }
                if cursor.italic {
                    cursor_font.style = FontStyle::Italic;
                }
                let cursor_font = super::display_font_for_intensity(&cursor_font, base_font.weight);
                let text = SharedString::from(ch.to_string());
                let text_color = cursor_text_color(
                    cursor.cell_bg,
                    layout.background_color,
                    layout.exit_overlay_background,
                    cursor.color,
                );
                let shaped = window.text_system().shape_line(
                    text.clone(),
                    font_size,
                    &[TextRun {
                        len: text.len(),
                        font: cursor_font,
                        color: text_color,
                        background_color: None,
                        underline: None,
                        strikethrough: None,
                    }],
                    // Match the normal terminal text path so the glyph does
                    // not shift when a block cursor moves over it.
                    Some(geom.cell_width),
                );
                super::text::paint_shaped_line(
                    &shaped,
                    geom.cell_origin(cursor.line, cursor.col),
                    text_color,
                    geom,
                    layout.color_emoji_enabled,
                    window,
                    cx,
                );
            }
        }
        CursorShape::Beam => {
            // Half the thickness hangs over the left edge of the cell so the
            // bar sits between characters rather than on the first one.
            // Rounding up: a one-pixel bar shifted left reads better.
            let x = x0 - (t + 1) / 2;
            window.paint_quad(fill(geom.device_rect(x, y0, t, h), color));
        }
        CursorShape::Underline => {
            let y = m.underline_position.min(h + h / 4 - t);
            window.paint_quad(fill(geom.device_rect(x0, y0 + y, w, t), color));
        }
        CursorShape::DoubleUnderline => {
            let y = m.underline_position.min(h + h / 4 - 2 * t);
            window.paint_quad(fill(geom.device_rect(x0, y0 + (y - t).max(0), w, t), color));
            window.paint_quad(fill(geom.device_rect(x0, y0 + y + t, w, t), color));
        }
        CursorShape::HollowBlock => {
            // The block hollowed out by one cursor thickness.
            window.paint_quad(
                outline(geom.device_rect(x0, y0, w, h), color, BorderStyle::Solid)
                    .border_widths(geom.logical(t)),
            );
        }
        CursorShape::Hidden => {} // Already filtered in build_layout
    }
}

/// Paint the primary cursor at its grid position using the shape dictated by
/// the terminal mode + config. For Block shapes, shapes the underlying
/// character on top in the terminal's background color.
pub fn paint_cursor(
    layout: &LayoutState,
    geom: &CellGeometry,
    base_font: &Font,
    font_size: Pixels,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(cursor) = &layout.cursor else {
        return;
    };

    paint_cursor_info(cursor, layout, geom, base_font, font_size, window, cx);
}

/// Paint the secondary selection marker using the same glyph-aware cursor pass
/// as the primary cursor.
pub fn paint_anchor_cursor(
    layout: &LayoutState,
    geom: &CellGeometry,
    base_font: &Font,
    font_size: Pixels,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(anchor) = &layout.anchor_cursor else {
        return;
    };

    paint_cursor_info(anchor, layout, geom, base_font, font_size, window, cx);
}

#[cfg(test)]
mod tests {
    use super::super::super::{MIN_APCA_CONTRAST, apca_contrast};
    use super::cursor_text_color;
    use gpui::hsla;

    /// Issue #677: a black block cursor on Vercel Light used to paint the
    /// glyph in `hsla(0, 0, 0.08, 1)` because both stored backgrounds are
    /// transparent. The glyph has to clear the APCA floor against the cursor.
    #[test]
    fn cursor_text_on_transparent_backgrounds_contrasts_with_a_black_cursor() {
        let cursor = hsla(0.0, 0.0, 0.0, 1.0);
        let pane = crate::theme::vercel_light().background;
        let transparent = hsla(0.0, 0.0, 0.0, 0.0);
        let result = cursor_text_color(transparent, transparent, pane, cursor);

        assert_ne!(result, hsla(0.0, 0.0, 0.08, 1.0));
        let lc = apca_contrast(result, cursor).abs();
        assert!(
            lc >= MIN_APCA_CONTRAST,
            "block cursor glyph Lc {lc} is below {MIN_APCA_CONTRAST}"
        );
    }

    #[test]
    fn cursor_text_keeps_an_opaque_cell_or_layout_background() {
        let cursor = hsla(0.0, 0.0, 0.0, 1.0);
        let pane = hsla(0.0, 0.0, 1.0, 1.0);
        let transparent = hsla(0.0, 0.0, 0.0, 0.0);
        let cell = hsla(0.2, 0.4, 0.3, 1.0);
        let layout = hsla(0.55, 0.2, 0.7, 1.0);

        assert_eq!(cursor_text_color(cell, transparent, pane, cursor), cell);
        assert_eq!(cursor_text_color(transparent, layout, pane, cursor), layout);
    }
}
