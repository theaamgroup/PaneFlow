use super::*;
use crate::diff::review_terminal::ReviewCli;

impl Pane {
    pub(super) fn render_review_menu(&self, cx: &mut Context<Self>) -> AnyElement {
        let ui = pane_colors();
        let mut menu = crate::settings::components::menu_surface(div().id("pane-review-menu"), ui)
            .absolute()
            .top(px(PANE_HEADER_HEIGHT))
            .right(px(6.))
            .w(px(256.))
            .flex()
            .flex_col()
            .p(px(6.))
            .gap(px(2.))
            .occlude()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.review_menu_open = false;
                cx.notify();
            }))
            .child(
                div()
                    .text_color(ui.muted)
                    .text_size(px(12.))
                    .child("Review in a new agent tab"),
            );
        for (index, cli) in ReviewCli::all().into_iter().enumerate() {
            let checked = self.review_picks[index];
            menu = menu.child(
                crate::settings::components::select_item(
                    SharedString::from(format!("review-pick-{index}")),
                    false,
                    ui,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.review_picks[index] = !this.review_picks[index];
                    cx.notify();
                }))
                .child(if checked { "✓" } else { "○" })
                .child(cli.label()),
            );
        }
        menu = menu
            .child(
                crate::settings::components::select_item("review-launch", false, ui)
                    .on_click(cx.listener(|this, _, _, cx| {
                        let PaneSurface::Diff(view) = &this.surface else {
                            return;
                        };
                        let subject = view.read(cx).subject();
                        let base = view.read(cx).base_ref().to_owned();
                        let picks = ReviewCli::all()
                            .into_iter()
                            .enumerate()
                            .filter_map(|(i, cli)| this.review_picks[i].then_some(cli))
                            .collect();
                        cx.emit(PaneEvent::ReviewWithAgent {
                            subject,
                            base,
                            picks,
                        });
                        this.review_menu_open = false;
                        cx.notify();
                    }))
                    .child("Review with agent"),
            )
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(ui.muted)
                    .child("Prompt copied too · ⌘V to paste · Enter to submit"),
            );
        deferred(menu).priority(8).into_any_element()
    }
}
