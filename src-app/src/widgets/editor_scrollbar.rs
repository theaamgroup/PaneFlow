//! Permanent editor-style vertical scrollbar (#434, upstream 6003ae11).
//!
//! Unlike the div-overlay in `widgets::scrollbar` (a thin thumb floated over
//! popover lists), this is a fixed 15 px gutter that sits *beside* its
//! `overflow_y_scroll` host in a flex row and paints track + thumb through a
//! `canvas`. It reads the host's [`ScrollHandle`] every paint, so it needs no
//! content estimate, and it never holds its own scroll position: a track
//! click or a drag writes straight back into the handle.
//!
//! Mouse move / up are observed in the capture phase from the paint closure,
//! so a drag keeps scrolling after the pointer leaves the gutter and a
//! release anywhere ends it. State is `Rc<Cell<_>>` so the clones handed to
//! the paint and event closures share one drag / hover / track.

use std::cell::Cell;
use std::rc::Rc;

use gpui::{
    AnyElement, BorderStyle, Bounds, Context, Corners, DispatchPhase, Edges, InteractiveElement,
    IntoElement, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement, Pixels,
    ScrollHandle, StatefulInteractiveElement, Styled, canvas, div, point, px, quad,
};

use super::scrollbar::geometry::{SCROLLBAR_SIZE, Track, scrollbar_track};

/// A thumb drag in progress: where the pointer was pressed, the scroll
/// offset at that moment, and the scroll units one pixel of travel is worth.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Drag {
    mouse_y: Pixels,
    offset: f32,
    units_per_pixel: f32,
}

/// Scroll units the content moves for one pixel of thumb travel: the whole
/// scrollable range spread over the track room left beside the thumb.
fn units_per_pixel(track: Track, thumb: Bounds<Pixels>, max: f32) -> f32 {
    max / f32::from(track.bounds.size.height - thumb.size.height).max(1.0)
}

/// Scroll offset that centres the thumb on a track click at `y`, clamped to
/// `[0, max]` so a click near either end parks the thumb flush with it.
fn centred_click_offset(
    track: Track,
    thumb: Bounds<Pixels>,
    y: Pixels,
    units_per_pixel: f32,
    max: f32,
) -> f32 {
    (f32::from(y - track.bounds.top() - thumb.size.height / 2.0) * units_per_pixel).clamp(0.0, max)
}

/// Scroll offset for a drag whose pointer has moved to `mouse_y`.
fn drag_offset(drag: Drag, mouse_y: Pixels, max: f32) -> f32 {
    (drag.offset + f32::from(mouse_y - drag.mouse_y) * drag.units_per_pixel).clamp(0.0, max)
}

/// Shared scrollbar state for one host. `Default` is a fresh, idle bar.
#[derive(Clone, Default)]
pub(crate) struct EditorScrollbar {
    /// Track + thumb laid out at the last paint; the click and hover tests
    /// read it because GPUI hands mouse events window coordinates.
    track: Rc<Cell<Track>>,
    drag: Rc<Cell<Option<Drag>>>,
    hovered: Rc<Cell<bool>>,
}

impl EditorScrollbar {
    /// Drop a drag in progress. The state outlives the frame that started
    /// it, so the dock close and tab switch paths call this: otherwise the
    /// capture-phase move listener keeps scrolling the next dock from the
    /// old anchor for as long as the button stays down.
    pub(crate) fn cancel_drag(&self) {
        self.drag.set(None);
    }

