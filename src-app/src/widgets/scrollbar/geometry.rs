//! Pure scrollbar geometry for the editor-style scrollbars: track size, the
//! minimum thumb, and the thumb rectangle for a given scroll state.
//!
//! GPU-free and side-effect free so it is unit-testable; painting and mouse
//! handling live in `widgets::editor_scrollbar`.

use gpui::{Bounds, Pixels, point, px, size};

/// Track thickness in px: the gutter the scrollbar occupies beside its host.
pub(crate) const SCROLLBAR_SIZE: f32 = 15.0;
/// Minimum thumb length in px, so a very long document still leaves
/// something to grab.
pub(crate) const MIN_THUMB: f32 = 25.0;

/// A laid-out track: its bounds plus the thumb rectangle, `None` when the
/// content fits and there is nothing to scroll.
#[derive(Clone, Copy, Default)]
pub(crate) struct Track {
    pub(crate) bounds: Bounds<Pixels>,
    pub(crate) thumb: Option<Bounds<Pixels>>,
}

/// Lay out the thumb inside `bounds` for a viewport `visible` units long
/// showing `total` units of content scrolled to `offset`. `horizontal` lays
/// the thumb along the x axis instead of y. The thumb length is the
/// viewport/content ratio of the track, floored at [`MIN_THUMB`] and capped
/// at the track; its start maps `offset / (total - visible)` onto the room
/// left in the track.
pub(crate) fn scrollbar_track(
    bounds: Bounds<Pixels>,
    visible: f64,
    total: f64,
    offset: f64,
    horizontal: bool,
) -> Track {
    let length = f32::from(if horizontal {
        bounds.size.width
    } else {
        bounds.size.height
    });
    let thumb = if length > 0.0 && total > visible && visible > 0.0 {
        let thumb_length = (length * (visible / total) as f32)
            .max(MIN_THUMB)
            .min(length);
        let start = ((offset / (total - visible)).clamp(0.0, 1.0) as f32) * (length - thumb_length);
        Some(if horizontal {
            Bounds::new(
                point(bounds.origin.x + px(start), bounds.origin.y),
                size(px(thumb_length), bounds.size.height),
            )
        } else {
            Bounds::new(
                point(bounds.origin.x, bounds.origin.y + px(start)),
                size(bounds.size.width, px(thumb_length)),
            )
        })
    } else {
        None
    };
    Track { bounds, thumb }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(height: f32) -> Bounds<Pixels> {
        Bounds::new(
            point(px(10.), px(20.)),
            size(px(SCROLLBAR_SIZE), px(height)),
        )
    }

    #[test]
    fn thumb_is_absent_when_content_fits_or_track_is_empty() {
        assert!(
            scrollbar_track(track(200.), 100., 100., 0., false)
                .thumb
                .is_none()
        );
        assert!(
            scrollbar_track(track(200.), 100., 50., 0., false)
                .thumb
                .is_none()
        );
        assert!(
            scrollbar_track(track(0.), 100., 400., 0., false)
                .thumb
                .is_none()
        );
        assert!(
            scrollbar_track(track(200.), 0., 400., 0., false)
                .thumb
                .is_none()
        );
    }

    #[test]
    fn thumb_height_scales_with_viewport_to_content_ratio() {
        let thumb = scrollbar_track(track(200.), 100., 400., 0., false)
            .thumb
            .expect("overflowing content shows a thumb");
        assert_eq!(f32::from(thumb.size.height), 50.);
        assert_eq!(f32::from(thumb.size.width), SCROLLBAR_SIZE);
        assert_eq!(f32::from(thumb.origin.x), 10.);

        let thumb = scrollbar_track(track(200.), 100., 500., 0., false)
            .thumb
            .expect("thumb");
        assert_eq!(f32::from(thumb.size.height), 40.);
    }

    #[test]
    fn thumb_never_shrinks_below_min_thumb() {
        let thumb = scrollbar_track(track(200.), 10., 100_000., 0., false)
            .thumb
            .expect("thumb");
        assert_eq!(f32::from(thumb.size.height), MIN_THUMB);
    }

    #[test]
    fn thumb_never_exceeds_a_track_shorter_than_min_thumb() {
        let thumb = scrollbar_track(track(20.), 10., 100_000., 0., false)
            .thumb
            .expect("thumb");
        assert_eq!(f32::from(thumb.size.height), 20.);
        assert_eq!(f32::from(thumb.origin.y), 20.);
    }

    #[test]
    fn thumb_start_tracks_the_scroll_offset_and_clamps() {
        // 200 px track, 50 px thumb -> 150 px of travel for 300 units of scroll.
        let at = |offset: f64| {
            f32::from(
                scrollbar_track(track(200.), 100., 400., offset, false)
                    .thumb
                    .expect("thumb")
                    .origin
                    .y,
            ) - 20.
        };
        assert_eq!(at(0.), 0.);
        assert_eq!(at(150.), 75.);
        assert_eq!(at(300.), 150.);
        assert_eq!(
            at(900.),
            150.,
            "offset past the end clamps to the track end"
        );
        assert_eq!(at(-50.), 0., "negative offset clamps to the track start");
    }

    #[test]
    fn horizontal_lays_the_thumb_along_x() {
        let bounds = Bounds::new(point(px(10.), px(20.)), size(px(200.), px(SCROLLBAR_SIZE)));
        let thumb = scrollbar_track(bounds, 100., 400., 150., true)
            .thumb
            .expect("thumb");
        assert_eq!(f32::from(thumb.size.width), 50.);
        assert_eq!(f32::from(thumb.size.height), SCROLLBAR_SIZE);
        assert_eq!(f32::from(thumb.origin.x), 85.);
        assert_eq!(f32::from(thumb.origin.y), 20.);
    }

    #[test]
    fn track_reports_its_own_bounds() {
        let bounds = track(200.);
        let laid = scrollbar_track(bounds, 100., 400., 0., false);
        assert_eq!(laid.bounds, bounds);
    }
}
