//! Reusable UI widgets shared across modals and settings screens.
//!
//! Each submodule exposes a self-contained GPUI entity + element pair that
//! the rest of the app can embed without knowing its internals.
//!
//! Two scrollbar systems coexist until #435 lands: `scrollbar` is the
//! div-overlay thumb floated over popover lists (its own 24 px minimum,
//! `metrics` / `track_click_offset` / `drag_offset` helpers), while
//! `editor_scrollbar` is the permanent 15 px canvas-painted gutter beside the
//! Changes dock (#434). Both share `scrollbar::geometry` for the thumb math.

pub mod callout;
pub(crate) mod editor_scrollbar;
pub mod scrollbar;
pub mod text_area;
pub mod text_input;
