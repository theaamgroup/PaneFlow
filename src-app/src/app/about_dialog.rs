//! About PaneFlow modal, styled as a compact native application dialog.

use gpui::{
    AnyElement, ClickEvent, Context, InteractiveElement, IntoElement, KeyDownEvent, MouseButton,
    ObjectFit, ParentElement, Styled, Window, deferred, div, hsla, img, linear_color_stop,
    linear_gradient, prelude::*, px, rgb, svg,
};

use crate::{
    PaneFlowApp,
    ui_primitives::{AnimatedHoverExt, lerp_color},
};

impl PaneFlowApp {
    /// Open About and move keyboard focus onto the card (issue #244). Without
    /// the focus move the card only looked modal: every keystroke, Escape
    /// included, still went into the focused PTY.
    pub(crate) fn open_about_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.show_about_dialog = true;
        self.about_dialog_focus.focus(window, cx);
        cx.notify();
    }

    /// Dismiss About and hand focus back to the workspace the card took it
    /// from. Every dismiss path (OK, the corner x, the backdrop, Escape) goes
    /// through here so none can strand focus on an unmounted node.
    pub(crate) fn close_about_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.show_about_dialog {
            return;
        }
        self.show_about_dialog = false;
        self.restore_focus_after_close_confirm(window, cx);
        cx.notify();
    }

    /// Escape and Enter both dismiss: OK is the card's only button, so Enter
    /// is the default-button gesture and Escape the cancel one. Consumed, so
    /// neither reaches a binding underneath.
    fn handle_about_dialog_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event.keystroke.key.as_str() {
            "escape" | "enter" => {
                self.close_about_dialog(window, cx);
                cx.stop_propagation();
            }
            _ => {}
        }
    }

    pub(crate) fn render_about_dialog(&self, cx: &mut Context<Self>) -> AnyElement {
        let ui = crate::theme::ui_colors();
        let version = env!("CARGO_PKG_VERSION");
        let button_hover_bg = gpui::Hsla::from(rgb(0x3a3a3a));

        // The credit plate's block caret rides the app-wide 530 ms blink phase
        // (`terminal/blink.rs`) instead of owning a timer: a decorative caret
        // does not justify a second ticker, and borrowing the shared phase puts
        // it in step with every terminal cursor already on screen. `try_global`
        // rather than `global` because a render pass must never panic - if the
        // phase was never installed the caret simply stays lit. The phase only
        // advances the drawing when something else marks the window dirty, so on
        // a fully idle window the caret holds whatever state it was last painted
        // in; that degrades to a static block rather than to a wrong one.
        let blink_phase = cx
            .try_global::<crate::terminal::blink::BlinkPhaseGlobal>()
            .map(|global| global.0.clone());
        let cursor_lit = blink_phase.is_none_or(|phase| phase.read(cx).visible);

        let close_x = div()
            .id("about-close-x")
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .w(px(30.))
            .h(px(30.))
            .rounded(px(7.))
            .animated_hover(move |style, delta| {
                style.bg(lerp_color(
                    button_hover_bg.opacity(0.0),
                    button_hover_bg,
                    delta,
                ));
            })
            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                this.close_about_dialog(window, cx);
                cx.stop_propagation();
            }))
            .child(
                svg()
                    .size(px(12.))
                    .flex_none()
                    .path("icons/close.svg")
                    .text_color(ui.text),
            );

        let header = div()
            .h(px(32.))
            .w_full()
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .pl(px(10.))
            .pr(px(2.))
            .bg(rgb(0x232323))
            .border_b_1()
            .border_color(rgb(0x343434))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(7.))
                    .child(
                        img("icons/paneflow.png")
                            .w(px(16.))
                            .h(px(16.))
                            .object_fit(ObjectFit::Contain),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .font_weight(gpui::FontWeight::NORMAL)
                            .text_color(ui.text)
                            .child("About PaneFlow"),
                    ),
            )
            .child(close_x);

        // Retro CRT credit plate. Radius, padding, border, text color and the
        // four-layer shadow stack are transcribed from the original CSS; two
        // details could not cross over. GPUI's `linear_gradient` carries exactly
        // two stops, so the CSS midpoint (#062a10 at 50%) is dropped - it lands
        // within two 8-bit steps of the straight interpolation between the
        // endpoints, so nothing is visually lost. And this GPUI rev has no
        // text-shadow, so the phosphor halo has to come from the outer box
        // shadow alone rather than from the glyphs.
        let credit_border = gpui::Hsla::from(rgb(0x2d8c4a));
        let credit_border_hover = gpui::Hsla::from(rgb(0x5cff8a));
        let credit_green = gpui::Hsla::from(rgb(0x5cff8a));

        let credit = div()
            .id("about-credit")
            .flex_none()
            .mt(px(18.))
            // Fixed height (8px padding + a 20px line + 8px padding) so this
            // plate and the original-credit plate below it are the same box
            // even if VT323's metrics resolve differently from the fallback
            // font.
            .h(px(36.))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(5.))
            .px(px(14.))
            .rounded(px(8.))
            .border_1()
            .border_color(credit_border)
            .bg(linear_gradient(
                180.,
                linear_color_stop(rgb(0x0e3d1a), 0.),
                linear_color_stop(rgb(0x021608), 1.),
            ))
            .shadow(vec![
                // Lit top edge of the bevel.
                gpui::BoxShadow::new(px(0.), px(1.), gpui::rgba(0x8cffaa99).into()).inset(),
                // Inner bottom shade, sinking the face into the frame.
                gpui::BoxShadow::new(px(0.), px(-2.), gpui::rgba(0x00000099).into())
                    .blur_radius(px(4.))
                    .inset(),
                // Drop shadow lifting the whole plate off the dialog.
                gpui::BoxShadow::new(px(0.), px(4.), gpui::rgba(0x00000080).into())
                    .blur_radius(px(14.)),
                // Phosphor bloom - the only glow available without text-shadow.
                gpui::BoxShadow::new(px(0.), px(0.), gpui::rgba(0x5cff8a33).into())
                    .blur_radius(px(18.)),
            ])
            .cursor_pointer()
            // Hover brightens the frame rather than the fill: animating `bg`
            // here would replace the gradient with a flat color mid-transition.
            .animated_hover(move |style, delta| {
                style.border_color(lerp_color(credit_border, credit_border_hover, delta));
            })
            .on_click(cx.listener(|_this, _: &ClickEvent, _, cx| {
                if let Err(e) =
                    crate::external_open::open_http_url("https://github.com/evilchinesefood")
                {
                    log::warn!("About: could not open the author's page: {e}");
                }
                // Deliberately leaves `show_about_dialog` alone: this is a
                // credit, not a dismiss control.
                cx.stop_propagation();
            }))
            .child(
                div()
                    .flex_none()
                    .font_family("VT323")
                    .text_size(px(16.))
                    .text_color(credit_green)
                    .child("> made with"),
            )
            // The heart is its own span for two reasons: it is the one glyph
            // that is not phosphor green, and VT323 has no ❤ - leaving the
            // family unset lets it fall back to a font that does.
            .child(
                div()
                    .flex_none()
                    .text_size(px(13.))
                    .text_color(rgb(0xff5c5c))
                    .child("❤"),
            )
            .child(
                div()
                    .flex_none()
                    .font_family("VT323")
                    .text_size(px(16.))
                    .text_color(credit_green)
                    .child("by david ayers"),
            )
            .child(
                div()
                    .flex_none()
                    .w(px(7.))
                    .h(px(14.))
                    .rounded(px(1.))
                    // Blinks by alpha, not by removal, so the line never
                    // reflows underneath the caret.
                    .bg(if cursor_lit {
                        credit_green
                    } else {
                        credit_green.opacity(0.0)
                    }),
            );

        // Issue #226: a text button, not a GitHub mark (none is bundled).
        // Same hover recipe as OK; click opens this fork and leaves About open.
        let github_button = div()
            .id("about-github")
            .flex_none()
            .mt(px(14.))
            .h(px(28.))
            .px(px(12.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(3.))
            .border_1()
            .border_color(rgb(0x666666))
            .bg(rgb(0x2d2d2d))
            .text_size(px(12.))
            .text_color(ui.text)
            .cursor_pointer()
            .animated_hover(move |style, delta| {
                style.bg(lerp_color(
                    gpui::Hsla::from(rgb(0x2d2d2d)),
                    button_hover_bg,
                    delta,
                ));
            })
            .on_click(cx.listener(|_this, _: &ClickEvent, _, cx| {
                if let Err(e) =
                    crate::external_open::open_http_url("https://github.com/theaamgroup/paneflow")
                {
                    log::warn!("About: could not open GitHub: {e}");
                }
                cx.stop_propagation();
            }))
            .child("View on GitHub");

        // Issue #227: attribution link to the original project. Replaces the
        // inert contributors placeholder. Does not restore an Arthur Jean
        // copyright line; the AAM line and the David Ayers plate stay as-is.
        let upstream_border = ui.muted.opacity(0.4);
        let upstream_hover = ui.muted;
        let upstream = div()
            .id("about-upstream")
            .flex_none()
            .mt(px(10.))
            .h(px(36.))
            .flex()
            .items_center()
            .justify_center()
            .px(px(14.))
            .rounded(px(8.))
            .border_1()
            .border_dashed()
            .border_color(upstream_border)
            .text_size(px(11.))
            .text_color(ui.muted)
            .cursor_pointer()
            .animated_hover(move |style, delta| {
                style.border_color(lerp_color(upstream_border, upstream_hover, delta));
            })
            .on_click(cx.listener(|_this, _: &ClickEvent, _, cx| {
                if let Err(e) =
                    crate::external_open::open_http_url("https://github.com/arthjean/paneflow")
                {
                    log::warn!("About: could not open the original project: {e}");
                }
                cx.stop_propagation();
            }))
            .child("Original project: arthjean/paneflow");

        let body = div()
            .w_full()
            // Grown from 320 to seat the GitHub button between Version and
            // copyright. Fixed, not auto, to keep the dialog a constant size
            // like the rest of this chrome.
            .h(px(362.))
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .bg(rgb(0x202020))
            .child(
                img("icons/paneflow.png")
                    .w(px(64.))
                    .h(px(64.))
                    .object_fit(ObjectFit::Contain),
            )
            .child(
                div()
                    .mt(px(14.))
                    .text_color(ui.text)
                    .text_size(px(16.))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child("PaneFlow"),
            )
            .child(
                div()
                    .mt(px(20.))
                    .text_color(ui.muted)
                    .text_size(px(12.))
                    .child(format!("Version {version}")),
            )
            .child(github_button)
            .child(
                div()
                    .mt(px(14.))
                    .text_color(ui.muted)
                    .text_size(px(12.))
                    .child("© 2026 AAM USA, Inc. All rights reserved."),
            )
            .child(credit)
            .child(upstream);

        let ok_button = div()
            .id("about-ok")
            .w(px(76.))
            .h(px(28.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(3.))
            .border_1()
            .border_color(rgb(0x666666))
            .bg(rgb(0x2d2d2d))
            .text_size(px(12.))
            .text_color(ui.text)
            .animated_hover(move |style, delta| {
                style.bg(lerp_color(
                    gpui::Hsla::from(rgb(0x2d2d2d)),
                    button_hover_bg,
                    delta,
                ));
            })
            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                this.close_about_dialog(window, cx);
                cx.stop_propagation();
            }))
            .child("OK");

        let footer = div()
            .w_full()
            .h(px(56.))
            .flex_none()
            .flex()
            .items_center()
            .justify_end()
            .px(px(14.))
            .bg(rgb(0x252525))
            .border_t_1()
            .border_color(rgb(0x343434))
            .child(ok_button);

        let dialog = div()
            .id("about-dialog")
            .occlude()
            .track_focus(&self.about_dialog_focus)
            .on_key_down(cx.listener(Self::handle_about_dialog_key_down))
            .w(px(382.))
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(rgb(0x202020))
            .border_1()
            .border_color(rgb(0x3a3a3a))
            .rounded(px(10.))
            .shadow_lg()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
            .child(header)
            .child(body)
            .child(footer);

        deferred(
            div()
                .id("about-dialog-backdrop")
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .bg(hsla(0., 0., 0., 0.55))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, window, cx| {
                        this.close_about_dialog(window, cx);
                    }),
                )
                .child(dialog),
        )
        .with_priority(10)
        .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    fn production_source() -> &'static str {
        include_str!("about_dialog.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("production half of about_dialog.rs")
    }

    /// Issue #244: About is a modal, so it owns the keyboard while it is up.
    /// Every open path moves focus onto the card, Escape (and Enter, OK being
    /// the only button) dismisses it, and dismissing hands focus back to the
    /// pane the user was typing in. Before this, a keystroke aimed at the
    /// dialog went into the focused PTY - including a live agent prompt.
    #[test]
    fn about_dialog_takes_focus_and_escape_dismisses_it() {
        use crate::source_probe::source_slice;

        let src = production_source();
        let card = source_slice(src, "id(\"about-dialog\")", "id(\"about-dialog-backdrop\")");
        assert!(
            card.contains(".track_focus(&self.about_dialog_focus)"),
            "the About card must track its own focus handle: {card}"
        );
        assert!(
            card.contains(".on_key_down(cx.listener(Self::handle_about_dialog_key_down))"),
            "the About card must route key events to its own handler: {card}"
        );
        let keys = source_slice(src, "fn handle_about_dialog_key_down(", "\n    }");
        assert!(
            keys.contains("\"escape\"") && keys.contains("self.close_about_dialog(window, cx)"),
            "Escape must dismiss About: {keys}"
        );
        let open = source_slice(src, "pub(crate) fn open_about_dialog(", "\n    }");
        assert!(
            open.contains("self.about_dialog_focus.focus(window, cx)"),
            "opening About must move focus onto the card: {open}"
        );
        let close = source_slice(src, "pub(crate) fn close_about_dialog(", "\n    }");
        assert!(
            close.contains("self.restore_focus_after_close_confirm(window, cx)"),
            "dismissing About must hand focus back to the workspace: {close}"
        );
        // Every open path goes through `open_about_dialog`, so none can skip
        // the focus move; every dismiss goes through `close_about_dialog`, so
        // none can strand focus on an unmounted node.
        for (name, file) in [
            ("main.rs", include_str!("../main.rs")),
            ("bootstrap.rs", include_str!("bootstrap.rs")),
            ("profile_menu.rs", include_str!("profile_menu.rs")),
        ] {
            let production = file.split("#[cfg(test)]").next().expect("production half");
            assert!(
                !production.contains("show_about_dialog = true"),
                "{name} must open About through open_about_dialog, not by flipping the flag"
            );
        }
        assert!(
            !src.contains("this.show_about_dialog = false"),
            "every About dismiss must go through close_about_dialog so focus is handed back"
        );
    }

    /// Issue #226: About exposes a View on GitHub control that opens this
    /// fork through `open_http_url` and does not dismiss the dialog.
    #[test]
    fn about_dialog_links_this_forks_github() {
        let src = production_source();
        assert!(
            src.contains("id(\"about-github\")"),
            "About must carry an about-github control"
        );
        assert!(
            src.contains("\"https://github.com/theaamgroup/paneflow\""),
            "About GitHub button must open this fork's repository"
        );
        assert!(
            src.contains("open_http_url"),
            "About web links must go through open_http_url, not open_url"
        );
        assert!(
            !src.contains("Add Contributors Here"),
            "the inert contributors placeholder must be gone"
        );
        let github_click = src
            .split("id(\"about-github\")")
            .nth(1)
            .and_then(|rest| rest.split("id(\"about-upstream\")").next())
            .expect("about-github click handler");
        assert!(
            !github_click.contains("show_about_dialog = false"),
            "View on GitHub must leave About open: {github_click}"
        );
        assert!(
            github_click.contains("open_http_url"),
            "View on GitHub must call open_http_url: {github_click}"
        );
    }

    /// Issue #227: the inert contributors plate is an original-credit button.
    /// The David Ayers plate stays.
    #[test]
    fn about_dialog_credits_the_original_project() {
        let src = production_source();
        assert!(
            src.contains("id(\"about-upstream\")"),
            "About must carry an about-upstream control"
        );
        assert!(
            src.contains("\"https://github.com/arthjean/paneflow\""),
            "original-credit button must open arthjean/paneflow"
        );
        assert!(
            src.contains("id(\"about-credit\")"),
            "the David Ayers credit plate must stay"
        );
        assert!(
            src.contains("\"https://github.com/evilchinesefood\""),
            "the David Ayers plate URL must stay"
        );
        assert!(
            !src.contains("Add Contributors Here"),
            "the inert contributors placeholder must be gone"
        );
        let upstream_click = src
            .split("id(\"about-upstream\")")
            .nth(1)
            .and_then(|rest| rest.split("let body =").next())
            .expect("about-upstream click handler");
        assert!(
            !upstream_click.contains("show_about_dialog = false"),
            "original-credit must leave About open: {upstream_click}"
        );
        assert!(
            upstream_click.contains("open_http_url"),
            "original-credit must call open_http_url: {upstream_click}"
        );
    }
}
