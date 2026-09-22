//! Toast types, helpers and rendering.
//!
//! Owns:
//! - `Toast`: ephemeral bottom-right confirmation/error pop-ups.
//! - `show_toast` / `push_toast`: convenience helpers attached to `PaneFlowApp`.
//! - `show_release_notes_toast` / `dismiss_toast`: the one sticky,
//!   action-bearing toast (#526) and the dismissal every toast ends through.
//! - `render_toast`: the deferred rendering block used by `Render for
//!   PaneFlowApp` to paint the active toast.

use gpui::{
    Animation, AnimationExt, AnyElement, AsyncApp, ClickEvent, Context, CursorStyle, IntoElement,
    MouseButton, ParentElement, Role, SharedString, Styled, WeakEntity, deferred, div, ease_in_out,
    prelude::*, px, svg,
};

use crate::PaneFlowApp;
use crate::app::constants::{TOAST_ENTER_MS, TOAST_EXIT_MS, TOAST_HOLD_MS};
use crate::settings::components::with_alpha;
use crate::theme::UiColors;
use crate::ui_primitives::{AnimatedHoverExt, icon_button_sm};

#[derive(Clone)]
pub(crate) struct Toast {
    pub(crate) message: String,
    /// How long the "hold" phase of the toast animation lasts, in ms.
    /// Must match the auto-dismiss timer in [`PaneFlowApp::push_toast`] -
    /// otherwise the exit animation plays early and the element persists
    /// as a ghost at opacity 0 until the dismiss task fires.
    pub(crate) hold_ms: u64,
    /// `Some` makes the toast **sticky**: it never times out, carries the
    /// action button plus a close glyph, and ignores `hold_ms`. An action the
    /// user has to reach for cannot sit behind a timer.
    pub(crate) action: Option<ToastAction>,
    /// Assigned by `show_next_toast` (never 0 once shown). Part of both
    /// animation ids so a replacement does not reuse a finished exit.
    pub(crate) serial: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ToastAction {
    /// "View release notes": opens this `https` URL in the default browser.
    OpenReleaseNotes(String),
}

impl Toast {
    /// Milliseconds until the toast dismisses itself, or `None` when it is
    /// sticky and only a click or a newer toast ends it.
    fn lifetime_ms(&self) -> Option<u64> {
        self.action
            .is_none()
            .then_some(TOAST_ENTER_MS + self.hold_ms + TOAST_EXIT_MS)
    }

    fn is_sticky(&self) -> bool {
        self.lifetime_ms().is_none()
    }
}

/// What an arriving toast does to the visible one: a sticky toast yields at
/// once (or it would hold every later confirmation hostage until clicked),
/// a timed one finishes first.
fn arrival_replaces_active(active: Option<&Toast>) -> bool {
    active.is_none_or(Toast::is_sticky)
}

impl PaneFlowApp {
    pub(crate) fn show_toast(&mut self, message: impl Into<String>, cx: &mut Context<Self>) {
        self.push_toast(message.into(), TOAST_HOLD_MS, cx);
    }

    pub(crate) fn push_toast(&mut self, message: String, hold_ms: u64, cx: &mut Context<Self>) {
        self.enqueue_toast(
            Toast {
                message,
                hold_ms,
                action: None,
                serial: 0,
            },
            cx,
        );
    }

    /// Issue #526: "Updated to PaneFlow x.y.z" with a "View release notes"
    /// button, raised once on the first launch of a newer version.
    pub(crate) fn show_release_notes_toast(&mut self, version: &str, cx: &mut Context<Self>) {
        self.enqueue_toast(
            Toast {
                message: format!("Updated to PaneFlow {version}"),
                hold_ms: 0,
                action: Some(ToastAction::OpenReleaseNotes(
                    crate::release_notes::release_notes_url(version),
                )),
                serial: 0,
            },
            cx,
        );
    }

    fn enqueue_toast(&mut self, toast: Toast, cx: &mut Context<Self>) {
        if arrival_replaces_active(self.toast.as_ref()) {
            self.show_next_toast(toast, cx);
        } else {
            self.toast_queue.push_back(toast);
            cx.notify();
        }
    }

    fn show_next_toast(&mut self, mut toast: Toast, cx: &mut Context<Self>) {
        // Skip 0, the construction sentinel, including after wrap.
        self.toast_serial = self.toast_serial.checked_add(1).unwrap_or(1);
        toast.serial = self.toast_serial;
        let lifetime_ms = toast.lifetime_ms();
        self.toast = Some(toast);
        cx.notify();

        let Some(total) = lifetime_ms else {
            // Sticky: no timer. Dropping the previous task also cancels it.
            self._toast_task = None;
            return;
        };
        self._toast_task = Some(cx.spawn(
            async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
                smol::Timer::after(std::time::Duration::from_millis(total)).await;
                let _ = cx.update(|cx| {
                    this.update(cx, |app: &mut Self, cx: &mut Context<Self>| {
                        app.dismiss_toast(cx);
                    })
                });
            },
        ));
    }

