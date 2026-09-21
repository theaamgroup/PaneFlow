//! App content inside the native macOS window.

use gpui::{CursorStyle, Hsla, IntoElement, ParentElement, Styled, Window, div, prelude::*, px};

/// Keep the live macOS shell geometry: no client inset or synthetic resize border.
pub(crate) fn native_window_shell(
    content: impl IntoElement,
    window: &mut Window,
    background: Hsla,
) -> impl IntoElement {
    window.set_client_inset(px(0.0));
    div().id("window-backdrop").relative().size_full().child(
        div()
            .id("window-surface")
            .size_full()
            .cursor(CursorStyle::Arrow)
            .bg(background)
            .child(content),
    )
}
