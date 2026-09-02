//! The System Info modal behind Help > System Info… (#184 Phase 4, upstream
//! #37).
//!
//! Shows the report [`crate::system_info`] collects, one label per row, and
//! copies it to the clipboard as Markdown on demand. Showing it before copying
//! is the point: the block goes into a public issue, so the reporter gets to
//! read exactly what they are about to publish.
//!
//! The card follows the app's own modal shape (themed `overlay` surface,
//! header with a close affordance, footer with the actions) rather than the
//! hardcoded native-dialog palette of `about_dialog.rs`, so it stays legible
//! under a light theme.

use gpui::{
    AnyElement, AppContext as _, AsyncApp, ClickEvent, ClipboardItem, Context, CursorStyle,
    FontWeight, InteractiveElement, IntoElement, KeyDownEvent, MouseButton, ParentElement, Pixels,
    Styled, WeakEntity, Window, deferred, div, hsla, prelude::*, px, svg,
};

use crate::PaneFlowApp;
use crate::settings::components::{card_color, secondary_button, with_alpha};
use crate::system_info::{SystemInfo, SystemInfoProbe};
use crate::ui_primitives::squircle::{squircle_border, squircle_fill};
use crate::ui_primitives::{BODY, LABEL_SM, TITLE, squircle_skin};

/// Width of the card. Wider than the About dialog because a GPU line can run
/// long (a dual-GPU machine lists both adapters on one row), and a value that
/// wraps mid-string is harder to read at a glance.
const DIALOG_WIDTH: Pixels = px(560.);
/// Label column. Sized for the longest label ("Terminal engine") so every
/// value starts on the same x, which is what makes the block scannable.
const LABEL_WIDTH: Pixels = px(116.);
/// The app's card corner, traced as a superellipse rather than an arc - the
/// same silhouette as a pane card and a settings card.
const CARD_RADIUS: Pixels = crate::app::constants::PANE_CARD_RADIUS;
/// One horizontal inset for the header, the rows and the footer, so every
/// edge in the card lines up on the same two verticals.
const CARD_PADDING: Pixels = px(20.);
/// Line box shared by both columns of a row.
const ROW_LINE_HEIGHT: Pixels = px(18.);
/// Height the collecting state reserves: the report's seven rows plus their
/// six 8px gaps, so the card does not grow under the pointer when the probes
/// answer a frame later.
const COLLECTING_MIN_HEIGHT: Pixels = px(174.);

/// State of the System Info modal. Absent from `PaneFlowApp` when closed.
///
/// The probes run off the render thread, so the modal opens before the report
/// exists: a click has to feel instant even on a host where a `sysctl` or the
/// Metal enumeration takes a moment.
pub(crate) enum SystemInfoDialog {
    /// Open, probes still running. In practice a frame or two.
    Collecting,
    Ready(SystemInfo),
}