    /// End the visible toast and show the next queued one, if any. The timer
    /// of a timed toast and every click on a sticky one land here.
    pub(crate) fn dismiss_toast(&mut self, cx: &mut Context<Self>) {
        if let Some(next) = self.toast_queue.pop_front() {
            self.show_next_toast(next, cx);
        } else {
            self.toast = None;
            self._toast_task = None;
            cx.notify();
        }
    }

    /// Build the deferred element that paints the active toast at the
    /// bottom-right of the window. Caller is responsible for the
    /// `if let Some(toast) = &self.toast` guard.
    pub(crate) fn render_toast(
        &self,
        toast: &Toast,
        ui: UiColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if let Some(action) = &toast.action {
            return self.render_sticky_toast(toast, action, ui, cx);
        }
        let is_error = toast_message_reads_like_error(&toast.message);
        let (icon, icon_color, max_w) = if is_error {
            ("icons/triangle-alert.svg", ui.agent_error, px(440.))
        } else {
            ("icons/check.svg", ui.vc_added, px(340.))
        };

        let header = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(9.))
            .child(
                svg()
                    .size(px(15.))
                    .flex_none()
                    .path(icon)
                    .text_color(icon_color),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_x_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_size(px(12.5))
                    .text_color(ui.text)
                    .child(toast.message.clone()),
            );

        let hold_ms = toast.hold_ms;
        let (element_id, animation_id) = toast_animation_identity("copy-toast", toast.serial);
        deferred(
            div()
                .id(SharedString::from(element_id))
                .absolute()
                .right(px(18.))
                .bottom(px(18.))
                .max_w(max_w)
                .min_w(px(220.))
                .rounded(px(8.))
                .bg(ui.subtle)
                .text_sm()
                .text_color(ui.text)
                .overflow_hidden()
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .pl(px(12.))
                        .pr(px(14.))
                        .py(px(11.))
                        .child(header),
                )
                .with_animations(
                    SharedString::from(animation_id),
                    vec![
                        Animation::new(std::time::Duration::from_millis(TOAST_ENTER_MS))
                            .with_easing(ease_in_out),
                        Animation::new(std::time::Duration::from_millis(hold_ms)),
                        Animation::new(std::time::Duration::from_millis(TOAST_EXIT_MS))
                            .with_easing(ease_in_out),
                    ],
                    |toast_el, stage, delta| {
                        let opacity = toast_stage_opacity(stage, delta);
                        match stage {
                            0 => {
                                let lift = 8.0 * (1.0 - delta);
                                toast_el.opacity(opacity).bottom(px(20.0 + lift))
                            }
                            1 => toast_el.opacity(opacity).bottom(px(20.0)),
                            _ => {
                                let drop = 8.0 * delta;
                                toast_el.opacity(opacity).bottom(px(20.0 + drop))
                            }
                        }
                    },
                ),
        )
        .priority(2)
        .into_any_element()
    }

    /// The sticky variant (#526): the same `subtle` skin and single row, plus
    /// one action button and a close glyph. It enters like any toast and then
    /// holds until a click - the button, the close glyph, or the surface -
    /// or until a newer toast replaces it. No shadow, no frame (DESIGN 5.8).
    fn render_sticky_toast(
        &self,
        toast: &Toast,
        action: &ToastAction,
        ui: UiColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let ToastAction::OpenReleaseNotes(url) = action.clone();
        let action_label: SharedString = "View release notes".into();

        let action_button = div()
            .id("toast-action")
            .role(Role::Button)
            .aria_label(action_label.clone())
            .flex_none()
            .flex()
            .items_center()
            .h(px(24.))
            .px(px(9.))
            .rounded(px(6.))
            .cursor(CursorStyle::PointingHand)
            .text_size(px(12.))
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_color(ui.text)
            .whitespace_nowrap()
            .animated_hover_bg(with_alpha(ui.text, 0.08), with_alpha(ui.text, 0.14))
            .child(action_label)
            // The surface dismisses on click; keep the press from arming it
            // so one click never dismisses twice (and eats the next toast).
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                if let Err(err) = crate::external_open::open_http_url(&url) {
                    log::warn!("toast: open release notes failed: {err}");
                }
                this.dismiss_toast(cx);
                cx.stop_propagation();
            }));

        let close = icon_button_sm(
            "toast-close",
            "icons/close.svg",
            "Dismiss",
            ui.muted,
            with_alpha(ui.text, 0.08),
        )
        .cursor(CursorStyle::PointingHand)
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
            this.dismiss_toast(cx);
            cx.stop_propagation();
        }));

        let (element_id, animation_id) = toast_animation_identity("sticky-toast", toast.serial);
        let row = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(9.))
            .child(
                svg()
                    .size(px(15.))
                    .flex_none()
                    .path("icons/check.svg")
                    .text_color(ui.vc_added),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_x_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_size(px(12.5))
                    .text_color(ui.text)
                    .child(toast.message.clone()),
            )
            .child(action_button)
            .child(close);

        deferred(
            div()
                .id(SharedString::from(element_id))
                .absolute()
                .right(px(18.))
                .bottom(px(18.))
                .max_w(px(440.))
                .min_w(px(220.))
                .rounded(px(8.))
                .bg(ui.subtle)
                .text_sm()
                .text_color(ui.text)
                .overflow_hidden()
                // A press on the toast must not reach the pane under it.
                .occlude()
                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                    this.dismiss_toast(cx);
                }))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .pl(px(12.))
                        .pr(px(8.))
                        .py(px(8.))
                        .child(row),
                )
                .with_animations(
                    SharedString::from(animation_id),
                    vec![
                        Animation::new(std::time::Duration::from_millis(TOAST_ENTER_MS))
                            .with_easing(ease_in_out),
                    ],
                    |toast_el, _, delta| {
                        let lift = 8.0 * (1.0 - delta);
                        toast_el
                            .opacity(toast_stage_opacity(0, delta))
                            .bottom(px(20.0 + lift))
                    },
                ),
        )
        .priority(2)
        .into_any_element()
    }
}