    /// The gutter element. Place it as the flex-row sibling of the
    /// `overflow_y_scroll` div that `scroll` tracks (give that div
    /// `min_w_0()` so the gutter is never squeezed out).
    pub(crate) fn render<T: 'static>(
        &self,
        scroll: &ScrollHandle,
        cx: &mut Context<T>,
    ) -> AnyElement {
        let down = self.clone();
        let handle = scroll.clone();
        let paint = self.clone();
        let paint_handle = scroll.clone();
        let owner = cx.entity().downgrade();
        let ui = crate::theme::ui_colors();
        let thumb_color = crate::theme::active_theme().scrollbar_thumb;
        div()
            .id("editor-vertical-scrollbar")
            .w(px(SCROLLBAR_SIZE))
            .h_full()
            .flex_none()
            .overflow_hidden()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |_, ev: &MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    let track = down.track.get();
                    let Some(thumb) = track.thumb else {
                        return;
                    };
                    let max = f32::from(handle.max_offset().y).max(0.0);
                    let units_per_pixel = units_per_pixel(track, thumb, max);
                    let mut offset = f32::from(-handle.offset().y).clamp(0.0, max);
                    if !thumb.contains(&ev.position) {
                        // Track click: jump so the thumb is centred under the
                        // pointer, then keep dragging from there.
                        offset =
                            centred_click_offset(track, thumb, ev.position.y, units_per_pixel, max);
                        handle.set_offset(point(handle.offset().x, px(-offset)));
                    }
                    down.drag.set(Some(Drag {
                        mouse_y: ev.position.y,
                        offset,
                        units_per_pixel,
                    }));
                    cx.notify();
                }),
            )
            .on_click(|_, _, cx| cx.stop_propagation())
            .child(
                canvas(
                    |_, _, _| {},
                    move |bounds, _, window, _| {
                        let visible = f64::from(f32::from(paint_handle.bounds().size.height));
                        let max = f64::from(f32::from(paint_handle.max_offset().y).max(0.0));
                        let track = scrollbar_track(
                            bounds,
                            visible,
                            visible + max,
                            f64::from(f32::from(-paint_handle.offset().y)),
                            false,
                        );
                        paint.track.set(track);
                        // A 1 px left border separates the gutter from the body.
                        let edges = Edges {
                            left: px(1.),
                            ..Default::default()
                        };
                        window.paint_quad(quad(
                            bounds,
                            Corners::default(),
                            gpui::transparent_black(),
                            edges,
                            ui.border,
                            BorderStyle::Solid,
                        ));
                        if let Some(thumb) = track.thumb {
                            let color = if paint.drag.get().is_some() {
                                thumb_color.blend(ui.text.opacity(0.2))
                            } else if paint.hovered.get() {
                                thumb_color.blend(ui.text.opacity(0.1))
                            } else {
                                thumb_color
                            };
                            window.paint_quad(quad(
                                thumb,
                                Corners::default(),
                                color,
                                edges,
                                ui.border,
                                BorderStyle::Solid,
                            ));
                        }
                        // Capture-phase move: hover tint, drag-to-scroll, and
                        // a release we missed (button no longer held).
                        let moving = paint.clone();
                        let handle = paint_handle.clone();
                        let moving_owner = owner.clone();
                        window.on_mouse_event(move |ev: &MouseMoveEvent, phase, _, cx| {
                            if phase != DispatchPhase::Capture {
                                return;
                            }
                            let hovered = moving
                                .track
                                .get()
                                .thumb
                                .is_some_and(|thumb| thumb.contains(&ev.position));
                            let mut changed = moving.hovered.replace(hovered) != hovered;
                            if ev.pressed_button != Some(MouseButton::Left) {
                                changed |= moving.drag.take().is_some();
                            } else if let Some(drag) = moving.drag.get() {
                                let max = f32::from(handle.max_offset().y).max(0.0);
                                let offset = drag_offset(drag, ev.position.y, max);
                                handle.set_offset(point(handle.offset().x, px(-offset)));
                                changed = true;
                                cx.stop_propagation();
                            }
                            if changed {
                                let _ = moving_owner.update(cx, |_, cx| cx.notify());
                            }
                        });
                        let released = paint.clone();
                        let released_owner = owner.clone();
                        window.on_mouse_event(move |ev: &MouseUpEvent, phase, _, cx| {
                            if phase == DispatchPhase::Capture
                                && ev.button == MouseButton::Left
                                && released.drag.take().is_some()
                            {
                                let _ = released_owner.update(cx, |_, cx| cx.notify());
                                cx.stop_propagation();
                            }
                        });
                    },
                )
                .size_full(),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::size;

    // A 200 px track at y=20 holding a 50 px thumb: 150 px of travel for
    // 300 units of scroll, so one pixel is worth two units.
    fn fixture() -> (Track, Bounds<Pixels>, f32) {
        let bounds = Bounds::new(point(px(0.), px(20.)), size(px(SCROLLBAR_SIZE), px(200.)));
        let track = scrollbar_track(bounds, 100., 400., 0., false);
        let thumb = track.thumb.expect("thumb");
        (track, thumb, 300.)
    }

    #[test]
    fn units_per_pixel_spreads_the_range_over_the_thumb_travel() {
        let (track, thumb, max) = fixture();
        assert_eq!(units_per_pixel(track, thumb, max), 2.0);
    }

    #[test]
    fn units_per_pixel_survives_a_thumb_that_fills_the_track() {
        let bounds = Bounds::new(point(px(0.), px(0.)), size(px(SCROLLBAR_SIZE), px(20.)));
        let track = scrollbar_track(bounds, 10., 100_000., 0., false);
        let thumb = track.thumb.expect("thumb");
        assert_eq!(f32::from(thumb.size.height), 20.);
        assert_eq!(units_per_pixel(track, thumb, 99_990.), 99_990.);
    }

    #[test]
    fn track_click_centres_the_thumb_on_the_click() {
        let (track, thumb, max) = fixture();
        let upp = units_per_pixel(track, thumb, max);
        let offset = centred_click_offset(track, thumb, px(120.), upp, max);
        assert_eq!(offset, 150.);
        let after = scrollbar_track(track.bounds, 100., 400., f64::from(offset), false)
            .thumb
            .expect("thumb");
        assert_eq!(f32::from(after.origin.y), 95.);
        assert_eq!(f32::from(after.bottom()), 145.);
    }

    #[test]
    fn track_click_clamps_at_both_ends() {
        let (track, thumb, max) = fixture();
        let upp = units_per_pixel(track, thumb, max);
        assert_eq!(centred_click_offset(track, thumb, px(25.), upp, max), 0.);
        assert_eq!(centred_click_offset(track, thumb, px(215.), upp, max), max);
    }

    #[test]
    fn drag_maps_pixel_delta_to_scroll_offset() {
        let drag = Drag {
            mouse_y: px(50.),
            offset: 100.,
            units_per_pixel: 2.,
        };
        assert_eq!(drag_offset(drag, px(50.), 300.), 100.);
        assert_eq!(drag_offset(drag, px(70.), 300.), 140.);
        assert_eq!(drag_offset(drag, px(30.), 300.), 60.);
    }

    #[test]
    fn drag_clamps_to_the_scroll_range() {
        let drag = Drag {
            mouse_y: px(50.),
            offset: 100.,
            units_per_pixel: 2.,
        };
        assert_eq!(drag_offset(drag, px(-100.), 300.), 0.);
        assert_eq!(drag_offset(drag, px(1_000.), 300.), 300.);
    }
}