impl PaneFlowApp {
    /// Help > System Info…: open the modal, then fill it in when the
    /// background probes answer.
    pub(crate) fn open_system_info_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let probe = SystemInfoProbe::capture(window);
        self.system_info_dialog = Some(SystemInfoDialog::Collecting);
        // Issue #244: the card owns the keyboard while it is up.
        self.system_info_dialog_focus.focus(window, cx);
        cx.notify();

        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let report = cx.background_spawn(async move { probe.resolve() }).await;
            log::info!("system info:\n{report}");
            let _ = this.update(cx, |app: &mut Self, cx: &mut Context<Self>| {
                // Closed while the probes ran: leave it closed rather than
                // resurrecting a modal the user already dismissed.
                if app.system_info_dialog.is_some() {
                    app.system_info_dialog = Some(SystemInfoDialog::Ready(report));
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Dismiss the modal and hand focus back to the workspace it took it
    /// from. Every dismiss path (Close, the corner x, the backdrop, Escape)
    /// goes through here.
    pub(crate) fn close_system_info_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.system_info_dialog.take().is_some() {
            self.restore_focus_after_close_confirm(window, cx);
            cx.notify();
        }
    }

    /// Escape dismisses; consumed so it does not reach a binding underneath.
    /// Enter is deliberately not a dismiss: the footer has two buttons (Close
    /// and Copy) and neither is the default one.
    fn handle_system_info_dialog_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.keystroke.key.as_str() == "escape" {
            self.close_system_info_dialog(window, cx);
            cx.stop_propagation();
        }
    }

    /// Put the report on the clipboard as Markdown bullets, and confirm with
    /// a toast.
    ///
    /// The modal stays open: copying is not the end of the errand. A reporter
    /// may want to read a value back, or copy again after switching windows,
    /// and closing the panel out from under them would cost a second trip
    /// through the Help menu. Dismissal stays the user's own gesture - Close,
    /// the corner button, or the backdrop.
    fn copy_system_info(&mut self, cx: &mut Context<Self>) {
        let Some(SystemInfoDialog::Ready(report)) = self.system_info_dialog.as_ref() else {
            return;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(report.to_string()));
        self.show_toast("System info copied to the clipboard", cx);
    }

    pub(crate) fn render_system_info_dialog(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(dialog) = self.system_info_dialog.as_ref() else {
            return div().into_any_element();
        };
        let ui = crate::theme::ui_colors();

        // Square hit target with the app's own corner, sized so the glyph sits
        // optically centred in it. `items_start` on the header row pins it to
        // the top of a two-line heading; the negative top margin lifts its
        // centre onto the title's baseline box rather than the block's.
        let close_x = squircle_skin(
            div()
                .id("system-info-close")
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .size(px(24.))
                // The hit target is 24px around a 12px glyph, so its own 6px
                // of slack is cancelled here: the glyph, not the box, lands on
                // the card's 20px inset. `-2` centres the box on the title's
                // 20px line box instead of on the two-line heading block.
                .mt(px(-2.))
                .mr(px(-6.))
                .cursor(CursorStyle::PointingHand)
                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                    this.close_system_info_dialog(window, cx);
                    cx.stop_propagation();
                })),
            "system-info-close-skin",
            px(8.),
            None,
            Some(ui.subtle),
        )
        .child(
            svg()
                .size(px(12.))
                .flex_none()
                .path("icons/close.svg")
                .text_color(ui.muted),
        );

        // Codex-quiet heading: no divider, no header fill. Hierarchy comes
        // from type and spacing, the way `custom_buttons_modal` does it.
        let header = div()
            .flex()
            .flex_row()
            .items_start()
            .justify_between()
            .gap(px(12.))
            .px(CARD_PADDING)
            .pt(px(16.))
            .pb(px(16.))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    // `flex_1` claims the width the close button leaves, the
                    // way `setting_text` does. Without it the column shrinks to
                    // a fraction of the card and the one-line subtitle wraps
                    // into a narrow stack.
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .text_size(TITLE)
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(ui.text)
                            .child("System info"),
                    )
                    .child(
                        div()
                            .text_size(LABEL_SM)
                            .text_color(ui.muted)
                            .child("Goes into a bug report. No paths, no environment."),
                    ),
            )
            .child(close_x);

        let body = match dialog {
            SystemInfoDialog::Collecting => div()
                .px(CARD_PADDING)
                .pb(px(4.))
                .min_h(COLLECTING_MIN_HEIGHT)
                .text_size(BODY)
                .line_height(ROW_LINE_HEIGHT)
                .text_color(ui.muted)
                .child("Collecting..."),
            SystemInfoDialog::Ready(report) => {
                let mut rows = div()
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .px(CARD_PADDING)
                    .pb(px(4.));
                for (label, value) in report.rows() {
                    rows = rows.child(
                        div()
                            .flex()
                            .flex_row()
                            .items_start()
                            .gap(px(12.))
                            .child(
                                div()
                                    .flex_none()
                                    .w(LABEL_WIDTH)
                                    .text_size(BODY)
                                    // Label and value use different families,
                                    // so their baselines only line up if both
                                    // sides are given the same line box.
                                    .line_height(ROW_LINE_HEIGHT)
                                    .text_color(ui.muted)
                                    .child(label),
                            )
                            .child(
                                // Monospace: these are identifiers, versions
                                // and driver strings, and a proportional font
                                // makes a build number harder to read back.
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .font_family("monospace")
                                    .text_size(BODY)
                                    .line_height(ROW_LINE_HEIGHT)
                                    .text_color(ui.text)
                                    .child(value),
                            ),
                    );
                }
                rows
            }
        };

        let is_ready = matches!(dialog, SystemInfoDialog::Ready(_));
        let footer = div()
            .flex()
            .flex_row()
            .items_center()
            .justify_end()
            .gap(px(8.))
            .px(CARD_PADDING)
            .pt(px(18.))
            .pb(px(16.))
            .child(secondary_button(
                "system-info-close-button",
                "Close",
                ui,
                cx.listener(|this, _: &ClickEvent, window, cx| {
                    this.close_system_info_dialog(window, cx);
                    cx.stop_propagation();
                }),
            ))
            // The Copy button appears only once there is something to copy, so
            // a click during the collecting frame cannot put an empty report
            // on the clipboard.
            .when(is_ready, |footer| {
                footer.child(secondary_button(
                    "system-info-copy",
                    "Copy",
                    ui,
                    cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.copy_system_info(cx);
                        cx.stop_propagation();
                    }),
                ))
            });

        // Same silhouette as a pane card and a settings card: the superellipse
        // is painted as an absolute fill under the content, with the hairline
        // stroked over it, so the corner matches the rest of the app instead of
        // GPUI's circular `rounded()`. The host keeps a matching `rounded()`
        // with no background of its own - it paints nothing, it only gives the
        // drop shadow the same corner as the fill.
        let card = div()
            .id("system-info-dialog")
            .occlude()
            .track_focus(&self.system_info_dialog_focus)
            .on_key_down(cx.listener(Self::handle_system_info_dialog_key_down))
            .relative()
            .w(DIALOG_WIDTH)
            .rounded(CARD_RADIUS)
            .shadow_lg()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
            .child(squircle_fill(CARD_RADIUS, card_color()))
            .child(
                div()
                    .relative()
                    .flex()
                    .flex_col()
                    .child(header)
                    .child(body)
                    .child(footer),
            )
            .child(squircle_border(
                CARD_RADIUS,
                px(1.),
                with_alpha(ui.border, 0.6),
            ));

        deferred(
            div()
                .id("system-info-backdrop")
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
                        this.close_system_info_dialog(window, cx);
                    }),
                )
                .child(card),
        )
        .with_priority(10)
        .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    /// Every macOS menu action needs BOTH a render-root `.on_action` in
    /// `main.rs` and an app-global fallback in
    /// `install_macos_menu_action_fallbacks`: AppKit validates the item
    /// through `is_action_available`, which can miss the root listeners while
    /// focus sits in a terminal, and the item then paints permanently greyed.
    /// The existing bootstrap test pins that pairing for Settings / Minimize
    /// / Zoom by name; this one pins it for System Info, plus the render hook
    /// without which the action would open a modal nobody can see.
    #[test]
    fn help_menu_system_info_is_wired_at_the_root_and_as_a_fallback() {
        let bootstrap = include_str!("bootstrap.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("production half of bootstrap.rs");
        let menu_bar = bootstrap
            .split("pub(crate) fn install_macos_menu_bar")
            .nth(1)
            .and_then(|rest| {
                rest.split("pub(crate) fn install_macos_menu_action_fallbacks")
                    .next()
            })
            .expect("install_macos_menu_bar body");
        let help_menu = menu_bar
            .split("Menu::new(\"Help\")")
            .nth(1)
            .expect("the Help menu");
        assert!(
            help_menu.contains("MenuItem::action(\"System Info…\", crate::ShowSystemInfo)"),
            "Help > System Info… must dispatch ShowSystemInfo:\n{help_menu}"
        );
        let help_at = help_menu
            .find("MenuItem::action(\"PaneFlow Help\", OpenHelp)")
            .expect("Help > PaneFlow Help");
        let system_info_at = help_menu
            .find("MenuItem::action(\"System Info…\"")
            .expect("Help > System Info…");
        assert!(
            help_at < system_info_at,
            "PaneFlow Help stays the first Help item"
        );

        let fallbacks = bootstrap
            .split("pub(crate) fn install_macos_menu_action_fallbacks")
            .nth(1)
            .expect("install_macos_menu_action_fallbacks body");
        let fallback = fallbacks
            .split("cx.on_action(|_: &crate::ShowSystemInfo, cx|")
            .nth(1)
            .and_then(|rest| rest.split("cx.on_action(").next())
            .expect("missing app-global fallback for ShowSystemInfo; the item would grey out");
        assert!(
            fallback.contains("app.open_system_info_dialog(window, cx)"),
            "the fallback must open the same dialog as the root listener: {fallback}"
        );

        let main = include_str!("../main.rs");
        assert!(
            main.contains("_: &ShowSystemInfo, window, cx|")
                && main.contains("this.open_system_info_dialog(window, cx)"),
            "main.rs must carry the render-root `.on_action` for ShowSystemInfo"
        );
        assert!(
            main.contains("self.render_system_info_dialog(cx)"),
            "main.rs must render the dialog from the app content tree"
        );
    }

    /// Issue #244: System Info is a modal, so it owns the keyboard while it
    /// is up. Opening moves focus onto the card, Escape dismisses it, and
    /// dismissing hands focus back to the pane the user was typing in.
    #[test]
    fn system_info_dialog_takes_focus_and_escape_dismisses_it() {
        use crate::source_probe::source_slice;

        let src = include_str!("system_info_dialog.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("production half of system_info_dialog.rs");
        let card = source_slice(
            src,
            "id(\"system-info-dialog\")",
            "id(\"system-info-backdrop\")",
        );
        assert!(
            card.contains(".track_focus(&self.system_info_dialog_focus)"),
            "the System Info card must track its own focus handle: {card}"
        );
        assert!(
            card.contains(".on_key_down(cx.listener(Self::handle_system_info_dialog_key_down))"),
            "the System Info card must route key events to its own handler: {card}"
        );
        let keys = source_slice(src, "fn handle_system_info_dialog_key_down(", "\n    }");
        assert!(
            keys.contains("\"escape\"")
                && keys.contains("self.close_system_info_dialog(window, cx)"),
            "Escape must dismiss System Info: {keys}"
        );
        let open = source_slice(src, "pub(crate) fn open_system_info_dialog(", "\n    }");
        assert!(
            open.contains("self.system_info_dialog_focus.focus(window, cx)"),
            "opening System Info must move focus onto the card: {open}"
        );
        let close = source_slice(src, "pub(crate) fn close_system_info_dialog(", "\n    }");
        assert!(
            close.contains("self.restore_focus_after_close_confirm(window, cx)"),
            "dismissing System Info must hand focus back to the workspace: {close}"
        );
    }

    /// Menu-only, like `About` / `OpenHelp` / `OpenSettings`: a registry entry
    /// with no default chord would sit in Settings > Keyboard Shortcuts as a
    /// permanently `Unassigned` row.
    #[test]
    fn show_system_info_stays_out_of_the_shortcut_registry() {
        let registry = include_str!("../keybindings/registry.rs");
        assert!(
            !registry.contains("ShowSystemInfo"),
            "ShowSystemInfo must not be listed in keybindings::registry::ACTIONS"
        );
        let defaults = include_str!("../keybindings/defaults.rs");
        assert!(
            !defaults.contains("show_system_info") && !defaults.contains("ShowSystemInfo"),
            "ShowSystemInfo must have no default chord"
        );
    }
}