/// Element id and animation id for one shown toast. The serial is in both:
/// GPUI stores `AnimationState` on the animation element id and would reuse a
/// finished exit (opacity 0) if the next toast kept the same id.
fn toast_animation_identity(prefix: &str, serial: u64) -> (String, String) {
    (
        format!("{prefix}-{serial}"),
        format!("{prefix}-anim-{serial}"),
    )
}

/// Opacity of the timed toast at `stage` and animation `delta`.
/// Stage 0 enters (`delta`), stage 1 holds at 1, later stages exit (`1 - delta`).
fn toast_stage_opacity(stage: usize, delta: f32) -> f32 {
    match stage {
        0 => delta,
        1 => 1.0,
        _ => 1.0 - delta,
    }
}

fn toast_message_reads_like_error(message: &str) -> bool {
    let message = message.to_lowercase();
    [
        "could not",
        "couldn't",
        "failed",
        "failure",
        "error",
        "invalid",
        "unavailable",
        "not found",
        "unsupported",
        "corrupt",
        "tampered",
        "timeout",
    ]
    .iter()
    .any(|needle| message.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn timed(hold_ms: u64) -> Toast {
        Toast {
            message: "Copied".into(),
            hold_ms,
            action: None,
            serial: 0,
        }
    }

    fn sticky() -> Toast {
        Toast {
            message: "Updated to PaneFlow 0.6.1".into(),
            hold_ms: 0,
            action: Some(ToastAction::OpenReleaseNotes(
                crate::release_notes::release_notes_url("0.6.1"),
            )),
            serial: 0,
        }
    }

    #[test]
    fn a_timed_toast_lives_for_enter_hold_exit() {
        assert_eq!(
            timed(TOAST_HOLD_MS).lifetime_ms(),
            Some(TOAST_ENTER_MS + TOAST_HOLD_MS + TOAST_EXIT_MS)
        );
        assert!(!timed(TOAST_HOLD_MS).is_sticky());
    }

    #[test]
    fn an_action_toast_never_times_out() {
        assert_eq!(sticky().lifetime_ms(), None);
        assert!(sticky().is_sticky());
    }

    #[test]
    fn an_arrival_replaces_a_sticky_toast_and_queues_behind_a_timed_one() {
        assert!(arrival_replaces_active(None));
        assert!(arrival_replaces_active(Some(&sticky())));
        assert!(!arrival_replaces_active(Some(&timed(TOAST_HOLD_MS))));
    }

    #[test]
    fn a_queued_toast_restarts_its_enter_animation() {
        // 1 then 2 are the serials `show_next_toast` assigns to successive toasts.
        let (first_element, first_animation) = toast_animation_identity("copy-toast", 1);
        let (second_element, second_animation) = toast_animation_identity("copy-toast", 2);
        assert_ne!(first_element, second_element);
        assert_ne!(first_animation, second_animation);
        assert!(first_element.contains('1'));
        assert!(first_animation.contains('1'));
        assert!(second_element.contains('2'));
        assert!(second_animation.contains('2'));
        assert_eq!(first_element, "copy-toast-1");
        assert_eq!(first_animation, "copy-toast-anim-1");
        assert_eq!(second_element, "copy-toast-2");
        assert_eq!(second_animation, "copy-toast-anim-2");

        let (sticky_element, sticky_animation) = toast_animation_identity("sticky-toast", 1);
        let (next_sticky_element, next_sticky_animation) =
            toast_animation_identity("sticky-toast", 2);
        assert_ne!(sticky_element, next_sticky_element);
        assert_ne!(sticky_animation, next_sticky_animation);
        assert!(sticky_element.contains('1'));
        assert!(sticky_animation.contains('1'));
        assert!(next_sticky_element.contains('2'));
        assert!(next_sticky_animation.contains('2'));

        // A fresh serial starts GPUI at stage 0. Reusing the finished exit is
        // stage 2 at delta 1.0, which this formula maps to opacity 0.
        let enter_opacity = toast_stage_opacity(0, 0.5);
        assert!(enter_opacity > 0.0);
        assert_eq!(enter_opacity, 0.5);
        assert_eq!(toast_stage_opacity(2, 1.0), 0.0);
    }
}
